//! The Raft consensus node — leader election (I3) and log replication (I4).
//!
//! [`RaftNode`] is the single-owner state machine at the heart of ADR 0013 D4's
//! actor model: it owns all of its state (role, persistent hard state, log,
//! election timers, per-peer replication indices) and is driven by explicit
//! calls — `tick` for the passage of time, `handle_*` for inbound RPCs and
//! responses, and `propose` for client writes. There are **no locks**; the async
//! event-loop shell (which wires these methods to a [`Transport`] and a real
//! clock) is added with the simulation suite in milestone I5, where it can be
//! tested under simulated time.
//!
//! Keeping the logic in synchronous methods that *return* the messages to send
//! (rather than performing I/O) is what makes consensus **deterministically
//! testable**: the tests below drive whole clusters of nodes by hand with an
//! injected clock and seeded RNG, no async runtime required.
//!
//! Everything here follows `docs/raft-notes.md`: §2.2 (terms), §4–§4.2 (election
//! and RequestVote), §6 (election restriction), §5.1/§5.3 (the AppendEntries
//! consistency check, conflict-only truncation, and the `nextIndex`/`matchIndex`
//! back-off), and §7 (the current-term commit rule — the **Figure-8** safety
//! property). The recommended start-of-term no-op entry is deferred to the
//! linearizable-read work; it is a read-safety/latency optimization, not a
//! replication-safety requirement (see [`RaftNode::propose`] and `become_leader`).
//!
//! [`Transport`]: crate::Transport

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::error::RaftError;
use crate::rng::Rng;
use crate::state::HardState;
use crate::storage::RaftLog;
use crate::transport::{
    AppendEntries, AppendEntriesResponse, InstallSnapshot, InstallSnapshotResponse, RequestVote,
    RequestVoteResponse,
};
use crate::types::{ClusterConfig, EntryPayload, LogEntry, LogIndex, NodeId, SnapshotMeta, Term};

/// Timing parameters for a node. Defaults match the paper's 150–300 ms election
/// window (`docs/raft-notes.md` §4.1).
#[derive(Debug, Clone)]
pub struct Config {
    /// Lower bound of the randomized election timeout.
    pub min_election_timeout: Duration,
    /// Upper bound of the randomized election timeout.
    pub max_election_timeout: Duration,
    /// How often a leader emits heartbeats. Must be `≪ min_election_timeout`.
    pub heartbeat_interval: Duration,
    /// Compaction trigger (I6): once more than this many committed entries sit
    /// above the last snapshot, [`RaftNode::needs_snapshot`] reports `true` and
    /// the shell should supply a state-machine snapshot via
    /// [`RaftNode::compact`].
    pub snapshot_threshold: u64,
    /// Promotion catch-up gate (I7c, thesis ch. 4): a configuration change may
    /// add a voter only if the leader has replicated to it to within this many
    /// entries of its own last log index. Without the gate, promoting a fresh
    /// node adds a voter that cannot ack anything for a long time and can
    /// stall commitment.
    pub catch_up_margin: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            min_election_timeout: Duration::from_millis(150),
            max_election_timeout: Duration::from_millis(300),
            heartbeat_interval: Duration::from_millis(50),
            snapshot_threshold: 8192,
            catch_up_margin: 256,
        }
    }
}

/// Per-peer replication bookkeeping a leader keeps (`docs/raft-notes.md` §3).
#[derive(Debug, Clone, PartialEq, Eq)]
struct LeaderState {
    /// For each peer, the index of the next log entry to send it (optimistic;
    /// initialized to the leader's `last_index + 1`).
    next_index: BTreeMap<NodeId, LogIndex>,
    /// For each peer, the highest index known to be replicated on it (truth;
    /// monotonic, initialized to 0).
    match_index: BTreeMap<NodeId, LogIndex>,
}

/// The server's current role (`docs/raft-notes.md` §2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Role {
    Follower,
    /// Campaigning; tracks the set of nodes (including self) that granted a vote.
    Candidate {
        votes: BTreeSet<NodeId>,
    },
    Leader(LeaderState),
}

/// A message the node wants sent to a peer as a result of a `tick` or a handler.
/// The driver (or a test) is responsible for delivering it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outbound {
    /// Send a `RequestVote` to `to`.
    RequestVote {
        /// Recipient.
        to: NodeId,
        /// The request.
        request: RequestVote,
    },
    /// Send an `AppendEntries` (a heartbeat in I3) to `to`.
    AppendEntries {
        /// Recipient.
        to: NodeId,
        /// The request.
        request: AppendEntries,
    },
    /// Send an `InstallSnapshot` to `to` — its `nextIndex` was compacted away
    /// (I6). Single-shot: the whole blob in one request (chunking is noted as
    /// future work; entries are small KV commands).
    InstallSnapshot {
        /// Recipient.
        to: NodeId,
        /// The request.
        request: InstallSnapshot,
    },
}

/// Tracks the cluster configurations visible in the log (I7b, Raft §6).
///
/// The paper's rule: a server uses the **latest configuration in its log** —
/// committed or not — for all decisions, from the moment the entry is
/// *appended*. Truncating a conflicting suffix therefore rolls the active
/// configuration back (P10, the nastiest edge), and compaction folds config
/// entries into the snapshot's config.
#[derive(Debug, Clone)]
struct ConfigTracker {
    /// The configuration as of the snapshot / bootstrap.
    base: ClusterConfig,
    /// The log index `base` corresponds to (the snapshot index, or 0).
    base_index: LogIndex,
    /// Config entries still present in the log, ascending by index.
    in_log: Vec<(LogIndex, ClusterConfig)>,
}

impl ConfigTracker {
    fn new(base: ClusterConfig, base_index: LogIndex) -> Self {
        ConfigTracker {
            base,
            base_index,
            in_log: Vec::new(),
        }
    }

    /// The configuration currently in force (the latest in the log).
    fn active(&self) -> &ClusterConfig {
        self.in_log.last().map(|(_, c)| c).unwrap_or(&self.base)
    }

    /// The index of the entry that produced the active configuration.
    fn latest_index(&self) -> LogIndex {
        self.in_log
            .last()
            .map(|(i, _)| *i)
            .unwrap_or(self.base_index)
    }

    /// The configuration in effect at `index` (for snapshotting).
    fn config_at(&self, index: LogIndex) -> &ClusterConfig {
        self.in_log
            .iter()
            .rev()
            .find(|(i, _)| *i <= index)
            .map(|(_, c)| c)
            .unwrap_or(&self.base)
    }

    /// Records a config entry appended at `index`. Anything previously known
    /// at or beyond `index` is superseded (an overwrite is truncate+append).
    fn on_append(&mut self, index: LogIndex, config: ClusterConfig) {
        self.in_log.retain(|(i, _)| *i < index);
        self.in_log.push((index, config));
    }

    /// P10: truncating the suffix from `from` rolls back any configuration
    /// the truncated entries carried.
    fn on_truncate(&mut self, from: LogIndex) {
        self.in_log.retain(|(i, _)| *i < from);
    }

    /// Folds every config entry a snapshot at `to` covers into the base.
    fn on_compact(&mut self, to: LogIndex) {
        self.base = self.config_at(to).clone();
        self.base_index = to;
        self.in_log.retain(|(i, _)| *i > to);
    }

    /// Adopts an installed snapshot's configuration as the new base; when the
    /// whole log was discarded, any tracked config entries go with it.
    fn on_install_snapshot(&mut self, config: ClusterConfig, index: LogIndex, log_wiped: bool) {
        // An empty config comes from a pre-I7b sender: keep the current base
        // rather than adopting a voterless cluster.
        if !config.voters.is_empty() {
            self.base = config;
        }
        self.base_index = index;
        if log_wiped {
            self.in_log.clear();
        } else {
            self.in_log.retain(|(i, _)| *i > index);
        }
    }
}

/// A Raft node: the consensus state machine for one server.
#[derive(Debug)]
pub struct RaftNode {
    id: NodeId,
    /// The cluster configurations visible in the log; `configs.active()` is
    /// the membership every decision uses (I7b, Raft §6).
    configs: ConfigTracker,
    role: Role,
    /// Durable log + hard state.
    log: RaftLog,
    /// In-memory copy of the persisted hard state; every mutation is written
    /// through to `log` before the corresponding reply (persist-before-respond).
    hard: HardState,
    /// Highest index known committed (volatile, but floored at the snapshot's
    /// `last_included_index` on recovery — snapshotted entries are
    /// definitionally committed and applied; P9).
    commit_index: LogIndex,
    /// In-memory copy of the persisted snapshot metadata (the blob stays on
    /// disk); kept in sync by `compact` and `handle_install_snapshot`. Cached
    /// because replication consults it on every heartbeat.
    snapshot: Option<SnapshotMeta>,
    /// The leader this node currently recognizes, if any.
    leader_id: Option<NodeId>,
    /// When this node last heard from a valid leader (volatile). Backs the
    /// thesis §4.2.3 vote guard that keeps removed servers (I7b) from
    /// disrupting the cluster they left.
    last_leader_contact: Option<Instant>,
    rng: Rng,
    config: Config,
    /// When, if still not a leader, this node will start an election.
    election_deadline: Instant,
    /// When, if a leader, this node will next emit heartbeats.
    heartbeat_deadline: Instant,
}

impl RaftNode {
    /// Creates a follower, recovering persisted hard state from `log`, and arms
    /// the first (randomized) election timer relative to `now`.
    ///
    /// `peers` seeds the **bootstrap** configuration (voters = `peers` +
    /// self). It only matters until a configuration is learned from the
    /// snapshot or a log entry — those always take precedence (Raft §6).
    pub fn new(
        id: NodeId,
        peers: Vec<NodeId>,
        log: RaftLog,
        rng: Rng,
        config: Config,
        now: Instant,
    ) -> Result<Self, RaftError> {
        let bootstrap = {
            let mut voters: BTreeSet<NodeId> = peers.into_iter().collect();
            voters.insert(id.clone());
            ClusterConfig::single(voters)
        };
        Self::with_bootstrap(id, bootstrap, log, rng, config, now)
    }

    /// Creates a node that joins as a **learner** (I7c, thesis ch. 4): its
    /// bootstrap configuration lists `voters` as the cluster and itself only
    /// as a non-voting learner, so it never campaigns or counts toward quorum
    /// until a replicated configuration promotes it. This is how a fresh node
    /// enters an existing cluster without stalling it.
    pub fn new_learner(
        id: NodeId,
        voters: Vec<NodeId>,
        log: RaftLog,
        rng: Rng,
        config: Config,
        now: Instant,
    ) -> Result<Self, RaftError> {
        let bootstrap = ClusterConfig {
            voters: voters.into_iter().collect(),
            old_voters: None,
            learners: [id.clone()].into_iter().collect(),
        };
        Self::with_bootstrap(id, bootstrap, log, rng, config, now)
    }

    fn with_bootstrap(
        id: NodeId,
        bootstrap: ClusterConfig,
        log: RaftLog,
        rng: Rng,
        config: Config,
        now: Instant,
    ) -> Result<Self, RaftError> {
        let hard = log.load_hard_state()?;
        let snapshot = log.snapshot_meta()?;
        // P9: everything a snapshot covers is committed and applied; the commit
        // index must never regress below it across a restart.
        let commit_floor = snapshot
            .as_ref()
            .map(|m| m.last_included_index)
            .unwrap_or(LogIndex::ZERO);

        let (base, base_index) = match &snapshot {
            // A pre-I7b snapshot has an empty ("unknown") config: fall back
            // to the bootstrap membership.
            Some(m) if !m.config.voters.is_empty() => (m.config.clone(), m.last_included_index),
            Some(m) => (bootstrap, m.last_included_index),
            None => (bootstrap, LogIndex::ZERO),
        };
        let mut configs = ConfigTracker::new(base, base_index);
        // Recover config entries surviving in the log (startup-only scan).
        let first = log.first_index()?;
        if first != LogIndex::ZERO {
            for entry in log.entries_from(first)? {
                if let EntryPayload::Config(c) = &entry.payload {
                    configs.on_append(entry.index, c.clone());
                }
            }
        }

        let mut node = RaftNode {
            id,
            configs,
            role: Role::Follower,
            log,
            hard,
            commit_index: commit_floor,
            snapshot,
            leader_id: None,
            last_leader_contact: None,
            rng,
            config,
            election_deadline: now,
            heartbeat_deadline: now,
        };
        node.arm_election_timer(now);
        Ok(node)
    }

    // --- accessors (for the driver and tests) ----------------------------

    /// This node's identifier.
    pub fn id(&self) -> &NodeId {
        &self.id
    }

    /// The node's current term.
    pub fn current_term(&self) -> Term {
        self.hard.current_term
    }

    /// Whether this node currently believes it is the leader.
    pub fn is_leader(&self) -> bool {
        matches!(self.role, Role::Leader(_))
    }

    /// The highest log index known to be committed.
    pub fn commit_index(&self) -> LogIndex {
        self.commit_index
    }

    /// The index of this node's last log entry (0 if empty).
    pub fn last_log_index(&self) -> Result<LogIndex, RaftError> {
        self.log.last_index()
    }

    /// The log entry at `index`, or `None` if absent.
    pub fn log_entry(&self, index: LogIndex) -> Result<Option<LogEntry>, RaftError> {
        self.log.get(index)
    }

    /// Whether this node is currently a candidate.
    pub fn is_candidate(&self) -> bool {
        matches!(self.role, Role::Candidate { .. })
    }

