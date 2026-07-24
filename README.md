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
| `--one-file-system` | Stop at filesystem boundaries instead of descending into other mounts (**Unix only** — see limitations) | scan |
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
  On NTFS, a file small enough to live *inside its MFT record* owns no cluster of
  its own, and its allocated size is reported as its logical length.
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

## Benchmarks

Warm cache, `hyperfine -N --warmup 5 --runs 20`, release build, output discarded.
Mac16,5 (16 cores, 64 GB), macOS 26.5.2, APFS. Mean ± σ over 20 runs:

| Fixture | `disk-tools --depth 0` | `disk-tools` (full tree) | `du -sh` | `diskus` |
|---------|------------------------|--------------------------|----------|----------|
| 105k small files, 1.2k dirs | **167.0 ms ± 11.8** | 220.9 ms ± 12.3 | 344.6 ms ± 48.7 | 100.4 ms ± 8.9 |
| 20k mixed files, 20 GB | **32.3 ms ± 1.6** | 43.8 ms ± 1.5 | 59.6 ms ± 2.1 | 24.4 ms ± 1.4 |
| 40 flat files, 7.8 GB | **3.5 ms ± 0.3** | 3.5 ms ± 0.3 | 6.7 ms ± 0.5 | 4.7 ms ± 0.3 |

`--depth 0` is the like-for-like comparison with `du -sh`: both print one summary
line. On that footing `disk-tools` is **1.8–2.1× faster than `du -sh`**, and it stays
1.4–1.9× faster even while rendering the whole tree, which `du -sh` never does.
`diskus` is 1.3–1.7× ahead on metadata-heavy trees because it accumulates one total
and keeps nothing, where `disk-tools` builds the tree the report — and the future TUI
— needs; on the flat media library `disk-tools` is the fastest of the three.

**Memory:** peak RSS is **≈ 630 bytes per entry** — 1.42 GB for a real 2,247,326-entry
tree, 868 MB for a 1,400,409-entry one.

Full protocol, raw numbers and caveats:
[kb/benchmarks/2026.07/2026.07.25-v0.1-scan-performance.md](kb/benchmarks/2026.07/2026.07.25-v0.1-scan-performance.md).
Reproduce with `just bench-fixtures <dir>` → `just bench <dir>` → `just bench-memory <path>`.

## Limitations (v0.1)

| Limitation | Detail |
|------------|--------|
| **APFS copy-on-write clones are overcounted** | On macOS, a cloned file (`cp -c`, Finder duplicate, many build tools) shares its blocks with the original, but each copy reports its *full* allocated size. A tree of clones therefore sums well above what deleting it would actually reclaim. Detecting shared extents needs per-file `fcntl` probing and is out of scope for v0.1. |
| Hardlinks are not deduplicated on Windows | The walk yields no `(volume, file index)` identity there without an extra `open()` per file, so each link is counted separately. Unix is unaffected. |
| `--one-file-system` does nothing on Windows | Mount-boundary detection needs a device id (`st_dev`), which the walk cannot obtain there without an extra `open()` per directory. The flag is accepted and silently has no effect off Unix. |
| Long UNC paths may be skipped on Windows | Only `C:\`-style drive paths get the `\\?\` prefix that lifts the `MAX_PATH` limit; a `\\server\share\…` path longer than that can end up in the skipped list. |
| Wide glyphs misalign the right edge | Name widths are counted per `char`, so CJK, emoji and combining characters push the bar column out of alignment. |
| The whole tree is held in memory | An accepted trade-off — directory totals and the planned interactive TUI both need the full tree. Costs **≈ 630 bytes per entry**: 1.4 GB for a 2.2M-entry scan (measured, see [Benchmarks](#benchmarks)). |

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
| `just bench-fixtures <dir>` | Generate the benchmark fixtures (~28 GB) |
| `just bench <dir>` | Benchmark against `du -sh` and `diskus` (needs `hyperfine`, `diskus`) |
| `just bench-memory <path>` | Peak RSS of one scan |

CI runs `just verify` and `just build` on Linux, macOS and Windows
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

The workspace is `disk-tools-core` (the scanning engine — no printing, no logging,
`deny(unsafe_code)`) plus `disk-tools` (the CLI). Design notes, specs and handoffs
live under [`kb/`](kb/).
