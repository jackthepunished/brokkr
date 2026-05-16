//! Bloom filter over CAS digests.
//!
//! Phase 3 / M2. Each CAS node maintains one [`Bloom`] over the
//! digests it stores. `FindMissingBlobs` consults the filter before
//! the disk hit: a `contains(d) == false` answer is authoritative
//! ("definitely missing"), so the on-disk check can be skipped.
//! `contains(d) == true` is probabilistic — the disk check still
//! runs.
//!
//! ## Sizing
//!
//! The filter is sized from `(expected_items, false_positive_rate)`
//! via the standard bloom-filter formulas:
//!
//! - bits `m = ⌈-n · ln(p) / (ln 2)²⌉` rounded up to a 64-bit word.
//! - hash count `k = ⌈(m/n) · ln 2⌉`, clamped to ≥ 1.
//!
//! For `n = 1,000,000` and `p = 0.01` (the Phase 3 default) this
//! produces `m ≈ 9.6 Mbits ≈ 1.2 MiB` and `k = 7`. Sizing is a
//! one-time cost paid at construction; the hot path is plain bit
//! ops.
//!
//! ## Hashing
//!
//! We re-use the digest's existing sha256. The 64-hex-char hash
//! (256 bits, 32 bytes) is split into two `u64` halves (`h1`, `h2`)
//! by parsing the first 32 hex chars. From those we derive `k`
//! independent-enough hash functions via the
//! Kirsch–Mitzenmacher construction `h_i = h1 + i · h2`. The trick
//! is a textbook approximation: two independent base hashes
//! generate up to `O(m)` derived hashes with the same false-positive
//! behaviour as `k` truly independent ones, for free. Avoids
//! re-running sha256 on every check/insert.
//!
//! ## Concurrency
//!
//! [`Bloom`] is `Send` + `Sync` only for shared *reads*: callers
//! hold an `Arc<RwLock<Bloom>>` if they need concurrent insert +
//! query. Inserts are not lock-free; bloom rebuilds in Phase 3 M2
//! happen on a single thread.

use brokkr_common::Digest;

/// Saturating bloom filter over CAS digests. Inserts are
/// monotonic — `contains` never returns false after a successful
/// `insert` of the same digest. `clear` resets the filter to
/// empty.
#[derive(Debug)]
pub struct Bloom {
    /// Backing bit array, in 64-bit words.
    bits: Box<[u64]>,
    /// Total number of bits (= `bits.len() * 64`).
    bit_len: usize,
    /// Number of derived hash functions.
    k: u32,
    /// Approximate cardinality (inserts since creation / last
    /// clear). The filter cannot remove items, so this is an upper
    /// bound — duplicates inflate the count.
    items: u64,
}

impl Bloom {
    /// Build a bloom filter sized for `expected_items` at the given
    /// `fp_rate`. The fp_rate must be in `(0.0, 1.0)`; values
    /// outside that range are clamped to a safe default (1%).
    ///
    /// `expected_items` of zero is treated as one — a 64-bit filter
    /// is the minimum useful size.
    pub fn new(expected_items: u64, fp_rate: f64) -> Self {
        let n = expected_items.max(1) as f64;
        let p = if (0.0..1.0).contains(&fp_rate) && fp_rate > 0.0 {
            fp_rate
        } else {
            0.01
        };

        // m = ceil(-n * ln(p) / (ln 2)^2). The negative sign cancels
        // because ln(p) is negative for p in (0, 1).
        let ln2 = std::f64::consts::LN_2;
        let m_float = (-n * p.ln() / (ln2 * ln2)).ceil();
        let m = (m_float as usize).max(64);
        let words = m.div_ceil(64);
        let bit_len = words * 64;

        // k = ceil((m / n) * ln 2)
        let k_float = ((m_float / n) * ln2).ceil();
        let k = (k_float as u32).max(1);

        Self {
            bits: vec![0u64; words].into_boxed_slice(),
            bit_len,
            k,
            items: 0,
        }
    }

    /// Total bit count of the underlying array. Useful for
    /// diagnostics (`bit_len / 8` is the rough memory footprint).
    pub fn bit_len(&self) -> usize {
        self.bit_len
    }

    /// Number of derived hash functions in use.
    pub fn k(&self) -> u32 {
        self.k
    }

    /// Approximate cardinality — number of successful `insert` calls
    /// since construction or last `clear`. Duplicates count.
    pub fn items(&self) -> u64 {
        self.items
    }

    /// Record `digest` in the filter. Subsequent `contains(digest)`
    /// is guaranteed to return `true`.
    pub fn insert(&mut self, digest: &Digest) {
        let (h1, h2) = derive_pair(digest);
        for i in 0..self.k {
            let bit = self.bit_index(h1, h2, i);
            let word = bit / 64;
            let mask = 1u64 << (bit % 64);
            self.bits[word] |= mask;
        }
        self.items = self.items.saturating_add(1);
    }

