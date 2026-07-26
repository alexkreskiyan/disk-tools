//! The rules themselves, as data.
//!
//! v0.2 knew what was disposable through four hardcoded categories and a `match`.
//! A user could switch one off and nothing more — not see what `rust-target`
//! matched, not narrow it to a directory, not add one of their own. Here that
//! knowledge becomes a list of [`Rule`]s, and the four categories are five
//! ordinary entries in it ([`Rules::builtin`]).
//!
//! Two properties are load-bearing and neither is obvious from the types:
//!
//! **List order is precedence.** The first rule that claims a node wins, which
//! is what reproduces v0.2's "a junk match beats the age rule" without a special
//! case: the junk rules simply come first. It falls out of taking the *lowest*
//! rule index among the patterns that matched.
//!
//! **A rule that cannot be expressed matches nothing.** A disabled rule, an
//! unresolvable `~`, a root that is not UTF-8 — each drops that rule and leaves
//! the others alone. Every one of those is a case where the honest answer is
//! "unknown", and for input to a delete operation the only safe reading of
//! unknown is *no*.
//!
//! Compilation happens once, in [`Rules::new`]. The measured budget is 285 ns
//! per node (`kb/benchmarks/2026.07/2026.07.26-detect-budget.md`) and a scan of
//! `~/Projects` visits 2.3 million of them, so the shape of that struct is not
//! an aesthetic choice — see its fields.

use crate::paths::is_within;
use globset::{Candidate, GlobBuilder, GlobSet, GlobSetBuilder};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where this user's own directories are.
///
/// Supplied by the frontend, never discovered here — the core consults no
/// environment. Every field is `Option` because a frontend may genuinely not
/// know, and a `None` resolves to **nothing**: a rule rooted at an unknown `~`
/// is dropped, never widened to "any home". That is the safe direction, and it
/// is also what makes every rule testable with a temporary directory standing in
/// for a home.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserDirs {
    /// `$HOME` / `%USERPROFILE%`.
    pub home: Option<PathBuf>,
    /// `%LOCALAPPDATA%` on Windows. `None` elsewhere.
    pub local_app_data: Option<PathBuf>,
    /// `%APPDATA%` — the *roaming* profile, on Windows. `None` elsewhere.
    ///
    /// Not derivable from [`Self::local_app_data`]: the two are siblings, but a
    /// roaming profile can be redirected to a network share independently. It is
    /// here because it is a **denylist** root, never a candidate one.
    pub app_data: Option<PathBuf>,
}

/// How eligible a candidate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Tier {
    /// Regenerable, so removable without per-item confirmation.
    Auto,
    /// Needs the user to say yes to this specific path.
    Confirm,
}

impl Default for Tier {
    /// **Confirm**, so a rule that forgets to say gets the cautious answer.
    ///
    /// v0.3 lets a user mark their own rule `auto`, which is a claim the tool
    /// cannot check. Making the *unstated* case ask is the least this can do.
    fn default() -> Self {
        Tier::Confirm
    }
}

/// One rule: where to look, what to claim there, and what that claim means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Unique, and the name a [`crate::Detection`] and the report carry.
    pub name: String,

    /// Where this rule applies. `None` means **wherever the scan goes**.
    ///
    /// The config schema requires it, so that `clean` with no path knows what to
    /// walk. The type does not, because the built-in rules genuinely have no
    /// root: a `node_modules` is regenerable wherever it is found, which is why
    /// v0.2 matched it by name alone.
    ///
    /// Written in token form — `~`, `%LOCALAPPDATA%`, `%APPDATA%` — and resolved
    /// against a supplied [`UserDirs`] in [`Rules::new`]. The core still reads no
    /// environment; it is handed one.
    pub root: Option<String>,

    /// Globs relative to [`Self::root`]. A trailing `/` means **directory only**,
    /// as in gitignore.
    pub includes: Vec<String>,

    /// Globs, likewise relative, that this rule declines to claim. The scan still
    /// walks and counts them — that is the difference between this and a scan
    /// exclusion, which v0.3 deliberately does not have.
    pub excludes: Vec<String>,

    /// Names that must **all** be present beside a match.
    ///
    /// This is the whole of `rust-target`'s safety: `target/` is an ordinary
    /// directory name, and the `Cargo.toml` next to it is the only evidence that
    /// this particular one is build output.
    pub requires_sibling: Vec<String>,

    /// Skip a match whose enclosing repository has uncommitted work.
    ///
    /// v0.2 decided this from the category (`is_build_output`); with open-ended
    /// rules there is no enum left to ask, so each rule states it.
    pub requires_clean_repo: bool,

    /// Claim only what has been untouched at least this long. The boundary is
    /// inclusive, and an entry whose timestamp the platform never recorded
    /// matches nothing.
    pub older_than: Option<Duration>,

    /// Do not offer a match smaller than this.
    ///
    /// Applied by [`crate::plan`], **not** here: a `__pycache__` below the
    /// threshold must stay one skipped candidate rather than becoming a hundred
    /// tiny ones once the pass descends into it.
    pub min_size: u64,

    pub tier: Tier,

    pub enabled: bool,
}

