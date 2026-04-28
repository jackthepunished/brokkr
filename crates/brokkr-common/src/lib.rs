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
