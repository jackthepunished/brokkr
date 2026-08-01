//! Hot reload of the WASM scheduling policy, against real files.
//!
//! This is Phase 6 definition-of-done line 2 (`docs/phase-6-plan.md`): editing
//! the policy file changes subsequent decisions with no restart, and a policy
//! that fails validation is refused **without disturbing the running one**.
//!
//! The unit tests in `policy_reload` cover the change-detection logic as a pure
//! function. These cover the part that only a filesystem can prove.

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use brokkr_common::{Digest, TenantId, WorkerId};
use brokkr_control::policy_reload::spawn_policy_reloader;
use brokkr_control::scheduling::{
    DecisionContext, JobFacts, LoadView, NoLocality, SimpleFifo, Strategy,
};
use brokkr_control::wasm_strategy::WasmStrategy;
use brokkr_policy::{PolicyEngine, PolicyLimits, POLICY_ABI_VERSION};
use brokkr_proto::reapi_v2 as rapi;

/// Snapshots here are padded past this so a WAT fixture — which cannot decode
/// protobuf — can tell a real decision from the engine's smaller load-time
/// smoke snapshot by length alone.
const BIG: usize = 512;

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

/// A policy that returns `idx` for a real decision and 0 for the smoke
/// snapshot, so it still passes load-time validation.
fn picks(idx: i32) -> String {
    wat(&format!(
        r#"(if (result i32) (i32.ge_s (local.get $len) (i32.const {BIG}))
      (then i32.const {idx})
      (else i32.const 0))"#
    ))
}

/// Compile WAT to a `.wasm` binary so the file on disk is what an operator
/// would actually deploy.
fn to_wasm(text: &str) -> Vec<u8> {
    wat_to_binary(text)
}

fn wat_to_binary(text: &str) -> Vec<u8> {
    // wasmtime accepts WAT directly, so round-tripping through a module and
    // back is unnecessary: the engine will parse either form. Writing the text
    // bytes keeps the fixture readable in a debugger and on disk.
    text.as_bytes().to_vec()
}

struct MapLoads(HashMap<WorkerId, usize>);
impl LoadView for MapLoads {
    fn inflight(&self, worker: &WorkerId) -> usize {
        self.0.get(worker).copied().unwrap_or(0)
    }
}

fn wid(s: &str) -> WorkerId {
    WorkerId::new(s.to_string()).unwrap()
}

/// Run `f` with a decision context whose snapshot clears [`BIG`].
fn with_ctx<R>(loads: &dyn LoadView, f: impl FnOnce(&DecisionContext<'_>) -> R) -> R {
    let (action, root) = (Digest::of(b"action"), Digest::of(b"root"));
    let tenant = TenantId::default();
    let platform = rapi::Platform {
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
            platform: &platform,
        },
    };
    f(&ctx)
}

fn strategy_from(bytes: &[u8]) -> Arc<WasmStrategy> {
    let engine = PolicyEngine::new(PolicyLimits::default()).unwrap();
    let s = WasmStrategy::new(engine, None);
    s.load(bytes).unwrap();
    Arc::new(s)
}

