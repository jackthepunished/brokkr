# Phase 6 — WASM Scheduling Hooks

**Status:** design approved 2026-08-01, unstarted.
**Plan reference:** `docs/plan.md` §18 — "WASM-based extension hooks. User-defined
scheduling policies."
**ADR:** [0014](architecture/0014-wasm-scheduling-policies.md).
**Predecessor:** Phase 5 (custom Raft, HA control plane) — complete, see
`docs/phase-5-plan.md` and `docs/journal/phase-5.md`.

---

## Part I — What we are building and why

ADR 0008 introduced a pluggable `Strategy` trait and shipped two policies:
`SimpleFifo` (least-loaded) and `BinPacking`. It also promised a third,
`LocalityAware`, and named it a "later increment". That increment was never
built, and it is the right one to build — but not as a third hard-coded
`impl Strategy`.

Scheduling policy is exactly the kind of thing that is *operator-specific*: how
much you value input locality over spread, whether a GPU worker should be held
back for GPU work, how aggressively to pack before scaling down — these depend
on the fleet, not on Brokkr. Compiling a new policy into the control plane and
redeploying to answer those questions is the wrong loop.

**Phase 6 makes `Strategy` loadable at runtime from a WebAssembly module,** and
delivers `LocalityAware` as an *example policy* rather than as a built-in. The
operator writes (or adapts) a policy, points the control plane at the `.wasm`,
and iterates without a rebuild or a restart.

### The five decisions, and what each one rules out

| # | Decision | Rejected alternative | Why |
|---|---|---|---|
| D1 | **Operator-supplied** modules only — a local file path, no upload RPC, no per-tenant policies | Tenant-uploaded policies | An uploaded module is untrusted code influencing *other tenants'* placement. That needs a threat model, quotas, and an isolation story we do not have. The operator already runs the binary; a file they place next to it is inside the existing trust boundary. |
| D2 | **Hot-reloadable** — the file is watched and swapped without a restart | Load once at startup | The whole point is a fast iteration loop. A policy you must restart to change is barely better than a recompile. |
| D3 | **Enriched ABI + locality history** — candidates carry capability labels and in-flight counts; the job carries tenant, platform and action digest; each candidate additionally carries per-worker recent-action / recent-input-root counters | Minimal ABI (ids + load only) | A minimal ABI can only re-express `SimpleFifo`. Locality history is what makes ADR 0008's `LocalityAware` expressible *as a policy*, which is the load-bearing proof that the hook is real and not a toy. |
| D4 | **Fall back to the built-in, count it loudly** — trap / fuel exhaustion / deadline / bad index all degrade to `SimpleFifo` for that one decision, `warn!` with the reason, increment a per-reason counter | Fail the dispatch | Same reasoning as Phase 5's decision D1: a broken *policy* must not become a broken *cluster*. Dispatch is a hot path with queued work behind it; refusing to place a job punishes the tenant for the operator's bug. The counter and the log are what keep this out of the "silent degradation" category. |
| D5 | **wasmtime**, pooling allocator + `InstancePre`, **fuel *and* epoch interruption** | wasmi (smaller, interpreter, pure-Rust, no cranelift) | Recommended `wasmi`; the owner chose `wasmtime`, and the deciding property is real: **epoch interruption is the only mechanism that bounds *wall-clock* time inside a guest call.** Fuel bounds *work*. This call happens while the scheduler's dispatch mutex is held, so a guest that stalls stalls the whole cluster's placement. Fuel alone cannot promise it won't. Accepted cost: the cranelift dependency tree. |

### Definition of done

1. A `.wasm` policy loaded from disk makes real placement decisions, provably —
   an integration test where the policy picks a worker that *no* built-in would
   pick, and the job lands there.
2. Editing the policy file changes subsequent decisions with **no restart**, and
   a policy that fails validation is **refused without disturbing the running
   one**.
3. A trapping policy, an infinite-loop policy, and a policy returning an
   out-of-range index each degrade to `SimpleFifo`, each increment their own
   counter, and none of them fails a job.
4. `LocalityAware` exists as an example policy under `examples/policies/`,
   written in Rust, documented, and demonstrably preferring a worker that
   recently ran the same input root.
5. Measured: p99 added latency per placement decision **< 250µs**, and the
   number of `Strategy` calls per dispatch drain is **O(placements)**, not
   O(queue²).

---

## Part II — Architecture

### II.1 Crate layout

A new crate, **`brokkr-policy`**, owns everything WASM: the ABI types, module
loading and validation, the engine and its budgets.

