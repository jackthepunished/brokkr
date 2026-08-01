//! Measured decision latency for a WASM scheduling policy (ADR 0014).
//!
//! This is Phase 6 definition-of-done line 5 (`docs/phase-6-plan.md`): **p99
//! added latency per placement decision under 250µs**.
//!
//! The number matters because this call happens while the scheduler holds its
//! dispatch mutex. A slow policy does not slow one job; it slows placement for
//! the whole cluster.
//!
//! `#[ignore]`d because it is a measurement, not a correctness check: CI
//! runners are shared and noisy, and a latency assertion there would either be
//! flaky or so loose as to prove nothing. Run it deliberately:
//!
//! ```sh
//! cd examples/policies/locality
//! cargo build --release --target wasm32-unknown-unknown
//! cd ../../..
//! cargo test --release -p brokkr-control --test policy_latency -- --ignored --nocapture
//! ```
//!
//! Run it under `--release`. A debug-profile measurement of a JIT-compiled
//! guest measures the host's debug build, not the policy.

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use brokkr_common::{Digest, TenantId, WorkerId};
use brokkr_control::locality::LocalityIndex;
use brokkr_control::scheduling::{DecisionContext, JobFacts, LoadView, SimpleFifo, Strategy};
use brokkr_control::wasm_strategy::WasmStrategy;
use brokkr_policy::{PolicyEngine, PolicyLimits};
use brokkr_proto::reapi_v2 as rapi;

/// The DoD budget for one policy decision.
const P99_BUDGET: Duration = Duration::from_micros(250);

/// Decisions per measurement. Enough that the p99 is a real tail rather than a
/// single unlucky sample.
const SAMPLES: usize = 20_000;

/// Warm-up decisions, discarded. The first calls pay for lazy paging of the
/// compiled code and the pooling allocator's first slot handoffs, which is not
/// what steady-state dispatch looks like.
const WARMUP: usize = 2_000;

fn artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/policies/locality/target/wasm32-unknown-unknown/release")
        .join("brokkr_policy_locality.wasm")
}

fn policy_bytes() -> Vec<u8> {
    let path = artifact_path();
    std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "could not read the example policy at {}: {e}\n\n\
             Build it first:\n  \
             cd examples/policies/locality && \
             cargo build --release --target wasm32-unknown-unknown\n",
            path.display()
        )
    })
}

struct MapLoads(HashMap<WorkerId, usize>);
impl LoadView for MapLoads {
    fn inflight(&self, worker: &WorkerId) -> usize {
        self.0.get(worker).copied().unwrap_or(0)
    }
}

fn wid(n: usize) -> WorkerId {
    WorkerId::new(format!("worker-{n:04}")).unwrap()
}

/// Percentile of a sorted slice, by nearest-rank.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn report(label: &str, mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    let total: Duration = samples.iter().sum();
    let mean = total / samples.len() as u32;
    let p50 = percentile(&samples, 50.0);
    let p99 = percentile(&samples, 99.0);
    let p999 = percentile(&samples, 99.9);
    println!(
        "{label:<28} n={:<6} mean={mean:>10.2?}  p50={p50:>10.2?}  p99={p99:>10.2?}  \
         p99.9={p999:>10.2?}  max={:>10.2?}",
        samples.len(),
        samples.last().copied().unwrap_or_default(),
    );
    p99
}

/// Measure `candidate_count` candidates through `strategy`, returning the
/// per-decision samples.
fn measure(strategy: &dyn Strategy, candidate_count: usize) -> Vec<Duration> {
    let (action, root) = (Digest::of(b"the-action"), Digest::of(b"the-input-root"));
    let candidates: Vec<WorkerId> = (0..candidate_count).map(wid).collect();

    // A realistic, non-degenerate world: varied load, and a locality index with
    // history for a third of the fleet, so the guest has real work to do rather
    // than scoring a list of zeroes.
    let loads = MapLoads(
        candidates
            .iter()
            .enumerate()
            .map(|(i, w)| (w.clone(), i % 7))
            .collect(),
    );
    let mut locality = LocalityIndex::default();
    for (i, w) in candidates.iter().enumerate() {
        if i % 3 == 0 {
            for _ in 0..(i % 5 + 1) {
                locality.record(w, &action, Some(&root));
            }
        }
    }

    let tenant = TenantId::default();
    let platform = rapi::Platform {
        properties: vec![rapi::platform::Property {
            name: "os".to_string(),
            value: "linux".to_string(),
        }],
    };
    let ctx = DecisionContext {
        loads: &loads,
        locality: &locality,
        job: JobFacts {
            tenant: &tenant,
            action_digest: &action,
            input_root_digest: Some(&root),
            platform: &platform,
        },
    };

    for _ in 0..WARMUP {
        std::hint::black_box(strategy.choose_with(&candidates, &ctx));
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t0 = Instant::now();
        let chosen = strategy.choose_with(&candidates, &ctx);
        samples.push(t0.elapsed());
        std::hint::black_box(chosen);
    }
    samples
}

/// **DoD line 5.** p99 per-decision latency stays under 250µs.
///
/// Measured across fleet sizes, because the snapshot grows with the candidate
/// count and a policy that is fine for 8 workers and hopeless for 256 would be
/// a trap waiting for whoever scales up.
#[test]
#[ignore = "measurement; run under --release with the example policy built"]
fn policy_decision_latency_stays_within_budget() {
    let mut engine = PolicyEngine::new(PolicyLimits::default()).unwrap();
    engine.load(&policy_bytes()).unwrap();
    let wasm = WasmStrategy::new(engine, None);

    println!();
    println!("Policy decision latency (ADR 0014 DoD line 5, budget p99 < {P99_BUDGET:?})");
    println!();

    // The built-in, as a baseline: this is the cost the WASM path is added to.
    let baseline = report("SimpleFifo   64 workers", measure(&SimpleFifo, 64));

    let mut worst = Duration::ZERO;
    for n in [8usize, 32, 64, 128, 256] {
        let p99 = report(&format!("WasmStrategy {n:>3} workers"), measure(&wasm, n));
        worst = worst.max(p99);
        assert!(
            p99 < P99_BUDGET,
            "p99 for {n} candidates was {p99:?}, over the {P99_BUDGET:?} budget"
        );
    }

    println!();
    println!("baseline (SimpleFifo, 64) p99 = {baseline:?}");
    println!("worst WASM p99 across fleet sizes = {worst:?}");
    println!();

    assert_eq!(
        wasm.failure_counts().total(),
        0,
        "the measurement must not be measuring the fallback path"
    );
    assert!(
        wasm.decided() > 0,
        "the guest must actually have been deciding"
    );
}
