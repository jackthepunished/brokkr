//! `brokkr-control` daemon entrypoint.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use brokkr_cas::RedbCas;
use brokkr_control::policy_reload::spawn_policy_reloader;
use brokkr_control::registry::WorkerRegistry;
use brokkr_control::services::{ObservabilityDeps, ObservabilityService, PeerObservabilityService};
use brokkr_control::wasm_strategy::WasmStrategy;
use brokkr_control::{
    auth_interceptor, spawn_eviction_task, spawn_lease_reaper, ActionCacheService, Authenticator,
    CapabilitiesService, CasService, ExecutionService, JwtAuth, MetaKvActionCache, RedbMetaKv,
    Scheduler, SharedWorkerRegistry, WorkerServiceImpl,
};
use brokkr_policy::{PolicyEngine, PolicyLimits};
use brokkr_proto::brokkr_v1::observability_service_server::ObservabilityServiceServer;
use brokkr_proto::brokkr_v1::peer_observability_server::PeerObservabilityServer;
use brokkr_proto::brokkr_v1::worker_service_server::WorkerServiceServer;
use brokkr_proto::reapi_v2::{
    action_cache_server::ActionCacheServer, capabilities_server::CapabilitiesServer,
    content_addressable_storage_server::ContentAddressableStorageServer,
    execution_server::ExecutionServer,
};
use clap::Parser;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::transport::ServerTlsConfig;

#[derive(Debug, Parser)]
#[command(
    name = "brokkr-control",
    version,
    about = "Brokkr control plane daemon"
)]
struct Args {
    /// Address to bind the gRPC server on.
    #[arg(long, default_value = "127.0.0.1:7878")]
    listen: SocketAddr,

    /// Directory holding the control plane's persistent state: the CAS
    /// (`cas.redb`) and the metadata store (`meta.redb` — action cache today,
    /// cluster configuration from I8c; not disposable).
    #[arg(long, default_value = "./brokkr-data")]
    data_dir: PathBuf,

    /// TLS server certificate (PEM-encoded).
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// TLS private key (PEM-encoded).
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,

    /// CA certificate for verifying client certificates (mTLS).
    #[arg(long, requires_all = ["tls_cert", "tls_key"])]
    tls_client_ca: Option<PathBuf>,

    /// File holding the HMAC secret for validating client JWT bearer tokens
    /// (HS256). Mutually exclusive with `--auth-jwt-rsa-pem-file`.
    #[arg(long)]
    auth_jwt_hmac_secret_file: Option<PathBuf>,

    /// File holding the RSA public key (PEM) for validating client JWT bearer
    /// tokens (RS256). Mutually exclusive with `--auth-jwt-hmac-secret-file`.
    #[arg(long)]
    auth_jwt_rsa_pem_file: Option<PathBuf>,

    /// Required JWT issuer (`iss`), if set.
    #[arg(long)]
    auth_jwt_issuer: Option<String>,

    /// Required JWT audience (`aud`), if set.
    #[arg(long)]
    auth_jwt_audience: Option<String>,

    /// JWT claim that carries the tenant id.
    #[arg(long, default_value = "tenant")]
    auth_jwt_tenant_claim: String,

    /// Address to bind the worker-facing gRPC listener on. Defaults to
    /// `--listen` (single-port mode) when no `--tls-client-ca` is set, or
    /// `listen+1` otherwise. Ignored when `--single-port` is set.
    ///
    /// The worker listener serves `WorkerService` only, and (when mTLS is
    /// configured) requires a client TLS certificate. Splitting the worker
    /// plane onto its own port is required by issue #139: the worker's
    /// `batch_update_blobs` writes hit the same `ContentAddressableStorage`
    /// backend as client traffic, and that backend is JWT-gated, so workers
    /// must reach it via an mTLS-authenticated connection instead of a
    /// tokenless one.
    #[arg(long)]
    worker_listen: Option<SocketAddr>,

    /// Bind client-facing services and `WorkerService` on the *same* port
    /// (`--listen`). Dev-only. Incompatible with `--auth-jwt-*`: if JWT
    /// auth is on, the worker would share the JWT-gated listener and
    /// every worker→CAS write would be rejected with `UNAUTHENTICATED`
    /// (issue #139). The control plane refuses to start in that
    /// combination; see [`Args::validate_auth_flags`].
    #[arg(long, default_value_t = false)]
    single_port: bool,

    /// EXPERIMENTAL (Phase 5 I8c): replicate control-plane metadata (the
    /// action cache; cluster config to follow) through the embedded
    /// from-scratch Raft instead of plain redb. Runs a single-voter Raft on
    /// this node — writes are committed-and-applied log entries, reads are
    /// ReadIndex-linearizable, and the log + snapshots live in
    /// `data_dir/raft.redb`. With `--raft-peer`s configured this node joins
    /// a multi-node HA cluster (I9). Off by default: exactly the
    /// single-node redb behavior.
    #[arg(long, default_value_t = false)]
    raft: bool,

    /// Stable Raft identity of THIS control plane (I9). Every node in a
    /// cluster needs a distinct id, consistent across restarts.
    #[arg(long, default_value = "control-0")]
    node_id: String,

    /// A Raft peer as `id=host:port` (repeatable), naming every OTHER
    /// member of the control-plane cluster. Requires `--raft` and
    /// `--raft-listen`. All members must agree on the full membership.
    #[arg(long = "raft-peer")]
    raft_peers: Vec<String>,

    /// Address to serve `brokkr.v1.RaftService` on for peer traffic (I9).
    /// Plaintext in this phase — run peer links on a trusted network;
    /// mTLS for the raft plane is a noted follow-up.
    #[arg(long)]
    raft_listen: Option<SocketAddr>,

    /// The client-plane address to advertise to the cluster (I9b), as
    /// `host:port`. When this node is the Raft leader it publishes this
    /// address under `cfg/nodes/<node-id>`, so every replica can turn a
    /// leader *id* into something a client can actually dial: a follower
    /// refusing a metadata write answers `FAILED_PRECONDITION` with both
    /// `x-brokkr-leader` and `x-brokkr-leader-addr`.
    ///
    /// Defaults to `--listen`. A wildcard bind (`0.0.0.0` / `::`) cannot be
    /// advertised — it is not reachable — so with a wildcard `--listen` this
    /// flag is required whenever `--raft` is on.
    #[arg(long)]
    advertise_addr: Option<String>,

    /// This node's certificate for the **Raft peer plane** (I9d, ADR 0011).
    ///
    /// The peer plane is mutual-TLS-only: a peer is not a tenant and carries
    /// no user identity, so there is no JWT here — but `AppendEntries` on this
    /// plane appends to the replicated log, which makes an unauthenticated
    /// peer port a write path into consensus itself.
    ///
    /// All three of `--raft-tls-cert`, `--raft-tls-key` and `--raft-tls-ca`
    /// must be given together; they configure both the `--raft-listen` server
    /// and the outbound peer channels, because a one-sided configuration
    /// fails at handshake time instead of at startup.
    #[arg(long, requires_all = ["raft_tls_key", "raft_tls_ca"])]
    raft_tls_cert: Option<PathBuf>,

    /// Private key for [`Self::raft_tls_cert`] (I9d).
    #[arg(long, requires_all = ["raft_tls_cert", "raft_tls_ca"])]
    raft_tls_key: Option<PathBuf>,

    /// CA that verifies peer certificates on the Raft plane (I9d). Both
    /// directions verify against it: inbound peers must present a certificate
    /// it signs, and outbound channels verify the peer's server certificate.
    #[arg(long, requires_all = ["raft_tls_cert", "raft_tls_key"])]
    raft_tls_ca: Option<PathBuf>,

