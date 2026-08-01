//! End-to-end: a WebAssembly policy really decides where jobs land.
//!
//! This is Phase 6 definition-of-done line 1 (`docs/phase-6-plan.md`). The
//! scheduler unit tests prove the plumbing in isolation; this proves the whole
//! path — `Scheduler::execute` → `try_dispatch` → `Strategy::choose_with` →
//! snapshot → guest → candidate index → the job arriving on that worker's
//! channel.
//!
//! The policies are inline WebAssembly text, so this needs no `wasm32` target
//! and commits no binaries (CLAUDE.md rule 4).

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use brokkr_cas::Cas as _;
use brokkr_common::{Digest, TenantId, WorkerId};
use brokkr_control::registry::{WorkerCapabilities, WorkerRegistry};
use brokkr_control::scheduling::Strategy;
use brokkr_control::wasm_strategy::WasmStrategy;
use brokkr_control::{Scheduler, SharedWorkerRegistry};
use brokkr_policy::{PolicyEngine, PolicyLimits, POLICY_ABI_VERSION};
use brokkr_proto::brokkr_v1 as bv1;
use brokkr_proto::reapi_v2 as rapi;
use bytes::Bytes;
use prost::Message as _;
use tokio::sync::Mutex;

/// A policy module whose `brokkr_choose` runs `body`.
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

/// The engine's load-time smoke snapshot is small (~165 bytes) and carries two
/// candidates. Real snapshots here are padded past this so a WAT fixture — which
/// cannot decode protobuf — can tell the two apart by length alone.
const BIG: usize = 512;

/// Padding carried on the action's platform, so real snapshots clear [`BIG`].
/// Every worker advertises the same property, so it constrains nothing.
fn padded_platform() -> rapi::Platform {
    rapi::Platform {
        properties: vec![
            rapi::platform::Property {
                name: "os".to_string(),
                value: "linux".to_string(),
            },
            rapi::platform::Property {
                name: "brokkr-test-padding".to_string(),
                value: "x".repeat(BIG),
            },
        ],
    }
}

async fn stage_action(cas: &brokkr_cas::InMemoryCas) -> Digest {
    let command = rapi::Command {
        arguments: vec!["/bin/echo".to_string(), "hi".to_string()],
        ..Default::default()
    };
    let command_bytes = command.encode_to_vec();
    let command_digest = Digest::of(&command_bytes);
    let action = rapi::Action {
        command_digest: Some(rapi::Digest {
            hash: command_digest.hash().to_string(),
            size_bytes: command_digest.size_bytes(),
        }),
        platform: Some(padded_platform()),
        input_root_digest: Some(rapi::Digest {
            hash: Digest::of(b"inputs").hash().to_string(),
            size_bytes: Digest::of(b"inputs").size_bytes(),
        }),
        ..Default::default()
    };
    let action_bytes = action.encode_to_vec();
    let action_digest = Digest::of(&action_bytes);
    cas.batch_update_blobs(vec![
        (action_digest.clone(), Bytes::from(action_bytes)),
        (command_digest, Bytes::from(command_bytes)),
    ])
    .await
    .unwrap();
    action_digest
}

async fn register_and_connect(
    scheduler: &Arc<Scheduler>,
    registry: &SharedWorkerRegistry,
    id: &str,
) -> tokio::sync::mpsc::Receiver<bv1::Job> {
    let wid = WorkerId::new(id.to_string()).unwrap();
    let caps = WorkerCapabilities {
        hostname: id.to_string(),
        labels: BTreeMap::from([
            ("os".to_string(), "linux".to_string()),
            ("brokkr-test-padding".to_string(), "x".repeat(BIG)),
        ]),
    };
    registry
        .lock()
        .await
        .register(wid.clone(), caps, Instant::now());
    let (tx, rx) = tokio::sync::mpsc::channel::<bv1::Job>(8);
    scheduler.connect_worker(wid, tx).await;
    rx
}

/// An action cache that always misses and accepts writes, so every execution
/// really goes through dispatch.
#[derive(Debug)]
struct AlwaysMiss;

#[async_trait::async_trait]
impl brokkr_cas::ActionCache for AlwaysMiss {
    async fn get_action_result(
        &self,
        _digest: &Digest,
    ) -> Result<Option<rapi::ActionResult>, brokkr_cas::CasError> {
        Ok(None)
    }
    async fn update_action_result(
        &self,
        _digest: &Digest,
        _result: rapi::ActionResult,
    ) -> Result<(), brokkr_cas::CasError> {
        Ok(())
    }
}

fn strategy_for(body: &str, registry: SharedWorkerRegistry) -> Arc<WasmStrategy> {
    let engine = PolicyEngine::new(PolicyLimits::default()).unwrap();
    let s = WasmStrategy::new(engine, Some(registry));
    s.load(wat(body).as_bytes()).unwrap();
    Arc::new(s)
}

