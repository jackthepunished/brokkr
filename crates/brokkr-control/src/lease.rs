//! Job lease bookkeeping (ADR 0009).
//!
//! A lease records that a job has been dispatched to a worker and is expected
//! to report before a deadline. The scheduler's dispatcher creates a lease on
//! dispatch and resolves it on report; lease expiry or worker disconnect moves
//! the job's payload back to the pending queue for reassignment (the §16 DoD
//! crash-recovery path).
//!
//! This module is the pure bookkeeping core — the dispatcher wiring that drives
//! it is a separate increment. It's generic over the re-dispatch `payload` and
//! takes an explicit `now: Instant`, so it's deterministic under test.

use std::collections::HashMap;
use std::time::Instant;

use brokkr_common::{JobId, WorkerId};

/// An active lease: which worker holds the job, when it's due, and the payload
/// needed to re-dispatch the job if the lease fails.
#[derive(Debug, Clone)]
struct Lease<P> {
    worker_id: WorkerId,
    deadline: Instant,
    payload: P,
}

/// Tracks active job leases keyed by [`JobId`]. Generic over the re-dispatch
/// `payload` so it can be unit-tested without the scheduler's job types.
///
/// Not internally synchronized — the scheduler holds it under its own lock.
#[derive(Debug)]
pub struct LeaseTable<P> {
    leases: HashMap<JobId, Lease<P>>,
}

impl<P> Default for LeaseTable<P> {
    fn default() -> Self {
        Self {
            leases: HashMap::new(),
        }
    }
}

impl<P> LeaseTable<P> {
    /// An empty lease table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a lease: `job_id` dispatched to `worker_id`, due by `deadline`,
    /// carrying `payload` for re-dispatch. Replaces any existing lease for the
    /// same job (a reassignment supersedes the old one).
    pub fn insert(&mut self, job_id: JobId, worker_id: WorkerId, deadline: Instant, payload: P) {
        self.leases.insert(
            job_id,
            Lease {
                worker_id,
                deadline,
                payload,
            },
        );
    }

    /// Resolve a lease because the worker reported: drop it and return its
    /// payload, or `None` if there was no such lease (a late report after
    /// expiry/reassignment — the caller should discard the result).
    pub fn complete(&mut self, job_id: &JobId) -> Option<P> {
        self.leases.remove(job_id).map(|l| l.payload)
    }

    /// The worker currently holding `job_id`, if leased.
    pub fn worker_of(&self, job_id: &JobId) -> Option<&WorkerId> {
        self.leases.get(job_id).map(|l| &l.worker_id)
    }

    /// Whether `job_id` is currently leased.
    pub fn contains(&self, job_id: &JobId) -> bool {
        self.leases.contains_key(job_id)
    }

    /// Whether `worker_id` currently holds any lease. With worker capacity 1
    /// (ADR 0009) this is the worker's "busy" predicate for the dispatcher.
    pub fn is_worker_busy(&self, worker_id: &WorkerId) -> bool {
        self.leases.values().any(|l| &l.worker_id == worker_id)
    }

    /// Number of active leases.
    pub fn len(&self) -> usize {
        self.leases.len()
    }

