//! Brokkr control plane.
//!
//! Houses the API gateway (REAPI gRPC), action cache, scheduler, worker
//! registry, and metadata store. Strongly consistent. Phase 5 replaces the
//! embedded `redb` store with a custom Raft KV.

#![deny(missing_docs)]
