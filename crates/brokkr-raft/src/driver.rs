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
use crate::types::{LogIndex, NodeId, SnapshotMeta, Term};

/// A point-in-time snapshot of a driver's node state, for observability/tests.
#[derive(Debug, Clone)]
pub struct DriverStatus {
    /// Whether the node currently believes it is the leader.
    pub is_leader: bool,
    /// The node's current term.
    pub term: Term,
    /// The node's commit index.
    pub commit_index: LogIndex,
    /// The node's last log index.
    pub last_log_index: LogIndex,
    /// The leader the node currently recognizes, if any.
    pub leader: Option<NodeId>,
    /// The node's installed snapshot metadata, if any (I6).
    pub snapshot: Option<SnapshotMeta>,
}

/// Work delivered into the driver's event loop.
enum Inbound {
    RequestVote(RequestVote, oneshot::Sender<RequestVoteResponse>),
    AppendEntries(AppendEntries, oneshot::Sender<AppendEntriesResponse>),
    InstallSnapshot(InstallSnapshot, oneshot::Sender<InstallSnapshotResponse>),
    Compact(Bytes, oneshot::Sender<Result<SnapshotMeta, RaftError>>),
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
    /// current commit index (I6). The shell calls this when the committed log
    /// outgrows the snapshot threshold; for I8's KV it is wired to the state
    /// machine's `snapshot()` callback.
    pub async fn compact(&self, data: Bytes) -> Result<SnapshotMeta, RaftError> {
        self.query(|tx| Inbound::Compact(data, tx)).await?
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

/// Runs a [`RaftNode`] as an async task over a [`Transport`].
pub struct RaftDriver<T: Transport> {
    node: RaftNode,
    transport: Arc<T>,
    inbound: mpsc::Receiver<Inbound>,
    proposals: mpsc::Receiver<Proposal>,
    peer_reply_tx: mpsc::UnboundedSender<PeerReply>,
    peer_reply_rx: mpsc::UnboundedReceiver<PeerReply>,
    tick_period: Duration,
}

impl<T: Transport + 'static> RaftDriver<T> {
    /// Creates a driver for `node` that reaches peers via `transport`, ticking
    /// every `tick_period` of (possibly simulated) time. Returns the driver
    /// (call [`RaftDriver::run`] on its own task) and a [`RaftHandle`] to drive
    /// it. `tick_period` should be well below the election timeout.
    pub fn new(node: RaftNode, transport: Arc<T>, tick_period: Duration) -> (Self, RaftHandle) {
        let (inbound_tx, inbound_rx) = mpsc::channel(256);
        let (proposal_tx, proposal_rx) = mpsc::channel(256);
        let (peer_reply_tx, peer_reply_rx) = mpsc::unbounded_channel();
        let handle = RaftHandle {
            inbound: inbound_tx,
            proposals: proposal_tx,
        };
        let driver = RaftDriver {
            node,
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
                        match self.node.propose(prop.command) {
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
            }
            Ok(())
        }
        .instrument(span)
        .await
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
            Inbound::Status(reply) => {
                let status = DriverStatus {
                    is_leader: self.node.is_leader(),
                    term: self.node.current_term(),
                    commit_index: self.node.commit_index(),
                    last_log_index: self.node.last_log_index()?,
                    leader: self.node.leader_id().cloned(),
                    snapshot: self.node.snapshot_meta(),
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
