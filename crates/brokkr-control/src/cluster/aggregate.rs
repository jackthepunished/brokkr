//! Pure aggregation of per-node observability state into a cluster view.
//!
//! # The rule that matters: not everything can be combined
//!
//! Aggregation is three different operations depending on what is being
//! aggregated, and an implementation that cannot tell them apart produces
//! confident nonsense — which is worse than the separate per-node views
//! fan-out was meant to replace.
//!
//! | Data | Rule |
//! |---|---|
//! | workers, jobs | **union**, each keeping `owning_node` |
//! | CAS stats, policy counters | **one entry per node**, never combined |
//! | leader, term, quorum | **from Raft**, never from counting replies |
//!
//! Each control-plane node opens its own CAS, so summing `objects` across
//! three nodes reports storage that does not exist. Each runs its own policy
//! engine, so two nodes disagreeing about quarantine is information rather
//! than noise.

use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::RwLock;

use crate::views::{
    unreachable_node_view, CasStatsView, JobSummary, NodeView, PolicyView, RaftRole, WorkerView,
};

/// One node's complete observability state — this node's own, or a peer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeState {
    /// The node itself.
    pub node: NodeView,
    /// Workers in this node's registry.
    pub workers: Vec<WorkerView>,
    /// This node's scheduling-policy state.
    pub policy: PolicyView,
    /// This node's CAS size.
    pub cas: CasStatsView,
    /// Recently completed jobs from this node's history ring.
    pub jobs: Vec<JobSummary>,
}

/// The result of asking one peer for its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerOutcome {
    /// The peer answered.
    Answered(NodeState),
    /// A known cluster member that did not answer within the deadline.
    ///
    /// Carried rather than dropped so the merged view can show the node as
    /// present-but-silent.
    Unreachable {
        /// The peer's Raft node id.
        node_id: String,
        /// The peer's advertised address.
        advertise_addr: String,
    },
}

/// A cluster-wide view, assembled from every node that answered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClusterSnapshot {
    /// Every known node, reachable or not, sorted by id.
    pub nodes: Vec<NodeView>,
    /// Every worker across every node that answered, sorted by id.
    pub workers: Vec<WorkerView>,
    /// One policy view per answering node, sorted by owner. Never combined.
    pub policies: Vec<PolicyView>,
    /// One CAS view per answering node, sorted by owner. Never summed.
    pub cas: Vec<CasStatsView>,
    /// Recently completed jobs across every answering node, newest first.
    ///
    /// Ordered by completion time rather than by id, because that is what
    /// "recent" means and it is the only field that can order records
    /// originating on different nodes.
    pub jobs: Vec<JobSummary>,
    /// The node claiming leadership at the highest term, if exactly one does.
    pub leader_id: Option<String>,
    /// True when any known node was silent, or leadership is ambiguous.
    pub degraded: bool,
    /// When this snapshot was assembled. `None` before the first poll.
    pub as_of: Option<SystemTime>,
}

/// A [`ClusterSnapshot`] behind a lock. The poller is the only writer.
pub type SharedSnapshot = Arc<RwLock<ClusterSnapshot>>;

