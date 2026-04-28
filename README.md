# Brokkr

**A distributed build & compute grid, written in Rust.**
*Many hammers. One forge.*

Brokkr is a self-hosted, open-source compute platform that turns a fleet of
Linux machines into a single, coherent grid for executing arbitrary jobs —
builds, tests, ML training, transcoding, anything that fits inside a sandbox.
It speaks the [Bazel Remote Execution API v2][reapi] so existing tooling
(`bazel`, `buck2`, `pants`, custom REAPI clients) works unchanged.

The interesting parts of distributed computing — content-addressable
storage, hermetic sandboxing, scheduling, and consensus — are implemented
from scratch as the project's educational core. There is no Docker, no
runc, no embedded etcd, no third-party Raft.

> **Status:** Phase 1 complete. `brokk run` executes commands end-to-end
> across a control plane + worker pair, with action-cache hits on the
> second invocation. Hermetic sandboxing (Phase 2) and distributed CAS
> (Phase 3) are next. **Not yet production-ready.**

## What works today

```sh
# Terminal 1: control plane (gRPC server, in-memory CAS + action cache).
cargo run -p brokkr-control -- --listen 127.0.0.1:7878 --data-dir /tmp/brokkr

# Terminal 2: a worker that registers and pulls jobs.
cargo run -p brokkr-worker  -- --control http://127.0.0.1:7878

# Terminal 3: submit a job.
cargo run -p brokkr-cli -- run -- /bin/echo "hello world"
# → hello world
# → [brokk] exit=0 cache_hit=false

cargo run -p brokkr-cli -- run -- /bin/echo "hello world"
# → hello world
# → [brokk] exit=0 cache_hit=true   ← served from the action cache
```

Behind the scenes that one command:
1. hashes a REAPI `Action` + `Command` and uploads them to the CAS,
2. calls `Execute`, which streams a `google.longrunning.Operation`,
3. dispatches a `brokkr.v1.Job` to the worker over a bidi gRPC stream,
4. spawns the process on the worker, captures stdout/stderr,
5. uploads the outputs back to the CAS,
6. records the result in the action cache (only on `exit_code == 0`),
7. returns an `ExecuteResponse` to the client.

## Architecture

Brokkr is a workspace of nine crates with a strict DAG dependency graph.

```
                                         brokkr-cli (binary: brokk)
                                                │
                                                ▼
                                          brokkr-sdk
                                                │
                       ┌────────────────────────┴──────────────────────────┐
                       │                                                   │
                       ▼                                                   ▼
                brokkr-proto  ◀───  brokkr-common  ───▶  brokkr-control (binary: brokkr-control)
                                                                          │
                                                                          ├──▶ brokkr-cas
                                                                          │
                                                                          └──▶ brokkr-worker (binary: brokkr-worker)
                                                                                       │
                                                                                       └──▶ brokkr-sandbox  (Phase 2)
```

| Crate              | Responsibility                                                                   |
| ------------------ | -------------------------------------------------------------------------------- |
| `brokkr-common`    | Shared `Digest` newtype, error helpers, IDs. Universal dep, kept tiny.           |
| `brokkr-proto`     | Vendored REAPI v2 protos + internal `brokkr.v1` worker dispatch protocol.        |
| `brokkr-cas`       | `Cas` trait, in-memory + `redb`-backed CAS, action cache.                        |
| `brokkr-control`   | Tonic gRPC server: REAPI services + scheduler + worker stream.                   |
| `brokkr-worker`    | Worker daemon: registers, pulls jobs, runs them, uploads outputs.                |
| `brokkr-sandbox`   | (Phase 2) Linux namespaces + cgroups + seccomp from scratch — no runc, no Docker. |
| `brokkr-sdk`       | Ergonomic Rust client for the REAPI surface.                                     |
| `brokkr-cli`       | The `brokk` command-line interface.                                              |
| `brokkr-test-utils`| Internal test helpers (not published).                                           |

## Engineering invariants

Brokkr aims for correctness > performance > ergonomics, in that order.

- **No `unwrap` / `expect` / `panic!` in library crates.** Errors are
  propagated with `?` against `thiserror` enums. Workspace-level clippy
  lints enforce this (see [`clippy.toml`](clippy.toml)).
- **No `unsafe` without a `// SAFETY:` comment** justifying invariants.
- **No external container runtimes.** The sandbox is built directly on
  the kernel primitives; rolling our own is the educational point.
- **No off-the-shelf Raft.** Phase 5 implements consensus from scratch.
- **Public APIs use `bytes::Bytes`, not `Vec<u8>`.** All IDs are newtypes.
- **CI gate**: `cargo fmt --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace` on Linux
  x86_64 + aarch64.

## Roadmap

The full plan lives in [`docs/plan.md`](docs/plan.md). At a glance:

| Phase | Theme                       | Status      |
| ----- | --------------------------- | ----------- |
| 0     | Bootstrap                   | done        |
| 1     | First end-to-end slice      | done        |
| 2     | Hermetic Linux sandboxing   | next        |
| 3     | Distributed CAS (sharded)   | planned     |
| 4     | Scheduler + multi-tenancy   | planned     |
| 5     | Consensus + HA (custom Raft)| planned     |
| 6+    | Web UI, FUSE inputs, RBE+   | planned     |

Phase retrospectives are committed to [`docs/journal/`](docs/journal/) at
the close of each phase.

## Quick start (developer)

```sh
# One-time setup (Rust toolchain is pinned via rust-toolchain.toml).
rustup show

# Build everything.
cargo build --workspace

# Run the full test suite (25 tests including end-to-end gRPC).
cargo test --workspace

# Lint (CI runs the same).
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

There is also a [`justfile`](justfile) with `fmt`, `lint`, `test`, `ci`,
`brokk`, and `phase` recipes if you have [`just`](https://github.com/casey/just) installed.

## Documentation

- [`docs/plan.md`](docs/plan.md) — vision, architecture, roadmap,
  engineering practice. Single source of truth.
- [`docs/architecture/`](docs/architecture/) — Architecture Decision Records.
- [`docs/journal/`](docs/journal/) — phase retrospectives.
- [`CHANGELOG.md`](CHANGELOG.md) — every notable change since bootstrap.
- [`CLAUDE.md`](CLAUDE.md) — operating manual when pair-programming with
  AI assistants on this repo.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — how to propose changes.

## Why "Brokkr"?

In Norse mythology, **Brokkr** is the dwarven smith who, with his brother
Eitri, forges the gods' most prized artifacts in a single furnace —
including Thor's hammer Mjölnir. The grid here is the forge; every
worker is a hammer.

## License

Apache-2.0. See [`LICENSE`](LICENSE).

[reapi]: https://github.com/bazelbuild/remote-apis
