//! Shared fixtures for Phase 1 integration tests.
//!
//! Spins up a full in-process cluster (control plane + worker) over an
//! ephemeral TCP port and returns the SDK endpoint URL plus the temp-dir
//! guard. Drop the guard to clean up the on-disk redb databases.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    dead_code
)]

use std::sync::Arc;
use std::time::Duration;

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
use brokkr_worker::{run_worker, WorkerConfig};
use tokio::net::TcpListener;
use tonic::transport::Server;

pub async fn boot_cluster() -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(RedbCas::open(dir.path().join("cas.redb")).unwrap());
    let ac = Arc::new(RedbActionCache::open(dir.path().join("ac.redb")).unwrap());
    let scheduler = Scheduler::new(cas.clone(), ac.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let endpoint = format!("http://{addr}");
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let scheduler_for_server = scheduler.clone();
    tokio::spawn(async move {
        Server::builder()
            .add_service(ContentAddressableStorageServer::new(CasService::new(cas)))
            .add_service(ActionCacheServer::new(ActionCacheService::new(ac)))
            .add_service(CapabilitiesServer::new(CapabilitiesService))
            .add_service(ExecutionServer::new(ExecutionService::new(
                scheduler_for_server.clone(),
            )))
            .add_service(WorkerServiceServer::new(WorkerServiceImpl::new(
                scheduler_for_server,
            )))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    // Server ready window.
    tokio::time::sleep(Duration::from_millis(80)).await;

    let worker_endpoint = endpoint.clone();
    tokio::spawn(async move {
        let cfg = WorkerConfig {
            control_endpoint: worker_endpoint,
            hostname: "test-worker".to_string(),
        };
        let _ = run_worker(cfg).await;
    });

    // Worker register + stream-claim window.
    tokio::time::sleep(Duration::from_millis(120)).await;

    (endpoint, dir)
}
