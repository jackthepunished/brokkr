//! The wasmtime-backed policy engine: load, validate, and run one bounded
//! decision (ADR 0014).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use wasmtime::{
    Config, Engine, InstanceAllocationStrategy, InstancePre, Linker, Module,
    PoolingAllocationConfig, Store, StoreLimits, StoreLimitsBuilder,
};

use crate::{
    Decision, PolicyError, PolicyFailure, PolicyLimits, EXPORT_ABI_VERSION, EXPORT_ALLOC,
    EXPORT_CHOOSE, EXPORT_MEMORY, POLICY_ABI_VERSION,
};

/// Ceiling on a guest's linear memory, in bytes. A scheduling policy reads a
/// snapshot of at most a few hundred kilobytes and writes nothing; anything
/// wanting more is misbehaving, and the pooling allocator wants a bound anyway.
const MAX_GUEST_MEMORY: usize = 16 * 1024 * 1024;

/// How often the epoch ticker advances wasmtime's epoch. The deadline is
/// expressed in these ticks, so this is the granularity of the wall-clock
/// budget.
const EPOCH_TICK: Duration = Duration::from_millis(1);

/// Per-store host state. Only the memory limiter — the guest gets no host
/// functions at all.
struct HostState {
    limits: StoreLimits,
}

/// A loaded, validated policy module.
///
/// Holds an `InstancePre` — imports resolved and typechecked once, at load —
/// but deliberately **no** `Store`. See the crate docs for why.
struct LoadedModule {
    pre: InstancePre<HostState>,
}

/// Runs operator-supplied WebAssembly scheduling policies under a hard fuel and
/// wall-clock budget.
///
/// `Send + Sync`, which is the whole reason no `Store` is retained: `Store` is
/// `Send` but not `Sync`, while `Engine` and `InstancePre` are both.
pub struct PolicyEngine {
    engine: Engine,
    limits: PolicyLimits,
    module: Option<LoadedModule>,
    /// Consecutive decision failures. Reset by any success, including a
    /// decline. Reaching `limits.quarantine_threshold` stops the guest being
    /// called at all.
    consecutive_failures: Arc<AtomicU32>,
}

