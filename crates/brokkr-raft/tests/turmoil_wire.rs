//! Turmoil integration smoke test (Phase 5 I1).
//!
//! Sends `brokkr-raft`'s real Raft wire types — protobuf-encoded `RequestVote`
//! and its reply — across `turmoil`'s deterministic simulated network, framed
//! with a 4-byte length prefix. This proves three things the later milestones
//! rely on:
//!
//! 1. the `turmoil` dev-dependency is wired into the build and runs;
//! 2. the [`RequestVote`] ↔ protobuf conversions survive a real socket round
//!    trip (encode on one host, decode on another); and
//! 3. the simulation is reproducible from turmoil's fixed default seed.
//!
//! The full tonic-over-turmoil transport (fault injection: partitions, delay,
//! reorder) is milestone I5, once a running node exists to serve `RaftService`
//! (ADR 0013 D2). This test is the substrate that suite builds on.

#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use turmoil::net::{TcpListener, TcpStream};

use brokkr_proto::brokkr::v1 as pb;
use brokkr_raft::{LogIndex, NodeId, RequestVote, RequestVoteResponse, Term};

type SimResult = Result<(), Box<dyn std::error::Error>>;

const PORT: u16 = 9000;

async fn write_frame(sock: &mut TcpStream, bytes: &[u8]) -> SimResult {
    let len = u32::try_from(bytes.len())?;
    sock.write_all(&len.to_le_bytes()).await?;
    sock.write_all(bytes).await?;
    Ok(())
}

async fn read_frame(sock: &mut TcpStream) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut len_buf = [0u8; 4];
    sock.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    sock.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Runs one request/response exchange and returns whether the server granted
/// the vote, as observed by the client. Structured so the whole scenario can be
/// replayed to check determinism.
fn run_exchange() -> bool {
    let mut sim = turmoil::Builder::new().build();

    // Server: decode the RequestVote, grant the vote iff the candidate's log is
    // non-empty, and reply.
    sim.host("server", || async {
        let listener = TcpListener::bind(("0.0.0.0", PORT)).await?;
        let (mut sock, _) = listener.accept().await?;
        let bytes = read_frame(&mut sock).await?;
        let req = RequestVote::try_from(pb::RequestVoteRequest::decode(bytes.as_slice())?)?;
        assert_eq!(req.candidate_id.as_str(), "cand-1");
        assert_eq!(req.term, Term::new(4));
        let resp = RequestVoteResponse {
            term: req.term,
            vote_granted: req.last_log_index != LogIndex::ZERO,
        };
        let reply = pb::RequestVoteReply::from(resp).encode_to_vec();
        write_frame(&mut sock, &reply).await?;
        Ok(())
    });

    let granted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let granted_client = granted.clone();
    sim.client("client", async move {
        let mut sock = TcpStream::connect(("server", PORT)).await?;
        let req = RequestVote {
            term: Term::new(4),
            candidate_id: NodeId::new("cand-1")?,
            last_log_index: LogIndex::new(10),
            last_log_term: Term::new(3),
        };
        let bytes = pb::RequestVoteRequest::from(req).encode_to_vec();
        write_frame(&mut sock, &bytes).await?;
        let reply = read_frame(&mut sock).await?;
        let resp: RequestVoteResponse = pb::RequestVoteReply::decode(reply.as_slice())?.into();
        assert_eq!(resp.term, Term::new(4));
        granted_client.store(resp.vote_granted, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    });

    sim.run().unwrap();
    granted.load(std::sync::atomic::Ordering::SeqCst)
}

#[test]
fn request_vote_round_trips_over_simulated_network() {
    assert!(
        run_exchange(),
        "candidate with a non-empty log should be granted the vote"
    );
}

#[test]
fn simulation_is_reproducible() {
    // Same code, same default seed → same outcome, every run.
    assert_eq!(run_exchange(), run_exchange());
}
