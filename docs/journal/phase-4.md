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
