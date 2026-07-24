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

# CI runs this recipe on all three platforms, so anything added here is
# enforced there too.

# Pre-commit gate: formatting, lints, tests.
verify: fmt-check lint test

# =============================================================================
# Benchmarks
# =============================================================================
#
# Requires `hyperfine` and `diskus` (`cargo install hyperfine diskus`).
# Recorded results live in kb/benchmarks/.

# Generate the benchmark fixtures under DIR (~28 GB, a few minutes).
bench-fixtures DIR:
    scripts/bench-fixtures.sh "{{DIR}}"

# This is the exact invocation behind the numbers in kb/benchmarks/ — keep the
# two in step, or the recorded results stop being reproducible.
#   -N            no intermediate shell; its ~2-3 ms spawn cost swamps the
#                 `media` fixture and inflates du's variance
#   --output=null discards stdout, so rendering is compared on equal terms
#   --warmup 5    the spec's warm-cache condition
#   --depth 0     the like-for-like comparison with `du -sh`: one summary line
#                 each, where the default invocation also formats every entry

# Benchmark the release binary against `du -sh` and `diskus` over DIR's fixtures.
bench DIR:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release
    bin="$PWD/target/release/disk-tools"
    for fixture in node_modules cache media; do
        path="{{DIR}}/$fixture"
        echo
        echo "### $fixture"
        # Single quotes survive hyperfine's own argument splitting under -N, so
        # a fixture path containing spaces still reaches the tool as one arg.
        hyperfine -N --warmup 5 --runs 20 --output=null \
            -n "disk-tools --depth 0"   "'$bin' '$path' --depth 0" \
            -n "disk-tools (full tree)" "'$bin' '$path'" \
            -n "du -sh"                 "du -sh '$path'" \
            -n "diskus"                 "diskus '$path'"
    done

# `time -l` is BSD/macOS; GNU time spells the same thing `-v`. Extra ARGS are
# passed to disk-tools, e.g. `just bench-memory ~/Projects --depth 0`.

# Peak RSS of one scan of DIR (AC3 of the v0.1 spec's Task 11).
bench-memory DIR *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release
    if /usr/bin/time -l true 2>/dev/null; then flag=-l; else flag=-v; fi
    /usr/bin/time "$flag" ./target/release/disk-tools "{{DIR}}" {{ARGS}} > /dev/null
