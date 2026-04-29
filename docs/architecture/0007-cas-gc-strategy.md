# 0007 — CAS garbage collection strategy

- **Status:** proposed (placeholder — to be filled out during Phase 3 design)
- **Date:** 2026-04-30
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

**To be decided during Phase 3 design.** The current placeholder
direction:

- Refcount table in `refcount.redb` (already sketched in
  `docs/plan.md` §10).
- Increment on `UpdateActionResult` for every output digest + every
  input digest in the referenced `Action`.
- Decrement on action-cache TTL expiry or explicit eviction.
- Warm-tier LRU eviction independent of refcount (a refcounted blob
  can be cold-tier-only if it's not hot).

## Alternatives to evaluate

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

- How are refcounts kept consistent across sharded CAS nodes?
- What happens to a blob whose only reference is a soft-evicted action
  cache entry?
- Do we refcount per-tier (hot/warm/cold) or per-blob globally?
- How does GC interact with in-flight `BatchUpdateBlobs` writes?
- What is the failure mode if the refcount table diverges from
  ground truth — repair scan, or fail-stop?

## References

- `docs/plan.md` §6.1 (CAS), §10 (Storage layout), §15 (Phase 3),
  §31 (Open Questions — GC item).
- Bigtable paper (TTL/garbage collection patterns).
- bazel-remote GC implementation (LRU prior art):
  <https://github.com/buchgr/bazel-remote>
