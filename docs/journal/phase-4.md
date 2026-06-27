# Phase 4 — Scheduler & Multi-Tenancy

- **Status:** in progress
- **Plan:** `docs/plan.md` §16
- **Started:** 2026-06-27

Goal: real scheduling across many workers, fair sharing across tenants,
and REAPI-compatibility good enough to run a real Bazel build. This
journal accumulates a short retrospective per increment.

The Phase 3 "Deferred to Phase 4" list (`docs/journal/phase-3.md`) is the
backlog seed alongside the §16 task list: worker registry + capabilities,
constraint matching, scheduling strategies (SimpleFifo → BinPacking →
LocalityAware), job leases, tenants/quotas, fair scheduling, auth, Bazel
compat.

## I1 — worker registry (§16 task 1, data model)

- **Date:** 2026-06-27
- **Affected crate:** `brokkr-control`
- **Outcome:** `brokkr-control::registry::WorkerRegistry` — the
  in-memory liveness + capability store the rest of Phase 4 scheduling
  builds on. Records `WorkerCapabilities { hostname, labels }` and a
  `last_seen` instant per worker; evicts workers that miss
  `HeartbeatPolicy::max_missed` heartbeats (default 5 s × 3 = 15 s
  deadline, straight from §16 task 1). Ten unit tests, full workspace
  clippy/test green.

### Decisions

- **Injected clock, not `Instant::now()`.** Every time-sensitive method
  (`register`, `record_heartbeat`, `evict_stale`, `healthy`,
  `is_stale`) takes an explicit `now: Instant`. This is the testing
  strategy's "no `SystemTime::now()` in tests; use injected clocks" rule
  applied at the type level — eviction-boundary tests pin `t0` once and
  add `Duration`s, so they're deterministic and need no sleeps. The
  control-plane RPC layer will pass `Instant::now()` at the edge.
- **Registry is not internally synchronized.** It's a plain `&mut self`
  data structure; the control plane wraps it in its own
  `Mutex`/`RwLock`. Keeping the lock out of the registry makes the
  eviction logic trivially unit-testable and avoids guessing the
  concurrency shape before the RPC wiring exists.
- **Capabilities start minimal.** `WorkerCapabilities` mirrors the
  surface the existing `RegisterWorkerRequest` proto already carries
  (hostname + `labels`). The richer `WorkerCapability` sketch in plan §8
  (CPU cores, memory, installed tools, GPU) is deferred until the
  constraint matcher actually needs each field — no speculative schema.
- **`BTreeMap` for labels.** Deterministic iteration/equality; the
  constraint matcher will want a stable order, and it makes
  `WorkerCapabilities: Eq` meaningful.
- **`healthy()` is read-only; `evict_stale()` mutates.** Two distinct
  needs: the scheduler wants to *skip* stale workers when picking
  (without taking a write lock), while a periodic reaper wants to
  *remove* them. Splitting the two keeps the read path lock-light.
- **Strictly-greater staleness.** A worker exactly at the deadline is
  still alive; only `elapsed > deadline` evicts. Boundary pinned by a
  test (`worker_within_deadline_is_not_stale_or_evicted`).

### Scope note

This increment is the data model only. Wiring it into
`WorkerServiceImpl::register` (currently mints a throwaway UUID and
drops the hostname/labels), adding a heartbeat RPC + a background
eviction tick, and teaching the scheduler to select from `healthy()`
are the next increments.

### Environment note

The dev host is Windows; `brokkr-sandbox`/`brokkr-worker` are Linux-only,
so the workspace is built and tested under WSL2 Ubuntu (cargo 1.85) with
a Linux-native `CARGO_TARGET_DIR`. Verification is **per changed crate**
(`brokkr-control`: fmt + `clippy --all-targets -D warnings` + test all
green), relying on the project's real Linux CI for the rest: a full
`cargo test --workspace` is *not* green on this WSL2 host — ~6
`brokkr-sandbox` seccomp arg-filter tests (`PR_SET_TSC`/ioctl/RDTSC)
can't trap under the virtualized kernel, and a few upstream
`brokkr-worker` files trip rustfmt's CRLF check. Neither is touched by
this work. (Lesson carried from PR #96: confirm reality on `origin/main`,
not just the working tree.)

## I2 — register persists into the registry (§16 task 1, wiring)

