//! In-memory worker registry with capability tracking and heartbeat-based
//! liveness.
//!
//! This is the Phase 4 (`docs/plan.md` §16, task 1) foundation: the control
//! plane records each worker's declared capabilities and the last time it was
//! heard from, and evicts workers that miss too many heartbeats. The registry
//! is deliberately transport-agnostic — it takes an explicit `now: Instant` on
//! every time-sensitive call so it is deterministic under test (no
//! `Instant::now()` / `SystemTime::now()` reached for internally). The
//! heartbeat RPC and the scheduler's worker selection plug into this in later
//! increments.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use brokkr_common::WorkerId;
use thiserror::Error;

/// Default heartbeat interval the control plane suggests to workers.
///
/// Plan §16, task 1: "Workers heartbeat every 5s".
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Default number of consecutive heartbeats a worker may miss before the
/// control plane evicts it.
///
/// Plan §16, task 1: "control plane evicts after 3 missed heartbeats".
pub const DEFAULT_MAX_MISSED_HEARTBEATS: u32 = 3;

/// Errors returned by [`WorkerRegistry`] operations.
#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum RegistryError {
    /// A heartbeat (or other lookup) referenced a worker the registry has no
    /// record of — either it was never registered or it has already been
    /// evicted.
    #[error("unknown worker: {0}")]
    UnknownWorker(WorkerId),
}

/// Static description of what a worker can run.
///
/// Phase 4 starts with the same surface the `brokkr.v1.RegisterWorkerRequest`
/// proto already carries (a human label plus free-form key/value labels).
/// Richer fields (CPU cores, memory, installed tools, GPU) from the
/// `WorkerCapability` sketch in plan §8 are added when constraint matching
/// needs them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerCapabilities {
    /// Hostname or human-friendly label. May be empty.
    pub hostname: String,
    /// Free-form constraint labels, e.g. `{"os": "linux", "arch": "x86_64"}`.
    ///
    /// A `BTreeMap` (not `HashMap`) so iteration and equality are
    /// deterministic — the scheduler's constraint matcher will want a stable
    /// order.
    pub labels: BTreeMap<String, String>,
}

/// Liveness policy: how often workers heartbeat and how many they may miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatPolicy {
    /// Expected interval between heartbeats.
    pub interval: Duration,
    /// Number of consecutive missed heartbeats tolerated before eviction.
    pub max_missed: u32,
}

impl HeartbeatPolicy {
    /// The staleness deadline: a worker not heard from within
    /// `interval * max_missed` is evictable.
    ///
    /// Saturates instead of overflowing on absurd configurations.
    pub fn deadline(&self) -> Duration {
        self.interval.saturating_mul(self.max_missed)
    }
}

impl Default for HeartbeatPolicy {
    fn default() -> Self {
        Self {
            interval: DEFAULT_HEARTBEAT_INTERVAL,
            max_missed: DEFAULT_MAX_MISSED_HEARTBEATS,
        }
    }
}

/// A single registered worker's record.
#[derive(Debug, Clone)]
pub struct WorkerRecord {
    /// What the worker declared it can run.
    pub capabilities: WorkerCapabilities,
    /// When the worker first registered (monotonic).
    pub registered_at: Instant,
    /// When the worker was last heard from (registration counts as the first
    /// heartbeat).
    pub last_seen: Instant,
}

impl WorkerRecord {
    /// Whether this worker is stale as of `now` under `policy` — i.e. it has
    /// not been heard from within the policy's [`deadline`](HeartbeatPolicy::deadline).
    ///
    /// Uses `saturating_duration_since` so a `now` earlier than `last_seen`
    /// (clock skew in a caller-supplied instant) reads as "not stale" rather
    /// than panicking.
    pub fn is_stale(&self, now: Instant, policy: &HeartbeatPolicy) -> bool {
        now.saturating_duration_since(self.last_seen) > policy.deadline()
    }
}

/// In-memory registry of live workers.
///
/// Not internally synchronized — callers wrap it in their own lock (the
/// control plane holds one behind a `tokio::sync::Mutex`/`RwLock`). Keeping
/// the registry lock-free here makes the eviction logic trivially testable.
#[derive(Debug)]
pub struct WorkerRegistry {
    workers: HashMap<WorkerId, WorkerRecord>,
    policy: HeartbeatPolicy,
}

impl WorkerRegistry {
    /// Create an empty registry with the given heartbeat policy.
    pub fn new(policy: HeartbeatPolicy) -> Self {
        Self {
            workers: HashMap::new(),
            policy,
        }
    }

    /// The heartbeat policy in effect.
    pub fn policy(&self) -> &HeartbeatPolicy {
        &self.policy
    }

