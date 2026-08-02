//! Cluster-wide observability aggregation (ADR 0012).
//!
//! One task per node polls every Raft peer into a [`ClusterSnapshot`]; every
//! operator-facing handler reads that snapshot. Peer traffic is therefore
//! constant and independent of how many operators are watching — an operator
//! console is exactly the thing left open on a wall display, and per-request
//! fan-out would make an idle dashboard expensive.
//!
//! The cost is bounded, known staleness: nothing here is fresher than one poll
//! interval. [`ClusterSnapshot::as_of`] carries that so a consumer can show it.

mod aggregate;
mod probe;

pub use aggregate::{merge, ClusterSnapshot, NodeState, PeerOutcome, SharedSnapshot};

pub use probe::{poll_peers, GrpcPeerProbe, PeerAddr, PeerProbe, ProbeError};

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tracing::Instrument as _;

/// How the poller is configured.
#[derive(Debug, Clone)]
pub struct PollerConfig {
    /// How often the snapshot is refreshed. Governs the local read *and* peer
    /// fan-out. Never zero — rejected at startup.
    pub interval: Duration,
    /// Per-peer deadline. Must be below `interval`.
    pub peer_timeout: Duration,
    /// How often to re-measure this node's own CAS.
    ///
    /// Slower than `interval` on purpose: `RedbCas` answers by scanning the
    /// blob table under a throughput permit, so measuring it every poll would
    /// be O(n) each time and could take a permit real traffic needs.
    pub cas_interval: Duration,
}

/// Produces this node's own observability state.
#[async_trait::async_trait]
pub trait LocalStateSource: Send + Sync {
    /// Build this node's state.
    ///
    /// `refresh_cas` is false on rounds where the CAS measurement is being
    /// skipped, in which case the implementation reuses its previous value.
    async fn local_state(&self, refresh_cas: bool) -> NodeState;
}

/// Supplies the current peer set from the Raft cluster configuration.
#[async_trait::async_trait]
pub trait PeerDirectory: Send + Sync {
    /// Peers other than this node. Empty when Raft is disabled.
    async fn peers(&self) -> Vec<PeerAddr>;
}

/// Everything the poller needs.
///
/// Not `Debug`: it holds trait objects over the local state source, the peer
/// directory and the transport, none of which requires `Debug`, and adding
/// that bound to all three for a log line would be the tail wagging the dog.
pub struct PollerDeps {
    /// Builds this node's own state.
    pub local: Arc<dyn LocalStateSource>,
    /// Current peer set, re-read each round so membership changes are picked
    /// up without a restart.
    pub peers: Arc<dyn PeerDirectory>,
    /// Transport to peers.
    pub probe: Arc<dyn PeerProbe>,
}

