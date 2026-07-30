//! Three-node `RaftKv` cluster tests (Phase 5 I8c, plan §17 task 6).
//!
//! Runs three Raft-backed KVs over a deterministic in-process switchboard
//! transport on tokio's paused clock (the same harness shape as
//! `brokkr-raft`'s driver tests) and proves the plan's scenarios: a write
//! through the leader survives killing that leader and is readable —
//! linearizably, via ReadIndex — from the new one; and a follower write is
//! refused with a structured `NotLeader` leader hint, the redirect signal
//! the service layer forwards to clients.

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
use brokkr_control::metakv::{MetaKv, MetaKvError};
use brokkr_control::{KvMachine, RaftKv};
use brokkr_raft::{
    AppendEntries, AppendEntriesResponse, Config, InstallSnapshot, InstallSnapshotResponse, NodeId,
    RaftDriver, RaftHandle, RaftLog, RaftNode, RaftRpc, RequestVote, RequestVoteResponse, Rng,
    Transport,
};
use bytes::Bytes;

fn node_id(i: usize) -> NodeId {
    NodeId::new(format!("n{i}")).unwrap()
}

/// Shared registry of handles + partition groups (the switchboard fabric).
#[derive(Clone, Default)]
struct Fabric {
    handles: Arc<Mutex<HashMap<NodeId, RaftHandle>>>,
    groups: Arc<Mutex<Vec<BTreeSet<NodeId>>>>,
}

impl Fabric {
    fn register(&self, id: NodeId, handle: RaftHandle) {
        self.handles.lock().unwrap().insert(id, handle);
    }

    fn connected(&self, a: &NodeId, b: &NodeId) -> bool {
        let groups = self.groups.lock().unwrap();
        groups.is_empty() || groups.iter().any(|g| g.contains(a) && g.contains(b))
    }

    fn handle(&self, id: &NodeId) -> Option<RaftHandle> {
        self.handles.lock().unwrap().get(id).cloned()
    }
}

/// Delivers RPCs straight to peer handles, honoring partitions. A killed
/// node's driver is gone, so calls into its handle fail like a dead host.
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
        req: InstallSnapshot,
    ) -> Result<InstallSnapshotResponse, brokkr_raft::RaftError> {
        self.reachable(to)?.install_snapshot(req).await
    }
}

struct Cluster {
    handles: Vec<RaftHandle>,
    kvs: Vec<RaftKv>,
    drivers: Vec<tokio::task::JoinHandle<()>>,
    _dirs: Vec<tempfile::TempDir>,
}

fn cluster(n: usize) -> Cluster {
    let ids: Vec<NodeId> = (0..n).map(node_id).collect();
    let fabric = Fabric::default();
    let mut handles = Vec::new();
    let mut kvs = Vec::new();
    let mut drivers = Vec::new();
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
        let machine = KvMachine::default();
        let shared = machine.shared();
        let transport = Arc::new(Switchboard {
            me: ids[i].clone(),
            fabric: fabric.clone(),
        });
        let (driver, handle) = RaftDriver::new(
            node,
            Box::new(machine),
            transport,
            Duration::from_millis(15),
        );
        fabric.register(ids[i].clone(), handle.clone());
        kvs.push(RaftKv::new(handle.clone(), shared));
        handles.push(handle);
        dirs.push(dir);
        drivers.push(tokio::spawn(async move {
            // A killed driver ends silently; live drivers must not fail.
            let _ = driver.run().await;
        }));
    }
    Cluster {
        handles,
        kvs,
        drivers,
        _dirs: dirs,
    }
}

/// Advances the paused clock in small steps, yielding so message round
/// trips make progress.
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

async fn leader_of(c: &Cluster, alive: &[usize]) -> Option<usize> {
    let mut found = Vec::new();
    for &i in alive {
        if let Ok(status) = c.handles[i].status().await {
            if status.is_leader {
                found.push(i);
            }
        }
    }
    (found.len() == 1).then(|| found[0])
}

async fn await_leader(c: &Cluster, alive: &[usize]) -> usize {
    for _ in 0..400 {
        settle(Duration::from_millis(25)).await;
        if let Some(leader) = leader_of(c, alive).await {
            return leader;
        }
    }
    panic!("no single leader among {alive:?}");
}

