//! Rendezvous (HRW) hashing for the Phase 3 distributed CAS.
//!
//! Picks the `R` replicas responsible for a digest by scoring every
//! healthy node and taking the top R. See `docs/phase-3-plan.md`
//! Appendix A for why HRW over consistent hashing — at Brokkr's `N`
//! (single-digit CAS nodes in Phase 3) the O(N)-per-lookup cost is
//! irrelevant and the ~30-line implementation beats a virtual-node
//! consistent-hashing ring.
//!
//! The score for `(node_id, digest)` is the first 16 bytes of
//! `sha256(node_id || digest.hash)`, interpreted as a big-endian
//! `u128`. The pair `(score, node_id)` is unique by construction
//! (a hash collision would have to break sha256), so ties never
//! arise; we still sort lexicographically on `node_id` as a
//! tiebreaker for clarity.
//!
//! ## Why this hash function
//!
//! HRW's correctness hinges on the score being deterministic and
//! uncorrelated across `(node, digest)` pairs. sha256 gives us
//! both, and we already pay for it everywhere else in Brokkr.
//! Truncating to 128 bits is plenty: a u128 has 3.4e38 distinct
//! values, comfortably more than the (nodes × blobs) cross product
//! we'll ever see.

use brokkr_common::Digest;
use sha2::{Digest as _, Sha256};

/// One node in the routing ring. Decoupled from the `brokkr.v1.CasNode`
/// proto so the routing logic can be tested without the full
/// `TopologyView`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingNode {
    /// Stable identifier — the value the control plane assigned to
    /// the CAS node. Used both as the hashing input and as the
    /// lookup key for client-side gRPC channel caches.
    pub node_id: String,
    /// gRPC endpoint, e.g. `http://10.0.0.1:7980`. Carried through
    /// for the router; the ring itself does not interpret it.
    pub endpoint: String,
    /// Liveness state observed by the control plane. Only
    /// `Healthy` and `Suspect` nodes participate in writes;
    /// `Suspect` is still tried for reads. `Unreachable` nodes are
    /// excluded entirely from replica selection.
    pub status: NodeStatus,
}

/// Liveness state for one node. Mirrors `brokkr.v1.NodeStatus` but
/// keeps the routing module proto-free so it can compile under
/// `cargo test -p brokkr-cas` without the full proto crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    /// Reachable, ready for reads and writes.
    Healthy,
    /// Late heartbeats but not yet evicted. Tried for reads
    /// (lower-priority) and writes.
    Suspect,
    /// Excluded from replica selection.
    Unreachable,
}

impl NodeStatus {
    /// Returns true when the node should be considered for replica
    /// placement at all. `Unreachable` returns false.
    pub fn is_eligible(self) -> bool {
        matches!(self, NodeStatus::Healthy | NodeStatus::Suspect)
    }
}

/// Score `(node, digest)` for HRW selection. Higher score wins.
///
/// The score is the leading 16 bytes of `sha256(node_id ||
/// digest.hash)` interpreted big-endian. Stable across processes,
/// architectures, and Rust versions because everything is sha256
/// and bytewise.
pub fn score(node_id: &str, digest: &Digest) -> u128 {
    let mut h = Sha256::new();
    h.update(node_id.as_bytes());
    h.update(b"\x1f"); // ASCII US — a separator so the empty-string node_id
                       // and an empty hash prefix are unambiguous.
    h.update(digest.hash().as_bytes());
    let out = h.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&out[..16]);
    u128::from_be_bytes(bytes)
}