    /// Register (or re-register) a worker, recording `now` as both its
    /// registration time and its first heartbeat.
    ///
    /// Re-registering an existing `id` replaces its capabilities and resets
    /// `registered_at` — a worker that reconnects starts a fresh lifecycle.
    pub fn register(&mut self, id: WorkerId, capabilities: WorkerCapabilities, now: Instant) {
        tracing::debug!(worker_id = %id, hostname = %capabilities.hostname, "worker registered");
        self.workers.insert(
            id,
            WorkerRecord {
                capabilities,
                registered_at: now,
                last_seen: now,
            },
        );
    }

    /// Record a heartbeat from `id`, advancing its `last_seen` to `now`.
    ///
    /// Returns [`RegistryError::UnknownWorker`] if the worker is not
    /// registered (the caller should tell the worker to re-register).
    pub fn record_heartbeat(&mut self, id: &WorkerId, now: Instant) -> Result<(), RegistryError> {
        match self.workers.get_mut(id) {
            Some(record) => {
                record.last_seen = now;
                tracing::trace!(worker_id = %id, "heartbeat");
                Ok(())
            }
            None => Err(RegistryError::UnknownWorker(id.clone())),
        }
    }

    /// Remove every worker that is stale as of `now`, returning the evicted
    /// IDs sorted lexically (for deterministic logging / assertions).
    pub fn evict_stale(&mut self, now: Instant) -> Vec<WorkerId> {
        let policy = self.policy;
        let mut evicted: Vec<WorkerId> = self
            .workers
            .iter()
            .filter(|(_, record)| record.is_stale(now, &policy))
            .map(|(id, _)| id.clone())
            .collect();
        evicted.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        for id in &evicted {
            self.workers.remove(id);
            tracing::info!(worker_id = %id, "worker evicted (missed heartbeats)");
        }
        evicted
    }

    /// Fetch a worker's record, if registered.
    pub fn get(&self, id: &WorkerId) -> Option<&WorkerRecord> {
        self.workers.get(id)
    }

    /// Whether `id` is currently registered (regardless of staleness).
    pub fn contains(&self, id: &WorkerId) -> bool {
        self.workers.contains_key(id)
    }

    /// Number of registered workers (including any not yet evicted stale ones).
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// Whether the registry has no workers.
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// Iterate over all registered workers.
    pub fn iter(&self) -> impl Iterator<Item = (&WorkerId, &WorkerRecord)> {
        self.workers.iter()
    }

    /// Iterate over workers that are live (not stale) as of `now`.
    ///
    /// This is the set the scheduler will pick from — it does not mutate the
    /// registry, so a stale-but-not-yet-evicted worker is simply skipped.
    pub fn healthy(&self, now: Instant) -> impl Iterator<Item = (&WorkerId, &WorkerRecord)> {
        let policy = self.policy;
        self.workers
            .iter()
            .filter(move |(_, record)| !record.is_stale(now, &policy))
    }
}