    /// Whether there are no active leases.
    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }

    /// Remove and return every lease whose deadline has passed as of `now`
    /// (`now >= deadline`), as `(job_id, payload)` pairs for requeue. Sorted by
    /// job id for deterministic handling/logging.
    pub fn take_expired(&mut self, now: Instant) -> Vec<(JobId, P)> {
        let expired: Vec<JobId> = self
            .leases
            .iter()
            .filter(|(_, l)| now >= l.deadline)
            .map(|(id, _)| id.clone())
            .collect();
        Self::drain(&mut self.leases, expired)
    }

    /// Remove and return every lease held by `worker_id` (e.g. on disconnect),
    /// as `(job_id, payload)` pairs for requeue. Sorted by job id.
    pub fn take_worker(&mut self, worker_id: &WorkerId) -> Vec<(JobId, P)> {
        let held: Vec<JobId> = self
            .leases
            .iter()
            .filter(|(_, l)| &l.worker_id == worker_id)
            .map(|(id, _)| id.clone())
            .collect();
        Self::drain(&mut self.leases, held)
    }

    fn drain(leases: &mut HashMap<JobId, Lease<P>>, mut ids: Vec<JobId>) -> Vec<(JobId, P)> {
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids.into_iter()
            .filter_map(|id| leases.remove(&id).map(|l| (id, l.payload)))
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn jid(s: &str) -> JobId {
        JobId::new(s.to_string()).unwrap()
    }
    fn wid(s: &str) -> WorkerId {
        WorkerId::new(s.to_string()).unwrap()
    }

    #[test]
    fn insert_then_complete_returns_payload() {
        let t0 = Instant::now();
        let mut t = LeaseTable::new();
        t.insert(
            jid("j1"),
            wid("w1"),
            t0 + Duration::from_secs(10),
            "payload-1",
        );
        assert_eq!(t.len(), 1);
        assert!(t.contains(&jid("j1")));
        assert_eq!(t.worker_of(&jid("j1")), Some(&wid("w1")));

        assert_eq!(t.complete(&jid("j1")), Some("payload-1"));
        assert!(t.is_empty());
    }

    #[test]
    fn complete_absent_is_none() {
        let mut t: LeaseTable<&str> = LeaseTable::new();
        // A late report for a job that was already reassigned/expired.
        assert_eq!(t.complete(&jid("ghost")), None);
    }

    #[test]
    fn insert_replaces_existing_lease() {
        let t0 = Instant::now();
        let mut t = LeaseTable::new();
        t.insert(jid("j1"), wid("w1"), t0 + Duration::from_secs(5), "first");
        t.insert(jid("j1"), wid("w2"), t0 + Duration::from_secs(5), "second");
        assert_eq!(t.len(), 1);
        assert_eq!(t.worker_of(&jid("j1")), Some(&wid("w2")));
        assert_eq!(t.complete(&jid("j1")), Some("second"));
    }

    #[test]
    fn take_expired_returns_only_past_deadline_sorted() {
        let t0 = Instant::now();
        let mut t = LeaseTable::new();
        t.insert(
            jid("j-late"),
            wid("w1"),
            t0 + Duration::from_secs(1),
            "late",
        );
        t.insert(
            jid("j-early"),
            wid("w1"),
            t0 + Duration::from_secs(1),
            "early",
        );
        t.insert(
            jid("j-future"),
            wid("w2"),
            t0 + Duration::from_secs(60),
            "future",
        );

        // At t0+2s the two 1s-deadline leases are expired; the 60s one is not.
        let expired = t.take_expired(t0 + Duration::from_secs(2));
        let ids: Vec<&str> = expired.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["j-early", "j-late"]); // sorted by id
        assert_eq!(t.len(), 1);
        assert!(t.contains(&jid("j-future")));
    }

    #[test]
    fn take_expired_is_inclusive_at_deadline() {
        let t0 = Instant::now();
        let mut t = LeaseTable::new();
        let deadline = t0 + Duration::from_secs(5);
        t.insert(jid("j1"), wid("w1"), deadline, "p");
        // Exactly at the deadline counts as expired (now >= deadline).
        let expired = t.take_expired(deadline);
        assert_eq!(expired.len(), 1);
        assert!(t.is_empty());
    }

    #[test]
    fn take_worker_returns_only_that_workers_leases() {
        let t0 = Instant::now();
        let d = t0 + Duration::from_secs(30);
        let mut t = LeaseTable::new();
        t.insert(jid("j1"), wid("w-dead"), d, "a");
        t.insert(jid("j2"), wid("w-dead"), d, "b");
        t.insert(jid("j3"), wid("w-live"), d, "c");

        let reassigned = t.take_worker(&wid("w-dead"));
        let ids: Vec<&str> = reassigned.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["j1", "j2"]); // sorted
        assert_eq!(t.len(), 1);
        assert_eq!(t.worker_of(&jid("j3")), Some(&wid("w-live")));
    }

    #[test]
    fn take_worker_unknown_is_empty() {
        let mut t: LeaseTable<&str> = LeaseTable::new();
        assert!(t.take_worker(&wid("nobody")).is_empty());
    }

    #[test]
    fn is_worker_busy_reflects_held_leases() {
        let t0 = Instant::now();
        let d = t0 + Duration::from_secs(30);
        let mut t = LeaseTable::new();
        assert!(!t.is_worker_busy(&wid("w1")));
        t.insert(jid("j1"), wid("w1"), d, "p");
        assert!(t.is_worker_busy(&wid("w1")));
        assert!(!t.is_worker_busy(&wid("w2")));
        t.complete(&jid("j1"));
        assert!(!t.is_worker_busy(&wid("w1")));
    }
}
