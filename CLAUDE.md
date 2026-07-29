# disc-tools

Cross-platform disk-utilities CLI in Rust, distributed as `disk-tools`. It finds
what eats disk space — a fast parallel scan printing a dust-style size-sorted
tree — shows what could go (`disk-tools preview`), removes it (`disk-tools
clean`, **to the OS trash**), and browses it interactively (`disk-tools ui`).

**v0.1 (scan + tree report), v0.2 (detectors + cleanup engine), v0.3 (config +
declarative rules), v0.4 (the TUI) and v0.5 (preview + clean) are complete.**
Detection is declarative and reads a TOML config; flags beat the file; the
cleanup verbs walk the rule roots when given no path; `clean` refuses while
anything needing confirmation is in the plan. See the
[concept](kb/concepts/2026.07/2026.07.14-disk-tools.md) for the full vision and
its Roadmap for what lands when; the
[v0.1](kb/specs/2026.07/2026.07.14-disk-tools-v0.1-scan-report.md),
[v0.2](kb/specs/2026.07/2026.07.25-disk-tools-v0.2-detectors-cleanup.md) and
[v0.3](kb/specs/2026.07/2026.07.26-disk-tools-v0.3-config-rules.md) specs are
the authoritative task breakdowns; [v0.4](kb/specs/2026.07/2026.07.26-disk-tools-v0.4-tui.md)
(the TUI) **is complete** — `disk-tools ui` browses a directory as a table, sizes
its subdirectories in the background, colours them by what the rules say, filters
with `/`, writes rules back to `config.toml` with every comment intact, and
always gives the terminal back.
**[v0.5](kb/specs/2026.07/2026.07.29-disk-tools-v0.5-preview-clean.md) is
complete** — `preview` shows and `clean` removes, on an identical flag set;
`--apply` and `--allow-dirty` are gone; the report unfolds by `-d` and orders by
`--sort`; where a candidate goes is a third tier (`purge` / `trash` / `confirm`);
and both verbs speak `--json`. **v0.6 is duplicates.** User-facing usage, flags,
the safety model and the documented limitations live in the [README](README.md).

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
| `just lint-windows` | The same, cross-checked against `x86_64-pc-windows-msvc` — clippy only sees what the host compiles, so `#[cfg(windows)]` code is otherwise linted by CI alone |
| `just fmt` / `just fmt-check` | Format / check formatting |
| `just verify` | Pre-commit gate: `fmt-check` + `lint` + `lint-windows` + `doc` + `check-minimal` + `test` |
| `just doc` | Build docs with warnings as errors — catches a public item linking to a private one, which clippy cannot see |
| `just check-minimal` | Core without default features: proves a scan-only consumer still compiles without the trash backend |
| `just smoke-trash` | The `#[ignore]`d tests that move real files to the OS trash, in both crates |
| `just coverage-branch` | Nightly-only branch coverage; advisory, mirrored by a non-blocking CI job |
| `just check` | `cargo check --workspace --all-targets` — CI runs it pinned to MSRV 1.88 |
| `just run <ARGS>` | Run the CLI, e.g. `just run scan ~/Downloads --json` |
| `just release` | Optimized host build → `target/release/disk-tools` |
| `just install-cli` | `cargo install --path cli`, then check that the installed copy is the one first on PATH |
| `just bench-fixtures <dir>` / `just bench <dir>` / `just bench-memory <path>` / `just bench-phases <path>` / `just bench-stat <dir>` | Benchmark harness — needs `hyperfine` + `diskus`; results recorded in `kb/benchmarks/` |

CI (`.github/workflows/ci.yml`) runs `just verify` + `just build` + `just smoke-trash`
on Linux, macOS and Windows, plus a Linux job pinned to MSRV 1.88 running
`just check` and a non-blocking nightly branch-coverage job. It calls the justfile
recipes rather than duplicating cargo commands, so a new local check added there
is automatically enforced in CI.

## Project Structure

