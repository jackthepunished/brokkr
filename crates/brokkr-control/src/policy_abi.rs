//! Host side of the WASM scheduling-policy ABI (ADR 0014).
//!
//! Turns a [`DecisionContext`] plus a candidate list into the
//! `brokkr.v1.DecisionSnapshot` a guest module reads. This is deliberately a
//! pure function over borrowed inputs — no runtime, no wasmtime, no I/O — so
//! the encoding can be tested exhaustively before any engine exists, and so a
//! future non-WASM consumer of the same snapshot costs nothing.
//!
//! The guest contract itself lives in `brokkr/v1/policy.proto`; this module is
//! only the projection.

use brokkr_common::WorkerId;
use brokkr_proto::brokkr_v1 as bv1;

use crate::registry::WorkerRegistry;
use crate::scheduling::DecisionContext;

/// ABI version a guest must agree with.
///
/// Bump **only** for a change a module compiled against the previous version
/// could not survive. Adding a field to `DecisionSnapshot` does not qualify —
/// protobuf field numbers already make that safe, which is most of why the ABI
/// is protobuf at all. Renaming or repurposing a field, or changing what an
/// existing field means, does.
pub const POLICY_ABI_VERSION: u32 = 1;

/// The candidate index a guest returns to mean "no preference — use the
/// built-in policy for this decision".
///
/// Distinct from a failure: declining is a legitimate answer for a policy that
/// does not recognise the job, and does not count against the failure counter
/// or the quarantine threshold. Any *other* out-of-range value is a failure.
pub const DECLINE: i32 = -1;

