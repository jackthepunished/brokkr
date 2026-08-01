//! Trait abstraction over CAS backends.

use async_trait::async_trait;
use brokkr_common::Digest;
use bytes::Bytes;

use crate::error::CasError;

/// Result of writing a single blob in a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateResult {
    /// Digest the client claimed for the blob.
    pub digest: Digest,
    /// Outcome: `Ok(())` if stored, `Err(...)` if rejected (e.g. digest mismatch).
    pub status: Result<(), String>,
}

/// Size of a single CAS store.
///
/// **Per store, never summed across nodes.** Each control-plane node opens its
/// own CAS, so three nodes holding one blob is three copies of one blob, not
/// three blobs. Adding these together reports storage that does not exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CasStats {
    /// Number of distinct blobs stored.
    pub objects: u64,
    /// Total bytes stored, summing each blob once.
    pub bytes: u64,
}

/// Content-Addressable Storage backend.
///
/// Mirrors the three core REAPI `ContentAddressableStorage` RPCs. Backends must
/// reject blobs whose bytes do not match their declared digest.
#[async_trait]
pub trait Cas: Send + Sync + 'static {
    /// Return the subset of `digests` that are NOT present in the CAS.
    async fn find_missing_blobs(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError>;

    /// Insert a batch of `(digest, bytes)` pairs.
    ///
    /// Each entry is validated independently; a mismatch on one blob does not
    /// abort the batch. The returned vector reports per-entry status in input
    /// order.
    async fn batch_update_blobs(
        &self,
        blobs: Vec<(Digest, Bytes)>,
    ) -> Result<Vec<UpdateResult>, CasError>;

    /// Read a batch of blobs by digest. Missing blobs surface as
    /// `Err(CasError::NotFound)` for that entry; the overall call still
    /// succeeds.
    async fn batch_read_blobs(
        &self,
        digests: &[Digest],
    ) -> Result<Vec<Result<Bytes, CasError>>, CasError>;

    /// Enumerate every digest currently held in the CAS. Used by GC
    /// (M5) to compute `unreachable = local - reachable`. Default
    /// implementation returns the empty set so backends that don't
    /// support enumeration (e.g. write-only stubs) still compile;
    /// real backends override.
    ///
    /// Implementations should stream where possible — Phase 3 ships
    /// the eager `Vec` shape because backends only hold thousands
    /// of blobs locally; a future iteration may return an async
    /// stream when the bloom rebuild or GC walk grows.
    async fn list_digests(&self) -> Result<Vec<Digest>, CasError> {
        Ok(Vec::new())
    }

    /// Remove a single blob. `Ok(())` whether the blob was present
    /// or not — `delete` is idempotent. Backends that genuinely
    /// can't delete (cold-tier S3 in archive mode, say) should
    /// return a `CasError::Other` describing the constraint until
    /// a dedicated `Unsupported` variant lands. Default
    /// implementation returns `Ok(())` to keep non-GC test backends
    /// compiling.
    async fn delete_blob(&self, _digest: &Digest) -> Result<(), CasError> {
        Ok(())
    }

    /// Size of this store.
    ///
    /// The default implementation derives stats from
    /// [`list_digests`](Cas::list_digests), which is a full scan on most
    /// backends. **Backends that can answer cheaply should override this** —
    /// callers may poll it, and `RedbCas` in particular takes a throughput
    /// permit for a scan that a poller could otherwise steal from real
    /// traffic.
    async fn stats(&self) -> Result<CasStats, CasError> {
        let digests = self.list_digests().await?;
        let bytes = digests
            .iter()
            .map(|d| u64::try_from(d.size_bytes()).unwrap_or(0))
            .sum();
        Ok(CasStats {
            objects: digests.len() as u64,
            bytes,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::in_memory::InMemoryCas;

    #[tokio::test]
    async fn stats_counts_objects_and_bytes() {
        let cas = InMemoryCas::new();
        let a = Bytes::from_static(b"hello");
        let b = Bytes::from_static(b"world!!");
        cas.batch_update_blobs(vec![
            (Digest::of(&a), a.clone()),
            (Digest::of(&b), b.clone()),
        ])
        .await
        .unwrap();

        let stats = cas.stats().await.unwrap();
        assert_eq!(stats.objects, 2);
        assert_eq!(stats.bytes, (a.len() + b.len()) as u64);
    }

    #[tokio::test]
    async fn stats_on_an_empty_cas_is_zero_not_an_error() {
        let stats = InMemoryCas::new().stats().await.unwrap();
        assert_eq!(stats.objects, 0);
        assert_eq!(stats.bytes, 0);
    }
}