    /// Whether this node is currently a follower.
    pub fn is_follower(&self) -> bool {
        matches!(self.role, Role::Follower)
    }

    /// The leader this node currently recognizes, if any.
    pub fn leader_id(&self) -> Option<&NodeId> {
        self.leader_id.as_ref()
    }

    /// Who this node voted for in its current term, if anyone.
    pub fn voted_for(&self) -> Option<&NodeId> {
        self.hard.voted_for.as_ref()
    }

    /// When this node will next start an election if it does not hear from a
    /// leader (a driver uses this to schedule the next `tick`).
    pub fn election_deadline(&self) -> Instant {
        self.election_deadline
    }

    /// The installed snapshot's metadata, if any (I6).
    pub fn snapshot_meta(&self) -> Option<SnapshotMeta> {
        self.snapshot.clone()
    }

    /// The cluster configuration currently in force — the latest one in the
    /// log, committed or not (I7b, Raft §6).
    pub fn active_config(&self) -> &ClusterConfig {
        self.configs.active()
    }

    /// The installed snapshot (metadata + opaque blob), if any. The blob is
    /// read from storage; the shell uses it to restore a state machine.
    pub fn snapshot(&self) -> Result<Option<(SnapshotMeta, Bytes)>, RaftError> {
        self.log.snapshot()
    }

    /// The lowest index still present in the log ([`LogIndex::ZERO`] when
    /// empty). After compaction this is `snapshot.last_included_index + 1`.
    pub fn first_log_index(&self) -> Result<LogIndex, RaftError> {
        self.log.first_index()
    }

    /// The index of the last entry the current snapshot covers (ZERO if none).
    fn snapshot_index(&self) -> LogIndex {
        self.snapshot
            .as_ref()
            .map(|m| m.last_included_index)
            .unwrap_or(LogIndex::ZERO)
    }

    /// Every member the leader replicates to: voters of both configurations
    /// (when joint) plus learners, excluding self.
    fn replication_targets(&self) -> Vec<NodeId> {
        let active = self.configs.active();
        let mut members: BTreeSet<NodeId> = active.voters.iter().cloned().collect();
        if let Some(old) = &active.old_voters {
            members.extend(old.iter().cloned());
        }
        members.extend(active.learners.iter().cloned());
        members.remove(&self.id);
        members.into_iter().collect()
    }

    /// Every member whose vote can count: voters of both configurations when
    /// joint (learners never vote), excluding self.
    fn vote_targets(&self) -> Vec<NodeId> {
        let active = self.configs.active();
        let mut voters: BTreeSet<NodeId> = active.voters.iter().cloned().collect();
        if let Some(old) = &active.old_voters {
            voters.extend(old.iter().cloned());
        }
        voters.remove(&self.id);
        voters.into_iter().collect()
    }

    /// The term at `index`: from the log entry if present, or from the
    /// snapshot metadata when `index` is exactly the snapshot's last included
    /// entry. `None` for indices compacted away below the snapshot (their
    /// terms are gone by design) or beyond the end of the log.
    fn term_at(&self, index: LogIndex) -> Result<Option<Term>, RaftError> {
        if let Some(entry) = self.log.get(index)? {
            return Ok(Some(entry.term));
        }
        match &self.snapshot {
            Some(meta) if meta.last_included_index == index => Ok(Some(meta.last_included_term)),
            _ => Ok(None),
        }
    }

    // --- time -------------------------------------------------------------

    /// Advances logical time to `now`, returning any messages to send.
    ///
    /// - A non-leader whose election timer has expired starts an election.
    /// - A leader whose heartbeat timer has expired replicates to all peers
    ///   (an empty `AppendEntries` doubles as a heartbeat).
    pub fn tick(&mut self, now: Instant) -> Result<Vec<Outbound>, RaftError> {
        if self.is_leader() {
            if now >= self.heartbeat_deadline {
                self.arm_heartbeat_timer(now);
                return self.replicate_all();
            }
            Ok(Vec::new())
        } else {
            if now >= self.election_deadline {
                // Only voters campaign (I7c). A learner — or a node whose
                // active configuration no longer names it at all — re-arms
                // its timer and stays quiet instead of bumping terms it can
                // never win with.
                if !self.is_voter() {
                    self.arm_election_timer(now);
                    return Ok(Vec::new());
                }
                return self.start_election(now);
            }
            Ok(Vec::new())
        }
    }

    /// Whether this node may vote and campaign under the active configuration:
    /// it is in `voters`, or in `old_voters` while a joint change is in flight.
    fn is_voter(&self) -> bool {
        let active = self.configs.active();
        active.voters.contains(&self.id)
            || active
                .old_voters
                .as_ref()
                .is_some_and(|old| old.contains(&self.id))
    }

    fn arm_election_timer(&mut self, now: Instant) {
        let timeout = self.rng.election_timeout(
            self.config.min_election_timeout,
            self.config.max_election_timeout,
        );
        self.election_deadline = now + timeout;
    }

    fn arm_heartbeat_timer(&mut self, now: Instant) {
        self.heartbeat_deadline = now + self.config.heartbeat_interval;
    }

    // --- term handling ----------------------------------------------------

