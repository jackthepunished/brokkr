//! Core Raft newtypes and the replicated [`LogEntry`].
//!
//! All identifiers are newtypes (CLAUDE.md architectural invariant). [`Term`]
//! and [`LogIndex`] wrap `u64`; [`NodeId`] wraps a validated `String`. A
//! [`LogEntry`] converts losslessly to and from its wire protobuf
//! (`brokkr.v1.LogEntry`) and is stored on disk in that same encoding
//! (ADR 0013 D1).

use std::fmt;

use bytes::Bytes;
use prost::Message;

use crate::error::RaftError;

use brokkr_proto::brokkr::v1 as pb;

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

/// One entry in the replicated log: a state-machine `command` tagged with the
/// `term` in which the leader created it and its 1-based `index`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// The leader's term when this entry was created.
    pub term: Term,
    /// The entry's 1-based position in the log.
    pub index: LogIndex,
    /// The opaque state-machine command.
    pub command: Bytes,
}

impl LogEntry {
    /// Constructs a log entry.
    pub fn new(term: Term, index: LogIndex, command: impl Into<Bytes>) -> Self {
        LogEntry {
            term,
            index,
            command: command.into(),
        }
    }

    /// Encodes the entry to its protobuf wire/disk form (ADR 0013 D1).
    pub fn encode(&self) -> Vec<u8> {
        pb::LogEntry::from(self).encode_to_vec()
    }

    /// Decodes an entry from its protobuf wire/disk form.
    pub fn decode(bytes: &[u8]) -> Result<Self, RaftError> {
        Ok(pb::LogEntry::decode(bytes)?.into())
    }
}

impl From<&LogEntry> for pb::LogEntry {
    fn from(e: &LogEntry) -> Self {
        pb::LogEntry {
            term: e.term.get(),
            index: e.index.get(),
            command: e.command.to_vec(),
        }
    }
}

impl From<LogEntry> for pb::LogEntry {
    fn from(e: LogEntry) -> Self {
        pb::LogEntry::from(&e)
    }
}

impl From<pb::LogEntry> for LogEntry {
    fn from(p: pb::LogEntry) -> Self {
        LogEntry {
            term: Term::new(p.term),
            index: LogIndex::new(p.index),
            command: Bytes::from(p.command),
        }
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
        let back: LogEntry = proto.into();
        assert_eq!(entry, back);
    }

    #[test]
    fn empty_command_round_trips() {
        let entry = LogEntry::new(Term::ZERO, LogIndex::ZERO, Bytes::new());
        let decoded = LogEntry::decode(&entry.encode()).unwrap();
        assert_eq!(entry, decoded);
        assert!(decoded.command.is_empty());
    }
}
