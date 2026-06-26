//! Multi-worker scheduling primitives (ADR 0008): the connected-worker
//! registry and the pluggable worker-selection [`Strategy`].
//!
//! This module is the data-model foundation for §16 task 3. The scheduler
//! wiring that routes jobs through it is a separate increment; keeping the
//! policy (`Strategy`) and the connection state (`ConnectedWorkers`) here —
//! both proto-light and free of dispatch plumbing — makes them unit-testable
//! in isolation.

use std::collections::HashMap;

use brokkr_common::WorkerId;
use brokkr_proto::brokkr_v1 as bv1;
use tokio::sync::mpsc;

/// Read-only view of per-worker load, handed to a [`Strategy`] so policies can
/// prefer (or avoid) busy workers without depending on `ConnectedWorkers`.
pub trait LoadView {
    /// Number of jobs dispatched to `worker` but not yet reported back
    /// (its in-flight count). Unknown workers read as `0`.
    fn inflight(&self, worker: &WorkerId) -> usize;
}

/// Worker-selection policy: pick one worker from an eligible candidate set.
///
/// Candidates are pre-filtered by the caller to workers that are *both*
/// connected and satisfy the action's platform constraints
/// ([`crate::matching::eligible_workers`]). The strategy only decides *which*
/// of those gets the job.
pub trait Strategy: Send + Sync {
    /// Choose a worker from `candidates` given current `loads`. Returns `None`
    /// iff `candidates` is empty.
    fn choose(&self, candidates: &[WorkerId], loads: &dyn LoadView) -> Option<WorkerId>;
}

/// Simplest strategy: the least-loaded candidate, ties broken by worker id
/// for determinism. Stateless, so it needs no `&mut self` / interior
/// mutability. (`BinPacking` / `LocalityAware` are later increments behind the
/// same trait — see ADR 0008.)
#[derive(Debug, Default, Clone, Copy)]
pub struct SimpleFifo;

impl Strategy for SimpleFifo {
    fn choose(&self, candidates: &[WorkerId], loads: &dyn LoadView) -> Option<WorkerId> {
        candidates
            .iter()
            .min_by(|a, b| {
                loads
                    .inflight(a)
                    .cmp(&loads.inflight(b))
                    .then_with(|| a.as_str().cmp(b.as_str()))
            })
            .cloned()
    }
}

/// A connected worker's job-dispatch channel plus its live in-flight count.
struct WorkerConn {
    job_tx: mpsc::Sender<bv1::Job>,
    inflight: usize,
}

/// Registry of workers that currently hold a live `WorkerService.Stream`, each
/// with its own job channel.
///
/// Distinct from [`crate::registry::WorkerRegistry`]: that tracks liveness and
/// capabilities (a worker is *registered* once it has handshaked and keeps
/// heartbeating), whereas this tracks the *connection* (a worker is
/// *connected* only while its bidi stream is open). The scheduler consults
/// both — eligibility comes from the registry + matcher, routability from
/// here.
#[derive(Default)]
pub struct ConnectedWorkers {
    workers: HashMap<WorkerId, WorkerConn>,
}

impl ConnectedWorkers {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a worker's job channel when its stream opens. Replaces any
    /// previous connection for the same id (a reconnect supersedes the old
    /// stream) and resets its in-flight count.
    pub fn connect(&mut self, id: WorkerId, job_tx: mpsc::Sender<bv1::Job>) {
        self.workers.insert(
            id,
            WorkerConn {
                job_tx,
                inflight: 0,
            },
        );
    }

    /// Remove a worker when its stream closes.
    pub fn disconnect(&mut self, id: &WorkerId) {
        self.workers.remove(id);
    }

    /// Whether `id` currently has an open stream.
    pub fn is_connected(&self, id: &WorkerId) -> bool {
        self.workers.contains_key(id)
    }

    /// The currently-connected worker ids.
    pub fn connected_ids(&self) -> impl Iterator<Item = &WorkerId> {
        self.workers.keys()
    }

    /// Number of connected workers.
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// Whether no worker is connected.
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// Clone the job sender for `id`, if connected. The clone lets the caller
    /// send a job without holding the registry lock across the `.await`.
    pub fn sender(&self, id: &WorkerId) -> Option<mpsc::Sender<bv1::Job>> {
        self.workers.get(id).map(|c| c.job_tx.clone())
    }

    /// Increment `id`'s in-flight count (on dispatch). No-op if not connected.
    pub fn inc_inflight(&mut self, id: &WorkerId) {
        if let Some(conn) = self.workers.get_mut(id) {
            conn.inflight += 1;
        }
    }

    /// Decrement `id`'s in-flight count (on result / timeout), saturating at
    /// zero. No-op if not connected.
    pub fn dec_inflight(&mut self, id: &WorkerId) {
        if let Some(conn) = self.workers.get_mut(id) {
            conn.inflight = conn.inflight.saturating_sub(1);
        }
    }
}

