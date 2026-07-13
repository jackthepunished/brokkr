# 0007 — CAS garbage collection strategy

- **Status:** accepted for M5a; coordination barrier supersedes the
  earlier refcount placeholder.
- **Date:** 2026-04-30 (placeholder); 2026-07-09 (M5a fill-in, #144)
- **Deciders:** Brokkr maintainers

## Context

The CAS accumulates blobs indefinitely. Without a garbage collector,
local NVMe and cold S3 storage grow without bound. We need a GC
strategy that:

- Never deletes a blob still reachable from a live action-cache entry.
- Reclaims space predictably under storage pressure.
- Survives a crash mid-collection without losing data or double-freeing.
- Works across the tiered storage hierarchy (hot / warm / cold).
- Eventually composes with hash-prefix sharding (Phase 3) and Raft-backed
  metadata (Phase 5).

The tentative direction (`docs/plan.md` §31): **reference counting backed
by the action cache, plus LRU eviction in the warm tier.** This ADR is
the placeholder for the full decision.

## Decision

**Mark-and-sweep, in-process (M5a).** The CAS GC is a mark-and-sweep
collector parameterised on the local CAS and the action cache: the
mark phase unions every digest directly inlined in every
`ActionResult` reachable from the action cache; the sweep phase
deletes every local blob not in that set. Decisions are summarised
below; details are in `docs/phase-3-plan.md §5.6` and the M5a
retrospective in `docs/journal/phase-3.md`.

The placeholder refcount sketch has been **superseded** (see
"Refcount → mark-sweep" below). LRU retention survives in the warm
tier, as a capacity-only mechanism — it does not make correctness
claims about reachability.

### Coordination barrier (M5a, fixes #144)

Issue #144 is that `gc::plan` snapshots the action cache and the CAS
at two different points in time, in two separate redb transactions.
A worker that uploads a blob (`BatchUpdateBlobs`) and writes an
`ActionResult` referencing it (`UpdateActionResult`) in the window
between those two reads gets its blob GC'd on the very next sweep.
The cached `ActionResult` then points at `NotFound` — i.e. a
*poisoned cache*.

We close the in-process race with a writer-coordinated barrier:

- **Mechanism.** `tokio::sync::Mutex<()>` per `ActionCache`
  instance, exposed via the trait method `ActionCache::gc_window`
  and the guard type `GcWindowGuard` (which holds an
  `OwnedMutexGuard<()>`). Default trait impl returns a no-op guard
  so non-`Redb` and test-only backends stay trivial.
- **Worker protocol.** Hold the guard across both
  `cas.batch_update_blobs(...)` and
  `ac.update_action_result(...)` for the same logical action. The
  two control-plane `update_action_result` call sites (the REAPI
  `UpdateActionResult` handler and the scheduler's exit_code==0
  store) acquire the barrier before committing.
- **GC protocol.** `gc::sweep` holds the barrier for the duration
  of `plan + sweep_with_plan`. `plan_locked` and
  `sweep_with_plan_locked` are also exported for callers that need
  to compose the steps explicitly. The non-locked `plan` and
  `sweep_with_plan` remain available for dry-runs and metrics,
  with a SAFETY doc-comment.
- **Cancellation.** `lock_owned()` returns a cancel-safe guard:
  dropping the future (task abort) releases the mutex, so a worker
  cancelled mid-`(CAS-write, AC-write)` does not deadlock the GC.
  The orphan blob is reclaimable on the next sweep — same failure
  mode as "worker crashes after upload but before AC write"
  (CAS-orphan handling, M5b).
- **Tests.** Three — `sweep_blocks_concurrent_worker_writes`
  (positive: barrier blocks a second `gc_window()` until the
  sweep releases), `update_action_result_serializes_against_sweep`
  (positive: AC write blocks behind the sweep's guard), and
  `sweep_with_noop_barrier_deletes_in_flight_blob` (negative: bug
  returns if the override is removed).

#### Scope — in-process only

The barrier is per-process. A worker in a *separate* process that
uploads via gRPC does not hold this mutex — coordinating it would
require a control-plane-coordinated protocol (e.g. a "GC paused"
flag in the CAS service, or a `BatchUpdateBlobs` opt-in for an
explicit barrier). That race is tracked as a follow-up issue and is
the next step on the GC roadmap. **Honest scope:** this fix closes
the in-process-library race (which is what issue #144 demonstrates
and what the current code exhibits); the cross-process race is
filed as a follow-up.

#### Refcount → mark-sweep

The original refcount sketch (table in `refcount.redb`,
incremented on every `UpdateActionResult`, decremented on TTL
expiry) was dropped before M5a shipped. Reasoning:

1. **Recovery is harder.** Mark-sweep is self-healing — every sweep
   rebuilds reachability from the action cache. Refcount drift
   requires a separate repair scan, and the failure mode (delete a
   still-live blob) is silent corruption of the cache.
2. **Phase 3 has no TTL.** AC entries live forever in M5a, so the
   "decrement on TTL" path never fires — refcount would be
   monotonically increasing, equivalent to set membership. Mark-
   sweep gives the same correctness for less state.
3. **Simplicity.** The mark phase needs only the action cache. The
   sweep phase needs only the CAS. No new table, no new schema,
   no new invariants.
4. **Bazel et al. use mark-and-sweep** for similar reachability
   patterns (see `bazel-remote`, prior art in §References).

LRU eviction remains in the warm tier, where it serves a
*capacity* purpose, not a reachability one — see "Alternatives
evaluated" below.

## Alternatives evaluated

- **Reference counting + LRU.** Skipped. Set membership + reachability
  recovery + minimal state beat a per-blob counter. LRU survives in
  the warm tier as capacity-only.
- **Mark-and-sweep with periodic full scans.** **Selected.** See
  "Decision".
- **Generational / time-based eviction (Bigtable-style TTL).**
  Future-milestone: M5b's atime + retention window is a
  generational-style refinement on top of mark-sweep. Replaces the
  raw "delete anything unreachable" semantics with "delete anything
  unreachable *for ≥ N days*".
- **External GC service.** Skipped for Phase 3. Adds an RPC for
  negligible value when the collector is already in-process and
  the CAS lives next to the action cache.
- **Hybrid: refcount for correctness, LRU for capacity, mark-sweep
  as periodic safety net.** Theoretical best-of-all; in practice the
  refcount safety net duplicates the mark-sweep safety net. Rejected
  as over-engineering for current load.

(Deferred — fill out before Phase 3 implementation begins.)

- Reference counting + LRU (tentative).
- Mark-and-sweep with periodic full scans.
- Generational / time-based eviction (Bigtable-style TTL columns).
- External GC service vs. in-process collector.
- Hybrid: refcount for correctness, LRU for capacity, mark-sweep as
  periodic safety net.

## Consequences

To be filled out alongside the decision.

## Open questions for Phase 3

- ~~How are refcounts kept consistent across sharded CAS nodes?~~
  *Resolved: no refcounts. Mark-sweep runs locally on each CAS node;
  sharding (Phase 3) is per-digest, and each node's mark-sweep is
  computed locally.*
- ~~What happens to a blob whose only reference is a soft-evicted
  action cache entry?~~ *Resolved: not applicable — M5a has no
  AC eviction (M5b adds the retention window).*
- ~~Do we refcount per-tier (hot/warm/cold) or per-blob globally?~~
  *Resolved: no refcounts; LRU lives in the warm tier and is
  capacity-only.*
- ~~How does GC interact with in-flight `BatchUpdateBlobs` writes?~~
  *Resolved: writer-coordinated barrier (`ActionCache::gc_window`).
  See "Coordination barrier" above. Cross-process workers not
  covered; tracked in follow-up issue.*
- ~~What is the failure mode if the refcount table diverges from
  ground truth — repair scan, or fail-stop?~~ *Resolved: no
  refcount table. The failure mode is "sweep deletes a live blob"
  and is bounded by the coordination barrier — a worker holding
  the barrier cannot land a write during the sweep window.*

## Out of scope for M5a

- Transitive `Directory` reachability walk (M5b, per
  `docs/phase-3-plan.md §5.6`).
- Atime tracking + retention window (M5b per the M5a retrospective
  deferral).
- Tombstones for cancellation / orphan safety (separate issue; M5b at
  earliest).
- Chunked sweeps (perf optimisation; M5b).
- Cross-process GC coordination (separate issue; M5b or Phase 4).
- `brokk admin gc` CLI subcommand (Phase 4).
- Control-plane GC daemon / cron (Phase 4).
- `/metrics` counters for GC (Phase 4 observability).

## References

- Issue #144 — GC deletes blobs referenced by concurrent
  `UpdateActionResult` (the bug this ADR section resolves).
- `docs/plan.md` §6.1 (CAS), §10 (Storage layout), §15 (Phase 3),
  §31 (Open Questions — GC item).
- `docs/phase-3-plan.md §5.6` — canonical GC specification; line
  616 is the invariant "*GC never deletes a blob that's referenced
  from `live` ActionResults.*"
- `docs/journal/phase-3.md` — M5a retrospective (what shipped vs.
  deferred; the "default trait methods, not breaking changes"
  decision that constrains the in-scope shape of `gc_window`).
- Bigtable paper (TTL/garbage collection patterns).
- bazel-remote GC implementation (LRU prior art):
  <https://github.com/buchgr/bazel-remote>
