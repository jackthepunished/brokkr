//! The control plane's replicable metadata store (Phase 5 I8, plan §17
//! task 6).
//!
//! [`MetaKv`] is the seam the I8 stop-and-ask decided on: **action-cache
//! writes and cluster-level configuration replicate through Raft; CAS blob
//! bytes (Phase 3 quorum replication owns those) and scheduler
//! queue/registry/leases (ephemeral by ADR design) do not.** Everything that
//! must survive a control-plane leader kill goes through this trait; two
//! implementations are planned:
//!
//! - [`RedbMetaKv`] (here): the existing single-node redb storage, unchanged
//!   in behavior — the default, and exactly today's semantics.
//! - `RaftKv` (I8c): proposes prost-encoded commands through `brokkr-raft`,
//!   applies committed entries to a redb-backed materialized state machine,
//!   and serves linearizable reads via ReadIndex.
//!
//! Keys are namespaced byte strings (`ac/<digest-hash>` for action-cache
//! entries; cluster config claims its own prefix in I8c), so one KV instance
//! carries every replicated namespace and `scan_prefix` recovers a namespace
//! wholesale.
//!
//! [`MetaKvActionCache`] adapts any [`MetaKv`] to the REAPI
//! [`ActionCache`] trait, which is what the scheduler and the
//! `ActionCacheService` actually consume (`Arc<dyn ActionCache>`), keeping
//! the rest of the control plane oblivious to the storage seam.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use brokkr_cas::{ActionCache, CasError};
use brokkr_common::Digest;
use brokkr_proto::reapi_v2::ActionResult;
use bytes::Bytes;
use prost::Message;
use redb::{Database, TableDefinition};
use thiserror::Error;
use tokio::sync::Semaphore;

/// Single table for all metadata namespaces: namespaced key → value bytes.
const META_KV: TableDefinition<'static, &[u8], &[u8]> = TableDefinition::new("meta_kv");

/// Key prefix for action-cache entries (`ac/<digest-hash>`).
const AC_PREFIX: &[u8] = b"ac/";

/// Default max concurrent `spawn_blocking` tasks for [`RedbMetaKv`].
const DEFAULT_META_KV_CONCURRENCY: usize = 16;

/// Errors surfaced by a [`MetaKv`] backend.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MetaKvError {
    /// The underlying store failed (open, read, write, or commit).
    #[error("metadata store error: {0}")]
    Storage(String),

    /// Too many concurrent operations in flight.
    #[error("metadata store concurrency limit ({limit}) exceeded")]
    ThroughputLimit {
        /// The configured concurrency limit.
        limit: usize,
    },

    /// The write was routed to a replica that is not the Raft leader (I8c).
    /// Carries the leader's identity when known, so the caller can redirect.
    #[error("not the metadata leader (leader hint: {leader:?} at {leader_addr:?})")]
    NotLeader {
        /// The current leader, if known.
        leader: Option<String>,

        /// The leader's client-plane advertise address, once that leader's
        /// `cfg/nodes/<id>` record has replicated here (I9b). `None` is normal
        /// in the window between an election and that record committing — the
        /// id alone is still a useful hint, so it is not withheld.
        leader_addr: Option<String>,
    },
}

impl From<MetaKvError> for CasError {
    fn from(e: MetaKvError) -> Self {
        // Exhaustive on purpose (the I8a forcing function, now tripped by
        // I8c exactly as designed): every variant gets a deliberate mapping,
        // and NotLeader keeps its structured leader hint all the way to the
        // service layer (FAILED_PRECONDITION + leader metadata).
        match e {
            MetaKvError::Storage(msg) => CasError::Redb(msg),
            MetaKvError::ThroughputLimit { limit } => CasError::ThroughputLimit { limit },
            MetaKvError::NotLeader {
                leader,
                leader_addr,
            } => CasError::NotLeader {
                leader,
                leader_addr,
            },
        }
    }
}

/// Get/put/delete/scan over opaque bytes — the replication seam for the
/// control plane's durable metadata (see the module docs for what does and
/// does not go through here).
#[async_trait]
pub trait MetaKv: Send + Sync + 'static {
    /// The value stored under `key`, or `None`.
    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>, MetaKvError>;

    /// Stores `value` under `key`, overwriting any previous value.
    async fn put(&self, key: &[u8], value: Bytes) -> Result<(), MetaKvError>;

    /// Removes `key` (a no-op if absent).
    async fn delete(&self, key: &[u8]) -> Result<(), MetaKvError>;

    /// Every `(key, value)` whose key starts with `prefix`, ascending by key.
    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Bytes, Bytes)>, MetaKvError>;
}

