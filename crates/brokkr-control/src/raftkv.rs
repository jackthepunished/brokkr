//! The Raft-replicated [`MetaKv`] (Phase 5 I8c, plan §17 task 6).
//!
//! [`RaftKv`] is the second implementation behind the I8a seam: writes are
//! prost-encoded [`KvCommand`]s proposed through `brokkr-raft` and
//! acknowledged only once **committed and applied** (so a subsequent
//! leader-local read observes them); reads run the **ReadIndex** protocol
//! ([`RaftHandle::read_index`]) and are served from the applied
//! [`KvMachine`] — linearizable end to end, per the I8 stop-and-ask.
//!
//! The materialized state is an in-memory ordered map shared between the
//! machine (the driver's single writer) and [`RaftKv`] (readers).
//! Durability lives a layer below, where it belongs: the Raft **log and
//! snapshots** are redb-backed, and a restart rebuilds the map by
//! restore-plus-tail-replay (P9) — the recovery path I8b already tests.
//!
//! A write reaching a non-leader replica fails with
//! [`MetaKvError::NotLeader`] carrying the leader hint; the service layer
//! turns that into gRPC `FAILED_PRECONDITION` + leader metadata so clients
//! redirect instead of failing hard (plan §17 task 7: "clients can talk to
//! any; followers redirect").

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use brokkr_raft::{LogEntry, RaftError, RaftHandle, StateMachine};
use bytes::Bytes;
use prost::Message;

use crate::metakv::{MetaKv, MetaKvError};

/// The replicated write operation, prost-encoded into a Raft log entry's
/// opaque command bytes.
#[derive(Clone, PartialEq, Message)]
pub struct KvCommand {
    /// The operation: [`KV_OP_PUT`] or [`KV_OP_DELETE`].
    #[prost(uint32, tag = "1")]
    pub op: u32,
    /// The namespaced key.
    #[prost(bytes = "vec", tag = "2")]
    pub key: Vec<u8>,
    /// The value (empty for deletes).
    #[prost(bytes = "vec", tag = "3")]
    pub value: Vec<u8>,
}

/// [`KvCommand::op`]: insert-or-overwrite.
pub const KV_OP_PUT: u32 = 1;
/// [`KvCommand::op`]: remove (a no-op if absent).
pub const KV_OP_DELETE: u32 = 2;

/// The materialized map, shared between the applying machine and readers.
type SharedMap = Arc<RwLock<BTreeMap<Vec<u8>, Bytes>>>;

/// The KV state machine the Raft driver applies committed entries to.
///
/// Only command payloads mutate state (`Noop`/`Config` entries are
/// consensus-internal); an undecodable command is skipped with a warning —
/// it can only mean corruption, and one poisoned entry must not wedge the
/// apply loop for everything behind it.
#[derive(Debug, Default)]
pub struct KvMachine {
    map: SharedMap,
}

impl KvMachine {
    /// The shared handle readers ([`RaftKv`]) use.
    pub fn shared(&self) -> SharedMap {
        Arc::clone(&self.map)
    }

