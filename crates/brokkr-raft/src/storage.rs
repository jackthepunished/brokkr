//! Durable Raft log and hard state on redb (ADR 0013 D1).
//!
//! One `raft.redb` file per node holds two tables:
//!
//! ```text
//! ├─ "log":  u64  → &[u8]   // protobuf-encoded LogEntry, keyed by 1-based index
//! └─ "meta": &str → &[u8]   // hard state: current_term, voted_for
//! ```
//!
//! Log entries are stored in the same protobuf encoding they take on the wire,
//! so a leader replicates stored bytes without re-encoding.
//!
//! Every mutating method commits its redb transaction before returning — the
//! foundation of the **persist-before-respond** rule (`docs/raft-notes.md` §3).
//! The rigorous crash-consistency tests and the wiring into the node's reply
//! path are milestone I2; this module provides the schema and the primitive
//! operations they build on.

use std::path::Path;

use redb::{Database, ReadableTable, TableDefinition};

use crate::error::RaftError;
use crate::types::{LogEntry, LogIndex, NodeId, Term};

/// Log table: 1-based index → protobuf-encoded [`LogEntry`].
const LOG_TABLE: TableDefinition<'static, u64, &[u8]> = TableDefinition::new("log");
/// Hard-state table: string key → raw value bytes.
const META_TABLE: TableDefinition<'static, &str, &[u8]> = TableDefinition::new("meta");

/// `meta` key for the persisted `currentTerm` (little-endian `u64`).
const META_CURRENT_TERM: &str = "current_term";
/// `meta` key for the persisted `votedFor` (UTF-8 node id; absent = none).
const META_VOTED_FOR: &str = "voted_for";

/// Maps any `Display` storage error into [`RaftError::Storage`].
fn stor<E: std::fmt::Display>(e: E) -> RaftError {
    RaftError::Storage(e.to_string())
}

/// The durable Raft log and hard state for a single node.
///
/// redb handles its own internal locking, so all methods take `&self` and may be
/// called concurrently.
#[derive(Debug)]
pub struct RaftLog {
    db: Database,
}

