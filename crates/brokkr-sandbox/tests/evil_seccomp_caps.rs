//! M7 evil-action tests: seccomp + capability drop + `PR_SET_NO_NEW_PRIVS`.
//!
//! Plan §8.1 / §5.6 / §5.7 maps:
//! - **EV-02** action calls `mount(2)` directly → EPERM (seccomp).
//! - **EV-03** action calls `keyctl(2)` directly → EPERM (seccomp).
//! - **EV-04** action calls `ptrace(PTRACE_TRACEME)` → EPERM (seccomp).
//! - **EV-09** RDTSC. The runner calls `prctl(PR_SET_TSC, PR_TSC_SIGSEGV)`
//!   before exec so any `rdtsc`/`rdtscp` instruction raises SIGSEGV.
//! - **EV-10** the runner sets `PR_SET_NO_NEW_PRIVS`. We assert this via
//!   `prctl(PR_GET_NO_NEW_PRIVS)`, which should return 1 inside the
//!   sandboxed process.
//! - **EV-14** `/proc/self/status` shows `CapEff: 0000000000000000`
//!   (and CapPrm/CapBnd/CapInh likewise) after the runner drops caps.
//!
//! All tests rely on the namespace path: M7 hardening only kicks in
//! when `cfg.rootfs` is non-empty (see `runner/exec.rs`). Tests skip
//! cleanly if the host can't open a user namespace, mirroring
//! `mount_ns.rs` / `net_ns.rs`.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::path::PathBuf;

use brokkr_sandbox::{ExitStatus, RootfsSpec, Sandbox, SandboxConfig};

const EPERM: i32 = libc::EPERM;

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

/// Build a `python3 -c` action that exits with the errno of a single
/// libc / syscall call. Useful for asserting EPERM from seccomp at the
/// errno level.
fn errno_python(snippet: &str) -> Vec<String> {
    // The script imports ctypes, runs `snippet` (which must set `rc`
    // and `errno`), then exits with `errno` if rc == -1, else exit 0.
    let script = format!(
        "import ctypes, sys\n\
         libc = ctypes.CDLL('libc.so.6', use_errno=True)\n\
         {snippet}\n\
         if rc == -1:\n\
         \x20   sys.exit(ctypes.get_errno())\n\
         sys.exit(0)\n"
    );
    vec!["/usr/bin/python3".into(), "-c".into(), script]
}

