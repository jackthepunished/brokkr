//! Host-side sandbox API: spawning the runner, feeding it a config, and
//! collecting its outcome. The actual namespace / cgroup / seccomp work
//! happens in the runner (see `crate::runner`); the host's job is to set
//! the runner up and wait.

use std::path::{Path, PathBuf};

use crate::config::SandboxConfig;
use crate::error::SandboxError;
use crate::outcome::SandboxOutcome;

#[cfg(target_os = "linux")]
mod cgroup;
#[cfg(target_os = "linux")]
mod ipc;
#[cfg(target_os = "linux")]
mod linux;

/// Host-side sandbox handle.
///
/// One [`Sandbox`] can run many actions sequentially (Phase 2 doesn't
/// support concurrency on a single worker; see plan §13).
#[derive(Debug, Clone)]
pub struct Sandbox {
    runner_binary: PathBuf,
    cgroup_root: Option<PathBuf>,
}

impl Sandbox {
    /// Create a sandbox that spawns `runner_binary` (the path to
    /// `brokkr-sandboxd`) for each action. Use [`Self::with_default_runner`]
    /// when relying on `$PATH` discovery.
    pub fn new(runner_binary: impl Into<PathBuf>) -> Self {
        Self {
            runner_binary: runner_binary.into(),
            cgroup_root: None,
        }
    }

    /// Look up `brokkr-sandboxd` next to the current executable, then on
    /// `$PATH`. Returns `Unsupported` if neither location has the binary.
    pub fn with_default_runner() -> Result<Self, SandboxError> {
        if let Some(path) = discover_runner_binary() {
            Ok(Self::new(path))
        } else {
            Err(SandboxError::Unsupported(
                "brokkr-sandboxd not found next to the worker binary or on PATH",
            ))
        }
    }

    /// Use `cgroup_root` (e.g. `/sys/fs/cgroup/brokkr.slice`) as the
    /// parent cgroup for every action. The runner pid will be moved into
    /// a freshly-created leaf inside it before the action starts; on
    /// exit the leaf is removed. Without this set the sandbox does no
    /// cgroup work and [`crate::ResourceAccounting`] stays at zero.
    ///
    /// `cgroup_root` must be a writable cgroup-v2 directory whose
    /// parent has the `cpu`, `memory`, and `pids` controllers enabled
    /// in `cgroup.subtree_control`. `scripts/install-cgroup-slice.sh`
    /// sets that up; alternatively, run the worker under a systemd
    /// unit with `Delegate=yes`.
    pub fn with_cgroup_root(mut self, cgroup_root: impl Into<PathBuf>) -> Self {
        self.cgroup_root = Some(cgroup_root.into());
        self
    }

    /// Path to the runner binary that this sandbox will spawn.
    pub fn runner_binary(&self) -> &Path {
        &self.runner_binary
    }

    /// Configured cgroup root, if any.
    pub fn cgroup_root(&self) -> Option<&Path> {
        self.cgroup_root.as_deref()
    }

    /// Execute `cfg` inside the sandbox. On Linux this spawns
    /// `brokkr-sandboxd`, sends the config over file descriptor 3, and
    /// waits for the child to exit, draining stdout/stderr. On non-Linux
    /// hosts this returns [`SandboxError::Unsupported`].
    #[tracing::instrument(
        name = "sandbox::run",
        skip(self, cfg),
        fields(
            runner = %self.runner_binary.display(),
            argv0 = cfg.argv.first().map(String::as_str).unwrap_or(""),
        ),
    )]
    pub async fn run(&self, cfg: SandboxConfig) -> Result<SandboxOutcome, SandboxError> {
        if cfg.argv.is_empty() {
            return Err(SandboxError::Setup {
                step: "validate config",
                source: std::io::Error::other("argv must not be empty"),
            });
        }

        #[cfg(target_os = "linux")]
        {
            linux::run_action(&self.runner_binary, self.cgroup_root.as_deref(), cfg).await
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = cfg;
            Err(SandboxError::Unsupported(
                "the Phase 2 sandbox is Linux-only",
            ))
        }
    }
}

fn discover_runner_binary() -> Option<PathBuf> {
    const NAME: &str = "brokkr-sandboxd";

    // 1. Adjacent to the current executable.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // 2. $PATH lookup.
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}