- **Date:** 2026-06-27
- **Affected crate:** `brokkr-control`
- **Outcome:** `WorkerServiceImpl.register` now writes into a shared
  `WorkerRegistry` instead of throwing the worker's identity away. It
  records hostname + labels as `WorkerCapabilities`, and advertises a
  `heartbeat_seconds` taken from the registry's policy interval (5 s)
  rather than the old hardcoded 30 — closing the gap where the server
  told workers to heartbeat every 30 s but the eviction policy expected
  every 5 s. Two handler tests; `brokkr-control` fmt/clippy/test green.

### Decisions

- **`SharedWorkerRegistry = Arc<Mutex<WorkerRegistry>>`.** The registry
  is now mutated from a request handler and (soon) a background tick, so
  it needs shared ownership + interior mutability. An async
  `tokio::sync::Mutex` is right because the critical section is tiny
  (insert / read policy) and never held across an `.await` other than
  the lock itself. `with_registry` + `registry()` expose the handle so
  the next increments (heartbeat RPC, eviction loop) and tests share one
  source of truth; `new(scheduler)` keeps its single-arg shape so
  `main.rs` and the Phase 1 fixture are untouched.
- **Advertise the policy interval, not a magic number.** Tying
  `heartbeat_seconds` to `policy().interval` means the value the worker
  is told and the value the eviction deadline assumes can't drift apart.
- **`#[tracing::instrument]` over a manual `span.enter()`.** The handler
  now `.await`s the registry lock; holding an `Entered` guard across an
  await is the classic tracing footgun. `instrument` wraps the whole
  future correctly and records the assigned `worker_id` field.
- **Worker id still server-minted (UUID).** Registration identity stays
  server-assigned; the heartbeat RPC (next) will let a worker prove
  liveness with that id and get a re-register signal on
  `UnknownWorker`.

### Next increment

Heartbeat RPC: add `Heartbeat` to `brokkr.v1.worker.proto`
(`HeartbeatRequest { worker_id }` → `HeartbeatResponse { known }`),
handle it via `registry.record_heartbeat`, and have the worker call it
on the advertised cadence. Then a background eviction tick that calls
`evict_stale` on an interval. (Proto change → first increment of Phase 4
that regenerates `brokkr-proto`.)

## I3 — heartbeat RPC (§16 task 1, liveness ping)

- **Date:** 2026-06-27
- **Affected crates:** `brokkr-proto`, `brokkr-control`
- **Outcome:** `brokkr.v1.WorkerService.Heartbeat`
  (`HeartbeatRequest{worker_id}` → `HeartbeatResponse{known}`) plus its
  control-plane handler. `heartbeat` refreshes the worker's `last_seen`
  through `WorkerRegistry::record_heartbeat`; the response's `known` flag
  tells the worker whether the control plane still has a record of it.
  Three handler tests; `brokkr-control` + `brokkr-proto` fmt/clippy/test
  green.

### Decisions

- **Unknown worker is `known=false`, not an error.** A heartbeat from an
  evicted (or never-registered) worker is an expected, recoverable
  state, not a fault. Returning `known=false` gives the worker a clean
  "re-register" signal; returning a gRPC error would make it retry a
  dead identity or backoff for the wrong reason. Only a *malformed*
  request (missing `worker_id`) is `INVALID_ARGUMENT`.
- **No new proto message churn.** Reused the existing `WorkerId` message
  for the request; the response is a single bool. Resisted adding a
  server-suggested next-interval field — the cadence is already fixed at
  register time, and re-negotiating it per heartbeat is YAGNI until
  there's a reason to vary it.
- **Server-side only this increment.** The RPC is implemented and tested
  via direct handler calls; no worker calls it yet. Splitting the
  transport/handler (here) from the worker's send-loop + the background
  eviction tick (next) keeps each unit independently testable and the
  diffs small. The proto addition is backward-compatible — existing
  clients that never call `Heartbeat` are unaffected.

### Next increment (I4)

Worker-side: a heartbeat send-loop in `brokkr-worker` that pings on the
advertised `heartbeat_seconds` and re-registers on `known=false`. Control
side: a background eviction task (`tokio::time::interval` →
`evict_stale`) wired into the `brokkr-control` binary. Target: an
end-to-end test where a worker that stops heartbeating is evicted after
the deadline. This closes §16 task 1.

