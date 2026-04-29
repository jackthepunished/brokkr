//! Linux-only implementation of `Sandbox::run`.
//!
//! Phase 2 / M2 walkthrough:
//!
//! 1. Serialise the [`SandboxConfig`] to JSON.
//! 2. Create a config pipe (`pipe(2)`).
//! 3. Spawn `brokkr-sandboxd` with the read end of the pipe `dup2`'d to
//!    fd 3, stdout / stderr captured.
//! 4. Close our copy of the read end; write the JSON to the write end;
//!    close.
//! 5. Wait for the child to exit; collect stdout / stderr.
//!
//! Subsequent milestones extend this with cgroup attachment (M6),
//! resource-accounting readback (M6), and wall-clock enforcement (M8).

use std::fs::File;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use bytes::Bytes;
use tokio::process::Command;

use super::ipc::create_config_pipe;
use crate::config::SandboxConfig;
use crate::error::SandboxError;
use crate::outcome::{ExitStatus, ResourceAccounting, SandboxOutcome, SandboxTimings};

pub(super) async fn run_action(
    runner_binary: &Path,
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
                // dup2 atomically clears FD_CLOEXEC on the target.
                nix::unistd::dup2(child_read_fd, TARGET_FD).map_err(io::Error::from)?;
                nix::unistd::close(child_read_fd).map_err(io::Error::from)?;
            } else {
                // dup2(N, N) is a no-op and does NOT clear CLOEXEC, so we
                // have to do it explicitly. Without this, the runner's fd 3
                // (which inherited O_CLOEXEC from `pipe2`) would close on
                // `execve`, leaving the runner unable to read its config.
                nix::fcntl::fcntl(
                    TARGET_FD,
                    nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
                )
                .map_err(io::Error::from)?;
            }
            Ok(())
        });
    }

    let child = cmd.spawn().map_err(|e| SandboxError::Setup {
        step: "spawn runner",
        source: e,
    })?;
    // (If `cmd.spawn()` had failed, `pipe.reader` and `pipe.writer` would
    // close via Drop on the early return above — no fd leaks.)

    // Decompose the pipe so the host's copy of the read end closes
    // immediately (the child has its own copy at fd 3) and `writer` is
    // free to move into the synchronous-write block below.
    let crate::host::ipc::ConfigPipe { writer, reader } = pipe;
    drop(reader);

    // Write the JSON payload synchronously and close the write end so the
    // runner sees EOF on fd 3. Phase-2 config payloads are well under the
    // 64 KiB Linux pipe-buffer size, so this never blocks. We deliberately
    // bound `file`'s scope so its Drop closes the fd *before* we wait on
    // the child — otherwise the runner would block forever on `read_to_end`.
    //
    // EPIPE is intentionally tolerated here: it means the runner already
    // exited (e.g. its `pre_exec` or startup failed). In that case the
    // runner's stderr + exit status is the authoritative error, not the
    // pipe write — let `wait_with_output` collect them and report.
    let write_err = {
        use std::io::Write as _;
        // OwnedFd → File is a safe conversion (impl From in std).
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

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| SandboxError::Setup {
            step: "wait for runner",
            source: e,
        })?;

    // If the host's write hit EPIPE *and* the runner exited non-zero with a
    // diagnostic on stderr, prefer the runner's message — it's almost
    // certainly more informative than "Broken pipe".
    if write_err.is_some() && !output.status.success() && !output.stderr.is_empty() {
        return Err(SandboxError::RunnerCrashed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    let teardown_start = Instant::now();
    let exec_elapsed = teardown_start - exec_start;

    let exit_status = if let Some(code) = output.status.code() {
        ExitStatus::Exited(code)
    } else if let Some(signal) = output.status.signal() {
        ExitStatus::Signaled { signal }
    } else {
        // Should not happen: every Unix exit status has a code or a signal.
        ExitStatus::Signaled { signal: -1 }
    };

    Ok(SandboxOutcome {
        exit_status,
        stdout: Bytes::from(output.stdout),
        stderr: Bytes::from(output.stderr),
        accounting: ResourceAccounting::default(),
        timings: SandboxTimings {
            setup: setup_elapsed,
            execution: exec_elapsed,
            teardown: teardown_start.elapsed(),
        },
    })
}
