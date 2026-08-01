//! The `LocalityAware` example policy, run against the real host.
//!
//! This is Phase 6 definition-of-done line 4 (`docs/phase-6-plan.md`): the
//! policy ADR 0008 promised, delivered as a module, demonstrably preferring a
//! worker that recently ran the same input root.
//!
//! Every test here is `#[ignore]`d, because it needs an artifact this
//! repository deliberately does not commit (CLAUDE.md rule 4) and a target CI
//! does not install. They are **not** silently skipped: if the artifact is
//! missing the test fails with the exact command to build it, so running the
//! suite and seeing "ignored" is the only way to not run them.
//!
//! ```sh
//! rustup target add wasm32-unknown-unknown
//! cd examples/policies/locality
//! cargo build --release --target wasm32-unknown-unknown
//! cargo test -p brokkr-control --test locality_example_policy -- --ignored
//! ```

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

use std::collections::HashMap;
use std::path::PathBuf;

use brokkr_common::{Digest, TenantId, WorkerId};
use brokkr_control::locality::LocalityIndex;
use brokkr_control::scheduling::{
    DecisionContext, JobFacts, LoadView, LocalityView, NoLocality, SimpleFifo, Strategy,
};
use brokkr_control::wasm_strategy::WasmStrategy;
use brokkr_policy::{PolicyEngine, PolicyLimits};
use brokkr_proto::reapi_v2 as rapi;

/// Where the built example lands, relative to this crate.
fn artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/policies/locality/target/wasm32-unknown-unknown/release")
        .join("brokkr_policy_locality.wasm")
}

/// Read the built module, failing with the build command rather than skipping.
fn policy_bytes() -> Vec<u8> {
    let path = artifact_path();
    std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "could not read the example policy at {}: {e}\n\n\
             Build it first:\n  \
             rustup target add wasm32-unknown-unknown\n  \
             cd examples/policies/locality\n  \
             cargo build --release --target wasm32-unknown-unknown\n",
            path.display()
        )
    })
}

fn strategy() -> WasmStrategy {
    let mut engine = PolicyEngine::new(PolicyLimits::default()).unwrap();
    engine.load(&policy_bytes()).unwrap();
    WasmStrategy::new(engine, None)
}

fn wid(s: &str) -> WorkerId {
    WorkerId::new(s.to_string()).unwrap()
}

struct MapLoads(HashMap<WorkerId, usize>);
impl LoadView for MapLoads {
    fn inflight(&self, worker: &WorkerId) -> usize {
        self.0.get(worker).copied().unwrap_or(0)
    }
}

