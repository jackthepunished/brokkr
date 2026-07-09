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
    /// Build server TLS configuration from CLI arguments.
    fn tls_config(&self) -> Result<Option<ServerTlsConfig>> {
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
    /// The two cases we refuse:
    ///
    /// 1. `--single-port` + `--auth-jwt-*`. With single-port mode,
    ///    `WorkerService` shares the JWT-gated listener, so worker→CAS
    ///    writes are rejected. There is no fix other than splitting the
    ///    listener (which `--single-port` opts out of).
    /// 2. `--auth-jwt-*` + no `--tls-client-ca` + no `--single-port`.
    ///    Without a client CA the worker port can't authenticate callers,
    ///    so the worker→CAS path is again unreachable. The operator must
    ///    either configure mTLS or drop auth (and accept single-port
    ///    mode).
    fn validate_auth_flags(&self) -> Result<()> {
        let auth_on = self.authenticator()?.is_enabled();
        if self.single_port && auth_on {
            anyhow::bail!(
                "--single-port and --auth-jwt-* are incompatible (issue #139): \
                 WorkerService would share the JWT-gated listener and every \
                 worker->CAS write would be rejected with UNAUTHENTICATED; \
                 either drop --single-port so the worker port can use mTLS, \
                 or drop --auth-jwt-* for local-dev single-port mode"
            );
        }
        if auth_on && self.tls_client_ca.is_none() {
            anyhow::bail!(
                "--auth-jwt-* requires --tls-client-ca so WorkerService can be \
                 authenticated by mTLS on the worker port (issue #139); \
                 alternatively pass --single-port with --auth-jwt-* disabled for \
                 local dev"
            );
        }
        Ok(())
    }

    /// The address the worker listener should bind to. Resolution:
    ///
    /// * `--worker-listen` if explicitly set, else
    /// * `--listen` itself (single-port mode), else
    /// * `port_one_above(--listen)` (split mode with mTLS).
    ///
    /// Only the third case is actually a different address today, but
    /// this is the single source of truth that commit 2 will use when it
    /// splits the listener.
    #[allow(dead_code)] // wired in commit 2 (split listener); see issue #139.
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

    let tls_cfg = args.tls_config()?;
    match &tls_cfg {
        Some(_) => {
            tracing::warn!("TLS ENABLED — mTLS required for production");
        }
        None => {
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

    tracing::info!(addr = %args.listen, data_dir = ?args.data_dir, "brokkr-control starting");

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

    let mut server = Server::builder();
    if let Some(tls_cfg) = tls_cfg {
        server = server.tls_config(tls_cfg)?;
    }
    // Client-facing services require client auth (the interceptor injects the
    // authoritative tenant). The internal WorkerService is mTLS-authenticated,
    // not token-gated.
    server
        .add_service(ContentAddressableStorageServer::with_interceptor(
            CasService::new(cas),
            auth_interceptor(auth.clone()),
        ))
        .add_service(ActionCacheServer::with_interceptor(
            ActionCacheService::new(action_cache),
            auth_interceptor(auth.clone()),
        ))
        .add_service(CapabilitiesServer::with_interceptor(
            CapabilitiesService,
            auth_interceptor(auth.clone()),
        ))
        .add_service(ExecutionServer::with_interceptor(
            ExecutionService::new(scheduler.clone()),
            auth_interceptor(auth.clone()),
        ))
        .add_service(WorkerServiceServer::new(worker_service))
        .serve(args.listen)
        .await
        .context("control plane server exited")?;
    Ok(())
}