impl std::fmt::Debug for PolicyEngine {
    // `Engine` and `InstancePre` are not `Debug`; report the parts an operator
    // would actually want to see.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyEngine")
            .field("limits", &self.limits)
            .field("loaded", &self.module.is_some())
            .field(
                "consecutive_failures",
                &self.consecutive_failures.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl PolicyEngine {
    /// Build an engine with no module loaded.
    ///
    /// Spawns the epoch ticker thread. That thread is the *only* reason the
    /// wall-clock deadline is real — without it the epoch never advances and
    /// `set_epoch_deadline` never fires. Do not "optimize" it away.
    pub fn new(limits: PolicyLimits) -> Result<Self, PolicyError> {
        let mut pool = PoolingAllocationConfig::default();
        pool.total_memories(64);
        pool.total_core_instances(64);
        pool.max_memory_size(MAX_GUEST_MEMORY);

        let mut config = Config::new();
        config
            .consume_fuel(true)
            .epoch_interruption(true)
            .wasm_multi_memory(false)
            .allocation_strategy(InstanceAllocationStrategy::Pooling(pool));

        let engine = Engine::new(&config).map_err(|e| PolicyError::Engine(e.to_string()))?;

        // The ticker holds a weak handle so it exits once the engine is
        // dropped, rather than pinning it alive for the process lifetime.
        let weak = engine.weak();
        std::thread::Builder::new()
            .name("brokkr-policy-epoch".to_string())
            .spawn(move || loop {
                std::thread::sleep(EPOCH_TICK);
                match weak.upgrade() {
                    Some(e) => e.increment_epoch(),
                    None => break,
                }
            })
            .map_err(|e| PolicyError::Engine(format!("spawning the epoch ticker: {e}")))?;

        Ok(Self {
            engine,
            limits,
            module: None,
            consecutive_failures: Arc::new(AtomicU32::new(0)),
        })
    }

    /// Compile, validate, and install `wasm` as the live policy.
    ///
    /// Validation is deliberately thorough, because everything it catches here
    /// it would otherwise catch on the dispatch path, once per decision,
    /// forever:
    ///
    /// 1. It compiles.
    /// 2. The three required exports are present with the right signatures.
    /// 3. `brokkr_abi_version()` matches [`POLICY_ABI_VERSION`].
    /// 4. A synthetic decision runs to a valid answer inside the normal budget.
    ///
    /// On any failure the previously loaded module — if any — is left
    /// **untouched**. A bad edit costs a log line, not the scheduler.
    pub fn load(&mut self, wasm: &[u8]) -> Result<(), PolicyError> {
        let module =
            Module::new(&self.engine, wasm).map_err(|e| PolicyError::Compile(format!("{e:#}")))?;

        // No imports at all: the guest gets no clocks, files, sockets or
        // randomness. That is what makes "same snapshot => same decision" a
        // testable property rather than an aspiration.
        let linker: Linker<HostState> = Linker::new(&self.engine);
        let pre = linker
            .instantiate_pre(&module)
            .map_err(|e| PolicyError::Compile(format!("resolving imports: {e:#}")))?;

        let candidate = LoadedModule { pre };
        self.check_exports_and_abi(&candidate)?;
        self.smoke_test(&candidate)?;

        self.module = Some(candidate);
        self.consecutive_failures.store(0, Ordering::Relaxed);
        Ok(())
    }

    /// Whether a module is currently installed.
    pub fn is_loaded(&self) -> bool {
        self.module.is_some()
    }

    /// Consecutive decision failures so far.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// Whether the policy is quarantined and no longer being called.
    pub fn is_quarantined(&self) -> bool {
        self.consecutive_failures() >= self.limits.quarantine_threshold
    }

    /// Run one decision.
    ///
    /// `snapshot` is an encoded `brokkr.v1.DecisionSnapshot`; `candidate_count`
    /// is how many candidates it contains, used to validate the returned index.
    ///
    /// Never panics and never blocks beyond the configured deadline. Every
    /// error is a [`PolicyFailure`] the caller is expected to absorb by using
    /// its built-in policy for this one placement.
    pub fn decide(
        &self,
        snapshot: &[u8],
        candidate_count: usize,
    ) -> Result<Decision, PolicyFailure> {
        let Some(module) = self.module.as_ref() else {
            return Err(PolicyFailure::NotLoaded);
        };
        if self.is_quarantined() {
            return Err(PolicyFailure::Quarantined {
                consecutive: self.consecutive_failures(),
            });
        }

        let result = self.call(module, snapshot, candidate_count);
        match &result {
            Ok(_) => {
                // Any success clears the streak, including a decline.
                self.consecutive_failures.store(0, Ordering::Relaxed);
            }
            Err(f) if f.counts_toward_quarantine() => {
                let n = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if n == self.limits.quarantine_threshold {
                    tracing::error!(
                        consecutive = n,
                        reason = f.reason(),
                        "scheduling policy quarantined after repeated failures; \
                         falling back to the built-in strategy until the module is reloaded"
                    );
                }
            }
            Err(_) => {}
        }
        result
    }

    /// One guest invocation: fresh store, budgets applied, snapshot written,
    /// `brokkr_choose` called, store dropped.
    fn call(
        &self,
        module: &LoadedModule,
        snapshot: &[u8],
        candidate_count: usize,
    ) -> Result<Decision, PolicyFailure> {
        let mut store = self.new_store()?;
        let instance = module
            .pre
            .instantiate(&mut store)
            .map_err(|e| PolicyFailure::Instantiate(format!("{e:#}")))?;

        let len = i32::try_from(snapshot.len())
            .map_err(|_| PolicyFailure::Memory("snapshot exceeds i32::MAX bytes".to_string()))?;

        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, EXPORT_ALLOC)
            .map_err(|e| PolicyFailure::Memory(format!("{EXPORT_ALLOC}: {e:#}")))?;
        let ptr = alloc
            .call(&mut store, len)
            .map_err(|e| self.classify(&mut store, e))?;

        let memory = instance
            .get_memory(&mut store, EXPORT_MEMORY)
            .ok_or_else(|| PolicyFailure::Memory(format!("no `{EXPORT_MEMORY}` export")))?;
        let offset = usize::try_from(ptr)
            .map_err(|_| PolicyFailure::Memory(format!("{EXPORT_ALLOC} returned {ptr}")))?;
        memory
            .write(&mut store, offset, snapshot)
            .map_err(|e| PolicyFailure::Memory(format!("writing the snapshot: {e}")))?;

        let choose = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, EXPORT_CHOOSE)
            .map_err(|e| PolicyFailure::Memory(format!("{EXPORT_CHOOSE}: {e:#}")))?;
        let raw = choose
            .call(&mut store, (ptr, len))
            .map_err(|e| self.classify(&mut store, e))?;

        crate::interpret(raw, candidate_count)
    }

    /// A store with this decision's fuel, deadline, and memory ceiling.
    fn new_store(&self) -> Result<Store<HostState>, PolicyFailure> {
        let state = HostState {
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_GUEST_MEMORY)
                .instances(1)
                .build(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|e| PolicyFailure::Instantiate(format!("setting fuel: {e}")))?;
        // The ticker advances the epoch once per millisecond, so the deadline
        // is `deadline_ms` ticks away. At least 1, or a zero would mean "the
        // deadline has already passed" and every call would be interrupted.
        store.set_epoch_deadline(self.limits.deadline_ms.max(1));
        Ok(store)
    }

