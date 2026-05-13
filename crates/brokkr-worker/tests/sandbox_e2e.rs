//! Phase 2 / M9 worker-sandbox integration test.
//!
//! Exercises [`brokkr_worker::runner::Runner::Sandboxed`] end-to-end:
//! a fake REAPI `Command` is fed through the worker's runner
//! abstraction, which spawns `brokkr-sandboxd`, sets up namespaces,
//! and runs the action. We assert exit code, stdout, and that the
//! sandbox isolation actually fired (hostname inside is the
//! configured value, not the host's).
//!
//! ### Skip policy
//!
//! Same as the M3+ evil-action tests: hosts without unprivileged
//! user namespaces (or with AppArmor's userns restriction enabled)
//! skip cleanly. The Phase 1 in-process integration tests don't
//! depend on this — they use `Runner::Plain`.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::path::PathBuf;

use brokkr_proto::reapi_v2 as rapi;
use brokkr_sandbox::Sandbox;
use brokkr_worker::runner::{run_command, RunOutcome};
use brokkr_worker::{Runner, SandboxRunner, SandboxTemplate};

/// Locate the `brokkr-sandboxd` binary alongside the test binary.
/// `CARGO_BIN_EXE_brokkr-sandboxd` is only set for tests in the
/// crate that owns the `[[bin]]` target, so we resolve it via
/// `current_exe`: integration tests live in `target/<profile>/deps/`,
/// and the runner binary is its sibling at `target/<profile>/`.
fn runner_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let target_profile = exe.parent().unwrap().parent().unwrap();
    let runner = target_profile.join("brokkr-sandboxd");
    assert!(
        runner.is_file(),
        "brokkr-sandboxd not built at {runner:?} — \
         run `cargo build -p brokkr-sandbox --bin brokkr-sandboxd` first"
    );
    runner
}

fn unsupported_reason() -> Option<String> {
    if let Ok(s) = std::fs::read_to_string("/proc/sys/user/max_user_namespaces") {
        if s.trim().parse::<u64>().unwrap_or(0) == 0 {
            return Some("user.max_user_namespaces = 0".into());
        }
    } else {
        return Some("/proc/sys/user/max_user_namespaces missing".into());
    }
    if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
        if s.trim() != "1" {
            return Some(format!("unprivileged_userns_clone = {}", s.trim()));
        }
    }
    if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
    {
        if s.trim() == "1" {
            return Some("apparmor_restrict_unprivileged_userns = 1".into());
        }
    }
    None
}

macro_rules! skip_if_unsupported {
    () => {
        if let Some(reason) = unsupported_reason() {
            eprintln!("skip: {reason}");
            return;
        }
    };
}

fn sandboxed_runner() -> Runner {
    let sandbox = Sandbox::new(runner_binary());
    Runner::Sandboxed(Box::new(SandboxRunner {
        sandbox,
        template: SandboxTemplate::brokkr_default(),
    }))
}

#[tokio::test]
async fn echo_hello_world_runs_inside_sandbox() {
    skip_if_unsupported!();
    let runner = sandboxed_runner();
    let command = rapi::Command {
        arguments: vec!["/bin/echo".into(), "hello world".into()],
        ..Default::default()
    };
    let RunOutcome {
        exit_code,
        stdout,
        stderr,
    } = run_command(&runner, &command).await.unwrap();
    assert_eq!(exit_code, 0, "stderr={}", String::from_utf8_lossy(&stderr));
    assert_eq!(stdout.as_ref(), b"hello world\n");
}

#[tokio::test]
async fn hostname_inside_sandbox_is_brokkr_sandbox() {
    // Confirms the worker's brokkr_default template propagates
    // DeterminismPolicy::brokkr_defaults() into the runner, which
    // calls sethostname inside the new UTS namespace.
    skip_if_unsupported!();
    let runner = sandboxed_runner();
    let command = rapi::Command {
        arguments: vec!["/bin/hostname".into()],
        ..Default::default()
    };
    let outcome = run_command(&runner, &command).await.unwrap();
    assert_eq!(
        outcome.exit_code,
        0,
        "stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&outcome.stdout).trim(),
        "brokkr-sandbox",
    );
}

#[tokio::test]
async fn host_etc_shadow_is_not_visible_inside_sandbox() {
    // Smoke: the worker's default rootfs really does isolate the
    // host filesystem. If `/etc/shadow` were leaking, `cat` would
    // succeed and print the host's password hashes — the worker is
    // misconfigured.
    skip_if_unsupported!();
    let runner = sandboxed_runner();
    let command = rapi::Command {
        arguments: vec!["/bin/cat".into(), "/etc/shadow".into()],
        ..Default::default()
    };
    let outcome = run_command(&runner, &command).await.unwrap();
    assert_ne!(
        outcome.exit_code, 0,
        "the sandbox let /etc/shadow through — bind allowlist is too wide"
    );
}

#[tokio::test]
async fn plain_runner_remains_phase1_compatible() {
    // Phase 1 fallback path is unaffected: same RunOutcome shape, no
    // sandbox dependency. Useful for in-process integration tests
    // (boot_cluster fixture) and for hosts where the sandbox can't
    // run.
    let command = rapi::Command {
        arguments: vec!["/bin/echo".into(), "phase1".into()],
        ..Default::default()
    };
    let outcome = run_command(&Runner::Plain, &command).await.unwrap();
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout.as_ref(), b"phase1\n");
}
