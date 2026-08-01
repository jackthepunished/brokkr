# Writing a scheduling policy

Brokkr can hand every worker-placement decision to a WebAssembly module you
supply. This is [ADR 0014](../architecture/0014-wasm-scheduling-policies.md).

Why you would want to: how much a warm cache is worth relative to an idle
worker, whether GPU machines should be held back, how tightly to pack before
scaling down — these depend on your fleet, not on Brokkr. Editing
`scheduling.rs` and redeploying the control plane to answer them is a bad loop.

---

## Quick start

```sh
rustup target add wasm32-unknown-unknown
cd examples/policies/locality
cargo build --release --target wasm32-unknown-unknown

brokkr-control \
  --policy-wasm examples/policies/locality/target/wasm32-unknown-unknown/release/brokkr_policy_locality.wasm
```

Edit the weights at the top of `examples/policies/locality/src/lib.rs`, rebuild,
and the control plane picks it up within `--policy-reload-interval-secs`
(default 5). **No restart.**

---

## What a policy must export

Three functions and a memory:

| Export | Signature | Purpose |
|---|---|---|
| `brokkr_abi_version` | `() -> i32` | Must return `1`. Checked at load; a mismatch is refused. |
| `brokkr_alloc` | `(i32) -> i32` | Return a pointer to `len` writable bytes. |
| `brokkr_choose` | `(i32 ptr, i32 len) -> i32` | Return a candidate index, or `-1`. |
| `memory` | — | Your linear memory, exported. |

The host calls `brokkr_alloc(len)`, writes an encoded
`brokkr.v1.DecisionSnapshot` at the returned pointer, then calls
`brokkr_choose(ptr, len)`.

**You never need to free.** The host drops the whole store after every call, so
the entire linear memory is reclaimed at once. Leaking in `brokkr_alloc` is
correct, not sloppy.

### Return values

| Return | Meaning |
|---|---|
| `0 .. candidates.len()` | Place the job on that candidate. |
| `-1` | **Decline** — no preference, use the built-in for this decision. |
| anything else | A failure. Counted as `bad_index`, and the built-in decides. |

`-1` matters. A policy that does not recognise a job should say so rather than
guess; declining is **not** counted as a failure and does not contribute to
quarantine.

---

## What you get

The snapshot is defined in
[`crates/brokkr-proto/protos/brokkr/v1/policy.proto`](../../crates/brokkr-proto/protos/brokkr/v1/policy.proto).
Protobuf, so a field added in a later release will not break a module you
compiled today.

```proto
message DecisionSnapshot {
  uint32          abi_version = 1;
  PolicyJobFacts  job         = 2;
  repeated PolicyCandidate candidates = 3;   // never empty
}

message PolicyJobFacts {
  string tenant             = 1;
  string action_digest      = 2;   // lowercase hex sha256
  string input_root_digest  = 3;   // empty if the action has no input root
  repeated PolicyPlatformProperty platform = 4;
}

message PolicyCandidate {
  string worker_id                 = 1;
  uint32 inflight                  = 2;   // dispatched but not yet reported
  repeated PolicyPlatformProperty labels = 3;
  uint32 input_root_hits           = 4;   // recent completions on this input root
  uint32 action_hits               = 5;   // recent completions of this exact action
}
```

Two things worth internalising:

- **Candidates are already filtered.** Every one of them is connected, idle, and
  satisfies the action's platform constraints. You are choosing *which*, never
  *whether*.
- **Candidate order is the index space, and it is sorted by worker id.** The
  order is stable for identical cluster state, which is what makes a policy's
  behaviour reproducible.

`input_root_hits` and `action_hits` come from a bounded per-worker window
(`--policy-locality-window`, default 64 completions). They answer *"recently,
how often?"* — not *"ever"*.

---

## What you do **not** get

No WASI. No clocks, files, sockets, randomness, or host functions of any kind —
the import set is empty.

That is deliberate, and it buys the property everything else rests on: **the
same snapshot always produces the same decision**. A policy that could read a
clock would be untestable and unreproducible, and you would never be able to
answer "why did it place that job there?"

If you need state across decisions, you cannot have it. Encode the tradeoff in
the weights instead.

---

## Budgets

Your policy runs while the scheduler holds its dispatch mutex, so a policy that
stalls stalls placement for the whole cluster. Two independent bounds:

| Flag | Default | Bounds |
|---|---|---|
| `--policy-fuel` | 1,000,000 | **Work.** Catches an accidental O(n²). |
| `--policy-deadline-ms` | 5 | **Wall-clock time.** The one that actually protects the cluster. |

Both are per decision. Fuel cannot bound time — a guest can consume very little
fuel while taking a long time — which is why there are two.

Keep `brokkr_choose` to a single pass over the candidates. Anything that looks
like a nested loop over candidates is already too much.

---

## When your policy misbehaves

**A broken policy must not become a broken cluster.** Every runtime failure
degrades to the built-in `SimpleFifo` for that one placement, logs a `warn` with
the reason, and increments a counter for that reason:

| Reason | What it means |
|---|---|
| `trap` | Your policy trapped — `unreachable`, an out-of-bounds access, a panic. |
| `fuel_exhausted` | Too much work for one decision. Raise `--policy-fuel` or simplify. |
| `deadline` | Too much wall-clock time. Often means the host is oversubscribed rather than that the policy is slow. |
| `bad_index` | Returned an index that is neither a candidate nor `-1`. |
| `memory` | `brokkr_alloc` returned something unusable, or `memory` is not exported. |
| `instantiate` | The module could not be instantiated for this decision. |
| `not_loaded` | No policy is loaded — a misconfiguration, not a broken policy. |
| `quarantined` | See below. |

**Quarantine.** After 16 consecutive failures
(`--policy-quarantine-threshold`) the policy stops being called at all and the
built-in serves directly. Falling back per decision is not enough on its own: a
policy that traps on every call would otherwise burn its full deadline forever
while dutifully logging about it.

Any success clears the streak, including a decline. **Reloading the module
clears quarantine** — that is the fix path.

---

## When your policy will not load

Load-time failures are loud, and the **currently running policy keeps serving**.
A bad edit costs you a log line, not your scheduler.

The control plane refuses a module that:

1. does not compile,
2. is missing a required export, or has one with the wrong signature,
3. reports an ABI version other than `1`,
4. fails a synthetic two-candidate decision run inside the normal budgets.

(4) is what stops a module that traps on its first call from ever going live.

The one asymmetry worth knowing: naming a `--policy-wasm` that cannot be loaded
**at startup** is a fatal startup error, not a degradation. You said you wanted
a policy; silently running `SimpleFifo` because you misspelled the path is the
kind of quiet misconfiguration this project does not ship. Once the process is
up and jobs are queued, degrading beats stopping.

---

## Hot reload

The file is polled every `--policy-reload-interval-secs` (default 5; `0`
disables). Change detection is by **content digest**, not modification time —
an edit that swaps one constant for another changes neither the file length nor,
within a filesystem's timestamp granularity, its mtime, and silently ignoring
that edit would be the worst possible behaviour for an iteration loop.

- Valid module → swapped in, logged at `info`, quarantine cleared.
- Invalid module → logged at `error`, previous policy keeps serving, and it is
  not recompiled every interval (fix it and it retries).
- Deleted or unreadable file → warned once, previous policy keeps serving.
  Deleting the file is not an instruction to stop scheduling.

---

## Writing one in a language other than Rust

Nothing here is Rust-specific. Any toolchain producing a core WebAssembly module
with those three exports, an exported `memory`, and no imports will work. You
will need a protobuf decoder for your language, or you can hand-decode the few
fields you care about — protobuf's wire format is simple, and the snapshot is
flat by design.

---

## Trust model

Policies are **operator-supplied**: a file you place next to the binary, inside
the trust boundary you already own. There is no upload RPC and no per-tenant
policy, because a tenant-supplied module influencing *other tenants'* placement
needs a threat model, quotas, and isolation that do not exist yet.

The sandbox is still real — no imports, bounded memory, bounded fuel, bounded
time — but it is defence in depth around code you chose to run, not a boundary
against an adversary you invited in.

---

## See also

- [ADR 0014](../architecture/0014-wasm-scheduling-policies.md) — the decision and its alternatives
- [ADR 0008](../architecture/0008-multi-worker-scheduling.md) — the `Strategy` seam this extends
- `docs/phase-6-plan.md` — design notes and measurements
- `examples/policies/locality/` — a complete, working policy
