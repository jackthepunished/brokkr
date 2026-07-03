//! The Raft consensus node — leader election (milestone I3).
//!
//! [`RaftNode`] is the single-owner state machine at the heart of ADR 0013 D4's
//! actor model: it owns all of its state (role, persistent hard state, log,
//! election timers) and is driven by explicit calls — `tick` for the passage of
//! time, and `handle_*` for inbound RPCs and responses. There are **no locks**;
//! the async event-loop shell (which wires these methods to a [`Transport`] and
//! a real clock) is added with the simulation suite in milestone I5, where it
//! can be tested under simulated time.
//!
//! Keeping the logic in synchronous, side-effect-free-ish methods (each returns
//! the messages to send rather than sending them) is what makes leader election
//! **deterministically testable**: the tests below drive whole clusters of nodes
//! by hand with an injected clock and seeded RNG, no async runtime required.
//!
//! Everything here follows `docs/raft-notes.md`: §2.2 (terms as a logical
//! clock), §4 (leader election), §4.1 (randomized timeouts), §4.2 (RequestVote),
//! and §6 (the election restriction). Log replication is milestone I4; the
//! `AppendEntries` handling here does only what election needs (recognize a
//! leader, suppress elections via heartbeats).
//!
//! [`Transport`]: crate::Transport

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::error::RaftError;
use crate::rng::Rng;
use crate::state::HardState;
use crate::storage::RaftLog;
use crate::transport::{AppendEntries, AppendEntriesResponse, RequestVote, RequestVoteResponse};
use crate::types::{LogIndex, NodeId, Term};

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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            min_election_timeout: Duration::from_millis(150),
            max_election_timeout: Duration::from_millis(300),
            heartbeat_interval: Duration::from_millis(50),
        }
    }
}

/// The server's current role (`docs/raft-notes.md` §2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Role {
    Follower,
    /// Campaigning; tracks the set of nodes (including self) that granted a vote.
    Candidate {
        votes: BTreeSet<NodeId>,
    },
    Leader,
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
}

