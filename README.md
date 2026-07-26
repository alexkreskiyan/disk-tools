# disk-tools

Find what's eating your disk. `disk-tools` walks a directory tree in parallel,
measures **real on-disk (allocated) size**, and prints a size-sorted tree of the
biggest consumers — or JSON.

**v0.3 — scan, a cleanup engine, and a config file.** `disk-tools clean` finds
regenerable junk and stale files and offers to remove them **to the OS trash**.
It is a dry run by default: nothing is deleted without `--apply`.

What counts as junk is no longer fixed. Detection is a list of rules you can
read and edit — `disk-tools config init` writes the defaults out with their
comments — and a rule can be narrowed to a directory, given a size floor, or
written from scratch. A TUI comes next, on the same core. See the
[concept](kb/concepts/2026.07/2026.07.14-disk-tools.md) for the full vision and
the specs for the
[scanner](kb/specs/2026.07/2026.07.14-disk-tools-v0.1-scan-report.md),
the [cleanup engine](kb/specs/2026.07/2026.07.25-disk-tools-v0.2-detectors-cleanup.md)
and [configuration](kb/specs/2026.07/2026.07.26-disk-tools-v0.3-config-rules.md).

Cross-platform: macOS, Linux, Windows.

## Install

From a checkout:

```bash
git clone https://github.com/alexkreskiyan/disk-tools.git
cd disk-tools
just install-cli                # installs into ~/.cargo/bin, and checks
                                # that the copy you just built is the one
                                # first on your PATH
```

Or build without installing:

```bash
just build                      # → target/debug/disk-tools
just release                    # → target/release/disk-tools
```

There are no published binaries yet — see the concept's *Distribution — deferred*
for what packaging will involve.

Requires Rust **1.85** or newer (edition 2024, pinned as `rust-version` in the
workspace manifest).

## Usage

```
disk-tools <COMMAND> [OPTIONS] <PATH>
```

Three verbs, and a bare `disk-tools` prints help rather than guessing:

| | |
|---|---|
| `disk-tools scan <PATH>` | measure and report |
| `disk-tools clean <PATH>` | find removable junk |
| `disk-tools ui <PATH>` | interactive browser — *planned, v0.4* |

The path is **always explicit** — `disk-tools` never scans the current directory by
accident.

```console
$ disk-tools scan project
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
$ disk-tools scan project --depth 1
    6.8M  project                                      ████████████████████ 100%
    5.7M    node_modules                                  █████████████████  85%
 1000.0K    assets                                                      ███  14%
   44.0K    src                                                               1%
    4.0K    README.md                                                         0%
```

Only what is 1 MiB or larger:

```console
$ disk-tools scan project --min-size 1M
    6.8M  project                                      ████████████████████ 100%
    5.7M    node_modules                                  █████████████████  85%
    4.0M      bundle.pack                                    ██████████████  70%
    1.7M      .cache                                                 ██████  30%
    1.7M        webpack.bin                            ████████████████████ 100%
```

Machine-readable output:

```console
$ disk-tools scan project --json | jq '.root.allocated'
7077888
```

Anything the scan could not read is reported after the tree — a count plus the
first ten paths, or all of them under `--verbose`:

```console
$ disk-tools scan project --depth 1
    6.8M  project                                      ████████████████████ 100%
    ...
      0B    locked                                                            0%
1 entry skipped:
  project/locked (permission denied)
```

## Cleaning up

`disk-tools clean [PATH]` looks for things that can be regenerated, and tells you
what it would remove:

```console
$ disk-tools clean ~/code
   40.0K  pycache       auto  ~/code/proj/__pycache__
    1.2M  node-modules  auto  ~/code/proj/node_modules

Reclaimable: 1.2M

Not touched:
  ~/code/proj/target — uncommitted changes; --allow-dirty to include

Dry run — nothing was removed. Re-run with --apply.
```

