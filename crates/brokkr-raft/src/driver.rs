//! The async event-loop shell that runs a [`RaftNode`] (milestone I5b).
//!
//! [`RaftNode`] is a synchronous state machine that only *returns* the messages
//! it wants sent (ADR 0013 D4). [`RaftDriver`] is the imperative shell around it:
//! a single `tokio` task that owns the node — **no locks** — and a
//! `tokio::select!` loop that drives it from four sources:
//!
//! - a periodic **tick** (timer) → [`RaftNode::tick`];
//! - **inbound RPCs** from peers (delivered over a channel by the server side) →
//!   the node's `handle_request_vote` / `handle_append_entries`, whose reply is
//!   returned to the caller;
//! - **replies** to our own outbound RPCs → the node's `*_response` handlers;
//! - **client proposals** → [`RaftNode::propose`].
//!
//! Whenever the node returns [`Outbound`] messages, the driver dispatches each on
//! a detached task through the injected [`Transport`], funnelling the peer's
//! reply back into the loop. Because the node is touched only inside the loop,
//! there is exactly one writer and no shared-state locking.
//!
//! The driver is transport- and clock-agnostic: it reads "now" from
//! `tokio::time`, so it runs on real time in production and on **simulated** time
//! under `turmoil` or `tokio::time::pause()` — which is what makes the
//! multi-node tests deterministic.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;

use crate::error::RaftError;
use crate::node::{Outbound, RaftNode};
use crate::transport::{
    AppendEntries, AppendEntriesResponse, InstallSnapshot, InstallSnapshotResponse, RaftRpc,
    RequestVote, RequestVoteResponse, Transport,
};
use crate::types::{ClusterConfig, LogEntry, LogIndex, NodeId, SnapshotMeta, Term};

/// The replicated state machine the shell applies committed entries to
/// (I8b, plan §17 task 6). `brokkr-raft` stays payload-agnostic: the driver
/// feeds **every** committed entry through [`StateMachine::apply`] in log
/// order (implementations typically ignore `Noop` and `Config` payloads),
/// asks for a serialized snapshot when the log outgrows the compaction
/// threshold, and restores from a snapshot blob at startup or when the
/// leader installs one.
pub trait StateMachine: Send + 'static {
    /// Applies one committed entry. Called exactly once per index, in order.
    fn apply(&mut self, entry: &LogEntry);

    /// Serializes the current state — everything applied so far. Used as the
    /// snapshot blob (I6): restoring it and replaying the log tail must
    /// reproduce this exact state (P9).
    fn snapshot(&self) -> Bytes;

    /// Replaces the current state with a deserialized snapshot blob.
    fn restore(&mut self, snapshot: &[u8]);
}

/// A point-in-time snapshot of a driver's node state, for observability/tests.
#[derive(Debug, Clone)]
pub struct DriverStatus {
    /// Whether the node currently believes it is the leader.
    pub is_leader: bool,
    /// The node's current term.
    pub term: Term,
    /// The node's commit index.
    pub commit_index: LogIndex,
    /// The highest index applied to the state machine (I8b).
    pub last_applied: LogIndex,
    /// The node's last log index.
    pub last_log_index: LogIndex,
    /// The leader the node currently recognizes, if any.
    pub leader: Option<NodeId>,
    /// The node's installed snapshot metadata, if any (I6).
    pub snapshot: Option<SnapshotMeta>,
    /// The cluster configuration currently in force (I7b).
    pub config: ClusterConfig,
}

/// Work delivered into the driver's event loop.
enum Inbound {
    RequestVote(RequestVote, oneshot::Sender<RequestVoteResponse>),
    AppendEntries(AppendEntries, oneshot::Sender<AppendEntriesResponse>),
    InstallSnapshot(InstallSnapshot, oneshot::Sender<InstallSnapshotResponse>),
    Compact(Bytes, oneshot::Sender<Result<SnapshotMeta, RaftError>>),
    ConfChange(BTreeSet<NodeId>, oneshot::Sender<Result<(), RaftError>>),
    AddLearner(NodeId, oneshot::Sender<Result<(), RaftError>>),
    ReadIndex(oneshot::Sender<Result<LogIndex, RaftError>>),
    ProposeCommitted(Bytes, oneshot::Sender<Result<LogIndex, RaftError>>),
    Status(oneshot::Sender<DriverStatus>),
}

