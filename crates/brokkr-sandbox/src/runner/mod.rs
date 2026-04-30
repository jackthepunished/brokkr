//! Runner-side entry point — runs *inside* `brokkr-sandboxd`, after the
//! re-exec.
//!
//! Phase 2 lights up runner-side work incrementally:
//!
//! - **M2**: read [`SandboxConfig`](crate::SandboxConfig) from fd 3,
//!   chdir, `execvpe`. No isolation.
//! - **M3**: user namespace + mount namespace + `pivot_root` into a
//!   tmpfs rootfs assembled from [`crate::RootfsSpec`].
//! - **M4** (this milestone): PID namespace + an init that reaps
//!   zombies and mirrors the action's exit status.
//! - **M5–M8**: network / cgroup / seccomp / capability / determinism.
//!
//! [`run_as_runner`] returns `!`: it always ends in either `execve` or
//! `_exit`. Errors before exec are written to stderr and exit with code
//! 127, matching the convention of `sh: command not found`.

#[cfg(target_os = "linux")]
mod exec;
#[cfg(target_os = "linux")]
mod mount;
#[cfg(target_os = "linux")]
mod pidns;
#[cfg(target_os = "linux")]
mod userns;

/// Translate a `nix::errno::Errno` into a `std::io::Error` via its raw
/// errno number. Used pervasively in the runner because nix's mount /
/// unshare / pivot_root return `Errno`, and the runner reports failures
/// as plain `io::Error` for uniformity.
#[cfg(target_os = "linux")]
fn nix_io(errno: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(errno as i32)
}

/// Format an `Errno` for inclusion in a runner diagnostic message.
#[cfg(target_os = "linux")]
fn errno_message(errno: nix::errno::Errno) -> String {
    format!("{} ({})", errno.desc(), errno as i32)
}

/// Print a setup-failure diagnostic and exit 127 — matches the
/// `sh: command not found` convention so the host's existing
/// `ExitStatus::Exited(127)` plumbing surfaces it as a runner error.
#[cfg(target_os = "linux")]
fn die(step: &str, message: &str) -> ! {
    eprintln!("brokkr-sandboxd: {step}: {message}");
    std::process::exit(127);
}

/// Runner-side `main`. Called from the `brokkr-sandboxd` binary.
///
/// Always terminates the process — either by `execve`-ing the action or by
/// exiting with code 127 if setup fails.
pub fn run_as_runner() -> ! {
    #[cfg(target_os = "linux")]
    {
        exec::runner_main()
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("brokkr-sandboxd: this binary only runs on Linux");
        std::process::exit(127);
    }
}
