//! Real-process HA cluster test (Phase 5 I9, plan §17 task 7 / DoD 1).
//!
//! Spawns THREE real `brokkr-control` binaries as a Raft cluster
//! (`--raft --node-id --raft-peer --raft-listen`), drives the replicated
//! metadata through the public REAPI `ActionCache` surface, then **kills the
//! leader with SIGKILL** and measures how long until a survivor accepts a
//! write again — the paper's failover, on real processes and real sockets.
//!
//! **DoD 1: kill the leader → the cluster elects a new one in < 2 s.**
//!
//! The leader is discovered the way a client would: try the write on every
//! node; followers answer `FAILED_PRECONDITION` (the I8c redirect), the
//! leader accepts. Durability is asserted the same way: the pre-kill write
//! must be readable from the new leader.
//!
//! `#[ignore]` by default: spawns processes. Run after a workspace build:
//!
//! ```text
//! cargo build --workspace
//! cargo test -p brokkr-control --test raft_ha_cluster -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use brokkr_proto::reapi_v2::{
    self as rapi, action_cache_client::ActionCacheClient, Digest as PbDigest,
};
use tonic::transport::Channel;

/// Poll `addr` until it accepts a TCP connection or `budget` elapses.
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

/// Kill + reap a child on drop so a failing assertion never leaks processes.
struct Reap(std::process::Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn digest(name: &str) -> PbDigest {
    // Any well-formed sha256-hex digest works as an AC key.
    let mut hash = String::with_capacity(64);
    for byte in name.bytes().cycle().take(32) {
        hash.push_str(&format!("{byte:02x}"));
    }
    PbDigest {
        hash,
        size_bytes: 42,
    }
}

fn sample_result(marker: &str) -> rapi::ActionResult {
    rapi::ActionResult {
        stdout_raw: marker.as_bytes().to_vec(),
        exit_code: 0,
        ..Default::default()
    }
}

async fn client(endpoint: &str) -> Option<ActionCacheClient<Channel>> {
    let channel = Channel::from_shared(endpoint.to_string())
        .ok()?
        .connect_timeout(Duration::from_millis(300))
        .timeout(Duration::from_millis(500))
        .connect()
        .await
        .ok()?;
    Some(ActionCacheClient::new(channel))
}

/// Attempts the write on `endpoint`; `Ok(true)` iff this node is the leader.
async fn try_update(endpoint: &str, d: &PbDigest, marker: &str) -> bool {
    let Some(mut client) = client(endpoint).await else {
        return false;
    };
    client
        .update_action_result(rapi::UpdateActionResultRequest {
            instance_name: String::new(),
            action_digest: Some(d.clone()),
            action_result: Some(sample_result(marker)),
            ..Default::default()
        })
        .await
        .is_ok()
}

/// Loops over `endpoints` until one accepts the write; returns its index.
async fn await_writable(
    endpoints: &[String],
    d: &PbDigest,
    marker: &str,
    budget: Duration,
) -> usize {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        for (i, endpoint) in endpoints.iter().enumerate() {
            if try_update(endpoint, d, marker).await {
                return i;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no node accepted a write within {budget:?}");
}

#[tokio::test]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn leader_kill_fails_over_in_under_two_seconds() {
    let control_bin = PathBuf::from(env!("CARGO_BIN_EXE_brokkr-control"));

    // Three nodes: client port, worker port (auto = client+1), raft port.
    let n = 3;
    let client_ports: Vec<u16> = (0..n).map(|_| free_port()).collect();
    let raft_ports: Vec<u16> = (0..n).map(|_| free_port()).collect();
    let dirs: Vec<tempfile::TempDir> = (0..n).map(|_| tempfile::tempdir().unwrap()).collect();

    let mut children: Vec<Option<Reap>> = Vec::new();
    for i in 0..n {
        let mut cmd = Command::new(&control_bin);
        cmd.args(["--listen", &format!("127.0.0.1:{}", client_ports[i])])
            .arg("--data-dir")
            .arg(dirs[i].path())
            .args(["--raft", "--node-id", &format!("control-{i}")])
            .args(["--raft-listen", &format!("127.0.0.1:{}", raft_ports[i])]);
        for (j, port) in raft_ports.iter().enumerate() {
            if j != i {
                cmd.args(["--raft-peer", &format!("control-{j}=127.0.0.1:{port}")]);
            }
        }
        children.push(Some(Reap(cmd.spawn().unwrap())));
    }
    let endpoints: Vec<String> = client_ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();
    for (i, port) in client_ports.iter().enumerate() {
        assert!(
            wait_for_listen(&format!("127.0.0.1:{port}"), Duration::from_secs(15)),
            "control node {i} did not start listening"
        );
    }

    // Find the leader by writing through the public AC surface.
    let d1 = digest("pre-kill");
    let leader = await_writable(&endpoints, &d1, "pre-kill", Duration::from_secs(10)).await;
    eprintln!("leader is node {leader}");

    // DoD 1: SIGKILL the leader, then measure time until a survivor accepts
    // a write. The clock starts at the kill.
    let d2 = digest("post-kill");
    children[leader] = None; // Reap::drop → kill + wait
    let survivors: Vec<String> = endpoints
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != leader)
        .map(|(_, e)| e.clone())
        .collect();
    let killed_at = Instant::now();
    let new_leader_pos =
        await_writable(&survivors, &d2, "post-kill", Duration::from_secs(10)).await;
    let failover = killed_at.elapsed();
    eprintln!("time to a writable new leader: {failover:?}");
    assert!(
        failover < Duration::from_secs(2),
        "DoD 1 violated: failover took {failover:?} (>= 2s)"
    );

    // Durability across the failover: the pre-kill write is readable from
    // the new leader (linearizably — the AC read path runs ReadIndex).
    let mut client = client(&survivors[new_leader_pos]).await.unwrap();
    let got = client
        .get_action_result(rapi::GetActionResultRequest {
            instance_name: String::new(),
            action_digest: Some(d1),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        got.stdout_raw, b"pre-kill",
        "the committed pre-kill write survived the leader's death"
    );
}
