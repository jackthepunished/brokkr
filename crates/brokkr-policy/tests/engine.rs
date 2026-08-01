//! Engine behaviour against real WebAssembly modules.
//!
//! Every fixture is **WebAssembly text**, compiled by wasmtime at test time.
//! That is deliberate: it means CI needs no `wasm32-unknown-unknown` target,
//! the repository carries no committed binaries (CLAUDE.md rule 4), and each
//! policy's behaviour is readable in the diff rather than hidden in a blob.
//!
//! WAT is genuinely bad at one thing — decoding protobuf — so the fixtures here
//! ignore the snapshot's contents and exercise the *contract*: budgets, traps,
//! return-value handling, validation, and quarantine. A policy that really
//! parses a snapshot is the Rust example under `examples/policies/`.

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

use std::time::{Duration, Instant};

use brokkr_policy::{
    Decision, PolicyEngine, PolicyError, PolicyFailure, PolicyLimits, POLICY_ABI_VERSION,
};
use brokkr_proto::brokkr_v1 as bv1;
use prost::Message as _;

/// The boilerplate every fixture shares: a memory, a bump allocator, and a
/// correct `brokkr_abi_version`. Only `brokkr_choose` differs between them.
fn policy(choose_body: &str) -> String {
    format!(
        r#"(module
  (memory (export "memory") 2)
  (global $bump (mut i32) (i32.const 1024))
  (func (export "brokkr_abi_version") (result i32)
    i32.const {version})
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
    {choose_body})
)"#,
        version = POLICY_ABI_VERSION,
    )
}

/// Snapshots handed to a fixture are padded past [`BIG`] bytes so a WAT
/// policy can distinguish "a real decision" from the engine's load-time smoke
/// snapshot using nothing but the length it is given.
///
/// That distinction is what lets us test failures that happen *at decision
/// time*. `load` deliberately refuses a module that fails its smoke decision,
/// so a fixture that always traps can never be loaded at all — which is the
/// desired behaviour, tested separately. The interesting production case is
/// the policy that is fine on one input and breaks on another, and these
/// fixtures model exactly that.
const BIG: i32 = 512;

/// A snapshot with `n` candidates, padded past [`BIG`]. Contents beyond the
/// candidate count are irrelevant to WAT fixtures.
fn snapshot(n: usize) -> Vec<u8> {
    bv1::DecisionSnapshot {
        abi_version: POLICY_ABI_VERSION,
        job: Some(bv1::PolicyJobFacts {
            tenant: "t".repeat(BIG as usize),
            action_digest: "a".repeat(64),
            input_root_digest: "b".repeat(64),
            platform: Vec::new(),
        }),
        candidates: (0..n)
            .map(|i| bv1::PolicyCandidate {
                worker_id: format!("w{i}"),
                inflight: i as u32,
                ..Default::default()
            })
            .collect(),
    }
    .encode_to_vec()
}

/// A policy that returns 0 for the small smoke snapshot and runs
/// `on_big` for any real (padded) decision. Lets a fixture misbehave at
/// decision time while still passing validation at load time.
fn policy_that_misbehaves_on_real_input(on_big: &str) -> String {
    policy(&format!(
        r#"(if (result i32) (i32.ge_s (local.get $len) (i32.const {BIG}))
      (then {on_big})
      (else i32.const 0))"#
    ))
}

fn engine_with(wat: &str, limits: PolicyLimits) -> Result<PolicyEngine, PolicyError> {
    let mut e = PolicyEngine::new(limits)?;
    e.load(wat.as_bytes())?;
    Ok(e)
}

fn engine(wat: &str) -> PolicyEngine {
    engine_with(wat, PolicyLimits::default()).unwrap()
}

// ---------------------------------------------------------------- happy path

#[test]
fn a_valid_policy_decides() {
    let e = engine(&policy("i32.const 1"));
    assert!(e.is_loaded());
    assert_eq!(e.decide(&snapshot(3), 3).unwrap(), Decision::Chose(1));
}