    /// Path to a WebAssembly scheduling policy (ADR 0014).
    ///
    /// The module decides which of the eligible, idle, connected workers gets
    /// each job. It is *operator-supplied* — a file you place next to the
    /// binary, inside the trust boundary you already own — not something
    /// tenants can upload.
    ///
    /// Without this flag the built-in `SimpleFifo` strategy is used and no
    /// WASM engine is started. With it, any decision the policy declines or
    /// fails is served by `SimpleFifo` anyway, so a broken policy degrades
    /// placement quality rather than stopping the cluster. See
    /// `docs/operations/writing-a-scheduling-policy.md`.
    #[arg(long)]
    policy_wasm: Option<PathBuf>,

    /// Fuel granted to each policy decision — a bound on *work*.
    ///
    /// Catches an accidental O(n²) in a policy. It cannot bound *time*; that
    /// is what `--policy-deadline-ms` is for.
    #[arg(long, default_value_t = brokkr_policy::DEFAULT_FUEL)]
    policy_fuel: u64,

    /// Wall-clock budget for each policy decision, in milliseconds.
    ///
    /// The bound that actually matters: this call happens while the
    /// scheduler's dispatch mutex is held, so a policy that stalls stalls
    /// placement for the whole cluster.
    #[arg(long, default_value_t = brokkr_policy::DEFAULT_DEADLINE_MS)]
    policy_deadline_ms: u64,

    /// Consecutive policy failures before the module is quarantined.
    ///
    /// Falling back per decision is not enough on its own: a policy that fails
    /// every call would burn its full deadline forever. Past this count the
    /// guest stops being called at all until the module is reloaded.
    #[arg(long, default_value_t = brokkr_policy::QUARANTINE_THRESHOLD)]
    policy_quarantine_threshold: u32,

    /// Completions remembered per worker for the locality signal (ADR 0014).
    ///
    /// Deeper means a repeated build's input root stays visible after more
    /// unrelated work interleaves, at a linear cost in memory and in the scan
    /// per candidate.
    #[arg(long, default_value_t = brokkr_control::locality::DEFAULT_WINDOW)]
    policy_locality_window: usize,

    /// How often `--policy-wasm` is checked for changes, in seconds.
    ///
    /// Editing the file swaps the policy with no restart — that iteration loop
    /// is most of why the hook exists. A module that fails validation never
    /// becomes live, so a bad edit costs a log line rather than the scheduler.
    /// `0` disables reloading.
    #[arg(long, default_value_t = brokkr_control::policy_reload::DEFAULT_RELOAD_INTERVAL.as_secs())]
    policy_reload_interval_secs: u64,

    /// Bind address for the operator observability listener (ADR 0012).
    ///
    /// Deliberately **not** the tenant-facing `--listen` port. ADR 0011's auth
    /// resolves a token to a tenant and has no scope concept, so a tenant
    /// reaching this service could enumerate every worker and every other
    /// tenant's jobs. Defaults to loopback; a non-loopback bind requires
    /// operator mTLS or an explicit opt-out.
    #[arg(long, default_value = "127.0.0.1:7880")]
    observe_listen: SocketAddr,

    /// Server certificate for the operator listener. All three of
    /// `--observe-tls-cert`, `--observe-tls-key` and `--observe-tls-ca` must
    /// be given together; the CA is what authorizes callers.
    #[arg(long, requires_all = ["observe_tls_key", "observe_tls_ca"])]
    observe_tls_cert: Option<PathBuf>,

    /// Private key for [`Args::observe_tls_cert`].
    #[arg(long, requires_all = ["observe_tls_cert", "observe_tls_ca"])]
    observe_tls_key: Option<PathBuf>,

    /// CA that verifies operator client certificates.
    #[arg(long, requires_all = ["observe_tls_cert", "observe_tls_key"])]
    observe_tls_ca: Option<PathBuf>,

    /// Permit a non-loopback `--observe-listen` with no operator mTLS.
    ///
    /// For an already-isolated network. A flag someone had to type is a
    /// decision; the same bind reached by accident is an incident.
    #[arg(long)]
    observe_allow_insecure_bind: bool,
}

/// Build the scheduling strategy from the policy flags (ADR 0014).
///
/// Returns `None` when no `--policy-wasm` was given, meaning "use the built-in
/// `SimpleFifo`" — the default, and the only behaviour before Phase 6.
///
/// Every failure here is a **startup error**, deliberately, even though a
/// failure at *decision* time degrades instead. The two are not in tension: an
/// operator who names a policy file has stated an intent, and silently running
/// `SimpleFifo` because the file was misspelled would be exactly the kind of
/// quiet misconfiguration issue #139 established we do not ship. A module that
/// loads and later misbehaves is a different situation — the cluster is
/// running, jobs are queued, and degrading beats stopping.
fn build_policy_strategy(
    args: &Args,
    registry: SharedWorkerRegistry,
) -> Result<Option<Arc<WasmStrategy>>> {
    let Some(path) = args.policy_wasm.as_ref() else {
        return Ok(None);
    };
    let limits = PolicyLimits {
        fuel: args.policy_fuel,
        deadline_ms: args.policy_deadline_ms,
        quarantine_threshold: args.policy_quarantine_threshold,
    };
    let wasm = std::fs::read(path)
        .with_context(|| format!("reading the scheduling policy {}", path.display()))?;
    let engine = PolicyEngine::new(limits)
        .with_context(|| "starting the scheduling-policy engine".to_string())?;
    let strategy = WasmStrategy::new(engine, Some(registry));
    strategy
        .load(&wasm)
        .with_context(|| format!("loading the scheduling policy {}", path.display()))?;
    tracing::info!(
        policy = %path.display(),
        fuel = limits.fuel,
        deadline_ms = limits.deadline_ms,
        quarantine_threshold = limits.quarantine_threshold,
        "WASM scheduling policy loaded"
    );
    Ok(Some(Arc::new(strategy)))
}

/// The Raft peer-plane server, built inside the Raft branch but spawned later.
///
/// `PeerObservability` (ADR 0012) shares this listener, and its dependencies —
/// the scheduler in particular — do not exist until after the Raft branch has
/// produced the action cache. So the pieces are captured here and the listener
/// is spawned once both services can be mounted on it. Splitting them across
/// two ports would mean a second address to configure and firewall for no
/// reason.
type RaftPeerServerParts = (
    brokkr_proto::brokkr_v1::raft_service_server::RaftServiceServer<
        brokkr_raft::RaftServiceAdapter<brokkr_raft::RaftHandle>,
    >,
    Option<ServerTlsConfig>,
    SocketAddr,
);

/// Refuse an unauthenticated observability listener on a routable address.
///
/// ADR 0012's argument is that *the listener is the boundary* — but a listener
/// bound to `0.0.0.0` with no authentication is not a boundary, it is an
/// unauthenticated read of the whole cluster offered to the network. Loopback
/// needs nothing further; anything else needs mTLS or an explicit override.
///
/// Pure over its three inputs so every combination is testable without binding
/// a socket. Same posture issue #139 established for the other planes: a
/// misconfiguration is a startup error, never a runtime surprise.
fn validate_observe_bind(
    listen: SocketAddr,
    mtls_configured: bool,
    allow_insecure: bool,
) -> Result<(), String> {
    if listen.ip().is_loopback() || mtls_configured || allow_insecure {
        return Ok(());
    }
    Err(format!(
        "--observe-listen ({listen}) is not a loopback address and no operator \
         mTLS is configured, which would serve unauthenticated cluster state to \
         the network. Either pass --observe-tls-cert / --observe-tls-key / \
         --observe-tls-ca, or bind loopback and reach it over SSH. If the \
         network is already isolated, --observe-allow-insecure-bind says so \
         deliberately."
    ))
}