impl Default for WorkerRegistry {
    fn default() -> Self {
        Self::new(HeartbeatPolicy::default())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    fn wid(s: &str) -> WorkerId {
        WorkerId::new(s.to_string()).unwrap()
    }

    fn caps(hostname: &str) -> WorkerCapabilities {
        WorkerCapabilities {
            hostname: hostname.to_string(),
            labels: BTreeMap::new(),
        }
    }

    /// Test policy: 1s interval, 3 missed → 3s deadline. Keeps the injected
    /// instants easy to reason about.
    fn test_policy() -> HeartbeatPolicy {
        HeartbeatPolicy {
            interval: Duration::from_secs(1),
            max_missed: 3,
        }
    }

    #[test]
    fn deadline_is_interval_times_max_missed() {
        assert_eq!(test_policy().deadline(), Duration::from_secs(3));
        assert_eq!(
            HeartbeatPolicy::default().deadline(),
            Duration::from_secs(15)
        );
    }

    #[test]
    fn register_then_get_returns_record() {
        let t0 = Instant::now();
        let mut reg = WorkerRegistry::new(test_policy());
        reg.register(wid("w1"), caps("host-a"), t0);

        let record = reg.get(&wid("w1")).unwrap();
        assert_eq!(record.capabilities.hostname, "host-a");
        assert_eq!(record.registered_at, t0);
        assert_eq!(record.last_seen, t0);
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    #[test]
    fn heartbeat_advances_last_seen() {
        let t0 = Instant::now();
        let mut reg = WorkerRegistry::new(test_policy());
        reg.register(wid("w1"), caps("h"), t0);

        let t1 = t0 + Duration::from_secs(2);
        reg.record_heartbeat(&wid("w1"), t1).unwrap();
        assert_eq!(reg.get(&wid("w1")).unwrap().last_seen, t1);
        // registered_at is unchanged by a heartbeat.
        assert_eq!(reg.get(&wid("w1")).unwrap().registered_at, t0);
    }

    #[test]
    fn heartbeat_for_unknown_worker_errors() {
        let mut reg = WorkerRegistry::new(test_policy());
        let err = reg
            .record_heartbeat(&wid("ghost"), Instant::now())
            .unwrap_err();
        assert_eq!(err, RegistryError::UnknownWorker(wid("ghost")));
    }

    #[test]
    fn worker_within_deadline_is_not_stale_or_evicted() {
        let t0 = Instant::now();
        let mut reg = WorkerRegistry::new(test_policy()); // 3s deadline
        reg.register(wid("w1"), caps("h"), t0);

        // Exactly at the deadline is NOT stale (strictly-greater comparison).
        let at_deadline = t0 + Duration::from_secs(3);
        assert!(!reg
            .get(&wid("w1"))
            .unwrap()
            .is_stale(at_deadline, reg.policy()));
        assert!(reg.evict_stale(at_deadline).is_empty());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn worker_past_deadline_is_evicted() {
        let t0 = Instant::now();
        let mut reg = WorkerRegistry::new(test_policy()); // 3s deadline
        reg.register(wid("w1"), caps("h"), t0);

        let past = t0 + Duration::from_secs(3) + Duration::from_nanos(1);
        assert!(reg.get(&wid("w1")).unwrap().is_stale(past, reg.policy()));
        let evicted = reg.evict_stale(past);
        assert_eq!(evicted, vec![wid("w1")]);
        assert!(!reg.contains(&wid("w1")));
        assert!(reg.is_empty());
    }

    #[test]
    fn heartbeat_keeps_worker_alive_across_deadline() {
        let t0 = Instant::now();
        let mut reg = WorkerRegistry::new(test_policy()); // 3s deadline
        reg.register(wid("w1"), caps("h"), t0);

        // Heartbeat at +2s, then check at +4s: only 2s since last_seen → alive.
        reg.record_heartbeat(&wid("w1"), t0 + Duration::from_secs(2))
            .unwrap();
        let evicted = reg.evict_stale(t0 + Duration::from_secs(4));
        assert!(
            evicted.is_empty(),
            "fresh heartbeat should prevent eviction"
        );
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn evict_stale_only_removes_the_stale_and_returns_sorted() {
        let t0 = Instant::now();
        let mut reg = WorkerRegistry::new(test_policy()); // 3s deadline
        reg.register(wid("w-zeta"), caps("h"), t0);
        reg.register(wid("w-alpha"), caps("h"), t0);
        // Keep w-mid fresh.
        reg.register(wid("w-mid"), caps("h"), t0);
        reg.record_heartbeat(&wid("w-mid"), t0 + Duration::from_secs(4))
            .unwrap();

        let now = t0 + Duration::from_secs(5);
        let evicted = reg.evict_stale(now);
        // Sorted lexically; w-mid survives.
        assert_eq!(evicted, vec![wid("w-alpha"), wid("w-zeta")]);
        assert!(reg.contains(&wid("w-mid")));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn healthy_skips_stale_without_evicting() {
        let t0 = Instant::now();
        let mut reg = WorkerRegistry::new(test_policy());
        reg.register(wid("fresh"), caps("h"), t0);
        reg.register(wid("stale"), caps("h"), t0);
        reg.record_heartbeat(&wid("fresh"), t0 + Duration::from_secs(5))
            .unwrap();

        let now = t0 + Duration::from_secs(6);
        let healthy: Vec<&str> = {
            let mut v: Vec<&str> = reg.healthy(now).map(|(id, _)| id.as_str()).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(healthy, vec!["fresh"]);
        // healthy() is read-only: the stale worker is still registered.
        assert_eq!(reg.len(), 2);
        assert!(reg.contains(&wid("stale")));
    }

    #[test]
    fn reregister_resets_lifecycle() {
        let t0 = Instant::now();
        let mut reg = WorkerRegistry::new(test_policy());
        reg.register(wid("w1"), caps("old-host"), t0);

        let t1 = t0 + Duration::from_secs(10);
        reg.register(wid("w1"), caps("new-host"), t1);
        let record = reg.get(&wid("w1")).unwrap();
        assert_eq!(record.capabilities.hostname, "new-host");
        assert_eq!(record.registered_at, t1);
        assert_eq!(record.last_seen, t1);
        assert_eq!(reg.len(), 1, "re-register replaces, not duplicates");
    }
}
