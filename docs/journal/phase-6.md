# Phase 6 — WASM scheduling hooks

**Shipped:** 2026-08-01. PRs #178–#189 (design, P0–P9).
**ADR:** [0014](../architecture/0014-wasm-scheduling-policies.md), accepted.
**Plan:** `docs/phase-6-plan.md`.

An operator-supplied WebAssembly module now decides worker placement. It is
hot-swappable without a restart, hard-bounded in both work and wall-clock time,
and degrades to the built-in strategy — loudly and countably — whenever it
misbehaves.

---

## What shipped

| # | Increment | PR |
|---|---|---|
| — | Design + ADR 0014 | #178 |
| P0 | MSRV 1.85 → 1.94; `time` unpinned, one advisory suppression dropped | #179, #180 |
| P1 | One `Strategy` call per placement, not per queued slot | #181 |
| P2 | `choose_with` + `DecisionContext` + `LocalityView` | #182 |
| P3 | `LocalityIndex` — bounded per-worker completion history | #183 |
| P4 | `brokkr/v1/policy.proto` + host-side snapshot builder | #184 |
| P5 | `brokkr-policy`: the wasmtime engine | #185 |
| P6 | `WasmStrategy` + CLI flags + wiring | #187 |
| P7 | Hot reload | #188 |
| P8 | The `LocalityAware` example, operator guide, ADR accepted | #189 |
| P9 | Measurement, this retrospective | — |

## Definition of done

| # | Criterion | Result |
|---|---|---|
| 1 | A loaded policy makes real placement decisions | ✅ `a_wasm_policy_routes_a_job_to_a_worker_no_builtin_would_pick` |
| 2 | Editing the file swaps the policy with no restart; an invalid edit is refused without disturbing the running one | ✅ 4 tests in `policy_hot_reload.rs` |
| 3 | Trap / timeout / bad index each degrade to the built-in, counted, without failing a job | ✅ `a_trapping_policy_still_places_every_job` + 8 unit tests |
| 4 | `LocalityAware` exists as an example, preferring a worker warm for the same input root | ✅ `examples/policies/locality/`, 6 tests |
| 5 | p99 decision latency < 250µs; `Strategy` calls per drain are O(placements) | ✅ measured below; O(placements) pinned by a call-counting test |

### Measured — decision latency

`cargo test --release -p brokkr-control --test policy_latency -- --ignored`,
20,000 samples per row after 2,000 warm-up decisions, against the real
`LocalityAware` module.

| Strategy | Workers | mean | p50 | p99 | p99.9 |
|---|---|---|---|---|---|
| `SimpleFifo` (baseline) | 64 | 1.76µs | 1.72µs | 3.43µs | 11.06µs |
| `WasmStrategy` | 8 | 30.00µs | 28.52µs | **56.77µs** | 170.94µs |
| `WasmStrategy` | 32 | 33.54µs | 31.89µs | **63.84µs** | 219.10µs |
| `WasmStrategy` | 64 | 40.10µs | 38.12µs | **67.32µs** | 232.39µs |
| `WasmStrategy` | 128 | 56.69µs | 53.87µs | **103.59µs** | 232.02µs |
| `WasmStrategy` | 256 | 84.10µs | 81.42µs | **130.66µs** | 268.34µs |

Worst p99 is 131µs against a 250µs budget, at a fleet size well past anything
this project has run. Two things worth saying plainly:

- **A WASM decision costs roughly 20–50× a built-in one.** 1.7µs → 30µs at 8
  workers. That ratio is the honest price of the feature, and it is only
  affordable because P1 made the scheduler ask *once per placement* instead of
  once per queued slot. Without that, a 100-job queue would have been holding
  the dispatch mutex for half a second.
- **p99.9 approaches the budget at 256 workers** (268µs). Not a failure — the
  DoD is on p99 — but it is the number to watch if fleets get much larger, and
  it is why the epoch deadline exists rather than only fuel.

### Measured — CI wall-clock (R4)

| Job | Before Phase 6 | After |
|---|---|---|
| `cargo test (x86_64)` | ~2m11s | ~2m56s |
| `cargo test (aarch64)` | ~1m52s | ~2m23s |
| `cargo test`, cold cache | — | 4m49s |

Trimming wasmtime's default features took its tree from 176 crates / 46s to 101
crates / 31s. The remaining cost is cranelift, and it is the accepted price of
epoch interruption.

---

## What surprised

**The runtime choice was not the hard part; the MSRV was.** The plan's R1 asked
"is there a wasmtime that builds on our pinned 1.85?" There is — 34.0.2, with
pooling, fuel and epoch interruption all present, verified with a probe crate.
It just carries two unpatched advisories (RUSTSEC-2026-0114,
RUSTSEC-2026-0222) with no fix anywhere in the 34.x line, and the lowest version
clearing both does not build on 1.85 either. So the option the plan was written
around never actually existed, and the toolchain bump was forced rather than
chosen. Across the whole workspace it cost exactly two new clippy lints, both
genuine improvements — much cheaper than the plan feared.

