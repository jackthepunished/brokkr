//! Linux host-compatibility probes for the Phase 2 sandbox.
//!
//! Returns a structured [`Report`] rather than printing directly so different
//! callers (the worker's `--check-host` flag, a future `brokk doctor`, and
//! integration tests) can format it as they wish. Linux-only checks are
//! gated on `target_os = "linux"`; on other hosts the report contains a
//! single `Fail` outcome explaining why.

pub mod linux;
pub mod outcome;
pub mod run;
pub mod status;

// Re-export types so callers can use `checks::Status`, `checks::Outcome`, etc.
pub use outcome::{Outcome, Report};
pub use status::Status;
