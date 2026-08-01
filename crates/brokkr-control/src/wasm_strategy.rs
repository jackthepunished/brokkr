//! [`Strategy`] backed by an operator-supplied WebAssembly module (ADR 0014).
//!
//! This is the adapter between two things that deliberately do not know about
//! each other: `brokkr-policy` owns the runtime and knows nothing of workers or
//! scheduling traits, and `scheduling::Strategy` knows nothing of WebAssembly.
//! Putting the trait in `brokkr-policy` would have required it to depend on
//! this crate, which depends on it — a cycle, and the crate graph is a DAG.
//!
//! # The failure posture, in one place
//!
//! Every way a guest can fail — trapping, running out of fuel, blowing the
//! wall-clock deadline, returning an index that isn't a candidate, or being
//! quarantined — ends the same way: a `warn!` naming the reason, a counter
//! increment, and **the built-in strategy's answer for that one placement**.
//!
//! A broken policy must not become a broken cluster. This is the same
//! reasoning as Phase 5's decision D1 about best-effort action-cache writes,
//! and the counter plus the log line are what keep it out of the "silent
//! degradation" category.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use brokkr_common::WorkerId;
use brokkr_policy::{Decision, PolicyEngine, PolicyError, PolicyFailure};
use prost::Message as _;

use crate::policy_abi::build_snapshot;
use crate::scheduling::{DecisionContext, SimpleFifo, Strategy};
use crate::worker_service::SharedWorkerRegistry;

/// Per-reason decision-failure counts, exposed for observability.
///
/// One counter per [`PolicyFailure::reason`] tag rather than a single total,
/// because the reasons need different fixes: "your policy is too slow" and
/// "your policy returns garbage indices" are not the same incident.
#[derive(Debug, Default)]
pub struct PolicyFailureCounts {
    trap: AtomicU64,
    fuel_exhausted: AtomicU64,
    deadline: AtomicU64,
    bad_index: AtomicU64,
    instantiate: AtomicU64,
    memory: AtomicU64,
    not_loaded: AtomicU64,
    quarantined: AtomicU64,
}

impl PolicyFailureCounts {
    fn record(&self, failure: &PolicyFailure) {
        let counter = match failure {
            PolicyFailure::Trap(_) => &self.trap,
            PolicyFailure::FuelExhausted => &self.fuel_exhausted,
            PolicyFailure::Deadline => &self.deadline,
            PolicyFailure::BadIndex { .. } => &self.bad_index,
            PolicyFailure::Instantiate(_) => &self.instantiate,
            PolicyFailure::Memory(_) => &self.memory,
            PolicyFailure::NotLoaded => &self.not_loaded,
            PolicyFailure::Quarantined { .. } => &self.quarantined,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Total decision failures across every reason.
    pub fn total(&self) -> u64 {
        [
            &self.trap,
            &self.fuel_exhausted,
            &self.deadline,
            &self.bad_index,
            &self.instantiate,
            &self.memory,
            &self.not_loaded,
            &self.quarantined,
        ]
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .sum()
    }

    /// Failures for one reason tag, as returned by [`PolicyFailure::reason`].
    /// Unknown tags read as `0`.
    pub fn for_reason(&self, reason: &str) -> u64 {
        let counter = match reason {
            "trap" => &self.trap,
            "fuel_exhausted" => &self.fuel_exhausted,
            "deadline" => &self.deadline,
            "bad_index" => &self.bad_index,
            "instantiate" => &self.instantiate,
            "memory" => &self.memory,
            "not_loaded" => &self.not_loaded,
            "quarantined" => &self.quarantined,
            _ => return 0,
        };
        counter.load(Ordering::Relaxed)
    }
}

/// A [`Strategy`] that asks a WebAssembly policy, falling back to a built-in.
pub struct WasmStrategy {
    /// The engine, behind an `RwLock` so hot reload (a write) can swap the
    /// module without quiescing dispatch (a read).
    ///
    /// `RwLock` rather than `arc-swap` because writes happen at most once per
    /// reload poll — read contention is nil, and it is one fewer dependency.
    engine: Arc<RwLock<PolicyEngine>>,
    /// The answer used whenever the policy declines or fails.
    fallback: SimpleFifo,
    /// Supplies candidate capability labels for the snapshot.
    registry: Option<SharedWorkerRegistry>,
    counts: Arc<PolicyFailureCounts>,
    /// Decisions the guest actually decided (neither declined nor failed).
    decided: AtomicU64,
    /// Decisions where the guest declined with `DECLINE`.
    declined: AtomicU64,
}

impl std::fmt::Debug for WasmStrategy {
    // `PolicyEngine` is `Debug` but is behind a lock this must not block on.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmStrategy")
            .field("decided", &self.decided.load(Ordering::Relaxed))
            .field("declined", &self.declined.load(Ordering::Relaxed))
            .field("failures", &self.counts.total())
            .finish_non_exhaustive()
    }
}

impl WasmStrategy {
    /// Wrap an engine.
    ///
    /// `registry` is optional: without it candidates carry no capability
    /// labels, which is honest rather than an error.
    pub fn new(engine: PolicyEngine, registry: Option<SharedWorkerRegistry>) -> Self {
        Self {
            engine: Arc::new(RwLock::new(engine)),
            fallback: SimpleFifo,
            registry,
            counts: Arc::new(PolicyFailureCounts::default()),
            decided: AtomicU64::new(0),
            declined: AtomicU64::new(0),
        }
    }

