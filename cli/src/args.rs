//! Command-line surface: parse arguments into a [`ScanOptions`] for the core.
//!
//! `-n`/`--number` and `--json` are the CLI's own concerns (display count and
//! output format), not [`ScanOptions`] fields — they are declared here so
//! `--help` lists them, and consumed by the renderer (Task 7) and JSON output
//! (Task 8).

use crate::config::Config;
use clap::{Parser, Subcommand};
use disk_tools_core::{
    CleanOptions, DetectOptions, RuleError, Rules, ScanOptions, UserDirs, age_rule,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// disk-tools — find what's eating your disk.
#[derive(Parser, Debug)]
// Three verbs, no privileged one. `disk-tools <PATH>` used to scan, which made
// scanning the default and everything else a subcommand — readable while there
// was one other verb, lopsided once there are three. A bare `disk-tools` now
// prints help and exits 2, which keeps the guarantee that mattered about the old
// shape: the working directory is never scanned by accident.
#[command(version, about, arg_required_else_help = true)]
pub struct Args {
    /// List every skipped entry instead of just the first ten.
    ///
    /// Global rather than per-command: it is about diagnostics, not about any
    /// one report.
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,

    /// Read this file instead of the one in your config directory.
    ///
    /// Global rather than per-command so that `config init` can be pointed at a
    /// path too — otherwise the one verb that writes the file would be the one
    /// verb unable to say where.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Measure a directory and print a size-sorted tree.
    Scan(ScanArgs),

    /// Show what `clean` would remove. Changes nothing on disk.
    ///
    /// Takes exactly the flags `clean` does, so acting on what you see means
    /// retyping the line with the other verb.
    Preview(CleanArgs),

    /// Remove what the rules claim, to the OS trash. Removes immediately.
    ///
    /// There is no --apply: the verb is the intent. Each rule says what happens
    /// to what it claims — purge destroys, trash recovers, confirm waits for
    /// --yes — and clean refuses, removing nothing at all and exiting 2, while
    /// anything needing confirmation is in the plan. Use --safe to drop those,
    /// or --yes to take them.
    Clean(CleanArgs),

    /// Browse a directory, with each entry coloured by what the rules say.
    Ui(UiArgs),

    /// Inspect and create the configuration file.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(clap::Args, Debug)]
pub struct UiArgs {
    /// Directory to open. Defaults to the working directory.
    ///
    /// The only place in this tool where the working directory is implied. The
    /// rule against it exists so that nothing huge is *scanned* by accident;
    /// opening a browser costs one directory listing, so the reason does not
    /// carry. `scan` still demands a path and `clean` asks the rules.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Write the default configuration, comments and all, so it can be edited.
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
}

#[derive(clap::Args, Debug)]
pub struct ScanArgs {
    /// Directory (or file) to scan. Always explicit — never defaults to the current directory.
    #[arg(value_name = "PATH")]
    pub root: PathBuf,

    // Every field below that a config file can also set is an `Option`, and
    // none of them carries a clap `default_value`. That is not tidiness: with a
    // default applied here, `--min-size 0` and an absent `--min-size` arrive
    // identical, and the first has to beat the file while the second defers to
    // it. Defaults are applied by the merge in `resolve`, and stated in the doc
    // comments — a flag whose default is written down nowhere is undocumented.
    /// Show at most this many entries. Default: all of them.
    #[arg(short = 'n', long = "number")]
    pub number: Option<usize>,

    /// Hide entries below this size, e.g. 1M, 512K, 2G (1024-based). Default: 0.
    #[arg(long = "min-size", value_parser = parse_size)]
    pub min_size: Option<u64>,

    /// Print at most this many levels deep (display only; traversal is always full).
    ///
    /// 0 shows the root alone. Default: unlimited.
    #[arg(short = 'd', long)]
    pub depth: Option<usize>,

    /// Rank and report apparent size rather than allocated size.
    ///
    /// A config file can turn this on; the command line cannot turn it back off.
    #[arg(long)]
    pub apparent: bool,

    /// Stop at filesystem boundaries instead of descending into other mounts.
    ///
    /// A config file can turn this on; the command line cannot turn it back off.
    #[arg(long = "one-file-system")]
    pub one_file_system: bool,

    /// Emit JSON instead of the tree report.
    #[arg(long)]
    pub json: bool,
}

/// The flags `preview` and `clean` share.
///
/// **One struct, both verbs, deliberately.** Preview is used by retyping the
/// same line with the other verb, so a flag either verb did not accept would
/// break the copy at exactly the moment the user had decided to act. Where a
/// flag has nothing to do under `preview` it is still accepted and does nothing.
#[derive(clap::Args, Debug)]
pub struct CleanArgs {
    /// Directory to examine. Without one, the roots of your configured rules.
    ///
    /// Omitting it is not the same as `.` — nothing here ever falls back to the
    /// working directory. With no path and no rooted rule there is nothing to
    /// examine, and the tool says so rather than guessing.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Drop everything that needs per-item confirmation.
    ///
    /// Keeps both of the other tiers: purge is a stronger claim that something
    /// regenerates than trash, not a weaker one, so this is about confirmation
    /// and not about destinations.
    ///
    /// A config file can turn this on; the command line cannot turn it back off
    /// — which for this one is the right direction, since the file may only make
    /// a cleanup more cautious.
    #[arg(long)]
    pub safe: bool,

    /// Delete the whole plan outright instead of trashing. Nothing can be put back.
    ///
    /// Per rule this is tier = "purge", which applies to exactly what that rule
    /// claims. Neither cancels the confirmation: this decides where a candidate
    /// goes, not whether you were asked about it.
    #[arg(long)]
    pub purge: bool,

    /// Remove candidates that need confirmation too.
    ///
    /// Without it, `clean` stops when the plan holds anything that needs
    /// confirming, removes nothing and exits 2. There is no config key for this:
    /// a file that answered yes in advance would cancel the confirmation, and
    /// cancel it invisibly.
    #[arg(long)]
    pub yes: bool,

    /// Ignore anything smaller than this, e.g. 1M, 512K (1024-based). Default: 0.
    ///
    /// Unlike the scan flag of the same name this narrows the plan itself, so a
    /// candidate it hides is one `clean` will not remove.
    #[arg(long = "min-size", value_parser = parse_size)]
    pub min_size: Option<u64>,

    /// Also offer anything untouched for this long: 90d, 6m, 1y.
    #[arg(long = "older-than", value_parser = parse_duration, value_name = "DURATION")]
    pub older_than: Option<Duration>,

    // Neither carries a clap `default_value`, for the reason the whole file
    // gives: a value stated explicitly and a flag left off must stay
    // distinguishable, so that a `[report]` key can be added later without
    // changing what an absent flag means. Their defaults are applied in
    // `cleanup` and written down here.
    /// How far the report unfolds: 0 groups by rule, 1 lists every candidate. Default: 0.
    ///
    /// Display only. It never changes the plan, so a candidate a shallow report
    /// does not name is one that will still be removed.
    #[arg(short = 'd', long, value_name = "N")]
    pub depth: Option<usize>,

    /// Order the report by `name` or by `size`. Default: name.
    #[arg(long, value_enum, value_name = "KEY")]
    pub sort: Option<Sort>,

    /// Emit JSON instead of the report.
    ///
    /// The whole plan, or the whole outcome — `-d` and `--sort` do not reach it.
    #[arg(long)]
    pub json: bool,
}

/// What the report is ordered by.
///
/// A value rather than a `--by-size` flag, so that ordering by a timestamp can
/// be added without a second flag meaning the same kind of thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Sort {
    /// By name — the rule's at depth 0, the path's below it.
    Name,
    /// Largest first.
    Size,
}

/// Whether this invocation removes anything.
///
/// The verb, as a value. It replaces v0.2's `--apply` boolean and lives here
/// rather than in the renderer because it is what was *asked for*, and because
/// the report and the removal must not be able to disagree about it: the
/// closing line of a preview promises nothing happened, and printing that
/// before a removal would make the last sentence a user reads the false one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// `preview`: print the plan. Nothing on disk changes.
    Preview,
    /// `clean`: the removal follows immediately.
    Removing,
}

