# Phase 0 — Bootstrap

- **Status:** complete
- **Date:** 2026-04-28
- **Exit criteria (from `docs/plan.md` §11):** workspace builds clean on Linux
  x86_64 + aarch64, `brokk version` prints a populated build stamp, CI is green,
  ADR 0001 merged, REAPI protos compile.

## What landed

- Cargo workspace with 9 crates (`brokkr-common`, `-proto`, `-cas`, `-control`,
  `-worker`, `-sandbox`, `-sdk`, `-cli`, `-test-utils`).
- Toolchain pinned via `rust-toolchain.toml` to **1.85.0** (rustfmt + clippy).
- Workspace-level lints: `unsafe_code = deny`, `clippy::unwrap_used = deny`,
  `expect_used = deny`, `panic = deny`. `clippy.toml` disallows `Result::unwrap`
  and `Option::unwrap` by name.
- `rustfmt.toml`, `deny.toml`, `justfile` for repeatable local workflows.
- Vendored REAPI v2 + supporting googleapis protos in `crates/brokkr-proto`,
  compiled via `tonic-build` with a vendored `protoc` (no system dependency).
  Module hierarchy mirrors proto package paths so generated `super::super::...`
  references resolve.
- `brokk version` and `brokk init` (stub) subcommands. Build stamp (git SHA,
  rustc version, target triple) injected via `brokkr-cli/build.rs`.
- GitHub Actions CI: `fmt`, `clippy -D warnings`, `test --all-targets`,
  `build --release`, on Linux x86_64 and aarch64.
- Apache-2.0 license, root `README.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`,
  `CHANGELOG.md`, `CLAUDE.md` operating manual.
- ADR 0001 — Rust everywhere — accepted.

## What surprised me

- **MSRV had to jump 1.78 → 1.85** during bootstrap: `getrandom 0.4` (transitive
  via `tonic`) requires edition 2024, which only stabilized in 1.85. Cheaper to
  bump than to pin every transitive dep.
- **Generated proto module paths.** A flat `pub mod reapi { pub mod v2 { include!
  } }` failed because the generated code uses `super::super::...::google::
  longrunning` paths. The fix was to mirror protobuf package hierarchy verbatim
  (`build::bazel::remote::execution::v2`) and re-export friendlier aliases.
- **Generated code contains markdown code fences in doc comments**, which rustdoc
  tries to compile as doctests. `[lib] doctest = false` on `brokkr-proto` is the
  right knob — the proto crate is pure codegen, doctests there are noise.
- **`protoc-bin-vendored`** removes the only system dependency that would have
  surfaced in CI and on contributor machines. Worth the build-script complexity.
- A few `rustfmt.toml` knobs I wanted (`imports_granularity`, `group_imports`)
  are still nightly-only. Removed; revisit when stabilized.

## What I deferred

- `cargo deny check` is wired in `justfile` but not yet a CI gate. Phase 1 task.
- No coverage tooling. Add `cargo-llvm-cov` when there is real code to cover.
- No release automation (`cargo-release`, tag-driven publishing). Not needed
  until first internal release.
- `brokk init` is a stub that prints a TODO. Real workspace scaffolding is
  Phase 1.
- No Windows or macOS CI. Workers are Linux-only by design; the CLI/control
  plane will gain those targets when there is something for them to do.

## Conventions established (to keep)

- All IDs are newtypes (`WorkerId`, `JobId`, `Digest { hash, size }`).
- Public async fns return `Result<T, ThisErrorEnum>`.
- Blobs in public APIs use `bytes::Bytes`, never `Vec<u8>`.
- Crate dependency graph is a DAG; `brokkr-common` is the only universal dep.
- Conventional commits, `feat/<short>` / `fix/<short>` branches.

## Ready for Phase 1

Phase 1 (per `docs/plan.md` §11) is the local CAS + single-node control plane +
plain-process worker, end-to-end over gRPC. Workspace, toolchain, protos, CI,
and the operating manual are all in place. Nothing in Phase 0 needs to be
revisited before starting.
