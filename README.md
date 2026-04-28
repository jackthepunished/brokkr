# Brokkr

**Distributed Build & Compute Grid.**
*Many hammers. One forge.*

Brokkr is a self-hosted, open-source distributed compute platform that turns a
fleet of Linux machines into a single, coherent grid for executing arbitrary
jobs — builds, tests, ML training, transcoding. It speaks the
[Bazel Remote Execution API v2][reapi] so existing tooling works out of the box.

> Status: **Pre-Phase 0.** Not yet usable. Read the plan before contributing.

## Documentation

- [`docs/plan.md`](docs/plan.md) — single source of truth: vision, architecture,
  roadmap, engineering practice.
- [`CLAUDE.md`](CLAUDE.md) — operating manual for AI pair-programming on this repo.
- `docs/architecture/` — Architecture Decision Records.
- `docs/journal/` — phase retrospectives.

## Quick start (developer)

Requires Rust 1.85 (pinned via `rust-toolchain.toml`).

```sh
cargo build --workspace
cargo test  --workspace
```

## License

Apache-2.0. See [`LICENSE`](LICENSE).

[reapi]: https://github.com/bazelbuild/remote-apis
