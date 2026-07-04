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
//! **persist-before-respond** rule (`docs/raft-notes.md` §3). Hard state
//! (`currentTerm` + `votedFor`) is written atomically as a unit via
//! [`RaftLog::save_hard_state`] so a crash can never expose a torn vote. The
//! crash-consistency tests below (uncommitted writes are invisible; committed
//! state survives a real process abort in `tests/crash_consistency.rs`) prove
//! this. Wiring these primitives into the node's reply path is milestone I3.

use std::path::Path;

use redb::{Database, ReadableTable, TableDefinition};

use crate::error::RaftError;
use crate::state::HardState;
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

    /// Returns every entry with index `>= from`, in ascending order — the batch a
    /// leader replicates to a follower starting at its `nextIndex`
    /// (`docs/raft-notes.md` §5.3).
    pub fn entries_from(&self, from: LogIndex) -> Result<Vec<LogEntry>, RaftError> {
        let read = self.db.begin_read().map_err(stor)?;
        let table = read.open_table(LOG_TABLE).map_err(stor)?;
        let mut entries = Vec::new();
        for item in table.range(from.get()..).map_err(stor)? {
            let (_key, value) = item.map_err(stor)?;
            entries.push(LogEntry::decode(value.value())?);
        }
        Ok(entries)
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

    /// The last entry's `(index, term)`, or `(ZERO, ZERO)` for an empty log, in a
    /// **single** read transaction. Hot paths (heartbeats, `RequestVote`,
    /// replication) need both together; calling `last_index()` then `last_term()`
    /// would re-read the log two or three times.
    pub fn last_index_and_term(&self) -> Result<(LogIndex, Term), RaftError> {
        let read = self.db.begin_read().map_err(stor)?;
        let table = read.open_table(LOG_TABLE).map_err(stor)?;
        let last = table.last().map_err(stor)?;
        let result = match last {
            Some((key, value)) => (
                LogIndex::new(key.value()),
                LogEntry::decode(value.value())?.term,
            ),
            None => (LogIndex::ZERO, Term::ZERO),
        };
        Ok(result)
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

    /// Atomically persists the hard state (`currentTerm` **and** `votedFor`) in
    /// a single durable transaction, then returns. Because both fields commit
    /// together, a crash can never expose a torn `(term, vote)` pair
    /// (`docs/raft-notes.md` §3, [`HardState`]). This is the core
    /// persist-before-respond primitive.
    pub fn save_hard_state(&self, state: &HardState) -> Result<(), RaftError> {
        let term_bytes = state.current_term.get().to_le_bytes();
        let write = self.db.begin_write().map_err(stor)?;
        {
            let mut table = write.open_table(META_TABLE).map_err(stor)?;
            table
                .insert(META_CURRENT_TERM, &term_bytes[..])
                .map_err(stor)?;
            match &state.voted_for {
                Some(id) => {
                    table
                        .insert(META_VOTED_FOR, id.as_str().as_bytes())
                        .map_err(stor)?;
                }
                None => {
                    table.remove(META_VOTED_FOR).map_err(stor)?;
                }
            }
        }
        write.commit().map_err(stor)?;
        Ok(())
    }

    /// Loads the persisted hard state, defaulting to [`HardState::new`] (term 0,
    /// no vote) on a fresh store.
    ///
    /// Both keys are read within a **single** redb read transaction, so the
    /// returned `(currentTerm, votedFor)` pair is always a consistent committed
    /// snapshot: a concurrent [`RaftLog::save_hard_state`] can never interleave
    /// between the two reads to yield a torn pair (e.g. an old term with a new
    /// vote). Reading them in separate transactions would reintroduce exactly
    /// the torn-state hazard this atomicity exists to prevent.
    pub fn load_hard_state(&self) -> Result<HardState, RaftError> {
        let read = self.db.begin_read().map_err(stor)?;
        let table = read.open_table(META_TABLE).map_err(stor)?;
        let current_term = match table.get(META_CURRENT_TERM).map_err(stor)? {
            Some(guard) => Term::new(u64_from_le(guard.value())?),
            None => Term::ZERO,
        };
        let voted_for = match table.get(META_VOTED_FOR).map_err(stor)? {
            Some(guard) => {
                let s = std::str::from_utf8(guard.value()).map_err(stor)?;
                Some(NodeId::new(s)?)
            }
            None => None,
        };
        Ok(HardState {
            current_term,
            voted_for,
        })
    }

    /// Loads `currentTerm`, or [`Term::ZERO`] if never set.
    pub fn current_term(&self) -> Result<Term, RaftError> {
        match self.get_meta(META_CURRENT_TERM)? {
            Some(bytes) => Ok(Term::new(u64_from_le(&bytes)?)),
            None => Ok(Term::ZERO),
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

    fn hard(term: u64, voted: Option<&str>) -> HardState {
        HardState {
            current_term: Term::new(term),
            voted_for: voted.map(|v| NodeId::new(v).unwrap()),
        }
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
            log.save_hard_state(&hard(3, Some("node-b"))).unwrap();
        }
        let log = RaftLog::open(&path).unwrap();
        assert_eq!(log.last_index().unwrap(), LogIndex::new(2));
        assert_eq!(log.load_hard_state().unwrap(), hard(3, Some("node-b")));
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
    fn hard_state_defaults_to_zero_no_vote() {
        let (_dir, log) = temp_log();
        assert_eq!(log.load_hard_state().unwrap(), HardState::new());
        assert_eq!(log.current_term().unwrap(), Term::ZERO);
        assert_eq!(log.voted_for().unwrap(), None);
    }

    #[test]
    fn hard_state_round_trips() {
        let (_dir, log) = temp_log();
        log.save_hard_state(&hard(9, Some("candidate-1"))).unwrap();
        assert_eq!(log.load_hard_state().unwrap(), hard(9, Some("candidate-1")));
    }

    #[test]
    fn stepping_to_higher_term_clears_vote_atomically() {
        let (_dir, log) = temp_log();
        // Voted for A in term 5.
        log.save_hard_state(&hard(5, Some("A"))).unwrap();
        assert_eq!(log.load_hard_state().unwrap(), hard(5, Some("A")));

        // Step to term 6: the vote must be gone, never (6, Some("A")).
        let stepped = log.load_hard_state().unwrap().stepped_to(Term::new(6));
        log.save_hard_state(&stepped).unwrap();
        assert_eq!(log.load_hard_state().unwrap(), hard(6, None));
    }

    #[test]
    fn uncommitted_write_is_invisible_after_reopen() {
        // Persist-before-respond: a write that is never committed (as if the
        // process crashed before the commit fsync) must leave no trace. Only
        // committed state is ever observable.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raft.redb");
        {
            let log = RaftLog::open(&path).unwrap();
            log.save_hard_state(&hard(5, Some("A"))).unwrap(); // committed

            // Begin a write that bumps the term to 999, then drop it WITHOUT
            // committing — redb rolls it back exactly as a crash would.
            let write = log.db.begin_write().unwrap();
            {
                let mut table = write.open_table(META_TABLE).unwrap();
                let bogus = 999u64.to_le_bytes();
                table.insert(META_CURRENT_TERM, &bogus[..]).unwrap();
            }
            drop(write); // no commit

            // The live handle still sees only the committed state.
            assert_eq!(log.load_hard_state().unwrap(), hard(5, Some("A")));
        }

        // Reopen from disk: the uncommitted term 999 never happened.
        let log = RaftLog::open(&path).unwrap();
        assert_eq!(log.load_hard_state().unwrap(), hard(5, Some("A")));
    }

    #[test]
    fn load_hard_state_reads_a_consistent_pair_under_concurrent_writes() {
        // Regression test for the torn-read hazard: `load_hard_state` must read
        // both keys in ONE transaction. The writer only ever commits pairs where
        // (term is odd) <=> (a vote is recorded). A non-atomic reader (two
        // separate read transactions) could observe an (odd, None) or
        // (even, Some) pair that was never committed together; the invariant
        // below would then fail.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(RaftLog::open(dir.path().join("raft.redb")).unwrap());
        log.save_hard_state(&hard(0, None)).unwrap();

        let done = Arc::new(AtomicBool::new(false));
        let writer = {
            let log = Arc::clone(&log);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                for term in 1u64..=400 {
                    let voted = if term % 2 == 1 { Some("n") } else { None };
                    log.save_hard_state(&hard(term, voted)).unwrap();
                }
                done.store(true, Ordering::SeqCst);
            })
        };

        // Read continuously for the whole write window (plus a minimum count).
        let mut reads = 0u32;
        while !done.load(Ordering::SeqCst) || reads < 200 {
            let hs = log.load_hard_state().unwrap();
            let term_is_odd = hs.current_term.get() % 2 == 1;
            assert_eq!(
                term_is_odd,
                hs.voted_for.is_some(),
                "torn read: term={} voted_for={:?}",
                hs.current_term.get(),
                hs.voted_for
            );
            reads += 1;
        }
        writer.join().unwrap();
    }

    #[test]
    fn committed_log_and_hard_state_recover_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raft.redb");
        {
            let log = RaftLog::open(&path).unwrap();
            log.append_all(&[entry(2, 1, b"a"), entry(2, 2, b"b")])
                .unwrap();
            log.save_hard_state(&hard(2, Some("leader"))).unwrap();
        }
        let log = RaftLog::open(&path).unwrap();
        assert_eq!(log.last_index().unwrap(), LogIndex::new(2));
        assert_eq!(log.last_term().unwrap(), Term::new(2));
        assert_eq!(log.load_hard_state().unwrap(), hard(2, Some("leader")));
    }
}
