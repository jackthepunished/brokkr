# 0002 — REAPI compatibility

- **Status:** accepted
- **Date:** 2026-04-30
- **Deciders:** Brokkr maintainers

## Context

Brokkr's purpose is to run arbitrary jobs (builds, tests, ML training,
transcoding) on a fleet of Linux workers and to do so for users who already
have a build tool. The dominant build tools that need a remote executor
(Bazel, Buck2, Pants) all speak the **Bazel Remote Execution API v2 (REAPI)**.
The spec lives at <https://github.com/bazelbuild/remote-apis> and is defined
in protobuf.

This is a load-bearing choice because the protocol shapes:

- The data model (Action, Command, Directory, Digest, ActionResult).
- The wire (gRPC over HTTP/2; ByteStream for blobs >4 MiB).
- The semantics of caching (`GetActionResult` is the cache; `Execute` is the
  scheduler entry point).
- The integration surface for every client we want to attract.

Project axiom: **compatibility is leverage** (`docs/plan.md` §1). We do not
ask users to rewrite their toolchain; we meet them where they already are.

There is also a tempting alternative: invent a smaller, cleaner Brokkr-native
protocol that fits exactly what we need. This is what every greenfield system
considers and most regret.

## Decision

**REAPI v2 is the canonical public protocol.** Brokkr implements every REAPI
service required by Bazel as a remote executor:

- `build.bazel.remote.execution.v2.Execution` (`Execute`, `WaitExecution`)
- `build.bazel.remote.execution.v2.ActionCache`
  (`GetActionResult`, `UpdateActionResult`)
- `build.bazel.remote.execution.v2.ContentAddressableStorage`
  (`FindMissingBlobs`, `BatchUpdateBlobs`, `BatchReadBlobs`, `GetTree`)
- `build.bazel.remote.execution.v2.Capabilities` (`GetCapabilities`)
- `google.bytestream.ByteStream` (`Read`, `Write`, `QueryWriteStatus`)

Brokkr's own internal RPCs (worker lifecycle, admin, debug) live in a
**separate proto namespace**, `brokkr.v1.*`, and are versioned independently.
They never extend or replace REAPI types in place.

Wire-level Bazel conformance is a Phase 4 hard requirement: a real
`bazel build //...` against `brokk` as the remote executor must succeed
(`docs/plan.md` §16, Phase 4 task 9).

## Alternatives considered

- **Invent a Brokkr-native protocol.**
  - Pros: zero legacy baggage; can model exactly what we need; smaller surface.
  - Cons: reinvents `Action` / `Digest` / `ActionResult` poorly; we would have
    to write Bazel/Buck2/Pants adapters anyway, paying the REAPI cost twice;
    zero day-one users; opaque to anyone who already knows REAPI.

- **Buck2-native protocol.**
  - Pros: Buck2 is technically interesting and gaining traction; its protocol
    is well-engineered.
  - Cons: not an open standard; not stable; smaller user base than Bazel;
    Buck2 itself can speak REAPI.

- **HTTP/REST + JSON.**
  - Pros: easier to debug with `curl`; lower barrier for casual contributors.
  - Cons: blob transfer is poor over HTTP/1.1 (no multiplexing, head-of-line
    blocking); JSON-over-the-wire is wasteful for digests and binary blobs;
    no native streaming primitives without HTTP/2 (at which point gRPC is the
    sensible layer); protobuf already exists and is ratified by the spec.

- **REAPI v1 / pre-v2 dialects.**
  - Cons: deprecated by Bazel; not maintained.

- **`bazel-remote` HTTP cache protocol** (subset of REAPI).
  - Pros: dead simple; many existing CI integrations use it.
  - Cons: cache-only — no remote execution, no scheduling, no worker pool.
    We need the full REAPI surface, not just the cache.

## Consequences

### Positive

- **Day-one users.** Existing Bazel/Buck2/Pants installations can point at
  `brokk` with one config flag.
- **Free conformance harness.** Bazel's own test suite tells us when we're
  spec-compliant.
- **Free prior art.** BuildBuddy, EngFlow, bazel-remote, and buildbarn all
  implement the same surface; their public docs and source illuminate
  every dark corner of the spec.
- **Stable contract.** REAPI v2 evolves slowly and additively. Our public
  surface inherits that stability without effort on our part.

### Negative

- **Inherit REAPI quirks.** ByteStream's `resource_name` format, the
  asymmetry between `Execute` and `WaitExecution`, the
  `do_not_cache` flag's interaction with `update_enabled`, and the
  `Tree`-vs-`Directory` ambiguity — all are ours to implement faithfully,
  warts included.
- **Strict digest validation is mandatory.** Every blob upload must be
  hash-verified server-side; mismatches return `INVALID_ARGUMENT`. No
  shortcuts.
- **Streaming gRPC complexity.** ByteStream `Write` is resumable and
  per-spec; we cannot replace it with a simpler unary RPC.
- **No room to fix REAPI's mistakes.** When REAPI is wrong (e.g.,
  `Platform` constraints are bag-of-strings rather than a proper type
  system), we cannot improve it without breaking compatibility — only
  layer on top in `brokkr.v1.*`.

### Neutral

- **Brokkr extensions** (`WorkerService`, `AdminService`, `JobLease`,
  `WorkerCapability`) live in `brokkr.v1.*` and are versioned per-namespace
  (`docs/plan.md` §9). Internal protocol evolution does not touch the
  public REAPI surface.
- **Vendored protos.** REAPI + `googleapis` protos live under
  `crates/brokkr-proto/protos/`; `build.rs` invokes `tonic-build` with a
  vendored `protoc` so the build needs no system protobuf installation.
- **Versioning policy** — for REAPI we follow upstream; for `brokkr.v1.*` we
  hold the line on backwards compatibility within a major version, per the
  CLAUDE.md hard rule on public API stability.

## References

- REAPI v2 spec: <https://github.com/bazelbuild/remote-apis>
- `docs/plan.md` §6 (Core Components), §8 (Data Model), §9 (Wire Protocol),
  §16 (Phase 4 Bazel conformance).
- `crates/brokkr-proto/protos/` — vendored proto layout.
- `crates/brokkr-proto/build.rs` — codegen pipeline.
- BuildBuddy: <https://www.buildbuddy.io/blog/>
- EngFlow: <https://blog.engflow.com/>
- bazel-remote: <https://github.com/buchgr/bazel-remote>
- buildbarn: <https://github.com/buildbarn/bb-remote-execution>