#[test]
fn a_policy_may_decline_and_that_is_not_a_failure() {
    let e = engine(&policy("i32.const -1"));
    assert_eq!(e.decide(&snapshot(3), 3).unwrap(), Decision::Declined);
    assert_eq!(
        e.consecutive_failures(),
        0,
        "declining must not count as a failure"
    );
}

/// The property the whole no-WASI, fresh-`Store`-per-call design exists to
/// protect: identical input, identical answer, every time.
#[test]
fn decisions_are_deterministic_across_many_calls() {
    // A policy with mutable global state, to prove state does not leak between
    // decisions: it increments a counter and returns it modulo 2. If the store
    // were reused the answer would alternate.
    let wat = format!(
        r#"(module
  (memory (export "memory") 2)
  (global $bump (mut i32) (i32.const 1024))
  (global $calls (mut i32) (i32.const 0))
  (func (export "brokkr_abi_version") (result i32) i32.const {version})
  (func (export "brokkr_alloc") (param $len i32) (result i32)
    global.get $bump)
  (func (export "brokkr_choose") (param $ptr i32) (param $len i32) (result i32)
    global.get $calls
    i32.const 1
    i32.add
    global.set $calls
    global.get $calls
    i32.const 2
    i32.rem_u)
)"#,
        version = POLICY_ABI_VERSION,
    );
    let e = engine(&wat);
    let snap = snapshot(2);
    let first = e.decide(&snap, 2).unwrap();
    for i in 0..200 {
        assert_eq!(
            e.decide(&snap, 2).unwrap(),
            first,
            "call {i} diverged: guest state leaked between decisions"
        );
    }
}

// ------------------------------------------------------------------ failures

#[test]
fn a_trapping_policy_is_a_trap_failure() {
    let e = engine(&policy_that_misbehaves_on_real_input("unreachable"));
    let err = e.decide(&snapshot(2), 2).unwrap_err();
    assert_eq!(err.reason(), "trap", "got {err:?}");
}

#[test]
fn an_out_of_range_index_is_a_bad_index_failure() {
    let e = engine(&policy_that_misbehaves_on_real_input("i32.const 999"));
    let err = e.decide(&snapshot(2), 2).unwrap_err();
    assert!(
        matches!(
            err,
            PolicyFailure::BadIndex {
                returned: 999,
                candidates: 2
            }
        ),
        "got {err:?}"
    );
}

/// -2 is negative but is not the decline sentinel. Treating it as a decline
/// would let a buggy policy silently disable itself.
#[test]
fn a_negative_index_that_is_not_decline_is_a_failure() {
    let e = engine(&policy_that_misbehaves_on_real_input("i32.const -2"));
    let err = e.decide(&snapshot(2), 2).unwrap_err();
    assert_eq!(err.reason(), "bad_index", "got {err:?}");
}

#[test]
fn a_policy_that_does_too_much_work_exhausts_its_fuel() {
    // A counting loop far longer than the fuel budget allows.
    let wat = policy(
        r#"(local $i i32)
    (local.set $i (i32.const 0))
    (loop $l
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (i32.const 100000000))))
    i32.const 0"#,
    );
    let limits = PolicyLimits {
        fuel: 10_000,
        // Generous, so fuel is unambiguously what runs out first.
        deadline_ms: 5_000,
        ..PolicyLimits::default()
    };
    // The load-time smoke test runs this same policy, so loading must fail —
    // which is itself the desired behaviour, and is asserted separately below.
    let err = engine_with(&wat, limits).unwrap_err();
    match err {
        PolicyError::SmokeTest(f) => assert_eq!(f.reason(), "fuel_exhausted", "got {f:?}"),
        other => panic!("expected a smoke-test failure, got {other:?}"),
    }
}

