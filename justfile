# disk-tools development commands. Run `just --list`.
# This justfile is the single entry point for local tooling — add new local
# operations here rather than as ad-hoc commands.
set shell := ["bash", "-cu"]

[private]
default:
    @just --list

# =============================================================================
# Build & run
# =============================================================================

# Build the whole workspace.
build:
    cargo build --workspace

# Run the CLI, e.g. `just run ~/Downloads --json`.
run *ARGS:
    cargo run -p disk-tools -- {{ARGS}}

# =============================================================================
# Quality
# =============================================================================

# Run all workspace tests.
test:
    cargo test --workspace

# Format code.
fmt:
    cargo fmt --all

# Check formatting without writing changes.
fmt-check:
    cargo fmt --all -- --check

# Lint with clippy; warnings are errors.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Pre-commit gate: formatting, lints, tests. CI runs this recipe on all three
# platforms, so anything added here is enforced there too.
verify: fmt-check lint test