```
disc-tools/
├── Cargo.toml          # workspace: members core, cli; edition 2024, MSRV 1.88
├── justfile            # single entry point for local tooling
├── .github/workflows/
│   └── ci.yml          # verify matrix on ×3 OS + an MSRV-pinned check job
├── core/               # disk-tools-core (lib) — the engine
│   ├── Cargo.toml      # rayon; serde + trash (optional); windows-sys on Windows
│   └── src/
│       ├── lib.rs      # deny(unsafe_code); pub fn scan(); re-exports
│       ├── options.rs  # ScanOptions
│       ├── walk.rs     # read_dir + rayon par_iter recursion, skip collection
│       ├── size.rs     # allocated (blocks*512 | GetCompressedFileSizeW) + apparent
│       ├── dedup.rs    # hardlink attribution + the link groups it finds
│       ├── tree.rs     # ScanNode / ScanTree / SkippedEntry + aggregation
│       ├── windows_dir.rs  # cfg(windows): AllocationSize, file id, LastWriteTime
│       ├── paths.rs    # the path comparisons that decide what is a candidate
│       ├── rules.rs    # Rule / Rules — one GlobSet, list order is precedence
│       ├── detect.rs   # the one pass that applies them
│       ├── git.rs      # is there uncommitted work here?
│       ├── clean.rs    # denylist, tiers, totals → CleanPlan. Writes nothing
│       ├── measure.rs  # one subtree's bytes and its claim; reports each directory as it finishes
│       └── trash.rs    # cfg(feature="trash"): the only code that removes anything
├── cli/                # disk-tools (bin) — CLI frontend
│   ├── Cargo.toml      # clap, toml, serde, serde_ignored, indicatif, unicode-width
│   ├── src/
│   │   ├── main.rs     # verb dispatch (scan | preview | clean | ui); spinner to stderr
│   │   ├── args.rs     # clap derive; parse_size, parse_duration; Mode
│   │   ├── config/     # locate/parse/validate the TOML file; `config init`
│   │   │   └── write.rs    # putting a rule back, comments and all
│   │   ├── ui/         # the TUI
│   │   │   ├── term.rs     # restores the terminal on every path
│   │   │   ├── app.rs      # cwd, cursor, order, filter — every key as a function
│   │   │   ├── listing.rs  # one directory, one metadata call per entry
│   │   │   ├── sort.rs     # four orders; reports the one it could apply
│   │   │   ├── layout.rs   # the table: which columns fit, and what is in them
│   │   │   ├── edit.rs     # the rule form and its chooser, as values
│   │   │   └── measure.rs  # the sizing worker, its queue and its session cache
│   │   ├── env.rs      # UserDirs + XDG from the environment — what the core refuses
│   │   └── render/
│   │       ├── mod.rs
│   │       ├── tree.rs     # dust-style tree, parent-relative bars
│   │       ├── json.rs     # --json: the tree, a plan, or an outcome — raw byte counts
│   │       ├── clean.rs    # the plan by depth, and what a removal did
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
| `scan(&ScanOptions) -> ScanTree` | `core/src/lib.rs` | Walk → dedup → aggregate, in that order |
| `plan(&ScanTree, &CleanOptions) -> CleanPlan` | `core/src/clean.rs` | Decides what may go and what that frees. **Writes nothing** |
| `CleanPlan::merge(Vec<CleanPlan>)` | `core/src/clean.rs` | One plan from several roots. Additive **only because** `Rules::scan_roots` drops nested roots |
| `measure(root, claim, cancel, finished) -> Measured` | `core/src/measure.rs` | One subtree's bytes **and what the rules claim of them**, from one walk. Reports **every directory as that directory finishes**, so an outer walk subsumes the inner ones instead of racing them |
| `Claim` | `core/src/measure.rs` | The rules, `now`, and whether `root` is *already* claimed — without the last, a walk of a `node_modules` reports nothing reclaimable, since nothing inside one matches `**/node_modules/` |
| `apply(&CleanPlan, progress) -> CleanOutcome` | `core/src/trash.rs` | The only function that removes anything. Takes **no** removal mode: where each candidate goes is already on it, so `preview` prints exactly what this does. Trashing batches; purging is per item |
| `ScanOptions` | `core/src/options.rs` | The scan's whole input — and the file that states the core reads no config and no environment |
| `ScanNode` / `ScanTree` | `core/src/tree.rs` | A node carries `path`, sizes, `is_dir`, `modified`, `links`, `children`; the tree adds `skipped` and `link_groups` |
| `Rule` / `Rules` | `core/src/rules.rs` | Detection as data. **List order is precedence**; a rule that cannot be expressed matches nothing |
| `Rules::state -> State` | `core/src/rules.rs` | Why a row is that colour: `untracked` / `in scope` / `included` / `excluded`. Shares `matching`, `excluded` **and `predicates_hold`** with `detect`, so `included` means exactly "detect would claim this" |
| `Facts` | `core/src/rules.rs` | What the caller already knows — siblings, mtime, `now` — so the other predicates need no filesystem and no clock. `any_sibling` is a *predicate over names*, not a name: `requires_sibling` is a glob and only `Rules` has it compiled |
| `DetectOptions` / `Detection` | `core/src/detect.rs` | The pass's input and output. `now` is mandatory, so a rule's `older_than` can never be half-armed |
| `CleanPlan` / `Candidate` / `Excluded` | `core/src/clean.rs` | Sorted, non-overlapping candidates; refusals carried with a reason; `filtered_out` / `too_small` count the user's own narrowings. `Candidate.tier` is the **rule's** word and `Candidate.purge` is where it actually goes — see the invariant |
| `CleanOutcome` | `core/src/trash.rs` | `trashed` / `purged` / `failed` — never a `Result`, and the two halves are **never added up by the core**: one figure over both would not say what can be brought back |
| `SkippedEntry` / `SkipReason` | `core/src/tree.rs` | Failures returned **as data** — the core never prints |
| `RenderOptions` | `cli/src/render/tree.rs` | Display-only knobs |
| `Intent` | `cli/src/args.rs` | `Preview` or `Removing` — the verb, as a value. The closing line of the report differs, and printing the wrong one before a deletion is the defect it exists for |
| `Report` | `cli/src/args.rs` | `depth` and `sort` — display only. Nothing in it can keep a candidate out of the removal, only out of the printout |
| `Config` / `Environment` | `cli/src/config.rs`, `cli/src/args.rs` | The file's contents, and everything the frontend resolved before the args became work |

Invariants worth keeping in mind:

- **Phase order matters.** Hardlink attribution must settle before any directory
  total is summed, or totals drift run to run under the parallel walk.
- **Display filters never touch totals.** `--depth`, `--min-size` and `-n` prune
  output only; directory sizes stay full-subtree (du semantics).
- **stdout is for the report, stderr for everything else** — that is what keeps
  `--json` pipe-clean.
- **Windows reads its facts from the directory, not from each file.** Allocated
  size and file identity both come from one `GetFileInformationByHandleEx` call
  per directory; `size.rs` is only the fallback there. Unix takes the per-file
  path and is unchanged.
- **The safe-list is data, not code.** Five built-in `Rule`s replace v0.2's four
  hardcoded categories, and a user may edit any of them. **The denylist is the
  one thing no rule, flag or config can reach.**
- **A rule that cannot be expressed matches nothing** — disabled, an unresolvable
  `~`, a non-UTF-8 root. Unknown reads as *no*, never as *any*.
- **`requires_sibling` is a glob over file names, matched `all`.** Outside Cargo
  no build system offers a fixed marker name — a `bin/` is proven by
  `Whatever.csproj` — and an exact comparison made the field silently claim
  nothing. Separate matchers rather than one `GlobSet`, because two required
  siblings are two questions and a set could only say *something* matched.
- **`deny(unsafe_code)` is exempted per function, never per module** — four
  `#[cfg(windows)]` functions, each listed in `core/src/lib.rs`.
