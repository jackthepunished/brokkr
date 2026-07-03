//! From-scratch Raft consensus for the Brokkr control plane.
//!
//! `brokkr-raft` implements the Raft algorithm (Ongaro & Ousterhout, 2014) by
//! hand — no external Raft crate (CLAUDE.md rule 10). The design is fixed by
//! [ADR 0013](../../../docs/architecture/0013-custom-raft.md) and the spec in
//! `docs/raft-notes.md`.
//!
//! This crate is being built incrementally across Phase 5 milestones I1–I9.
//! The current scaffold (I1) provides the foundational pieces the consensus
//! state machine is built on:
//!
//! - [`types`] — the [`Term`], [`LogIndex`], and [`NodeId`] newtypes plus
//!   [`LogEntry`], with lossless conversions to/from the wire protobuf.
//! - [`error`] — the [`RaftError`] typed error enum.
//! - [`rng`] — a small deterministic, seeded PRNG ([`Rng`]) for election-timeout
//!   jitter (ADR 0013 D3: no external dependency, reproducible under simulation).
//! - [`state`] — the [`HardState`] (`currentTerm` + `votedFor`) that must be
//!   persisted atomically before responding to an RPC (`docs/raft-notes.md` §3).
//! - [`storage`] — the redb-backed [`RaftLog`], persisting the replicated log
//!   and hard state (ADR 0013 D1) with atomic, persist-before-respond writes
//!   (crash-consistency proven by the I2 tests).
//! - [`transport`] — the async [`Transport`] trait and its request/reply types,
//!   with a production [`TonicTransport`] and an in-process
//!   [`InMemoryTransport`] for deterministic tests (ADR 0013 D2).
//!
//! The consensus state machine (leader election, log replication, safety) is
//! added in milestones I3–I4; it is intentionally absent here.

#![deny(missing_docs)]

pub mod error;
pub mod rng;
pub mod state;
pub mod storage;
pub mod transport;
pub mod types;

pub use error::RaftError;
pub use rng::Rng;
pub use state::HardState;
pub use storage::RaftLog;
pub use transport::{
    AppendEntries, AppendEntriesResponse, InMemoryTransport, InstallSnapshot,
    InstallSnapshotResponse, RequestVote, RequestVoteResponse, TonicTransport, Transport,
};
pub use types::{LogEntry, LogIndex, NodeId, Term};
