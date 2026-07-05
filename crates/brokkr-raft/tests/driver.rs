//! Async `RaftDriver` tests over a deterministic in-process transport (I5b).
//!
//! These run the real async event loop ([`RaftDriver`]) on `tokio`'s **paused**
//! clock, so time only advances when the test says so — a leader election and a
//! committed proposal are exercised end-to-end through async channels, tasks and
//! timers, deterministically. The `switchboard` transport delivers RPCs directly
//! to peer [`RaftHandle`]s and can drop them to model a partition.
//!
//! The `turmoil` + real-tonic-transport variant is a follow-up increment.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use brokkr_raft::{
    AppendEntries, AppendEntriesResponse, Config, NodeId, RaftDriver, RaftHandle, RaftLog,
    RaftNode, RaftRpc, RequestVote, RequestVoteResponse, Rng, Transport,
};

/// The node id for cluster member `i` (nodes are named `n0`, `n1`, …).
fn node_id(i: usize) -> NodeId {
    NodeId::new(format!("n{i}")).unwrap()
}

/// Shared wiring: a registry of every node's handle plus the current partition
/// groups. All nodes' transports share it, so it can be populated after the
/// drivers are built (breaking the driver↔transport cycle) and mutated to inject
/// partitions.
#[derive(Clone, Default)]
struct Fabric {
    handles: Arc<Mutex<HashMap<NodeId, RaftHandle>>>,
    groups: Arc<Mutex<Vec<BTreeSet<NodeId>>>>,
}

impl Fabric {
    fn register(&self, id: NodeId, handle: RaftHandle) {
        self.handles.lock().unwrap().insert(id, handle);
    }

    fn partition(&self, groups: &[&[usize]]) {
        *self.groups.lock().unwrap() = groups
            .iter()
            .map(|g| g.iter().map(|&i| node_id(i)).collect())
            .collect();
    }

    fn heal(&self) {
        self.groups.lock().unwrap().clear();
    }

    fn connected(&self, a: &NodeId, b: &NodeId) -> bool {
        let groups = self.groups.lock().unwrap();
        if groups.is_empty() {
            return true;
        }
        groups.iter().any(|g| g.contains(a) && g.contains(b))
    }

    fn handle(&self, id: &NodeId) -> Option<RaftHandle> {
        self.handles.lock().unwrap().get(id).cloned()
    }
}

/// A transport that delivers RPCs straight to a peer's [`RaftHandle`], honoring
/// the fabric's partitions. (Real gRPC-over-`turmoil` is a separate test.)
struct Switchboard {
    me: NodeId,
    fabric: Fabric,
}

impl Switchboard {
    fn reachable(&self, to: &NodeId) -> Result<RaftHandle, brokkr_raft::RaftError> {
        if !self.fabric.connected(&self.me, to) {
            return Err(brokkr_raft::RaftError::Transport("partitioned".to_string()));
        }
        self.fabric
            .handle(to)
            .ok_or_else(|| brokkr_raft::RaftError::UnknownPeer(to.to_string()))
    }
}

#[async_trait]
impl Transport for Switchboard {
    async fn request_vote(
        &self,
        to: &NodeId,
        req: RequestVote,
    ) -> Result<RequestVoteResponse, brokkr_raft::RaftError> {
        self.reachable(to)?.request_vote(req).await
    }

    async fn append_entries(
        &self,
        to: &NodeId,
        req: AppendEntries,
    ) -> Result<AppendEntriesResponse, brokkr_raft::RaftError> {
        self.reachable(to)?.append_entries(req).await
    }

    async fn install_snapshot(
        &self,
        to: &NodeId,
        req: brokkr_raft::InstallSnapshot,
    ) -> Result<brokkr_raft::InstallSnapshotResponse, brokkr_raft::RaftError> {
        self.reachable(to)?.install_snapshot(req).await
    }
}

