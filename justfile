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

# Builds for the host target only. Cross-OS builds belong to a release workflow
# (see the concept's "Distribution — deferred"), not to a local recipe: they need
# a cross-linker per target, and CI already proves the code compiles and passes
# on Linux, macOS and Windows.

# Build the optimized binary for this machine.
release:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release
    echo
    echo "binary:  $PWD/target/release/disk-tools"
    echo "install: cargo install --path cli"

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

# rustdoc catches what clippy cannot: a public item documenting a link to a
# private one resolves to nothing in the built docs. Two of those reached code
# review before this recipe existed, which is why it does.
#
# `--all-features` because the serde-gated code carries doc links of its own.

# Build the docs; warnings are errors.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# `core/Cargo.toml` promises a scan-only consumer can drop `clean` and stop
# compiling `objc2`/`windows` for a capability it never calls. Nothing enforced
# that until this recipe: the default build never exercises the cfg-gated paths,
# so the promise could rot unnoticed.

# Type-check the core without its default features.
check-minimal:
    cargo check -p disk-tools-core --no-default-features --all-targets

# Nightly-only (`-Z coverage-options=branch`), so it is not part of `verify` and
# does not gate CI. Worth having anyway: line coverage counts a line with two
# branches as covered once either runs, which hides precisely the half-tested
# conditions the safety rules are made of.

# Branch coverage report; needs a nightly toolchain and cargo-llvm-cov.
coverage-branch:
    cargo +nightly llvm-cov --workspace --all-features --branch --summary-only

# `--all-targets` so tests and benches are checked too — an API stabilized after
# the MSRV is just as breaking there. CI runs this under the pinned 1.85.

# Type-check everything, including tests.
check:
    cargo check --workspace --all-targets

# CI runs this recipe on all three platforms, so anything added here is
# enforced there too.

# Pre-commit gate: formatting, lints, docs, feature minimality, tests.
verify: fmt-check lint doc check-minimal test

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

# Prefix with RAYON_NUM_THREADS=1 to see which phases actually scale — only the
# walk does, and only across directories.

# Print where a scan of DIR spends its time, split by phase.
bench-phases DIR:
    DT_PHASE_PATH="{{DIR}}" cargo test --release -p disk-tools-core --lib \
        -- --ignored --nocapture phase_split

# Moves real files into your Trash — that is the point, and why the tests behind
# it are `#[ignore]` rather than part of `just test`.

# Smoke-test the OS trash backend and time it on 10,000 files.
smoke-trash:
    cargo test -p disk-tools-core --lib -- --ignored --nocapture trash

# Answers whether parallelising the per-directory loop is worth doing, or whether
# the kernel serialises the metadata path anyway.

# Does stat-ing one directory's entries scale?
bench-stat DIR:
    DT_PHASE_PATH="{{DIR}}" cargo test --release -p disk-tools-core --lib \
        -- --ignored --nocapture dir_stat_scaling

# Peak RSS of one scan of DIR (AC3 of the v0.1 spec's Task 11).
bench-memory DIR *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release
    if /usr/bin/time -l true 2>/dev/null; then flag=-l; else flag=-v; fi
    /usr/bin/time "$flag" ./target/release/disk-tools "{{DIR}}" {{ARGS}} > /dev/null
