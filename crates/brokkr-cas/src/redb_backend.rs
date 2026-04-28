//! `redb`-backed persistent CAS.
//!
//! Single-node, embedded, ACID. Phase 1 storage default for the dev control
//! plane. Phase 3 replaces this with a sharded, replicated CAS.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use brokkr_common::Digest;
use bytes::Bytes;
use redb::{Database, TableDefinition};

use crate::error::CasError;
use crate::traits::{Cas, UpdateResult};

/// Table mapping `digest_hash_hex` → blob bytes.
const BLOBS: TableDefinition<&str, &[u8]> = TableDefinition::new("blobs");

/// On-disk CAS backed by a `redb` database.
#[derive(Debug, Clone)]
pub struct RedbCas {
    db: Arc<Database>,
}

impl RedbCas {
    /// Open or create a CAS database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CasError> {
        let db = Database::create(path.as_ref())?;
        // Ensure the table exists by opening a write txn that defines it.
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(BLOBS)?;
        }
        txn.commit()?;
        Ok(Self { db: Arc::new(db) })
    }
}

#[async_trait]
impl Cas for RedbCas {
    async fn find_missing_blobs(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
        let db = self.db.clone();
        let digests = digests.to_vec();
        tokio::task::spawn_blocking(move || -> Result<Vec<Digest>, CasError> {
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
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<UpdateResult>, CasError> {
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
        let db = self.db.clone();
        let digests = digests.to_vec();
        tokio::task::spawn_blocking(move || -> Result<Vec<Result<Bytes, CasError>>, CasError> {
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
}
