//! M5 evil-action tests: network namespace + optional loopback.
//!
//! Plan §8.1 maps:
//! - **EV-08** action attempts to `connect()` to a public address
//!   (1.1.1.1:443) — must fail with `ENETUNREACH` regardless of
//!   policy because the netns has no default route.
//! - **EV-07-ish** with `NetworkPolicy::None`, even `127.0.0.1` is
//!   unreachable because `lo` is `DOWN`. With
//!   `NetworkPolicy::Loopback`, the same connect succeeds at the route
//!   layer and fails at TCP with `ECONNREFUSED` (no listener) — proving
//!   loopback was actually brought up.
//!
//! All three tests drive the action via `python3 -c` so we can read
//! `errno` directly through the action's exit code (Linux errno values
//! 101 ENETUNREACH, 111 ECONNREFUSED both fit in the 0–255 exit-code
//! window). Skip if `python3` isn't present on the host.
//!
//! Skip rules match `mount_ns.rs` — see that module's
//! `unsupported_reason`.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::path::PathBuf;

use brokkr_sandbox::{ExitStatus, NetworkPolicy, RootfsSpec, Sandbox, SandboxConfig};

const ENETUNREACH: i32 = libc::ENETUNREACH;
const ECONNREFUSED: i32 = libc::ECONNREFUSED;

fn runner_path() -> &'static str {
    env!("CARGO_BIN_EXE_brokkr-sandboxd")
}

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
    if !std::path::Path::new("/usr/bin/python3").exists() {
        return Some("/usr/bin/python3 missing".into());
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

/// Build an action that tries to connect a TCP socket to `addr:port`
/// and exits with the connect's errno (or 0 on unexpected success).
fn connect_action(addr: &str, port: u16) -> Vec<String> {
    let script = format!(
        "import socket, sys\n\
         s = socket.socket()\n\
         s.settimeout(1)\n\
         try:\n\
         \x20   s.connect(('{addr}', {port}))\n\
         \x20   sys.exit(0)\n\
         except OSError as e:\n\
         \x20   sys.exit(e.errno or 1)\n"
    );
    vec!["/usr/bin/python3".into(), "-c".into(), script]
}

#[tokio::test]
async fn ev08_public_address_unreachable() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: connect_action("1.1.1.1", 443),
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        // Default: NetworkPolicy::None — empty netns.
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(ENETUNREACH),
        "expected ENETUNREACH (101); stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

#[tokio::test]
async fn loopback_down_makes_127_0_0_1_unreachable() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: connect_action("127.0.0.1", 1),
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        network: NetworkPolicy::None,
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(ENETUNREACH),
        "with lo DOWN, 127.0.0.1 should be ENETUNREACH (101); stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

#[tokio::test]
async fn loopback_up_makes_127_0_0_1_route_but_not_connect() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: connect_action("127.0.0.1", 1),
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        network: NetworkPolicy::Loopback,
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(ECONNREFUSED),
        "with lo UP, 127.0.0.1:1 should be ECONNREFUSED (111); stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}