impl Default for Rule {
    fn default() -> Self {
        Rule {
            name: String::new(),
            root: None,
            includes: Vec::new(),
            excludes: Vec::new(),
            requires_sibling: Vec::new(),
            requires_clean_repo: false,
            older_than: None,
            min_size: 0,
            tier: Tier::default(),
            enabled: true,
        }
    }
}

/// A rule whose text cannot be compiled.
///
/// Distinct from a rule that is merely *dropped*: a bad glob is something the
/// user wrote and can fix, so it is reported rather than silently ignored. An
/// unresolvable `~` is not — that is the machine's answer, not the user's
/// mistake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError {
    pub rule: String,
    pub pattern: String,
    pub message: String,
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rule `{}`: invalid pattern `{}`: {}",
            self.rule, self.pattern, self.message
        )
    }
}

impl std::error::Error for RuleError {}

/// The rules, compiled once for the whole scan.
///
/// The layout is dictated by cost. Detection visits every node of the tree —
/// 2.3 million on a 171 GB `~/Projects` — so the two things that must not happen
/// per node are matching each rule separately and rebuilding the path's derived
/// forms per pattern. Hence **one** [`GlobSet`] across all rules, with a
/// pattern-index-to-rule-index map beside it, and a single [`Candidate`] built by
/// the caller and passed in.
#[derive(Clone, Default)]
pub struct Rules {
    /// Only the rules that survived compilation; index into this is what the
    /// `owner` maps hold, and the lowest such index wins.
    rules: Vec<Rule>,

    /// Resolved root per rule, positionally. `None` means unrooted.
    roots: Vec<Option<PathBuf>>,

    includes: GlobSet,
    include_owner: Vec<usize>,
    /// Whether the include pattern at this index had a trailing `/`. [`GlobSet`]
    /// cannot express "directory only", so the slash is stripped before
    /// compiling and remembered here.
    include_dir_only: Vec<bool>,

    excludes: GlobSet,
    exclude_owner: Vec<usize>,
}

