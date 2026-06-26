//! Platform constraint matching (plan §16, task 2).
//!
//! Decides whether a worker's advertised capabilities satisfy an action's
//! required REAPI `Platform`. This is the eligibility primitive the scheduler
//! uses to pick a worker for a job.
//!
//! Kept separate from [`crate::registry`] on purpose: the registry stays
//! proto-free (a plain liveness/capability store), and the proto-aware
//! matching lives here — the same decoupling Phase 3 used between
//! `brokkr-cas::ring` and `brokkr-proto`.

use std::collections::BTreeMap;
use std::time::Instant;

use brokkr_common::WorkerId;
use brokkr_proto::reapi_v2 as rapi;

use crate::registry::{WorkerCapabilities, WorkerRecord, WorkerRegistry};

/// Whether `labels` satisfy every property in `platform`.
///
/// REAPI semantics: a worker satisfies a `Platform` iff, for every
/// `Property{name, value}` in the requirements, the worker advertises a
/// capability with that exact name and value. An empty platform is satisfied
/// by every worker.
///
/// Brokkr models worker capabilities as single-valued labels
/// (`BTreeMap<String, String>`), so a `name` that appears in the requirements
/// with two *different* values is unsatisfiable by any single worker — which
/// is the correct outcome for single-valued attributes like `os`/`arch`.
/// Multi-valued worker capabilities (a worker advertising several values for
/// one `name`, e.g. multiple supported ISAs) would need a richer capability
/// model; that is deferred until a workload needs it.
pub fn labels_satisfy_platform(
    labels: &BTreeMap<String, String>,
    platform: &rapi::Platform,
) -> bool {
    platform
        .properties
        .iter()
        .all(|p| labels.get(&p.name).is_some_and(|v| v == &p.value))
}

/// [`labels_satisfy_platform`] over a [`WorkerCapabilities`].
pub fn worker_satisfies(caps: &WorkerCapabilities, platform: &rapi::Platform) -> bool {
    labels_satisfy_platform(&caps.labels, platform)
}

/// Iterate the workers that are both live (not stale as of `now`) and satisfy
/// `platform`'s hard constraints — i.e. the candidates the scheduler may
/// dispatch this action to.
///
/// Soft / preferred constraints (plan §16's "soft") are not modelled yet:
/// REAPI's `Platform` has no soft notion, so expressing them needs a Brokkr
/// convention (a future ADR). This function is hard-constraint matching only.
pub fn eligible_workers<'a>(
    registry: &'a WorkerRegistry,
    now: Instant,
    platform: &'a rapi::Platform,
) -> impl Iterator<Item = (&'a WorkerId, &'a WorkerRecord)> {
    registry
        .healthy(now)
        .filter(move |(_, record)| worker_satisfies(&record.capabilities, platform))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn platform(pairs: &[(&str, &str)]) -> rapi::Platform {
        rapi::Platform {
            properties: pairs
                .iter()
                .map(|(name, value)| rapi::platform::Property {
                    name: name.to_string(),
                    value: value.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn empty_platform_is_satisfied_by_any_worker() {
        assert!(labels_satisfy_platform(&labels(&[]), &platform(&[])));
        assert!(labels_satisfy_platform(
            &labels(&[("os", "linux")]),
            &platform(&[])
        ));
    }

    #[test]
    fn single_property_match_and_mismatch() {
        let w = labels(&[("os", "linux"), ("arch", "x86_64")]);
        assert!(labels_satisfy_platform(&w, &platform(&[("os", "linux")])));
        assert!(!labels_satisfy_platform(
            &w,
            &platform(&[("os", "windows")])
        ));
    }

    #[test]
    fn missing_property_name_is_not_satisfied() {
        let w = labels(&[("os", "linux")]);
        assert!(!labels_satisfy_platform(&w, &platform(&[("gpu", "a100")])));
    }

    #[test]
    fn all_properties_must_match() {
        let w = labels(&[("os", "linux"), ("arch", "x86_64")]);
        assert!(labels_satisfy_platform(
            &w,
            &platform(&[("os", "linux"), ("arch", "x86_64")])
        ));
        // One satisfied, one not → overall unsatisfied.
        assert!(!labels_satisfy_platform(
            &w,
            &platform(&[("os", "linux"), ("arch", "aarch64")])
        ));
    }

    #[test]
    fn same_name_two_values_is_unsatisfiable_for_single_valued_labels() {
        let w = labels(&[("os", "linux")]);
        assert!(!labels_satisfy_platform(
            &w,
            &platform(&[("os", "linux"), ("os", "windows")])
        ));
    }

    #[test]
    fn eligible_workers_filters_by_health_and_constraints() {
        use std::time::Duration;

        use crate::registry::{HeartbeatPolicy, WorkerRegistry};

        let t0 = Instant::now();
        let policy = HeartbeatPolicy {
            interval: Duration::from_secs(1),
            max_missed: 3, // 3s deadline
        };
        let mut reg = WorkerRegistry::new(policy);

        let linux = WorkerCapabilities {
            hostname: "linux-box".to_string(),
            labels: labels(&[("os", "linux")]),
        };
        let windows = WorkerCapabilities {
            hostname: "win-box".to_string(),
            labels: labels(&[("os", "windows")]),
        };
        reg.register(WorkerId::new("w-linux".to_string()).unwrap(), linux, t0);
        reg.register(
            WorkerId::new("w-win".to_string()).unwrap(),
            windows.clone(),
            t0,
        );
        // A second linux worker that will go stale.
        reg.register(
            WorkerId::new("w-linux-stale".to_string()).unwrap(),
            WorkerCapabilities {
                hostname: "stale".to_string(),
                labels: labels(&[("os", "linux")]),
            },
            t0,
        );

        // At t0+5s the stale worker is past the 3s deadline; keep the other two
        // alive with a heartbeat.
        let now = t0 + Duration::from_secs(5);
        reg.record_heartbeat(&WorkerId::new("w-linux".to_string()).unwrap(), now)
            .unwrap();
        reg.record_heartbeat(&WorkerId::new("w-win".to_string()).unwrap(), now)
            .unwrap();

        let want_linux = platform(&[("os", "linux")]);
        let mut eligible: Vec<&str> = eligible_workers(&reg, now, &want_linux)
            .map(|(id, _)| id.as_str())
            .collect();
        eligible.sort_unstable();
        // Only the live linux worker: the windows worker fails the constraint,
        // the stale linux worker fails the health check.
        assert_eq!(eligible, vec!["w-linux"]);
    }
}
