# disc-tools

Cross-platform disk-utilities CLI in Rust, distributed as `disk-tools`. Its first
capability is finding what eats disk space — a fast parallel scan printing a
dust-style size-sorted tree of the largest directories and files — with cleanup,
junk/old/duplicate detectors and a TUI planned on top of the same core.

Currently building **v0.1** (scan + tree report). See the
[concept](kb/concepts/2026.07/2026.07.14-disk-tools.md) for the full vision and
its Roadmap for what lands when; the
[v0.1 spec](kb/specs/2026.07/2026.07.14-disk-tools-v0.1-scan-report.md) is the
authoritative task breakdown. User-facing usage, flags and the documented v0.1
limitations live in the [README](README.md).

## Important: Documentation Requirements

All code changes MUST be reflected in documentation. When making changes:

1. Update relevant snapshots in `kb/architecture/` or `kb/guides/`
2. Keep `CLAUDE.md` in sync with project structure changes
3. Run `/document-repository` to refresh docs after significant changes (creates a new dated snapshot under `kb/<folder>/<YYYY.MM>/`)

If unsure how to document a change, ask the user before proceeding.

## Quick Reference

Stack: **Rust** — a Cargo workspace with `disk-tools-core` (lib) + `disk-tools` (bin).

`justfile` is the single entry point for local tooling; add new operations there
rather than as ad-hoc commands.

| Command | Description |
|---------|-------------|
| `just` | List all recipes |
| `just build` | Build the workspace (`cargo build --workspace`) |
| `just test` | Run all tests (`cargo test --workspace`) |
| `just lint` | Clippy, warnings as errors |
| `just fmt` / `just fmt-check` | Format / check formatting |
| `just verify` | Pre-commit gate: `fmt-check` + `lint` + `test` |
| `just check` | `cargo check --workspace --all-targets` — CI runs it pinned to MSRV 1.85 |
| `just run <ARGS>` | Run the CLI, e.g. `just run ~/Downloads --json` |
| `just release` | Optimized host build → `target/release/disk-tools` |
| `just bench-fixtures <dir>` / `just bench <dir>` / `just bench-memory <path>` | Benchmark harness — needs `hyperfine` + `diskus`; results recorded in `kb/benchmarks/` |

CI (`.github/workflows/ci.yml`) runs `just verify` + `just build` on Linux, macOS
and Windows, plus a Linux job pinned to MSRV 1.85 running `just check`. It calls the
justfile recipes rather than duplicating cargo commands, so a new local check added
there is automatically enforced in CI.

## Project Structure

```
disc-tools/
├── Cargo.toml          # workspace: members core, cli; edition 2024, MSRV 1.85
├── justfile            # single entry point for local tooling
├── .github/workflows/
│   └── ci.yml          # verify matrix on ×3 OS + an MSRV-pinned check job
├── core/               # disk-tools-core (lib) — the scanning engine
│   ├── Cargo.toml      # rayon; serde (optional); windows-sys on Windows
│   └── src/
│       ├── lib.rs      # deny(unsafe_code); pub fn scan(); re-exports
│       ├── options.rs  # ScanOptions
│       ├── walk.rs     # read_dir + rayon par_iter recursion, skip collection
│       ├── size.rs     # allocated (blocks*512 | GetCompressedFileSizeW) + apparent
│       ├── dedup.rs    # hardlink identity → lexicographically-first attribution
│       └── tree.rs     # ScanNode / ScanTree / SkippedEntry / SkipReason + aggregation
├── cli/                # disk-tools (bin) — CLI frontend
│   ├── Cargo.toml      # clap, terminal_size, serde_json, indicatif, unicode-width
│   ├── src/
│   │   ├── main.rs     # args → scan → render; spinner + skips to stderr
│   │   ├── args.rs     # clap derive; parse_size; validate_root
│   │   └── render/
│   │       ├── mod.rs
│   │       ├── tree.rs     # dust-style tree, parent-relative bars
│   │       ├── json.rs     # --json (full tree, raw byte counts)
│   │       └── skipped.rs  # skipped-entries summary (capped at 10)
│   └── tests/cli.rs    # integration tests
├── scripts/
│   └── bench-fixtures.sh   # generates the three benchmark fixture shapes
├── README.md           # user-facing usage, flags, limitations, benchmarks
├── CLAUDE.md           # this file
└── kb/                 # agentic knowledge base (dated snapshots)
```

