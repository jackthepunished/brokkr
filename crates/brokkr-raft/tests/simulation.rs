//! Deterministic in-process simulation of a Raft cluster under faults (I5a).
//!
//! This drives the synchronous [`RaftNode`] directly through a controlled,
//! seeded network scheduler — no async runtime — so partitions, message
//! reordering / delay / loss, and process crashes are all reproducible from a
//! fixed seed. The property checked throughout is **State Machine Safety**
//! (`docs/raft-notes.md` §8): every node's *committed* log is a prefix of every
//! other's — no divergence, ever.
//!
//! The companion `turmoil`-based async harness (running the real tonic transport
//! over a simulated network) is milestone I5b; this deterministic simulator is
//! the exhaustive safety/linearizability oracle it complements.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tempfile::TempDir;

use brokkr_raft::{
    AppendEntriesResponse, Config, LogIndex, NodeId, Outbound, RaftLog, RaftNode, RequestVote,
    RequestVoteResponse, Rng,
};

// The wire payloads that travel between nodes in the simulated network.
#[derive(Clone)]
enum Msg {
    RequestVote(RequestVote),
    AppendEntries(brokkr_raft::AppendEntries),
    VoteResp(RequestVoteResponse),
    AppendResp(AppendEntriesResponse),
}

struct Packet {
    from: usize,
    to: usize,
    msg: Msg,
    deliver_at: Instant,
}

/// A deterministic simulated Raft cluster.
struct Sim {
    ids: Vec<NodeId>,
    /// `None` marks a crashed node (its persistent state survives on disk).
    nodes: Vec<Option<RaftNode>>,
    _dirs: Vec<TempDir>,
    paths: Vec<PathBuf>,
    seeds: Vec<u64>,
    cfg: Config,
    clock: Instant,
    rng: Rng,
    queue: Vec<Packet>,
    /// Partition groups; empty means fully connected.
    groups: Vec<BTreeSet<usize>>,
    step: Duration,
}

impl Sim {
    fn new(n: usize, base_seed: u64) -> Self {
        let t0 = Instant::now();
        let ids: Vec<NodeId> = (0..n)
            .map(|i| NodeId::new(format!("n{i}")).unwrap())
            .collect();
        let cfg = Config::default();
        let mut dirs = Vec::new();
        let mut paths = Vec::new();
        let mut nodes = Vec::new();
        let mut seeds = Vec::new();
        for i in 0..n {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("raft.redb");
            let seed = base_seed.wrapping_mul(1103).wrapping_add(i as u64 * 31 + 1);
            let node = Self::spawn(&ids, i, &path, seed, &cfg, t0);
            dirs.push(dir);
            paths.push(path);
            nodes.push(Some(node));
            seeds.push(seed);
        }
        Sim {
            ids,
            nodes,
            _dirs: dirs,
            paths,
            seeds,
            cfg,
            clock: t0,
            rng: Rng::seed_from_u64(base_seed ^ 0xa5a5),
            queue: Vec::new(),
            groups: Vec::new(),
            step: Duration::from_millis(5),
        }
    }

