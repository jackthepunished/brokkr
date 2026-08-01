//! Raft state projections (ADR 0013).

use brokkr_raft::DriverStatus;

/// A node's role in the Raft cluster, as an operator sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftRole {
    /// This node believes it is the leader.
    Leader,
    /// This node recognises some other node as leader.
    Follower,
    /// This node recognises no leader — mid-election, or partitioned from one.
    ///
    /// Deliberately distinct from [`Self::Follower`]. "Nobody is leading" is
    /// exactly what an operator needs to see during an incident, and folding
    /// it into `Follower` would hide it.
    Unknown,
}

impl RaftRole {
    /// A stable lowercase tag, for the wire and for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Leader => "leader",
            Self::Follower => "follower",
            Self::Unknown => "unknown",
        }
    }
}

/// One control-plane node, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeView {
    /// The node's Raft id.
    pub node_id: String,
    /// The address this node advertises to peers and clients.
    pub advertise_addr: String,
    /// The node's Raft role.
    pub role: RaftRole,
    /// The node's current Raft term.
    pub term: u64,
    /// The highest log index the node knows to be committed.
    pub commit_index: u64,
    /// The highest index applied to the state machine. Lag behind
    /// `commit_index` is normal and transient; sustained lag is not.
    pub last_applied: u64,
    /// Whether this node answered the most recent poll.
    pub reachable: bool,
    /// Seconds since this node last answered. Zero when it just did.
    pub last_seen_secs: u64,
}

/// Project a live node's [`DriverStatus`].
pub fn node_view_from_status(
    node_id: &str,
    advertise_addr: &str,
    status: &DriverStatus,
) -> NodeView {
    let role = if status.is_leader {
        RaftRole::Leader
    } else if status.leader.is_some() {
        RaftRole::Follower
    } else {
        RaftRole::Unknown
    };
    NodeView {
        node_id: node_id.to_string(),
        advertise_addr: advertise_addr.to_string(),
        role,
        term: status.term.get(),
        commit_index: status.commit_index.get(),
        last_applied: status.last_applied.get(),
        reachable: true,
        last_seen_secs: 0,
    }
}

/// A node that is known to the cluster configuration but did not answer.
///
/// Present rather than omitted: dropping it would make "a node I know about is
/// not answering" indistinguishable from "that node does not exist", which is
/// the difference between a degraded cluster and a smaller one.
pub fn unreachable_node_view(node_id: &str, advertise_addr: &str) -> NodeView {
    NodeView {
        node_id: node_id.to_string(),
        advertise_addr: advertise_addr.to_string(),
        role: RaftRole::Unknown,
        term: 0,
        commit_index: 0,
        last_applied: 0,
        reachable: false,
        last_seen_secs: 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use brokkr_raft::{LogIndex, NodeId, Term};

    use super::*;

    fn status(is_leader: bool, leader: Option<&str>) -> DriverStatus {
        DriverStatus {
            is_leader,
            term: Term::new(7),
            commit_index: LogIndex::new(42),
            last_applied: LogIndex::new(41),
            last_log_index: LogIndex::new(43),
            leader: leader.map(|s| NodeId::new(s).unwrap()),
            snapshot: None,
            config: Default::default(),
        }
    }

    #[test]
    fn a_leader_projects_as_leader() {
        let v = node_view_from_status("node-1", "10.0.0.1:7878", &status(true, Some("node-1")));
        assert_eq!(v.node_id, "node-1");
        assert_eq!(v.advertise_addr, "10.0.0.1:7878");
        assert_eq!(v.role, RaftRole::Leader);
        assert_eq!(v.term, 7);
        assert_eq!(v.commit_index, 42);
        assert_eq!(v.last_applied, 41);
        assert!(v.reachable);
    }

    #[test]
    fn a_node_that_recognises_a_leader_projects_as_follower() {
        let v = node_view_from_status("node-2", "10.0.0.2:7878", &status(false, Some("node-1")));
        assert_eq!(v.role, RaftRole::Follower);
    }

    /// A node in an election, or partitioned from the leader, recognises
    /// nobody. That is distinct from being a follower and must not be
    /// flattened into it — "nobody is leading" is exactly what an operator
    /// needs to see during an incident.
    #[test]
    fn a_node_recognising_no_leader_is_unknown_not_follower() {
        let v = node_view_from_status("node-3", "10.0.0.3:7878", &status(false, None));
        assert_eq!(v.role, RaftRole::Unknown);
    }

    /// An unreachable node still appears, with its identity and zeroed state.
    /// Dropping it would make "a node I know about is not answering"
    /// indistinguishable from "that node does not exist".
    #[test]
    fn an_unreachable_node_is_present_but_marked() {
        let v = unreachable_node_view("node-4", "10.0.0.4:7878");
        assert_eq!(v.node_id, "node-4");
        assert_eq!(v.advertise_addr, "10.0.0.4:7878");
        assert!(!v.reachable);
        assert_eq!(v.role, RaftRole::Unknown);
        assert_eq!(v.term, 0);
        assert_eq!(v.commit_index, 0);
    }

    /// The wire tags are stable and distinct; the proto carries them as
    /// strings so adding a role later cannot renumber an existing one.
    #[test]
    fn role_tags_are_stable_and_distinct() {
        assert_eq!(RaftRole::Leader.as_str(), "leader");
        assert_eq!(RaftRole::Follower.as_str(), "follower");
        assert_eq!(RaftRole::Unknown.as_str(), "unknown");
    }
}