/// `redb`-backed [`MetaKv`] — the single-node default. Same discipline as
/// the other redb stores: every operation runs on `spawn_blocking`, bounded
/// by a semaphore so a burst degrades into a clean
/// [`MetaKvError::ThroughputLimit`] instead of unbounded blocking tasks.
#[derive(Debug, Clone)]
pub struct RedbMetaKv {
    db: Arc<Database>,
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
}

impl RedbMetaKv {
    /// Opens (creating if absent) the store at `path` with the default
    /// concurrency limit.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MetaKvError> {
        Self::open_with_limit(path, DEFAULT_META_KV_CONCURRENCY)
    }

    /// Opens (creating if absent) the store at `path`; at most
    /// `max_concurrent` operations run concurrently.
    pub fn open_with_limit(
        path: impl AsRef<Path>,
        max_concurrent: usize,
    ) -> Result<Self, MetaKvError> {
        let db = Database::create(path.as_ref()).map_err(stor)?;
        let txn = db.begin_write().map_err(stor)?;
        {
            // Opening a table in a write txn creates it if absent.
            let _ = txn.open_table(META_KV).map_err(stor)?;
        }
        txn.commit().map_err(stor)?;
        Ok(Self {
            db: Arc::new(db),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
        })
    }

    /// Runs `f` against the database on the blocking pool, bounded by the
    /// semaphore. The permit **moves into the blocking task**, so the
    /// concurrency bound holds even when the calling future is cancelled
    /// mid-operation (a dropped RPC must not leak an unbounded blocking task
    /// past the limit).
    async fn run_blocking<T, F>(&self, f: F) -> Result<T, MetaKvError>
    where
        T: Send + 'static,
        F: FnOnce(&Database) -> Result<T, MetaKvError> + Send + 'static,
    {
        let permit = self.semaphore.clone().try_acquire_owned().map_err(|_| {
            MetaKvError::ThroughputLimit {
                limit: self.max_concurrent,
            }
        })?;
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            f(&db)
        })
        .await
        .map_err(stor)?
    }
}

/// Maps any `Display` storage error into [`MetaKvError::Storage`].
fn stor<E: std::fmt::Display>(e: E) -> MetaKvError {
    MetaKvError::Storage(e.to_string())
}

#[async_trait]
impl MetaKv for RedbMetaKv {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>, MetaKvError> {
        let key = key.to_vec();
        self.run_blocking(move |db| {
            let txn = db.begin_read().map_err(stor)?;
            let table = txn.open_table(META_KV).map_err(stor)?;
            Ok(table
                .get(key.as_slice())
                .map_err(stor)?
                .map(|guard| Bytes::copy_from_slice(guard.value())))
        })
        .await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn put(&self, key: &[u8], value: Bytes) -> Result<(), MetaKvError> {
        let key = key.to_vec();
        self.run_blocking(move |db| {
            let txn = db.begin_write().map_err(stor)?;
            {
                let mut table = txn.open_table(META_KV).map_err(stor)?;
                table.insert(key.as_slice(), value.as_ref()).map_err(stor)?;
            }
            txn.commit().map_err(stor)
        })
        .await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn delete(&self, key: &[u8]) -> Result<(), MetaKvError> {
        let key = key.to_vec();
        self.run_blocking(move |db| {
            let txn = db.begin_write().map_err(stor)?;
            {
                let mut table = txn.open_table(META_KV).map_err(stor)?;
                table.remove(key.as_slice()).map_err(stor)?;
            }
            txn.commit().map_err(stor)
        })
        .await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Bytes, Bytes)>, MetaKvError> {
        let prefix = prefix.to_vec();
        self.run_blocking(move |db| {
            let txn = db.begin_read().map_err(stor)?;
            let table = txn.open_table(META_KV).map_err(stor)?;
            let mut out = Vec::new();
            for item in table.range(prefix.as_slice()..).map_err(stor)? {
                let (k, v) = item.map_err(stor)?;
                if !k.value().starts_with(&prefix) {
                    break; // past the namespace: keys are ordered
                }
                out.push((
                    Bytes::copy_from_slice(k.value()),
                    Bytes::copy_from_slice(v.value()),
                ));
            }
            Ok(out)
        })
        .await
    }
}

/// Adapts any [`MetaKv`] to the REAPI [`ActionCache`] trait: entries live
/// under `ac/<digest-hash>` as protobuf-encoded [`ActionResult`]s. This is
/// what `main` hands the scheduler and `ActionCacheService`; swapping the
/// inner KV for `RaftKv` (I8c) replicates the action cache without touching
/// either consumer.
#[derive(Debug, Clone)]
pub struct MetaKvActionCache<K: MetaKv> {
    kv: Arc<K>,
}

impl<K: MetaKv> MetaKvActionCache<K> {
    /// Wraps a metadata store.
    pub fn new(kv: Arc<K>) -> Self {
        Self { kv }
    }

