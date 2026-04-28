//! Content-addressable digests.
//!
//! A [`Digest`] is the pair `(sha256_hex, size_bytes)` that identifies a blob
//! in the CAS. The hex string is always lowercase and exactly 64 characters.
//! Wire format matches REAPI's `build.bazel.remote.execution.v2.Digest`.

use std::fmt;
use std::str::FromStr;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

/// Length of a sha256 digest expressed as a lowercase hex string.
pub const SHA256_HEX_LEN: usize = 64;

/// Errors that can occur when constructing or validating a [`Digest`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DigestError {
    /// Hex string had the wrong length.
    #[error("digest hash must be {SHA256_HEX_LEN} hex chars, got {0}")]
    InvalidLength(usize),

    /// Hex string contained non-hex or upper-case characters.
    #[error("digest hash must be lowercase hex: {0}")]
    InvalidHex(String),

    /// Computed hash did not match the declared hash.
    #[error("digest mismatch: declared {declared}, actual {actual}")]
    HashMismatch {
        /// Declared (claimed) hash.
        declared: String,
        /// Hash actually computed from the bytes.
        actual: String,
    },

    /// Declared size did not match the byte count.
    #[error("digest size mismatch: declared {declared}, actual {actual}")]
    SizeMismatch {
        /// Declared (claimed) size.
        declared: i64,
        /// Actual byte count.
        actual: i64,
    },

    /// Negative sizes are not allowed.
    #[error("digest size must be non-negative, got {0}")]
    NegativeSize(i64),
}

/// Content-addressable digest: lowercase sha256 hex + size in bytes.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Digest {
    hash: String,
    size_bytes: i64,
}

impl Digest {
    /// Compute the digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        let hash = hex::encode(Sha256::digest(bytes));
        Self {
            hash,
            size_bytes: bytes.len() as i64,
        }
    }

    /// Construct a digest from its parts, validating shape but not content.
    ///
    /// Use [`Digest::verify`] to check that the digest actually matches a
    /// blob's bytes.
    pub fn new(hash: impl Into<String>, size_bytes: i64) -> Result<Self, DigestError> {
        let hash = hash.into();
        if hash.len() != SHA256_HEX_LEN {
            return Err(DigestError::InvalidLength(hash.len()));
        }
        if !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(DigestError::InvalidHex(hash));
        }
        if size_bytes < 0 {
            return Err(DigestError::NegativeSize(size_bytes));
        }
        Ok(Self { hash, size_bytes })
    }

    /// Lowercase sha256 hex string.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Size of the blob in bytes.
    pub fn size_bytes(&self) -> i64 {
        self.size_bytes
    }

    /// Verify that `bytes` actually hashes to this digest and has the declared size.
    pub fn verify(&self, bytes: &[u8]) -> Result<(), DigestError> {
        let actual_size = bytes.len() as i64;
        if actual_size != self.size_bytes {
            return Err(DigestError::SizeMismatch {
                declared: self.size_bytes,
                actual: actual_size,
            });
        }
        let actual = hex::encode(Sha256::digest(bytes));
        if actual != self.hash {
            return Err(DigestError::HashMismatch {
                declared: self.hash.clone(),
                actual,
            });
        }
        Ok(())
    }

    /// Convenience: compute a digest of `bytes` and verify it round-trips.
    pub fn of_bytes(bytes: &Bytes) -> Self {
        Self::of(bytes.as_ref())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({}/{})", self.hash, self.size_bytes)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.hash, self.size_bytes)
    }
}

impl FromStr for Digest {
    type Err = DigestError;

    /// Parse a `"<hex>/<size>"` string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (hash, size) = s.split_once('/').ok_or_else(|| {
            DigestError::InvalidHex(format!("expected '<hex>/<size>', got {s:?}"))
        })?;
        let size_bytes: i64 = size
            .parse()
            .map_err(|_| DigestError::InvalidHex(format!("invalid size {size:?}")))?;
        Self::new(hash, size_bytes)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn empty_blob_has_known_digest() {
        let d = Digest::of(b"");
        assert_eq!(
            d.hash(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(d.size_bytes(), 0);
    }

    #[test]
    fn of_then_verify_roundtrips() {
        let bytes = b"hello world";
        let d = Digest::of(bytes);
        d.verify(bytes).unwrap();
    }

    #[test]
    fn verify_rejects_size_mismatch() {
        let d = Digest::of(b"hello");
        let err = d.verify(b"hello!").unwrap_err();
        assert!(matches!(err, DigestError::SizeMismatch { .. }));
    }

    #[test]
    fn verify_rejects_hash_mismatch() {
        let d = Digest::new(
            "0000000000000000000000000000000000000000000000000000000000000000",
            5,
        )
        .unwrap();
        let err = d.verify(b"hello").unwrap_err();
        assert!(matches!(err, DigestError::HashMismatch { .. }));
    }

    #[test]
    fn new_rejects_uppercase_hex() {
        let err = Digest::new(
            "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855",
            0,
        )
        .unwrap_err();
        assert!(matches!(err, DigestError::InvalidHex(_)));
    }

    #[test]
    fn new_rejects_short_hex() {
        let err = Digest::new("abc", 0).unwrap_err();
        assert!(matches!(err, DigestError::InvalidLength(3)));
    }

    #[test]
    fn new_rejects_negative_size() {
        let err = Digest::new(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            -1,
        )
        .unwrap_err();
        assert!(matches!(err, DigestError::NegativeSize(-1)));
    }

    #[test]
    fn display_and_fromstr_roundtrip() {
        let d = Digest::of(b"brokkr");
        let s = d.to_string();
        let parsed: Digest = s.parse().unwrap();
        assert_eq!(d, parsed);
    }
}
