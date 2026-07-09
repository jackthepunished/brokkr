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
        .env_clear()
        // If anything between `spawn` and the final `wait` returns
        // early (cgroup setup, config-pipe write, etc.), `Child` gets
        // dropped. Without `kill_on_drop`, the runner would keep
        // running orphaned — this guarantees SIGKILL on drop so the
        // wall-clock contract holds on the error paths too.
        .kill_on_drop(true);

    // SAFETY: pre_exec runs in the freshly-forked child between fork and
    // exec. We perform only async-signal-safe operations: dup2(2),
    // close(2), fcntl(2), and setsid(2) (all listed in the POSIX
    // signal-safety(7) table). We do not allocate, touch globals, or
    // call non-reentrant libc routines.
    //
    // setsid makes the runner a new session leader and the leader of
    // a single-member process group. On the namespace path,
    // init (forked inside the new pidns) inherits the runner's pgid;
    // init then forks the action, which also inherits it. On timeout
    // or on host-side error, the host sends `killpg(-runner_pid,
    // SIGKILL)` — which reaches init, SIGKILLing it and triggering
    // kernel pidns teardown. On the M2 no-isolation path the runner
    // is the action, so killpg just kills the runner (same effect as
    // today). See issue #142.
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
            // New session / process-group leader. Async-signal-safe
            // per POSIX signal-safety(7); only depends on the calling
            // process (returns EPERM if already a session leader, but
            // tokio's Command::spawn does not set that up, so we never
            // hit it in practice).
            nix::unistd::setsid().map_err(io::Error::from)?;
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|e| SandboxError::Setup {
        step: "spawn runner",
        source: e,
    })?;

    // M6 + #142: take the runner's pid *now* so we can hand it to
    // the KillpgOnDrop wrapper below. We then move `child` into the
    // wrapper, which fires killpg on any host-side early return.
    // `Command::kill_on_drop(true)` stays on the inner Command as a
    // belt-and-suspenders for paths that bypass the wrapper, but it
    // only kills the runner — not the namespace pidns init + action
    // — so the wrapper's killpg is what actually closes the leak.
    let mut child = {
        let runner_pid_raw = child.id().ok_or_else(|| SandboxError::Setup {
            step: "read runner pid",
            source: io::Error::other("tokio child has no pid"),
        })?;
        KillpgOnDrop::new(child, runner_pid_raw as i32)
    };

    // Decompose the pipe so the host's copy of the read end closes
    // immediately (the child has its own copy at fd 3) and `writer` is
    // free to move into the synchronous-write block below.
    let crate::host::ipc::ConfigPipe { writer, reader } = pipe;
    drop(reader);

    // M6: create a per-action cgroup and attach the runner pid before
    // we let the runner make progress. The runner is currently parked
    // on `read_to_end(fd 3)`; it can't fork until we close the writer.
    // We pass through KillpgOnDrop::id() here — the pid was captured
    // once at spawn time (above) and never changes.
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
    // SIGKILL on timeout because it consumes the Child. The wrapper
    // forwards `.stdout`/`.stderr` directly.
    let stdout = child.stdout.take().ok_or_else(|| SandboxError::Setup {
        step: "take stdout",
        source: io::Error::other("child stdout already taken"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| SandboxError::Setup {
        step: "take stderr",
        source: io::Error::other("child stderr already taken"),
    })?;
    let stdout_task = tokio::spawn(read_capped(stdout, MAX_CAPTURED_OUTPUT_BYTES, "stdout"));
    let stderr_task = tokio::spawn(read_capped(stderr, MAX_CAPTURED_OUTPUT_BYTES, "stderr"));

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
                // grandchildren); otherwise killpg the runner's
                // process group. killpg reaches the runner + the
                // namespace pidns init (which inherited the runner's
                // pgid via fork) + the action (which inherited via
                // init). SIGKILLing init triggers kernel pidns
                // teardown — the kernel sweeps every remaining
                // process in the namespace. ESRCH is success: the
                // M2 path where the runner IS the action self-exits,
                // and the group is already empty.
                //
                // If killpg fails for any other reason (EPERM, etc.)
                // we fall back to child.kill() so we still have a
                // chance of reaping. Issue #142.
                let cgroup_killed = match &cgroup {
                    Some(cg) => cg.kill_all().is_ok(),
                    None => false,
                };
                let pid_group_killed = match nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(runner_pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                ) {
                    Ok(()) => true,
                    Err(nix::errno::Errno::ESRCH) => true, // already gone
                    Err(e) => {
                        tracing::warn!(
                            runner_pid,
                            error = %e,
                            "killpg on runner process group failed; falling back to child.kill()"
                        );
                        false
                    }
                };
                if !cgroup_killed && !pid_group_killed {
                    let _ = child.kill().await;
                }
                // Bound the post-kill wait. If the kernel won't reap
                // the runner within the same deadline budget, surface
                // a TimedOut error rather than hang forever.
                let s = match tokio::time::timeout(deadline, child.wait()).await {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        return Err(SandboxError::Setup {
                            step: "wait for runner after timeout",
                            source: e,
                        });
                    }
                    Err(_) => {
                        return Err(SandboxError::Setup {
                            step: "wait for runner after timeout kill",
                            source: io::Error::new(
                                io::ErrorKind::TimedOut,
                                "runner did not exit after timeout SIGKILL",
                            ),
                        });
                    }
                };
                (s, true)
            }
        },
    };

    let stdout_buf = join_capture(stdout_task.await, "stdout");
    let stderr_buf = join_capture(stderr_task.await, "stderr");

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

