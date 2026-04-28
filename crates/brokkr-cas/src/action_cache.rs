//! REAPI Action Cache.
//!
//! Maps action digest → serialized [`brokkr_proto::reapi_v2::ActionResult`]
//! protobuf bytes. Phase 1 storage is single-node `redb`; semantics match the
//! REAPI `ActionCache` service (`GetActionResult`, `UpdateActionResult`).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use brokkr_common::Digest;
use brokkr_proto::reapi_v2::ActionResult;
use prost::Message;
use redb::{Database, TableDefinition};

use crate::error::CasError;

const ACTION_RESULTS: TableDefinition<&str, &[u8]> = TableDefinition::new("action_results");

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
}

/// `redb`-backed [`ActionCache`].
#[derive(Debug, Clone)]
pub struct RedbActionCache {
    db: Arc<Database>,
}

impl RedbActionCache {
    /// Open or create an action-cache database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CasError> {
        let db = Database::create(path.as_ref())?;
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(ACTION_RESULTS)?;
        }
        txn.commit()?;
        Ok(Self { db: Arc::new(db) })
    }
}

#[async_trait]
impl ActionCache for RedbActionCache {
    async fn get_action_result(
        &self,
        action_digest: &Digest,
    ) -> Result<Option<ActionResult>, CasError> {
        let db = self.db.clone();
        let key = action_digest.hash().to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<ActionResult>, CasError> {
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
        let db = self.db.clone();
        let key = action_digest.hash().to_string();
        let mut buf = Vec::with_capacity(result.encoded_len());
        result
            .encode(&mut buf)
            .map_err(|e| CasError::Redb(format!("ActionResult encode: {e}")))?;
        tokio::task::spawn_blocking(move || -> Result<(), CasError> {
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
}
