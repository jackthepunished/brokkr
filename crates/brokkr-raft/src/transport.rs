//! Peer-to-peer transport abstraction for Raft RPCs (ADR 0013 D2).
//!
//! The node calls out through the [`Transport`] trait and answers inbound RPCs
//! through the [`RaftRpc`] trait. Two [`Transport`] implementations ship:
//!
//! - [`TonicTransport`] — production gRPC over a tonic [`Channel`] per peer.
//! - [`InMemoryTransport`] — routes calls to in-process [`RaftRpc`] handlers,
//!   with no sockets, for deterministic unit tests of the consensus logic
//!   (I2–I4).
//!
//! [`RaftServiceAdapter`] bridges a [`RaftRpc`] handler to the generated tonic
//! server, so a node can serve `RaftService`. Running the tonic stack over
//! `turmoil`'s simulated network for fault-injection is milestone I5 (ADR 0013
//! notes this glue is carried until the node exists); the deterministic
//! consensus tests before then drive [`InMemoryTransport`].
//!
//! Internal request/reply types keep the state machine free of generated prost
//! shapes; conversions to and from the wire protobuf are unit-tested for
//! round-trip fidelity.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tonic::transport::Channel;
use tonic::{Request, Response, Status};

use brokkr_proto::brokkr::v1 as pb;
use brokkr_proto::brokkr::v1::raft_service_client::RaftServiceClient;
use brokkr_proto::brokkr::v1::raft_service_server::{RaftService, RaftServiceServer};

use crate::error::RaftError;
use crate::types::{LogEntry, LogIndex, NodeId, Term};

// ---------------------------------------------------------------------------
// Internal request/reply types
// ---------------------------------------------------------------------------

/// A `RequestVote` call (Raft §5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestVote {
    /// Candidate's term.
    pub term: Term,
    /// Candidate requesting the vote.
    pub candidate_id: NodeId,
    /// Index of the candidate's last log entry.
    pub last_log_index: LogIndex,
    /// Term of the candidate's last log entry.
    pub last_log_term: Term,
}

/// The reply to a [`RequestVote`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestVoteResponse {
    /// Voter's current term.
    pub term: Term,
    /// Whether the vote was granted.
    pub vote_granted: bool,
}

/// An `AppendEntries` call (Raft §5.3), also used as a heartbeat when `entries`
/// is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendEntries {
    /// Leader's term.
    pub term: Term,
    /// Leader, so a follower can redirect clients.
    pub leader_id: NodeId,
    /// Index of the entry preceding `entries`.
    pub prev_log_index: LogIndex,
    /// Term of `prev_log_index`.
    pub prev_log_term: Term,
    /// Entries to store (empty for a heartbeat).
    pub entries: Vec<LogEntry>,
    /// Leader's commit index.
    pub leader_commit: LogIndex,
}

/// The reply to an [`AppendEntries`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendEntriesResponse {
    /// Follower's current term.
    pub term: Term,
    /// Whether the consistency check passed.
    pub success: bool,
    /// Conflict fast-backtrack hint: the term of the conflicting entry.
    pub conflict_term: Term,
    /// Conflict fast-backtrack hint: first index the follower holds for
    /// `conflict_term`.
    pub conflict_index: LogIndex,
}

/// An `InstallSnapshot` call (Raft §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallSnapshot {
    /// Leader's term.
    pub term: Term,
    /// Leader, so a follower can redirect clients.
    pub leader_id: NodeId,
    /// The snapshot replaces all entries up through this index.
    pub last_included_index: LogIndex,
    /// Term of `last_included_index`.
    pub last_included_term: Term,
    /// Byte offset of this chunk within the snapshot.
    pub offset: u64,
    /// Raw snapshot chunk.
    pub data: Bytes,
    /// Whether this is the final chunk.
    pub done: bool,
}

/// The reply to an [`InstallSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallSnapshotResponse {
    /// Follower's current term.
    pub term: Term,
}

// ---------------------------------------------------------------------------
// Wire conversions
// ---------------------------------------------------------------------------

impl From<RequestVote> for pb::RequestVoteRequest {
    fn from(r: RequestVote) -> Self {
        pb::RequestVoteRequest {
            term: r.term.get(),
            candidate_id: r.candidate_id.into_string(),
            last_log_index: r.last_log_index.get(),
            last_log_term: r.last_log_term.get(),
        }
    }
}