impl LoadView for ConnectedWorkers {
    fn inflight(&self, worker: &WorkerId) -> usize {
        self.workers.get(worker).map(|c| c.inflight).unwrap_or(0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    fn wid(s: &str) -> WorkerId {
        WorkerId::new(s.to_string()).unwrap()
    }

    /// In-memory `LoadView` for testing strategies without a `ConnectedWorkers`.
    struct MapLoads(HashMap<WorkerId, usize>);
    impl LoadView for MapLoads {
        fn inflight(&self, worker: &WorkerId) -> usize {
            self.0.get(worker).copied().unwrap_or(0)
        }
    }

    fn loads(pairs: &[(&str, usize)]) -> MapLoads {
        MapLoads(pairs.iter().map(|(k, v)| (wid(k), *v)).collect())
    }

    #[test]
    fn simple_fifo_empty_candidates_is_none() {
        assert!(SimpleFifo.choose(&[], &loads(&[])).is_none());
    }

    #[test]
    fn simple_fifo_picks_least_loaded() {
        let cands = vec![wid("a"), wid("b"), wid("c")];
        let l = loads(&[("a", 3), ("b", 1), ("c", 5)]);
        assert_eq!(SimpleFifo.choose(&cands, &l), Some(wid("b")));
    }

    #[test]
    fn simple_fifo_breaks_ties_by_id() {
        let cands = vec![wid("zebra"), wid("alpha"), wid("mid")];
        // All equally loaded → lexicographically smallest id wins.
        let l = loads(&[("zebra", 2), ("alpha", 2), ("mid", 2)]);
        assert_eq!(SimpleFifo.choose(&cands, &l), Some(wid("alpha")));
    }

    #[test]
    fn simple_fifo_unknown_load_reads_as_zero() {
        let cands = vec![wid("busy"), wid("fresh")];
        // "fresh" has no entry → inflight 0 → chosen over busy=4.
        let l = loads(&[("busy", 4)]);
        assert_eq!(SimpleFifo.choose(&cands, &l), Some(wid("fresh")));
    }

    fn dummy_channel() -> mpsc::Sender<bv1::Job> {
        let (tx, _rx) = mpsc::channel(1);
        tx
    }

    #[test]
    fn connect_disconnect_tracks_membership() {
        let mut cw = ConnectedWorkers::new();
        assert!(cw.is_empty());
        cw.connect(wid("w1"), dummy_channel());
        cw.connect(wid("w2"), dummy_channel());
        assert_eq!(cw.len(), 2);
        assert!(cw.is_connected(&wid("w1")));
        assert!(cw.sender(&wid("w1")).is_some());

        cw.disconnect(&wid("w1"));
        assert!(!cw.is_connected(&wid("w1")));
        assert!(cw.sender(&wid("w1")).is_none());
        assert_eq!(cw.len(), 1);
    }

    #[test]
    fn inflight_tracking_increments_decrements_and_saturates() {
        let mut cw = ConnectedWorkers::new();
        cw.connect(wid("w1"), dummy_channel());
        assert_eq!(cw.inflight(&wid("w1")), 0);

        cw.inc_inflight(&wid("w1"));
        cw.inc_inflight(&wid("w1"));
        assert_eq!(cw.inflight(&wid("w1")), 2);

        cw.dec_inflight(&wid("w1"));
        assert_eq!(cw.inflight(&wid("w1")), 1);

        // Saturates at zero rather than underflowing.
        cw.dec_inflight(&wid("w1"));
        cw.dec_inflight(&wid("w1"));
        assert_eq!(cw.inflight(&wid("w1")), 0);

        // Unknown worker reads as zero; mutators are no-ops.
        assert_eq!(cw.inflight(&wid("ghost")), 0);
        cw.inc_inflight(&wid("ghost"));
        assert_eq!(cw.inflight(&wid("ghost")), 0);
    }

    #[test]
    fn reconnect_resets_inflight() {
        let mut cw = ConnectedWorkers::new();
        cw.connect(wid("w1"), dummy_channel());
        cw.inc_inflight(&wid("w1"));
        assert_eq!(cw.inflight(&wid("w1")), 1);
        // Reconnect (new stream) replaces the entry and resets the count.
        cw.connect(wid("w1"), dummy_channel());
        assert_eq!(cw.inflight(&wid("w1")), 0);
        assert_eq!(cw.len(), 1);
    }

    #[test]
    fn connected_workers_is_a_loadview_for_the_strategy() {
        let mut cw = ConnectedWorkers::new();
        cw.connect(wid("a"), dummy_channel());
        cw.connect(wid("b"), dummy_channel());
        cw.inc_inflight(&wid("a"));
        cw.inc_inflight(&wid("a"));
        cw.inc_inflight(&wid("b"));

        let cands = vec![wid("a"), wid("b")];
        // b (1 in-flight) beats a (2 in-flight).
        assert_eq!(SimpleFifo.choose(&cands, &cw), Some(wid("b")));
    }
}