impl RaftLog {
    /// Opens (creating if absent) the Raft store at `path` and ensures both
    /// tables exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RaftError> {
        let db = Database::create(path).map_err(stor)?;
        let write = db.begin_write().map_err(stor)?;
        {
            // Opening a table in a write txn creates it if absent.
            write.open_table(LOG_TABLE).map_err(stor)?;
            write.open_table(META_TABLE).map_err(stor)?;
        }
        write.commit().map_err(stor)?;
        Ok(RaftLog { db })
    }

    /// Appends (or overwrites) a single entry at its index, committing durably.
    pub fn append(&self, entry: &LogEntry) -> Result<(), RaftError> {
        self.append_all(std::slice::from_ref(entry))
    }

    /// Appends (or overwrites) a batch of entries in a single durable
    /// transaction.
    pub fn append_all(&self, entries: &[LogEntry]) -> Result<(), RaftError> {
        let write = self.db.begin_write().map_err(stor)?;
        {
            let mut table = write.open_table(LOG_TABLE).map_err(stor)?;
            for entry in entries {
                table
                    .insert(entry.index.get(), entry.encode().as_ref())
                    .map_err(stor)?;
            }
        }
        write.commit().map_err(stor)?;
        Ok(())
    }

    /// Returns the entry at `index`, or `None` if absent.
    pub fn get(&self, index: LogIndex) -> Result<Option<LogEntry>, RaftError> {
        let read = self.db.begin_read().map_err(stor)?;
        let table = read.open_table(LOG_TABLE).map_err(stor)?;
        match table.get(index.get()).map_err(stor)? {
            Some(guard) => Ok(Some(LogEntry::decode(guard.value())?)),
            None => Ok(None),
        }
    }

    /// The highest index present, or [`LogIndex::ZERO`] for an empty log.
    pub fn last_index(&self) -> Result<LogIndex, RaftError> {
        let read = self.db.begin_read().map_err(stor)?;
        let table = read.open_table(LOG_TABLE).map_err(stor)?;
        // Bind into a local so the borrowing `AccessGuard` is dropped before the
        // function returns (it must not outlive `table`).
        let last = table.last().map_err(stor)?;
        let index = match last {
            Some((key, _)) => LogIndex::new(key.value()),
            None => LogIndex::ZERO,
        };
        Ok(index)
    }

    /// The term of the last entry, or [`Term::ZERO`] for an empty log. Used by
    /// the election restriction (`docs/raft-notes.md` §6).
    pub fn last_term(&self) -> Result<Term, RaftError> {
        let last = self.last_index()?;
        if last == LogIndex::ZERO {
            return Ok(Term::ZERO);
        }
        Ok(self.get(last)?.map(|e| e.term).unwrap_or(Term::ZERO))
    }

    /// Removes every entry with index `>= from` (conflict truncation,
    /// `docs/raft-notes.md` §5.1 step 3). Truncation only ever happens on
    /// followers; a leader never deletes its own entries.
    pub fn truncate_from(&self, from: LogIndex) -> Result<(), RaftError> {
        let write = self.db.begin_write().map_err(stor)?;
        {
            let mut table = write.open_table(LOG_TABLE).map_err(stor)?;
            let mut to_remove = Vec::new();
            for item in table.range(from.get()..).map_err(stor)? {
                let (key, _value) = item.map_err(stor)?;
                to_remove.push(key.value());
            }
            for key in to_remove {
                table.remove(key).map_err(stor)?;
            }
        }
        write.commit().map_err(stor)?;
        Ok(())
    }

    /// Persists `currentTerm`.
    pub fn set_current_term(&self, term: Term) -> Result<(), RaftError> {
        self.put_meta(META_CURRENT_TERM, &term.get().to_le_bytes())
    }

    /// Loads `currentTerm`, or [`Term::ZERO`] if never set.
    pub fn current_term(&self) -> Result<Term, RaftError> {
        match self.get_meta(META_CURRENT_TERM)? {
            Some(bytes) => Ok(Term::new(u64_from_le(&bytes)?)),
            None => Ok(Term::ZERO),
        }
    }

    /// Persists `votedFor` (`Some` to record a vote, `None` to clear it).
    pub fn set_voted_for(&self, node: Option<&NodeId>) -> Result<(), RaftError> {
        match node {
            Some(id) => self.put_meta(META_VOTED_FOR, id.as_str().as_bytes()),
            None => self.remove_meta(META_VOTED_FOR),
        }
    }

    /// Loads `votedFor`, or `None` if this node has not voted in its current
    /// term.
    pub fn voted_for(&self) -> Result<Option<NodeId>, RaftError> {
        match self.get_meta(META_VOTED_FOR)? {
            Some(bytes) => {
                let s = String::from_utf8(bytes).map_err(stor)?;
                Ok(Some(NodeId::new(s)?))
            }
            None => Ok(None),
        }
    }

    fn put_meta(&self, key: &str, value: &[u8]) -> Result<(), RaftError> {
        let write = self.db.begin_write().map_err(stor)?;
        {
            let mut table = write.open_table(META_TABLE).map_err(stor)?;
            table.insert(key, value).map_err(stor)?;
        }
        write.commit().map_err(stor)?;
        Ok(())
    }

    fn remove_meta(&self, key: &str) -> Result<(), RaftError> {
        let write = self.db.begin_write().map_err(stor)?;
        {
            let mut table = write.open_table(META_TABLE).map_err(stor)?;
            table.remove(key).map_err(stor)?;
        }
        write.commit().map_err(stor)?;
        Ok(())
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>, RaftError> {
        let read = self.db.begin_read().map_err(stor)?;
        let table = read.open_table(META_TABLE).map_err(stor)?;
        match table.get(key).map_err(stor)? {
            Some(guard) => Ok(Some(guard.value().to_vec())),
            None => Ok(None),
        }
    }
}

