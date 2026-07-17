//! Command-line surface: parse arguments into a [`ScanOptions`] for the core.
//!
//! `-n`/`--number` and `--json` are the CLI's own concerns (display count and
//! output format), not [`ScanOptions`] fields — they are declared here so
//! `--help` lists them, and consumed by the renderer (Task 7) and JSON output
//! (Task 8).

use clap::Parser;
use disk_tools_core::ScanOptions;
use std::path::{Path, PathBuf};

/// disk-tools — find what's eating your disk.
#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Args {
    /// Directory (or file) to scan. Always explicit — never defaults to the CWD.
    #[arg(value_name = "PATH")]
    pub root: PathBuf,

    /// Show at most this many entries.
    #[arg(short = 'n', long = "number")]
    pub number: Option<usize>,

    /// Hide entries below this size, e.g. `1M`, `512K`, `2G` (1024-based).
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

impl Args {
    /// Extract the scan-affecting arguments as the core's [`ScanOptions`]. The
    /// display-only flags (`number`, `json`) stay behind — the core neither
    /// knows nor cares about them. Consumes `self`: once the options are built
    /// the CLI has no further use for the parsed args, so `root` moves rather
    /// than clones.
    pub fn into_scan_options(self) -> ScanOptions {
        ScanOptions {
            root: self.root,
            min_size: self.min_size,
            depth: self.depth,
            apparent: self.apparent,
            one_file_system: self.one_file_system,
        }
    }
}

/// Parse a size like `1M` / `512K` / `2G` (1024-based) or a bare byte count.
///
/// Used as a clap `value_parser`, so a malformed value surfaces as a clean
/// usage error rather than a panic.
fn parse_size(s: &str) -> Result<u64, String> {
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

    #[test]
    fn path_maps_to_scan_options_root() {
        let args = parse(&["/some/path"]).expect("parse");
        assert_eq!(args.into_scan_options().root, PathBuf::from("/some/path"));
    }

    #[test]
    fn missing_path_is_a_usage_error() {
        // clap rejects a missing required positional before main runs, so the
        // CWD is never scanned by accident.
        let err = parse(&[]).expect_err("no path must fail");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn min_size_suffix_parsing() {
        assert_eq!(
            parse(&["/x", "--min-size", "1M"]).unwrap().min_size,
            1_048_576
        );
        assert_eq!(
            parse(&["/x", "--min-size", "512K"]).unwrap().min_size,
            524_288
        );
        assert_eq!(
            parse(&["/x", "--min-size", "1048576"]).unwrap().min_size,
            1_048_576
        );
        // Default when the flag is absent.
        assert_eq!(parse(&["/x"]).unwrap().min_size, 0);
    }

    #[test]
    fn min_size_rejects_garbage() {
        assert!(parse(&["/x", "--min-size", "1Q"]).is_err());
        assert!(parse(&["/x", "--min-size", "12x"]).is_err());
        assert!(parse(&["/x", "--min-size", "abc"]).is_err());
    }

    #[test]
    fn min_size_explicit_zero() {
        assert_eq!(parse(&["/x", "--min-size", "0"]).unwrap().min_size, 0);
    }

    #[test]
    fn min_size_is_case_insensitive() {
        assert_eq!(
            parse(&["/x", "--min-size", "1m"]).unwrap().min_size,
            1_048_576
        );
        assert_eq!(
            parse(&["/x", "--min-size", "2g"]).unwrap().min_size,
            2u64 << 30
        );
    }

    #[test]
    fn min_size_trims_surrounding_whitespace() {
        assert_eq!(
            parse(&["/x", "--min-size", " 1M "]).unwrap().min_size,
            1_048_576
        );
    }

    #[test]
    fn min_size_g_and_t_multipliers() {
        assert_eq!(
            parse(&["/x", "--min-size", "2G"]).unwrap().min_size,
            2u64 << 30
        );
        assert_eq!(
            parse(&["/x", "--min-size", "1T"]).unwrap().min_size,
            1u64 << 40
        );
    }

    #[test]
    fn min_size_long_unit_aliases() {
        // KiB/MiB/GiB/TiB and their non-"i" long forms must all match the same
        // 1024-based multiplier as the single-letter suffix.
        assert_eq!(
            parse(&["/x", "--min-size", "1KB"]).unwrap().min_size,
            1 << 10
        );
        assert_eq!(
            parse(&["/x", "--min-size", "1KiB"]).unwrap().min_size,
            1 << 10
        );
        assert_eq!(
            parse(&["/x", "--min-size", "1MB"]).unwrap().min_size,
            1 << 20
        );
        assert_eq!(
            parse(&["/x", "--min-size", "1MiB"]).unwrap().min_size,
            1 << 20
        );
        assert_eq!(
            parse(&["/x", "--min-size", "1GB"]).unwrap().min_size,
            1 << 30
        );
        assert_eq!(
            parse(&["/x", "--min-size", "1GiB"]).unwrap().min_size,
            1 << 30
        );
        assert_eq!(
            parse(&["/x", "--min-size", "1TB"]).unwrap().min_size,
            1 << 40
        );
        assert_eq!(
            parse(&["/x", "--min-size", "1TiB"]).unwrap().min_size,
            1 << 40
        );
    }

    #[test]
    fn min_size_digit_overflow_is_a_clean_error_not_a_panic() {
        // The digit run alone (~1e20) already exceeds u64::MAX (~1.8e19), so
        // this exercises the `digits.parse()` failure path, distinct from the
        // `checked_mul` overflow below.
        let err = parse(&["/x", "--min-size", "99999999999999999999T"])
            .expect_err("digit overflow must not panic");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn min_size_multiplication_overflow_is_a_clean_error_not_a_panic() {
        // The digits (2e10) fit comfortably in a u64 on their own; only the
        // `* 1<<40` multiplication overflows, exercising `checked_mul` rather
        // than the digit-parse failure above.
        let err = parse(&["/x", "--min-size", "20000000000T"])
            .expect_err("multiplication overflow must not panic");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn number_flag_parses_short_and_long() {
        assert_eq!(parse(&["/x", "-n", "5"]).unwrap().number, Some(5));
        assert_eq!(parse(&["/x", "--number", "5"]).unwrap().number, Some(5));
        assert_eq!(parse(&["/x"]).unwrap().number, None);
    }

    #[test]
    fn flags_map_onto_scan_options() {
        let opts = parse(&["/x", "--depth", "3", "--apparent", "--one-file-system"])
            .unwrap()
            .into_scan_options();
        assert_eq!(opts.depth, Some(3));
        assert!(opts.apparent);
        assert!(opts.one_file_system);
    }

    #[test]
    fn help_lists_all_flags() {
        use clap::CommandFactory;

        let help = Args::command().render_long_help().to_string();
        for flag in [
            "-n",
            "--min-size",
            "--depth",
            "--apparent",
            "--one-file-system",
            "--json",
        ] {
            assert!(
                help.contains(flag),
                "--help must mention {flag}, got:\n{help}"
            );
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
