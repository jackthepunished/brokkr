# 0008 — Multi-worker scheduling: per-worker queues + pluggable strategy

- **Status:** accepted
- **Date:** 2026-06-27
- **Deciders:** Brokkr maintainers

## Context

Through Phase 4 task 2 the scheduler is still the Phase 1 shape: a single
shared job queue (`mpsc`), and `WorkerService.Stream` claims *the* single
receiver via `take_receiver()` — only one worker can ever be connected.
Task 1 added a `WorkerRegistry` (liveness + capabilities) and task 2 added
constraint matching (`matching::eligible_workers`) plus admission control,
but jobs still flow through one queue to one worker.

Phase 4 §16 task 3 requires real multi-worker scheduling with pluggable
strategies (`SimpleFifo` → `BinPacking` → `LocalityAware`). That needs a way
to (a) track which workers currently have a live stream, (b) route a job to a
*specific* worker, and (c) choose *which* eligible worker gets each job.

## Decision

**Per-worker job queues with submit-time routing.**

- A `ConnectedWorkers` registry maps `WorkerId → (job channel, in-flight
  count)`. A worker is added on `Stream` connect and removed on disconnect.
  This is distinct from `WorkerRegistry`: a worker is *registered* (known,
  heartbeating) before and independently of being *connected* (streaming).
- On `execute()`, the scheduler computes the eligible candidates
  (`matching::eligible_workers` ∩ connected), asks a pluggable `Strategy` to
  pick one, and routes the job by sending it to that worker's channel.
- `Strategy::choose(candidates, loads)` is the policy seam. `SimpleFifo`
  (the first/default strategy) picks the least-loaded candidate with a
  deterministic id tie-break; `BinPacking` and `LocalityAware` are later
  increments behind the same trait.
- Per-worker in-flight counts are maintained by the dispatch path
  (increment on send, decrement on result/timeout) and exposed to the
  strategy via a `LoadView`.

FIFO ordering is preserved *per worker* by each worker's own ordered channel;
"SimpleFifo" here refers to the worker-selection policy (least-loaded), not a
global job order.

## Alternatives considered

- **Central dispatcher + global pending queue.** A dispatcher task holds one
  pending queue and assigns jobs to idle eligible workers on job-arrival or
  worker-idle. Pros: global backpressure, natural home for leases and fair
  scheduling (task 4). Cons: more moving parts now (idle tracking, requeue on
  failure) than task 3 needs. **Deferred** — task 4's fair-scheduling/leases
  work is the right time to introduce a global queue, evolving from this.
- **Pull / work-stealing (workers pull jobs they're eligible for).** Rejected:
  constraint matching is server-side and our worker transport is a
  server-push bidi stream; a pull model would have to ship constraints to
  workers or filter a shared queue per worker, fighting both.

## Consequences

- **Positive:** Incremental — fits the existing push/bidi worker stream;
  lands the `Strategy` trait now; constraint matching (task 2) plugs straight
  in as the candidate filter; each strategy is independently testable.
- **Negative:** No *global* queue yet — a job routed to a busy worker waits in
  that worker's channel rather than being rebalanced to a worker that frees up
  first. Acceptable for task 3; task 4 (leases, fair scheduling) introduces
  global queueing/rebalancing. Lease-based reassignment of a crashed worker's
  in-flight jobs is also task 4.
- **Neutral:** `ConnectedWorkers` and `WorkerRegistry` are separate concerns
  (connection vs. liveness/capability); the scheduler consults both.

## References

- `docs/plan.md` §6.3 (scheduler), §16 (Phase 4 tasks 3–4)
- ADR 0002 (REAPI compatibility) — push-based execution model
- `crates/brokkr-control/src/matching.rs` (task 2 eligibility)