    fn lock_write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<Vec<u8>, Bytes>> {
        // A poisoned lock means a writer panicked mid-mutation; the map is
        // a pure function of the applied log, so continuing is sound.
        self.map.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl StateMachine for KvMachine {
    fn apply(&mut self, entry: &LogEntry) {
        let Some(command) = entry.command() else {
            return; // Noop / Config entries carry no state-machine work
        };
        let decoded = match KvCommand::decode(command.as_ref()) {
            Ok(decoded) => decoded,
            Err(e) => {
                tracing::warn!(index = %entry.index, error = %e, "skipping undecodable KvCommand");
                return;
            }
        };
        let mut map = self.lock_write();
        match decoded.op {
            KV_OP_PUT => {
                map.insert(decoded.key, Bytes::from(decoded.value));
            }
            KV_OP_DELETE => {
                map.remove(&decoded.key);
            }
            other => {
                tracing::warn!(index = %entry.index, op = other, "skipping unknown KvCommand op");
            }
        }
    }

    fn snapshot(&self) -> Bytes {
        // Length-prefixed (key, value) pairs, ascending by key.
        let map = self.map.read().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for (key, value) in map.iter() {
            out.extend_from_slice(&(key.len() as u32).to_le_bytes());
            out.extend_from_slice(key);
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            out.extend_from_slice(value);
        }
        Bytes::from(out)
    }

    fn restore(&mut self, snapshot: &[u8]) {
        let mut rebuilt = BTreeMap::new();
        let mut rest = snapshot;
        while rest.len() >= 4 {
            let Some((key, after)) = take_chunk(rest) else {
                tracing::warn!("truncated KV snapshot: discarding the malformed tail");
                break;
            };
            let Some((value, after)) = take_chunk(after) else {
                tracing::warn!("truncated KV snapshot: discarding the malformed tail");
                break;
            };
            rebuilt.insert(key.to_vec(), Bytes::copy_from_slice(value));
            rest = after;
        }
        *self.lock_write() = rebuilt;
    }
}

/// Reads one `u32`-length-prefixed chunk, returning `(chunk, rest)`.
fn take_chunk(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    if bytes.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let rest = &bytes[4..];
    if rest.len() < len {
        return None;
    }
    Some((&rest[..len], &rest[len..]))
}

/// Maps a Raft-layer failure into the [`MetaKv`] error surface: the leader
/// hint survives structurally; everything else is a storage-layer failure.
fn kv_err(e: RaftError) -> MetaKvError {
    match e {
        RaftError::NotLeader { leader } => MetaKvError::NotLeader { leader },
        other => MetaKvError::Storage(other.to_string()),
    }
}

/// The Raft-backed [`MetaKv`]: linearizable writes via committed proposals,
/// linearizable reads via ReadIndex over the applied [`KvMachine`].
#[derive(Debug, Clone)]
pub struct RaftKv {
    handle: RaftHandle,
    map: SharedMap,
}

impl RaftKv {
    /// Wraps a running driver's handle and its machine's shared map
    /// ([`KvMachine::shared`]).
    pub fn new(handle: RaftHandle, map: SharedMap) -> Self {
        Self { handle, map }
    }

    fn read_map(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<Vec<u8>, Bytes>> {
        self.map.read().unwrap_or_else(|e| e.into_inner())
    }

    async fn write(&self, command: KvCommand) -> Result<(), MetaKvError> {
        let bytes = Bytes::from(command.encode_to_vec());
        self.handle.propose_committed(bytes).await.map_err(kv_err)?;
        Ok(())
    }
}

#[async_trait]
impl MetaKv for RaftKv {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>, MetaKvError> {
        // ReadIndex resolves only after the machine applied up to the
        // confirmed index (I8b) — the map read below is linearizable.
        self.handle.read_index().await.map_err(kv_err)?;
        Ok(self.read_map().get(key).cloned())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn put(&self, key: &[u8], value: Bytes) -> Result<(), MetaKvError> {
        self.write(KvCommand {
            op: KV_OP_PUT,
            key: key.to_vec(),
            value: value.to_vec(),
        })
        .await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn delete(&self, key: &[u8]) -> Result<(), MetaKvError> {
        self.write(KvCommand {
            op: KV_OP_DELETE,
            key: key.to_vec(),
            value: Vec::new(),
        })
        .await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Bytes, Bytes)>, MetaKvError> {
        self.handle.read_index().await.map_err(kv_err)?;
        let map = self.read_map();
        Ok(map
            .range(prefix.to_vec()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (Bytes::copy_from_slice(key), value.clone()))
            .collect())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]
mod tests {
    use std::time::Duration;

    use brokkr_raft::{Config as RaftConfig, InMemoryTransport, NodeId, RaftDriver, RaftNode, Rng};

    use super::*;

    /// Boots a single-voter Raft-backed KV on the real clock (the election
    /// resolves in one timeout; leadership is immediate thereafter).
    async fn single_voter_kv() -> (tempfile::TempDir, RaftKv) {
        let dir = tempfile::tempdir().unwrap();
        let log = brokkr_raft::RaftLog::open(dir.path().join("raft.redb")).unwrap();
        let node = RaftNode::new(
            NodeId::new("solo").unwrap(),
            Vec::new(),
            log,
            Rng::seed_from_u64(1),
            RaftConfig::default(),
            tokio::time::Instant::now().into_std(),
        )
        .unwrap();
        let machine = KvMachine::default();
        let shared = machine.shared();
        let (driver, handle) = RaftDriver::new(
            node,
            Box::new(machine),
            Arc::new(InMemoryTransport::new()),
            Duration::from_millis(10),
        );
        tokio::spawn(async move {
            if let Err(e) = driver.run().await {
                panic!("raft driver exited: {e}");
            }
        });
        for _ in 0..200 {
            if handle.status().await.unwrap().is_leader {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(handle.status().await.unwrap().is_leader);
        (dir, RaftKv::new(handle, shared))
    }

    /// The same contract `RedbMetaKv` satisfies (plan §17 task 6: "the
    /// contract suite runs against both impls").
    #[tokio::test]
    async fn raft_kv_satisfies_the_metakv_contract() {
        let (_dir, kv) = single_voter_kv().await;
        crate::metakv::tests::metakv_contract_suite(&kv).await;
    }

    #[tokio::test]
    async fn raft_kv_write_is_visible_to_a_following_read() {
        let (_dir, kv) = single_voter_kv().await;
        kv.put(b"cfg/cluster", Bytes::from_static(b"v1"))
            .await
            .unwrap();
        // propose_committed acks only after apply: the read MUST see it.
        assert_eq!(
            kv.get(b"cfg/cluster").await.unwrap(),
            Some(Bytes::from_static(b"v1"))
        );
    }

    #[test]
    fn kv_machine_snapshot_round_trips() {
        let mut machine = KvMachine::default();
        for (k, v) in [("a", "1"), ("b", "2"), ("c", "3")] {
            let cmd = KvCommand {
                op: KV_OP_PUT,
                key: k.as_bytes().to_vec(),
                value: v.as_bytes().to_vec(),
            };
            machine.apply(&brokkr_raft::LogEntry::new(
                brokkr_raft::Term::new(1),
                brokkr_raft::LogIndex::new(1),
                Bytes::from(cmd.encode_to_vec()),
            ));
        }
        let blob = machine.snapshot();

        let mut restored = KvMachine::default();
        restored.restore(&blob);
        assert_eq!(restored.snapshot(), blob, "restore(snapshot(s)) == s");

        // A truncated blob keeps the intact prefix and drops the tail.
        let mut partial = KvMachine::default();
        partial.restore(&blob[..blob.len() - 3]);
        let partial_map = partial.shared();
        let map = partial_map.read().unwrap();
        assert!(map.len() < 3 && map.contains_key(b"a".as_slice()));
    }
}
