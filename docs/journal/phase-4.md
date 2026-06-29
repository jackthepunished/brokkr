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

## I11 — leases + global queue ADR + LeaseTable (§16 task 4)

- **Date:** 2026-06-27
- **Affected crate:** `brokkr-control` (+ ADR 0009)
- **Decision taken with the owner:** task 4 starts with **global queue +
  leases + crash-reassignment** (the §16 DoD), over tenants/quotas first
  or finishing `LocalityAware`.
- **Outcome:** ADR 0009 records the design — a global pending queue
  drained by an event-driven dispatcher, time-bounded job leases, and
  requeue-on-failure (lease expiry *or* worker disconnect → another
  worker), at-least-once made safe by Brokkr's determinism axiom. The
  first code increment lands `lease::LeaseTable<P>`, the pure lease
  bookkeeping core, clock-injected and generic over the re-dispatch
  payload. Seven unit tests; `brokkr-control` green.

### Decisions

- **Worker capacity = 1 leased job (ADR 0009).** The worker control loop
  already runs actions serially, so the scheduler leases one job per
  worker and treats it busy until report/expiry/disconnect. Per-worker
  parallelism is a later knob.
- **`LeaseTable` carries the re-dispatch payload.** A lease holds enough
  to re-queue the job if it fails, so reassignment needs no separate
  in-flight map. Generic `<P>` keeps the bookkeeping unit-testable
  without the scheduler's `bv1::Job`/waiter types.
- **`complete` returns `Option<P>`.** A late report after expiry/
  reassignment finds no lease → `None` → caller discards it. This is the
  at-least-once seam: the retry's result is the one that counts.
- **`take_expired` / `take_worker` return sorted `(job_id, payload)`.**
  Deterministic requeue order for logging/tests; `take_worker` is the
  disconnect-reassignment path (the I9-deferred crash recovery).
- **Inclusive expiry (`now >= deadline`).** Matches intuition for "due
  by"; pinned by a test. (The registry uses strictly-greater for
  staleness — different semantics, deliberately: a missed *heartbeat*
  tolerates the exact boundary, a *lease* deadline does not.)
- **Foundation split from wiring.** I11 = ADR + `LeaseTable` (tested);
  I12 = the dispatcher/`execute` rewrite (pending queue, lease on
  dispatch, requeue on expiry/disconnect, crash-reassignment integration
  test). The dispatcher re-touches the scheduler core, so it's staged
  behind the reviewable ADR.

### Next increment (I12)

The dispatcher rewrite: `execute` enqueues (waiter survives retries) +
`try_dispatch` assigns queued jobs to idle eligible workers, leasing each;
`report` completes the lease and re-dispatches; worker disconnect / lease
expiry requeue via `take_worker` / `take_expired`. Headline test: a worker
that disconnects mid-job → the job is reassigned to a second worker and
completes (the §16 DoD).

## I12 — dispatcher rewrite: global queue + leases + crash reassignment (§16 task 4, DoD)

- **Date:** 2026-06-28
- **Affected crate:** `brokkr-control`
- **Outcome:** The §16 DoD is met — *worker crash mid-job → job retried on
  another worker, completes*. The scheduler now enqueues every action onto
  a global pending queue, an event-driven `try_dispatch` leases each job to
  an idle eligible worker, `report` completes the lease + re-dispatches,
  and **worker disconnect requeues the worker's in-flight job for
  reassignment**. 59 lib tests + the real-gRPC `end_to_end` + the 200-RPC
  `phase1_dod` soak all green through the new dispatch.

### Decisions

- **Single dispatch mutex (`Inner` = connected + pending + leases).**
  Folded the three pieces of dispatch state under one lock so every
  routing decision is atomic and there's *no inter-lock ordering to get
  wrong*. The earlier worry (registry↔connected↔pending↔leases ordering)
  evaporates: registry is locked before `Inner` (registry is only ever
  locked alone elsewhere), and `waiters` is always acquired alone.
- **Worker connect/disconnect moved onto the scheduler.** `ConnectedWorkers`
  is now private inside `Inner`; `WorkerService.Stream` calls
  `connect_worker` / `disconnect_worker` instead of poking a shared handle.
  `disconnect_worker` is the crash-recovery entry point — it `take_worker`s
  the dead worker's leases and requeues them.
