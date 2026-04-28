//! Content-Addressable Storage (CAS) for Brokkr.
//!
//! Phase 1 ships an in-memory and `redb`-backed single-node CAS implementing
//! the REAPI `ContentAddressableStorage` and `ByteStream` services.
//! Phase 3 adds hash-prefix sharding, replication, and tiered storage.

pub mod memory;
pub use memory::InMemoryCas;
