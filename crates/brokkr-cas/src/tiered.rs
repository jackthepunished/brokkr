//! Tiered storage for the Phase 3 distributed CAS.
//!
//! Phase 3 / M3a. Composes two tiers in front of the warm
//! (on-disk) backend:
//!
//! - **Hot** — in-memory size-bounded LRU. Read on hit, promote
//!   from warm on miss.
//! - **Warm** — any [`Cas`] implementation (typically [`crate::RedbCas`]).
//!
//! A future M3b milestone (`feat/phase3-tiered-storage-cold`) will
//! introduce a third tier — OpenDAL-backed S3 / MinIO — behind a
//! Cargo feature.
//!
//! ## Why this lives behind a wrapper, not inside `RedbCas`
//!
//! Same reason as the M2 [`crate::BloomCas`] decorator: the tier
//! composition is one specific deployment shape, not a property
//! of any particular backend. A test that wants an undecorated
//! `RedbCas` keeps having it; a CAS node that wants hot caching
//! wraps it in [`TieredCas`].
//!
//! ## Promotion / eviction policy
//!
//! - **Reads** that hit hot return immediately. Reads that miss
//!   hot fall through to warm; on a warm hit we *promote* the
//!   blob into hot (LRU eviction may push out an older blob to
//!   stay under the byte budget).
//! - **Writes** populate warm authoritatively and hot eagerly.
//!   This keeps the hot tier warm for blobs the worker just
//!   produced — they're the most likely to be read back as
//!   action inputs.
//! - **No tier demotion in M3a.** Warm-to-cold demotion is M3b;
//!   in M3a a blob lives in warm forever (subject to M5's GC).
//!
//! ## Atomicity
//!
//! `batch_update_blobs` is atomic per-entry, not per-batch: each
//! `(digest, bytes)` either lands in both tiers or in neither.
//! The hot insert follows the warm `Ok(())` for that entry; a
//! warm-side write failure means the hot side is also untouched.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use brokkr_common::Digest;
use bytes::Bytes;

use crate::error::CasError;
use crate::traits::{Cas, UpdateResult};

/// Composite CAS with an in-memory hot LRU in front of a warm
/// `Cas` backend. Cheap to clone — clones share the same hot tier
/// and the same backend handle.
#[derive(Clone)]
pub struct TieredCas<W: Cas> {
    warm: Arc<W>,
    hot: Arc<Mutex<HotTier>>,
}

impl<W: Cas + std::fmt::Debug> std::fmt::Debug for TieredCas<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TieredCas")
            .field("warm", &self.warm)
            .field("hot", &"<HotTier>")
            .finish()
    }
}

impl<W: Cas> TieredCas<W> {
    /// Compose a hot LRU of `hot_capacity_bytes` in front of `warm`.
    /// A capacity of zero disables the hot tier (every read falls
    /// straight through to warm); useful for tests.
    pub fn new(warm: Arc<W>, hot_capacity_bytes: usize) -> Self {
        Self {
            warm,
            hot: Arc::new(Mutex::new(HotTier::new(hot_capacity_bytes))),
        }
    }

    /// Bytes currently held in the hot tier. Useful for /metrics
    /// and tests.
    pub fn hot_bytes(&self) -> usize {
        match self.hot.lock() {
            Ok(g) => g.bytes,
            Err(p) => p.into_inner().bytes,
        }
    }

    /// Number of blobs currently held in the hot tier.
    pub fn hot_len(&self) -> usize {
        match self.hot.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        }
    }

    fn hot_get(&self, digest: &Digest) -> Option<Bytes> {
        match self.hot.lock() {
            Ok(mut g) => g.get(digest),
            Err(p) => p.into_inner().get(digest),
        }
    }

    fn hot_put(&self, digest: Digest, bytes: Bytes) {
        match self.hot.lock() {
            Ok(mut g) => g.put(digest, bytes),
            Err(p) => p.into_inner().put(digest, bytes),
        }
    }
}