/// A client proposal delivered into the driver's event loop.
struct Proposal {
    command: Bytes,
    reply: oneshot::Sender<Result<(), RaftError>>,
}

/// A reply to one of our outbound RPCs, fed back into the loop.
enum PeerReply {
    Vote(NodeId, RequestVoteResponse),
    Append(NodeId, AppendEntriesResponse),
    Snapshot(NodeId, InstallSnapshotResponse),
}

/// A cloneable handle to a running [`RaftDriver`]. It is both the inbound-RPC
/// sink (it implements [`RaftRpc`], so a server can forward peer RPCs to the
/// node) and the client interface ([`RaftHandle::propose`], [`RaftHandle::status`]).
#[derive(Clone, Debug)]
pub struct RaftHandle {
    inbound: mpsc::Sender<Inbound>,
    proposals: mpsc::Sender<Proposal>,
}

impl RaftHandle {
    /// Proposes a client command; resolves once the leader has appended it
    /// (errors with [`RaftError::NotLeader`] if this node is not the leader).
    pub async fn propose(&self, command: Bytes) -> Result<(), RaftError> {
        let (tx, rx) = oneshot::channel();
        self.proposals
            .send(Proposal { command, reply: tx })
            .await
            .map_err(|_| RaftError::Transport("raft driver stopped".to_string()))?;
        rx.await
            .map_err(|_| RaftError::Transport("raft driver dropped reply".to_string()))?
    }

    /// Returns a snapshot of the node's state.
    pub async fn status(&self) -> Result<DriverStatus, RaftError> {
        self.query(Inbound::Status).await
    }

    /// Compacts the node's committed log prefix into a snapshot whose opaque
    /// blob is `data` — the state machine's serialized state at the node's
    /// current commit index (I6). Compaction also happens automatically once
    /// the committed log outgrows `Config::snapshot_threshold` (I8b), using
    /// [`StateMachine::snapshot`]; this manual entry point remains for tests
    /// and operational tooling. `data` must describe the fully-applied state.
    pub async fn compact(&self, data: Bytes) -> Result<SnapshotMeta, RaftError> {
        self.query(|tx| Inbound::Compact(data, tx)).await?
    }

    /// Performs the ReadIndex protocol (I8b): confirms this node is still the
    /// leader with a quorum round, then resolves with an index such that any
    /// state-machine read served **after** this call returns linearizable
    /// data — the driver has applied at least up to the returned index.
    /// Errors with [`RaftError::NotLeader`] on a non-leader or if leadership
    /// is lost during confirmation.
    pub async fn read_index(&self) -> Result<LogIndex, RaftError> {
        self.query(Inbound::ReadIndex).await?
    }

    /// Proposes a client command and resolves only once it is **committed
    /// and applied** to the state machine (I8c) — a subsequent leader-local
    /// read observes it. Returns the entry's log index. Errors with
    /// [`RaftError::NotLeader`] on a non-leader, or if leadership is lost
    /// and the entry is overwritten before committing.
    pub async fn propose_committed(&self, command: Bytes) -> Result<LogIndex, RaftError> {
        self.query(|tx| Inbound::ProposeCommitted(command, tx))
            .await?
    }

    /// Begins a joint-consensus membership change to `new_voters` (I7b).
    /// Resolves once the leader has appended C_old,new; the C_new phase and
    /// any step-down happen automatically as commits advance.
    pub async fn propose_conf_change(&self, new_voters: BTreeSet<NodeId>) -> Result<(), RaftError> {
        self.query(|tx| Inbound::ConfChange(new_voters, tx)).await?
    }

    /// Adds `id` as a non-voting learner (I7c): replicated to, never counted.
    /// Promote it later with [`RaftHandle::propose_conf_change`] once the
    /// catch-up gate is satisfied.
    pub async fn propose_add_learner(&self, id: NodeId) -> Result<(), RaftError> {
        self.query(|tx| Inbound::AddLearner(id, tx)).await?
    }

    async fn query<R>(
        &self,
        make: impl FnOnce(oneshot::Sender<R>) -> Inbound,
    ) -> Result<R, RaftError> {
        let (tx, rx) = oneshot::channel();
        self.inbound
            .send(make(tx))
            .await
            .map_err(|_| RaftError::Transport("raft driver stopped".to_string()))?;
        rx.await
            .map_err(|_| RaftError::Transport("raft driver dropped reply".to_string()))
    }
}

