//! The full HA story on real processes (Phase 5 I9b W6, plan §17 task 7).
//!
//! `raft_ha_cluster.rs` proves DoD 1 through the raw `ActionCache` surface.
//! This test proves the thing a *user* cares about: three control nodes, a
//! worker, and `brokk run` — kill the leader and builds keep working, with the
//! cache intact.
//!
//! **The D1 subtlety this test is built around.** Under decision D1 (owner,
//! 2026-07-30) a build routed to a *follower* succeeds but is **not cached**,
//! because the follower cannot write the replicated action cache and the
//! result is too expensive to throw away. So the write whose survival matters
//! must be made **through the leader** — asserting a cache hit on a
//! follower-routed build would pass for the wrong reason, or fail for a reason
//! that is by design.
//!
//! `#[ignore]` by default: spawns processes. Run after a workspace build:
//!
//! ```text
//! cargo build --workspace
//! cargo test -p brokkr-control --test raft_ha_e2e -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use std::env::consts::EXE_SUFFIX;

/// Kill + reap a child on drop so a failing assertion never leaks processes.
struct Reap(std::process::Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sibling_bin(name: &str) -> PathBuf {
    let control = env!("CARGO_BIN_EXE_brokkr-control");
    Path::new(control)
        .parent()
        .unwrap()
        .join(format!("{name}{EXE_SUFFIX}"))
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn wait_for_listen(addr: &str, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(addr).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// One `brokk run` against `endpoints`, returning (exit status, stderr).
///
/// `brokk` prints `[brokk] exit=… cache_hit=…` and, when the server had
/// something to say, a `[brokk] <message>` line — which is how this test
/// observes "ran but not cached" without reaching into server logs.
fn brokk_run<S: AsRef<OsStr>>(endpoints: &[S], marker: &str) -> (bool, String) {
    // `brokk run --control A [--control B ...] -- echo <marker>`
    let mut cmd = Command::new(sibling_bin("brokk"));
    cmd.arg("run");
    for endpoint in endpoints {
        cmd.arg("--control").arg(endpoint);
    }
    cmd.arg("--").arg("echo").arg(marker);
    let out = cmd.output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn cache_hit(stderr: &str) -> bool {
    stderr.contains("cache_hit=true")
}

fn not_cached(stderr: &str) -> bool {
    stderr.contains("not cached")
}

/// Find which endpoint is the leader by running a throwaway action against
/// each: the leader's run reports nothing unusual, a follower's reports
/// "not cached" (D1). Returns the index of the leader.
fn find_leader(endpoints: &[String]) -> usize {
    for (i, endpoint) in endpoints.iter().enumerate() {
        let (ok, stderr) = brokk_run(std::slice::from_ref(endpoint), &format!("probe-{i}"));
        if ok && !not_cached(&stderr) {
            return i;
        }
    }
    panic!("no node accepted a cacheable build; is the cluster up?");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn brokk_run_survives_killing_the_leader() {
    let control_bin = PathBuf::from(env!("CARGO_BIN_EXE_brokkr-control"));
    let worker_bin = sibling_bin("brokkr-worker");

    let n = 3usize;
    let client_ports: Vec<u16> = (0..n).map(|_| free_port()).collect();
    let raft_ports: Vec<u16> = (0..n).map(|_| free_port()).collect();
    let dirs: Vec<tempfile::TempDir> = (0..n).map(|_| tempfile::tempdir().unwrap()).collect();

    let mut controls: Vec<Option<Reap>> = Vec::new();
    for i in 0..n {
        let mut cmd = Command::new(&control_bin);
        cmd.args(["--listen", &format!("127.0.0.1:{}", client_ports[i])])
            .arg("--data-dir")
            .arg(dirs[i].path())
            .args(["--raft", "--node-id", &format!("control-{i}")])
            // I9b W1: without an advertise address the leader hint carries an
            // id nobody can dial, so the redirect is unusable.
            .args([
                "--advertise-addr",
                &format!("127.0.0.1:{}", client_ports[i]),
            ])
            .args(["--raft-listen", &format!("127.0.0.1:{}", raft_ports[i])]);
        for (j, port) in raft_ports.iter().enumerate() {
            if j != i {
                cmd.args(["--raft-peer", &format!("control-{j}=127.0.0.1:{port}")]);
            }
        }
        controls.push(Some(Reap(cmd.spawn().unwrap())));
    }

    let endpoints: Vec<String> = client_ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();
    for (i, port) in client_ports.iter().enumerate() {
        assert!(
            wait_for_listen(&format!("127.0.0.1:{port}"), Duration::from_secs(20)),
            "control node {i} did not start listening"
        );
    }

    // ONE WORKER PER CONTROL NODE — this is not padding, it is a property of
    // the design. The worker registry is per-node and ephemeral by ADR
    // 0008-0010: it is deliberately *not* replicated through Raft, because
    // workers re-register and leases reassign on failure. So a node with no
    // worker attached cannot execute anything, and a build routed there fails
    // `NoEligibleWorker` no matter how healthy the Raft cluster is. An HA
    // control plane therefore needs workers on every node, not just a
    // replicated log.
    //
    // Each worker is given the full endpoint list rotated so that worker `i`
    // attaches to node `i` first; when its node dies it rotates to a survivor
    // and re-registers, which is I9b W4 under test.
    let mut workers: Vec<Reap> = Vec::new();
    for i in 0..n {
        let mut worker_cmd = Command::new(&worker_bin);
        for k in 0..n {
            worker_cmd.args(["--control", &endpoints[(i + k) % n]]);
        }
        // `--no-sandbox`: dev hosts and CI lack unprivileged user namespaces.
        worker_cmd.arg("--no-sandbox");
        workers.push(Reap(worker_cmd.spawn().unwrap()));
    }
    // Registration + stream-claim window for all of them.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let leader = find_leader(&endpoints);
    eprintln!("leader is node {leader}");
    let follower = (0..n).find(|&i| i != leader).unwrap();

    // 1. A build routed at a FOLLOWER succeeds — and says it was not cached.
    //    This is D1: correct result, no cache entry, and the client is told.
    let (ok, stderr) = brokk_run(&[endpoints[follower].clone()], "follower-routed");
    assert!(ok, "a follower-routed build must still succeed:\n{stderr}");
    assert!(
        not_cached(&stderr),
        "a follower-routed build must report that it was not cached:\n{stderr}"
    );

    // 2. The same action through the LEADER is cached. This is the write whose
    //    survival the whole test is about, which is why it must not go to a
    //    follower.
    let (ok, stderr) = brokk_run(&[endpoints[leader].clone()], "leader-routed");
    assert!(ok, "a leader-routed build must succeed:\n{stderr}");
    assert!(
        !not_cached(&stderr),
        "a leader-routed build must be cached:\n{stderr}"
    );

    // 3. SIGKILL the leader.
    controls[leader] = None; // Reap::drop → kill + wait
    let survivors: Vec<String> = endpoints
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != leader)
        .map(|(_, e)| e.clone())
        .collect();
    let killed_at = Instant::now();

    // 4. A NEW action succeeds against the survivors. The client is given the
    //    whole survivor list, exactly as an operator would configure it.
    let (ok, stderr) = brokk_run(&survivors, "post-kill");
    let failover = killed_at.elapsed();
    eprintln!("first successful build after the kill: {failover:?}");
    assert!(ok, "a build must succeed after the leader dies:\n{stderr}");
    assert!(
        failover < Duration::from_secs(5),
        "a post-kill build took {failover:?} (>= 5s budget)"
    );

    // 5. The point of replicating the action cache: the leader-routed write
    //    made *before* the kill is still a cache hit afterwards, served by a
    //    node that was a follower when the write happened.
    let (ok, stderr) = brokk_run(&survivors, "leader-routed");
    assert!(ok, "the pre-kill action must still run:\n{stderr}");
    assert!(
        cache_hit(&stderr),
        "the pre-kill leader-routed write must survive the failover as a cache hit:\n{stderr}"
    );

    eprintln!("I9b W6 OK: failover-to-build {failover:?}, cache survived");
}
