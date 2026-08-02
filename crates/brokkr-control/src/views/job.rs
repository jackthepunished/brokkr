//! Job projections and the bounded completed-job history.

use std::collections::VecDeque;

/// What happened to a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Waiting for an eligible, idle worker.
    Queued,
    /// Leased to a worker and running.
    Running,
    /// Reported a zero exit code.
    Succeeded,
    /// Reported a non-zero exit code, or failed to dispatch.
    Failed,
}

impl JobState {
    /// A stable lowercase tag, for the wire and for `ListJobs` filtering.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    /// Parse a filter tag. `None` for anything unrecognised, which callers
    /// treat as "no filter" rather than an error — a client from a newer
    /// release asking for a state we do not know should get everything rather
    /// than a rejection.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One job, as an operator sees it.
///
/// Ids are plain `String`s rather than the project's newtypes, consistently
/// with every other view DTO ([`WorkerView`](super::WorkerView),
/// [`NodeView`](super::NodeView), [`PolicyView`](super::PolicyView)). The
/// newtype rule guards *domain* values against being constructed out of
/// invariant; a view is a read-only projection shaped for the wire, is never
/// used to construct a domain object, and maps one-to-one onto its proto
/// message. Typing these would add conversions in both directions and make
/// this DTO inconsistent with its siblings, for no invariant actually
/// protected.
///
/// `action_digest` carries the hash only, not the size. That is enough to
/// identify and grep for an action, which is what an operator console is for.
/// It is deliberately *not* enough to fetch the action from CAS — a read-only
/// observability surface is not a CAS client, and adding `size_bytes` is a
/// small additive change if a consumer ever needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSummary {
    /// Server-assigned job id.
    pub job_id: String,
    /// Submitting tenant.
    pub tenant: String,
    /// Lowercase hex sha256 of the action.
    pub action_digest: String,
    /// The job's state.
    pub state: JobState,
    /// The worker that ran it, once one was chosen.
    pub worker_id: Option<String>,
    /// Exit code, once reported.
    pub exit_code: Option<i32>,
    /// Wall-clock completion time, in Unix milliseconds.
    ///
    /// **The global merge key.** Three nodes each keep their own ring, so a
    /// union of them has no meaningful order unless one field orders records
    /// that originated on different machines — and "recent jobs" that are not
    /// actually the most recent is a display that lies.
    ///
    /// Wall-clock (`SystemTime`), deliberately, unlike the monotonic `Instant`
    /// used for worker liveness: this one *must* be comparable across nodes.
    /// Clock skew between control-plane nodes therefore skews the ordering.
    /// That is accepted, and is why the field is exposed rather than hidden
    /// behind an opaque rank an operator could not sanity-check.
    pub completed_at_unix_ms: u64,
    /// The control-plane node that scheduled this job.
    ///
    /// Per node like every other node-local record: the history ring lives in
    /// one node's memory and is never a cluster-wide fact.
    pub owning_node: String,
}

/// A bounded ring of recently completed jobs.
///
/// In-memory and bounded on purpose — durable job history is a
/// scheduler-storage decision ADR 0012 explicitly defers.
#[derive(Debug)]
pub struct JobHistory {
    entries: VecDeque<JobSummary>,
    capacity: usize,
}

impl JobHistory {
    /// A history retaining `capacity` completed jobs.
    ///
    /// Clamped to at least 1: a zero capacity would record nothing while still
    /// costing the call on every report, which is a configuration mistake
    /// rather than a way to disable the feature.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Record a completed job, evicting the oldest if at capacity.
    pub fn record(&mut self, summary: JobSummary) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(summary);
    }

    /// The most recent `limit` jobs, newest first.
    pub fn recent(&self, limit: usize) -> Vec<JobSummary> {
        self.entries.iter().rev().take(limit).cloned().collect()
    }

    /// The most recent `limit` jobs matching `state`, newest first.
    pub fn filtered(&self, state: Option<JobState>, limit: usize) -> Vec<JobSummary> {
        self.entries
            .iter()
            .rev()
            .filter(|j| state.is_none_or(|s| j.state == s))
            .take(limit)
            .cloned()
            .collect()
    }
}