#[async_trait]
impl RaftRpc for RaftHandle {
    async fn request_vote(&self, req: RequestVote) -> Result<RequestVoteResponse, RaftError> {
        self.query(|tx| Inbound::RequestVote(req, tx)).await
    }

    async fn append_entries(&self, req: AppendEntries) -> Result<AppendEntriesResponse, RaftError> {
        self.query(|tx| Inbound::AppendEntries(req, tx)).await
    }

    async fn install_snapshot(
        &self,
        req: InstallSnapshot,
    ) -> Result<InstallSnapshotResponse, RaftError> {
        self.query(|tx| Inbound::InstallSnapshot(req, tx)).await
    }
}

/// Consumes an [`AppliedWaiter`]'s sender inside `retain_mut` (a oneshot
/// sender must be moved to send; swap in a dummy channel's sender).
fn take_sender(w: &mut AppliedWaiter) -> oneshot::Sender<Result<LogIndex, RaftError>> {
    let (dummy, _) = oneshot::channel();
    std::mem::replace(&mut w.sender, dummy)
}

/// A caller waiting for the state machine to apply up to `index`.
struct AppliedWaiter {
    index: LogIndex,
    /// `Some(term)` for a committed-proposal ack: the entry applied at
    /// `index` must carry this term, or the proposal was overwritten.
    expected_term: Option<Term>,
    sender: oneshot::Sender<Result<LogIndex, RaftError>>,
}

/// Runs a [`RaftNode`] as an async task over a [`Transport`], applying
/// committed entries to a [`StateMachine`] (I8b).
pub struct RaftDriver<T: Transport> {
    node: RaftNode,
    machine: Box<dyn StateMachine>,
    /// Highest log index fed to `machine` (P9: never regresses below the
    /// snapshot index).
    last_applied: LogIndex,
    /// Waiters for `last_applied` reaching an index: confirmed reads (no
    /// term expectation) and committed-proposal acks (the applied entry must
    /// carry the proposal's term, else leadership changed and the entry was
    /// overwritten — resolved as `NotLeader`, never as a false success).
    awaiting_apply: Vec<AppliedWaiter>,
    /// Registered reads waiting for the node to confirm leadership.
    reads_awaiting_confirm:
        std::collections::HashMap<u64, oneshot::Sender<Result<LogIndex, RaftError>>>,
    transport: Arc<T>,
    inbound: mpsc::Receiver<Inbound>,
    proposals: mpsc::Receiver<Proposal>,
    peer_reply_tx: mpsc::UnboundedSender<PeerReply>,
    peer_reply_rx: mpsc::UnboundedReceiver<PeerReply>,
    tick_period: Duration,
}