    fn spawn(
        ids: &[NodeId],
        i: usize,
        path: &PathBuf,
        seed: u64,
        cfg: &Config,
        now: Instant,
    ) -> RaftNode {
        let log = RaftLog::open(path).unwrap();
        let peers: Vec<NodeId> = ids
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, id)| id.clone())
            .collect();
        RaftNode::new(
            ids[i].clone(),
            peers,
            log,
            Rng::seed_from_u64(seed),
            cfg.clone(),
            now,
        )
        .unwrap()
    }

    fn idx(&self, id: &NodeId) -> usize {
        self.ids.iter().position(|x| x == id).unwrap()
    }

    fn connected(&self, a: usize, b: usize) -> bool {
        if self.groups.is_empty() {
            return true;
        }
        self.groups.iter().any(|g| g.contains(&a) && g.contains(&b))
    }

    fn latency(&mut self) -> Duration {
        // 1–10 ms; the spread naturally reorders concurrent messages.
        Duration::from_millis(1 + self.rng.gen_range_u64(10))
    }

    fn push(&mut self, from: usize, to: usize, msg: Msg) {
        let deliver_at = self.clock + self.latency();
        self.queue.push(Packet {
            from,
            to,
            msg,
            deliver_at,
        });
    }

    fn enqueue_outbound(&mut self, from: usize, outs: Vec<Outbound>) {
        for o in outs {
            let (to_id, msg) = match o {
                Outbound::RequestVote { to, request } => (to, Msg::RequestVote(request)),
                Outbound::AppendEntries { to, request } => (to, Msg::AppendEntries(request)),
            };
            let to = self.idx(&to_id);
            self.push(from, to, msg);
        }
    }

    fn deliver_due(&mut self) {
        let now = self.clock;
        let mut due = Vec::new();
        let mut i = 0;
        while i < self.queue.len() {
            if self.queue[i].deliver_at <= now {
                due.push(self.queue.swap_remove(i));
            } else {
                i += 1;
            }
        }
        // Deterministic delivery order.
        due.sort_by_key(|p| (p.deliver_at, p.from, p.to));
        for p in due {
            // A partition (or a crashed recipient) silently drops the message.
            if !self.connected(p.from, p.to) || self.nodes[p.to].is_none() {
                continue;
            }
            self.process(p);
        }
    }

    fn process(&mut self, p: Packet) {
        let now = self.clock;
        let from_id = self.ids[p.from].clone();
        match p.msg {
            Msg::RequestVote(req) => {
                let resp = match &mut self.nodes[p.to] {
                    Some(n) => n.handle_request_vote(req, now).unwrap(),
                    None => return,
                };
                self.push(p.to, p.from, Msg::VoteResp(resp));
            }
            Msg::AppendEntries(req) => {
                let resp = match &mut self.nodes[p.to] {
                    Some(n) => n.handle_append_entries(req, now).unwrap(),
                    None => return,
                };
                self.push(p.to, p.from, Msg::AppendResp(resp));
            }
            Msg::VoteResp(resp) => {
                let outs = match &mut self.nodes[p.to] {
                    Some(n) => n.handle_request_vote_response(from_id, resp, now).unwrap(),
                    None => return,
                };
                self.enqueue_outbound(p.to, outs);
            }
            Msg::AppendResp(resp) => {
                let outs = match &mut self.nodes[p.to] {
                    Some(n) => n
                        .handle_append_entries_response(from_id, resp, now)
                        .unwrap(),
                    None => return,
                };
                self.enqueue_outbound(p.to, outs);
            }
        }
    }

    fn tick_all(&mut self) {
        let now = self.clock;
        for i in 0..self.nodes.len() {
            let outs = match &mut self.nodes[i] {
                Some(n) => n.tick(now).unwrap(),
                None => continue,
            };
            self.enqueue_outbound(i, outs);
        }
    }

    /// Advances simulated time by `dur`, ticking every live node and delivering
    /// due packets in small fixed steps.
    fn advance(&mut self, dur: Duration) {
        let target = self.clock + dur;
        while self.clock < target {
            self.clock += self.step;
            self.deliver_due();
            self.tick_all();
        }
    }

    fn leaders(&self) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&i| self.nodes[i].as_ref().is_some_and(|n| n.is_leader()))
            .collect()
    }

    /// Advances until a single leader exists, or panics after `rounds`.
    fn advance_until_stable_leader(&mut self, rounds: usize) -> usize {
        for _ in 0..rounds {
            self.advance(Duration::from_millis(50));
            let leaders = self.leaders();
            if leaders.len() == 1 {
                // Let one more heartbeat round settle followers.
                self.advance(Duration::from_millis(100));
                if self.leaders().len() == 1 {
                    return self.leaders()[0];
                }
            }
        }
        panic!("no stable leader after {rounds} rounds");
    }

    /// Attempts a client write on node `i`; returns whether it was accepted
    /// (i.e. `i` was the leader).
    fn propose(&mut self, i: usize, cmd: &[u8]) -> bool {
        let result = match &mut self.nodes[i] {
            Some(n) => n.propose(Bytes::copy_from_slice(cmd)),
            None => return false,
        };
        match result {
            Ok(outs) => {
                self.enqueue_outbound(i, outs);
                true
            }
            Err(_) => false,
        }
    }

    fn partition(&mut self, groups: &[&[usize]]) {
        self.groups = groups.iter().map(|g| g.iter().copied().collect()).collect();
    }

    fn heal(&mut self) {
        self.groups.clear();
    }

    /// Crashes node `i`: its volatile state is lost, but its persisted log +
    /// hard state remain on disk. In-flight messages *to* it are lost.
    fn crash(&mut self, i: usize) {
        self.nodes[i] = None; // drops the RaftNode → closes its redb file
        self.queue.retain(|p| p.from != i);
    }

    /// Restarts a crashed node `i`, recovering its persisted state from disk.
    fn restart(&mut self, i: usize) {
        let node = Self::spawn(
            &self.ids,
            i,
            &self.paths[i],
            self.seeds[i],
            &self.cfg,
            self.clock,
        );
        self.nodes[i] = Some(node);
    }

    /// The committed command sequence a live node has applied.
    fn committed(&self, i: usize) -> Vec<Bytes> {
        let node = self.nodes[i].as_ref().expect("node is live");
        let ci = node.commit_index().get();
        (1..=ci)
            .filter_map(|idx| node.log_entry(LogIndex::new(idx)).unwrap())
            .map(|e| e.command)
            .collect()
    }

    fn max_commit(&self) -> u64 {
        (0..self.nodes.len())
            .filter_map(|i| self.nodes[i].as_ref())
            .map(|n| n.commit_index().get())
            .max()
            .unwrap_or(0)
    }

    /// **The core safety oracle** (`docs/raft-notes.md` §8, State Machine
    /// Safety): every live node's committed log must be a prefix of every
    /// other's — no committed index ever holds different commands on two nodes.
    fn assert_no_divergence(&self) {
        let live: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| self.nodes[i].is_some())
            .collect();
        for &a in &live {
            let ca = self.committed(a);
            for &b in &live {
                let cb = self.committed(b);
                let m = ca.len().min(cb.len());
                assert_eq!(
                    &ca[..m],
                    &cb[..m],
                    "divergence: n{a} and n{b} disagree on a committed entry"
                );
            }
        }
    }
}