```
brokkr-control ──> brokkr-policy ──> brokkr-common
     │                   └── wasmtime, brokkr-proto
     └── impl Strategy for WasmStrategy   // the adapter lives HERE
```

`brokkr-policy` deliberately does **not** know about `Strategy`. If it imported
that trait from `brokkr-control` — which in turn depends on `brokkr-policy` to
use it — that is a cycle, and the DAG invariant is not negotiable. So
`brokkr-policy` exposes a `PolicyEngine` with a pure shape (snapshot in, chosen
index out; no I/O, no blocking, no knowledge of workers-as-such), and
`brokkr-control` owns the thin `WasmStrategy` adapter.

The payoff beyond the DAG: the engine is testable with no scheduler at all, and
a future consumer — an admission hook, a preemption policy, a GC policy — reuses
it without touching `scheduler.rs`.

### II.2 The store/instance model (resolving the `Send + Sync` tension)

`Strategy: Send + Sync`. `wasmtime::Store` is `Send` but **not** `Sync`. These
are reconcilable only if no `Store` is ever held in the strategy.

**We do not pool `Store`s.** A reused `Store` carries the previous decision's
guest heap and globals into the next one, which silently breaks determinism —
the same snapshot could yield different answers depending on call history, and
that is the one property the whole testing strategy rests on.

What we pool is the *expensive* part:

- One `wasmtime::Engine`, configured with `PoolingAllocationConfig` so linear
  memories and instance slots are recycled rather than mmap'd fresh.
- One `wasmtime::InstancePre<HostState>` per loaded module — imports resolved
  and typechecked **once**, at load.
- Per decision: a fresh `Store` + `InstancePre::instantiate`. With the pooling
  allocator this is a slot handoff and a memory reset, in the low tens of
  microseconds.

So `PolicyEngine` holds only `Engine` + `InstancePre` + config, all of which are
`Send + Sync`. The tension dissolves; it never needed to be fought.

### II.3 The call-site refactor (prerequisite, lands first)

`scheduler.rs::try_dispatch` currently calls `strategy.choose` **once per
pending slot, on every pass**, and the outer `loop` re-runs until nothing can be
placed. For a queue of Q jobs that is O(Q²) strategy calls per drain. With the
built-ins that is a few comparisons and nobody noticed. With a WASM call at even
50µs, a 100-job queue would hold the dispatch mutex for half a second.

The fix does not need a new trait contract — it needs the *existing* one to be
read carefully. `Strategy::choose` is documented to return `None` **iff**
`candidates` is empty. So "can this slot be dispatched at all?" is
`!candidates.is_empty()`, and does not require calling the policy. Restructure:

1. Walk the slots, computing each one's candidate set.
2. Keep the lowest-start-tag slot whose candidate set is **non-empty**.
3. Call `choose` **exactly once**, on that slot's candidates.

O(1) policy calls per placement. This is a standalone correctness-neutral
improvement to the built-ins too, and it lands as its own commit with its own
test before any WASM code exists.

### II.4 Widening the strategy interface, additively

Locality history and job facts have nowhere to live in
`choose(&self, candidates, loads)`. Rather than churn the signature and every
built-in and test, add a second method with a default implementation:

```rust
pub trait Strategy: Send + Sync {
    fn choose(&self, candidates: &[WorkerId], loads: &dyn LoadView) -> Option<WorkerId>;

    /// Choose with full decision context. Defaults to ignoring everything the
    /// built-ins don't use, so `SimpleFifo` / `BinPacking` need no change.
    fn choose_with(&self, candidates: &[WorkerId], ctx: &DecisionContext<'_>) -> Option<WorkerId> {
        self.choose(candidates, ctx.loads)
    }
}
```

The scheduler calls `choose_with`. `WasmStrategy` overrides it. Zero blast
radius on existing code, and the built-ins stay honest about what they actually
consult.

```rust
pub struct DecisionContext<'a> {
    pub loads: &'a dyn LoadView,
    pub locality: &'a dyn LocalityView,
    pub job: JobFacts<'a>,   // tenant, platform properties, action digest, input root
}

pub trait LocalityView {
    /// How many of `worker`'s recent completions used this input root.
    fn input_root_hits(&self, worker: &WorkerId, input_root: &Digest) -> u32;
    /// How many of `worker`'s recent completions were this exact action.
    fn action_hits(&self, worker: &WorkerId, action: &Digest) -> u32;
}
```