    /// Turn a wasmtime call error into the right [`PolicyFailure`].
    ///
    /// Fuel exhaustion and a deadline hit both surface as traps, and telling
    /// them apart matters: "your policy does too much work" and "your policy
    /// took too long on a busy host" need different fixes. Remaining fuel is
    /// the discriminator — a call killed by the epoch usually still has some.
    fn classify(&self, store: &mut Store<HostState>, err: wasmtime::Error) -> PolicyFailure {
        if let Some(trap) = err.downcast_ref::<wasmtime::Trap>() {
            match trap {
                wasmtime::Trap::OutOfFuel => return PolicyFailure::FuelExhausted,
                wasmtime::Trap::Interrupt => return PolicyFailure::Deadline,
                _ => return PolicyFailure::Trap(format!("{trap}")),
            }
        }
        // Not a typed trap: fall back on the fuel reading, then on the text.
        if store.get_fuel().is_ok_and(|f| f == 0) {
            return PolicyFailure::FuelExhausted;
        }
        let text = format!("{err:#}");
        if text.contains("epoch deadline") || text.contains("interrupt") {
            return PolicyFailure::Deadline;
        }
        if text.contains("fuel") {
            return PolicyFailure::FuelExhausted;
        }
        PolicyFailure::Trap(text)
    }

    /// Confirm the required exports exist with the right signatures, and that
    /// the module's ABI version matches ours.
    fn check_exports_and_abi(&self, module: &LoadedModule) -> Result<(), PolicyError> {
        let mut store = self
            .new_store()
            .map_err(|f| PolicyError::Engine(f.to_string()))?;
        let instance = module
            .pre
            .instantiate(&mut store)
            .map_err(|e| PolicyError::Compile(format!("instantiating for validation: {e:#}")))?;

        if instance.get_memory(&mut store, EXPORT_MEMORY).is_none() {
            return Err(PolicyError::MissingExport {
                name: EXPORT_MEMORY,
                detail: "the guest must export its linear memory".to_string(),
            });
        }
        instance
            .get_typed_func::<i32, i32>(&mut store, EXPORT_ALLOC)
            .map_err(|e| PolicyError::MissingExport {
                name: EXPORT_ALLOC,
                detail: format!("expected (i32) -> i32: {e:#}"),
            })?;
        instance
            .get_typed_func::<(i32, i32), i32>(&mut store, EXPORT_CHOOSE)
            .map_err(|e| PolicyError::MissingExport {
                name: EXPORT_CHOOSE,
                detail: format!("expected (i32, i32) -> i32: {e:#}"),
            })?;
        let version_fn = instance
            .get_typed_func::<(), i32>(&mut store, EXPORT_ABI_VERSION)
            .map_err(|e| PolicyError::MissingExport {
                name: EXPORT_ABI_VERSION,
                detail: format!("expected () -> i32: {e:#}"),
            })?;

        let found = version_fn
            .call(&mut store, ())
            .map_err(|e| PolicyError::Compile(format!("calling {EXPORT_ABI_VERSION}: {e:#}")))?;
        let found = u32::try_from(found).unwrap_or(u32::MAX);
        if found != POLICY_ABI_VERSION {
            return Err(PolicyError::AbiMismatch {
                found,
                expected: POLICY_ABI_VERSION,
            });
        }
        Ok(())
    }

    /// Run a synthetic two-candidate decision within the normal budget.
    ///
    /// This is what stops a module that traps, spins, or returns garbage on its
    /// very first call from ever becoming the live policy. It costs one
    /// decision's worth of time at load, which is free compared to discovering
    /// the same thing once per placement in production.
    fn smoke_test(&self, module: &LoadedModule) -> Result<(), PolicyError> {
        let snapshot = smoke_snapshot();
        match self.call(module, &snapshot, 2) {
            Ok(_) => Ok(()),
            Err(f) => Err(PolicyError::SmokeTest(f)),
        }
    }
}

/// A minimal, valid two-candidate snapshot for load-time validation.
fn smoke_snapshot() -> Vec<u8> {
    use brokkr_proto::brokkr_v1 as bv1;
    use prost::Message as _;

    bv1::DecisionSnapshot {
        abi_version: POLICY_ABI_VERSION,
        job: Some(bv1::PolicyJobFacts {
            tenant: String::new(),
            action_digest: "0".repeat(64),
            input_root_digest: "1".repeat(64),
            platform: Vec::new(),
        }),
        candidates: vec![
            bv1::PolicyCandidate {
                worker_id: "brokkr-smoke-a".to_string(),
                ..Default::default()
            },
            bv1::PolicyCandidate {
                worker_id: "brokkr-smoke-b".to_string(),
                ..Default::default()
            },
        ],
    }
    .encode_to_vec()
}
