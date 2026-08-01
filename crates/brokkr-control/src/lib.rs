//! Brokkr control plane.
//!
//! Houses the API gateway (REAPI gRPC), action cache, scheduler, worker
//! registry, and metadata store. Strongly consistent. Phase 5 replaces the
//! embedded `redb` store with a custom Raft KV.

#![deny(missing_docs)]

pub mod auth;
pub mod fairqueue;
pub mod lease;
pub mod locality;
pub mod matching;
pub mod membership;
pub mod metakv;
pub mod policy_abi;
pub mod policy_reload;
pub mod raftkv;
pub mod registry;
pub mod scheduler;
pub mod scheduling;
pub mod services;
pub mod views;
pub mod wasm_strategy;
pub mod worker_service;

pub use auth::{auth_interceptor, AuthError, Authenticator, JwtAuth};
pub use fairqueue::FairQueue;
pub use lease::LeaseTable;
pub use matching::{eligible_workers, labels_satisfy_platform, worker_satisfies};
pub use membership::{Membership, MembershipServiceImpl};
pub use metakv::{MetaKv, MetaKvActionCache, MetaKvError, RedbMetaKv};
pub use raftkv::{KvMachine, RaftKv};
pub use registry::{
    HeartbeatPolicy, RegistryError, WorkerCapabilities, WorkerRecord, WorkerRegistry,
};
pub use scheduler::{spawn_lease_reaper, Scheduler};
pub use scheduling::{BinPacking, ConnectedWorkers, LoadView, SimpleFifo, Strategy};
pub use services::{ActionCacheService, CapabilitiesService, CasService, ExecutionService};
pub use worker_service::{spawn_eviction_task, SharedWorkerRegistry, WorkerServiceImpl};
