//! `brokkr-control` daemon entrypoint.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use brokkr_cas::{RedbActionCache, RedbCas};
use brokkr_control::registry::WorkerRegistry;
use brokkr_control::{
    auth_interceptor, spawn_eviction_task, spawn_lease_reaper, ActionCacheService, Authenticator,
    CapabilitiesService, CasService, ExecutionService, JwtAuth, Scheduler, SharedWorkerRegistry,
    WorkerServiceImpl,
};
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

    /// Directory holding the control plane's persistent state (CAS + action cache).
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
    let action_cache = Arc::new(
        RedbActionCache::open(args.data_dir.join("action_cache.redb"))
            .context("opening action cache database")?,
    );
    // One shared worker registry, three users: the scheduler reads it for
    // platform-constraint admission control, the worker service writes
    // registrations / heartbeats into it, and the eviction reaper prunes
    // stale entries.
    let worker_registry: SharedWorkerRegistry =
        Arc::new(tokio::sync::Mutex::new(WorkerRegistry::default()));
    let scheduler =
        Scheduler::with_worker_registry(cas.clone(), action_cache.clone(), worker_registry.clone());

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

    if single_port {
        // Worker port == client port; no second listener. The client_handle
        // owns the lifetime.
        client_handle
            .await
            .with_context(|| "client listener task panicked")??;
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
