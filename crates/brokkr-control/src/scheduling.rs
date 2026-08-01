//! Multi-worker scheduling primitives (ADR 0008): the connected-worker
//! registry and the pluggable worker-selection [`Strategy`].
//!
//! This module is the data-model foundation for §16 task 3. The scheduler
//! wiring that routes jobs through it is a separate increment; keeping the
//! policy (`Strategy`) and the connection state (`ConnectedWorkers`) here —
//! both proto-light and free of dispatch plumbing — makes them unit-testable
//! in isolation.

use std::collections::HashMap;
use std::sync::Arc;

use brokkr_common::{Digest, TenantId, WorkerId};
use brokkr_proto::brokkr_v1 as bv1;
use brokkr_proto::reapi_v2 as rapi;
use tokio::sync::{mpsc, Mutex};

/// Read-only view of per-worker load, handed to a [`Strategy`] so policies can
/// prefer (or avoid) busy workers without depending on `ConnectedWorkers`.
pub trait LoadView {
    /// Number of jobs dispatched to `worker` but not yet reported back
    /// (its in-flight count). Unknown workers read as `0`.
    fn inflight(&self, worker: &WorkerId) -> usize;
}

/// Read-only view of per-worker *locality* history: how much of a candidate's
/// recent work overlaps with the job about to be placed.
///
/// This is the signal ADR 0008's never-built `LocalityAware` needed. A worker
/// that recently ran an action with the same input root very likely still has
/// those inputs materialized locally, so sending it the next such action skips
/// a fetch. Both counters are drawn from a bounded per-worker window, so they
/// answer "recently, how often?" and not "ever".
pub trait LocalityView {
    /// How many of `worker`'s recent completions used this input root.
    fn input_root_hits(&self, worker: &WorkerId, input_root: &Digest) -> u32;
    /// How many of `worker`'s recent completions were this exact action.
    fn action_hits(&self, worker: &WorkerId, action: &Digest) -> u32;
}

/// A [`LocalityView`] that knows nothing. Every lookup reads as `0`.
///
/// Used where no history is tracked yet, and by tests that only exercise
/// load-based policies. It is deliberately *not* an error case: a policy asking
/// about locality on a cluster that isn't recording it should see "no recent
/// overlap", which is the truth, rather than fail.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoLocality;

impl LocalityView for NoLocality {
    fn input_root_hits(&self, _worker: &WorkerId, _input_root: &Digest) -> u32 {
        0
    }
    fn action_hits(&self, _worker: &WorkerId, _action: &Digest) -> u32 {
        0
    }
}

/// What a [`Strategy`] may know about the job it is placing.
///
/// Borrowed rather than owned: this is built once per placement, on the
/// dispatch path, under the scheduler's lock.
#[derive(Debug, Clone, Copy)]
pub struct JobFacts<'a> {
    /// The submitting tenant (ADR 0010).
    pub tenant: &'a TenantId,
    /// Digest of the REAPI `Action` being placed.
    pub action_digest: &'a Digest,
    /// Digest of the action's input root, when it has one. `None` for an
    /// action with no inputs.
    pub input_root_digest: Option<&'a Digest>,
    /// The action's platform constraints. Candidates already satisfy these
    /// ([`crate::matching::eligible_workers`]); a policy may still read them to
    /// distinguish *how well* a candidate matches.
    pub platform: &'a rapi::Platform,
}

/// Everything a [`Strategy::choose_with`] implementation is given.
///
/// Grouped into one struct so that adding a signal later — GPU class, rack
/// topology, a cost model — is an additive change to this type rather than a
/// new trait method and a new default implementation.
pub struct DecisionContext<'a> {
    /// Per-worker in-flight counts.
    pub loads: &'a dyn LoadView,
    /// Per-worker recent-completion overlap with this job.
    pub locality: &'a dyn LocalityView,
    /// The job being placed.
    pub job: JobFacts<'a>,
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

    /// Choose a worker with the full [`DecisionContext`].
    ///
    /// This is what the scheduler actually calls. The default implementation
    /// forwards to [`Strategy::choose`], ignoring everything the load-only
    /// built-ins don't consult — so `SimpleFifo` and `BinPacking` need no
    /// change, and stay honest about what they actually read. Policies that
    /// want locality or job facts (Phase 6's WASM hook, ADR 0014) override
    /// this instead.
    ///
    /// **Contract, binding on every implementation:** returns `None` *iff*
    /// `candidates` is empty. A policy that cannot decide must fall back to a
    /// built-in answer, never to `None` — returning `None` for a non-empty
    /// candidate set stalls the placement of a job that had somewhere to go.
    fn choose_with(&self, candidates: &[WorkerId], ctx: &DecisionContext<'_>) -> Option<WorkerId> {
        self.choose(candidates, ctx.loads)
    }
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

