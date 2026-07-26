//! Command-line surface: parse arguments into a [`ScanOptions`] for the core.
//!
//! `-n`/`--number` and `--json` are the CLI's own concerns (display count and
//! output format), not [`ScanOptions`] fields — they are declared here so
//! `--help` lists them, and consumed by the renderer (Task 7) and JSON output
//! (Task 8).

use crate::config::Config;
use clap::{Parser, Subcommand};
use disk_tools_core::{
    CleanOptions, DetectOptions, Removal, RuleError, Rules, ScanOptions, UserDirs, age_rule,
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

    /// Find removable junk. Dry-run by default — nothing is deleted without --apply.
    Clean(CleanArgs),

    /// Inspect and create the configuration file.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
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

    /// Show at most this many entries.
    #[arg(short = 'n', long = "number")]
    pub number: Option<usize>,

    /// Hide entries below this size, e.g. 1M, 512K, 2G (1024-based).
    #[arg(long = "min-size", default_value = "0", value_parser = parse_size)]
    pub min_size: u64,

    /// Print at most this many levels deep (display only; traversal is always full).
    #[arg(long)]
    pub depth: Option<usize>,

    /// Rank and report apparent size rather than allocated size.
    #[arg(long)]
    pub apparent: bool,

    /// Stop at filesystem boundaries instead of descending into other mounts.
    #[arg(long = "one-file-system")]
    pub one_file_system: bool,

    /// Emit JSON instead of the tree report.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Debug)]
pub struct CleanArgs {
    /// Directory to examine.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Only offer regenerable safe-list categories; skip anything needing
    /// per-item confirmation.
    #[arg(long)]
    pub safe: bool,

    /// Actually remove the candidates, to the OS trash.
    #[arg(long)]
    pub apply: bool,

    /// Delete outright instead of trashing. Nothing can be put back. Requires --apply.
    #[arg(long, requires = "apply")]
    pub purge: bool,

    /// Include build output whose project has uncommitted changes.
    #[arg(long = "allow-dirty")]
    pub allow_dirty: bool,

    /// Ignore anything smaller than this, e.g. 1M, 512K (1024-based).
    #[arg(long = "min-size", default_value = "0", value_parser = parse_size)]
    pub min_size: u64,

    /// Also offer anything untouched for this long: 90d, 6m, 1y.
    #[arg(long = "older-than", value_parser = parse_duration, value_name = "DURATION")]
    pub older_than: Option<Duration>,
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
        scan: ScanOptions,
        /// Boxed: the compiled rule set carries two glob automata, and without
        /// the indirection every `Mode::Scan` — which needs none of it — would
        /// pay for the space anyway.
        clean: Box<CleanOptions>,
        apply: bool,
        removal: Removal,
    },
    /// Write the default configuration to `target`.
    ConfigInit { target: PathBuf, force: bool },
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
            Command::Scan(scan) => Ok(Mode::Scan {
                options: ScanOptions {
                    root: scan.root,
                    min_size: scan.min_size,
                    depth: scan.depth,
                    apparent: scan.apparent,
                    one_file_system: scan.one_file_system,
                },
                number: scan.number,
                json: scan.json,
            }),

            Command::Config {
                action: ConfigAction::Init { force },
            } => config_path
                .map(|target| Mode::ConfigInit { target, force })
                .ok_or(ResolveError::NoConfigPath),

            Command::Clean(clean) => {
                // The configured rules — the built-ins when the file said nothing
                // — plus the age rule **last** if it was asked for. Order is
                // precedence, so appending it there is what keeps a `target/`
                // reported as build output rather than merely as something old,
                // which is what decides its tier.
                let mut rules = config.rules;
                if let Some(older_than) = clean.older_than {
                    rules.push(age_rule(older_than));
                }

                Ok(Mode::Clean {
                    scan: ScanOptions {
                        root: clean.path,
                        ..ScanOptions::default()
                    },
                    clean: Box::new(CleanOptions {
                        detect: DetectOptions {
                            rules: Rules::new(rules, &user_dirs).map_err(ResolveError::Rule)?,
                            now,
                        },
                        user_dirs,
                        safe_only: clean.safe,
                        allow_dirty: clean.allow_dirty,
                        min_size: clean.min_size,
                    }),
                    apply: clean.apply,
                    removal: if clean.purge {
                        Removal::Purge
                    } else {
                        Removal::Trash
                    },
                })
            }
        }
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
            Mode::Clean { scan, .. } => scan,
            other => panic!("expected a scan or a clean, got {other:?}"),
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

        // And each verb keeps its own required path.
        for verb in ["scan", "clean"] {
            let err = parse(&[verb]).expect_err("{verb} needs a path");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{verb} must demand a path"
            );
        }
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

    #[test]
    fn clean_subcommand_parses_its_flags() {
        let mode = resolved(&[
            "clean",
            "/x",
            "--safe",
            "--allow-dirty",
            "--older-than",
            "90d",
            "--apply",
        ])
        .expect("parse");

        let Mode::Clean {
            scan, clean, apply, ..
        } = mode
        else {
            panic!("expected the clean subcommand");
        };
        assert_eq!(scan.root, PathBuf::from("/x"));
        assert!(clean.safe_only);
        assert!(clean.allow_dirty);
        assert!(apply);
        assert_eq!(
            older_than_of(&clean),
            Some(Duration::from_secs(90 * 24 * 60 * 60))
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

    /// `--purge` alone would read as "prepare to delete permanently" and do
    /// nothing, which is the worst possible reading of a destructive flag. clap
    /// enforces the pairing so the intent has to be stated twice.
    #[test]
    fn purge_requires_apply() {
        let err = parse(&["clean", "/x", "--purge"])
            .expect_err("--purge without --apply must be a usage error");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);

        assert!(parse(&["clean", "/x", "--purge", "--apply"]).is_ok());
    }

    #[test]
    fn removal_defaults_to_the_trash() {
        let mode = resolved(&["clean", "/x", "--apply"]).expect("resolve");
        let Mode::Clean { removal, .. } = mode else {
            panic!("expected the clean subcommand");
        };
        assert_eq!(
            removal,
            Removal::Trash,
            "nothing becomes unrecoverable without being asked for"
        );

        let mode = resolved(&["clean", "/x", "--apply", "--purge"]).expect("resolve");
        let Mode::Clean { removal, .. } = mode else {
            panic!("expected the clean subcommand");
        };
        assert_eq!(removal, Removal::Purge);
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

    #[test]
    fn clean_without_a_path_is_a_usage_error() {
        let err = parse(&["clean"]).expect_err("clean needs a path");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn help_documents_both_forms() {
        use clap::CommandFactory;

        let help = Args::command().render_long_help().to_string();
        assert!(
            help.contains("clean"),
            "--help must list the subcommand:\n{help}"
        );

        let clean_help = Args::command()
            .find_subcommand_mut("clean")
            .expect("the clean subcommand exists")
            .render_long_help()
            .to_string();
        for flag in ["--safe", "--apply", "--allow-dirty", "--older-than"] {
            assert!(
                clean_help.contains(flag),
                "`clean --help` must mention {flag}, got:\n{clean_help}"
            );
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
