# Brokkr dev shortcuts. Run `just` to list recipes.

set shell := ["bash", "-cu"]

default:
    @just --list

# Format the workspace.
fmt:
    cargo fmt --all

# Check formatting (CI mode).
fmt-check:
    cargo fmt --all --check

# Lint with clippy. Warnings are errors.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Run the full test suite.
test:
    cargo test --workspace --all-targets

# Build everything in release mode.
build:
    cargo build --workspace --release

# Audit dependencies (advisories, licenses, bans).
deny:
    cargo deny check

# What CI runs locally. Run before pushing.
ci: fmt-check lint test

# Run the brokk CLI.
brokk *ARGS:
    cargo run -p brokkr-cli -- {{ARGS}}

# Print the current Brokkr phase from the plan.
phase:
    @grep -E '^\*\*Status:\*\*' docs/plan.md | head -n1
