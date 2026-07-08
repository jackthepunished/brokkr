//! Core Raft newtypes and the replicated [`LogEntry`].
//!
//! All identifiers are newtypes (CLAUDE.md architectural invariant). [`Term`]
//! and [`LogIndex`] wrap `u64`; [`NodeId`] wraps a validated `String`. A
//! [`LogEntry`] converts losslessly to and from its wire protobuf
//! (`brokkr.v1.LogEntry`) and is stored on disk in that same encoding
//! (ADR 0013 D1).

use std::collections::BTreeSet;
use std::fmt;

use bytes::Bytes;
use prost::Message;

use brokkr_proto::brokkr::v1 as pb;

use crate::error::RaftError;

/// Maximum byte length of a [`NodeId`].
pub const NODE_ID_MAX_LEN: usize = 128;

/// A Raft term — a logical clock that increases monotonically. Each term begins
/// with an election and has at most one leader (`docs/raft-notes.md` §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Term(u64);

impl Term {
    /// The initial term before any election has occurred.
    pub const ZERO: Term = Term(0);

    /// Wraps a raw term number.
    pub const fn new(value: u64) -> Self {
        Term(value)
    }

    /// The raw term number.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next term (saturating; a `u64` term never realistically overflows).
    pub const fn next(self) -> Self {
        Term(self.0.saturating_add(1))
    }
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for Term {
    fn from(v: u64) -> Self {
        Term(v)
    }
}

/// A 1-based position in the replicated log. Index `0` denotes "before the first
/// entry" (an empty log), used for `prev_log_index` of the first entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LogIndex(u64);

impl LogIndex {
    /// The sentinel index for an empty log / "before the first entry".
    pub const ZERO: LogIndex = LogIndex(0);

    /// Wraps a raw index.
    pub const fn new(value: u64) -> Self {
        LogIndex(value)
    }

    /// The raw index.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next index (saturating).
    pub const fn next(self) -> Self {
        LogIndex(self.0.saturating_add(1))
    }

    /// The previous index, or [`LogIndex::ZERO`] if already zero (saturating).
    pub const fn prev(self) -> Self {
        LogIndex(self.0.saturating_sub(1))
    }
}

impl fmt::Display for LogIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for LogIndex {
    fn from(v: u64) -> Self {
        LogIndex(v)
    }
}

/// A stable identifier for a Raft node. Non-empty and at most
/// [`NODE_ID_MAX_LEN`] bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(String);

impl NodeId {
    /// Constructs a [`NodeId`], validating it is non-empty and within
    /// [`NODE_ID_MAX_LEN`].
    pub fn new(inner: impl Into<String>) -> Result<Self, RaftError> {
        let inner = inner.into();
        if inner.is_empty() {
            return Err(RaftError::InvalidNodeId("empty".to_string()));
        }
        if inner.len() > NODE_ID_MAX_LEN {
            return Err(RaftError::InvalidNodeId(format!(
                "exceeds {NODE_ID_MAX_LEN} bytes: got {}",
                inner.len()
            )));
        }
        Ok(NodeId(inner))
    }

    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes self and returns the inner [`String`].
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for NodeId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Metadata describing a snapshot: the log prefix it replaces (Raft §7).
///
/// The snapshot blob itself is opaque `Bytes` produced by the state machine
/// (for I8's KV, a serialized map); this metadata is what consensus needs —
/// the last log entry the snapshot covers, so replication and elections can
/// reason about a log whose prefix has been compacted away. Because config
/// entries live in the log and the snapshot replaces the prefix, the
/// configuration in effect at `last_included_index` rides along (I7b) —
/// a node restoring from snapshot must learn its membership from here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotMeta {
    /// The snapshot replaces all log entries up to and including this index.
    pub last_included_index: LogIndex,
    /// The term of the entry at `last_included_index`.
    pub last_included_term: Term,
    /// The cluster configuration as of `last_included_index`. Empty voters
    /// mean "unknown" (a pre-I7b snapshot): the node falls back to its
    /// bootstrap configuration.
    pub config: ClusterConfig,
}

/// A cluster membership configuration (I7, Raft §6 / thesis ch. 4).
///
/// Configurations travel *in the log*: appending a [`EntryPayload::Config`]
/// entry is what changes the membership a node uses (applied on append, not
/// commit — the paper's rule). `old_voters: Some(..)` marks the joint
/// configuration C_old,new, where agreement — elections and commits alike —
/// requires a strict majority in **both** voter sets. Learners are replicated
/// to but never count toward quorum.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClusterConfig {
    /// Voting members (during a joint change, C_new's voters).
    pub voters: BTreeSet<NodeId>,
    /// C_old's voters while the joint C_old,new is in flight; `None` otherwise.
    pub old_voters: Option<BTreeSet<NodeId>>,
    /// Non-voting members being caught up before promotion (thesis ch. 4).
    pub learners: BTreeSet<NodeId>,
}