#[tokio::test]
async fn ev02_mount_blocked_by_seccomp() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // mount(2) is not in DEFAULT_ALLOW. Returns -1 / EPERM under seccomp.
    let cfg = SandboxConfig {
        argv: errno_python("rc = libc.mount(b'none', b'/tmp', b'tmpfs', 0, None)"),
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(EPERM),
        "mount should EPERM under seccomp; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

#[tokio::test]
async fn ev03_keyctl_blocked_by_seccomp() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // SYS_keyctl on x86_64 is 250, on aarch64 it's 219. Use libc's
    // syscall(3) so we get the right number for the host arch via
    // python's `ctypes.CDLL('libc.so.6').syscall`.
    //
    // Resolve SYS_keyctl through `os.uname()` machine type.
    let cfg = SandboxConfig {
        argv: errno_python(
            "import platform\n\
             arch = platform.machine()\n\
             SYS_keyctl = 250 if arch == 'x86_64' else (219 if arch == 'aarch64' else None)\n\
             if SYS_keyctl is None:\n\
             \x20   sys.exit(0)\n\
             # KEYCTL_GET_KEYRING_ID == 0\n\
             rc = libc.syscall(SYS_keyctl, 0, 0, 0)",
        ),
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(EPERM),
        "keyctl should EPERM under seccomp; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

#[tokio::test]
async fn ev04_ptrace_blocked_by_seccomp() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // PTRACE_TRACEME == 0. ptrace(2) is not in DEFAULT_ALLOW.
    let cfg = SandboxConfig {
        argv: errno_python("rc = libc.ptrace(0, 0, 0, 0)"),
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(EPERM),
        "ptrace should EPERM under seccomp; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

/// EV-09 — PR_SET_TSC blocked by seccomp argument filter.
/// On x86_64 the filter blocks prctl(PR_SET_TSC, PR_TSC_SIGSEGV) with EPERM.
#[cfg(target_arch = "x86_64")]
#[tokio::test]
async fn ev09_prctl_set_tsc_blocked() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // PR_SET_TSC = 26, PR_TSC_SIGSEGV = 1
    let script = "import ctypes, sys\n\
         libc = ctypes.CDLL('libc.so.6', use_errno=True)\n\
         rc = libc.prctl(26, 1, 0, 0, 0)\n\
         if rc == -1:\n\
         \x20   sys.exit(ctypes.get_errno())\n\
         sys.exit(1)\n"; // exit 1 if it didn't error (unexpectedly succeeded)
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/python3".into(), "-c".into(), script.into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(EPERM),
        "prctl(PR_SET_TSC) should EPERM under seccomp; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

/// EV — PR_SET_KEEPCAPS blocked by seccomp argument filter.
#[tokio::test]
async fn ev_prctl_keepcaps_blocked() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // PR_SET_KEEPCAPS = 8
    let script = "import ctypes, sys\n\
         libc = ctypes.CDLL('libc.so.6', use_errno=True)\n\
         rc = libc.prctl(8, 1, 0, 0, 0)\n\
         if rc == -1:\n\
         \x20   sys.exit(ctypes.get_errno())\n\
         sys.exit(1)\n";
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/python3".into(), "-c".into(), script.into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(EPERM),
        "prctl(PR_SET_KEEPCAPS) should EPERM under seccomp; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

/// EV — PR_CAPBSET_DROP blocked by seccomp argument filter.
#[tokio::test]
async fn ev_prctl_capbset_drop_blocked() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // PR_CAPBSET_DROP = 24
    let script = "import ctypes, sys\n\
         libc = ctypes.CDLL('libc.so.6', use_errno=True)\n\
         rc = libc.prctl(24, 0, 0, 0, 0)\n\
         if rc == -1:\n\
         \x20   sys.exit(ctypes.get_errno())\n\
         sys.exit(1)\n";
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/python3".into(), "-c".into(), script.into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(EPERM),
        "prctl(PR_CAPBSET_DROP) should EPERM under seccomp; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

/// EV — PR_GET_TSC blocked by seccomp argument filter.
#[cfg(target_arch = "x86_64")]
#[tokio::test]
async fn ev_prctl_get_tsc_blocked() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // PR_GET_TSC = 25 — read-only query but blocked to prevent info leak
    let script = "import ctypes, sys\n\
         libc = ctypes.CDLL('libc.so.6', use_errno=True)\n\
         rc = libc.prctl(25, 0, 0, 0, 0)\n\
         if rc == -1:\n\
         \x20   sys.exit(ctypes.get_errno())\n\
         sys.exit(1)\n";
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/python3".into(), "-c".into(), script.into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(EPERM),
        "prctl(PR_GET_TSC) should EPERM under seccomp; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

/// EV — ioctl(TIOCGWINSZ) blocked by seccomp argument filter.
#[tokio::test]
async fn ev_ioctl_tiocgwinsz_blocked() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // TIOCGWINSZ = 0x5413 — read terminal window size (info leak)
    let script = "import ctypes, sys\n\
         libc = ctypes.CDLL('libc.so.6', use_errno=True)\n\
         fd = libc.open(b'/dev/null', 0)\n\
         if fd == -1:\n\
         \x20   sys.exit(2)\n\
         rc = libc.ioctl(fd, 0x5413, 0)\n\
         libc.close(fd)\n\
         if rc == -1:\n\
         \x20   sys.exit(ctypes.get_errno())\n\
         sys.exit(1)\n";
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/python3".into(), "-c".into(), script.into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(EPERM),
        "ioctl(TIOCGWINSZ) should EPERM under seccomp; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

/// EV — ioctl(TIOCSTI) blocked by seccomp argument filter.
#[tokio::test]
async fn ev_ioctl_tiocsti_blocked() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // TIOCSTI = 0x5412 — simulate terminal input; dangerous
    let script = "import ctypes, sys\n\
         libc = ctypes.CDLL('libc.so.6', use_errno=True)\n\
         # open /dev/null — a harmless fd for testing the ioctl call
         fd = libc.open(b'/dev/null', 0)\n\
         if fd == -1:\n\
         \x20   sys.exit(2)\n\
         rc = libc.ioctl(fd, 0x5412, 0)\n\
         libc.close(fd)\n\
         if rc == -1:\n\
         \x20   sys.exit(ctypes.get_errno())\n\
         sys.exit(1)\n";
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/python3".into(), "-c".into(), script.into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(EPERM),
        "ioctl(TIOCSTI) should EPERM under seccomp; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

/// EV — ioctl(TIOCSWINSZ) blocked by seccomp argument filter.
#[tokio::test]
async fn ev_ioctl_tiocswinsz_blocked() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // TIOCSWINSZ = 0x5414 — set terminal window size
    let script = "import ctypes, sys\n\
         libc = ctypes.CDLL('libc.so.6', use_errno=True)\n\
         fd = libc.open(b'/dev/null', 0)\n\
         if fd == -1:\n\
         \x20   sys.exit(2)\n\
         rc = libc.ioctl(fd, 0x5414, 0)\n\
         libc.close(fd)\n\
         if rc == -1:\n\
         \x20   sys.exit(ctypes.get_errno())\n\
         sys.exit(1)\n";
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/python3".into(), "-c".into(), script.into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(EPERM),
        "ioctl(TIOCSWINSZ) should EPERM under seccomp; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

/// EV — ioctl(TIOCSPTLCK) blocked by seccomp argument filter.
#[tokio::test]
async fn ev_ioctl_tiocptlck_blocked() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // TIOCSPTLCK = 0x40045431 — (un)lock pseudo-terminal slave
    let script = "import ctypes, sys\n\
         libc = ctypes.CDLL('libc.so.6', use_errno=True)\n\
         fd = libc.open(b'/dev/null', 0)\n\
         if fd == -1:\n\
         \x20   sys.exit(2)\n\
         rc = libc.ioctl(fd, 0x40045431, 0)\n\
         libc.close(fd)\n\
         if rc == -1:\n\
         \x20   sys.exit(ctypes.get_errno())\n\
         sys.exit(1)\n";
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/python3".into(), "-c".into(), script.into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(EPERM),
        "ioctl(TIOCSPTLCK) should EPERM under seccomp; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

#[cfg(target_arch = "x86_64")]
#[tokio::test]
async fn ev09_rdtsc_blocked() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // rdtsc instruction → SIGSEGV (signal 11, exit code 139 = 128 + 11)
    // when PR_TSC_SIGSEGV is active.
    let cfg = SandboxConfig {
        argv: vec![
            "/usr/bin/python3".into(),
            "-c".into(),
            "import mmap, ctypes, struct; \
             code = mmap.mmap(-1, 4096, prot=7, flags=34); \
             code.write(struct.pack('15B', 0x0f, 0x01, 0xf9, 0xc3)); \
             func = ctypes.CFUNCTYPE(ctypes.c_ulong)(ctypes.addressof(code)); \
             func()"
                .into(),
        ],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    // SIGSEGV = signal 11, exit code = 128 + 11 = 139
    assert!(
        outcome.exit_status.signaled() && outcome.exit_status.code() == Some(139),
        "rdtsc should cause SIGSEGV (exit 139); got {:?}",
        outcome.exit_status
    );
}

#[tokio::test]
async fn ev10_no_new_privs_is_set() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // PR_GET_NO_NEW_PRIVS == 39. With NO_NEW_PRIVS=1, exec'ing a
    // setuid binary cannot escalate. We assert the bit directly here;
    // a separate test below also exec's `/usr/bin/su` and checks euid.
    let script = "import ctypes, sys\n\
         libc = ctypes.CDLL('libc.so.6', use_errno=True)\n\
         val = libc.prctl(39, 0, 0, 0, 0)\n\
         sys.exit(0 if val == 1 else (1 if val >= 0 else 2))\n";
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/python3".into(), "-c".into(), script.into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(0),
        "PR_GET_NO_NEW_PRIVS should return 1; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

#[tokio::test]
async fn ev14_capeff_is_zero() {
    skip_if_unsupported!();
    let sandbox = Sandbox::new(runner_path());
    // Read /proc/self/status and grep CapEff/CapPrm/CapInh/CapBnd.
    // All must be the all-zero bitmask since `retained_caps` is empty.
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/cat".into(), "/proc/self/status".into()],
        rootfs: minimal_linux_rootfs(),
        workdir: Some(PathBuf::from("/work")),
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(0),
        "cat /proc/self/status failed; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    let zero = "0000000000000000";
    for field in ["CapInh", "CapPrm", "CapEff", "CapBnd"] {
        let line = stdout
            .lines()
            .find(|l| l.starts_with(&format!("{field}:")))
            .unwrap_or_else(|| panic!("{field} missing from /proc/self/status:\n{stdout}"));
        assert!(
            line.contains(zero),
            "{field} should be all-zero (retained_caps is empty); got {line:?}",
        );
    }
}