impl TryFrom<pb::RequestVoteRequest> for RequestVote {
    type Error = RaftError;
    fn try_from(p: pb::RequestVoteRequest) -> Result<Self, Self::Error> {
        Ok(RequestVote {
            term: Term::new(p.term),
            candidate_id: NodeId::new(p.candidate_id)?,
            last_log_index: LogIndex::new(p.last_log_index),
            last_log_term: Term::new(p.last_log_term),
        })
    }
}

impl From<RequestVoteResponse> for pb::RequestVoteReply {
    fn from(r: RequestVoteResponse) -> Self {
        pb::RequestVoteReply {
            term: r.term.get(),
            vote_granted: r.vote_granted,
        }
    }
}

impl From<pb::RequestVoteReply> for RequestVoteResponse {
    fn from(p: pb::RequestVoteReply) -> Self {
        RequestVoteResponse {
            term: Term::new(p.term),
            vote_granted: p.vote_granted,
        }
    }
}

impl From<AppendEntries> for pb::AppendEntriesRequest {
    fn from(r: AppendEntries) -> Self {
        pb::AppendEntriesRequest {
            term: r.term.get(),
            leader_id: r.leader_id.into_string(),
            prev_log_index: r.prev_log_index.get(),
            prev_log_term: r.prev_log_term.get(),
            entries: r.entries.iter().map(pb::LogEntry::from).collect(),
            leader_commit: r.leader_commit.get(),
        }
    }
}

impl TryFrom<pb::AppendEntriesRequest> for AppendEntries {
    type Error = RaftError;
    fn try_from(p: pb::AppendEntriesRequest) -> Result<Self, Self::Error> {
        Ok(AppendEntries {
            term: Term::new(p.term),
            leader_id: NodeId::new(p.leader_id)?,
            prev_log_index: LogIndex::new(p.prev_log_index),
            prev_log_term: Term::new(p.prev_log_term),
            entries: p.entries.into_iter().map(LogEntry::from).collect(),
            leader_commit: LogIndex::new(p.leader_commit),
        })
    }
}

impl From<AppendEntriesResponse> for pb::AppendEntriesReply {
    fn from(r: AppendEntriesResponse) -> Self {
        pb::AppendEntriesReply {
            term: r.term.get(),
            success: r.success,
            conflict_term: r.conflict_term.get(),
            conflict_index: r.conflict_index.get(),
        }
    }
}

impl From<pb::AppendEntriesReply> for AppendEntriesResponse {
    fn from(p: pb::AppendEntriesReply) -> Self {
        AppendEntriesResponse {
            term: Term::new(p.term),
            success: p.success,
            conflict_term: Term::new(p.conflict_term),
            conflict_index: LogIndex::new(p.conflict_index),
        }
    }
}

impl From<InstallSnapshot> for pb::InstallSnapshotRequest {
    fn from(r: InstallSnapshot) -> Self {
        pb::InstallSnapshotRequest {
            term: r.term.get(),
            leader_id: r.leader_id.into_string(),
            last_included_index: r.last_included_index.get(),
            last_included_term: r.last_included_term.get(),
            offset: r.offset,
            data: r.data.to_vec(),
            done: r.done,
        }
    }
}

impl TryFrom<pb::InstallSnapshotRequest> for InstallSnapshot {
    type Error = RaftError;
    fn try_from(p: pb::InstallSnapshotRequest) -> Result<Self, Self::Error> {
        Ok(InstallSnapshot {
            term: Term::new(p.term),
            leader_id: NodeId::new(p.leader_id)?,
            last_included_index: LogIndex::new(p.last_included_index),
            last_included_term: Term::new(p.last_included_term),
            offset: p.offset,
            data: Bytes::from(p.data),
            done: p.done,
        })
    }
}

impl From<InstallSnapshotResponse> for pb::InstallSnapshotReply {
    fn from(r: InstallSnapshotResponse) -> Self {
        pb::InstallSnapshotReply { term: r.term.get() }
    }
}

impl From<pb::InstallSnapshotReply> for InstallSnapshotResponse {
    fn from(p: pb::InstallSnapshotReply) -> Self {
        InstallSnapshotResponse {
            term: Term::new(p.term),
        }
    }
}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

/// Outbound transport: how a node reaches its peers.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Sends a `RequestVote` to `to`.
    async fn request_vote(
        &self,
        to: &NodeId,
        req: RequestVote,
    ) -> Result<RequestVoteResponse, RaftError>;

    /// Sends an `AppendEntries` to `to`.
    async fn append_entries(
        &self,
        to: &NodeId,
        req: AppendEntries,
    ) -> Result<AppendEntriesResponse, RaftError>;

    /// Sends an `InstallSnapshot` to `to`.
    async fn install_snapshot(
        &self,
        to: &NodeId,
        req: InstallSnapshot,
    ) -> Result<InstallSnapshotResponse, RaftError>;
}

