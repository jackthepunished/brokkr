//! Runner-side entry point — runs *inside* `brokkr-sandboxd`, after the
//! re-exec.
//!
//! Phase 2 / M2 lights up only the bare bones: read the
//! [`SandboxConfig`](crate::SandboxConfig) from file descriptor 3, optionally
//! `chdir` into [`SandboxConfig::workdir`], then `execvpe` the action.
//! Subsequent milestones add namespace setup, cgroup attachment, seccomp,
//! capability dropping, and determinism guards before the final `execve`.
//!
//! [`run_as_runner`] returns `!`: it always ends in either `execve` or
//! `_exit`. Errors before exec are written to stderr and exit with code
//! 127, matching the convention of `sh: command not found`.

#[cfg(target_os = "linux")]
mod exec;

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