    /// The universal rule (`docs/raft-notes.md` §2.2): if `term` exceeds our
    /// own, adopt it, revert to follower, and clear the vote — persisting the
    /// new hard state. Returns whether a step-down occurred.
    fn observe_term(&mut self, term: Term, now: Instant) -> Result<bool, RaftError> {
        if term > self.hard.current_term {
            self.hard = self.hard.stepped_to(term);
            self.role = Role::Follower;
            self.leader_id = None;
            self.log.save_hard_state(&self.hard)?;
            // Re-arm the election timer. A node that just stepped down is a fresh
            // follower in the new term; a leader's `election_deadline` is stale
            // (leaders track only the heartbeat timer), so without this the very
            // next `tick` would immediately start an election and fight the node
            // that just demonstrated the higher term.
            self.arm_election_timer(now);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // --- elections --------------------------------------------------------

    /// Starts an election (candidate rules, `docs/raft-notes.md` §4): bump the
    /// term, vote for self (persisted), and request votes from every voter in
    /// the active configuration (I7b — learners are never asked). A cluster
    /// whose sole voter is this node wins immediately.
    fn start_election(&mut self, now: Instant) -> Result<Vec<Outbound>, RaftError> {
        let new_term = self.hard.current_term.next();
        // Advance the term (clearing any prior vote), then vote for ourselves,
        // and persist before we count on that vote.
        self.hard = self.hard.stepped_to(new_term).voting_for(self.id.clone());
        self.log.save_hard_state(&self.hard)?;

        let mut votes = BTreeSet::new();
        votes.insert(self.id.clone());
        let self_quorum = self.configs.active().has_quorum(&votes);
        self.role = Role::Candidate { votes };
        self.leader_id = None;
        self.arm_election_timer(now);

        // A cluster whose quorum is satisfied by this node alone (e.g. a
        // single-voter bootstrap) elects immediately.
        if self_quorum {
            return self.become_leader(now);
        }

        let (last_log_index, last_log_term) = self.log.last_index_and_term()?;
        let request = RequestVote {
            term: new_term,
            candidate_id: self.id.clone(),
            last_log_index,
            last_log_term,
        };
        Ok(self
            .vote_targets()
            .into_iter()
            .map(|peer| Outbound::RequestVote {
                to: peer,
                request: request.clone(),
            })
            .collect())
    }

    fn become_leader(&mut self, now: Instant) -> Result<Vec<Outbound>, RaftError> {
        let last = self.log.last_index()?;
        let mut next_index = BTreeMap::new();
        let mut match_index = BTreeMap::new();
        for peer in self.replication_targets() {
            next_index.insert(peer.clone(), last.next());
            match_index.insert(peer, LogIndex::ZERO);
        }
        self.role = Role::Leader(LeaderState {
            next_index,
            match_index,
        });
        self.leader_id = Some(self.id.clone());
        self.arm_heartbeat_timer(now);
        // NOTE: Raft recommends appending a no-op entry here so the leader can
        // commit and learn its commit index for its term (docs/raft-notes.md §7,
        // §11). It is a read-safety / commit-latency optimization, not required
        // for replication *safety*; it is deferred to the linearizable-read work
        // (I8/read path). Until then a fresh leader commits its inherited entries
        // once a client `propose` provides a current-term entry that reaches a
        // majority — which is exactly the Figure-8 current-term commit rule.
        self.replicate_all()
    }

    /// Builds an `AppendEntries` for every replication target starting at its
    /// `nextIndex`. An up-to-date peer receives an empty (heartbeat) request.
    fn replicate_all(&self) -> Result<Vec<Outbound>, RaftError> {
        let Role::Leader(state) = &self.role else {
            return Ok(Vec::new());
        };
        let targets = self.replication_targets();
        let mut out = Vec::with_capacity(targets.len());
        for peer in &targets {
            out.push(self.append_entries_for(peer, state)?);
        }
        Ok(out)
    }

    /// Builds the `AppendEntries` to send `peer` given the leader `state` — or
    /// an `InstallSnapshot` (I6, Raft §7) when the entries the peer needs were
    /// compacted away (`nextIndex <= snapshot.last_included_index`).
    fn append_entries_for(
        &self,
        peer: &NodeId,
        state: &LeaderState,
    ) -> Result<Outbound, RaftError> {
        let next = state
            .next_index
            .get(peer)
            .copied()
            .unwrap_or_else(|| LogIndex::new(1));
        if next <= self.snapshot_index() {
            let (meta, data) = self.log.snapshot()?.ok_or_else(|| {
                RaftError::Snapshot("snapshot metadata cached but blob missing".to_string())
            })?;
            return Ok(Outbound::InstallSnapshot {
                to: peer.clone(),
                request: InstallSnapshot {
                    term: self.hard.current_term,
                    leader_id: self.id.clone(),
                    last_included_index: meta.last_included_index,
                    last_included_term: meta.last_included_term,
                    offset: 0,
                    data,
                    done: true,
                    config: meta.config,
                },
            });
        }
        let prev_log_index = next.prev();
        let prev_log_term = if prev_log_index == LogIndex::ZERO {
            Term::ZERO
        } else {
            // `term_at` also resolves the snapshot boundary: for the first
            // entry after compaction, `prev` is the snapshot's last included
            // index, whose term lives in the snapshot metadata.
            self.term_at(prev_log_index)?.unwrap_or(Term::ZERO)
        };
        let entries = self.log.entries_from(next)?;
        Ok(Outbound::AppendEntries {
            to: peer.clone(),
            request: AppendEntries {
                term: self.hard.current_term,
                leader_id: self.id.clone(),
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit: self.commit_index,
            },
        })
    }

    /// Rebuilds the `AppendEntries` for a single peer (after its `nextIndex`
    /// moved on a success or a back-off).
    fn replicate_to(&self, peer: &NodeId) -> Result<Vec<Outbound>, RaftError> {
        let Role::Leader(state) = &self.role else {
            return Ok(Vec::new());
        };
        Ok(vec![self.append_entries_for(peer, state)?])
    }

    /// Aligns the leader's per-peer bookkeeping with the active configuration
    /// (I7b): new members start optimistic (`next = last + 1`, `match = 0`);
    /// members no longer named by any configuration in force are dropped and
    /// stop being replicated to.
    fn refresh_leader_state(&mut self) -> Result<(), RaftError> {
        let targets: BTreeSet<NodeId> = self.replication_targets().into_iter().collect();
        let optimistic_next = self.log.last_index()?.next();
        if let Role::Leader(state) = &mut self.role {
            state.next_index.retain(|peer, _| targets.contains(peer));
            state.match_index.retain(|peer, _| targets.contains(peer));
            for target in targets {
                state
                    .next_index
                    .entry(target.clone())
                    .or_insert(optimistic_next);
                state.match_index.entry(target).or_insert(LogIndex::ZERO);
            }
        }
        Ok(())
    }

    // --- RequestVote (§4.2) ----------------------------------------------

    /// Handles an inbound `RequestVote` and returns the reply. Any granted vote
    /// (and any term bump) is persisted before returning (persist-before-respond).
    pub fn handle_request_vote(
        &mut self,
        request: RequestVote,
        now: Instant,
    ) -> Result<RequestVoteResponse, RaftError> {
        // Thesis §4.2.3 (leader stickiness): a server that believes a current
        // leader exists — it is one, or heard from one within the minimum
        // election timeout — disregards RequestVote entirely: it neither
        // updates its term nor grants. Without this, a server *removed* by a
        // membership change (I7b) keeps timing out and its ever-higher terms
        // would depose the legitimate leader of the cluster it left. A stale
        // minority leader still steps down via AppendEntries traffic, so
        // liveness is unharmed.
        if self.leader_is_current(now) {
            return Ok(RequestVoteResponse {
                term: self.hard.current_term,
                vote_granted: false,
            });
        }

        self.observe_term(request.term, now)?;
        let term = self.hard.current_term;

        // Rule 1: reject a stale term.
        if request.term < term {
            return Ok(RequestVoteResponse {
                term,
                vote_granted: false,
            });
        }

        // Rule 2: grant iff we have not voted for someone else this term AND the
        // candidate's log is at least as up-to-date as ours (the election
        // restriction, §6).
        let free_to_vote = match &self.hard.voted_for {
            None => true,
            Some(voted) => voted == &request.candidate_id,
        };
        let up_to_date =
            self.candidate_is_up_to_date(request.last_log_term, request.last_log_index)?;

        if free_to_vote && up_to_date {
            self.hard = self.hard.voting_for(request.candidate_id.clone());
            self.log.save_hard_state(&self.hard)?; // persist before replying
            self.arm_election_timer(now); // granting a vote counts as hearing out a candidate
            Ok(RequestVoteResponse {
                term,
                vote_granted: true,
            })
        } else {
            Ok(RequestVoteResponse {
                term,
                vote_granted: false,
            })
        }
    }

    /// Whether this node believes a current leader exists (thesis §4.2.3): it
    /// is the leader itself, or it heard from one within the minimum election
    /// timeout.
    fn leader_is_current(&self, now: Instant) -> bool {
        if self.is_leader() {
            return true;
        }
        self.last_leader_contact.is_some_and(|contact| {
            now.checked_duration_since(contact).unwrap_or_default()
                < self.config.min_election_timeout
        })
    }

    /// The election restriction (`docs/raft-notes.md` §6): the candidate's log is
    /// at least as up-to-date as ours iff its last `(term, index)` is
    /// lexicographically `>=` ours — a later last-term wins, ties break on the
    /// longer log.
    fn candidate_is_up_to_date(
        &self,
        candidate_last_term: Term,
        candidate_last_index: LogIndex,
    ) -> Result<bool, RaftError> {
        let (our_index, our_term) = self.log.last_index_and_term()?;
        Ok((candidate_last_term, candidate_last_index) >= (our_term, our_index))
    }

    /// Handles a peer's reply to our `RequestVote`. If it carries a higher term
    /// we step down; if it grants a vote and the active configuration's quorum
    /// is reached — majorities in **both** voter sets while a joint change is
    /// in flight (I7b, Raft §6) — we become leader.
    pub fn handle_request_vote_response(
        &mut self,
        from: NodeId,
        response: RequestVoteResponse,
        now: Instant,
    ) -> Result<Vec<Outbound>, RaftError> {
        if self.observe_term(response.term, now)? {
            return Ok(Vec::new()); // stepped down; no longer a candidate
        }
        // Ignore replies from an old term.
        if response.term != self.hard.current_term {
            return Ok(Vec::new());
        }

        let reached_quorum = if let Role::Candidate { votes } = &mut self.role {
            if response.vote_granted {
                votes.insert(from);
            }
            // `has_quorum` only counts voters, so stray grants from nodes
            // outside the configuration can never elect us.
            self.configs.active().has_quorum(votes)
        } else {
            false
        };

        if reached_quorum {
            self.become_leader(now)
        } else {
            Ok(Vec::new())
        }
    }

    // --- AppendEntries receiver (§5.1) -----------------------------------

    /// Handles an inbound `AppendEntries` — the five-step consistency check
    /// (`docs/raft-notes.md` §5.1). On a term match it recognizes the leader and
    /// resets the election timer, then checks the log at
    /// `prev_log_index`/`prev_log_term`, truncates only on a genuine conflict,
    /// appends new entries, and advances the commit index. Log mutations are
    /// durably committed before the reply.
    pub fn handle_append_entries(
        &mut self,
        request: AppendEntries,
        now: Instant,
    ) -> Result<AppendEntriesResponse, RaftError> {
        self.observe_term(request.term, now)?;
        let term = self.hard.current_term;

        // Step 1: reject a stale leader.
        if request.term < term {
            return Ok(Self::reject(term));
        }

        // Valid leader for this term: accept its authority, step down if we were
        // campaigning, and suppress our own election timer.
        self.role = Role::Follower;
        self.leader_id = Some(request.leader_id.clone());
        self.last_leader_contact = Some(now);
        self.arm_election_timer(now);

        // Step 2: our log must contain an entry at prev_log_index whose term
        // matches prev_log_term; otherwise ask the leader to back off.
        if !self.log_matches(request.prev_log_index, request.prev_log_term)? {
            let (conflict_term, conflict_index) = self.conflict_hint(request.prev_log_index)?;
            return Ok(AppendEntriesResponse {
                term,
                success: false,
                conflict_term,
                conflict_index,
                match_index: LogIndex::ZERO,
            });
        }

        // Steps 3 & 4: overwrite conflicting entries, append new ones.
        self.append_new_entries(&request.entries)?;

        // The highest index this reply guarantees now matches the leader.
        let match_index =
            LogIndex::new(request.prev_log_index.get() + request.entries.len() as u64);

        // Step 5: advance our commit index toward the leader's.
        if request.leader_commit > self.commit_index {
            self.commit_index = request.leader_commit.min(match_index);
        }

        Ok(AppendEntriesResponse {
            term,
            success: true,
            conflict_term: Term::ZERO,
            conflict_index: LogIndex::ZERO,
            match_index,
        })
    }

    fn reject(term: Term) -> AppendEntriesResponse {
        AppendEntriesResponse {
            term,
            success: false,
            conflict_term: Term::ZERO,
            conflict_index: LogIndex::ZERO,
            match_index: LogIndex::ZERO,
        }
    }

    /// Whether our log holds an entry at `prev_index` with term `prev_term`
    /// (trivially true for the empty prefix, `prev_index == 0`).
    ///
    /// Indices at or below our snapshot match by construction: everything a
    /// snapshot covers is committed, and committed entries agree on every log
    /// (Leader Completeness) — the boundary index itself is still checked
    /// against the snapshot's recorded term via `term_at`.
    fn log_matches(&self, prev_index: LogIndex, prev_term: Term) -> Result<bool, RaftError> {
        if prev_index == LogIndex::ZERO {
            return Ok(true);
        }
        if prev_index < self.snapshot_index() {
            return Ok(true);
        }
        Ok(self.term_at(prev_index)? == Some(prev_term))
    }

    /// The fast-backtrack hint (`docs/raft-notes.md` §5.3) for a failed check at
    /// `prev_index`: if our log is too short, point the leader just past our last
    /// entry; otherwise report the conflicting term and the first index we hold
    /// for it, so the leader can skip a whole term in one round trip.
    fn conflict_hint(&self, prev_index: LogIndex) -> Result<(Term, LogIndex), RaftError> {
        let last = self.log.last_index()?;
        if prev_index > last {
            return Ok((Term::ZERO, last.next()));
        }
        let conflict_term = self
            .log
            .get(prev_index)?
            .map(|e| e.term)
            .unwrap_or(Term::ZERO);
        let mut first = prev_index;
        while first > LogIndex::new(1) {
            let candidate = first.prev();
            match self.log.get(candidate)? {
                Some(e) if e.term == conflict_term => first = candidate,
                _ => break,
            }
        }
        Ok((conflict_term, first))
    }

    /// Steps 3 & 4 of the receiver: keep any incoming entry we already store with
    /// the same term; on the first genuine conflict, truncate from there and
    /// append the rest. Entries we already hold are never re-truncated — a
    /// delayed or duplicated request must not erase a committed suffix
    /// (`docs/raft-notes.md` §5.1).
    fn append_new_entries(&mut self, entries: &[LogEntry]) -> Result<(), RaftError> {
        let snapshot_index = self.snapshot_index();
        for (i, entry) in entries.iter().enumerate() {
            // Entries the snapshot already covers are committed and applied;
            // never re-append them into the compacted region (I6).
            if entry.index <= snapshot_index {
                continue;
            }
            match self.log.get(entry.index)? {
                Some(existing) if existing.term == entry.term => continue,
                Some(_) => {
                    self.log.truncate_from(entry.index)?;
                    // P10: the truncated suffix may have carried config
                    // entries — the active configuration rolls back with it.
                    self.configs.on_truncate(entry.index);
                    self.log.append_all(&entries[i..])?;
                    self.note_appended_configs(&entries[i..]);
                    return Ok(());
                }
                None => {
                    self.log.append_all(&entries[i..])?;
                    self.note_appended_configs(&entries[i..]);
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Configurations apply the moment they are appended (Raft §6): record
    /// every config entry in `entries` in the tracker.
    fn note_appended_configs(&mut self, entries: &[LogEntry]) {
        for entry in entries {
            if let EntryPayload::Config(c) = &entry.payload {
                self.configs.on_append(entry.index, c.clone());
            }
        }
    }

    // --- AppendEntries leader side (§5.3, §7) -----------------------------

    /// Handles a peer's reply to our `AppendEntries`. A higher term steps us
    /// down. On success we advance that peer's `matchIndex`/`nextIndex`, try to
    /// advance the commit index, and keep replicating if it is still behind. On
    /// failure we back off its `nextIndex` (using the conflict hint) and retry.
    pub fn handle_append_entries_response(
        &mut self,
        from: NodeId,
        response: AppendEntriesResponse,
        now: Instant,
    ) -> Result<Vec<Outbound>, RaftError> {
        if self.observe_term(response.term, now)? {
            return Ok(Vec::new());
        }
        // Only the current-term leader acts on these.
        if response.term != self.hard.current_term || !self.is_leader() {
            return Ok(Vec::new());
        }

        if response.success {
            self.record_match(&from, response.match_index);
            let mut outs = self.advance_commit_index(now)?;
            if self.peer_is_behind(&from)? {
                outs.extend(self.replicate_to(&from)?);
            }
            Ok(outs)
        } else {
            self.back_off(&from, response.conflict_index);
            self.replicate_to(&from)
        }
    }

    fn record_match(&mut self, peer: &NodeId, match_index: LogIndex) {
        if let Role::Leader(state) = &mut self.role {
            let entry = state
                .match_index
                .entry(peer.clone())
                .or_insert(LogIndex::ZERO);
            if match_index > *entry {
                *entry = match_index;
            }
            state.next_index.insert(peer.clone(), match_index.next());
        }
    }

    fn back_off(&mut self, peer: &NodeId, conflict_index: LogIndex) {
        if let Role::Leader(state) = &mut self.role {
            let target = conflict_index.max(LogIndex::new(1));
            state.next_index.insert(peer.clone(), target);
        }
    }

    fn peer_is_behind(&self, peer: &NodeId) -> Result<bool, RaftError> {
        let next = match &self.role {
            Role::Leader(state) => state
                .next_index
                .get(peer)
                .copied()
                .unwrap_or_else(|| LogIndex::new(1)),
            _ => return Ok(false),
        };
        Ok(next <= self.log.last_index()?)
    }

    /// The leader commit rule (`docs/raft-notes.md` §7 — the **Figure-8 rule**):
    /// advance `commitIndex` to the largest `N` with `N > commitIndex`, the
    /// active configuration's **quorum** holding index `N` (majorities in both
    /// voter sets while a joint change is in flight — I7b, Raft §6; learners
    /// never count), **and** `log[N].term == currentTerm`. That last clause is
    /// the whole point: a leader must never commit an entry from a *previous*
    /// term by replica count — such entries commit only indirectly, once a
    /// current-term entry commits over them.
    ///
    /// A moved commit index may demand configuration follow-ups (the returned
    /// messages): a committed C_old,new makes the leader append C_new, and a
    /// committed C_new that excludes the leader makes it step down.
    fn advance_commit_index(&mut self, now: Instant) -> Result<Vec<Outbound>, RaftError> {
        let current_term = self.hard.current_term;
        let last = self.log.last_index()?;
        let new_commit = {
            let state = match &self.role {
                Role::Leader(s) => s,
                _ => return Ok(Vec::new()),
            };
            let mut found = self.commit_index;
            let mut n = last;
            while n > self.commit_index {
                let mut acks: BTreeSet<NodeId> = state
                    .match_index
                    .iter()
                    .filter(|(_, m)| **m >= n)
                    .map(|(p, _)| p.clone())
                    .collect();
                acks.insert(self.id.clone());
                if self.configs.active().has_quorum(&acks) {
                    let term_n = self.log.get(n)?.map(|e| e.term).unwrap_or(Term::ZERO);
                    if term_n == current_term {
                        found = n;
                        break;
                    }
                }
                n = n.prev();
            }
            found
        };
        if new_commit == self.commit_index {
            return Ok(Vec::new());
        }
        self.commit_index = new_commit;
        self.after_commit_config_actions(now)
    }

    /// Leader-side reactions to a moved commit index (I7b, Raft §6):
    /// - the joint C_old,new committed → append and replicate C_new;
    /// - C_new committed and this leader is not in it → step down.
    fn after_commit_config_actions(&mut self, now: Instant) -> Result<Vec<Outbound>, RaftError> {
        if !self.is_leader() {
            return Ok(Vec::new());
        }
        // Only a *committed* latest configuration triggers anything.
        if self.configs.latest_index() > self.commit_index {
            return Ok(Vec::new());
        }
        let active = self.configs.active().clone();
        if active.is_joint() {
            // Phase two of the change: the joint config is committed, so the
            // cluster can safely move to C_new alone.
            let final_config = ClusterConfig {
                voters: active.voters,
                old_voters: None,
                learners: active.learners,
            };
            return self.append_config_entry(final_config, now);
        }
        if !active.voters.contains(&self.id) {
            // A committed C_new excludes us: our job is done; step down.
            self.role = Role::Follower;
            self.leader_id = None;
            self.arm_election_timer(now);
        }
        Ok(Vec::new())
    }

    /// Appends a configuration entry to the leader's own log — it takes
    /// effect immediately (applied on append) — and replicates it.
    fn append_config_entry(
        &mut self,
        config: ClusterConfig,
        now: Instant,
    ) -> Result<Vec<Outbound>, RaftError> {
        let index = self.log.last_index()?.next();
        let entry = LogEntry::with_payload(
            self.hard.current_term,
            index,
            EntryPayload::Config(config.clone()),
        );
        self.log.append(&entry)?;
        self.configs.on_append(index, config);
        self.refresh_leader_state()?;
        // The new entry may already satisfy quorum (e.g. every added member
        // is caught up); recursion is bounded because the follow-up either
        // steps down or finds a non-joint committed config containing us.
        let mut outs = self.advance_commit_index(now)?;
        outs.extend(self.replicate_all()?);
        Ok(outs)
    }

    // --- snapshots (I6, Raft §7) -------------------------------------------

    /// Handles an inbound `InstallSnapshot` from a leader whose log no longer
    /// holds the entries we need (Raft §7).
    ///
    /// Single-shot only: `offset != 0` or `done == false` is rejected with
    /// [`RaftError::Snapshot`] (chunking is future work — entries are small KV
    /// commands, so blobs stay modest). A snapshot that is older than our own
    /// snapshot or our commit index is stale and ignored: installing it would
    /// regress applied state (P9). Otherwise: if our log holds the snapshot's
    /// last-included entry with a matching term, the tail beyond it is
    /// retained; on any conflict (or a too-short log) the entire log is
    /// discarded — both installs are atomic with the prefix drop.
    pub fn handle_install_snapshot(
        &mut self,
        request: InstallSnapshot,
        now: Instant,
    ) -> Result<InstallSnapshotResponse, RaftError> {
        self.observe_term(request.term, now)?;
        let term = self.hard.current_term;

        // Stale leader: reply with our term, take no action.
        if request.term < term {
            return Ok(InstallSnapshotResponse { term });
        }
        if request.offset != 0 || !request.done {
            return Err(RaftError::Snapshot(
                "chunked InstallSnapshot is not supported (single-shot only)".to_string(),
            ));
        }

        // A valid leader for this term, exactly as in `handle_append_entries`.
        self.role = Role::Follower;
        self.leader_id = Some(request.leader_id.clone());
        self.last_leader_contact = Some(now);
        self.arm_election_timer(now);

        // Stale snapshot: nothing it covers is news to us.
        if request.last_included_index <= self.snapshot_index()
            || request.last_included_index <= self.commit_index
        {
            return Ok(InstallSnapshotResponse { term });
        }

        let meta = SnapshotMeta {
            last_included_index: request.last_included_index,
            last_included_term: request.last_included_term,
            config: request.config.clone(),
        };
        let basis_matches = self.log.get(meta.last_included_index)?.map(|e| e.term)
            == Some(meta.last_included_term);
        if basis_matches {
            // Raft §7: "retain log entries following it".
            self.log.compact_to(meta.clone(), &request.data)?;
        } else {
            // Raft §7: "discard the entire log".
            self.log
                .install_snapshot_replacing_log(meta.clone(), &request.data)?;
        }
        // The snapshot replaces config entries too: adopt its configuration
        // as the new base (I7b), keeping any retained tail's configs.
        self.configs
            .on_install_snapshot(request.config, meta.last_included_index, !basis_matches);
        // The guard above ensures this only ever moves the commit index forward.
        self.commit_index = meta.last_included_index;
        self.snapshot = Some(meta);
        Ok(InstallSnapshotResponse { term })
    }

    /// Handles a peer's reply to our `InstallSnapshot`. A higher term steps us
    /// down; otherwise the peer now holds everything up to our snapshot's last
    /// included index, so its `matchIndex`/`nextIndex` jump there and normal
    /// log replication resumes.
    pub fn handle_install_snapshot_response(
        &mut self,
        from: NodeId,
        response: InstallSnapshotResponse,
        now: Instant,
    ) -> Result<Vec<Outbound>, RaftError> {
        if self.observe_term(response.term, now)? {
            return Ok(Vec::new());
        }
        if response.term != self.hard.current_term || !self.is_leader() {
            return Ok(Vec::new());
        }
        let Some(snapshot_index) = self.snapshot.as_ref().map(|m| m.last_included_index) else {
            return Ok(Vec::new());
        };
        self.record_match(&from, snapshot_index);
        let mut outs = self.advance_commit_index(now)?;
        if self.peer_is_behind(&from)? {
            outs.extend(self.replicate_to(&from)?);
        }
        Ok(outs)
    }

    /// Whether the committed-but-uncompacted portion of the log has outgrown
    /// [`Config::snapshot_threshold`]. The shell polls this and, when `true`,
    /// obtains a state-machine snapshot and calls [`RaftNode::compact`].
    pub fn needs_snapshot(&self) -> bool {
        let uncompacted = self
            .commit_index
            .get()
            .saturating_sub(self.snapshot_index().get());
        uncompacted > self.config.snapshot_threshold
    }

    /// Compacts the committed prefix of the log into a snapshot whose opaque
    /// blob is `data` — the state machine's serialized state at exactly the
    /// current commit index (the caller guarantees this correspondence; for
    /// I8's KV, a serialized map). Persisted atomically with the prefix drop.
    ///
    /// Errors with [`RaftError::Snapshot`] if there is nothing new to compact.
    pub fn compact(&mut self, data: Bytes) -> Result<SnapshotMeta, RaftError> {
        let index = self.commit_index;
        if index == LogIndex::ZERO || index <= self.snapshot_index() {
            return Err(RaftError::Snapshot(format!(
                "nothing to compact: commit index {index}, snapshot already at {}",
                self.snapshot_index()
            )));
        }
        let term = self.term_at(index)?.ok_or_else(|| {
            RaftError::Snapshot(format!("no term known for committed index {index}"))
        })?;
        let meta = SnapshotMeta {
            last_included_index: index,
            last_included_term: term,
            // The snapshot replaces config entries too: record the
            // configuration in effect at the compaction point (I7b).
            config: self.configs.config_at(index).clone(),
        };
        self.log.compact_to(meta.clone(), &data)?;
        self.snapshot = Some(meta.clone());
        self.configs.on_compact(index);
        Ok(meta)
    }

    // --- client interface -------------------------------------------------

    /// Appends `command` to the leader's log and replicates it, returning the
    /// resulting `AppendEntries` to send. Errors with [`RaftError::NotLeader`] if
    /// this node is not the leader (the caller — the control plane in I8 —
    /// redirects the client to the current leader).
    pub fn propose(&mut self, command: Bytes, now: Instant) -> Result<Vec<Outbound>, RaftError> {
        if !self.is_leader() {
            return Err(RaftError::NotLeader {
                leader: self.leader_id.as_ref().map(|l| l.to_string()),
            });
        }
        let index = self.log.last_index()?.next();
        let entry = LogEntry::new(self.hard.current_term, index, command);
        self.log.append(&entry)?;
        // A cluster whose quorum this node satisfies alone commits immediately.
        let mut outs = self.advance_commit_index(now)?;
        outs.extend(self.replicate_all()?);
        Ok(outs)
    }

    /// Begins a joint-consensus membership change to `new_voters` (I7b, Raft
    /// §6): appends the joint configuration C_old,new — in force from this
    /// very append — and replicates it. Once C_old,new commits (majorities in
    /// **both** voter sets), the leader automatically appends C_new; once
    /// C_new commits and excludes this leader, it steps down.
    ///
    /// Errors: [`RaftError::NotLeader`] on a non-leader;
    /// [`RaftError::ConfChange`] if a change is already in flight (the latest
    /// config in the log is uncommitted or joint) or `new_voters` is empty.
    pub fn propose_conf_change(
        &mut self,
        new_voters: BTreeSet<NodeId>,
        now: Instant,
    ) -> Result<Vec<Outbound>, RaftError> {
        if !self.is_leader() {
            return Err(RaftError::NotLeader {
                leader: self.leader_id.as_ref().map(|l| l.to_string()),
            });
        }
        if new_voters.is_empty() {
            return Err(RaftError::ConfChange(
                "the new configuration must have at least one voter".to_string(),
            ));
        }
        let active = self.configs.active();
        if active.is_joint() || self.configs.latest_index() > self.commit_index {
            return Err(RaftError::ConfChange(
                "a configuration change is already in flight (one at a time)".to_string(),
            ));
        }
        if new_voters == active.voters {
            return Err(RaftError::ConfChange(
                "the new configuration is identical to the current one".to_string(),
            ));
        }

        // Catch-up gate (I7c, thesis ch. 4): every *added* voter must already
        // be replicated to within `catch_up_margin` entries of the leader's
        // log. Otherwise the new configuration immediately depends on a
        // member that cannot ack anything for a long time — the stall the
        // learner phase exists to prevent.
        let last = self.log.last_index()?;
        let floor = LogIndex::new(last.get().saturating_sub(self.config.catch_up_margin));
        if let Role::Leader(state) = &self.role {
            for added in new_voters.difference(&active.voters) {
                let matched = state
                    .match_index
                    .get(added)
                    .copied()
                    .unwrap_or(LogIndex::ZERO);
                if matched < floor {
                    return Err(RaftError::ConfChange(format!(
                        "voter {added} is not caught up (replicated to {matched}, \
                         leader log at {last}); add it as a learner first"
                    )));
                }
            }
        }

        let joint = ClusterConfig {
            old_voters: Some(active.voters.clone()),
            learners: active
                .learners
                .iter()
                .filter(|l| !new_voters.contains(*l))
                .cloned()
                .collect(),
            voters: new_voters,
        };
        self.append_config_entry(joint, now)
    }

    /// Adds `id` as a non-voting **learner** (I7c, thesis ch. 4): it is
    /// replicated to — and so catches up — but never votes or counts toward
    /// quorum. Once [`RaftNode::propose_conf_change`] sees it caught up, it
    /// may be promoted into the voter set.
    ///
    /// A learner addition is a single (non-joint) configuration entry: it
    /// changes no quorum, so joint consensus is unnecessary — but it still
    /// respects the one-change-in-flight rule.
    pub fn propose_add_learner(
        &mut self,
        id: NodeId,
        now: Instant,
    ) -> Result<Vec<Outbound>, RaftError> {
        if !self.is_leader() {
            return Err(RaftError::NotLeader {
                leader: self.leader_id.as_ref().map(|l| l.to_string()),
            });
        }
        let active = self.configs.active();
        if active.is_joint() || self.configs.latest_index() > self.commit_index {
            return Err(RaftError::ConfChange(
                "a configuration change is already in flight (one at a time)".to_string(),
            ));
        }
        if active.voters.contains(&id) {
            return Err(RaftError::ConfChange(format!("{id} is already a voter")));
        }
        if active.learners.contains(&id) {
            return Err(RaftError::ConfChange(format!("{id} is already a learner")));
        }
        let mut with_learner = active.clone();
        with_learner.learners.insert(id);
        self.append_config_entry(with_learner, now)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::types::LogEntry;
    use bytes::Bytes;
    use std::collections::VecDeque;

    fn nid(id: &str) -> NodeId {
        NodeId::new(id).unwrap()
    }

    /// Builds a node `id` whose peers are `peers`, with a seeded RNG.
    fn make(id: &str, peers: &[&str], seed: u64, now: Instant) -> (tempfile::TempDir, RaftNode) {
        let dir = tempfile::tempdir().unwrap();
        let log = RaftLog::open(dir.path().join("raft.redb")).unwrap();
        let node = RaftNode::new(
            nid(id),
            peers.iter().map(|p| nid(p)).collect(),
            log,
            Rng::seed_from_u64(seed),
            Config::default(),
            now,
        )
        .unwrap();
        (dir, node)
    }

    fn request_vote(candidate: &str, term: u64) -> RequestVote {
        RequestVote {
            term: Term::new(term),
            candidate_id: nid(candidate),
            last_log_index: LogIndex::ZERO,
            last_log_term: Term::ZERO,
        }
    }

    // --- single-node & RequestVote receiver logic ------------------------

    #[test]
    fn single_node_self_elects() {
        let t0 = Instant::now();
        let (_d, mut node) = make("solo", &[], 1, t0);
        assert!(node.is_follower());
        let outs = node.tick(t0 + Duration::from_secs(1)).unwrap();
        assert!(node.is_leader(), "a one-node cluster is its own majority");
        assert!(outs.is_empty(), "no peers to message");
        assert_eq!(node.current_term(), Term::new(1));
        assert_eq!(node.leader_id(), Some(&nid("solo")));
    }

    #[test]
    fn grants_then_denies_second_candidate_same_term() {
        let now = Instant::now();
        let (_d, mut node) = make("v", &["a", "b"], 1, now);

        assert!(
            node.handle_request_vote(request_vote("a", 1), now)
                .unwrap()
                .vote_granted
        );
        assert_eq!(node.voted_for(), Some(&nid("a")));

        // Already voted for `a` this term → deny `b`.
        assert!(
            !node
                .handle_request_vote(request_vote("b", 1), now)
                .unwrap()
                .vote_granted
        );

        // Re-request from `a` in the same term is idempotent → granted again.
        assert!(
            node.handle_request_vote(request_vote("a", 1), now)
                .unwrap()
                .vote_granted
        );
    }

    #[test]
    fn denies_stale_term_and_reports_own() {
        let now = Instant::now();
        let (_d, mut node) = make("v", &["a", "b"], 1, now);
        // Bump the node to term 3 by granting a term-3 vote.
        node.handle_request_vote(request_vote("a", 3), now).unwrap();
        assert_eq!(node.current_term(), Term::new(3));

        let resp = node.handle_request_vote(request_vote("b", 2), now).unwrap();
        assert!(!resp.vote_granted, "term 2 < current term 3 is stale");
        assert_eq!(
            resp.term,
            Term::new(3),
            "reply carries our term so the stale sender updates"
        );
    }

    /// Thesis §4.2.3 (leader stickiness, required by I7b): a current leader
    /// disregards even a higher-term RequestVote — it stays leader and its
    /// term does not move. (It still steps down to higher terms carried by
    /// AppendEntries traffic, which is what preserves liveness.)
    #[test]
    fn a_current_leader_ignores_higher_term_request_votes() {
        let t0 = Instant::now();
        let mut c = Cluster::new(&["n0", "n1", "n2"], &[1, 2, 3], t0);
        c.tick_and_deliver(0, t0 + Duration::from_secs(1));
        assert!(c.nodes[0].is_leader());
        assert_eq!(c.nodes[0].current_term(), Term::new(1));

        let resp = c.nodes[0]
            .handle_request_vote(request_vote("n1", 2), t0)
            .unwrap();
        assert!(!resp.vote_granted, "a serving leader never grants");
        assert!(c.nodes[0].is_leader(), "the leader is not disrupted");
        assert_eq!(c.nodes[0].current_term(), Term::new(1), "term untouched");
    }

    /// The follower half of thesis §4.2.3: fresh leader contact suppresses
    /// votes; once the contact goes stale a vote is granted normally.
    #[test]
    fn recent_leader_contact_suppresses_votes_until_it_goes_stale() {
        let t0 = Instant::now();
        let (_d, mut follower) = make("f", &["ldr", "c"], 1, t0);

        // Hear from a valid leader at t0.
        follower
            .handle_append_entries(append_entries("ldr", 1, 0, 0, vec![], 0), t0)
            .unwrap();

        // 50 ms later (< min election timeout): the vote is disregarded and
        // the term does not move.
        let soon = t0 + Duration::from_millis(50);
        let resp = follower
            .handle_request_vote(request_vote("c", 5), soon)
            .unwrap();
        assert!(!resp.vote_granted);
        assert_eq!(follower.current_term(), Term::new(1), "term not bumped");

        // Well past the minimum election timeout: the candidate is heard out.
        let later = t0 + Duration::from_secs(1);
        let resp = follower
            .handle_request_vote(request_vote("c", 5), later)
            .unwrap();
        assert!(resp.vote_granted);
        assert_eq!(follower.current_term(), Term::new(5));
    }

    // --- election restriction (§6) ---------------------------------------

    /// A voter whose last log entry is `(term 2, index 3)`.
    fn voter_with_log(now: Instant) -> (tempfile::TempDir, RaftNode) {
        let dir = tempfile::tempdir().unwrap();
        let log = RaftLog::open(dir.path().join("raft.redb")).unwrap();
        log.append_all(&[
            LogEntry::new(Term::new(1), LogIndex::new(1), Bytes::from_static(b"a")),
            LogEntry::new(Term::new(2), LogIndex::new(2), Bytes::from_static(b"b")),
            LogEntry::new(Term::new(2), LogIndex::new(3), Bytes::from_static(b"c")),
        ])
        .unwrap();
        let node = RaftNode::new(
            nid("v"),
            vec![nid("c")],
            log,
            Rng::seed_from_u64(1),
            Config::default(),
            now,
        )
        .unwrap();
        (dir, node)
    }

    fn asks(cand_term: u64, cand_index: u64) -> RequestVote {
        // term 5 keeps the request non-stale so only the *log* comparison decides.
        RequestVote {
            term: Term::new(5),
            candidate_id: nid("c"),
            last_log_index: LogIndex::new(cand_index),
            last_log_term: Term::new(cand_term),
        }
    }

    #[test]
    fn election_restriction_up_to_date_comparator() {
        let now = Instant::now();

        let granted = |req: RequestVote| {
            let (_d, mut n) = voter_with_log(now);
            n.handle_request_vote(req, now).unwrap().vote_granted
        };

        // Voter's last is (term 2, index 3).
        assert!(granted(asks(3, 1)), "higher last-term wins even if shorter");
        assert!(granted(asks(2, 3)), "an equal log is up-to-date");
        assert!(granted(asks(2, 4)), "equal last-term, longer log wins");
        assert!(!granted(asks(2, 2)), "equal last-term, shorter log loses");
        assert!(!granted(asks(1, 9)), "lower last-term loses even if longer");
    }

    // --- multi-node cluster harness --------------------------------------

    struct Cluster {
        _dirs: Vec<tempfile::TempDir>,
        nodes: Vec<RaftNode>,
    }

    impl Cluster {
        fn new(ids: &[&str], seeds: &[u64], now: Instant) -> Self {
            let all: Vec<NodeId> = ids.iter().map(|s| nid(s)).collect();
            let mut dirs = Vec::new();
            let mut nodes = Vec::new();
            for (i, id) in all.iter().enumerate() {
                let dir = tempfile::tempdir().unwrap();
                let log = RaftLog::open(dir.path().join("raft.redb")).unwrap();
                let peers: Vec<NodeId> = all.iter().filter(|p| *p != id).cloned().collect();
                let node = RaftNode::new(
                    id.clone(),
                    peers,
                    log,
                    Rng::seed_from_u64(seeds[i]),
                    Config::default(),
                    now,
                )
                .unwrap();
                dirs.push(dir);
                nodes.push(node);
            }
            Cluster { _dirs: dirs, nodes }
        }

        fn leaders(&self) -> Vec<usize> {
            self.nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.is_leader())
                .map(|(i, _)| i)
                .collect()
        }

        /// Delivers a batch of `(sender_index, message)` to quiescence, routing
        /// each reply back to its sender. Messages addressed to nodes the
        /// cluster does not contain (e.g. unreachable members of a proposed
        /// configuration) are silently dropped, like a dead host.
        fn deliver_all(&mut self, initial: Vec<(usize, Outbound)>, now: Instant) {
            let mut queue: VecDeque<(usize, Outbound)> = initial.into_iter().collect();
            let mut budget = 10_000;
            while let Some((from, out)) = queue.pop_front() {
                budget -= 1;
                assert!(budget > 0, "message storm — election is not converging");
                match out {
                    Outbound::RequestVote { to, request } => {
                        let Some(ti) = self.try_idx(&to) else {
                            continue;
                        };
                        let resp = self.nodes[ti].handle_request_vote(request, now).unwrap();
                        let more = self.nodes[from]
                            .handle_request_vote_response(to, resp, now)
                            .unwrap();
                        for o in more {
                            queue.push_back((from, o));
                        }
                    }
                    Outbound::AppendEntries { to, request } => {
                        let Some(ti) = self.try_idx(&to) else {
                            continue;
                        };
                        let resp = self.nodes[ti].handle_append_entries(request, now).unwrap();
                        let more = self.nodes[from]
                            .handle_append_entries_response(to, resp, now)
                            .unwrap();
                        for o in more {
                            queue.push_back((from, o));
                        }
                    }
                    Outbound::InstallSnapshot { to, request } => {
                        let Some(ti) = self.try_idx(&to) else {
                            continue;
                        };
                        let resp = self.nodes[ti]
                            .handle_install_snapshot(request, now)
                            .unwrap();
                        let more = self.nodes[from]
                            .handle_install_snapshot_response(to, resp, now)
                            .unwrap();
                        for o in more {
                            queue.push_back((from, o));
                        }
                    }
                }
            }
        }

        fn try_idx(&self, id: &NodeId) -> Option<usize> {
            self.nodes.iter().position(|n| n.id() == id)
        }

        fn tick_and_deliver(&mut self, i: usize, now: Instant) {
            let outs = self.nodes[i].tick(now).unwrap();
            let batch = outs.into_iter().map(|o| (i, o)).collect();
            self.deliver_all(batch, now);
        }
    }

    /// Finds the `RequestVote` addressed to `target` in a tick's output.
    fn rv_to(outs: &[Outbound], target: &str) -> Outbound {
        outs.iter()
            .find(|o| matches!(o, Outbound::RequestVote { to, .. } if to.as_str() == target))
            .unwrap()
            .clone()
    }

    #[test]
    fn three_node_happy_election() {
        let t0 = Instant::now();
        let mut c = Cluster::new(&["n0", "n1", "n2"], &[1, 2, 3], t0);

        // n0 times out and campaigns; the others grant.
        c.tick_and_deliver(0, t0 + Duration::from_secs(1));

        assert_eq!(c.leaders(), vec![0], "exactly one leader");
        assert!(c.nodes[1].is_follower() && c.nodes[2].is_follower());
        assert_eq!(c.nodes[0].current_term(), Term::new(1));
        assert_eq!(c.nodes[1].leader_id(), Some(&nid("n0")));
        assert_eq!(c.nodes[1].voted_for(), Some(&nid("n0")));
    }

    #[test]
    fn heartbeats_suppress_further_elections() {
        let t0 = Instant::now();
        let mut c = Cluster::new(&["n0", "n1", "n2"], &[1, 2, 3], t0);
        let lead_at = t0 + Duration::from_secs(1);
        c.tick_and_deliver(0, lead_at);
        assert!(c.nodes[0].is_leader());

        // Heartbeat every 50 ms for 500 ms — well past the 150–300 ms election
        // window. If heartbeats did not reset follower timers, n1/n2 would have
        // started elections; assert they never do.
        let mut now = lead_at;
        for _ in 0..10 {
            now += Duration::from_millis(50);
            c.tick_and_deliver(0, now);
            assert!(c.nodes[1].tick(now).unwrap().is_empty());
            assert!(c.nodes[2].tick(now).unwrap().is_empty());
        }
        assert_eq!(c.leaders(), vec![0]);
        assert!(c.nodes[1].is_follower() && c.nodes[2].is_follower());
        assert_eq!(c.nodes[0].current_term(), Term::new(1));
    }

    #[test]
    fn a_stepped_down_leader_does_not_immediately_re_elect() {
        let t0 = Instant::now();
        let mut c = Cluster::new(&["n0", "n1", "n2"], &[1, 2, 3], t0);
        c.tick_and_deliver(0, t0 + Duration::from_secs(1));
        assert!(c.nodes[0].is_leader());

        // Long past its (stale) election deadline, a follower's heartbeat reply
        // carries a higher term, forcing the leader to step down.
        let now = t0 + Duration::from_secs(30);
        c.nodes[0]
            .handle_append_entries_response(
                nid("n1"),
                AppendEntriesResponse {
                    term: Term::new(2),
                    success: false,
                    conflict_term: Term::ZERO,
                    conflict_index: LogIndex::ZERO,
                    match_index: LogIndex::ZERO,
                },
                now,
            )
            .unwrap();
        assert!(c.nodes[0].is_follower());
        assert_eq!(c.nodes[0].current_term(), Term::new(2));

        // The election timer was re-armed on step-down, so an immediate tick must
        // NOT start a fresh election (which would fight the higher-term node).
        let outs = c.nodes[0].tick(now).unwrap();
        assert!(
            outs.is_empty(),
            "a just-stepped-down node must wait a full timeout"
        );
        assert!(c.nodes[0].is_follower());
        assert_eq!(
            c.nodes[0].current_term(),
            Term::new(2),
            "no churn: term stays 2"
        );
    }

    #[test]
    fn split_vote_in_term_1_resolves_in_term_2() {
        let t0 = Instant::now();
        let mut c = Cluster::new(&["n0", "n1", "n2", "n3"], &[10, 20, 30, 40], t0);
        let now1 = t0 + Duration::from_secs(1);

        // Force n0 and n1 to both campaign in term 1.
        let n0_out = c.nodes[0].tick(now1).unwrap();
        let n1_out = c.nodes[1].tick(now1).unwrap();
        assert!(c.nodes[0].is_candidate() && c.nodes[1].is_candidate());

        // Split the four votes 2–2: n2→n0, n3→n1; every cross-request is denied
        // (the recipient already voted this term).
        c.deliver_all(vec![(0, rv_to(&n0_out, "n2"))], now1); // n0 gains n2 → {n0,n2}
        c.deliver_all(vec![(1, rv_to(&n1_out, "n3"))], now1); // n1 gains n3 → {n1,n3}
        c.deliver_all(vec![(0, rv_to(&n0_out, "n1"))], now1); // denied (n1 voted self)
        c.deliver_all(vec![(0, rv_to(&n0_out, "n3"))], now1); // denied (n3 voted n1)
        c.deliver_all(vec![(1, rv_to(&n1_out, "n0"))], now1); // denied (n0 voted self)
        c.deliver_all(vec![(1, rv_to(&n1_out, "n2"))], now1); // denied (n2 voted n0)

        assert!(c.leaders().is_empty(), "term 1 is a 2–2 split: no majority");
        assert_eq!(c.nodes[0].current_term(), Term::new(1));

        // Randomized timeouts break the tie: the earliest-expiring node campaigns
        // in term 2 and wins (only one candidate this term).
        let (i, deadline) = c
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (i, n.election_deadline()))
            .min_by_key(|(_, d)| *d)
            .unwrap();
        c.tick_and_deliver(i, deadline);

        assert_eq!(c.leaders().len(), 1, "term 2 elects exactly one leader");
        let leader = c.leaders()[0];
        assert_eq!(c.nodes[leader].current_term(), Term::new(2));
    }

    // --- log replication (I4) --------------------------------------------

    fn batch(from: usize, outs: Vec<Outbound>) -> Vec<(usize, Outbound)> {
        outs.into_iter().map(|o| (from, o)).collect()
    }

    /// Builds a follower `id` pre-seeded with a log of `(term, index)` entries.
    fn make_with_log(
        id: &str,
        peers: &[&str],
        entries: &[(u64, u64)],
        now: Instant,
    ) -> (tempfile::TempDir, RaftNode) {
        let dir = tempfile::tempdir().unwrap();
        let log = RaftLog::open(dir.path().join("raft.redb")).unwrap();
        let seeded: Vec<LogEntry> = entries
            .iter()
            .map(|(t, i)| LogEntry::new(Term::new(*t), LogIndex::new(*i), Bytes::new()))
            .collect();
        log.append_all(&seeded).unwrap();
        let node = RaftNode::new(
            nid(id),
            peers.iter().map(|p| nid(p)).collect(),
            log,
            Rng::seed_from_u64(1),
            Config::default(),
            now,
        )
        .unwrap();
        (dir, node)
    }

    fn append_entries(
        leader: &str,
        term: u64,
        prev_index: u64,
        prev_term: u64,
        entries: Vec<LogEntry>,
        leader_commit: u64,
    ) -> AppendEntries {
        AppendEntries {
            term: Term::new(term),
            leader_id: nid(leader),
            prev_log_index: LogIndex::new(prev_index),
            prev_log_term: Term::new(prev_term),
            entries,
            leader_commit: LogIndex::new(leader_commit),
        }
    }

    #[test]
    fn leader_replicates_and_commits_a_proposed_entry() {
        let t0 = Instant::now();
        let mut c = Cluster::new(&["n0", "n1", "n2"], &[1, 2, 3], t0);
        let now = t0 + Duration::from_secs(1);
        c.tick_and_deliver(0, now);
        assert!(c.nodes[0].is_leader());

        let out = c.nodes[0].propose(Bytes::from_static(b"x"), now).unwrap();
        c.deliver_all(batch(0, out), now);

        assert_eq!(c.nodes[0].commit_index(), LogIndex::new(1));
        for i in 0..3 {
            assert_eq!(c.nodes[i].last_log_index().unwrap(), LogIndex::new(1));
            assert_eq!(
                c.nodes[i]
                    .log_entry(LogIndex::new(1))
                    .unwrap()
                    .unwrap()
                    .command()
                    .cloned(),
                Some(Bytes::from_static(b"x"))
            );
        }
    }

    #[test]
    fn single_node_leader_commits_proposal_immediately() {
        let t0 = Instant::now();
        let (_d, mut node) = make("solo", &[], 1, t0);
        let now = t0 + Duration::from_secs(1);
        node.tick(now).unwrap();
        assert!(node.is_leader());
        node.propose(Bytes::from_static(b"only"), now).unwrap();
        assert_eq!(
            node.commit_index(),
            LogIndex::new(1),
            "self is its own majority"
        );
    }

    #[test]
    fn propose_on_a_follower_is_rejected() {
        let now = Instant::now();
        let (_d, mut node) = make("f", &["a", "b"], 1, now);
        let err = node.propose(Bytes::from_static(b"x"), now).unwrap_err();
        assert!(matches!(err, RaftError::NotLeader { .. }));
    }

    #[test]
    fn consistency_check_rejects_a_too_short_log_with_a_hint() {
        let now = Instant::now();
        // Follower holds only index 1.
        let (_d, mut follower) = make_with_log("f", &["ldr"], &[(1, 1)], now);
        // Leader claims prev at index 3 — beyond the follower's log.
        let req = append_entries("ldr", 5, 3, 2, vec![], 0);
        let resp = follower.handle_append_entries(req, now).unwrap();
        assert!(!resp.success);
        assert_eq!(
            resp.conflict_index,
            LogIndex::new(2),
            "point the leader just past our last entry"
        );
        assert_eq!(
            follower.last_log_index().unwrap(),
            LogIndex::new(1),
            "log unchanged on rejection"
        );
    }

    #[test]
    fn conflicting_tail_is_overwritten_not_duplicated() {
        let now = Instant::now();
        // Follower's log diverges at index 3: it holds a stale term-2 entry there.
        let (_d, mut follower) = make_with_log("f", &["ldr"], &[(1, 1), (1, 2), (2, 3)], now);
        // Leader replicates the authoritative index 3 & 4 (term 5) after prev (2, term 1).
        let entries = vec![
            LogEntry::new(Term::new(5), LogIndex::new(3), Bytes::from_static(b"new3")),
            LogEntry::new(Term::new(5), LogIndex::new(4), Bytes::from_static(b"new4")),
        ];
        let req = append_entries("ldr", 5, 2, 1, entries, 0);
        let resp = follower.handle_append_entries(req, now).unwrap();
        assert!(resp.success);
        assert_eq!(resp.match_index, LogIndex::new(4));
        assert_eq!(
            follower.last_log_index().unwrap(),
            LogIndex::new(4),
            "no duplication"
        );
        assert_eq!(
            follower.log_entry(LogIndex::new(3)).unwrap().unwrap().term,
            Term::new(5),
            "the stale term-2 entry at index 3 was overwritten"
        );
    }

    #[test]
    fn idempotent_append_does_not_truncate_a_matching_suffix() {
        let now = Instant::now();
        let (_d, mut follower) = make_with_log("f", &["ldr"], &[(1, 1), (1, 2), (1, 3)], now);
        // Re-send entries the follower already holds (a delayed/duplicated request).
        let entries = vec![LogEntry::new(Term::new(1), LogIndex::new(2), Bytes::new())];
        let req = append_entries("ldr", 1, 1, 1, entries, 0);
        let resp = follower.handle_append_entries(req, now).unwrap();
        assert!(resp.success);
        assert_eq!(
            follower.last_log_index().unwrap(),
            LogIndex::new(3),
            "a matching suffix must NOT be truncated by a stale/duplicate request"
        );
    }

    #[test]
    fn lagging_follower_catches_up_via_backoff() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(1);
        // n0 starts already holding three entries; it becomes leader in term 4.
        let (_d0, mut n0) = make_with_log("n0", &["n1", "n2"], &[(1, 1), (1, 2), (1, 3)], t0);
        // (n0's saved term is 0; campaigning bumps it to 1 — fine for this test.)
        let (_d1, mut n1) = make("n1", &["n0", "n2"], 2, t0);
        let (_d2, mut n2) = make("n2", &["n0", "n1"], 3, t0);

        // Elect n0 (it has the longest log, so it is electable).
        let mut outs = n0.tick(now).unwrap();
        // Drive the exchange by hand across the three nodes until quiescent.
        let mut queue: VecDeque<(usize, Outbound)> =
            batch(0, outs.split_off(0)).into_iter().collect();
        let mut budget = 10_000;
        while let Some((from, out)) = queue.pop_front() {
            budget -= 1;
            assert!(budget > 0, "not converging");
            match out {
                Outbound::RequestVote { to, request } => {
                    let resp = route_rv(&mut n1, &mut n2, &to, request, now);
                    let more = match from {
                        0 => n0.handle_request_vote_response(to, resp, now).unwrap(),
                        _ => vec![],
                    };
                    for o in more {
                        queue.push_back((0, o));
                    }
                }
                Outbound::AppendEntries { to, request } => {
                    let resp = route_ae(&mut n1, &mut n2, &to, request, now);
                    let more = n0.handle_append_entries_response(to, resp, now).unwrap();
                    for o in more {
                        queue.push_back((0, o));
                    }
                }
                Outbound::InstallSnapshot { .. } => {
                    unreachable!("no compaction happens in this test")
                }
            }
        }
        assert!(n0.is_leader());
        // Both followers, initially empty, were backfilled to n0's full log.
        assert_eq!(n1.last_log_index().unwrap(), LogIndex::new(3));
        assert_eq!(n2.last_log_index().unwrap(), LogIndex::new(3));
    }

    fn route_rv(
        n1: &mut RaftNode,
        n2: &mut RaftNode,
        to: &NodeId,
        req: RequestVote,
        now: Instant,
    ) -> RequestVoteResponse {
        if to.as_str() == "n1" {
            n1.handle_request_vote(req, now).unwrap()
        } else {
            n2.handle_request_vote(req, now).unwrap()
        }
    }

    fn route_ae(
        n1: &mut RaftNode,
        n2: &mut RaftNode,
        to: &NodeId,
        req: AppendEntries,
        now: Instant,
    ) -> AppendEntriesResponse {
        if to.as_str() == "n1" {
            n1.handle_append_entries(req, now).unwrap()
        } else {
            n2.handle_append_entries(req, now).unwrap()
        }
    }

    /// **The Figure-8 regression test** (`docs/raft-notes.md` §7). A prior-term
    /// entry that reaches a majority *via a later leader's replication* must NOT
    /// be committed by replica count; it commits only once a current-term entry
    /// commits over it.
    #[test]
    fn figure_8_prior_term_entry_not_committed_by_replica_count() {
        let t0 = Instant::now();
        let mut c = Cluster::new(&["n0", "n1", "n2"], &[1, 2, 3], t0);
        let now = t0 + Duration::from_secs(1);

        // n0 wins term 1.
        c.tick_and_deliver(0, now);
        assert!(c.nodes[0].is_leader());
        assert_eq!(c.nodes[0].current_term(), Term::new(1));

        // n0 proposes entry A (term 1) and we place it on the majority {n0, n1}
        // WITHOUT n0 learning n1 stored it — so A is on a majority but uncommitted.
        let out = c.nodes[0].propose(Bytes::from_static(b"A"), now).unwrap();
        let ae_to_n1 = rv_or_ae_to(&out, "n1");
        if let Outbound::AppendEntries { request, .. } = ae_to_n1 {
            let _ = c.nodes[1].handle_append_entries(request, now).unwrap();
        }
        assert_eq!(
            c.nodes[0].commit_index(),
            LogIndex::ZERO,
            "A is not committed yet"
        );
        assert_eq!(c.nodes[0].last_log_index().unwrap(), LogIndex::new(1));
        assert_eq!(c.nodes[1].last_log_index().unwrap(), LogIndex::new(1));

        // n1 (whose log holds A) wins term 2; n0 steps down. Becoming leader,
        // n1 re-replicates A (a *term-1* entry) to the whole cluster.
        let now2 = now + Duration::from_secs(1);
        c.tick_and_deliver(1, now2);
        assert!(c.nodes[1].is_leader());
        assert_eq!(c.nodes[1].current_term(), Term::new(2));

        // KEY ASSERTION: A is now on a majority (re-replicated by n1), but because
        // it is from a prior term, the current-term rule forbids committing it.
        assert_eq!(
            c.nodes[1].commit_index(),
            LogIndex::ZERO,
            "a prior-term entry on a majority must NOT be committed by replica count (Figure 8)"
        );

        // n1 proposes B (term 2). When B reaches a majority, commit jumps to 2 —
        // committing A indirectly via the Log Matching Property.
        let out = c.nodes[1].propose(Bytes::from_static(b"B"), now2).unwrap();
        c.deliver_all(batch(1, out), now2);
        assert_eq!(
            c.nodes[1].commit_index(),
            LogIndex::new(2),
            "a current-term entry on a majority commits, sweeping the prior-term entry in"
        );
    }

    fn rv_or_ae_to(outs: &[Outbound], target: &str) -> Outbound {
        outs.iter()
            .find(|o| match o {
                Outbound::AppendEntries { to, .. } => to.as_str() == target,
                Outbound::RequestVote { to, .. } => to.as_str() == target,
                Outbound::InstallSnapshot { to, .. } => to.as_str() == target,
            })
            .unwrap()
            .clone()
    }

    // --- snapshots & compaction (I6) ---------------------------------------

    #[test]
    fn compact_drops_prefix_and_survives_restart() {
        let t0 = Instant::now();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raft.redb");
        let mk = |now: Instant| {
            RaftNode::new(
                nid("solo"),
                vec![],
                RaftLog::open(&path).unwrap(),
                Rng::seed_from_u64(1),
                Config::default(),
                now,
            )
            .unwrap()
        };

        let mut node = mk(t0);
        let now = t0 + Duration::from_secs(1);
        node.tick(now).unwrap();
        assert!(node.is_leader());
        for k in 0..5u32 {
            node.propose(Bytes::copy_from_slice(&k.to_le_bytes()), now)
                .unwrap();
        }
        assert_eq!(node.commit_index(), LogIndex::new(5));

        let meta = node.compact(Bytes::from_static(b"machine@5")).unwrap();
        assert_eq!(meta.last_included_index, LogIndex::new(5));
        assert_eq!(meta.last_included_term, Term::new(1));
        // The whole log was committed, so compaction empties it — but the
        // last (index, term) must survive via the snapshot fallback.
        assert_eq!(node.first_log_index().unwrap(), LogIndex::ZERO);
        assert_eq!(node.last_log_index().unwrap(), LogIndex::new(5));
        assert!(node.log_entry(LogIndex::new(3)).unwrap().is_none());

        // Appending continues seamlessly above the snapshot.
        node.propose(Bytes::from_static(b"six"), now).unwrap();
        assert_eq!(node.last_log_index().unwrap(), LogIndex::new(6));
        assert_eq!(node.first_log_index().unwrap(), LogIndex::new(6));

        // Restart: snapshot metadata recovers and floors the commit index (P9).
        drop(node);
        let node = mk(t0 + Duration::from_secs(2));
        assert_eq!(node.snapshot_meta(), Some(meta.clone()));
        assert!(node.commit_index() >= LogIndex::new(5));
        let (m, data) = node.snapshot().unwrap().unwrap();
        assert_eq!(m, meta);
        assert_eq!(data, Bytes::from_static(b"machine@5"));
    }

    #[test]
    fn compact_with_nothing_new_errors() {
        let t0 = Instant::now();
        let (_d, mut node) = make("solo", &[], 1, t0);
        // Nothing committed at all.
        let err = node.compact(Bytes::new()).unwrap_err();
        assert!(matches!(err, RaftError::Snapshot(_)));

        let now = t0 + Duration::from_secs(1);
        node.tick(now).unwrap();
        node.propose(Bytes::from_static(b"a"), now).unwrap();
        node.compact(Bytes::from_static(b"s1")).unwrap();
        // Compacting again with no new commits is refused.
        let err = node.compact(Bytes::from_static(b"s2")).unwrap_err();
        assert!(matches!(err, RaftError::Snapshot(_)));
    }

    #[test]
    fn needs_snapshot_tracks_committed_growth_past_threshold() {
        let t0 = Instant::now();
        let dir = tempfile::tempdir().unwrap();
        let log = RaftLog::open(dir.path().join("raft.redb")).unwrap();
        let cfg = Config {
            snapshot_threshold: 4,
            ..Config::default()
        };
        let mut node =
            RaftNode::new(nid("solo"), vec![], log, Rng::seed_from_u64(1), cfg, t0).unwrap();
        let now = t0 + Duration::from_secs(1);
        node.tick(now).unwrap();
        for k in 0..4u32 {
            node.propose(Bytes::copy_from_slice(&k.to_le_bytes()), now)
                .unwrap();
        }
        assert!(!node.needs_snapshot(), "exactly at the threshold is fine");
        node.propose(Bytes::from_static(b"tip"), now).unwrap();
        assert!(node.needs_snapshot(), "one past the threshold triggers");
        node.compact(Bytes::from_static(b"s")).unwrap();
        assert!(!node.needs_snapshot(), "compaction resets the trigger");
    }

    #[test]
    fn leader_ships_snapshot_to_a_follower_whose_entries_were_compacted() {
        let t0 = Instant::now();
        let mut c = Cluster::new(&["n0", "n1", "n2"], &[1, 2, 3], t0);
        let now = t0 + Duration::from_secs(1);
        c.tick_and_deliver(0, now);
        assert!(c.nodes[0].is_leader());

        // Three fully replicated, committed entries; one heartbeat round then
        // propagates the leader's commit index to the followers.
        for k in 0..3u32 {
            let out = c.nodes[0]
                .propose(Bytes::copy_from_slice(&k.to_le_bytes()), now)
                .unwrap();
            c.deliver_all(batch(0, out), now);
        }
        c.tick_and_deliver(0, now + Duration::from_millis(60));
        assert_eq!(c.nodes[2].commit_index(), LogIndex::new(3));

        // Entry 4 reaches only n1 (a majority with the leader): committed on
        // n0 while n2 is left behind at index 3.
        let out = c.nodes[0].propose(Bytes::from_static(b"e4"), now).unwrap();
        let to_n1 = rv_or_ae_to(&out, "n1");
        if let Outbound::AppendEntries { request, .. } = to_n1 {
            let resp = c.nodes[1].handle_append_entries(request, now).unwrap();
            c.nodes[0]
                .handle_append_entries_response(nid("n1"), resp, now)
                .unwrap();
        }
        assert_eq!(c.nodes[0].commit_index(), LogIndex::new(4));

        // The leader compacts everything it committed; index 4 is now gone
        // from its log, so n2 can only be caught up by snapshot.
        c.nodes[0]
            .compact(Bytes::from_static(b"machine@4"))
            .unwrap();
        let outs = c.nodes[0].tick(now + Duration::from_millis(120)).unwrap();
        let to_n2 = rv_or_ae_to(&outs, "n2");
        let Outbound::InstallSnapshot { request, .. } = to_n2 else {
            panic!("expected InstallSnapshot for the lagging follower, got {to_n2:?}");
        };
        assert_eq!(request.last_included_index, LogIndex::new(4));
        assert!(request.done && request.offset == 0, "single-shot");

        // n2 installs it: conflicting basis (it never saw entry 4) discards
        // the log; state jumps to the snapshot.
        let resp = c.nodes[2].handle_install_snapshot(request, now).unwrap();
        assert_eq!(c.nodes[2].commit_index(), LogIndex::new(4));
        assert_eq!(
            c.nodes[2].snapshot_meta().unwrap().last_included_index,
            LogIndex::new(4)
        );
        assert_eq!(c.nodes[2].first_log_index().unwrap(), LogIndex::ZERO);
        assert_eq!(c.nodes[2].last_log_index().unwrap(), LogIndex::new(4));

        // The leader records the catch-up; n2 is no longer behind.
        let more = c.nodes[0]
            .handle_install_snapshot_response(nid("n2"), resp, now)
            .unwrap();
        assert!(more.is_empty(), "peer is fully caught up");

        // Replication above the snapshot boundary resumes normally: the
        // prev term for entry 5 comes from the snapshot metadata on both ends.
        let out = c.nodes[0].propose(Bytes::from_static(b"e5"), now).unwrap();
        c.deliver_all(batch(0, out), now);
        assert_eq!(c.nodes[2].last_log_index().unwrap(), LogIndex::new(5));
        assert_eq!(c.nodes[0].commit_index(), LogIndex::new(5));
    }

    #[test]
    fn stale_install_snapshot_is_ignored() {
        let now = Instant::now();
        let (_d, mut follower) = make_with_log(
            "f",
            &["ldr"],
            &[(1, 1), (1, 2), (1, 3), (1, 4), (1, 5)],
            now,
        );
        // A heartbeat commits everything the follower holds.
        let req = append_entries("ldr", 1, 5, 1, vec![], 5);
        follower.handle_append_entries(req, now).unwrap();
        assert_eq!(follower.commit_index(), LogIndex::new(5));

        // A snapshot older than the commit index must be a no-op.
        let resp = follower
            .handle_install_snapshot(
                InstallSnapshot {
                    term: Term::new(1),
                    leader_id: nid("ldr"),
                    last_included_index: LogIndex::new(3),
                    last_included_term: Term::new(1),
                    offset: 0,
                    data: Bytes::from_static(b"old"),
                    done: true,
                    config: ClusterConfig::default(),
                },
                now,
            )
            .unwrap();
        assert_eq!(resp.term, Term::new(1));
        assert!(follower.snapshot_meta().is_none(), "stale snapshot ignored");
        assert!(follower.log_entry(LogIndex::new(2)).unwrap().is_some());
        assert_eq!(follower.commit_index(), LogIndex::new(5));
    }

    #[test]
    fn install_snapshot_with_matching_basis_retains_the_tail() {
        let now = Instant::now();
        let (_d, mut follower) = make_with_log("f", &["ldr"], &[(1, 1), (1, 2), (2, 3)], now);
        let resp = follower
            .handle_install_snapshot(
                InstallSnapshot {
                    term: Term::new(2),
                    leader_id: nid("ldr"),
                    last_included_index: LogIndex::new(2),
                    last_included_term: Term::new(1), // matches our entry 2
                    offset: 0,
                    data: Bytes::from_static(b"machine@2"),
                    done: true,
                    config: ClusterConfig::default(),
                },
                now,
            )
            .unwrap();
        assert_eq!(resp.term, Term::new(2));
        assert_eq!(
            follower.first_log_index().unwrap(),
            LogIndex::new(3),
            "the tail beyond the snapshot survives"
        );
        assert_eq!(follower.commit_index(), LogIndex::new(2));
        assert_eq!(
            follower.snapshot_meta().unwrap().last_included_index,
            LogIndex::new(2)
        );
    }

    #[test]
    fn chunked_install_snapshot_is_rejected() {
        let now = Instant::now();
        let (_d, mut follower) = make("f", &["ldr"], 1, now);
        let mut req = InstallSnapshot {
            term: Term::new(1),
            leader_id: nid("ldr"),
            last_included_index: LogIndex::new(1),
            last_included_term: Term::new(1),
            offset: 4096,
            data: Bytes::from_static(b"chunk"),
            done: true,
            config: ClusterConfig::default(),
        };
        let err = follower
            .handle_install_snapshot(req.clone(), now)
            .unwrap_err();
        assert!(matches!(err, RaftError::Snapshot(_)));
        req.offset = 0;
        req.done = false;
        let err = follower.handle_install_snapshot(req, now).unwrap_err();
        assert!(matches!(err, RaftError::Snapshot(_)));
    }

    #[test]
    fn election_restriction_uses_snapshot_term_after_full_compaction() {
        let now = Instant::now();
        let (_d, mut voter) = make("v", &["c"], 1, now);
        // A leader replicates three entries (last: term 2, index 3) and
        // commits them; the voter then compacts its entire log away.
        let entries = vec![
            LogEntry::new(Term::new(1), LogIndex::new(1), Bytes::from_static(b"a")),
            LogEntry::new(Term::new(2), LogIndex::new(2), Bytes::from_static(b"b")),
            LogEntry::new(Term::new(2), LogIndex::new(3), Bytes::from_static(b"c")),
        ];
        voter
            .handle_append_entries(append_entries("ldr", 2, 0, 0, entries, 3), now)
            .unwrap();
        assert_eq!(voter.commit_index(), LogIndex::new(3));
        voter.compact(Bytes::from_static(b"machine@3")).unwrap();
        assert_eq!(voter.first_log_index().unwrap(), LogIndex::ZERO);

        // The comparator still sees (term 2, index 3) via the snapshot. The
        // requests arrive after the leader-stickiness window (§4.2.3) so the
        // election restriction alone decides.
        let later = now + Duration::from_secs(1);
        assert!(
            !voter
                .handle_request_vote(asks(2, 2), later)
                .unwrap()
                .vote_granted,
            "shorter log than the compacted (2,3) loses"
        );
        assert!(
            voter
                .handle_request_vote(asks(2, 3), later)
                .unwrap()
                .vote_granted,
            "an equal log is still up-to-date after compaction"
        );
    }

    // --- membership changes (I7b, Raft §6) ---------------------------------

    fn voter_set(names: &[&str]) -> BTreeSet<NodeId> {
        names.iter().map(|n| nid(n)).collect()
    }

    fn config_entry(term: u64, index: u64, config: ClusterConfig) -> LogEntry {
        LogEntry::with_payload(
            Term::new(term),
            LogIndex::new(index),
            EntryPayload::Config(config),
        )
    }

    #[test]
    fn config_applies_on_append_and_rolls_back_on_truncation() {
        // P10 — the plan calls this the nastiest edge, so it gets its own test.
        let now = Instant::now();
        let (_d, mut follower) = make("f", &["ldr"], 1, now);
        let bootstrap = follower.active_config().clone();

        // A leader replicates a joint config: it applies the moment it is
        // appended, not committed.
        let joint = ClusterConfig {
            voters: voter_set(&["a", "b"]),
            old_voters: Some(voter_set(&["f", "ldr"])),
            learners: BTreeSet::new(),
        };
        let req = append_entries("ldr", 2, 0, 0, vec![config_entry(2, 1, joint.clone())], 0);
        follower.handle_append_entries(req, now).unwrap();
        assert_eq!(follower.active_config(), &joint);
        assert!(follower.active_config().is_joint());

        // A higher-term leader overwrites index 1 with a plain command: the
        // truncation must roll the configuration back to the bootstrap one.
        let overwrite = append_entries(
            "ldr2",
            3,
            0,
            0,
            vec![LogEntry::new(
                Term::new(3),
                LogIndex::new(1),
                Bytes::from_static(b"cmd"),
            )],
            0,
        );
        follower.handle_append_entries(overwrite, now).unwrap();
        assert_eq!(
            follower.active_config(),
            &bootstrap,
            "P10: the truncated joint config no longer governs this node"
        );
        assert!(!follower.active_config().is_joint());
    }

    #[test]
    fn conf_change_adds_a_node_end_to_end() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(1);
        // Three founding members; n3 starts blank and out of the cluster.
        let founders = ["n0", "n1", "n2"];
        let mut dirs = Vec::new();
        let mut nodes = Vec::new();
        for (i, id) in founders.iter().enumerate() {
            let peers: Vec<&str> = founders.iter().filter(|p| *p != id).copied().collect();
            let (d, n) = make(id, &peers, 1 + i as u64, t0);
            dirs.push(d);
            nodes.push(n);
        }
        let (d3, n3) = make("n3", &["n0", "n1", "n2"], 7, t0);
        dirs.push(d3);
        nodes.push(n3);
        let mut c = Cluster { _dirs: dirs, nodes };

        c.tick_and_deliver(0, now);
        assert!(c.nodes[0].is_leader());
        assert_eq!(c.nodes[0].active_config().voters.len(), 3);

        // Add n3: the joint config goes in force immediately on the leader,
        // and the whole two-phase change runs to completion in one cascade.
        let outs = c.nodes[0]
            .propose_conf_change(voter_set(&["n0", "n1", "n2", "n3"]), now)
            .unwrap();
        c.deliver_all(batch(0, outs), now);

        for i in 0..4 {
            let cfg = c.nodes[i].active_config();
            assert!(!cfg.is_joint(), "n{i} settled on C_new");
            assert_eq!(cfg.voters.len(), 4, "n{i} sees the four-voter config");
        }
        // Joint at index 1, C_new at index 2 — both committed.
        assert_eq!(c.nodes[0].commit_index(), LogIndex::new(2));

        // The grown cluster keeps serving: a write commits with 3-of-4.
        let outs = c.nodes[0].propose(Bytes::from_static(b"w"), now).unwrap();
        c.deliver_all(batch(0, outs), now);
        assert_eq!(c.nodes[0].commit_index(), LogIndex::new(3));
        assert_eq!(c.nodes[3].last_log_index().unwrap(), LogIndex::new(3));
    }

    #[test]
    fn removed_leader_finishes_the_change_then_steps_down() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(1);
        let mut c = Cluster::new(&["n0", "n1", "n2"], &[1, 2, 3], t0);
        c.tick_and_deliver(0, now);
        assert!(c.nodes[0].is_leader());

        // Remove the leader itself: joint commits (majorities of {n0,n1,n2}
        // and {n1,n2}), C_new commits, and n0 steps down (paper §6).
        let outs = c.nodes[0]
            .propose_conf_change(voter_set(&["n1", "n2"]), now)
            .unwrap();
        c.deliver_all(batch(0, outs), now);

        assert!(
            !c.nodes[0].is_leader(),
            "a committed C_new that excludes the leader forces a step-down"
        );
        assert_eq!(c.nodes[1].active_config().voters, voter_set(&["n1", "n2"]));

        // The remaining pair elects among themselves and keeps serving.
        let later = now + Duration::from_secs(1);
        c.tick_and_deliver(1, later);
        assert_eq!(c.leaders(), vec![1], "n1 wins with n2's vote alone");
        let outs = c.nodes[1].propose(Bytes::from_static(b"w"), later).unwrap();
        c.deliver_all(batch(1, outs), later);
        assert!(c.nodes[1].commit_index() >= LogIndex::new(3));
    }

    #[test]
    fn joint_config_cannot_commit_without_a_new_set_majority() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(1);
        let mut c = Cluster::new(&["n0", "n1", "n2"], &[1, 2, 3], t0);
        c.tick_and_deliver(0, now);
        assert!(c.nodes[0].is_leader());

        // Move toward {n0, x, y} where x and y are unreachable: the joint
        // config demands a majority of BOTH voter sets, and {n0} alone can
        // never satisfy {n0, x, y}.
        let outs = c.nodes[0]
            .propose_conf_change(voter_set(&["n0", "x", "y"]), now)
            .unwrap();
        c.deliver_all(batch(0, outs), now);
        let outs = c.nodes[0]
            .propose(Bytes::from_static(b"stuck"), now)
            .unwrap();
        c.deliver_all(batch(0, outs), now);

        assert_eq!(
            c.nodes[0].commit_index(),
            LogIndex::ZERO,
            "no dual-majority window: nothing commits while C_new lacks a majority"
        );
        assert!(c.nodes[0].active_config().is_joint(), "stuck in joint");

        // And only one change may be in flight.
        let err = c.nodes[0]
            .propose_conf_change(voter_set(&["n0", "n1"]), now)
            .unwrap_err();
        assert!(matches!(err, RaftError::ConfChange(_)));
    }

    #[test]
    fn conf_change_input_validation() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(1);
        let (_d, mut follower) = make("f", &["a", "b"], 1, t0);
        let err = follower
            .propose_conf_change(voter_set(&["a"]), now)
            .unwrap_err();
        assert!(matches!(err, RaftError::NotLeader { .. }));

        let (_d2, mut solo) = make("solo", &[], 1, t0);
        solo.tick(now).unwrap();
        assert!(solo.is_leader());
        let err = solo.propose_conf_change(BTreeSet::new(), now).unwrap_err();
        assert!(matches!(err, RaftError::ConfChange(_)));
        let err = solo
            .propose_conf_change(voter_set(&["solo"]), now)
            .unwrap_err();
        assert!(
            matches!(err, RaftError::ConfChange(_)),
            "an unchanged voter set is rejected"
        );
    }