/// Maximum bytes captured from a single runner output stream before
/// truncation.
///
/// A sandboxed action can write unbounded data to stdout/stderr; buffering it
/// all on the host is an OOM vector (issue #67). Past the cap we keep draining
/// the pipe (so the runner doesn't block on a full pipe) but discard the rest.
const MAX_CAPTURED_OUTPUT_BYTES: usize = 50 * 1024 * 1024;

/// Read from `r`, retaining at most `cap` bytes; drain and drop the excess
/// with a one-time `warn`.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    mut r: R,
    cap: usize,
    stream: &str,
) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut warned = false;
    loop {
        match r.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if buf.len() >= cap && !warned {
                        warned = true;
                        tracing::warn!(
                            stream,
                            cap,
                            "captured runner output exceeded cap; truncating and draining the rest"
                        );
                    }
                }
                // Past the cap we keep looping to drain `r` without buffering.
            }
            Err(_) => break,
        }
    }
    buf
}

/// Resolve a joined stdout/stderr pump task into its captured bytes.
///
/// A `JoinError` means the pump task panicked or was cancelled (e.g. the
/// runtime shutting down mid-action). We can't recover the bytes it was
/// holding, so we fall back to an empty buffer — but we log a warning so the
/// truncation is visible to an operator instead of vanishing silently, which
/// `unwrap_or_default()` would have done (issue #68).
fn join_capture(joined: Result<Vec<u8>, tokio::task::JoinError>, stream: &str) -> Vec<u8> {
    match joined {
        Ok(buf) => buf,
        Err(e) => {
            tracing::warn!(
                stream,
                error = %e,
                "output capture task failed to join; {stream} will be reported as empty"
            );
            Vec::new()
        }
    }
}

/// Wrapper around `tokio::process::Child` that SIGKILLs the runner's
/// whole process group on drop, unless `wait` has already consumed
/// the child (i.e. the runner exited normally). This closes the
/// error-path leak called out in issue #142: the host's early
/// returns between `spawn` and `wait` (cgroup setup failure,
/// config-pipe write error, stdout/stderr take failures) would
/// otherwise drop the Child and leave the namespace PID 1 + the
/// action alive on the host. `Command::kill_on_drop(true)` only
/// reaches the immediate child, which is not enough on the
/// namespace path.
struct KillpgOnDrop {
    child: Option<tokio::process::Child>,
    runner_pid: i32,
}

impl KillpgOnDrop {
    fn new(child: tokio::process::Child, runner_pid: i32) -> Self {
        Self {
            child: Some(child),
            runner_pid,
        }
    }

    /// Take the child out of the wrapper so Drop becomes a no-op.
    /// Returns `None` if `wait` has already been called (shouldn't
    /// happen — we only expose `wait` and `id`, both of which take
    /// the child exactly once).
    fn take(&mut self) -> tokio::process::Child {
        self.child
            .take()
            .expect("KillpgOnDrop::take called after child was already taken")
    }

    /// Pass-through to `tokio::process::Child::id`.
    fn id(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    /// Pass-through to `tokio::process::Child::wait`. Takes the
    /// child out of the wrapper on the first call so Drop becomes a
    /// no-op; a second call would have panicked on `take()` anyway.
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.take().wait().await
    }
}

impl std::ops::Deref for KillpgOnDrop {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target {
        // Safe: `child` is only `None` after `take` (i.e. after
        // `wait` was called), and post-`wait` the wrapper is
        // about to be dropped without further field access.
        self.child
            .as_ref()
            .expect("KillpgOnDrop::deref called after child was taken")
    }
}

impl std::ops::DerefMut for KillpgOnDrop {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.child
            .as_mut()
            .expect("KillpgOnDrop::deref_mut called after child was taken")
    }
}

impl Drop for KillpgOnDrop {
    fn drop(&mut self) {
        if self.child.take().is_some() {
            // Best-effort killpg. ESRCH means the runner already
            // exited (e.g. M2 path where the runner IS the action
            // and self-exited before the host could wait); treat
            // that as success. EPERM / EACCES shouldn't happen
            // because we own the runner's session, but log them
            // so a future regression is visible.
            match nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.runner_pid),
                nix::sys::signal::Signal::SIGKILL,
            ) {
                Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
                Err(e) => tracing::warn!(
                    runner_pid = self.runner_pid,
                    error = %e,
                    "killpg on runner process group failed during drop"
                ),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::join_capture;

    #[tokio::test]
    async fn join_capture_passes_through_successful_output() {
        let task = tokio::spawn(async { vec![1u8, 2, 3] });
        assert_eq!(join_capture(task.await, "stdout"), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn join_capture_returns_empty_when_task_panics() {
        // A panicking pump task yields a JoinError; the buffer must come back
        // empty rather than propagating the panic or hanging.
        let task = tokio::spawn(async {
            panic!("simulated pump panic");
            #[allow(unreachable_code)]
            Vec::<u8>::new()
        });
        assert!(join_capture(task.await, "stderr").is_empty());
    }
}
