# BROKKR

**Distributed Build & Compute Grid**

> *"Many hammers. One forge."*

This document is the **single source of truth** for Brokkr's design, roadmap, engineering standards, and the master prompt used to drive Claude Code while building it. Anyone (or any AI) joining the project should read this end to end before writing a line of code.

**Document version:** 0.1.0
**Last updated:** 2026-04-27
**Owner:** Berat
**Status:** Pre-Phase 0 (planning complete, not yet bootstrapped)

---

## Table of Contents

- [Part I — Project Foundation](#part-i--project-foundation)
  - [1. Vision & Philosophy](#1-vision--philosophy)
  - [2. What Brokkr Is (and Isn't)](#2-what-brokkr-is-and-isnt)
  - [3. Naming & Terminology](#3-naming--terminology)
  - [4. Success Criteria](#4-success-criteria)
- [Part II — Technical Design](#part-ii--technical-design)
  - [5. Architecture Overview](#5-architecture-overview)
  - [6. Core Components](#6-core-components)
  - [7. Technology Stack](#7-technology-stack)
  - [8. Data Model](#8-data-model)
  - [9. Wire Protocol](#9-wire-protocol)
  - [10. Storage Layout](#10-storage-layout)
- [Part III — Roadmap](#part-iii--roadmap)
  - [11. Phased Development Plan](#11-phased-development-plan)
  - [12. Phase 0 — Bootstrapping](#12-phase-0--bootstrapping)
  - [13. Phase 1 — First End-to-End Slice](#13-phase-1--first-end-to-end-slice)
  - [14. Phase 2 — Hermetic Sandboxing](#14-phase-2--hermetic-sandboxing)
  - [15. Phase 3 — Distributed Cache](#15-phase-3--distributed-cache)
  - [16. Phase 4 — Scheduler & Multi-Tenancy](#16-phase-4--scheduler--multi-tenancy)
  - [17. Phase 5 — Consensus & HA](#17-phase-5--consensus--ha)
  - [18. Phase 6+ — Advanced Features](#18-phase-6--advanced-features)
- [Part IV — Engineering Practice](#part-iv--engineering-practice)
  - [19. Repository Structure](#19-repository-structure)
  - [20. Coding Standards](#20-coding-standards)
  - [21. Testing Strategy](#21-testing-strategy)
  - [22. Observability](#22-observability)
  - [23. Performance Targets](#23-performance-targets)
- [Part V — Working With Claude Code](#part-v--working-with-claude-code)
  - [24. CLAUDE.md (Master System Prompt)](#24-claudemd-master-system-prompt)
  - [25. How to Use Claude Code on Brokkr](#25-how-to-use-claude-code-on-brokkr)
  - [26. Per-Task Prompt Templates](#26-per-task-prompt-templates)
  - [27. Anti-Patterns](#27-anti-patterns)
- [Part VI — Reference](#part-vi--reference)
  - [28. Reading List](#28-reading-list)
  - [29. Glossary](#29-glossary)
  - [30. Decision Log Template](#30-decision-log-template)
  - [31. Open Questions](#31-open-questions)

---

# Part I — Project Foundation

## 1. Vision & Philosophy

Brokkr is a **self-hosted, open-source distributed compute platform** that turns a fleet of Linux machines into a single, coherent grid for executing arbitrary jobs — builds, tests, ML training, transcoding, anything that fits in a sandbox.

It exists because:

1. CI is slow and expensive. Engineers wait. Bills explode.
2. Existing solutions (Bazel Remote Execution, BuildBuddy, EngFlow) are excellent but proprietary, narrow, or commercially closed.
3. Building this is the most educational distributed-systems project I can attempt — it touches storage, consensus, scheduling, sandboxing, networking, and protocol design simultaneously.

**Project axioms** (these never change):

- **Determinism is sacred.** The same inputs produce the same outputs, on any worker, at any time. Anything that breaks determinism is a bug.
- **Correctness > performance > convenience.** A slow correct system can be optimized. A fast wrong system is worthless.
- **Compatibility is leverage.** Speak existing protocols (Bazel REAPI). Don't ask users to rewrite their toolchain.
- **Boring tech in the data plane.** Use exotic ideas in design, but pick proven primitives (TCP, gRPC, Merkle trees) for execution.
- **Security is not bolted on.** Untrusted code runs by default. Every layer assumes the layer above it is hostile.
- **Observable from day one.** If you can't trace a job's lifecycle through the system in 30 seconds, the system is broken.

## 2. What Brokkr Is (and Isn't)

**Brokkr is:**

- A **Remote Execution API (REAPI) compatible** server, so existing Bazel/Buck2/Pants users can point their tooling at it on day one.
- A **content-addressable storage (CAS)** system for build inputs and outputs, with deduplication and tiered storage.
- An **action cache** keyed on `hash(command + environment + inputs)` so repeated work is never re-executed.
- A **sandbox runtime** that executes jobs hermetically using Linux primitives (namespaces, cgroups, seccomp).
- A **scheduler** that distributes jobs to workers based on capability matching, locality, and load.
- A **CLI + SDK** so users without Bazel can wrap arbitrary commands (`brokk run -- go build ./...`).

**Brokkr is NOT:**

- A general-purpose container orchestrator (use Kubernetes).
- A long-running service runtime (jobs are short-lived, batch-style).
- A SaaS product. Brokkr is the engine; a hosted offering can come later.
- A Windows or macOS host platform. **Workers run Linux only.** Clients can be any OS.
- A replacement for `docker run`. The sandbox is purpose-built for deterministic, ephemeral build/compute jobs.

## 3. Naming & Terminology

**Project name:** `brokkr` (always lowercase in code, paths, and CLI)
**Pronunciation:** `BROCK-er`
**CLI binary:** `brokk`
**Origin:** Brokkr, the Norse dwarven smith who, with his brother Eitri, forged Mjölnir. The name evokes craftsmanship, collaboration, and small skilled workers producing legendary results — exactly what a distributed compute grid does.

**User-facing terminology** (use sparingly in docs and CLI output, never in code identifiers):

| Concept | Brokkr term | Code identifier |
|---|---|---|
| Worker node | Smith | `Worker` |
| Cluster | Forge | `Cluster` |
| Control plane | Anvil | `ControlPlane` |
| Job | Work | `Job` / `Action` |
| Cache | Hoard | `Cache` |
| Built artifact | Ingot | `Artifact` |
| CLI | `brokk` | n/a |

**Rule:** flavored terminology lives in docs, marketing, and a few user-facing CLI strings. Code uses plain technical names so the codebase is greppable and approachable.

## 4. Success Criteria

A project this large needs concrete checkpoints. "Success" for Brokkr is measured in tiers:

**Tier 1 — Personal mastery (the real goal).**
After Phase 4 ships, I should be able to:

- Implement Raft from scratch without consulting external code.
- Explain to a senior engineer how Brokkr's CAS deduplicates inputs across millions of files.
- Debug a sandbox escape, scheduler livelock, or replication divergence using only logs and traces.
- Write a coherent design doc for any of: distributed transactions, lock-free data structures, custom binary protocols.

**Tier 2 — Functional MVP (Phase 4 complete).**
Brokkr can:

- Run real Bazel builds against `brokk` as the remote executor.
- Survive a worker crashing mid-build without job loss.
- Cache hit rate >95% on incremental builds of a medium repository.
- Serve at least 100 concurrent jobs across 10+ workers.
- Boot a fresh cluster in under 60 seconds.

**Tier 3 — Public reception (post-Phase 6).**
Optional and far in the future:

- 100+ GitHub stars with non-trivial issues filed by strangers.
- One external contributor merging a PR.
- A blog post written about Brokkr by someone who isn't me.

Tier 1 is non-negotiable. Tier 2 is the ship goal. Tier 3 is a bonus, not a metric to optimize for.

---

# Part II — Technical Design

## 5. Architecture Overview

```
                          ┌─────────────────────────────────┐
                          │           CLI / SDK              │
                          │  (brokk run, brokk cache, etc.)  │
                          └────────────────┬─────────────────┘
                                           │ gRPC over TLS
                                           │ (REAPI v2 + Brokkr ext)
                          ┌────────────────▼─────────────────┐
                          │         CONTROL PLANE             │
                          │  ┌─────────────────────────────┐  │
                          │  │  API Gateway (gRPC)          │  │
                          │  │  Action Cache Lookup         │  │
                          │  │  Scheduler                   │  │
                          │  │  Auth / mTLS                 │  │
                          │  │  Metadata Store (Raft KV)    │  │
                          │  └─────────────────────────────┘  │
                          └─┬───────────────┬─────────────┬───┘
                            │               │             │
              ┌─────────────▼──┐   ┌────────▼────────┐   │
              │  CAS NODE A    │   │  CAS NODE B     │   │  ...
              │  (sharded by   │   │                 │   │
              │   hash prefix) │   │                 │   │
              └────────────────┘   └─────────────────┘   │
                                                          │
                          ┌──────────────────────────────▼───┐
                          │            WORKER POOL            │
                          │   ┌──────┐ ┌──────┐ ┌──────┐    │
                          │   │Worker│ │Worker│ │Worker│ ...│
                          │   │ + sandbox + sandbox + sandbox│
                          │   └──────┘ └──────┘ └──────┘    │
                          └──────────────────────────────────┘
```

**Plane separation:**

- **Control plane** holds metadata, makes scheduling decisions, serves the API. Strongly consistent (Raft). Rarely scales beyond 3–5 nodes.
- **Data plane** is CAS storage and worker execution. Eventually consistent for storage, stateless for workers. Scales horizontally to thousands.

**Request lifecycle for a job:**

1. Client calls `Execute(action)` over gRPC.
2. Control plane hashes the action; checks **action cache**. Hit → return cached result. Done.
3. Miss → control plane verifies all input blobs exist in CAS (`FindMissingBlobs`). Client uploads any missing blobs.
4. Scheduler picks a worker matching the action's platform requirements.
5. Worker downloads inputs from CAS (lazily, via FUSE in later phases).
6. Worker executes inside sandbox.
7. Worker uploads output blobs to CAS, returns ActionResult to control plane.
8. Control plane stores ActionResult in action cache, returns to client.

## 6. Core Components

### 6.1 CAS (Content-Addressable Storage)

- Every blob identified by `(sha256, size)` tuple.
- API: `FindMissingBlobs`, `BatchUpdateBlobs`, `BatchReadBlobs`, `Read` (streaming), `Write` (streaming, resumable).
- Deduplication is automatic — same hash, same blob, stored once.
- Directory trees represented as Merkle DAGs (`Directory` proto contains hashes of child files/dirs).
- Tiered storage:
  - **Hot:** in-memory LRU (small, frequently accessed blobs)
  - **Warm:** local NVMe per CAS node
  - **Cold:** S3-compatible object storage (MinIO for self-hosted, S3 for cloud)
- Garbage collection: reference counted via action cache + retention policies.

### 6.2 Action Cache

- Key: `digest(Action proto)` — captures command, args, env, input root digest, platform.
- Value: `ActionResult` proto — output digests, exit code, stderr/stdout digests.
- Stored in metadata store (Raft KV), not CAS, because it must be strongly consistent.
- TTL configurable per tenant; default 30 days.

### 6.3 Scheduler

- Pluggable strategies (start with `simple-fifo`, evolve to `bin-packing`, then `locality-aware`).
- Workers register capabilities (CPU arch, OS, available tools, GPU info, RAM, disk).
- Actions specify platform requirements (`Platform` proto — key/value constraints).
- Match: worker satisfies all action constraints AND has spare capacity.
- Backpressure: if no worker matches, action queues; if queue exceeds threshold, surface to user.

### 6.4 Worker

- Long-running daemon on each Linux node.
- Heartbeats to control plane every N seconds with capability + load.
- Receives actions via streaming gRPC.
- For each action:
  1. Materialize inputs (Phase 1: download to local dir; Phase 3: FUSE mount from CAS).
  2. Build sandbox (Phase 1: bare process; Phase 2: namespaces+cgroups+seccomp).
  3. Execute, capture stdout/stderr/exit.
  4. Upload outputs.
  5. Tear down sandbox.
- Crash-safe: in-flight actions are reassigned by scheduler if worker disappears.

### 6.5 Sandbox Runtime (Phase 2)

- Built directly on Linux primitives — no Docker, no containerd, no runc dependency.
- Isolation layers (in order of construction):
  1. **Mount namespace + pivot_root** — only see input files + minimal `/dev`, `/proc`, `/tmp`.
  2. **PID namespace** — sandboxed process is PID 1, can't see host processes.
  3. **Network namespace** — by default, no network. Opt-in only.
  4. **User namespace** — map sandbox UID 0 to unprivileged host UID.
  5. **cgroups v2** — CPU quota, memory limit, PIDs limit, IO throttling.
  6. **seccomp-bpf** — whitelist of allowed syscalls (deny RDTSC, ptrace, mount, etc.).
  7. **Capability dropping** — `CAP_SYS_ADMIN` etc. removed.
- Output: process exit code, stdout, stderr, declared output files.
- Future: optional gVisor-style userspace kernel for higher isolation.

### 6.6 Metadata Store

- Strongly consistent KV store, internal to control plane.
- Phase 0–4: SQLite or RocksDB on a single control-plane node.
- Phase 5: replace with custom Raft KV (the educational centerpiece).
- Stores: action cache, worker registry, tenants, ACLs, GC metadata.

### 6.7 CLI + SDK

- CLI binary `brokk` written in Rust.
- Subcommands: `init`, `run`, `build`, `cache`, `worker`, `cluster`, `admin`, `version`.
- Library crate `brokkr-sdk` exposes the same APIs for embedding.
- Optional Go shim later (so non-Rust users can integrate).

## 7. Technology Stack

**Default stack** (chosen for maximum learning + production credibility):

| Layer | Choice | Rationale |
|---|---|---|
| Core language | **Rust** (stable, edition 2021) | Memory safety + low-level control + best ecosystem for this domain (Tokio, Tonic, Prost). Forces correctness. Most respected language for this kind of work today. |
| Async runtime | **Tokio** | De facto standard. Mature scheduler, ecosystem alignment. |
| RPC | **Tonic** (gRPC over HTTP/2) | REAPI is gRPC-native. Tonic is the canonical Rust impl. |
| Protobuf codegen | **Prost** | Clean API, no runtime descriptor overhead. |
| Serialization (non-RPC) | **bincode** for hot paths, **serde_json** for config | bincode is fast and stable; JSON for human-edited files. |
| Embedded KV (Phase 0–4) | **redb** or **sled** | Pure Rust, embedded, ACID. Pick `redb` (more conservative, B-tree based). |
| Object storage client | **rust-s3** or **opendal** | OpenDAL gives multi-backend abstraction (S3, MinIO, local). Prefer OpenDAL. |
| Hashing | **sha2** crate (SHA-256) | REAPI requires it. |
| Logging | **tracing** + **tracing-subscriber** | Structured logs + spans for distributed tracing. |
| Metrics | **metrics** crate + **metrics-exporter-prometheus** | Prometheus is standard. |
| Distributed tracing | **tracing-opentelemetry** + OTLP exporter | Industry standard. |
| CLI parsing | **clap** v4 (derive macros) | Best-in-class. |
| Sandbox primitives | **nix** crate + **caps** crate + **libseccomp** | Direct Linux syscall bindings. |
| FUSE (Phase 3) | **fuser** | Modern, maintained FUSE binding. |
| Testing — property-based | **proptest** | Catches non-obvious bugs. |
| Testing — fault injection | **turmoil** (Tokio-native deterministic simulation) | Lets us test distributed code deterministically. Critical for Raft. |
| Build system | **Cargo workspaces** | Multi-crate monorepo. |
| CI | **GitHub Actions** initially | Self-hosted on Brokkr later (eat our own dog food). |
| OS target | **Linux x86_64** primary, **Linux aarch64** secondary | Workers must be Linux. CLI/control plane can run on macOS/Windows for development. |
| Min Rust version | **MSRV 1.78** | Pin and bump deliberately. |

**Rejected alternatives** (and why):

- **Go for control plane:** considered, but splitting languages adds protocol drift risk and prevents code reuse. Rust everywhere is harder up front, deeper learning long term.
- **C++:** maximum performance, minimum safety. Not worth the bug surface for a solo project.
- **Zig:** intriguing but ecosystem too young for this domain.
- **etcd as metadata store (forever):** fine for Phase 0–4, but the whole point is to learn consensus by writing it. Replace in Phase 5.
- **Docker for sandboxing:** abdicates the most educational part of the project.

## 8. Data Model

Brokkr is REAPI-compatible. The canonical proto definitions live at:
`https://github.com/bazelbuild/remote-apis/tree/main/build/bazel/remote/execution/v2`

**Core types** (paraphrased from REAPI):

```protobuf
message Digest {
  string hash = 1;       // hex-encoded SHA-256
  int64  size_bytes = 2;
}

message Action {
  Digest command_digest = 1;
  Digest input_root_digest = 2;
  google.protobuf.Duration timeout = 6;
  bool   do_not_cache = 7;
  Digest platform_digest = 9;
}

message Command {
  repeated string arguments = 1;
  repeated EnvironmentVariable environment_variables = 2;
  repeated string output_paths = 7;
  string  working_directory = 6;
  Platform platform = 9;
}

message Directory {
  repeated FileNode files = 1;
  repeated DirectoryNode directories = 2;
  repeated SymlinkNode symlinks = 3;
}

message ActionResult {
  repeated OutputFile output_files = 2;
  repeated OutputDirectory output_directories = 4;
  int32  exit_code = 4;
  Digest stdout_digest = 5;
  Digest stderr_digest = 6;
  ExecutedActionMetadata execution_metadata = 7;
}
```

**Brokkr extensions** (our own proto namespace `brokkr.v1`):

```protobuf
message WorkerCapability {
  string  worker_id = 1;
  Platform platform = 2;
  uint32  cpu_cores = 3;
  uint64  memory_bytes = 4;
  uint64  disk_bytes = 5;
  repeated string installed_tools = 6;  // e.g. "go-1.22", "rustc-1.78"
  GpuInfo gpu = 7;                       // optional
}

message JobLease {
  string job_id = 1;
  string worker_id = 2;
  google.protobuf.Timestamp leased_at = 3;
  google.protobuf.Duration  lease_duration = 4;
}
```

## 9. Wire Protocol

- **Transport:** gRPC over HTTP/2, TLS by default (mTLS for worker↔control-plane).
- **Compatibility:** must implement these REAPI services exactly:
  - `Execution` (`Execute`, `WaitExecution`)
  - `ActionCache` (`GetActionResult`, `UpdateActionResult`)
  - `ContentAddressableStorage` (`FindMissingBlobs`, `BatchUpdateBlobs`, `BatchReadBlobs`, `GetTree`)
  - `Capabilities` (`GetCapabilities`)
  - `bytestream.ByteStream` (`Read`, `Write`, `QueryWriteStatus`) — for blobs >4 MiB.
- **Brokkr-internal services** (proto namespace `brokkr.v1`):
  - `WorkerService` — workers register, heartbeat, lease jobs, report results.
  - `AdminService` — cluster ops, metrics, debug endpoints.
- **Versioning:** all Brokkr protos versioned (`brokkr.v1`, `brokkr.v2`); never break wire compat within a major version.

## 10. Storage Layout

**Worker local storage:**
```
/var/lib/brokkr/
├── cas/                     # local CAS cache, content-addressed
│   ├── sha256/
│   │   ├── 00/00ab1f.../    # first 2 hex chars sharded
│   │   └── ...
├── work/                    # job working directories
│   └── <job_id>/
│       ├── inputs/
│       └── outputs/
└── logs/
```

**CAS node storage:**
```
/var/lib/brokkr-cas/
├── hot/                     # mmap'd blobs <1 MiB
├── warm/                    # local NVMe blobs
│   └── sha256/<sharded>
└── meta/
    └── refcount.redb        # GC reference counts
```

**Cold storage** (S3-compatible bucket): same sharded layout, used for blobs not accessed in N days.

**Control plane:**
```
/var/lib/brokkr-control/
├── action-cache.redb        # action cache KV
├── workers.redb             # worker registry
├── tenants.redb             # tenant config + quotas
└── raft/                    # Phase 5: Raft log + snapshots
    ├── log/
    └── snapshots/
```

---

# Part III — Roadmap

## 11. Phased Development Plan

Each phase is a **shippable milestone**, not a time estimate. Move to the next only when the previous is *demonstrably correct*, not just compiling.

| Phase | Theme | Demo |
|---|---|---|
| **0** | Bootstrapping | `cargo build` succeeds across the workspace; CI green; `brokk version` runs. |
| **1** | First end-to-end slice | Single worker. `brokk run -- echo hello` returns "hello", caches result, second run is a cache hit. |
| **2** | Hermetic sandbox | The above runs inside namespaces+cgroups+seccomp. Sandboxed process can't read host files. |
| **3** | Distributed CAS | Multiple CAS nodes with hash-prefix sharding. Workers fetch inputs over network. FUSE materialization. |
| **4** | Scheduler & multi-tenancy | 10+ workers, parallel jobs, fair scheduling across tenants, REAPI-compatible enough to run a real Bazel build. |
| **5** | Raft consensus & HA | Replace embedded KV with custom Raft. Survive control-plane node loss. |
| **6+** | Advanced features | Speculative execution, FUSE optimizations, GPU scheduling, federation, web UI, etc. |

**Phase exit criteria** (every phase must satisfy these before moving on):
- All public APIs documented with rustdoc.
- All new modules have unit tests with ≥80% line coverage on logic-heavy code.
- At least one integration test exercises the new capability end to end.
- Tracing spans cover the new code paths.
- A short blog-style retrospective is written in `docs/journal/phase-N.md` describing what was learned, what surprised, and what was deferred.

## 12. Phase 0 — Bootstrapping

**Goal:** stand up the skeleton. No real functionality yet. By the end, `cargo build` and `cargo test` work across the workspace, CI is green, and a `brokk version` binary prints the version.

**Tasks:**

1. Initialize Cargo workspace at repo root.
2. Create crates (see Repository Structure section): `brokkr-cli`, `brokkr-sdk`, `brokkr-proto`, `brokkr-cas`, `brokkr-control`, `brokkr-worker`, `brokkr-common`.
3. Add `rust-toolchain.toml` pinning Rust 1.78.
4. Add `rustfmt.toml`, `clippy.toml` (deny warnings, deny `unwrap_used` in lib code).
5. Add `.editorconfig`, `.gitignore`, `LICENSE` (MIT or Apache-2.0; pick one — Apache-2.0 recommended for infra projects).
6. Add `README.md` (short — link to this plan).
7. Add `CLAUDE.md` (the master prompt — see Section 24).
8. Vendor REAPI proto files into `crates/brokkr-proto/protos/`.
9. Set up `build.rs` in `brokkr-proto` to invoke `tonic-build`.
10. Implement minimal `brokkr-cli` with `clap` exposing `brokk version` and `brokk init`.
11. Set up GitHub Actions workflow: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace`, `cargo build --release` on Linux x86_64 and aarch64.
12. Add CONTRIBUTING.md, CODE_OF_CONDUCT.md placeholders.
13. Add a justfile or Makefile for common dev commands.

**Definition of done:** A teammate could `git clone`, run `cargo build`, run `brokk version`, and see output. CI is green.

## 13. Phase 1 — First End-to-End Slice

**Goal:** the thinnest possible slice through every layer. One control plane, one worker, both on `localhost`. No real isolation, no network distribution, but the *protocol* and *flow* are real.

**Concretely, this command must work:**
```
brokk run --command "echo hello world"
# → stdout: "hello world"
# → exit code: 0
# → second invocation hits cache, returns instantly
```

**Tasks:**

1. **Proto compilation:** wire up REAPI protos through Prost/Tonic. Generate Rust types for `Action`, `Command`, `Digest`, `ActionResult`.
2. **CAS (in-memory first):** implement `BatchUpdateBlobs`, `BatchReadBlobs`, `FindMissingBlobs` against an in-memory `HashMap<Digest, Bytes>`. Make sure `Digest` validation is strict (hash matches content).
3. **CAS (on-disk):** add a `redb`-backed implementation that persists to `/var/lib/brokkr/cas/`.
4. **Action cache:** implement `GetActionResult`, `UpdateActionResult` against a `redb` table keyed on action digest.
5. **Control plane server:** Tonic server exposing CAS, ActionCache, Capabilities, and a stub Execution service.
6. **Worker daemon:** registers with control plane, polls for jobs over a streaming RPC, executes them as a child process (no sandbox yet — `tokio::process::Command`), uploads outputs.
7. **CLI `brokk run`:**
   - Hashes the command, builds an `Action`.
   - Uploads any missing input blobs (none for `echo`, but the code path runs).
   - Calls `Execute`.
   - Streams the result.
   - Pretty-prints stdout, exit code, cache hit/miss.
8. **Integration test:** spin up control plane + worker in-process, run an action, assert the result. Run the same action twice, assert second is a cache hit (verified by execution metadata).
9. **Tracing:** spans for `client::execute`, `control::dispatch`, `worker::run_action`. Output to stdout in dev, to OTLP in prod.

**Definition of done:** the integration test passes deterministically 100 times in a row. Cache hit measurably faster than miss.

## 14. Phase 2 — Hermetic Sandboxing

**Goal:** worker executes actions inside a real Linux sandbox. A malicious action cannot read host files, see host processes, exhaust host memory, or persist anything outside its declared outputs.

**Tasks:**

1. **Mount namespace + pivot_root.**
   - Create new mount namespace.
   - Build a minimal rootfs in a tmpfs: just `/usr/bin`, `/lib`, `/lib64` bind-mounted read-only from host (configurable allowlist), plus `/tmp` and `/work` (writable tmpfs).
   - `pivot_root` into it.
   - Verify the sandboxed process cannot see `/etc/shadow`.
2. **PID namespace.** Sandboxed process is PID 1. Use a small reaper to handle SIGCHLD.
3. **User namespace.** Map host UID `brokkr-sandbox` (unprivileged, dedicated) to UID 0 inside.
4. **Network namespace.** New empty namespace by default — no `lo`, no routes. Opt-in network access reads from action's `Platform` constraints.
5. **cgroups v2.** Per-action cgroup with CPU, memory, pids, io limits. OOM kill = action failure with structured error.
6. **seccomp-bpf.** Default-deny syscall filter; whitelist `read`, `write`, `open`, `openat`, `close`, `mmap`, `munmap`, `brk`, `execve`, `wait4`, `exit_group`, `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn`, `clone`, `clone3`, `pipe`, `pipe2`, `dup`, `dup2`, `dup3`, `getpid`, `gettid`, `getuid`, `geteuid`, `getgid`, `getegid`, `getcwd`, `chdir`, `fchdir`, `stat`, `fstat`, `lstat`, `newfstatat`, `lseek`, `prlimit64`, `arch_prctl`, `sched_yield`, `nanosleep`, `clock_gettime`, `clock_nanosleep`, `futex`. (Iterate from this list; some compilers need more.)
7. **Capability dropping.** All capabilities removed except as explicitly required.
8. **Determinism guards.**
   - `LD_PRELOAD` blocked.
   - `/proc/self/environ` minimal.
   - Hostname set to fixed value (`brokkr-sandbox`).
   - Timezone forced to UTC.
   - Source date epoch injected.
9. **Resource accounting.** Track and report CPU time, max RSS, IO bytes per action.
10. **Tests.** A suite of "evil action" tests:
    - tries to `cat /etc/passwd` → must fail.
    - tries to `mount` → must fail.
    - tries to fork bomb → must be killed by pids cgroup.
    - tries to allocate 100GB → must be killed by memory cgroup.
    - tries to `nslookup` → must fail (no network).
    - tries `RDTSC` → must fail (seccomp).

**Definition of done:** every evil-action test passes. A real-world action (e.g., `gcc hello.c -o hello`) succeeds.

## 15. Phase 3 — Distributed Cache

**Goal:** CAS scales across multiple nodes. Workers fetch inputs over the network. Inputs are materialized lazily via FUSE.

**Tasks:**

1. **Hash-prefix sharding.** CAS cluster has N nodes; blob with hash starting with prefix `P` lives on node `hash_to_node(P)`. Use rendezvous hashing (HRW) for stable assignment under node changes.
2. **Replication.** Each blob replicated to `R` nodes (default 2). Reads hit closest replica; writes go to all.
3. **Coordinator/router.** Either a thin gateway service or client-side routing. Start with client-side (simpler) using a published cluster topology.
4. **Tiered storage.**
   - Hot: in-memory LRU per node (size configurable).
   - Warm: local disk.
   - Cold: S3/MinIO via OpenDAL, async-promoted on read.
5. **FUSE input materialization.**
   - Mount a FUSE filesystem at `/work/<job>/inputs/`.
   - Lazily fetch file contents from CAS on `read()`.
   - Cache opened files locally for the action's lifetime.
   - Goal: a 5 GB input tree should mount in <100 ms; only files actually read are downloaded.
6. **GC.** Reference counting via action cache + LRU eviction in warm tier.
7. **Bloom filter.** Each CAS node maintains a bloom filter of held digests; `FindMissingBlobs` consults the filter first to avoid disk hits.

**Definition of done:** 3-node CAS cluster, kill any one node, builds keep working. FUSE materialization measurably reduces input transfer for partial-read workloads.

## 16. Phase 4 — Scheduler & Multi-Tenancy

**Goal:** real scheduling across many workers, fair sharing across tenants, REAPI-compatible enough to run real Bazel.

**Tasks:**

1. **Worker registry with capabilities.** Workers heartbeat every 5s; control plane evicts after 3 missed heartbeats.
2. **Constraint matching.** Action's `Platform` requirements matched against worker capabilities. Hard constraints (must match) vs. soft (preferred).
3. **Scheduling strategies** (pluggable trait):
   - `SimpleFifo` — first available worker.
   - `BinPacking` — fill workers to threshold before using new ones.
   - `LocalityAware` — prefer worker that has recent inputs cached locally.
4. **Tenants.** Every API call carries a tenant ID (from auth). Quotas: max concurrent jobs, max CPU-seconds/day, max storage.
5. **Fair scheduling.** Weighted fair queuing: each tenant gets a slice proportional to its weight; no tenant can starve another.
6. **Preemption.** Optional. Lower-priority job evicted to free a worker for higher-priority job.
7. **Job leases.** Worker leases a job for N seconds; must renew. If lease expires, job returns to queue.
8. **Auth.**
   - mTLS between workers ↔ control plane.
   - OIDC or static API tokens for clients.
9. **Bazel compatibility test.** A real `bazel build //...` against `brokk` as the remote executor must succeed on a small open-source project.

**Definition of done:** Bazel test passes. Two tenants running concurrently each get fair share. Worker crash mid-job → job retried on another worker, completes successfully.

## 17. Phase 5 — Consensus & HA

**Goal:** the metadata store becomes a custom Raft implementation. Control plane survives node loss.

**This is the educational centerpiece.** Do not skip, do not import an existing Raft library. Implement from the paper.

**Tasks:**

1. **Read the Raft paper end to end.** Take notes. (`docs/raft-notes.md`.)
2. **Implement Raft state machine.**
   - Leader election (RequestVote RPC).
   - Log replication (AppendEntries RPC).
   - Safety properties (election restriction, leader completeness).
   - Persistent state (currentTerm, votedFor, log) via redb.
3. **Implement deterministic simulation tests with `turmoil`.**
   - Network partitions.
   - Message reordering.
   - Process crashes mid-write.
   - Property: linearizability of all committed entries.
4. **Log compaction via snapshots.**
5. **Membership changes** (joint consensus).
6. **Replace embedded KV with Raft-backed KV** in the control plane.
7. **HA control plane.** 3 or 5 control-plane nodes. Clients can talk to any; followers redirect to leader.
8. **Jepsen-style tests.** Inject faults under load; verify no committed write is ever lost.

**Definition of done:** kill the leader; cluster elects a new one in <2s. Partition the cluster; minority side stops accepting writes; rejoin → consistent. Run for 1M operations under fault injection with zero divergence.

## 18. Phase 6+ — Advanced Features

In rough priority order. Pick what's interesting; nothing here is required.

- **Speculative execution.** Predict next jobs, pre-warm caches.
- **Cross-region replication.** CRDTs for action cache; async S3 cross-region for CAS.
- **GPU scheduling.** CUDA capability detection, MIG/MPS sharing.
- **Reproducibility verification.** Run every Nth job on 3 workers; quarantine on divergence.
- **Web UI.** React + tRPC, browse jobs, view logs, see cluster health.
- **Federation.** Multiple Brokkr clusters share caches across organizations.
- **Stream-mode execution.** Not just batch — long-running services with structured restarts.
- **Bazel BES integration.** Build event stream for IDE integrations.
- **Connector plugins.** PostgreSQL CDC, Kafka, Elasticsearch sources/sinks.
- **WASM-based extension hooks.** User-defined scheduling policies.

---

# Part IV — Engineering Practice

## 19. Repository Structure

```
brokkr/
├── Cargo.toml                     # workspace root
├── rust-toolchain.toml            # pin: 1.78.0
├── rustfmt.toml
├── clippy.toml
├── deny.toml                      # cargo-deny config
├── .editorconfig
├── .gitignore
├── LICENSE
├── README.md                      # 1-page intro, links to docs/
├── CLAUDE.md                      # master prompt for Claude Code
├── CONTRIBUTING.md
├── CHANGELOG.md
├── justfile                       # dev shortcuts
│
├── crates/
│   ├── brokkr-common/             # shared types, utilities, errors
│   ├── brokkr-proto/              # protobuf definitions + generated code
│   ├── brokkr-cas/                # CAS server + client
│   ├── brokkr-control/            # control plane (action cache, scheduler, API gateway)
│   ├── brokkr-worker/             # worker daemon
│   ├── brokkr-sandbox/            # sandbox runtime (Linux primitives)
│   ├── brokkr-sdk/                # client library
│   ├── brokkr-cli/                # `brokk` binary
│   └── brokkr-test-utils/         # shared test helpers, fixtures
│
├── docs/
│   ├── plan.md                    # this file (the source of truth)
│   ├── architecture/              # ADRs and architecture deep-dives
│   │   ├── 0001-language-rust.md
│   │   ├── 0002-reapi-compat.md
│   │   └── ...
│   ├── journal/                   # phase retrospectives
│   │   └── phase-0.md
│   └── runbooks/                  # operational guides
│
├── examples/
│   ├── hello-world/               # smallest working example
│   └── bazel-integration/         # how to point Bazel at brokk
│
├── tests/                         # workspace-level integration tests
│   └── e2e/
│
├── scripts/                       # dev/ops scripts (bash, python)
│
└── .github/
    └── workflows/
        ├── ci.yml
        ├── release.yml
        └── security.yml
```

**Crate dependencies** (allowed direction; arrows = "depends on"):

```
brokkr-cli ──► brokkr-sdk ──► brokkr-proto ──► brokkr-common
brokkr-control ─┐
brokkr-worker  ─┴► brokkr-proto ──► brokkr-common
brokkr-cas    ──► brokkr-proto ──► brokkr-common
brokkr-sandbox ─► brokkr-common
brokkr-worker ──► brokkr-sandbox
```

**Rule:** no cycles, no shortcuts. `brokkr-common` is the only universal dependency.

## 20. Coding Standards

### 20.1 Rust style

- Edition 2021.
- `cargo fmt` enforced.
- `cargo clippy -- -D warnings` enforced.
- **No `unwrap()` or `expect()` in library crates.** Tests and binaries may use them sparingly. Use `?` with proper error types.
- **No `panic!()` in library code** except for unreachable invariants documented in the code.
- Errors: use `thiserror` for library error enums, `anyhow` only in binaries.
- Public APIs documented with `///` rustdoc; module roots have `//!` overview.
- **No `unsafe`** without an `// SAFETY:` comment explaining why it's sound.
- Lint config (`clippy.toml`):
  ```
  disallowed-methods = ["std::result::Result::unwrap", "std::option::Option::unwrap"]
  ```
- `#![deny(missing_docs)]` on every public crate.

### 20.2 Project conventions

- **One concept per module.** If a file exceeds ~500 lines, split it.
- **Newtype everything.** Don't pass raw `String` for IDs; define `WorkerId(String)`, `JobId(String)`, etc.
- **No `Vec<u8>` for blobs in public APIs.** Use `bytes::Bytes` (cheap clone, slice-able).
- **All async functions return `Result<T, E>` with a typed error.**
- **Use `tracing::instrument` on every public async function** at module-or-higher visibility.
- **No `tokio::spawn` without a span attached** (use `tokio::spawn(async move { ... }.in_current_span())`).
- **No global mutable state.** Use dependency injection via constructor.
- **Configuration is explicit.** No magic env vars; every config field has a struct field.
- **Feature flags** for optional dependencies (e.g. `s3-storage`, `gpu-scheduling`).

### 20.3 Git hygiene

- **Conventional commits:** `feat(cas):`, `fix(worker):`, `docs:`, `refactor:`, `test:`, `chore:`.
- **Atomic commits.** One logical change per commit.
- **No "WIP" commits on `main`.** Squash before merge.
- **PRs require:** green CI + at least one reviewer (in solo phase, self-review with 24h gap = OK).
- **Conventional branch names:** `feat/cas-sharding`, `fix/worker-leak`.

## 21. Testing Strategy

| Layer | Tool | Coverage target |
|---|---|---|
| Unit | `#[cfg(test)]` modules | ≥80% lines on logic-heavy code |
| Integration | `tests/` directory in each crate | Every public API has at least one happy-path + one error-path test |
| End-to-end | Workspace `tests/e2e/` | Each phase's "definition of done" demo, scripted |
| Property-based | `proptest` | All serialization, hashing, and parsing code |
| Fault injection | `turmoil` (Tokio simulation) | Every distributed protocol (Phase 5+) |
| Fuzz | `cargo-fuzz` | Proto parsers, hash boundaries, FUSE edge cases |
| Sandbox security | `tests/sandbox-evil/` | Every isolation boundary tested with an attacker |
| Performance | `criterion` benchmarks | Hot paths (hashing, CAS read/write) |
| Compatibility | Bazel REAPI conformance suite | Phase 4+ |

**Test invariants:**

- Tests must be deterministic. No `std::time::SystemTime::now()` in tests; use injected clocks.
- No real network in unit tests. Use `tokio::test` with in-memory transports.
- Integration tests use `testcontainers` only for irreplaceable deps (e.g. real S3 via MinIO).
- Each `cargo test` run must complete in <2 minutes locally.

## 22. Observability

Every code path must be observable.

- **Logging via `tracing`.** Structured fields, never `format!()` into messages.
- **Span every RPC.** Inbound + outbound. Propagate trace IDs across services via gRPC metadata (`traceparent` header per W3C Trace Context).
- **Metrics exposed at `/metrics`** in Prometheus format, on every service.
- **Standard metrics every service emits:**
  - `brokkr_rpc_duration_seconds{service, method, status}` (histogram)
  - `brokkr_rpc_inflight{service, method}` (gauge)
  - `brokkr_errors_total{service, kind}` (counter)
  - Service-specific: `brokkr_cas_hits_total`, `brokkr_action_cache_hit_ratio`, `brokkr_jobs_running`, `brokkr_worker_capacity_utilization`, etc.
- **Health endpoints:** `/livez`, `/readyz`, `/metrics`, `/debug/pprof` (CPU/heap profiling via `pprof-rs`).
- **Trace export to OTLP** for production; stdout for development.

## 23. Performance Targets

These are aspirational targets, not Phase 0 requirements. Track regressions starting Phase 4.

| Metric | Target |
|---|---|
| Action cache lookup latency (p99) | < 5 ms |
| CAS read of cached 1 MiB blob (p99) | < 10 ms |
| CAS write 1 MiB (p99) | < 50 ms |
| Worker job dispatch latency | < 100 ms |
| Cold action execution overhead vs. native | < 200 ms |
| Sandbox setup time | < 50 ms |
| FUSE first-byte latency | < 30 ms |
| Cluster boot (3 nodes) | < 60 s |
| Memory per worker (idle) | < 100 MiB |
| Memory per CAS node (10k blobs cached) | < 1 GiB |

---

# Part V — Working With Claude Code

## 24. CLAUDE.md (Master System Prompt)

The following block is the contents of `CLAUDE.md` at the repo root. Claude Code reads this on every session and uses it as a system prompt for all coding work on Brokkr.

> Place this verbatim into a file named `CLAUDE.md` at the repo root. Update it as the project evolves.

```markdown
# CLAUDE.md — Brokkr Project Instructions

You are working on **Brokkr**, a distributed build and compute grid written in Rust.
Read `docs/plan.md` if you have not already — it is the single source of truth for
architecture, conventions, and roadmap. This file is the condensed operating manual.

## Identity & tone

- You are a senior systems engineer pair-programming with Berat.
- Be precise, concise, and skeptical. Push back when something seems wrong.
- Prefer "I am uncertain about X — let me check" over confident hallucinations.
- When the user provides a guess about implementation details, verify it against
  the actual code or docs before acting.

## Hard rules (NEVER violate)

1. **Never use `unwrap()` or `expect()` in library crates.** Tests and binaries
   may use them sparingly. Always propagate errors with `?` and typed error enums
   (`thiserror`).
2. **Never introduce `unsafe` without a `// SAFETY:` comment** explaining why
   the invariants hold.
3. **Never break the public API of a published crate without a major version bump.**
4. **Never commit generated files** unless explicitly required (build.rs output, etc.).
5. **Never disable a failing test to make CI green.** Fix it or mark `#[ignore]`
   with a TODO and a tracking issue link.
6. **Never add a dependency without justification.** New crates require a one-line
   rationale in the PR description. Prefer well-maintained crates with >1k downloads
   and recent commits.
7. **Never run `cargo update` as a side effect** of an unrelated change. Lockfile
   changes are their own commit.
8. **Never assume the user's environment.** Always check before suggesting commands
   that depend on installed tools (e.g. `bazel`, `docker`).
9. **Never use Docker, runc, containerd, or any external container runtime in
   `brokkr-sandbox`.** The sandbox is built on raw Linux primitives — that is the
   educational point.
10. **Never bring in an existing Raft crate (raft-rs, openraft, etc.).** Phase 5
    requires a from-scratch implementation. If you are tempted, stop and ask.

## Required workflow for any code change

1. **Restate the goal in one sentence** before writing code.
2. **Identify the affected crate(s) and modules.**
3. **Check `docs/plan.md` for the relevant phase and section.**
4. **Read existing code in the area** before adding new code; respect existing
   patterns or argue explicitly for changing them.
5. **Write the test first** when feasible. At minimum, write the test in the same
   commit as the implementation.
6. **Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and
   `cargo test --workspace` before claiming done.**
7. **Update rustdoc** for any changed public API.
8. **Add a tracing span** for any new RPC handler or async function on a hot path.
9. **Update `CHANGELOG.md`** under `## Unreleased`.

## Architectural invariants

- **Crate dependency graph is a DAG.** No cycles. `brokkr-common` is the only
  universal dependency.
- **Control plane and data plane are separate.** Don't put scheduling logic in
  the worker or storage logic in the control plane.
- **Wire protocol is gRPC + protobuf, REAPI-compatible.** Don't invent ad-hoc
  HTTP endpoints for things that should be RPC methods.
- **All IDs are newtypes** (`WorkerId(String)`, `JobId(String)`, `Digest { hash, size }`).
- **All blobs in public APIs use `bytes::Bytes`**, not `Vec<u8>`.
- **All async public functions return `Result<T, ErrorEnum>`** where `ErrorEnum`
  uses `thiserror::Error`.
- **Configuration is explicit structs, not env vars.** Env vars only override
  config in binaries (`brokkr-cli`, `brokkr-control`, `brokkr-worker`).

## Phase awareness

- The current phase is in `docs/plan.md` Section 11. Do not implement features
  from later phases unless the user explicitly opts in. If a task seems to require
  Phase N+1 capability, surface that and ask.
- Sandbox features (namespaces, cgroups, seccomp) are **Phase 2**. In Phase 1,
  `brokkr-worker` runs jobs as plain child processes — that is correct, not a bug.
- Distributed CAS sharding is **Phase 3**. Phase 0–2 use a single CAS node.
- Custom Raft is **Phase 5**. Earlier phases use embedded `redb`.

## Style — Rust specifics

- Format with `rustfmt`, default config + `tab_spaces = 4`, `max_width = 100`.
- Imports grouped: std, external crates, local crates, super, self.
- Prefer `tracing::info!(field = ?value, "message")` over interpolated messages.
- Prefer `let-else` over `if let` chains for early returns.
- Prefer `#[derive(Debug)]` everywhere; if it can't derive Debug, document why.
- Module structure: small files, deep modules. A file >500 lines is a smell.

## Style — commits & PRs

- Conventional commits: `feat(cas): add bloom filter for FindMissingBlobs`.
- Branch naming: `feat/<short>`, `fix/<short>`, `refactor/<short>`, `docs/<short>`.
- PR description template: motivation, what changed, how it was tested, related
  ADR or plan section.

## When the user asks for something risky or ambiguous

- If the user asks for a quick hack, do it but **leave a `// TODO(brokkr-XXXX): ...`
  comment** with a clear description of what would be done properly.
- If the user asks to skip a test, push back once. If they confirm, mark `#[ignore]`
  with reason; never delete.
- If the user proposes an architectural change that conflicts with `docs/plan.md`,
  flag the conflict and ask whether to update the plan.

## What to do when stuck

1. Re-read `docs/plan.md` for the relevant section.
2. Look at how a similar crate (RocksDB, etcd, TiKV, BuildBuddy) solved it.
3. Read the relevant paper from `docs/reading-list.md`.
4. Ask the user with a specific, narrowed question — never a vague "what should I do?".

## Output expectations

- When proposing changes, show **diffs** or **specific file paths + line ranges**,
  not full-file rewrites unless the file is short.
- For multi-step tasks, output a **plan first**, wait for approval, then execute.
- Keep explanations focused on *why*, not *what* (the code shows the what).
- When you finish a task, output:
  - Files changed.
  - Tests added or updated.
  - Any TODOs left.
  - Any plan-doc updates required.
```

## 25. How to Use Claude Code on Brokkr

Brokkr is too large for any single Claude Code session to "build the whole thing." Treat Claude Code as a **highly skilled junior** that needs a clear task per session.

### Session anatomy (recommended)

1. **Open the repo in Claude Code.** It will read `CLAUDE.md` automatically.
2. **State the task as a single sentence.** Bad: "Let's work on CAS." Good: "Implement `FindMissingBlobs` against the in-memory CAS in `brokkr-cas/src/in_memory.rs` per Phase 1 task 2."
3. **Reference the plan section.** Always link the task back to `docs/plan.md` Section X.Y.
4. **Ask for a plan first.** Never let it dive into code on a non-trivial task. Use the plan/code split.
5. **Review the plan.** Push back, refine, then approve.
6. **Let it implement.** Have it run tests, lints, and formatters.
7. **Review the diff.** Look for: `unwrap()`, missing tracing, missing tests, dependency creep, plan deviations.
8. **Commit using conventional commits.**

### Workflow split (mirrors your Windsurf workflow)

You already have a Plan/Code split workflow. Apply it here:

| Step | Model (Claude Code) | Purpose |
|---|---|---|
| **Plan** | Opus 4.7 (default), Plan mode | Decompose task, identify files, write skeleton |
| **Code** | Opus 4.7 (or Sonnet 4.6 for simpler tasks) | Implement against the approved plan |
| **Review** | Opus 4.7 in a fresh session | Independent review of the diff |

The Review pass in a fresh session matters: it catches drift you'd miss after a long coding session.

### MCPs to enable

- **Filesystem MCP** — for reading/writing files in the repo.
- **Git MCP** — for commit/branch operations.
- **Context7 MCP** — for fetching up-to-date docs (you already use this in Windsurf).
- **GitHub MCP** — for issues/PRs.
- **Sequential Thinking MCP** (optional) — for deeply layered design tasks.

## 26. Per-Task Prompt Templates

### 26.1 New module / feature

```
Context: Brokkr Phase <N>, task <N.M> from docs/plan.md.
Goal: <one sentence>.
Affected crate(s): <list>.
Constraints: respect CLAUDE.md hard rules; no Phase N+1 features.
Acceptance criteria:
  - Public API documented with rustdoc.
  - Unit tests covering happy path + at least one error path.
  - One integration test in tests/.
  - Tracing spans on public async fns.
  - cargo fmt + clippy clean, cargo test --workspace passes.
Deliver: a plan first. Wait for my approval before implementing.
```

### 26.2 Bug fix

```
Context: bug in <crate>::<module>.
Symptom: <observed behavior>.
Expected: <desired behavior>.
Reproduce with: <command or test>.
Goal: diagnose root cause, propose minimal fix, add a regression test.
Constraints: do NOT refactor unrelated code; do NOT add features.
Deliver: (1) hypothesis, (2) confirmation via failing test, (3) fix, (4) green test.
```

### 26.3 Refactor

```
Context: <module> needs refactor — current issue: <e.g., file >500 lines, mixed concerns>.
Goal: <e.g., split into <list of new modules> following SRP>.
Constraints: zero behavioral change; same public API; same test outcomes.
Acceptance: every existing test passes unchanged; new module structure documented.
Deliver: a plan listing the new module layout, then the refactor in small commits.
```

### 26.4 Architecture decision (ADR)

```
Context: need to decide between <options> for <subsystem>.
Goal: produce an ADR in docs/architecture/<NNNN>-<slug>.md following our ADR template.
Constraints: cite at least 2 prior-art systems; list pros/cons honestly; pick a default.
Deliver: the ADR text. I will commit it after review.
```

### 26.5 Dependency addition

```
Context: considering adding crate `<name>` to <our-crate>.
Goal: justify or reject the dependency.
Constraints: prefer crates with >1k downloads, active maintenance (last release <12 months),
permissive license (MIT/Apache-2.0/BSD), no GPL.
Deliver: short memo with: what it does, alternatives considered, license, maintenance status,
binary size impact (if measurable), recommendation.
```

## 27. Anti-Patterns

Things to **forbid** in Claude Code sessions, even if it offers them:

- "Let me just use Docker for the sandbox to start" — **No.** The sandbox is the educational core.
- "I'll import `raft-rs` to save time" — **No.** Phase 5 is from scratch.
- "Let me skip the test for now" — **No.** Tests are written in the same commit.
- "Let me add `unwrap()` here, we can clean it up later" — **No.** Library crates never unwrap.
- "I'll regenerate the protos, you can review later" — **No.** Generated code must be inspected.
- "Let me bump these 12 dependencies while I'm here" — **No.** Dependency updates are their own PR.
- "I'll use `eprintln!` for now, we'll add tracing later" — **No.** Tracing from day one.
- "Let me put this config in an env var" — **No.** Config is explicit structs.
- "I added a TODO; we can fix it in Phase X" — **Sometimes OK** if the TODO has a specific tracking issue and is documented in `docs/plan.md`.

If Claude Code suggests any of these, push back with a one-liner pointing at the relevant CLAUDE.md rule.

---

# Part VI — Reference

## 28. Reading List

**Read in this rough order. Don't skip the papers — they are short and dense.**

### Tier 1 — Read before Phase 1

- **Designing Data-Intensive Applications**, Martin Kleppmann. The single most important backend book of the last decade.
- **The Raft Paper.** "In Search of an Understandable Consensus Algorithm" — Ongaro & Ousterhout, 2014. <https://raft.github.io/raft.pdf>
- **REAPI specification.** <https://github.com/bazelbuild/remote-apis>
- **Tonic & Prost docs.** <https://docs.rs/tonic>, <https://docs.rs/prost>

### Tier 2 — Read before Phase 2

- **`man 7 namespaces`**, **`man 7 cgroups`**, **`man 2 unshare`**, **`man 2 seccomp`**.
- **Aleksa Sarai's container blog posts.** <https://www.cyphar.com/blog>
- **gVisor design doc.** <https://gvisor.dev/docs/architecture_guide/>
- **Bazel's sandbox source.** <https://github.com/bazelbuild/bazel/tree/master/src/main/tools>

### Tier 3 — Read before Phase 3

- **The Bigtable paper** — for tiered storage ideas.
- **The Dynamo paper** — for hashing and replication.
- **OpenDAL docs.** <https://opendal.apache.org/>
- **FUSE protocol overview.** <https://www.kernel.org/doc/html/latest/filesystems/fuse.html>

### Tier 4 — Read before Phase 5

- **Raft paper, again.** Then the **Raft thesis** (Ongaro PhD thesis, 250 pages).
- **Diego Ongaro's Raft lectures.** <https://www.youtube.com/watch?v=YbZ3zDzDnrw>
- **etcd source code**, especially `raft/`. <https://github.com/etcd-io/etcd>
- **TiKV source code**, especially `tikv/raftstore`. <https://github.com/tikv/tikv>
- **Jepsen blog posts.** <https://jepsen.io/analyses>

### Tier 5 — Inspirational, anytime

- **BuildBuddy blog.** <https://www.buildbuddy.io/blog/>
- **EngFlow blog.** <https://blog.engflow.com/>
- **Phil Eaton's blog** (databases, systems). <https://eatonphil.com/>
- **Hillel Wayne's "Computer Things"** (formal methods). <https://buttondown.email/hillelwayne>

## 29. Glossary

| Term | Definition |
|---|---|
| **Action** | A REAPI message describing a unit of work: command, env, input root digest, platform. |
| **ActionResult** | The output of executing an Action: exit code, output digests, stdout/stderr digests. |
| **Anvil** | (flavor term) the control plane. |
| **Bazel** | A monorepo build tool from Google; the canonical REAPI client. |
| **BuildBuddy / EngFlow** | Commercial REAPI implementations; Brokkr's prior art. |
| **CAS** | Content-Addressable Storage. Blobs identified by SHA-256. |
| **Digest** | `(sha256_hex, size_bytes)`. |
| **Forge** | (flavor) a Brokkr cluster. |
| **gRPC** | RPC framework on HTTP/2 + protobuf; REAPI's transport. |
| **Hoard** | (flavor) the cache. |
| **Ingot** | (flavor) a built artifact. |
| **Lease** | A time-bounded claim a worker has on a job. |
| **Merkle tree / DAG** | Hash-linked tree structure; a directory's hash recursively includes file hashes. |
| **OTLP** | OpenTelemetry Protocol — standard for shipping traces/metrics/logs. |
| **REAPI** | Remote Execution API v2; the gRPC interface Bazel and friends use. |
| **Smith** | (flavor) a worker node. |
| **Tenant** | An authenticated entity (user, team, org) with quotas and isolation. |
| **TTL** | Time to live; cache eviction time bound. |
| **Work** | (flavor) a job. |

## 30. Decision Log Template

Place ADRs (Architecture Decision Records) in `docs/architecture/`. Format:

```markdown
# NNNN — <Decision Title>

- **Status:** proposed | accepted | superseded by NNNN | deprecated
- **Date:** YYYY-MM-DD
- **Deciders:** Berat, <others>

## Context
<What is the situation that requires a decision? Why now?>

## Decision
<The decision in 1–3 sentences.>

## Alternatives considered
- **Option A:** <pros / cons>
- **Option B:** <pros / cons>
- **Option C:** <pros / cons>

## Consequences
- **Positive:** <…>
- **Negative:** <…>
- **Neutral:** <…>

## References
- <Links to papers, blog posts, prior art>
```

**ADRs to write before Phase 0 ends:**

- `0001-language-rust-everywhere.md`
- `0002-reapi-compatibility.md`
- `0003-embedded-kv-redb.md`
- `0004-tracing-from-day-one.md`
- `0005-no-docker-for-sandbox.md`
- `0006-license-apache-2-0.md`

## 31. Open Questions

Track unresolved design questions here. Resolve before they become blockers.

- [ ] **License:** Apache-2.0 (default for infra) vs. MIT (simpler)? **Tentative: Apache-2.0** for explicit patent grant.
- [ ] **MSRV policy:** lock to 1.78 or follow latest stable? **Tentative: lock, bump quarterly.**
- [ ] **Single binary vs. multi-binary?** Probably multi (`brokk`, `brokkr-control`, `brokkr-worker`, `brokkr-cas`) — clearer ops, separate metrics.
- [ ] **Action cache TTL default?** 30 days starting point.
- [ ] **GC strategy for CAS?** Reference counting + LRU. Need an ADR.
- [ ] **mTLS vs. token auth as default?** Probably mTLS for worker↔control, tokens for client↔control.
- [ ] **Schema evolution policy for protos?** Brokkr-internal protos versioned per-namespace; REAPI is upstream so we follow upstream.
- [ ] **Telemetry-by-default?** No. Self-hosted infra ships with telemetry off; opt-in only.
- [ ] **Crate publishing strategy?** Don't publish to crates.io until Phase 4 stable.

---

## Appendix A — First Day Checklist

When you sit down to start Phase 0:

1. [ ] Verify GitHub org/repo name `brokkr` is available (or pick fallback: `brokkrhq`, `brokkr-build`).
2. [ ] Verify domain `brokkr.io` / `brokkr.dev` / `brokkr.sh`.
3. [ ] Spin up a Linux dev box (local VM, WSL2, or remote VPS — Brokkr's worker code requires Linux).
4. [ ] Install Rust 1.78 via rustup; install `cargo-watch`, `cargo-deny`, `cargo-nextest`, `just`.
5. [ ] `git init` the repo, push to GitHub, set branch protection on `main`.
6. [ ] Drop in this `plan.md` at `docs/plan.md`.
7. [ ] Drop in `CLAUDE.md` from Section 24.
8. [ ] Write ADR 0001 (Rust everywhere).
9. [ ] Open Claude Code, point it at the repo, ask it to execute Phase 0 Task 1.
10. [ ] When CI is green and `brokk version` works: commit `docs/journal/phase-0.md` describing what you learned.

## Appendix B — Inspirational Targets

When in doubt about a design choice, ask: *"What would these projects do?"*

- **Rust quality:** `tikv`, `databend`, `risingwave`.
- **Sandbox/runtime:** `youki`, `crun`, `gVisor`.
- **REAPI servers:** BuildBuddy (Go), EngFlow (Go), `bazel-remote` (Go), `buildbarn` (Go).
- **Distributed coordination:** `etcd`, `nomad`, `consul`.
- **Storage engines:** RocksDB, FoundationDB, TiKV.

Steal liberally. Cite when you do.

---

*End of plan. Build something they'll be jealous of.*