    #[test]
    fn snapshot_carries_config_across_restart() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(1);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raft.redb");
        {
            let log = RaftLog::open(&path).unwrap();
            let mut node = RaftNode::new(
                nid("f"),
                vec![nid("ldr")],
                log,
                Rng::seed_from_u64(1),
                Config::default(),
                t0,
            )
            .unwrap();
            // Learn a committed config {a, b, f} from a leader, then compact.
            let cfg = ClusterConfig::single(voter_set(&["a", "b", "f"]));
            let entries = vec![
                LogEntry::new(Term::new(2), LogIndex::new(1), Bytes::from_static(b"c1")),
                config_entry(2, 2, cfg.clone()),
            ];
            node.handle_append_entries(append_entries("ldr", 2, 0, 0, entries, 2), now)
                .unwrap();
            assert_eq!(node.active_config(), &cfg);
            let meta = node.compact(Bytes::from_static(b"machine@2")).unwrap();
            assert_eq!(
                meta.config, cfg,
                "the snapshot records the config at its point"
            );
        }
        // Restart with a DIFFERENT bootstrap: the snapshot's config wins.
        let log = RaftLog::open(&path).unwrap();
        let node = RaftNode::new(
            nid("f"),
            vec![nid("zzz")],
            log,
            Rng::seed_from_u64(1),
            Config::default(),
            t0 + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(
            node.active_config().voters,
            voter_set(&["a", "b", "f"]),
            "membership recovered from the snapshot, not the bootstrap list"
        );
    }

