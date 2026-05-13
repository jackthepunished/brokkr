//! Action runner — either Phase 1 plain-process spawn or the Phase 2
//! sandboxed variant.
//!
//! The worker holds a [`Runner`] per its [`crate::WorkerConfig`]:
//!
//! - [`Runner::Plain`] reproduces Phase 1: `tokio::process::Command`
//!   directly against the host. No isolation. Kept for hosts that
//!   can't run the sandbox (no unprivileged userns, no cgroup
//!   delegation) and for in-process integration tests that pre-date
//!   the sandbox crate.
//! - [`Runner::Sandboxed`] wraps a [`brokkr_sandbox::Sandbox`] plus a
//!   per-action template ([`SandboxTemplate`]) — the worker's default
//!   rootfs allowlist, resource limits, network policy, and
//!   determinism knobs. Each `run_command` call clones the template,
//!   overlays the REAPI `Command`'s argv / env / working_directory,
//!   and feeds the result through `Sandbox::run`.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use brokkr_proto::reapi_v2 as rapi;
use brokkr_sandbox::{
    DeterminismPolicy, ExitStatus, NetworkPolicy, ResourceLimits, RootfsSpec, Sandbox,
    SandboxConfig,
};
use bytes::Bytes;
use sha2::{Digest as _, Sha256};
use tokio::process::Command;

/// Outcome of running a `Command`.
#[derive(Debug)]
pub struct RunOutcome {
    /// Process exit code (negative means killed by signal on Unix; on
    /// the sandbox path, signal-kill is mapped to `128 + signal`,
    /// timeout to `124`, OOM to `137`).
    pub exit_code: i32,
    /// Captured stdout.
    pub stdout: Bytes,
    /// Captured stderr.
    pub stderr: Bytes,
}

/// Strategy for running one action.
///
/// `Sandboxed` carries a `Box` because [`SandboxRunner`] embeds a
/// [`SandboxTemplate`] with three `Vec`s and a `RootfsSpec` — boxing
/// it keeps the enum compact and silences clippy's
/// `large_enum_variant` lint.
#[derive(Debug, Clone)]
pub enum Runner {
    /// Phase 1 fallback — no isolation. The action is spawned as a
    /// child of the worker via `tokio::process::Command`.
    Plain,
    /// Phase 2 sandbox via `brokkr-sandbox`.
    Sandboxed(Box<SandboxRunner>),
}

/// Bundle of a sandbox handle plus the per-action template applied to
/// every job. The template's `argv` / `env` / `workdir` are clobbered
/// per action; everything else is the worker-level default.
#[derive(Debug, Clone)]
pub struct SandboxRunner {
    /// The sandbox handle — points at `brokkr-sandboxd` and
    /// optionally a cgroup root.
    pub sandbox: Sandbox,
    /// Per-action template.
    pub template: SandboxTemplate,
}

/// Default knobs applied to every action a worker runs through the
/// sandbox. The REAPI `Command`'s argv / env override the
/// corresponding fields; the rest are constant for the worker's
/// lifetime.
#[derive(Debug, Clone)]
pub struct SandboxTemplate {
    /// Rootfs layout — ro bind allowlist, tmpfs mounts, symlinks.
    pub rootfs: RootfsSpec,
    /// Per-action cgroup limits (memory, pids, cpu, wall-clock).
    pub limits: ResourceLimits,
    /// Network namespace policy.
    pub network: NetworkPolicy,
    /// Determinism guards (hostname, TZ, env scrubbing).
    pub determinism: DeterminismPolicy,
    /// Working directory inside the sandbox.
    pub workdir: PathBuf,
}

impl SandboxTemplate {
    /// The worker's default Phase 2 template: minimal usrmerge rootfs
    /// (host `/usr` ro-bound, tmpfs `/etc` / `/tmp` / `/work`),
    /// no network, `brokkr_defaults` determinism, no resource limits
    /// (the deployer is expected to set memory/pids/timeout via CLI).
    pub fn brokkr_default() -> Self {
        Self {
            rootfs: default_rootfs(),
            limits: ResourceLimits::default(),
            network: NetworkPolicy::None,
            determinism: DeterminismPolicy::brokkr_defaults(),
            workdir: PathBuf::from("/work"),
        }
    }
}

