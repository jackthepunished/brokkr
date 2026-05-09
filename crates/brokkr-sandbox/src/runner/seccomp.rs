//! Default-deny seccomp-bpf filter (M7).
//!
//! See `docs/phase-2-plan.md` §5.6. The filter is installed in the runner
//! after all setup syscalls have completed and immediately before
//! `execve`, so the action runs under the restricted policy.
//!
//! This file is a scaffold; the real allowlist + `seccompiler` wiring
//! lands in M7-B.

#[allow(dead_code)] // wired up in M7-B
pub(super) fn install(_extra_allow: &[String]) -> std::io::Result<()> {
    // TODO(M7-B): implement default-deny filter via seccompiler.
    Ok(())
}