/// Inbound handler: how a node answers RPCs from peers. Implemented by the
/// consensus state machine (milestones I3–I4).
#[async_trait]
pub trait RaftRpc: Send + Sync {
    /// Handles an inbound `RequestVote`.
    async fn request_vote(&self, req: RequestVote) -> Result<RequestVoteResponse, RaftError>;

    /// Handles an inbound `AppendEntries`.
    async fn append_entries(&self, req: AppendEntries) -> Result<AppendEntriesResponse, RaftError>;

    /// Handles an inbound `InstallSnapshot`.
    async fn install_snapshot(
        &self,
        req: InstallSnapshot,
    ) -> Result<InstallSnapshotResponse, RaftError>;
}

// ---------------------------------------------------------------------------
// InMemoryTransport
// ---------------------------------------------------------------------------

/// A [`Transport`] that dispatches directly to in-process [`RaftRpc`] handlers.
///
/// Deterministic and socket-free — the substrate for unit-testing the consensus
/// logic (I2–I4) and, later, for the linearizability checker.
#[derive(Clone, Default)]
pub struct InMemoryTransport {
    peers: HashMap<NodeId, Arc<dyn RaftRpc>>,
}

impl InMemoryTransport {
    /// An empty transport with no peers.
    pub fn new() -> Self {
        InMemoryTransport {
            peers: HashMap::new(),
        }
    }

    /// Registers a peer handler (builder style).
    pub fn with_peer(mut self, id: NodeId, handler: Arc<dyn RaftRpc>) -> Self {
        self.peers.insert(id, handler);
        self
    }

    /// Registers a peer handler.
    pub fn insert_peer(&mut self, id: NodeId, handler: Arc<dyn RaftRpc>) {
        self.peers.insert(id, handler);
    }

    fn peer(&self, to: &NodeId) -> Result<Arc<dyn RaftRpc>, RaftError> {
        self.peers
            .get(to)
            .cloned()
            .ok_or_else(|| RaftError::UnknownPeer(to.to_string()))
    }
}

impl fmt::Debug for InMemoryTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemoryTransport")
            .field("peers", &self.peers.len())
            .finish()
    }
}

#[async_trait]
impl Transport for InMemoryTransport {
    async fn request_vote(
        &self,
        to: &NodeId,
        req: RequestVote,
    ) -> Result<RequestVoteResponse, RaftError> {
        self.peer(to)?.request_vote(req).await
    }

    async fn append_entries(
        &self,
        to: &NodeId,
        req: AppendEntries,
    ) -> Result<AppendEntriesResponse, RaftError> {
        self.peer(to)?.append_entries(req).await
    }

    async fn install_snapshot(
        &self,
        to: &NodeId,
        req: InstallSnapshot,
    ) -> Result<InstallSnapshotResponse, RaftError> {
        self.peer(to)?.install_snapshot(req).await
    }
}

// ---------------------------------------------------------------------------
// TonicTransport
// ---------------------------------------------------------------------------

/// A [`Transport`] over gRPC, holding one lazily-connecting tonic [`Channel`]
/// per peer. Production transport; the same channels can be built over
/// `turmoil`'s simulated TCP for the I5 fault-injection suite.
#[derive(Clone, Default)]
pub struct TonicTransport {
    peers: HashMap<NodeId, Channel>,
}

impl TonicTransport {
    /// An empty transport with no peers.
    pub fn new() -> Self {
        TonicTransport {
            peers: HashMap::new(),
        }
    }

    /// Registers a peer channel (builder style).
    pub fn with_peer(mut self, id: NodeId, channel: Channel) -> Self {
        self.peers.insert(id, channel);
        self
    }

    /// Registers a peer channel.
    pub fn insert_peer(&mut self, id: NodeId, channel: Channel) {
        self.peers.insert(id, channel);
    }

    fn client(&self, to: &NodeId) -> Result<RaftServiceClient<Channel>, RaftError> {
        let channel = self
            .peers
            .get(to)
            .ok_or_else(|| RaftError::UnknownPeer(to.to_string()))?;
        Ok(RaftServiceClient::new(channel.clone()))
    }
}

impl fmt::Debug for TonicTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TonicTransport")
            .field("peers", &self.peers.len())
            .finish()
    }
}

