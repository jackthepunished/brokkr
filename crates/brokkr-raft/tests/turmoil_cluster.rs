//! Tonic-over-turmoil multi-node cluster tests (Phase 5 I5c, §17 task 3).
//!
//! Runs a real 3-node Raft cluster — [`RaftDriver`]s over the production
//! [`TonicTransport`] — with every RPC crossing `turmoil`'s deterministic
//! simulated network as genuine gRPC/HTTP2:
//!
//! - **server side:** each host serves `brokkr.v1.RaftService` by wrapping its
//!   [`RaftHandle`] in [`RaftServiceAdapter`] and feeding tonic's
//!   `serve_with_incoming` from a `turmoil::net::TcpListener`;
//! - **client side:** each peer [`Channel`] dials through a connector that
//!   opens a `turmoil::net::TcpStream`, so tonic's whole client stack (h2,
//!   reconnect, timeouts) runs on the simulation.
//!
//! This closes the gap the I5b tests left open: those drove the driver over an
//! in-process switchboard; here the wire is the real one. Cluster *state* is
//! observed out-of-band through the [`RaftHandle`]s (as in the driver tests) —
//! only Raft traffic is subject to the simulated network, which is exactly
//! what `turmoil::partition`/`turmoil::repair` manipulate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::server::Connected;
use tonic::transport::{Channel, Endpoint, Server, Uri};
use tower::service_fn;
use turmoil::net::{TcpListener, TcpStream};

use brokkr_raft::{
    Config, NodeId, RaftDriver, RaftHandle, RaftLog, RaftNode, RaftServiceAdapter, Rng,
    StateMachine, TonicTransport,
};

/// Minimal test state machine: applied commands accumulate as a
/// length-prefixed byte log; snapshot/restore move the raw blob.
#[derive(Default)]
struct TestMachine {
    state: Vec<u8>,
}

impl StateMachine for TestMachine {
    fn apply(&mut self, entry: &brokkr_raft::LogEntry) {
        if let Some(command) = entry.command() {
            self.state
                .extend_from_slice(&u32::try_from(command.len()).unwrap().to_le_bytes());
            self.state.extend_from_slice(command);
        }
    }

    fn snapshot(&self) -> Bytes {
        Bytes::from(self.state.clone())
    }

    fn restore(&mut self, snapshot: &[u8]) {
        self.state = snapshot.to_vec();
    }
}

const PORT: u16 = 9100;
const N: usize = 3;
const TICK: Duration = Duration::from_millis(15);

/// The turmoil host name (and Raft node id) of cluster member `i`.
fn host(i: usize) -> String {
    format!("n{i}")
}

/// Out-of-band registry of every node's handle, for observation and proposals.
type Registry = Arc<Mutex<HashMap<String, RaftHandle>>>;

