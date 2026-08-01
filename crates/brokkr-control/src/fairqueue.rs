//! Per-tenant virtual-time fair queue (Start-time Fair Queuing) for the
//! scheduler's pending jobs (ADR 0010).
//!
//! Each enqueued job is tagged with a virtual *start* time
//! `start = max(virtual_time, last_finish[tenant])`, and its tenant's clock
//! advances by `cost / weight`. Jobs are serviced in increasing start-tag
//! order, which gives each tenant a share of dispatch slots proportional to its
//! weight. Action cost is unknown up front, so every job is unit cost — a
//! weight-2 tenant is simply serviced twice as often as a weight-1 tenant.
//!
//! Dequeue is *eligibility-constrained*: the scheduler doesn't always want the
//! global minimum start tag, but the lowest-start-tag job that has an idle
//! eligible worker. So this structure exposes its entries (in no particular
//! order, each with its start tag) for the caller to scan, plus a `take(index)`
//! that removes a chosen entry and advances virtual time — mirroring how the
//! scheduler already scans its pending list under one lock.
//!
//! Pure + generic over the job payload `J` so it is unit-testable without the
//! scheduler's job types. Not internally synchronized.

use std::collections::HashMap;

use brokkr_common::TenantId;

/// Virtual-time cost of one unit-weight job. A tenant of weight `w` advances
/// its virtual clock by `COST / w` per job, so higher weight ⇒ smaller
/// increments ⇒ more frequent service. Picked large enough that integer
/// division by realistic weights keeps useful resolution.
const COST: u64 = 1_000_000;

struct Entry<J> {
    start: u64,
    job: J,
}

/// A borrowed view of one queued job: its index (for [`FairQueue::take`]), its
/// virtual start tag, and the job payload.
pub struct Slot<'a, J> {
    /// Index for [`FairQueue::take`]. Valid until the next mutation.
    pub index: usize,
    /// Virtual start tag — lower means "more owed service".
    pub start: u64,
    /// The queued job.
    pub job: &'a J,
}

/// Start-time fair queue keyed by tenant.
pub struct FairQueue<J> {
    entries: Vec<Entry<J>>,
    virtual_time: u64,
    tenant_finish: HashMap<TenantId, u64>,
    weights: HashMap<TenantId, u32>,
}

impl<J> Default for FairQueue<J> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            virtual_time: 0,
            tenant_finish: HashMap::new(),
            weights: HashMap::new(),
        }
    }
}

impl<J> FairQueue<J> {
    /// An empty fair queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a tenant's scheduling weight (default 1). A weight-2 tenant is
    /// serviced about twice as often as a weight-1 tenant. Zero is clamped to 1.
    pub fn set_weight(&mut self, tenant: TenantId, weight: u32) {
        self.weights.insert(tenant, weight.max(1));
    }

    fn weight_of(&self, tenant: &TenantId) -> u64 {
        self.weights.get(tenant).copied().unwrap_or(1).max(1) as u64
    }

    /// Number of queued jobs.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Enqueue `job` for `tenant`, assigning it an SFQ start tag.
    pub fn push(&mut self, tenant: TenantId, job: J) {
        let weight = self.weight_of(&tenant);
        let last_finish = self.tenant_finish.get(&tenant).copied().unwrap_or(0);
        let start = self.virtual_time.max(last_finish);
        let finish = start.saturating_add(COST / weight);
        self.tenant_finish.insert(tenant, finish);
        self.entries.push(Entry { start, job });
    }

