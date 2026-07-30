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

/// Key prefix for per-node cluster configuration (`cfg/nodes/<node-id>`) —
/// the `cfg/` namespace I8a reserved for cluster-level configuration.
pub const CFG_NODES_PREFIX: &[u8] = b"cfg/nodes/";

/// The replicated key holding `node_id`'s [`NodeRecord`].
pub fn node_record_key(node_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(CFG_NODES_PREFIX.len() + node_id.len());
    key.extend_from_slice(CFG_NODES_PREFIX);
    key.extend_from_slice(node_id.as_bytes());
    key
}

/// What a control-plane node publishes about itself, so that any replica can
/// turn a leader *id* into something a client can dial (I9b).
///
/// Only the **leader** publishes, and only its own record — a follower cannot
/// propose, and the leader's address is the only one a redirect ever needs.
/// prost-encoded rather than added to a `.proto`, matching [`KvCommand`]:
/// these are internal replicated values, not a wire contract.
#[derive(Clone, PartialEq, Message)]
pub struct NodeRecord {
    /// The client-plane address (`host:port`) clients should dial. Never a
    /// wildcard bind — `brokkr-control` refuses to publish one.
    #[prost(string, tag = "1")]
    pub advertise_addr: String,
}

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

    /// The client-plane address `node_id` published, if its [`NodeRecord`] has
    /// been applied here.
    ///
    /// Read straight from the applied map, **without** ReadIndex. This is a
    /// routing *hint*: demanding linearizability for it would be pointless
    /// (the leader can change the instant after we answer) and impossible
    /// anyway — this runs on a follower, which cannot serve a linearizable
    /// read. A stale-but-plausible address costs the client one failed dial;
    /// blocking on consensus to produce it would cost every redirect a round
    /// trip.
    pub fn published_addr(&self, node_id: &str) -> Option<String> {
        let raw = self.read_map().get(&node_record_key(node_id)).cloned()?;
        match NodeRecord::decode(raw.as_ref()) {
            Ok(record) if !record.advertise_addr.is_empty() => Some(record.advertise_addr),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(node_id, error = %e, "undecodable NodeRecord; ignoring the address hint");
                None
            }
        }
    }

    /// Whether this replica currently believes it holds leadership — the gate
    /// for leader-only work such as [`Self::publish_node_record`].
    pub async fn is_leader(&self) -> Result<bool, MetaKvError> {
        self.handle
            .status()
            .await
            .map(|status| status.is_leader)
            .map_err(|e| self.kv_err(e))
    }

    /// Publishes this node's own [`NodeRecord`] — a leader-only operation
    /// (see [`NodeRecord`]). Idempotent: skipped when the applied value
    /// already matches, so it costs at most one entry per leadership term.
    pub async fn publish_node_record(
        &self,
        node_id: &str,
        advertise_addr: &str,
    ) -> Result<(), MetaKvError> {
        if self.published_addr(node_id).as_deref() == Some(advertise_addr) {
            return Ok(());
        }
        let record = NodeRecord {
            advertise_addr: advertise_addr.to_string(),
        };
        self.write(KvCommand {
            op: KV_OP_PUT,
            key: node_record_key(node_id),
            value: record.encode_to_vec(),
        })
        .await
    }

    /// Maps a Raft-layer failure onto the [`MetaKv`] error surface, resolving
    /// the leader's published address so the redirect is actionable (I9b).
    /// The leader hint survives structurally; everything else is a
    /// storage-layer failure.
    fn kv_err(&self, e: RaftError) -> MetaKvError {
        match e {
            RaftError::NotLeader { leader } => {
                let leader_addr = leader.as_deref().and_then(|id| self.published_addr(id));
                MetaKvError::NotLeader {
                    leader,
                    leader_addr,
                }
            }
            other => MetaKvError::Storage(other.to_string()),
        }
    }

    async fn write(&self, command: KvCommand) -> Result<(), MetaKvError> {
        let bytes = Bytes::from(command.encode_to_vec());
        self.handle
            .propose_committed(bytes)
            .await
            .map_err(|e| self.kv_err(e))?;
        Ok(())
    }
}

#[async_trait]
impl MetaKv for RaftKv {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>, MetaKvError> {
        // ReadIndex resolves only after the machine applied up to the
        // confirmed index (I8b) — the map read below is linearizable.
        self.handle.read_index().await.map_err(|e| self.kv_err(e))?;
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
        self.handle.read_index().await.map_err(|e| self.kv_err(e))?;
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
    fn node_record_keys_are_namespaced_and_disjoint_from_the_ac_prefix() {
        let key = node_record_key("control-1");
        assert_eq!(key, b"cfg/nodes/control-1".to_vec());
        assert!(key.starts_with(CFG_NODES_PREFIX));
        // The action-cache namespace must never collide with cluster config:
        // one `MetaKv` instance carries both (I8a).
        assert!(!key.starts_with(b"ac/"));
        // Distinct ids get distinct keys, and no id is a prefix-collision of
        // another under `scan_prefix`.
        assert_ne!(node_record_key("control-1"), node_record_key("control-11"));
    }

    #[tokio::test]
    async fn the_leader_publishes_its_address_and_any_replica_resolves_it() {
        let (_dir, kv) = single_voter_kv().await;
        assert_eq!(kv.published_addr("solo"), None, "nothing published yet");

        kv.publish_node_record("solo", "10.0.0.1:7878")
            .await
            .unwrap();
        assert_eq!(
            kv.published_addr("solo"),
            Some("10.0.0.1:7878".to_string()),
            "the applied record resolves without a ReadIndex round trip"
        );

        // Idempotent: republishing the same address proposes nothing new.
        kv.publish_node_record("solo", "10.0.0.1:7878")
            .await
            .unwrap();
        assert_eq!(kv.published_addr("solo"), Some("10.0.0.1:7878".to_string()));

        // A changed address (restart on a new port) overwrites.
        kv.publish_node_record("solo", "10.0.0.1:9999")
            .await
            .unwrap();
        assert_eq!(kv.published_addr("solo"), Some("10.0.0.1:9999".to_string()));

        // An unknown node has no address, and that is not an error.
        assert_eq!(kv.published_addr("nobody"), None);
    }

    #[tokio::test]
    async fn a_corrupt_or_empty_node_record_yields_no_address_rather_than_panicking() {
        let (_dir, kv) = single_voter_kv().await;

        // Garbage under a well-formed key: a decode failure must degrade to
        // "no hint", never take down the redirect path.
        kv.put(
            &node_record_key("garbled"),
            Bytes::from_static(&[0xff, 0xff, 0xff]),
        )
        .await
        .unwrap();
        assert_eq!(kv.published_addr("garbled"), None);

        // A record that decodes but carries no address is also just "no hint".
        let empty = NodeRecord {
            advertise_addr: String::new(),
        };
        kv.put(
            &node_record_key("blank"),
            Bytes::from(empty.encode_to_vec()),
        )
        .await
        .unwrap();
        assert_eq!(kv.published_addr("blank"), None);
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
