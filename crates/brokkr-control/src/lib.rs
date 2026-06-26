//! Brokkr control plane.
//!
//! Houses the API gateway (REAPI gRPC), action cache, scheduler, worker
//! registry, and metadata store. Strongly consistent. Phase 5 replaces the
//! embedded `redb` store with a custom Raft KV.

#![deny(missing_docs)]

pub mod membership;
pub mod registry;
pub mod scheduler;
pub mod services;
pub mod worker_service;

pub use membership::{Membership, MembershipServiceImpl};
pub use registry::{
    HeartbeatPolicy, RegistryError, WorkerCapabilities, WorkerRecord, WorkerRegistry,
};
pub use scheduler::Scheduler;
pub use services::{ActionCacheService, CapabilitiesService, CasService, ExecutionService};
pub use worker_service::{spawn_eviction_task, SharedWorkerRegistry, WorkerServiceImpl};