    /// A handle on the engine, for the reload path (P7) to swap a module.
    pub fn engine(&self) -> Arc<RwLock<PolicyEngine>> {
        Arc::clone(&self.engine)
    }

    /// Per-reason decision-failure counts.
    pub fn failure_counts(&self) -> Arc<PolicyFailureCounts> {
        Arc::clone(&self.counts)
    }

    /// Decisions the guest actually made.
    pub fn decided(&self) -> u64 {
        self.decided.load(Ordering::Relaxed)
    }

    /// Decisions the guest declined, yielding to the built-in.
    pub fn declined(&self) -> u64 {
        self.declined.load(Ordering::Relaxed)
    }

    /// Install a new module, replacing any current one.
    ///
    /// On failure the running policy is untouched — the engine validates
    /// before it swaps.
    pub fn load(&self, wasm: &[u8]) -> Result<(), PolicyError> {
        let mut guard = self
            .engine
            .write()
            .map_err(|_| PolicyError::Engine("policy engine lock poisoned".to_string()))?;
        guard.load(wasm)
    }

    /// Record a failure and log it, then hand back the built-in's answer.
    fn fall_back(
        &self,
        failure: &PolicyFailure,
        candidates: &[WorkerId],
        ctx: &DecisionContext<'_>,
    ) -> Option<WorkerId> {
        self.counts.record(failure);
        tracing::warn!(
            reason = failure.reason(),
            detail = %failure,
            candidates = candidates.len(),
            "scheduling policy failed; falling back to the built-in strategy for this placement"
        );
        self.fallback.choose(candidates, ctx.loads)
    }
}

impl Strategy for WasmStrategy {
    fn choose(
        &self,
        candidates: &[WorkerId],
        loads: &dyn crate::scheduling::LoadView,
    ) -> Option<WorkerId> {
        // Reached only if something calls the narrow entry point directly. A
        // WASM policy needs the full context, so without it there is nothing to
        // ask and the built-in answer is the honest one.
        self.fallback.choose(candidates, loads)
    }