## I4 — eviction tick + worker heartbeat loop (§16 task 1 — CLOSED)

- **Date:** 2026-06-27
- **Affected crates:** `brokkr-control`, `brokkr-worker`
- **Outcome:** Liveness loop is now closed end-to-end.
  `spawn_eviction_task` drives `WorkerRegistry::evict_stale` once per
  heartbeat interval and is wired into the control-plane binary; the
  worker runs a background loop pinging `WorkerService.Heartbeat` on the
  advertised cadence. A worker that keeps heartbeating stays registered;
  one that stops is evicted after `interval * max_missed`, and its next
  heartbeat returns `known=false`. `brokkr-control` (25 lib tests) +
  `brokkr-worker` green; the existing end-to-end fixture now exercises
  live heartbeats through the real server.

### Decisions

- **Reaper reads the policy interval, ticks once per interval.** Eviction
  lag is bounded to one interval; the *deadline* enforcement stays in
  `evict_stale`. A zero interval disables the reaper instead of panicking
  (`tokio::time::interval` rejects a zero period).
- **`known=false` ⇒ stop, don't silently spin.** The worker's heartbeat
  loop breaks on `known=false` and logs; full re-registration (re-open
  the job stream under a new id) is left as `TODO(brokkr-410)` — it's a
  reconnect-state-machine change that deserves its own increment rather
  than being smuggled into the heartbeat loop.
- **No `tokio` `test-util` dependency.** A paused-clock test of the
  spawned reaper would need `tokio`'s `test-util` feature (not in
  `full`). Rather than add a dep mid-loop, the spawn wrapper is covered
  by (a) `evict_stale`'s injected-clock unit tests and (b) the
  deterministic `eviction_is_observable_via_heartbeat` composition test
  that drives register → evict → heartbeat through the RPC handlers with
  no timers. The wrapper itself is ~15 lines of obvious glue.
- **Heartbeat task aborted on stream exit.** The worker holds the
  `JoinHandle` and aborts it when the job stream closes, so the heartbeat
  loop never outlives the worker session.

### §16 task 1 status

Done across I1–I4: registry data model → register wiring → `Heartbeat`
RPC → eviction tick + worker heartbeat loop. All on PR #98. **Next
milestone:** §16 task 2 — constraint matching (match an Action's
`Platform` requirements against `WorkerCapabilities.labels`; hard vs.
soft constraints), as a new branch off `origin/main` once #98 merges
(else stacked).

## I5 — platform constraint matching (§16 task 2, matcher)

- **Date:** 2026-06-27
- **Affected crate:** `brokkr-control`
- **Outcome:** `brokkr-control::matching` — the eligibility primitive the
  scheduler will use to pick workers. `labels_satisfy_platform` /
  `worker_satisfies` implement REAPI matching (every required
  `Property{name,value}` must be advertised; empty platform matches all),
  and `eligible_workers(registry, now, platform)` yields the live workers
  that also satisfy the constraints. First increment of task 2; #98
  (task 1) merged, so this is a fresh branch off `main`. Six unit tests;
  `brokkr-control` fmt/clippy/test green (31 lib tests).

### Decisions

- **Matcher lives outside `registry`.** The registry stays proto-free (a
  plain liveness/capability store); the proto-aware matching is its own
  module that depends on both `registry` and `brokkr-proto`. Same
  decoupling Phase 3 drew between `brokkr-cas::ring` and the proto — it
  keeps the registry's unit tests free of proto fixtures and avoids
  leaking `i32`-encoded proto enums into the capability model.
- **Hard constraints only; soft is deferred (needs an ADR).** Plan §16
  asks for hard vs. soft constraints, but REAPI's `Platform` has no soft
  notion — modelling "preferred" needs a Brokkr-specific convention
  (e.g. a reserved property-name prefix, or a `brokkr.v1` extension).
  Rather than invent wire semantics inline, I5 ships REAPI-faithful hard
  matching and flags soft as a follow-up ADR. Documented in the module.
- **Single-valued labels ⇒ duplicate-name requirements are
  unsatisfiable.** `WorkerCapabilities.labels` is `BTreeMap<_,_>` (one
  value per name). A platform requiring `os=linux` *and* `os=windows`
  can't be met by any single worker — the correct outcome for
  single-valued attributes. Multi-valued worker capabilities (a worker
  advertising several values for one name) would need a richer model;
  deferred until a workload needs it, with a test pinning the current
  behaviour.
