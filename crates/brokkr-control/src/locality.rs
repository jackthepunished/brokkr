//! Per-worker completion history, the source of the [`LocalityView`] signal a
//! scheduling [`Strategy`](crate::scheduling::Strategy) may consult (ADR 0014).
//!
//! The idea is small: a worker that recently ran an action with a given input
//! root very likely still has those inputs materialized locally, so sending it
//! the next action with the same input root skips a fetch. This module records
//! just enough to answer that, and nothing more.
//!
//! Two properties are load-bearing and are what most of the code here is for:
//!
//! - **Bounded.** A per-worker window and an LRU cap over workers, so a
//!   long-running control plane with a churning fleet cannot grow this without
//!   limit. The ceiling is explicit: `max_workers × window` entries.
//! - **Survives disconnect.** A worker that reconnects almost certainly still
//!   has its inputs on disk. Forgetting its history when its stream drops would
//!   discard precisely the signal being collected.

use std::collections::{HashMap, VecDeque};

use brokkr_common::{Digest, WorkerId};

use crate::scheduling::LocalityView;

/// Per-worker completions retained by default. Deep enough that a repeated
/// build's input root is still visible after unrelated work interleaves,
/// shallow enough that the scan per candidate stays trivial.
pub const DEFAULT_WINDOW: usize = 64;

/// Workers retained by default before the least-recently-active is evicted.
/// Well above any plausible fleet size, so eviction is a safety net rather than
/// a routine event.
pub const DEFAULT_MAX_WORKERS: usize = 1024;

/// One completed job, reduced to the two digests locality is about.
#[derive(Debug, Clone)]
struct Completion {
    action: Digest,
    /// `None` when the action had no input root, or its digest failed
    /// validation — locality is a hint, so an unusable one is simply absent.
    input_root: Option<Digest>,
}

/// One worker's recent completions, plus when it was last active.
#[derive(Debug, Default)]
struct WorkerHistory {
    recent: VecDeque<Completion>,
    /// Monotonic counter value at the last `record`. Used to pick an eviction
    /// victim without maintaining an LRU list.
    last_touch: u64,
}

/// Bounded per-worker history of recently completed actions.
#[derive(Debug)]
pub struct LocalityIndex {
    window: usize,
    max_workers: usize,
    workers: HashMap<WorkerId, WorkerHistory>,
    /// Monotonic tick, incremented per `record`. Only ordering matters, so
    /// wrapping after 2^64 completions is not a concern worth code.
    tick: u64,
}

impl Default for LocalityIndex {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW, DEFAULT_MAX_WORKERS)
    }
}

impl LocalityIndex {
    /// An index retaining `window` completions per worker, for at most
    /// `max_workers` workers. Both are clamped to at least 1: a zero window
    /// would record nothing while still paying for the bookkeeping, which is
    /// a configuration mistake rather than a way to disable the feature.
    pub fn new(window: usize, max_workers: usize) -> Self {
        Self {
            window: window.max(1),
            max_workers: max_workers.max(1),
            workers: HashMap::new(),
            tick: 0,
        }
    }

    /// Record that `worker` completed `action` (whose inputs were rooted at
    /// `input_root`, when it had one).
    ///
    /// Called when a lease completes on a worker's own report — the one place
    /// the scheduler learns "worker W finished action A". Deliberately *not*
    /// called on lease expiry: a lease that timed out means the worker never
    /// reported, so it is most likely dead or partitioned and its cache state
    /// is unknown.
    ///
    /// A failing action still counts. Locality is about whether the inputs are
    /// present on that worker, and an action that ran and exited non-zero
    /// fetched its inputs just the same. Over-recording costs at most one
    /// mistaken preference — a cache miss, which is the status quo — whereas
    /// under-recording silently loses the signal.
    pub fn record(&mut self, worker: &WorkerId, action: &Digest, input_root: Option<&Digest>) {
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        let history = self.workers.entry(worker.clone()).or_default();
        history.recent.push_back(Completion {
            action: action.clone(),
            input_root: input_root.cloned(),
        });
        if history.recent.len() > self.window {
            history.recent.pop_front();
        }
        history.last_touch = tick;

        if self.workers.len() > self.max_workers {
            self.evict_least_recent(worker);
        }
    }

