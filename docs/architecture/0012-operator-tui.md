# 0012 — Operator TUI + gRPC observability read-model

- **Status:** accepted
- **Date:** 2026-06-30
- **Deciders:** Brokkr maintainers

## Context

Brokkr's only client surface is the `brokk` CLI (one-shot `run`) plus the SDK,
which wraps REAPI `Execute` / CAS / `ActionCache`. There is **no read API** for
cluster, job, or CAS state. The control plane holds rich live state — the
`WorkerRegistry` (workers, capabilities, liveness), the `Scheduler` (pending fair
queue, leases, per-tenant in-flight counts/quotas), and the redb CAS /
action-cache — but exposes none of it for *observation*; the only adjacent
streaming RPC is `MembershipService.WatchTopology`, which publishes CAS-node
membership, not workers or jobs.

An out-of-tree web dashboard was designed
(`docs/superpowers/specs/2026-05-17-brokkr-frontend-design.md`) proposing an
`axum` REST/SSE gateway plus a `brokkr-control::views` read-model. Neither
exists yet.

The owner wants an **in-terminal** operator console — "see what Brokkr is doing"
(which workers are up, what jobs are running/queued/failed, CAS size/dedup) —
without dropping to the CLI or standing up a browser. Operator UI is **Phase 6+**
in `docs/plan.md` §11; the tree is on Phase 4. This is an explicit opt-in
pull-forward of a Phase-6 observability slice. It is read-mostly and additive, so
it does not block Phase 5 (Raft).

## Decision

**Build a read-only operator TUI as a new binary crate `brokkr-tui`, fed by a new
gRPC `brokkr.v1.ObservabilityService` over a shared in-process read-model
(`brokkr-control::views`). Render with `ratatui` over a hand-rolled
Elm-architecture core; transport is gRPC via `brokkr-sdk`.**

### Read-model — `brokkr-control::views`

- View DTOs decoupled from internal types — `ClusterInfo`, `WorkerView`,
  `JobSummary`, `JobDetail`, `CasStats`. Internal state types must not leak
  (same boundary the web spec defines).
- Sourced from the existing handles: `WorkerRegistry` (workers + liveness +
  capabilities), `Scheduler` (queued/leased jobs, per-tenant in-flight),
  CAS/action-cache (object count, bytes, dedup).
- **Job history:** the scheduler keeps no finished-job index today — a completed
  job's outcome returns to the caller and leaves the queue. Add a bounded
  in-memory ring buffer of the last *N* completed jobs, populated in
  `Scheduler::report()`; live queued/running come from the pending queue + lease
  table. **Durable job history is deferred** (a larger scheduler-storage
  decision, Phase 5+).

### Transport — gRPC `ObservabilityService` (not REST/SSE)

- New `brokkr/v1/observability.proto`: unary `GetCluster`, `ListWorkers`,
  `GetWorker`, `ListJobs` (state filter + limit), `GetJob`, `GetCasStats`; plus a
  server-streaming `WatchEvents` (worker up/down, job state change, GC) that
  drives the live UI without polling.
- Mounted in the `brokkr-control` binary behind the **same auth interceptor**
  (ADR 0011) as a read-only scope; internal-only posture (bind localhost / mTLS
  as for the other services).
- **Why gRPC over the web spec's axum REST/SSE gateway:** the consumer here is a
  *Rust* client. gRPC gives server-streaming for free, matches the architectural
  invariant "wire protocol is gRPC + protobuf — don't invent ad-hoc HTTP for
  things that should be RPC", and lets the TUI ship **independently of the
  (unbuilt) browser gateway**. The read-model is the expensive part and is
  shared: a later `axum` gateway wraps the same `views` query fns for the
  browser, which needs REST/SSE anyway.

### TUI architecture — the "not ratatui simply" part