    fn key(action_digest: &Digest) -> Vec<u8> {
        [AC_PREFIX, action_digest.hash().as_bytes()].concat()
    }
}

#[async_trait]
impl<K: MetaKv> ActionCache for MetaKvActionCache<K> {
    #[tracing::instrument(
        level = "info",
        name = "metakv::get_action_result",
        skip_all,
        fields(action = %action_digest.hash())
    )]
    async fn get_action_result(
        &self,
        action_digest: &Digest,
    ) -> Result<Option<ActionResult>, CasError> {
        let Some(bytes) = self.kv.get(&Self::key(action_digest)).await? else {
            return Ok(None);
        };
        let decoded = ActionResult::decode(bytes.as_ref())
            .map_err(|e| CasError::Redb(format!("ActionResult decode: {e}")))?;
        Ok(Some(decoded))
    }

    #[tracing::instrument(
        level = "info",
        name = "metakv::update_action_result",
        skip_all,
        fields(action = %action_digest.hash())
    )]
    async fn update_action_result(
        &self,
        action_digest: &Digest,
        result: ActionResult,
    ) -> Result<(), CasError> {
        let bytes = Bytes::from(result.encode_to_vec());
        self.kv.put(&Self::key(action_digest), bytes).await?;
        Ok(())
    }

    #[tracing::instrument(level = "info", name = "metakv::list_entries", skip_all)]
    async fn list_entries(&self) -> Result<Vec<(Digest, ActionResult)>, CasError> {
        let entries = self.kv.scan_prefix(AC_PREFIX).await?;
        let mut out = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            // Key layout: `ac/<hash-hex>`. A malformed entry must not fail
            // the whole GC sweep (matches RedbActionCache) — but on what
            // becomes the replicated path (I8c) it must not be invisible
            // either: warn on every skip.
            let Ok(hash) = std::str::from_utf8(&key[AC_PREFIX.len()..]) else {
                tracing::warn!(key = ?key, "skipping action-cache entry with non-utf8 key");
                continue;
            };
            // As in `RedbActionCache::list_entries`, the digest size is a
            // stand-in (the encoded length); only the hash keys the cache.
            let Ok(action_digest) = Digest::new(hash.to_string(), value.len() as i64) else {
                tracing::warn!(
                    hash,
                    "skipping action-cache entry with an invalid digest hash"
                );
                continue;
            };
            let Ok(decoded) = ActionResult::decode(value.as_ref()) else {
                tracing::warn!(hash, "skipping an undecodable cached ActionResult");
                continue;
            };
            out.push((action_digest, decoded));
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]
pub(crate) mod tests {
    use super::*;

