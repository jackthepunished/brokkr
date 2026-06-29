# 0009 — Job leases, global queue, and crash reassignment

- **Status:** accepted
- **Date:** 2026-06-27
- **Deciders:** Brokkr maintainers

## Context

After ADR 0008 the scheduler routes each action directly to a chosen connected
worker (per-worker channels), and **fails fast** with `NoEligibleWorker` when
no eligible worker is connected. Two gaps remain for Phase 4 §16 task 4 and its
definition of done:

1. **No crash recovery.** "Worker crash mid-job → job retried on another
   worker, completes successfully" is the headline §16 DoD. Today a worker that
   disconnects mid-job just drops its in-flight jobs to a timeout; nothing
   retries them.
2. **No global queue / backpressure.** A submit with every eligible worker busy
   should *wait* (queue), not fail. ADR 0008 explicitly deferred the global
   pending queue to this task; it is also the point at which fair scheduling
   (per-tenant weighting) will later be applied.

This ADR covers the queue + lease + reassignment mechanism. Tenants/quotas and
weighted fair queuing build on the queue and are their own increments.

## Decision

**A global pending queue drained by an event-driven dispatcher, with
time-bounded job leases and requeue-on-failure.**

- **Worker capacity = 1 leased job.** A worker's control loop runs one action
  at a time (receive → run → report), so the scheduler leases it exactly one
  job and treats it as busy until it reports, the lease expires, or it
  disconnects. (A future per-worker parallelism knob can raise this.)
- **Global pending queue.** `execute()` builds the job, registers a result
  waiter (keyed by `JobId`, surviving retries), pushes the job onto a FIFO
  pending queue, and awaits its result under the existing overall execution
  timeout. FIFO now; per-tenant weighted fair queuing replaces the ordering
  later without changing this shape.
- **Event-driven dispatcher.** A `try_dispatch` pass assigns queued jobs to
  idle workers that satisfy each job's platform (reusing
  `matching::eligible_workers` ∩ connected ∩ idle, picked by the ADR 0008
  `Strategy`). It runs on every state change that could enable progress: job
  enqueued, worker connected, worker became idle (reported), lease expired,
  worker disconnected.
- **Leases.** Dispatching creates a `Lease { worker_id, deadline, payload }`
  keyed by `JobId`, where `payload` is everything needed to re-dispatch the
  job. The worker must report before `deadline`. A `LeaseTable` tracks active
  leases and supports: complete (worker reported), `take_expired(now)`, and
  `take_worker(worker_id)` (for disconnect).
- **Requeue on failure.** On lease expiry *or* worker disconnect the job's
  payload is moved back to the pending queue (bounded retry count) and the
  dispatcher reassigns it to another worker. The result waiter is untouched, so
  the original `execute()` caller transparently gets the retried result.
- **At-least-once, made safe by determinism.** A worker whose lease expired may
  still finish and report late; by then the job may have been retried
  elsewhere. Late reports for a job with no waiter/lease are discarded. Brokkr's
  determinism axiom means a double-run yields the same result, so at-least-once
  is acceptable (exactly-once would need fencing we don't want here).

## Alternatives considered

- **Keep fail-fast, no queue.** Simplest, but cannot satisfy the crash-recovery
  DoD and pushes backpressure onto every client as retry-loops. Rejected.
- **Per-worker job buffering (today's model) for retries.** Buffering several
  jobs at a worker can't reassign them when that worker dies — the whole point
  of leases is a central record that survives the worker. Rejected for task 4.
- **Exactly-once via fencing tokens / dedup.** Heavier (needs persistent fencing
  state, idempotency keys). Determinism already gives us safe retries, so
  deferred unless a non-deterministic-action escape hatch ever needs it.
- **Lease renewal RPC now.** A long-running action could outlive a fixed lease.
  Renewal (worker periodically extends its lease) is real but additive; the
  first cut sizes the lease from the action timeout and adds renewal as a
  follow-up increment.

## Consequences

- **Positive:** Delivers the crash-recovery DoD; adds backpressure (submits
  queue instead of failing); the queue is the natural seam for per-tenant fair
  scheduling and quotas next.
- **Negative:** Re-touches the dispatch core (again) — staged as ADR + tested
  `LeaseTable`/queue foundation first, then the `execute`/dispatcher rewrite.
  At-least-once can double-run an action across a lease-expiry boundary (safe
  under determinism, but worth noting for any future non-deterministic path).
- **Neutral:** Worker capacity is pinned at 1 for now; per-worker parallelism
  and lease renewal are future knobs behind the same structures.

## References

- `docs/plan.md` §6.3 (scheduler), §16 task 4, §16 DoD
- ADR 0008 (multi-worker scheduling — deferred the global queue here)
- `crates/brokkr-control/src/{scheduler,scheduling,matching}.rs`
