# disk-tools

Find what's eating your disk. `disk-tools` walks a directory tree in parallel,
measures **real on-disk (allocated) size**, and prints a size-sorted tree of the
biggest consumers — or JSON.

**v0.7 — scan, preview, clean, duplicates, rules made of parts, and a browser.**
Two verbs, not one flag: **`disk-tools preview` shows what would go and changes
nothing; `disk-tools clean` removes it**, to the OS trash, immediately. They take
an identical flag set, because the way a preview is acted on is to retype the
same line with the other verb.

`clean` still refuses while anything in the plan is not regenerable — that is the
guard that mattered, and it stayed when `--apply` went.

**`--dup` searches by content instead of by rule.** Same two verbs, same
removal, same refusal: `preview --dup <PATH>` finds files whose bytes are
identical, groups them, and says which copy stays.

**A rule is a name, a consequence and a list of parts** — satisfying any part
satisfies the rule. That is what lets one rule say "a `bin/` beside a `*.csproj`
**or** beside a `*.fsproj`", which three independent lists could not. Duplicates
have their own rules on the same shape, where the parts pool instead of matching.

**`--explain` says what a command would do and does nothing else** — the file it
read, the rules in force, the ones dropped and why, where each value came from,
and whether the run would stop.

What counts as junk is not fixed. Detection is a list of rules you can read and
edit — `disk-tools config init` writes the defaults out with their comments —
and a rule can be narrowed to a directory, given a size floor, or written from
scratch. `disk-tools ui` walks the same rules interactively: it colours every
row by what they say about it, sizes directories in the background, and can
**write a rule back to the config file** without disturbing what is already
there. See the [concept](kb/concepts/2026.07/2026.07.14-disk-tools.md) for the
full vision and the specs for the
[scanner](kb/specs/2026.07/2026.07.14-disk-tools-v0.1-scan-report.md),
the [cleanup engine](kb/specs/2026.07/2026.07.25-disk-tools-v0.2-detectors-cleanup.md),
[configuration](kb/specs/2026.07/2026.07.26-disk-tools-v0.3-config-rules.md),
the [browser](kb/specs/2026.07/2026.07.26-disk-tools-v0.4-tui.md)
[preview + clean](kb/specs/2026.07/2026.07.29-disk-tools-v0.5-preview-clean.md)
[duplicates](kb/specs/2026.07/2026.07.30-disk-tools-v0.6-duplicates.md)
and [rules made of parts](kb/specs/2026.07/2026.07.30-disk-tools-v0.7-duplicate-rules.md).

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

Requires Rust **1.88** or newer (edition 2024, pinned as `rust-version` in the
workspace manifest and enforced by a CI job). 1.88 rather than something older
because `ratatui` 0.30.2 needs it; before the browser the floor was 1.85.

## Usage

```
disk-tools <COMMAND> [OPTIONS] <PATH>
```

Four verbs, and a bare `disk-tools` prints help rather than guessing:

| | |
|---|---|
| `disk-tools scan <PATH>` | measure and report |
| `disk-tools preview [PATH]` | what `clean` would remove. Changes nothing |
| `disk-tools clean [PATH]` | remove it, to the OS trash |
| `disk-tools ui [PATH]` | interactive browser; defaults to the current directory |

`scan` demands a path — it never scans the current directory by accident.
`preview` and `clean` ask your rules where to look when you give them none, and
`ui` opens where you are, which is the one case where guessing is what you meant.

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

## Browsing

`disk-tools ui [PATH]` opens a browser over the same core. No path means the
directory you are in.

```
~/Projects                                    ← where you are, and any notice
    size │    clean │ name↑        │  created │  modified │ total
    3.1G │   890.0M │ Projects/    │       2y │       11m │ ███████ 100%   ← here
         │          │ ../          │          │           │
    1.2G │   890.0M │ old-app/     │       2y │       11m │ ███     38%
       ⠹ │          │ current/     │      30d │        2h │
    4.0K │          │ README.md    │      30d │        2h │
rules: included  excluded  in scope  untracked
q quit  ↵ enter  ← up  / filter  n/s/c/m sort  r sizes  R config
```

The row under the labels is **the directory you are in**, in the same columns as
everything below it. `..` is the way out of here, not a description of here, and
without a row of its own the one directory the screen is about was the one thing
it never said anything about. Its figures are the sum of the listing rather than
a walk of their own — measuring `cwd` would walk every row a second time,
through itself.

### `clean` — what a rule would take

The `clean` column is the point of the browser: how much of this row `clean`
would remove under the rules in force. It is worked out by the same walk that
counts the bytes, so it costs no second pass, and by the same code that decides
a candidate — a figure here and a line in `clean`'s report cannot disagree.

- A **file** a rule claims is claimed whole.
- A **directory** carries what the walk found inside it: `Projects/` above is
  3.1G of which 890M is junk, all of it in `old-app/`.
- A directory a rule claims **outright** is reclaimable in full, including the
  parts of it no pattern would match on their own. Nothing inside a
  `node_modules` matches `**/node_modules/`, and it all goes anyway.

An **empty cell means nothing to take *or* nothing known yet.** The two are told
apart one column to the left: a row still being walked is spinning. A `0` there
would read as a verdict on a directory nobody has been into.

