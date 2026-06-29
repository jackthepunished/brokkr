# 0010 — Tenants and weighted fair scheduling

- **Status:** accepted
- **Date:** 2026-06-28
- **Deciders:** Brokkr maintainers

## Context

Phase 4 §16 task 4 calls for multi-tenancy: every request carries a tenant id,
tenants have quotas (max concurrent jobs, CPU-seconds/day, storage), and the
scheduler shares capacity fairly — "two tenants running concurrently each get
fair share" is a §16 definition-of-done item. The scheduler currently has a
single global FIFO pending queue (ADR 0009); fairness needs per-tenant
accounting on top of it.

Auth-derived identity is a *later* task (§16 task 8). This ADR covers how
tenancy and fair scheduling work *before* auth lands, in a way auth can slot
into without rework.

## Decision

**Tenant id from a gRPC metadata header; virtual-time (start-time) fair queuing
over per-tenant-tagged pending jobs.**

### Tenant identity

- The control plane reads the tenant id from a request metadata header,
  `x-brokkr-tenant`, falling back to a `"default"` tenant when absent. A
  `brokkr_common::TenantId` newtype carries it through the scheduler.
- This is **client-asserted** until auth (§16 task 8); auth then becomes the
  authoritative source of the same `TenantId`, so no plumbing changes. We do
  **not** overload the REAPI `instance_name` field (it means routing/namespace,
  not identity).

### Fair queuing — virtual-time SFQ

- The global pending queue becomes a **per-tenant-tagged** fair queue using
  **Start-time Fair Queuing (SFQ)**: each enqueued job gets a virtual *start*
  tag `start = max(virtual_time, last_finish[tenant])` and advances its tenant's
  clock by `cost / weight`; the scheduler services jobs in increasing start-tag
  order, setting `virtual_time` to the start tag of the job it dispatches.
- **Unit cost.** Action cost is unknown up front (we don't know runtime), so
  every job is cost 1. With per-tenant `weight`, a weight-2 tenant is serviced
  twice as often as a weight-1 tenant — proportional share without needing to
  predict job size.
- **Eligibility-constrained dequeue.** Dispatch still respects platform
  matching + idle workers (ADR 0008/0009): the scheduler dequeues the
  *lowest-start-tag job that has an idle eligible worker*, not strictly the
  global minimum. So fairness degrades gracefully when a tenant's jobs can only
  run on busy/again-constrained workers.
- Integer fixed-point virtual time (no floats) keeps it deterministic and
  `Ord`-clean for tests.

### Quotas

- Enforced at admission in `execute`, starting with **max concurrent jobs** per
  tenant (the cheapest to track: a per-tenant in-flight gauge). CPU-seconds/day
  and storage quotas are later sub-increments (they need usage accounting that
  doesn't exist yet). Over-quota → gRPC `RESOURCE_EXHAUSTED`.

## Alternatives considered

- **Deficit/weighted round-robin** instead of SFQ. Simpler, but coarser and
  less precise under bursty arrivals; SFQ gives cleaner proportional share and
  is the textbook fit. Chosen against per the owner's call.
- **REAPI `instance_name` as tenant.** Rejected — conflates routing with
  identity and gets awkward when auth lands.
- **Tenant only after auth.** Rejected — we want to demo the two-tenant
  fair-share DoD now; the header is a fine pre-auth source.

## Consequences

- **Positive:** Delivers the fair-share DoD; the SFQ structure is a small,
  pure, unit-testable component; auth later just supplies the `TenantId`.
- **Negative:** Unit cost means a tenant submitting many tiny jobs and one
  submitting few huge jobs are treated equally per-job, not per-CPU-second —
  acceptable until usage accounting (CPU-second quotas) exists. Client-asserted
  tenant id is spoofable until auth (documented; task 8 closes it).
- **Neutral:** Per-worker capacity stays 1 (ADR 0009); fairness is over
  *dispatch order*, not over concurrent slots-per-tenant (that's the
  max-concurrent quota's job).

## References

- `docs/plan.md` §6.3, §16 task 4 + DoD, §16 task 8 (auth)
- ADR 0008 (multi-worker scheduling), ADR 0009 (leases + global queue)
- SFQ: Goyal, Vin, Cheng, "Start-time Fair Queueing" (SIGCOMM '96)