/// The Raft peer plane's transport security, resolved from the flags (I9d).
#[derive(Debug, Clone, PartialEq, Eq)]
enum RaftPlaneTls {
    /// Plaintext — development only; documented as such in
    /// `docs/operations/running-a-cluster.md`.
    Disabled,
    /// Mutual TLS: this node's identity plus the CA that verifies peers.
    Enabled {
        cert: PathBuf,
        key: PathBuf,
        ca: PathBuf,
    },
}

/// Validate the Raft-plane TLS flags as a set (I9d).
///
/// A pure function over the four inputs so every half-configuration is
/// testable without binding a socket — which matters because the failure this
/// guards against is *silent until a peer first tries to replicate*. Issue
/// #139 established the posture for the client and worker planes: a
/// misconfiguration is a startup error, never a runtime surprise.
fn resolve_raft_tls(
    raft: bool,
    cert: Option<&PathBuf>,
    key: Option<&PathBuf>,
    ca: Option<&PathBuf>,
) -> Result<RaftPlaneTls> {
    match (cert, key, ca) {
        (None, None, None) => Ok(RaftPlaneTls::Disabled),
        (Some(cert), Some(key), Some(ca)) => {
            if !raft {
                anyhow::bail!(
                    "--raft-tls-cert/--raft-tls-key/--raft-tls-ca configure the Raft peer \
                     plane but --raft is off; enable --raft or drop the TLS flags"
                );
            }
            Ok(RaftPlaneTls::Enabled {
                cert: cert.clone(),
                key: key.clone(),
                ca: ca.clone(),
            })
        }
        // clap's `requires_all` already rejects most partial sets; this arm
        // catches the rest and, more importantly, states the rule in one
        // place rather than trusting an attribute to be the whole contract.
        _ => {
            let missing = [
                ("--raft-tls-cert", cert.is_none()),
                ("--raft-tls-key", key.is_none()),
                ("--raft-tls-ca", ca.is_none()),
            ]
            .into_iter()
            .filter(|(_, absent)| *absent)
            .map(|(flag, _)| flag)
            .collect::<Vec<_>>()
            .join(", ");
            anyhow::bail!(
                "the Raft peer plane is half-configured: missing {missing}. Pass all of \
                 --raft-tls-cert, --raft-tls-key and --raft-tls-ca, or none of them \
                 (plaintext, development only)"
            )
        }
    }
}

/// One parsed `--raft-peer id=host:port`.
struct RaftPeer {
    id: String,
    addr: String,
}

impl Args {
    /// Build the base server TLS configuration: identity (server cert+key)
    /// plus the client CA root, if `--tls-client-ca` is set. The
    /// per-port `client_auth_optional` posture is applied on top of
    /// this by [`Args::tls_config`].
    fn base_tls_config_opt(&self) -> Result<Option<ServerTlsConfig>> {
        let has_server_cert = self.tls_cert.is_some();
        let has_server_key = self.tls_key.is_some();
        let has_client_ca = self.tls_client_ca.is_some();

        // Reject inconsistent combinations.
        if has_client_ca && (!has_server_cert || !has_server_key) {
            anyhow::bail!("--tls-client-ca requires --tls-cert and --tls-key to be provided");
        }

        let (Some(tls_cert), Some(tls_key)) = (&self.tls_cert, &self.tls_key) else {
            return Ok(None);
        };
        let cert_pem = std::fs::read(tls_cert).context("reading tls_cert")?;
        let key_pem = std::fs::read(tls_key).context("reading tls_key")?;
        let identity = tonic::transport::Identity::from_pem(cert_pem, key_pem);

        let mut cfg = ServerTlsConfig::new().identity(identity);

        // mTLS requires client CA root.
        if let Some(ca_path) = &self.tls_client_ca {
            let ca_pem = std::fs::read(ca_path).context("reading tls_client_ca")?;
            cfg = cfg.client_ca_root(tonic::transport::Certificate::from_pem(ca_pem));
        } else {
            tracing::warn!("starting without client certificate verification (mTLS disabled)");
        }

        Ok(Some(cfg))
    }

    /// Per-port TLS configuration.
    ///
    /// * `worker_port = true` (the `WorkerService` listener): when
    ///   `--tls-client-ca` is set, the server **requires** a client
    ///   certificate signed by that CA (tonic 0.12's `client_auth_optional`
    ///   defaults to `false` after `client_ca_root`). The worker is
    ///   authenticated at the transport layer (ADR 0011) — the worker
    ///   port carries no JWT interceptor.
    /// * `worker_port = false` (the client-facing listener): when
    ///   `--tls-client-ca` is set, we mark the cert *optional* on the
    ///   client port — the JWT `auth_interceptor` is the authoritative
    ///   auth boundary for client traffic, and a client cert is just an
    ///   additional identity hint we may use later (cert-CN binding is
    ///   a deferred ADR-0011 follow-up). Without a CA, both ports
    ///   behave as plain mTLS-ready-but-not-enforced.
    fn tls_config(&self, worker_port: bool) -> Result<Option<ServerTlsConfig>> {
        let Some(base) = self.base_tls_config_opt()? else {
            return Ok(None);
        };
        let has_client_ca = self.tls_client_ca.is_some();
        let cfg = match (worker_port, has_client_ca) {
            (true, true) => base, // require cert (tonic default after client_ca_root)
            (true, false) => {
                // Defensive: validate_auth_flags() should have refused this
                // combination, but if we get here, log and proceed without
                // requiring a cert (the worker port would otherwise be
                // unable to accept any connection at all).
                tracing::warn!(
                    "worker port bound without a client CA; WorkerService is unauthenticated \
                     (this should have been refused at startup by validate_auth_flags)"
                );
                base
            }
            (false, true) => base.client_auth_optional(true),
            (false, false) => base,
        };
        Ok(Some(cfg))
    }

    /// TLS for the operator observability listener (ADR 0012).
    ///
    /// Independent of the client and worker planes: an operator console and a
    /// tenant are different audiences, and reusing the client CA here would
    /// mean any tenant certificate authorized cluster-wide reads.
    fn observe_tls_config(&self) -> Result<Option<ServerTlsConfig>> {
        let (Some(cert), Some(key), Some(ca)) = (
            self.observe_tls_cert.as_ref(),
            self.observe_tls_key.as_ref(),
            self.observe_tls_ca.as_ref(),
        ) else {
            return Ok(None);
        };
        let cert_pem = std::fs::read(cert)
            .with_context(|| format!("reading --observe-tls-cert {}", cert.display()))?;
        let key_pem = std::fs::read(key)
            .with_context(|| format!("reading --observe-tls-key {}", key.display()))?;
        let ca_pem = std::fs::read(ca)
            .with_context(|| format!("reading --observe-tls-ca {}", ca.display()))?;
        Ok(Some(
            ServerTlsConfig::new()
                .identity(tonic::transport::Identity::from_pem(cert_pem, key_pem))
                .client_ca_root(tonic::transport::Certificate::from_pem(ca_pem)),
        ))
    }

