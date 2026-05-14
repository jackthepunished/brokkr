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
    /// return [`CasError::Unsupported`] (M5b will add this
    /// variant). Default implementation returns `Ok(())` to keep
    /// non-GC test backends compiling.
    async fn delete_blob(&self, _digest: &Digest) -> Result<(), CasError> {
        Ok(())
    }
}
