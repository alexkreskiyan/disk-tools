//! The configuration file: where it lives, what it may say, and what it means.
//!
//! `disk_tools_core::ScanOptions` states the rule this module exists to keep:
//! the core reads no config and consults no environment. So the file is found,
//! read, validated and converted **here**, and the core is handed values.
//!
//! Two things about the shape below are deliberate rather than incidental.
//!
//! **The file's rule schema is not the core's [`Rule`].** A rule in the file must
//! name a `root`, so that `clean` with no path knows what to walk; the core type
//! allows `None`, because the built-in rules genuinely apply wherever the scan
//! goes. [`RuleEntry`] is the bridge, and `root = "*"` is how the file spells the
//! permissive case.
//!
//! **A malformed file stops the program; an unfamiliar key does not.** A syntax
//! error or a rule missing its `includes` means the user's intent is unknown,
//! and guessing about the input to a delete operation is not on. An unknown key
//! is far more likely a typo or a newer version's setting, and refusing to run
//! over one would make the tool brittle for no safety gained — so it is a
//! warning, naming the key so the typo is findable.

pub mod write;

use crate::args::{parse_duration, parse_size};
use disk_tools_core::{Rule, Tier, UserDirs, builtin_rules};
use serde::Deserialize;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// What `config init` writes, comments included.
///
/// A template rather than serialized output, because **the comments are the
/// feature**: the default config is how a user finds out what the tool matches
/// and where. Serializing [`builtin_rules`] would produce a correct file that
/// explains nothing.
///
/// The cost of a template is that it can drift from the code it documents.
/// `the_written_config_parses_back_to_the_builtin_rules` is what stops that, and
/// it is the reason this may be a string at all.
pub const DEFAULT_CONFIG: &str = r#"# disk-tools configuration.
#
# Precedence: command-line flag > this file > built-in default.
#
# One exception, and it is a limitation rather than a rule: a true/false setting
# turned on here cannot be turned back off from the command line, because a flag
# can only be passed or not passed. `--purge` and `--yes` are absent from this
# file for the same reason inverted — a file that silently deleted past the
# trash, or answered a confirmation in advance, would hide that it had. The git
# guard is settled per rule instead, by `requires-clean-repo`.
#
# The never-touch denylist is NOT here and cannot be configured.

[scan]                          # walk behaviour, for `scan <PATH>` only
one-file-system = false

# `scan` only, and display only — none of it ever changes a total. `preview`
# and `clean` list every candidate: that list is what you act on by running the
# other verb, and a truncated one would be acting on what was never shown.
#
# Commented-out keys are the built-in defaults; uncomment to change them.
#   n     = 20    # show at most this many entries. Default: all of them.
#   depth = 2     # print at most this many levels. 0 is the root alone,
#                 # exactly as --depth 0 means. Default: unlimited.
[report]
min-size = "0"
apparent = false

[clean]
require-confirmation = true     # --apply refuses while confirm-tier remains
safe                 = false    # as if --safe were always passed

# Rules. List order is precedence: the first match claims the node, and a
# claimed node is never descended into.
#
# `root` is where the rule applies, and answers "what do I clean when no path is
# named". Use "*" for a rule that applies wherever the scan goes.
#
# A trailing `/` in `includes` means directory only, as in gitignore — which is
# why `**/*.pyc` matches files and `**/node_modules/` does not match a file of
# that name.
#
# `requires-sibling` is a glob matched against the file names *beside* a match,
# and each pattern given has to find something of its own. It is a glob because
# most build systems name their marker after the project — the file that proves
# a `bin/` is .NET output is `Whatever.csproj`. A pattern with no metacharacters
# matches itself, so "Cargo.toml" still means exactly that.
#
#   requires-sibling = "*.csproj"                   # a .NET project lives here
#   requires-sibling = ["*.csproj", "*.sln"]        # and a solution beside it
#
# `tier` says what `clean` does with what the rule claims. Three answers to one
# question, and an unstated tier is the cautious one:
#
#   purge     destroys it. No confirmation, and no trash — for content a single
#             command regenerates, where the trash is a chore rather than a
#             safety net, since it frees nothing until it is emptied.
#   trash     moves it to the OS trash. No confirmation.
#   confirm   nothing, until you pass --yes. The default when `tier` is absent.
#
# `--safe` drops what needs confirming, so it keeps *both* of the others: purge
# is a stronger claim of regenerability than trash, not a weaker one. Anything
# but `confirm` is a claim this tool cannot check — it takes your word and acts.

