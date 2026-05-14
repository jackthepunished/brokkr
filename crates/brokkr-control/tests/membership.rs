//! Integration test for Phase 3 M1 membership + router.
//!
//! Boots the `MembershipService` gRPC server in-process, has a real
//! tonic client subscribe to `WatchTopology`, and asserts that:
//!
//! 1. The first message on the stream is the current view.
//! 2. After `Membership::set_nodes`, the next stream message carries
//!    the updated generation + nodes.
//! 3. The brokkr-cas `Router`, fed from the stream, picks the same
//!    primary for a digest that an in-process `Router` built from
//!    the same topology would pick — i.e. the proto round-trip is
//!    lossless.

#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::time::Duration;

use brokkr_cas::{NodeStatus, RingNode, Router as CasRouter, Topology as CasTopology};
use brokkr_common::Digest;
use brokkr_control::{Membership, MembershipServiceImpl};
use brokkr_proto::brokkr_v1::{
    self as bv1, membership_service_client::MembershipServiceClient,
    membership_service_server::MembershipServiceServer,
};
use tokio::net::TcpListener;
use tokio_stream::StreamExt as _;
use tonic::transport::Server;

fn proto_node(id: &str) -> bv1::CasNode {
    bv1::CasNode {
        node_id: id.to_string(),
        endpoint: format!("http://{id}:7980"),
        status: bv1::NodeStatus::Healthy as i32,
        capacity_bytes: 0,
        used_bytes: 0,
    }
}

async fn boot_membership_service(m: Membership) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let svc = MembershipServiceImpl::new(m);
    tokio::spawn(async move {
        Server::builder()
            .add_service(MembershipServiceServer::new(svc))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    format!("http://{addr}")
}

#[tokio::test]
async fn watch_topology_streams_current_view_then_updates() {
    let membership = Membership::new(2);
    membership.set_nodes(vec![proto_node("a"), proto_node("b")]);
    // Generation should now be 2 (1 → 2 on the first set_nodes).
    let endpoint = boot_membership_service(membership.clone()).await;

    let mut client = MembershipServiceClient::connect(endpoint).await.unwrap();
    let mut stream = client
        .watch_topology(bv1::WatchTopologyRequest {})
        .await
        .unwrap()
        .into_inner();

    // (1) First message on connect = current view.
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.generation, 2);
    assert_eq!(first.nodes.len(), 2);
    assert_eq!(first.replication_factor, 2);

    // (2) Mutate the membership; the next message should arrive
    // with the bumped generation.
    membership.set_nodes(vec![proto_node("a"), proto_node("b"), proto_node("c")]);
    let second = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(second.generation, 3);
    assert_eq!(second.nodes.len(), 3);
}

#[tokio::test]
async fn router_routes_consistently_with_local_topology() {
    let membership = Membership::new(2);
    membership.set_nodes(vec![
        proto_node("a"),
        proto_node("b"),
        proto_node("c"),
        proto_node("d"),
    ]);
    let endpoint = boot_membership_service(membership.clone()).await;
    let mut client = MembershipServiceClient::connect(endpoint).await.unwrap();
    let view = client
        .watch_topology(bv1::WatchTopologyRequest {})
        .await
        .unwrap()
        .into_inner()
        .next()
        .await
        .unwrap()
        .unwrap();

    // Convert the proto TopologyView → brokkr-cas Topology and
    // assert the resulting router agrees with one built directly.
    let topology = CasTopology {
        generation: view.generation,
        nodes: view
            .nodes
            .iter()
            .map(|n| RingNode {
                node_id: n.node_id.clone(),
                endpoint: n.endpoint.clone(),
                status: match bv1::NodeStatus::try_from(n.status).unwrap_or_default() {
                    bv1::NodeStatus::Healthy => NodeStatus::Healthy,
                    bv1::NodeStatus::Suspect => NodeStatus::Suspect,
                    _ => NodeStatus::Unreachable,
                },
            })
            .collect(),
        replication_factor: view.replication_factor,
    };
    let router_from_stream = CasRouter::new(topology);

    let direct = CasTopology {
        generation: view.generation,
        nodes: vec![
            RingNode {
                node_id: "a".into(),
                endpoint: "http://a:7980".into(),
                status: NodeStatus::Healthy,
            },
            RingNode {
                node_id: "b".into(),
                endpoint: "http://b:7980".into(),
                status: NodeStatus::Healthy,
            },
            RingNode {
                node_id: "c".into(),
                endpoint: "http://c:7980".into(),
                status: NodeStatus::Healthy,
            },
            RingNode {
                node_id: "d".into(),
                endpoint: "http://d:7980".into(),
                status: NodeStatus::Healthy,
            },
        ],
        replication_factor: 2,
    };
    let router_direct = CasRouter::new(direct);

    for i in 0..32 {
        let d = Digest::of(format!("router-rt-{i}").as_bytes());
        let from_stream: Vec<String> = router_from_stream
            .primary_replicas_for(&d)
            .into_iter()
            .map(|n| n.node_id)
            .collect();
        let from_direct: Vec<String> = router_direct
            .primary_replicas_for(&d)
            .into_iter()
            .map(|n| n.node_id)
            .collect();
        assert_eq!(
            from_stream, from_direct,
            "router disagrees with direct topology for digest {d:?}",
        );
    }
}