**Contract, restated and enforced for every implementation including WASM:**
`choose_with` returns `None` **iff** `candidates` is empty. `WasmStrategy` never
returns `None` for a non-empty candidate set — a guest that declines or fails
yields the `SimpleFifo` answer, not nothing. A test asserts this for every
strategy, including deliberately broken WASM modules.

---

## Part III — The guest ABI

This is the load-bearing decision, because operators compile against it and
modules compiled today must keep working.

### III.1 Encoding: protobuf

A new `brokkr/v1/policy.proto`, generated into `brokkr-proto` alongside
everything else. Three reasons, in order of weight:

1. **Forward compatibility.** Field numbers mean adding `gpu_class` next year
   does not break a module compiled today. A hand-rolled packed struct would
   either freeze the ABI or silently misparse on drift — and misparsing here
   produces *wrong placements*, not errors.
2. It is already the project's wire language (architectural invariant), and the
   codegen path exists.
3. Guests in other languages have protobuf libraries.

The cost — a guest must link a protobuf decoder — is real and is the reason the
snapshot is kept small and flat.

### III.2 Exports the guest must provide

```wat
(export "brokkr_abi_version" (func))   ;; () -> i32
(export "brokkr_alloc"       (func))   ;; (i32) -> i32   host writes the snapshot here
(export "brokkr_choose"      (func))   ;; (i32 ptr, i32 len) -> i32
```

- `brokkr_abi_version` is called once at load and gated against
  `POLICY_ABI_VERSION`. A mismatch **refuses the module** — this is the one
  failure that is loud at load rather than degrading at runtime, because a
  version-mismatched module would otherwise misparse every snapshot silently.
- `brokkr_alloc(len) -> ptr` lets the guest own its allocator. The host writes
  `len` bytes of encoded snapshot at `ptr`. The host never frees; the store is
  dropped after the call, which frees everything.
- `brokkr_choose(ptr, len) -> i32` returns a **candidate index**, or **`-1`**
  meaning *"no preference — use the built-in."*

`-1` matters more than it looks. It lets a policy punt on a case it does not
understand instead of guessing, and it is distinguishable from a failure: a
`-1` is **not** counted as a policy failure and does not trip the circuit
breaker. Any other out-of-range value **is** a failure (`BadIndex`).

### III.3 What crosses the boundary

Per decision, one `DecisionSnapshot`:

```proto
message DecisionSnapshot {
  uint32          abi_version = 1;
  JobFacts        job         = 2;
  repeated Candidate candidates = 3;   // index in this list is what choose() returns
}

message JobFacts {
  string tenant             = 1;
  bytes  action_digest      = 2;
  bytes  input_root_digest  = 3;
  repeated PlatformProperty platform = 4;
}

message Candidate {
  string worker_id                = 1;
  uint32 inflight                 = 2;
  repeated PlatformProperty labels = 3;
  uint32 input_root_hits          = 4;   // locality: recent completions on this input root
  uint32 action_hits              = 5;   // locality: recent completions of this exact action
}
```

Read-only, **by value**. The guest gets a copy; it cannot hold a reference into
host state across calls, and there is nothing for it to corrupt.

### III.4 No WASI

The policy computes. It does not read clocks, files, sockets, or randomness. No
WASI, no host functions beyond the three exports above — the import set is
empty. This is what makes *"same snapshot ⇒ same decision"* a testable property
rather than an aspiration, and it removes an entire class of sandbox-escape
surface for free.

---

## Part IV — Locality bookkeeping

The counters in `Candidate` need a source. `LocalityIndex` lives in the
scheduler's `Inner` (already mutex-guarded, so no new synchronization):

- Per worker, a bounded `VecDeque<(action_digest, input_root_digest)>` of its
  most recent completions. Capacity `--policy-locality-window`, default **64**.
- Pushed when a lease completes on a successful report — the one place the
  scheduler already learns "worker W finished action A".
- **Not** cleared on disconnect. A worker that reconnects very likely still has
  its inputs materialized locally; forgetting on disconnect would throw away
  precisely the signal we are collecting. Instead the index is bounded by an LRU
  over *workers* (cap 1024) so a churning fleet cannot grow it without bound.

Memory ceiling is therefore explicit and small: 1024 workers × 64 entries × 64
bytes ≈ 4 MiB worst case.

`input_root_hits` / `action_hits` are computed by the host at snapshot-build
time (a scan of ≤64 entries per candidate), not exposed as a raw list. Keeping
the ABI to two `uint32`s per candidate keeps the encoded snapshot small and the
guest simple, and it means the window size can change without an ABI bump.