[[rules]]
name                = "rust-target"
root                = "*"
includes            = ["**/target/"]
requires-sibling    = "Cargo.toml"   # without it, target/ is an ordinary directory
requires-clean-repo = true
tier                = "trash"

[[rules]]
name     = "node-modules"
root     = "*"
includes = ["**/node_modules/"]
tier     = "trash"

[[rules]]
name     = "pycache"
root     = "*"
includes = ["**/__pycache__/", "**/*.pyc"]
tier     = "trash"

# The tilde is the whole safety of this one: `~/Library/Caches` is regenerable
# user data, and `/Library/Caches` is on the denylist. No `**` — the cache root
# itself is the candidate, not each thing inside it.
[[rules]]
name     = "user-caches"
root     = "~"
includes = [".cache/", "Library/Caches/"]
tier     = "trash"

[[rules]]
name     = "windows-temp"
root     = "%LOCALAPPDATA%"
includes = ["Temp/"]
tier     = "trash"
"#;

/// The file's contents, validated and converted.
#[derive(Debug)]
pub struct Config {
    /// Ready for `Rules::new`. The built-ins when the file said nothing about
    /// rules — an absent `[[rules]]` is not a request to have none.
    pub rules: Vec<Rule>,

    // Merged against the flags in `Args::resolve`, which is the only place the
    // order flag > file > default is expressed.
    pub scan: ScanSettings,
    pub report: ReportSettings,
    pub clean: CleanSettings,

    /// Unknown keys, by their dotted path. The caller prints them; this module
    /// neither logs nor decides where diagnostics go.
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            rules: builtin_rules(),
            scan: ScanSettings::default(),
            report: ReportSettings::default(),
            clean: CleanSettings::default(),
            warnings: Vec::new(),
        }
    }
}

// Every field is `Option` because absence has to stay distinguishable from a
// written default — that is what v0.3 Task 3's "a flag beats the file, and an
// explicit `--min-size 0` beats it too" is built on. Task 2 parses and validates
// them; Task 3 merges them.

/// `[scan]` — how the walk behaves.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanSettings {
    pub one_file_system: Option<bool>,
}

/// `[report]` — what is shown. Never changes a total, and never reaches `clean`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReportSettings {
    pub number: Option<usize>,
    pub depth: Option<usize>,
    pub min_size: Option<u64>,
    pub apparent: Option<bool>,
}

/// `[clean]`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanSettings {
    pub require_confirmation: Option<bool>,
    pub safe: Option<bool>,
}

