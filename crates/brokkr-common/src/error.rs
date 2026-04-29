//! Error types for brokkr-common and Phase 1.

use thiserror::Error;

/// CAS-specific errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CasError {
    /// Blob not found in CAS.
    #[error("blob not found: {0}")]
    NotFound(super::Digest),

    /// Size mismatch between digest metadata and actual blob size.
    #[error("size mismatch for {digest}: expected {expected}, got {actual}")]
    SizeMismatch {
        /// The digest with the mismatched size.
        digest: super::Digest,
        /// Expected size in bytes.
        expected: i64,
        /// Actual size in bytes.
        actual: i64,
    },

    /// Hash mismatch - content doesn't match the declared digest.
    #[error("hash mismatch for blob of size {actual}: expected hash prefix {expected_hex}")]
    HashMismatch {
        /// Expected hash (hex prefix).
        expected_hex: String,
        /// Actual blob size.
        actual: i64,
    },
}

/// Top-level error type for Brokkr operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Invalid digest format string.
    #[error("invalid digest format: {0}")]
    InvalidDigest(#[from] super::digest_mod::DigestParseError),

    /// Digest hash has wrong byte length.
    #[error("digest hash length mismatch: {0}")]
    DigestHashLength(#[from] super::digest_mod::DigestConversionError),

    /// CAS operation failed.
    #[error("CAS error: {0}")]
    Cas(#[from] CasError),

    /// IO operation failed.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