#[test]
fn replicates_and_commits_under_latency() {
    let mut sim = Sim::new(3, 1);
    let leader = sim.advance_until_stable_leader(40);
    for k in 0..8u32 {
        assert!(sim.propose(leader, &k.to_le_bytes()));
        sim.advance(Duration::from_millis(120));
    }
    sim.advance(Duration::from_secs(1));
    sim.assert_no_divergence();
    assert!(sim.max_commit() >= 8, "all eight writes committed");
}

#[test]
fn minority_partition_cannot_commit_and_heals_consistently() {
    let mut sim = Sim::new(5, 7);
    let leader = sim.advance_until_stable_leader(60);

    // Isolate the leader with one companion (a minority of 2); the other 3 form
    // a majority that can elect and commit.
    let companion = (0..5).find(|&i| i != leader).unwrap();
    let majority: Vec<usize> = (0..5).filter(|&i| i != leader && i != companion).collect();
    sim.partition(&[&[leader, companion], &majority]);
    sim.advance(Duration::from_secs(2));

    // A write to the old (now minority) leader must never commit on its side.
    let minority_commit_before = sim.nodes[leader].as_ref().unwrap().commit_index().get();
    sim.propose(leader, b"minority-write");
    sim.advance(Duration::from_secs(1));
    assert_eq!(
        sim.nodes[leader].as_ref().unwrap().commit_index().get(),
        minority_commit_before,
        "a minority leader cannot advance its commit index"
    );

    // The majority elects a leader and commits a write.
    let maj_leader = *majority
        .iter()
        .find(|&&i| sim.nodes[i].as_ref().unwrap().is_leader())
        .expect("majority side elected a leader");
    assert!(sim.propose(maj_leader, b"majority-write"));
    sim.advance(Duration::from_secs(1));
    let committed_in_majority = sim.max_commit();
    assert!(committed_in_majority >= 1);

    // Heal: the cluster reconverges to a single leader with no divergence.
    sim.heal();
    sim.advance(Duration::from_secs(3));
    assert_eq!(sim.leaders().len(), 1, "one leader after healing");
    sim.assert_no_divergence();
}