impl ClusterConfig {
    /// A non-joint configuration with the given voters and no learners.
    pub fn single(voters: impl IntoIterator<Item = NodeId>) -> Self {
        ClusterConfig {
            voters: voters.into_iter().collect(),
            old_voters: None,
            learners: BTreeSet::new(),
        }
    }

    /// Whether this is the joint configuration C_old,new.
    pub fn is_joint(&self) -> bool {
        self.old_voters.is_some()
    }

    /// Whether `acks` satisfies quorum: a strict majority of `voters`, **and**
    /// of `old_voters` when joint (Raft §6 — no decision without agreement in
    /// both configurations). Learners never count; an empty voter set can
    /// never reach quorum.
    pub fn has_quorum(&self, acks: &BTreeSet<NodeId>) -> bool {
        fn majority(voters: &BTreeSet<NodeId>, acks: &BTreeSet<NodeId>) -> bool {
            !voters.is_empty()
                && voters.iter().filter(|v| acks.contains(*v)).count() * 2 > voters.len()
        }
        majority(&self.voters, acks)
            && self
                .old_voters
                .as_ref()
                .is_none_or(|old| majority(old, acks))
    }
}

impl From<&ClusterConfig> for pb::ClusterConfig {
    fn from(c: &ClusterConfig) -> Self {
        pb::ClusterConfig {
            voters: c.voters.iter().map(|n| n.as_str().to_string()).collect(),
            old_voters: c
                .old_voters
                .iter()
                .flatten()
                .map(|n| n.as_str().to_string())
                .collect(),
            learners: c.learners.iter().map(|n| n.as_str().to_string()).collect(),
        }
    }
}

impl TryFrom<pb::ClusterConfig> for ClusterConfig {
    type Error = RaftError;
    fn try_from(p: pb::ClusterConfig) -> Result<Self, Self::Error> {
        let parse = |ids: Vec<String>| -> Result<BTreeSet<NodeId>, RaftError> {
            ids.into_iter().map(NodeId::new).collect()
        };
        let voters = parse(p.voters)?;
        let old = parse(p.old_voters)?;
        let learners = parse(p.learners)?;
        // A node cannot both vote and be a non-voting learner. Voters and
        // `old_voters` *do* overlap during a joint change (that is the point),
        // so only learner disjointness is an invariant. This holds for every
        // config, including the empty "unknown membership" sentinel a
        // snapshot may carry (which trivially has no overlap).
        if let Some(id) = voters.intersection(&learners).next() {
            return Err(RaftError::Codec(format!(
                "node {id} is both a voter and a learner"
            )));
        }
        if let Some(id) = old.intersection(&learners).next() {
            return Err(RaftError::Codec(format!(
                "node {id} is both an old voter and a learner"
            )));
        }
        Ok(ClusterConfig {
            voters,
            old_voters: if old.is_empty() { None } else { Some(old) },
            learners,
        })
    }
}

/// The payload of a log entry (I7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryPayload {
    /// An opaque state-machine command.
    Command(Bytes),
    /// A leader's start-of-term no-op (reserved for the linearizable-read
    /// path; never produced yet).
    Noop,
    /// A cluster membership configuration (joint consensus, I7).
    Config(ClusterConfig),
}

/// One entry in the replicated log: a payload tagged with the `term` in which
/// the leader created it and its 1-based `index`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// The leader's term when this entry was created.
    pub term: Term,
    /// The entry's 1-based position in the log.
    pub index: LogIndex,
    /// What the entry carries (I7): a command, a no-op, or a configuration.
    pub payload: EntryPayload,
}

impl LogEntry {
    /// Constructs a command entry (the common case).
    pub fn new(term: Term, index: LogIndex, command: impl Into<Bytes>) -> Self {
        LogEntry {
            term,
            index,
            payload: EntryPayload::Command(command.into()),
        }
    }

    /// Constructs an entry with an explicit payload.
    pub fn with_payload(term: Term, index: LogIndex, payload: EntryPayload) -> Self {
        LogEntry {
            term,
            index,
            payload,
        }
    }

    /// The state-machine command, or `None` for no-op / configuration entries
    /// (which produce no state-machine output).
    pub fn command(&self) -> Option<&Bytes> {
        match &self.payload {
            EntryPayload::Command(c) => Some(c),
            _ => None,
        }
    }

