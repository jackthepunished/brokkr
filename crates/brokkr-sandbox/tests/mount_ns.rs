//! M3 evil-action tests: mount namespace + pivot_root + bind allowlist.
//!
//! Plan §8.1 maps:
//! - **EV-01** `cat /etc/shadow` → no such file or directory.
//! - **EV-15** recursive bind-mount escape attempt → host's mountinfo
//!   unchanged after the action runs.
//!
//! Plus a positive `ls /` test that asserts the sandbox root contains
//! exactly the entries we put there.
//!
//! Skip-on-not-supported: unprivileged user namespaces are required.
//! The probe runs `unshare(1)` to actually attempt the namespace
//! creation rather than guessing from sysctls — that catches AppArmor
//! `restrict_unprivileged_userns` (Ubuntu 24.04+), SELinux denials,
//! container restrictions, and the legacy
//! `/proc/sys/kernel/unprivileged_userns_clone = 0` knob in one shot.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::path::PathBuf;

use brokkr_sandbox::{ExitStatus, RootfsSpec, Sandbox, SandboxConfig};

fn runner_path() -> &'static str {
    env!("CARGO_BIN_EXE_brokkr-sandboxd")
}

/// Build a "minimal Linux rootfs" suitable for the M3 evil tests. Binds
/// `/usr` and (when present) `/lib64` and `/lib`, lays down `/etc`,
/// `/tmp`, `/work` as tmpfs, and creates `/bin /sbin /lib /lib64`
/// symlinks pointing into the bound `/usr` layout for usrmerge systems.
fn minimal_linux_rootfs() -> RootfsSpec {
    let mut ro_binds = vec![(PathBuf::from("/usr"), PathBuf::from("/usr"))];
    for p in ["/lib64", "/lib"] {
        let path = PathBuf::from(p);
        // Bind only when the host path is a real directory; on usrmerge
        // systems these are symlinks and we'll just create symlinks in
        // the tmpfs rootfs instead.
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

/// Probe whether unprivileged user namespaces work on this host. If
/// they don't, return Some(reason) so the caller can skip the test
/// cleanly.
///
/// Strategy: cheap static gates first (clear diagnostic strings), then
/// an authoritative runtime probe via `unshare(1)` from util-linux —
/// catches AppArmor / SELinux / container restrictions that the
/// sysctls miss.
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

    // Authoritative runtime probe.
    match std::process::Command::new("unshare")
        .args(["--user", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(o) if o.status.success() => None,
        Ok(o) => Some(format!(
            "unshare --user true failed ({}): {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Some(format!("unshare(1) probe could not run: {e}")),
    }
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
async fn ev01_cat_etc_shadow_fails() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/cat".to_string(), "/etc/shadow".into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = match sandbox.run(cfg).await {
        Ok(o) => o,
        Err(e) => panic!("sandbox.run failed: {e:#}"),
    };
    assert_ne!(
        outcome.exit_status,
        ExitStatus::Exited(0),
        "/etc/shadow should not be readable inside the sandbox"
    );
    let stderr = String::from_utf8_lossy(&outcome.stderr);
    assert!(
        stderr.contains("No such file") || stderr.contains("cannot open"),
        "expected no-such-file diagnostic; got: {stderr}"
    );
}

#[tokio::test]
async fn ls_root_shows_only_expected_entries() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/ls".to_string(), "-A".into(), "/".into()],
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
    let entries: Vec<&str> = stdout.lines().collect();

    // Every entry must be one of the things we deliberately mounted /
    // symlinked into the sandbox.
    let allowed: &[&str] = &["usr", "lib", "lib64", "sbin", "bin", "etc", "tmp", "work"];
    for entry in &entries {
        assert!(
            allowed.contains(entry),
            "unexpected entry {entry:?} in sandbox root; full listing: {entries:?}"
        );
    }

    // Sanity-check that some host paths definitely DON'T leak.
    for forbidden in ["home", "root", "var", "boot", "proc", "sys"] {
        assert!(
            !entries.contains(&forbidden),
            "host path {forbidden:?} leaked into sandbox; listing: {entries:?}"
        );
    }
}

#[tokio::test]
async fn ev15_host_mountinfo_unchanged_after_sandbox() {
    skip_if_unsupported!();
    let snapshot_before = std::fs::read_to_string("/proc/self/mountinfo").unwrap();

    let sandbox = Sandbox::new(runner_path());
    // The action mounts a tmpfs over /work inside the sandbox, then exits.
    // With MS_REC|MS_PRIVATE on / and a fresh mount namespace, this must
    // not be observable from the host's mountinfo.
    let cfg = SandboxConfig {
        argv: vec![
            "/usr/bin/sh".to_string(),
            "-c".into(),
            "/usr/bin/mount -t tmpfs none /work 2>&1; exit 0".into(),
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

    let snapshot_after = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert_eq!(
        snapshot_before, snapshot_after,
        "host mountinfo changed across sandbox run — propagation leaked",
    );
}