#[async_trait]
impl<W: Cas + 'static> Cas for TieredCas<W> {
    async fn find_missing_blobs(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
        // Hot is a cache, not an authoritative store: a "missing
        // from hot" answer doesn't mean missing from the CAS. Just
        // delegate. (Composing with `BloomCas` is how a caller
        // would short-circuit this further.)
        self.warm.find_missing_blobs(digests).await
    }

    async fn batch_update_blobs(
        &self,
        blobs: Vec<(Digest, Bytes)>,
    ) -> Result<Vec<UpdateResult>, CasError> {
        // Clone the bytes for hot-tier population — we don't know
        // how many succeed until the warm response.
        let pairs: Vec<(Digest, Bytes)> = blobs.clone();
        let results = self.warm.batch_update_blobs(blobs).await?;
        for (r, (digest, bytes)) in results.iter().zip(pairs.into_iter()) {
            if r.status.is_ok() {
                debug_assert_eq!(&r.digest, &digest);
                self.hot_put(digest, bytes);
            }
        }
        Ok(results)
    }

    async fn batch_read_blobs(
        &self,
        digests: &[Digest],
    ) -> Result<Vec<Result<Bytes, CasError>>, CasError> {
        // First pass: serve from hot what we can; collect a list
        // of indices that still need a warm fetch.
        let mut out: Vec<Option<Result<Bytes, CasError>>> =
            (0..digests.len()).map(|_| None).collect();
        let mut to_fetch: Vec<(usize, Digest)> = Vec::new();
        for (i, d) in digests.iter().enumerate() {
            if let Some(b) = self.hot_get(d) {
                out[i] = Some(Ok(b));
            } else {
                to_fetch.push((i, d.clone()));
            }
        }
        if to_fetch.is_empty() {
            return Ok(out.into_iter().flatten().collect());
        }

        // Warm pass — only the cold misses go to disk.
        let warm_digests: Vec<Digest> = to_fetch.iter().map(|(_, d)| d.clone()).collect();
        let warm_results = self.warm.batch_read_blobs(&warm_digests).await?;
        for ((i, digest), warm_res) in to_fetch.into_iter().zip(warm_results.into_iter()) {
            if let Ok(bytes) = &warm_res {
                // Promote into hot. Cloning is cheap (Bytes is
                // Arc-backed).
                self.hot_put(digest, bytes.clone());
            }
            out[i] = Some(warm_res);
        }
        Ok(out.into_iter().flatten().collect())
    }
}

/// In-memory size-bounded LRU. Eviction is by bytes, not entry
/// count, because blob sizes vary widely (a small action protobuf
/// vs. a multi-MiB compiler binary) and a count-based cap would
/// either over- or under-allocate memory for both.
///
/// Implementation: a `HashMap` for O(1) lookups paired with a
/// linked list of digests in LRU order. The linked list is a
/// `Vec<NodeIndex>` with prev/next indices into a node pool; we
/// keep a free-list of evicted indices to reuse. This is the
/// classic hash + intrusive doubly-linked-list trick, encoded
/// purely in safe Rust with `usize` indices.
struct HotTier {
    capacity_bytes: usize,
    bytes: usize,
    map: HashMap<Digest, NodeIdx>,
    nodes: Vec<Slot>,
    free: Vec<NodeIdx>,
    head: NodeIdx,
    tail: NodeIdx,
}

type NodeIdx = usize;
const NONE: NodeIdx = usize::MAX;

struct Slot {
    digest: Digest,
    bytes: Bytes,
    prev: NodeIdx,
    next: NodeIdx,
}