    /// Encodes the entry to its protobuf wire/disk form (ADR 0013 D1).
    pub fn encode(&self) -> Bytes {
        Bytes::from(pb::LogEntry::from(self).encode_to_vec())
    }

    /// Decodes an entry from its protobuf wire/disk form. Entries written
    /// before I7 carry no `kind` field and decode as commands.
    pub fn decode(bytes: &[u8]) -> Result<Self, RaftError> {
        pb::LogEntry::decode(bytes)?.try_into()
    }
}

impl From<&LogEntry> for pb::LogEntry {
    fn from(e: &LogEntry) -> Self {
        let (kind, command, config) = match &e.payload {
            EntryPayload::Command(c) => (pb::EntryKind::Command, c.to_vec(), None),
            EntryPayload::Noop => (pb::EntryKind::Noop, Vec::new(), None),
            EntryPayload::Config(c) => (
                pb::EntryKind::Config,
                Vec::new(),
                Some(pb::ClusterConfig::from(c)),
            ),
        };
        pb::LogEntry {
            term: e.term.get(),
            index: e.index.get(),
            command,
            kind: kind as i32,
            config,
        }
    }
}

impl From<LogEntry> for pb::LogEntry {
    fn from(e: LogEntry) -> Self {
        pb::LogEntry::from(&e)
    }
}

