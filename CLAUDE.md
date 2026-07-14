# disc-tools

Cross-platform disk-utilities CLI in Rust, distributed as `disk-tools`. Its first
capability is finding what eats disk space — a fast parallel scan printing a
dust-style size-sorted tree of the largest directories and files — with cleanup,
junk/old/duplicate detectors and a TUI planned on top of the same core.

Currently building **v0.1** (scan + tree report). See the
[concept](kb/concepts/2026.07/2026.07.14-disk-tools.md) for the full vision and
its Roadmap for what lands when; the
[v0.1 spec](kb/specs/2026.07/2026.07.14-disk-tools-v0.1-scan-report.md) is the
authoritative task breakdown.

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
| `just run <ARGS>` | Run the CLI, e.g. `just run ~/Downloads --json` (no-op until Task 6 lands the clap surface) |

## Project Structure

```
disc-tools/
├── Cargo.toml          # workspace: members core, cli; edition 2024, MSRV 1.85
├── justfile            # single entry point for local tooling
├── core/               # disk-tools-core (lib) — the scanning engine
│   ├── Cargo.toml      # no dependencies yet (rayon/filesize arrive in Tasks 2-3)
│   └── src/
│       ├── lib.rs      # forbid(unsafe_code); re-exports
│       ├── options.rs  # ScanOptions
│       └── tree.rs     # ScanNode / ScanTree / SkippedEntry / SkipReason
├── cli/                # disk-tools (bin) — CLI frontend
│   ├── Cargo.toml
│   └── src/main.rs     # placeholder until Task 6
├── CLAUDE.md           # this file
└── kb/                 # agentic knowledge base (dated snapshots)
```

Planned modules, not yet present (spec §2): `core/src/{walk,size,dedup,units}.rs`,
`cli/src/{args.rs,render/}`.

## Key Concepts

_TODO_ — no abstractions defined yet. Populate with core types/traits (`file:line`) once `src/` exists.

## Configuration

- **`.env`** — gitignored; environment config (contents _TODO_)
- **`chat/`**, **`logs/`** — runtime output directories, gitignored
- `.direnv/` present in ignore list → `direnv` likely used for env loading

## Knowledge Base

All agentic documentation lives under `kb/` with a fixed chronological layout:

```
kb/<folder>/<YYYY.MM>/<YYYY.MM.DD>-<slug>.md
```

| Folder | Purpose | Latest snapshot |
|--------|---------|-----------------|
| `kb/architecture/` | System design, key patterns | `2026.07/2026.07.14` |
| `kb/guides/` | Developer-facing how-tos | `2026.07/2026.07.14` |

Files are always written under a `<YYYY.MM>/` folder — never directly under `kb/<folder>/`. Filenames begin with `<YYYY.MM.DD>-` and never include the folder name.

## Documentation

**Architecture** (snapshots from `kb/architecture/2026.07/`)
- [Overview](kb/architecture/2026.07/2026.07.14-overview.md)

**Guides** (snapshots from `kb/guides/2026.07/`)
- [Development](kb/guides/2026.07/2026.07.14-development.md)