**`HashMap` iteration order bit this project for the second time.** The
candidate list handed to a policy *is* the index space it returns into, and it
was being built by iterating `WorkerRegistry`'s `HashMap`. Identical cluster
state therefore placed the same job on different workers run to run — silently
defeating the exact determinism the ABI is built to guarantee. The built-ins
were unaffected because they already tie-break on worker id, which is precisely
why nobody noticed until a policy could name a *position*.

Phase 5's turmoil partition test (#174) failed for the same underlying reason.
The lesson worth carrying forward: **any collection whose iteration order
reaches an observable decision needs a deterministic order, not just a
deterministic result.**

**Two of the most valuable bugs were found by tests refusing to pass.** The
candidate-order bug surfaced as a 1-in-4 flake with the guest reporting a
*successful* decision — which is what ruled out the fallback path and pointed at
the input rather than the engine. The hot-reload seed race surfaced as two tests
that simply never saw the swap. In both cases the temptation was to loosen the
assertion; in both cases the assertion was right and the code was wrong.

**`(mtime, len)` is not a change detector.** The first hot-reload implementation
compared a stat stamp. A policy edit that swaps one constant for another changes
neither the length nor — within a filesystem's timestamp granularity, a whole
second on some — the mtime. Silently ignoring an operator's edit is the single
worst thing a hot-reload feature can do, and it would have shipped. Detection is
content-addressed now, which is what the rest of this project does anyway.

**The load-time smoke test made testing harder in a good way.** `PolicyEngine::load`
runs a synthetic decision before installing a module, so a fixture that always
traps can never be loaded — which meant the obvious "test a trapping policy"
fixture was impossible to write. The fix was not to weaken the check but to
write fixtures that pass validation and misbehave only on a real snapshot,
branching on payload length. That models the actual production case — *fine on
one input, broken on another* — far better than the fixture I originally wanted.

---

## What was decided against, and why

**Stores are not pooled.** "Pooled pre-instantiated stores" was the brief, and
the literal reading is wrong: a reused `Store` carries the previous decision's
guest heap into the next, so the same snapshot could yield different answers
depending on call history. What is pooled is the expensive part — memory slots
via `PoolingAllocationConfig`, and import resolution via `InstancePre` — with a
fresh `Store` per decision. That also dissolves the `Send + Sync` tension
entirely rather than fighting it: no `Store` is ever held, so `PolicyEngine`
contains only `Send + Sync` types.

**No WASI, no host functions, empty import set.** The guest gets no clocks,
files, sockets, or randomness. This is what makes *"same snapshot ⇒ same
decision"* a testable property rather than an aspiration, and it removes a class
of escape surface for free. A policy that needs state across decisions cannot
have it; encode the tradeoff in weights instead.

**Fuel *and* epoch, not either.** Fuel bounds work; only the epoch bounds
wall-clock time. This call happens under the dispatch mutex, so a guest that
stalls stalls the cluster — and a fuel budget cannot promise it won't. The epoch
ticker thread is the only reason the deadline is real, and it is commented as
such so nobody optimizes it away.

**Declining is not failing.** `-1` lets a policy punt on a job it does not
understand instead of guessing. It does not count toward the failure counters or
the quarantine threshold. Conflating the two would have punished exactly the
policies that are behaving well.

**Quarantine, not just fallback.** Per-decision fallback is correct but
insufficient: a policy that traps on every call would burn its full deadline
forever while dutifully logging about it. After 16 consecutive failures the
guest stops being called at all until the module is reloaded.

---

## Deferred

- **One hook only.** Admission control, preemption, retry policy and GC policy
  are all plausible next hooks and were all explicitly kept out. `brokkr-policy`
  is factored so the *next* one is cheap; that is not permission to build it.
- **No tenant-uploaded policies.** A module supplied by one tenant influencing
  another tenant's placement needs a threat model, quotas, and isolation this
  phase did not attempt.
- **No policy-visible metrics endpoint.** The failure counters exist on
  `WasmStrategy` and are exercised by tests, but nothing exposes them over RPC
  yet — that belongs with ADR 0012's observability read-model, still unstarted.
- **p99.9 at large fleet sizes** is close enough to the budget to be worth
  revisiting if anyone runs 500+ workers.

---

## Files worth knowing

| Path | What |
|---|---|
| `crates/brokkr-policy/` | the engine: load, validate, one bounded decision |
| `crates/brokkr-control/src/wasm_strategy.rs` | the adapter, and the whole failure posture in one place |
| `crates/brokkr-control/src/policy_abi.rs` | host-side snapshot projection |
| `crates/brokkr-control/src/policy_reload.rs` | hot reload |
| `crates/brokkr-control/src/locality.rs` | bounded per-worker completion history |
| `crates/brokkr-proto/protos/brokkr/v1/policy.proto` | the ABI |
| `examples/policies/locality/` | a complete working policy |
| `docs/operations/writing-a-scheduling-policy.md` | the operator guide |