- **`clean` refuses while a confirm-tier candidate remains,** unless `--safe` or
  `--yes`, and exits **2** — not 1, which already means "the removal partly
  failed". It reads the *plan*, not the arguments, which is why clap could never
  have enforced it. There is no interactive prompt and no config key for
  `--yes`: a file that answered yes in advance would cancel the confirmation
  invisibly.
- **`preview` shows and `clean` removes.** Identical flag sets, resolved by one
  function, because a preview is acted on by retyping the same line with the
  other verb. `--apply` is gone.
- **Three tiers, one field: `purge` · `trash` · `confirm`.** All three answer
  *what does `clean` do with this*, which the old `purge`/`auto`/`confirm` could
  not — `purge` named a destination while `auto` named a ceremony. `--safe`
  drops what needs confirming, so it **admits purge**: that is a stronger claim
  of regenerability than trash, not a weaker one.
- **`Candidate.tier` is the rule's word; `Candidate.purge` is where it goes.**
  Two fields because `--purge` overrides only the destination. Rewriting the
  tier would let one flag cancel a confirmation it has nothing to do with, so
  `--safe` and the refusal read `tier` and `apply` reads `purge`.
- **The plan says what will happen, so `apply` takes no removal mode.** That is
  what lets `preview` print exactly what `clean` does instead of a description
  kept in step by hand.