/// Names only — the compiled sets have no useful `Debug`, and a rule list
/// printed in full would bury whatever else was being inspected.
impl fmt::Debug for Rules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rules")
            .field(
                "rules",
                &self.rules.iter().map(|r| &r.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Rules {
    /// Compile `rules` against this user's directories.
    ///
    /// Rules are kept in the order given, and that order is their precedence. A
    /// rule is **dropped** — matching nothing, leaving the rest untouched — when
    /// it is disabled, when its root token cannot be resolved, or when its
    /// resolved root is not UTF-8. Only a malformed glob is an error, because
    /// only a malformed glob is something the user can correct.
    pub fn new(rules: Vec<Rule>, dirs: &UserDirs) -> Result<Self, RuleError> {
        let mut compiled = Rules::default();
        let mut includes = GlobSetBuilder::new();
        let mut excludes = GlobSetBuilder::new();

        for rule in rules {
            if !rule.enabled {
                continue;
            }
            // Two different "cannot express this rule" cases, both answered the
            // same way. `None` here is never "match anything".
            let Some(root) = resolve_root(rule.root.as_deref(), dirs) else {
                continue;
            };
            let prefix = match root.as_deref().map(glob_prefix) {
                Some(Some(prefix)) => Some(prefix),
                // A root that exists but is not UTF-8: globset speaks `&str`, so
                // the rule cannot be compiled at all.
                Some(None) => continue,
                None => None,
            };

            let index = compiled.rules.len();
            for pattern in &rule.includes {
                let (bare, dir_only) = strip_dir_marker(pattern);
                let glob = compile(&rule.name, pattern, prefix.as_deref(), bare)?;
                includes.add(glob);
                compiled.include_owner.push(index);
                compiled.include_dir_only.push(dir_only);
            }
            for pattern in &rule.excludes {
                let (bare, _) = strip_dir_marker(pattern);
                let glob = compile(&rule.name, pattern, prefix.as_deref(), bare)?;
                excludes.add(glob);
                compiled.exclude_owner.push(index);
            }

            compiled.roots.push(root);
            compiled.rules.push(rule);
        }

        // Building cannot fail: every glob in it was accepted individually above.
        compiled.includes = includes.build().expect("globs already validated");
        compiled.excludes = excludes.build().expect("globs already validated");
        Ok(compiled)
    }

    /// The rules v0.3 ships, in precedence order.
    ///
    /// These are ordinary rules, not a privileged set — `config init` writes them
    /// out and a user may edit any of them. Three carry no root because they
    /// genuinely have none; the two cache rules do, and are dropped entirely when
    /// `dirs` cannot say where the user lives.
    ///
    /// Infallible: the patterns are literals in this file, so the only error
    /// [`Self::new`] can return cannot arise.
    pub fn builtin(dirs: &UserDirs) -> Self {
        Rules::new(builtin_rules(), dirs).expect("built-in globs are valid")
    }

    pub fn get(&self, name: &str) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.name == name)
    }

    /// Is this rule list empty of anything that could ever match?
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The resolved roots of the rules that survived compilation.
    ///
    /// What `clean` with no path walks (v0.3 Task 4), and what a caller needs to
    /// tell a user that their rules cover nothing.
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.roots.iter().filter_map(Option::as_deref)
    }

    pub(crate) fn rule_at(&self, index: usize) -> &Rule {
        &self.rules[index]
    }

    /// Can this directory's whole subtree be skipped?
    ///
    /// True only when every rule is rooted somewhere that neither contains `dir`
    /// nor lies beneath it. One comparison per directory replaces a glob match
    /// per file underneath it — with all rules rooted at `~/Projects/github`, a
    /// scan of `~` costs almost nothing to detect over.
    ///
    /// An unrooted rule applies everywhere, so its presence disables pruning
    /// outright.
    pub(crate) fn prunes(&self, dir: &Path) -> bool {
        !self.roots.iter().any(|root| match root {
            None => true,
            // Either the directory is inside the rule's territory, or the rule's
            // territory is somewhere below the directory and still to be reached.
            Some(root) => is_within(dir, root) || is_within(root, dir),
        })
    }

    /// Indices of the rules whose includes match, lowest first, deduplicated.
    ///
    /// Lowest first *is* the precedence order, and it is why the caller can walk
    /// this list and stop at the first rule whose other predicates also hold.
    pub(crate) fn matching(&self, candidate: &Candidate<'_>, is_dir: bool) -> Vec<usize> {
        let mut owners: Vec<usize> = self
            .includes
            .matches_candidate(candidate)
            .into_iter()
            // A pattern written with a trailing `/` must not claim a *file* of
            // that name — `node_modules` as a file is not a dependency tree.
            .filter(|pattern| is_dir || !self.include_dir_only[*pattern])
            .map(|pattern| self.include_owner[pattern])
            .collect();
        owners.sort_unstable();
        owners.dedup();
        owners
    }

    /// Does rule `index` decline this path?
    pub(crate) fn excluded(&self, index: usize, candidate: &Candidate<'_>) -> bool {
        self.excludes
            .matches_candidate(candidate)
            .into_iter()
            .any(|pattern| self.exclude_owner[pattern] == index)
    }
}

/// The rule `--older-than` adds, by the name the report prints.
///
/// Not a built-in: an always-on age rule would bury the safe-list candidates
/// that are the point of the feature under confirm-tier noise on the very first
/// run. It is appended **last**, so any junk rule that also matches claims the
/// node first — which is what v0.2's "a junk match beats the age rule" meant.
///
/// Confirm tier, always: "you have not touched this in a while" is not evidence
/// that it regenerates.
pub fn age_rule(older_than: Duration) -> Rule {
    Rule {
        name: "old".into(),
        includes: vec!["**".into()],
        older_than: Some(older_than),
        tier: Tier::Confirm,
        ..Rule::default()
    }
}

