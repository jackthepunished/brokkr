//! In-memory Content-Addressable Storage.

use std::collections::HashMap;

use bytes::Bytes;
use parking_lot::RwLock;

use brokkr_common::Digest;

/// In-memory CAS store backed by a `HashMap`.
pub struct InMemoryCas {
    blobs: RwLock<HashMap<Digest, Bytes>>,
}

impl InMemoryCas {
    /// Create a new empty in-memory CAS.
    pub fn new() -> Self {
        Self {
            blobs: RwLock::new(HashMap::new()),
        }
    }

    /// Insert blobs into the store.
    pub fn batch_update(&self, blobs: Vec<(Digest, Bytes)>) -> Vec<(Digest, i64)> {
        let mut store = self.blobs.write();
        blobs
            .into_iter()
            .map(|(digest, data)| {
                let size = data.len() as i64;
                store.insert(digest.clone(), data);
                (digest, size)
            })
            .collect()
    }

    /// Read blobs by digest. Returns `None` for missing blobs.
    pub fn batch_read(&self, digests: Vec<Digest>) -> Vec<Option<Bytes>> {
        let store = self.blobs.read();
        digests
            .into_iter()
            .map(|d| store.get(&d).cloned())
            .collect()
    }

    /// Return digests that are NOT present in the store.
    pub fn find_missing(&self, digests: Vec<Digest>) -> Vec<Digest> {
        let store = self.blobs.read();
        digests
            .into_iter()
            .filter(|d| !store.contains_key(d))
            .collect()
    }

    /// Returns the number of blobs currently stored.
    pub fn len(&self) -> usize {
        self.blobs.read().len()
    }

    /// Returns true if the store contains no blobs.
    pub fn is_empty(&self) -> bool {
        self.blobs.read().is_empty()
    }
}

impl Default for InMemoryCas {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn make_digest(hash: &str, size: i64) -> Digest {
        Digest {
            hash: hash.to_string(),
            size_bytes: size,
        }
    }

    #[test]
    fn batch_update_and_read() {
        let cas = InMemoryCas::new();
        let d1 = make_digest("abc123", 5);
        let blob = Bytes::from(&b"hello"[..]);

        cas.batch_update(vec![(d1.clone(), blob.clone())]);

        let results = cas.batch_read(vec![d1]);
        assert_eq!(results, vec![Some(blob)]);
    }

    #[test]
    fn find_missing_returns_absent() {
        let cas = InMemoryCas::new();
        let d1 = make_digest("abc123", 5);
        let d2 = make_digest("def456", 3);

        cas.batch_update(vec![(d1.clone(), Bytes::from(&b"hello"[..]))]);

        let missing = cas.find_missing(vec![d1, d2.clone()]);
        assert_eq!(missing, vec![d2]);
    }

    #[test]
    fn read_nonexistent_returns_none() {
        let cas = InMemoryCas::new();
        let missing = cas.find_missing(vec![make_digest("nonexistent", 0)]);
        assert!(missing.len() == 1);
    }
}