    #[test]
    fn install_snapshot_adopts_the_carried_config() {
        let now = Instant::now();
        let (_d, mut fresh) = make("g", &["ldr"], 1, now);
        let carried = ClusterConfig::single(voter_set(&["a", "b", "g"]));
        fresh
            .handle_install_snapshot(
                InstallSnapshot {
                    term: Term::new(3),
                    leader_id: nid("ldr"),
                    last_included_index: LogIndex::new(9),
                    last_included_term: Term::new(3),
                    offset: 0,
                    data: Bytes::from_static(b"machine@9"),
                    done: true,
                    config: carried.clone(),
                },
                now,
            )
            .unwrap();
        assert_eq!(fresh.active_config(), &carried);
        assert_eq!(fresh.snapshot_meta().unwrap().config, carried);
        assert_eq!(fresh.commit_index(), LogIndex::new(9));
    }

    // --- learners & the catch-up gate (I7c, thesis ch. 4) -------------------

    /// Builds a learner node `id` whose bootstrap cluster is `voters`.
    fn make_learner(
        id: &str,
        voters: &[&str],
        seed: u64,
        now: Instant,
    ) -> (tempfile::TempDir, RaftNode) {
        let dir = tempfile::tempdir().unwrap();
        let log = RaftLog::open(dir.path().join("raft.redb")).unwrap();
        let node = RaftNode::new_learner(
            nid(id),
            voters.iter().map(|p| nid(p)).collect(),
            log,
            Rng::seed_from_u64(seed),
            Config::default(),
            now,
        )
        .unwrap();
        (dir, node)
    }