#[async_trait]
impl Transport for TonicTransport {
    #[tracing::instrument(level = "debug", skip(self, req), fields(peer = %to, term = req.term.get()))]
    async fn request_vote(
        &self,
        to: &NodeId,
        req: RequestVote,
    ) -> Result<RequestVoteResponse, RaftError> {
        let mut client = self.client(to)?;
        let reply = client
            .request_vote(pb::RequestVoteRequest::from(req))
            .await?;
        Ok(reply.into_inner().into())
    }

    #[tracing::instrument(level = "debug", skip(self, req), fields(peer = %to, term = req.term.get(), entries = req.entries.len()))]
    async fn append_entries(
        &self,
        to: &NodeId,
        req: AppendEntries,
    ) -> Result<AppendEntriesResponse, RaftError> {
        let mut client = self.client(to)?;
        let reply = client
            .append_entries(pb::AppendEntriesRequest::from(req))
            .await?;
        Ok(reply.into_inner().into())
    }

    #[tracing::instrument(level = "debug", skip(self, req), fields(peer = %to, term = req.term.get(), last_included = req.last_included_index.get()))]
    async fn install_snapshot(
        &self,
        to: &NodeId,
        req: InstallSnapshot,
    ) -> Result<InstallSnapshotResponse, RaftError> {
        let mut client = self.client(to)?;
        let reply = client
            .install_snapshot(pb::InstallSnapshotRequest::from(req))
            .await?;
        Ok(reply.into_inner().into())
    }
}

// ---------------------------------------------------------------------------
// Server adapter
// ---------------------------------------------------------------------------

/// Bridges a [`RaftRpc`] handler to the generated tonic `RaftService` server.
///
/// Wrap a node's handler and call [`RaftServiceAdapter::into_server`] to get a
/// service ready for `tonic::transport::Server`.
pub struct RaftServiceAdapter<H: RaftRpc> {
    handler: Arc<H>,
}

impl<H: RaftRpc> RaftServiceAdapter<H> {
    /// Wraps a handler.
    pub fn new(handler: Arc<H>) -> Self {
        RaftServiceAdapter { handler }
    }

    /// Consumes self into a tonic server service.
    pub fn into_server(self) -> RaftServiceServer<Self>
    where
        H: 'static,
    {
        RaftServiceServer::new(self)
    }
}

// Manual `Debug` (the wrapped `H: RaftRpc` handler is not required to be `Debug`).
impl<H: RaftRpc> fmt::Debug for RaftServiceAdapter<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RaftServiceAdapter").finish_non_exhaustive()
    }
}