    fn choose_with(&self, candidates: &[WorkerId], ctx: &DecisionContext<'_>) -> Option<WorkerId> {
        // The `None`-iff-empty contract, first and unconditionally: whatever a
        // guest does, an empty candidate set yields `None` and a non-empty one
        // yields `Some`.
        if candidates.is_empty() {
            return None;
        }

        // A poisoned lock means a previous holder panicked. Rather than
        // propagate that into dispatch, treat it as one more reason to use the
        // built-in.
        let Ok(engine) = self.engine.read() else {
            return self.fall_back(
                &PolicyFailure::Instantiate("policy engine lock poisoned".to_string()),
                candidates,
                ctx,
            );
        };

        // Building the snapshot needs the registry for capability labels.
        // `try_lock` deliberately: this runs under the scheduler's dispatch
        // mutex, and blocking on a second lock here is how deadlocks are made.
        // Failing to get it costs labels, not the decision.
        let registry_guard = self.registry.as_ref().and_then(|r| r.try_lock().ok());
        let snapshot = build_snapshot(candidates, ctx, registry_guard.as_deref());
        drop(registry_guard);

        let encoded = snapshot.encode_to_vec();
        match engine.decide(&encoded, candidates.len()) {
            Ok(Decision::Chose(idx)) => {
                self.decided.fetch_add(1, Ordering::Relaxed);
                // `interpret` already bounds the index, so this cannot be out
                // of range; `get` rather than indexing so a future change to
                // that contract degrades instead of panicking on the hot path.
                match candidates.get(idx) {
                    Some(w) => Some(w.clone()),
                    None => self.fall_back(
                        &PolicyFailure::BadIndex {
                            returned: idx as i32,
                            candidates: candidates.len(),
                        },
                        candidates,
                        ctx,
                    ),
                }
            }
            Ok(Decision::Declined) => {
                self.declined.fetch_add(1, Ordering::Relaxed);
                // Not a failure: the policy said "no preference". No warn, no
                // counter, no quarantine pressure.
                self.fallback.choose(candidates, ctx.loads)
            }
            Err(failure) => self.fall_back(&failure, candidates, ctx),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::HashMap;

    use brokkr_common::{Digest, TenantId};
    use brokkr_policy::{PolicyLimits, POLICY_ABI_VERSION};
    use brokkr_proto::reapi_v2 as rapi;

    use super::*;
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

    /// A policy whose `brokkr_choose` body is `body`.
    fn wat(body: &str) -> String {
        format!(
            r#"(module
  (memory (export "memory") 4)
  (global $bump (mut i32) (i32.const 1024))
  (func (export "brokkr_abi_version") (result i32) i32.const {POLICY_ABI_VERSION})
  (func (export "brokkr_alloc") (param $len i32) (result i32)
    (local $p i32)
    global.get $bump
    local.set $p
    global.get $bump
    local.get $len
    i32.add
    global.set $bump
    local.get $p)
  (func (export "brokkr_choose") (param $ptr i32) (param $len i32) (result i32)
    {body})
)"#
        )
    }

    fn strategy(body: &str) -> WasmStrategy {
        let mut engine = PolicyEngine::new(PolicyLimits::default()).unwrap();
        engine.load(wat(body).as_bytes()).unwrap();
        WasmStrategy::new(engine, None)
    }

    /// Fixtures distinguish a real decision from the engine's load-time smoke
    /// snapshot by payload length alone, since WAT cannot decode protobuf.
    /// Test snapshots are padded well past this; the smoke snapshot is ~165
    /// bytes and cannot reach it.
    const BIG: usize = 512;

    /// Runs `f` with a `DecisionContext` over `loads`.
    ///
    /// The platform carries a padding property so the encoded snapshot clears
    /// [`BIG`]. Platform properties are free-form and unvalidated, which makes
    /// them the honest place to pad — unlike the tenant or a digest, which
    /// have real shapes a reader would be entitled to trust.
    fn with_ctx<R>(loads: &dyn LoadView, f: impl FnOnce(&DecisionContext<'_>) -> R) -> R {
        let (action, root) = (Digest::of(b"action"), Digest::of(b"root"));
        let tenant = TenantId::default();
        let plat = rapi::Platform {
            properties: vec![rapi::platform::Property {
                name: "brokkr-test-padding".to_string(),
                value: "x".repeat(BIG),
            }],
        };
        let ctx = DecisionContext {
            loads,
            locality: &NoLocality,
            job: JobFacts {
                tenant: &tenant,
                action_digest: &action,
                input_root_digest: Some(&root),
                platform: &plat,
            },
        };
        f(&ctx)
    }

    /// The policy really decides — it picks a worker no built-in would.
    /// `SimpleFifo` always takes the least-loaded; this policy takes index 2,
    /// which is deliberately the *most* loaded.
    #[test]
    fn the_policy_decides_and_can_beat_the_builtin() {
        // Index 2 only on a real snapshot: the engine's smoke snapshot has
        // just two candidates, so an unconditional `2` would fail validation.
        let s = strategy(&format!(
            r#"(if (result i32) (i32.ge_s (local.get $len) (i32.const {BIG}))
      (then i32.const 2)
      (else i32.const 0))"#
        ));
        let cands = vec![wid("a"), wid("b"), wid("c")];
        let loads = MapLoads(HashMap::from([(wid("a"), 0), (wid("b"), 1), (wid("c"), 9)]));

        with_ctx(&loads, |ctx| {
            assert_eq!(
                SimpleFifo.choose(&cands, ctx.loads),
                Some(wid("a")),
                "the built-in would pick the idle worker"
            );
            assert_eq!(
                s.choose_with(&cands, ctx),
                Some(wid("c")),
                "the policy's answer must win"
            );
        });
        assert_eq!(s.decided(), 1);
        assert_eq!(s.failure_counts().total(), 0);
    }

    #[test]
    fn declining_yields_the_builtin_and_is_not_a_failure() {
        let s = strategy("i32.const -1");
        let cands = vec![wid("busy"), wid("idle")];
        let loads = MapLoads(HashMap::from([(wid("busy"), 5), (wid("idle"), 0)]));

        with_ctx(&loads, |ctx| {
            assert_eq!(s.choose_with(&cands, ctx), Some(wid("idle")));
        });
        assert_eq!(s.declined(), 1);
        assert_eq!(s.decided(), 0);
        assert_eq!(
            s.failure_counts().total(),
            0,
            "a decline must not be counted as a failure"
        );
    }

    /// A trapping policy must not fail the placement, and must be counted
    /// under its own reason.
    #[test]
    fn a_trapping_policy_falls_back_and_is_counted() {
        // Misbehave only on a real (large) snapshot, so the module still
        // passes the engine's load-time smoke decision.
        let s = strategy(&format!(
            r#"(if (result i32) (i32.ge_s (local.get $len) (i32.const {BIG}))
      (then unreachable)
      (else i32.const 0))"#
        ));
        let cands = vec![wid("busy"), wid("idle")];
        let loads = MapLoads(HashMap::from([(wid("busy"), 5), (wid("idle"), 0)]));

        with_ctx(&loads, |ctx| {
            assert_eq!(
                s.choose_with(&cands, ctx),
                Some(wid("idle")),
                "a trap must degrade to the built-in, not fail the placement"
            );
        });
        assert_eq!(s.failure_counts().for_reason("trap"), 1);
        assert_eq!(s.failure_counts().total(), 1);
    }

