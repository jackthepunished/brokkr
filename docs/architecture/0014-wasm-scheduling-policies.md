# 0014 — WASM scheduling policies: operator-supplied, hot-reloadable `Strategy`

- **Status:** proposed
- **Date:** 2026-08-01
- **Deciders:** Brokkr maintainers
- **Supersedes in part:** [0008](0008-multi-worker-scheduling.md) — the promised
  built-in `LocalityAware` becomes an example *policy*, not an `impl Strategy`.

## Context

ADR 0008 established `Strategy::choose(candidates, loads)` as the
worker-selection seam and shipped `SimpleFifo` and `BinPacking` behind it. It
named `LocalityAware` a "later increment". That increment was never built, and
building it as a third hard-coded implementation would be a mistake.

Scheduling policy is operator-specific in a way the rest of the control plane is
not. How much input locality is worth relative to spread, whether GPU workers
should be reserved, how tight to pack before scaling down — these are properties
of a *fleet*, not of Brokkr. Answering them by editing `scheduling.rs`,
rebuilding, and redeploying the control plane is a bad iteration loop, and it
guarantees the shipped set of policies is whatever the maintainers happened to
imagine.

`docs/plan.md` §18 lists "WASM-based extension hooks. User-defined scheduling
policies." This ADR is that item.

## Decision

**Load `Strategy` from a WebAssembly module supplied by the operator, watched on
disk and hot-swappable, with a hard-bounded call and an unconditional fall back
to the built-in.**

Five sub-decisions, each of which rules something out:

1. **Operator-supplied only.** A local file path (`--policy-wasm`). No upload
   RPC, no per-tenant policies. Tenant-uploaded code influencing *other
   tenants'* placement is a different problem with a threat model we have not
   done; a file the operator places next to the binary is already inside the
   existing trust boundary.

2. **Hot-reloadable.** The file is polled and swapped without a restart. A
   policy you must restart to change is barely better than a recompile, which
   defeats the purpose.

3. **Enriched ABI with locality history.** Candidates carry worker id,
   capability labels, in-flight count, and per-worker recent-completion counters
   for the job's action digest and input root. The job carries tenant, platform
   properties, action digest and input root. A minimal ids-and-load ABI could
   only re-express `SimpleFifo`; the locality counters are what make ADR 0008's
   `LocalityAware` expressible *as a policy*, which is the proof the hook is
   real.

4. **Fall back to the built-in and count it loudly.** Trap, fuel exhaustion,
   deadline, or an out-of-range index degrade to `SimpleFifo` for that decision,
   with a `warn!` naming the reason and a per-reason counter. After 16
   consecutive failures the policy is quarantined until the file changes. A
   broken policy must not become a broken cluster — the same reasoning as Phase
   5's decision D1 about best-effort action-cache writes, and the counter plus
   log line are what keep this out of the "silent degradation" category.

5. **wasmtime**, with the pooling allocator, `InstancePre`, **and both fuel and
   epoch interruption**.

## Consequences

### Why wasmtime, and what it costs

The recommendation on the table was `wasmi` — a pure-Rust interpreter with a
tiny dependency tree and no cranelift. The decision went to wasmtime on one
property, and it is the right property:

> **Epoch interruption is the only mechanism that bounds *wall-clock* time
> inside a guest call. Fuel bounds *work*.**

`Strategy::choose` is called synchronously while the scheduler's dispatch mutex
is held. A guest that stalls stalls placement for the entire cluster. A fuel
budget cannot promise a deadline — a guest can consume very little fuel while
taking a long time. Epochs can. Given the failure posture in sub-decision 4 is
"degrade rather than fail", a *hard* time bound is what makes that posture
honest rather than aspirational.

**Dependency justification (CLAUDE.md rule 6):** *wasmtime — the only Rust WASM
runtime offering epoch-based interruption, which is the only mechanism that
bounds wall-clock time in a guest call made while the dispatch mutex is held.*

Accepted costs: a large dependency tree (cranelift et al.) through `cargo deny`,
and materially longer compile times. Mitigated by putting the runtime behind a
`wasm-policy` feature on `brokkr-policy`.

### Stores are not pooled; allocations are

`Strategy: Send + Sync`, and `wasmtime::Store` is `Send` but not `Sync`. Rather
than fight that, no `Store` is held at all:

- One `Engine` with `PoolingAllocationConfig` (memory slots recycled).
- One `InstancePre` per loaded module (imports resolved once, at load).
- A **fresh** `Store` and instance per decision.

A reused `Store` would carry the previous decision's guest heap into the next
one, making the same snapshot yield different answers depending on call history.
That destroys the determinism property the entire test strategy rests on, for a
saving the pooling allocator already provides. So `PolicyEngine` holds only
`Engine + InstancePre`, both `Send + Sync`, and the tension disappears.

### The call site had to change first

`try_dispatch` called `choose` once per pending slot per placement — O(Q²) per
drain. Harmless for a comparison-based built-in, pathological for a WASM call.
Because `choose` is documented to return `None` **iff** `candidates` is empty,
dispatchability can be decided without calling the policy at all; the policy is
then called exactly once, for the winning slot. This lands as its own change,
before any WASM code, and improves the built-ins too.

### New crate, and the DAG

`brokkr-policy` owns the runtime and knows nothing about `Strategy` —
`brokkr-control` owns the `WasmStrategy` adapter. Putting the trait in
`brokkr-policy` would have required it to depend on `brokkr-control`, which
depends on it: a cycle, and the DAG invariant is not negotiable.

### The ABI is protobuf, and there is no WASI

Protobuf field numbers mean a snapshot field added next year does not break a
module compiled today; a hand-rolled layout would either freeze the ABI or
silently misparse, and misparsing here yields *wrong placements* rather than
errors. The guest's import set is **empty** — no clocks, files, sockets, or
randomness — which is what makes "same snapshot ⇒ same decision" testable, and
removes a class of escape surface for free.

### What this does not do

One hook, for one decision. Admission control, preemption, retry policy, and GC
policy are all plausible next hooks and are all explicitly out of scope. The
engine is factored to make the *next* one cheap; that is not permission to build
it now.

## Alternatives considered

| Alternative | Why not |
|---|---|
| A third built-in `LocalityAware` (as ADR 0008 planned) | Ships one maintainer's guess at a fleet-specific tradeoff and leaves the next question needing another rebuild. |
| `wasmi` | Smaller and pure-Rust, but no epoch interruption — cannot bound wall-clock time under the dispatch lock. Rejected by the owner on exactly this ground. |
| Lua / Rhai / a config DSL | Cheaper to embed, but no memory isolation, no fuel/deadline story, and no path to policies written in a language of the operator's choosing. |
| Tenant-uploaded policies over an RPC | Untrusted code influencing other tenants' placement. Needs a threat model, quotas, and per-tenant isolation that do not exist yet. |
| Load once at startup | Kills the iteration loop that motivates the whole ADR. |
| Fail the dispatch on policy error | Makes an operator's policy bug into the tenants' outage. |

## References

- `docs/phase-6-plan.md` — the implementation plan (sequencing P0–P9).
- [0008](0008-multi-worker-scheduling.md) — the `Strategy` seam this extends.
- `docs/plan.md` §18 — "WASM-based extension hooks."
