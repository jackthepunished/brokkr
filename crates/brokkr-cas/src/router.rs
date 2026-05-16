//! Client-side router for the Phase 3 distributed CAS.
//!
//! Holds the current topology view (subscribed from the control
//! plane's `MembershipService.WatchTopology`) and exposes
//! convenience methods to enumerate the responsible replicas for a
//! given digest. The router does *not* issue any RPCs itself — that
//! is the caller's job. Decoupling keeps the routing logic testable
//! in isolation.
//!
//! The router is intentionally tiny: every `replicas_for_*` call
//! re-runs the HRW computation. At Phase 3's N (single-digit nodes)
//! this is microseconds; caching the top-R per-digest would be a
//! premature optimisation.
//!
//! ## Topology lifecycle
//!
//! A `Router` is created from a `TopologyView` snapshot. As updates
//! arrive on the gRPC stream, the caller invokes
//! [`Router::update_topology`] — the router atomically swaps its
//! internal view (no locks on the read path beyond an `RwLock`
//! shared read guard).

use std::sync::RwLock;

use brokkr_common::Digest;

use crate::ring::{replicas_for, RingNode};

/// Snapshot of the cluster topology used by the router. Decoupled
/// from the `brokkr.v1.TopologyView` proto so this module compiles
/// without the proto crate; callers (e.g. the worker) translate the
/// proto into this type at the boundary.
#[derive(Debug, Clone, Default)]
pub struct Topology {
    /// Monotonic generation number; bumped on every change.
    pub generation: u64,
    /// All known CAS nodes.
    pub nodes: Vec<RingNode>,
    /// Default replication factor.
    pub replication_factor: u32,
}

impl Topology {
    /// Number of eligible (Healthy / Suspect) nodes.
    pub fn eligible_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.status.is_eligible()).count()
    }
}

/// Client-side router that watches the topology view and picks
/// replicas per digest using rendezvous hashing.
///
/// Cheap to clone via `Arc` if you need to share it across tasks.
#[derive(Debug)]
pub struct Router {
    inner: RwLock<Topology>,
}

impl Router {
    /// Build a router with the given initial topology.
    pub fn new(initial: Topology) -> Self {
        Self {
            inner: RwLock::new(initial),
        }
    }

    /// Atomically swap the router's topology view. Returns the
    /// previous generation so the caller can log "view updated:
    /// G_old → G_new".
    pub fn update_topology(&self, next: Topology) -> u64 {
        let mut guard = match self.inner.write() {
            Ok(g) => g,
            // RwLock poisoning means a prior writer panicked — for a
            // pure-data swap there's nothing dangerous left over;
            // recover by overwriting with the new view.
            Err(poisoned) => poisoned.into_inner(),
        };
        let prev = guard.generation;
        *guard = next;
        prev
    }

    /// Snapshot the current generation. Mostly useful for tests and
    /// diagnostics.
    pub fn generation(&self) -> u64 {
        self.read().generation
    }

    /// Snapshot the configured default replication factor.
    pub fn replication_factor(&self) -> u32 {
        self.read().replication_factor
    }

    /// Return the top `r` replicas for `digest`, primary-first. See
    /// [`crate::ring::replicas_for`] for ordering semantics.
    pub fn replicas_for(&self, digest: &Digest, r: usize) -> Vec<RingNode> {
        let view = self.read();
        replicas_for(digest, &view.nodes, r)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Convenience: replicas at the router's configured
    /// `replication_factor`.
    pub fn primary_replicas_for(&self, digest: &Digest) -> Vec<RingNode> {
        let view = self.read();
        let r = view.replication_factor as usize;
        replicas_for(digest, &view.nodes, r)
            .into_iter()
            .cloned()
            .collect()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Topology> {
        match self.inner.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::NodeStatus;

    fn node(id: &str) -> RingNode {
        RingNode {
            node_id: id.to_string(),
            endpoint: format!("http://{id}:7980"),
            status: NodeStatus::Healthy,
        }
    }

    fn digest(s: &str) -> Digest {
        Digest::of(s.as_bytes())
    }

    fn topo(gen: u64, r: u32, ids: &[&str]) -> Topology {
        Topology {
            generation: gen,
            nodes: ids.iter().map(|i| node(i)).collect(),
            replication_factor: r,
        }
    }

    #[test]
    fn router_returns_configured_replication_factor() {
        let r = Router::new(topo(1, 2, &["a", "b", "c"]));
        let replicas = r.primary_replicas_for(&digest("blob"));
        assert_eq!(replicas.len(), 2);
    }

    #[test]
    fn router_swaps_topology_atomically() {
        let r = Router::new(topo(1, 2, &["a", "b"]));
        assert_eq!(r.generation(), 1);
        assert_eq!(r.update_topology(topo(2, 2, &["a", "b", "c"])), 1);
        assert_eq!(r.generation(), 2);
        // The new node is in the pool now.
        let mut seen = std::collections::HashSet::new();
        for i in 0..32 {
            let d = digest(&format!("x-{i}"));
            for n in r.primary_replicas_for(&d) {
                seen.insert(n.node_id);
            }
        }
        assert!(
            seen.contains("c"),
            "node c not reachable after topology swap"
        );
    }

    #[test]
    fn router_handles_empty_topology() {
        let r = Router::new(Topology::default());
        let replicas = r.primary_replicas_for(&digest("blob"));
        assert!(replicas.is_empty());
    }

    #[test]
    fn router_respects_status() {
        let mut t = topo(1, 2, &["a", "b", "c"]);
        t.nodes[1].status = NodeStatus::Unreachable;
        let r = Router::new(t);
        for i in 0..32 {
            let d = digest(&format!("y-{i}"));
            for n in r.primary_replicas_for(&d) {
                assert_ne!(n.node_id, "b", "Unreachable node b was selected");
            }
        }
    }
}