- **Capacity-1 ⇒ BinPacking degenerates to SimpleFifo (documented).** A
  worker holds at most one lease, so it's busy-excluded from candidates
  after one job; load is always 0 among candidates and the two strategies
  coincide. Spreading now emerges from *busy-exclusion*, not in-flight
  counts (so `ConnectedWorkers::inc/dec_inflight` are unused by the
  scheduler — kept + still unit-tested for a future capacity>1). Pinned by
  `binpacking_with_capacity_one_spreads_like_simplefifo`; re-activating
  packing is a deliberate future change (per-worker capacity knob).
- **At-least-once, bounded retries.** `report` discards a result whose
  lease is already gone (late/duplicate after reassignment) — safe under
  determinism. Requeues bump an attempt counter; past `MAX_ATTEMPTS` the
  waiter is dropped so `execute` fails instead of looping forever.
- **Overall timeout spans retries.** The caller's `execute` wait (and each
  lease) is sized from the action timeout; reassignment reuses the same
  waiter, so a crash-and-retry is transparent as long as it fits the
  budget. Lease-*expiry*-based reassignment (slow worker, not disconnect)
  is the remaining follow-up — crash recovery via disconnect is live now.

### Next increment

Lease-expiry reaper (`tokio::time::interval` → `LeaseTable::take_expired`
→ requeue → `try_dispatch`), wired into the binary like the eviction
reaper; a lease-renewal RPC for long actions; then §16 task 4's other
halves — tenants/quotas and weighted fair queuing over the pending queue.

## I13 — lease-expiry reaper (§16 task 4)

- **Date:** 2026-06-28
- **Affected crate:** `brokkr-control`
- **Outcome:** `Scheduler::reap_expired_leases` (tested via
  `reap_expired_at(now)`) requeues + re-dispatches jobs whose lease
  expired — a worker still connected but gone silent — bounded by
  `MAX_ATTEMPTS`. `spawn_lease_reaper` drives it on an interval, wired into
  the binary at half the lease window. Jobs now carry a per-attempt
  `lease_duration = min(action timeout, DEFAULT_LEASE_DURATION = 60s)`, so
  a hung worker is retried before the caller's deadline (vs. the previous
  lease == overall-timeout, where expiry coincided with giving up). 60 lib
  tests green.

### Decisions

- **Shared requeue path.** Disconnect and expiry both end in
  `Inner::requeue_taken` (bump attempts → push-front or give-up), and
  give-up jobs are failed via `fail_jobs` (drop the waiter). One code path,
  two triggers.
- **Separate, shorter lease window.** Added `DEFAULT_LEASE_DURATION` (60s)
  distinct from the overall execute timeout. Without it the lease only
  expired when the caller had already given up, so the reaper was inert in
  production; now a silent worker's job is retried mid-flight.
- **Test seam `reap_expired_at(now)`.** The reaper reads `Instant::now()`;
  splitting out an instant-taking inner method lets the test force expiry
  with a far-future instant — deterministic, no sleeping on the real lease
  window.
- **Known limitation: expired-but-connected workers aren't excluded.**
  Unlike disconnect (which removes the worker), an expired lease leaves the
  worker connected, so the deterministic `Strategy` may re-pick it. The job
  still makes progress (or fails after `MAX_ATTEMPTS`), but "reassign
  strictly elsewhere" wants lease **renewal** (so a merely-slow worker
  keeps its lease) or per-job tried-worker tracking. Documented + pinned by
  a mechanism-level test (asserts re-dispatch, not the target). Renewal is
  the natural next lease increment.

### Next

Lease renewal RPC (long-running actions extend their lease; only truly
silent/dead workers expire), which also fixes the re-pick limitation. Then
§16 task 4's other half: **tenants/quotas + weighted fair queuing** over
the pending queue (the "two tenants get fair share" DoD) — its own ADR
(0010) + an options check-in before implementing.

## I14 — lease renewal via heartbeat (§16 task 4)

- **Date:** 2026-06-28
- **Affected crate:** `brokkr-control`
- **Outcome:** Each worker heartbeat renews the leases that worker holds
  (`Scheduler::renew_worker_leases` → `LeaseTable::renew_worker`), so a
  lease expires only when a worker *stops heartbeating*, not merely because
  it's running a long action. Closes the lease lifecycle. 62 lib tests
  green.

### Decisions

- **Renew on heartbeat, not a new RPC.** The simplest, most elegant
  design: the worker already heartbeats every `heartbeat_seconds` to prove
  liveness; piggybacking lease renewal on that signal means *no proto
  change and no worker-side change*. Lease lifetime becomes "the worker is
  alive" — exactly what a lease should track. A dedicated `RenewLease` RPC
  (or an `active_job_id` on the heartbeat) would only matter if we needed
  per-job renewal semantics; with capacity-1 "renew the worker's lease" is
  unambiguous.