`core/src/units.rs` from the spec's §2 sketch was never split out — size
formatting lives in `cli/src/render/tree.rs`, since only the renderer needs it
(the core returns raw byte counts).

## Key Concepts

| Item | Where | Role |
|------|-------|------|
| `scan(&ScanOptions) -> ScanTree` | `core/src/lib.rs:35` | The one public entry point; runs walk → dedup → aggregate in that order |
| `ScanOptions` | `core/src/options.rs` | `root`, `min_size`, `depth`, `apparent`, `one_file_system` — the core's whole input |
| `ScanNode` / `ScanTree` | `core/src/tree.rs:7,48` | The result: a node carries `path`, `allocated`, `apparent`, `is_dir`, `children`; the tree adds `skipped` |
| `SkippedEntry` / `SkipReason` | `core/src/tree.rs:40,27` | Failures returned **as data** — the core never prints |
| `RenderOptions` | `cli/src/render/tree.rs:16` | Display-only knobs (`number`, `depth`, `min_size`, `apparent`, `width`) |

Invariants worth keeping in mind:

- **Phase order matters.** Hardlink attribution must settle before any directory
  total is summed, or totals drift run to run under the parallel walk.
- **Display filters never touch totals.** `--depth`, `--min-size` and `-n` prune
  output only; directory sizes stay full-subtree (du semantics).
- **stdout is for the report, stderr for everything else** — that is what keeps
  `--json` pipe-clean.

## Configuration

The tool reads **no** configuration — `<PATH>` and the flags are its entire input
(`--config` and a TOML file arrive in v0.3). What configuration exists is build-time:

| File | Holds |
|------|-------|
| `Cargo.toml` (workspace) | `version`, `edition = "2024"`, `rust-version = "1.85"`, inherited by both crates |
| `core/Cargo.toml` | The optional `serde` feature; `windows-sys` under `[target.'cfg(windows)'.dependencies]` |
| `cli/Cargo.toml` | Enables the core's `serde` feature for `--json` |
| `.gitattributes` | `* text=auto eol=lf` — a CRLF checkout would fail `cargo fmt --check` on Windows |

`.gitignore` also lists `.env`, `.direnv/`, `chat/` and `logs/`; none of these
exist in the repository — they are leftovers from the initial scaffold, not
features.

## Knowledge Base

All agentic documentation lives under `kb/` with a fixed chronological layout:

```
kb/<folder>/<YYYY.MM>/<YYYY.MM.DD>-<slug>.md
```

| Folder | Purpose | Latest snapshot |
|--------|---------|-----------------|
| `kb/architecture/` | System design, key patterns | `2026.07/2026.07.25` |
| `kb/guides/` | Developer-facing how-tos | `2026.07/2026.07.25` |
| `kb/benchmarks/` | Recorded performance/memory measurements | `2026.07/2026.07.25` |
| `kb/concepts/` | Concept documents (`/write-concept`) | `2026.07` |
| `kb/specs/` | Feature specs (`/write-spec`) | `2026.07` |
| `kb/brainstorms/` | Brainstorm sessions (`/brainstorm`) | `2026.07` |
| `kb/research/` | Research reports (`/research`) | `2026.07` |
| `kb/plans/` | Execution plans (`/brainstorm`) | `2026.07` |
| `kb/handoffs/` | Task handoffs (`/implement-task`) | `2026.07` |

Files are always written under a `<YYYY.MM>/` folder — never directly under `kb/<folder>/`. Filenames begin with `<YYYY.MM.DD>-` and never include the folder name.

## Documentation

**Architecture** (snapshots from `kb/architecture/2026.07/`)
- [Overview](kb/architecture/2026.07/2026.07.25-overview.md) — the three-phase pipeline, data model, invariants, platform splits
- [Rust crate structure](kb/architecture/2026.07/2026.07.25-rust-crates.md) — workspace, feature flags, unsafe policy

**Guides** (snapshots from `kb/guides/2026.07/`)
- [Development](kb/guides/2026.07/2026.07.25-development.md) — workflow, justfile recipes, CI, benchmark harness
- [Testing](kb/guides/2026.07/2026.07.25-testing.md) — test layout, platform gating, fixture patterns

**Benchmarks** (snapshots from `kb/benchmarks/2026.07/`)
- [v0.1 scan performance and memory](kb/benchmarks/2026.07/2026.07.25-v0.1-scan-performance.md)
