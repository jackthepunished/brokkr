//! Bloom-filter-decorated CAS.
//!
//! Wraps any [`Cas`] backend with a [`Bloom`] over its held
//! digests. `find_missing_blobs` consults the bloom first; for any
//! digest that the filter says is *definitely* missing, we skip
//! the underlying backend's lookup. A `contains` answer of "maybe
//! present" still flows through to the backend (per the bloom's
//! probabilistic contract). The decorator is transparent on
//! `batch_update_blobs` (each successfully-stored blob is also
//! inserted into the bloom) and on `batch_read_blobs` (bloom
//! doesn't help reads).
//!
//! ## Construction
//!
//! [`BloomCas::new`] builds an empty bloom; callers that already
//! have persisted state should follow up with [`BloomCas::rebuild_from`]
//! to populate the filter from a known-good source of digests
//! (typically the warm tier's redb table). M2 ships
//! [`BloomCas::new`] only; the periodic rebuild path and the
//! peer-exchange optimisation are explicit follow-ups (see
//! `docs/phase-3-plan.md` §5.2).
//!
//! ## Concurrency
//!
//! The bloom is held behind an `RwLock`; reads (`find_missing_blobs`)
//! take a shared read guard, writes (`insert` after a successful
//! store) take an exclusive write guard. The inner backend handles
//! its own concurrency.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use brokkr_common::Digest;
use bytes::Bytes;

use crate::bloom::Bloom;
use crate::error::CasError;
use crate::traits::{Cas, UpdateResult};

/// CAS backend decorated with a [`Bloom`] over its held digests.
///
/// Cheap to clone: the inner backend and the bloom are both behind
/// `Arc`/`RwLock`. Cloning shares the filter — every clone sees
/// the same set of inserts.
#[derive(Clone)]
pub struct BloomCas<C: Cas> {
    inner: Arc<C>,
    bloom: Arc<RwLock<Bloom>>,
}

impl<C: Cas + std::fmt::Debug> std::fmt::Debug for BloomCas<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BloomCas")
            .field("inner", &self.inner)
            .field("bloom", &"<RwLock<Bloom>>")
            .finish()
    }
}

impl<C: Cas> BloomCas<C> {
    /// Wrap `inner` with a fresh bloom sized for
    /// `(expected_items, fp_rate)`. The filter starts empty;
    /// callers that already have persisted data should call
    /// [`Self::rebuild_from`] before serving traffic.
    pub fn new(inner: Arc<C>, expected_items: u64, fp_rate: f64) -> Self {
        Self {
            inner,
            bloom: Arc::new(RwLock::new(Bloom::new(expected_items, fp_rate))),
        }
    }

    /// Replace the bloom's contents with the union of `digests`.
    /// Used to seed the filter from an authoritative source of
    /// digests (e.g. scanning the warm tier's redb table on
    /// startup, or the periodic rebuild).
    pub fn rebuild_from<I: IntoIterator<Item = Digest>>(&self, digests: I) {
        let mut g = match self.bloom.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.clear();
        for d in digests {
            g.insert(&d);
        }
    }

    /// Number of inserts the bloom has seen since construction or
    /// last rebuild. Useful for `/metrics`.
    pub fn approximate_items(&self) -> u64 {
        match self.bloom.read() {
            Ok(g) => g.items(),
            Err(p) => p.into_inner().items(),
        }
    }

    fn insert(&self, digest: &Digest) {
        let mut g = match self.bloom.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.insert(digest);
    }

    fn contains(&self, digest: &Digest) -> bool {
        match self.bloom.read() {
            Ok(g) => g.contains(digest),
            Err(p) => p.into_inner().contains(digest),
        }
    }
}