- **`eligible_workers` composes `healthy()` + the matcher.** Eligibility
  = live AND satisfies-constraints. Returning an iterator (not a Vec)
  keeps it allocation-free for the scheduler's pick path.

### Next increment (I6)

Teach the scheduler to use `eligible_workers`. The current scheduler is
single-worker (one queue, one stream); I6 introduces capability-aware
*selection* — extract the action's `Platform` (from `Command.platform` /
`Action.platform`) and pick an eligible worker — as the data-model step
toward multi-worker dispatch. Full multi-worker fan-out (multiple
concurrent streams) is a later increment; I6 should be the smallest step
that makes selection constraint-aware without rewriting dispatch.

## I6 — constraint-aware admission control (§16 task 2)

- **Date:** 2026-06-27
- **Affected crate:** `brokkr-control`
- **Decision taken with the owner:** "admission control first" (vs. a
  full multi-worker dispatch redesign or pausing). So I6 uses the matcher
  to *reject* un-runnable actions without restructuring dispatch.
- **Outcome:** `Scheduler::execute` now consults a shared `WorkerRegistry`
  (when wired in) and returns the typed `ExecutionError::NoEligibleWorker`
  → gRPC `FAILED_PRECONDITION` if no live worker satisfies the action's
  platform, instead of enqueuing a job that no worker can claim. The
  binary builds one registry shared by the scheduler (reads), the worker
  service (writes), and the eviction reaper. Three scheduler tests;
  `brokkr-control` green (34 lib tests).

### Decisions

- **Admission, not routing.** Delivery stays single-queue/single-worker;
  the matcher only gates *admission*. This is the smallest step that puts
  the matcher to work and gives clients a fast, correct
  `FAILED_PRECONDITION` instead of a 30-minute timeout. Real per-worker
  routing + scheduling strategies (task 3) is the redesign that follows.
- **Opt-in via constructor, off by default.** `new` /
  `with_execution_timeout` leave the registry `None` (admission skipped),
  so the Phase-1 in-process fixtures and unit tests are unchanged; the
  binary opts in with `with_worker_registry`. Avoids a flag-day behaviour
  change in tests while making production fail-fast.
- **Check after the cache lookup, before enqueue.** A cache hit needs no
  worker, so admission runs only on the dispatch path, after the
  Action/Command are fetched (we need the platform) and before the job is
  queued.
- **Deprecated `Command.platform` fallback, scoped allow.** REAPI v2.2
  moved platform to `Action.platform`; older clients still set
  `Command.platform`. We accept both with a one-line `#[allow(deprecated)]`
  rather than dropping v2.0 compatibility.

### Known gap → next increment (I7)

Admission control is now live in the binary, but the CLI worker still
registers with **empty labels** (`run_worker` sends
`labels: Default::default()`). So an action with any platform constraint
will be rejected in production until the worker advertises its real
capabilities. I7: have `brokkr-worker` advertise `os` / `arch` (and
configurable labels) at registration so constrained actions can actually
be scheduled. Actions with no platform requirements are unaffected (empty
platform matches any healthy worker).

## I7 — worker advertises os/arch capabilities (§16 task 2 — CLOSED)

- **Date:** 2026-06-27
- **Affected crate:** `brokkr-worker`
- **Outcome:** `run_worker` now registers with `os` / `arch` labels from
  `std::env::consts`, so the matcher + admission control chain works
  end-to-end: worker advertises capabilities → control plane stores them
  → constrained actions are matched and either placed or rejected with
  `NoEligibleWorker`. One unit test on the label helper;
  `brokkr-worker` green.

### Decisions

- **Auto-detect, no config-surface change.** Populated the labels
  directly in `run_worker` via a small `default_capability_labels()`
  helper rather than adding a `labels` field to `WorkerConfig`. Keeps the
  two struct-literal construction sites (the CLI binary and the in-process
  test fixture) untouched, and `os`/`arch` are the labels actions most
  commonly constrain on. Configurable / richer capabilities (installed
  tools, GPU, RAM — plan §6.3 / §8 `WorkerCapability`) are deferred to a
  later increment when a workload needs them.
