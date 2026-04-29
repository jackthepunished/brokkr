//! Smoke tests for `brokkr-sandbox`'s M2 re-exec model.
//!
//! These exercise the full host → runner → exec path: the host spawns
//! `brokkr-sandboxd`, hands it a [`SandboxConfig`] over fd 3, and waits.
//! At M2 there's no namespace setup, so the action runs as a plain child
//! process — Phase-1 parity inside the new structure.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use brokkr_sandbox::{ExitStatus, Sandbox, SandboxConfig};

fn runner_path() -> &'static str {
    env!("CARGO_BIN_EXE_brokkr-sandboxd")
}

#[tokio::test]
async fn echo_hello_runs() {
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig::new(vec!["/bin/echo".to_string(), "hello world".to_string()]);
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(outcome.exit_status, ExitStatus::Exited(0));
    assert_eq!(outcome.stdout.as_ref(), b"hello world\n");
    assert!(outcome.stderr.is_empty());
}

#[tokio::test]
async fn false_exits_one() {
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig::new(vec!["/bin/false".to_string()]);
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(outcome.exit_status, ExitStatus::Exited(1));
}

#[tokio::test]
async fn nonexistent_argv0_returns_127() {
    // execvpe fails, the runner writes a diagnostic to stderr and _exit(127).
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig::new(vec!["/this/path/does/not/exist".to_string()]);
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(outcome.exit_status, ExitStatus::Exited(127));
    let stderr = String::from_utf8_lossy(&outcome.stderr);
    assert!(
        stderr.contains("execvpe failed") || stderr.contains("/this/path/does/not/exist"),
        "expected diagnostic; got: {stderr}"
    );
}

#[tokio::test]
async fn empty_argv_is_a_setup_error() {
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig::new(vec![]);
    let err = sandbox.run(cfg).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("argv"), "got: {msg}");
}

#[tokio::test]
async fn env_is_passed_through() {
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: vec![
            "/bin/sh".to_string(),
            "-c".into(),
            "echo $BROKKR_TEST".into(),
        ],
        env: vec![("BROKKR_TEST".to_string(), "42".to_string())],
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(outcome.exit_status, ExitStatus::Exited(0));
    assert_eq!(outcome.stdout.as_ref(), b"42\n");
}

#[tokio::test]
async fn workdir_is_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: vec!["/bin/pwd".to_string()],
        workdir: Some(dir.path().to_path_buf()),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(outcome.exit_status, ExitStatus::Exited(0));
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    assert_eq!(
        stdout.trim(),
        // pwd may resolve symlinks (e.g. /tmp → /private/tmp on some hosts);
        // we canonicalize to compare against what the kernel reports.
        dir.path().canonicalize().unwrap().to_string_lossy()
    );
}

#[tokio::test]
async fn timings_are_populated() {
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig::new(vec!["/bin/true".to_string()]);
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(outcome.exit_status, ExitStatus::Exited(0));
    // Setup and execution should both be > 0; teardown might round to 0 on
    // very fast paths, so just assert it's non-negative (Duration always is).
    assert!(
        outcome.timings.setup.as_nanos() > 0,
        "setup duration should be > 0"
    );
    assert!(
        outcome.timings.execution.as_nanos() > 0,
        "execution duration should be > 0"
    );
}