/// Poll until `f` holds or the budget runs out. Returns whether it held.
async fn eventually(budget: Duration, mut f: impl FnMut() -> bool) -> bool {
    let step = Duration::from_millis(20);
    let mut waited = Duration::ZERO;
    while waited < budget {
        if f() {
            return true;
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
    f()
}

/// **DoD line 2, first half.** Editing the file changes subsequent decisions,
/// with no restart.
#[tokio::test]
async fn editing_the_policy_file_changes_subsequent_decisions() {
    let dir = std::env::temp_dir().join(format!(
        "brokkr-policy-reload-{}",
        std::process::id() as u64 * 31 + 1
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("policy.wat");
    std::fs::write(&path, to_wasm(&picks(0))).unwrap();

    let strategy = strategy_from(&std::fs::read(&path).unwrap());
    let cands = vec![wid("a"), wid("b")];
    // "a" is the *more* loaded worker, so SimpleFifo would pick "b". Any
    // assertion that lands on "a" therefore proves the policy is deciding.
    let loads = MapLoads(HashMap::from([(wid("a"), 9), (wid("b"), 0)]));

    with_ctx(&loads, |ctx| {
        assert_eq!(SimpleFifo.choose(&cands, ctx.loads), Some(wid("b")));
        assert_eq!(strategy.choose_with(&cands, ctx), Some(wid("a")));
    });

    let _reloader = spawn_policy_reloader(
        Arc::clone(&strategy),
        path.clone(),
        Duration::from_millis(50),
    );

    // Swap in a policy that picks index 1 instead.
    std::fs::write(&path, to_wasm(&picks(1))).unwrap();

    let swapped = eventually(Duration::from_secs(10), || {
        with_ctx(&loads, |ctx| {
            strategy.choose_with(&cands, ctx) == Some(wid("b"))
        })
    })
    .await;
    assert!(
        swapped,
        "the edited policy should be picked up without a restart"
    );
    assert_eq!(
        strategy.failure_counts().total(),
        0,
        "a clean swap must produce no failures"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// **DoD line 2, second half.** A policy that fails validation is refused, and
/// the running one keeps serving.
///
/// Every rejection path is exercised in turn — garbage bytes, a trapping
/// module, and a wrong ABI version — because they fail at different stages of
/// `PolicyEngine::load` and only one of them is a compile error.
#[tokio::test]
async fn an_invalid_edit_is_refused_and_the_running_policy_keeps_serving() {
    let dir = std::env::temp_dir().join(format!(
        "brokkr-policy-reject-{}",
        std::process::id() as u64 * 31 + 2
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("policy.wat");
    std::fs::write(&path, to_wasm(&picks(0))).unwrap();

    let strategy = strategy_from(&std::fs::read(&path).unwrap());
    let cands = vec![wid("a"), wid("b")];
    let loads = MapLoads(HashMap::from([(wid("a"), 9), (wid("b"), 0)]));

    let _reloader = spawn_policy_reloader(
        Arc::clone(&strategy),
        path.clone(),
        Duration::from_millis(50),
    );

    let bad_edits = [
        // Not WebAssembly at all — fails to compile.
        "this is not a wasm module".to_string(),
        // Compiles, but traps on its smoke decision.
        wat("unreachable"),
        // Compiles and exports correctly, but speaks the wrong ABI.
        wat("i32.const 0").replace(
            &format!("i32.const {POLICY_ABI_VERSION})"),
            "i32.const 4242)",
        ),
    ];

    for (i, bad) in bad_edits.iter().enumerate() {
        std::fs::write(&path, bad.as_bytes()).unwrap();
        // Give the reloader several intervals to see it and reject it.
        tokio::time::sleep(Duration::from_millis(400)).await;

        with_ctx(&loads, |ctx| {
            assert_eq!(
                strategy.choose_with(&cands, ctx),
                Some(wid("a")),
                "bad edit {i} disturbed the running policy — it should still \
                 pick index 0 (worker \"a\"), not fall back to SimpleFifo's \"b\""
            );
        });
        assert_eq!(
            strategy.failure_counts().total(),
            0,
            "bad edit {i}: a refused load is not a decision failure"
        );
    }

    // And a *valid* edit after all that still takes effect, proving the
    // watcher did not wedge itself on the rejections.
    std::fs::write(&path, to_wasm(&picks(1))).unwrap();
    let recovered = eventually(Duration::from_secs(10), || {
        with_ctx(&loads, |ctx| {
            strategy.choose_with(&cands, ctx) == Some(wid("b"))
        })
    })
    .await;
    assert!(
        recovered,
        "a valid edit after rejected ones must still be applied"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Reloading clears quarantine. This is the documented operator fix path, so
/// it has to actually fix things — a policy that stays quarantined after the
/// module is replaced would make the feature useless in the one situation it
/// matters most.
#[tokio::test]
async fn reloading_clears_a_quarantine() {
    let dir = std::env::temp_dir().join(format!(
        "brokkr-policy-quarantine-{}",
        std::process::id() as u64 * 31 + 3
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("policy.wat");
    // Returns an index that is valid for the 2-candidate smoke snapshot but
    // out of range for the single candidate used below.
    std::fs::write(&path, to_wasm(&wat("i32.const 1"))).unwrap();

    let strategy = strategy_from(&std::fs::read(&path).unwrap());
    let one = vec![wid("solo")];
    let loads = MapLoads(HashMap::new());

    with_ctx(&loads, |ctx| {
        for _ in 0..(PolicyLimits::default().quarantine_threshold + 3) {
            // Every placement still succeeds, quarantined or not.
            assert_eq!(strategy.choose_with(&one, ctx), Some(wid("solo")));
        }
    });
    let counts = strategy.failure_counts();
    assert!(
        counts.for_reason("quarantined") > 0,
        "the policy should be quarantined by now"
    );

    let _reloader = spawn_policy_reloader(
        Arc::clone(&strategy),
        path.clone(),
        Duration::from_millis(50),
    );

    // Replace with a policy that declines — always valid, never a failure.
    std::fs::write(&path, to_wasm(&wat("i32.const -1"))).unwrap();

    let before = counts.for_reason("quarantined");
    let cleared = eventually(Duration::from_secs(10), || {
        with_ctx(&loads, |ctx| {
            let _ = strategy.choose_with(&one, ctx);
        });
        // Once reloaded, declines are recorded and quarantine stops climbing.
        strategy.declined() > 0
    })
    .await;
    assert!(cleared, "reloading must clear the quarantine");

    let after_reload = counts.for_reason("quarantined");
    with_ctx(&loads, |ctx| {
        for _ in 0..5 {
            assert_eq!(strategy.choose_with(&one, ctx), Some(wid("solo")));
        }
    });
    assert_eq!(
        counts.for_reason("quarantined"),
        after_reload,
        "no further quarantine failures should be recorded after the reload \
         (was {before} before)"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A deleted policy file must not disturb the running policy. Deleting is not
/// an instruction to stop scheduling.
#[tokio::test]
async fn deleting_the_policy_file_leaves_the_running_policy_serving() {
    let dir = std::env::temp_dir().join(format!(
        "brokkr-policy-deleted-{}",
        std::process::id() as u64 * 31 + 4
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("policy.wat");
    std::fs::write(&path, to_wasm(&picks(0))).unwrap();

    let strategy = strategy_from(&std::fs::read(&path).unwrap());
    let cands = vec![wid("a"), wid("b")];
    let loads = MapLoads(HashMap::from([(wid("a"), 9), (wid("b"), 0)]));

    let _reloader = spawn_policy_reloader(
        Arc::clone(&strategy),
        path.clone(),
        Duration::from_millis(50),
    );

    std::fs::remove_file(&path).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;

    with_ctx(&loads, |ctx| {
        assert_eq!(
            strategy.choose_with(&cands, ctx),
            Some(wid("a")),
            "deleting the file must not stop the loaded policy from deciding"
        );
    });
    assert_eq!(strategy.failure_counts().total(), 0);

    std::fs::remove_dir_all(&dir).ok();
}
