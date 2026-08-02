//! The operator listener is separate from the tenant-facing listener.
//!
//! ADR 0011's auth has no scope concept — `Authenticator::authenticate`
//! returns a `TenantId` and nothing else. If `ObservabilityService` were
//! mounted on the client port, any tenant's token could enumerate every worker
//! and every other tenant's jobs. The entire security argument for D4 in
//! `docs/superpowers/specs/2026-08-02-observability-read-model-design.md`
//! rests on that separation, so it gets its own test rather than being assumed.

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

use brokkr_proto::brokkr_v1::observability_service_client::ObservabilityServiceClient;
use brokkr_proto::brokkr_v1::{
    GetCasStatsRequest, GetClusterRequest, GetPolicyRequest, ListWorkersRequest,
};

mod common;

#[tokio::test]
async fn observability_answers_on_the_operator_listener() {
    let (_client, observe, _dir) = common::boot_with_observability().await;
    let mut c = ObservabilityServiceClient::connect(observe).await.unwrap();

    let info = c
        .get_cluster(GetClusterRequest {})
        .await
        .unwrap()
        .into_inner()
        .cluster
        .unwrap();
    assert_eq!(info.nodes.len(), 1, "a single node reports exactly itself");
    assert_eq!(info.nodes[0].node_id, "test-node");
    assert!(info.nodes[0].reachable);
    assert!(!info.degraded, "a reachable local node is not degraded");
    assert!(info.quorum_healthy);
    // No Raft configured: the node claims no role, because nothing elected it.
    assert_eq!(info.nodes[0].role, "unknown");
    assert_eq!(info.leader_id, "");
}

/// Every RPC answers, and the per-node shapes are repeated rather than scalar —
/// the wire type is where "these must never be combined" is enforced.
#[tokio::test]
async fn every_read_rpc_answers_with_per_node_shapes() {
    let (_client, observe, _dir) = common::boot_with_observability().await;
    let mut c = ObservabilityServiceClient::connect(observe).await.unwrap();

    let workers = c
        .list_workers(ListWorkersRequest {})
        .await
        .unwrap()
        .into_inner()
        .workers;
    assert!(
        workers.is_empty(),
        "no worker was registered in this fixture"
    );

    let policies = c
        .get_policy(GetPolicyRequest {})
        .await
        .unwrap()
        .into_inner()
        .policies;
    assert_eq!(policies.len(), 1, "one entry per node, never combined");
    assert!(!policies[0].loaded, "no WASM policy in this fixture");
    assert_eq!(policies[0].owning_node, "test-node");

    let stores = c
        .get_cas_stats(GetCasStatsRequest {})
        .await
        .unwrap()
        .into_inner()
        .stores;
    assert_eq!(stores.len(), 1, "one entry per node, never summed");
    assert_eq!(stores[0].owning_node, "test-node");
    assert_eq!(stores[0].objects, 0);
}

/// The load-bearing test. If this ever passes by serving a real reply, the
/// separation D4 depends on has been lost.
#[tokio::test]
async fn observability_is_not_reachable_on_the_tenant_listener() {
    let (client, _observe, _dir) = common::boot_with_observability().await;

    // Connecting may well succeed — it is the same transport, and tonic
    // connects lazily to a port that is genuinely listening. What must not
    // happen is the *service* answering there.
    let Ok(mut c) = ObservabilityServiceClient::connect(client).await else {
        // Refusing the connection outright is also an acceptable outcome.
        return;
    };
    let status = match c.get_cluster(GetClusterRequest {}).await {
        Ok(reply) => panic!(
            "ObservabilityService answered on the tenant port — the separation \
             D4 depends on has been lost. Got: {reply:?}"
        ),
        Err(status) => status,
    };
    assert_eq!(
        status.code(),
        tonic::Code::Unimplemented,
        "expected the tenant listener to not implement this service; got {status:?}"
    );
}