#[tonic::async_trait]
impl<H: RaftRpc + 'static> RaftService for RaftServiceAdapter<H> {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn request_vote(
        &self,
        request: Request<pb::RequestVoteRequest>,
    ) -> Result<Response<pb::RequestVoteReply>, Status> {
        let req = RequestVote::try_from(request.into_inner())
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let resp = self
            .handler
            .request_vote(req)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(resp.into()))
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn append_entries(
        &self,
        request: Request<pb::AppendEntriesRequest>,
    ) -> Result<Response<pb::AppendEntriesReply>, Status> {
        let req = AppendEntries::try_from(request.into_inner())
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let resp = self
            .handler
            .append_entries(req)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(resp.into()))
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn install_snapshot(
        &self,
        request: Request<pb::InstallSnapshotRequest>,
    ) -> Result<Response<pb::InstallSnapshotReply>, Status> {
        let req = InstallSnapshot::try_from(request.into_inner())
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let resp = self
            .handler
            .install_snapshot(req)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(resp.into()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn node(id: &str) -> NodeId {
        NodeId::new(id).unwrap()
    }

    fn sample_request_vote() -> RequestVote {
        RequestVote {
            term: Term::new(4),
            candidate_id: node("cand"),
            last_log_index: LogIndex::new(10),
            last_log_term: Term::new(3),
        }
    }

    #[test]
    fn request_vote_wire_round_trips() {
        let rv = sample_request_vote();
        let proto = pb::RequestVoteRequest::from(rv.clone());
        let back = RequestVote::try_from(proto).unwrap();
        assert_eq!(rv, back);
    }

    #[test]
    fn append_entries_wire_round_trips() {
        let ae = AppendEntries {
            term: Term::new(5),
            leader_id: node("leader"),
            prev_log_index: LogIndex::new(7),
            prev_log_term: Term::new(4),
            entries: vec![
                LogEntry::new(Term::new(5), LogIndex::new(8), Bytes::from_static(b"a")),
                LogEntry::new(Term::new(5), LogIndex::new(9), Bytes::from_static(b"b")),
            ],
            leader_commit: LogIndex::new(7),
        };
        let proto = pb::AppendEntriesRequest::from(ae.clone());
        assert_eq!(proto.entries.len(), 2);
        let back = AppendEntries::try_from(proto).unwrap();
        assert_eq!(ae, back);
    }

    #[test]
    fn install_snapshot_wire_round_trips() {
        let is = InstallSnapshot {
            term: Term::new(6),
            leader_id: node("leader"),
            last_included_index: LogIndex::new(100),
            last_included_term: Term::new(6),
            offset: 4096,
            data: Bytes::from_static(b"snapshot-chunk"),
            done: true,
        };
        let proto = pb::InstallSnapshotRequest::from(is.clone());
        let back = InstallSnapshot::try_from(proto).unwrap();
        assert_eq!(is, back);
    }

    #[test]
    fn append_entries_response_wire_round_trips() {
        let resp = AppendEntriesResponse {
            term: Term::new(9),
            success: false,
            conflict_term: Term::new(7),
            conflict_index: LogIndex::new(12),
        };
        let proto = pb::AppendEntriesReply::from(resp);
        let back = AppendEntriesResponse::from(proto);
        assert_eq!(resp, back);
    }

    #[test]
    fn try_from_rejects_empty_node_id() {
        let proto = pb::RequestVoteRequest {
            term: 1,
            candidate_id: String::new(),
            last_log_index: 0,
            last_log_term: 0,
        };
        assert!(RequestVote::try_from(proto).is_err());
    }

    /// A canned handler that records the last request it saw and replies with a
    /// fixed term.
    struct StubHandler {
        reply_term: Term,
        grant: bool,
    }

    #[async_trait]
    impl RaftRpc for StubHandler {
        async fn request_vote(&self, _req: RequestVote) -> Result<RequestVoteResponse, RaftError> {
            Ok(RequestVoteResponse {
                term: self.reply_term,
                vote_granted: self.grant,
            })
        }

        async fn append_entries(
            &self,
            req: AppendEntries,
        ) -> Result<AppendEntriesResponse, RaftError> {
            Ok(AppendEntriesResponse {
                term: self.reply_term,
                success: true,
                conflict_term: Term::ZERO,
                conflict_index: req.prev_log_index,
            })
        }

        async fn install_snapshot(
            &self,
            _req: InstallSnapshot,
        ) -> Result<InstallSnapshotResponse, RaftError> {
            Ok(InstallSnapshotResponse {
                term: self.reply_term,
            })
        }
    }

    #[tokio::test]
    async fn in_memory_transport_routes_to_handler() {
        let handler = Arc::new(StubHandler {
            reply_term: Term::new(4),
            grant: true,
        });
        let transport = InMemoryTransport::new().with_peer(node("peer"), handler);

        let resp = transport
            .request_vote(&node("peer"), sample_request_vote())
            .await
            .unwrap();
        assert_eq!(resp.term, Term::new(4));
        assert!(resp.vote_granted);
    }

    #[tokio::test]
    async fn in_memory_transport_reports_unknown_peer() {
        let transport = InMemoryTransport::new();
        let err = transport
            .request_vote(&node("missing"), sample_request_vote())
            .await
            .unwrap_err();
        assert!(matches!(err, RaftError::UnknownPeer(_)));
    }

    #[tokio::test]
    async fn in_memory_transport_append_and_snapshot_route() {
        let handler = Arc::new(StubHandler {
            reply_term: Term::new(2),
            grant: false,
        });
        let transport = InMemoryTransport::new().with_peer(node("peer"), handler);

        let ae = AppendEntries {
            term: Term::new(2),
            leader_id: node("leader"),
            prev_log_index: LogIndex::new(3),
            prev_log_term: Term::new(1),
            entries: vec![],
            leader_commit: LogIndex::new(3),
        };
        let ae_resp = transport.append_entries(&node("peer"), ae).await.unwrap();
        assert!(ae_resp.success);
        assert_eq!(ae_resp.conflict_index, LogIndex::new(3));

        let is = InstallSnapshot {
            term: Term::new(2),
            leader_id: node("leader"),
            last_included_index: LogIndex::new(5),
            last_included_term: Term::new(2),
            offset: 0,
            data: Bytes::new(),
            done: true,
        };
        let is_resp = transport.install_snapshot(&node("peer"), is).await.unwrap();
        assert_eq!(is_resp.term, Term::new(2));
    }
}