/// Run the poll loop until the task is dropped.
///
/// The loop **always** runs. With `--raft` off the peer set is empty, so a
/// round is one local read and no network — but it still happens. Publishing
/// once and returning would leave a single-node operator staring at
/// permanently stale workers, jobs, CAS and policy, which is the exact
/// opposite of the point. "No peer traffic" and "no refresh" are different
/// things, and only the first is ever intended.
pub fn spawn_poller(
    shared: SharedSnapshot,
    deps: PollerDeps,
    cfg: PollerConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(
        async move {
            // A zero interval is rejected at startup, so `tokio::time::interval`
            // — which panics on a zero period — cannot be reached with one.
            // Clamp anyway rather than panic in a background task: a panic here
            // would take out observability silently.
            let period = if cfg.interval.is_zero() {
                tracing::error!(
                    "observability poll interval was zero at spawn; falling back to 2s. \
                     This should have been rejected at startup."
                );
                Duration::from_secs(2)
            } else {
                cfg.interval
            };

            let mut ticker = tokio::time::interval(period);
            // The first tick completes immediately, which is what we want: an
            // operator connecting during startup should not wait a full
            // interval for the first snapshot.
            let mut since_cas = cfg.cas_interval;
            loop {
                ticker.tick().await;
                let refresh_cas = since_cas >= cfg.cas_interval;
                since_cas = if refresh_cas {
                    Duration::ZERO
                } else {
                    since_cas.saturating_add(period)
                };

                let local = deps.local.local_state(refresh_cas).await;
                let peers = deps.peers.peers().await;
                let outcomes = poll_peers(deps.probe.as_ref(), &peers, cfg.peer_timeout).await;
                let snapshot = merge(local, outcomes, SystemTime::now());

                if snapshot.degraded {
                    tracing::warn!(
                        nodes = snapshot.nodes.len(),
                        reachable = snapshot.nodes.iter().filter(|n| n.reachable).count(),
                        leader = ?snapshot.leader_id,
                        "cluster observability is degraded"
                    );
                }
                *shared.write().await = snapshot;
            }
        }
        .in_current_span(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::RwLock;

    use super::*;
    use crate::views::{CasStatsView, NodeView, PolicyView, RaftRole};

    struct Local {
        calls: AtomicUsize,
        cas_refreshes: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LocalStateSource for Local {
        async fn local_state(&self, refresh_cas: bool) -> NodeState {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if refresh_cas {
                self.cas_refreshes.fetch_add(1, Ordering::Relaxed);
            }
            NodeState {
                node: NodeView {
                    node_id: "solo".to_string(),
                    advertise_addr: "solo:7878".to_string(),
                    role: RaftRole::Leader,
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
                    owning_node: "solo".to_string(),
                },
                cas: CasStatsView {
                    objects: 0,
                    bytes: 0,
                    owning_node: "solo".to_string(),
                },
            }
        }
    }

    struct NoPeers;
    #[async_trait::async_trait]
    impl PeerDirectory for NoPeers {
        async fn peers(&self) -> Vec<PeerAddr> {
            Vec::new()
        }
    }

    struct NeverCalled;
    #[async_trait::async_trait]
    impl PeerProbe for NeverCalled {
        async fn get_local_state(&self, addr: &str) -> Result<NodeState, ProbeError> {
            panic!("probe must not be called with an empty peer set (addr={addr})");
        }
    }

    /// With no peers the loop still refreshes. Publishing once and returning
    /// would leave a single-node operator staring at permanently stale state —
    /// "no peer traffic" and "no refresh" are different things.
    #[tokio::test]
    async fn the_loop_keeps_refreshing_with_no_peers() {
        let shared: SharedSnapshot = Arc::new(RwLock::new(ClusterSnapshot::default()));
        let local = Arc::new(Local {
            calls: AtomicUsize::new(0),
            cas_refreshes: AtomicUsize::new(0),
        });
        let handle = spawn_poller(
            shared.clone(),
            PollerDeps {
                local: local.clone(),
                peers: Arc::new(NoPeers),
                probe: Arc::new(NeverCalled),
            },
            PollerConfig {
                interval: Duration::from_millis(20),
                peer_timeout: Duration::from_millis(10),
                cas_interval: Duration::from_millis(20),
            },
        );

        tokio::time::sleep(Duration::from_millis(150)).await;
        handle.abort();

        let calls = local.calls.load(Ordering::Relaxed);
        assert!(
            calls >= 3,
            "expected repeated refreshes, got {calls} — the loop published once and stopped"
        );
        let snap = shared.read().await;
        assert_eq!(snap.nodes.len(), 1);
        assert!(!snap.degraded, "a single node with no peers is healthy");
        assert!(snap.as_of.is_some(), "the snapshot must carry its age");
    }

    /// CAS is measured on its own slower cadence, because `RedbCas` answers by
    /// scanning under a throughput permit a poller could otherwise steal.
    #[tokio::test]
    async fn cas_is_refreshed_less_often_than_the_poll() {
        let shared: SharedSnapshot = Arc::new(RwLock::new(ClusterSnapshot::default()));
        let local = Arc::new(Local {
            calls: AtomicUsize::new(0),
            cas_refreshes: AtomicUsize::new(0),
        });
        let handle = spawn_poller(
            shared,
            PollerDeps {
                local: local.clone(),
                peers: Arc::new(NoPeers),
                probe: Arc::new(NeverCalled),
            },
            PollerConfig {
                interval: Duration::from_millis(20),
                // Ten times the poll interval.
                cas_interval: Duration::from_millis(200),
                peer_timeout: Duration::from_millis(10),
            },
        );

        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.abort();

        let polls = local.calls.load(Ordering::Relaxed);
        let cas = local.cas_refreshes.load(Ordering::Relaxed);
        assert!(polls >= 5, "expected several polls, got {polls}");
        assert!(
            cas < polls,
            "CAS was measured {cas} times across {polls} polls; it must be rarer"
        );
    }
}