    /// Build the client [`Authenticator`] from the `--auth-jwt-*` arguments.
    /// With no key source configured this is `Disabled` (open mode); the caller
    /// warns loudly in that case.
    fn authenticator(&self) -> Result<Authenticator> {
        let claim = self.auth_jwt_tenant_claim.clone();
        let jwt = match (&self.auth_jwt_hmac_secret_file, &self.auth_jwt_rsa_pem_file) {
            (Some(_), Some(_)) => anyhow::bail!(
                "provide only one of --auth-jwt-hmac-secret-file / --auth-jwt-rsa-pem-file"
            ),
            (Some(path), None) => {
                let secret = std::fs::read(path).context("reading auth HMAC secret file")?;
                Some(JwtAuth::hmac(&secret, claim))
            }
            (None, Some(path)) => {
                let pem = std::fs::read(path).context("reading auth RSA PEM file")?;
                Some(JwtAuth::rsa_pem(&pem, claim).map_err(|e| anyhow::anyhow!("{e}"))?)
            }
            (None, None) => None,
        };
        let auth = match jwt {
            Some(mut jwt) => {
                if let Some(iss) = &self.auth_jwt_issuer {
                    jwt = jwt.with_issuer(iss);
                }
                if let Some(aud) = &self.auth_jwt_audience {
                    jwt = jwt.with_audience(aud);
                }
                Authenticator::Jwt(Box::new(jwt))
            }
            None => Authenticator::Disabled,
        };
        Ok(auth)
    }

    /// Parses and validates the I9 Raft cluster flags: every peer is
    /// `id=host:port`, ids are unique and distinct from `--node-id`, and
    /// peers require both `--raft` and `--raft-listen`.
    fn raft_peers(&self) -> Result<Vec<RaftPeer>> {
        let mut peers = Vec::new();
        for spec in &self.raft_peers {
            let Some((id, addr)) = spec.split_once('=') else {
                anyhow::bail!("--raft-peer must be id=host:port, got {spec:?}");
            };
            if id.is_empty() || addr.is_empty() {
                anyhow::bail!("--raft-peer must be id=host:port, got {spec:?}");
            }
            if id == self.node_id {
                anyhow::bail!("--raft-peer {id} collides with our own --node-id");
            }
            if peers.iter().any(|p: &RaftPeer| p.id == id) {
                anyhow::bail!("duplicate --raft-peer id {id}");
            }
            peers.push(RaftPeer {
                id: id.to_string(),
                addr: addr.to_string(),
            });
        }
        if !peers.is_empty() {
            if !self.raft {
                anyhow::bail!("--raft-peer requires --raft");
            }
            if self.raft_listen.is_none() {
                anyhow::bail!("--raft-peer requires --raft-listen so peers can reach this node");
            }
        }
        Ok(peers)
    }

    /// The client-plane address this node advertises to the cluster (I9b).
    ///
    /// `--advertise-addr` when set, else `--listen`. A wildcard bind is
    /// refused rather than published: `0.0.0.0:7878` is a *binding*
    /// instruction, not an address a peer or client can dial, and publishing
    /// it would hand every redirected client a hint guaranteed to fail. The
    /// check only fires under `--raft`, so single-node operators keep binding
    /// wildcards without having to name themselves.
    fn resolved_advertise_addr(&self) -> Result<String> {
        if let Some(addr) = &self.advertise_addr {
            if addr.trim().is_empty() {
                anyhow::bail!("--advertise-addr must not be empty");
            }
            return Ok(addr.clone());
        }
        if self.raft && self.listen.ip().is_unspecified() {
            anyhow::bail!(
                "--listen is a wildcard address ({}) which cannot be advertised to the \
                 cluster; pass --advertise-addr <host:port> naming how peers and clients \
                 reach this node",
                self.listen
            );
        }
        Ok(self.listen.to_string())
    }

    /// Reject flag combinations that would silently mis-configure auth
    /// (issue #139). Runs before any listener is bound so the operator
    /// gets a clean startup error instead of a runtime `UNAUTHENTICATED`
    /// on every action.
    ///
    /// The cases we refuse, in order (each with its own actionable message):
    ///
    /// 1. `--single-port` + `--auth-jwt-*`. With single-port mode,
    ///    `WorkerService` shares the JWT-gated listener, so worker→CAS
    ///    writes are rejected. There is no fix other than splitting the
    ///    listener (which `--single-port` opts out of).
    /// 2. `--auth-jwt-*` + no `--tls-client-ca`. Without a client CA the
    ///    worker port can't authenticate callers, so the worker→CAS path
    ///    is again unreachable. The operator must either configure mTLS
    ///    or drop auth (and accept single-port mode).
    /// 3. `--auth-jwt-*` + a worker listener that *resolves* to the same
    ///    address as `--listen` — an explicit matching `--worker-listen`,
    ///    an IPv6 `--listen` (the port-bump heuristic is v4-only), or a
    ///    port already at 65535. This is single-port mode in everything
    ///    but the flag, and would mis-configure auth exactly like case 1.
    fn validate_auth_flags(&self) -> Result<()> {
        let auth_on = self.authenticator()?.is_enabled();
        if !auth_on {
            return Ok(());
        }
        if self.single_port {
            anyhow::bail!(
                "--single-port and --auth-jwt-* are incompatible (issue #139): \
                 WorkerService would share the JWT-gated listener and every \
                 worker->CAS write would be rejected with UNAUTHENTICATED; \
                 either drop --single-port so the worker port can use mTLS, \
                 or drop --auth-jwt-* for local-dev single-port mode"
            );
        }
        if self.tls_client_ca.is_none() {
            anyhow::bail!(
                "--auth-jwt-* requires --tls-client-ca so WorkerService can be \
                 authenticated by mTLS on the worker port (issue #139); \
                 alternatively pass --single-port with --auth-jwt-* disabled for \
                 local dev"
            );
        }
        if self.effective_single_port() {
            anyhow::bail!(
                "--auth-jwt-* is enabled but the worker listener resolves to the \
                 same address as --listen ({}), which is single-port mode in \
                 everything but the flag (issue #139): WorkerService would share \
                 the JWT-gated listener and every worker->CAS write would be \
                 rejected with UNAUTHENTICATED; pass a --worker-listen distinct \
                 from --listen (the automatic port bump only applies to IPv4 \
                 addresses below port 65535)",
                self.listen
            );
        }
        Ok(())
    }

    /// Whether the server will effectively run one shared listener: the
    /// explicit `--single-port` flag, or a worker address that resolves to
    /// `--listen` itself (no client CA, an IPv6 `--listen`, an explicit
    /// matching `--worker-listen`, or port saturation at 65535). This is the
    /// single source of truth for both startup validation and listener
    /// binding in `main`.
    fn effective_single_port(&self) -> bool {
        self.single_port || self.resolved_worker_listen() == self.listen
    }

