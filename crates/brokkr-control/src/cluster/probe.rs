//! Asking peers for their observability state.
//!
//! The transport sits behind a trait so the poller's *policy* — deadlines,
//! what counts as unreachable, concurrency — is testable without a socket.
//! That discipline has repeatedly paid off in this codebase (`rotation_plan`,
//! `redirect::classify`, `resolve_raft_tls`, `should_reload`).

use std::time::Duration;

use thiserror::Error;

use super::aggregate::{NodeState, PeerOutcome};

/// A peer's identity and where to reach it, from the Raft cluster config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddr {
    /// The peer's Raft node id.
    pub node_id: String,
    /// The peer's advertised address.
    pub advertise_addr: String,
}

/// Why a peer could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProbeError {
    /// The peer refused, reset, or could not be resolved.
    #[error("peer unreachable: {0}")]
    Unreachable(String),
    /// The peer answered with something unusable.
    #[error("peer returned an unusable reply: {0}")]
    Malformed(String),
}

/// How the poller reaches a peer.
#[async_trait::async_trait]
pub trait PeerProbe: Send + Sync {
    /// Fetch one peer's node-local state.
    async fn get_local_state(&self, addr: &str) -> Result<NodeState, ProbeError>;
}

/// Probe every peer concurrently, converting any failure into
/// [`PeerOutcome::Unreachable`].
///
/// Never returns an error. A round in which every peer is down is a *degraded
/// cluster*, not a failed observation — and an observability path is most
/// needed exactly then.
///
/// `deadline` is applied per peer, and probes run concurrently, so a round
/// costs one deadline rather than one per peer.
pub async fn poll_peers(
    probe: &dyn PeerProbe,
    peers: &[PeerAddr],
    deadline: Duration,
) -> Vec<PeerOutcome> {
    let futures = peers.iter().map(|peer| async move {
        match tokio::time::timeout(deadline, probe.get_local_state(&peer.advertise_addr)).await {
            Ok(Ok(state)) => PeerOutcome::Answered(state),
            Ok(Err(e)) => {
                tracing::debug!(
                    node_id = %peer.node_id,
                    addr = %peer.advertise_addr,
                    error = %e,
                    "observability peer probe failed"
                );
                PeerOutcome::Unreachable {
                    node_id: peer.node_id.clone(),
                    advertise_addr: peer.advertise_addr.clone(),
                }
            }
            Err(_) => {
                tracing::debug!(
                    node_id = %peer.node_id,
                    addr = %peer.advertise_addr,
                    ?deadline,
                    "observability peer probe timed out"
                );
                PeerOutcome::Unreachable {
                    node_id: peer.node_id.clone(),
                    advertise_addr: peer.advertise_addr.clone(),
                }
            }
        }
    });
    futures::future::join_all(futures).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::views::{CasStatsView, NodeView, PolicyView, RaftRole};

    fn node_state(id: &str, role: RaftRole) -> NodeState {
        NodeState {
            node: NodeView {
                node_id: id.to_string(),
                advertise_addr: format!("{id}:7878"),
                role,
                term: 1,
                commit_index: 1,
                last_applied: 1,
                reachable: true,
                last_seen_secs: 0,
            },
            workers: Vec::new(),
            jobs: Vec::new(),
            policy: PolicyView {
                loaded: false,
                quarantined: false,
                decided: 0,
                declined: 0,
                failures_by_reason: BTreeMap::new(),
                owning_node: id.to_string(),
            },
            cas: CasStatsView {
                objects: 0,
                bytes: 0,
                owning_node: id.to_string(),
            },
        }
    }

    /// A probe whose behaviour is scripted per address.
    struct FakeProbe {
        healthy: Vec<String>,
        slow: Vec<String>,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl PeerProbe for FakeProbe {
        async fn get_local_state(&self, addr: &str) -> Result<NodeState, ProbeError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.slow.iter().any(|a| a == addr) {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            if self.healthy.iter().any(|a| a == addr) {
                let id = addr.split(':').next().unwrap_or(addr);
                return Ok(node_state(id, RaftRole::Follower));
            }
            Err(ProbeError::Unreachable("connection refused".to_string()))
        }
    }

    fn peers() -> Vec<PeerAddr> {
        vec![
            PeerAddr {
                node_id: "node-2".to_string(),
                advertise_addr: "node-2:7878".to_string(),
            },
            PeerAddr {
                node_id: "node-3".to_string(),
                advertise_addr: "node-3:7878".to_string(),
            },
        ]
    }

    #[tokio::test]
    async fn every_healthy_peer_answers() {
        let probe = FakeProbe {
            healthy: vec!["node-2:7878".to_string(), "node-3:7878".to_string()],
            slow: Vec::new(),
            calls: AtomicUsize::new(0),
        };
        let out = poll_peers(&probe, &peers(), Duration::from_millis(500)).await;
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|o| matches!(o, PeerOutcome::Answered(_))));
    }

    /// A refused peer becomes `Unreachable` carrying its identity, not an
    /// error that aborts the round.
    #[tokio::test]
    async fn a_refused_peer_becomes_unreachable_not_an_error() {
        let probe = FakeProbe {
            healthy: vec!["node-2:7878".to_string()],
            slow: Vec::new(),
            calls: AtomicUsize::new(0),
        };
        let out = poll_peers(&probe, &peers(), Duration::from_millis(500)).await;
        assert_eq!(out.len(), 2);
        let unreachable: Vec<&PeerOutcome> = out
            .iter()
            .filter(|o| matches!(o, PeerOutcome::Unreachable { .. }))
            .collect();
        assert_eq!(unreachable.len(), 1);
        match unreachable[0] {
            PeerOutcome::Unreachable {
                node_id,
                advertise_addr,
            } => {
                assert_eq!(node_id, "node-3");
                assert_eq!(advertise_addr, "node-3:7878");
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    /// One hung peer must not stall the round. This is the property that keeps
    /// a single wedged node from freezing every operator's console.
    #[tokio::test]
    async fn a_peer_slower_than_the_deadline_is_treated_as_unreachable() {
        let probe = FakeProbe {
            healthy: vec!["node-2:7878".to_string(), "node-3:7878".to_string()],
            slow: vec!["node-3:7878".to_string()],
            calls: AtomicUsize::new(0),
        };
        let started = std::time::Instant::now();
        let out = poll_peers(&probe, &peers(), Duration::from_millis(100)).await;
        let elapsed = started.elapsed();

        assert_eq!(out.len(), 2);
        assert!(
            out.iter().any(|o| matches!(
                o,
                PeerOutcome::Unreachable { node_id, .. } if node_id == "node-3"
            )),
            "the slow peer should have timed out"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the round took {elapsed:?}; a hung peer stalled it"
        );
    }

    /// Peers are probed concurrently, so a round costs one deadline rather
    /// than one per peer.
    #[tokio::test]
    async fn peers_are_probed_concurrently() {
        let probe = FakeProbe {
            healthy: Vec::new(),
            slow: vec!["node-2:7878".to_string(), "node-3:7878".to_string()],
            calls: AtomicUsize::new(0),
        };
        let started = std::time::Instant::now();
        let _ = poll_peers(&probe, &peers(), Duration::from_millis(200)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(600),
            "two 200ms deadlines took {elapsed:?}; probes ran serially"
        );
    }
}

/// [`PeerProbe`] over the real `PeerObservability` RPC on the Raft peer plane.
///
/// Constructs a channel per call rather than pooling: a poll happens every few
/// seconds, so connection setup is not the cost that matters, and a pooled
/// channel to a peer that has gone away is a stale-handle problem this does not
/// need to have.
#[derive(Debug, Default)]
pub struct GrpcPeerProbe;

#[async_trait::async_trait]
impl PeerProbe for GrpcPeerProbe {
    async fn get_local_state(&self, addr: &str) -> Result<NodeState, ProbeError> {
        use brokkr_proto::brokkr_v1::peer_observability_client::PeerObservabilityClient;

        let url = if addr.starts_with("http://") || addr.starts_with("https://") {
            addr.to_string()
        } else {
            format!("http://{addr}")
        };
        let mut client = PeerObservabilityClient::connect(url)
            .await
            .map_err(|e| ProbeError::Unreachable(e.to_string()))?;
        let reply = client
            .get_local_state(brokkr_proto::brokkr_v1::GetLocalStateRequest {})
            .await
            .map_err(|e| ProbeError::Unreachable(e.to_string()))?
            .into_inner();
        node_state_from_proto(reply)
    }
}

/// Convert a peer's reply into a [`NodeState`].
///
/// A reply missing any of its four parts is `Malformed` rather than
/// substituted with a default: a peer that answered without saying who it is
/// would otherwise be merged in as an empty node, which is worse than being
/// treated as unreachable.
fn node_state_from_proto(
    reply: brokkr_proto::brokkr_v1::GetLocalStateReply,
) -> Result<NodeState, ProbeError> {
    use crate::views::{
        CasStatsView, JobState, JobSummary, NodeView, PolicyView, RaftRole, WorkerView,
    };

    let missing = |what: &str| ProbeError::Malformed(format!("reply has no {what}"));
    let node = reply.node.ok_or_else(|| missing("node"))?;
    let policy = reply.policy.ok_or_else(|| missing("policy"))?;
    let cas = reply.cas.ok_or_else(|| missing("cas"))?;

    Ok(NodeState {
        node: NodeView {
            node_id: node.node_id,
            advertise_addr: node.advertise_addr,
            role: match node.role.as_str() {
                "leader" => RaftRole::Leader,
                "follower" => RaftRole::Follower,
                "standalone" => RaftRole::Standalone,
                // An unrecognised role from a newer peer reads as Unknown
                // rather than failing the probe: forward compatibility matters
                // more here than precision about a role we cannot act on.
                _ => RaftRole::Unknown,
            },
            term: node.term,
            commit_index: node.commit_index,
            last_applied: node.last_applied,
            reachable: true,
            last_seen_secs: node.last_seen_secs,
        },
        workers: reply
            .workers
            .into_iter()
            .map(|w| WorkerView {
                worker_id: w.worker_id,
                hostname: w.hostname,
                labels: w.labels.into_iter().collect(),
                inflight: w.inflight,
                last_seen_secs: w.last_seen_secs,
                stale: w.stale,
                owning_node: w.owning_node,
            })
            .collect(),
        policy: PolicyView {
            loaded: policy.loaded,
            quarantined: policy.quarantined,
            decided: policy.decided,
            declined: policy.declined,
            failures_by_reason: policy.failures_by_reason.into_iter().collect(),
            owning_node: policy.owning_node,
        },
        cas: CasStatsView {
            objects: cas.objects,
            bytes: cas.bytes,
            owning_node: cas.owning_node,
        },
        jobs: reply
            .jobs
            .into_iter()
            .map(|j| JobSummary {
                job_id: j.job_id,
                tenant: j.tenant,
                action_digest: j.action_digest,
                // An unrecognised state from a newer peer reads as Failed
                // rather than dropping the job: a job we cannot classify is
                // still a job that happened, and hiding it would be worse than
                // showing it with an imprecise label.
                state: JobState::from_str_opt(&j.state).unwrap_or(JobState::Failed),
                worker_id: Some(j.worker_id).filter(|w| !w.is_empty()),
                exit_code: j.has_exit_code.then_some(j.exit_code),
                completed_at_unix_ms: j.completed_at_unix_ms,
                owning_node: j.owning_node,
            })
            .collect(),
    })
}