/// Run `f` with a decision context for `action` rooted at `input_root`.
fn with_ctx<R>(
    loads: &dyn LoadView,
    locality: &dyn LocalityView,
    action: &Digest,
    input_root: Option<&Digest>,
    f: impl FnOnce(&DecisionContext<'_>) -> R,
) -> R {
    let tenant = TenantId::default();
    let platform = rapi::Platform::default();
    let ctx = DecisionContext {
        loads,
        locality,
        job: JobFacts {
            tenant: &tenant,
            action_digest: action,
            input_root_digest: input_root,
            platform: &platform,
        },
    };
    f(&ctx)
}

/// **DoD line 4.** The example prefers the worker whose cache is warm for this
/// input root, even though that worker is busier — which is exactly the
/// tradeoff `SimpleFifo` cannot express.
#[test]
#[ignore = "needs examples/policies/locality built for wasm32-unknown-unknown"]
fn the_example_prefers_a_worker_warm_for_this_input_root() {
    let s = strategy();
    let (action, root) = (Digest::of(b"the-action"), Digest::of(b"the-input-root"));

    let mut idx = LocalityIndex::default();
    // "warm" ran this input root three times recently.
    for _ in 0..3 {
        idx.record(&wid("warm"), &action, Some(&root));
    }

    let cands = vec![wid("cold"), wid("warm")];
    // "warm" is also the busier worker, so a load-only policy avoids it.
    let loads = MapLoads(HashMap::from([(wid("cold"), 0), (wid("warm"), 2)]));

    with_ctx(&loads, &idx, &action, Some(&root), |ctx| {
        assert_eq!(
            SimpleFifo.choose(&cands, ctx.loads),
            Some(wid("cold")),
            "the built-in would take the idle worker"
        );
        assert_eq!(
            s.choose_with(&cands, ctx),
            Some(wid("warm")),
            "the locality policy should take the warm one: \
             3 input-root hits (30) beats 2 in-flight (-8)"
        );
    });
    assert_eq!(s.decided(), 1);
    assert_eq!(s.failure_counts().total(), 0);
}

/// Load still wins eventually. A warm-but-saturated worker must lose to a cold
/// idle one, or the policy would pile every job onto one machine — the failure
/// mode a pure locality rule has and a weighted score does not.
#[test]
#[ignore = "needs examples/policies/locality built for wasm32-unknown-unknown"]
fn a_saturated_warm_worker_loses_to_an_idle_cold_one() {
    let s = strategy();
    let (action, root) = (Digest::of(b"the-action"), Digest::of(b"the-input-root"));

    let mut idx = LocalityIndex::default();
    idx.record(&wid("warm"), &action, Some(&root));

    let cands = vec![wid("cold"), wid("warm")];
    // One input-root hit is worth 10; 20 in-flight jobs cost 80.
    let loads = MapLoads(HashMap::from([(wid("cold"), 0), (wid("warm"), 20)]));

    with_ctx(&loads, &idx, &action, Some(&root), |ctx| {
        assert_eq!(
            s.choose_with(&cands, ctx),
            Some(wid("cold")),
            "locality must not outweigh a saturated worker without limit"
        );
    });
}

/// With no locality history at all the policy has nothing to add, and its
/// scoring reduces to preferring the least loaded — the same answer the
/// built-in gives.
#[test]
#[ignore = "needs examples/policies/locality built for wasm32-unknown-unknown"]
fn with_no_history_it_agrees_with_the_builtin() {
    let s = strategy();
    let (action, root) = (Digest::of(b"a"), Digest::of(b"r"));
    let cands = vec![wid("busy"), wid("idle")];
    let loads = MapLoads(HashMap::from([(wid("busy"), 4), (wid("idle"), 0)]));

    with_ctx(&loads, &NoLocality, &action, Some(&root), |ctx| {
        assert_eq!(s.choose_with(&cands, ctx), Some(wid("idle")));
        assert_eq!(
            s.choose_with(&cands, ctx),
            SimpleFifo.choose(&cands, ctx.loads)
        );
    });
}

/// An action with no input root carries no locality signal, so the policy
/// declines rather than guessing — and declining is not a failure.
#[test]
#[ignore = "needs examples/policies/locality built for wasm32-unknown-unknown"]
fn an_action_without_an_input_root_is_declined_not_guessed() {
    let s = strategy();
    let action = Digest::of(b"a");
    let cands = vec![wid("busy"), wid("idle")];
    let loads = MapLoads(HashMap::from([(wid("busy"), 4), (wid("idle"), 0)]));

    with_ctx(&loads, &NoLocality, &action, None, |ctx| {
        assert_eq!(s.choose_with(&cands, ctx), Some(wid("idle")));
    });
    assert_eq!(s.declined(), 1, "it should decline, not decide");
    assert_eq!(s.decided(), 0);
    assert_eq!(s.failure_counts().total(), 0, "declining is not a failure");
}

/// The property the ABI's whole design protects: identical input, identical
/// answer. Run against a real module that actually decodes the snapshot, not
/// just a WAT fixture that ignores it.
#[test]
#[ignore = "needs examples/policies/locality built for wasm32-unknown-unknown"]
fn the_example_is_deterministic() {
    let s = strategy();
    let (action, root) = (Digest::of(b"a"), Digest::of(b"r"));
    let mut idx = LocalityIndex::default();
    idx.record(&wid("w1"), &action, Some(&root));
    idx.record(&wid("w2"), &action, Some(&root));
    idx.record(&wid("w2"), &action, Some(&root));

    let cands = vec![wid("w0"), wid("w1"), wid("w2")];
    let loads = MapLoads(HashMap::from([
        (wid("w0"), 1),
        (wid("w1"), 1),
        (wid("w2"), 3),
    ]));

    with_ctx(&loads, &idx, &action, Some(&root), |ctx| {
        let first = s.choose_with(&cands, ctx);
        assert!(first.is_some());
        for i in 0..200 {
            assert_eq!(s.choose_with(&cands, ctx), first, "call {i} diverged");
        }
    });
    assert_eq!(s.failure_counts().total(), 0);
}

/// The module must satisfy the `None`-iff-empty contract like every other
/// strategy.
#[test]
#[ignore = "needs examples/policies/locality built for wasm32-unknown-unknown"]
fn the_example_honours_the_none_iff_empty_contract() {
    let s = strategy();
    let (action, root) = (Digest::of(b"a"), Digest::of(b"r"));
    let loads = MapLoads(HashMap::new());
    with_ctx(&loads, &NoLocality, &action, Some(&root), |ctx| {
        assert!(s.choose_with(&[], ctx).is_none());
        assert!(s.choose_with(&[wid("only")], ctx).is_some());
    });
}