- The denylist stays absolute: no flag and no tier overrides it, `purge`
  included.
- **Trashing is batched** — one backend call, not one per candidate, because on
  macOS each is a ~230 ms `osascript` round-trip. The per-item loop survives as
  the diagnostic path, run only when the batch reports a failure.
- **The core reads no clock and no environment.** `now` and `UserDirs` come from
  `cli/src/env.rs`; that is what makes every rule testable with a temp directory
  standing in for a home.
- **Every path the tool acts on is made absolute in `Args::resolve`**, through
  `args::absolute` — `std::path::absolute`, never `canonicalize`. A rooted rule
  is compiled against an absolute root (`~` resolves to one), so a relative path
  produces nodes no such glob can match and the run silently claims nothing:
  `preview .` inside a project said "Nothing to clean" while `preview /full/path`
  to the same directory found gigabytes. The core cannot do this itself — it
  reads no environment, so it has no working directory to join.
  **Known limitation:** the working directory the kernel returns is already
  symlink-resolved, so a home behind a symlink (`/var` → `/private/var` on
  macOS) makes `~`-rooted rules miss a relative path. Fixing that would mean
  `canonicalize`, which would report and remove somewhere the user never named.
- **No candidate nests inside another** (`detect` never descends into a match),
  which is what lets totals be summed and removals not repeat.
- **A measured total is keyed on its absolute path**, never on the row it came
  from. That is what lets a walk outlive the screen moving on, and it is why the
  browser has no generation counter: a stale answer is not wrong, it is just
  about somewhere else. **A rule change is the exception** — `Sizer`'s `epoch`
  drops answers measured against rules no longer in force, which is a different
  question rather than a different place.
- **Sizes are copied onto the rows before they are sorted.** Sorting first and
  filling the column in after leaves "by size" showing name order in any
  directory measured earlier, because `absorb_sizes` re-sorts only when a walk
  finishes and there no walk has to run. `App::arrive` is the one place that
  orders classify → request → apply → sort.
- **The current directory is a row, not the `..` row.** `..` is the way out;
  `App::here` is what the screen is about. Its figures are the sum of the
  listing — walking `cwd` would cover every row a second time, through them.
- **A fixed column is as wide as its label plus a sort arrow.** `created↑` in a
  seven-wide column shifted every separator after it, in the header only, and
  only while that column sorted.
- **The TUI cancels only what the user asks it to** (`r`) and what exit
  requires. Cancelling on navigation destroyed work that was about to be wanted.
- **The config file is edited, never regenerated.** `toml_edit` keeps comments,
  spacing and key order; serialising the parsed form would throw away the
  explanations that make the file worth having. An **absent `[[rules]]` means
  "leave the built-ins alone"**, so adding the first rule writes them out too
  rather than silently turning five rules into one.
- **A colour and a candidate are decided by the same code.** `detect::claim` and
  `Rules::state` both run `matching` → `excluded` → `predicates_hold`, in that
  order. A `target/` with no `Cargo.toml` beside it is `in scope`, not
  `included`, because that is what `clean` would do with it.
- **`in scope` is a state of its own.** "My rule does not cover this" and "my
  rule is not running" are different problems, and folding them together leaves
  the user unable to tell which they have.
- **Anything that really deletes is `#[ignore]`d** and run by `just smoke-trash`.

## Configuration

`disk-tools` reads a TOML file: `$XDG_CONFIG_HOME/disk-tools/config.toml` when
that is set (on **every** platform), otherwise `%APPDATA%\disk-tools\config.toml`
on Windows and `~/.config/disk-tools/config.toml` elsewhere. `--config <PATH>`
overrides it; `disk-tools config init` writes the commented defaults.