---

## Part V — Budgets, failure, and reload

### V.1 Budgets

| Knob | Default | Purpose |
|------|---------|---------|
| `--policy-fuel` | 1_000_000 | Bounds *work*. Catches accidental O(n²) in a policy. |
| `--policy-deadline-ms` | 5 | Bounds *wall-clock*, via epoch interruption. The real safety net, because the dispatch mutex is held. |
| `--policy-reload-interval-secs` | 5 | mtime/size poll cadence. |
| `--policy-locality-window` | 64 | Per-worker completion history depth. |

Epoch ticking is a dedicated OS thread calling `Engine::increment_epoch` on a
1ms cadence; the store's deadline is `deadline_ms` ticks. This thread is the
only reason the deadline is real — document it as such so nobody "optimizes" it
away.

### V.2 Failure taxonomy and the circuit breaker

```rust
enum PolicyFailure { Trap, FuelExhausted, Deadline, BadIndex, Instantiate, NotLoaded }
```

Every variant: `warn!` with the reason and the job id, increment
`wasm_policy_failures` (per-reason, exposed the way
`uncached_results_not_leader` is), and **fall back to `SimpleFifo` for that
decision**. Never fail the job.

Falling back forever is not good enough on its own. A policy that traps on every
call would burn its full deadline per decision, permanently, while dutifully
logging about it. So: **after `POLICY_QUARANTINE_THRESHOLD` (16) consecutive
failures the policy is quarantined** — `WasmStrategy` stops calling it entirely
and serves `SimpleFifo` directly — until the file changes on disk. A single
`error!` marks entry into quarantine. This is what makes "fall back and count"
safe under sustained load rather than merely correct in principle.

A successful call (including a `-1` decline) resets the consecutive counter.

### V.3 Hot reload

A background task polls `(mtime, len)` of `--policy-wasm` every
`--policy-reload-interval-secs`. On change:

1. Read the bytes.
2. Compile (`Module::new`) — a compile failure is logged and **the running
   policy is untouched**.
3. Validate: required exports present with the right signatures;
   `brokkr_abi_version()` matches; then a **smoke decision** against a synthetic
   two-candidate snapshot, within the normal fuel and deadline budgets, must
   return a valid index or `-1`.
4. Only on full success, swap.

The invariant is: **a module that cannot pass validation never becomes the live
policy.** A bad edit degrades to "your change didn't take effect, and here's the
log line saying why" — not to a quarantined cluster.

Storage is `std::sync::RwLock<Option<Arc<LoadedPolicy>>>`. Writes happen at most
once per poll interval, so read contention is nil, and no new dependency
(`arc-swap`) is needed. The hot path clones the `Arc` and **drops the read guard
before calling the guest** — so a decision in flight during a swap completes
against the module it started with, and the next decision uses the new one.
There is no torn state and no need to quiesce dispatch.

Reload resets quarantine — that is the operator's fix path.

---

## Part VI — Testing

### VI.1 The wasm32-toolchain problem, and the way around it

Building a `.wasm` fixture normally needs the `wasm32-unknown-unknown` target in
CI, or a committed binary (which CLAUDE.md rule 4 disallows by default). Both
are avoidable: **wasmtime's `Module::new` accepts WAT text directly.** Fixture
policies are inline WAT string constants in the test file — no build step, no
new CI target, no committed binaries, and the fixture is readable in the diff.

The trivial-but-essential fixtures are all easy in WAT because they need no
decoding: *always return 0*, *always return the last index*, *always return
999*, *unreachable* (trap), *infinite loop* (deadline), *tight counting loop*
(fuel), *return -1* (decline), *wrong abi_version*, *missing export*.

The one thing WAT is genuinely bad at is decoding protobuf — so the *real*
example policy is Rust:

- `examples/policies/locality/` — a real `LocalityAware` in Rust, `no_std`-ish,
  built with a documented `cargo build --target wasm32-unknown-unknown --release`.
- Its integration test is `#[ignore]`d and gated on the built artifact existing,
  with the build command in the failure message. Never silently skipped.
- Its encoder counterpart is covered by a host-side round-trip test: the
  snapshot the host builds decodes, via `prost`, to the values it was built from.

### VI.2 Layers