    /// The address the worker listener should bind to. Resolution:
    ///
    /// * `--worker-listen` if explicitly set, else
    /// * `--listen` itself (single-port mode), else
    /// * `port_one_above(--listen)` (split mode with mTLS).
    fn resolved_worker_listen(&self) -> SocketAddr {
        if let Some(addr) = self.worker_listen {
            return addr;
        }
        if self.tls_client_ca.is_none() {
            return self.listen;
        }
        let SocketAddr::V4(v4) = self.listen else {
            // IPv6 / non-v4 is out of scope for the port-bump heuristic.
            return self.listen;
        };
        let mut bumped = v4;
        bumped.set_port(v4.port().saturating_add(1));
        SocketAddr::V4(bumped)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    // Reject unsafe auth/TLS combinations up front (issue #139).
    args.validate_auth_flags()?;
    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("creating data dir {:?}", args.data_dir))?;
    let cas =
        Arc::new(RedbCas::open(args.data_dir.join("cas.redb")).context("opening CAS database")?);
    // Durable control-plane metadata goes through the MetaKv seam (I8): the
    // action cache lives in `meta.redb` under the `ac/` namespace. Swapping
    // `RedbMetaKv` for `RaftKv` (I8c) is what makes this state survive a
    // leader kill; nothing downstream of the trait changes.
    let raft_peers = args.raft_peers()?;
    let advertise_addr = args.resolved_advertise_addr()?;
    let raft_tls = resolve_raft_tls(
        args.raft,
        args.raft_tls_cert.as_ref(),
        args.raft_tls_key.as_ref(),
        args.raft_tls_ca.as_ref(),
    )?;
    // Captured out of the Raft branch so the observability service can report
    // this node's role, term and commit index (ADR 0012). `None` with `--raft`
    // off, in which case the node reports itself as a single member of no
    // claimed role.
    let mut raft_handle_for_observability: Option<Arc<brokkr_raft::RaftHandle>> = None;
    let mut raft_peer_parts: Option<RaftPeerServerParts> = None;
    let action_cache: Arc<dyn brokkr_cas::ActionCache> = if args.raft {
        // I8c/I9: Raft-replicated metadata. With no peers this is a
        // single-voter cluster; with `--raft-peer`s it is a real HA cluster
        // — the log + snapshots are rebuilt on restart (restore + tail
        // replay), and peers reach us via `--raft-listen`.
        let raft_log = brokkr_raft::RaftLog::open(args.data_dir.join("raft.redb"))
            .map_err(|e| anyhow::anyhow!("opening raft log: {e}"))?;
        let node_id = brokkr_raft::NodeId::new(&args.node_id)
            .map_err(|e| anyhow::anyhow!("raft node id: {e}"))?;

        let mut transport = brokkr_raft::TonicTransport::new();
        let mut peer_ids = Vec::new();
        for peer in &raft_peers {
            let peer_id = brokkr_raft::NodeId::new(&peer.id)
                .map_err(|e| anyhow::anyhow!("raft peer id {}: {e}", peer.id))?;
            // Keepalive is load-bearing, not decoration (the I5c lesson): a
            // partition that drops packets silently leaves the h2 connection
            // "alive" forever, and without keepalive the healed cluster can
            // never re-integrate this peer.
            // Scheme follows the plane's security: an https endpoint with no
            // TLS config (or the reverse) fails at handshake time, which is
            // exactly the late failure `resolve_raft_tls` exists to prevent.
            let scheme = match &raft_tls {
                RaftPlaneTls::Enabled { .. } => "https",
                RaftPlaneTls::Disabled => "http",
            };
            let mut endpoint =
                tonic::transport::Endpoint::from_shared(format!("{scheme}://{}", peer.addr))
                    .with_context(|| format!("raft peer endpoint {}", peer.addr))?
                    .connect_timeout(Duration::from_millis(500))
                    .timeout(Duration::from_secs(1))
                    .http2_keep_alive_interval(Duration::from_millis(500))
                    .keep_alive_timeout(Duration::from_millis(500))
                    .keep_alive_while_idle(true);
            if let RaftPlaneTls::Enabled { cert, key, ca } = &raft_tls {
                // Same plumbing as the worker plane, not a second code path:
                // a parallel TLS implementation is how the half-configured
                // states issue #139 closed get reintroduced.
                let ca_pem = std::fs::read(ca).context("reading --raft-tls-ca")?;
                let cert_pem = std::fs::read(cert).context("reading --raft-tls-cert")?;
                let key_pem = std::fs::read(key).context("reading --raft-tls-key")?;
                let tls = tonic::transport::ClientTlsConfig::new()
                    .ca_certificate(tonic::transport::Certificate::from_pem(ca_pem))
                    .identity(tonic::transport::Identity::from_pem(cert_pem, key_pem));
                endpoint = endpoint
                    .tls_config(tls)
                    .context("raft peer TLS configuration")?;
            }
            let channel = endpoint.connect_lazy();
            transport.insert_peer(peer_id.clone(), channel);
            peer_ids.push(peer_id);
        }

        // Distinct per-node seeds keep election timeouts de-synchronized;
        // identical seeds would make every node campaign in lockstep.
        let seed = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            args.node_id.hash(&mut hasher);
            hasher.finish()
        };
        let node = brokkr_raft::RaftNode::new(
            node_id,
            peer_ids,
            raft_log,
            brokkr_raft::Rng::seed_from_u64(seed),
            brokkr_raft::Config::default(),
            std::time::Instant::now(),
        )
        .map_err(|e| anyhow::anyhow!("initializing raft node: {e}"))?;
        let machine = brokkr_control::KvMachine::default();
        let shared = machine.shared();
        let (driver, handle) = brokkr_raft::RaftDriver::new(
            node,
            Box::new(machine),
            Arc::new(transport),
            Duration::from_millis(50),
        );
        tokio::spawn(async move {
            if let Err(e) = driver.run().await {
                tracing::error!(error = %e, "raft driver exited");
            }
        });

        // Serve RaftService for peers (I9). Plaintext: trusted network only.
        if let Some(raft_addr) = args.raft_listen {
            let adapter = brokkr_raft::RaftServiceAdapter::new(Arc::new(handle.clone()));
            let peer_tls = match &raft_tls {
                RaftPlaneTls::Disabled => None,
                RaftPlaneTls::Enabled { cert, key, ca } => {
                    let cert_pem = std::fs::read(cert).context("reading --raft-tls-cert")?;
                    let key_pem = std::fs::read(key).context("reading --raft-tls-key")?;
                    let ca_pem = std::fs::read(ca).context("reading --raft-tls-ca")?;
                    // `client_ca_root` makes the client certificate mandatory
                    // in tonic 0.12 — the peer plane is mutual-only, so an
                    // unauthenticated peer must not be able to append to the
                    // replicated log.
                    Some(
                        ServerTlsConfig::new()
                            .identity(tonic::transport::Identity::from_pem(cert_pem, key_pem))
                            .client_ca_root(tonic::transport::Certificate::from_pem(ca_pem)),
                    )
                }
            };
            // Captured rather than spawned here: `PeerObservability` shares
            // this listener and its dependencies do not exist yet.
            raft_peer_parts = Some((adapter.into_server(), peer_tls, raft_addr));
            match &raft_tls {
                RaftPlaneTls::Enabled { .. } => tracing::info!(
                    raft_addr = %raft_addr, node_id = %args.node_id,
                    peers = raft_peers.len(), "raft peer plane listening (mTLS)"
                ),
                RaftPlaneTls::Disabled => tracing::warn!(
                    raft_addr = %raft_addr, node_id = %args.node_id,
                    peers = raft_peers.len(),
                    "RAFT PEER PLANE IS PLAINTEXT — anyone who can reach this port can \
                     append to the replicated log. Development only; pass \
                     --raft-tls-cert/--raft-tls-key/--raft-tls-ca in production"
                ),
            }
        }
        if raft_peers.is_empty() {
            tracing::warn!("METADATA VIA RAFT (single-voter) — experimental");
        } else {
            tracing::warn!(
                peers = raft_peers.len(),
                "METADATA VIA RAFT (HA cluster, I9) — experimental"
            );
        }
        raft_handle_for_observability = Some(Arc::new(handle.clone()));
        let raft_kv = Arc::new(brokkr_control::RaftKv::new(handle.clone(), shared));