impl<T: Transport + 'static> RaftDriver<T> {
    /// Creates a driver for `node` that applies committed entries to
    /// `machine` and reaches peers via `transport`, ticking every
    /// `tick_period` of (possibly simulated) time. Returns the driver (call
    /// [`RaftDriver::run`] on its own task) and a [`RaftHandle`] to drive it.
    /// `tick_period` should be well below the election timeout.
    pub fn new(
        node: RaftNode,
        machine: Box<dyn StateMachine>,
        transport: Arc<T>,
        tick_period: Duration,
    ) -> (Self, RaftHandle) {
        let (inbound_tx, inbound_rx) = mpsc::channel(256);
        let (proposal_tx, proposal_rx) = mpsc::channel(256);
        let (peer_reply_tx, peer_reply_rx) = mpsc::unbounded_channel();
        let handle = RaftHandle {
            inbound: inbound_tx,
            proposals: proposal_tx,
        };
        let driver = RaftDriver {
            node,
            machine,
            last_applied: LogIndex::ZERO,
            awaiting_apply: Vec::new(),
            reads_awaiting_confirm: std::collections::HashMap::new(),
            transport,
            inbound: inbound_rx,
            proposals: proposal_rx,
            peer_reply_tx,
            peer_reply_rx,
            tick_period,
        };
        (driver, handle)
    }

    /// The current (possibly simulated) time. Under `turmoil` or
    /// `tokio::time::pause()` this follows the simulation clock, which is what
    /// keeps the driver deterministic.
    fn now() -> std::time::Instant {
        tokio::time::Instant::now().into_std()
    }

    /// Runs the event loop until every handle is dropped.
    pub async fn run(mut self) -> Result<(), RaftError> {
        let span = tracing::info_span!("raft_driver", node = %self.node.id());
        async move {
            // Startup restore (I8b): the state machine resumes from the
            // snapshot, then the committed log tail replays through the
            // ordinary apply path below (P9).
            if let Some((meta, blob)) = self.node.snapshot()? {
                self.machine.restore(&blob);
                self.last_applied = meta.last_included_index;
            }
            self.post_event()?;

            let mut ticker = tokio::time::interval(self.tick_period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        let outs = self.node.tick(Self::now())?;
                        self.dispatch(outs);
                    }
                    maybe = self.inbound.recv() => {
                        let Some(msg) = maybe else { break };
                        self.handle_inbound(msg)?;
                    }
                    maybe = self.proposals.recv() => {
                        let Some(prop) = maybe else { break };
                        match self.node.propose(prop.command, Self::now()) {
                            Ok(outs) => {
                                self.dispatch(outs);
                                let _ = prop.reply.send(Ok(()));
                            }
                            Err(e) => {
                                let _ = prop.reply.send(Err(e));
                            }
                        }
                    }
                    Some(reply) = self.peer_reply_rx.recv() => {
                        let now = Self::now();
                        let outs = match reply {
                            PeerReply::Vote(from, r) => {
                                self.node.handle_request_vote_response(from, r, now)?
                            }
                            PeerReply::Append(from, r) => {
                                self.node.handle_append_entries_response(from, r, now)?
                            }
                            PeerReply::Snapshot(from, r) => {
                                self.node.handle_install_snapshot_response(from, r, now)?
                            }
                        };
                        self.dispatch(outs);
                    }
                }
                // Every arm may have moved the commit index, installed a
                // snapshot, or resolved/failed a pending read.
                self.post_event()?;
            }
            Ok(())
        }
        .instrument(span)
        .await
    }

    /// The I8b bookkeeping run after every node interaction: restore from a
    /// freshly installed snapshot, apply newly committed entries, auto-compact
    /// past the threshold, and settle linearizable reads.
    fn post_event(&mut self) -> Result<(), RaftError> {
        // A leader-installed snapshot (I6) jumps state past the log we hold:
        // restore the machine from it instead of replaying (the covered
        // entries are gone). Term-checked proposal waiters skipped over by
        // the jump cannot verify their outcome — fail them rather than
        // guessing (reads are index-only and stay valid).
        if self.node.snapshot_index() > self.last_applied {
            if let Some((meta, blob)) = self.node.snapshot()? {
                self.machine.restore(&blob);
                self.last_applied = meta.last_included_index;
                let jumped_to = self.last_applied;
                self.awaiting_apply.retain_mut(|w| {
                    if w.index <= jumped_to && w.expected_term.is_some() {
                        let _ = take_sender(w).send(Err(RaftError::Transport(
                            "proposal outcome unknown: state jumped via snapshot".to_string(),
                        )));
                        false
                    } else {
                        true
                    }
                });
            }
        }

        // Apply the committed tail, in order, exactly once per index —
        // resolving each index's waiters against the entry actually applied.
        while self.last_applied < self.node.commit_index() {
            let next = self.last_applied.next();
            let entry = self.node.log_entry(next)?.ok_or_else(|| {
                RaftError::Storage(format!("committed entry {next} missing from the log"))
            })?;
            self.machine.apply(&entry);
            self.last_applied = next;
            self.awaiting_apply.retain_mut(|w| {
                if w.index != next {
                    return true;
                }
                let result = match w.expected_term {
                    // A committed-proposal ack: the applied entry must be
                    // OUR entry; a different term means it was overwritten.
                    Some(expected) if entry.term != expected => {
                        Err(RaftError::NotLeader { leader: None })
                    }
                    _ => Ok(next),
                };
                let _ = take_sender(w).send(result);
                false
            });
        }

        // Auto-compaction (I8b): once the committed log outgrows the
        // threshold, snapshot the fully-applied machine. `last_applied ==
        // commit_index` here, which is exactly the basis `compact` uses.
        if self.node.needs_snapshot() {
            self.node.compact(self.machine.snapshot())?;
        }

        // Settle reads: confirmations (or leadership-loss failures) from the
        // node, then any confirmed reads whose index is already applied.
        for (ticket, result) in self.node.drain_read_results() {
            let Some(sender) = self.reads_awaiting_confirm.remove(&ticket) else {
                continue; // caller gave up (dropped receiver)
            };
            match result {
                Ok(index) if index <= self.last_applied => {
                    let _ = sender.send(Ok(index));
                }
                Ok(index) => self.awaiting_apply.push(AppliedWaiter {
                    index,
                    expected_term: None,
                    sender,
                }),
                Err(e) => {
                    let _ = sender.send(Err(e));
                }
            }
        }
        Ok(())
    }

    fn handle_inbound(&mut self, msg: Inbound) -> Result<(), RaftError> {
        let now = Self::now();
        match msg {
            Inbound::RequestVote(req, reply) => {
                let resp = self.node.handle_request_vote(req, now)?;
                let _ = reply.send(resp);
            }
            Inbound::AppendEntries(req, reply) => {
                let resp = self.node.handle_append_entries(req, now)?;
                let _ = reply.send(resp);
            }
            Inbound::InstallSnapshot(req, reply) => {
                match self.node.handle_install_snapshot(req, now) {
                    Ok(resp) => {
                        let _ = reply.send(resp);
                    }
                    // A malformed request from a peer (e.g. chunked) must not
                    // kill this node's event loop: drop the reply — the sender
                    // observes an error — and keep running. Local faults
                    // (storage) stay fatal below.
                    Err(RaftError::Snapshot(reason)) => {
                        tracing::warn!(reason, "rejected inbound InstallSnapshot");
                    }
                    Err(e) => return Err(e),
                }
            }
            Inbound::Compact(data, reply) => {
                let _ = reply.send(self.node.compact(data));
            }
            Inbound::ConfChange(new_voters, reply) => {
                match self.node.propose_conf_change(new_voters, now) {
                    Ok(outs) => {
                        self.dispatch(outs);
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Inbound::AddLearner(id, reply) => match self.node.propose_add_learner(id, now) {
                Ok(outs) => {
                    self.dispatch(outs);
                    let _ = reply.send(Ok(()));
                }
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            },
            Inbound::ReadIndex(reply) => match self.node.start_read(now) {
                Ok((ticket, outs)) => {
                    self.reads_awaiting_confirm.insert(ticket, reply);
                    self.dispatch(outs);
                }
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            },
            Inbound::ProposeCommitted(command, reply) => {
                match self.node.propose(command, now) {
                    Ok(outs) => {
                        // Single-writer loop: the entry we just appended is
                        // the last log index, at the current term.
                        let index = self.node.last_log_index()?;
                        let term = self.node.current_term();
                        self.awaiting_apply.push(AppliedWaiter {
                            index,
                            expected_term: Some(term),
                            sender: reply,
                        });
                        self.dispatch(outs);
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Inbound::Status(reply) => {
                let status = DriverStatus {
                    is_leader: self.node.is_leader(),
                    term: self.node.current_term(),
                    commit_index: self.node.commit_index(),
                    last_applied: self.last_applied,
                    last_log_index: self.node.last_log_index()?,
                    leader: self.node.leader_id().cloned(),
                    snapshot: self.node.snapshot_meta(),
                    config: self.node.active_config().clone(),
                };
                let _ = reply.send(status);
            }
        }
        Ok(())
    }

    /// Sends each outbound message on a detached task, funnelling the reply back
    /// into the loop. A failed send (unreachable/partitioned peer) is dropped —
    /// Raft retries on the next tick.
    fn dispatch(&self, outs: Vec<Outbound>) {
        for out in outs {
            let transport = Arc::clone(&self.transport);
            let replies = self.peer_reply_tx.clone();
            match out {
                Outbound::RequestVote { to, request } => {
                    tokio::spawn(
                        async move {
                            if let Ok(resp) = transport.request_vote(&to, request).await {
                                let _ = replies.send(PeerReply::Vote(to, resp));
                            }
                        }
                        .in_current_span(),
                    );
                }
                Outbound::AppendEntries { to, request } => {
                    tokio::spawn(
                        async move {
                            if let Ok(resp) = transport.append_entries(&to, request).await {
                                let _ = replies.send(PeerReply::Append(to, resp));
                            }
                        }
                        .in_current_span(),
                    );
                }
                Outbound::InstallSnapshot { to, request } => {
                    tokio::spawn(
                        async move {
                            if let Ok(resp) = transport.install_snapshot(&to, request).await {
                                let _ = replies.send(PeerReply::Snapshot(to, resp));
                            }
                        }
                        .in_current_span(),
                    );
                }
            }
        }
    }
}
