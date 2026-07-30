//! REAPI Action Cache.
//!
//! Maps action digest → serialized [`brokkr_proto::reapi_v2::ActionResult`]
//! protobuf bytes. Phase 1 storage is single-node `redb`; semantics match the
//! REAPI `ActionCache` service (`GetActionResult`, `UpdateActionResult`).

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use brokkr_common::Digest;
use brokkr_proto::reapi_v2::ActionResult;
use prost::Message;
use redb::{Database, ReadableTable, TableDefinition};
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};

use crate::error::CasError;

const ACTION_RESULTS: TableDefinition<'static, &str, &[u8]> =
    TableDefinition::new("action_results");

/// Default max concurrent `spawn_blocking` tasks for [`RedbActionCache`].
const DEFAULT_ACTION_CACHE_CONCURRENCY: usize = 16;

/// Opaque handle representing exclusive access to the GC critical section.
///
/// Returned from [`ActionCache::gc_window`]. Drop it to release the
/// barrier. The guard is the in-process coordination mechanism that
/// closes the mark/sweep race described in issue #144: as long as a
/// worker holds a guard across `(cas.batch_update_blobs, ac.update_action_result)`
/// and the GC holds a guard across `(plan, sweep_with_plan)`, no
/// interleave can produce a live `ActionResult` whose digests point
/// at blobs that have been deleted.
///
/// The default trait impl returns a guard whose `_permit` is `None`
/// — i.e. a no-op. Production backends that can be observed by the
/// GC (anything other than an ephemeral test double) MUST override
/// [`ActionCache::gc_window`] and return a guard that releases on
/// drop.
#[must_use = "the GC barrier is held only while the guard is alive"]
pub struct GcWindowGuard {
    _permit: Option<OwnedMutexGuard<()>>,
}

impl fmt::Debug for GcWindowGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // OwnedMutexGuard doesn't implement Debug; we don't want to
        // surface internal state to log lines. Opacity is the point.
        f.debug_struct("GcWindowGuard")
            .field("acquired", &self._permit.is_some())
            .finish()
    }
}

/// REAPI Action Cache backend.
#[async_trait]
pub trait ActionCache: Send + Sync + 'static {
    /// Look up the cached `ActionResult` for an action digest.
    async fn get_action_result(
        &self,
        action_digest: &Digest,
    ) -> Result<Option<ActionResult>, CasError>;

    /// Insert or overwrite the cached `ActionResult` for an action digest.
    async fn update_action_result(
        &self,
        action_digest: &Digest,
        result: ActionResult,
    ) -> Result<(), CasError>;

    /// Enumerate every cached `ActionResult`. Used by GC (M5) to
    /// build the reachability set. Default implementation returns
    /// empty so non-GC-aware backends still compile.
    ///
    /// Each entry is `(action_digest, ActionResult)`. Order is
    /// implementation-defined.
    async fn list_entries(&self) -> Result<Vec<(Digest, ActionResult)>, CasError> {
        Ok(Vec::new())
    }

    /// Acquire the GC coordination barrier (issue #144).
    ///
    /// Workers performing the `(cas.batch_update_blobs,
    /// ac.update_action_result)` pair for the same logical action
    /// MUST hold the returned [`GcWindowGuard`] for the duration of
    /// both calls. [`crate::gc::sweep`] holds the guard for the
    /// entire `plan + sweep_with_plan` window. With the guard held
    /// on both sides, no `(upload, AC-write)` pair can land in
    /// between `cas.list_digests()` and `cas.delete_blob(d)` and
    /// produce a cached `ActionResult` whose digests point at
    /// deleted blobs.
    ///
    /// The default implementation returns a no-op guard (the inner
    /// `OwnedMutexGuard<()>` is `None`). Backends that need GC
    /// coordination MUST override this and return a guard that
    /// releases on drop. Backends that override without holding a
    /// guard across both writes regress the bug.
    ///
    /// **Scope:** this is an in-process barrier. A worker in a
    /// separate process uploading via gRPC cannot hold this
    /// barrier; that race is fixed separately (planned M5b/Phase 4).
    async fn gc_window(&self) -> Result<GcWindowGuard, CasError> {
        Ok(GcWindowGuard { _permit: None })
    }
}

/// `redb`-backed [`ActionCache`].
#[derive(Debug)]
pub struct RedbActionCache {
    db: Arc<Database>,
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
    // Coordination barrier for GC (issue #144). Acquired by
    // `gc_window`; held across `(CAS write, AC write)` by workers and
    // across `plan + sweep_with_plan` by `brokkr_cas::gc::sweep`.
    // Shared (not per-cache-clone) because workers and the GC may
    // observe *different* `Clone`s of the same logical cache.
    gc_mutex: Arc<Mutex<()>>,
}