    #[test]
    fn an_out_of_range_index_falls_back_and_is_counted_as_bad_index() {
        let s = strategy(&format!(
            r#"(if (result i32) (i32.ge_s (local.get $len) (i32.const {BIG}))
      (then i32.const 987)
      (else i32.const 0))"#
        ));
        let cands = vec![wid("busy"), wid("idle")];
        let loads = MapLoads(HashMap::from([(wid("busy"), 5), (wid("idle"), 0)]));

        with_ctx(&loads, |ctx| {
            assert_eq!(s.choose_with(&cands, ctx), Some(wid("idle")));
        });
        assert_eq!(s.failure_counts().for_reason("bad_index"), 1);
    }

    #[test]
    fn an_engine_with_no_module_falls_back_and_counts_not_loaded() {
        let engine = PolicyEngine::new(PolicyLimits::default()).unwrap();
        let s = WasmStrategy::new(engine, None);
        let cands = vec![wid("busy"), wid("idle")];
        let loads = MapLoads(HashMap::from([(wid("busy"), 5), (wid("idle"), 0)]));

        with_ctx(&loads, |ctx| {
            assert_eq!(s.choose_with(&cands, ctx), Some(wid("idle")));
        });
        assert_eq!(s.failure_counts().for_reason("not_loaded"), 1);
    }

    /// The contract every `Strategy` is held to, now including this one.
    /// Checked for a working policy, a declining one, and a trapping one,
    /// because a guest must never be able to turn a placeable job into a
    /// stalled one.
    #[test]
    fn choose_with_returns_none_iff_candidates_is_empty() {
        let trapping = format!(
            r#"(if (result i32) (i32.ge_s (local.get $len) (i32.const {BIG}))
      (then unreachable)
      (else i32.const 0))"#
        );
        let bodies = ["i32.const 0", "i32.const -1", &trapping];
        let loads = MapLoads(HashMap::new());
        for body in bodies {
            let s = strategy(body);
            with_ctx(&loads, |ctx| {
                assert!(
                    s.choose_with(&[], ctx).is_none(),
                    "empty candidates must yield None ({body})"
                );
                assert!(
                    s.choose_with(&[wid("only")], ctx).is_some(),
                    "a non-empty candidate set must always yield Some ({body})"
                );
            });
        }
    }

    /// Once quarantined the guest stops being called entirely, but placement
    /// keeps working — that is the whole point of the quarantine.
    #[test]
    fn a_quarantined_policy_still_places_every_job() {
        let s = strategy(&format!(
            r#"(if (result i32) (i32.ge_s (local.get $len) (i32.const {BIG}))
      (then i32.const 987)
      (else i32.const 0))"#
        ));
        let cands = vec![wid("busy"), wid("idle")];
        let loads = MapLoads(HashMap::from([(wid("busy"), 5), (wid("idle"), 0)]));

        with_ctx(&loads, |ctx| {
            for _ in 0..(PolicyLimits::default().quarantine_threshold + 5) {
                assert_eq!(
                    s.choose_with(&cands, ctx),
                    Some(wid("idle")),
                    "every placement must succeed, quarantined or not"
                );
            }
        });
        let counts = s.failure_counts();
        assert_eq!(
            counts.for_reason("bad_index"),
            u64::from(PolicyLimits::default().quarantine_threshold)
        );
        assert!(
            counts.for_reason("quarantined") > 0,
            "later calls must be counted as quarantined, not as bad_index"
        );
    }

    #[test]
    fn failure_counts_are_separated_by_reason() {
        let counts = PolicyFailureCounts::default();
        counts.record(&PolicyFailure::Trap("x".into()));
        counts.record(&PolicyFailure::Trap("y".into()));
        counts.record(&PolicyFailure::Deadline);
        assert_eq!(counts.for_reason("trap"), 2);
        assert_eq!(counts.for_reason("deadline"), 1);
        assert_eq!(counts.for_reason("fuel_exhausted"), 0);
        assert_eq!(counts.for_reason("nonsense"), 0);
        assert_eq!(counts.total(), 3);
    }
}