Editing a rule (`a`) or re-reading the config (`R`) **measures again**. Every
figure in the column was worked out against rules that no longer exist, and a
screen still answering the previous question is the one thing changing a rule is
meant to stop.

### Keys

| Key | Does |
|-----|------|
| `↑` `↓`, `j` `k` | Move |
| `PgUp` `PgDn`, `Home` `End` | A screenful, or the whole way |
| `↵`, `l`, `→` | Enter the directory. On a file, nothing |
| `←`, `h`, `Backspace` | Up one level, landing on the directory you left |
| `n` `s` `c` `m` | Sort by name, size, created, modified — one press from anywhere. The same key again reverses |
| `/` | Filter this listing. Letters narrow it as you type, `↵` keeps it, `Esc` drops it |
| `r` | Measure this directory's subdirectories again |
| `R` | Read `config.yml` again |
| `D` | Remove what the rules claim under this row — see below |
| `q` | Leave |

The parent row (`..`) is an ordinary entry, so `↵` on it goes up. It is never
filtered away — a screen where no key does anything would be worse than a
listing that shows one row you did not ask for.

### Sizes arrive while you watch

A directory's size costs a walk of everything beneath it, so it is computed in
the background. A spinner sits beside the figure while it climbs, and the `total`
column fills in when it settles.

Three things follow from that, and all three are deliberate:

- **Percentages are against the sum of what is known**, not against a parent
  total. That total would cost a second walk of the level above and nobody asked
  for it. A directory still being measured is left out of both sides, so the
  denominator does not drift under the rows that have settled.
- **Navigating away does not cancel anything.** A walk in flight is a walk that
  will be wanted; it finishes, and its answer is there when you come back.
- **Totals are kept for the session.** Stepping in and out of a directory does
  not recompute it, and a walk of one directory records everything beneath it —
  so entering a directory that has been measured is free, at any depth. `r` is
  how you say the disk has changed.

A chosen order survives that. Sizes are copied onto the rows **before** they are
sorted, so walking into a directory whose totals are already known comes out in
the order you asked for rather than in name order — the case where nothing
finishes, because nothing has to run.

Ages are relative (`2h`, `30d`, `2y`) rather than dates. A date needs a timezone
and nothing here can supply one; `SystemTime` is UTC, and printing UTC to
someone browsing their own disk is wrong by up to half a day without saying so.

### The four states

Every row is coloured by what the configured rules say about it.

| State | Colour | Means |
|-------|--------|-------|
| `included` | yellow | A rule claims it — this is what `clean` would offer to remove |
| `excluded` | green | A rule names it to be left alone |
| `in scope` | blue | Inside some rule's territory, matched by none of its patterns |
| `untracked` | plain | No rule's root contains it. Nothing is watching this |

`in scope` is a state of its own rather than a shade of `untracked`, because "my
rule does not cover this" and "my rule is not running" are different problems
with different fixes. A rule that could not be compiled — disabled, an
unresolvable `~` — is not in force, so its territory reads as `untracked`.

The legend appears only when something in the current directory is under a rule.
Every state carries its word as well as its colour: a row of swatches is no
legend to a reader who cannot tell them apart.

`included` means exactly *`clean` would claim this*. The browser runs the same
`includes` → `excludes` → predicates sequence `clean` does, so a `target/` with
no `Cargo.toml` beside it shows as `in scope`, not as junk. One predicate is
deliberately not consulted: `requires-clean-repo` costs a `git status` per
repository, and it is a question about whether `clean` will *act*, not about
whether a rule claims the path.

### What it will not do

`ui` needs a terminal and refuses a pipe with a sentence rather than escape
sequences; use `scan` or `preview` for something you can redirect. It never
deletes on its own account: `D` runs `clean`'s plan, with its tiers, its
refusals and its denylist, and asks before any of it happens.

### Removing from the browser

`D` cleans what the rules claim under the row the cursor is on — the same plan
`clean <that path>` would make, from the screen that already shows you which
rows are junk and how much of each is reclaimable.

Three things keep it a keystroke rather than a hazard.

**It only removes what a rule already claims.** On a row nothing claims it says
so and stops. Anything else would make the browser a general file deleter and
the tiers and the denylist decoration on it.

**It always asks**, and what changes with the tier is the price of a mistake,
not whether you are asked. On the command line the confirmation is the verb —
you type `clean` yourself, and that is the moment of intent. A keypress has no
such moment, so the modal supplies one:

```console
This destroys files. There is no way back.
/Users/you/Projects/thing
    2.4G  rust-target     3 items  destroyed
  392.0K  node-modules    1 item   to the Trash

  Frees 2.4G

  Y confirm  N / Esc cancel
```

One question, two answers, whichever tier the plan holds: `Y` confirms, `N` or
`Esc` cancels. A plan with **anything** destroying in it
What the tier changes is what the modal **says**: a plan holding anything
destroying announces itself in red and names every share as destroyed, because a
subtree usually holds both and the heading is the only place that difference can
be seen before it happens.

**It plans on a worker.** Walking a tree and asking git about every repository
in it takes seconds; doing that on the UI thread would freeze the one screen
whose point is that it does not. `Esc` abandons a plan still being walked, and
costs nothing, because nothing has happened.

`Backspace` is deliberately *not* the key: it means "up one level" today, and a
destructive action must not share a finger with a navigation habit.