#[async_trait]
impl<C: Cas + 'static> Cas for BloomCas<C> {
    async fn find_missing_blobs(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
        // Partition into (bloom-definitely-missing) | (bloom-maybe-present).
        // The bloom answer is authoritative for misses; we surface
        // those directly. For the "maybe" set we ask the backend.
        let mut definitely_missing = Vec::new();
        let mut maybe_present = Vec::new();
        for d in digests {
            if self.contains(d) {
                maybe_present.push(d.clone());
            } else {
                definitely_missing.push(d.clone());
            }
        }
        if maybe_present.is_empty() {
            return Ok(definitely_missing);
        }
        let backend_missing = self.inner.find_missing_blobs(&maybe_present).await?;
        // Merge: definitely_missing already includes everything
        // the bloom ruled out; tack on whatever the backend
        // confirmed.
        let mut out = definitely_missing;
        out.extend(backend_missing);
        Ok(out)
    }

    async fn batch_update_blobs(
        &self,
        blobs: Vec<(Digest, Bytes)>,
    ) -> Result<Vec<UpdateResult>, CasError> {
        let results = self.inner.batch_update_blobs(blobs).await?;
        for r in &results {
            if r.status.is_ok() {
                self.insert(&r.digest);
            }
        }
        Ok(results)
    }

    async fn batch_read_blobs(
        &self,
        digests: &[Digest],
    ) -> Result<Vec<Result<Bytes, CasError>>, CasError> {
        // The bloom doesn't help reads: even a "definitely missing"
        // answer would just save us a NotFound from the backend,
        // and the backend already returns NotFound cheaply. Just
        // delegate.
        self.inner.batch_read_blobs(digests).await
    }

    async fn list_digests(&self) -> Result<Vec<Digest>, CasError> {
        // The bloom is a fast-path, not an authoritative store.
        // GC needs every digest the backend actually holds;
        // delegate.
        self.inner.list_digests().await
    }

    async fn delete_blob(&self, digest: &Digest) -> Result<(), CasError> {
        // Delegate the primary effect to the inner store. We
        // intentionally DO NOT touch the bloom: a Bloom filter
        // cannot support per-entry remove (deletes do not
        // subtract, per `docs/phase-3-plan.md` §5.2; the bloom
        // is always a superset of the actual contents and the
        // periodic `rebuild_from` path re-tightens it). Leaving
        // a stale bit cannot cause a correctness violation —
        // `find_missing_blobs` (above) asks the backend on every
        // bloom-yes, so a "present" hit on a deleted digest just
        // falls through to a backend `NotFound`. The bloom's
        // `approximate_items()` metric will be slightly inflated
        // until the next rebuild; that's a `/metrics` cosmetic.
        self.inner.delete_blob(digest).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::in_memory::InMemoryCas;

    fn blob(payload: &[u8]) -> (Digest, Bytes) {
        (Digest::of(payload), Bytes::copy_from_slice(payload))
    }

    #[tokio::test]
    async fn empty_bloom_routes_every_digest_to_definitely_missing() {
        let inner = Arc::new(InMemoryCas::new());
        let cas = BloomCas::new(inner, 1024, 0.01);
        let (d, _) = blob(b"x");
        let missing = cas.find_missing_blobs(&[d.clone()]).await.unwrap();
        assert_eq!(missing, vec![d]);
    }

    #[tokio::test]
    async fn insert_updates_bloom_so_present_blob_consults_backend() {
        let inner = Arc::new(InMemoryCas::new());
        let cas = BloomCas::new(inner, 1024, 0.01);
        let (d, b) = blob(b"hi");
        cas.batch_update_blobs(vec![(d.clone(), b)]).await.unwrap();
        // After insert, the bloom answers "maybe", and the backend
        // confirms presence. find_missing_blobs returns no missing.
        let missing = cas.find_missing_blobs(&[d]).await.unwrap();
        assert!(missing.is_empty());
    }

    #[tokio::test]
    async fn find_missing_returns_same_result_as_undecorated_for_present_blobs() {
        let inner: Arc<InMemoryCas> = Arc::new(InMemoryCas::new());
        let cas = BloomCas::new(inner.clone(), 1024, 0.01);
        let (d1, b1) = blob(b"a");
        let (d2, _) = blob(b"b");
        cas.batch_update_blobs(vec![(d1.clone(), b1)])
            .await
            .unwrap();
        // d1 is stored; d2 is not. The decorator must agree with
        // the plain inner backend.
        let from_decorated = cas
            .find_missing_blobs(&[d1.clone(), d2.clone()])
            .await
            .unwrap();
        let from_inner = inner
            .find_missing_blobs(&[d1.clone(), d2.clone()])
            .await
            .unwrap();
        assert_eq!(from_decorated, from_inner);
        assert_eq!(from_decorated, vec![d2]);
    }

    #[tokio::test]
    async fn rebuild_from_seeds_bloom_for_existing_data() {
        let inner: Arc<InMemoryCas> = Arc::new(InMemoryCas::new());
        // Pre-populate the backend without going through the
        // decorator.
        let (d, b) = blob(b"prepopulated");
        inner
            .batch_update_blobs(vec![(d.clone(), b)])
            .await
            .unwrap();
        let cas = BloomCas::new(inner, 1024, 0.01);

        // Before rebuild, the bloom is empty — `find_missing_blobs`
        // ignores the backend and reports the digest as missing.
        assert_eq!(
            cas.find_missing_blobs(&[d.clone()]).await.unwrap(),
            vec![d.clone()],
        );

        cas.rebuild_from([d.clone()]);
        assert!(cas.find_missing_blobs(&[d]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn approximate_items_tracks_inserts_through_decorator() {
        let inner = Arc::new(InMemoryCas::new());
        let cas = BloomCas::new(inner, 1024, 0.01);
        for i in 0..10 {
            let (d, b) = blob(format!("blob-{i}").as_bytes());
            cas.batch_update_blobs(vec![(d, b)]).await.unwrap();
        }
        assert_eq!(cas.approximate_items(), 10);
    }

    #[tokio::test]
    async fn read_after_bloom_filter_skip_is_still_authoritative() {
        // Even if a digest is "definitely missing" per the bloom,
        // the read path doesn't consult the bloom and returns the
        // backend's NotFound. This is by design — `batch_read_blobs`
        // doesn't get any speedup from the bloom and skipping the
        // backend would risk returning false negatives if the
        // bloom is stale.
        let inner: Arc<InMemoryCas> = Arc::new(InMemoryCas::new());
        let cas = BloomCas::new(inner, 1024, 0.01);
        let (d, _) = blob(b"unseen");
        let results = cas.batch_read_blobs(&[d]).await.unwrap();
        assert!(matches!(results[0], Err(CasError::NotFound(_))));
    }

    // --- #143 regression tests ---

    #[tokio::test]
    async fn bloom_cas_delete_blob_delegates_to_inner() {
        let inner: Arc<InMemoryCas> = Arc::new(InMemoryCas::new());
        let cas = BloomCas::new(inner.clone(), 1024, 0.01);
        let (d, b) = blob(b"will be deleted");
        cas.batch_update_blobs(vec![(d.clone(), b)]).await.unwrap();
        // Sanity: backend has it before delete.
        assert!(inner.batch_read_blobs(&[d.clone()]).await.unwrap()[0].is_ok());

        cas.delete_blob(&d).await.unwrap();

        // Backend is gone.
        let read = inner.batch_read_blobs(&[d.clone()]).await.unwrap();
        assert!(matches!(read[0], Err(CasError::NotFound(_))));
        // And a re-read through the decorator also returns
        // NotFound — even though the bloom still has a stale
        // bit for `d`, `batch_read_blobs` is bloom-independent
        // and the backend says missing.
        let dec_read = cas.batch_read_blobs(&[d.clone()]).await.unwrap();
        assert!(matches!(dec_read[0], Err(CasError::NotFound(_))));
        // `find_missing_blobs` must also surface this digest as
        // missing — the stale bloom bit would otherwise cause a
        // "maybe present" answer; the backend then confirms the
        // miss. This is the documented staleness tolerance.
        let missing = cas.find_missing_blobs(&[d.clone()]).await.unwrap();
        assert_eq!(missing, vec![d]);
    }

    #[tokio::test]
    async fn bloom_cas_delete_blob_on_missing_is_ok() {
        // Idempotent contract: deleting an absent digest must
        // return Ok(()).
        let cas = BloomCas::new(Arc::new(InMemoryCas::new()), 1024, 0.01);
        let phantom = Digest::of(b"never stored");
        cas.delete_blob(&phantom).await.unwrap();
    }

    #[tokio::test]
    async fn bloom_cas_list_digests_delegates_to_inner() {
        let inner: Arc<InMemoryCas> = Arc::new(InMemoryCas::new());
        let cas = BloomCas::new(inner.clone(), 1024, 0.01);
        let (d1, b1) = blob(b"a");
        let (d2, b2) = blob(b"b");
        cas.batch_update_blobs(vec![(d1.clone(), b1), (d2.clone(), b2)])
            .await
            .unwrap();
        let listed: std::collections::HashSet<Digest> =
            cas.list_digests().await.unwrap().into_iter().collect();
        assert_eq!(listed.len(), 2);
        assert!(listed.contains(&d1));
        assert!(listed.contains(&d2));
    }
}