**Nothing is deleted without `--apply`.** The command above touched nothing; it
is a report. And when you do pass `--apply`, everything goes to the **OS trash**
— a cleanup tool that is wrong once should cost you a trip to the Trash, not
your data. (`--purge` deletes outright instead; see [below](#purge).)

### What it looks for

Five rules ship built in. They are ordinary rules, not a privileged set — `disk-tools
config init` writes them into your config file, where you can narrow, disable or
replace any of them. See [Configuration](#configuration).

| Rule | Matches | Requires |
|------|---------|----------|
| `rust-target` | `target/` | a `Cargo.toml` beside it |
| `node-modules` | `node_modules/` | — |
| `pycache` | `__pycache__/`, loose `*.pyc` | — |
| `user-caches` | `~/.cache`, `~/Library/Caches` | a known home directory |
| `windows-temp` | `%LOCALAPPDATA%\Temp` | Windows |
| `old` | anything untouched for `--older-than` | the flag; **off by default** |

**List order is precedence.** The first rule that matches a path claims it, which
is why an ancient `target/` is reported as `rust-target` rather than as `old` —
and therefore keeps that rule's tier.

A match is never descended into: a `node_modules` with 40,000 files is one
candidate, not 40,000.

`rust-target` needs the manifest because `target/` is an ordinary directory name.
Without that check, someone's `target/` of photographs would be build output.

### Four things stand between a match and a deletion

**1. The never-touch denylist.** `/System`, `/Library/Caches` (no tilde),
`/Windows`, `/Program Files`, `~/Library/Application Support`, `%APPDATA%` — and
anything inside them. **No flag overrides this**, including `--apply`. Note the
tilde: `~/Library/Caches` is a candidate, `/Library/Caches` is not.

**2. Two tiers.** The built-in rules above are `auto` — regenerable, so removable
without argument. Anything matched by age is `confirm`. `--safe` offers only the
`auto` tier, and `--apply` refuses while anything `confirm` is still in the plan.

> **A rule you write sets its own tier, and `auto` is a claim the tool cannot
> check.** For the five built-ins, `auto` is this project's word that the content
> comes back from a build or a lockfile install. For a rule in your config file it
> is *your* word, and `--apply` will take it without asking. Nothing verifies that
> what you marked regenerable is. If you are unsure, leave `tier` out — an
> unstated tier is `confirm`.

**3. The git guard.** Build output belonging to a project with uncommitted
changes is left alone — you may be mid-work, and it only regenerates identically
from committed source. `--allow-dirty` includes it anyway. If `git` is missing or
the repository cannot be read, the answer is "dirty": when the tool cannot know,
it does not delete.

**4. The dry run itself**, which is the default.

Exclusions are always reported with a reason, and the two reasons are not
interchangeable — `--allow-dirty` relaxes the guard and nothing else.

### Removing

```console
$ disk-tools clean ~/code --apply
Removed 3 of 3. Freed 2.0M.
```

The plan is printed first, to stderr, so the last thing you see before a deletion
is the list of what is about to go. A partial failure exits non-zero and names
every path still on disk.

**`--apply` stops while anything not regenerable is in the plan:**

```console
$ disk-tools clean ~/code --older-than 90d --apply
   40.0K  pycache       auto     ~/code/proj/__pycache__
    1.1M  node-modules  auto     ~/code/proj/node_modules
    4.0K  old           confirm  ~/code/proj/notes.txt

Reclaimable: 1.2M

Dry run — nothing was removed. Re-run with --apply.

1 candidate is not regenerable, and nothing was removed.
Add --safe to take only the regenerable ones, or --yes to take all of them.
```

It exits 2 and removes nothing at all — not even the two regenerable ones, since
a partial removal you did not ask for is its own surprise. `--safe` takes the
`auto` tier and leaves the rest; `--yes` takes everything, saying the count aloud
first.

There is no prompt. A real one needs stdin, TTY detection and a story for piped
input, which belongs with the interactive browser; a refusal gives the same
guarantee — nothing not regenerable goes without you saying so — for the price of
one flag. Set `require-confirmation = false` in your config to get v0.2's
behaviour back, where reading the list and typing `--apply` was the confirmation.

### Cutting the noise

A cleanup of a Python project can turn up a hundred and fifty `__pycache__`
directories of a few kilobytes each, burying the two entries that actually
matter. `--min-size` drops them:

```console
$ disk-tools clean ~/code --min-size 1M
    2.0M  node-modules  auto  ~/code/proj/node_modules

Reclaimable: 2.0M

150 more candidates are below --min-size.
```

A rule can carry its own `min-size`, and the report says which threshold applied
— "below `--min-size`" or "below their rule's own min-size". The remedies differ:
one is a flag on this command line, the other a line in a file you have to go and
find.

**This narrows the plan, not just the display.** `scan --min-size` hides rows
while the totals stay whole; here the report *is* the list of what `--apply` will
remove, so showing two entries and removing a hundred and fifty would be exactly
the mismatch every other rule here exists to prevent. The count of what was
dropped is printed, and it is a separate line from `--safe`'s — the two have
different remedies.

### Purge

The trash is not free. On macOS every removal is an `osascript` round-trip to
Finder — **~230 ms per call**, whatever the size of what is being removed — so a
tree of many small `__pycache__` directories used to take minutes. Removals are
now sent in **one batch**, which brought 60 such directories from 14 s to 1.4 s.

Where even that is more ceremony than the content deserves:

```console
$ disk-tools clean ~/code --apply --purge
Deleting outright — these will NOT go to the trash and cannot be put back.
Removed 3 of 3. Freed 2.0M.
```

**`--purge` deletes permanently.** No trash, no "Put Back", nothing to recover.
The same 60 directories take 0.02 s.

It requires `--apply` — on its own it is a usage error, so the intent has to be
stated twice — and the report never claims anything is recoverable after it. Use
it for build output and caches you would not miss; use the default for anything
you would.

### Reclaimable is an upper bound

A candidate holding content hardlinked from outside it does not free its full
size when removed. Those are flagged `(shared)` and the total is reported as
"at most":

```
Reclaimable: at most 4.0G — 1 candidate shares content with something outside
it, so removing them may free less.
```

**On Windows the absence of that flag proves nothing.** Getting a link count
there needs a file handle per file, which this tool will not spend, so sharing
with anything outside the scanned tree is invisible. Sharing *within* the scan is
detected on both platforms. Do not compare a Windows figure with a Unix one and
conclude anything.

## Configuration

`disk-tools` reads one file. Write the defaults out and edit them:

```console
$ disk-tools config init
/Users/you/.config/disk-tools/config.toml
```

It refuses to overwrite an existing file without `--force`, and the path it
prints is the one it read: `$XDG_CONFIG_HOME/disk-tools/config.toml` when that is
set — on **every** platform, since exporting it is you saying where your
configuration lives — otherwise `%APPDATA%\disk-tools\config.toml` on Windows and
`~/.config/disk-tools/config.toml` elsewhere. `--config <PATH>` overrides all of
it.

**No config file is an ordinary state**: the built-in rules apply and nothing is
reported. A `--config` path that is *not there* is an error, because that is a
typo and defaults quietly substituted for it would leave you cleaning under rules
you never wrote.

### A rule

```toml
[[rules]]
name                = "github-node-modules"
root                = "~/Projects/github"   # "*" means wherever the scan goes
includes            = ["**/node_modules/"]  # trailing / is directory-only
excludes            = ["**/vendor/**"]
requires-sibling    = "Cargo.toml"          # a file that must be beside a match
requires-clean-repo = true                  # skip if the repo has uncommitted work
older-than          = "90d"
min-size            = "10M"
tier                = "confirm"             # unstated means confirm
enabled             = true
```

`root` is required, and answers "what do I clean when no path is named". Use
`"*"` for a rule that applies wherever the scan goes — which is what three of the
five built-ins say.

A trailing `/` in `includes` means **directory only**, as in gitignore. That is
why `**/*.pyc` matches files and `**/node_modules/` does not match a file of that
name.

`~`, `%LOCALAPPDATA%` and `%APPDATA%` are expanded from your environment. **A
token that cannot be resolved disables its rule** rather than widening it — an
unknown home is never treated as "any home".

`min-size` on a rule narrows the plan just as `--min-size` does, and the report
says which of the two applied. They are different things to go and change.

### Running with no path

```console
$ disk-tools clean
disk-tools: examining /Users/you
```

With no `<PATH>`, `clean` walks the roots of your enabled rules, merged so none
contains another. The default config roots `user-caches` at your home directory,
so a bare `clean` walks all of it — slow, but a dry run. Narrow that rule, or
name a path.

If nothing names a directory it says so rather than reporting an empty plan:

```console
$ disk-tools clean
disk-tools: no rule names a directory to clean.
Pass a path, or give a rule a `root` other than "*".
```

### What the file cannot do

| Not configurable | Why |
|------------------|-----|
| **The denylist** | A denylist a config can edit is not a denylist. `/System`, `/Windows`, `/Library/Caches` and the rest stay in the binary, and no rule, flag or file reaches them |
| `--purge` | Deleting past the trash stays an explicit choice on one run. A default in a file would make it invisible |
| `--allow-dirty` | Same: a file that silently disabled the git guard would hide that it had |
| `--yes` | A file answering yes in advance cancels the confirmation, and cancels it invisibly |

One limitation worth knowing: a true/false setting turned **on** in the file
cannot be turned back off from the command line, because a flag can only be
passed or not passed. For `[clean] safe` that direction is the right one — the
file may only make a cleanup more cautious.

## Flags

`disk-tools scan <PATH>`:

| Flag | Effect | Scope |
|------|--------|-------|
| `<PATH>` | Directory (or file) to scan. Required — never defaults to the CWD | scan |
| `-n`, `--number <N>` | Print at most `N` entries | display |
| `--min-size <SIZE>` | Hide entries below `SIZE`. Bare bytes or a 1024-based `K`/`M`/`G`/`T` suffix (`512K`, `1M`, `2G`; `KB`/`KiB` etc. also accepted) | display |
| `--depth <N>` | Print at most `N` levels below the root | display |
| `--apparent` | Rank and report **apparent** size instead of allocated | display |
| `--one-file-system` | Stop at filesystem boundaries instead of descending into other mounts (**Unix only** — see limitations) | scan |
| `--json` | Emit JSON instead of the tree report | display |
| `-v`, `--verbose` | List every skipped entry instead of just the first ten. **Global** — works with any verb | display |
| `--config <PATH>` | Read this file instead of the one in your config directory. **Global** | — |
| `-h`, `--help` / `-V`, `--version` | Print help / version | — |

`disk-tools clean [PATH]` takes its own. **The path is optional** — without it,
the roots of your configured rules are walked; see
[Configuration](#running-with-no-path).

| Flag | Effect |
|------|--------|
| `--apply` | **Actually remove**, to the OS trash. Without it nothing is touched |
| `--yes` | Also remove what is not regenerable. Without it `--apply` refuses while any `confirm`-tier candidate remains. Requires `--apply` |
| `--safe` | Offer only the `auto` tier — regenerable output, nothing needing confirmation |
| `--min-size <SIZE>` | Ignore anything smaller. **Narrows the plan, not just the printout** — unlike `scan`'s flag of the same name, what is shown is what `--apply` removes |
| `--purge` | Delete **permanently** instead of trashing. Nothing can be put back; requires `--apply` |
| `--allow-dirty` | Include build output whose project has uncommitted changes. Relaxes **only** the git guard |
| `--older-than <DURATION>` | Also offer anything untouched for this long: `90d`, `2w`, `6m`, `1y`. A bare number is rejected — `90` could mean seconds as easily as days. `m` is 30 days, `y` is 365 |

**`--depth` and `--min-size` filter what is printed, never what is counted.** A
directory's size is always its full subtree, exactly like `du`. Hiding a 400 MB
child does not shrink its parent's number — that's the point: you still see where
the space went.

`--json` always emits the **full** tree with raw byte counts; the display filters
apply to the tree report only.

## How sizes are measured

- **Allocated (default)** — what the file actually occupies on disk: `blocks × 512`
  on Unix, `AllocationSize` from the directory listing on Windows. Sparse and
  compressed files therefore report *less* than their content length, and a
  1-byte file reports a whole cluster.
- **Apparent (`--apparent`)** — the content length, i.e. what `ls -l` shows.
- **Hardlinks are counted once.** The shared blocks are attributed to the
  **lexicographically first** path that reaches them, so directory totals are
  reproducible run to run despite the parallel walk. Identity is `(device, inode)`
  on Unix and `(volume serial, file id)` on Windows.
- **Symlinks are never followed**, and their targets are never counted.

## Output streams

| Stream | Content |
|--------|---------|
| stdout | the tree report, or JSON under `--json` |
| stderr | the progress spinner and the skipped-entries summary |

So `disk-tools <path> --json > out.json` yields valid JSON, and the spinner is
suppressed entirely when stderr is not a terminal.

Closing the pipe early — `disk-tools scan ~ | head` — is treated as a normal end of
output: the scan stops quietly and exits `0`, like any other Unix filter.

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
The comparison with `diskus` **depends on the tree's shape and the machine's load** —
independent reruns have put it anywhere from 1.7× ahead to slightly behind. It
accumulates one total and keeps nothing, where `disk-tools` builds the tree that the
report, and the planned TUI, both need.

Cleanup adds one cost worth knowing: the git guard spawns `git status` once per
repository, measured at **~23 ms each**, which dominates a dry run over a tree of
many projects — see [the note](kb/benchmarks/2026.07/2026.07.25-clean-latency.md).
Every other rule is a pure pass over the already-built tree.

**The parallelism has a low ceiling.** Only recursion into subdirectories runs in
parallel; the per-entry loop inside a single directory does not, and neither does
hardlink attribution or aggregation. Measured on 16 cores: 2.1–2.7× end to end, and
**1.08× for 50,000 files in one flat directory**. See the note below for the
per-phase split.

**Memory:** peak RSS is **≈ 630 bytes per entry** — 1.42 GB for a real 2,247,326-entry
tree, 868 MB for a 1,400,409-entry one.

Full protocol, raw numbers and caveats:
[kb/benchmarks/2026.07/2026.07.25-v0.1-scan-performance.md](kb/benchmarks/2026.07/2026.07.25-v0.1-scan-performance.md).
Reproduce with `just bench-fixtures <dir>` → `just bench <dir>` → `just bench-memory <path>`.

## Limitations

Cleanup, first — these are the ones worth reading before `--apply`:

| Limitation | Detail |
|------------|--------|
| **On Windows, one failure mode cannot be caught** | The `trash` crate calls `CoCreateInstance(...).unwrap()` on its Windows delete path. If COM cannot be initialised — a service, a session-0 process, some sandboxes — it **panics** rather than returning an error, aborting the run mid-way. No wrapper here can convert a panic, so "the summary names what survived" holds for every failure the backend *reports*, and not for that one. `just smoke-trash` runs on all three platforms in CI so a COM-hostile environment shows up as a red build. |
| **On macOS, recoverability cannot be verified by a test** | `~/.Trash` needs Full Disk Access and the crate offers no way to list it there, so an automated test can assert only that the original path is gone — which an unrecoverable delete would also satisfy. It rests on the crate's documented behaviour and one manual check. Linux and Windows can be checked properly. |
| **`(shared)` is not detectable across the scan boundary on Windows** | See [Reclaimable is an upper bound](#reclaimable-is-an-upper-bound). Absence of the flag there is not evidence of unshared content. |
| **Removing to the trash costs ~230 ms per batch on macOS** | Every trash operation is an `osascript` round-trip to Finder. Removals are batched into one call, so the cost is per *run* rather than per candidate — 60 small directories take 1.4 s, against 0.02 s for `--purge`. |
| **A dry run costs ~23 ms per repository** | The git guard spawns `git status --porcelain` once per repository. Over 50 dirty Rust projects a `clean` is 1.2 s against 15 ms for a plain scan — 80×. `--allow-dirty` skips it entirely and is free. [Measured.](kb/benchmarks/2026.07/2026.07.25-clean-latency.md) |
| **`--apply` refuses rather than prompting** | For the `confirm` tier it stops and asks you to add `--safe` or `--yes`, rather than asking about each path. A real prompt needs stdin, TTY detection and a story for piped input, which belongs with the interactive browser. See [Removing](#removing). |
| **`auto` on a rule you wrote is unverified** | The tool takes your word that the content regenerates, and removes it without asking. For the five built-in rules `auto` is this project's claim; for yours it is yours. See [Two tiers](#four-things-stand-between-a-match-and-a-deletion). |

And the scanner's, unchanged from v0.1:

| Limitation | Detail |
|------------|--------|
| **APFS copy-on-write clones are overcounted** | On macOS, a cloned file (`cp -c`, Finder duplicate, many build tools) shares its blocks with the original, but each copy reports its *full* allocated size. A tree of clones therefore sums well above what deleting it would actually reclaim. Detecting shared extents needs per-file `fcntl` probing and is out of scope for v0.1. |
| `--one-file-system` does nothing on Windows | Mount-boundary detection needs a device id per *candidate directory*; the walk has the volume serial of the directory it is listing, not of the subdirectory it is about to enter. The flag is accepted and silently has no effect off Unix. |
| Long UNC paths may be skipped on Windows | Affects only the per-file fallback used when the directory listing does not cover an entry: there, only `C:\`-style drive paths get the `\\?\` prefix that lifts the `MAX_PATH` limit, so a longer `\\server\share\…` path can end up skipped. |
| The whole tree is held in memory | An accepted trade-off — directory totals and the planned interactive TUI both need the full tree. Costs **≈ 630 bytes per entry**: 1.4 GB for a 2.2M-entry scan (measured, see [Benchmarks](#benchmarks)). |

## Development

`justfile` is the single entry point for local tooling — add new operations there
rather than as ad-hoc commands.

| Recipe | Runs |
|--------|------|
| `just` | List all recipes |
| `just build` | `cargo build --workspace` |
| `just run <ARGS>` | `cargo run -p disk-tools -- <ARGS>` |
| `just release` | Optimized build for this machine (`target/release/disk-tools`) |
| `just test` | `cargo test --workspace` |
| `just fmt` / `just fmt-check` | `cargo fmt --all` / `--check` |
| `just lint` | Clippy, warnings as errors |
| `just verify` | Pre-commit gate: `fmt-check` + `lint` + `test` |
| `just check` | `cargo check --workspace --all-targets` (CI runs it pinned to the MSRV) |
| `just bench-fixtures <dir>` | Generate the benchmark fixtures (~28 GB) |
| `just bench <dir>` | Benchmark against `du -sh` and `diskus` (needs `hyperfine`, `diskus`) |
| `just bench-memory <path>` | Peak RSS of one scan |
| `just bench-phases <path>` | Where a scan spends its time, by phase |
| `just bench-stat <dir>` | Whether stat-ing one directory's entries scales |

CI runs `just verify` and `just build` on Linux, macOS and Windows, plus a
Linux job pinned to the MSRV running `just check`
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

The workspace is `disk-tools-core` (the scanning engine — no printing, no logging,
`deny(unsafe_code)`) plus `disk-tools` (the CLI). Design notes, specs and handoffs
live under [`kb/`](kb/).
