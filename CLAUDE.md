# disc-tools

Rust tooling for chat/Discord data (working title). **Scaffold — no source code yet.** This file and `kb/` were bootstrapped by `/document-repository`; sections marked _TODO_ get filled as code lands.

## Important: Documentation Requirements

All code changes MUST be reflected in documentation. When making changes:

1. Update relevant snapshots in `kb/architecture/` or `kb/guides/`
2. Keep `CLAUDE.md` in sync with project structure changes
3. Run `/document-repository` to refresh docs after significant changes (creates a new dated snapshot under `kb/<folder>/<YYYY.MM>/`)

If unsure how to document a change, ask the user before proceeding.

## Quick Reference

Intended stack: **Rust** (inferred from `.gitignore`: `/.cargo/`, `/target/`). No `Cargo.toml` exists yet.

| Command | Description |
|---------|-------------|
| `cargo build` | Build (once `Cargo.toml` exists) — _TODO_ |
| `cargo test` | Run tests — _TODO_ |
| `cargo run` | Run the app — _TODO_ |

## Project Structure

Current tracked layout (empty scaffold):

```
disc-tools/
├── .gitignore          # ignores .idea, .cargo, target, .env, chat/*.txt, logs/*.log
├── CLAUDE.md           # this file
└── kb/                 # agentic knowledge base (dated snapshots)
    ├── architecture/
    └── guides/
```

Anticipated (from `.gitignore` hints, not yet present):

```
disc-tools/
├── Cargo.toml          # _TODO_ — crate manifest
├── src/                # _TODO_ — Rust sources
├── chat/               # runtime chat exports (*.txt, gitignored)
└── logs/               # runtime logs (*.log, gitignored)
```

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
