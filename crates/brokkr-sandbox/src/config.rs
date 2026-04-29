//! Configuration types for a single sandboxed action.
//!
//! [`SandboxConfig`] is also the IPC payload between the host worker and the
//! `brokkr-sandboxd` runner: it serialises to JSON and is sent over the
//! configuration pipe (file descriptor 3 by convention).
//!
//! Phase 2 milestones light up the runner-side handling of these fields
//! incrementally — see `docs/phase-2-plan.md` §9. M2 honours `argv`, `env`,
//! and `workdir`; the rest are accepted on the wire so the API stays stable
//! while later milestones add behaviour.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Full configuration for one sandboxed action.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Command to execute. `argv[0]` is looked up via `PATH` if it does not
    /// contain a slash.
    pub argv: Vec<String>,

    /// Environment passed to the action. M2 passes this verbatim. M8 will
    /// scrub `LD_PRELOAD` etc. and inject deterministic defaults per
    /// [`DeterminismPolicy`].
    #[serde(default)]
    pub env: Vec<(String, String)>,

    /// Working directory inside the sandbox. M2 chdirs the runner to this
    /// path before exec.
    #[serde(default)]
    pub workdir: Option<PathBuf>,

    /// What the action's stdin should be wired to.
    #[serde(default)]
    pub stdin: StdioPolicy,

    /// Filesystem layout (M3+).
    #[serde(default)]
    pub rootfs: RootfsSpec,

    /// Resource limits (M6).
    #[serde(default)]
    pub limits: ResourceLimits,

    /// Network policy (M5).
    #[serde(default)]
    pub network: NetworkPolicy,

    /// Determinism guards (M8).
    #[serde(default)]
    pub determinism: DeterminismPolicy,

    /// Capabilities to retain (M7). Default: empty (drop everything).
    #[serde(default)]
    pub retained_caps: Vec<String>,

    /// Additional syscalls to allow on top of the default seccomp whitelist
    /// (M7).
    #[serde(default)]
    pub extra_seccomp_allow: Vec<String>,
}

impl SandboxConfig {
    /// Construct a minimal config that runs `argv` with no env, no workdir,
    /// default policies for everything else.
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            ..Default::default()
        }
    }
}

/// What the action's stdin should be wired to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StdioPolicy {
    /// `/dev/null`.
    #[default]
    Null,
    /// Inherit the runner's stdin (rarely useful; mostly for interactive
    /// debugging).
    InheritStdin,
}

/// Filesystem layout for the sandbox rootfs. M2 ignores this; M3 starts
/// honouring it. See `docs/phase-2-plan.md` §5.1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RootfsSpec {
    /// Read-only host paths to bind into the rootfs. Each entry is
    /// `(host_path, sandbox_path)`.
    #[serde(default)]
    pub ro_binds: Vec<(PathBuf, PathBuf)>,

    /// Read-write tmpfs mounts inside the sandbox. Each entry is
    /// `(sandbox_path, size_bytes)`.
    #[serde(default)]
    pub tmpfs: Vec<(PathBuf, u64)>,

    /// Optional input tree to materialize under [`SandboxConfig::workdir`].
    #[serde(default)]
    pub input_root: Option<PathBuf>,
}

/// Per-action resource limits. `None` means "do not constrain". M6 wires
/// these into per-action cgroups.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// CPU bandwidth limit, in milli-CPU (1000 = 1 core). Maps to
    /// `cpu.max`.
    #[serde(default)]
    pub cpu_milli: Option<u64>,
    /// Memory limit in bytes. Maps to `memory.max`.
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    /// Maximum concurrent PIDs. Maps to `pids.max`.
    #[serde(default)]
    pub pids_max: Option<u64>,
    /// Per-block-device read throughput limit. Maps to `io.max`.
    #[serde(default)]
    pub io_read_bytes_per_sec: Option<u64>,
    /// Per-block-device write throughput limit. Maps to `io.max`.
    #[serde(default)]
    pub io_write_bytes_per_sec: Option<u64>,
    /// Wall-clock timeout, in seconds. The runner SIGKILLs the action when
    /// this elapses and reports [`crate::ExitStatus::Timeout`].
    #[serde(default)]
    pub wall_clock_secs: Option<u64>,
}

/// Network policy for the action's network namespace. M5 wires this in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// New empty netns. No interfaces, no routes — fully hermetic.
    #[default]
    None,
    /// New netns with the loopback interface brought up.
    Loopback,
}

/// Determinism guards. M8 wires these in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeterminismPolicy {
    /// Hostname to set inside the UTS namespace. `None` = leave as-is.
    /// Default policy in `brokkr-worker` will set this to `brokkr-sandbox`.
    #[serde(default)]
    pub hostname: Option<String>,
    /// Force `/etc/localtime` → `Etc/UTC`.
    #[serde(default)]
    pub timezone_utc: bool,
    /// `SOURCE_DATE_EPOCH` to inject for reproducible-build tooling.
    #[serde(default)]
    pub source_date_epoch: Option<i64>,
    /// Strip `LD_PRELOAD` and `LD_LIBRARY_PATH` from the env before exec.
    #[serde(default)]
    pub strip_ld_preload: bool,
    /// Replace `PATH` with a fixed default (`/usr/bin:/bin`).
    #[serde(default)]
    pub strip_path: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_json() {
        let cfg = SandboxConfig {
            argv: vec!["/bin/echo".into(), "hi".into()],
            env: vec![("PATH".into(), "/usr/bin".into())],
            workdir: Some("/work".into()),
            limits: ResourceLimits {
                memory_bytes: Some(1 << 30),
                ..Default::default()
            },
            network: NetworkPolicy::Loopback,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: SandboxConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.argv, cfg.argv);
        assert_eq!(back.env, cfg.env);
        assert_eq!(back.workdir, cfg.workdir);
        assert_eq!(back.limits.memory_bytes, Some(1 << 30));
        assert_eq!(back.network, NetworkPolicy::Loopback);
    }

    #[test]
    fn defaults_compose_into_a_minimal_config() {
        let cfg = SandboxConfig::new(vec!["/bin/true".into()]);
        assert_eq!(cfg.stdin, StdioPolicy::Null);
        assert_eq!(cfg.network, NetworkPolicy::None);
        assert!(cfg.env.is_empty());
        assert!(cfg.rootfs.ro_binds.is_empty());
        assert!(cfg.limits.memory_bytes.is_none());
    }
}