impl Clone for RedbActionCache {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            semaphore: self.semaphore.clone(),
            max_concurrent: self.max_concurrent,
            gc_mutex: self.gc_mutex.clone(),
        }
    }
}

impl RedbActionCache {
    /// Open or create an action-cache database at `path` with the default concurrency limit.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CasError> {
        Self::open_with_limit(path, DEFAULT_ACTION_CACHE_CONCURRENCY)
    }

    /// Open or create an action-cache database at `path` with a custom concurrency limit.
    ///
    /// `max_concurrent` bounds the number of simultaneous `spawn_blocking` tasks
    /// for redb I/O. Requests that would exceed this limit return
    /// `CasError::ThroughputLimit`.
    pub fn open_with_limit(
        path: impl AsRef<Path>,
        max_concurrent: usize,
    ) -> Result<Self, CasError> {
        let db = Database::create(path.as_ref())?;
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(ACTION_RESULTS)?;
        }
        txn.commit()?;
        Ok(Self {
            db: Arc::new(db),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            gc_mutex: Arc::new(Mutex::new(())),
        })
    }
}

#[async_trait]
impl ActionCache for RedbActionCache {
    async fn get_action_result(
        &self,
        action_digest: &Digest,
    ) -> Result<Option<ActionResult>, CasError> {
        let _permit = self
            .semaphore
            .try_acquire()
            .map_err(|_| CasError::ThroughputLimit {
                limit: self.max_concurrent,
            })?;
        let db = self.db.clone();
        let key = action_digest.hash().to_string();
        let span = tracing::info_span!("redb::get_action_result");
        tokio::task::spawn_blocking(move || {
            let _guard = span.enter();
            let txn = db.begin_read()?;
            let table = txn.open_table(ACTION_RESULTS)?;
            let Some(entry) = table.get(key.as_str())? else {
                return Ok(None);
            };
            let bytes = entry.value();
            let decoded = ActionResult::decode(bytes)
                .map_err(|e| CasError::Redb(format!("ActionResult decode: {e}")))?;
            Ok(Some(decoded))
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
    }

    async fn update_action_result(
        &self,
        action_digest: &Digest,
        result: ActionResult,
    ) -> Result<(), CasError> {
        let _permit = self
            .semaphore
            .try_acquire()
            .map_err(|_| CasError::ThroughputLimit {
                limit: self.max_concurrent,
            })?;
        let db = self.db.clone();
        let key = action_digest.hash().to_string();
        let mut buf = Vec::with_capacity(result.encoded_len());
        result
            .encode(&mut buf)
            .map_err(|e| CasError::Redb(format!("ActionResult encode: {e}")))?;
        let span = tracing::info_span!("redb::update_action_result");
        tokio::task::spawn_blocking(move || {
            let _guard = span.enter();
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(ACTION_RESULTS)?;
                table.insert(key.as_str(), buf.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
    }

    async fn list_entries(&self) -> Result<Vec<(Digest, ActionResult)>, CasError> {
        let _permit = self
            .semaphore
            .try_acquire()
            .map_err(|_| CasError::ThroughputLimit {
                limit: self.max_concurrent,
            })?;
        let db = self.db.clone();
        let span = tracing::info_span!("redb::list_entries");
        tokio::task::spawn_blocking(move || {
            let _guard = span.enter();
            let txn = db.begin_read()?;
            let table = txn.open_table(ACTION_RESULTS)?;
            let mut out = Vec::new();
            for entry in table.iter()? {
                let (key, value) = entry?;
                let hash = key.value().to_string();
                let bytes = value.value();
                // Decode the stored ActionResult. The size we
                // pass to `Digest::new` is the *encoded length*
                // of the ActionResult; the action's digest is
                // keyed on the encoded Action, not on the result.
                // We construct the key digest via a size of zero
                // (the only field that matters for keying is the
                // hash hex) — but a Digest with `size_bytes=0`
                // would fail other validation, so we use the
                // value's byte length as a stand-in.
                let action_digest = match Digest::new(hash, bytes.len() as i64) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                let decoded = match ActionResult::decode(bytes) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                out.push((action_digest, decoded));
            }
            Ok(out)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
    }

    async fn gc_window(&self) -> Result<GcWindowGuard, CasError> {
        // `lock_owned` returns an `OwnedMutexGuard<()>` which is
        // `Send + 'static` — required because the guard is held
        // across `await` points while being passed by value
        // through the public trait method. It is also cancel-safe:
        // dropping the future (e.g. task abort) drops the guard
        // and releases the mutex, so a worker that is cancelled
        // mid-`(CAS-write, AC-write)` does not deadlock the GC.
        let permit = self.gc_mutex.clone().lock_owned().await;
        Ok(GcWindowGuard {
            _permit: Some(permit),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn sample_result() -> ActionResult {
        ActionResult {
            stdout_raw: b"hello world\n".to_vec(),
            exit_code: 0,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn miss_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RedbActionCache::open(dir.path().join("ac.redb")).unwrap();
        let d = Digest::of(b"any action");
        let got = cache.get_action_result(&d).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn update_then_get_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RedbActionCache::open(dir.path().join("ac.redb")).unwrap();
        let d = Digest::of(b"action-1");
        let r = sample_result();
        cache.update_action_result(&d, r.clone()).await.unwrap();
        let got = cache.get_action_result(&d).await.unwrap().unwrap();
        assert_eq!(got.stdout_raw, r.stdout_raw);
        assert_eq!(got.exit_code, 0);
    }

    #[tokio::test]
    async fn second_update_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RedbActionCache::open(dir.path().join("ac.redb")).unwrap();
        let d = Digest::of(b"action-2");
        cache
            .update_action_result(&d, sample_result())
            .await
            .unwrap();
        let updated = ActionResult {
            stdout_raw: b"second".to_vec(),
            exit_code: 7,
            ..Default::default()
        };
        cache.update_action_result(&d, updated).await.unwrap();
        let got = cache.get_action_result(&d).await.unwrap().unwrap();
        assert_eq!(got.stdout_raw, b"second");
        assert_eq!(got.exit_code, 7);
    }

    #[tokio::test]
    async fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ac.redb");
        let d = Digest::of(b"persist");
        {
            let cache = RedbActionCache::open(&path).unwrap();
            cache
                .update_action_result(&d, sample_result())
                .await
                .unwrap();
        }
        let cache = RedbActionCache::open(&path).unwrap();
        let got = cache.get_action_result(&d).await.unwrap().unwrap();
        assert_eq!(got.stdout_raw, b"hello world\n");
    }

    /// Test that exceeding the concurrency limit returns `ThroughputLimit`.
    #[tokio::test]
    async fn get_action_result_throughput_limit() {
        let dir = tempfile::tempdir().unwrap();
        // Limit of 1 so the second concurrent call fails immediately.
        let cache = RedbActionCache::open_with_limit(dir.path().join("ac.redb"), 1).unwrap();
        let d = Digest::of(b"any-action");
        let r = sample_result();
        cache.update_action_result(&d, r).await.unwrap();

        let first = cache.get_action_result(&d);
        let second = cache.get_action_result(&d);

        let err = match tokio::join!(first, second) {
            (Ok(Some(_)), Err(e)) if matches!(e, CasError::ThroughputLimit { .. }) => e,
            (Err(e), Ok(Some(_))) if matches!(e, CasError::ThroughputLimit { .. }) => e,
            (Ok(Some(_)), Ok(Some(_))) => {
                // Both ok — one finished before the other started.
                return;
            }
            other => {
                panic!("unexpected: {other:?}");
            }
        };
        assert!(matches!(err, CasError::ThroughputLimit { limit: 1 }));
    }

    /// Test that exceeding the concurrency limit returns `ThroughputLimit`.
    #[tokio::test]
    async fn update_action_result_throughput_limit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RedbActionCache::open_with_limit(dir.path().join("ac.redb"), 1).unwrap();
        let d = Digest::of(b"any-action");

        let first = cache.update_action_result(&d, sample_result());
        let second = cache.update_action_result(&d, sample_result());

        let err = match tokio::join!(first, second) {
            (Ok(()), Err(e)) if matches!(e, CasError::ThroughputLimit { .. }) => e,
            (Err(e), Ok(())) if matches!(e, CasError::ThroughputLimit { .. }) => e,
            (Ok(_a), Ok(_b)) => {
                // Both ok — one finished before the other started.
                return;
            }
            (Err(a), Err(b)) => {
                panic!("both failed: {a:?}, {b:?}");
            }
            other => {
                panic!("unexpected: {other:?}");
            }
        };
        assert!(matches!(err, CasError::ThroughputLimit { limit: 1 }));
    }

    /// Test that exceeding the concurrency limit returns `ThroughputLimit`.
    #[tokio::test]
    async fn list_entries_throughput_limit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RedbActionCache::open_with_limit(dir.path().join("ac.redb"), 1).unwrap();
        let d = Digest::of(b"any-action");
        cache
            .update_action_result(&d, sample_result())
            .await
            .unwrap();

        let first = cache.list_entries();
        let second = cache.list_entries();

        let err = match tokio::join!(first, second) {
            (Ok(_), Err(e)) if matches!(e, CasError::ThroughputLimit { .. }) => e,
            (Err(e), Ok(_)) if matches!(e, CasError::ThroughputLimit { .. }) => e,
            (Ok(_a), Ok(_b)) => {
                // Both ok — one finished before the other started.
                return;
            }
            (Err(a), Err(b)) => {
                panic!("both failed: {a:?}, {b:?}");
            }
            other => {
                panic!("unexpected: {other:?}");
            }
        };
        assert!(matches!(err, CasError::ThroughputLimit { limit: 1 }));
    }
}