    #[test]
    fn a_learner_never_campaigns() {
        let t0 = Instant::now();
        let (_d, mut learner) = make_learner("l", &["n0", "n1"], 1, t0);
        // Long past any election timeout, repeatedly: a learner re-arms and
        // stays a quiet follower instead of bumping terms.
        let mut now = t0;
        for _ in 0..10 {
            now += Duration::from_secs(1);
            let outs = learner.tick(now).unwrap();
            assert!(outs.is_empty(), "a learner sends no RequestVote");
        }
        assert!(learner.is_follower());
        assert_eq!(learner.current_term(), Term::ZERO, "term never moved");
    }

    #[test]
    fn add_learner_replicates_without_joint_consensus() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(1);
        // Two founding voters + a blank learner.
        let (d0, n0) = make("n0", &["n1"], 1, t0);
        let (d1, n1) = make("n1", &["n0"], 2, t0);
        let (dl, l) = make_learner("l", &["n0", "n1"], 3, t0);
        let mut c = Cluster {
            _dirs: vec![d0, d1, dl],
            nodes: vec![n0, n1, l],
        };
        c.tick_and_deliver(0, now);
        assert!(c.nodes[0].is_leader());

        let outs = c.nodes[0].propose_add_learner(nid("l"), now).unwrap();
        c.deliver_all(batch(0, outs), now);