/// The five shipped rules, in precedence order.
///
/// Public so that Task 2's `config init` renders **this** list rather than a
/// second copy of it that could drift.
pub fn builtin_rules() -> Vec<Rule> {
    vec![
        Rule {
            name: "rust-target".into(),
            includes: vec!["**/target/".into()],
            // Without the manifest this is an ordinary directory that happens to
            // share a name with a build one.
            requires_sibling: vec!["Cargo.toml".into()],
            requires_clean_repo: true,
            tier: Tier::Auto,
            ..Rule::default()
        },
        Rule {
            name: "node-modules".into(),
            includes: vec!["**/node_modules/".into()],
            tier: Tier::Auto,
            ..Rule::default()
        },
        Rule {
            name: "pycache".into(),
            includes: vec!["**/__pycache__/".into(), "**/*.pyc".into()],
            tier: Tier::Auto,
            ..Rule::default()
        },
        // The tilde is the entire safety of this one: `~/Library/Caches` is
        // regenerable user data and `/Library/Caches` is on the denylist. v0.2
        // kept the two apart in code; here the distinction is visible as data.
        //
        // No `**`: the cache *root* is the candidate, never its contents
        // individually — which is what `same_path` used to say.
        Rule {
            name: "user-caches".into(),
            root: Some("~".into()),
            includes: vec![".cache/".into(), "Library/Caches/".into()],
            tier: Tier::Auto,
            ..Rule::default()
        },
        // Separate from `user-caches` because it has a different root, and one
        // rule cannot have two. v0.2 folded both into a single category, which
        // hid the fact that they are anchored differently.
        Rule {
            name: "windows-temp".into(),
            root: Some("%LOCALAPPDATA%".into()),
            includes: vec!["Temp/".into()],
            tier: Tier::Auto,
            ..Rule::default()
        },
    ]
}

/// `Some(None)` for an unrooted rule, `Some(Some(path))` for a resolved one, and
/// `None` when a token names a directory this frontend could not find.
///
/// The last case is why an unknown home matches nothing rather than everything.
fn resolve_root(root: Option<&str>, dirs: &UserDirs) -> Option<Option<PathBuf>> {
    let Some(root) = root else {
        return Some(None);
    };

    let expanded = match split_token(root) {
        Some(("~", rest)) => dirs.home.as_ref()?.join(rest),
        Some(("%LOCALAPPDATA%", rest)) => dirs.local_app_data.as_ref()?.join(rest),
        Some(("%APPDATA%", rest)) => dirs.app_data.as_ref()?.join(rest),
        Some((_, _)) | None => PathBuf::from(root),
    };
    Some(Some(expanded))
}

/// Split a leading `~` / `%VAR%` token from the rest of the path, if the path
/// starts with one. The remainder is relative and may be empty.
fn split_token(root: &str) -> Option<(&str, &str)> {
    let end = root.find(['/', '\\']).unwrap_or(root.len());
    let (token, rest) = root.split_at(end);
    if token == "~" || (token.starts_with('%') && token.ends_with('%') && token.len() > 2) {
        Some((token, rest.trim_start_matches(['/', '\\'])))
    } else {
        None
    }
}

/// A resolved root as a glob prefix, or `None` if it is not UTF-8.
///
/// Separators are normalised to `/` because that is what globset's patterns use;
/// its [`Candidate`] normalises the path being tested the same way on Windows.
fn glob_prefix(root: &Path) -> Option<String> {
    let text = root.to_str()?;
    let text = text.replace('\\', "/");
    Some(text.trim_end_matches('/').to_owned())
}

/// Strip a trailing `/`, reporting whether there was one.
fn strip_dir_marker(pattern: &str) -> (&str, bool) {
    match pattern.strip_suffix('/') {
        Some(bare) => (bare, true),
        None => (pattern, false),
    }
}

