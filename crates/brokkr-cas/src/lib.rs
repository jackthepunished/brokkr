//! Content-Addressable Storage (CAS) for Brokkr.
//!
//! Phase 1 ships an in-memory and `redb`-backed single-node CAS implementing
//! the REAPI `ContentAddressableStorage` and `ByteStream` services.
//! Phase 3 adds hash-prefix sharding, replication, and tiered storage.

#![deny(missing_docs)]

pub mod action_cache;
pub mod bloom;
pub mod bloom_cas;
pub mod error;
pub mod gc;
pub mod in_memory;
pub mod peer;
pub mod redb_backend;
pub mod replicated;
pub mod ring;
pub mod router;
pub mod tiered;
pub mod traits;
pub mod tree;

pub use action_cache::{ActionCache, GcWindowGuard, RedbActionCache};
pub use bloom::Bloom;
pub use bloom_cas::BloomCas;
pub use error::CasError;
pub use in_memory::InMemoryCas;
pub use peer::{repair_cluster, repair_node, RepairReport};
pub use redb_backend::RedbCas;
pub use replicated::{ReplicaPool, ReplicatedCas, StaticPool};
pub use ring::{replicas_for, score, NodeStatus, RingNode};
pub use router::{Router, Topology};
pub use tiered::TieredCas;
pub use traits::Cas;