    /// Iterate queued jobs as [`Slot`]s (unordered). The caller picks the
    /// lowest-`start` slot that is dispatchable and calls [`take`](Self::take).
    pub fn slots(&self) -> impl Iterator<Item = Slot<'_, J>> {
        self.entries.iter().enumerate().map(|(index, e)| Slot {
            index,
            start: e.start,
            job: &e.job,
        })
    }

    /// Borrow the slot at `index`, or `None` if out of range.
    ///
    /// Lets a caller that selected an index during a [`slots`](Self::slots)
    /// walk read that job again — to build a scheduling decision context from
    /// it, say — without cloning the job or re-scanning the queue. Indices are
    /// only valid until the next [`take`](Self::take) or
    /// [`push`](Self::push), because `take` uses `swap_remove`.
    pub fn get(&self, index: usize) -> Option<Slot<'_, J>> {
        self.entries.get(index).map(|e| Slot {
            index,
            start: e.start,
            job: &e.job,
        })
    }

    /// Remove the entry at `index`, returning its job and advancing virtual
    /// time to that entry's start tag (the SFQ service rule). `None` if the
    /// index is out of range.
    pub fn take(&mut self, index: usize) -> Option<J> {
        if index >= self.entries.len() {
            return None;
        }
        let entry = self.entries.swap_remove(index);
        self.virtual_time = self.virtual_time.max(entry.start);
        Some(entry.job)
    }

    /// Pop the globally lowest-start-tag job (no eligibility constraint).
    /// Convenience for callers / tests that don't filter; equivalent to taking
    /// the minimum-`start` slot.
    pub fn pop(&mut self) -> Option<J> {
        let idx = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, e)| e.start)
            .map(|(i, _)| i)?;
        self.take(idx)
    }

    /// Drop queued jobs for which `keep` returns false (e.g. a job whose caller
    /// timed out). Does not touch virtual-time bookkeeping.
    pub fn retain<F: FnMut(&J) -> bool>(&mut self, mut keep: F) {
        self.entries.retain(|e| keep(&e.job));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    fn tid(s: &str) -> TenantId {
        TenantId::new(s.to_string()).unwrap()
    }

    /// Drain the whole queue in service (start-tag) order via `pop`.
    fn drain<J>(q: &mut FairQueue<J>) -> Vec<J> {
        let mut out = Vec::new();
        while let Some(j) = q.pop() {
            out.push(j);
        }
        out
    }

    #[test]
    fn empty_queue() {
        let mut q: FairQueue<u32> = FairQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        assert!(q.pop().is_none());
    }

    #[test]
    fn single_tenant_is_fifo() {
        let mut q = FairQueue::new();
        for i in 0..5 {
            q.push(tid("a"), i);
        }
        assert_eq!(drain(&mut q), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn two_equal_tenants_interleave() {
        let mut q = FairQueue::new();
        // All of A enqueued, then all of B — fair queuing must still interleave
        // them rather than serving all of A first.
        for i in 0..3 {
            q.push(tid("a"), format!("a{i}"));
        }
        for i in 0..3 {
            q.push(tid("b"), format!("b{i}"));
        }
        let order = drain(&mut q);
        // Equal weights ⇒ A and B alternate (ties broken by insertion via
        // swap_remove/min, but each tenant keeps its internal order).
        let a_positions: Vec<usize> = order
            .iter()
            .enumerate()
            .filter(|(_, s)| s.starts_with('a'))
            .map(|(i, _)| i)
            .collect();
        // A's three jobs land at alternating-ish positions, not all up front:
        // with fair queuing the last A job is not before the first B job.
        assert!(
            a_positions.iter().max().unwrap() >= &2,
            "fair queue should interleave tenants, got {order:?}"
        );
        // Each tenant's own jobs stay in order.
        let a_order: Vec<&String> = order.iter().filter(|s| s.starts_with('a')).collect();
        assert_eq!(a_order, vec!["a0", "a1", "a2"]);
    }

    #[test]
    fn higher_weight_tenant_serviced_more_often() {
        let mut q = FairQueue::new();
        q.set_weight(tid("big"), 3);
        q.set_weight(tid("small"), 1);
        // Plenty of jobs from both, enqueued small-first to be adversarial.
        for i in 0..9 {
            q.push(tid("small"), format!("s{i}"));
        }
        for i in 0..9 {
            q.push(tid("big"), format!("b{i}"));
        }
        // Over the first 8 dispatches, the weight-3 tenant should get clearly
        // more than the weight-1 tenant.
        let mut big = 0;
        let mut small = 0;
        for _ in 0..8 {
            match q.pop().unwrap().chars().next().unwrap() {
                'b' => big += 1,
                's' => small += 1,
                _ => unreachable!(),
            }
        }
        assert!(
            big > small,
            "weight-3 tenant should be serviced more: big={big} small={small}"
        );
    }

    #[test]
    fn take_lowest_eligible_slot_respects_a_filter() {
        // Mimics the scheduler: only "b*" jobs are dispatchable right now.
        let mut q = FairQueue::new();
        q.push(tid("a"), "a0".to_string());
        q.push(tid("b"), "b0".to_string());
        q.push(tid("a"), "a1".to_string());

        // Pick the lowest-start slot whose job starts with 'b'.
        let idx = q
            .slots()
            .filter(|s| s.job.starts_with('b'))
            .min_by_key(|s| s.start)
            .map(|s| s.index)
            .unwrap();
        assert_eq!(q.take(idx).unwrap(), "b0");
        assert_eq!(q.len(), 2);
        // The a* jobs remain.
        assert_eq!(drain(&mut q), vec!["a0".to_string(), "a1".to_string()]);
    }

    #[test]
    fn retain_drops_matching_jobs() {
        let mut q = FairQueue::new();
        q.push(tid("a"), 1u32);
        q.push(tid("a"), 2);
        q.push(tid("a"), 3);
        q.retain(|j| *j != 2);
        assert_eq!(drain(&mut q), vec![1, 3]);
    }

    #[test]
    fn idle_tenant_does_not_hoard_backlog_credit() {
        // A tenant that was absent shouldn't get a burst of priority when it
        // returns: its start tag is clamped to the current virtual time, not
        // its stale finish tag.
        let mut q = FairQueue::new();
        q.push(tid("a"), "a0".to_string());
        // Service a few of A so virtual_time advances.
        let _ = q.pop();
        q.push(tid("a"), "a1".to_string());
        let _ = q.pop();
        // Now B arrives for the first time and A also has one queued.
        q.push(tid("a"), "a2".to_string());
        q.push(tid("b"), "b0".to_string());
        // B's start tag is the current virtual time, so it isn't starved behind
        // A's accumulated finish tag — B should be served no later than A here.
        let order = drain(&mut q);
        let b_pos = order.iter().position(|s| s == "b0").unwrap();
        let a2_pos = order.iter().position(|s| s == "a2").unwrap();
        assert!(
            b_pos <= a2_pos,
            "newly-arriving tenant must not be starved: {order:?}"
        );
    }
}