The file supplies the **rules**. An absent `[[rules]]` leaves the built-ins
alone; an empty list means none. `root` is required, and `"*"` is how a rule says
it applies wherever the scan goes. `tier` is one of `purge` / `trash` /
`confirm`, and `"auto"` is an **error** naming `trash` rather than an alias.

The **denylist is not in the file** and cannot be put there, and neither is
`--yes` or a global `--purge`. Per rule, `tier = "purge"` says the same thing
about exactly what that rule claims, which is finer and visible where it applies;
the git guard is `requires-clean-repo` for the same reason.

Precedence is **flag > file > built-in default**, expressed in exactly one place
(`Args::resolve`). No overridable flag carries a clap `default_value`: with one,
`--min-size 0` and an absent `--min-size` would arrive identical, and the first
has to beat the file while the second defers to it. One limitation: a boolean
turned **on** in the file cannot be turned back off from the command line, since
a flag can only be passed or not passed.

What configuration exists beyond that is build-time:

| File | Holds |
|------|-------|
| `Cargo.toml` (workspace) | `version`, `edition = "2024"`, `rust-version = "1.88"`, inherited by both crates |
| `core/Cargo.toml` | The optional `serde` feature; `windows-sys` under `[target.'cfg(windows)'.dependencies]` |
| `cli/Cargo.toml` | Enables the core's `serde` feature for `--json`; `toml_edit` for writing rules back |
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
| `kb/architecture/` | System design, key patterns | `2026.07/2026.07.30` |
| `kb/guides/` | Developer-facing how-tos | `2026.07/2026.07.25` |
| `kb/benchmarks/` | Recorded performance/memory measurements | `2026.07/2026.07.26` |
| `kb/concepts/` | Concept documents (`/write-concept`) | `2026.07` |
| `kb/specs/` | Feature specs (`/write-spec`) | `2026.07/2026.07.29` |
| `kb/brainstorms/` | Brainstorm sessions (`/brainstorm`) | `2026.07` |
| `kb/research/` | Research reports (`/research`) | `2026.07/2026.07.25` |
| `kb/plans/` | Execution plans (`/brainstorm`) | `2026.07/2026.07.25` |
| `kb/handoffs/` | Task handoffs (`/implement-task`) | `2026.07/2026.07.27` |

Files are always written under a `<YYYY.MM>/` folder — never directly under `kb/<folder>/`. Filenames begin with `<YYYY.MM.DD>-` and never include the folder name.

## Documentation

**Architecture** (snapshots from `kb/architecture/2026.07/`)
- [After v0.5: two verbs, three tiers, and a plan that says what it will do](kb/architecture/2026.07/2026.07.30-preview-and-clean.md) — what was ceremony and what was a guard, why `--purge` must not rewrite a tier, display versus plan
- [After v0.4: the browser, and why it does not scan](kb/architecture/2026.07/2026.07.27-tui-lazy-model.md) — the lazy model, what replaced the generation counter, what four rounds of real use found
- [After v0.3: detection as data](kb/architecture/2026.07/2026.07.26-overview.md) — the rule engine, the config path, multi-root `clean`, which invariants moved
- [Overview](kb/architecture/2026.07/2026.07.25-overview.md) — the three-phase pipeline, data model, invariants, platform splits
- [Rust crate structure](kb/architecture/2026.07/2026.07.25-rust-crates.md) — workspace, feature flags, unsafe policy

**Guides** (snapshots from `kb/guides/2026.07/`)
- [Development](kb/guides/2026.07/2026.07.25-development.md) — workflow, justfile recipes, CI, benchmark harness
- [Testing](kb/guides/2026.07/2026.07.25-testing.md) — test layout, platform gating, fixture patterns

**Benchmarks** (snapshots from `kb/benchmarks/2026.07/`)
- [v0.1 scan performance and memory](kb/benchmarks/2026.07/2026.07.25-v0.1-scan-performance.md)
- [What the cleanup engine costs](kb/benchmarks/2026.07/2026.07.25-clean-latency.md) — the git guard at ~23 ms per repository
- [The trash backend](kb/benchmarks/2026.07/2026.07.25-trash-backend.md) — 10,000 files across three platforms
- [What detection costs](kb/benchmarks/2026.07/2026.07.26-detect-budget.md) — 285 ns per node before v0.3's rule engine, 201 ns after
