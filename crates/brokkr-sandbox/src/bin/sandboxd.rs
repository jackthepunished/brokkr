//! `brokkr-sandboxd` — the Phase 2 sandbox runner binary.
//!
//! The host worker spawns this binary once per action with a
//! [`SandboxConfig`](brokkr_sandbox::SandboxConfig) on file descriptor 3.
//! The runner reads the config, sets up the sandbox, and `execve`s the
//! action.
//!
//! Phase 2 lights up the sandbox setup incrementally — see
//! `docs/phase-2-plan.md` §9. M2 ships this as a thin re-exec shim that
//! reads the config and executes the action with no isolation, mirroring
//! Phase 1 behaviour inside the new process model.

fn main() -> ! {
    brokkr_sandbox::run_as_runner()
}
