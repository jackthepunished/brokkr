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

use brokkr_cas::RedbCas;
use brokkr_control::{
    ActionCacheService, CapabilitiesService, CasService, ExecutionService, MetaKvActionCache,
    RedbMetaKv, Scheduler, WorkerServiceImpl,
};
use brokkr_proto::brokkr_v1::worker_service_server::WorkerServiceServer;
use brokkr_proto::reapi_v2::{
    action_cache_server::ActionCacheServer, capabilities_server::CapabilitiesServer,
    content_addressable_storage_server::ContentAddressableStorageServer,
    execution_server::ExecutionServer,
};
use brokkr_worker::{run_worker, Runner, WorkerConfig};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

pub async fn boot_cluster() -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(RedbCas::open(dir.path().join("cas.redb")).unwrap());
    // Same backend main.rs ships (I8a): the action cache behind the MetaKv
    // seam, so the integration suite exercises the production path.
    let meta_kv = Arc::new(RedbMetaKv::open(dir.path().join("meta.redb")).unwrap());
    let ac = Arc::new(MetaKvActionCache::new(meta_kv));
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
        // Phase 1 fixtures intentionally use Runner::Plain — the
        // sandbox path is exercised separately by the brokkr-worker
        // sandbox-mode integration tests, which require the
        // brokkr-sandboxd binary and an unprivileged userns.
        let cfg = WorkerConfig {
            // Open / single-port test fixture: WorkerService shares the
            // client listener, so one `ControlPlane` with no separate worker
            // port is exactly right (issue #139).
            control_planes: vec![brokkr_worker::ControlPlane::single_port(worker_endpoint)],
            hostname: "test-worker".to_string(),
            runner: Runner::Plain,
            tls: None,
        };
        let _ = run_worker(cfg).await;
    });

    // Worker register + stream-claim window.
    tokio::time::sleep(Duration::from_millis(120)).await;

    (endpoint, dir)
}

/// Boot a control plane with the tenant listener **and** the operator
/// observability listener on separate ephemeral ports (ADR 0012).
///
/// Returns `(client_endpoint, observe_endpoint, tempdir)`. No worker is
/// spawned: the tests that use this are about the *routing* of observability
/// RPCs, not about running jobs.
///
/// Deliberately a second function rather than a change to [`boot_cluster`] —
/// its callers do not need a second endpoint, and a rival boot pattern in a
/// new file would be worse than one extra function here.
pub async fn boot_with_observability() -> (String, String, tempfile::TempDir) {
    use brokkr_control::cluster::{
        spawn_poller, ClusterSnapshot, GrpcPeerProbe, PollerConfig, PollerDeps, SharedSnapshot,
    };
    use brokkr_control::services::{LocalState, ObservabilityDeps, ObservabilityService};
    use brokkr_control::WorkerRegistry;
    use brokkr_proto::brokkr_v1::observability_service_server::ObservabilityServiceServer;

    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(RedbCas::open(dir.path().join("cas.redb")).unwrap());
    let meta_kv = Arc::new(RedbMetaKv::open(dir.path().join("meta.redb")).unwrap());
    let ac = Arc::new(MetaKvActionCache::new(meta_kv));
    let registry = Arc::new(tokio::sync::Mutex::new(WorkerRegistry::default()));
    let scheduler = Scheduler::with_worker_registry(cas.clone(), ac.clone(), registry.clone());

    // Tenant-facing listener: exactly the services main.rs mounts there, and
    // deliberately NOT ObservabilityService.
    let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client_endpoint = format!("http://{}", client_listener.local_addr().unwrap());
    let client_scheduler = scheduler.clone();
    let client_cas = cas.clone();
    let client_ac = ac.clone();
    tokio::spawn(async move {
        Server::builder()
            .add_service(ContentAddressableStorageServer::new(CasService::new(
                client_cas,
            )))
            .add_service(ActionCacheServer::new(ActionCacheService::new(client_ac)))
            .add_service(CapabilitiesServer::new(CapabilitiesService))
            .add_service(ExecutionServer::new(ExecutionService::new(
                client_scheduler,
            )))
            .serve_with_incoming(TcpListenerStream::new(client_listener))
            .await
            .unwrap();
    });

    // Operator listener: only ObservabilityService.
    let observe_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let observe_endpoint = format!("http://{}", observe_listener.local_addr().unwrap());
    let deps = ObservabilityDeps {
        node_id: "test-node".to_string(),
        advertise_addr: "127.0.0.1:0".to_string(),
        registry,
        scheduler,
        cas,
        policy: None,
        raft: None,
    };

    // The service reads a snapshot, so the fixture runs a real poller — with
    // no peers, so a round is one local read. Using the production path here
    // rather than hand-writing a snapshot means these tests would catch a
    // poller that stopped refreshing.
    let snapshot: SharedSnapshot = Arc::new(tokio::sync::RwLock::new(ClusterSnapshot::default()));
    spawn_poller(
        snapshot.clone(),
        PollerDeps {
            local: Arc::new(LocalState::new(deps)),
            peers: Arc::new(NoPeers),
            probe: Arc::new(GrpcPeerProbe),
        },
        PollerConfig {
            interval: Duration::from_millis(50),
            peer_timeout: Duration::from_millis(25),
            cas_interval: Duration::from_millis(50),
        },
    );

    tokio::spawn(async move {
        Server::builder()
            .add_service(ObservabilityServiceServer::new(ObservabilityService::new(
                snapshot,
            )))
            .serve_with_incoming(TcpListenerStream::new(observe_listener))
            .await
            .unwrap();
    });

    // Long enough for the first poll to land, so a caller does not race an
    // empty snapshot.
    tokio::time::sleep(Duration::from_millis(150)).await;
    (client_endpoint, observe_endpoint, dir)
}

/// A [`PeerDirectory`] with no peers, for single-node fixtures.
#[derive(Debug)]
struct NoPeers;

#[async_trait::async_trait]
impl brokkr_control::cluster::PeerDirectory for NoPeers {
    async fn peers(&self) -> Vec<brokkr_control::cluster::PeerAddr> {
        Vec::new()
    }
}
