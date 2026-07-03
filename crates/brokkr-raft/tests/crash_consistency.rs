//! Real-process crash test for the Raft hard state (Phase 5 I2).
//!
//! The unit tests in `storage.rs` prove that an *uncommitted* write is invisible
//! after reopen. This integration test proves the other half of
//! persist-before-respond end to end: a hard state that `save_hard_state`
//! *committed* survives the process being killed the instant afterwards — no
//! torn write, no corruption.
//!
//! It works by re-executing the test binary as a child (selected by name +
//! env var). The child commits a hard state and then `std::process::abort()`s —
//! the moral equivalent of a power loss. The parent reopens the store and
//! asserts the committed state is intact.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::process::Command;

use brokkr_raft::{HardState, NodeId, RaftLog, Term};

/// When set, its value is the `raft.redb` path the crash child should write to.
const CRASH_ENV: &str = "BROKKR_RAFT_CRASH_DB_PATH";

fn committed_child_state() -> HardState {
    HardState {
        current_term: Term::new(7),
        voted_for: Some(NodeId::new("leader-x").expect("valid node id")),
    }
}

/// The crash child. In a normal `cargo test` run the env var is unset and this
/// is a no-op that passes; when the parent re-execs this test by name with the
/// env var set, it commits a hard state and aborts without a clean exit.
#[test]
fn crash_child_commits_hard_state_then_aborts() {
    let Ok(path) = std::env::var(CRASH_ENV) else {
        return; // parent run: nothing to do
    };
    let log = RaftLog::open(&path).expect("child: open store");
    log.save_hard_state(&committed_child_state())
        .expect("child: commit hard state");
    // The commit above fsync'd. Now die as hard as a machine losing power.
    std::process::abort();
}

#[test]
fn committed_hard_state_survives_process_abort() {
    if std::env::var(CRASH_ENV).is_ok() {
        return; // we are the crash child; don't recurse
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raft.redb");

    // Pre-seed a different state so we can tell the child's write apart from it.
    {
        let log = RaftLog::open(&path).unwrap();
        log.save_hard_state(&HardState::new()).unwrap();
    }

    // Re-exec ourselves, running only the crash child, with the env var set.
    let exe = std::env::current_exe().unwrap();
    let output = Command::new(exe)
        .args(["--exact", "crash_child_commits_hard_state_then_aborts"])
        .env(CRASH_ENV, &path)
        .output()
        .unwrap();

    // The child aborted, so it must not have exited successfully.
    assert!(
        !output.status.success(),
        "crash child should have aborted, not exited cleanly (status: {:?})",
        output.status
    );

    // Reopen: the child's committed hard state is durable and uncorrupted.
    let log = RaftLog::open(&path).unwrap();
    assert_eq!(log.load_hard_state().unwrap(), committed_child_state());
}
