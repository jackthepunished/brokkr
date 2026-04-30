//! Shared types, errors, and utilities used across all Brokkr crates.
//!
//! `brokkr-common` is the only crate every other Brokkr crate may depend on.
//! Keep it small and dependency-light.

#![deny(missing_docs)]

pub mod digest;
pub mod ids;

pub use digest::{Digest, DigestError};
pub use ids::{IdError, JobId, WorkerId};