**It no longer edits rules either.** The browser used to compose a rule and write
it back into the config with every comment intact; that rested on `toml_edit`,
and the configuration is YAML now. The one lossless YAML editor available
produces wrong indentation on exactly the operation this needed — appending a
mapping to a sequence — and a config editor that can corrupt a config is worse
than no config editor. `R` still re-reads the file, so the loop is: edit in your
editor, press `R`, see the colours change.

## Cleaning up

Two verbs. **`preview` shows, `clean` removes.**

```console
$ disk-tools preview ~/code
   40.0K  pycache        1 candidate   trash
    1.2M  node-modules   1 candidate   trash

Reclaimable: 1.2M

Not touched:
  ~/code/proj/target — uncommitted changes; set requires-clean-repo = false on the rule to include

Preview — nothing was removed. The same line with `clean` removes it.
```

`preview` changes nothing on disk, whatever flags it is given. `clean` takes the
**same flags** and does it:

```console
$ disk-tools clean ~/code
Removed 2 of 2. Freed 1.2M.
```

That symmetry is the point: you act on a preview by retyping the line with the
other verb, so a flag one of them refused would break the copy at exactly the
moment you had decided to act.

Everything goes to the **OS trash** unless a rule or `--purge` says otherwise —
a cleanup tool that is wrong once should cost you a trip to the Trash, not your
data. See [tiers](#three-things-stand-between-a-match-and-a-deletion) and
[purge](#purge).

> **`clean` removes immediately.** There is no `--apply` any more: a flag that
> decided whether a command was destructive is a flag that gets forgotten in both
> directions. What replaced it is the verb — and the one guard that was about the
> *plan* rather than about ceremony is still there, so a bare `clean` takes only
> what your rules say is regenerable and refuses while anything else is in it.

### How much of the report you get

`-d, --depth` decides how far it unfolds, and `0` is the default:

```console
$ disk-tools preview ~/code                 # -d 0: one line per rule
   40.0K  pycache        1 candidate   trash
    1.2M  node-modules   1 candidate   trash

$ disk-tools preview ~/code -d 1            # every candidate
   40.0K  pycache       trash  ~/code/proj/__pycache__
    1.2M  node-modules  trash  ~/code/proj/node_modules

$ disk-tools preview ~/code -d 2            # and inside each one
    1.2M  node-modules  trash  ~/code/proj/node_modules
  890.0K    .bin/
  310.0K    typescript/
```

Level 0 is what "what would this take" asks of a whole run before it asks it of
any one path — and it is how a rule set that has grown a mistake shows it in three
lines rather than in nine hundred. Level 2 and deeper answer "why is this four
gigabytes", which never changes a decision, since a candidate is removed whole.

`--sort name` (the default) or `--sort size` orders the rows. Equal sizes are
ordered by path, so two runs over one unchanged disk print identically and can be
diffed.

All of this is **display only**. Nothing it hides is anything `clean` will spare —
for that, see [`--min-size`](#cutting-the-noise) and `--safe`.

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

### Three things stand between a match and a deletion

**1. The never-touch denylist.** `/System`, `/Library/Caches` (no tilde),
`/Windows`, `/Program Files`, `~/Library/Application Support`, `%APPDATA%` — and
anything inside them. **No flag, no rule and no tier overrides this**, `purge`
included. Note the tilde: `~/Library/Caches` is a candidate, `/Library/Caches` is
not.

**2. Three tiers.** Each rule says what `clean` does with what it claims:

| `tier` | What `clean` does | `--safe` admits |
|--------|-------------------|-----------------|
| `purge` | destroys it — no confirmation, and no trash | yes |
| `trash` | moves it to the OS trash | yes |
| `confirm` (unstated default) | nothing, until `--yes` | no |

All three answer one question, which is why they are one field. `purge` is
`trash` plus "no undo" — the same claim of regenerability, made harder — so
**`--safe` keeps it**: that flag drops what needs confirming, and purge needs
none.

`purge` earns its place because the trash does not free space until it is
emptied. Moving 20 GB of `node_modules` into it leaves a full disk full and adds
a second, manual step; for what a single command regenerates, the trash is not a
safety net but a chore.

> **A rule you write sets its own tier, and anything but `confirm` is a claim the
> tool cannot check.** For the built-ins, `trash` is this project's word that the
> content comes back from a build or a lockfile install. For a rule in your config
> file it is *your* word, and `clean` will act on it without asking. If you are
> unsure, leave `tier` out — an unstated tier is `confirm`.

**3. The git guard.** Build output belonging to a project with uncommitted
changes is left alone — you may be mid-work, and it only regenerates identically
from committed source. It is the rule's own `requires-clean-repo`, so turning it
off is a line in the file next to the rule it guards; there is no flag, because a
global switch would be a coarser duplicate of a setting that was already there.
If `git` is missing or the repository cannot be read, the answer is "dirty": when
the tool cannot know, it does not delete.

Exclusions are always reported with a reason, and the two reasons are not
interchangeable: one is a setting you chose and can unchoose, the other is
absolute.

### The refusal

`clean` prints the plan to stderr first, so the last thing you see before a
deletion is the list of what is about to go. A partial failure exits non-zero and
names every path still on disk.

**It stops while anything not regenerable is in the plan:**

```console
$ disk-tools clean ~/code --older-than 90d -d 1
   40.0K  pycache       trash    ~/code/proj/__pycache__
    1.1M  node-modules  trash    ~/code/proj/node_modules
    4.0K  old           confirm  ~/code/proj/notes.txt

Reclaimable: 1.2M

Preview — nothing was removed. The same line with `clean` removes it.

1 candidate is not regenerable, and nothing was removed.
Add --safe to take only the regenerable ones, or --yes to take all of them.
```

It exits **2** and removes nothing at all — not even the two regenerable ones,
since a partial removal you did not ask for is its own surprise. `--safe` drops
what needs confirming and takes the rest; `--yes` takes everything, saying the
count aloud first.

Exit 2 rather than 1, because 1 already means "the removal partly failed" — and a
script that cannot tell *nothing happened, add a flag* from *some things could not
be deleted* has lost the distinction it most needs.

There is no prompt. A real one needs stdin, TTY detection and a story for piped
input, which belongs with the interactive browser; a refusal gives the same
guarantee — nothing that is not regenerable goes without you saying so — for the
price of one flag. Set `require-confirmation = false` in your config if you would
rather the list itself were the confirmation.

### Cutting the noise

A cleanup of a Python project can turn up a hundred and fifty `__pycache__`
directories of a few kilobytes each, burying the two entries that actually
matter. `--min-size` drops them:

```console
$ disk-tools preview ~/code --min-size 1M -d 1
    2.0M  node-modules  trash  ~/code/proj/node_modules

Reclaimable: 2.0M

150 more candidates are below --min-size.
```

A rule can carry its own `min-size`, and the report says which threshold applied
— "below `--min-size`" or "below their rule's own min-size". The remedies differ:
one is a flag on this command line, the other a line in a file you have to go and
find.

**This narrows the plan, not just the display.** `scan --min-size` hides rows
while the totals stay whole; here the report *is* the list of what `clean` will
remove, so showing two entries and removing a hundred and fifty would be exactly
the mismatch every other rule here exists to prevent. That is the difference
between this flag and `-d`: one changes the answer, the other only how much of it
you are shown. The count of what was dropped is printed, and it is a separate line
from `--safe`'s — the two have different remedies.

### Purge

The trash is not free. On macOS every removal is an `osascript` round-trip to
Finder — **~230 ms per call**, whatever the size of what is being removed — so a
tree of many small `__pycache__` directories used to take minutes. Removals are
now sent in **one batch**, which brought 60 such directories from 14 s to 1.4 s.

And the trash frees no space at all until it is emptied, which for content a
single command regenerates makes it a chore rather than a safety net.

Two ways past it. **Per rule**, which is the one to reach for:

```yaml
clean-rules:
  - name: node-modules
    tier: purge            # regenerated by one command; the trash adds nothing
    parts:
      - root: "~/Projects"
        includes: ["**/node_modules/"]
```

**Or for one run**, over everything in the plan:

```console
$ disk-tools clean ~/code --purge
3 candidates are being deleted outright — NOT to the trash, and cannot be put back.
Removed 3 of 3. Freed 2.0M.
```

**Either way it deletes permanently.** No trash, no "Put Back", nothing to
recover. The same 60 directories take 0.02 s instead of 1.4.

The rule is the safer of the two: it applies to exactly what you wrote it about,
while the flag takes the whole plan. And a mixed run says which half is which:

```
Removed 4 of 4. Freed 3.2M.
  1.2M in the trash, recoverable; 2.0M destroyed, not.
```

`--purge` does **not** cancel the confirmation. It decides where a candidate
goes, not whether you were asked about it, so `clean --purge` still refuses while
anything in the plan needs confirming. And the denylist is untouched by both.

Use either for build output and caches you would not miss; leave the default for
anything you would.

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

## Duplicates

`--dup` changes what a candidate **is**: not what a rule claims, but a file whose
contents are byte-for-byte identical to another's. Everything else is the same —
the same two verbs, the same denylist, the same refusal.

```console
$ disk-tools preview --dup ~/Downloads
Each group keeps one copy. The size is what removing the others frees.

   60.8M  ×3  keeps /Users/me/Downloads/IMG_1826.MOV
   57.5M  ×3  keeps /Users/me/Downloads/IMG_1829.MOV
   23.9M  ×4  keeps /Users/me/Downloads/IMG20250831181435.jpg

Reclaimable: 1.6G — 288 copies in 247 groups.

Preview — nothing was removed. The same line with `clean` removes it.
```

`-d 1` names every path and why the keeper is the keeper:

```console
$ disk-tools preview --dup -d 1 ~/Downloads
   60.8M  ×3  30.4M each
  keep    /Users/me/Downloads/IMG_1826.MOV   (created 4mo)
  remove  /Users/me/Downloads/ChatExport_2026-04-05/files/IMG_1826.MOV
  remove  /Users/me/Downloads/ChatExport_2026-06-03/files/IMG_1826.MOV
```

**With no path, your `duplicate-rules:` say where to look** — exactly as the
clean rules do for an ordinary run. The shipped rule is unrooted, so a bare
`preview --dup` has nowhere to go and says which list to edit.

### Which copy stays

One per group, chosen by `--keep`:

| Value | Keeps |
|---|---|
| `oldest-created` | the copy that existed first. **Default** |
| `newest-created` | the most recently made copy |
| `oldest-modified` | the earliest modification time |
| `newest-modified` | the latest |
| `first` | the first path in byte order. Reads no metadata, so it can never degrade |

Creation time is the default because it is the one that survives copying: a copy
is a new inode with a new creation time, while `cp -p`, `rsync` and unpacking an
archive all carry the *modification* time of the original onto the copy. Sorting
by path was tried and rejected on live data — it keeps `IMG (1).jpg` over
`IMG.jpg`.

`--keep-in <PATH>` beats `--keep` outright, and can be repeated:

```console
$ disk-tools preview --dup --keep-in ~/Photos ~/Downloads
```

The first of those roots that any copy in a group lies under wins, and `--keep`
then chooses among the copies inside it. **It is not a promise that nothing under
that root is removed**: a group keeps exactly one copy, so three copies inside
`~/Photos` still lose two.

### Where a date is missing

`created()` is not recorded by every filesystem — Linux without `statx` birth
times has none at all. The rules **degrade rather than mislead**:

- a copy whose date the platform did not record never wins;
- a group where *no* copy has the date asked for falls back to the other date,
  and then to the path;
- every such group is marked `(*)`, and the report says how many there were.

So a run under `--keep oldest-created` on a filesystem without creation times
still produces a plan, and still says that it is not the plan you asked for.

### What is not searched

- **Files below `--min-size`, which defaults to 1 MiB here** (it is 0 for a
  rule-based run). This is the flag that decides how much is *read*: below it a
  file is never opened. Dropping it to `0` on one real `~/Downloads` cost 5.6×
  the hashing to find 5% more space — see the
  [measurements](kb/benchmarks/2026.07/2026.07.30-duplicate-cost.md).
- **Anything inside a directory your clean rules claim.** A `node_modules` is a
  duplicate farm, and removing one file out of one breaks a tree that should have
  gone wholesale. There is no flag to turn this off.
- **Anything no duplicate rule matches**, and anything a matching rule excludes —
  `.git` by default. See [Where duplicates are looked for](#where-duplicates-are-looked-for).
- **Hardlinks to each other.** Two names for one inode free nothing when one goes,
  so they are not duplicates. A separate file with the same contents still is.
- **Symlinks, empty files**, and anything that changed size between the scan and
  the hash.

`--older-than` is refused here rather than accepted. It works by adding a rule,
and rules only prune under `--dup` — so it would have meant *exclude everything
older than this from the search*, which is the opposite of what it says
everywhere else.

### How it decides two files are identical

Files are bucketed by size — a unique size is proof of unique content, and it
costs nothing, since the scan already measured it. Buckets with more than one
member get an xxh3-128 of their first 16 KiB, and whatever still collides gets a
**blake3 hash of the whole file**. There is no byte-for-byte pass after that: a
collision in a 256-bit cryptographic digest is not a thing that happens, and
doubling every read to defend against it would be a real cost against an
unreachable one.

A file that cannot be read drops out of its group **and is reported**. If that
leaves fewer than two members, the group disappears: a copy is never offered for
removal against content the tool could not read.

### Removing them

Every duplicate is `confirm`-tier, always — the copy that stays is a judgement
call, and no rule can make it for you. So:

```console
$ disk-tools clean --dup ~/Downloads
…
288 candidates are not regenerable, and nothing was removed.
Add --safe to take only the regenerable ones, or --yes to take all of them.
```

`--yes` is what takes them, and `--safe --dup` plans nothing at all, which is
correct: `--safe` means *only what needs no confirmation*, and a duplicate never
qualifies.

## Configuration

`disk-tools` reads one file. Write the defaults out and edit them:

```console
$ disk-tools config init
/Users/you/.config/disk-tools/config.yml
```

It refuses to overwrite an existing file without `-f` / `--force`, and the path it
prints is the one it read: `$XDG_CONFIG_HOME/disk-tools/config.yml` when that is
set — on **every** platform, since exporting it is you saying where your
configuration lives — otherwise `%APPDATA%\disk-tools\config.yml` on Windows and
`~/.config/disk-tools/config.yml` elsewhere. `--config <PATH>` overrides all of
it.

**No config file is an ordinary state**: the built-in rules apply and nothing is
reported. A `--config` path that is *not there* is an error, because that is a
typo and defaults quietly substituted for it would leave you cleaning under rules
you never wrote.

> **Two spellings changed in v0.7, and both are refused by name rather than
> ignored.** `rules:` is now `clean-rules:`, and a rule is a list of `parts:`;
> `requires-sibling` is now `requires`. An unfamiliar key here is a *warning*, so
> a stale file would otherwise stop applying without a word while `clean` ran on
> the built-in rules — which is the one outcome worth stopping for.
>
> **The file was TOML until v0.6.** A `config.toml` sitting where `config.yml`
> should be is refused the same way.

The examples below are lifted from what `config init` writes; the comments in
that file are the documentation, and this section is the short version.

### A rule is a list of parts

```yaml
clean-rules:
  - name: dotnet-output          # what the report calls it
    tier: purge                  # what happens to what it claims
    parts:
      - root: "~/Projects"       # "*" means wherever the scan goes
        includes: ["**/bin/", "**/obj/"]
        requires: ["*.csproj"]
      - root: "~/work"
        includes: ["**/bin/"]
        requires: ["*.fsproj"]
        older-than: 90d
```

**A node is claimed when it satisfies any one part.** The rule carries its
identity and its consequence; a part carries everything that decides whether an
object qualifies — `root`, `includes`, `excludes`, `requires`,
`requires-clean-repo`, `older-than`, `min-size`.

That split is what the shape is for. `requires` is matched **all**, so one part
listing both `*.csproj` and `*.fsproj` demands *both* beside the node. Two parts
ask two questions, and either answer claims it — under one name, one tier and one
row in the report.

There is no shorter spelling. Two ways to write one thing would have to be
answered for in every example, every error message and every field added later;
two lines per rule is the cheaper side of that.

| In a part | |
|---|---|
| `root` | required. `"*"` means wherever the scan goes. `~`, `%LOCALAPPDATA%` and `%APPDATA%` expand from your environment — and **a token that cannot be resolved drops that part**, never widens it |
| `includes` | globs relative to `root`. A trailing `/` means **directory only**, as in gitignore: `**/*.pyc` matches files, `**/node_modules/` does not match a file of that name. **`*` stops at a separator, `**` crosses one** — `*/` is the direct children of `root`, `**/` is everything under it |
| `excludes` | matched by `includes`, but left alone |
| `requires` | paths relative to the directory holding the match, each of which must find something. `Cargo.toml` is the file beside it; `src/main.rs` descends from there. Globs, because build systems name their marker after the project. Matched **all** |
| `requires-clean-repo` | skip a match whose repository has uncommitted work |
| `older-than`, `min-size` | this part only, so "old `bin/`, any `obj/`" is sayable |

**No pattern may leave its root.** `..` is refused wherever it appears — it is
the only way a pattern could escape, and every way it could is a mistake that
would otherwise be silent: the glob simply never matches and the part stops
claiming anything.

`min-size` on a part narrows the plan just as `--min-size` does, and the report
says which of the two applied. They are different things to go and change.

### Where duplicates are looked for

`duplicate-rules:` has the same parts, and they mean something else:

```yaml
duplicate-rules:
  - name: photos-and-their-copies
    keep-in: ["~/Photos"]
    parts:
      - root: "~/Photos"
        includes: ["**"]
      - root: "~/Downloads"
        includes: ["**"]
```

| | a part contributes |
|---|---|
| a **clean** rule | matchers. Anything satisfying any part is claimed, independently |
| a **duplicate** rule | a population. Everything satisfying any part is **pooled**, and copies are compared *within that pool* |

So adding a part to a clean rule adds candidates; adding one here can **create
groups that did not exist**, because two populations can now pair. The example
above is exactly that: a photo in the library and its copy in the downloads are
comparable only because one rule covers both.

**A file belongs to one pool: the first rule, in list order, whose parts match
it.** Overlapping pools are not an ambiguity but a corrupt plan — one would name
a file its keeper while another listed it for removal. The cost is that two
*separate* rules never compare their files with each other, so the report always
says what it searched:

```console
Searched 2 pools of 4120 files, and copies are only ever compared within one:
  photos  4000 files
  scans    120 files
```

A duplicate rule also carries `keep` and `keep-in` — per area, which is what
made the flags worth having — and a `tier` that may be `trash` or `confirm`.
**`purge` is refused here**: for a clean rule it means "one command regenerates
this", and nothing regenerates a copy. The other copy is the only thing that
makes removal safe, and the trash is the only way back from a keeper chosen
wrongly. The `--purge` flag still applies — typed by hand, like `--yes`.

The shipped rule searches everywhere except `.git`. Git LFS stores its objects
**verbatim**, so every tracked file pairs with its own object; neither side may
go, and the exclusion is data rather than a hardcoded skip so you can see and
edit it.

> **A rooted pool is worth more than any tuning.** Narrowing a search of
> `~/Projects` to one subdirectory took it from 49.8 GiB read in 13.6 s to
> 125 MiB in 0.38 s. [Measured.](kb/benchmarks/2026.08/2026.08.01-duplicate-rules-cost.md)

### Running with no path

```console
$ disk-tools preview
disk-tools: examining /Users/you
```

With no `<PATH>`, both verbs walk the roots of your enabled rules, merged so none
contains another. The default config roots `user-caches` at your home directory,
so a bare invocation walks all of it.

> **Try that one with `preview` first.** A bare `clean` walks your whole home and
> then removes everything your rules claim in it, which with the shipped rules is
> every `node_modules`, `target/` and cache under it. That is what the verb says
> it does, and it is recoverable from the trash — but it is not what most people
> mean to type while exploring. Narrow the rule's `root`, or name a path.

If nothing names a directory it says so rather than reporting an empty plan:

```console
$ disk-tools preview
disk-tools: no rule names a directory to clean.
Pass a path, or give a rule a `root` other than "*".
```

### What the file cannot do

| Not configurable | Why |
|------------------|-----|
| **The denylist** | A denylist a config can edit is not a denylist. `/System`, `/Windows`, `/Library/Caches` and the rest stay in the binary, and no rule, flag, tier or file reaches them |
| `--purge` as a global | Sending a *whole plan* past the trash stays an explicit choice on one run. Per rule it is `tier = "purge"`, which applies to exactly what you wrote it about |
| `--yes` | A file answering yes in advance cancels the confirmation, and cancels it invisibly |
| A global git-guard switch | It belongs to the rule that wants it: `requires-clean-repo`. A second, coarser way to turn it off would only be a way to turn it off by accident |

One limitation worth knowing: a true/false setting turned **on** in the file
cannot be turned back off from the command line, because a flag can only be
passed or not passed. For `clean.safe` that direction is the right one — the
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
| `--explain` | Say what this command would do — the file it read, the rules in force and the ones dropped and why, where it would look, where things would go, and whether it would stop — then exit **without walking, reading or removing anything**. **Global** | — |
| `-h`, `--help` / `-V`, `--version` | Print help / version | — |

`disk-tools preview [PATH]` and `disk-tools clean [PATH]` take **the same set** —
that is what lets you act on a preview by changing the first word. **The path is
optional**; without it the roots of your configured rules are walked, see
[Configuration](#running-with-no-path).

| Flag | Effect | Changes |
|------|--------|---------|
| `--dup` | Look for **duplicate files** instead of what the clean rules claim. Without a PATH it walks the roots of your `duplicate-rules:`. No config key — a file that switched what `clean` removes would do it invisibly | the plan |
| `--keep <RULE>` | Which copy of a group stays: `first`, `oldest-created`, `newest-created`, `oldest-modified`, `newest-modified`. **Overrides every rule's own.** Requires `--dup` | the plan |
| `--keep-in <PATH>` | Prefer to keep copies under this path; repeatable, earlier wins, beats `--keep`. Replaces a rule's own list rather than extending it. Requires `--dup` | the plan |
| `--safe` | Drop everything that needs confirming. Keeps `purge` and `trash` — it is about confirmation, not about destinations | the plan |
| `--min-size <SIZE>` | Ignore anything smaller. **Narrows the plan, not just the printout** — unlike `scan`'s flag of the same name, what is shown is what `clean` removes. Defaults to 0, or to **1 MiB under `--dup`**, where it decides how much is read | the plan |
| `--older-than <DURATION>` | Also offer anything untouched for this long: `90d`, `2w`, `6m`, `1y`. A bare number is rejected — `90` could mean seconds as easily as days. `m` is 30 days, `y` is 365. **Refused with `--dup`**, where it would mean the opposite: it works by adding a rule, and under `--dup` the rules only prune | the plan |
| `--purge` | Send the **whole plan** past the trash. Nothing can be put back. Per rule this is `tier = "purge"`; neither cancels the confirmation | the plan |
| `--yes` | Also remove what needs confirming. Without it `clean` refuses while any `confirm`-tier candidate remains, and exits 2 | the plan |
| `-d`, `--depth <N>` | `0` groups by rule (default), `1` lists candidates, `2`+ unfolds inside them. Under `--dup`: `0` is one line per group, `1`+ names every path, and there is nothing to unfold past that | the display |
| `--sort <KEY>` | `name` or `size`. Largest first, ties broken by path. Defaults to `name`, and to **`size` under `--dup`**, where the list is hundreds of groups and only the top of it is a decision | the display |
| `--json` | The whole plan (`preview`) or the whole outcome (`clean`). Ignores `-d` and `--sort` | the display |

`preview` accepts every one of them and still changes nothing — including
`--purge` and `--yes`, which do nothing there. A flag it refused would break the
copy at exactly the moment you had decided to act.

**Display flags filter what is printed, never what is counted.** In `scan`, a
directory's size is always its full subtree, exactly like `du`: hiding a 400 MB
child does not shrink its parent's number. In `preview` and `clean`, nothing `-d`
hides is anything `clean` will spare.

`--json` always emits the **full** answer with raw byte counts, and no display
flag can change a byte of it. `preview --json` is a plan and `clean --json` is an
outcome — two documents, told apart by the fields they have and by the exit code.

`disk-tools ui [PATH]`:

| Flag | Effect |
|------|--------|
| `[PATH]` | Directory to open. Optional — defaults to the current directory, which is the one case where guessing is what you meant |
| `--config <PATH>` | The config file to read the rules from, and to write them back to |

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

**The duplicate search reads real bytes**, which nothing else here does. Bucketing
by size keeps that off most of a tree — on one real `~/Downloads`, 61% of eligible
files have a unique size and are never opened, and 9% of the eligible bytes are
read. A cold run is bound by the disk at ~850 MiB/s rather than by blake3, which
moves 3–4 GiB/s warm. A project tree is the adversarial case: 89% of files there
share a size with something, and the funnel narrows almost nothing.
[Measured.](kb/benchmarks/2026.07/2026.07.30-duplicate-cost.md)

Cleanup adds one cost worth knowing: the git guard spawns `git status` once per
repository, measured at **~23 ms each**, which dominates a `preview` over a tree of
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

Cleanup, first — these are the ones worth reading before your first `clean`:

| Limitation | Detail |
|------------|--------|
| **On Windows, one failure mode cannot be caught** | The `trash` crate calls `CoCreateInstance(...).unwrap()` on its Windows delete path. If COM cannot be initialised — a service, a session-0 process, some sandboxes — it **panics** rather than returning an error, aborting the run mid-way. No wrapper here can convert a panic, so "the summary names what survived" holds for every failure the backend *reports*, and not for that one. `just smoke-trash` runs on all three platforms in CI so a COM-hostile environment shows up as a red build. |
| **On macOS, recoverability cannot be verified by a test** | `~/.Trash` needs Full Disk Access and the crate offers no way to list it there, so an automated test can assert only that the original path is gone — which an unrecoverable delete would also satisfy. It rests on the crate's documented behaviour and one manual check. Linux and Windows can be checked properly. |
| **`(shared)` is not detectable across the scan boundary on Windows** | See [Reclaimable is an upper bound](#reclaimable-is-an-upper-bound). Absence of the flag there is not evidence of unshared content. |
| **Removing to the trash costs ~230 ms per batch on macOS** | Every trash operation is an `osascript` round-trip to Finder. Removals are batched into one call, so the cost is per *run* rather than per candidate — 60 small directories take 1.4 s, against 0.02 s past the trash. |
| **The trash frees nothing until it is emptied** | A `clean` that only trashes has moved the problem, not solved it. On a disk that is actually full, `tier = "purge"` on the rules whose content a command regenerates is what recovers the space. |
| **Planning costs ~23 ms per repository** | The git guard spawns `git status --porcelain` once per repository. Over 50 dirty Rust projects a `preview` is 1.2 s against 15 ms for a plain scan — 80×. It runs only for rules that set `requires-clean-repo`. [Measured.](kb/benchmarks/2026.07/2026.07.25-clean-latency.md) |
| **`clean` refuses rather than prompting** | For the `confirm` tier it stops and asks you to add `--safe` or `--yes`, rather than asking about each path. A real prompt needs stdin, TTY detection and a story for piped input, which belongs with the interactive browser. See [The refusal](#the-refusal). |
| **A bare `clean` removes** | It walks your rules' roots and takes what they claim. That is what the verb says, and it goes to the trash, but the shipped config roots `user-caches` at your home directory — so `preview` is the one to explore with. See [Running with no path](#running-with-no-path). |
| **The browser writes your config file** | `a` then `↵` edits `config.yml` in place. Comments survive and neighbouring rules are untouched, but it is a program editing a file you may also be editing — `R` re-reads, and there is no merge. See [The browser writes your config file](#the-browser-writes-your-config-file). |
| **Row colours do not consult `requires-clean-repo`** | `included` means the rule's patterns and its cheap predicates match. Whether `clean` will actually act also depends on the git guard, which costs a `git status` per repository and is not run per row. A yellow row inside a dirty repository is still refused by `clean`, with a reason. |
| **`--dup` does not compare bytes, it compares blake3 hashes** | Two files are called identical when a 256-bit cryptographic digest of each agrees. There is no final `memcmp`, which would double every read to defend against something that does not happen. |
| **`--keep-in` does not protect a directory** | It decides which copy a group *keeps*, not which copies are safe. Three copies inside `~/Photos` still lose two. See [Which copy stays](#which-copy-stays). |
| **A duplicate search can be slow and reads your files** | It is the only thing here bounded by disk throughput. `--min-size` is the control, and it defaults to 1 MiB for that reason. |
| **A tier you wrote is unverified** | `trash` and `purge` both say "this regenerates", and the tool takes your word for it: it removes without asking, and `purge` without a way back. For the built-in rules that word is this project's; for yours it is yours. An unstated tier is `confirm`. See [Three tiers](#three-things-stand-between-a-match-and-a-deletion). |

And the scanner's, unchanged from v0.1:

| Limitation | Detail |
|------------|--------|
| **APFS copy-on-write clones are overcounted** | On macOS, a cloned file (`cp -c`, Finder duplicate, many build tools) shares its blocks with the original, but each copy reports its *full* allocated size. A tree of clones therefore sums well above what deleting it would actually reclaim. Detecting shared extents needs per-file `fcntl` probing and is out of scope for v0.1. |
| `--one-file-system` does nothing on Windows | Mount-boundary detection needs a device id per *candidate directory*; the walk has the volume serial of the directory it is listing, not of the subdirectory it is about to enter. The flag is accepted and silently has no effect off Unix. |
| Long UNC paths may be skipped on Windows | Affects only the per-file fallback used when the directory listing does not cover an entry: there, only `C:\`-style drive paths get the `\\?\` prefix that lifts the `MAX_PATH` limit, so a longer `\\server\share\…` path can end up skipped. |
| `scan` holds the whole tree in memory | An accepted trade-off: directory totals need it. Costs **≈ 630 bytes per entry** — 1.4 GB for a 2.2M-entry scan (measured, see [Benchmarks](#benchmarks)). The browser does **not** work this way; it lists one directory at a time and keeps only the totals it has computed, which is a path and a `u64` per directory visited. |

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
| `just lint-windows` | The same against `x86_64-pc-windows-msvc`. Clippy only sees what the host compiles, so `#[cfg(windows)]` code is otherwise linted by CI alone — which is how one lint reached `main` |
| `just verify` | Pre-commit gate: `fmt-check` + `lint` + `lint-windows` + `doc` + `check-minimal` + `test` |
| `just smoke-trash` | The `#[ignore]`d tests that move real files to the OS trash |
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
