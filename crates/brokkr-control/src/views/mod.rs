//! Read-model DTOs and pure projections for the observability surface
//! (ADR 0012).
//!
//! Every type here is a *view*: a snapshot of state shaped for an external
//! consumer, deliberately decoupled from the internal types it is derived
//! from. Internal state must not leak across this boundary — that rule is
//! what lets the scheduler and registry change shape without breaking the
//! wire format or the TUI.
//!
//! # `owning_node` is on every node-local DTO, on purpose
//!
//! In an HA cluster the worker registry, the job-history ring, the CAS and the
//! policy engine are all **per node** — see
//! `docs/operations/running-a-cluster.md`. Aggregation unions them, so each
//! record must say which node it came from or the merged view would present
//! three different local truths as one cluster fact.

mod cas;
mod policy;
mod worker;

pub use cas::{cas_stats_view, CasStatsView};
pub use policy::{policy_view, PolicyView, REASONS};
pub use worker::{worker_views, WorkerView};