fn compile(
    rule: &str,
    original: &str,
    prefix: Option<&str>,
    bare: &str,
) -> Result<globset::Glob, RuleError> {
    let pattern = match prefix {
        Some(prefix) => format!("{prefix}/{bare}"),
        None => bare.to_owned(),
    };

    GlobBuilder::new(&pattern)
        // The same rule `paths::eq_os` follows: the filesystem is
        // case-insensitive on Windows and may not be anywhere else, and folding
        // where it does not would put a directory the user never named up for
        // deletion.
        .case_insensitive(cfg!(windows))
        .build()
        .map_err(|err| RuleError {
            rule: rule.to_owned(),
            pattern: original.to_owned(),
            message: err.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(home: &str) -> UserDirs {
        UserDirs {
            home: Some(PathBuf::from(home)),
            ..UserDirs::default()
        }
    }

    fn rule(name: &str, includes: &[&str]) -> Rule {
        Rule {
            name: name.into(),
            includes: includes.iter().map(|s| (*s).to_owned()).collect(),
            ..Rule::default()
        }
    }

    /// Which rules match, by name — the shape every test below asserts on.
    fn matches(rules: &Rules, path: &str, is_dir: bool) -> Vec<String> {
        let candidate = Candidate::new(path);
        rules
            .matching(&candidate, is_dir)
            .into_iter()
            .map(|index| rules.rule_at(index).name.clone())
            .collect()
    }

    /// The property the whole design rests on: precedence is the order the rules
    /// were written in, with no tie-breaking rule anywhere else.
    #[test]
    fn the_earlier_rule_wins() {
        let rules = Rules::new(
            vec![rule("first", &["**/x/"]), rule("second", &["**/x/"])],
            &UserDirs::default(),
        )
        .expect("compile");

        assert_eq!(matches(&rules, "/p/x", true), vec!["first", "second"]);
        assert_eq!(
            rules.matching(&Candidate::new("/p/x"), true).first(),
            Some(&0),
            "the caller takes the lowest index, which is the earlier rule"
        );
    }

    #[test]
    fn a_disabled_rule_is_not_compiled_at_all() {
        let rules = Rules::new(
            vec![
                Rule {
                    enabled: false,
                    ..rule("off", &["**/x/"])
                },
                rule("on", &["**/x/"]),
            ],
            &UserDirs::default(),
        )
        .expect("compile");

        assert_eq!(matches(&rules, "/p/x", true), vec!["on"]);
        assert!(rules.get("off").is_none(), "and it is not even listed");
    }

    /// An unknown home is never "any home". The rule that needed it drops; the
    /// rule beside it is untouched.
    #[test]
    fn an_unresolvable_token_drops_only_its_own_rule() {
        let rules = Rules::new(
            vec![
                Rule {
                    root: Some("~".into()),
                    ..rule("needs-home", &[".cache/"])
                },
                rule("rootless", &["**/x/"]),
            ],
            &UserDirs::default(),
        )
        .expect("compile");

        assert!(rules.get("needs-home").is_none());
        assert_eq!(matches(&rules, "/p/x", true), vec!["rootless"]);
        assert!(
            matches(&rules, "/home/me/.cache", true).is_empty(),
            "with no home there is nothing for the cache rule to match"
        );
    }

    #[test]
    fn a_resolved_token_anchors_the_pattern() {
        let rules = Rules::new(
            vec![Rule {
                root: Some("~".into()),
                ..rule("caches", &[".cache/"])
            }],
            &dirs("/home/me"),
        )
        .expect("compile");

        assert_eq!(matches(&rules, "/home/me/.cache", true), vec!["caches"]);
        assert!(
            matches(&rules, "/other/.cache", true).is_empty(),
            "the root is part of the pattern, not a hint"
        );
    }

    /// The gitignore convention, and the reason `include_dir_only` exists at all.
    #[test]
    fn a_trailing_slash_refuses_a_file_of_that_name() {
        let rules = Rules::new(
            vec![rule("dirs", &["**/node_modules/"])],
            &UserDirs::default(),
        )
        .expect("compile");

        assert_eq!(matches(&rules, "/p/node_modules", true), vec!["dirs"]);
        assert!(
            matches(&rules, "/p/node_modules", false).is_empty(),
            "a file named node_modules is not a dependency tree"
        );
    }

    #[test]
    fn without_a_trailing_slash_both_kinds_match() {
        let rules =
            Rules::new(vec![rule("any", &["**/*.pyc"])], &UserDirs::default()).expect("compile");

        assert_eq!(matches(&rules, "/p/stale.pyc", false), vec!["any"]);
        assert_eq!(matches(&rules, "/p/stale.pyc", true), vec!["any"]);
    }

    #[test]
    fn excludes_are_scoped_to_their_own_rule() {
        let rules = Rules::new(
            vec![
                Rule {
                    excludes: vec!["**/vendor/**".into()],
                    ..rule("narrow", &["**/node_modules/"])
                },
                rule("wide", &["**/node_modules/"]),
            ],
            &UserDirs::default(),
        )
        .expect("compile");

        let candidate = Candidate::new("/p/vendor/a/node_modules");
        assert!(rules.excluded(0, &candidate), "the rule that declared it");
        assert!(
            !rules.excluded(1, &candidate),
            "and only that rule — the other still claims it"
        );
    }

    /// The user's own text, so it is reported rather than silently dropped.
    #[test]
    fn a_malformed_glob_names_the_rule_and_the_pattern() {
        let err = Rules::new(vec![rule("broken", &["**/["])], &UserDirs::default())
            .expect_err("an unclosed class must not compile");

        assert_eq!(err.rule, "broken");
        assert_eq!(err.pattern, "**/[");
        assert!(!err.message.is_empty());
        assert!(
            err.to_string().contains("broken") && err.to_string().contains("**/["),
            "the message must name both: {err}"
        );
    }

    /// The error carries the pattern **as written**, trailing slash included —
    /// a message quoting a pattern the user cannot find in their file is worse
    /// than no message.
    #[test]
    fn the_reported_pattern_is_the_one_the_user_wrote() {
        let err = Rules::new(vec![rule("broken", &["**/[/"])], &UserDirs::default())
            .expect_err("still malformed once the marker is stripped");

        assert_eq!(err.pattern, "**/[/");
    }

    /// A home that is not UTF-8 cannot become a glob, and guessing is not an
    /// option when the output feeds a delete.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_root_drops_the_rule_rather_than_panicking() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let home = PathBuf::from(OsString::from_vec(vec![b'/', 0xff, 0xfe]));
        let rules = Rules::new(
            vec![
                Rule {
                    root: Some("~".into()),
                    ..rule("caches", &[".cache/"])
                },
                rule("rootless", &["**/x/"]),
            ],
            &UserDirs {
                home: Some(home),
                ..UserDirs::default()
            },
        )
        .expect("compile");

        assert!(rules.get("caches").is_none());
        assert_eq!(matches(&rules, "/p/x", true), vec!["rootless"]);
    }

    // ---- pruning ---------------------------------------------------------

    #[test]
    fn an_unrooted_rule_disables_pruning() {
        let rules =
            Rules::new(vec![rule("anywhere", &["**/x/"])], &UserDirs::default()).expect("compile");

        assert!(!rules.prunes(Path::new("/anything/at/all")));
    }

    #[test]
    fn a_subtree_outside_every_root_is_pruned() {
        let rules = Rules::new(
            vec![Rule {
                root: Some("~/Projects".into()),
                ..rule("scoped", &["**/target/"])
            }],
            &dirs("/home/me"),
        )
        .expect("compile");

        assert!(rules.prunes(Path::new("/home/me/Downloads")));
        assert!(rules.prunes(Path::new("/var/log")));
    }

    /// Both directions matter. Inside the root the rule may fire; *above* it the
    /// walk still has to get there, and pruning would cut the path to it.
    #[test]
    fn the_root_and_its_ancestors_are_never_pruned() {
        let rules = Rules::new(
            vec![Rule {
                root: Some("~/Projects".into()),
                ..rule("scoped", &["**/target/"])
            }],
            &dirs("/home/me"),
        )
        .expect("compile");

        assert!(!rules.prunes(Path::new("/home/me/Projects")));
        assert!(!rules.prunes(Path::new("/home/me/Projects/deep/inside")));
        assert!(
            !rules.prunes(Path::new("/home/me")),
            "an ancestor of the root"
        );
        assert!(!rules.prunes(Path::new("/")), "and the way to it");
    }

    #[test]
    fn no_rules_prunes_everything() {
        let rules = Rules::default();

        assert!(rules.prunes(Path::new("/anything")));
        assert!(rules.is_empty());
    }

    // ---- the built-ins ---------------------------------------------------

    #[test]
    fn the_builtins_are_in_precedence_order() {
        let rules = Rules::builtin(&dirs("/home/me"));

        let names: Vec<_> = ["rust-target", "node-modules", "pycache", "user-caches"]
            .into_iter()
            .filter(|name| rules.get(name).is_some())
            .collect();
        assert_eq!(
            names,
            vec!["rust-target", "node-modules", "pycache", "user-caches"]
        );
    }

    /// The cache rules are the only rooted built-ins, so without a home the other
    /// three still work — which is exactly what v0.2 did.
    #[test]
    fn without_a_home_only_the_rootless_builtins_survive() {
        let rules = Rules::builtin(&UserDirs::default());

        assert!(rules.get("node-modules").is_some());
        assert!(rules.get("pycache").is_some());
        assert!(rules.get("rust-target").is_some());
        assert!(rules.get("user-caches").is_none());
        assert!(rules.get("windows-temp").is_none());
    }

    #[test]
    fn the_builtin_safe_list_is_auto_tier() {
        let rules = Rules::builtin(&dirs("/home/me"));

        for name in ["rust-target", "node-modules", "pycache", "user-caches"] {
            assert_eq!(
                rules.get(name).expect(name).tier,
                Tier::Auto,
                "{name} is regenerable"
            );
        }
    }

    /// A rule that says nothing about its tier must ask, not assume.
    #[test]
    fn an_unstated_tier_is_confirm() {
        assert_eq!(Rule::default().tier, Tier::Confirm);
        assert_eq!(Tier::default(), Tier::Confirm);
    }

    #[test]
    fn only_rust_target_wants_a_clean_repository() {
        let rules = Rules::builtin(&dirs("/home/me"));

        assert!(
            rules
                .get("rust-target")
                .expect("present")
                .requires_clean_repo
        );
        for name in ["node-modules", "pycache", "user-caches"] {
            assert!(
                !rules.get(name).expect(name).requires_clean_repo,
                "{name} is not produced from a working tree"
            );
        }
    }

    /// `~/Library/Caches` is a candidate and `/Library/Caches` is denied, and the
    /// only thing telling them apart is that the rule is anchored to a known
    /// home. This is that anchoring, asserted directly.
    #[test]
    fn the_cache_rule_matches_only_under_the_home_it_was_given() {
        let rules = Rules::builtin(&dirs("/home/me"));

        assert_eq!(
            matches(&rules, "/home/me/Library/Caches", true),
            vec!["user-caches"]
        );
        assert!(
            matches(&rules, "/Library/Caches", true).is_empty(),
            "the system cache is not this user's"
        );
    }

    /// The cache *root* is the candidate; its contents are not offered
    /// separately. v0.2 said this with `same_path`, and the absent `**` says it
    /// here.
    #[test]
    fn the_cache_rule_does_not_match_inside_the_cache() {
        let rules = Rules::builtin(&dirs("/home/me"));

        assert!(matches(&rules, "/home/me/.cache/pip", true).is_empty());
    }

    #[test]
    fn roots_lists_only_the_rooted_rules() {
        let rules = Rules::builtin(&dirs("/home/me"));

        assert_eq!(
            rules.roots().collect::<Vec<_>>(),
            vec![Path::new("/home/me")],
            "only user-caches is rooted when there is no %LOCALAPPDATA%"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_root_matches_whatever_its_case() {
        let rules = Rules::builtin(&UserDirs {
            local_app_data: Some(PathBuf::from(r"C:\Users\Me\AppData\Local")),
            ..UserDirs::default()
        });

        assert_eq!(
            matches(&rules, r"c:\users\me\appdata\local\temp", true),
            vec!["windows-temp"]
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn case_is_significant_off_windows() {
        let rules = Rules::builtin(&dirs("/home/me"));

        assert!(
            matches(&rules, "/home/me/.Cache", true).is_empty(),
            "`.Cache` is not `.cache` where the filesystem says so"
        );
    }
}
