//! Linux-only implementation of `Sandbox::run`.
//!
//! Phase 2 evolution:
//!
//! - **M2**: pipe a JSON config to `brokkr-sandboxd` over fd 3, wait, drain.
//! - **M3–M5**: namespaces / rootfs / netns done inside the runner.
//! - **M6** (this milestone): per-action cgroup, wall-clock timeout
//!   enforcement, OOM detection, accounting readback.
//!
//! ### M6 ordering: when does the cgroup attach happen?
//!
//! ```text
//! host           runner (brokkr-sandboxd)
//! ────────────   ────────────────────────
//! spawn          execve, then read_to_end(fd 3)  ← BLOCKS here
//! attach pid
//! write config
//! close          unblocks, does namespace setup, fork, exec action
//! wait
//! ```
//!
//! The attach lands between `spawn` and `write config`. The runner is
//! parked on `read_to_end(fd 3)` for that whole window because the
//! pipe stays open until the host closes its writer end, so the
//! attach is guaranteed to complete *before* the runner's children
//! exist. cgroups are inherited by descendants, so init / the action
//! / their children all land in the same cgroup automatically.

use std::fs::File;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use uuid::Uuid;

use super::cgroup::Cgroup;
use super::ipc::create_config_pipe;
use crate::config::SandboxConfig;
use crate::error::SandboxError;
use crate::outcome::{ExitStatus, ResourceAccounting, SandboxOutcome, SandboxTimings};