#[test]
fn leader_crash_triggers_reelection_without_divergence() {
    let mut sim = Sim::new(5, 13);
    let leader = sim.advance_until_stable_leader(60);
    for k in 0..4u32 {
        assert!(sim.propose(leader, &k.to_le_bytes()));
        sim.advance(Duration::from_millis(150));
    }
    let committed_before = sim.max_commit();
    assert!(committed_before >= 4);

    // Kill the leader.
    sim.crash(leader);
    let new_leader = sim.advance_until_stable_leader(80);
    assert_ne!(new_leader, leader);

    // The new leader keeps serving; nothing committed before the crash is lost.
    for k in 4..8u32 {
        assert!(sim.propose(new_leader, &k.to_le_bytes()));
        sim.advance(Duration::from_millis(150));
    }
    sim.advance(Duration::from_secs(1));
    sim.assert_no_divergence();
    assert!(
        sim.max_commit() >= committed_before,
        "no committed entry lost"
    );
}

#[test]
fn crashed_follower_restarts_and_catches_up() {
    let mut sim = Sim::new(3, 21);
    let leader = sim.advance_until_stable_leader(40);
    let follower = (0..3).find(|&i| i != leader).unwrap();

    for k in 0..5u32 {
        assert!(sim.propose(leader, &k.to_le_bytes()));
        sim.advance(Duration::from_millis(120));
    }
    // Crash a follower, keep committing on the (still-majority) leader, then bring
    // it back — it must catch up from its persisted log with no divergence.
    sim.crash(follower);
    sim.advance(Duration::from_millis(300));
    for k in 5..9u32 {
        assert!(sim.propose(leader, &k.to_le_bytes()));
        sim.advance(Duration::from_millis(120));
    }
    sim.restart(follower);
    sim.advance(Duration::from_secs(2));
    sim.assert_no_divergence();
    assert_eq!(
        sim.committed(follower).len() as u64,
        sim.nodes[leader].as_ref().unwrap().commit_index().get(),
        "restarted follower caught up to the leader's commit index"
    );
}

#[test]
fn soak_random_faults_never_diverge() {
    // Many client writes interleaved with random partitions, heals, crashes and
    // restarts — the committed history must stay linearizable throughout.
    let mut sim = Sim::new(5, 2024);
    let mut writes = 0u32;
    let mut fault_rng = Rng::seed_from_u64(999);

    for round in 0..60 {
        // Occasionally inject a fault.
        match fault_rng.gen_range_u64(6) {
            0 => {
                // Random 2/3 partition.
                let cut = (fault_rng.gen_range_u64(5)) as usize;
                let a: Vec<usize> = (0..5).filter(|&i| i != cut && i % 2 == 0).collect();
                let b: Vec<usize> = (0..5).filter(|&i| !a.contains(&i)).collect();
                if !a.is_empty() && !b.is_empty() {
                    sim.partition(&[&a, &b]);
                }
            }
            1 => sim.heal(),
            2 => {
                if sim.leaders().len() == 1 {
                    let l = sim.leaders()[0];
                    sim.crash(l);
                }
            }
            3 => {
                // Restart any crashed node.
                if let Some(i) = (0..5).find(|&i| sim.nodes[i].is_none()) {
                    sim.restart(i);
                }
            }
            _ => {}
        }

        // Give the cluster time to elect/replicate, then try a write.
        sim.advance(Duration::from_millis(120));
        let leaders = sim.leaders();
        if let Some(&l) = leaders.first() {
            if sim.propose(l, format!("w{round}").as_bytes()) {
                writes += 1;
            }
        }
        sim.advance(Duration::from_millis(120));

        // Safety must hold after every single round.
        sim.assert_no_divergence();
    }

    // Heal and let everything converge; safety still holds and progress happened.
    sim.heal();
    for _ in 0..5 {
        if sim.nodes.iter().any(|n| n.is_none()) {
            let i = (0..5).find(|&i| sim.nodes[i].is_none()).unwrap();
            sim.restart(i);
        }
        sim.advance(Duration::from_secs(1));
    }
    sim.advance(Duration::from_secs(2));
    sim.assert_no_divergence();
    assert!(
        writes > 0,
        "the cluster made progress across the fault sequence"
    );
    assert!(
        sim.max_commit() > 0,
        "entries were committed despite faults"
    );
}
