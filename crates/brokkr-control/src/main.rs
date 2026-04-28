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
use tonic::transport::Server;

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

    tracing::info!(addr = %args.listen, data_dir = ?args.data_dir, "brokkr-control starting");

    Server::builder()
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
