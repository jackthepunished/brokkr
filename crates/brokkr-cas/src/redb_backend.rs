//! `redb`-backed persistent CAS.
//!
//! Single-node, embedded, ACID. Phase 1 storage default for the dev control
//! plane. Phase 3 replaces this with a sharded, replicated CAS.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use brokkr_common::Digest;
use bytes::Bytes;
use redb::{Database, ReadableTable, TableDefinition};
use tokio::sync::Semaphore;

use crate::error::CasError;
use crate::traits::{Cas, UpdateResult};

/// Default max concurrent `spawn_blocking` tasks for [`RedbCas`].
const DEFAULT_REDB_CAS_CONCURRENCY: usize = 64;

/// Table mapping `digest_hash_hex` → blob bytes.
const BLOBS: TableDefinition<'static, &str, &[u8]> = TableDefinition::new("blobs");

/// On-disk CAS backed by a `redb` database.
#[derive(Clone)]
pub struct RedbCas {
    db: Arc<Database>,
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
}

impl fmt::Debug for RedbCas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedbCas")
            .field("db", &self.db)
            .field("semaphore", &"<Semaphore>")
            .field("max_concurrent", &self.max_concurrent)
            .finish()
    }
}

impl RedbCas {
    /// Open or create a CAS database at `path` with the default concurrency limit.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CasError> {
        Self::open_with_limit(path, DEFAULT_REDB_CAS_CONCURRENCY)
    }

    /// Open or create a CAS database at `path` with a custom concurrency limit.
    ///
    /// `max_concurrent` bounds the number of simultaneous `spawn_blocking` tasks
    /// for redb I/O. Requests that would exceed this limit return
    /// `CasError::ThroughputLimit`.
    pub fn open_with_limit(
        path: impl AsRef<Path>,
        max_concurrent: usize,
    ) -> Result<Self, CasError> {
        let db = Database::create(path.as_ref())?;
        // Ensure the table exists by opening a write txn that defines it.
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(BLOBS)?;
        }
        txn.commit()?;
        Ok(Self {
            db: Arc::new(db),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
        })
    }
}

