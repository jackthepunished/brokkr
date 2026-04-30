//! PID-namespace init + reaper for the runner.
//!
//! After [`super::userns::setup_namespaces`] has unshared a new PID
//! namespace, the *next* `fork(2)` is PID 1 in that namespace — the
//! kernel calls this process the "init" of the namespace. Phase 2 / M4
//! uses a small init that:
//!
//! 1. mounts a fresh `/proc` so userspace tools see only the sandbox's
//!    PIDs;
//! 2. forks once more for the action (PID 2);
//! 3. loops on `waitpid(-1, …)` reaping any process that exits, until
//!    the action itself exits — at which point init translates the
//!    action's wait-status to its own exit code/signal and exits. Init
//!    dying as PID 1 makes the kernel SIGKILL every other PID in the
//!    namespace, which is exactly the EV-13 cleanup we want.
//!
//! The outer runner (the process the host spawned, still in the host's
//! PID namespace) then `waitpid`s on init and re-raises whatever signal
//! killed init — or exits with init's exit code — so the host's
//! existing `std::process::ExitStatus` mapping is preserved.

use nix::mount::{mount, MsFlags};
use nix::sys::signal::{self, SigHandler};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::Pid;

use super::{die, errno_message, nix_io};

/// Translate a terminal `WaitStatus` into a process exit on the current
/// process and never return. Used by both the outer runner (mirroring
/// init's exit) and init (mirroring the action's exit) so the host
/// observes the action's status as if it were the runner's own.
///
/// On `Exited`: exit with `code`. On `Signaled`: re-raise the same
/// signal after restoring the default disposition; fall back to
/// `128 + signal` if the kernel doesn't actually deliver it (e.g.
/// SIGSTOP).
pub(super) fn exit_with(status: WaitStatus) -> ! {
    match status {
        WaitStatus::Exited(_, code) => std::process::exit(code),
        WaitStatus::Signaled(_, sig, _) => {
            // SAFETY: signal()/raise() are async-signal-safe; on
            // failure we fall through to process::exit below so we
            // still terminate.
            #[allow(unsafe_code)]
            unsafe {
                let _ = signal::signal(sig, SigHandler::SigDfl);
            }
            let _ = signal::raise(sig);
            std::process::exit(128 + sig as i32);
        }
        // Stopped/Continued/StillAlive shouldn't reach here — waitpid
        // without WUNTRACED/WCONTINUED only returns terminal statuses.
        _ => std::process::exit(127),
    }
}

/// Mount procfs onto `/proc`. Must be called from inside the new PID
/// namespace so the resulting procfs reflects the sandbox's PIDs and
/// not the host's.
pub(super) fn mount_proc() -> std::io::Result<()> {
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .map_err(nix_io)
}

/// Block until `target` exits, reaping every other zombie that lands on
/// PID 1 along the way. Diverges via `exit_with`.
pub(super) fn reap_until(target: Pid) -> ! {
    loop {
        let status = match waitpid(Pid::from_raw(-1), None) {
            Ok(s) => s,
            Err(errno) => die("waitpid", &errno_message(errno)),
        };
        let pid = match status {
            WaitStatus::Exited(p, _) | WaitStatus::Signaled(p, _, _) => p,
            // Non-terminal statuses can't appear without WUNTRACED; ignore.
            _ => continue,
        };
        if pid == target {
            exit_with(status);
        }
        // Orphan: keep reaping.
    }
}