    /// The [`MetaKv`] contract every implementation must satisfy. I8c runs
    /// this same suite against `RaftKv`.
    pub(crate) async fn metakv_contract_suite<K: MetaKv>(kv: &K) {
        // Miss.
        assert_eq!(kv.get(b"absent").await.unwrap(), None);

        // Put / get round trip.
        kv.put(b"k1", Bytes::from_static(b"v1")).await.unwrap();
        assert_eq!(
            kv.get(b"k1").await.unwrap(),
            Some(Bytes::from_static(b"v1"))
        );

        // Overwrite wins.
        kv.put(b"k1", Bytes::from_static(b"v2")).await.unwrap();
        assert_eq!(
            kv.get(b"k1").await.unwrap(),
            Some(Bytes::from_static(b"v2"))
        );

        // Delete removes; deleting the absent key is a no-op.
        kv.delete(b"k1").await.unwrap();
        assert_eq!(kv.get(b"k1").await.unwrap(), None);
        kv.delete(b"k1").await.unwrap();

        // Prefix scan honors namespace boundaries and orders ascending.
        kv.put(b"ac/2", Bytes::from_static(b"b")).await.unwrap();
        kv.put(b"ac/1", Bytes::from_static(b"a")).await.unwrap();
        kv.put(b"cfg/x", Bytes::from_static(b"c")).await.unwrap();
        // `ac0` sorts after `ac/` but does not share the prefix.
        kv.put(b"ac0", Bytes::from_static(b"d")).await.unwrap();
        let scanned = kv.scan_prefix(b"ac/").await.unwrap();
        assert_eq!(
            scanned,
            vec![
                (Bytes::from_static(b"ac/1"), Bytes::from_static(b"a")),
                (Bytes::from_static(b"ac/2"), Bytes::from_static(b"b")),
            ]
        );
        let empty = kv.scan_prefix(b"nothing/").await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn redb_metakv_satisfies_the_contract() {
        let dir = tempfile::tempdir().unwrap();
        let kv = RedbMetaKv::open(dir.path().join("meta.redb")).unwrap();
        metakv_contract_suite(&kv).await;
    }

    #[tokio::test]
    async fn redb_metakv_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.redb");
        {
            let kv = RedbMetaKv::open(&path).unwrap();
            kv.put(b"k", Bytes::from_static(b"v")).await.unwrap();
        }
        let kv = RedbMetaKv::open(&path).unwrap();
        assert_eq!(kv.get(b"k").await.unwrap(), Some(Bytes::from_static(b"v")));
    }

    #[tokio::test]
    async fn redb_metakv_enforces_the_concurrency_limit() {
        let dir = tempfile::tempdir().unwrap();
        let kv = RedbMetaKv::open_with_limit(dir.path().join("meta.redb"), 1).unwrap();
        kv.put(b"k", Bytes::from_static(b"v")).await.unwrap();

        let first = kv.get(b"k");
        let second = kv.get(b"k");
        match tokio::join!(first, second) {
            (Ok(_), Err(MetaKvError::ThroughputLimit { limit }))
            | (Err(MetaKvError::ThroughputLimit { limit }), Ok(_)) => assert_eq!(limit, 1),
            // Both ok — one finished before the other started.
            (Ok(_), Ok(_)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    fn sample_result() -> ActionResult {
        ActionResult {
            stdout_raw: b"hello world\n".to_vec(),
            exit_code: 0,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn action_cache_round_trips_through_the_kv() {
        let dir = tempfile::tempdir().unwrap();
        let kv = Arc::new(RedbMetaKv::open(dir.path().join("meta.redb")).unwrap());
        let cache = MetaKvActionCache::new(kv);

        let d = Digest::of(b"action-1");
        assert!(cache.get_action_result(&d).await.unwrap().is_none());
        cache
            .update_action_result(&d, sample_result())
            .await
            .unwrap();
        let got = cache.get_action_result(&d).await.unwrap().unwrap();
        assert_eq!(got.stdout_raw, b"hello world\n");

        // Overwrite wins, exactly like RedbActionCache.
        let updated = ActionResult {
            stdout_raw: b"second".to_vec(),
            exit_code: 7,
            ..Default::default()
        };
        cache.update_action_result(&d, updated).await.unwrap();
        let got = cache.get_action_result(&d).await.unwrap().unwrap();
        assert_eq!(got.exit_code, 7);
    }

    #[tokio::test]
    async fn action_cache_list_entries_sees_only_the_ac_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let kv = Arc::new(RedbMetaKv::open(dir.path().join("meta.redb")).unwrap());
        // Foreign namespace data must not leak into the GC sweep.
        kv.put(b"cfg/cluster", Bytes::from_static(b"not an action result"))
            .await
            .unwrap();
        let cache = MetaKvActionCache::new(kv);

        cache
            .update_action_result(&Digest::of(b"a1"), sample_result())
            .await
            .unwrap();
        cache
            .update_action_result(&Digest::of(b"a2"), sample_result())
            .await
            .unwrap();
        let entries = cache.list_entries().await.unwrap();
        assert_eq!(entries.len(), 2);
        for (digest, result) in entries {
            assert_eq!(digest.hash().len(), 64, "sha256 hex hash keys");
            assert_eq!(result.stdout_raw, b"hello world\n");
        }
    }
}