- **Extracted a testable helper.** `run_worker` itself needs a live
  server to exercise, so the label logic is a standalone fn with a unit
  test; the flow into the registry is already covered by the control-plane
  handler test `register_persists_capabilities_into_registry`.

### §16 task 2 status

Done across I5–I7: matcher (`brokkr-control::matching`) → admission
control in the scheduler → worker capability advertisement. All on
PR #99. Hard-constraint matching is complete end-to-end. **Deferred:**
soft/preferred constraints (needs a Brokkr convention — future ADR);
richer worker capabilities; per-worker *routing* (multi-worker dispatch)
and scheduling strategies are §16 task 3, the next milestone — that's the
multi-worker redesign the I6 decision deferred.

## I8 — multi-worker scheduling foundation (§16 task 3, ADR 0008)

- **Date:** 2026-06-27
- **Affected crate:** `brokkr-control` (+ ADR 0008)
- **Decision taken with the owner:** dispatch model = **per-worker queues
  with submit-time routing** (vs. a central dispatcher, or a
  strategy-only first step). Recorded in
  `docs/architecture/0008-multi-worker-scheduling.md`.
- **Outcome:** `brokkr-control::scheduling` — the policy + connection
  data model for multi-worker dispatch, with no dispatch plumbing yet so
  it's fully unit-testable: a `Strategy` trait + `LoadView`, a
  `SimpleFifo` strategy (least-loaded candidate, deterministic id
  tie-break), and `ConnectedWorkers` (per-worker job channel + in-flight
  count, distinct from `WorkerRegistry`). Eight unit tests;
  `brokkr-control` green.

### Decisions

- **Per-worker queues, route at submit (ADR 0008).** On `execute` the
  scheduler will compute eligible candidates (matcher ∩ connected), let
  the `Strategy` pick one, and send the job to that worker's channel.
  Chosen over a central pending-queue dispatcher (deferred to task 4,
  where leases + fair scheduling want a global queue) and over a
  pull/work-stealing model (rejected — fights server-side matching + the
  push bidi stream).
- **`ConnectedWorkers` ≠ `WorkerRegistry`.** Connection (stream open) and
  liveness/capability (registered + heartbeating) are separate lifecycles;
  the scheduler consults both. Keeping them in separate types avoids
  conflating "known" with "reachable right now".
- **`SimpleFifo` = least-loaded, not literally first.** "First available
  worker" with no capacity cap would always pick `candidates[0]` and never
  spread load. Least-loaded (stateless, deterministic tie-break) is the
  simplest strategy that actually balances; documented in ADR 0008 that
  per-worker FIFO ordering is preserved by each worker's own channel.
- **Foundation split from wiring.** I8 lands the trait + registry +
  strategy with unit tests; I9 does the riskier scheduler/worker-service
  rewrite (replace the single `take_receiver` queue with per-worker
  channels + routing). Both land on the task-3 PR.

### Next increment (I9)

Wire it in: `WorkerService.Stream` registers a per-worker channel in
`ConnectedWorkers` on connect (and removes on disconnect) instead of the
single `take_receiver`; `Scheduler::execute` routes via
matcher → `Strategy::choose` → that worker's channel, tracking in-flight
counts; results decrement in-flight. Plus an integration test with two
connected workers proving jobs spread and constraints route correctly.

## I9 — multi-worker dispatch wiring (§16 task 3)

- **Date:** 2026-06-27
- **Affected crate:** `brokkr-control`
- **Outcome:** The dispatch core is rewritten for per-worker routing. The
  single shared queue (`Scheduler::take_receiver`) is gone;
  `WorkerService.Stream` registers a per-worker channel in the shared
  `ConnectedWorkers` keyed by the `Hello` worker id, and
  `Scheduler::execute` routes each action to a `Strategy`-chosen eligible
  connected worker, tracking in-flight load. 43 lib tests green; the
  real-gRPC `end_to_end` test still passes (proving the new
  Hello→connect→route→report path), plus a two-worker spread test.

### Decisions

- **Scheduler owns `ConnectedWorkers`; the worker service borrows it.**
  `WorkerServiceImpl::new(scheduler)` reads `scheduler.connected_workers()`,
  so the binary and the fixtures don't have to wire a third shared handle —
  one accessor keeps the scheduler and stream pointed at the same map.