/// The reason wasmtime was chosen over wasmi: fuel bounds *work*, and only the
/// epoch bounds *time*. An infinite loop with effectively unlimited fuel must
/// still be interrupted, and promptly, because this call happens under the
/// scheduler's dispatch mutex.
#[test]
fn an_infinite_loop_is_stopped_by_the_wall_clock_deadline() {
    let wat = policy(
        r#"(loop $l (br $l))
    i32.const 0"#,
    );
    let limits = PolicyLimits {
        fuel: u64::MAX,
        deadline_ms: 20,
        ..PolicyLimits::default()
    };
    let started = Instant::now();
    let err = engine_with(&wat, limits).unwrap_err();
    let elapsed = started.elapsed();

    match err {
        PolicyError::SmokeTest(f) => assert_eq!(f.reason(), "deadline", "got {f:?}"),
        other => panic!("expected a smoke-test failure, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "the deadline did not bound wall-clock time: {elapsed:?}"
    );
}

// ---------------------------------------------------------------- validation

#[test]
fn a_module_with_a_wrong_abi_version_is_refused_at_load() {
    let wat = policy("i32.const 0").replace(
        &format!("i32.const {POLICY_ABI_VERSION})\n"),
        "i32.const 9999)\n",
    );
    let mut e = PolicyEngine::new(PolicyLimits::default()).unwrap();
    match e.load(wat.as_bytes()).unwrap_err() {
        PolicyError::AbiMismatch { found, expected } => {
            assert_eq!(found, 9999);
            assert_eq!(expected, POLICY_ABI_VERSION);
        }
        other => panic!("expected AbiMismatch, got {other:?}"),
    }
    assert!(!e.is_loaded());
}

#[test]
fn a_module_missing_an_export_is_refused_at_load() {
    let wat = format!(
        r#"(module
  (memory (export "memory") 1)
  (func (export "brokkr_abi_version") (result i32) i32.const {POLICY_ABI_VERSION})
  (func (export "brokkr_alloc") (param i32) (result i32) i32.const 1024)
)"#
    );
    let mut e = PolicyEngine::new(PolicyLimits::default()).unwrap();
    match e.load(wat.as_bytes()).unwrap_err() {
        PolicyError::MissingExport { name, .. } => assert_eq!(name, "brokkr_choose"),
        other => panic!("expected MissingExport, got {other:?}"),
    }
}

#[test]
fn a_module_that_is_not_wasm_at_all_is_refused_at_load() {
    let mut e = PolicyEngine::new(PolicyLimits::default()).unwrap();
    assert!(matches!(
        e.load(b"this is not a wasm module").unwrap_err(),
        PolicyError::Compile(_)
    ));
    assert!(!e.is_loaded());
}

/// A module that traps on its very first call must never become the live
/// policy — otherwise every placement would pay a full trap before falling
/// back, forever.
#[test]
fn a_module_that_traps_immediately_fails_its_smoke_test() {
    let mut e = PolicyEngine::new(PolicyLimits::default()).unwrap();
    match e.load(policy("unreachable").as_bytes()).unwrap_err() {
        PolicyError::SmokeTest(f) => assert_eq!(f.reason(), "trap"),
        other => panic!("expected SmokeTest, got {other:?}"),
    }
    assert!(!e.is_loaded());
}

/// The invariant that makes hot reload safe: a rejected module leaves the
/// running one serving.
#[test]
fn a_failed_load_leaves_the_previous_policy_untouched() {
    let mut e = PolicyEngine::new(PolicyLimits::default()).unwrap();
    e.load(policy("i32.const 1").as_bytes()).unwrap();
    assert_eq!(e.decide(&snapshot(3), 3).unwrap(), Decision::Chose(1));

    // Every rejection path, one after another.
    assert!(e.load(b"garbage").is_err());
    assert!(e.load(policy("unreachable").as_bytes()).is_err());
    let wrong_version = policy("i32.const 0").replace(
        &format!("i32.const {POLICY_ABI_VERSION})\n"),
        "i32.const 9999)\n",
    );
    assert!(e.load(wrong_version.as_bytes()).is_err());

    assert!(e.is_loaded());
    assert_eq!(
        e.decide(&snapshot(3), 3).unwrap(),
        Decision::Chose(1),
        "the previously loaded policy must still be serving"
    );
}