/// Bin-packing: fill a worker toward `cap` in-flight jobs before spreading to a
/// fresh one, so idle workers can scale down. Among candidates still under
/// `cap`, picks the *most*-loaded (packs tight); if every candidate is at or
/// over `cap`, falls back to the least-loaded so work still flows rather than
/// stalling. Ties broken by worker id for determinism.
#[derive(Debug, Clone, Copy)]
pub struct BinPacking {
    /// Soft per-worker in-flight target. A worker under this is preferred for
    /// packing; the cap is not a hard admission limit (the fallback still
    /// places work when everyone is saturated).
    cap: usize,
}

impl BinPacking {
    /// Create a bin-packing strategy with the given soft per-worker in-flight
    /// `cap`. A `cap` of 0 is clamped to 1 (a 0 cap would send every candidate
    /// straight to the least-loaded fallback, i.e. degenerate to spreading).
    pub fn new(cap: usize) -> Self {
        Self { cap: cap.max(1) }
    }
}

impl Strategy for BinPacking {
    fn choose(&self, candidates: &[WorkerId], loads: &dyn LoadView) -> Option<WorkerId> {
        // Prefer the most-loaded worker still under cap (pack it tighter);
        // `min_by` with a comparator that ranks higher load — then lower id —
        // as "smaller" yields that worker deterministically.
        let packed = candidates
            .iter()
            .filter(|w| loads.inflight(w) < self.cap)
            .min_by(|a, b| {
                loads
                    .inflight(b)
                    .cmp(&loads.inflight(a))
                    .then_with(|| a.as_str().cmp(b.as_str()))
            });
        if let Some(w) = packed {
            return Some(w.clone());
        }
        // Everyone is at/over cap — fall back to least-loaded so work still
        // flows (same rule as `SimpleFifo`).
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

/// Shared, mutex-guarded [`ConnectedWorkers`] handle. The scheduler reads it to
/// route jobs and track load; `WorkerService.Stream` writes connect/disconnect.
pub type SharedConnectedWorkers = Arc<Mutex<ConnectedWorkers>>;

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

    #[test]
    fn bin_packing_empty_candidates_is_none() {
        assert!(BinPacking::new(4).choose(&[], &loads(&[])).is_none());
    }

    #[test]
    fn bin_packing_prefers_most_loaded_under_cap() {
        let cands = vec![wid("a"), wid("b"), wid("c")];
        // cap 4: a=3 (highest under cap) wins over b=1, c=0 — pack it tight.
        let l = loads(&[("a", 3), ("b", 1), ("c", 0)]);
        assert_eq!(BinPacking::new(4).choose(&cands, &l), Some(wid("a")));
    }

    #[test]
    fn bin_packing_skips_workers_at_cap() {
        let cands = vec![wid("a"), wid("b")];
        // cap 2: a is at cap (2) so excluded; b (1, under cap) is chosen even
        // though a is more loaded.
        let l = loads(&[("a", 2), ("b", 1)]);
        assert_eq!(BinPacking::new(2).choose(&cands, &l), Some(wid("b")));
    }

    #[test]
    fn bin_packing_falls_back_to_least_loaded_when_all_at_cap() {
        let cands = vec![wid("a"), wid("b"), wid("c")];
        // cap 2: all at/over cap → least-loaded fallback picks c=2 (a=4, b=3).
        let l = loads(&[("a", 4), ("b", 3), ("c", 2)]);
        assert_eq!(BinPacking::new(2).choose(&cands, &l), Some(wid("c")));
    }

    #[test]
    fn bin_packing_ties_break_by_id() {
        let cands = vec![wid("zebra"), wid("alpha")];
        // Equal load under cap → lower id wins.
        let l = loads(&[("zebra", 1), ("alpha", 1)]);
        assert_eq!(BinPacking::new(4).choose(&cands, &l), Some(wid("alpha")));
    }

    #[test]
    fn bin_packing_zero_cap_is_clamped_and_spreads() {
        // cap clamped to 1: with two idle workers nobody is under cap=1? 0<1 is
        // true, so both are under cap → most-loaded (both 0) → lower id.
        let cands = vec![wid("a"), wid("b")];
        assert_eq!(
            BinPacking::new(0).choose(&cands, &loads(&[])),
            Some(wid("a"))
        );
    }

    fn digest(hash: &str) -> Digest {
        Digest::new(hash.repeat(64 / hash.len()), 1).unwrap()
    }

    /// Build a `DecisionContext` over the given load and locality views.
    fn ctx<'a>(
        loads: &'a dyn LoadView,
        locality: &'a dyn LocalityView,
        tenant: &'a TenantId,
        action: &'a Digest,
        input_root: &'a Digest,
        platform: &'a rapi::Platform,
    ) -> DecisionContext<'a> {
        DecisionContext {
            loads,
            locality,
            job: JobFacts {
                tenant,
                action_digest: action,
                input_root_digest: Some(input_root),
                platform,
            },
        }
    }

    #[test]
    fn no_locality_reports_no_overlap_rather_than_failing() {
        let w = wid("a");
        assert_eq!(NoLocality.input_root_hits(&w, &digest("ab")), 0);
        assert_eq!(NoLocality.action_hits(&w, &digest("cd")), 0);
    }

    /// `choose_with`'s default implementation forwards to `choose`, so the
    /// load-only built-ins behave identically through either entry point.
    /// This is what lets `Strategy` gain a richer signal without touching them.
    #[test]
    fn choose_with_defaults_to_choose_for_the_builtins() {
        let cands = vec![wid("a"), wid("b"), wid("c")];
        let l = loads(&[("a", 3), ("b", 1), ("c", 5)]);
        let (t, ad, ir, p) = (
            TenantId::default(),
            digest("ab"),
            digest("cd"),
            rapi::Platform::default(),
        );
        let c = ctx(&l, &NoLocality, &t, &ad, &ir, &p);

        assert_eq!(
            SimpleFifo.choose_with(&cands, &c),
            SimpleFifo.choose(&cands, &l)
        );
        assert_eq!(
            BinPacking::new(4).choose_with(&cands, &c),
            BinPacking::new(4).choose(&cands, &l)
        );
    }

    /// A policy that overrides `choose_with` sees the job facts and locality
    /// the built-ins ignore. This is the seam Phase 6's WASM hook plugs into
    /// (ADR 0014); proving it here keeps the trait honest before any runtime
    /// exists.
    #[test]
    fn choose_with_can_be_overridden_to_use_locality() {
        struct StickiestInputRoot;
        impl Strategy for StickiestInputRoot {
            fn choose(&self, candidates: &[WorkerId], loads: &dyn LoadView) -> Option<WorkerId> {
                SimpleFifo.choose(candidates, loads)
            }
            fn choose_with(
                &self,
                candidates: &[WorkerId],
                ctx: &DecisionContext<'_>,
            ) -> Option<WorkerId> {
                let Some(root) = ctx.job.input_root_digest else {
                    return self.choose(candidates, ctx.loads);
                };
                candidates
                    .iter()
                    .max_by_key(|w| ctx.locality.input_root_hits(w, root))
                    .cloned()
            }
        }

        struct Warm(HashMap<WorkerId, u32>);
        impl LocalityView for Warm {
            fn input_root_hits(&self, worker: &WorkerId, _root: &Digest) -> u32 {
                self.0.get(worker).copied().unwrap_or(0)
            }
            fn action_hits(&self, _worker: &WorkerId, _action: &Digest) -> u32 {
                0
            }
        }

        let cands = vec![wid("cold"), wid("warm")];
        // "warm" is the *more* loaded worker, so a load-only policy would
        // avoid it. Locality must win here, or the override isn't wired.
        let l = loads(&[("cold", 0), ("warm", 7)]);
        let warm = Warm(HashMap::from([(wid("warm"), 5)]));
        let (t, ad, ir, p) = (
            TenantId::default(),
            digest("ab"),
            digest("cd"),
            rapi::Platform::default(),
        );

        assert_eq!(SimpleFifo.choose(&cands, &l), Some(wid("cold")));
        assert_eq!(
            StickiestInputRoot.choose_with(&cands, &ctx(&l, &warm, &t, &ad, &ir, &p)),
            Some(wid("warm")),
            "the overriding policy must see locality, not just load"
        );
    }

    /// The contract every implementation is held to: `None` iff empty.
    /// Phase 6's `WasmStrategy` will be added to this list — a guest that traps
    /// or declines must yield the built-in's answer, never `None`, because
    /// `None` for a non-empty candidate set stalls a placeable job.
    #[test]
    fn choose_with_returns_none_iff_candidates_is_empty() {
        let l = loads(&[("a", 1)]);
        let (t, ad, ir, p) = (
            TenantId::default(),
            digest("ab"),
            digest("cd"),
            rapi::Platform::default(),
        );
        let c = ctx(&l, &NoLocality, &t, &ad, &ir, &p);
        let strategies: Vec<Box<dyn Strategy>> =
            vec![Box::new(SimpleFifo), Box::new(BinPacking::new(2))];
        let non_empty = vec![wid("a")];
        for s in &strategies {
            assert!(s.choose_with(&[], &c).is_none(), "empty must yield None");
            assert!(
                s.choose_with(&non_empty, &c).is_some(),
                "non-empty must yield Some"
            );
        }
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
