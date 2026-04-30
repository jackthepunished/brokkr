//! M4 evil-action tests: PID namespace + init reaper.
//!
//! Plan §8.1 maps:
//! - **AC-01** `cat /proc/1/comm` is the runner, not the host's init.
//! - **EV-16** action sees only its own pidns: `getpid()` = 2 (PID 1 is
//!   the runner-as-init), and only a small handful of numeric entries
//!   appear under `/proc`.
//! - **EV-13** orphaned child does not outlive the sandbox: a shell
//!   that backgrounds `sleep 60` and exits causes init to exit
//!   immediately; the kernel SIGKILLs the orphan as part of pidns
//!   teardown, so the whole sandbox returns in well under the orphan's
//!   sleep budget.
//!
//! Skip rules match `mount_ns.rs` — see that module's `unsupported_reason`.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use brokkr_sandbox::{ExitStatus, RootfsSpec, Sandbox, SandboxConfig};

fn runner_path() -> &'static str {
    env!("CARGO_BIN_EXE_brokkr-sandboxd")
}

/// Same minimal rootfs as the M3 tests use — keeping it duplicated is
/// fine for a 30-line helper, and avoids a shared-test-utils module
/// only two files would import.
fn minimal_linux_rootfs() -> RootfsSpec {
    let mut ro_binds = vec![(PathBuf::from("/usr"), PathBuf::from("/usr"))];
    for p in ["/lib64", "/lib"] {
        let path = PathBuf::from(p);
        if path.is_dir() && !path.is_symlink() {
            ro_binds.push((path.clone(), path));
        }
    }
    let symlinks = vec![
        (PathBuf::from("/bin"), PathBuf::from("usr/bin")),
        (PathBuf::from("/sbin"), PathBuf::from("usr/sbin")),
        (PathBuf::from("/lib"), PathBuf::from("usr/lib")),
        (PathBuf::from("/lib64"), PathBuf::from("usr/lib64")),
    ];
    RootfsSpec {
        ro_binds,
        tmpfs: vec![
            (PathBuf::from("/etc"), 4 * 1024 * 1024),
            (PathBuf::from("/tmp"), 16 * 1024 * 1024),
            (PathBuf::from("/work"), 16 * 1024 * 1024),
        ],
        symlinks,
        input_root: None,
    }
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

#[tokio::test]
async fn ac01_proc_pid1_is_the_runner() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/cat".into(), "/proc/1/comm".into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(0),
        "stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    let comm = String::from_utf8_lossy(&outcome.stdout);
    // TASK_COMM_LEN is 16 bytes (15 chars + NUL); "brokkr-sandboxd"
    // is exactly 15 chars and survives intact.
    assert_eq!(
        comm.trim(),
        "brokkr-sandboxd",
        "PID 1 should be the runner; got {comm:?}"
    );
}

#[tokio::test]
async fn ev16_action_sees_pid2_and_short_proc_listing() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // Print our own pid, then list numeric /proc entries.
    let cfg = SandboxConfig {
        argv: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            // $$ is the shell's pid (the action), then count digit-only
            // entries under /proc.
            "echo pid=$$; ls /proc | grep -E '^[0-9]+$' | sort -n".into(),
        ],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(0),
        "stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    let stdout = String::from_utf8_lossy(&outcome.stdout);

    // First line is "pid=<n>" — must be 2 (PID 1 is init).
    let pid_line = stdout.lines().next().unwrap_or_default();
    let pid: u32 = pid_line
        .strip_prefix("pid=")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("could not parse pid line: {pid_line:?}"));
    assert_eq!(
        pid, 2,
        "action should be PID 2 in its pidns; full stdout: {stdout}"
    );

    // The numeric /proc entries are PID 1 (init), PID 2 (the shell),
    // and possibly transient children spawned by the pipeline (ls,
    // grep, sort) — all single- or low-double-digit numbers. Anything
    // >= 100 would mean we're seeing the host pidns.
    let pids: Vec<u32> = stdout
        .lines()
        .skip(1)
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert!(
        !pids.is_empty(),
        "no numeric /proc entries; stdout: {stdout}"
    );
    for p in &pids {
        assert!(
            *p < 100,
            "unexpectedly large PID {p} in sandbox /proc — looks like host pidns leaked. stdout: {stdout}"
        );
    }
}

#[tokio::test]
async fn ev13_orphaned_child_does_not_outlive_sandbox() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // Action backgrounds a long sleep then exits. Init (PID 1) sees
    // the action exit, exits itself — the kernel then SIGKILLs every
    // remaining process in the pidns including the orphaned sleep.
    // The whole call must return in well under the orphan's sleep
    // budget; if the cleanup didn't work, this test would hang.
    let cfg = SandboxConfig {
        argv: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            "/usr/bin/sleep 60 & exit 0".into(),
        ],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };

    let started = Instant::now();
    let outcome = sandbox.run(cfg).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(0),
        "stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "sandbox took {elapsed:?} — orphaned sleep was not cleaned up"
    );
}