impl TryFrom<pb::LogEntry> for LogEntry {
    type Error = RaftError;
    fn try_from(p: pb::LogEntry) -> Result<Self, Self::Error> {
        let kind = pb::EntryKind::try_from(p.kind)
            .map_err(|_| RaftError::Codec(format!("unknown log entry kind {}", p.kind)))?;
        // `kind` is the discriminator: reject entries whose other fields do not
        // match it instead of silently normalizing them into a different
        // meaning. Our encoder only ever emits canonical combinations, so this
        // only ever fires on corrupt or forged wire/disk data.
        let payload = match kind {
            pb::EntryKind::Command => {
                if p.config.is_some() {
                    return Err(RaftError::Codec(
                        "COMMAND log entry must not carry a config".to_string(),
                    ));
                }
                EntryPayload::Command(Bytes::from(p.command))
            }
            pb::EntryKind::Noop => {
                if !p.command.is_empty() || p.config.is_some() {
                    return Err(RaftError::Codec(
                        "NOOP log entry must not carry command or config data".to_string(),
                    ));
                }
                EntryPayload::Noop
            }
            pb::EntryKind::Config => {
                if !p.command.is_empty() {
                    return Err(RaftError::Codec(
                        "CONFIG log entry must not carry command data".to_string(),
                    ));
                }
                let config = ClusterConfig::try_from(p.config.ok_or_else(|| {
                    RaftError::Codec("CONFIG log entry without a config".to_string())
                })?)?;
                // A configuration that governs the cluster must name at least
                // one voter; an empty set could never reach quorum. (The empty
                // `ClusterConfig` sentinel is only meaningful for a snapshot's
                // "unknown membership", never for a log entry.)
                if config.voters.is_empty() {
                    return Err(RaftError::Codec(
                        "CONFIG log entry must name at least one voter".to_string(),
                    ));
                }
                EntryPayload::Config(config)
            }
        };
        Ok(LogEntry {
            term: Term::new(p.term),
            index: LogIndex::new(p.index),
            payload,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn term_orders_and_increments() {
        assert!(Term::new(1) < Term::new(2));
        assert_eq!(Term::ZERO.next(), Term::new(1));
        assert_eq!(Term::new(5).get(), 5);
        assert_eq!(Term::default(), Term::ZERO);
    }

    #[test]
    fn log_index_next_and_prev_saturate() {
        assert_eq!(LogIndex::ZERO.next(), LogIndex::new(1));
        assert_eq!(LogIndex::new(1).prev(), LogIndex::ZERO);
        assert_eq!(LogIndex::ZERO.prev(), LogIndex::ZERO); // saturating
        assert!(LogIndex::new(3) > LogIndex::new(2));
    }

    #[test]
    fn node_id_accepts_valid() {
        let id = NodeId::new("node-a").unwrap();
        assert_eq!(id.as_str(), "node-a");
        assert_eq!(id.to_string(), "node-a");
    }

    #[test]
    fn node_id_rejects_empty() {
        let err = NodeId::new("").unwrap_err();
        assert!(matches!(err, RaftError::InvalidNodeId(_)));
    }

    #[test]
    fn node_id_rejects_too_long() {
        let long = "x".repeat(NODE_ID_MAX_LEN + 1);
        assert!(NodeId::new(long).is_err());
        let exact = "y".repeat(NODE_ID_MAX_LEN);
        assert!(NodeId::new(exact).is_ok());
    }

    #[test]
    fn log_entry_protobuf_round_trips() {
        let entry = LogEntry::new(
            Term::new(7),
            LogIndex::new(42),
            Bytes::from_static(b"set x=1"),
        );
        let bytes = entry.encode();
        let decoded = LogEntry::decode(&bytes).unwrap();
        assert_eq!(entry, decoded);
    }

    #[test]
    fn log_entry_proto_conversion_round_trips() {
        let entry = LogEntry::new(Term::new(3), LogIndex::new(9), Bytes::from_static(b"cmd"));
        let proto: pb::LogEntry = (&entry).into();
        assert_eq!(proto.term, 3);
        assert_eq!(proto.index, 9);
        let back: LogEntry = proto.try_into().unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn empty_command_round_trips() {
        let entry = LogEntry::new(Term::ZERO, LogIndex::ZERO, Bytes::new());
        let decoded = LogEntry::decode(&entry.encode()).unwrap();
        assert_eq!(entry, decoded);
        assert_eq!(decoded.command(), Some(&Bytes::new()));
    }

    // --- entry payloads & cluster configs (I7a) ----------------------------

    fn ids(names: &[&str]) -> BTreeSet<NodeId> {
        names.iter().map(|n| NodeId::new(*n).unwrap()).collect()
    }

    #[test]
    fn noop_and_config_payloads_round_trip() {
        let noop = LogEntry::with_payload(Term::new(2), LogIndex::new(5), EntryPayload::Noop);
        assert_eq!(LogEntry::decode(&noop.encode()).unwrap(), noop);
        assert_eq!(noop.command(), None);

        let config = ClusterConfig {
            voters: ids(&["a", "b", "c"]),
            old_voters: Some(ids(&["a", "b", "d"])),
            learners: ids(&["e"]),
        };
        let entry = LogEntry::with_payload(
            Term::new(3),
            LogIndex::new(6),
            EntryPayload::Config(config.clone()),
        );
        let decoded = LogEntry::decode(&entry.encode()).unwrap();
        assert_eq!(decoded, entry);
        assert_eq!(decoded.command(), None);
        let EntryPayload::Config(back) = decoded.payload else {
            panic!("expected a config payload");
        };
        assert!(back.is_joint());
        assert_eq!(back, config);
    }

    #[test]
    fn pre_i7_encoding_decodes_as_command() {
        // Exactly the bytes an I6-era node wrote to disk: fields 1–3 only.
        let legacy = pb::LogEntry {
            term: 4,
            index: 11,
            command: b"set x=1".to_vec(),
            kind: 0,
            config: None,
        };
        let decoded = LogEntry::decode(&legacy.encode_to_vec()).unwrap();
        assert_eq!(
            decoded.payload,
            EntryPayload::Command(Bytes::from_static(b"set x=1"))
        );
    }

    #[test]
    fn config_entry_with_invalid_node_id_fails_decode() {
        let proto = pb::LogEntry {
            term: 1,
            index: 1,
            command: Vec::new(),
            kind: pb::EntryKind::Config as i32,
            config: Some(pb::ClusterConfig {
                voters: vec![String::new()], // empty node id
                old_voters: vec![],
                learners: vec![],
            }),
        };
        assert!(LogEntry::try_from(proto).is_err());
    }

    #[test]
    fn config_entry_without_config_fails_decode() {
        let proto = pb::LogEntry {
            term: 1,
            index: 1,
            command: Vec::new(),
            kind: pb::EntryKind::Config as i32,
            config: None,
        };
        assert!(matches!(
            LogEntry::try_from(proto).unwrap_err(),
            RaftError::Codec(_)
        ));
    }

    #[test]
    fn config_entry_without_voters_fails_decode() {
        // A governing config from the log must name a voter; an empty voter
        // set could never reach quorum.
        let proto = pb::LogEntry {
            term: 1,
            index: 1,
            command: Vec::new(),
            kind: pb::EntryKind::Config as i32,
            config: Some(pb::ClusterConfig {
                voters: vec![],
                old_voters: vec![],
                learners: vec!["l".to_string()],
            }),
        };
        assert!(matches!(
            LogEntry::try_from(proto).unwrap_err(),
            RaftError::Codec(_)
        ));
    }

    #[test]
    fn cluster_config_rejects_learner_voter_overlap() {
        // A node cannot be both a voter and a learner...
        let overlap = pb::ClusterConfig {
            voters: vec!["a".to_string(), "b".to_string()],
            old_voters: vec![],
            learners: vec!["b".to_string()],
        };
        assert!(ClusterConfig::try_from(overlap).is_err());

        // ...nor both an old voter and a learner.
        let old_overlap = pb::ClusterConfig {
            voters: vec!["a".to_string()],
            old_voters: vec!["c".to_string()],
            learners: vec!["c".to_string()],
        };
        assert!(ClusterConfig::try_from(old_overlap).is_err());

        // But voters and old_voters overlapping is normal during a joint change.
        let joint = pb::ClusterConfig {
            voters: vec!["a".to_string(), "b".to_string(), "d".to_string()],
            old_voters: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            learners: vec![],
        };
        assert!(ClusterConfig::try_from(joint).is_ok());
    }

    #[test]
    fn non_canonical_log_entry_payloads_are_rejected() {
        let base = |kind: pb::EntryKind| pb::LogEntry {
            term: 1,
            index: 1,
            command: Vec::new(),
            kind: kind as i32,
            config: None,
        };
        let config = || {
            Some(pb::ClusterConfig {
                voters: vec!["a".to_string()],
                old_voters: vec![],
                learners: vec![],
            })
        };

        // COMMAND must not carry a config.
        let mut cmd = base(pb::EntryKind::Command);
        cmd.command = b"x".to_vec();
        cmd.config = config();
        assert!(LogEntry::try_from(cmd).is_err());

        // NOOP must carry neither command nor config.
        let mut noop_cmd = base(pb::EntryKind::Noop);
        noop_cmd.command = b"x".to_vec();
        assert!(LogEntry::try_from(noop_cmd).is_err());
        let mut noop_cfg = base(pb::EntryKind::Noop);
        noop_cfg.config = config();
        assert!(LogEntry::try_from(noop_cfg).is_err());

        // CONFIG must not carry command data.
        let mut cfg_cmd = base(pb::EntryKind::Config);
        cfg_cmd.command = b"x".to_vec();
        cfg_cmd.config = config();
        assert!(LogEntry::try_from(cfg_cmd).is_err());
    }

    #[test]
    fn single_config_quorum_is_a_strict_majority() {
        let c = ClusterConfig::single(ids(&["a", "b", "c"]));
        assert!(!c.is_joint());
        assert!(c.has_quorum(&ids(&["a", "b"])));
        assert!(!c.has_quorum(&ids(&["a"])));
        // Non-voters in the ack set contribute nothing.
        assert!(!c.has_quorum(&ids(&["a", "x", "y", "z"])));
    }

    #[test]
    fn joint_config_requires_majorities_in_both_sets() {
        let joint = ClusterConfig {
            voters: ids(&["d", "e", "f"]),           // C_new
            old_voters: Some(ids(&["a", "b", "c"])), // C_old
            learners: BTreeSet::new(),
        };
        // Majority of C_new only: not enough.
        assert!(!joint.has_quorum(&ids(&["d", "e"])));
        // Majority of C_old only: not enough.
        assert!(!joint.has_quorum(&ids(&["a", "b"])));
        // Majorities in both: quorum.
        assert!(joint.has_quorum(&ids(&["a", "b", "d", "e"])));
    }

    #[test]
    fn learners_never_count_toward_quorum() {
        let c = ClusterConfig {
            voters: ids(&["a", "b", "c"]),
            old_voters: None,
            learners: ids(&["l1", "l2"]),
        };
        assert!(!c.has_quorum(&ids(&["a", "l1", "l2"])));
        assert!(c.has_quorum(&ids(&["a", "b", "l1"])));
    }

    #[test]
    fn empty_voter_set_never_has_quorum() {
        let c = ClusterConfig::default();
        assert!(!c.has_quorum(&ids(&["a", "b"])));
        assert!(!c.has_quorum(&BTreeSet::new()));
    }

    #[test]
    fn cluster_config_proto_round_trips_and_empty_old_means_single() {
        let joint = ClusterConfig {
            voters: ids(&["a", "b"]),
            old_voters: Some(ids(&["c"])),
            learners: ids(&["l"]),
        };
        let back = ClusterConfig::try_from(pb::ClusterConfig::from(&joint)).unwrap();
        assert_eq!(back, joint);

        let single = ClusterConfig::single(ids(&["a", "b"]));
        let proto = pb::ClusterConfig::from(&single);
        assert!(proto.old_voters.is_empty());
        let back = ClusterConfig::try_from(proto).unwrap();
        assert_eq!(back.old_voters, None);
    }
}
