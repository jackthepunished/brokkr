//! Hermetic sandbox runtime for Brokkr workers.
//!
//! Built directly on Linux primitives — mount/PID/user/network namespaces,
//! cgroups v2, seccomp-bpf, capability dropping. **No Docker, runc, or
//! containerd dependency, ever.** Phase 2 lights this crate up incrementally
//! per `docs/phase-2-plan.md`.
//!
//! ## Architecture
//!
//! Two halves share this crate:
//!
//! - **Host side** ([`Sandbox`], [`SandboxConfig`], [`SandboxOutcome`]):
//!   the worker imports this and calls [`Sandbox::run`] for each action.
//! - **Runner side** ([`run_as_runner`]): the body of the
//!   `brokkr-sandboxd` binary. The host spawns it once per action, sends
//!   the config over file descriptor 3, and waits for it to `execve` the
//!   action.
//!
//! [`checks`] is a separate, free-standing module that runs Linux
//! probes ahead of any sandbox work; the worker's `--check-host` flag
//! delegates to it.
//!
//! Splitting host and runner across two processes is the standard pattern
//! for Linux sandboxes (bubblewrap, crun, nsjail, runj). See
//! `docs/phase-2-plan.md` §3.1 for the rationale.

#![deny(missing_docs)]
// The sandbox crate calls `pre_exec`, `dup2`, and `from_raw_fd` directly.
// Every `unsafe` block below has a `// SAFETY:` justification per
// `CLAUDE.md` rule #2; we override the workspace-level deny so the crate
// can compile.
#![allow(unsafe_code)]

mod config;
mod error;
mod host;
mod outcome;
mod runner;

pub mod checks;
pub mod host_check;

pub use config::{
    DeterminismPolicy, NetworkPolicy, ResourceLimits, RootfsSpec, SandboxConfig, StdioPolicy,
};
pub use error::SandboxError;
pub use host::Sandbox;
pub use outcome::{ExitStatus, ResourceAccounting, SandboxOutcome, SandboxTimings};
pub use runner::run_as_runner;