/// Spins up an `n`-node cluster of drivers over a shared [`Fabric`], keeping the
/// temp dirs alive for the caller.
fn cluster(n: usize) -> (Fabric, Vec<RaftHandle>, Vec<tempfile::TempDir>) {
    let ids: Vec<NodeId> = (0..n).map(node_id).collect();
    let fabric = Fabric::default();
    let mut handles = Vec::new();
    let mut dirs = Vec::new();
    for i in 0..n {
        let dir = tempfile::tempdir().unwrap();
        let log = RaftLog::open(dir.path().join("raft.redb")).unwrap();
        let peers: Vec<NodeId> = ids.iter().filter(|id| **id != ids[i]).cloned().collect();
        let node = RaftNode::new(
            ids[i].clone(),
            peers,
            log,
            Rng::seed_from_u64(7 + i as u64 * 13),
            Config::default(),
            tokio::time::Instant::now().into_std(),
        )
        .unwrap();
        let transport = Arc::new(Switchboard {
            me: ids[i].clone(),
            fabric: fabric.clone(),
        });
        let (driver, handle) = RaftDriver::new(node, transport, Duration::from_millis(15));
        fabric.register(ids[i].clone(), handle.clone());
        handles.push(handle);
        dirs.push(dir);
        // Surface a driver-loop failure instead of letting it masquerade as
        // "no leader found" in a later assertion.
        tokio::spawn(async move {
            if let Err(e) = driver.run().await {
                panic!("raft driver task exited with error: {e}");
            }
        });
    }
    (fabric, handles, dirs)
}

/// Advances the paused clock by `total`, yielding between small steps so async
/// message round-trips can make progress.
async fn settle(total: Duration) {
    let mut elapsed = Duration::ZERO;
    let step = Duration::from_millis(10);
    while elapsed < total {
        tokio::time::advance(step).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        elapsed += step;
    }
}

async fn leaders(handles: &[RaftHandle]) -> Vec<usize> {
    let mut out = Vec::new();
    for (i, h) in handles.iter().enumerate() {
        if h.status().await.map(|s| s.is_leader).unwrap_or(false) {
            out.push(i);
        }
    }
    out
}

#[tokio::test(start_paused = true)]
async fn driver_cluster_elects_a_leader_and_commits() {
    let (_fabric, handles, _dirs) = cluster(3);

    // Elect.
    settle(Duration::from_secs(2)).await;
    let leaders = leaders(&handles).await;
    assert_eq!(leaders.len(), 1, "exactly one leader emerges");
    let leader = leaders[0];

    // A committed proposal replicates to a majority.
    handles[leader]
        .propose(Bytes::from_static(b"hello"))
        .await
        .unwrap();
    settle(Duration::from_secs(1)).await;

    let status = handles[leader].status().await.unwrap();
    assert!(status.is_leader);
    assert_eq!(status.commit_index.get(), 1, "the write committed");
}

#[tokio::test(start_paused = true)]
async fn driver_minority_partition_cannot_commit_then_heals() {
    let (fabric, handles, _dirs) = cluster(5);
    settle(Duration::from_secs(2)).await;
    let leader = leaders(&handles).await[0];

    // Commit something with the whole cluster connected.
    handles[leader]
        .propose(Bytes::from_static(b"a"))
        .await
        .unwrap();
    settle(Duration::from_secs(1)).await;
    let committed = handles[leader].status().await.unwrap().commit_index.get();
    assert!(committed >= 1);

    // Isolate the leader with one companion (minority of 2). Its commit index
    // must not advance; the majority side keeps the cluster alive.
    let others: Vec<usize> = (0..5).filter(|&i| i != leader).collect();
    let companion = others[0];
    let majority: Vec<usize> = others[1..].to_vec();
    fabric.partition(&[&[leader, companion], &majority]);
    let _ = handles[leader].propose(Bytes::from_static(b"stuck")).await;
    settle(Duration::from_secs(2)).await;
    assert_eq!(
        handles[leader].status().await.unwrap().commit_index.get(),
        committed,
        "a partitioned minority leader cannot advance its commit index"
    );

    // Heal: a single leader re-forms.
    fabric.heal();
    settle(Duration::from_secs(3)).await;
    assert_eq!(leaders(&handles).await.len(), 1, "one leader after healing");
}