    /// Drop the least recently active worker, never `keep` (which was just
    /// recorded and would otherwise be evictable on a `max_workers` of 1).
    ///
    /// This scan is O(workers), but it only runs while over the cap — the
    /// common path is a single hash lookup.
    fn evict_least_recent(&mut self, keep: &WorkerId) {
        let victim = self
            .workers
            .iter()
            .filter(|(id, _)| *id != keep)
            .min_by_key(|(id, h)| (h.last_touch, id.as_str().to_string()))
            .map(|(id, _)| id.clone());
        if let Some(v) = victim {
            self.workers.remove(&v);
        }
    }

    /// Number of workers currently tracked.
    pub fn tracked_workers(&self) -> usize {
        self.workers.len()
    }

    /// Number of completions retained for `worker`.
    pub fn history_len(&self, worker: &WorkerId) -> usize {
        self.workers.get(worker).map_or(0, |h| h.recent.len())
    }

    /// Forget everything about `worker`.
    ///
    /// Not called on disconnect — see the module docs — but the control plane
    /// needs a way to drop a worker that has been decommissioned rather than
    /// waiting for LRU pressure.
    pub fn forget(&mut self, worker: &WorkerId) {
        self.workers.remove(worker);
    }
}

impl LocalityView for LocalityIndex {
    fn input_root_hits(&self, worker: &WorkerId, input_root: &Digest) -> u32 {
        self.workers.get(worker).map_or(0, |h| {
            h.recent
                .iter()
                .filter(|c| c.input_root.as_ref() == Some(input_root))
                .count() as u32
        })
    }

