//! The Raft peer plane over mTLS (Phase 5 I9d, ADR 0011 amendment).
//!
//! I9a shipped the peer plane in plaintext on the reasoning that it runs on a
//! trusted network. That is a deployment assumption, not a security property:
//! `AppendEntries` on this plane **appends to the replicated log**, so an
//! unauthenticated peer port is a write path into consensus itself — a
//! strictly larger hole than the client or worker planes, where the worst case
//! is unauthorized reads and writes of *cache* data.
//!
//! Two things are proven here, and they are different claims:
//!
//! 1. a three-node cluster whose peer links are mutual-TLS still elects and
//!    replicates (the security did not break the consensus), and
//! 2. a node presenting a certificate signed by an untrusted CA is **refused**
//!    (the security is actually enforced, not merely configured).
//!
//! `#[ignore]` by default: spawns processes. Run after a workspace build:
//!
//! ```text
//! cargo build --workspace
//! cargo test -p brokkr-control --test raft_mtls_cluster -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use brokkr_proto::reapi_v2::{
    self as rapi, action_cache_client::ActionCacheClient, Digest as PbDigest,
};
use tonic::transport::Channel;

/// Kill + reap a child on drop so a failing assertion never leaks processes.
struct Reap(std::process::Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
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

fn digest(name: &str) -> PbDigest {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    PbDigest {
        hash: hex::encode(h.finalize()),
        size_bytes: name.len() as i64,
    }
}

fn sample_result(marker: &str) -> rapi::ActionResult {
    rapi::ActionResult {
        exit_code: 0,
        stdout_raw: marker.as_bytes().to_vec(),
        ..Default::default()
    }
}

async fn client(endpoint: &str) -> Option<ActionCacheClient<Channel>> {
    ActionCacheClient::connect(endpoint.to_string()).await.ok()
}

/// Try a write on `endpoint`; `true` when this node is the leader and took it.
async fn try_update(endpoint: &str, d: &PbDigest, marker: &str) -> bool {
    let Some(mut c) = client(endpoint).await else {
        return false;
    };
    c.update_action_result(rapi::UpdateActionResultRequest {
        instance_name: String::new(),
        action_digest: Some(d.clone()),
        action_result: Some(sample_result(marker)),
        ..Default::default()
    })
    .await
    .is_ok()
}

/// Spawn `n` control nodes whose **client** ports are plaintext (so the test
/// can drive them without minting client certs) and whose **raft** ports use
/// the given TLS fixtures. `peer_cert`/`peer_key` is what each node presents
/// to its peers; `peer_ca` is what it verifies them against.
#[allow(clippy::type_complexity)]
fn spawn_cluster(
    n: usize,
    peer_cert: Option<(&str, &str, &str)>,
) -> (Vec<Option<Reap>>, Vec<String>, Vec<tempfile::TempDir>) {
    let control_bin = PathBuf::from(env!("CARGO_BIN_EXE_brokkr-control"));
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
            .args([
                "--advertise-addr",
                &format!("127.0.0.1:{}", client_ports[i]),
            ])
            .args(["--raft-listen", &format!("127.0.0.1:{}", raft_ports[i])]);
        if let Some((cert, key, ca)) = peer_cert {
            cmd.arg("--raft-tls-cert")
                .arg(fixture(cert))
                .arg("--raft-tls-key")
                .arg(fixture(key))
                .arg("--raft-tls-ca")
                .arg(fixture(ca));
        }
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
            wait_for_listen(&format!("127.0.0.1:{port}"), Duration::from_secs(20)),
            "control node {i} did not start listening"
        );
    }
    (children, endpoints, dirs)
}

/// Poll every endpoint until one accepts a write, or the budget expires.
async fn await_writable(
    endpoints: &[String],
    d: &PbDigest,
    marker: &str,
    budget: Duration,
) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        for endpoint in endpoints {
            if try_update(endpoint, d, marker).await {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// mTLS on the peer plane does not break consensus: the cluster still elects a
/// leader and replicates through it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn an_mtls_peer_plane_still_elects_and_replicates() {
    let (_children, endpoints, _dirs) =
        spawn_cluster(3, Some(("server.pem", "server.key", "ca.pem")));

    let d = digest("mtls-write");
    assert!(
        await_writable(&endpoints, &d, "mtls-write", Duration::from_secs(20)).await,
        "an mTLS cluster must still elect a leader and accept writes"
    );

    // And the write really replicated: read it back from whichever node is
    // leader now (reads are leader-served; followers redirect).
    let mut found = false;
    for endpoint in &endpoints {
        let Some(mut c) = client(endpoint).await else {
            continue;
        };
        if let Ok(resp) = c
            .get_action_result(rapi::GetActionResultRequest {
                instance_name: String::new(),
                action_digest: Some(d.clone()),
                ..Default::default()
            })
            .await
        {
            assert_eq!(resp.into_inner().stdout_raw, b"mtls-write");
            found = true;
            break;
        }
    }
    assert!(found, "the replicated write must be readable over mTLS");
}

/// The enforcement half: a cluster whose nodes present a certificate signed by
/// an **untrusted** CA must never form. If this passed, the peer plane would
/// be configured-but-not-enforcing — the exact failure mode issue #139 found
/// on the worker plane, where the CA was loaded but the client certificate was
/// not actually required.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn a_peer_with_an_untrusted_certificate_cannot_join() {
    // `badworker.pem` is signed by `badca`, but every node verifies against
    // `ca.pem` — so each peer rejects the others' certificates.
    let (_children, endpoints, _dirs) =
        spawn_cluster(3, Some(("badworker.pem", "badworker.key", "ca.pem")));

    let d = digest("should-not-commit");
    let wrote = await_writable(&endpoints, &d, "should-not-commit", Duration::from_secs(12)).await;
    assert!(
        !wrote,
        "a cluster whose peers cannot authenticate each other must not commit writes: \
         no quorum can form when every AppendEntries fails the TLS handshake"
    );
}