- `ratatui` + `crossterm`. Hand-rolled **Elm architecture (TEA)**: `Model` +
  `Action` enum + pure `update(&mut Model, Action)` + `view(&Model, Frame)`. A
  `tokio::mpsc` **action bus** merges three sources — `crossterm::EventStream`
  (input), a render tick, and the `WatchEvents` gRPC stream. A `Component` trait
  composes panels.
- Hand-rolled TEA over `tui-realm` keeps the dep surface small (rule 6) and makes
  `update()` unit-testable without a terminal — matching the codebase's
  test-first, typed-error ethos.
- Terminal lifecycle: raw mode + alternate screen with a **panic/error hook that
  restores the terminal** (the failure naive ratatui apps get wrong).

### Dependencies (rule 6 — each justified)

- `ratatui`, `crossterm` — the de-facto Rust TUI renderer + async event backend.
- `tui-logger` — bridges the existing `tracing` output into an in-TUI log panel;
  near-free given every control path is already instrumented.
- `futures` — stream merging for the action bus (already a workspace dep).
- `tui-tree-widget` — directory/digest tree, **V1 inspector only** (add when that
  panel lands, not in the MVP).
- No `color-eyre`: a manual panic hook + `anyhow` covers terminal restore without
  another dep.

### Implementation plan (WS0–WS3)

- **WS0 — `brokkr-tui` scaffold** (no backend dependency): crate + workspace
  wiring, terminal lifecycle + panic-restore, TEA skeleton + action bus +
  `Component` trait, `TestBackend` buffer-snapshot + `update()` unit tests.
- **WS1 — `views` read-model in control:** DTOs + query fns over
  registry/scheduler/CAS; recent-jobs ring buffer in `report()`; CAS `stats()`.
  Unit tests against registry/scheduler fakes.
- **WS2 — `ObservabilityService`:** proto + service impl mounted behind the auth
  interceptor; SDK read methods + a `watch_events()` stream wrapper. One
  in-process integration test that boots control and walks the RPCs.
- **WS3 — MVP panels:** Cluster/Workers, Jobs (live + last-N), Logs (`tui-logger`),
  wired to `ListWorkers` / `ListJobs` + `WatchEvents`.
- **V1 (separate, deferred):** job detail + live log tail, digest inspector
  (tree), replication ring (Phase 3 data), mutating actions behind a
  typed-confirm modal.

## Alternatives considered

- **Reuse the web spec's `axum` REST/SSE gateway as the TUI transport.** Rejected
  *as the TUI's transport*: JSON-over-HTTP + SSE from a Rust client is awkward and
  couples TUI delivery to building the whole browser gateway. The `views`
  read-model is still shared; the gateway remains the browser's path.
- **`tui-realm` component framework instead of hand-rolled TEA.** Rejected for the
  first cut — one more dep for boilerplate we don't yet need; revisit if the panel
  count grows.
- **A persistent job-history store now.** Deferred — a bounded in-memory ring
  covers the MVP "recent jobs" view without a scheduler-storage change.
- **Expose observation by reusing REAPI / `WatchTopology` only.** Insufficient:
  REAPI has no worker/job listing, and `WatchTopology` is CAS-node membership.

## Consequences

- **Positive:** a live operator console that ships on Phase 4 infrastructure; a
  reusable `views` read-model the future web UI shares; gRPC-native streaming;
  `update()` logic testable without a terminal.
- **Negative:** pulls a Phase-6 concern forward; adds a TUI dependency cluster
  (scoped to the new binary crate); a new service surface to maintain and
  auth-scope.
- **Neutral:** job history is in-memory/bounded until a durable store is
  justified; all mutating actions are out of scope for the read-only MVP.

## References

- `docs/plan.md` §11 (phases) + §11.1 (this slice), §22 (observability).
- ADR 0011 (auth — interceptor reused for the read scope), ADR 0002 (REAPI),
  ADR 0008 / 0009 / 0010 (scheduler/tenant state the `views` read).
- `docs/superpowers/specs/2026-05-17-brokkr-frontend-design.md` (web dashboard +
  the shared read-model concept).