/// Why the configuration could not be used.
///
/// Every variant names the file, because a user with a global config and a
/// `--config` override needs to know which one is being complained about.
#[derive(Debug)]
pub enum ConfigError {
    /// `--config` named a file that is not there. Absent by default is fine;
    /// absent when explicitly named is a typo worth stopping for.
    Missing(PathBuf),
    Read(PathBuf, io::Error),
    /// Malformed TOML. The message carries the line and column, from `toml`.
    Parse(PathBuf, String),
    /// Well-formed TOML that does not describe a usable rule set.
    Invalid(PathBuf, String),
    /// `config init` will not write over something.
    Exists(PathBuf),
    Write(PathBuf, io::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Missing(path) => {
                write!(f, "{}: no such file", path.display())
            }
            ConfigError::Read(path, err) => write!(f, "{}: {err}", path.display()),
            ConfigError::Parse(path, message) => write!(f, "{}: {message}", path.display()),
            ConfigError::Invalid(path, message) => write!(f, "{}: {message}", path.display()),
            ConfigError::Exists(path) => write!(
                f,
                "{}: already exists; pass --force to overwrite it",
                path.display()
            ),
            ConfigError::Write(path, err) => write!(f, "{}: {err}", path.display()),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Where the configuration file is, if this environment implies one.
///
/// `$XDG_CONFIG_HOME` wins on **every** platform when set. A user who exports it
/// has said where their configuration lives; ignoring that on Windows because
/// the platform has a different habit would be overruling them. `%APPDATA%` and
/// `~/.config` are the fallbacks, not the rule.
pub fn locate(explicit: Option<&Path>, dirs: &UserDirs, xdg: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Some(xdg) = xdg {
        return Some(config_file(&xdg));
    }
    platform_dir(dirs).map(|dir| config_file(&dir))
}

fn config_file(dir: &Path) -> PathBuf {
    dir.join("disk-tools").join("config.toml")
}

#[cfg(windows)]
fn platform_dir(dirs: &UserDirs) -> Option<PathBuf> {
    dirs.app_data.clone()
}

#[cfg(not(windows))]
fn platform_dir(dirs: &UserDirs) -> Option<PathBuf> {
    dirs.home.as_ref().map(|home| home.join(".config"))
}

/// Find and read the configuration, falling back to the built-in rules.
///
/// An absent file at the **default** path is an ordinary state — the tool ships
/// working defaults and needs no config to clean. An absent file at a path the
/// user *named* is a mistake: silently substituting defaults would hide it, and
/// they would be cleaning under rules they did not write.
pub fn load(
    explicit: Option<&Path>,
    dirs: &UserDirs,
    xdg: Option<PathBuf>,
) -> Result<Config, ConfigError> {
    let Some(path) = locate(explicit, dirs, xdg) else {
        return Ok(Config::default());
    };

    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&path, &text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if explicit.is_some() {
                Err(ConfigError::Missing(path))
            } else {
                Ok(Config::default())
            }
        }
        Err(err) => Err(ConfigError::Read(path, err)),
    }
}

/// Write the default configuration to `target`.
///
/// Refuses an existing file without `--force`: a config is something a user
/// edits, and a command that reads as "show me the defaults" must not be able to
/// throw their edits away.
pub fn init(target: &Path, force: bool) -> Result<(), ConfigError> {
    if !force && target.exists() {
        return Err(ConfigError::Exists(target.to_path_buf()));
    }
    if let Some(parent) = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| ConfigError::Write(parent.to_path_buf(), err))?;
    }
    std::fs::write(target, DEFAULT_CONFIG)
        .map_err(|err| ConfigError::Write(target.to_path_buf(), err))
}

/// Parse a config from a literal, for the precedence tests in `args`.
///
/// Those tests are about the **merge**, not about parsing, so they need a
/// `Config` and nothing else. Going through the real `parse` rather than
/// building the struct by hand is what keeps them honest: a test that assembled
/// its own `Config` would still pass if the file's keys stopped reaching it.
#[cfg(test)]
pub fn parse_for_test(text: &str) -> Config {
    parse(Path::new("/test/config.toml"), text).expect("the literal must parse")
}

/// The whole of parsing, with no filesystem in it — which is what lets every
/// case below be tested from a string literal.
fn parse(path: &Path, text: &str) -> Result<Config, ConfigError> {
    let mut warnings = Vec::new();
    let syntax = |err: toml::de::Error| ConfigError::Parse(path.to_path_buf(), err.to_string());

    let deserializer = toml::Deserializer::parse(text).map_err(syntax)?;
    let file: FileConfig = serde_ignored::deserialize(deserializer, |key| {
        warnings.push(readable(&key.to_string()));
    })
    .map_err(syntax)?;

    let invalid = |message: String| ConfigError::Invalid(path.to_path_buf(), message);

    // Absent means "say nothing about rules", which leaves the built-ins alone.
    // An explicitly empty list means "no rules", which is a thing a user may
    // legitimately want and is not the same statement.
    let rules = match file.rules {
        Some(entries) => convert(entries).map_err(invalid)?,
        None => builtin_rules(),
    };

    Ok(Config {
        rules,
        scan: ScanSettings {
            one_file_system: file.scan.one_file_system,
        },
        report: ReportSettings {
            number: file.report.n,
            depth: file.report.depth,
            min_size: file
                .report
                .min_size
                .map(|value| size(&value, "[report] min-size"))
                .transpose()
                .map_err(invalid)?,
            apparent: file.report.apparent,
        },
        clean: CleanSettings {
            require_confirmation: file.clean.require_confirmation,
            safe: file.clean.safe,
        },
        warnings,
    })
}

