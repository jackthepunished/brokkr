//! Shared types, errors, and utilities used across all Brokkr crates.
//!
//! `brokkr-common` is the only crate every other Brokkr crate may depend on.
//! Keep it small and dependency-light.

#![deny(missing_docs)]

mod digest_mod;
mod error;
mod id;

pub use digest_mod::Digest;
pub use error::{CasError, Error};
pub use id::{JobId, TenantId, WorkerId};

/// Compute the SHA-256 digest of data.
#[must_use]
pub fn sha256(data: &[u8]) -> Digest {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    digest_mod::Digest::from_hash(result.into())
}