/// Decodes a little-endian `u64` from an 8-byte meta value.
fn u64_from_le(bytes: &[u8]) -> Result<u64, RaftError> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| RaftError::Storage(format!("expected 8-byte u64, got {}", bytes.len())))?;
    Ok(u64::from_le_bytes(arr))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn temp_log() -> (tempfile::TempDir, RaftLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = RaftLog::open(dir.path().join("raft.redb")).unwrap();
        (dir, log)
    }

    fn entry(term: u64, index: u64, cmd: &'static [u8]) -> LogEntry {
        LogEntry::new(
            Term::new(term),
            LogIndex::new(index),
            Bytes::from_static(cmd),
        )
    }

    #[test]
    fn empty_log_reports_zero() {
        let (_dir, log) = temp_log();
        assert_eq!(log.last_index().unwrap(), LogIndex::ZERO);
        assert_eq!(log.last_term().unwrap(), Term::ZERO);
        assert_eq!(log.get(LogIndex::new(1)).unwrap(), None);
    }

    #[test]
    fn append_then_get_round_trips() {
        let (_dir, log) = temp_log();
        let e = entry(1, 1, b"a");
        log.append(&e).unwrap();
        assert_eq!(log.get(LogIndex::new(1)).unwrap(), Some(e));
        assert_eq!(log.last_index().unwrap(), LogIndex::new(1));
        assert_eq!(log.last_term().unwrap(), Term::new(1));
    }

    #[test]
    fn append_all_is_atomic_batch() {
        let (_dir, log) = temp_log();
        log.append_all(&[entry(1, 1, b"a"), entry(1, 2, b"b"), entry(2, 3, b"c")])
            .unwrap();
        assert_eq!(log.last_index().unwrap(), LogIndex::new(3));
        assert_eq!(log.last_term().unwrap(), Term::new(2));
        assert_eq!(
            log.get(LogIndex::new(2)).unwrap().unwrap().command,
            Bytes::from_static(b"b")
        );
    }

    #[test]
    fn survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raft.redb");
        {
            let log = RaftLog::open(&path).unwrap();
            log.append_all(&[entry(3, 1, b"x"), entry(3, 2, b"y")])
                .unwrap();
            log.set_current_term(Term::new(3)).unwrap();
            log.set_voted_for(Some(&NodeId::new("node-b").unwrap()))
                .unwrap();
        }
        let log = RaftLog::open(&path).unwrap();
        assert_eq!(log.last_index().unwrap(), LogIndex::new(2));
        assert_eq!(log.current_term().unwrap(), Term::new(3));
        assert_eq!(
            log.voted_for().unwrap(),
            Some(NodeId::new("node-b").unwrap())
        );
    }

    #[test]
    fn truncate_from_removes_suffix_only() {
        let (_dir, log) = temp_log();
        log.append_all(&[entry(1, 1, b"a"), entry(1, 2, b"b"), entry(1, 3, b"c")])
            .unwrap();
        log.truncate_from(LogIndex::new(2)).unwrap();
        assert_eq!(log.get(LogIndex::new(1)).unwrap(), Some(entry(1, 1, b"a")));
        assert_eq!(log.get(LogIndex::new(2)).unwrap(), None);
        assert_eq!(log.get(LogIndex::new(3)).unwrap(), None);
        assert_eq!(log.last_index().unwrap(), LogIndex::new(1));
    }

    #[test]
    fn overwrite_at_index_replaces_entry() {
        let (_dir, log) = temp_log();
        log.append(&entry(1, 1, b"old")).unwrap();
        log.append(&entry(2, 1, b"new")).unwrap();
        let got = log.get(LogIndex::new(1)).unwrap().unwrap();
        assert_eq!(got.term, Term::new(2));
        assert_eq!(got.command, Bytes::from_static(b"new"));
    }

    #[test]
    fn hard_state_defaults_and_clears() {
        let (_dir, log) = temp_log();
        assert_eq!(log.current_term().unwrap(), Term::ZERO);
        assert_eq!(log.voted_for().unwrap(), None);

        log.set_current_term(Term::new(9)).unwrap();
        let id = NodeId::new("candidate-1").unwrap();
        log.set_voted_for(Some(&id)).unwrap();
        assert_eq!(log.current_term().unwrap(), Term::new(9));
        assert_eq!(log.voted_for().unwrap(), Some(id));

        log.set_voted_for(None).unwrap();
        assert_eq!(log.voted_for().unwrap(), None);
    }
}
