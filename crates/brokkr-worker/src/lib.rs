//! Brokkr worker daemon.
//!
//! Registers with the control plane, leases jobs, materializes inputs from
//! CAS, runs actions inside [`brokkr_sandbox`], and uploads outputs.
//! Phase 1 runs jobs as plain child processes; Phase 2 wraps them in the
//! sandbox.

#![deny(missing_docs)]

pub mod runner;
pub mod worker;

pub use worker::{run_worker, WorkerConfig};
