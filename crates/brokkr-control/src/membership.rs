//! Cluster membership for Phase 3 distributed CAS.
//!
//! Holds the authoritative list of `CasNode`s plus the ring
//! configuration (replication factor for now; ring secret reserved
//! for Phase 6+), and exposes the `brokkr.v1.MembershipService.WatchTopology`
//! RPC: a long-lived server-streamed RPC that publishes the current
//! `TopologyView` immediately on connect, then a new view every time
//! the cluster state changes.
//!
//! Phase 3 keeps the registry read-only via gRPC. Operators add /
//! remove nodes via the [`Membership`] handle (called from
//! configuration loading in `brokkr-control`'s binary, or from a
//! future admin RPC). The membership service only publishes.

use std::pin::Pin;
use std::sync::Arc;

use brokkr_proto::brokkr_v1::{
    self as bv1, membership_service_server::MembershipService, TopologyView, WatchTopologyRequest,
};
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::{Stream, StreamExt as _};
use tonic::{Request, Response, Status};

/// In-process handle to the cluster membership. Clone-cheap: holds a
/// `watch::Sender` that every subscribed client tails.
///
/// The handle owns the topology generation counter; every mutation
/// bumps it. Subscribers see the new view via the `WatchTopology`
/// stream (and can also pull a snapshot synchronously via
/// [`Membership::current`] for testing).
#[derive(Debug, Clone)]
pub struct Membership {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    tx: watch::Sender<TopologyView>,
}

impl Membership {
    /// Create an empty membership with the given default replication
    /// factor. Generation 1.
    pub fn new(replication_factor: u32) -> Self {
        let initial = TopologyView {
            generation: 1,
            nodes: Vec::new(),
            replication_factor,
            ring_secret: Vec::new(),
        };
        let (tx, _rx) = watch::channel(initial);
        Self {
            inner: Arc::new(Inner { tx }),
        }
    }

    /// Snapshot the current topology view. Cheap; clones the
    /// `TopologyView` proto.
    pub fn current(&self) -> TopologyView {
        self.inner.tx.borrow().clone()
    }

    /// Subscribe to topology changes. The returned stream yields the
    /// current view immediately, then a new value on every mutation.
    pub fn subscribe(&self) -> WatchStream<TopologyView> {
        WatchStream::new(self.inner.tx.subscribe())
    }

    /// Replace the cluster's node list. Bumps the generation iff the
    /// effective view changed (same generation otherwise — avoids
    /// pointless stream wakeups when an admin re-applies the same
    /// configuration). Returns the new generation.
    pub fn set_nodes(&self, nodes: Vec<bv1::CasNode>) -> u64 {
        self.inner.tx.send_if_modified(|view| {
            if view.nodes == nodes {
                false
            } else {
                view.generation = view.generation.saturating_add(1);
                view.nodes = nodes;
                true
            }
        });
        self.inner.tx.borrow().generation
    }

    /// Update the default replication factor. Bumps the generation
    /// iff the value changed.
    pub fn set_replication_factor(&self, r: u32) -> u64 {
        self.inner.tx.send_if_modified(|view| {
            if view.replication_factor == r {
                false
            } else {
                view.generation = view.generation.saturating_add(1);
                view.replication_factor = r;
                true
            }
        });
        self.inner.tx.borrow().generation
    }
}

/// gRPC implementation of `brokkr.v1.MembershipService`. Wraps a
/// [`Membership`] handle and serves its current view + updates.
#[derive(Debug, Clone)]
pub struct MembershipServiceImpl {
    membership: Membership,
}

impl MembershipServiceImpl {
    /// Bind the gRPC service to a [`Membership`] handle.
    pub fn new(membership: Membership) -> Self {
        Self { membership }
    }
}

type WatchStreamItem = Result<TopologyView, Status>;
type WatchTopologyStream = Pin<Box<dyn Stream<Item = WatchStreamItem> + Send + 'static>>;

#[tonic::async_trait]
impl MembershipService for MembershipServiceImpl {
    type WatchTopologyStream = WatchTopologyStream;

    async fn watch_topology(
        &self,
        _req: Request<WatchTopologyRequest>,
    ) -> Result<Response<Self::WatchTopologyStream>, Status> {
        let stream = self.membership.subscribe().map(Ok);
        Ok(Response::new(Box::pin(stream) as WatchTopologyStream))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn node(id: &str, endpoint: &str) -> bv1::CasNode {
        bv1::CasNode {
            node_id: id.to_string(),
            endpoint: endpoint.to_string(),
            status: bv1::NodeStatus::Healthy as i32,
            capacity_bytes: 0,
            used_bytes: 0,
        }
    }

    #[test]
    fn new_membership_has_generation_one_and_no_nodes() {
        let m = Membership::new(2);
        let v = m.current();
        assert_eq!(v.generation, 1);
        assert!(v.nodes.is_empty());
        assert_eq!(v.replication_factor, 2);
    }

    #[test]
    fn setting_nodes_bumps_generation() {
        let m = Membership::new(2);
        assert_eq!(m.set_nodes(vec![node("a", "http://a")]), 2);
        assert_eq!(m.current().nodes.len(), 1);
    }

    #[test]
    fn setting_same_nodes_does_not_bump_generation() {
        let m = Membership::new(2);
        m.set_nodes(vec![node("a", "http://a")]);
        assert_eq!(
            m.set_nodes(vec![node("a", "http://a")]),
            2,
            "idempotent re-apply must not bump the generation",
        );
    }

    #[test]
    fn setting_different_replication_factor_bumps_generation() {
        let m = Membership::new(2);
        assert_eq!(m.set_replication_factor(3), 2);
        // Re-apply same value should not bump again.
        assert_eq!(m.set_replication_factor(3), 2);
    }

    #[tokio::test]
    async fn subscribers_observe_updates() {
        let m = Membership::new(2);
        let mut stream = m.subscribe();
        // First yield is the current view.
        let first = stream.next().await.unwrap();
        assert_eq!(first.generation, 1);
        m.set_nodes(vec![node("a", "http://a")]);
        let second = stream.next().await.unwrap();
        assert_eq!(second.generation, 2);
        assert_eq!(second.nodes.len(), 1);
    }
}
