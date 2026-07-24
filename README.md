# disk-tools

Find what's eating your disk. `disk-tools` walks a directory tree in parallel,
measures **real on-disk (allocated) size**, and prints a size-sorted tree of the
biggest consumers — or JSON.

**v0.1 — scan + tree report.** This is the anchoring capability of a wider
disk-utilities suite; cleanup, junk/old/duplicate detectors, config and a TUI are
planned on top of the same core. See the
[concept](kb/concepts/2026.07/2026.07.14-disk-tools.md) for the full vision and the
[v0.1 spec](kb/specs/2026.07/2026.07.14-disk-tools-v0.1-scan-report.md) for what
shipped.

Cross-platform: macOS, Linux, Windows.

## Install

From a checkout:

```bash
git clone https://github.com/alexkreskiyan/disk-tools.git
cd disk-tools
cargo install --path cli        # installs `disk-tools` into ~/.cargo/bin
```

Or build without installing:

```bash
just build                      # → target/debug/disk-tools
cargo build --release           # → target/release/disk-tools
```

Requires Rust **1.85** or newer (edition 2024, pinned as `rust-version` in the
workspace manifest).

## Usage

```
disk-tools [OPTIONS] <PATH>
```

The path is **always explicit** — `disk-tools` never scans the current directory by
accident.

```console
$ disk-tools project
    6.8M  project                                      ████████████████████ 100%
    5.7M    node_modules                                  █████████████████  85%
    4.0M      bundle.pack                                    ██████████████  70%
    1.7M      .cache                                                 ██████  30%
    1.7M        webpack.bin                            ████████████████████ 100%
 1000.0K    assets                                                      ███  14%
  880.0K      hero.png                                   ██████████████████  88%
  120.0K      logo.svg                                                   ██  12%
   44.0K    src                                                               1%
   32.0K      main.rs                                       ███████████████  73%
   12.0K      lib.rs                                                  █████  27%
    4.0K    README.md                                                         0%
```

Every bar and percentage is **parent-relative**: each entry shows its share of the
directory that contains it, and each directory is 100% of itself. So `bundle.pack`
is 70% of `node_modules`, which is in turn 85% of `project`.

Just the top level:

```console
$ disk-tools project --depth 1
    6.8M  project                                      ████████████████████ 100%
    5.7M    node_modules                                  █████████████████  85%
 1000.0K    assets                                                      ███  14%
   44.0K    src                                                               1%
    4.0K    README.md                                                         0%
```

Only what is 1 MiB or larger:

```console
$ disk-tools project --min-size 1M
    6.8M  project                                      ████████████████████ 100%
    5.7M    node_modules                                  █████████████████  85%
    4.0M      bundle.pack                                    ██████████████  70%
    1.7M      .cache                                                 ██████  30%
    1.7M        webpack.bin                            ████████████████████ 100%
```

Machine-readable output:

```console
$ disk-tools project --json | jq '.root.allocated'
7077888
```

Anything the scan could not read is reported after the tree — a count plus the
first ten paths, or all of them under `--verbose`:

```console
$ disk-tools project --depth 1
    6.8M  project                                      ████████████████████ 100%
    ...
      0B    locked                                                            0%
1 entry skipped:
  project/locked (permission denied)
```

## Flags

| Flag | Effect | Scope |
|------|--------|-------|
| `<PATH>` | Directory (or file) to scan. Required — never defaults to the CWD | scan |
| `-n`, `--number <N>` | Print at most `N` entries | display |
| `--min-size <SIZE>` | Hide entries below `SIZE`. Bare bytes or a 1024-based `K`/`M`/`G`/`T` suffix (`512K`, `1M`, `2G`; `KB`/`KiB` etc. also accepted) | display |
| `--depth <N>` | Print at most `N` levels below the root | display |
| `--apparent` | Rank and report **apparent** size instead of allocated | display |
| `--one-file-system` | Stop at filesystem boundaries instead of descending into other mounts | scan |
| `--json` | Emit JSON instead of the tree report | display |
| `-v`, `--verbose` | List every skipped entry instead of just the first ten | display |
| `-h`, `--help` / `-V`, `--version` | Print help / version | — |

**`--depth` and `--min-size` filter what is printed, never what is counted.** A
directory's size is always its full subtree, exactly like `du`. Hiding a 400 MB
child does not shrink its parent's number — that's the point: you still see where
the space went.

`--json` always emits the **full** tree with raw byte counts; the display filters
apply to the tree report only.

## How sizes are measured

- **Allocated (default)** — what the file actually occupies on disk:
  `blocks × 512` on Unix, `GetCompressedFileSizeW` on Windows. Sparse and
  compressed files therefore report *less* than their content length, and a
  1-byte file reports a whole block.
- **Apparent (`--apparent`)** — the content length, i.e. what `ls -l` shows.
- **Hardlinks are counted once.** The shared blocks are attributed to the
  **lexicographically first** path that reaches them, so directory totals are
  reproducible run to run despite the parallel walk. (Unix only — see below.)
- **Symlinks are never followed**, and their targets are never counted.

## Output streams

| Stream | Content |
|--------|---------|
| stdout | the tree report, or JSON under `--json` |
| stderr | the progress spinner and the skipped-entries summary |

So `disk-tools <path> --json > out.json` yields valid JSON, and the spinner is
suppressed entirely when stderr is not a terminal.

## Limitations (v0.1)

| Limitation | Detail |
|------------|--------|
| **APFS copy-on-write clones are overcounted** | On macOS, a cloned file (`cp -c`, Finder duplicate, many build tools) shares its blocks with the original, but each copy reports its *full* allocated size. A tree of clones therefore sums well above what deleting it would actually reclaim. Detecting shared extents needs per-file `fcntl` probing and is out of scope for v0.1. |
| Hardlinks are not deduplicated on Windows | The walk yields no `(volume, file index)` identity there without an extra `open()` per file, so each link is counted separately. Unix is unaffected. |
| Long UNC paths may be skipped on Windows | Only `C:\`-style drive paths get the `\\?\` prefix that lifts the `MAX_PATH` limit; a `\\server\share\…` path longer than that can end up in the skipped list. |
| Wide glyphs misalign the right edge | Name widths are counted per `char`, so CJK, emoji and combining characters push the bar column out of alignment. |
| The whole tree is held in memory | An accepted trade-off — directory totals and the planned interactive TUI both need the full tree. Peak memory grows with the number of entries scanned. |

## Development

`justfile` is the single entry point for local tooling — add new operations there
rather than as ad-hoc commands.

| Recipe | Runs |
|--------|------|
| `just` | List all recipes |
| `just build` | `cargo build --workspace` |
| `just run <ARGS>` | `cargo run -p disk-tools -- <ARGS>` |
| `just test` | `cargo test --workspace` |
| `just fmt` / `just fmt-check` | `cargo fmt --all` / `--check` |
| `just lint` | Clippy, warnings as errors |
| `just verify` | Pre-commit gate: `fmt-check` + `lint` + `test` |

CI runs `just verify` and `just build` on Linux, macOS and Windows
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

The workspace is `disk-tools-core` (the scanning engine — no printing, no logging,
`deny(unsafe_code)`) plus `disk-tools` (the CLI). Design notes, specs and handoffs
live under [`kb/`](kb/).
