# 0004 — Tracing from day one

- **Status:** accepted
- **Date:** 2026-04-30
- **Deciders:** Brokkr maintainers

## Context

Brokkr's project axioms include "**observable from day one** — if you can't
trace a job's lifecycle through the system in 30 seconds, the system is
broken" (`docs/plan.md` §1). This is not aspirational; it is a constraint
on every line of code written from Phase 0 onward.

Distributed systems retrofitted with observability are observability
catastrophes. The instrumentation has to be load-bearing in the type
system: every async function on the hot path needs a span, every RPC has
to propagate context, every error has to carry structured fields. Adding
this in Phase 4 means re-touching every async fn in the codebase.

We must commit to one observability stack before Phase 1 writes its first
RPC handler.

## Decision

Use the **`tracing` crate** as the universal facade for both logging and
spans, paired with **`tracing-subscriber`** for sinks and
**`tracing-opentelemetry`** for OTLP export. Concretely:

- **Facade:** `tracing` is the *only* logging/observability API used in
  library crates. No `log`, no `slog`, no `println!`, no `eprintln!`.
- **Spans on every public async fn** at module-or-higher visibility.
  Use `#[tracing::instrument(skip(...))]` with explicit field allowlists.
- **Structured fields** (`tracing::info!(field = ?value, "message")`),
  never `format!("…{value}…")` interpolation.
- **Span on every RPC** — inbound (server handler) and outbound (client
  call). Span names follow `<service>.<method>` (`cas.batch_read_blobs`).
- **W3C Trace Context propagation** through gRPC metadata
  (`traceparent` header). Helper lives in
  `brokkr-common::trace_context::{inject, extract}`.
- **Subscriber per binary**, not per library:
  - **Dev profile:** pretty stdout subscriber with `RUST_LOG`-driven
    `EnvFilter`.
  - **Prod profile:** JSON subscriber to stdout *plus* OTLP exporter to
    a configured collector endpoint.
- **No `tokio::spawn` without `.in_current_span()`** — orphaned tasks
  break trace continuity. Enforced by code review.

Standard span fields, populated wherever they exist in scope:
`tenant_id`, `job_id`, `worker_id`, `digest`, `phase`. These names are
fixed across crates so dashboards and trace queries stay portable.

## Alternatives considered

- **`log` + `env_logger`.**
  - Pros: simplest, smallest, matches a decade of Rust convention.
  - Cons: no spans (cannot represent "this work happens inside that
    work"); no contextual fields without the `kv` extension that few
    appenders implement; useless for tracing a job across crate
    boundaries; would have to be replaced anyway by Phase 3.

- **`slog`.**
  - Pros: structured logging, contextual loggers via `BorrowedKV`.
  - Cons: no real spans, weaker async story than `tracing`, smaller
    ecosystem in 2026, no first-party OTLP integration.

- **`opentelemetry-rust` directly (no `tracing` facade).**
  - Pros: one fewer indirection layer; full OTel feature surface
    available natively.
  - Cons: ergonomics are noticeably worse than `tracing!` macros;
    every crate would need to pin an OTel API version; loses the
    pluggable subscriber model that makes tests easy.

- **`tracing` + Jaeger-only (no OpenTelemetry).**
  - Pros: lighter dep tree.
  - Cons: locks us to one backend; OTLP is the industry standard and
    Jaeger speaks it natively.

- **printf debugging / `eprintln!` until Phase N.**
  - Cons: rejected by project axiom; explicitly listed as an
    anti-pattern in `docs/plan.md` §27.

## Consequences

### Positive

- **Cross-service traces.** A `brokk run` invocation produces a single
  trace spanning CLI → control plane → CAS → worker → sandbox, because
  every hop propagates the same trace context.
- **Structured logs are queryable.** `tenant_id=X` filters work
  uniformly in stdout JSON, in OTLP collectors, and in tests.
- **Performance overhead is negligible.** `tracing`'s zero-cost
  filtering means events below the active level cost ~1ns; spans cost
  ~50ns when enabled. Far below our latency targets (`docs/plan.md`
  §23).
- **Test ergonomics.** `tracing-test` lets unit tests assert on
  emitted spans/events without taking dependencies on global state.
- **Graceful degradation.** OTLP collector down? The exporter buffers
  and drops; the application keeps running. The dev subscriber writes
  to stdout regardless.

### Negative

- **Discipline cost.** Contributors must remember `.in_current_span()`
  on spawned tasks and `skip(self)` on `&self` instrument macros.
  Mitigated by clippy lints below.
- **Subscriber init order matters.** Initializing tracing after the
  first event drops that event. Each binary's `main` initializes the
  subscriber as its first non-trivial action.
- **Span explosion is a real failure mode.** `#[instrument]` on a hot
  inner function produces millions of spans. Reserve `instrument` for
  public-facing async fns; use `tracing::trace_span!` inside hot loops
  and disable by default.

### Neutral

- **Lint enforcement.** `clippy.toml` will list:
  ```
  disallowed-macros = [
      { path = "std::println", reason = "use tracing::info! / debug!" },
      { path = "std::eprintln", reason = "use tracing::warn! / error!" },
      { path = "std::dbg", reason = "use tracing::debug!" },
  ]
  ```
  Binaries that legitimately print user-facing output (`brokk` CLI)
  opt out via `#[allow(clippy::disallowed_macros)]` on the specific
  call site.
- **Dependency footprint** is meaningful but justified: `tracing`,
  `tracing-subscriber`, `tracing-opentelemetry`, and the OTLP exporter
  add ~15 transitive crates. All are widely used and actively
  maintained.
- **Span fields documented centrally** in
  `crates/brokkr-common/src/trace_fields.rs` so cross-crate field
  names do not drift.

## References

- `docs/plan.md` §1 (Project axioms), §22 (Observability), §27
  (Anti-patterns), §23 (Performance targets).
- `tracing`: <https://docs.rs/tracing>
- `tracing-opentelemetry`: <https://docs.rs/tracing-opentelemetry>
- W3C Trace Context: <https://www.w3.org/TR/trace-context/>
- "Tracing in Rust" (Tokio blog):
  <https://tokio.rs/blog/2019-08-tracing>
- OpenTelemetry Protocol (OTLP):
  <https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/protocol/otlp.md>
