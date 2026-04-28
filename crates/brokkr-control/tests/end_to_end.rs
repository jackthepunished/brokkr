//! End-to-end Phase 1 test: control plane + worker + SDK in-process.
//!
//! Spins up the gRPC server and a worker over an ephemeral TCP port, runs
//! `echo hello world` via the SDK, then runs the same command again to assert
//! a cache hit. Phase 1 exit criterion (plan §13).

#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

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
use brokkr_sdk::{run_command, BrokkrClient};
use brokkr_worker::{run_worker, WorkerConfig};
use tokio::net::TcpListener;
use tonic::transport::Server;

async fn boot_cluster() -> (String, tempfile::TempDir) {
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

    // Boot a worker against this control plane.
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

#[tokio::test]
async fn echo_hello_world_runs_and_caches() {
    let (endpoint, _dir) = boot_cluster().await;
    let mut client = BrokkrClient::connect(endpoint).await.unwrap();

    let argv = vec!["/bin/echo".to_string(), "hello world".to_string()];

    let first = run_command(&mut client, &argv, false).await.unwrap();
    assert_eq!(first.exit_code, 0, "first run exit code");
    assert!(
        !first.cache_hit,
        "first run must not be cached, got cache_hit=true"
    );
    assert_eq!(first.stdout.as_ref(), b"hello world\n");

    let second = run_command(&mut client, &argv, false).await.unwrap();
    assert_eq!(second.exit_code, 0, "second run exit code");
    assert!(
        second.cache_hit,
        "second run must hit the action cache, got cache_hit=false"
    );
    assert_eq!(second.stdout.as_ref(), b"hello world\n");
}

#[tokio::test]
async fn nonzero_exit_is_not_cached() {
    let (endpoint, _dir) = boot_cluster().await;
    let mut client = BrokkrClient::connect(endpoint).await.unwrap();
    let argv = vec!["/bin/false".to_string()];
    let first = run_command(&mut client, &argv, false).await.unwrap();
    assert_ne!(first.exit_code, 0);
    assert!(!first.cache_hit);

    // Second run must also miss because exit != 0 isn't cached.
    let second = run_command(&mut client, &argv, false).await.unwrap();
    assert!(!second.cache_hit, "failures must not be cached");
}
