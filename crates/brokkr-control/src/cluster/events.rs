//! Turning successive snapshots into a delta stream (ADR 0012).
//!
//! [`diff`] is pure, so the entire event contract is testable without a timer,
//! a socket, or a subscriber.
//!
//! # What "immediate" can and cannot mean here
//!
//! `PeerObservability.GetLocalState` is unary, so nothing a *peer* observes can
//! reach an operator faster than the next poll. That is fine for the one case
//! it affects, and not a limitation at all for the other:
//!
//! - **Leadership change is immediate everywhere.** When leadership moves,
//!   *every* node observes it in its own Raft state — it starts an election, or
//!   accepts `AppendEntries` from a new leader. No peer push is required.
//! - **Policy quarantine is immediate locally, one poll interval remotely.** A
//!   node learns its own policy was quarantined at once. A quarantine on a
//!   *different* node surfaces on the next poll. Accepted: it is a
//!   placement-quality signal, not an availability one, and buying that latency
//!   would cost the streaming peer transport the poller design exists to avoid.

use super::aggregate::ClusterSnapshot;

/// Something that changed between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterEvent {
    /// A full replacement of the subscriber's world.
    ///
    /// Sent on subscribe, on reconnect, and whenever a subscriber falls behind.
    /// Carrying the whole state rather than a cursor means a client needs no
    /// reconciliation logic and cannot silently miss deltas.
    Snapshot(Box<ClusterSnapshot>),
    /// A known node stopped answering.
    NodeUnreachable {
        /// The node that stopped answering.
        node_id: String,
    },
    /// A previously silent node answered again.
    NodeRecovered {
        /// The node that answered again.
        node_id: String,
    },
    /// A worker appeared in some node's registry.
    WorkerAdded {
        /// The worker that appeared.
        worker_id: String,
        /// The node whose registry it appeared in.
        owning_node: String,
    },
    /// A worker left some node's registry.
    WorkerRemoved {
        /// The worker that left.
        worker_id: String,
        /// The node whose registry it left.
        owning_node: String,
    },
    /// A worker passed its heartbeat deadline.
    WorkerStale {
        /// The worker that went stale.
        worker_id: String,
        /// The node whose registry holds it.
        owning_node: String,
    },
    /// A node's scheduling policy was quarantined after repeated failures.
    PolicyQuarantined {
        /// The node whose policy was quarantined.
        owning_node: String,
    },
    /// A node's scheduling policy left quarantine, normally via a reload.
    PolicyRecovered {
        /// The node whose policy recovered.
        owning_node: String,
    },
    /// Leadership moved. Either side may be `None` — losing the leader
    /// entirely is a leadership change, and is the case an operator most needs
    /// pushed at them.
    LeaderChanged {
        /// Who was leading, if anyone.
        from: Option<String>,
        /// Who leads now, if anyone.
        to: Option<String>,
    },
}

impl ClusterEvent {
    /// A stable lowercase tag, for the wire and for logs.
    ///
    /// Free of interpolated detail so metric label cardinality stays bounded,
    /// the same rule `PolicyFailure::reason` follows.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Snapshot(_) => "snapshot",
            Self::NodeUnreachable { .. } => "node_unreachable",
            Self::NodeRecovered { .. } => "node_recovered",
            Self::WorkerAdded { .. } => "worker_added",
            Self::WorkerRemoved { .. } => "worker_removed",
            Self::WorkerStale { .. } => "worker_stale",
            Self::PolicyQuarantined { .. } => "policy_quarantined",
            Self::PolicyRecovered { .. } => "policy_recovered",
            Self::LeaderChanged { .. } => "leader_changed",
        }
    }
}