/// Build the worker's default rootfs: ro-bind the host's `/usr` and
/// (if they're real directories on this host) `/lib` / `/lib64`,
/// tmpfs-mount `/etc` / `/tmp` / `/work`, and create the standard
/// usrmerge symlinks (`/bin` → `usr/bin`, etc.).
pub fn default_rootfs() -> RootfsSpec {
    let mut ro_binds = vec![(PathBuf::from("/usr"), PathBuf::from("/usr"))];
    for p in ["/lib64", "/lib"] {
        let path = PathBuf::from(p);
        if path.is_dir() && !path.is_symlink() {
            ro_binds.push((path.clone(), path));
        }
    }
    RootfsSpec {
        ro_binds,
        tmpfs: vec![
            (PathBuf::from("/etc"), 4 * 1024 * 1024),
            (PathBuf::from("/tmp"), 64 * 1024 * 1024),
            (PathBuf::from("/work"), 64 * 1024 * 1024),
        ],
        symlinks: vec![
            (PathBuf::from("/bin"), PathBuf::from("usr/bin")),
            (PathBuf::from("/sbin"), PathBuf::from("usr/sbin")),
            (PathBuf::from("/lib"), PathBuf::from("usr/lib")),
            (PathBuf::from("/lib64"), PathBuf::from("usr/lib64")),
        ],
        input_root: None,
    }
}

/// Run `command` via the configured `runner`. Dispatch is a thin
/// branch — both arms produce the same `RunOutcome` shape so the
/// worker's job handling stays runner-agnostic.
pub async fn run_command(runner: &Runner, command: &rapi::Command) -> Result<RunOutcome> {
    match runner {
        Runner::Plain => run_plain(command).await,
        Runner::Sandboxed(s) => run_sandboxed(s, command).await,
    }
}

async fn run_plain(command: &rapi::Command) -> Result<RunOutcome> {
    let mut argv = command.arguments.iter();
    let argv0 = argv
        .next()
        .ok_or_else(|| anyhow!("Command.arguments is empty"))?;
    let mut cmd = Command::new(argv0);
    cmd.args(argv);
    for env in &command.environment_variables {
        cmd.env(&env.name, &env.value);
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| anyhow!("spawning {argv0}: {e}"))?;
    let exit_code = output.status.code().unwrap_or(-1);
    Ok(RunOutcome {
        exit_code,
        stdout: Bytes::from(output.stdout),
        stderr: Bytes::from(output.stderr),
    })
}

async fn run_sandboxed(runner: &SandboxRunner, command: &rapi::Command) -> Result<RunOutcome> {
    if command.arguments.is_empty() {
        return Err(anyhow!("Command.arguments is empty"));
    }
    let env: Vec<(String, String)> = command
        .environment_variables
        .iter()
        .map(|ev| (ev.name.clone(), ev.value.clone()))
        .collect();

    // REAPI's working_directory is relative to the input root. Phase 2
    // doesn't materialise the input root yet (Phase 3 FUSE), so if the
    // caller specified one we honour it as a sandbox-relative path
    // under workdir; otherwise we land in the template's default
    // workdir.
    let workdir = if command.working_directory.is_empty() {
        runner.template.workdir.clone()
    } else {
        runner.template.workdir.join(&command.working_directory)
    };

    let cfg = SandboxConfig {
        argv: command.arguments.clone(),
        env,
        workdir: Some(workdir),
        stdin: Default::default(),
        rootfs: runner.template.rootfs.clone(),
        limits: runner.template.limits,
        network: runner.template.network,
        determinism: runner.template.determinism.clone(),
        retained_caps: Vec::new(),
        extra_seccomp_allow: Vec::new(),
    };

    let outcome = runner
        .sandbox
        .run(cfg)
        .await
        .map_err(|e| anyhow!("sandbox: {e}"))?;

    let exit_code = match outcome.exit_status {
        ExitStatus::Exited(c) => c,
        // Mirror common shell conventions so action callers can read
        // the exit code meaningfully even without the structured
        // ExitStatus (which Phase 2 doesn't propagate beyond the
        // worker — REAPI ActionResult only carries i32).
        ExitStatus::Signaled { signal } => 128 + signal,
        ExitStatus::OutOfMemory => 137,
        ExitStatus::Timeout => 124,
    };
    Ok(RunOutcome {
        exit_code,
        stdout: outcome.stdout,
        stderr: outcome.stderr,
    })
}

/// Compute the sha256 digest of `bytes` as a REAPI [`Digest`](rapi::Digest).
pub fn proto_digest(bytes: &[u8]) -> rapi::Digest {
    rapi::Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: bytes.len() as i64,
    }
}