#[test]
fn an_engine_with_no_module_reports_not_loaded() {
    let e = PolicyEngine::new(PolicyLimits::default()).unwrap();
    assert!(!e.is_loaded());
    assert_eq!(
        e.decide(&snapshot(2), 2).unwrap_err().reason(),
        "not_loaded"
    );
}

// ---------------------------------------------------------------- quarantine

/// A policy that fails every call must eventually stop being called at all.
/// Falling back per-decision is correct but not sufficient: without this, a
/// broken policy burns its full deadline on every placement, forever.
#[test]
fn repeated_failures_quarantine_the_policy() {
    // `load` runs a smoke decision, so a policy that always fails cannot be
    // loaded. Use one that succeeds on the smoke snapshot's 2 candidates and
    // fails when asked about 1 — index 1 is valid for 2, out of range for 1.
    let e = engine(&policy("i32.const 1"));
    let limits = PolicyLimits::default();

    assert_eq!(e.decide(&snapshot(2), 2).unwrap(), Decision::Chose(1));
    assert_eq!(e.consecutive_failures(), 0);

    for i in 1..limits.quarantine_threshold {
        let err = e.decide(&snapshot(1), 1).unwrap_err();
        assert_eq!(err.reason(), "bad_index");
        assert_eq!(e.consecutive_failures(), i);
        assert!(!e.is_quarantined(), "too early at {i}");
    }

    // The threshold-th failure trips it.
    let err = e.decide(&snapshot(1), 1).unwrap_err();
    assert_eq!(err.reason(), "bad_index");
    assert!(e.is_quarantined());

    // From here the guest is not called at all.
    let err = e.decide(&snapshot(1), 1).unwrap_err();
    assert_eq!(err.reason(), "quarantined", "got {err:?}");
    // Even a call that *would* have succeeded is refused while quarantined.
    let err = e.decide(&snapshot(2), 2).unwrap_err();
    assert_eq!(err.reason(), "quarantined", "got {err:?}");
}

/// One success clears the streak. A policy that fails intermittently — a bad
/// edge case rather than a broken module — must not accumulate its way into
/// quarantine over hours.
#[test]
fn a_success_resets_the_failure_streak() {
    let e = engine(&policy("i32.const 1"));
    for _ in 0..5 {
        assert!(e.decide(&snapshot(1), 1).is_err());
    }
    assert_eq!(e.consecutive_failures(), 5);

    assert_eq!(e.decide(&snapshot(2), 2).unwrap(), Decision::Chose(1));
    assert_eq!(e.consecutive_failures(), 0);
    assert!(!e.is_quarantined());
}

/// Declining is a success for this purpose. A policy that punts on every job it
/// does not recognise is behaving correctly, not failing.
#[test]
fn declining_resets_the_failure_streak_too() {
    let mut e = PolicyEngine::new(PolicyLimits::default()).unwrap();
    e.load(policy("i32.const 1").as_bytes()).unwrap();
    for _ in 0..5 {
        assert!(e.decide(&snapshot(1), 1).is_err());
    }
    assert_eq!(e.consecutive_failures(), 5);

    e.load(policy("i32.const -1").as_bytes()).unwrap();
    assert_eq!(e.decide(&snapshot(1), 1).unwrap(), Decision::Declined);
    assert_eq!(e.consecutive_failures(), 0);
}

/// Reloading is the operator's fix path, so it must clear quarantine.
#[test]
fn reloading_clears_quarantine() {
    let mut e = PolicyEngine::new(PolicyLimits::default()).unwrap();
    e.load(policy("i32.const 1").as_bytes()).unwrap();
    for _ in 0..PolicyLimits::default().quarantine_threshold {
        assert!(e.decide(&snapshot(1), 1).is_err());
    }
    assert!(e.is_quarantined());

    e.load(policy("i32.const 0").as_bytes()).unwrap();
    assert!(!e.is_quarantined());
    assert_eq!(e.consecutive_failures(), 0);
    assert_eq!(e.decide(&snapshot(1), 1).unwrap(), Decision::Chose(0));
}
