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
so the whole workspace is built and tested under WSL2 Ubuntu (cargo
1.85) with a Linux-native `CARGO_TARGET_DIR`. `cargo fmt --check`,
`cargo clippy --workspace --all-targets -D warnings`, and
`cargo test --workspace` are all green there (sandbox evil-action tests
skip cleanly without privileged user namespaces, as in Phase 2/3).