        // Publish this node's client-plane address whenever we hold
        // leadership (I9b). Only the leader can propose, and the leader's
        // address is the only one a redirect ever needs, so leadership *is*
        // the trigger — a follower has nothing to publish and no way to
        // publish it. `publish_node_record` is idempotent, so a stable leader
        // writes one entry per term, not one per tick.
        let publisher_kv = Arc::clone(&raft_kv);
        let publisher_id = args.node_id.clone();
        let publisher_addr = advertise_addr.clone();
        tokio::spawn(async move {
            loop {
                // A step-down clears `was_leader`, so regaining leadership
                // republishes — the record may have been truncated by the
                // term that deposed us.
                if matches!(publisher_kv.is_leader().await, Ok(true)) {
                    match publisher_kv
                        .publish_node_record(&publisher_id, &publisher_addr)
                        .await
                    {
                        Ok(()) => {}
                        // Losing leadership mid-propose is ordinary, not an
                        // error worth shouting about: the next leader
                        // publishes its own address.
                        Err(brokkr_control::MetaKvError::NotLeader { .. }) => {}
                        Err(e) => tracing::warn!(
                            error = %e,
                            node_id = %publisher_id,
                            "could not publish this node's advertise address; \
                             redirects to this node will carry an id but no address"
                        ),
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        tracing::info!(
            node_id = %args.node_id,
            advertise_addr = %advertise_addr,
            "publishing this node's client-plane address while leader"
        );

        Arc::new(MetaKvActionCache::new(raft_kv))
    } else {
        let meta_kv = Arc::new(
            RedbMetaKv::open(args.data_dir.join("meta.redb"))
                .context("opening metadata database")?,
        );
        Arc::new(MetaKvActionCache::new(meta_kv))
    };
    // Pre-I8 deployments kept the action cache in its own database. It is no
    // longer read — say so, out loud, instead of leaving operators to wonder
    // which redb file is live (and to explain the post-upgrade cold cache).
    let legacy_ac = args.data_dir.join("action_cache.redb");
    if legacy_ac.exists() {
        tracing::warn!(
            path = %legacy_ac.display(),
            "legacy action-cache database is no longer read (the action cache \
             now lives in meta.redb); the file can be deleted"
        );
    }
    // One shared worker registry, three users: the scheduler reads it for
    // platform-constraint admission control, the worker service writes
    // registrations / heartbeats into it, and the eviction reaper prunes
    // stale entries.
    let worker_registry: SharedWorkerRegistry =
        Arc::new(tokio::sync::Mutex::new(WorkerRegistry::default()));
    let policy_strategy = build_policy_strategy(&args, worker_registry.clone())?;
    if let (Some(strategy), Some(path)) = (policy_strategy.clone(), args.policy_wasm.clone()) {
        let interval = Duration::from_secs(args.policy_reload_interval_secs);
        spawn_policy_reloader(strategy, path, interval);
    }
    let scheduler = match policy_strategy.clone() {
        Some(strategy) => Scheduler::with_strategy(
            cas.clone(),
            action_cache.clone(),
            worker_registry.clone(),
            strategy,
        ),
        None => Scheduler::with_worker_registry(
            cas.clone(),
            action_cache.clone(),
            worker_registry.clone(),
        ),
    };
    // Cloned before the listener tasks move their copies; the observability
    // service reads per-worker in-flight counts from it.
    let scheduler_for_observability = scheduler.clone();

    let client_tls_cfg = args.tls_config(false)?;
    let worker_tls_cfg = args.tls_config(true)?;
    match (&client_tls_cfg, &worker_tls_cfg) {
        (Some(_), _) | (None, Some(_)) => {
            tracing::warn!("TLS ENABLED — mTLS required for production");
        }
        (None, None) => {
            tracing::warn!("TLS DISABLED — NOT FOR PRODUCTION USE");
        }
    }

    // Client authentication (ADR 0011). Open mode (no JWT key configured)
    // warns loudly, like the TLS-disabled posture.
    let auth = Arc::new(args.authenticator()?);
    if auth.is_enabled() {
        tracing::warn!("CLIENT AUTH ENABLED — JWT bearer required on client RPCs");
    } else {
        tracing::warn!("CLIENT AUTH DISABLED — NOT FOR PRODUCTION USE");
    }

    let worker_listen_addr = args.resolved_worker_listen();
    let single_port = args.effective_single_port();
    if single_port {
        tracing::info!(
            client_addr = %args.listen,
            "single-port mode: WorkerService shares the client listener (dev-only, no auth)"
        );
    } else {
        tracing::info!(
            client_addr = %args.listen,
            worker_addr = %worker_listen_addr,
            "split-port mode: client-facing services and WorkerService on separate listeners"
        );
    }
    tracing::info!(data_dir = ?args.data_dir, "brokkr-control starting");

    let worker_service =
        WorkerServiceImpl::with_registry(scheduler.clone(), worker_registry.clone());
    // Background liveness reaper: evict workers that stop heartbeating. Held
    // for the server's lifetime; aborting it on shutdown is implicit (process
    // exit). The eviction decision lives in `WorkerRegistry::evict_stale`.
    let _eviction = spawn_eviction_task(worker_registry.clone());
    // Background lease reaper: reassign jobs whose lease expired (a connected
    // but silent worker). Checked at a fraction of the lease window.
    let reap_interval = (scheduler.lease_duration() / 2).max(Duration::from_secs(1));
    let _lease_reaper = spawn_lease_reaper(scheduler.clone(), reap_interval);

    // Bind the client-facing listener. Carries CAS / ActionCache / Capabilities
    // / Execution (all gated by `auth_interceptor` when JWT auth is on). In
    // single-port mode it also carries `WorkerService`.
    let client_listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding client listener on {}", args.listen))?;
    let mut client_server = Server::builder();
    if let Some(cfg) = client_tls_cfg {
        client_server = client_server.tls_config(cfg)?;
    }
    let auth_for_client = auth.clone();
    let action_cache_for_client = action_cache.clone();
    let cas_for_client = cas.clone();
    let worker_service_for_client = if single_port {
        Some(WorkerServiceImpl::with_registry(
            scheduler.clone(),
            worker_registry.clone(),
        ))
    } else {
        None
    };
    let client_addr = args.listen;
    let client_handle = tokio::spawn(async move {
        // Build the service set; WorkerService joins in single-port mode.
        // `Server::add_service` returns a `Router` whose own `add_service`
        // consumes self and chains; the original `Server` is *not* consumed
        // by this call (note `&mut self`). We still hold `client_server` for
        // the rest of the closure's lifetime to keep its TLS settings.
        let mut server = client_server;
        let mut router = server
            .add_service(ContentAddressableStorageServer::with_interceptor(
                CasService::new(cas_for_client),
                auth_interceptor(auth_for_client.clone()),
            ))
            .add_service(ActionCacheServer::with_interceptor(
                ActionCacheService::new(action_cache_for_client),
                auth_interceptor(auth_for_client.clone()),
            ))
            .add_service(CapabilitiesServer::with_interceptor(
                CapabilitiesService,
                auth_interceptor(auth_for_client.clone()),
            ))
            .add_service(ExecutionServer::with_interceptor(
                ExecutionService::new(scheduler.clone()),
                auth_interceptor(auth_for_client.clone()),
            ));
        if let Some(ws) = worker_service_for_client {
            router = router.add_service(WorkerServiceServer::new(ws));
        }
        let _ = &mut server; // keep `server` live until we're done with the router.
        router
            .serve_with_incoming(TcpListenerStream::new(client_listener))
            .await
            .with_context(|| format!("client listener ({client_addr}) exited"))
    });

    // Operator observability listener (ADR 0012). Its own bind address, never
    // the tenant-facing one: ADR 0011's auth resolves a token to a tenant and
    // has no scope concept, so a tenant reaching this service could enumerate
    // every worker and every other tenant's jobs.
    let observe_mtls = args.observe_tls_cert.is_some();
    validate_observe_bind(
        args.observe_listen,
        observe_mtls,
        args.observe_allow_insecure_bind,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    let observe_listener = TcpListener::bind(args.observe_listen)
        .await
        .with_context(|| format!("binding observability listener on {}", args.observe_listen))?;
    let mut observe_server = Server::builder();
    if let Some(cfg) = args.observe_tls_config()? {
        observe_server = observe_server.tls_config(cfg)?;
    }
    if observe_mtls {
        tracing::info!(
            addr = %args.observe_listen,
            "operator observability listener bound (read-only, mTLS)"
        );
    } else if args.observe_listen.ip().is_loopback() {
        tracing::info!(
            addr = %args.observe_listen,
            "operator observability listener bound (read-only, loopback only)"
        );
    } else {
        tracing::warn!(
            addr = %args.observe_listen,
            "OPERATOR OBSERVABILITY LISTENER IS UNAUTHENTICATED ON A ROUTABLE \
             ADDRESS — permitted only because --observe-allow-insecure-bind was given"
        );
    }
    let observe_deps = ObservabilityDeps {
        node_id: args.node_id.clone(),
        advertise_addr: advertise_addr.clone(),
        registry: worker_registry.clone(),
        scheduler: scheduler_for_observability,
        cas: cas.clone(),
        policy: policy_strategy.clone(),
        raft: raft_handle_for_observability.clone(),
    };
    // Raft peer plane, deferred from the Raft branch so `PeerObservability`
    // can share the listener (ADR 0012). Spawned before the operator listener
    // so a peer that comes up first has something to talk to.
    if let Some((raft_server, peer_tls, raft_addr)) = raft_peer_parts {
        let peer_deps = observe_deps.clone();
        tokio::spawn(async move {
            let mut builder = Server::builder();
            if let Some(tls) = peer_tls {
                match builder.tls_config(tls) {
                    Ok(b) => builder = b,
                    Err(e) => {
                        tracing::error!(error = %e, "raft peer TLS configuration rejected");
                        return;
                    }
                }
            }
            if let Err(e) = builder
                .add_service(raft_server)
                .add_service(PeerObservabilityServer::new(PeerObservabilityService::new(
                    peer_deps,
                )))
                .serve(raft_addr)
                .await
            {
                tracing::error!(error = %e, "raft peer listener exited");
            }
        });
    }

    let observe_addr = args.observe_listen;
    let observe_handle = tokio::spawn(async move {
        observe_server
            .add_service(ObservabilityServiceServer::new(ObservabilityService::new(
                observe_deps,
            )))
            .serve_with_incoming(TcpListenerStream::new(observe_listener))
            .await
            .with_context(|| format!("observability listener ({observe_addr}) exited"))
    });

    if single_port {
        // Worker port == client port; no second listener. The client and
        // observability listeners own the lifetime between them.
        tokio::select! {
            r = client_handle => r.with_context(|| "client listener task panicked")??,
            r = observe_handle => r.with_context(|| "observability listener task panicked")??,
        }
        return Ok(());
    }

    // Split-port mode: bind a second listener dedicated to `WorkerService`
    // AND `ContentAddressableStorage` (so workers can upload stdout/stderr
    // without a JWT bearer — they authenticate at the TLS layer via the
    // mTLS client cert they presented on this port; ADR 0011, issue #139).
    // The worker port requires a client cert when `--tls-client-ca` is set
    // (tonic 0.12 enforces this implicitly: `client_auth_optional` defaults
    // to `false` after `client_ca_root`). It carries no JWT interceptor —
    // the worker is authenticated at the transport layer.
    //
    // ActionCache / Capabilities / Execution stay on the JWT-gated client
    // port: they are client-only services and a worker has no business
    // reading them.
    let worker_listener = tokio::net::TcpListener::bind(worker_listen_addr)
        .await
        .with_context(|| format!("binding worker listener on {worker_listen_addr}"))?;
    let mut worker_server = Server::builder();
    if let Some(cfg) = worker_tls_cfg {
        worker_server = worker_server.tls_config(cfg)?;
    }
    let cas_for_worker = cas.clone();
    let worker_addr = worker_listen_addr;
    let worker_handle = tokio::spawn(async move {
        worker_server
            .add_service(WorkerServiceServer::new(worker_service))
            // CAS on the worker port is unauthenticated by interceptor
            // (mTLS already established the worker's identity at the
            // transport layer). Sharing the same `RedbCas` instance with
            // the client port means both paths see the same content —
            // the client uploads inputs here, the worker uploads
            // stdout/stderr here, both reads see the union.
            .add_service(ContentAddressableStorageServer::new(CasService::new(
                cas_for_worker,
            )))
            .serve_with_incoming(TcpListenerStream::new(worker_listener))
            .await
            .with_context(|| format!("worker listener ({worker_addr}) exited"))
    });

    // Whichever listener exits first aborts the process. A half-running
    // control plane (client up, worker down, or vice versa) is not a
    // useful state to leave behind.
    tokio::select! {
        r = client_handle => r.with_context(|| "client listener task panicked")??,
        r = worker_handle => r.with_context(|| "worker listener task panicked")??,
        r = observe_handle => r.with_context(|| "observability listener task panicked")??,
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]
mod tests {
    /// Loopback is the intended posture and needs nothing further: reach it
    /// over SSH.
    #[test]
    fn a_loopback_observability_bind_needs_no_further_authorization() {
        let v4: SocketAddr = "127.0.0.1:7880".parse().unwrap();
        assert!(validate_observe_bind(v4, false, false).is_ok());
        let v6: SocketAddr = "[::1]:7880".parse().unwrap();
        assert!(validate_observe_bind(v6, false, false).is_ok());
    }

    /// A routable bind with no authentication would serve the whole cluster's
    /// state to the network. The error must name both remedies, because an
    /// operator hitting this at deploy time needs to know what to do, not just
    /// that they are wrong.
    #[test]
    fn a_routable_observability_bind_without_mtls_is_rejected() {
        let wildcard: SocketAddr = "0.0.0.0:7880".parse().unwrap();
        let err = validate_observe_bind(wildcard, false, false).unwrap_err();
        assert!(
            err.contains("--observe-tls-cert"),
            "must name the remedy: {err}"
        );
        assert!(
            err.contains("--observe-allow-insecure-bind"),
            "must name the override: {err}"
        );

        // A specific routable address is no better than the wildcard.
        let specific: SocketAddr = "10.0.0.5:7880".parse().unwrap();
        assert!(validate_observe_bind(specific, false, false).is_err());
    }

    #[test]
    fn a_routable_bind_is_allowed_with_mtls_or_an_explicit_override() {
        let addr: SocketAddr = "0.0.0.0:7880".parse().unwrap();
        assert!(validate_observe_bind(addr, true, false).is_ok());
        assert!(validate_observe_bind(addr, false, true).is_ok());
    }

    use super::*;

    /// Parses `Args` from flags, prepending a real HMAC secret file so JWT
    /// auth is *enabled* (validate_auth_flags short-circuits when it is not).
    fn args_with_auth(extra: &[&str]) -> (tempfile::TempDir, Args) {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("hmac.key");
        std::fs::write(&secret, b"test-secret").unwrap();
        let secret = secret.to_str().unwrap().to_string();
        let mut argv = vec![
            "brokkr-control".to_string(),
            "--auth-jwt-hmac-secret-file".to_string(),
            secret,
        ];
        argv.extend(extra.iter().map(|s| s.to_string()));
        let args = Args::try_parse_from(argv).unwrap();
        (dir, args)
    }

    /// I9d: the Raft peer plane is all-or-nothing. Every half-configuration
    /// must fail at **startup**, naming what is missing — the failure this
    /// guards against is otherwise silent until a peer first tries to
    /// replicate, which is the posture issue #139 established for the other
    /// two planes.
    #[test]
    fn raft_plane_tls_is_all_or_nothing() {
        let cert = PathBuf::from("/c.pem");
        let key = PathBuf::from("/k.pem");
        let ca = PathBuf::from("/ca.pem");

        // None of the three: plaintext, which is legal (dev-only).
        assert_eq!(
            resolve_raft_tls(true, None, None, None).unwrap(),
            RaftPlaneTls::Disabled
        );
        // ...and legal without --raft too, since nothing is configured.
        assert_eq!(
            resolve_raft_tls(false, None, None, None).unwrap(),
            RaftPlaneTls::Disabled
        );

        // All three: mutual TLS.
        assert_eq!(
            resolve_raft_tls(true, Some(&cert), Some(&key), Some(&ca)).unwrap(),
            RaftPlaneTls::Enabled {
                cert: cert.clone(),
                key: key.clone(),
                ca: ca.clone(),
            }
        );

        // Every partial combination is refused, and the message names the
        // missing flag(s) rather than saying "invalid configuration".
        for (c, k, a, expected) in [
            (Some(&cert), None, None, "--raft-tls-key"),
            (None, Some(&key), None, "--raft-tls-cert"),
            (None, None, Some(&ca), "--raft-tls-cert"),
            (Some(&cert), Some(&key), None, "--raft-tls-ca"),
            (Some(&cert), None, Some(&ca), "--raft-tls-key"),
            (None, Some(&key), Some(&ca), "--raft-tls-cert"),
        ] {
            let err = resolve_raft_tls(true, c, k, a).unwrap_err().to_string();
            assert!(
                err.contains("half-configured") && err.contains(expected),
                "expected the error to name {expected}, got: {err}"
            );
        }

        // Fully configured but --raft off: the flags would silently do
        // nothing, so say so instead of starting an unsecured-but-unused
        // plane.
        let err = resolve_raft_tls(false, Some(&cert), Some(&key), Some(&ca))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--raft is off"),
            "expected the error to point at --raft, got: {err}"
        );
    }

    /// I9b: the advertise address defaults to `--listen`, an explicit value
    /// wins, and a wildcard bind under `--raft` is refused rather than
    /// published as an undialable hint.
    #[test]
    fn advertise_addr_resolution_refuses_only_unusable_wildcards() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["brokkr-control".to_string()];
            argv.extend(extra.iter().map(|s| s.to_string()));
            Args::try_parse_from(argv).unwrap()
        };

        // Default: whatever we bind is what we advertise.
        let args = parse(&["--listen", "127.0.0.1:7878"]);
        assert_eq!(
            args.resolved_advertise_addr().unwrap(),
            "127.0.0.1:7878".to_string()
        );

        // Explicit wins, including over a wildcard bind — the normal
        // production shape: bind everywhere, advertise one reachable name.
        let args = parse(&[
            "--raft",
            "--listen",
            "0.0.0.0:7878",
            "--advertise-addr",
            "control-1.internal:7878",
        ]);
        assert_eq!(
            args.resolved_advertise_addr().unwrap(),
            "control-1.internal:7878".to_string()
        );

        // Wildcard + --raft + no explicit value: refused, and the message
        // must name the flag that fixes it.
        let args = parse(&["--raft", "--listen", "0.0.0.0:7878"]);
        let err = args.resolved_advertise_addr().unwrap_err().to_string();
        assert!(
            err.contains("--advertise-addr"),
            "the error must name the fix, got: {err}"
        );

        // The IPv6 wildcard is just as undialable.
        let args = parse(&["--raft", "--listen", "[::]:7878"]);
        assert!(args.resolved_advertise_addr().is_err());

        // Without --raft nothing is published, so a wildcard bind stays legal:
        // single-node operators are not made to name themselves.
        let args = parse(&["--listen", "0.0.0.0:7878"]);
        assert_eq!(
            args.resolved_advertise_addr().unwrap(),
            "0.0.0.0:7878".to_string()
        );

        // An empty explicit value is a typo, not a default.
        let args = parse(&["--advertise-addr", "   "]);
        assert!(args.resolved_advertise_addr().is_err());
    }

    #[test]
    fn auth_with_split_ports_is_accepted() {
        let (_d, args) = args_with_auth(&[
            "--listen",
            "127.0.0.1:50051",
            "--tls-cert",
            "/dev/null",
            "--tls-key",
            "/dev/null",
            "--tls-client-ca",
            "/dev/null",
        ]);
        assert!(args.validate_auth_flags().is_ok());
        assert!(!args.effective_single_port());
    }

    #[test]
    fn auth_with_single_port_flag_is_refused() {
        let (_d, args) = args_with_auth(&[
            "--single-port",
            "--tls-cert",
            "/dev/null",
            "--tls-key",
            "/dev/null",
            "--tls-client-ca",
            "/dev/null",
        ]);
        let err = args.validate_auth_flags().unwrap_err().to_string();
        assert!(err.contains("--single-port"), "got: {err}");
    }

    #[test]
    fn auth_without_client_ca_is_refused() {
        let (_d, args) = args_with_auth(&[]);
        let err = args.validate_auth_flags().unwrap_err().to_string();
        assert!(err.contains("--tls-client-ca"), "got: {err}");
    }

    #[test]
    fn auth_with_worker_listen_equal_to_listen_is_refused() {
        // Effectively single-port even though the flag was never passed.
        let (_d, args) = args_with_auth(&[
            "--listen",
            "127.0.0.1:50051",
            "--worker-listen",
            "127.0.0.1:50051",
            "--tls-cert",
            "/dev/null",
            "--tls-key",
            "/dev/null",
            "--tls-client-ca",
            "/dev/null",
        ]);
        assert!(args.effective_single_port());
        let err = args.validate_auth_flags().unwrap_err().to_string();
        assert!(err.contains("resolves to the same address"), "got: {err}");
    }

    #[test]
    fn auth_with_ipv6_listen_is_refused_without_explicit_worker_listen() {
        // The port-bump heuristic is IPv4-only, so an IPv6 --listen resolves
        // the worker listener onto the same address.
        let (_d, args) = args_with_auth(&[
            "--listen",
            "[::1]:50051",
            "--tls-cert",
            "/dev/null",
            "--tls-key",
            "/dev/null",
            "--tls-client-ca",
            "/dev/null",
        ]);
        assert!(args.effective_single_port());
        assert!(args.validate_auth_flags().is_err());

        // A distinct explicit worker listener resolves the conflict.
        let (_d2, args) = args_with_auth(&[
            "--listen",
            "[::1]:50051",
            "--worker-listen",
            "[::1]:50052",
            "--tls-cert",
            "/dev/null",
            "--tls-key",
            "/dev/null",
            "--tls-client-ca",
            "/dev/null",
        ]);
        assert!(args.validate_auth_flags().is_ok());
    }

    #[test]
    fn auth_with_saturated_port_is_refused() {
        // 65535 + 1 saturates back to 65535: the bump silently collapses the
        // two listeners onto one address.
        let (_d, args) = args_with_auth(&[
            "--listen",
            "127.0.0.1:65535",
            "--tls-cert",
            "/dev/null",
            "--tls-key",
            "/dev/null",
            "--tls-client-ca",
            "/dev/null",
        ]);
        assert!(args.effective_single_port());
        assert!(args.validate_auth_flags().is_err());
    }
}
