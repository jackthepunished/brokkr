//! Shared types, errors, and utilities used across all Brokkr crates.
//!
//! `brokkr-common` is the only crate every other Brokkr crate may depend on.
//! Keep it small and dependency-light.

#![allow(missing_docs)]

use std::fmt;

/// A content-addressable blob digest.
///
/// Identified by SHA-256 hash (hex-encoded) and size in bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest {
    /// Hex-encoded SHA-256 hash.
    pub hash: String,
    /// Size in bytes.
    pub size_bytes: i64,
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.hash, self.size_bytes)
    }
}

impl Digest {
    /// Create a new Digest. Returns None if size_bytes is negative.
    pub fn new(hash: String, size_bytes: i64) -> Option<Self> {
        if size_bytes < 0 {
            return None;
        }
        Some(Self { hash, size_bytes })
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn new_valid() {
        let d = Digest::new("abc123".to_string(), 42).unwrap();
        assert_eq!(d.hash, "abc123");
        assert_eq!(d.size_bytes, 42);
    }

    #[test]
    fn new_negative_size_returns_none() {
        assert!(Digest::new("abc123".to_string(), -1).is_none());
    }
}