    fn action_hits(&self, worker: &WorkerId, action: &Digest) -> u32 {
        self.workers.get(worker).map_or(0, |h| {
            h.recent.iter().filter(|c| &c.action == action).count() as u32
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    fn wid(s: &str) -> WorkerId {
        WorkerId::new(s.to_string()).unwrap()
    }

    /// A distinct valid digest per `n`.
    fn dig(n: u8) -> Digest {
        Digest::of(&[n])
    }

    #[test]
    fn an_empty_index_reports_no_hits() {
        let idx = LocalityIndex::default();
        assert_eq!(idx.input_root_hits(&wid("a"), &dig(1)), 0);
        assert_eq!(idx.action_hits(&wid("a"), &dig(1)), 0);
        assert_eq!(idx.tracked_workers(), 0);
    }

    #[test]
    fn records_are_counted_per_worker_and_per_digest() {
        let mut idx = LocalityIndex::default();
        let (a, b) = (wid("a"), wid("b"));
        idx.record(&a, &dig(1), Some(&dig(10)));
        idx.record(&a, &dig(2), Some(&dig(10)));
        idx.record(&a, &dig(1), Some(&dig(11)));
        idx.record(&b, &dig(1), Some(&dig(10)));

        assert_eq!(idx.input_root_hits(&a, &dig(10)), 2);
        assert_eq!(idx.input_root_hits(&a, &dig(11)), 1);
        assert_eq!(idx.action_hits(&a, &dig(1)), 2);
        assert_eq!(idx.action_hits(&a, &dig(2)), 1);
        // b's history is its own.
        assert_eq!(idx.input_root_hits(&b, &dig(10)), 1);
        assert_eq!(idx.action_hits(&b, &dig(2)), 0);
        // An untracked worker is not an error, just no signal.
        assert_eq!(idx.action_hits(&wid("ghost"), &dig(1)), 0);
    }

    #[test]
    fn an_action_without_an_input_root_records_the_action_only() {
        let mut idx = LocalityIndex::default();
        let a = wid("a");
        idx.record(&a, &dig(1), None);
        assert_eq!(idx.action_hits(&a, &dig(1)), 1);
        assert_eq!(idx.input_root_hits(&a, &dig(10)), 0);
    }

    #[test]
    fn the_per_worker_window_is_bounded_and_evicts_oldest_first() {
        let mut idx = LocalityIndex::new(3, 16);
        let a = wid("a");
        // 5 distinct actions into a window of 3: the first two fall out.
        for n in 1..=5u8 {
            idx.record(&a, &dig(n), Some(&dig(100)));
        }
        assert_eq!(idx.history_len(&a), 3);
        assert_eq!(
            idx.action_hits(&a, &dig(1)),
            0,
            "oldest should have aged out"
        );
        assert_eq!(idx.action_hits(&a, &dig(2)), 0);
        assert_eq!(idx.action_hits(&a, &dig(5)), 1, "newest must be retained");
        // The shared input root is still counted, but only within the window.
        assert_eq!(idx.input_root_hits(&a, &dig(100)), 3);
    }

    #[test]
    fn a_zero_window_is_clamped_rather_than_silently_recording_nothing() {
        let mut idx = LocalityIndex::new(0, 0);
        let a = wid("a");
        idx.record(&a, &dig(1), Some(&dig(10)));
        assert_eq!(idx.history_len(&a), 1);
        assert_eq!(idx.tracked_workers(), 1);
    }

    #[test]
    fn the_worker_lru_is_bounded_and_evicts_the_least_recently_active() {
        let mut idx = LocalityIndex::new(4, 2);
        let (a, b, c) = (wid("a"), wid("b"), wid("c"));
        idx.record(&a, &dig(1), None);
        idx.record(&b, &dig(2), None);
        // Touch `a` so `b` becomes the least recently active.
        idx.record(&a, &dig(3), None);
        // A third worker pushes over the cap of 2.
        idx.record(&c, &dig(4), None);

        assert_eq!(idx.tracked_workers(), 2);
        assert_eq!(
            idx.action_hits(&b, &dig(2)),
            0,
            "b was least recent, evicted"
        );
        assert_eq!(idx.action_hits(&a, &dig(1)), 1, "a was touched, retained");
        assert_eq!(idx.action_hits(&c, &dig(4)), 1, "c was just recorded");
    }

    #[test]
    fn eviction_never_drops_the_worker_just_recorded() {
        // Cap of 1: every record is immediately over the cap, and the naive
        // "evict the minimum" would evict the worker just written.
        let mut idx = LocalityIndex::new(4, 1);
        let (a, b) = (wid("a"), wid("b"));
        idx.record(&a, &dig(1), None);
        idx.record(&b, &dig(2), None);
        assert_eq!(idx.tracked_workers(), 1);
        assert_eq!(
            idx.action_hits(&b, &dig(2)),
            1,
            "the most recent record must survive its own eviction pass"
        );
    }

    #[test]
    fn forget_drops_one_worker_only() {
        let mut idx = LocalityIndex::default();
        let (a, b) = (wid("a"), wid("b"));
        idx.record(&a, &dig(1), None);
        idx.record(&b, &dig(1), None);
        idx.forget(&a);
        assert_eq!(idx.action_hits(&a, &dig(1)), 0);
        assert_eq!(idx.action_hits(&b, &dig(1)), 1);
        assert_eq!(idx.tracked_workers(), 1);
    }

    /// Eviction picks a deterministic victim when two workers tie on
    /// `last_touch`, so a fleet at the cap doesn't evict differently run to
    /// run. (Ties are only reachable via the wrapping tick, but determinism
    /// here is free and the alternative is a heisenbug.)
    #[test]
    fn eviction_is_deterministic_under_a_tie() {
        let run = || {
            let mut idx = LocalityIndex::new(4, 2);
            for id in ["a", "b", "c"] {
                idx.record(&wid(id), &dig(1), None);
            }
            (0..3)
                .map(|i| {
                    let id = ["a", "b", "c"][i];
                    idx.action_hits(&wid(id), &dig(1))
                })
                .collect::<Vec<_>>()
        };
        let first = run();
        for _ in 0..20 {
            assert_eq!(run(), first, "eviction must not depend on hash order");
        }
    }
}
