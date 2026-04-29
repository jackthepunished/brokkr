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
    // Use a stable absolute path that exists on every Linux host. We
    // deliberately avoid tempfile + canonicalize here so the test surface
    // is just `chdir(workdir); pwd`.
    let workdir = std::path::PathBuf::from("/tmp");
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: vec!["/bin/sh".to_string(), "-c".into(), "pwd".into()],
        workdir: Some(workdir.clone()),
        ..Default::default()
    };
    let outcome = match sandbox.run(cfg).await {
        Ok(o) => o,
        Err(e) => panic!("sandbox.run failed: {e:#}"),
    };
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(0),
        "exit={:?} stderr={}",
        outcome.exit_status,
        String::from_utf8_lossy(&outcome.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&outcome.stdout).trim(),
        workdir.to_string_lossy(),
    );
}

#[tokio::test]
async fn timings_are_populated() {
    // Sleep briefly so even on fast machines with coarse timer resolution
    // the *aggregate* runtime is comfortably above zero. We don't assert
    // any individual phase is > 0 — `setup`, `execution`, or `teardown`
    // can legitimately round to zero on a hot path.
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "sleep 0.01".to_string(),
    ]);
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(outcome.exit_status, ExitStatus::Exited(0));
    let total = outcome.timings.setup + outcome.timings.execution + outcome.timings.teardown;
    assert!(
        total.as_millis() >= 5,
        "expected aggregate timing ≥ 5 ms, got {total:?}"
    );
}
