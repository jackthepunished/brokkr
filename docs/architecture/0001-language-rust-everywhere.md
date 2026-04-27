# 0001 — Rust everywhere

- **Status:** accepted
- **Date:** 2026-04-28
- **Deciders:** Brokkr maintainers

## Context

Brokkr is a distributed compute grid: a control plane, a content-addressable
storage tier, a Linux sandbox runtime, a worker daemon, a CLI, and a client
SDK. The natural temptation is to split languages by domain — Go for the
control plane (because every prior-art REAPI server, BuildBuddy / EngFlow /
bazel-remote / buildbarn, is Go), Rust for the sandbox and CAS (because
Linux primitives and zero-copy paths matter), and TypeScript for any future
web UI. That split is what most teams pick.

We need to commit to a single language for the data plane and control plane
before writing real code, because the choice shapes the proto code generators,
the async runtime, the error model, the test harness, and most of the
dependency tree.

The decision is also load-bearing on the *learning* axis: Brokkr is the
single largest distributed-systems project I will attempt this year (see
`docs/plan.md` §4 Tier 1). Whatever language is chosen, I will internalize it
deeply.

## Decision

Use **Rust (edition 2021, MSRV 1.85)** for every native component of Brokkr:
control plane, CAS, worker, sandbox, SDK, CLI. Internal protos via
`prost`/`tonic`. No second native language until there is a concrete
need (e.g. a Go shim purely for downstream integration) — and even then, the
core stays Rust.

## Alternatives considered

- **Go for the control plane, Rust for the data plane.**
  - Pros: matches every prior-art REAPI server; Go's gRPC story is mature;
    GC-friendly for orchestration code that mostly shuffles bytes.
  - Cons: doubles the toolchain surface; forces protobuf maintenance in two
    code generators; risks subtle wire/type drift between planes; halves the
    code reuse for shared abstractions (digest math, retry policies, tenant
    contexts); reduces the depth of Rust learning, which is part of the goal.

- **C++ everywhere.**
  - Pros: maximum control, minimum runtime overhead, established sandbox
    primitives (Bazel's `linux-sandbox` is C++).
  - Cons: memory-safety bug surface is unacceptable for code that runs
    untrusted actions; build/dep story is a tax on a solo project;
    correctness > performance per the project axioms.

- **Zig everywhere.**
  - Pros: comptime is genuinely interesting; small, principled language.
  - Cons: ecosystem (gRPC, async, FUSE bindings, seccomp helpers) is too
    young for the data plane today; would force re-implementing too much
    plumbing before reaching the educational core (Raft, sandbox).

- **Go everywhere.**
  - Pros: fastest path to a working REAPI server; richest prior-art to learn
    from; trivial cross-compilation; mature gRPC.
  - Cons: GC pauses in CAS hot paths; weaker story for the sandbox layer
    (cgo for namespaces/seccomp is workable but unpleasant); the project
    deliberately wants to learn Rust to depth, not avoid it.

## Consequences

### Positive

- **Memory safety end-to-end.** The sandbox is the most security-sensitive
  component; running untrusted code with a memory-safe orchestrator is the
  right default.
- **One toolchain, one error model, one async runtime** (`tokio`). Trivial
  code reuse across crates; `brokkr-common` can house types and helpers used
  by every plane.
- **`tonic` + `prost`** give us a single proto codegen pipeline shared by
  REAPI types and our internal `brokkr.v1` protos.
- **Forces correctness.** The `unwrap_used = "deny"` and
  `unsafe_code = "deny"` lints in the workspace are easy to enforce when the
  whole codebase is Rust.
- **Maximum learning return** for a solo project. Rust touches every layer
  Brokkr cares about: lock-free data structures, async, FFI, FUSE, custom
  binary protocols, eventually Raft.

### Negative

- **Compile times are real.** Mitigated by `Swatinem/rust-cache` in CI,
  workspace-level incremental builds, and `lto = "thin"` only in release.
- **Smaller pool of contributors fluent in Rust + REAPI** than would exist
  for Go. Acceptable because Tier-1 success is personal mastery, not external
  contributors.
- **No mature Rust REAPI server to crib from.** Prior art is all Go. We will
  port ideas, not code. This is intentional.

### Neutral

- The CLI/control plane can run on macOS and Windows for development; only
  workers strictly require Linux. Rust handles this without source forks.
- If a future component genuinely benefits from a different language (e.g. a
  small Go shim for users embedding Brokkr in a Go program), it is a
  client-side concern and does not change this decision.

## References

- `docs/plan.md` §1 (Vision), §4 (Success criteria), §7 (Technology stack).
- BuildBuddy: <https://www.buildbuddy.io/blog/>
- EngFlow: <https://blog.engflow.com/>
- bazel-remote: <https://github.com/buchgr/bazel-remote>
- buildbarn: <https://github.com/buildbarn/bb-remote-execution>
- TiKV (Rust prior art at scale): <https://github.com/tikv/tikv>
