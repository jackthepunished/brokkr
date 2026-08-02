//! The operator listener is separate from the tenant-facing listener.
//!
//! ADR 0011's auth has no scope concept — `Authenticator::authenticate`
//! returns a `TenantId` and nothing else. If `ObservabilityService` were
//! mounted on the client port, any tenant's token could enumerate every worker
//! and every other tenant's jobs. The entire security argument for D4 in
//! `docs/superpowers/specs/2026-08-02-observability-read-model-design.md`
//! rests on that separation, so it gets its own test rather than being assumed.

#![allow(
    clippy::unwrap_used,
    // `expect` messages here carry the diagnostic — "the stream must send
    // something without waiting for a change" is the whole point of the
    // assertion. `split_port_cluster.rs` allows it for the same reason.
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::panic
)]

use brokkr_proto::brokkr_v1::observability_service_client::ObservabilityServiceClient;
use brokkr_proto::brokkr_v1::{
    GetCasStatsRequest, GetClusterRequest, GetJobRequest, GetPolicyRequest, ListJobsRequest,
    ListWorkersRequest, WatchEventsRequest,
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
    // No Raft configured. "standalone" rather than "unknown": nothing elected
    // it, but it is also not mid-election, and conflating the two would make
    // every single-node deployment report itself degraded forever.
    assert_eq!(info.nodes[0].role, "standalone");
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

/// `ListJobs` answers, and an unknown state filter is treated as no filter
/// rather than an error — a newer client asking about a state this server does
/// not know should get everything rather than a rejection.
#[tokio::test]
async fn list_jobs_answers_and_tolerates_an_unknown_filter() {
    let (_client, observe, _dir) = common::boot_with_observability().await;
    let mut c = ObservabilityServiceClient::connect(observe).await.unwrap();

    let all = c
        .list_jobs(ListJobsRequest {
            state_filter: String::new(),
            limit: 0,
        })
        .await
        .unwrap()
        .into_inner()
        .jobs;
    assert!(all.is_empty(), "no job has run in this fixture");

    // Not a rejection.
    let unknown = c
        .list_jobs(ListJobsRequest {
            state_filter: "evaporated".to_string(),
            limit: 10,
        })
        .await
        .unwrap()
        .into_inner()
        .jobs;
    assert!(unknown.is_empty());
}

/// A job that is not in any node's ring is `NotFound`, not an empty reply.
/// "I do not have that job" and "that job had no data" are different answers,
/// and the ring is bounded so a genuinely old job legitimately falls out.
#[tokio::test]
async fn get_job_reports_not_found_rather_than_an_empty_reply() {
    let (_client, observe, _dir) = common::boot_with_observability().await;
    let mut c = ObservabilityServiceClient::connect(observe).await.unwrap();

    let status = match c
        .get_job(GetJobRequest {
            job_id: "no-such-job".to_string(),
        })
        .await
    {
        Ok(reply) => panic!("expected NotFound, got a reply: {reply:?}"),
        Err(status) => status,
    };
    assert_eq!(status.code(), tonic::Code::NotFound);
}

/// The resync contract's first half: a subscriber's very first message is
/// always a full `Snapshot`, so a client that has just connected — or
/// reconnected — needs no reconciliation logic.
#[tokio::test]
async fn watch_events_opens_with_a_full_snapshot() {
    use tokio_stream::StreamExt as _;

    let (_client, observe, _dir) = common::boot_with_observability().await;
    let mut c = ObservabilityServiceClient::connect(observe).await.unwrap();

    let mut stream = c
        .watch_events(WatchEventsRequest {})
        .await
        .unwrap()
        .into_inner();

    let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("the stream must send something without waiting for a change")
        .expect("stream ended immediately")
        .unwrap();

    let event = first.event.expect("the first message must carry an event");
    let snapshot = match event {
        brokkr_proto::brokkr_v1::cluster_event::Event::Snapshot(s) => s,
        other => panic!("the first message must be a Snapshot, got {other:?}"),
    };
    let cluster = snapshot.cluster.expect("a snapshot carries the cluster");
    assert_eq!(cluster.nodes.len(), 1);
    assert_eq!(cluster.nodes[0].node_id, "test-node");
    // Per-node collections are present even when empty, so a client can
    // replace its whole world from this one message.
    assert_eq!(snapshot.policies.len(), 1);
    assert_eq!(snapshot.stores.len(), 1);
}

/// A second subscriber gets its own opening snapshot. Nothing about the
/// stream is single-consumer, and a console opened twice must work twice.
#[tokio::test]
async fn every_subscriber_gets_its_own_opening_snapshot() {
    use tokio_stream::StreamExt as _;

    let (_client, observe, _dir) = common::boot_with_observability().await;
    let mut a = ObservabilityServiceClient::connect(observe.clone())
        .await
        .unwrap();
    let mut b = ObservabilityServiceClient::connect(observe).await.unwrap();

    for client in [&mut a, &mut b] {
        let mut stream = client
            .watch_events(WatchEventsRequest {})
            .await
            .unwrap()
            .into_inner();
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("each subscriber gets an opening snapshot")
            .expect("stream ended")
            .unwrap();
        assert!(matches!(
            first.event,
            Some(brokkr_proto::brokkr_v1::cluster_event::Event::Snapshot(_))
        ));
    }
}
