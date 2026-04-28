//! In-memory CAS backend. Phase 1 default for tests and the dev control plane.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use brokkr_common::Digest;
use bytes::Bytes;

use crate::error::CasError;
use crate::traits::{Cas, UpdateResult};

/// In-memory `HashMap<Digest, Bytes>` CAS.
///
/// Thread-safe via a `RwLock`. Suitable for tests and a single-node dev server;
/// not for production (no persistence, no eviction).
#[derive(Debug, Default)]
pub struct InMemoryCas {
    blobs: RwLock<HashMap<Digest, Bytes>>,
}

impl InMemoryCas {
    /// Create an empty CAS.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of blobs currently stored.
    pub fn len(&self) -> usize {
        self.blobs.read().map(|g| g.len()).unwrap_or(0)
    }

    /// True if the CAS holds no blobs.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl Cas for InMemoryCas {
    async fn find_missing_blobs(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
        let guard = self
            .blobs
            .read()
            .map_err(|_| std::io::Error::other("cas lock poisoned"))?;
        Ok(digests
            .iter()
            .filter(|d| !guard.contains_key(d))
            .cloned()
            .collect())
    }

    async fn batch_update_blobs(
        &self,
        blobs: Vec<(Digest, Bytes)>,
    ) -> Result<Vec<UpdateResult>, CasError> {
        let mut out = Vec::with_capacity(blobs.len());
        let mut guard = self
            .blobs
            .write()
            .map_err(|_| std::io::Error::other("cas lock poisoned"))?;
        for (digest, bytes) in blobs {
            let status = match digest.verify(bytes.as_ref()) {
                Ok(()) => {
                    guard.insert(digest.clone(), bytes);
                    Ok(())
                }
                Err(e) => Err(e.to_string()),
            };
            out.push(UpdateResult { digest, status });
        }
        Ok(out)
    }

    async fn batch_read_blobs(
        &self,
        digests: &[Digest],
    ) -> Result<Vec<Result<Bytes, CasError>>, CasError> {
        let guard = self
            .blobs
            .read()
            .map_err(|_| std::io::Error::other("cas lock poisoned"))?;
        Ok(digests
            .iter()
            .map(|d| {
                guard
                    .get(d)
                    .cloned()
                    .ok_or_else(|| CasError::NotFound(d.clone()))
            })
            .collect())
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
    async fn roundtrip_single_blob() {
        let cas = InMemoryCas::new();
        let (d, b) = blob(b"hello");
        let res = cas
            .batch_update_blobs(vec![(d.clone(), b.clone())])
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert!(res[0].status.is_ok());

        let read = cas.batch_read_blobs(&[d.clone()]).await.unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].as_ref().unwrap(), &b);
    }

    #[tokio::test]
    async fn find_missing_returns_only_missing() {
        let cas = InMemoryCas::new();
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

    #[tokio::test]
    async fn rejects_digest_mismatch() {
        let cas = InMemoryCas::new();
        let lying = Digest::of(b"hello"); // declared digest of "hello"
        let bytes = Bytes::from_static(b"world"); // but we send "world"
        let res = cas
            .batch_update_blobs(vec![(lying.clone(), bytes)])
            .await
            .unwrap();
        assert!(res[0].status.is_err());

        let read = cas.batch_read_blobs(&[lying]).await.unwrap();
        assert!(matches!(read[0], Err(CasError::NotFound(_))));
    }

    #[tokio::test]
    async fn read_missing_blob_is_per_entry_error() {
        let cas = InMemoryCas::new();
        let d = Digest::of(b"absent");
        let read = cas.batch_read_blobs(&[d]).await.unwrap();
        assert!(matches!(read[0], Err(CasError::NotFound(_))));
    }

    #[tokio::test]
    async fn empty_cas_starts_empty() {
        let cas = InMemoryCas::new();
        assert!(cas.is_empty());
        assert_eq!(cas.len(), 0);
    }
}
