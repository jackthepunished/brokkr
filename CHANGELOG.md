# Changelog

All notable changes to Brokkr will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `brokkr-common`: foundational types — `Digest`, `WorkerId`, `JobId`,
  `TenantId`, typed `Error`/`CasError` enums, `sha256()` helper.
  Phase 1 building block; all other Phase 1 crates depend on these.
- Phase 0 bootstrap: Cargo workspace, 9 crates, toolchain pin to Rust 1.85,
  rustfmt/clippy/deny configuration, root README, CONTRIBUTING, LICENSE
  (Apache-2.0), CHANGELOG, CODE_OF_CONDUCT, justfile.
- `CLAUDE.md` operating manual at the repo root.
- `docs/plan.md` — project source of truth.
- Vendored REAPI v2 + supporting googleapis protos in `brokkr-proto`,
  compiled via `tonic-build` with a vendored `protoc` (no system dependency).
- `brokk version` and `brokk init` (stub) subcommands, with git SHA + rustc
  + target triple embedded at build time.
- GitHub Actions CI (`fmt`, `clippy -D warnings`, `test`, `build --release`)
  on Linux x86_64 and aarch64.
- ADR 0001 — Rust everywhere.

### Changed
- MSRV bumped from 1.78 → 1.85 during bootstrap (transitive deps require
  edition 2024).
