//! `brokkr-control` daemon entrypoint.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use brokkr_cas::{RedbActionCache, RedbCas};
use brokkr_control::{
    ActionCacheService, CapabilitiesService, CasService, ExecutionService, Scheduler,
    WorkerServiceImpl,
};
use brokkr_proto::brokkr_v1::worker_service_server::WorkerServiceServer;
use brokkr_proto::reapi_v2::{
    action_cache_server::ActionCacheServer, capabilities_server::CapabilitiesServer,
    content_addressable_storage_server::ContentAddressableStorageServer,
    execution_server::ExecutionServer,
};
use clap::Parser;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

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

    /// PEM-encoded server certificate (enables TLS).
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// PEM-encoded server private key (enables TLS).
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,

    /// PEM-encoded CA certificate used to verify connecting client certificates (mTLS).
    #[arg(long, requires_all = ["tls_cert", "tls_key"])]
    tls_client_ca: Option<PathBuf>,
}

impl Args {
    fn tls_config(&self) -> Result<Option<ServerTlsConfig>> {
        let (Some(cert_path), Some(key_path)) = (&self.tls_cert, &self.tls_key) else {
            return Ok(None);
        };
        let cert_pem = std::fs::read_to_string(cert_path)
            .with_context(|| format!("reading TLS cert {:?}", cert_path))?;
        let key_pem = std::fs::read_to_string(key_path)
            .with_context(|| format!("reading TLS key {:?}", key_path))?;
        let identity = Identity::from_pem(cert_pem, key_pem);

        let mut cfg = ServerTlsConfig::new().identity(identity);
        if let Some(ca_path) = &self.tls_client_ca {
            let ca_pem = std::fs::read_to_string(ca_path)
                .with_context(|| format!("reading client CA {:?}", ca_path))?;
            cfg = cfg.client_ca_root(Certificate::from_pem(ca_pem));
        }
        Ok(Some(cfg))
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
    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("creating data dir {:?}", args.data_dir))?;
    let cas =
        Arc::new(RedbCas::open(args.data_dir.join("cas.redb")).context("opening CAS database")?);
    let action_cache = Arc::new(
        RedbActionCache::open(args.data_dir.join("action_cache.redb"))
            .context("opening action cache database")?,
    );
    let scheduler = Scheduler::new(cas.clone(), action_cache.clone());

    let tls_config = args.tls_config().context("loading TLS configuration")?;
    let tls_configured = tls_config.is_some();
    if tls_configured {
        tracing::warn!("TLS ENABLED — mTLS required for production deployments");
    } else {
        tracing::warn!("TLS DISABLED — NOT FOR PRODUCTION USE");
    }

    tracing::info!(addr = %args.listen, data_dir = ?args.data_dir, tls = tls_configured, "brokkr-control starting");

    let mut server = Server::builder();
    if let Some(tls_cfg) = tls_config {
        server = server.tls_config(tls_cfg)
            .context("configuring TLS")?;
    }

    server
        .add_service(ContentAddressableStorageServer::new(CasService::new(cas)))
        .add_service(ActionCacheServer::new(ActionCacheService::new(
            action_cache,
        )))
        .add_service(CapabilitiesServer::new(CapabilitiesService))
        .add_service(ExecutionServer::new(ExecutionService::new(
            scheduler.clone(),
        )))
        .add_service(WorkerServiceServer::new(WorkerServiceImpl::new(scheduler)))
        .serve(args.listen)
        .await
        .context("control plane server exited")?;
    Ok(())
}