/// How the plan is shown, as opposed to how it is chosen.
///
/// Every field here is display-only, and that separation is the point: a
/// candidate this hides is still one `clean` removes. The narrowing flags —
/// `--safe`, `--min-size`, `--older-than` — live in [`CleanOptions`] instead,
/// because they change the plan itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// 0 groups by rule, 1 lists candidates, 2+ unfolds inside them.
    pub depth: usize,
    pub sort: Sort,

    /// Emit the whole thing as JSON, ignoring the two above.
    ///
    /// They lay a report out for a person; a machine-readable output quietly
    /// shortened by one of them would say nothing about having been.
    pub json: bool,
}

/// What the parsed arguments actually asked for.
#[derive(Debug)]
pub enum Mode {
    Scan {
        options: ScanOptions,
        /// `-n`: display-only, so it never reaches the core.
        number: Option<usize>,
        json: bool,
    },
    Clean {
        /// What to walk: the path if one was given, otherwise the roots of the
        /// enabled rules, already merged so none contains another.
        ///
        /// **Empty means there is nothing to examine** — every rule unrooted,
        /// disabled, or dropped. The caller says so; an empty plan would read as
        /// "nothing to clean", which is a different statement.
        roots: Vec<PathBuf>,

        /// May the removal take candidates that are not regenerable?
        ///
        /// `--yes`, or a config that turned `require-confirmation` off. Resolved
        /// to one boolean because the decision is made once — leaving both
        /// inputs to be re-combined at the point of deletion is how they come to
        /// disagree.
        confirm_tier_allowed: bool,

        /// Whether those roots came from the rules rather than the command line.
        ///
        /// Only then are they worth announcing: a user who named a path knows
        /// where they pointed, and one who did not is entitled to be told before
        /// a walk of their whole home directory begins.
        roots_from_rules: bool,
        /// Boxed: the compiled rule set carries two glob automata, and without
        /// the indirection every `Mode::Scan` — which needs none of it — would
        /// pay for the space anyway.
        clean: Box<CleanOptions>,

        /// Which verb this was. The only thing that differs between them.
        intent: Intent,

        /// How to show the plan. Never how to choose it: nothing in here can
        /// keep a candidate out of the removal, only out of the printout.
        report: Report,
    },
    /// Write the default configuration to `target`.
    ConfigInit { target: PathBuf, force: bool },

    /// Open the browser at this directory.
    Ui {
        root: PathBuf,
        /// The same rules `clean` would apply, so the two verbs cannot disagree
        /// about what is junk.
        rules: Box<Rules>,
        /// Everything needed to build those rules again, for the reload key.
        /// The browser outlives the file it was started from — editing the
        /// config and restarting to see the effect is the loop this removes.
        reload: Box<Reload>,
        now: SystemTime,
    },
}

/// What the frontend resolved before the arguments could be turned into work.
///
/// The core reads no clock, no environment and no config file, so all three
/// arrive from here. Bundled rather than passed loose because they are one
/// thing — the state of the world at the moment the program started — and
/// because two of them are only meaningful together.
#[derive(Debug)]
pub struct Environment {
    pub now: SystemTime,
    pub user_dirs: UserDirs,
    pub config: Config,
    /// Where the config file is or would be. `None` when nothing in this
    /// environment implies a path, which is also the one case `config init`
    /// cannot serve.
    pub config_path: Option<PathBuf>,
}

/// What it takes to read the rules again from scratch.
///
/// A copy of the inputs rather than a closure: `resolve` is a pure function of
/// its `Environment`, and handing the browser a closure over it would make the
/// mode inspectable only by running it.
#[derive(Debug, Clone)]
pub struct Reload {
    /// The file the rules came from, already located. Passed back as the
    /// explicit path so a reload cannot resolve to a *different* file than the
    /// one the browser started with.
    pub path: Option<PathBuf>,
    pub user_dirs: UserDirs,
}

