# Contributing to Brokkr

Brokkr is in active early development. The bar for contributions is high
because the project is also a learning vehicle — see
[`docs/plan.md`](docs/plan.md) §4 for the success criteria.

## Before you start

1. Read [`docs/plan.md`](docs/plan.md) end to end.
2. Read [`CLAUDE.md`](CLAUDE.md) — the engineering operating manual.
3. Find the relevant phase in §11 of the plan. Don't implement features from
   future phases without an explicit decision.

## Workflow

- Branch naming: `feat/<short>`, `fix/<short>`, `refactor/<short>`, `docs/<short>`.
- Conventional commits: `feat(cas): add bloom filter for FindMissingBlobs`.
- One logical change per commit. Squash WIP before merging.
- Every PR runs `cargo fmt --check`, `cargo clippy -- -D warnings`,
  `cargo test --workspace`. All must be green.
- New dependencies need a one-line justification in the PR description.

## Hard rules

See `CLAUDE.md` §"Hard rules". The non-negotiable ones:

- No `unwrap()` / `expect()` / `panic!()` in library crates.
- No `unsafe` without a `// SAFETY:` comment.
- No external container runtime in `brokkr-sandbox`.
- No external Raft crate in Phase 5.
- No `cargo update` as a side effect of an unrelated change.