async fn recv_within(
    rx: &mut tokio::sync::mpsc::Receiver<bv1::Job>,
    budget: Duration,
) -> Option<bv1::Job> {
    for _ in 0..((budget.as_millis() / 10).max(1)) {
        if let Ok(job) = rx.try_recv() {
            return Some(job);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    rx.try_recv().ok()
}

/// **DoD line 1.** A policy that picks the *last* candidate routes the job
/// there — a choice no built-in would make.
///
/// `SimpleFifo` picks the least-loaded, breaking ties by worker id, so with
/// three idle workers it always picks `w-a`. This policy returns index 2, and
/// the scheduler sorts candidates by worker id, so index 2 is `w-c`. The test
/// asserts the job did *not* land on `w-a` — that asymmetry is what makes it a
/// real proof rather than a coincidence.
///
/// This test is also what caught the candidate list arriving in `HashMap`
/// order: it failed roughly one run in four, always by landing on `w-a`, with
/// the guest reporting a successful decision. See the sort in `try_dispatch`.
#[tokio::test]
async fn a_wasm_policy_routes_a_job_to_a_worker_no_builtin_would_pick() {
    let cas = Arc::new(brokkr_cas::InMemoryCas::new());
    let action_digest = stage_action(cas.as_ref()).await;
    let registry: SharedWorkerRegistry = Arc::new(Mutex::new(WorkerRegistry::default()));

    // WAT cannot decode the snapshot to count candidates, so the policy
    // returns a fixed index 2 — valid because this test always presents
    // exactly three — and 0 for the engine's smaller load-time smoke snapshot.
    let strategy = strategy_for(
        &format!(
            r#"(if (result i32) (i32.ge_s (local.get $len) (i32.const {BIG}))
      (then i32.const 2)
      (else i32.const 0))"#
        ),
        registry.clone(),
    );
    let scheduler = Scheduler::with_strategy(
        cas,
        Arc::new(AlwaysMiss),
        registry.clone(),
        strategy.clone(),
    );

    let mut rx_a = register_and_connect(&scheduler, &registry, "w-a").await;
    let mut rx_b = register_and_connect(&scheduler, &registry, "w-b").await;
    let mut rx_c = register_and_connect(&scheduler, &registry, "w-c").await;

    let exec = {
        let (s, d) = (scheduler.clone(), action_digest.clone());
        tokio::spawn(async move { s.execute(d, true, TenantId::default()).await })
    };

    let budget = Duration::from_millis(800);
    let on_a = recv_within(&mut rx_a, budget).await.is_some();
    let on_b = recv_within(&mut rx_b, Duration::from_millis(50))
        .await
        .is_some();
    let on_c = recv_within(&mut rx_c, Duration::from_millis(50))
        .await
        .is_some();
    exec.abort();

    assert_eq!(
        [on_a, on_b, on_c].iter().filter(|x| **x).count(),
        1,
        "exactly one worker should have received the job (a={on_a} b={on_b} c={on_c})"
    );
    assert!(
        !on_a,
        "the job landed on w-a, which is exactly what SimpleFifo would have \
         chosen — the policy's decision did not take effect"
    );
    assert_eq!(strategy.decided(), 1, "the guest must have decided");
    assert_eq!(strategy.failure_counts().total(), 0);
}

/// **DoD line 3.** A policy that traps on every real decision still places
/// every job. The cluster keeps working; the counter records the damage.
#[tokio::test]
async fn a_trapping_policy_still_places_every_job() {
    let cas = Arc::new(brokkr_cas::InMemoryCas::new());
    let action_digest = stage_action(cas.as_ref()).await;
    let registry: SharedWorkerRegistry = Arc::new(Mutex::new(WorkerRegistry::default()));

    let strategy = strategy_for(
        &format!(
            r#"(if (result i32) (i32.ge_s (local.get $len) (i32.const {BIG}))
      (then unreachable)
      (else i32.const 0))"#
        ),
        registry.clone(),
    );
    let scheduler = Scheduler::with_strategy(
        cas,
        Arc::new(AlwaysMiss),
        registry.clone(),
        strategy.clone(),
    );
    let mut rx = register_and_connect(&scheduler, &registry, "w-a").await;

    // Several jobs in sequence, each completed, so the trap is hit repeatedly.
    for i in 0..3 {
        let exec = {
            let (s, d) = (scheduler.clone(), action_digest.clone());
            tokio::spawn(async move { s.execute(d, true, TenantId::default()).await })
        };
        let job = recv_within(&mut rx, Duration::from_millis(800))
            .await
            .unwrap_or_else(|| panic!("job {i} was never dispatched despite the policy trapping"));
        scheduler
            .report(bv1::JobResult {
                job_id: job.job_id.clone(),
                result: Some(rapi::ActionResult {
                    exit_code: 0,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        exec.await.unwrap().unwrap();
    }

    let counts = strategy.failure_counts();
    assert_eq!(counts.for_reason("trap"), 3, "every trap must be counted");
    assert_eq!(strategy.decided(), 0, "the guest never decided anything");
}

/// A declining policy hands every decision to the built-in, and that is not
/// counted as damage.
#[tokio::test]
async fn a_declining_policy_defers_to_the_builtin_without_counting_failures() {
    let cas = Arc::new(brokkr_cas::InMemoryCas::new());
    let action_digest = stage_action(cas.as_ref()).await;
    let registry: SharedWorkerRegistry = Arc::new(Mutex::new(WorkerRegistry::default()));

    let strategy = strategy_for("i32.const -1", registry.clone());
    let scheduler = Scheduler::with_strategy(
        cas,
        Arc::new(AlwaysMiss),
        registry.clone(),
        strategy.clone(),
    );
    let mut rx_a = register_and_connect(&scheduler, &registry, "w-a").await;
    let mut rx_b = register_and_connect(&scheduler, &registry, "w-b").await;

    let exec = {
        let (s, d) = (scheduler.clone(), action_digest.clone());
        tokio::spawn(async move { s.execute(d, true, TenantId::default()).await })
    };
    // SimpleFifo breaks ties by worker id, so the built-in answer is w-a.
    let got_a = recv_within(&mut rx_a, Duration::from_millis(800))
        .await
        .is_some();
    let got_b = recv_within(&mut rx_b, Duration::from_millis(50))
        .await
        .is_some();
    exec.abort();

    assert!(got_a && !got_b, "the built-in tie-break should pick w-a");
    assert_eq!(strategy.declined(), 1);
    assert_eq!(strategy.failure_counts().total(), 0);
}

/// The candidate list a policy sees must be sorted by worker id.
///
/// That list is the index space `brokkr_choose` returns into, and both of its
/// sources (`WorkerRegistry::workers` and `ConnectedWorkers`) are `HashMap`s.
/// Without a sort, identical cluster state hands the guest a differently
/// ordered list run to run, so the same policy places the same job on
/// different workers — the exact failure this test caught, and the same class
/// of bug as the Phase 5 turmoil partition test (#174).
///
/// Registered out of order on purpose, so insertion order cannot be mistaken
/// for correct order.
#[tokio::test]
async fn the_candidate_order_a_policy_sees_is_deterministic() {
    for attempt in 0..25 {
        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref()).await;
        let registry: SharedWorkerRegistry = Arc::new(Mutex::new(WorkerRegistry::default()));

        // Index 0 must always be the lexicographically smallest id.
        let strategy = strategy_for(
            &format!(
                r#"(if (result i32) (i32.ge_s (local.get $len) (i32.const {BIG}))
      (then i32.const 0)
      (else i32.const 0))"#
            ),
            registry.clone(),
        );
        let scheduler = Scheduler::with_strategy(
            cas,
            Arc::new(AlwaysMiss),
            registry.clone(),
            strategy.clone(),
        );

        // Deliberately not alphabetical.
        let mut rx_z = register_and_connect(&scheduler, &registry, "w-zulu").await;
        let mut rx_a = register_and_connect(&scheduler, &registry, "w-alpha").await;
        let mut rx_m = register_and_connect(&scheduler, &registry, "w-mike").await;

        let exec = {
            let (s, d) = (scheduler.clone(), action_digest.clone());
            tokio::spawn(async move { s.execute(d, true, TenantId::default()).await })
        };
        let on_a = recv_within(&mut rx_a, Duration::from_millis(800))
            .await
            .is_some();
        let on_m = recv_within(&mut rx_m, Duration::from_millis(20))
            .await
            .is_some();
        let on_z = recv_within(&mut rx_z, Duration::from_millis(20))
            .await
            .is_some();
        exec.abort();

        assert_eq!(strategy.failure_counts().total(), 0, "attempt {attempt}");
        assert!(
            on_a && !on_m && !on_z,
            "attempt {attempt}: index 0 must always be w-alpha, the smallest id \
             (alpha={on_a} mike={on_m} zulu={on_z})"
        );
    }
}

/// The `None`-iff-empty contract holds through the real strategy object, so a
/// guest can never turn a placeable job into a stalled one.
#[tokio::test]
async fn the_none_iff_empty_contract_holds_for_the_wasm_strategy() {
    let registry: SharedWorkerRegistry = Arc::new(Mutex::new(WorkerRegistry::default()));
    let strategy = strategy_for("i32.const 0", registry.clone());
    let loads = brokkr_control::scheduling::ConnectedWorkers::new();
    let (tenant, action, root) = (TenantId::default(), Digest::of(b"a"), Digest::of(b"r"));
    let platform = padded_platform();
    let ctx = brokkr_control::scheduling::DecisionContext {
        loads: &loads,
        locality: &brokkr_control::scheduling::NoLocality,
        job: brokkr_control::scheduling::JobFacts {
            tenant: &tenant,
            action_digest: &action,
            input_root_digest: Some(&root),
            platform: &platform,
        },
    };
    assert!(strategy.choose_with(&[], &ctx).is_none());
    assert!(strategy
        .choose_with(&[WorkerId::new("only".to_string()).unwrap()], &ctx)
        .is_some());
}