impl Args {
    /// Turn the parsed arguments into what the core needs.
    ///
    /// Fallible again as of v0.3: the rules being compiled are now partly the
    /// user's, so a malformed glob is possible here in a way it was not while
    /// every pattern was a literal in the core. Everything clap can check —
    /// a missing path, `--purge` without `--apply` — is still caught before this.
    pub fn resolve(self, env: Environment) -> Result<Mode, ResolveError> {
        let Environment {
            now,
            user_dirs,
            config,
            config_path,
        } = env;

        match self.command {
            // Flag, then file, then built-in default — the one place the
            // order is expressed, so that no setting can quietly follow a
            // different one.
            //
            // Booleans are the exception and it is a limitation, not a choice:
            // clap distinguishes only "passed" from "not passed", so `||` is
            // the whole of the merge and a file that turns one on cannot be
            // overruled from the command line. Undoing that would take a
            // `--no-…` counterpart per flag.
            Command::Scan(scan) => Ok(Mode::Scan {
                options: ScanOptions {
                    root: absolute(scan.root),
                    min_size: scan.min_size.or(config.report.min_size).unwrap_or(0),
                    depth: scan.depth.or(config.report.depth),
                    apparent: scan.apparent || config.report.apparent.unwrap_or(false),
                    one_file_system: scan.one_file_system
                        || config.scan.one_file_system.unwrap_or(false),
                },
                // `None` all the way down means "every entry", which is what a
                // scan has always printed without `-n`.
                number: scan.number.or(config.report.number),
                json: scan.json,
            }),

            Command::Ui(ui) => Ok(Mode::Ui {
                // Absolute, like every other path here. It was once left as
                // typed so the path line would echo what the user wrote — but a
                // relative `.` is a path no rooted rule can match, so the screen
                // coloured a home full of junk as untracked and said nothing
                // about why.
                root: absolute(ui.path.unwrap_or_else(|| PathBuf::from("."))),
                // No age rule: `--older-than` is a `clean` flag, and a browser
                // that coloured every old file as junk would say nothing.
                rules: Box::new(Rules::new(config.rules, &user_dirs).map_err(ResolveError::Rule)?),
                reload: Box::new(Reload {
                    path: config_path,
                    user_dirs,
                }),
                now,
            }),

            Command::Config {
                action: ConfigAction::Init { force },
            } => config_path
                .map(|target| Mode::ConfigInit { target, force })
                .ok_or(ResolveError::NoConfigPath),

            // One function for both, so that "the two verbs take the same flags
            // and resolve them the same way" is a property of the code rather
            // than of two arms staying in step.
            Command::Preview(clean) => {
                Self::cleanup(clean, Intent::Preview, now, user_dirs, config)
            }
            Command::Clean(clean) => Self::cleanup(clean, Intent::Removing, now, user_dirs, config),
        }
    }

    /// The shared resolution of `preview` and `clean`.
    fn cleanup(
        clean: CleanArgs,
        intent: Intent,
        now: SystemTime,
        user_dirs: UserDirs,
        config: Config,
    ) -> Result<Mode, ResolveError> {
        // The configured rules — the built-ins when the file said nothing —
        // plus the age rule **last** if it was asked for. Order is precedence,
        // so appending it there is what keeps a `target/` reported as build
        // output rather than merely as something old, which is what decides its
        // tier.
        let mut rules = config.rules;
        let clean_settings = config.clean;
        if let Some(older_than) = clean.older_than {
            rules.push(age_rule(older_than));
        }

        let rules = Rules::new(rules, &user_dirs).map_err(ResolveError::Rule)?;
        // A path narrows the walk to itself; the rules still apply within it,
        // and one rooted elsewhere simply matches nothing — `Rules::prunes` sees
        // to that. With no path, the rules say where to look, which is what
        // `root` is required for.
        let roots_from_rules = clean.path.is_none();
        let roots = match clean.path {
            Some(path) => vec![absolute(path)],
            None => rules.scan_roots(),
        };

        Ok(Mode::Clean {
            roots,
            // **True** when nothing says otherwise. The concept asks for
            // confirmation on this tier, and the asymmetry the denylist already
            // states applies: refusing too readily costs a user one extra flag,
            // refusing too rarely costs them data.
            confirm_tier_allowed: clean.yes || !clean_settings.require_confirmation.unwrap_or(true),
            roots_from_rules,
            clean: Box::new(CleanOptions {
                detect: DetectOptions { rules, now },
                user_dirs,
                safe_only: clean.safe || clean_settings.safe.unwrap_or(false),
                purge_all: clean.purge,
                min_size: clean.min_size.unwrap_or(0),
            }),
            intent,
            report: Report {
                // Grouped by rule unless asked for more. The overview is what
                // the question "what would this take" wants first; the list is
                // one keystroke away.
                depth: clean.depth.unwrap_or(0),
                sort: clean.sort.unwrap_or(Sort::Name),
                json: clean.json,
            },
        })
    }
}

/// Why the arguments could not be turned into work.
#[derive(Debug)]
pub enum ResolveError {
    /// A rule's glob does not compile. The user's text, so it names both.
    Rule(RuleError),
    /// `config init` with nothing to say where the file should go — no home, no
    /// `%APPDATA%`, no `XDG_CONFIG_HOME`. Guessing would put a file somewhere
    /// the user would never look for it.
    NoConfigPath,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Rule(err) => write!(f, "{err}"),
            ResolveError::NoConfigPath => write!(
                f,
                "cannot tell where your config directory is; pass --config with a path"
            ),
        }
    }
}

/// Parse a size like `1M` / `512K` / `2G` (1024-based) or a bare byte count.
///
/// Used as a clap `value_parser`, so a malformed value surfaces as a clean
/// usage error rather than a panic.
pub(crate) fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, unit) = s.split_at(split);
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid size `{s}`: expected a number, optionally with K/M/G/T"))?;
    let mult: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1 << 40,
        other => return Err(format!("unknown size suffix `{other}` in `{s}`")),
    };
    value
        .checked_mul(mult)
        .ok_or_else(|| format!("size `{s}` overflows 64 bits"))
}

/// Parse a duration like `90d` / `6m` / `1y`.
///
/// A `value_parser` like [`parse_size`], for the same reason: a malformed value
/// becomes a clean usage error rather than a panic.
///
/// **A bare number is rejected.** `--older-than 90` could as reasonably mean
/// seconds as days, and guessing wrong on the input to a deletion rule is not a
/// guess worth making.
///
/// `m` is 30 days and `y` is 365 — approximations, and stated here because `m`
/// could otherwise be read as minutes. Nothing shorter than a day is offered:
/// this rule exists to find things untouched for a long time.
pub(crate) fn parse_duration(s: &str) -> Result<Duration, String> {
    const DAY: u64 = 24 * 60 * 60;

    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, unit) = s.split_at(split);
    let value: u64 = digits.parse().map_err(|_| {
        format!("invalid duration `{s}`: expected a number with d/w/m/y, e.g. `90d`")
    })?;

    let days: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "d" => 1,
        "w" => 7,
        "m" => 30,
        "y" => 365,
        "" => {
            return Err(format!(
                "duration `{s}` needs a unit: d (days), w (weeks), m (months), y (years)"
            ));
        }
        other => return Err(format!("unknown duration suffix `{other}` in `{s}`")),
    };

    value
        .checked_mul(days)
        .and_then(|days| days.checked_mul(DAY))
        .map(Duration::from_secs)
        .ok_or_else(|| format!("duration `{s}` is too large"))
}