/// Merge this node's state with its peers' outcomes.
///
/// Deterministic: every output collection is sorted, because peer replies
/// arrive in completion order and an unsorted merge would reorder on every
/// poll.
pub fn merge(local: NodeState, peers: Vec<PeerOutcome>, as_of: SystemTime) -> ClusterSnapshot {
    let mut nodes = vec![local.node];
    let mut workers = local.workers;
    let mut policies = vec![local.policy];
    let mut cas = vec![local.cas];
    let mut jobs = local.jobs;
    let mut any_silent = false;

    for outcome in peers {
        match outcome {
            PeerOutcome::Answered(state) => {
                nodes.push(state.node);
                workers.extend(state.workers);
                policies.push(state.policy);
                cas.push(state.cas);
                jobs.extend(state.jobs);
            }
            PeerOutcome::Unreachable {
                node_id,
                advertise_addr,
            } => {
                any_silent = true;
                nodes.push(unreachable_node_view(&node_id, &advertise_addr));
            }
        }
    }

    nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    workers.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
    policies.sort_by(|a, b| a.owning_node.cmp(&b.owning_node));
    cas.sort_by(|a, b| a.owning_node.cmp(&b.owning_node));
    // Jobs sort by completion time, not by id: "recent" is a time question,
    // and each node keeps its own ring, so this is the only field that can
    // order records from different machines. Union first and limit later —
    // limiting per node before the union would let a burst on one node evict
    // another node's genuinely newer jobs from the result.
    jobs.sort_by(|a, b| {
        b.completed_at_unix_ms
            .cmp(&a.completed_at_unix_ms)
            // Millisecond resolution makes ties likely, and an unstable
            // tie-break would reorder the list between identical calls.
            .then_with(|| a.job_id.cmp(&b.job_id))
    });

    let leader_id = elect(&nodes);
    // A cluster with no consensus configured is not degraded — there is
    // nothing for it to be degraded *from*. Reporting otherwise would make
    // every single-node deployment show a permanent warning and send operators
    // chasing a problem that does not exist.
    let standalone = nodes
        .iter()
        .filter(|n| n.reachable)
        .all(|n| n.role == RaftRole::Standalone);
    ClusterSnapshot {
        nodes,
        workers,
        policies,
        cas,
        jobs,
        degraded: any_silent || (!standalone && leader_id.is_none()),
        leader_id,
        as_of: Some(as_of),
    }
}