/// A Raft node: the consensus state machine for one server.
#[derive(Debug)]
pub struct RaftNode {
    id: NodeId,
    /// The other members of the cluster (self is excluded).
    peers: Vec<NodeId>,
    role: Role,
    /// Durable log + hard state.
    log: RaftLog,
    /// In-memory copy of the persisted hard state; every mutation is written
    /// through to `log` before the corresponding reply (persist-before-respond).
    hard: HardState,
    /// Highest index known committed (volatile).
    commit_index: LogIndex,
    /// The leader this node currently recognizes, if any.
    leader_id: Option<NodeId>,
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
    pub fn new(
        id: NodeId,
        peers: Vec<NodeId>,
        log: RaftLog,
        rng: Rng,
        config: Config,
        now: Instant,
    ) -> Result<Self, RaftError> {
        let hard = log.load_hard_state()?;
        let mut node = RaftNode {
            id,
            peers,
            role: Role::Follower,
            log,
            hard,
            commit_index: LogIndex::ZERO,
            leader_id: None,
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
        matches!(self.role, Role::Leader)
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

    // --- time -------------------------------------------------------------

    /// Advances logical time to `now`, returning any messages to send.
    ///
    /// - A non-leader whose election timer has expired starts an election.
    /// - A leader whose heartbeat timer has expired emits heartbeats.
    pub fn tick(&mut self, now: Instant) -> Result<Vec<Outbound>, RaftError> {
        if self.is_leader() {
            if now >= self.heartbeat_deadline {
                self.arm_heartbeat_timer(now);
                return self.heartbeats();
            }
            Ok(Vec::new())
        } else {
            if now >= self.election_deadline {
                return self.start_election(now);
            }
            Ok(Vec::new())
        }
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
    /// term, vote for self (persisted), and request votes from all peers. A
    /// single-node cluster wins immediately.
    fn start_election(&mut self, now: Instant) -> Result<Vec<Outbound>, RaftError> {
        let new_term = self.hard.current_term.next();
        // Advance the term (clearing any prior vote), then vote for ourselves,
        // and persist before we count on that vote.
        self.hard = self.hard.stepped_to(new_term).voting_for(self.id.clone());
        self.log.save_hard_state(&self.hard)?;

        let mut votes = BTreeSet::new();
        votes.insert(self.id.clone());
        self.role = Role::Candidate { votes };
        self.leader_id = None;
        self.arm_election_timer(now);

        // A one-node cluster is its own majority.
        if self.is_majority(1) {
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
            .peers
            .iter()
            .map(|peer| Outbound::RequestVote {
                to: peer.clone(),
                request: request.clone(),
            })
            .collect())
    }

    fn become_leader(&mut self, now: Instant) -> Result<Vec<Outbound>, RaftError> {
        self.role = Role::Leader;
        self.leader_id = Some(self.id.clone());
        self.arm_heartbeat_timer(now);
        // TODO(I4): append a no-op entry so the leader can commit and learn its
        // commit index for its term (docs/raft-notes.md §7, §11).
        self.heartbeats()
    }

    fn heartbeats(&self) -> Result<Vec<Outbound>, RaftError> {
        let (prev_log_index, prev_log_term) = self.log.last_index_and_term()?;
        let request = AppendEntries {
            term: self.hard.current_term,
            leader_id: self.id.clone(),
            prev_log_index,
            prev_log_term,
            entries: Vec::new(),
            leader_commit: self.commit_index,
        };
        Ok(self
            .peers
            .iter()
            .map(|peer| Outbound::AppendEntries {
                to: peer.clone(),
                request: request.clone(),
            })
            .collect())
    }

    fn cluster_size(&self) -> usize {
        self.peers.len() + 1
    }

    /// Whether `count` votes constitute a strict majority of the whole cluster.
    fn is_majority(&self, count: usize) -> bool {
        count * 2 > self.cluster_size()
    }

    // --- RequestVote (§4.2) ----------------------------------------------

    /// Handles an inbound `RequestVote` and returns the reply. Any granted vote
    /// (and any term bump) is persisted before returning (persist-before-respond).
    pub fn handle_request_vote(
        &mut self,
        request: RequestVote,
        now: Instant,
    ) -> Result<RequestVoteResponse, RaftError> {
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
    /// we step down; if it grants a vote and we reach a majority we become
    /// leader (and emit initial heartbeats).
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

        let cluster = self.cluster_size();
        let reached_majority = if let Role::Candidate { votes } = &mut self.role {
            if response.vote_granted {
                votes.insert(from);
            }
            votes.len() * 2 > cluster
        } else {
            false
        };

        if reached_majority {
            self.become_leader(now)
        } else {
            Ok(Vec::new())
        }
    }

    // --- AppendEntries (heartbeat only in I3; replication is I4) ----------

    /// Handles an inbound `AppendEntries`. In I3 this recognizes a legitimate
    /// leader for the term, steps down if we were campaigning, and resets the
    /// election timer so heartbeats suppress elections. The log-consistency
    /// check and entry replication arrive in I4.
    pub fn handle_append_entries(
        &mut self,
        request: AppendEntries,
        now: Instant,
    ) -> Result<AppendEntriesResponse, RaftError> {
        self.observe_term(request.term, now)?;
        let term = self.hard.current_term;

        // Reject a stale leader.
        if request.term < term {
            return Ok(AppendEntriesResponse {
                term,
                success: false,
                conflict_term: Term::ZERO,
                conflict_index: LogIndex::ZERO,
            });
        }

        // Valid leader for this term: accept its authority and suppress our own
        // election timer.
        self.role = Role::Follower;
        self.leader_id = Some(request.leader_id.clone());
        self.arm_election_timer(now);

        // TODO(I4): consistency check at prev_log_index/prev_log_term, conflict
        // truncation, entry append, and commit-index advance. I3 sends only
        // empty heartbeats, so acknowledging is sufficient for elections.
        Ok(AppendEntriesResponse {
            term,
            success: true,
            conflict_term: Term::ZERO,
            conflict_index: LogIndex::ZERO,
        })
    }

    /// Handles a peer's reply to our `AppendEntries`. In I3 the only action is
    /// the universal term rule: a reply carrying a higher term makes this leader
    /// step down. Replication bookkeeping (`matchIndex`/`nextIndex`, commit
    /// advance) is milestone I4.
    pub fn handle_append_entries_response(
        &mut self,
        _from: NodeId,
        response: AppendEntriesResponse,
        now: Instant,
    ) -> Result<(), RaftError> {
        self.observe_term(response.term, now)?;
        Ok(())
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

    #[test]
    fn higher_term_request_vote_steps_a_leader_down() {
        let t0 = Instant::now();
        let mut c = Cluster::new(&["n0", "n1", "n2"], &[1, 2, 3], t0);
        c.tick_and_deliver(0, t0 + Duration::from_secs(1));
        assert!(c.nodes[0].is_leader());
        assert_eq!(c.nodes[0].current_term(), Term::new(1));

        // A RequestVote from a higher term forces the leader back to follower.
        let resp = c.nodes[0]
            .handle_request_vote(request_vote("n1", 2), t0)
            .unwrap();
        assert!(c.nodes[0].is_follower());
        assert_eq!(c.nodes[0].current_term(), Term::new(2));
        assert!(resp.vote_granted, "empty logs are equally up-to-date");
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

        fn idx(&self, id: &NodeId) -> usize {
            self.nodes.iter().position(|n| n.id() == id).unwrap()
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
        /// each reply back to its sender.
        fn deliver_all(&mut self, initial: Vec<(usize, Outbound)>, now: Instant) {
            let mut queue: VecDeque<(usize, Outbound)> = initial.into_iter().collect();
            let mut budget = 10_000;
            while let Some((from, out)) = queue.pop_front() {
                budget -= 1;
                assert!(budget > 0, "message storm — election is not converging");
                match out {
                    Outbound::RequestVote { to, request } => {
                        let ti = self.idx(&to);
                        let resp = self.nodes[ti].handle_request_vote(request, now).unwrap();
                        let more = self.nodes[from]
                            .handle_request_vote_response(to, resp, now)
                            .unwrap();
                        for o in more {
                            queue.push_back((from, o));
                        }
                    }
                    Outbound::AppendEntries { to, request } => {
                        let ti = self.idx(&to);
                        let resp = self.nodes[ti].handle_append_entries(request, now).unwrap();
                        self.nodes[from]
                            .handle_append_entries_response(to, resp, now)
                            .unwrap();
                    }
                }
            }
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
}