- **Worker id comes from `Hello`, registration happens in the pump.** The
  stream handler must return the outbound stream synchronously, but the
  worker id is only known once the first inbound message arrives. So the
  spawned pump reads `Hello`, *then* registers the per-worker channel and
  spawns the outbound forwarder. A first message that isn't a valid
  `Hello` closes the stream.
- **In-flight inc/dec straddles the wait, owned by `execute`.** Increment
  under the same `connected` lock as selection (so concurrent submits
  can't both pick the one idle worker); decrement once after the result
  *or* timeout resolves (an inner `async` block funnels every early
  return through one decrement). The inbound pump does **not** touch
  in-flight — keeps the accounting single-owner and race-free.
- **Lock order: registry then connected, never both held.** `execute`
  snapshots registry-eligible ids (releasing the registry lock) before
  taking the `connected` lock, so the two mutexes are never held at once
  — no lock-ordering deadlock against `register` (registry only) or the
  stream (connected only).
- **Fail-fast when no eligible connected worker.** No global pending
  queue yet (ADR 0008 → task 4), so a submit with nothing to run on
  returns `NoEligibleWorker` immediately. The in-process fixtures connect
  their worker well within the existing readiness window, so the
  real-gRPC e2e stays deterministic.
- **Disconnect drops in-flight jobs to timeout.** When a worker's stream
  ends, it's removed from `ConnectedWorkers`; its in-flight jobs' waiters
  time out via the scheduler timeout. Lease-based reassignment to another
  worker is task 4.

### §16 task 3 status

I8 (foundation) + I9 (wiring) deliver multi-worker dispatch with the
`SimpleFifo` strategy on PR #100. **Next:** `BinPacking` and
`LocalityAware` strategies (I10), then §16 task 4 (job leases,
tenants/quotas, fair scheduling — where the global queue + lease-based
reassignment land). A full two-process / two-worker gRPC integration test
is also worth adding once a second strategy gives it more to assert.

## I10 — BinPacking strategy + selectable strategy (§16 task 3)

- **Date:** 2026-06-27
- **Affected crate:** `brokkr-control`
- **Outcome:** A second selection strategy, `BinPacking`, behind the
  existing `Strategy` trait, plus `Scheduler::with_strategy` so the
  binary can choose. `BinPacking(cap)` packs the most-loaded worker still
  under `cap` (then falls back to least-loaded when all are saturated),
  versus `SimpleFifo`'s always-spread. Six unit tests + a scheduler test
  proving the injected strategy is honoured. 50 lib tests green.

### Decisions

- **BinPacking fits the existing `choose(candidates, loads)` signature.**
  It only needs per-worker load (from `LoadView`) + a `cap`, so no trait
  change. The cap is a *soft* target: when everyone is at/over cap it
  still places work (least-loaded fallback) rather than refusing — a hard
  admission limit is a task-4 concern (backpressure/queueing).
- **`cap` clamped to ≥1.** A 0 cap would send every candidate to the
  fallback (degenerate spread); clamping keeps the knob meaningful.
- **Selectable via a constructor, not a config flag yet.**
  `Scheduler::with_strategy` takes `Arc<dyn Strategy>`; the existing
  constructors keep defaulting to `SimpleFifo` (no call-site churn). A CLI
  flag to pick the strategy at runtime is a small follow-up once there's a
  reason to flip it in the binary.
- **`LocalityAware` deferred — needs a trait change.** "Prefer a worker
  that recently ran overlapping inputs" requires `choose` to see the
  action's input-root digest (the current signature only passes load) and
  per-worker recent-input state. Rather than speculatively widen the trait
  now, it gets its own increment that designs the locality-hint plumbing
  (and possibly a small ADR). Flagged here so it isn't silently dropped.

### §16 task 3 status

Multi-worker dispatch with two strategies (`SimpleFifo`, `BinPacking`)
shipped (I8–I10). Remaining under "scheduling strategies": `LocalityAware`
(own increment). **Next milestone:** §16 task 4 — job leases (incl.
reassigning a disconnected worker's in-flight jobs, deferred from I9),
tenants/quotas, fair scheduling; this is where ADR 0008's deferred global
pending queue lands, likely with its own ADR.