impl Default for JobHistory {
    fn default() -> Self {
        Self::new(super::DEFAULT_JOB_HISTORY)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    fn summary(id: &str) -> JobSummary {
        JobSummary {
            job_id: id.to_string(),
            tenant: "t".to_string(),
            action_digest: "a".repeat(64),
            state: JobState::Succeeded,
            worker_id: Some("w-a".to_string()),
            exit_code: Some(0),
            completed_at_unix_ms: 1_700_000_000_000,
            owning_node: "node-1".to_string(),
        }
    }

    #[test]
    fn an_empty_history_lists_nothing() {
        let h = JobHistory::new(4);
        assert!(h.recent(10).is_empty());
    }

    /// Newest first: an operator opening a console wants the last thing that
    /// happened, not the oldest thing still retained.
    #[test]
    fn recent_returns_newest_first() {
        let mut h = JobHistory::new(8);
        for id in ["j1", "j2", "j3"] {
            h.record(summary(id));
        }
        let recent = h.recent(10);
        let ids: Vec<&str> = recent.iter().map(|j| j.job_id.as_str()).collect();
        assert_eq!(ids, vec!["j3", "j2", "j1"]);
    }

    #[test]
    fn the_ring_is_bounded_and_drops_oldest_first() {
        let mut h = JobHistory::new(3);
        for id in ["j1", "j2", "j3", "j4", "j5"] {
            h.record(summary(id));
        }
        let recent = h.recent(10);
        let ids: Vec<&str> = recent.iter().map(|j| j.job_id.as_str()).collect();
        assert_eq!(ids, vec!["j5", "j4", "j3"]);
    }

    /// `usize::MAX` means "everything retained", so a caller that does not want
    /// to impose its own cap gets the whole configured ring rather than a
    /// silently-defaulted slice.
    #[test]
    fn an_unbounded_limit_returns_the_whole_ring() {
        let mut h = JobHistory::new(300);
        for i in 0..300 {
            h.record(summary(&format!("j{i}")));
        }
        assert_eq!(h.recent(usize::MAX).len(), 300);
        assert_eq!(h.filtered(None, usize::MAX).len(), 300);
    }

    #[test]
    fn recent_respects_its_limit() {
        let mut h = JobHistory::new(8);
        for id in ["j1", "j2", "j3", "j4"] {
            h.record(summary(id));
        }
        assert_eq!(h.recent(2).len(), 2);
        assert_eq!(h.recent(0).len(), 0);
    }

    /// A capacity of zero would silently record nothing while still costing
    /// the call on every report — a configuration mistake, not a way to
    /// disable the feature.
    #[test]
    fn a_zero_capacity_is_clamped() {
        let mut h = JobHistory::new(0);
        h.record(summary("j1"));
        assert_eq!(h.recent(10).len(), 1);
    }

    #[test]
    fn state_filter_selects_only_matching_jobs() {
        let mut h = JobHistory::new(8);
        h.record(summary("ok"));
        let mut failed = summary("bad");
        failed.state = JobState::Failed;
        failed.exit_code = Some(1);
        h.record(failed);

        let only_failed = h.filtered(Some(JobState::Failed), 10);
        assert_eq!(only_failed.len(), 1);
        assert_eq!(only_failed[0].job_id, "bad");
        assert_eq!(h.filtered(None, 10).len(), 2);
    }

    /// An unrecognised filter is "no filter", not an error: a client from a
    /// newer release asking about a state we do not know should get everything
    /// rather than a rejection.
    #[test]
    fn an_unknown_state_tag_parses_as_no_filter() {
        assert_eq!(JobState::from_str_opt("failed"), Some(JobState::Failed));
        assert_eq!(JobState::from_str_opt(""), None);
        assert_eq!(JobState::from_str_opt("evaporated"), None);
    }

    #[test]
    fn state_tags_are_stable_and_distinct() {
        let all = [
            JobState::Queued,
            JobState::Running,
            JobState::Succeeded,
            JobState::Failed,
        ];
        let mut tags: Vec<&str> = all.iter().map(|s| s.as_str()).collect();
        let n = tags.len();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), n, "state tags must be unique");
        for s in all {
            assert_eq!(JobState::from_str_opt(s.as_str()), Some(s), "round trip");
        }
    }
}