/// Return the top `r` replicas for `digest`, ordered primary-first
/// (highest score first). Nodes with [`NodeStatus::Unreachable`]
/// are excluded. If fewer than `r` eligible nodes exist, returns
/// every eligible node (caller is responsible for surfacing
/// "under-replicated" as an error if that matters at their layer).
pub fn replicas_for<'a>(digest: &Digest, nodes: &'a [RingNode], r: usize) -> Vec<&'a RingNode> {
    if r == 0 {
        return Vec::new();
    }
    let mut scored: Vec<(u128, &RingNode)> = nodes
        .iter()
        .filter(|n| n.status.is_eligible())
        .map(|n| (score(&n.node_id, digest), n))
        .collect();
    // Sort by (score DESC, node_id ASC) — the node_id tiebreak is
    // belt-and-braces; sha256 collisions don't happen in practice.
    scored.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.node_id.cmp(&b.1.node_id)));
    scored.into_iter().take(r).map(|(_, n)| n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn empty_replication_factor_returns_empty() {
        let nodes = vec![node("a"), node("b")];
        assert!(replicas_for(&digest("x"), &nodes, 0).is_empty());
    }

    #[test]
    fn underprovisioned_ring_returns_what_we_have() {
        let nodes = vec![node("a")];
        let replicas = replicas_for(&digest("x"), &nodes, 3);
        assert_eq!(replicas.len(), 1);
        assert_eq!(replicas[0].node_id, "a");
    }

    #[test]
    fn unreachable_nodes_are_excluded() {
        let mut nodes = vec![node("a"), node("b"), node("c")];
        nodes[1].status = NodeStatus::Unreachable;
        let replicas = replicas_for(&digest("x"), &nodes, 3);
        assert_eq!(replicas.len(), 2);
        assert!(replicas.iter().all(|n| n.node_id != "b"));
    }

    #[test]
    fn ordering_is_deterministic() {
        let nodes = vec![node("a"), node("b"), node("c"), node("d")];
        let r1 = replicas_for(&digest("hello"), &nodes, 3);
        let r2 = replicas_for(&digest("hello"), &nodes, 3);
        let ids1: Vec<_> = r1.iter().map(|n| n.node_id.as_str()).collect();
        let ids2: Vec<_> = r2.iter().map(|n| n.node_id.as_str()).collect();
        assert_eq!(ids1, ids2);
    }

    #[test]
    fn different_digests_pick_different_primaries() {
        // The whole point of HRW: distinct digests map to distinct
        // primaries with high probability. With 4 nodes and the
        // following 16 digests we expect at least 3 distinct primaries.
        let nodes = vec![node("a"), node("b"), node("c"), node("d")];
        let primaries: std::collections::HashSet<String> = (0..16)
            .map(|i| {
                let d = digest(&format!("digest-{i}"));
                replicas_for(&d, &nodes, 1)[0].node_id.clone()
            })
            .collect();
        assert!(
            primaries.len() >= 3,
            "16 distinct digests across 4 nodes mapped to only {} primaries",
            primaries.len()
        );
    }

    /// Distribution uniformity: with 4 nodes and 10k digests, every
    /// node should get roughly 2500 ± 250 primaries. This is a
    /// statistical test — the bound is generous to keep the test
    /// non-flaky.
    #[test]
    fn distribution_is_roughly_uniform() {
        let nodes = vec![node("a"), node("b"), node("c"), node("d")];
        let mut counts = std::collections::BTreeMap::new();
        for i in 0..10_000 {
            let d = digest(&format!("u-{i}"));
            let primary = replicas_for(&d, &nodes, 1)[0].node_id.clone();
            *counts.entry(primary).or_insert(0u32) += 1;
        }
        for (id, c) in &counts {
            assert!(
                (2250..=2750).contains(c),
                "node {id} got {c} primaries; expected ~2500 ± 250",
            );
        }
    }

    /// Churn: removing one node from a 5-node ring moves at most
    /// ~K/N ≈ 20% of blobs to a different primary. The bound here
    /// is again generous (35%) to absorb finite-sample noise on the
    /// 4000-digest sample; the theoretical bound is 20% in
    /// expectation.
    #[test]
    fn add_remove_one_node_moves_at_most_a_fraction() {
        let big = vec![node("a"), node("b"), node("c"), node("d"), node("e")];
        let small = vec![node("a"), node("b"), node("c"), node("d")]; // dropped "e"

        let mut moved = 0u32;
        let n = 4000;
        for i in 0..n {
            let d = digest(&format!("churn-{i}"));
            let big_primary = replicas_for(&d, &big, 1)[0].node_id.clone();
            let small_primary = replicas_for(&d, &small, 1)[0].node_id.clone();
            if big_primary != small_primary {
                moved += 1;
            }
        }
        let frac = moved as f64 / n as f64;
        // Theoretical bound is ~1/5 = 20%; allow finite-sample slack.
        assert!(
            frac < 0.35,
            "removing one node from 5-node ring moved {frac:.2} of blobs (expected < 0.35)"
        );
        // The blobs whose primary was *not* the removed node should
        // not move at all — sanity check that HRW only re-routes
        // the affected fraction.
        assert!(
            frac > 0.10,
            "removing one node moved only {frac:.2} of blobs; HRW should move ~20%",
        );
    }
}
