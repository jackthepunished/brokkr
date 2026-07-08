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
    cargo clippy --workspace --all-targets --locked -- -D warnings

# Run the full test suite (integration/unit + doctests).
# brokkr-proto is excluded from doctests: generated protobuf comments are not
# valid Rust, and `--doc` ignores its `doctest = false`.
test:
    cargo test --workspace --all-targets --locked
    cargo test --workspace --doc --exclude brokkr-proto --locked

# Build everything in release mode.
build:
    cargo build --workspace --release --locked

# Audit dependencies (advisories, licenses, bans).
deny:
    cargo deny check

# What CI runs locally. Run before pushing.
ci: fmt-check lint test deny

# Run the brokk CLI.
brokk *ARGS:
    cargo run -p brokkr-cli -- {{ARGS}}

# Print the current Brokkr phase from the plan.
phase:
    @grep -E '^\*\*Status:\*\*' docs/plan.md | head -n1