- **This resolves the I13 re-pick caveat.** Because a live worker's lease
  is renewed every heartbeat (5s) and the lease window is 60s, a healthy
  worker's lease never expires → it's never wrongly reassigned/re-picked.
  Only a worker that genuinely stopped heartbeating expires — and that
  worker is also being evicted from the registry and will disconnect, so
  the reassignment lands elsewhere. Lease expiry is now a true
  dead-worker backstop, complementing disconnect-based recovery.
- **Hung-action bound is the action timeout, not the lease.** A worker that
  is alive (heartbeating) but stuck in an infinite-loop action keeps its
  lease renewed; the *action timeout* (`execute`'s overall wait) is what
  bounds that case and returns `Timeout`. Lease = worker liveness; action
  timeout = work liveness. Clean separation.

### §16 task 4 status

Lease machinery is complete: global queue (I12), crash reassignment on
disconnect (I12), expiry reaper (I13), renewal on heartbeat (I14).
**Remaining:** tenants/quotas + weighted fair queuing — the "two tenants
get fair share" DoD. That's the next milestone: ADR 0010 + an
AskUserQuestion on tenant-id source, quota types, and the WFQ algorithm
before implementing.

## I15 — tenants + fair-queue foundation (§16 task 4, ADR 0010)

- **Date:** 2026-06-28
- **Affected crates:** `brokkr-common`, `brokkr-control` (+ ADR 0010)
- **Decisions taken with the owner:** tenant id from a **gRPC metadata
  header** (`x-brokkr-tenant`, default fallback); **virtual-time WFQ
  (SFQ)** for fair sharing.
- **Outcome:** ADR 0010 records the design. `brokkr_common::TenantId`
  newtype + `brokkr-control::fairqueue::FairQueue<J>`, a pure Start-time
  Fair Queue: per-tenant virtual start tags, weight-proportional service,
  eligibility-constrained dequeue via `slots()` + `take(index)`. 7 fair-queue
  + 2 tenant unit tests; both crates green. Scheduler wiring is I16.

### Decisions

- **SFQ with unit cost.** Action runtime is unknown up front, so every job
  is cost 1; a tenant of weight `w` advances its virtual clock by `COST/w`
  per job, so weight-2 is serviced ~2× as often. Integer fixed-point
  virtual time (no floats) keeps it deterministic + `Ord`-clean.
- **Eligibility-constrained dequeue, not strict global min.** Dispatch must
  still honour platform matching + idle workers (ADR 0008/0009), so
  `FairQueue` exposes `slots()` (each with its start tag) for the scheduler
  to scan and `take(index)` to remove the chosen one + advance virtual
  time. This mirrors the existing under-one-lock scan in `try_dispatch`, so
  wiring it in won't reintroduce a borrow/lock tangle. A `pop()` convenience
  (global min) backs the unit tests.
- **Header-sourced tenant id, pre-auth.** `x-brokkr-tenant` with a
  `"default"` fallback; client-asserted until auth (§16 task 8) makes it
  authoritative. Deliberately *not* REAPI `instance_name` (routing, not
  identity).
- **Foundation split from wiring.** I15 is the pure, fully-unit-tested
  pieces; I16 replaces `Inner.pending: VecDeque<PendingJob>` with the
  `FairQueue`, extracts the tenant in the `Execution`/`WorkerService`
  handlers, threads it into `PendingJob`, and switches `try_dispatch`'s
  scan to `slots()`/`take`. I17 adds the max-concurrent-per-tenant quota at
  admission.

### Next increment (I16)

Wire `FairQueue` into the scheduler: tenant extraction in `execute`
(header → `TenantId`), `PendingJob.tenant`, `Inner.pending: FairQueue`,
`try_dispatch` scans `slots()` for the lowest-start dispatchable job. Then
a two-tenant fairness integration test (the §16 DoD), and I17 quotas.

## I16 — fair dispatch wired in (§16 task 4, the fair-share DoD)

- **Date:** 2026-06-28
- **Affected crate:** `brokkr-control`
- **Outcome:** The §16 "two tenants running concurrently each get fair
  share" DoD is met. `Inner.pending` is now the per-tenant `FairQueue`;
  the `Execution` service extracts the tenant from `x-brokkr-tenant`
  (default `"default"`) and threads `TenantId` into `Scheduler::execute`
  → `PendingJob`; `try_dispatch` dequeues the lowest-virtual-start-tag job
  with an idle eligible worker. 70 lib tests incl.
  `two_tenants_share_a_worker_fairly`; real-gRPC e2e + soak green.

### Decisions

- **Dequeue = min-start-tag *dispatchable* slot.** Kept the existing
  under-one-lock scan shape (no closures borrowing `Inner`): iterate
  `pending.slots()`, compute each job's eligible idle worker as before, and
  track the dispatchable slot with the smallest start tag, then
  `pending.take(idx)`. So fairness composes cleanly with platform matching
  + capacity-1 leases.
- **Requeue re-tags.** Disconnect / expiry requeues `push` the job back
  into the fair queue (a fresh start tag for its tenant) rather than
  preserving the original tag. Simple and good enough; the job rejoins its
  tenant's fair share. Timeout cleanup uses `FairQueue::retain`.
- **Tenant from metadata, default fallback.** `x-brokkr-tenant` →
  `TenantId`, `"default"` when absent/malformed (ADR 0010). CAS/ByteStream
  uploads don't carry a tenant — tenancy is an *execution* concern for now;
  quota accounting for storage is a later sub-increment.
- **Deterministic fairness test.** Register the worker but connect it only
  *after* all six jobs (3 per tenant) are queued, so every job is tagged
  before any dispatch; then drive the single worker (recv → report) and
  assert the dispatch order interleaves tenants (first-B before last-A),
  3 each. Avoids the connect-before-enqueue race.

### Next increment (I17)

Per-tenant **max-concurrent quota** at admission: a per-tenant in-flight
gauge in `Inner`, checked in `execute`; over-quota →
`ExecutionError::QuotaExceeded` → gRPC `RESOURCE_EXHAUSTED`. Then §16 task 4
is complete (fair share + quotas); remaining Phase 4 is auth (task 8) and
the Bazel-compatibility test, then the exit-criteria wrap-up.

## I17 — per-tenant max-concurrent quota (§16 task 4 — COMPLETE)

- **Date:** 2026-06-28
- **Affected crate:** `brokkr-control`
- **Outcome:** Per-tenant max-concurrent-jobs quota. `with_tenant_quota`
  sets an optional limit; `execute` rejects admission over it with
  `ExecutionError::QuotaExceeded(limit)` → gRPC `RESOURCE_EXHAUSTED`. This
  **completes §16 task 4** (worker registry + matching + multi-worker
  dispatch + leases + fair scheduling + quotas). 72 lib tests.

### Decisions

- **In-flight count = queued + leased, per tenant.** Counted from
  admission until the `execute` call goes terminal. The check + increment
  happen under one `Inner` lock so two concurrent submits can't both pass;
  the decrement runs once after the await block (every terminal path).
- **`None` = unlimited (default).** Existing constructors leave the quota
  unset, so fixtures/Phase-1 paths are unaffected; the binary/tests opt in
  via `with_tenant_quota`.
- **Max-concurrent first.** Cheapest quota to enforce (a gauge); CPU-second
  and storage quotas need usage accounting that doesn't exist yet and are
  deferred (ADR 0010).

## §16 task 4 — DONE

Across I1–I17 (+ ADRs 0008/0009/0010): worker registry & heartbeat
eviction → constraint matching & admission → multi-worker dispatch with
`SimpleFifo`/`BinPacking` → global queue + leases (crash reassignment,
expiry reaper, heartbeat renewal) → per-tenant virtual-time fair queuing →
max-concurrent quotas. Both task-4 DoD items demonstrated: worker-crash
recovery and two-tenant fair share.

**Remaining Phase 4:** §16 task 8 (auth — mTLS worker↔control TLS flags
exist; client tokens/mTLS verification to add) and the
Bazel-compatibility test. The latter needs a real `bazel` client driving a
runnable cluster end-to-end — likely **not** feasible in the WSL2/no-bazel
dev env and a substantial lift; assess + surface to the owner before
attempting. Then the Phase 4 exit-criteria wrap-up (`docs/plan.md` §11).

## I18 — auth core: JWT client validation (§16 task 8, ADR 0011)

- **Date:** 2026-06-29
- **Affected crates:** `brokkr-control` (+ ADR 0011, workspace deps)
- **Decisions taken with the owner:** client auth = **OIDC/JWT** (over
  static tokens); **open mode** when unconfigured; JWT crate =
  **`jsonwebtoken`**.
- **Outcome:** `brokkr-control::auth` — `JwtAuth` validates a bearer
  token (HS256/RS256 signature, `exp`, optional `iss`/`aud`) and extracts
  the tenant from a configured claim; `Authenticator` = `Disabled`
  (header tenant) | `Jwt` (claim tenant, authoritative). 8 unit tests;
  80 lib tests green. Pure core — interceptor wiring is I19.

### Decisions

- **Tenant from the claim is authoritative.** Closes the ADR-0010
  client-asserted-tenant gap: when auth is on, the JWT's tenant claim wins
  over the `x-brokkr-tenant` header.
- **`serde_json::Value` claims, not a typed struct.** Decode into a
  `Value` and read the configured claim by name — avoids a `serde` derive
  and lets the tenant-claim name be configurable.
- **`exp` required, `aud` opt-in.** `Validation` requires `exp` by
  default (good); `validate_aud` is turned off unless `with_audience` is
  set, so tokens without an `aud` aren't spuriously rejected.
- **`jsonwebtoken` + MSRV pin.** The crate's transitive `simple_asn1` →
  `time` pulls `time` 0.3.51 which needs rustc 1.88 (> our MSRV 1.85), so
  pinned `simple_asn1` 0.6.2 + `time` 0.3.36 (lockfile-only, scoped to
  this dep — not an unrelated `cargo update`). Crypto backend `ring` was
  already in-tree via rustls/tonic TLS, so no new crypto backend.
- **Boxed `Jwt` variant.** `JwtAuth` (key + validation) dwarfs the unit
  `Disabled`, so `Authenticator::Jwt(Box<JwtAuth>)` (clippy
  `large_enum_variant`).

### Next increment (I19)

Wire it into the server: a tonic interceptor on the client-facing services
that authenticates the `authorization: Bearer` token → injects the
authoritative `TenantId` into request extensions (rejecting with
`UNAUTHENTICATED` when auth is on and the token is missing/invalid); the
`Execution` handler prefers the injected tenant over the header. Config +
binary flags (key/secret, iss/aud, claim) with the open-mode startup
warning. Worker↔control mTLS enforcement. Then the Bazel-compat assessment
+ Phase 4 exit-criteria wrap-up.

## I19 — auth wired into the server (§16 task 8 — client auth COMPLETE)

- **Date:** 2026-06-29
- **Affected crate:** `brokkr-control`
- **Outcome:** Client auth is enforced end-to-end. `auth_interceptor` (a
  tonic interceptor) validates the `Bearer` JWT and injects the
  authoritative `TenantId`; it guards the four client-facing services,
  while `WorkerService` stays mTLS-only. `Execution` prefers the injected
  tenant over the header. Binary `--auth-jwt-*` flags build the
  `Authenticator`; open-mode warns loudly. 83 lib tests + a 3-case gRPC
  integration test (`tests/auth.rs`).

### Decisions

- **Per-service `with_interceptor`, not a global layer.** Lets the
  internal `WorkerService` opt out (it's mTLS-authed, not token-gated)
  while every client-facing service is gated. The interceptor closure
  captures `Arc<Authenticator>` (so it's `Clone`, as tonic requires).
- **Tenant via request extensions.** The interceptor inserts the
  authenticated `TenantId` into the request's extensions; the handler
  reads it back and falls back to the header only in open mode. Keeps the
  authoritative-tenant logic out of every handler.
- **mTLS needs no new code.** tonic's `ServerTlsConfig::client_ca_root`
  (already wired to `--tls-client-ca`) requires clients present a cert
  signed by that CA — so worker↔control mTLS is enforced by configuring
  the flag; documented rather than re-implemented.
- **Integration test via `Capabilities`.** Picked the lightweight
  `Capabilities` RPC (returns immediately, no scheduler/worker) to test
  the interceptor at the gRPC layer without a full cluster — proves
  no-token / bad-token → `UNAUTHENTICATED` and valid-token → `Ok`.

### §16 task 8 status

Client auth (JWT bearer, tenant from claim, authoritative) + worker mTLS
enforcement are done. **Deferred:** live OIDC/JWKS-URL discovery + key
rotation (needs an HTTP client; static/configured keys cover the core);
deriving a worker identity from its client cert.

### Remaining Phase 4

- **Bazel-compatibility test** (§16 DoD "run a real `bazel` build"):
  needs a `bazel` client + a runnable two-process cluster + REAPI
  conformance closed. **Not feasible in this WSL2/no-bazel dev env** — to
  be recorded as a tracked Phase-4 gap rather than attempted here
  (stop-and-ask before any attempt).
- **Phase 4 exit-criteria review** (`docs/plan.md` §11) + journal
  retrospective: the next doc-focused increment.