/// `serde_ignored`'s path, as a user could find it in their file.
///
/// It emits a `?` segment for every `Option` layer it descends through, so a
/// misspelt key inside `[[rules]]` comes back as `rules.?.0.tyer`. That is an
/// artefact of how this file's schema is typed, not something the user wrote,
/// and a message pointing at a path that does not exist is worse than none.
fn readable(key: &str) -> String {
    key.split('.')
        .filter(|segment| *segment != "?")
        .collect::<Vec<_>>()
        .join(".")
}

/// Validate and convert the file's rules into the core's.
///
/// Each message names the rule, by its name where it has one and by its position
/// where it does not — an error about "a rule" in a file with twelve of them is
/// not an error a user can act on.
fn convert(entries: Vec<RuleEntry>) -> Result<Vec<Rule>, String> {
    let mut rules: Vec<Rule> = Vec::with_capacity(entries.len());

    for (index, entry) in entries.into_iter().enumerate() {
        let position = format!("rule #{}", index + 1);
        let name = entry.name.unwrap_or_default();
        if name.trim().is_empty() {
            return Err(format!(
                "{position}: `name` is required and must not be empty"
            ));
        }
        let where_ = format!("rule `{name}`");

        if rules.iter().any(|rule| rule.name == name) {
            return Err(format!(
                "{where_}: duplicate name; each rule needs its own, since that is what the report and the plan refer to"
            ));
        }

        let Some(root) = entry.root else {
            return Err(format!(
                "{where_}: `root` is required — use \"*\" for a rule that applies wherever the scan goes"
            ));
        };

        let includes = entry.includes.map(Strings::into_vec).unwrap_or_default();
        if includes.is_empty() {
            return Err(format!(
                "{where_}: `includes` is required and must not be empty; a rule that matches nothing is not a rule"
            ));
        }

        rules.push(Rule {
            // The file's spelling of "no root". The core distinguishes an
            // unrooted rule from one rooted at `/`, and this is how a required
            // field still expresses the former.
            root: (root != "*").then_some(root),
            includes,
            excludes: entry.excludes.map(Strings::into_vec).unwrap_or_default(),
            requires_sibling: entry
                .requires_sibling
                .map(Strings::into_vec)
                .unwrap_or_default(),
            requires_clean_repo: entry.requires_clean_repo.unwrap_or(false),
            older_than: entry
                .older_than
                .map(|value| {
                    parse_duration(&value).map_err(|err| format!("{where_}: `older-than`: {err}"))
                })
                .transpose()?,
            min_size: entry
                .min_size
                .map(|value| size(&value, &format!("{where_}: `min-size`")))
                .transpose()?
                .unwrap_or(0),
            tier: entry
                .tier
                .map(|word| tier(&word, &format!("{where_}: `tier`")))
                .transpose()?
                // Cautious when unstated: a rule that forgets to say gets the
                // answer that asks.
                .unwrap_or(Tier::Confirm),
            enabled: entry.enabled.unwrap_or(true),
            name,
        });
    }

    Ok(rules)
}

/// One `parse_size` for the flag and the file both. Two would drift, and this
/// one decides how much a deletion rule is allowed to skip.
fn size(value: &str, where_: &str) -> Result<u64, String> {
    parse_size(value).map_err(|err| format!("{where_}: {err}"))
}

