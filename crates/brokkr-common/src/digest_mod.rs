//! Content-addressable storage digest.

use std::fmt;
use std::str::FromStr;

use hex::FromHex;
use serde::{Deserialize, Serialize};

/// A SHA-256 content digest used throughout Brokkr and the Bazel REAPI.
///
/// The hash is stored as raw bytes (32 octets), not hex-encoded.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest {
    /// Raw SHA-256 bytes (32 octets).
    hash: [u8; 32],
    /// Size in bytes.
    size_bytes: i64,
}

impl Digest {
    /// Creates a `Digest` from raw SHA-256 bytes and a size.
    ///
    /// # Panics
    ///
    /// Panics if `hash` is not exactly 32 bytes.
    #[inline]
    #[must_use]
    pub fn new(hash: [u8; 32], size_bytes: i64) -> Self {
        Self { hash, size_bytes }
    }

    /// Creates a `Digest` from raw SHA-256 bytes produced by a hasher.
    #[inline]
    #[must_use]
    pub fn from_hash(hash: [u8; 32]) -> Self {
        Self {
            hash,
            size_bytes: 0,
        }
    }

    /// Sets the size_bytes field, returning a new Digest.
    #[inline]
    #[must_use]
    pub fn with_size(self, size_bytes: i64) -> Self {
        Self { size_bytes, ..self }
    }

    /// Returns the raw 32-byte SHA-256 hash.
    #[inline]
    #[must_use]
    pub fn hash_bytes(&self) -> &[u8; 32] {
        &self.hash
    }

    /// Returns the hex-encoded SHA-256 hash (64 lowercase hex chars).
    #[inline]
    #[must_use]
    pub fn hash_hex(&self) -> String {
        hex::encode(self.hash)
    }

    /// Returns the size in bytes.
    #[inline]
    #[must_use]
    pub fn size_bytes(&self) -> i64 {
        self.size_bytes
    }

    /// Returns true when the digest has a zero hash (unset / empty).
    #[inline]
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.hash.iter().all(|&b| b == 0)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.hash_hex(), self.size_bytes)
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({}/{})", self.hash_hex(), self.size_bytes)
    }
}

/// Parse a digest from `hex/size` string form.
impl FromStr for Digest {
    type Err = DigestParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (hash_part, size_part) = s
            .strip_prefix('/')
            .and_then(|s| s.split_once('/'))
            .or_else(|| s.split_once('/'))
            .ok_or(DigestParseError::Malformed)?;

        let hash_hex = hash_part.trim_end_matches('/');
        let hash = <[u8; 32]>::from_hex(hash_hex).map_err(|_| DigestParseError::BadHex)?;
        let size_bytes = size_part
            .parse()
            .map_err(|_| DigestParseError::BadSize)?;

        Ok(Self { hash, size_bytes })
    }
}

/// Parse errors for `Digest`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DigestParseError {
    #[error("digest must be in `hex/size` format")]
    Malformed,
    #[error("hex string must be exactly 64 hex characters (32 bytes)")]
    BadHex,
    #[error("size must be a valid non-negative integer")]
    BadSize,
}

impl Serialize for Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        FromStr::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Conversion error for Digest.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DigestConversionError {
    #[error("hash must be exactly 32 bytes, got {0}")]
    InvalidHashLength(usize),
}

impl TryFrom<&[u8]> for Digest {
    type Error = DigestConversionError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if value.len() != 32 {
            return Err(DigestConversionError::InvalidHashLength(value.len()));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(value);
        Ok(Self {
            hash,
            size_bytes: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256;

    #[test]
    #[allow(clippy::expect_used)]
    fn digest_display_roundtrip() {
        let original = sha256(b"hello world");
        let with_size = original.with_size(11);
        let s = with_size.to_string();
        let parsed: Digest = s.parse().expect("valid digest string");
        assert_eq!(
            parsed.hash_hex(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert_eq!(parsed.size_bytes(), 11);
    }

    #[test]
    fn digest_parse_error_invalid_format() {
        let result: Result<Digest, _> = "not valid".parse();
        assert!(result.is_err());
    }

    #[test]
    fn digest_parse_error_invalid_hex() {
        let result: Result<Digest, _> = "zzzzzzzz/11".parse();
        assert!(result.is_err());
    }

    #[test]
    fn digest_from_hash() {
        let d = Digest::from_hash([0u8; 32]);
        assert!(d.is_zero());
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn digest_try_from_slice() {
        let bytes = [0x00u8; 32];
        let d = Digest::try_from(bytes.as_slice()).expect("valid 32-byte slice");
        assert!(d.is_zero());
    }

    #[test]
    fn digest_try_from_slice_wrong_length() {
        let result = Digest::try_from(b"short".as_slice());
        assert!(result.is_err());
    }
}