/// Build the snapshot for one placement decision.
///
/// `candidates` are already filtered to workers that are connected, idle, and
/// satisfy the action's platform constraints — the policy only decides *which*
/// of them gets the job, and the order here is the index space the guest
/// returns into.
///
/// `registry` supplies capability labels. It is optional because the scheduler
/// itself runs without one in fixtures and single-worker setups; a policy then
/// sees candidates with no labels, which is honest rather than an error.
pub fn build_snapshot(
    candidates: &[WorkerId],
    ctx: &DecisionContext<'_>,
    registry: Option<&WorkerRegistry>,
) -> bv1::DecisionSnapshot {
    let input_root_hex = ctx
        .job
        .input_root_digest
        .map(|d| d.hash().to_string())
        .unwrap_or_default();

    bv1::DecisionSnapshot {
        abi_version: POLICY_ABI_VERSION,
        job: Some(bv1::PolicyJobFacts {
            tenant: ctx.job.tenant.as_str().to_string(),
            action_digest: ctx.job.action_digest.hash().to_string(),
            input_root_digest: input_root_hex,
            platform: ctx
                .job
                .platform
                .properties
                .iter()
                .map(|p| bv1::PolicyPlatformProperty {
                    name: p.name.clone(),
                    value: p.value.clone(),
                })
                .collect(),
        }),
        candidates: candidates
            .iter()
            .map(|w| bv1::PolicyCandidate {
                worker_id: w.as_str().to_string(),
                // Saturating rather than wrapping: an in-flight count above
                // u32::MAX is impossible, but a wrapped one would read as
                // *idle* and invert the policy's preference.
                inflight: u32::try_from(ctx.loads.inflight(w)).unwrap_or(u32::MAX),
                // `WorkerCapabilities::labels` is a `BTreeMap`, chosen in the
                // registry precisely so iteration is deterministic — so this
                // encodes to identical bytes for identical state, which the
                // engine's determinism tests depend on.
                labels: registry
                    .and_then(|r| r.get(w))
                    .map(|rec| {
                        rec.capabilities
                            .labels
                            .iter()
                            .map(|(name, value)| bv1::PolicyPlatformProperty {
                                name: name.clone(),
                                value: value.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                input_root_hits: ctx
                    .job
                    .input_root_digest
                    .map_or(0, |root| ctx.locality.input_root_hits(w, root)),
                action_hits: ctx.locality.action_hits(w, ctx.job.action_digest),
            })
            .collect(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::HashMap;

    use brokkr_common::{Digest, TenantId};
    use brokkr_proto::reapi_v2 as rapi;
    use prost::Message as _;

    use super::*;
    use crate::locality::LocalityIndex;
    use crate::scheduling::{JobFacts, LoadView, NoLocality};

    fn wid(s: &str) -> WorkerId {
        WorkerId::new(s.to_string()).unwrap()
    }

    struct MapLoads(HashMap<WorkerId, usize>);
    impl LoadView for MapLoads {
        fn inflight(&self, worker: &WorkerId) -> usize {
            self.0.get(worker).copied().unwrap_or(0)
        }
    }

    fn platform(pairs: &[(&str, &str)]) -> rapi::Platform {
        rapi::Platform {
            properties: pairs
                .iter()
                .map(|(n, v)| rapi::platform::Property {
                    name: n.to_string(),
                    value: v.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_snapshot_carries_the_job_facts_and_every_candidate() {
        let (action, root) = (Digest::of(b"action"), Digest::of(b"root"));
        let tenant = TenantId::new("acme".to_string()).unwrap();
        let plat = platform(&[("os", "linux"), ("arch", "x86_64")]);
        let loads = MapLoads(HashMap::from([(wid("a"), 3), (wid("b"), 0)]));
        let ctx = DecisionContext {
            loads: &loads,
            locality: &NoLocality,
            job: JobFacts {
                tenant: &tenant,
                action_digest: &action,
                input_root_digest: Some(&root),
                platform: &plat,
            },
        };

        let snap = build_snapshot(&[wid("a"), wid("b")], &ctx, None);

        assert_eq!(snap.abi_version, POLICY_ABI_VERSION);
        let job = snap.job.unwrap();
        assert_eq!(job.tenant, "acme");
        assert_eq!(job.action_digest, action.hash());
        assert_eq!(job.input_root_digest, root.hash());
        assert_eq!(job.platform.len(), 2);

        assert_eq!(snap.candidates.len(), 2);
        assert_eq!(snap.candidates[0].worker_id, "a");
        assert_eq!(snap.candidates[0].inflight, 3);
        assert_eq!(snap.candidates[1].worker_id, "b");
        assert_eq!(snap.candidates[1].inflight, 0);
    }

    /// Candidate order *is* the index space the guest returns into, so it must
    /// be preserved verbatim. A reordering here would silently place jobs on
    /// the wrong workers.
    #[test]
    fn candidate_order_is_preserved_because_it_is_the_index_space() {
        let (action, root) = (Digest::of(b"a"), Digest::of(b"r"));
        let tenant = TenantId::default();
        let plat = rapi::Platform::default();
        let loads = MapLoads(HashMap::new());
        let ctx = DecisionContext {
            loads: &loads,
            locality: &NoLocality,
            job: JobFacts {
                tenant: &tenant,
                action_digest: &action,
                input_root_digest: Some(&root),
                platform: &plat,
            },
        };
        let order = vec![wid("zulu"), wid("alpha"), wid("mike")];
        let snap = build_snapshot(&order, &ctx, None);
        let got: Vec<&str> = snap
            .candidates
            .iter()
            .map(|c| c.worker_id.as_str())
            .collect();
        assert_eq!(got, vec!["zulu", "alpha", "mike"]);
    }

    #[test]
    fn an_action_with_no_input_root_encodes_an_empty_string_and_zero_hits() {
        let action = Digest::of(b"action");
        let tenant = TenantId::default();
        let plat = rapi::Platform::default();
        let loads = MapLoads(HashMap::new());
        let mut idx = LocalityIndex::default();
        // Warm history that would produce hits if an input root were present.
        idx.record(&wid("a"), &action, Some(&Digest::of(b"root")));
        let ctx = DecisionContext {
            loads: &loads,
            locality: &idx,
            job: JobFacts {
                tenant: &tenant,
                action_digest: &action,
                input_root_digest: None,
                platform: &plat,
            },
        };
        let snap = build_snapshot(&[wid("a")], &ctx, None);
        assert_eq!(snap.job.unwrap().input_root_digest, "");
        assert_eq!(snap.candidates[0].input_root_hits, 0);
        // The action hit is still real and must survive.
        assert_eq!(snap.candidates[0].action_hits, 1);
    }

    #[test]
    fn locality_counters_come_from_the_index() {
        let (action, root) = (Digest::of(b"action"), Digest::of(b"root"));
        let tenant = TenantId::default();
        let plat = rapi::Platform::default();
        let loads = MapLoads(HashMap::new());
        let mut idx = LocalityIndex::default();
        idx.record(&wid("warm"), &action, Some(&root));
        idx.record(&wid("warm"), &Digest::of(b"other"), Some(&root));
        let ctx = DecisionContext {
            loads: &loads,
            locality: &idx,
            job: JobFacts {
                tenant: &tenant,
                action_digest: &action,
                input_root_digest: Some(&root),
                platform: &plat,
            },
        };
        let snap = build_snapshot(&[wid("warm"), wid("cold")], &ctx, None);
        assert_eq!(snap.candidates[0].input_root_hits, 2);
        assert_eq!(snap.candidates[0].action_hits, 1);
        assert_eq!(snap.candidates[1].input_root_hits, 0);
        assert_eq!(snap.candidates[1].action_hits, 0);
    }

    /// The whole reason the ABI is protobuf: a snapshot the host built must
    /// decode, field for field, to what it was built from.
    #[test]
    fn a_snapshot_round_trips_through_prost() {
        let (action, root) = (Digest::of(b"action"), Digest::of(b"root"));
        let tenant = TenantId::new("acme".to_string()).unwrap();
        let plat = platform(&[("os", "linux")]);
        let loads = MapLoads(HashMap::from([(wid("a"), 7)]));
        let mut idx = LocalityIndex::default();
        idx.record(&wid("a"), &action, Some(&root));
        let ctx = DecisionContext {
            loads: &loads,
            locality: &idx,
            job: JobFacts {
                tenant: &tenant,
                action_digest: &action,
                input_root_digest: Some(&root),
                platform: &plat,
            },
        };
        let snap = build_snapshot(&[wid("a")], &ctx, None);

        let bytes = snap.encode_to_vec();
        let back = bv1::DecisionSnapshot::decode(bytes.as_slice()).unwrap();
        assert_eq!(back, snap);

        // And spot-check through the decoded copy, not just by equality, so a
        // symmetric encode/decode bug can't hide.
        let job = back.job.unwrap();
        assert_eq!(job.tenant, "acme");
        assert_eq!(job.action_digest, action.hash());
        assert_eq!(job.input_root_digest, root.hash());
        assert_eq!(back.candidates[0].inflight, 7);
        assert_eq!(back.candidates[0].input_root_hits, 1);
        assert_eq!(back.candidates[0].action_hits, 1);
    }

    /// The same inputs must encode to the same bytes every time. A guest may
    /// hash or memoize on the snapshot, and more importantly the determinism
    /// tests for the engine itself rest on this.
    #[test]
    fn encoding_is_deterministic() {
        let (action, root) = (Digest::of(b"action"), Digest::of(b"root"));
        let tenant = TenantId::default();
        let plat = platform(&[("os", "linux"), ("arch", "x86_64"), ("gpu", "none")]);
        let loads = MapLoads(HashMap::from([(wid("a"), 1), (wid("b"), 2)]));
        let mut idx = LocalityIndex::default();
        idx.record(&wid("a"), &action, Some(&root));

        let encode = || {
            let ctx = DecisionContext {
                loads: &loads,
                locality: &idx,
                job: JobFacts {
                    tenant: &tenant,
                    action_digest: &action,
                    input_root_digest: Some(&root),
                    platform: &plat,
                },
            };
            build_snapshot(&[wid("a"), wid("b")], &ctx, None).encode_to_vec()
        };
        let first = encode();
        for _ in 0..50 {
            assert_eq!(encode(), first, "snapshot encoding must be deterministic");
        }
    }

    /// Labels must encode in a stable order, or the snapshot bytes would
    /// differ run to run for identical state. The registry stores them in a
    /// `BTreeMap` for exactly this reason; this pins that we still rely on it.
    #[test]
    fn candidate_labels_are_sorted_and_therefore_deterministic() {
        use crate::registry::{WorkerCapabilities, WorkerRegistry};
        use std::time::Instant;

        let mut reg = WorkerRegistry::default();
        reg.register(
            wid("a"),
            WorkerCapabilities {
                hostname: "h".to_string(),
                labels: std::collections::BTreeMap::from([
                    ("os".to_string(), "linux".to_string()),
                    ("arch".to_string(), "x86_64".to_string()),
                    ("zone".to_string(), "b".to_string()),
                ]),
            },
            Instant::now(),
        );

        let (action, root) = (Digest::of(b"a"), Digest::of(b"r"));
        let tenant = TenantId::default();
        let plat = rapi::Platform::default();
        let loads = MapLoads(HashMap::new());

        let encode = || {
            let ctx = DecisionContext {
                loads: &loads,
                locality: &NoLocality,
                job: JobFacts {
                    tenant: &tenant,
                    action_digest: &action,
                    input_root_digest: Some(&root),
                    platform: &plat,
                },
            };
            build_snapshot(&[wid("a")], &ctx, Some(&reg))
        };

        let snap = encode();
        let names: Vec<&str> = snap.candidates[0]
            .labels
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(names, vec!["arch", "os", "zone"], "labels must be sorted");

        let first = snap.encode_to_vec();
        for _ in 0..50 {
            assert_eq!(encode().encode_to_vec(), first);
        }
    }

    #[test]
    fn without_a_registry_candidates_carry_no_labels_rather_than_failing() {
        let (action, root) = (Digest::of(b"a"), Digest::of(b"r"));
        let tenant = TenantId::default();
        let plat = rapi::Platform::default();
        let loads = MapLoads(HashMap::new());
        let ctx = DecisionContext {
            loads: &loads,
            locality: &NoLocality,
            job: JobFacts {
                tenant: &tenant,
                action_digest: &action,
                input_root_digest: Some(&root),
                platform: &plat,
            },
        };
        let snap = build_snapshot(&[wid("a")], &ctx, None);
        assert!(snap.candidates[0].labels.is_empty());
    }

    /// An in-flight count above `u32::MAX` is impossible in practice, but a
    /// *wrapped* one would read as idle and invert the policy's preference —
    /// the worst possible failure mode for a saturated worker.
    #[test]
    fn an_absurd_inflight_count_saturates_rather_than_wrapping() {
        struct Absurd;
        impl LoadView for Absurd {
            fn inflight(&self, _w: &WorkerId) -> usize {
                usize::MAX
            }
        }
        let (action, root) = (Digest::of(b"a"), Digest::of(b"r"));
        let tenant = TenantId::default();
        let plat = rapi::Platform::default();
        let ctx = DecisionContext {
            loads: &Absurd,
            locality: &NoLocality,
            job: JobFacts {
                tenant: &tenant,
                action_digest: &action,
                input_root_digest: Some(&root),
                platform: &plat,
            },
        };
        let snap = build_snapshot(&[wid("a")], &ctx, None);
        assert_eq!(snap.candidates[0].inflight, u32::MAX);
    }
}