// ---- the file's own schema ----------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct FileConfig {
    #[serde(default)]
    scan: RawScan,
    #[serde(default)]
    report: RawReport,
    #[serde(default)]
    clean: RawClean,
    rules: Option<Vec<RuleEntry>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawScan {
    one_file_system: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawReport {
    n: Option<usize>,
    depth: Option<usize>,
    /// A string, so `"1M"` works and one parser serves both the flag and this.
    min_size: Option<String>,
    apparent: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawClean {
    require_confirmation: Option<bool>,
    safe: Option<bool>,
}

/// A rule as the file spells it.
///
/// Deliberately **not** `deny_unknown_fields`: that makes serde fail before
/// `serde_ignored` has anything to observe, and the two cannot both be used. An
/// unknown key is a warning here, so the observing side has to win.
///
/// Everything is `Option` even where it is required, so that the missing-field
/// message can name the rule. serde's own would say "missing field `root`" and
/// leave the user to find which of their rules it meant.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RuleEntry {
    name: Option<String>,
    root: Option<String>,
    includes: Option<Strings>,
    excludes: Option<Strings>,
    requires_sibling: Option<Strings>,
    requires_clean_repo: Option<bool>,
    older_than: Option<String>,
    min_size: Option<String>,
    tier: Option<String>,
    enabled: Option<bool>,
}

/// One pattern or several. `includes = "**/target/"` is the obvious thing to
/// write for a single glob, and refusing it would be pedantry.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Strings {
    One(String),
    Many(Vec<String>),
}

impl Strings {
    fn into_vec(self) -> Vec<String> {
        match self {
            Strings::One(one) => vec![one],
            Strings::Many(many) => many,
        }
    }
}

