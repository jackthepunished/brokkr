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
    InstallSnapshot(brokkr_raft::InstallSnapshot),
    VoteResp(RequestVoteResponse),
    AppendResp(AppendEntriesResponse),
    SnapResp(brokkr_raft::InstallSnapshotResponse),
}

/// The sim's stand-in for a state-machine snapshot blob (I6): the committed
/// command history, length-prefixed. Self-describing so a node that receives
/// it over `InstallSnapshot` contributes the right prefix to the oracle.
fn encode_history(cmds: &[Bytes]) -> Bytes {
    let mut out = Vec::new();
    for c in cmds {
        out.extend_from_slice(&u32::try_from(c.len()).unwrap().to_le_bytes());
        out.extend_from_slice(c);
    }
    Bytes::from(out)
}

fn decode_history(mut blob: &[u8]) -> Vec<Bytes> {
    let mut cmds = Vec::new();
    while blob.len() >= 4 {
        let len = u32::from_le_bytes(blob[..4].try_into().unwrap()) as usize;
        blob = &blob[4..];
        cmds.push(Bytes::copy_from_slice(&blob[..len]));
        blob = &blob[len..];
    }
    cmds
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
    /// The first `founders` ids are the bootstrap voters; the rest are spare
    /// slots that join as learners during membership churn (I7c).
    founders: usize,
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
        Self::new_with_threshold(n, base_seed, Config::default().snapshot_threshold)
    }

    /// A sim whose nodes compact automatically once the committed log outgrows
    /// `snapshot_threshold` (I6 exit criteria run the whole campaign at 16).
    fn new_with_threshold(n: usize, base_seed: u64, snapshot_threshold: u64) -> Self {
        Self::new_with_spares(n, 0, base_seed, snapshot_threshold)
    }

    /// A sim with `founders` running voters plus `spares` reserved node slots
    /// (I7c): a spare stays offline until membership churn spawns it as a
    /// learner via [`Sim::try_add_learner`].
    fn new_with_spares(
        founders: usize,
        spares: usize,
        base_seed: u64,
        snapshot_threshold: u64,
    ) -> Self {
        let total = founders + spares;
        let t0 = Instant::now();
        let ids: Vec<NodeId> = (0..total)
            .map(|i| NodeId::new(format!("n{i}")).unwrap())
            .collect();
        let cfg = Config {
            snapshot_threshold,
            ..Config::default()
        };
        let mut dirs = Vec::new();
        let mut paths = Vec::new();
        let mut nodes = Vec::new();
        let mut seeds = Vec::new();
        for i in 0..total {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("raft.redb");
            let seed = base_seed.wrapping_mul(1103).wrapping_add(i as u64 * 31 + 1);
            let node = if i < founders {
                Some(Self::spawn(&ids, founders, i, &path, seed, &cfg, t0))
            } else {
                None // spare: joins later as a learner
            };
            dirs.push(dir);
            paths.push(path);
            nodes.push(node);
            seeds.push(seed);
        }
        Sim {
            ids,
            founders,
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

    /// (Re)constructs node `i`. Founders bootstrap as voters of the founding
    /// set; spares bootstrap as **learners** of it (I7c) — either way, any
    /// configuration recovered from their log or snapshot takes precedence.
    fn spawn(
        ids: &[NodeId],
        founders: usize,
        i: usize,
        path: &PathBuf,
        seed: u64,
        cfg: &Config,
        now: Instant,
    ) -> RaftNode {
        let log = RaftLog::open(path).unwrap();
        if i < founders {
            let peers: Vec<NodeId> = ids[..founders]
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
        } else {
            RaftNode::new_learner(
                ids[i].clone(),
                ids[..founders].to_vec(),
                log,
                Rng::seed_from_u64(seed),
                cfg.clone(),
                now,
            )
            .unwrap()
        }
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
                Outbound::InstallSnapshot { to, request } => (to, Msg::InstallSnapshot(request)),
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
            Msg::InstallSnapshot(req) => {
                let resp = match &mut self.nodes[p.to] {
                    Some(n) => n.handle_install_snapshot(req, now).unwrap(),
                    None => return,
                };
                self.push(p.to, p.from, Msg::SnapResp(resp));
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
            Msg::SnapResp(resp) => {
                let outs = match &mut self.nodes[p.to] {
                    Some(n) => n
                        .handle_install_snapshot_response(from_id, resp, now)
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
    /// due packets in small fixed steps. Every step also runs the compaction
    /// trigger, exactly as a shell would (I6): any node whose committed log
    /// outgrew the threshold snapshots its state machine — here, its committed
    /// command history — and compacts.
    fn advance(&mut self, dur: Duration) {
        let target = self.clock + dur;
        while self.clock < target {
            self.clock += self.step;
            self.deliver_due();
            self.tick_all();
            self.maybe_compact();
        }
    }

    /// The shell-side snapshot trigger: compact every live node that reports
    /// [`RaftNode::needs_snapshot`], using its committed history as the blob.
    fn maybe_compact(&mut self) {
        for i in 0..self.nodes.len() {
            let needs = self.nodes[i].as_ref().is_some_and(|n| n.needs_snapshot());
            if !needs {
                continue;
            }
            let blob = encode_history(&self.committed(i));
            if let Some(n) = self.nodes[i].as_mut() {
                n.compact(blob).unwrap();
            }
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
        let now = self.clock;
        let result = match &mut self.nodes[i] {
            Some(n) => n.propose(Bytes::copy_from_slice(cmd), now),
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
            self.founders,
            i,
            &self.paths[i],
            self.seeds[i],
            &self.cfg,
            self.clock,
        );
        self.nodes[i] = Some(node);
    }

    // --- membership churn (I7c) -------------------------------------------

    /// The voter set the (presumed) leader `l` currently operates under.
    fn leader_voters(&self, l: usize) -> BTreeSet<NodeId> {
        self.nodes[l]
            .as_ref()
            .unwrap()
            .active_config()
            .voters
            .clone()
    }

    /// Spawns spare `i` (if offline) and asks leader `l` to add it as a
    /// learner. Returns whether the proposal was accepted.
    fn try_add_learner(&mut self, l: usize, spare: usize) -> bool {
        if self.nodes[spare].is_none() {
            self.restart(spare); // first spawn: bootstraps as a learner
        }
        let id = self.ids[spare].clone();
        let now = self.clock;
        let result = match &mut self.nodes[l] {
            Some(n) => n.propose_add_learner(id, now),
            None => return false,
        };
        match result {
            Ok(outs) => {
                self.enqueue_outbound(l, outs);
                true
            }
            Err(_) => false,
        }
    }

    /// Asks leader `l` to move the voter set to `voters` (joint consensus).
    /// Returns whether the proposal was accepted (the catch-up gate or the
    /// one-in-flight rule may refuse it — callers just retry later).
    fn try_conf_change(&mut self, l: usize, voters: BTreeSet<NodeId>) -> bool {
        let now = self.clock;
        let result = match &mut self.nodes[l] {
            Some(n) => n.propose_conf_change(voters, now),
            None => return false,
        };
        match result {
            Ok(outs) => {
                self.enqueue_outbound(l, outs);
                true
            }
            Err(_) => false,
        }
    }

    /// The committed command sequence a live node has applied: the history
    /// encoded in its snapshot blob (if any), then the committed log tail.
    fn committed(&self, i: usize) -> Vec<Bytes> {
        let node = self.nodes[i].as_ref().expect("node is live");
        let mut cmds = match node.snapshot().unwrap() {
            Some((_, blob)) => decode_history(&blob),
            None => Vec::new(),
        };
        let start = node
            .snapshot_meta()
            .map(|m| m.last_included_index.get())
            .unwrap_or(0)
            + 1;
        let ci = node.commit_index().get();
        for idx in start..=ci {
            if let Some(e) = node.log_entry(LogIndex::new(idx)).unwrap() {
                // Only commands produce state-machine output; no-op and
                // config entries (I7) contribute nothing to applied history.
                if let Some(c) = e.command() {
                    cmds.push(c.clone());
                }
            }
        }
        cmds
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
fn constant_compaction_preserves_history() {
    // threshold 16 with dozens of writes: every node snapshots repeatedly,
    // and the committed history (snapshot blob + log tail) never diverges.
    let mut sim = Sim::new_with_threshold(3, 42, 16);
    let leader = sim.advance_until_stable_leader(40);
    for k in 0..48u32 {
        assert!(sim.propose(leader, format!("w{k}").as_bytes()));
        sim.advance(Duration::from_millis(60));
        if k % 8 == 7 {
            sim.assert_no_divergence();
        }
    }
    sim.advance(Duration::from_secs(1));
    sim.assert_no_divergence();
    assert!(sim.max_commit() >= 48);
    for i in 0..3 {
        let node = sim.nodes[i].as_ref().unwrap();
        assert!(
            node.snapshot_meta().is_some(),
            "n{i} compacted at least once under threshold 16"
        );
        assert_eq!(sim.committed(i).len() as u64, node.commit_index().get());
    }
}

#[test]
fn crashed_follower_catches_up_via_install_snapshot() {
    // A follower misses enough writes that the leader compacts past its log:
    // catch-up can only happen through InstallSnapshot, and the restarted
    // node's history must match (restore-from-snapshot + tail replay, P9).
    let mut sim = Sim::new_with_threshold(3, 77, 16);
    let leader = sim.advance_until_stable_leader(40);
    let follower = (0..3).find(|&i| i != leader).unwrap();

    for k in 0..6u32 {
        assert!(sim.propose(leader, format!("a{k}").as_bytes()));
        sim.advance(Duration::from_millis(60));
    }
    sim.crash(follower);
    // 30 more writes: the survivors commit and compact (threshold 16), so the
    // crashed node's next entries no longer exist in anyone's log.
    for k in 0..30u32 {
        assert!(sim.propose(leader, format!("b{k}").as_bytes()));
        sim.advance(Duration::from_millis(60));
    }
    let leader_snap = sim.nodes[leader]
        .as_ref()
        .unwrap()
        .snapshot_meta()
        .expect("leader compacted during the follower's outage");
    assert!(
        leader_snap.last_included_index.get() > 6,
        "compaction moved past the crashed follower's log"
    );

    sim.restart(follower);
    sim.advance(Duration::from_secs(3));
    sim.assert_no_divergence();

    let f = sim.nodes[follower].as_ref().unwrap();
    assert!(
        f.snapshot_meta()
            .is_some_and(|m| m.last_included_index.get() > 6),
        "the follower received a snapshot, not just log entries"
    );
    assert_eq!(
        sim.committed(follower),
        sim.committed(leader),
        "restored-from-snapshot history matches the leader's"
    );
}

#[test]
fn soak_random_faults_never_diverge() {
    soak_random_faults(Config::default().snapshot_threshold);
}

/// I6 exit criteria: the full I5 fault campaign stays green while snapshots
/// are exercised constantly (`snapshot_threshold = 16`).
#[test]
fn soak_random_faults_with_constant_compaction_never_diverge() {
    soak_random_faults(16);
}

fn soak_random_faults(snapshot_threshold: u64) {
    // Many client writes interleaved with random partitions, heals, crashes and
    // restarts — the committed history must stay linearizable throughout.
    let mut sim = Sim::new_with_threshold(5, 2024, snapshot_threshold);
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

/// **The I7 exit criterion** (plan §17 task 5): the fault campaign stays green
/// with membership churn added to the mix — spares join as learners, get
/// promoted through joint consensus once the catch-up gate passes, and
/// founding voters retire — while compaction runs constantly (threshold 16)
/// and partitions/crashes/restarts fire at random. The linearizability oracle
/// must hold after every round.
#[test]
fn soak_random_faults_with_membership_churn() {
    let mut sim = Sim::new_with_spares(5, 2, 3033, 16);
    let mut fault_rng = Rng::seed_from_u64(4242);
    let mut writes = 0u32;
    // The churn plan, attempted in order with retries (proposals may be
    // refused by the one-in-flight rule or the catch-up gate, or lost to a
    // fault — the next round tries again):
    //   0: add n5 as learner       3: add n6 as learner
    //   1: promote n5 to voter     4: promote n6 to voter
    //   2: retire a founder        5: retire another founder
    let mut step = 0usize;

    for round in 0..120 {
        match fault_rng.gen_range_u64(8) {
            0 => {
                // Random 2/3-ish partition across every slot (spares too).
                let cut = (fault_rng.gen_range_u64(5)) as usize;
                let a: Vec<usize> = (0..5).filter(|&i| i != cut && i % 2 == 0).collect();
                let b: Vec<usize> = (0..7).filter(|&i| !a.contains(&i)).collect();
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
                // Revive any offline slot. A never-added spare comes up as an
                // idle learner and simply waits.
                if let Some(i) = (0..7).find(|&i| sim.nodes[i].is_none()) {
                    sim.restart(i);
                }
            }
            _ => {}
        }

        sim.advance(Duration::from_millis(120));

        // One churn attempt per round, through the current leader.
        if sim.leaders().len() == 1 {
            let l = sim.leaders()[0];
            let voters = sim.leader_voters(l);
            let leader_id = sim.nodes[l].as_ref().unwrap().id().clone();
            match step {
                0 | 3 => {
                    let spare = if step == 0 { 5 } else { 6 };
                    if sim.try_add_learner(l, spare) {
                        step += 1;
                    }
                }
                1 | 4 => {
                    let spare = if step == 1 { 5 } else { 6 };
                    let mut v = voters.clone();
                    v.insert(sim.ids[spare].clone());
                    if sim.try_conf_change(l, v) {
                        step += 1;
                    }
                }
                2 | 5 => {
                    // Retire the lowest-numbered founding voter that is not
                    // the current leader, keeping at least 4 voters.
                    if voters.len() >= 4 {
                        let victim = (0..5)
                            .map(|i| sim.ids[i].clone())
                            .find(|id| voters.contains(id) && *id != leader_id);
                        if let Some(victim) = victim {
                            let mut v = voters.clone();
                            v.remove(&victim);
                            if sim.try_conf_change(l, v) {
                                step += 1;
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        sim.advance(Duration::from_millis(120));
        let leaders = sim.leaders();
        if let Some(&l) = leaders.first() {
            if sim.propose(l, format!("w{round}").as_bytes()) {
                writes += 1;
            }
        }
        sim.advance(Duration::from_millis(120));

        // Safety must hold after every single round, churn included.
        sim.assert_no_divergence();
    }

    // Heal, revive everything, converge; safety and progress must hold.
    sim.heal();
    for _ in 0..5 {
        if let Some(i) = (0..7).find(|&i| sim.nodes[i].is_none()) {
            sim.restart(i);
        }
        sim.advance(Duration::from_secs(1));
    }
    sim.advance(Duration::from_secs(2));
    sim.assert_no_divergence();

    assert!(
        step >= 4,
        "churn made real progress (add → promote → retire → add), stalled at step {step}"
    );
    assert!(writes > 0, "the cluster kept serving writes through churn");
    assert!(
        sim.max_commit() > 16,
        "compaction was exercised during churn"
    );
}
