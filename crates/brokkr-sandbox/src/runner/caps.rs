//! Capability drop and `PR_SET_NO_NEW_PRIVS` for the runner (M7).
//!
//! See `docs/phase-2-plan.md` §5.7. The runner drops every capability
//! not explicitly retained via [`crate::SandboxConfig::retained_caps`]
//! and sets `no_new_privs` so subsequent `execve` cannot regain
//! privileges via setuid binaries or file capabilities.
//!
//! This file is a scaffold; the real capability manipulation lands in
//! M7-C.

#[allow(dead_code)] // wired up in M7-C
pub(super) fn drop_all_except(_retained: &[String]) -> std::io::Result<()> {
    // TODO(M7-C): implement capability drop + PR_SET_NO_NEW_PRIVS.
    Ok(())
}