        // A learner addition is a plain config entry, never a joint one.
        assert!(!c.nodes[0].active_config().is_joint());
        assert!(c.nodes[0].active_config().learners.contains(&nid("l")));
        assert_eq!(c.nodes[0].active_config().voters.len(), 2);

        // The learner receives entries like any member…
        let outs = c.nodes[0].propose(Bytes::from_static(b"w"), now).unwrap();
        c.deliver_all(batch(0, outs), now);
        assert_eq!(c.nodes[2].last_log_index().unwrap(), LogIndex::new(2));
        assert!(c.nodes[2].active_config().learners.contains(&nid("l")));

        // …but still never campaigns.
        let outs = c.nodes[2].tick(now + Duration::from_secs(5)).unwrap();
        assert!(outs.is_empty());
        assert!(c.nodes[2].is_follower());
    }

    #[test]
    fn promotion_is_gated_until_the_learner_catches_up() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(1);
        // A tight margin so a small log already trips the gate.
        let cfg = Config {
            catch_up_margin: 2,
            ..Config::default()
        };
        let mk = |id: &str, peers: &[&str], seed: u64| {
            let dir = tempfile::tempdir().unwrap();
            let log = RaftLog::open(dir.path().join("raft.redb")).unwrap();
            let node = RaftNode::new(
                nid(id),
                peers.iter().map(|p| nid(p)).collect(),
                log,
                Rng::seed_from_u64(seed),
                cfg.clone(),
                t0,
            )
            .unwrap();
            (dir, node)
        };
        let (d0, n0) = mk("n0", &["n1"], 1);
        let (d1, n1) = mk("n1", &["n0"], 2);
        let dirl = tempfile::tempdir().unwrap();
        let logl = RaftLog::open(dirl.path().join("raft.redb")).unwrap();
        let l = RaftNode::new_learner(
            nid("l"),
            vec![nid("n0"), nid("n1")],
            logl,
            Rng::seed_from_u64(3),
            cfg.clone(),
            t0,
        )
        .unwrap();
        let mut c = Cluster {
            _dirs: vec![d0, d1, dirl],
            nodes: vec![n0, n1, l],
        };
        c.tick_and_deliver(0, now);
        assert!(c.nodes[0].is_leader());

        // Six committed entries; the would-be voter has none of them.
        for k in 0..6u32 {
            let outs = c.nodes[0]
                .propose(Bytes::copy_from_slice(&k.to_le_bytes()), now)
                .unwrap();
            // Deliver only to n1 — the learner is not yet a member at all.
            let to_n1 = rv_or_ae_to(&outs, "n1");
            if let Outbound::AppendEntries { request, .. } = to_n1 {
                let resp = c.nodes[1].handle_append_entries(request, now).unwrap();
                c.nodes[0]
                    .handle_append_entries_response(nid("n1"), resp, now)
                    .unwrap();
            }
        }
        assert_eq!(c.nodes[0].commit_index(), LogIndex::new(6));

        // Promoting an uncaught-up node is refused with a pointer to the
        // learner flow.
        let err = c.nodes[0]
            .propose_conf_change(voter_set(&["n0", "n1", "l"]), now)
            .unwrap_err();
        assert!(matches!(err, RaftError::ConfChange(_)));
        assert!(
            err.to_string().contains("learner"),
            "error suggests the fix"
        );

        // The learner path: add, let it catch up, then promotion passes.
        let outs = c.nodes[0].propose_add_learner(nid("l"), now).unwrap();
        c.deliver_all(batch(0, outs), now);
        assert_eq!(c.nodes[2].last_log_index().unwrap(), LogIndex::new(7));

        let outs = c.nodes[0]
            .propose_conf_change(voter_set(&["n0", "n1", "l"]), now)
            .unwrap();
        c.deliver_all(batch(0, outs), now);
        let cfg_now = c.nodes[0].active_config();
        assert!(!cfg_now.is_joint(), "joint phase completed");
        assert_eq!(cfg_now.voters, voter_set(&["n0", "n1", "l"]));
        assert!(
            cfg_now.learners.is_empty(),
            "promotion removes the learner role"
        );
        // The promoted node now counts toward quorum: with n1 silenced, a
        // write still commits via {n0, l} — 2 of 3.
        let outs = c.nodes[0].propose(Bytes::from_static(b"z"), now).unwrap();
        for out in outs {
            if let Outbound::AppendEntries { to, request } = out {
                if to.as_str() == "l" {
                    let resp = c.nodes[2].handle_append_entries(request, now).unwrap();
                    c.nodes[0]
                        .handle_append_entries_response(nid("l"), resp, now)
                        .unwrap();
                }
            }
        }
        assert_eq!(
            c.nodes[0].commit_index().get(),
            c.nodes[0].last_log_index().unwrap().get(),
            "the promoted voter's ack completes the majority"
        );
    }

    #[test]
    fn add_learner_input_validation() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(1);
        let mut c = Cluster::new(&["n0", "n1", "n2"], &[1, 2, 3], t0);
        c.tick_and_deliver(0, now);
        assert!(c.nodes[0].is_leader());

        // An existing voter cannot become a learner.
        let err = c.nodes[0].propose_add_learner(nid("n1"), now).unwrap_err();
        assert!(matches!(err, RaftError::ConfChange(_)));

        // A follower rejects the proposal outright.
        let err = c.nodes[1].propose_add_learner(nid("x"), now).unwrap_err();
        assert!(matches!(err, RaftError::NotLeader { .. }));

        // Adding twice is refused once the first addition is in force.
        let outs = c.nodes[0].propose_add_learner(nid("x"), now).unwrap();
        c.deliver_all(batch(0, outs), now);
        let err = c.nodes[0].propose_add_learner(nid("x"), now).unwrap_err();
        assert!(matches!(err, RaftError::ConfChange(_)));
    }
}