/// The tier, by the word the file uses.
///
/// Read from a plain string rather than a serde enum so that both messages can
/// be written here. serde's own would list `auto` among the valid values while
/// rejecting it, or omit it and say nothing about where it went — and the whole
/// point of failing on `auto` is to name what replaced it.
fn tier(word: &str, where_: &str) -> Result<Tier, String> {
    match word {
        "purge" => Ok(Tier::Purge),
        "trash" => Ok(Tier::Trash),
        "confirm" => Ok(Tier::Confirm),
        // Not an alias. An alias lives for ever; an error costs one edit and
        // stops `clean` from starting on a file that has not been read through
        // — which, for a verb that now removes, is the safe direction.
        "auto" => Err(format!(
            "{where_}: `auto` was renamed to `trash` in v0.5. The three tiers say what \
             `clean` does: `purge` destroys, `trash` recovers, `confirm` waits for --yes"
        )),
        other => Err(format!(
            "{where_}: unknown tier `{other}`; expected `purge`, `trash` or `confirm`"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(text: &str) -> Result<Config, ConfigError> {
        parse(Path::new("/cfg/config.toml"), text)
    }

    fn rules_of(text: &str) -> Vec<Rule> {
        at(text).expect("parse").rules
    }

    fn message(text: &str) -> String {
        at(text).expect_err("must fail").to_string()
    }

    fn dirs(home: &str) -> UserDirs {
        UserDirs {
            home: Some(PathBuf::from(home)),
            app_data: Some(PathBuf::from(r"C:\Users\Me\AppData\Roaming")),
            ..UserDirs::default()
        }
    }

    // ---- location --------------------------------------------------------

    /// An exported `XDG_CONFIG_HOME` is the user saying where their config is,
    /// and it is honoured on every platform for that reason.
    #[test]
    fn xdg_config_home_wins_when_set() {
        assert_eq!(
            locate(None, &dirs("/home/me"), Some(PathBuf::from("/xdg"))),
            Some(PathBuf::from("/xdg/disk-tools/config.toml"))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn without_xdg_the_path_is_under_dot_config() {
        assert_eq!(
            locate(None, &dirs("/home/me"), None),
            Some(PathBuf::from("/home/me/.config/disk-tools/config.toml"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn without_xdg_the_path_is_under_appdata() {
        assert_eq!(
            locate(None, &dirs(r"C:\Users\Me"), None),
            Some(PathBuf::from(
                r"C:\Users\Me\AppData\Roaming\disk-tools\config.toml"
            ))
        );
    }

    /// `--config` is not a hint. Neither XDG nor the platform path is consulted
    /// once it is given, so what the user named is what is read.
    #[test]
    fn an_explicit_path_beats_everything() {
        let explicit = PathBuf::from("/somewhere/else.toml");

        assert_eq!(
            locate(
                Some(&explicit),
                &dirs("/home/me"),
                Some(PathBuf::from("/xdg"))
            ),
            Some(explicit)
        );
    }

    /// No home, no `%APPDATA%`, no XDG: there is no path to guess at, and the
    /// built-in rules apply.
    #[test]
    fn an_unknown_environment_yields_no_path() {
        assert_eq!(locate(None, &UserDirs::default(), None), None);
    }

    // ---- the written defaults -------------------------------------------

    /// The one test that makes a commented template a legitimate way to write
    /// this file. Without it, `config init` could quietly document rules the tool
    /// no longer has.
    #[test]
    fn the_written_config_parses_back_to_the_builtin_rules() {
        assert_eq!(
            rules_of(DEFAULT_CONFIG),
            builtin_rules(),
            "the template and the code must describe the same rules"
        );
    }

    /// And the rest of the template is schema-valid too, not merely its rules —
    /// a stray key there would otherwise only surface as a warning on a user's
    /// first run.
    #[test]
    fn the_written_config_has_no_unknown_keys() {
        let config = at(DEFAULT_CONFIG).expect("parse");

        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert_eq!(config.report.min_size, Some(0));
        assert_eq!(
            config.report.number, None,
            "`n` ships commented out: the written file must not change what a \
             scan prints, and without `-n` a scan has always printed everything"
        );
        assert_eq!(
            config.report.depth, None,
            "`depth` ships commented out too — 0 means the root alone for the \
             flag, so the file must not give the same value the opposite meaning"
        );
        assert_eq!(config.clean.require_confirmation, Some(true));
        assert_eq!(config.scan.one_file_system, Some(false));
    }

    // ---- rules -----------------------------------------------------------

    /// The file must name a root, and this is how it names "anywhere" — which is
    /// what the three unrooted built-ins mean and what `config init` writes.
    #[test]
    fn a_star_root_becomes_an_unrooted_rule() {
        let rules = rules_of(
            r#"
            [[rules]]
            name = "mine"
            root = "*"
            includes = ["**/x/"]
            "#,
        );

        assert_eq!(rules[0].root, None);
    }

    #[test]
    fn a_real_root_is_carried_through_as_written() {
        let rules = rules_of(
            r#"
            [[rules]]
            name = "mine"
            root = "~/Projects"
            includes = ["**/x/"]
            "#,
        );

        assert_eq!(
            rules[0].root.as_deref(),
            Some("~/Projects"),
            "the token is the core's to expand, not this module's"
        );
    }

    #[test]
    fn every_field_survives_the_conversion() {
        let rules = rules_of(
            r#"
            [[rules]]
            name = "mine"
            root = "*"
            includes = ["**/a/", "**/b"]
            excludes = "**/vendor/**"
            requires-sibling = ["Cargo.toml"]
            requires-clean-repo = true
            older-than = "90d"
            min-size = "1M"
            tier = "trash"
            enabled = false
            "#,
        );

        assert_eq!(
            rules[0],
            Rule {
                name: "mine".into(),
                root: None,
                includes: vec!["**/a/".into(), "**/b".into()],
                excludes: vec!["**/vendor/**".into()],
                requires_sibling: vec!["Cargo.toml".into()],
                requires_clean_repo: true,
                older_than: Some(Duration::from_secs(90 * 24 * 60 * 60)),
                min_size: 1_048_576,
                tier: Tier::Trash,
                enabled: false,
            }
        );
    }

    /// A single glob is the common case, and demanding brackets around it would
    /// be pedantry.
    #[test]
    fn includes_accepts_a_bare_string() {
        let rules = rules_of(
            r#"
            [[rules]]
            name = "mine"
            root = "*"
            includes = "**/x/"
            "#,
        );

        assert_eq!(rules[0].includes, vec!["**/x/".to_owned()]);
    }

    /// The three names, each accepted as written.
    #[test]
    fn every_tier_the_file_may_say() {
        for (word, expected) in [
            ("purge", Tier::Purge),
            ("trash", Tier::Trash),
            ("confirm", Tier::Confirm),
        ] {
            let rules = rules_of(&format!(
                "[[rules]]\nname = \"r\"\nroot = \"*\"\nincludes = [\"x\"]\ntier = \"{word}\"\n"
            ));
            assert_eq!(rules[0].tier, expected, "`{word}`");
        }
    }

    /// Not an alias. An alias lives for ever; an error costs one edit and stops
    /// `clean` — which now removes — from starting on a file nobody has read
    /// through.
    #[test]
    fn the_renamed_tier_is_refused_by_name() {
        let err =
            at("[[rules]]\nname = \"r\"\nroot = \"*\"\nincludes = [\"x\"]\ntier = \"auto\"\n")
                .expect_err("`auto` must not parse");

        let message = err.to_string();
        assert!(message.contains("auto"), "{message}");
        assert!(
            message.contains("trash"),
            "and it has to name what replaced it: {message}"
        );
    }

    #[test]
    fn an_unknown_tier_names_the_three() {
        let err =
            at("[[rules]]\nname = \"r\"\nroot = \"*\"\nincludes = [\"x\"]\ntier = \"maybe\"\n")
                .expect_err("`maybe` is not a tier");

        let message = err.to_string();
        for word in ["purge", "trash", "confirm"] {
            assert!(message.contains(word), "{word} missing from {message}");
        }
    }

    /// An unstated tier asks. The file cannot make a rule auto by omission.
    #[test]
    fn an_unstated_tier_is_confirm() {
        let rules = rules_of(
            r#"
            [[rules]]
            name = "mine"
            root = "*"
            includes = ["**/x/"]
            "#,
        );

        assert_eq!(rules[0].tier, Tier::Confirm);
    }

    /// Saying nothing about rules leaves the built-ins alone; saying "none"
    /// means none. A `Vec` with a serde default could not tell the two apart,
    /// and would have turned a file that only sets `[report] n` into a file that
    /// silently disables every rule.
    #[test]
    fn absent_rules_keep_the_builtins_but_an_empty_list_does_not() {
        assert_eq!(rules_of("[report]\nn = 5\n"), builtin_rules());
        assert!(rules_of("rules = []\n").is_empty());
    }

    // ---- what is refused -------------------------------------------------

    #[test]
    fn a_syntax_error_names_the_line() {
        let message = message("[scan]\none-file-system = \n");

        assert!(
            message.contains("line 2"),
            "the message must locate the mistake: {message}"
        );
        assert!(message.contains("/cfg/config.toml"), "{message}");
    }

    #[test]
    fn a_rule_without_a_root_is_refused_by_name() {
        let message = message(
            r#"
            [[rules]]
            name = "mine"
            includes = ["**/x/"]
            "#,
        );

        assert!(message.contains("rule `mine`"), "{message}");
        assert!(message.contains("root"), "{message}");
        assert!(
            message.contains('*'),
            "and it must say how to spell 'anywhere': {message}"
        );
    }

    #[test]
    fn a_rule_without_includes_is_refused_by_name() {
        for text in [
            "[[rules]]\nname = \"mine\"\nroot = \"*\"\n",
            "[[rules]]\nname = \"mine\"\nroot = \"*\"\nincludes = []\n",
        ] {
            let message = message(text);
            assert!(message.contains("rule `mine`"), "{message}");
            assert!(message.contains("includes"), "{message}");
        }
    }

    /// A nameless rule cannot be named back, so the message locates it by
    /// position instead.
    #[test]
    fn a_nameless_rule_is_refused_by_position() {
        let message = message("[[rules]]\nroot = \"*\"\nincludes = [\"**/x/\"]\n");

        assert!(message.contains("rule #1"), "{message}");
        assert!(message.contains("name"), "{message}");
    }

    /// Names are what the report and the plan refer to, so two rules cannot
    /// share one.
    #[test]
    fn a_duplicate_name_is_refused() {
        let message = message(
            r#"
            [[rules]]
            name = "mine"
            root = "*"
            includes = ["**/a/"]

            [[rules]]
            name = "mine"
            root = "*"
            includes = ["**/b/"]
            "#,
        );

        assert!(message.contains("rule `mine`"), "{message}");
        assert!(message.contains("duplicate"), "{message}");
    }

    #[test]
    fn a_bad_size_or_duration_names_the_key_and_the_rule() {
        let size = message(
            "[[rules]]\nname = \"mine\"\nroot = \"*\"\nincludes = [\"x\"]\nmin-size = \"1Q\"\n",
        );
        assert!(
            size.contains("rule `mine`") && size.contains("min-size"),
            "{size}"
        );

        let age = message(
            "[[rules]]\nname = \"mine\"\nroot = \"*\"\nincludes = [\"x\"]\nolder-than = \"90\"\n",
        );
        assert!(
            age.contains("rule `mine`") && age.contains("older-than"),
            "{age}"
        );
    }

    /// A malformed `[report] min-size` is refused when the config is read, not
    /// carried around as an unparsed string to fail later.
    #[test]
    fn a_bad_report_size_is_refused_at_read_time() {
        let message = message("[report]\nmin-size = \"12x\"\n");

        assert!(message.contains("[report] min-size"), "{message}");
    }

    // ---- what is only warned about ---------------------------------------

    /// A typo or a setting from a newer version. Refusing to run over one would
    /// make the tool brittle and protect nothing, so it is named and skipped.
    #[test]
    fn an_unknown_key_warns_and_parsing_continues() {
        let config = at("[scan]\none-file-sistem = true\n").expect("must not fail");

        assert_eq!(config.warnings, vec!["scan.one-file-sistem".to_owned()]);
        assert_eq!(
            config.rules,
            builtin_rules(),
            "and the rest of the file still applies"
        );
    }

    #[test]
    fn an_unknown_key_inside_a_rule_is_reported_with_its_path() {
        let config =
            at("[[rules]]\nname = \"mine\"\nroot = \"*\"\nincludes = [\"x\"]\ntyer = \"auto\"\n")
                .expect("must not fail");

        assert_eq!(config.warnings, vec!["rules.0.tyer".to_owned()]);
        assert_eq!(
            config.rules[0].tier,
            Tier::Confirm,
            "the misspelt key did not silently become a tier"
        );
    }

    // ---- init ------------------------------------------------------------

    #[test]
    fn init_writes_a_file_that_parses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("nested").join("config.toml");

        init(&target, false).expect("write");

        let text = std::fs::read_to_string(&target).expect("read back");
        assert_eq!(rules_of(&text), builtin_rules());
    }

    /// A config is something a user edits. "Show me the defaults" must not be
    /// able to throw those edits away.
    #[test]
    fn init_refuses_to_overwrite_without_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("config.toml");
        std::fs::write(&target, "# mine\n").expect("seed");

        let err = init(&target, false).expect_err("must refuse");
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("read"),
            "# mine\n",
            "and the file is untouched"
        );

        init(&target, true).expect("forced write");
        assert!(
            std::fs::read_to_string(&target)
                .expect("read")
                .contains("[[rules]]")
        );
    }

    // ---- load ------------------------------------------------------------

    /// The tool ships working defaults, so no config is an ordinary state.
    #[test]
    fn an_absent_default_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = load(None, &UserDirs::default(), Some(dir.path().to_path_buf()))
            .expect("must not fail");

        assert_eq!(config.rules, builtin_rules());
    }

    /// A file that **exists but cannot be read** is not the same as one that is
    /// not there, and only the second may fall back to defaults. Mutation
    /// testing found this: replacing the `NotFound` guard with `true` left every
    /// test passing, which meant an unreadable config would silently become the
    /// built-in rules — and the user would be cleaning under rules they did not
    /// write, with nothing said.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_an_error_rather_than_a_fallback() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        // Where `locate` will actually look, given this as `$XDG_CONFIG_HOME`.
        let path = config_file(dir.path());
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
        std::fs::write(&path, DEFAULT_CONFIG).expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        // Privileges that ignore the mode would read it fine and make this pass
        // for the wrong reason.
        if std::fs::read_to_string(&path).is_ok() {
            eprintln!("skipping: privileges ignore the unreadable mode");
            return;
        }

        let result = load(None, &UserDirs::default(), Some(dir.path().to_path_buf()));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("restore");

        let err = result.expect_err("an unreadable config must not become the defaults");
        assert!(
            matches!(err, ConfigError::Read(..)),
            "and it must say so, not claim the file is absent: {err}"
        );
    }

    /// But a path the user named and that is not there is a typo, and defaults
    /// substituted for it would mean cleaning under rules they never wrote.
    #[test]
    fn an_absent_explicit_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.toml");

        let err = load(Some(&missing), &UserDirs::default(), None).expect_err("must fail");

        assert!(err.to_string().contains("nope.toml"), "{err}");
    }
}