    /// `true` if `digest` is *probably* present, `false` if it is
    /// definitely absent. The probabilistic side is the false
    /// positive rate; false negatives never happen.
    pub fn contains(&self, digest: &Digest) -> bool {
        let (h1, h2) = derive_pair(digest);
        for i in 0..self.k {
            let bit = self.bit_index(h1, h2, i);
            let word = bit / 64;
            let mask = 1u64 << (bit % 64);
            if self.bits[word] & mask == 0 {
                return false;
            }
        }
        true
    }

    /// Reset the filter to empty. Used by the periodic rebuild path
    /// to compact accumulated noise — the bloom is always a
    /// superset of the actual contents, so a fresh rebuild from the
    /// warm tier's redb table tightens the false-positive rate.
    pub fn clear(&mut self) {
        for w in self.bits.iter_mut() {
            *w = 0;
        }
        self.items = 0;
    }

    fn bit_index(&self, h1: u64, h2: u64, i: u32) -> usize {
        // Kirsch–Mitzenmacher: h_i = h1 + i*h2 (mod m).
        let combined = h1.wrapping_add((i as u64).wrapping_mul(h2));
        (combined as usize) % self.bit_len
    }
}

/// Derive `(h1, h2)` from a digest's sha256 hex. The first 16 hex
/// chars become `h1`, the next 16 become `h2` — that's 128 of the
/// digest's 256 hash bits, plenty for the
/// Kirsch–Mitzenmacher derivation of more hash functions.
///
/// Digest construction validates the 64-char lowercase-hex shape;
/// the parses cannot fail in practice. The `unwrap_or(0)` is
/// defensive — falling back to zero would only cause a slightly
/// hotter false-positive rate on a hypothetical mis-constructed
/// digest, never a correctness violation.
fn derive_pair(digest: &Digest) -> (u64, u64) {
    let hex = digest.hash();
    debug_assert_eq!(hex.len(), 64, "digest hash should be 64 hex chars");
    let h1 = u64::from_str_radix(&hex[0..16], 16).unwrap_or(0);
    let h2 = u64::from_str_radix(&hex[16..32], 16).unwrap_or(0);
    (h1, h2)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn digest(s: &str) -> Digest {
        Digest::of(s.as_bytes())
    }

    #[test]
    fn empty_filter_returns_false_for_any_digest() {
        let bloom = Bloom::new(1024, 0.01);
        for i in 0..32 {
            assert!(!bloom.contains(&digest(&format!("x-{i}"))));
        }
    }

    #[test]
    fn insert_makes_contains_return_true() {
        let mut bloom = Bloom::new(1024, 0.01);
        let d = digest("hello");
        bloom.insert(&d);
        assert!(bloom.contains(&d));
    }

    #[test]
    fn clear_returns_filter_to_empty() {
        let mut bloom = Bloom::new(1024, 0.01);
        let d = digest("hello");
        bloom.insert(&d);
        assert!(bloom.contains(&d));
        bloom.clear();
        assert!(!bloom.contains(&d));
        assert_eq!(bloom.items(), 0);
    }

    #[test]
    fn sizing_picks_reasonable_parameters() {
        let bloom = Bloom::new(1_000_000, 0.01);
        // For n=1M, p=0.01: m ≈ 9.6 Mbits, k ≈ 7.
        assert!(bloom.bit_len() >= 9_000_000 && bloom.bit_len() <= 10_000_000);
        assert!((6..=8).contains(&bloom.k()));
    }

    #[test]
    fn items_counter_tracks_inserts() {
        let mut bloom = Bloom::new(1024, 0.01);
        for i in 0..50 {
            bloom.insert(&digest(&format!("x-{i}")));
        }
        assert_eq!(bloom.items(), 50);
    }

    /// False-positive rate stays within a generous bound of the
    /// configured target. Tests the actual statistical property
    /// rather than just the sizing formula.
    #[test]
    fn false_positive_rate_is_under_target() {
        let n = 10_000u64;
        let target_p = 0.01;
        let mut bloom = Bloom::new(n, target_p);

        for i in 0..n {
            bloom.insert(&digest(&format!("member-{i}")));
        }

        // Probe with 10x as many novel digests as members to get a
        // tight estimate; assert the rate is ≤ 2× the configured
        // target. The +slack absorbs statistical noise.
        let probe = 100_000u64;
        let mut hits = 0u64;
        for i in 0..probe {
            if bloom.contains(&digest(&format!("non-member-{i}"))) {
                hits += 1;
            }
        }
        let rate = hits as f64 / probe as f64;
        assert!(
            rate < target_p * 2.0,
            "false-positive rate {rate:.4} exceeded 2x target {target_p}; \
             hits={hits} / {probe}",
        );
    }

    #[test]
    fn distinct_digests_do_not_all_collide() {
        // Sanity check that the filter actually discriminates — if
        // it returned true for everything, the test above would
        // pass for the wrong reason.
        let mut bloom = Bloom::new(1024, 0.01);
        bloom.insert(&digest("member"));
        let mut misses = 0;
        for i in 0..100 {
            if !bloom.contains(&digest(&format!("not-member-{i}"))) {
                misses += 1;
            }
        }
        assert!(misses > 90, "filter saturated; only {misses}/100 misses");
    }
}
