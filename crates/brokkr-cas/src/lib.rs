//! Content-Addressable Storage (CAS) for Brokkr.
//!
//! Phase 1 ships an in-memory and `redb`-backed single-node CAS implementing
//! the REAPI `ContentAddressableStorage` and `ByteStream` services.
//! Phase 3 adds hash-prefix sharding, replication, and tiered storage.

#![deny(missing_docs)]

pub mod action_cache;
pub mod error;
pub mod in_memory;
pub mod redb_backend;
pub mod traits;

pub use action_cache::{ActionCache, RedbActionCache};
pub use error::CasError;
pub use in_memory::InMemoryCas;
pub use redb_backend::RedbCas;
pub use traits::Cas;