/// Everything that changed between `prev` and `next`.
///
/// Emitted in a fixed order — nodes, workers, policy, leadership — so two
/// identical transitions produce identical streams. Identical snapshots produce
/// nothing at all: without that, a 2s poller re-emits the entire world every
/// tick and the stream is noise rather than signal.
pub fn diff(prev: &ClusterSnapshot, next: &ClusterSnapshot) -> Vec<ClusterEvent> {
    let mut out = Vec::new();

    // Nodes: reachability transitions only. A node appearing or disappearing
    // from the configured set is a membership change, which the Raft layer
    // owns and which shows up here as reachability anyway.
    for n in &next.nodes {
        match prev.nodes.iter().find(|p| p.node_id == n.node_id) {
            Some(p) if p.reachable && !n.reachable => out.push(ClusterEvent::NodeUnreachable {
                node_id: n.node_id.clone(),
            }),
            Some(p) if !p.reachable && n.reachable => out.push(ClusterEvent::NodeRecovered {
                node_id: n.node_id.clone(),
            }),
            _ => {}
        }
    }

    // Workers: added, removed, and newly stale. Staleness is reported once, on
    // the transition — repeating it every poll would bury everything else.
    for w in &next.workers {
        match prev.workers.iter().find(|p| p.worker_id == w.worker_id) {
            None => out.push(ClusterEvent::WorkerAdded {
                worker_id: w.worker_id.clone(),
                owning_node: w.owning_node.clone(),
            }),
            Some(p) if !p.stale && w.stale => out.push(ClusterEvent::WorkerStale {
                worker_id: w.worker_id.clone(),
                owning_node: w.owning_node.clone(),
            }),
            _ => {}
        }
    }
    for w in &prev.workers {
        if !next.workers.iter().any(|n| n.worker_id == w.worker_id) {
            out.push(ClusterEvent::WorkerRemoved {
                worker_id: w.worker_id.clone(),
                owning_node: w.owning_node.clone(),
            });
        }
    }

    // Policy: quarantine transitions, both directions. Recovery matters as
    // much as onset — without it a console shows a quarantine banner that
    // never clears.
    for p in &next.policies {
        if let Some(before) = prev
            .policies
            .iter()
            .find(|b| b.owning_node == p.owning_node)
        {
            if !before.quarantined && p.quarantined {
                out.push(ClusterEvent::PolicyQuarantined {
                    owning_node: p.owning_node.clone(),
                });
            } else if before.quarantined && !p.quarantined {
                out.push(ClusterEvent::PolicyRecovered {
                    owning_node: p.owning_node.clone(),
                });
            }
        }
    }

    // Leadership last, so an operator reading the stream sees what moved
    // before being told who leads now.
    if prev.leader_id != next.leader_id {
        out.push(ClusterEvent::LeaderChanged {
            from: prev.leader_id.clone(),
            to: next.leader_id.clone(),
        });
    }

    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::views::{CasStatsView, NodeView, PolicyView, RaftRole, WorkerView};

    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn node(id: &str, role: RaftRole, reachable: bool) -> NodeView {
        NodeView {
            node_id: id.to_string(),
            advertise_addr: format!("{id}:7878"),
            role,
            term: 7,
            commit_index: 42,
            last_applied: 42,
            reachable,
            last_seen_secs: 0,
        }
    }

    fn worker(id: &str, owner: &str, stale: bool) -> WorkerView {
        WorkerView {
            worker_id: id.to_string(),
            hostname: id.to_string(),
            labels: BTreeMap::new(),
            inflight: 0,
            last_seen_secs: 1,
            stale,
            owning_node: owner.to_string(),
        }
    }

    fn policy(owner: &str, quarantined: bool) -> PolicyView {
        PolicyView {
            loaded: true,
            quarantined,
            decided: 0,
            declined: 0,
            failures_by_reason: BTreeMap::new(),
            owning_node: owner.to_string(),
        }
    }

    fn snap(
        nodes: Vec<NodeView>,
        workers: Vec<WorkerView>,
        policies: Vec<PolicyView>,
        leader: Option<&str>,
    ) -> ClusterSnapshot {
        ClusterSnapshot {
            nodes,
            workers,
            policies,
            cas: vec![CasStatsView {
                objects: 0,
                bytes: 0,
                owning_node: "node-1".to_string(),
            }],
            jobs: Vec::new(),
            leader_id: leader.map(|s| s.to_string()),
            degraded: false,
            as_of: Some(at()),
        }
    }

    fn base() -> ClusterSnapshot {
        snap(
            vec![node("node-1", RaftRole::Leader, true)],
            vec![worker("w-a", "node-1", false)],
            vec![policy("node-1", false)],
            Some("node-1"),
        )
    }

    /// Two identical snapshots produce nothing. Without this a 2s poller
    /// re-emits the entire world every tick and the stream is noise.
    #[test]
    fn identical_snapshots_produce_no_events() {
        assert!(diff(&base(), &base()).is_empty());
    }

    #[test]
    fn a_new_worker_produces_worker_added() {
        let mut next = base();
        next.workers.push(worker("w-b", "node-1", false));
        assert_eq!(
            diff(&base(), &next),
            vec![ClusterEvent::WorkerAdded {
                worker_id: "w-b".to_string(),
                owning_node: "node-1".to_string(),
            }]
        );
    }

    #[test]
    fn a_missing_worker_produces_worker_removed() {
        let mut next = base();
        next.workers.clear();
        assert_eq!(
            diff(&base(), &next),
            vec![ClusterEvent::WorkerRemoved {
                worker_id: "w-a".to_string(),
                owning_node: "node-1".to_string(),
            }]
        );
    }

    /// Staleness is reported once, on the transition. Repeating it every poll
    /// would bury everything else in the stream.
    #[test]
    fn a_worker_going_stale_produces_one_event_not_one_per_poll() {
        let mut next = base();
        next.workers[0].stale = true;
        assert_eq!(
            diff(&base(), &next),
            vec![ClusterEvent::WorkerStale {
                worker_id: "w-a".to_string(),
                owning_node: "node-1".to_string(),
            }]
        );
        // Still stale on the following poll: no repeat.
        assert!(diff(&next, &next).is_empty());
    }

    #[test]
    fn a_node_becoming_unreachable_produces_node_unreachable() {
        let prev = snap(
            vec![
                node("node-1", RaftRole::Leader, true),
                node("node-2", RaftRole::Follower, true),
            ],
            Vec::new(),
            Vec::new(),
            Some("node-1"),
        );
        let mut next = prev.clone();
        next.nodes[1].reachable = false;
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::NodeUnreachable {
                node_id: "node-2".to_string(),
            }]
        );
    }

    #[test]
    fn a_node_recovering_produces_node_recovered() {
        let mut prev = snap(
            vec![
                node("node-1", RaftRole::Leader, true),
                node("node-2", RaftRole::Follower, true),
            ],
            Vec::new(),
            Vec::new(),
            Some("node-1"),
        );
        prev.nodes[1].reachable = false;
        let mut next = prev.clone();
        next.nodes[1].reachable = true;
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::NodeRecovered {
                node_id: "node-2".to_string(),
            }]
        );
    }

    #[test]
    fn a_leadership_change_produces_leader_changed() {
        let prev = base();
        let mut next = base();
        next.nodes[0].role = RaftRole::Follower;
        next.leader_id = Some("node-2".to_string());
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::LeaderChanged {
                from: Some("node-1".to_string()),
                to: Some("node-2".to_string()),
            }]
        );
    }

    /// Losing the leader entirely is a leadership change too, and is the case
    /// an operator most needs pushed at them.
    #[test]
    fn losing_the_leader_produces_leader_changed_to_none() {
        let prev = base();
        let mut next = base();
        next.nodes[0].role = RaftRole::Unknown;
        next.leader_id = None;
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::LeaderChanged {
                from: Some("node-1".to_string()),
                to: None,
            }]
        );
    }

    #[test]
    fn a_policy_becoming_quarantined_produces_policy_quarantined() {
        let prev = base();
        let mut next = base();
        next.policies[0].quarantined = true;
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::PolicyQuarantined {
                owning_node: "node-1".to_string(),
            }]
        );
    }

    /// Recovery matters as much as onset — without it a console shows a
    /// quarantine banner that never clears.
    #[test]
    fn a_policy_leaving_quarantine_produces_policy_recovered() {
        let mut prev = base();
        prev.policies[0].quarantined = true;
        let next = base();
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::PolicyRecovered {
                owning_node: "node-1".to_string(),
            }]
        );
    }

    /// Events are emitted in a fixed order — nodes, workers, policy,
    /// leadership — so two identical transitions produce identical streams.
    #[test]
    fn events_are_ordered_deterministically() {
        let prev = snap(
            vec![
                node("node-1", RaftRole::Leader, true),
                node("node-2", RaftRole::Follower, true),
            ],
            vec![worker("w-a", "node-1", false)],
            vec![policy("node-1", false)],
            Some("node-1"),
        );
        let mut next = prev.clone();
        next.nodes[1].reachable = false;
        next.workers.clear();
        next.policies[0].quarantined = true;
        next.nodes[0].role = RaftRole::Unknown;
        next.leader_id = None;

        let events = diff(&prev, &next);
        let kinds: Vec<&str> = events.iter().map(ClusterEvent::kind).collect();
        assert_eq!(
            kinds,
            vec![
                "node_unreachable",
                "worker_removed",
                "policy_quarantined",
                "leader_changed"
            ],
            "events must be emitted nodes -> workers -> policy -> leadership"
        );
        assert_eq!(diff(&prev, &next), events, "and the same transition twice");
    }

    #[test]
    fn event_kinds_are_distinct_and_detail_free() {
        let all = [
            ClusterEvent::Snapshot(Box::new(base())),
            ClusterEvent::NodeUnreachable {
                node_id: "n".into(),
            },
            ClusterEvent::NodeRecovered {
                node_id: "n".into(),
            },
            ClusterEvent::WorkerAdded {
                worker_id: "w".into(),
                owning_node: "n".into(),
            },
            ClusterEvent::WorkerRemoved {
                worker_id: "w".into(),
                owning_node: "n".into(),
            },
            ClusterEvent::WorkerStale {
                worker_id: "w".into(),
                owning_node: "n".into(),
            },
            ClusterEvent::PolicyQuarantined {
                owning_node: "n".into(),
            },
            ClusterEvent::PolicyRecovered {
                owning_node: "n".into(),
            },
            ClusterEvent::LeaderChanged {
                from: None,
                to: None,
            },
        ];
        let mut kinds: Vec<&str> = all.iter().map(ClusterEvent::kind).collect();
        let n = kinds.len();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), n, "event kinds must be unique");
        assert!(
            all.iter()
                .all(|e| !e.kind().contains(|c: char| c.is_uppercase())),
            "tags are lowercase and carry no interpolated detail"
        );
    }
}
