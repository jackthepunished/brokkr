//! The SDK's observability client against a real control plane.
//!
//! Lives in `brokkr-control`'s test suite rather than the SDK's, because
//! `brokkr-control` already dev-depends on `brokkr-sdk` and the in-process
//! harness is here. Putting it the other way round would add a dev-dependency
//! edge back from the SDK to the control plane for no gain.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::panic
)]

use brokkr_sdk::ObservabilityClient;

mod common;

#[tokio::test]
async fn the_sdk_reads_every_observability_surface() {
    let (_client, observe, _dir) = common::boot_with_observability().await;
    let mut c = ObservabilityClient::connect(observe).await.unwrap();

    let cluster = c
        .get_cluster()
        .await
        .unwrap()
        .expect("a conformant server always sets the cluster field");
    assert_eq!(cluster.nodes.len(), 1);
    assert_eq!(cluster.nodes[0].node_id, "test-node");
    assert!(!cluster.degraded);
    assert!(cluster.quorum_healthy);

    assert!(c.list_workers().await.unwrap().is_empty());
    assert!(c.list_jobs(None, 0).await.unwrap().is_empty());

    // Per-node shapes, never a single combined figure.
    let policies = c.get_policy().await.unwrap();
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0].owning_node, "test-node");

    let stores = c.get_cas_stats().await.unwrap();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].owning_node, "test-node");
}

/// A job that has aged out of the bounded ring is `NotFound`, and the SDK
/// surfaces that as an `Rpc` error rather than swallowing it into an `Option`.
#[tokio::test]
async fn a_missing_job_is_an_error_not_a_silent_none() {
    let (_client, observe, _dir) = common::boot_with_observability().await;
    let mut c = ObservabilityClient::connect(observe).await.unwrap();

    let err = match c.get_job("no-such-job").await {
        Ok(job) => panic!("expected an error, got {job:?}"),
        Err(e) => e,
    };
    let text = err.to_string();
    assert!(
        text.contains("rpc failed"),
        "a server refusal must surface as an Rpc error, not a transport one: {text}"
    );
}

/// The stream's first item is a full snapshot, so a consumer needs no
/// reconciliation logic on connect or reconnect.
#[tokio::test]
async fn the_sdk_event_stream_opens_with_a_snapshot() {
    use futures::StreamExt as _;

    let (_client, observe, _dir) = common::boot_with_observability().await;
    let mut c = ObservabilityClient::connect(observe).await.unwrap();
    let mut stream = Box::pin(c.watch_events().await.unwrap());

    let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("the stream must send something without waiting for a change")
        .expect("stream ended immediately")
        .unwrap();

    assert!(
        matches!(
            first.event,
            Some(brokkr_proto::brokkr_v1::cluster_event::Event::Snapshot(_))
        ),
        "the first item must be a Snapshot, got {:?}",
        first.event
    );
}

/// Connecting to something that is not an observability listener fails as a
/// transport error, distinct from a server refusal.
#[tokio::test]
async fn an_unreachable_endpoint_is_a_transport_error() {
    // Port 1 on loopback: reserved, and nothing listens there.
    let err = match ObservabilityClient::connect("http://127.0.0.1:1").await {
        Ok(_) => panic!("expected a connection failure"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("transport"),
        "unreachable must be a transport error, not an rpc one: {err}"
    );
}