#[tokio::test(start_paused = true)]
async fn write_survives_killing_the_leader_and_reads_from_the_new_one() {
    let c = cluster(3);
    let all = [0, 1, 2];
    let leader = await_leader(&c, &all).await;

    // A committed-and-applied write through the leader's RaftKv.
    let kv = c.kvs[leader].clone();
    let write =
        tokio::spawn(async move { kv.put(b"cfg/cluster", Bytes::from_static(b"v1")).await });
    settle(Duration::from_secs(1)).await;
    write.await.unwrap().unwrap();

    // Kill the leader: its driver task dies; peers see a dead host.
    c.drivers[leader].abort();
    let survivors: Vec<usize> = all.into_iter().filter(|&i| i != leader).collect();
    let new_leader = await_leader(&c, &survivors).await;
    assert_ne!(new_leader, leader);

    // The write is still there — read linearizably (ReadIndex) from the
    // new leader. This is the §17 scenario: write, kill, read.
    let kv = c.kvs[new_leader].clone();
    let read = tokio::spawn(async move { kv.get(b"cfg/cluster").await });
    settle(Duration::from_secs(1)).await;
    assert_eq!(
        read.await.unwrap().unwrap(),
        Some(Bytes::from_static(b"v1")),
        "a committed write survives leader death"
    );

    // And the new leader keeps accepting writes.
    let kv = c.kvs[new_leader].clone();
    let write = tokio::spawn(async move { kv.put(b"cfg/epoch", Bytes::from_static(b"2")).await });
    settle(Duration::from_secs(1)).await;
    write.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn follower_write_is_refused_with_a_leader_hint() {
    let c = cluster(3);
    let all = [0, 1, 2];
    let leader = await_leader(&c, &all).await;
    let follower = all.into_iter().find(|&i| i != leader).unwrap();

    let kv = c.kvs[follower].clone();
    let write = tokio::spawn(async move { kv.put(b"k", Bytes::from_static(b"v")).await });
    settle(Duration::from_millis(200)).await;
    let err = write.await.unwrap().unwrap_err();
    let MetaKvError::NotLeader {
        leader: hint,
        leader_addr,
    } = err
    else {
        panic!("expected NotLeader, got {err}");
    };
    assert_eq!(
        hint.as_deref(),
        Some(node_id(leader).as_str()),
        "the refusal carries the leader's identity for the redirect"
    );
    // Nothing has published a node record yet, so the address hint is absent
    // — an id-without-address refusal, which the service layer must still
    // emit (I9b). The populated case is asserted below.
    assert_eq!(leader_addr, None);

    // Once the leader publishes its address, a *follower* resolves it from its
    // own applied map and the refusal becomes actionable: this is the whole
    // point of routing cluster config through Raft (I9b / W1).
    let leader_kv = c.kvs[leader].clone();
    let leader_name = node_id(leader).as_str().to_string();
    let publish = tokio::spawn(async move {
        leader_kv
            .publish_node_record(&leader_name, "10.0.0.9:7878")
            .await
    });
    settle(Duration::from_secs(1)).await;
    publish.await.unwrap().unwrap();

    let kv = c.kvs[follower].clone();
    let write = tokio::spawn(async move { kv.put(b"k", Bytes::from_static(b"v")).await });
    settle(Duration::from_millis(200)).await;
    let err = write.await.unwrap().unwrap_err();
    let MetaKvError::NotLeader { leader_addr, .. } = err else {
        panic!("expected NotLeader, got {err}");
    };
    assert_eq!(
        leader_addr.as_deref(),
        Some("10.0.0.9:7878"),
        "the follower resolves the leader's published address for the redirect"
    );

    // Reads on a follower are refused too (reads are leader-served in I8c).
    let kv = c.kvs[follower].clone();
    let read = tokio::spawn(async move { kv.get(b"k").await });
    settle(Duration::from_millis(200)).await;
    assert!(matches!(
        read.await.unwrap(),
        Err(MetaKvError::NotLeader { .. })
    ));
}