/// The path made absolute, without touching the filesystem.
///
/// **Every path this tool acts on goes through here first.** A rooted rule is
/// compiled against an absolute root — `~` resolves to one — so a relative path
/// produces nodes like `./target` that no such glob can match, and the run finds
/// nothing at all. `preview .` inside a project reported "Nothing to clean"
/// while `preview /full/path` to the same directory reported four gigabytes.
///
/// [`std::path::absolute`], never `canonicalize`. This joins the working
/// directory and drops `.` components **without following a single symlink**,
/// which is the whole reason `canonicalize` is banned here: the path shown has
/// to be the path acted on, and resolving links would report — and remove —
/// somewhere the user never named.
///
/// A failure (an empty path, or no readable working directory) leaves the path
/// as it was. Nothing downstream can succeed on such a path anyway, so it falls
/// to `validate_root` to say so plainly, and the rules see a relative path they
/// cannot match — which claims nothing, the safe direction.
fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

/// Fail before scanning if the root is missing or unreadable, so the user gets
/// a clear error up front instead of an empty scan.
pub fn validate_root(root: &Path) -> Result<(), String> {
    match root.try_exists() {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("path does not exist: {}", root.display())),
        Err(err) => Err(format!("cannot access {}: {err}", root.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("disk-tools").chain(args.iter().copied()))
    }

    /// `config init` resolves to a target, or to the one error that says why it
    /// cannot. Covered end to end already; here so that a change to `resolve`
    /// fails at the unit that owns the decision.
    #[test]
    fn config_init_resolves_to_the_located_path() {
        let mode = parse(&["config", "init"])
            .expect("parse")
            .resolve(env(Config::default()))
            .expect("resolve");

        assert!(matches!(
            mode,
            Mode::ConfigInit { ref target, force: false } if target == Path::new("/cfg/config.toml")
        ));

        let forced = parse(&["config", "init", "--force"])
            .expect("parse")
            .resolve(env(Config::default()))
            .expect("resolve");
        assert!(matches!(forced, Mode::ConfigInit { force: true, .. }));
    }

    /// No home, no `%APPDATA%`, no `XDG_CONFIG_HOME`: there is nowhere to write.
    /// Guessing would put the file somewhere the user would never look for it.
    #[test]
    fn config_init_without_a_known_directory_says_so() {
        let err = parse(&["config", "init"])
            .expect("parse")
            .resolve(Environment {
                config_path: None,
                ..env(Config::default())
            })
            .expect_err("nowhere to write");

        assert!(matches!(err, ResolveError::NoConfigPath));
        assert!(err.to_string().contains("--config"));
    }

    /// Both variants say something a user has to act on, so both are asserted.
    /// Mutation testing found the whole `Display` impl could be replaced with
    /// `Ok(())` — an empty message — with every test still passing.
    #[test]
    fn resolve_errors_say_what_to_do_about_them() {
        let rule = ResolveError::Rule(RuleError {
            rule: "mine".into(),
            pattern: "**/[".into(),
            message: "unclosed".into(),
        })
        .to_string();
        assert!(
            rule.contains("mine") && rule.contains("**/["),
            "a bad glob must name the rule and the pattern the user wrote: {rule}"
        );

        let no_path = ResolveError::NoConfigPath.to_string();
        assert!(
            no_path.contains("--config"),
            "and the one remedy has to be named: {no_path}"
        );
    }

    /// A fixed stand-in for everything the frontend resolves, so no test here
    /// depends on this machine's clock, home or config file.
    fn env(config: Config) -> Environment {
        Environment {
            now: SystemTime::UNIX_EPOCH,
            user_dirs: UserDirs::default(),
            config,
            config_path: Some(PathBuf::from("/cfg/config.toml")),
        }
    }

    /// Parse and resolve in one step, on the built-in rules.
    fn resolved(args: &[&str]) -> Result<Mode, clap::Error> {
        Ok(parse(args)?
            .resolve(env(Config::default()))
            .expect("the built-in rules compile"))
    }

    // ---- precedence: flag > file > default -------------------------------

    /// Resolve `args` against a config file built from `toml`.
    fn against(toml: &str, args: &[&str]) -> Mode {
        let config = crate::config::parse_for_test(toml);
        parse(args)
            .expect("parse")
            .resolve(env(config))
            .expect("resolve")
    }

    fn scan_with(toml: &str, args: &[&str]) -> ScanOptions {
        match against(toml, args) {
            Mode::Scan { options, .. } => options,
            other => panic!("expected a scan, got {other:?}"),
        }
    }

    /// One row per overridable setting, because the whole point is that they all
    /// behave the same way. A setting that quietly followed a different order
    /// would be the bug this task exists to prevent.
    #[test]
    fn every_setting_takes_the_flag_then_the_file_then_the_default() {
        // (setting, config, flag, expected)
        let file = "[report]\nmin-size = \"1M\"\nn = 5\ndepth = 3\n";

        assert_eq!(
            scan_with("", &["scan", "/x"]).min_size,
            0,
            "no file, no flag: the built-in default"
        );
        assert_eq!(
            scan_with(file, &["scan", "/x"]).min_size,
            1_048_576,
            "the file, when the flag is absent"
        );
        assert_eq!(
            scan_with(file, &["scan", "/x", "--min-size", "2M"]).min_size,
            2_097_152,
            "the flag, over the file"
        );

        assert_eq!(scan_with("", &["scan", "/x"]).depth, None);
        assert_eq!(scan_with(file, &["scan", "/x"]).depth, Some(3));
        assert_eq!(
            scan_with(file, &["scan", "/x", "--depth", "1"]).depth,
            Some(1)
        );

        let number = |toml: &str, args: &[&str]| match against(toml, args) {
            Mode::Scan { number, .. } => number,
            other => panic!("expected a scan, got {other:?}"),
        };
        assert_eq!(number("", &["scan", "/x"]), None);
        assert_eq!(number(file, &["scan", "/x"]), Some(5));
        assert_eq!(number(file, &["scan", "/x", "-n", "9"]), Some(9));
    }

    /// The trap this task exists for.
    ///
    /// `--min-size` used to carry `default_value = "0"`, which made an explicit
    /// zero and an absent flag arrive identical. With a file in play they are
    /// opposite statements: one overrides it, the other defers to it. Nothing
    /// short of dropping the clap default can tell them apart.
    #[test]
    fn an_explicit_zero_beats_the_file() {
        let file = "[report]\nmin-size = \"1M\"\n";

        assert_eq!(
            scan_with(file, &["scan", "/x", "--min-size", "0"]).min_size,
            0,
            "the user said zero, so it is zero"
        );
        assert_eq!(
            scan_with(file, &["scan", "/x"]).min_size,
            1_048_576,
            "and saying nothing still defers to the file"
        );
    }

    /// `--depth 0` shows the root alone, so `depth = 0` in the file must mean
    /// the same. Giving one value two opposite meanings is how a `config init`
    /// would have silently collapsed every report to one line.
    #[test]
    fn zero_depth_means_the_root_alone_in_both_places() {
        assert_eq!(
            scan_with("[report]\ndepth = 0\n", &["scan", "/x"]).depth,
            Some(0)
        );
        assert_eq!(
            scan_with("", &["scan", "/x", "--depth", "0"]).depth,
            Some(0)
        );
    }

    /// A file may turn a boolean on; the command line cannot turn it back off,
    /// since clap knows only "passed" and "not passed". Pinned as a limitation
    /// rather than left to be discovered.
    #[test]
    fn a_boolean_set_in_the_file_cannot_be_unset_by_a_flag() {
        let file = "[scan]\none-file-system = true\n\n[report]\napparent = true\n";

        let from_file = scan_with(file, &["scan", "/x"]);
        assert!(from_file.apparent);
        assert!(from_file.one_file_system);

        // There is no flag that turns either back off — which is the limitation.
        let with_flags = scan_with(file, &["scan", "/x", "--apparent", "--one-file-system"]);
        assert!(with_flags.apparent);
        assert!(with_flags.one_file_system);
    }

    #[test]
    fn the_file_can_only_make_a_cleanup_more_cautious() {
        let clean = |toml: &str, args: &[&str]| match against(toml, args) {
            Mode::Clean { clean, .. } => *clean,
            other => panic!("expected a clean, got {other:?}"),
        };

        assert!(!clean("", &["clean", "/x"]).safe_only);
        assert!(clean("[clean]\nsafe = true\n", &["clean", "/x"]).safe_only);
        assert!(clean("", &["clean", "/x", "--safe"]).safe_only);
    }

    /// `--allow-dirty` and `--purge` have no key in the file, and that is the
    /// point: a config that silently disabled the git guard, or deleted past the
    /// trash, would hide the fact that it had.
    #[test]
    fn the_dangerous_flags_have_no_file_counterpart() {
        let warnings =
            crate::config::parse_for_test("[clean]\nallow-dirty = true\npurge = true\n").warnings;

        assert_eq!(warnings, vec!["clean.allow-dirty", "clean.purge"]);
    }

    /// `-n` never reaches the core, so it is read off the resolved mode rather
    /// than off `ScanOptions`.
    fn number_of(args: &[&str]) -> Option<usize> {
        match resolved(args).expect("resolve") {
            Mode::Scan { number, .. } => number,
            other => panic!("expected a scan, got {other:?}"),
        }
    }

    fn scan_options(args: &[&str]) -> ScanOptions {
        match resolved(args).expect("resolve") {
            Mode::Scan { options, .. } => options,
            other => panic!("expected a scan, got {other:?}"),
        }
    }

    /// What a `clean` invocation will walk.
    fn clean_roots(args: &[&str]) -> Vec<PathBuf> {
        match resolved(args).expect("resolve") {
            Mode::Clean { roots, .. } => roots,
            other => panic!("expected a clean, got {other:?}"),
        }
    }

    #[test]
    fn path_maps_to_scan_options_root() {
        assert_eq!(
            scan_options(&["scan", "/some/path"]).root,
            PathBuf::from("/some/path")
        );
    }

    #[test]
    fn missing_path_is_a_usage_error() {
        // A bare `disk-tools` prints help rather than scanning anything. The
        // guarantee is unchanged from when this was a required positional — the
        // working directory is never scanned by accident — but the shape is
        // friendlier: help, and clap's usage exit code.
        let err = parse(&[]).expect_err("a bare invocation must not scan");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(err.exit_code(), 2, "still a usage error, not a success");

        // `scan` still demands one; `clean` no longer does, having the rules to
        // ask instead. Neither ever falls back to the working directory, which
        // is the guarantee that has not moved.
        let err = parse(&["scan"]).expect_err("scan needs a path");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);

        assert!(
            parse(&["clean"]).is_ok(),
            "clean without a path asks the rules where to look"
        );
    }

    #[test]
    fn min_size_suffix_parsing() {
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1M"]).min_size,
            1_048_576
        );
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "512K"]).min_size,
            524_288
        );
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1048576"]).min_size,
            1_048_576
        );
        // Default when the flag is absent.
        assert_eq!(scan_options(&["scan", "/x"]).min_size, 0);
    }

    #[test]
    fn min_size_rejects_garbage() {
        assert!(parse(&["scan", "/x", "--min-size", "1Q"]).is_err());
        assert!(parse(&["scan", "/x", "--min-size", "12x"]).is_err());
        assert!(parse(&["scan", "/x", "--min-size", "abc"]).is_err());
    }

    #[test]
    fn min_size_explicit_zero() {
        assert_eq!(scan_options(&["scan", "/x", "--min-size", "0"]).min_size, 0);
    }

    #[test]
    fn min_size_is_case_insensitive() {
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1m"]).min_size,
            1_048_576
        );
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "2g"]).min_size,
            2u64 << 30
        );
    }

    #[test]
    fn min_size_trims_surrounding_whitespace() {
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", " 1M "]).min_size,
            1_048_576
        );
    }

    #[test]
    fn min_size_g_and_t_multipliers() {
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "2G"]).min_size,
            2u64 << 30
        );
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1T"]).min_size,
            1u64 << 40
        );
    }

    #[test]
    fn min_size_long_unit_aliases() {
        // KiB/MiB/GiB/TiB and their non-"i" long forms must all match the same
        // 1024-based multiplier as the single-letter suffix.
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1KB"]).min_size,
            1 << 10
        );
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1KiB"]).min_size,
            1 << 10
        );
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1MB"]).min_size,
            1 << 20
        );
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1MiB"]).min_size,
            1 << 20
        );
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1GB"]).min_size,
            1 << 30
        );
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1GiB"]).min_size,
            1 << 30
        );
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1TB"]).min_size,
            1 << 40
        );
        assert_eq!(
            scan_options(&["scan", "/x", "--min-size", "1TiB"]).min_size,
            1 << 40
        );
    }

    #[test]
    fn min_size_digit_overflow_is_a_clean_error_not_a_panic() {
        // The digit run alone (~1e20) already exceeds u64::MAX (~1.8e19), so
        // this exercises the `digits.parse()` failure path, distinct from the
        // `checked_mul` overflow below.
        let err = parse(&["scan", "/x", "--min-size", "99999999999999999999T"])
            .expect_err("digit overflow must not panic");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn min_size_multiplication_overflow_is_a_clean_error_not_a_panic() {
        // The digits (2e10) fit comfortably in a u64 on their own; only the
        // `* 1<<40` multiplication overflows, exercising `checked_mul` rather
        // than the digit-parse failure above.
        let err = parse(&["scan", "/x", "--min-size", "20000000000T"])
            .expect_err("multiplication overflow must not panic");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn number_flag_parses_short_and_long() {
        assert_eq!(number_of(&["scan", "/x", "-n", "5"]), Some(5));
        assert_eq!(number_of(&["scan", "/x", "--number", "5"]), Some(5));
        assert_eq!(number_of(&["scan", "/x"]), None);
    }

    #[test]
    fn flags_map_onto_scan_options() {
        let opts = scan_options(&[
            "scan",
            "/x",
            "--depth",
            "3",
            "--apparent",
            "--one-file-system",
        ]);
        assert_eq!(opts.depth, Some(3));
        assert!(opts.apparent);
        assert!(opts.one_file_system);
    }

    /// The scan flags moved under `scan` when it stopped being the default verb,
    /// so this now asserts where they actually live — a flag documented only on
    /// a page the user has no reason to open is not documented.
    #[test]
    fn help_lists_all_flags() {
        use clap::CommandFactory;

        let top = Args::command().render_long_help().to_string();
        for verb in ["scan", "clean"] {
            assert!(
                top.contains(verb),
                "top-level help must list {verb}:\n{top}"
            );
        }

        let scan_help = Args::command()
            .find_subcommand_mut("scan")
            .expect("the scan subcommand exists")
            .render_long_help()
            .to_string();
        for flag in [
            "-n",
            "--min-size",
            "--depth",
            "--apparent",
            "--one-file-system",
            "--json",
        ] {
            assert!(
                scan_help.contains(flag),
                "`scan --help` must mention {flag}, got:\n{scan_help}"
            );
        }
    }

    // ---- the clean subcommand -------------------------------------------

    /// The age rule's threshold, if `--older-than` put one in the list.
    fn older_than_of(options: &CleanOptions) -> Option<Duration> {
        options.detect.rules.get("old")?.older_than
    }

    fn clean_options(args: &[&str]) -> CleanOptions {
        match resolved(args).expect("resolve") {
            Mode::Clean { clean, .. } => *clean,
            other => panic!("expected the clean subcommand, got {other:?}"),
        }
    }

    /// What the verb resolved to, and how far it may go.
    fn intent_of(args: &[&str]) -> Intent {
        match resolved(args).expect("resolve") {
            Mode::Clean { intent, .. } => intent,
            other => panic!("expected a cleanup, got {other:?}"),
        }
    }

    #[test]
    fn both_verbs_parse_their_flags() {
        for verb in ["preview", "clean"] {
            let mode = resolved(&[verb, "/x", "--safe", "--older-than", "90d"]).expect("parse");

            let Mode::Clean { roots, clean, .. } = mode else {
                panic!("{verb} must resolve to a cleanup");
            };
            assert_eq!(roots, vec![PathBuf::from("/x")]);
            assert!(clean.safe_only);
            assert_eq!(
                older_than_of(&clean),
                Some(Duration::from_secs(90 * 24 * 60 * 60))
            );
        }
    }

    /// The verb is the whole difference. Anything else that varied between them
    /// would be a preview describing a run the user is not about to get.
    #[test]
    fn the_verb_is_the_only_thing_that_differs() {
        let flags = ["/x", "--safe", "--purge", "--yes", "--min-size", "4K"];
        let of = |verb: &str| {
            let mut args = vec![verb];
            args.extend_from_slice(&flags);
            match resolved(&args).expect("resolve") {
                Mode::Clean {
                    roots,
                    confirm_tier_allowed,
                    roots_from_rules,
                    clean,
                    intent,
                    report: _,
                } => (
                    intent,
                    (
                        roots,
                        confirm_tier_allowed,
                        roots_from_rules,
                        clean.purge_all,
                        clean.safe_only,
                        clean.min_size,
                    ),
                ),
                other => panic!("expected a cleanup, got {other:?}"),
            }
        };

        let (preview, from_preview) = of("preview");
        let (clean, from_clean) = of("clean");

        assert_eq!(preview, Intent::Preview);
        assert_eq!(clean, Intent::Removing);
        assert_eq!(from_preview, from_clean, "everything else is identical");
    }

    /// Both are gone from the surface entirely. `--apply` because removing is
    /// what `clean` now *is*; `--allow-dirty` because `requires-clean-repo`
    /// settles the git guard per rule, where a global switch would be a coarser
    /// duplicate of it.
    #[test]
    fn apply_and_allow_dirty_are_not_flags_any_more() {
        for verb in ["preview", "clean"] {
            for flag in ["--apply", "--allow-dirty"] {
                let err = parse(&[verb, "/x", flag]).expect_err("must not parse");
                assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
            }
        }
        assert!(
            Rules::builtin(&UserDirs::default())
                .get("rust-target")
                .expect("a built-in")
                .requires_clean_repo,
            "the git guard survives as the per-rule setting that replaced the flag"
        );
    }

    /// The core looks up neither the clock nor the environment, so if these two
    /// do not arrive intact the `user-caches` rule matches nothing and two
    /// denylist entries go unenforced — silently, and only on a real machine.
    #[test]
    fn the_supplied_clock_and_directories_reach_the_core() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_750_000_000);
        let dirs = UserDirs {
            home: Some(PathBuf::from("/home/me")),
            local_app_data: Some(PathBuf::from("/local")),
            app_data: Some(PathBuf::from("/roaming")),
        };

        let mode = parse(&["clean", "/x", "--older-than", "1d"])
            .expect("parse")
            .resolve(Environment {
                now,
                user_dirs: dirs.clone(),
                ..env(Config::default())
            })
            .expect("resolve");

        let Mode::Clean { clean, .. } = mode else {
            panic!("expected the clean subcommand");
        };
        assert_eq!(clean.user_dirs, dirs);
        assert_eq!(clean.detect.now, now);
    }

    /// The bare form must be untouched by the subcommand's arrival — this is the
    /// regression proof for "identical to v0.1".
    #[test]
    fn a_bare_path_is_still_a_scan() {
        assert!(matches!(resolved(&["scan", "/x"]), Ok(Mode::Scan { .. })));
    }

    /// `--purge` stands alone now: there is no `--apply` left for it to require,
    /// and the verb it modifies already removes.
    #[test]
    fn purge_needs_no_companion_flag() {
        assert!(parse(&["clean", "/x", "--purge"]).is_ok());
        assert!(
            parse(&["preview", "/x", "--purge"]).is_ok(),
            "and preview takes it, to show what it would do"
        );
    }

    /// Likewise `--yes`, which used to require `--apply`. It still means only
    /// one thing, and `preview` accepts it so the line can be retyped intact.
    #[test]
    fn yes_needs_no_companion_flag() {
        for verb in ["preview", "clean"] {
            let mode = resolved(&[verb, "/x", "--yes"]).expect("resolve");
            let Mode::Clean {
                confirm_tier_allowed,
                ..
            } = mode
            else {
                panic!("expected a cleanup");
            };
            assert!(confirm_tier_allowed, "{verb}");
        }
    }

    /// Where a candidate goes is settled in the plan, from its rule's tier and
    /// this flag together. All the flag does is say "all of them".
    #[test]
    fn nothing_is_destroyed_without_being_asked_for() {
        for verb in ["preview", "clean"] {
            assert!(
                !clean_options(&[verb, "/x"]).purge_all,
                "{verb} without the flag leaves every rule's tier alone"
            );
            assert!(clean_options(&[verb, "/x", "--purge"]).purge_all, "{verb}");
        }
    }

    /// The verb decides, and nothing else does.
    #[test]
    fn preview_shows_and_clean_removes() {
        assert_eq!(intent_of(&["preview", "/x"]), Intent::Preview);
        assert_eq!(intent_of(&["clean", "/x"]), Intent::Removing);
        assert_eq!(
            intent_of(&["preview", "/x", "--purge", "--yes"]),
            Intent::Preview,
            "no flag can turn a preview into a removal"
        );
    }

    #[test]
    fn older_than_parses_duration_suffixes() {
        const DAY: u64 = 24 * 60 * 60;

        let age = |arg: &str| {
            older_than_of(&clean_options(&["clean", "/x", "--older-than", arg])).expect("age armed")
        };

        assert_eq!(age("1d"), Duration::from_secs(DAY));
        assert_eq!(age("2w"), Duration::from_secs(14 * DAY));
        assert_eq!(age("6m"), Duration::from_secs(180 * DAY));
        assert_eq!(age("1y"), Duration::from_secs(365 * DAY));
        // Case-insensitive, like `--min-size`.
        assert_eq!(age("90D"), Duration::from_secs(90 * DAY));
    }

    #[test]
    fn older_than_rejects_garbage() {
        for bad in ["12x", "abc", "d", "-1d"] {
            assert!(
                parse(&["clean", "/x", "--older-than", bad]).is_err(),
                "`{bad}` must be a usage error"
            );
        }
    }

    /// `90` could mean seconds or days. A deletion rule is not the place to
    /// guess, so the unit is required rather than defaulted.
    #[test]
    fn older_than_rejects_a_bare_number() {
        let err = parse(&["clean", "/x", "--older-than", "90"])
            .expect_err("a bare number must not be accepted");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn older_than_overflow_is_a_clean_error_not_a_panic() {
        let err = parse(&["clean", "/x", "--older-than", "99999999999999999999y"])
            .expect_err("overflow must not panic");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    /// D9: absent flag means the age rule is not in the list at all — not that
    /// it is present with a zero threshold, which would claim everything.
    #[test]
    fn absent_older_than_adds_no_age_rule() {
        let options = clean_options(&["clean", "/x"]);

        assert!(
            options.detect.rules.get("old").is_none(),
            "no --older-than must leave the age rule out of the list entirely"
        );
        assert!(
            options.detect.rules.get("node-modules").is_some(),
            "and the built-ins are still there"
        );
    }

    /// Order is precedence, so the age rule has to be **last** — ahead of the
    /// safe-list rules it would claim a `target/` as merely old, and the tier
    /// would change from auto to confirm with it.
    #[test]
    fn the_age_rule_is_appended_after_the_builtins() {
        let mut rules = disk_tools_core::builtin_rules();
        rules.push(age_rule(Duration::from_secs(1)));

        assert_eq!(
            rules.last().expect("non-empty").name,
            "old",
            "the age rule must sit after every built-in"
        );
    }

    // ---- what `clean` walks ---------------------------------------------

    /// The promise Task 2 made when it required `root` in the file: without a
    /// path, the rules say where to look.
    #[test]
    fn without_a_path_the_rule_roots_are_walked() {
        let config = crate::config::parse_for_test(
            "[[rules]]\nname = \"a\"\nroot = \"/tmp/a\"\nincludes = [\"**/x/\"]\n\n\
             [[rules]]\nname = \"b\"\nroot = \"/tmp/b\"\nincludes = [\"**/y/\"]\n",
        );
        let mode = parse(&["clean"])
            .expect("parse")
            .resolve(env(config))
            .expect("resolve");

        let Mode::Clean {
            roots,
            roots_from_rules,
            ..
        } = mode
        else {
            panic!("expected a clean");
        };
        assert_eq!(
            roots,
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
        assert!(roots_from_rules, "so the caller can announce them");
    }

    /// A path narrows the walk to itself. The rules still apply within it — one
    /// rooted elsewhere simply matches nothing, which `Rules::prunes` decides.
    #[test]
    fn a_path_is_the_only_thing_walked() {
        let config = crate::config::parse_for_test(
            "[[rules]]\nname = \"a\"\nroot = \"/tmp/a\"\nincludes = [\"**/x/\"]\n",
        );
        let mode = parse(&["clean", "/elsewhere"])
            .expect("parse")
            .resolve(env(config))
            .expect("resolve");

        let Mode::Clean {
            roots,
            roots_from_rules,
            ..
        } = mode
        else {
            panic!("expected a clean");
        };
        assert_eq!(roots, vec![PathBuf::from("/elsewhere")]);
        assert!(
            !roots_from_rules,
            "a user who named a path knows where they pointed"
        );
    }

    /// Every built-in rule but the cache ones is unrooted, so with no home and
    /// no path there is nothing to walk. That is a statement about the
    /// configuration, and the caller has to make it — an empty plan would read
    /// as "nothing to clean".
    #[test]
    fn unrooted_rules_alone_leave_nothing_to_walk() {
        assert!(clean_roots(&["clean"]).is_empty());
    }

    /// Unlike `clean`, a scan has no rules to ask and never falls back to the
    /// working directory.
    #[test]
    fn scan_still_demands_a_path() {
        let err = parse(&["scan"]).expect_err("scan needs a path");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    /// Both verbs are listed, and they take the **same** flags — the claim the
    /// whole split rests on.
    ///
    /// Asked of clap rather than of the rendered page. Scraping words beginning
    /// `--` out of the help text made the prose part of the assertion: naming
    /// `--apply` in `clean`'s description, to say where it went, registered as a
    /// flag `preview` did not have.
    #[test]
    fn both_verbs_take_the_same_flags() {
        use clap::CommandFactory;

        let mut command = Args::command();
        // Globals are propagated into subcommands at build time, so the two
        // verbs are only comparable after this.
        command.build();

        let top = command.render_long_help().to_string();
        for verb in ["preview", "clean"] {
            assert!(top.contains(verb), "--help must list {verb}:\n{top}");
        }

        let flags = |verb: &str| {
            let mut names: Vec<&str> = command
                .find_subcommand(verb)
                .unwrap_or_else(|| panic!("the {verb} subcommand exists"))
                .get_arguments()
                .filter_map(|arg| arg.get_long())
                .collect();
            names.sort_unstable();
            names
        };

        let clean = flags("clean");
        for flag in [
            "safe",
            "purge",
            "yes",
            "min-size",
            "older-than",
            "depth",
            "sort",
            "json",
        ] {
            assert!(
                clean.contains(&flag),
                "`clean` must take --{flag}: {clean:?}"
            );
        }
        for gone in ["apply", "allow-dirty"] {
            assert!(
                !clean.contains(&gone),
                "--{gone} was removed in v0.5: {clean:?}"
            );
        }
        assert_eq!(flags("preview"), clean, "the two must offer the same flags");
    }

    /// And what they *do* is documented where a user looks — the three tiers and
    /// both ways out of the refusal, in `clean --help` rather than only in the
    /// README.
    #[test]
    fn the_help_explains_the_tiers_and_the_refusal() {
        use clap::CommandFactory;

        let page = Args::command()
            .find_subcommand_mut("clean")
            .expect("the clean subcommand exists")
            .render_long_help()
            .to_string();

        for word in ["purge", "trash", "confirm", "--safe", "--yes"] {
            assert!(page.contains(word), "{word} missing from:\n{page}");
        }
    }

    /// `--help` is a terminal, not a rendered page.
    ///
    /// clap turns doc comments into help text, so a `**bold**` or a `[`link`]`
    /// written for rustdoc is printed to the user verbatim — and a multi-
    /// paragraph doc comment becomes `long_help`, which is how a note addressed
    /// to whoever maintains this file ended up in front of users.
    #[test]
    fn help_is_free_of_markup_and_maintainer_notes() {
        use clap::CommandFactory;

        let mut command = Args::command();
        let mut pages = vec![
            command.render_long_help().to_string(),
            command.render_help().to_string(),
        ];
        let mut clean = command
            .find_subcommand_mut("clean")
            .expect("the clean subcommand exists")
            .clone();
        pages.push(clean.render_long_help().to_string());

        for page in pages {
            assert!(
                !page.contains("**"),
                "emphasis markers reach the terminal as asterisks:\n{page}"
            );
            assert!(
                !page.contains("[`"),
                "rustdoc links reach the terminal verbatim:\n{page}"
            );
            for leak in ["Args::resolve", "Option<", "clap turns"] {
                assert!(
                    !page.contains(leak),
                    "`{leak}` is a note to a maintainer, not help for a user:\n{page}"
                );
            }
        }
    }

    #[test]
    fn validate_root_accepts_existing_and_rejects_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(validate_root(dir.path()).is_ok());

        let missing = dir.path().join("nope");
        let err = validate_root(&missing).expect_err("missing path must error");
        assert!(
            err.contains(&missing.display().to_string()),
            "error should name the path, got: {err}"
        );
    }

    /// `try_exists` returns `Err` (not `Ok(false)`) when the path can't even be
    /// probed — e.g. an unreadable ancestor. That is the "cannot access" arm,
    /// distinct from a plainly-absent path.
    #[cfg(unix)]
    #[test]
    fn validate_root_reports_unreadable_as_cannot_access() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        let child = locked.join("child");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        // Privileges that ignore the missing execute bit would probe `child`
        // fine and make this pass for the wrong reason — bail loudly instead.
        if child.try_exists().is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
                .expect("restore");
            eprintln!("skipping: privileges ignore the locked ancestor");
            return;
        }

        let result = validate_root(&child);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");

        let err = result.expect_err("an unreadable ancestor must error");
        assert!(
            err.contains("cannot access"),
            "error should be the access arm, got: {err}"
        );
    }
}