fn handles(registry: &Registry) -> Vec<(String, RaftHandle)> {
    registry
        .lock()
        .unwrap()
        .iter()
        .map(|(name, h)| (name.clone(), h.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// turmoil <-> tonic glue
// ---------------------------------------------------------------------------

/// `turmoil::net::TcpStream` behind tonic's [`Connected`], so the tonic server
/// can accept simulated connections through `serve_with_incoming`.
struct TurmoilIo(TcpStream);

impl Connected for TurmoilIo {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for TurmoilIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TurmoilIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// A lazily-connecting [`Channel`] to `peer` that dials through the simulated
/// network. Timeouts are bounded well below the polling cadence so RPCs to a
/// partitioned peer fail fast and Raft simply retries on its next tick.
///
/// HTTP/2 keepalive is essential, not decoration: turmoil partitions *drop*
/// packets silently, so an h2 connection that was alive when the partition
/// started never surfaces a socket error — without keepalive tonic would pin
/// the dead connection forever and the cluster could not re-integrate the
/// deposed leader after a heal.
fn peer_channel(peer: &str) -> Channel {
    Endpoint::try_from(format!("http://{peer}:{PORT}"))
        .unwrap()
        .connect_timeout(Duration::from_millis(200))
        .timeout(Duration::from_millis(200))
        .http2_keep_alive_interval(Duration::from_millis(250))
        .keep_alive_timeout(Duration::from_millis(250))
        .keep_alive_while_idle(true)
        .connect_with_connector_lazy(service_fn(|uri: Uri| async move {
            let authority = uri.authority().expect("peer uri has authority").to_string();
            let stream = TcpStream::connect(authority).await?;
            Ok::<_, io::Error>(TokioIo::new(stream))
        }))
}

/// Registers hosts `n0..nN` on the simulation. Each runs a full node: redb log
/// in a tempdir, `RaftNode` + `RaftDriver`, `TonicTransport` to its peers, and
/// a tonic server for inbound `RaftService` RPCs.
fn spawn_cluster(sim: &mut turmoil::Sim<'_>, registry: &Registry) {
    for i in 0..N {
        let registry = registry.clone();
        sim.host(host(i), move || {
            let registry = registry.clone();
            async move {
                let dir = tempfile::tempdir()?;
                let log = RaftLog::open(dir.path().join("raft.redb"))?;
                let mut peers = Vec::new();
                let mut transport = TonicTransport::new();
                for j in (0..N).filter(|&j| j != i) {
                    let id = NodeId::new(host(j))?;
                    transport.insert_peer(id.clone(), peer_channel(&host(j)));
                    peers.push(id);
                }
                let node = RaftNode::new(
                    NodeId::new(host(i))?,
                    peers,
                    log,
                    Rng::seed_from_u64(23 + i as u64 * 31),
                    Config::default(),
                    tokio::time::Instant::now().into_std(),
                )?;
                let (driver, handle) = RaftDriver::new(
                    node,
                    Box::new(TestMachine::default()),
                    Arc::new(transport),
                    TICK,
                );
                registry.lock().unwrap().insert(host(i), handle.clone());
                // Surface a driver-loop failure instead of letting it
                // masquerade as "no leader found" in a later assertion.
                tokio::spawn(async move {
                    if let Err(e) = driver.run().await {
                        panic!("raft driver task exited with error: {e}");
                    }
                });

                let listener = TcpListener::bind(("0.0.0.0", PORT)).await?;
                let (conn_tx, conn_rx) = mpsc::channel::<Result<TurmoilIo, io::Error>>(16);
                tokio::spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        if conn_tx.send(Ok(TurmoilIo(stream))).await.is_err() {
                            break;
                        }
                    }
                });
                Server::builder()
                    .add_service(RaftServiceAdapter::new(Arc::new(handle)).into_server())
                    .serve_with_incoming(ReceiverStream::new(conn_rx))
                    .await?;
                Ok(())
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Polling helpers (simulated time)
// ---------------------------------------------------------------------------

/// Polls until exactly one node in `among` reports itself leader.
async fn await_leader(registry: &Registry, among: &[String]) -> (String, RaftHandle) {
    for _ in 0..400 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let mut found = Vec::new();
        for (name, h) in handles(registry) {
            if !among.contains(&name) {
                continue;
            }
            if let Ok(status) = h.status().await {
                if status.is_leader {
                    found.push((name, h));
                }
            }
        }
        if found.len() == 1 {
            return found.remove(0);
        }
    }
    panic!("no single leader among {among:?} within the polling budget");
}

/// Polls until every node's commit index has reached `index`.
async fn await_commit_everywhere(registry: &Registry, index: u64) {
    'attempt: for _ in 0..400 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let all = handles(registry);
        if all.len() < N {
            continue;
        }
        for (_, h) in all {
            match h.status().await {
                Ok(status) if status.commit_index.get() >= index => {}
                _ => continue 'attempt,
            }
        }
        return;
    }
    panic!("commit index {index} did not reach every node within the polling budget");
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[test]
fn grpc_cluster_elects_a_leader_and_replicates() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .build();
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
    spawn_cluster(&mut sim, &registry);

    let reg = registry.clone();
    sim.client("tester", async move {
        let all: Vec<String> = (0..N).map(host).collect();
        let (_, leader) = await_leader(&reg, &all).await;
        leader.propose(Bytes::from_static(b"cmd-1")).await?;
        await_commit_everywhere(&reg, 1).await;
        Ok(())
    });

    sim.run().unwrap();
}

#[test]
fn grpc_cluster_survives_leader_partition_and_heals() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(120))
        .build();
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
    spawn_cluster(&mut sim, &registry);

    let reg = registry.clone();
    sim.client("tester", async move {
        let all: Vec<String> = (0..N).map(host).collect();
        let (leader_name, leader) = await_leader(&reg, &all).await;
        let old_term = leader.status().await?.term;
        leader.propose(Bytes::from_static(b"cmd-1")).await?;
        // No-op at 1 (I8b), the command at 2.
        await_commit_everywhere(&reg, 2).await;

        // Cut the leader off from both followers: its heartbeats and any
        // AppendEntries stop crossing the simulated network.
        let followers: Vec<String> = all
            .iter()
            .filter(|name| **name != leader_name)
            .cloned()
            .collect();
        for follower in &followers {
            turmoil::partition(leader_name.as_str(), follower.as_str());
        }

        // The majority elects a fresh leader at a higher term and commits.
        let (new_leader_name, new_leader) = await_leader(&reg, &followers).await;
        assert!(
            new_leader.status().await?.term > old_term,
            "the new leader must be at a higher term"
        );
        new_leader.propose(Bytes::from_static(b"cmd-2")).await?;

        // The isolated old leader may append but must never commit.
        let _ = leader.propose(Bytes::from_static(b"doomed")).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            leader.status().await?.commit_index.get(),
            2,
            "a minority-partitioned leader cannot advance its commit index"
        );

        // Heal: the old leader steps down and converges on the new log.
        for follower in &followers {
            turmoil::repair(leader_name.as_str(), follower.as_str());
        }
        // New leader's log: no-op@1, cmd-1@2, its own no-op@3, cmd-2@4.
        await_commit_everywhere(&reg, 4).await;
        let (final_name, _) = await_leader(&reg, &all).await;
        assert_eq!(
            final_name, new_leader_name,
            "the higher-term leader stays leader through the heal"
        );
        let old = leader.status().await?;
        assert!(!old.is_leader, "the deposed leader steps down");
        assert_eq!(
            old.commit_index.get(),
            4,
            "the deposed leader catches up to the committed log"
        );
        assert_eq!(
            old.last_log_index.get(),
            4,
            "the uncommitted minority entry was overwritten, not kept"
        );
        Ok(())
    });

    sim.run().unwrap();
}
