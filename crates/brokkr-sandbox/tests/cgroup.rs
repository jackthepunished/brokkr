//! M6 evil-action tests: cgroups v2 + wall-clock timeout.
//!
//! Plan §8.1 / §5.5 maps:
//! - **EV-04** allocate too much memory → OOM-killed → `OutOfMemory`.
//! - **EV-05** fork bomb under `pids.max` → bomb dies, action exits
//!   non-zero quickly.
//! - **AC-02-ish** wall-clock timeout fires after the configured
//!   number of seconds and reports `Timeout`. (Doesn't need a cgroup,
//!   so this one always runs.)
//! - **Accounting** populated for a normal action.
//!
//! ### Skip policy
//!
//! The cgroup tests need a *writable* cgroup-v2 slice with the `cpu`,
//! `memory`, and `pids` controllers enabled in the parent's
//! `cgroup.subtree_control`. We discover one via:
//!
//! 1. `BROKKR_TEST_CGROUP_ROOT` env var.
//! 2. `/sys/fs/cgroup/brokkr.slice/` if writable (matches what
//!    `scripts/install-cgroup-slice.sh` provisions).
//!
//! On hosts without either (CI runners, fresh WSL2 boxes), the cgroup
//! tests print a `skip:` line and pass. The wall-clock timeout test
//! does not need a cgroup and always runs.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use brokkr_sandbox::{ExitStatus, ResourceLimits, Sandbox, SandboxConfig};

fn runner_path() -> &'static str {
    env!("CARGO_BIN_EXE_brokkr-sandboxd")
}

/// Find a writable cgroup root usable as the action-cgroup parent, or
/// return None so the caller can skip.
fn writable_cgroup_root() -> Option<PathBuf> {
    let candidates = std::env::var_os("BROKKR_TEST_CGROUP_ROOT")
        .map(PathBuf::from)
        .into_iter()
        .chain(std::iter::once(PathBuf::from(
            "/sys/fs/cgroup/brokkr.slice",
        )));
    candidates.into_iter().find(|cand| probe_usable(cand))
}

/// A cgroup root is "usable" when we can mkdir a unique leaf under it
/// AND attach the current process into it. The second check matters
/// because cgroup-v2 cross-delegation moves require write on the
/// common ancestor — a slice we can mkdir into can still reject our
/// `cgroup.procs` write if our source cgroup is in a different
/// delegation tree.
///
/// Returns `true` only if both probes succeed; cleans up the leaf in
/// either case.
fn probe_usable(parent: &Path) -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let probe = parent.join(format!("brokkr-probe-{}-{}", std::process::id(), unique));

    if std::fs::create_dir(&probe).is_err() {
        return false;
    }
    let mover_ok =
        std::fs::write(probe.join("cgroup.procs"), std::process::id().to_string()).is_ok();
    // Drop the leaf. If the move succeeded, our own pid is inside —
    // rmdir would EBUSY. Move ourselves back to the parent first.
    if mover_ok {
        let _ = std::fs::write(parent.join("cgroup.procs"), std::process::id().to_string());
    }
    let _ = std::fs::remove_dir(&probe);
    mover_ok
}

macro_rules! skip_unless_cgroup {
    ($root:ident) => {
        let Some($root) = writable_cgroup_root() else {
            eprintln!("skip: no writable cgroup root (set BROKKR_TEST_CGROUP_ROOT or run scripts/install-cgroup-slice.sh)");
            return;
        };
    };
}

#[tokio::test]
async fn wall_clock_timeout_kills_long_action() {
    let sandbox = Sandbox::new(runner_path());
    let cfg = SandboxConfig {
        argv: vec!["/usr/bin/sleep".into(), "60".into()],
        limits: ResourceLimits {
            wall_clock_secs: Some(2),
            ..Default::default()
        },
        ..Default::default()
    };
    let started = Instant::now();
    let outcome = sandbox.run(cfg).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Timeout,
        "stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert!(
        elapsed >= Duration::from_secs(2),
        "timeout fired before deadline: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "timeout took too long: {elapsed:?}"
    );
}

#[tokio::test]
async fn fork_bomb_capped_by_pids_max() {
    skip_unless_cgroup!(root);
    let sandbox = Sandbox::new(runner_path()).with_cgroup_root(root);
    // Python fork loop that exits the moment `fork()` returns EAGAIN.
    // Children sleep so they keep counting toward the cap; the parent
    // alone hitting the cap still produces a deterministic non-zero
    // exit. A bash fork bomb retries forever and would just wall-clock.
    let cfg = SandboxConfig {
        argv: vec![
            "/usr/bin/python3".into(),
            "-c".into(),
            "import os, time, sys\n\
             try:\n\
             \x20   while True:\n\
             \x20       if os.fork() == 0:\n\
             \x20           time.sleep(30)\n\
             \x20           os._exit(0)\n\
             except OSError:\n\
             \x20   sys.exit(42)\n"
                .into(),
        ],
        limits: ResourceLimits {
            pids_max: Some(16),
            wall_clock_secs: Some(15),
            ..Default::default()
        },
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::Exited(42),
        "expected exit=42 from EAGAIN handler; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

#[tokio::test]
async fn memory_max_triggers_oom_status() {
    skip_unless_cgroup!(root);
    let sandbox = Sandbox::new(runner_path()).with_cgroup_root(root);
    // Allocate a 256 MiB bytearray under a 64 MiB cap. The kernel
    // OOM-killer fires as soon as the rss exceeds memory.max.
    let cfg = SandboxConfig {
        argv: vec![
            "/usr/bin/python3".into(),
            "-c".into(),
            "bytearray(256 * 1024 * 1024)".into(),
        ],
        limits: ResourceLimits {
            memory_bytes: Some(64 * 1024 * 1024),
            wall_clock_secs: Some(15),
            ..Default::default()
        },
        ..Default::default()
    };
    let outcome = sandbox.run(cfg).await.unwrap();
    assert_eq!(
        outcome.exit_status,
        ExitStatus::OutOfMemory,
        "expected OutOfMemory; stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

#[tokio::test]
async fn accounting_is_populated_for_a_normal_action() {
    skip_unless_cgroup!(root);
    let sandbox = Sandbox::new(runner_path()).with_cgroup_root(root);
    // Burn ~50ms of CPU and allocate a few MiB so the accounting
    // counters land above zero deterministically.
    let cfg = SandboxConfig {
        argv: vec![
            "/usr/bin/python3".into(),
            "-c".into(),
            "x = bytearray(8 * 1024 * 1024)\n\
             s = 0\n\
             for i in range(200_000):\n\
             \x20   s += i\n\
             print(s)\n"
                .into(),
        ],
        limits: ResourceLimits {
            wall_clock_secs: Some(15),
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
    let acc = &outcome.accounting;
    assert!(
        acc.cpu_user + acc.cpu_system >= Duration::from_millis(1),
        "cpu accounting empty: {acc:?}"
    );
    assert!(
        acc.memory_peak_bytes >= 1024 * 1024,
        "memory.peak too low: {acc:?}"
    );
    assert!(acc.max_pids >= 1, "pids.peak too low: {acc:?}");
}
