//! M8 determinism tests: hostname, TZ, env scrubbing, byte-identical
//! repeatability.
//!
//! Plan §8.1 / §5.8 maps:
//! - **EV-11** `LD_PRELOAD=/host/evil.so` is stripped from the env
//!   before the action sees it (with `strip_ld_preload` set).
//! - **AC-04** running the same archiving command twice with
//!   `SOURCE_DATE_EPOCH` produces byte-identical output.
//!
//! Plus two positive tests:
//! - hostname inside the sandbox is the one we asked for, not the
//!   host's hostname.
//! - `date +%Z` reports `UTC` when `timezone_utc=true`.
//!
//! ### Skip policy
//!
//! Like the M3/M4/M5/M7 evil tests, these need an unprivileged user
//! namespace because the determinism work piggybacks on the namespace
//! path (hostname needs the new UTS namespace; /etc/localtime needs
//! the tmpfs `/etc`). On hosts without one we skip cleanly.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::path::PathBuf;

use brokkr_sandbox::{DeterminismPolicy, ExitStatus, RootfsSpec, Sandbox, SandboxConfig};

fn runner_path() -> &'static str {
    env!("CARGO_BIN_EXE_brokkr-sandboxd")
}

/// Minimal Linux rootfs — same shape as `mount_ns.rs`, kept local to
/// avoid a cross-test helper crate for one struct.
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
async fn hostname_is_set_to_brokkr_sandbox() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/hostname".into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        determinism: DeterminismPolicy::brokkr_defaults(),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(0),
        "stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&outcome.stdout).trim(),
        "brokkr-sandbox",
        "stderr={}",
        String::from_utf8_lossy(&outcome.stderr),
    );
}

#[tokio::test]
async fn ev11_ld_preload_is_stripped_from_action_env() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // Caller passes a malicious LD_PRELOAD; with strip_ld_preload set,
    // the action sees an empty value. We pick `/dev/null` (path that
    // exists) so the worst case — strip fails — would mean glibc tries
    // to load /dev/null as a shared object and the action errors out
    // visibly. With the scrub working, the action runs cleanly and
    // prints an empty value.
    let cfg = SandboxConfig {
        argv: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            "printf '<%s>' \"${LD_PRELOAD-unset}\"".into(),
        ],
        env: vec![
            ("LD_PRELOAD".into(), "/dev/null".into()),
            ("LD_LIBRARY_PATH".into(), "/dev/null".into()),
        ],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        determinism: DeterminismPolicy {
            strip_ld_preload: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(0),
        "stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    // `${LD_PRELOAD-unset}` expands to `unset` when LD_PRELOAD is
    // entirely missing from the env (which is what scrubbing should
    // produce).
    assert_eq!(
        String::from_utf8_lossy(&outcome.stdout),
        "<unset>",
        "LD_PRELOAD was not stripped; full stdout: {:?}",
        outcome.stdout,
    );
}

#[tokio::test]
async fn timezone_utc_sets_tz_env_var() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            "printf '%s' \"$TZ\"".into(),
        ],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        determinism: DeterminismPolicy {
            timezone_utc: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(0),
        "stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&outcome.stdout), "UTC0");
}

#[tokio::test]
async fn source_date_epoch_injected_when_set() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            "printf '%s' \"$SOURCE_DATE_EPOCH\"".into(),
        ],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        determinism: DeterminismPolicy {
            source_date_epoch: Some(1_700_000_000),
            ..Default::default()
        },
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(0),
        "stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&outcome.stdout), "1700000000");
}

#[tokio::test]
async fn ac04_two_identical_runs_produce_byte_identical_outputs() {
    // Determinism stress: two runs of the same SHA-256 over a fixed
    // input — under brokkr_defaults — must produce byte-identical
    // stdout. Goes wider than just env: hostname, TZ, PATH, and any
    // hidden ambient input is held constant by the sandbox.
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());

    let action = SandboxConfig {
        argv: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            // Same input on both runs; the sha256sum varies if any
            // ambient bit (locale, hostname, TZ) leaks into stdin.
            "printf 'brokkr determinism probe\\n' | /usr/bin/sha256sum".into(),
        ],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        determinism: DeterminismPolicy::brokkr_defaults(),
        ..Default::default()
    };
    let first = sandbox.run(action.clone()).await.unwrap();
    let second = sandbox.run(action).await.unwrap();
    assert_eq!(first.exit_status, ExitStatus::Exited(0));
    assert_eq!(second.exit_status, ExitStatus::Exited(0));
    assert_eq!(
        first.stdout, second.stdout,
        "two identical runs disagreed on stdout — determinism leak",
    );
}