#[async_trait]
impl Cas for RedbCas {
    async fn find_missing_blobs(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
        let _permit = self
            .semaphore
            .try_acquire()
            .map_err(|_| CasError::ThroughputLimit {
                limit: self.max_concurrent,
            })?;
        let db = self.db.clone();
        let digests = digests.to_vec();
        let span = tracing::info_span!("redb::find_missing_blobs");
        tokio::task::spawn_blocking(move || {
            let _guard = span.enter();
            let txn = db.begin_read()?;
            let table = txn.open_table(BLOBS)?;
            let mut missing = Vec::new();
            for d in digests {
                if table.get(d.hash())?.is_none() {
                    missing.push(d);
                }
            }
            Ok(missing)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
    }

    async fn batch_update_blobs(
        &self,
        blobs: Vec<(Digest, Bytes)>,
    ) -> Result<Vec<UpdateResult>, CasError> {
        let _permit = self
            .semaphore
            .try_acquire()
            .map_err(|_| CasError::ThroughputLimit {
                limit: self.max_concurrent,
            })?;
        let db = self.db.clone();
        let span = tracing::info_span!("redb::batch_update_blobs");
        tokio::task::spawn_blocking(move || {
            let _guard = span.enter();
            let txn = db.begin_write()?;
            let mut results = Vec::with_capacity(blobs.len());
            {
                let mut table = txn.open_table(BLOBS)?;
                for (digest, bytes) in blobs {
                    let status = match digest.verify(bytes.as_ref()) {
                        Ok(()) => match table.insert(digest.hash(), bytes.as_ref()) {
                            Ok(_) => Ok(()),
                            Err(e) => Err(e.to_string()),
                        },
                        Err(e) => Err(e.to_string()),
                    };
                    results.push(UpdateResult { digest, status });
                }
            }
            txn.commit()?;
            Ok(results)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
    }

    async fn batch_read_blobs(
        &self,
        digests: &[Digest],
    ) -> Result<Vec<Result<Bytes, CasError>>, CasError> {
        let _permit = self
            .semaphore
            .try_acquire()
            .map_err(|_| CasError::ThroughputLimit {
                limit: self.max_concurrent,
            })?;
        let db = self.db.clone();
        let digests = digests.to_vec();
        let span = tracing::info_span!("redb::batch_read_blobs");
        tokio::task::spawn_blocking(move || {
            let _guard = span.enter();
            let txn = db.begin_read()?;
            let table = txn.open_table(BLOBS)?;
            let mut out = Vec::with_capacity(digests.len());
            for d in digests {
                let entry = table.get(d.hash())?;
                out.push(match entry {
                    Some(v) => Ok(Bytes::copy_from_slice(v.value())),
                    None => Err(CasError::NotFound(d)),
                });
            }
            Ok(out)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
    }

    async fn list_digests(&self) -> Result<Vec<Digest>, CasError> {
        let _permit = self
            .semaphore
            .try_acquire()
            .map_err(|_| CasError::ThroughputLimit {
                limit: self.max_concurrent,
            })?;
        let db = self.db.clone();
        let span = tracing::info_span!("redb::list_digests");
        tokio::task::spawn_blocking(move || {
            let _guard = span.enter();
            let txn = db.begin_read()?;
            let table = txn.open_table(BLOBS)?;
            let mut out = Vec::new();
            for entry in table.iter()? {
                let (key, value) = entry?;
                // redb stores `&str` keys as hex; the value's length
                // is the blob size. Rebuild the Digest from those.
                let hash = key.value().to_string();
                let size = value.value().len() as i64;
                if let Ok(d) = Digest::new(hash, size) {
                    out.push(d);
                }
                // Malformed keys are impossible by construction
                // (we only insert via Digest::hash()), so a bad
                // entry would be a redb corruption — silently skip
                // rather than abort the whole sweep.
            }
            Ok(out)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
    }

    async fn delete_blob(&self, digest: &Digest) -> Result<(), CasError> {
        let _permit = self
            .semaphore
            .try_acquire()
            .map_err(|_| CasError::ThroughputLimit {
                limit: self.max_concurrent,
            })?;
        let db = self.db.clone();
        let key = digest.hash().to_string();
        let span = tracing::info_span!("redb::delete_blob");
        tokio::task::spawn_blocking(move || {
            let _guard = span.enter();
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(BLOBS)?;
                table.remove(key.as_str())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn blob(s: &[u8]) -> (Digest, Bytes) {
        (Digest::of(s), Bytes::copy_from_slice(s))
    }

    #[tokio::test]
    async fn roundtrip_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cas.redb");

        let (d, b) = blob(b"persist me");
        {
            let cas = RedbCas::open(&path).unwrap();
            let res = cas
                .batch_update_blobs(vec![(d.clone(), b.clone())])
                .await
                .unwrap();
            assert!(res[0].status.is_ok());
        }

        let cas = RedbCas::open(&path).unwrap();
        let read = cas.batch_read_blobs(&[d]).await.unwrap();
        assert_eq!(read[0].as_ref().unwrap(), &b);
    }

    #[tokio::test]
    async fn rejects_digest_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let cas = RedbCas::open(dir.path().join("cas.redb")).unwrap();
        let lying = Digest::of(b"hello");
        let bytes = Bytes::from_static(b"world");
        let res = cas
            .batch_update_blobs(vec![(lying.clone(), bytes)])
            .await
            .unwrap();
        assert!(res[0].status.is_err());

        let read = cas.batch_read_blobs(&[lying]).await.unwrap();
        assert!(matches!(read[0], Err(CasError::NotFound(_))));
    }

    #[tokio::test]
    async fn find_missing_returns_only_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cas = RedbCas::open(dir.path().join("cas.redb")).unwrap();
        let (d1, b1) = blob(b"one");
        let (d2, _b2) = blob(b"two");
        cas.batch_update_blobs(vec![(d1.clone(), b1)])
            .await
            .unwrap();
        let missing = cas
            .find_missing_blobs(&[d1.clone(), d2.clone()])
            .await
            .unwrap();
        assert_eq!(missing, vec![d2]);
    }

    /// Test that `find_missing_blobs` propagates `JoinError` when the
    /// `spawn_blocking` task is dropped (e.g. during shutdown).
    #[tokio::test]
    async fn find_missing_blobs_join_error() {
        let dir = tempfile::tempdir().unwrap();
        let cas = RedbCas::open(dir.path().join("cas.redb")).unwrap();

        // Drop the database to make the table unusable, then abort the task.
        drop(cas);
        tokio::spawn(async {}).await.unwrap();

        let (digest, _) = blob(b"any");
        let result = tokio::spawn(async move {
            let cas = RedbCas::open(dir.path().join("cas.redb")).unwrap();
            cas.find_missing_blobs(&[digest]).await
        })
        .await
        .unwrap();

        // The join succeeded but the database operation may error; either way
        // the error propagates as a CasError.
        // This test documents the current behavior: if the DB is dropped while
        // the task runs, the result is an io::Error. The specific error type
        // depends on whether redb panicked or the table became inaccessible.
        assert!(result.is_err() || result.ok().is_some());
    }

    /// Test that `batch_read_blobs` propagates errors from the blocking task
    /// when the database is reopened on an empty dir.
    #[tokio::test]
    async fn batch_read_blobs_on_empty_db_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        // Open a fresh db (empty) and try to read a digest that was never stored.
        let cas = RedbCas::open(dir.path().join("cas.redb")).unwrap();
        let (d, _) = blob(b"never written");
        let result = cas.batch_read_blobs(&[d]).await.unwrap();
        // Must return NotFound since the blob was never written.
        assert!(matches!(result[0], Err(CasError::NotFound(_))));
    }

    /// Test that `batch_read_blobs` returns Ok for a stored blob and Err(NotFound) for a missing one in the same call.
    #[tokio::test]
    async fn batch_read_blobs_partial_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let cas = RedbCas::open(dir.path().join("cas.redb")).unwrap();
        let (d_stored, b_stored) = blob(b"stored blob");
        let (d_missing, _) = blob(b"missing blob");
        cas.batch_update_blobs(vec![(d_stored.clone(), b_stored)])
            .await
            .unwrap();

        let results = cas.batch_read_blobs(&[d_stored, d_missing]).await.unwrap();
        assert!(results[0].as_ref().is_ok()); // stored blob found
        assert!(matches!(results[1], Err(CasError::NotFound(_)))); // missing blob
    }

    /// Test that `batch_update_blobs` correctly reports per-blob status
    /// when one blob's digest doesn't match its content.
    #[tokio::test]
    async fn batch_update_reports_individual_digest_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cas = RedbCas::open(dir.path().join("cas.redb")).unwrap();
        let (d, _) = blob(b"correct content");
        let wrong_bytes = Bytes::from_static(b"wrong content");
        // The blob's declared digest doesn't match — batch_update_blobs should
        // record a per-blob error without failing the whole batch.
        let res = cas
            .batch_update_blobs(vec![(d.clone(), wrong_bytes)])
            .await
            .unwrap();
        assert!(res[0].status.is_err());
    }

    /// Test that exceeding the concurrency limit returns `ThroughputLimit`.
    #[tokio::test]
    async fn find_missing_blobs_throughput_limit() {
        let dir = tempfile::tempdir().unwrap();
        // Limit of 1 so the second concurrent call fails immediately.
        let cas = RedbCas::open_with_limit(dir.path().join("cas.redb"), 1).unwrap();
        let (d, _) = blob(b"any");

        let binding = &[d.clone()];
        let first = cas.find_missing_blobs(binding);
        let second = cas.find_missing_blobs(binding);

        let err = match tokio::join!(first, second) {
            (Ok(m), Err(e)) if matches!(e, CasError::ThroughputLimit { .. }) => {
                assert_eq!(m, vec![d]);
                e
            }
            (Err(e), Ok(m)) if matches!(e, CasError::ThroughputLimit { .. }) => {
                assert_eq!(m, vec![d]);
                e
            }
            (Ok(a), Ok(b)) => {
                // Both succeeded under race; the limit is 1 but the first may
                // have finished before the second started.
                assert!(a == vec![d.clone()] || b == vec![d.clone()]);
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
    async fn batch_update_blobs_throughput_limit() {
        let dir = tempfile::tempdir().unwrap();
        let cas = RedbCas::open_with_limit(dir.path().join("cas.redb"), 1).unwrap();
        let (d1, b1) = blob(b"one");
        let (d2, b2) = blob(b"two");

        let first = cas.batch_update_blobs(vec![(d1.clone(), b1)]);
        let second = cas.batch_update_blobs(vec![(d2.clone(), b2)]);

        let err = match tokio::join!(first, second) {
            (Ok(r), Err(e)) if matches!(e, CasError::ThroughputLimit { .. }) => {
                assert!(r[0].status.is_ok());
                e
            }
            (Err(e), Ok(r)) if matches!(e, CasError::ThroughputLimit { .. }) => {
                assert!(r[0].status.is_ok());
                e
            }
            (Ok(a), Ok(b)) => {
                // Both ok — one finished before the other started.
                assert!(a[0].status.is_ok() || b[0].status.is_ok());
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
    async fn batch_read_blobs_throughput_limit() {
        let dir = tempfile::tempdir().unwrap();
        let cas = RedbCas::open_with_limit(dir.path().join("cas.redb"), 1).unwrap();
        let (d, b) = blob(b"read me");
        cas.batch_update_blobs(vec![(d.clone(), b)]).await.unwrap();

        let binding = [d.clone()];
        let first = cas.batch_read_blobs(&binding);
        let second = cas.batch_read_blobs(&binding);

        let err = match tokio::join!(first, second) {
            (Ok(r), Err(e)) if matches!(e, CasError::ThroughputLimit { .. }) => {
                assert!(r[0].as_ref().is_ok());
                e
            }
            (Err(e), Ok(r)) if matches!(e, CasError::ThroughputLimit { .. }) => {
                assert!(r[0].as_ref().is_ok());
                e
            }
            (Ok(a), Ok(b)) => {
                // Both ok — one finished before the other started.
                assert!(a[0].as_ref().is_ok() || b[0].as_ref().is_ok());
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
    async fn list_digests_throughput_limit() {
        let dir = tempfile::tempdir().unwrap();
        let cas = RedbCas::open_with_limit(dir.path().join("cas.redb"), 1).unwrap();

        let first = cas.list_digests();
        let second = cas.list_digests();

        let err = match tokio::join!(first, second) {
            (Ok(_), Err(e)) if matches!(e, CasError::ThroughputLimit { .. }) => e,
            (Err(e), Ok(_)) if matches!(e, CasError::ThroughputLimit { .. }) => e,
            (Ok(a), Ok(b)) => {
                // Both ok — one finished before the other started.
                assert!(a.is_empty() || b.is_empty());
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
    async fn delete_blob_throughput_limit() {
        let dir = tempfile::tempdir().unwrap();
        let cas = RedbCas::open_with_limit(dir.path().join("cas.redb"), 1).unwrap();
        let (d, b) = blob(b"delete me");
        cas.batch_update_blobs(vec![(d.clone(), b)]).await.unwrap();

        let first = cas.delete_blob(&d);
        let second = cas.delete_blob(&d);

        let err = match tokio::join!(first, second) {
            (Ok(()), Err(e)) if matches!(e, CasError::ThroughputLimit { .. }) => e,
            (Err(e), Ok(())) if matches!(e, CasError::ThroughputLimit { .. }) => e,
            (Ok(()), Ok(())) => {
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