| Layer | What it proves |
|-------|----------------|
| `brokkr-policy` unit | Snapshot round-trip; version gate refuses; each failure variant is classified correctly; quarantine trips at 16 and resets on reload; a valid module returns the expected index. |
| Determinism | 100 calls with an identical snapshot return an identical answer (the property that D3's whole ABI design exists to protect). |
| `brokkr-control` unit | `choose_with` returns `None` iff candidates is empty, for `SimpleFifo`, `BinPacking`, and `WasmStrategy` backed by each broken fixture. `LocalityIndex` bounds, LRU eviction, survival across disconnect. |
| Scheduler integration | A WASM policy picking the *last* candidate (which no built-in would ever pick) demonstrably routes the job there — DoD 1. A trapping policy still places every job — DoD 3. |
| Reload integration | Swap the file, next decision changes; write garbage, the previous policy keeps serving and a log line explains — DoD 2. |
| Bench / measurement | p99 decision latency, and a counted assertion that a Q-job drain makes O(placements) strategy calls — DoD 5. |

---

## Part VII — Risks and gates

**R1 — MSRV. This is a gate, not a risk: resolve it first.** The toolchain is
pinned to **1.85.0** (`rust-toolchain.toml`). Recent wasmtime tracks stable
closely and very likely requires newer. Before any Phase 6 code:

- Determine the newest wasmtime that builds on 1.85.
- If a usable version exists (must have `PoolingAllocationConfig`, `fuel`, and
  `epoch_interruption`), pin it and record the pin's reason.
- If not, the toolchain bump is a **separate, standalone, first commit** — never
  bundled with a feature. It has a bonus: the `time` pin comment in `deny.toml`
  says the RUSTSEC-2026-0009 ignore can be dropped once MSRV ≥ 1.88. Dropping it
  is *also* its own commit (CLAUDE.md rule 7: lockfile changes stand alone).

**R2 — cargo-deny.** wasmtime pulls the cranelift tree. `Apache-2.0 WITH
LLVM-exception` is **already** in `deny.toml`'s allow list, so licenses should
pass; advisories and `multiple-versions` are the unknowns. Run `cargo deny check`
immediately after the dependency lands, before writing engine code — finding out
at PR time is the expensive order.

**R3 — Dependency justification (CLAUDE.md rule 6).** One line, in the PR and in
ADR 0014: *"wasmtime — the only Rust WASM runtime offering epoch-based
interruption, which is the only mechanism that bounds wall-clock time in a guest
call made while the dispatch mutex is held; fuel bounds work, not time."*

**R4 — Compile time.** Cranelift is heavy and CI is already six jobs. Mitigate by
putting wasmtime behind a **non-default `wasm-policy` feature on
`brokkr-policy`**, enabled by `brokkr-control` by default but disableable, so a
build that doesn't want it doesn't pay. Measure CI wall-clock before and after
and record it.

**R5 — Scope creep into a plugin platform.** The temptation to add admission
hooks, GC policies, and retry policies "while the machinery is here" is real and
must be resisted. Phase 6 ships **one** hook. The engine is designed to be
reusable (Part II.1) precisely so the *next* one is cheap — later.

---

## Part VIII — Sequencing

Each row is one PR, mergeable and green on its own.

| # | Increment | Depends on | DoD line |
|---|-----------|------------|----------|
| P0 | Resolve R1 (MSRV determination; toolchain bump as its own commit if needed) | — | gate |
| P1 | `try_dispatch` refactor: O(1) strategy calls per placement, with a call-counting test | — | 5 |
| P2 | `choose_with` + `DecisionContext` + `LocalityView` (traits only, built-ins unchanged, default impl) | P1 | — |
| P3 | `LocalityIndex` in `Inner`: bounded window, worker LRU, populated on lease completion, survives disconnect | P2 | 4 |
| P4 | `brokkr/v1/policy.proto` + host-side snapshot builder + round-trip tests (no wasmtime yet) | P3 | — |
| P5 | `brokkr-policy` crate: engine, pooling + `InstancePre`, fuel + epoch, load/validate, failure taxonomy, quarantine. WAT fixtures. Resolve R2/R3/R4 here. | P4 | 3 |
| P6 | `WasmStrategy` in `brokkr-control` + CLI flags + wiring | P5 | 1 |
| P7 | Hot reload: poller, validate-before-swap, quarantine reset, integration test | P6 | 2 |
| P8 | `examples/policies/locality/` + docs (`docs/operations/writing-a-scheduling-policy.md`) + ADR 0014 accepted | P7 | 4 |
| P9 | Measurement pass (p99 latency, CI wall-clock delta), retrospective in `docs/journal/phase-6.md`, plan.md §18 updated | P8 | 5 |