pub(super) async fn run_action(
    runner_binary: &Path,
    cgroup_root: Option<&Path>,
    cfg: SandboxConfig,
) -> Result<SandboxOutcome, SandboxError> {
    let setup_start = Instant::now();

    let payload = serde_json::to_vec(&cfg)?;

    let pipe = create_config_pipe().map_err(|e| SandboxError::Setup {
        step: "create config pipe",
        source: e,
    })?;
    let child_read_fd: RawFd = pipe.reader_raw();

    let mut cmd = Command::new(runner_binary);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();

    // SAFETY: pre_exec runs in the freshly-forked child between fork and
    // exec. We perform only async-signal-safe operations: dup2(2),
    // close(2), and fcntl(2) on file descriptors we own. We do not
    // allocate, touch globals, or call non-reentrant libc routines.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(move || {
            const TARGET_FD: RawFd = 3;
            if child_read_fd != TARGET_FD {
                nix::unistd::dup2(child_read_fd, TARGET_FD).map_err(io::Error::from)?;
                nix::unistd::close(child_read_fd).map_err(io::Error::from)?;
            } else {
                // dup2(N, N) is a no-op and does NOT clear CLOEXEC.
                nix::fcntl::fcntl(
                    TARGET_FD,
                    nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
                )
                .map_err(io::Error::from)?;
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|e| SandboxError::Setup {
        step: "spawn runner",
        source: e,
    })?;

    // Decompose the pipe so the host's copy of the read end closes
    // immediately (the child has its own copy at fd 3) and `writer` is
    // free to move into the synchronous-write block below.
    let crate::host::ipc::ConfigPipe { writer, reader } = pipe;
    drop(reader);

    // M6: create a per-action cgroup and attach the runner pid before
    // we let the runner make progress. The runner is currently parked
    // on `read_to_end(fd 3)`; it can't fork until we close the writer.
    let runner_pid = child.id().ok_or_else(|| SandboxError::Setup {
        step: "read runner pid",
        source: io::Error::other("tokio child has no pid"),
    })?;
    let cgroup = if let Some(root) = cgroup_root {
        let leaf = format!("action-{}", Uuid::new_v4());
        let cg = Cgroup::create(root, &leaf, &cfg.limits).map_err(SandboxError::Cgroup)?;
        cg.attach(runner_pid).map_err(SandboxError::Cgroup)?;
        Some(cg)
    } else {
        None
    };

    // Take stdout / stderr off the child so we can drive `wait` and
    // pipe-draining concurrently — wait_with_output wouldn't let us
    // SIGKILL on timeout because it consumes the Child.
    let stdout = child.stdout.take().ok_or_else(|| SandboxError::Setup {
        step: "take stdout",
        source: io::Error::other("child stdout already taken"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| SandboxError::Setup {
        step: "take stderr",
        source: io::Error::other("child stderr already taken"),
    })?;
    let stdout_task = tokio::spawn(read_to_end(stdout));
    let stderr_task = tokio::spawn(read_to_end(stderr));

    // Write the JSON payload. EPIPE is tolerated — see M2 notes for why.
    let write_err = {
        use std::io::Write as _;
        let mut file = File::from(writer);
        let res = file.write_all(&payload).and_then(|()| file.flush()).err();
        drop(file);
        res
    };
    if let Some(e) = &write_err {
        if e.kind() != io::ErrorKind::BrokenPipe {
            return Err(SandboxError::Setup {
                step: "write config payload",
                source: io::Error::new(e.kind(), e.to_string()),
            });
        }
    }

    let exec_start = Instant::now();
    let setup_elapsed = exec_start - setup_start;

    // Wait for the runner. If `wall_clock_secs` is set, race against a
    // deadline; on elapsed, ask the cgroup to SIGKILL every PID
    // inside (including the runner) and reap.
    let wall_clock = cfg.limits.wall_clock_secs.map(Duration::from_secs);
    let (wait_status, hit_timeout) = match wall_clock {
        None => (
            child.wait().await.map_err(|e| SandboxError::Setup {
                step: "wait for runner",
                source: e,
            })?,
            false,
        ),
        Some(deadline) => match tokio::time::timeout(deadline, child.wait()).await {
            Ok(Ok(s)) => (s, false),
            Ok(Err(e)) => {
                return Err(SandboxError::Setup {
                    step: "wait for runner",
                    source: e,
                });
            }
            Err(_elapsed) => {
                // SIGKILL the whole cgroup if we have one (catches
                // grandchildren); otherwise just kill the runner.
                if let Some(cg) = &cgroup {
                    let _ = cg.kill_all();
                } else {
                    let _ = child.kill().await;
                }
                let s = child.wait().await.map_err(|e| SandboxError::Setup {
                    step: "wait for runner after timeout",
                    source: e,
                })?;
                (s, true)
            }
        },
    };

    let stdout_buf = stdout_task.await.unwrap_or_default();
    let stderr_buf = stderr_task.await.unwrap_or_default();

    // If the host's write hit EPIPE *and* the runner exited non-zero with
    // a diagnostic on stderr, prefer the runner's message — same M2 logic.
    if write_err.is_some() && !wait_status.success() && !stderr_buf.is_empty() {
        return Err(SandboxError::RunnerCrashed(
            String::from_utf8_lossy(&stderr_buf).trim().to_string(),
        ));
    }

    let teardown_start = Instant::now();
    let exec_elapsed = teardown_start - exec_start;

    let oom = cgroup.as_ref().map(Cgroup::was_oom_killed).unwrap_or(false);
    let exit_status = if hit_timeout {
        ExitStatus::Timeout
    } else if oom {
        ExitStatus::OutOfMemory
    } else if let Some(code) = wait_status.code() {
        ExitStatus::Exited(code)
    } else if let Some(signal) = wait_status.signal() {
        ExitStatus::Signaled { signal }
    } else {
        ExitStatus::Signaled { signal: -1 }
    };

    let accounting = cgroup
        .as_ref()
        .map(Cgroup::accounting)
        .unwrap_or_else(ResourceAccounting::default);

    Ok(SandboxOutcome {
        exit_status,
        stdout: Bytes::from(stdout_buf),
        stderr: Bytes::from(stderr_buf),
        accounting,
        timings: SandboxTimings {
            setup: setup_elapsed,
            execution: exec_elapsed,
            teardown: teardown_start.elapsed(),
        },
    })
}

async fn read_to_end<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf).await;
    buf
}
