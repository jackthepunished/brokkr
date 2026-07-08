//! Typed error enum for `brokkr-raft`.

use thiserror::Error;

/// Errors surfaced by the Raft engine.
///
/// Library code never panics or unwraps (CLAUDE.md hard rule 1); every fallible
/// operation returns `Result<_, RaftError>`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RaftError {
    /// A node identifier failed validation (empty or too long).
    #[error("invalid node id: {0}")]
    InvalidNodeId(String),

    /// A durable-storage operation failed (open, read, write, or commit).
    #[error("storage error: {0}")]
    Storage(String),

    /// A log entry could not be encoded to or decoded from its protobuf form.
    #[error("codec error: {0}")]
    Codec(String),

    /// A peer-to-peer RPC failed at the transport layer.
    #[error("transport error: {0}")]
    Transport(String),

    /// An RPC was addressed to a peer this node does not know about.
    #[error("unknown peer: {0}")]
    UnknownPeer(String),

    /// A client `propose` reached a node that is not the leader. The caller
    /// should redirect to `leader` (if known).
    #[error("not the leader (current leader: {leader:?})")]
    NotLeader {
        /// The leader this node currently recognizes, if any.
        leader: Option<String>,
    },

    /// A snapshot operation failed or was invalid (nothing to compact, a
    /// missing snapshot, or an unsupported chunked `InstallSnapshot`).
    #[error("snapshot error: {0}")]
    Snapshot(String),

    /// A membership change was rejected (one already in flight, or an empty
    /// or unchanged voter set).
    #[error("configuration change rejected: {0}")]
    ConfChange(String),
}

impl From<prost::DecodeError> for RaftError {
    fn from(e: prost::DecodeError) -> Self {
        RaftError::Codec(e.to_string())
    }
}

impl From<prost::EncodeError> for RaftError {
    fn from(e: prost::EncodeError) -> Self {
        RaftError::Codec(e.to_string())
    }
}

impl From<tonic::Status> for RaftError {
    fn from(s: tonic::Status) -> Self {
        RaftError::Transport(s.to_string())
    }
}
