//! Public error type for the sandbox.
//!
//! Distinguishes setup-time failures (where the host couldn't even get the
//! action started) from in-action exit conditions (which surface via
//! [`crate::ExitStatus`] inside a successful [`crate::SandboxOutcome`]).

use thiserror::Error;

/// Anything that can go wrong while attempting to execute an action in the
/// sandbox. An action that runs and exits non-zero is *not* an error; that's
/// signalled via the `exit_status` of [`crate::SandboxOutcome`].
#[derive(Error, Debug)]
pub enum SandboxError {
    /// A host-side setup step (creating pipes, spawning the runner, writing
    /// the config payload, etc.) failed. The `step` is a stable identifier
    /// suitable for log fields and metrics labels.
    #[error("sandbox setup failed at step {step}: {source}")]
    Setup {
        /// Stable identifier of the failing step.
        step: &'static str,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A required kernel feature is missing or disabled on this host.
    #[error("the kernel does not support a feature we require: {0}")]
    Unsupported(&'static str),

    /// The runner process exited before the host could finish handing it
    /// the [`crate::SandboxConfig`] payload (M2: the host's pipe write hit
    /// `EPIPE` and the runner exited non-zero with diagnostics on stderr;
    /// later milestones will also surface namespace- or cgroup-setup
    /// failures that happen before the action begins).
    ///
    /// The string is whatever the runner wrote to stderr. A runner that
    /// reads the config successfully and only fails afterwards (e.g. a
    /// missing `argv[0]` or a bad `workdir`) does *not* produce this
    /// error; instead the action's exit status surfaces in
    /// [`crate::SandboxOutcome::exit_status`] (typically `Exited(127)`)
    /// alongside the runner's stderr.
    #[error("the sandbox runner exited abnormally before exec: {0}")]
    RunnerCrashed(String),

    /// A cgroup operation (open, write, mkdir, accounting readback) failed.
    #[error("cgroup operation failed: {0}")]
    Cgroup(#[source] std::io::Error),

    /// Generic I/O error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Config (de)serialization failed.
    #[error("config (de)serialization: {0}")]
    Json(#[from] serde_json::Error),
}