impl HotTier {
    fn new(capacity_bytes: usize) -> Self {
        Self {
            capacity_bytes,
            bytes: 0,
            map: HashMap::new(),
            nodes: Vec::new(),
            free: Vec::new(),
            head: NONE,
            tail: NONE,
        }
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn get(&mut self, digest: &Digest) -> Option<Bytes> {
        let idx = *self.map.get(digest)?;
        self.move_to_head(idx);
        Some(self.nodes[idx].bytes.clone())
    }

    fn put(&mut self, digest: Digest, bytes: Bytes) {
        if self.capacity_bytes == 0 {
            return;
        }
        if let Some(&idx) = self.map.get(&digest) {
            // Refresh existing entry. Adjust byte count for any
            // size change, then move to head.
            let old_len = self.nodes[idx].bytes.len();
            self.bytes = self
                .bytes
                .saturating_sub(old_len)
                .saturating_add(bytes.len());
            self.nodes[idx].bytes = bytes;
            self.move_to_head(idx);
            self.evict_until_under_capacity();
            return;
        }

        let new_len = bytes.len();
        // If a single blob exceeds the whole budget, skip caching
        // it — we'd otherwise evict every other blob just to hold
        // this one. The action-cache miss path still works; we
        // just don't hot-promote it.
        if new_len > self.capacity_bytes {
            return;
        }

        let idx = self.alloc(digest.clone(), bytes);
        self.map.insert(digest, idx);
        self.bytes = self.bytes.saturating_add(new_len);
        self.push_to_head(idx);
        self.evict_until_under_capacity();
    }

    fn evict_until_under_capacity(&mut self) {
        while self.bytes > self.capacity_bytes && self.tail != NONE {
            let victim = self.tail;
            let victim_len = self.nodes[victim].bytes.len();
            let victim_digest = self.nodes[victim].digest.clone();
            self.unlink(victim);
            self.map.remove(&victim_digest);
            self.bytes = self.bytes.saturating_sub(victim_len);
            self.recycle(victim);
        }
    }

    fn alloc(&mut self, digest: Digest, bytes: Bytes) -> NodeIdx {
        let slot = Slot {
            digest,
            bytes,
            prev: NONE,
            next: NONE,
        };
        if let Some(idx) = self.free.pop() {
            self.nodes[idx] = slot;
            idx
        } else {
            self.nodes.push(slot);
            self.nodes.len() - 1
        }
    }

    fn recycle(&mut self, idx: NodeIdx) {
        // Zero out the bytes / digest to drop them and to make
        // accidental reuse loud in debug builds.
        self.nodes[idx].bytes = Bytes::new();
        self.nodes[idx].prev = NONE;
        self.nodes[idx].next = NONE;
        self.free.push(idx);
    }

    fn push_to_head(&mut self, idx: NodeIdx) {
        self.nodes[idx].prev = NONE;
        self.nodes[idx].next = self.head;
        if self.head != NONE {
            self.nodes[self.head].prev = idx;
        }
        self.head = idx;
        if self.tail == NONE {
            self.tail = idx;
        }
    }

    fn unlink(&mut self, idx: NodeIdx) {
        let prev = self.nodes[idx].prev;
        let next = self.nodes[idx].next;
        if prev != NONE {
            self.nodes[prev].next = next;
        } else {
            self.head = next;
        }
        if next != NONE {
            self.nodes[next].prev = prev;
        } else {
            self.tail = prev;
        }
    }

    fn move_to_head(&mut self, idx: NodeIdx) {
        if idx == self.head {
            return;
        }
        self.unlink(idx);
        self.push_to_head(idx);
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

    fn hot_only() -> HotTier {
        HotTier::new(128)
    }

    #[test]
    fn hot_tier_empty_get_returns_none() {
        let mut hot = hot_only();
        assert!(hot.get(&Digest::of(b"x")).is_none());
    }

    #[test]
    fn hot_tier_put_then_get() {
        let mut hot = hot_only();
        let (d, b) = blob(b"hello");
        hot.put(d.clone(), b.clone());
        assert_eq!(hot.get(&d).unwrap(), b);
    }

    #[test]
    fn hot_tier_evicts_when_over_capacity() {
        let mut hot = HotTier::new(20);
        let a = (Digest::of(b"a"), Bytes::from_static(b"0123456789")); // 10 bytes
        let b = (Digest::of(b"b"), Bytes::from_static(b"0123456789")); // 10 bytes
        let c = (Digest::of(b"c"), Bytes::from_static(b"0123456789")); // 10 bytes
        hot.put(a.0.clone(), a.1.clone());
        hot.put(b.0.clone(), b.1.clone());
        // Capacity is 20 bytes; first two fit exactly. Adding c
        // evicts the oldest (a).
        hot.put(c.0.clone(), c.1.clone());
        assert!(hot.get(&a.0).is_none(), "a should have been evicted");
        assert!(hot.get(&b.0).is_some());
        assert!(hot.get(&c.0).is_some());
    }

    #[test]
    fn hot_tier_get_promotes_to_mru() {
        let mut hot = HotTier::new(20);
        let a = (Digest::of(b"a"), Bytes::from_static(b"0123456789"));
        let b = (Digest::of(b"b"), Bytes::from_static(b"0123456789"));
        let c = (Digest::of(b"c"), Bytes::from_static(b"0123456789"));
        hot.put(a.0.clone(), a.1.clone());
        hot.put(b.0.clone(), b.1.clone());
        // Touch a so it's the MRU; adding c should evict b instead.
        let _ = hot.get(&a.0);
        hot.put(c.0.clone(), c.1.clone());
        assert!(hot.get(&a.0).is_some());
        assert!(hot.get(&b.0).is_none(), "b should have been evicted");
        assert!(hot.get(&c.0).is_some());
    }

    #[test]
    fn hot_tier_skips_blobs_larger_than_capacity() {
        let mut hot = HotTier::new(10);
        let (d, b) = blob(b"this is much larger than ten bytes");
        hot.put(d.clone(), b);
        assert!(hot.get(&d).is_none());
        assert_eq!(hot.len(), 0);
    }

    #[test]
    fn zero_capacity_disables_hot_caching() {
        let mut hot = HotTier::new(0);
        let (d, b) = blob(b"x");
        hot.put(d.clone(), b);
        assert!(hot.get(&d).is_none());
    }

    #[tokio::test]
    async fn tiered_cas_warm_only_when_hot_capacity_zero() {
        let warm = Arc::new(InMemoryCas::new());
        let cas = TieredCas::new(warm, 0);
        let (d, b) = blob(b"hello");
        cas.batch_update_blobs(vec![(d.clone(), b.clone())])
            .await
            .unwrap();
        // Hot capacity is zero, so reads have to fall through.
        let read = cas.batch_read_blobs(&[d]).await.unwrap();
        assert_eq!(read[0].as_ref().unwrap(), &b);
        assert_eq!(cas.hot_len(), 0);
    }

    #[tokio::test]
    async fn tiered_cas_populates_hot_on_write() {
        let warm = Arc::new(InMemoryCas::new());
        let cas = TieredCas::new(warm, 1024);
        let (d, b) = blob(b"hello");
        cas.batch_update_blobs(vec![(d.clone(), b.clone())])
            .await
            .unwrap();
        assert_eq!(cas.hot_len(), 1);
        // Read should be served from hot (we can't distinguish the
        // path from the public Cas API; check via the hot counter
        // and the byte total).
        let read = cas.batch_read_blobs(&[d]).await.unwrap();
        assert_eq!(read[0].as_ref().unwrap(), &b);
    }

    #[tokio::test]
    async fn tiered_cas_promotes_from_warm_on_read() {
        let warm = Arc::new(InMemoryCas::new());
        // Pre-populate warm directly so the hot tier is empty.
        let (d, b) = blob(b"warm only");
        warm.batch_update_blobs(vec![(d.clone(), b.clone())])
            .await
            .unwrap();
        let cas = TieredCas::new(warm, 1024);
        assert_eq!(cas.hot_len(), 0);
        // First read: hot miss → warm hit → promote.
        let read = cas.batch_read_blobs(&[d.clone()]).await.unwrap();
        assert_eq!(read[0].as_ref().unwrap(), &b);
        assert_eq!(cas.hot_len(), 1);
        // Second read still works (now hot hit).
        let read2 = cas.batch_read_blobs(&[d]).await.unwrap();
        assert_eq!(read2[0].as_ref().unwrap(), &b);
    }

    #[tokio::test]
    async fn tiered_cas_find_missing_delegates_to_warm() {
        let warm = Arc::new(InMemoryCas::new());
        let cas = TieredCas::new(warm, 1024);
        let (d1, b1) = blob(b"one");
        let (d2, _) = blob(b"two");
        cas.batch_update_blobs(vec![(d1.clone(), b1)])
            .await
            .unwrap();
        let missing = cas.find_missing_blobs(&[d1, d2.clone()]).await.unwrap();
        assert_eq!(missing, vec![d2]);
    }

    #[tokio::test]
    async fn tiered_cas_does_not_cache_failed_writes() {
        let warm = Arc::new(InMemoryCas::new());
        let cas = TieredCas::new(warm, 1024);
        let lying = Digest::of(b"hello");
        let wrong = Bytes::from_static(b"world");
        cas.batch_update_blobs(vec![(lying.clone(), wrong)])
            .await
            .unwrap();
        // Write was rejected; hot should not have it.
        assert_eq!(cas.hot_len(), 0);
        // And the read still surfaces NotFound.
        let read = cas.batch_read_blobs(&[lying]).await.unwrap();
        assert!(matches!(read[0], Err(CasError::NotFound(_))));
    }
}