/// Determine the cluster's leader from the nodes that answered.
///
/// Reconciled by **term**, not by counting claimants. A node partitioned from
/// the cluster keeps believing it leads at its old term, so "how many nodes
/// claim leadership" is the wrong question — a stale claimant would make a
/// perfectly healthy cluster look ambiguous.
///
/// Only claimants at the highest term among *reachable* nodes count. Raft
/// guarantees a higher term supersedes a lower one, so a lower-term claimant is
/// stale by definition; its own term stays visible in its `NodeView`, it simply
/// does not win here.
fn elect(nodes: &[NodeView]) -> Option<String> {
    let highest_term = nodes
        .iter()
        .filter(|n| n.reachable)
        .map(|n| n.term)
        .max()
        .unwrap_or(0);
    let mut claimants = nodes
        .iter()
        .filter(|n| n.reachable && n.role == RaftRole::Leader && n.term == highest_term);
    match (claimants.next(), claimants.next()) {
        (Some(only), None) => Some(only.node_id.clone()),
        (Some(a), Some(b)) => {
            // Two leaders at the *same* term is impossible under Raft, so this
            // means our view is internally inconsistent — most likely a peer
            // answered mid-transition. Worth an error rather than a warning:
            // it is either a bug here or something genuinely alarming there.
            tracing::error!(
                term = highest_term,
                first = %a.node_id,
                second = %b.node_id,
                "two nodes claim leadership at the same term; reporting no leader"
            );
            None
        }
        // Zero claimants at the highest term: an election is in progress.
        (None, _) => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use super::*;

    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn node(id: &str, role: RaftRole) -> NodeView {
        NodeView {
            node_id: id.to_string(),
            advertise_addr: format!("{id}:7878"),
            role,
            term: 7,
            commit_index: 42,
            last_applied: 42,
            reachable: true,
            last_seen_secs: 0,
        }
    }

    fn worker(id: &str, owner: &str) -> WorkerView {
        WorkerView {
            worker_id: id.to_string(),
            hostname: id.to_string(),
            labels: BTreeMap::new(),
            inflight: 0,
            last_seen_secs: 1,
            stale: false,
            owning_node: owner.to_string(),
        }
    }

    fn policy(owner: &str, quarantined: bool) -> PolicyView {
        PolicyView {
            loaded: true,
            quarantined,
            decided: 5,
            declined: 1,
            failures_by_reason: BTreeMap::new(),
            owning_node: owner.to_string(),
        }
    }

    fn cas(owner: &str) -> CasStatsView {
        CasStatsView {
            objects: 10,
            bytes: 1000,
            owning_node: owner.to_string(),
        }
    }

    fn job(id: &str, owner: &str, completed_at_unix_ms: u64) -> crate::views::JobSummary {
        crate::views::JobSummary {
            job_id: id.to_string(),
            tenant: "t".to_string(),
            action_digest: "a".repeat(64),
            state: crate::views::JobState::Succeeded,
            worker_id: Some("w".to_string()),
            exit_code: Some(0),
            completed_at_unix_ms,
            owning_node: owner.to_string(),
        }
    }

    fn state(id: &str, role: RaftRole, workers: &[&str]) -> NodeState {
        NodeState {
            node: node(id, role),
            workers: workers.iter().map(|w| worker(w, id)).collect(),
            policy: policy(id, false),
            cas: cas(id),
            jobs: Vec::new(),
        }
    }

    /// Jobs from every node merge into one globally-ordered list. Sorting per
    /// node and concatenating would interleave wrongly; limiting per node
    /// first would drop genuinely-recent jobs.
    #[test]
    fn jobs_from_all_nodes_are_ordered_by_completion_time_not_by_node() {
        let mut n1 = state("node-1", RaftRole::Leader, &[]);
        n1.jobs = vec![
            job("j-old", "node-1", 1_000),
            job("j-newest", "node-1", 9_000),
        ];
        let mut n2 = state("node-2", RaftRole::Follower, &[]);
        n2.jobs = vec![job("j-middle", "node-2", 5_000)];

        let snap = merge(n1, vec![PeerOutcome::Answered(n2)], at());
        let ids: Vec<&str> = snap.jobs.iter().map(|j| j.job_id.as_str()).collect();
        assert_eq!(ids, vec!["j-newest", "j-middle", "j-old"]);
        // Each keeps the node that ran it.
        assert_eq!(snap.jobs[1].owning_node, "node-2");
    }

    /// Equal timestamps are likely at millisecond resolution, so the tie-break
    /// must be deterministic or the list reorders between identical calls.
    #[test]
    fn jobs_with_equal_timestamps_tie_break_deterministically() {
        let mut n1 = state("node-1", RaftRole::Leader, &[]);
        n1.jobs = vec![job("j-zulu", "node-1", 5_000)];
        let mut n2 = state("node-2", RaftRole::Follower, &[]);
        n2.jobs = vec![job("j-alpha", "node-2", 5_000)];

        let forward = merge(n1.clone(), vec![PeerOutcome::Answered(n2.clone())], at());
        let reverse = merge(n2, vec![PeerOutcome::Answered(n1)], at());
        let ids: Vec<&str> = forward.jobs.iter().map(|j| j.job_id.as_str()).collect();
        assert_eq!(ids, vec!["j-alpha", "j-zulu"]);
        assert_eq!(forward.jobs, reverse.jobs);
    }

    /// The whole point of fan-out: workers from every node appear exactly
    /// once, each labelled with the node that knows about it.
    #[test]
    fn workers_from_all_nodes_are_unioned_and_labelled() {
        let snap = merge(
            state("node-1", RaftRole::Leader, &["w-a"]),
            vec![
                PeerOutcome::Answered(state("node-2", RaftRole::Follower, &["w-b"])),
                PeerOutcome::Answered(state("node-3", RaftRole::Follower, &["w-c"])),
            ],
            at(),
        );
        let ids: Vec<&str> = snap.workers.iter().map(|w| w.worker_id.as_str()).collect();
        assert_eq!(ids, vec!["w-a", "w-b", "w-c"]);
        assert_eq!(snap.workers[0].owning_node, "node-1");
        assert_eq!(snap.workers[1].owning_node, "node-2");
        assert_eq!(snap.workers[2].owning_node, "node-3");
        assert!(!snap.degraded);
    }

    /// CAS stats are NEVER summed. Each node opens its own store, so the same
    /// blob on three nodes is three copies of one blob — a total would report
    /// storage that does not exist and a dedup ratio that means nothing.
    #[test]
    fn cas_stats_are_reported_per_node_never_summed() {
        let snap = merge(
            state("node-1", RaftRole::Leader, &[]),
            vec![
                PeerOutcome::Answered(state("node-2", RaftRole::Follower, &[])),
                PeerOutcome::Answered(state("node-3", RaftRole::Follower, &[])),
            ],
            at(),
        );
        assert_eq!(snap.cas.len(), 3, "one entry per node, not one total");
        assert!(
            snap.cas.iter().all(|c| c.objects == 10 && c.bytes == 1000),
            "per-node values must be preserved verbatim, not combined"
        );
        let owners: Vec<&str> = snap.cas.iter().map(|c| c.owning_node.as_str()).collect();
        assert_eq!(owners, vec!["node-1", "node-2", "node-3"]);
    }

    /// Policy counters are per node for the same reason: nodes may have
    /// different modules loaded, or differ in quarantine state. Two nodes
    /// disagreeing is real information, not a glitch to average away.
    #[test]
    fn policy_views_are_reported_per_node_never_summed() {
        let mut quarantined = state("node-2", RaftRole::Follower, &[]);
        quarantined.policy = policy("node-2", true);
        let snap = merge(
            state("node-1", RaftRole::Leader, &[]),
            vec![PeerOutcome::Answered(quarantined)],
            at(),
        );
        assert_eq!(snap.policies.len(), 2);
        assert!(!snap.policies[0].quarantined);
        assert!(snap.policies[1].quarantined);
        assert_eq!(snap.policies[1].owning_node, "node-2");
    }

    /// Leadership comes from Raft — the node that says it is leading — not
    /// from counting agreement among replies.
    #[test]
    fn the_leader_is_taken_from_raft_not_from_a_majority_of_replies() {
        let snap = merge(
            state("node-1", RaftRole::Follower, &[]),
            vec![
                PeerOutcome::Answered(state("node-2", RaftRole::Leader, &[])),
                PeerOutcome::Answered(state("node-3", RaftRole::Follower, &[])),
            ],
            at(),
        );
        assert_eq!(snap.leader_id.as_deref(), Some("node-2"));
        assert!(!snap.degraded);
    }

    /// One unreachable peer degrades the snapshot without failing it, and the
    /// missing node still appears — "known but silent" must stay distinct from
    /// "not a member".
    #[test]
    fn an_unreachable_peer_degrades_but_does_not_remove_the_node() {
        let snap = merge(
            state("node-1", RaftRole::Leader, &["w-a"]),
            vec![
                PeerOutcome::Answered(state("node-2", RaftRole::Follower, &["w-b"])),
                PeerOutcome::Unreachable {
                    node_id: "node-3".to_string(),
                    advertise_addr: "10.0.0.3:7878".to_string(),
                },
            ],
            at(),
        );
        assert!(snap.degraded);
        assert_eq!(snap.nodes.len(), 3, "the silent node must still be listed");
        let n3 = snap.nodes.iter().find(|n| n.node_id == "node-3").unwrap();
        assert!(!n3.reachable);
        assert_eq!(n3.advertise_addr, "10.0.0.3:7878");
        // Its workers are simply absent — we cannot know them.
        assert_eq!(snap.workers.len(), 2);
        // Leadership is still known, because the leader answered.
        assert_eq!(snap.leader_id.as_deref(), Some("node-1"));
    }

    /// Two nodes claiming leadership at the *same* term is impossible under
    /// Raft, so it means our view is internally inconsistent. Report none and
    /// mark degraded rather than picking one.
    #[test]
    fn two_claimed_leaders_at_the_same_term_report_no_leader_and_degraded() {
        let snap = merge(
            state("node-1", RaftRole::Leader, &[]),
            vec![PeerOutcome::Answered(state(
                "node-2",
                RaftRole::Leader,
                &[],
            ))],
            at(),
        );
        assert_eq!(snap.leader_id, None);
        assert!(snap.degraded);
    }

    /// A partitioned ex-leader keeps claiming leadership at its old term. It
    /// must NOT make a healthy cluster look ambiguous — the higher term wins,
    /// which is exactly what Raft guarantees.
    #[test]
    fn a_stale_claimant_at_a_lower_term_does_not_obscure_the_real_leader() {
        let mut stale = state("node-1", RaftRole::Leader, &[]);
        stale.node.term = 6; // the old term it was elected in
        let mut current = state("node-2", RaftRole::Leader, &[]);
        current.node.term = 7; // the term that superseded it

        let snap = merge(stale, vec![PeerOutcome::Answered(current)], at());

        assert_eq!(
            snap.leader_id.as_deref(),
            Some("node-2"),
            "the highest-term claimant is the leader"
        );
        assert!(
            !snap.degraded,
            "one stale claimant is not a degraded cluster"
        );
        // The stale node keeps its own term visible rather than being rewritten.
        let n1 = snap.nodes.iter().find(|n| n.node_id == "node-1").unwrap();
        assert_eq!(n1.term, 6);
    }

    /// An unreachable node must not participate in leader selection: its state
    /// is zeroed and unknown, so letting it vote would be inventing data.
    #[test]
    fn an_unreachable_node_does_not_participate_in_leader_selection() {
        let snap = merge(
            state("node-1", RaftRole::Leader, &[]),
            vec![PeerOutcome::Unreachable {
                node_id: "node-2".to_string(),
                advertise_addr: "10.0.0.2:7878".to_string(),
            }],
            at(),
        );
        assert_eq!(snap.leader_id.as_deref(), Some("node-1"));
        // Still degraded, because a known node is silent.
        assert!(snap.degraded);
    }

    /// No node claiming leadership is worth surfacing — that is an election in
    /// progress, and it is exactly what an operator wants to see.
    #[test]
    fn no_claimed_leader_reports_none_and_degraded() {
        let snap = merge(
            state("node-1", RaftRole::Unknown, &[]),
            vec![PeerOutcome::Answered(state(
                "node-2",
                RaftRole::Unknown,
                &[],
            ))],
            at(),
        );
        assert_eq!(snap.leader_id, None);
        assert!(snap.degraded);
    }

    /// Output ordering must not depend on which peer answered first. Replies
    /// arrive in completion order, which is nondeterministic; this project has
    /// shipped ordering bugs from exactly that class twice.
    #[test]
    fn merge_output_is_independent_of_reply_order() {
        let local = state("node-2", RaftRole::Leader, &["w-b"]);
        let p1 = PeerOutcome::Answered(state("node-1", RaftRole::Follower, &["w-a"]));
        let p3 = PeerOutcome::Answered(state("node-3", RaftRole::Follower, &["w-c"]));

        let forward = merge(local.clone(), vec![p1.clone(), p3.clone()], at());
        let reverse = merge(local, vec![p3, p1], at());

        assert_eq!(forward.nodes, reverse.nodes);
        assert_eq!(forward.workers, reverse.workers);
        assert_eq!(forward.cas, reverse.cas);
        assert_eq!(forward.policies, reverse.policies);
    }

    /// A node with no consensus configured is healthy, not degraded. There is
    /// nothing for it to be degraded from, and a permanent warning on every
    /// single-node deployment would train operators to ignore the field.
    #[test]
    fn a_standalone_node_is_healthy_despite_having_no_leader() {
        let snap = merge(state("solo", RaftRole::Standalone, &[]), vec![], at());
        assert_eq!(snap.leader_id, None);
        assert!(!snap.degraded, "no consensus configured is not degraded");
    }

    /// A single node with no peers is not degraded and reports itself.
    #[test]
    fn a_single_node_with_no_peers_is_healthy() {
        let snap = merge(state("solo", RaftRole::Leader, &["w-a"]), vec![], at());
        assert_eq!(snap.nodes.len(), 1);
        assert_eq!(snap.workers.len(), 1);
        assert!(!snap.degraded);
        assert_eq!(snap.leader_id.as_deref(), Some("solo"));
        assert_eq!(snap.as_of, Some(at()));
    }
}
