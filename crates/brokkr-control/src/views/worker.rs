//! Worker projections.

use std::collections::BTreeMap;
use std::time::Instant;

use brokkr_common::WorkerId;

use crate::registry::WorkerRegistry;

/// One worker, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerView {
    /// The worker's id.
    pub worker_id: String,
    /// Hostname the worker declared at registration. May be empty.
    pub hostname: String,
    /// The worker's capability labels. `BTreeMap` so ordering is deterministic.
    pub labels: BTreeMap<String, String>,
    /// Jobs dispatched to this worker but not yet reported back.
    pub inflight: u32,
    /// Seconds since this worker was last heard from.
    ///
    /// A *relative* value, computed by the node that owns the record.
    /// `Instant` is monotonic and process-local: it has no defined epoch and
    /// two nodes' values are not comparable, so an absolute timestamp here
    /// would be meaningless once aggregated.
    pub last_seen_secs: u64,
    /// Whether the worker is past its heartbeat deadline.
    pub stale: bool,
    /// The control-plane node whose registry holds this worker.
    pub owning_node: String,
}

/// Project every worker in `registry` into a [`WorkerView`].
///
/// `inflight` is supplied as a closure rather than a handle so this stays a
/// pure function over borrowed state — the scheduler's in-flight counts live
/// behind a different lock, and taking that lock here would invert the
/// scheduler's lock order.
///
/// Output is sorted by worker id. `WorkerRegistry` iterates a `HashMap`, and
/// an unsorted read-model would present a different order on every call —
/// which this project has shipped twice already (#174, and the Phase 6
/// candidate ordering).
pub fn worker_views(
    registry: &WorkerRegistry,
    now: Instant,
    owning_node: &str,
    inflight: &dyn Fn(&WorkerId) -> usize,
) -> Vec<WorkerView> {
    let policy = *registry.policy();
    let mut views: Vec<WorkerView> = registry
        .iter()
        .map(|(id, record)| WorkerView {
            worker_id: id.as_str().to_string(),
            hostname: record.capabilities.hostname.clone(),
            labels: record.capabilities.labels.clone(),
            inflight: u32::try_from(inflight(id)).unwrap_or(u32::MAX),
            // `saturating_duration_since` so a `now` earlier than `last_seen`
            // reads as zero rather than panicking — the same posture
            // `WorkerRecord::is_stale` takes.
            last_seen_secs: now.saturating_duration_since(record.last_seen).as_secs(),
            stale: record.is_stale(now, &policy),
            owning_node: owning_node.to_string(),
        })
        .collect();
    views.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
    views
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use brokkr_common::WorkerId;

    use super::*;
    use crate::registry::{WorkerCapabilities, WorkerRegistry};

    fn wid(s: &str) -> WorkerId {
        WorkerId::new(s.to_string()).unwrap()
    }

    /// `Instant` is monotonic and process-local, so it cannot cross the wire
    /// and two nodes' values are not comparable. Liveness must be converted to
    /// "seconds ago" by the node that owns the record.
    #[test]
    fn liveness_crosses_as_seconds_ago_not_as_an_instant() {
        let mut reg = WorkerRegistry::default();
        let t0 = Instant::now();
        reg.register(
            wid("w-a"),
            WorkerCapabilities {
                hostname: "host-a".to_string(),
                labels: BTreeMap::from([("os".to_string(), "linux".to_string())]),
            },
            t0,
        );

        let views = worker_views(&reg, t0 + Duration::from_secs(7), "node-1", &|_| 3);

        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.worker_id, "w-a");
        assert_eq!(v.hostname, "host-a");
        assert_eq!(v.last_seen_secs, 7);
        assert_eq!(v.inflight, 3);
        assert_eq!(v.owning_node, "node-1");
        assert!(!v.stale);
    }

    /// Every DTO sourced from node-local state carries the node that owns it,
    /// so aggregation can never present it as a single cluster fact.
    #[test]
    fn every_worker_carries_its_owning_node() {
        let mut reg = WorkerRegistry::default();
        let t0 = Instant::now();
        for id in ["w-a", "w-b"] {
            reg.register(
                wid(id),
                WorkerCapabilities {
                    hostname: id.to_string(),
                    labels: BTreeMap::new(),
                },
                t0,
            );
        }
        let views = worker_views(&reg, t0, "node-2", &|_| 0);
        assert_eq!(views.len(), 2);
        assert!(views.iter().all(|v| v.owning_node == "node-2"));
    }

    /// Output order must not depend on `HashMap` iteration order. This project
    /// has shipped that bug twice (#174, and the Phase 6 candidate ordering).
    #[test]
    fn worker_views_are_sorted_by_id() {
        let mut reg = WorkerRegistry::default();
        let t0 = Instant::now();
        for id in ["w-zulu", "w-alpha", "w-mike"] {
            reg.register(
                wid(id),
                WorkerCapabilities {
                    hostname: id.to_string(),
                    labels: BTreeMap::new(),
                },
                t0,
            );
        }
        let views = worker_views(&reg, t0, "n", &|_| 0);
        let ids: Vec<&str> = views.iter().map(|v| v.worker_id.as_str()).collect();
        assert_eq!(ids, vec!["w-alpha", "w-mike", "w-zulu"]);
    }

    /// A `now` earlier than `last_seen` (clock skew in a caller-supplied
    /// instant) must read as zero, not panic — the same posture
    /// `WorkerRecord::is_stale` already takes.
    #[test]
    fn a_now_before_last_seen_reads_as_zero_seconds() {
        let mut reg = WorkerRegistry::default();
        let t0 = Instant::now() + Duration::from_secs(60);
        reg.register(
            wid("w-a"),
            WorkerCapabilities {
                hostname: "h".to_string(),
                labels: BTreeMap::new(),
            },
            t0,
        );
        let views = worker_views(&reg, t0 - Duration::from_secs(30), "n", &|_| 0);
        assert_eq!(views[0].last_seen_secs, 0);
    }

    #[test]
    fn a_worker_past_the_heartbeat_deadline_is_stale() {
        let mut reg = WorkerRegistry::default();
        let t0 = Instant::now();
        reg.register(
            wid("w-a"),
            WorkerCapabilities {
                hostname: "h".to_string(),
                labels: BTreeMap::new(),
            },
            t0,
        );
        let deadline = reg.policy().deadline();
        let views = worker_views(&reg, t0 + deadline + Duration::from_secs(1), "n", &|_| 0);
        assert!(views[0].stale);
    }
}
