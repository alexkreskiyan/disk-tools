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
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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

/// What `clean` does with what this rule claims.
///
/// Three answers to **one** question, which is why they are one field. v0.5
/// renamed them from `purge` / `auto` / `confirm`: `purge` named a destination
/// while `auto` named a ceremony, so the contrast between the two resolved on no
/// axis at all.
///
/// [`Tier::Purge`] is [`Tier::Trash`] plus "no undo" — the same claim of
/// regenerability, made harder. As a separate `purge = true` key beside a tier
/// it would be writable against `confirm`, a combination that has to be
/// rejected; as a third value it cannot be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
// Lowercase, so the word in `--json` is the word in the config file. A consumer
// reading `"Trash"` and writing `tier = "Trash"` back would find it refused.
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum Tier {
    /// Deleted outright, no confirmation and no trash.
    ///
    /// For what a command regenerates: the trash does not free space until it
    /// is emptied, so moving 20 GB of `node_modules` into it leaves a full disk
    /// full and adds a second, manual step.
    Purge,
    /// Moved to the OS trash, without per-item confirmation.
    Trash,
    /// Nothing, until the user says so.
    Confirm,
}

impl Tier {
    /// Does this need the user to agree to each path?
    ///
    /// The one question `--safe` and the `clean` refusal both ask, named rather
    /// than written as `== Tier::Confirm` at each site — so that a fourth tier
    /// could be added without hunting for the comparisons that would then be
    /// wrong.
    pub fn needs_confirming(self) -> bool {
        self == Tier::Confirm
    }
}

impl Default for Tier {
    /// **Confirm**, so a rule that forgets to say gets the cautious answer.
    ///
    /// v0.3 lets a user mark their own rule as needing no confirmation, which is
    /// a claim the tool cannot check. Making the *unstated* case ask is the
    /// least this can do.
    fn default() -> Self {
        Tier::Confirm
    }
}

/// One self-contained statement about what qualifies.
///
/// A rule used to be a root plus three independent lists, and its meaning was
/// their cross product — so the pairing between a pattern and the marker that
/// justifies it was lost. `requires` is matched **all**, which made "a `bin/`
/// beside a `*.csproj` **or** beside a `*.fsproj`" unsayable in one rule. A part
/// is that pairing: everything deciding whether a node qualifies, in one place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Part {
    /// Where this part applies. `None` means **wherever the scan goes**.
    ///
    /// The config schema requires it, so that `clean` with no path knows what to
    /// walk. The type does not, because the built-in rules genuinely have no
    /// root: a `node_modules` is regenerable wherever it is found.
    ///
    /// Written in token form — `~`, `%LOCALAPPDATA%`, `%APPDATA%` — and resolved
    /// against a supplied [`UserDirs`] in [`Rules::new`]. The core still reads no
    /// environment; it is handed one.
    pub root: Option<String>,

    /// Globs relative to [`Self::root`]. A trailing `/` means **directory only**,
    /// as in gitignore.
    pub includes: Vec<String>,

    /// Globs, likewise relative, that this part declines to claim. The scan still
    /// walks and counts them — that is the difference between this and a scan
    /// exclusion, which this project deliberately does not have.
    pub excludes: Vec<String>,

    /// Globs that must **each** find something beside a match.
    ///
    /// This is the whole of `rust-target`'s safety: `target/` is an ordinary
    /// directory name, and the `Cargo.toml` next to it is the only evidence that
    /// this particular one is build output.
    ///
    /// Matched against the sibling's **file name alone**, and globs rather than
    /// names because most build systems do not offer a fixed one: the file that
    /// proves a `bin/` is .NET output is `Whatever.csproj`. A pattern with no
    /// metacharacters matches itself, so `Cargo.toml` still means `Cargo.toml`.
    pub requires: Vec<String>,

    /// Skip a match whose enclosing repository has uncommitted work.
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
}

/// One rule: a name, what its claim means, and the parts that make it.
///
/// A node is claimed when it satisfies **any** part. The parts carry the
/// matching; the rule carries identity and consequence, which is why a tier
/// lives here and not there — a tier on a part would make the part a rule, and
/// the rule would stop being the unit the report groups by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Unique, and the name a [`crate::Detection`] and the report carry.
    pub name: String,

    pub tier: Tier,

    pub enabled: bool,

    /// Satisfying any of them is satisfying the rule.
    pub parts: Vec<Part>,
}

impl Default for Rule {
    fn default() -> Self {
        Rule {
            name: String::new(),
            tier: Tier::default(),
            enabled: true,
            parts: Vec::new(),
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
    /// Only the rules that survived compilation.
    rules: Vec<Rule>,

    /// Every part of every surviving rule, flattened, in (rule, part) order —
    /// which is precedence order, so the **lowest index wins** exactly as it did
    /// when the index named a rule. Each entry is which rule the part belongs to
    /// and which part of it this is.
    owner: Vec<(usize, usize)>,

    /// Resolved root per **part**, positionally. `None` means unrooted.
    roots: Vec<Option<PathBuf>>,

    includes: GlobSet,
    include_owner: Vec<usize>,
    /// Whether the include pattern at this index had a trailing `/`. [`GlobSet`]
    /// cannot express "directory only", so the slash is stripped before
    /// compiling and remembered here.
    include_dir_only: Vec<bool>,

    excludes: GlobSet,
    exclude_owner: Vec<usize>,

    /// Each part's `requires`, compiled, positionally.
    ///
    /// Separate matchers rather than one [`GlobSet`], because these are `all`
    /// and not `any`: two required siblings are two questions, and a set would
    /// only be able to say that *something* matched.
    ///
    /// Nested and usually empty, which costs one `Vec` header per rule — the
    /// rules are compiled once and there are a handful of them.
    requires: Vec<Vec<globset::GlobMatcher>>,
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
            let rule_index = compiled.rules.len();
            let before = compiled.owner.len();

            for (part_index, part) in rule.parts.iter().enumerate() {
                // Two different "cannot express this" cases, both answered the
                // same way. `None` here is never "match anything" — and it drops
                // **this part**, leaving the rule's others standing.
                let Some(root) = resolve_root(part.root.as_deref(), dirs) else {
                    continue;
                };
                let prefix = match root.as_deref().map(glob_prefix) {
                    Some(Some(prefix)) => Some(prefix),
                    // A root that exists but is not UTF-8: globset speaks `&str`,
                    // so the part cannot be compiled at all.
                    Some(None) => continue,
                    None => None,
                };

                let index = compiled.owner.len();
                for pattern in &part.includes {
                    let (bare, dir_only) = strip_dir_marker(pattern);
                    let glob = compile(&rule.name, pattern, prefix.as_deref(), bare)?;
                    includes.add(glob);
                    compiled.include_owner.push(index);
                    compiled.include_dir_only.push(dir_only);
                }
                for pattern in &part.excludes {
                    let (bare, _) = strip_dir_marker(pattern);
                    let glob = compile(&rule.name, pattern, prefix.as_deref(), bare)?;
                    excludes.add(glob);
                    compiled.exclude_owner.push(index);
                }

                // No root prefix: these are matched against a bare file name, not
                // against a path. `*` therefore cannot reach past the name either,
                // which is what makes `*.csproj` mean "beside", not "anywhere under".
                let mut required = Vec::with_capacity(part.requires.len());
                for pattern in &part.requires {
                    required.push(compile(&rule.name, pattern, None, pattern)?.compile_matcher());
                }

                compiled.requires.push(required);
                compiled.roots.push(root);
                compiled.owner.push((rule_index, part_index));
            }

            // A rule none of whose parts survived is dropped whole. Keeping it
            // would put a name in `names()` and `get()` that can never match
            // anything — which is precisely the "my rule is not running" state
            // the browser has to be able to tell from "my rule does not cover
            // this", and it could not if the rule were still listed.
            if compiled.owner.len() == before {
                continue;
            }
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
    /// Every rule's name, in list order — which is precedence order, and so the
    /// order worth showing.
    pub fn names(&self) -> Vec<String> {
        self.rules.iter().map(|rule| rule.name.clone()).collect()
    }

    /// The rules themselves, to be edited and recompiled.
    ///
    /// Only the ones that survived compilation: a rule dropped for an
    /// unresolvable root is not here, and handing it back would let a browser
    /// silently rewrite the file without it.
    pub fn to_vec(&self) -> Vec<Rule> {
        self.rules.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// What `clean` with no path walks: the resolved roots of the rules that
    /// survived compilation, sorted, with any root that lies inside another
    /// dropped.
    ///
    /// Empty when no rule names a directory — every rule unrooted (`root = "*"`
    /// in the file), disabled, or dropped for a token that would not resolve.
    /// The caller has to say so rather than report an empty plan, which would
    /// read as "nothing to clean".
    pub fn scan_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<&Path> = self.roots.iter().filter_map(Option::as_deref).collect();
        // Sorted so a containing root always precedes what it contains, and so
        // the answer does not depend on the order rules were written in.
        roots.sort_unstable();
        roots.dedup();

        let mut merged: Vec<PathBuf> = Vec::with_capacity(roots.len());
        for root in roots {
            // Dropping a nested root is not tidiness. Walking `~` and
            // `~/Projects` both would put every candidate under the latter into
            // the plan twice, and `reclaimable` would report double what
            // removing them frees.
            if merged.iter().any(|kept| is_within(root, kept)) {
                continue;
            }
            merged.push(root.to_path_buf());
        }
        merged
    }

    /// The rule a compiled part belongs to.
    pub(crate) fn rule_at(&self, part: usize) -> &Rule {
        &self.rules[self.owner[part].0]
    }

    /// The compiled part itself — where every predicate that decided the match
    /// lives, including the two `plan` reads afterwards.
    pub(crate) fn part_at(&self, part: usize) -> &Part {
        let (rule, index) = self.owner[part];
        &self.rules[rule].parts[index]
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

    /// Do rule `index`'s non-glob predicates hold for this node?
    ///
    /// Shared with [`detect`](crate::detect) rather than duplicated, so a colour
    /// on screen and a candidate in a plan cannot come to different conclusions
    /// about the same directory. Every predicate here is answered from facts the
    /// caller already has: `requires_sibling` from the directory listing it just
    /// read, `older_than` from the entry's own mtime. Nothing here touches the
    /// filesystem.
    ///
    /// `requires_clean_repo` is **not** among them. It is not a question about
    /// whether a rule claims a path — `detect` claims it either way — but about
    /// whether `clean` will act on the claim, and answering it costs a git
    /// probe per repository.
    pub(crate) fn predicates_hold(&self, index: usize, facts: &Facts<'_>) -> bool {
        // `all`, so two required siblings are two questions: each pattern has to
        // find something of its own.
        if !self.requires[index]
            .iter()
            .all(|wanted| (facts.any_sibling)(&|name| wanted.is_match(name)))
        {
            return false;
        }
        !self
            .part_at(index)
            .older_than
            .is_some_and(|older_than| !is_older(facts.modified, older_than, facts.now))
    }

    /// Does any rule in force ask about the names beside a path?
    ///
    /// For callers that would have to read a directory to answer — most rule
    /// sets never ask, and a listing read to answer a question nobody posed is
    /// a listing read for nothing.
    pub fn wants_siblings(&self) -> bool {
        self.requires.iter().any(|wanted| !wanted.is_empty())
    }

    /// What the rules say about one path, for showing rather than for deciding.
    ///
    /// [`detect`](crate::detect) answers "may this be removed"; this answers
    /// "why is that", which is a different question with a different set of
    /// answers. In particular **[`State::InScope`] is its own state**: a path
    /// under a rule's root that nothing matches is not the same as a path no
    /// rule has ever heard of, and folding the two together would leave a user
    /// unable to tell "my rule does not cover this" from "my rule is not
    /// running".
    ///
    /// Precedence is the list order, as everywhere else: the first rule whose
    /// includes match and whose excludes do not is the one that speaks.
    ///
    /// A rule dropped at compile time — disabled, an unresolvable `~` — is not
    /// here at all, so paths under its intended root read as
    /// [`State::Untracked`]. That is the honest answer: nothing is watching them.
    pub fn state(&self, path: &Path, facts: &Facts<'_>) -> State {
        let candidate = Candidate::new(path);

        // Lowest index first, and the same two tests in the same order as
        // `detect::claim`, so `Included` means exactly "detect would claim this".
        let matching = self.matching(&candidate, facts.is_dir);
        let mut declined = false;
        for index in &matching {
            if self.excluded(*index, &candidate) {
                declined = true;
                continue;
            }
            if self.predicates_hold(*index, facts) {
                return State::Included;
            }
            // A glob matched but a predicate did not — a `target/` with no
            // `Cargo.toml` beside it. The user did not ask for that to be left
            // alone, so it is not excluded; the rule simply does not reach it,
            // which is what `InScope` says.
        }
        if declined {
            return State::Excluded;
        }

        let governing: Vec<usize> = (0..self.owner.len())
            .filter(|index| match &self.roots[*index] {
                // An unrooted rule applies wherever the scan goes.
                None => true,
                Some(root) => is_within(path, root),
            })
            .collect();

        if governing.is_empty() {
            return State::Untracked;
        }
        // An `excludes` that matches without any `includes` matching still says
        // something: the user has named this path to be left alone.
        if governing
            .iter()
            .any(|index| self.excluded(*index, &candidate))
        {
            return State::Excluded;
        }
        State::InScope
    }
}

/// What the caller already knows about a path, for the predicates that need
/// more than the path itself.
///
/// A borrowed closure for the siblings rather than a list: `detect` has
/// [`ScanNode`](crate::ScanNode)s and the browser has its own rows, and neither
/// should have to build a third representation to ask a question.
pub struct Facts<'a> {
    pub is_dir: bool,
    pub modified: Option<SystemTime>,
    /// Supplied by the caller — this crate reads no clock.
    pub now: SystemTime,

    /// Is there an entry beside the path whose name this accepts?
    ///
    /// A predicate rather than a name, because `requires_sibling` is a glob and
    /// only this crate has it compiled. The caller supplies the listing; the
    /// rule supplies the question.
    pub any_sibling: AnySibling<'a>,
}

/// One rule's question about one file name.
pub type NameTest<'a> = &'a dyn Fn(&OsStr) -> bool;

/// The caller's listing, asked a question it did not have to know in advance.
pub type AnySibling<'a> = &'a dyn Fn(NameTest<'_>) -> bool;

/// Has this been untouched for at least `older_than`?
///
/// A directory is judged on its **own** mtime: a directory's timestamp moves
/// when its entries change, and that is precisely the "still in use" evidence
/// wanted.
fn is_older(modified: Option<SystemTime>, older_than: Duration, now: SystemTime) -> bool {
    // Absence of evidence is not evidence of age. Every entry on a filesystem
    // that reports no timestamp would otherwise become a deletion candidate.
    let Some(modified) = modified else {
        return false;
    };
    let Some(threshold) = now.checked_sub(older_than) else {
        return false;
    };

    // "Older or exactly equal" — the boundary is inclusive.
    modified <= threshold
}

/// What the rules say about a path.
///
/// Four states, not three: see [`Rules::state`] for why `InScope` is not folded
/// into either of its neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No rule's root contains it. Nothing is watching this.
    Untracked,
    /// Inside some rule's territory, matched by none of its patterns.
    InScope,
    /// A rule claims it — this is what `clean` would offer to remove.
    Included,
    /// A rule names it to be left alone.
    Excluded,
}

impl State {
    /// For a legend. Colour alone excludes anyone who cannot distinguish the
    /// colours, so every state has to have a word too.
    pub fn label(self) -> &'static str {
        match self {
            State::Untracked => "untracked",
            State::InScope => "in scope",
            State::Included => "included",
            State::Excluded => "excluded",
        }
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
        tier: Tier::Confirm,
        parts: vec![Part {
            includes: vec!["**".into()],
            older_than: Some(older_than),
            ..Part::default()
        }],
        ..Rule::default()
    }
}

/// The five shipped rules, in precedence order.
///
/// Public so that `config init` renders **this** list rather than a second copy
/// of it that could drift.
///
/// Each is one part, because each says one thing. `user-caches` shows what a
/// second part would be for: two roots under one name and one tier.
pub fn builtin_rules() -> Vec<Rule> {
    vec![
        Rule {
            name: "rust-target".into(),
            tier: Tier::Trash,
            parts: vec![Part {
                includes: vec!["**/target/".into()],
                // Without the manifest this is an ordinary directory that
                // happens to share a name with a build one.
                requires: vec!["Cargo.toml".into()],
                requires_clean_repo: true,
                ..Part::default()
            }],
            ..Rule::default()
        },
        Rule {
            name: "node-modules".into(),
            tier: Tier::Trash,
            parts: vec![Part {
                includes: vec!["**/node_modules/".into()],
                ..Part::default()
            }],
            ..Rule::default()
        },
        Rule {
            name: "pycache".into(),
            tier: Tier::Trash,
            parts: vec![Part {
                includes: vec!["**/__pycache__/".into(), "**/*.pyc".into()],
                ..Part::default()
            }],
            ..Rule::default()
        },
        // The tilde is the entire safety of this one: `~/Library/Caches` is
        // regenerable user data and `/Library/Caches` is on the denylist. v0.2
        // kept the two apart in code; here the distinction is visible as data.
        //
        // No `**`: the cache *root* is the candidate, never its contents
        // individually.
        Rule {
            name: "user-caches".into(),
            tier: Tier::Trash,
            parts: vec![Part {
                root: Some("~".into()),
                includes: vec![".cache/".into(), "Library/Caches/".into()],
                ..Part::default()
            }],
            ..Rule::default()
        },
        // A separate rule rather than a second part of `user-caches`: the two
        // are different claims about different platforms, and a report naming
        // one of them should not be able to mean the other.
        Rule {
            name: "windows-temp".into(),
            tier: Tier::Trash,
            parts: vec![Part {
                root: Some("%LOCALAPPDATA%".into()),
                includes: vec!["Temp/".into()],
                ..Part::default()
            }],
            ..Rule::default()
        },
    ]
}

/// Expand the tokens a user may write at the head of a path: `~`,
/// `%APPDATA%`, `%LOCALAPPDATA%`.
///
/// The same expansion a rule's `root` gets, exported so that every other place a
/// user writes a path — `keep-in`, so far — means the same thing by `~`. `None`
/// when the token names a directory this frontend could not find, which is the
/// project's usual reading of unknown: not everywhere, but nowhere.
pub fn user_path(text: &str, dirs: &UserDirs) -> Option<PathBuf> {
    resolve_root(Some(text), dirs).flatten()
}

/// `Some(None)` for an unrooted rule, `Some(Some(path))` for a resolved one, and
/// `None` when a token names a directory this frontend could not find.
///
/// The last case is why an unknown home matches nothing rather than everything.
fn resolve_root(root: Option<&str>, dirs: &UserDirs) -> Option<Option<PathBuf>> {
    let Some(root) = root else {
        return Some(None);
    };

    // The first path segment, and whatever follows it. Nothing checks whether
    // that segment *looks* like a token — a `%FOO%` this build knows nothing
    // about and a plain `Projects` both fall to the same arm below, so asking
    // the question could not change the answer. Mutation testing is what showed
    // that: three mutants of the shape test survived, all of them equivalent.
    let end = root.find(['/', '\\']).unwrap_or(root.len());
    let (token, rest) = root.split_at(end);
    let rest = rest.trim_start_matches(['/', '\\']);

    // `join("")` appends a separator, so a bare `~` would resolve to
    // `/home/me/`. Harmless to every comparison here — they are component-wise —
    // but the resolved root is also printed, and a stray trailing slash reads as
    // a mistake.
    let under = |base: &PathBuf| {
        if rest.is_empty() {
            base.clone()
        } else {
            base.join(rest)
        }
    };

    let expanded = match token {
        "~" => under(dirs.home.as_ref()?),
        "%LOCALAPPDATA%" => under(dirs.local_app_data.as_ref()?),
        "%APPDATA%" => under(dirs.app_data.as_ref()?),
        // Not a token this build knows: the whole thing is a literal path.
        _ => PathBuf::from(root),
    };
    Some(Some(expanded))
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

    /// A one-part rule, which is what most of these tests are about.
    fn rule(name: &str, includes: &[&str]) -> Rule {
        Rule {
            name: name.into(),
            parts: vec![Part {
                includes: includes.iter().map(|s| (*s).to_owned()).collect(),
                ..Part::default()
            }],
            ..Rule::default()
        }
    }

    /// A fixed "now" far enough from the epoch that subtracting cannot
    /// underflow.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_750_000_000)
    }

    /// The state of one path, with nothing beside it and no timestamp — the
    /// plain case the four-state tests are about.
    fn state(rules: &Rules, path: &str, is_dir: bool) -> State {
        rules.state(
            Path::new(path),
            &Facts {
                is_dir,
                modified: None,
                now: now(),
                any_sibling: &|_| false,
            },
        )
    }

    /// The state of one path that has `siblings` beside it.
    fn state_beside(rules: &Rules, path: &str, siblings: &[&str]) -> State {
        rules.state(
            Path::new(path),
            &Facts {
                is_dir: true,
                modified: None,
                now: now(),
                any_sibling: &|wanted| {
                    siblings
                        .iter()
                        .any(|name| wanted(std::ffi::OsStr::new(name)))
                },
            },
        )
    }

    /// A rooted rule, since every state but one is about roots.
    /// A rooted rule, since every state but one is about roots.
    fn rooted(name: &str, root: &str, includes: &[&str]) -> Rule {
        let mut base = rule(name, includes);
        base.parts[0].root = Some(root.to_owned());
        base
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
                {
                    let mut base = rule("needs-home", &[".cache/"]);
                    base.parts[0].root = Some("~".into());
                    base
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
            vec![{
                let mut base = rule("caches", &[".cache/"]);
                base.parts[0].root = Some("~".into());
                base
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
                {
                    let mut base = rule("narrow", &["**/node_modules/"]);
                    base.parts[0].excludes = vec!["**/vendor/**".into()];
                    base
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
                {
                    let mut base = rule("caches", &[".cache/"]);
                    base.parts[0].root = Some("~".into());
                    base
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
            vec![{
                let mut base = rule("scoped", &["**/target/"]);
                base.parts[0].root = Some("~/Projects".into());
                base
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
            vec![{
                let mut base = rule("scoped", &["**/target/"]);
                base.parts[0].root = Some("~/Projects".into());
                base
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

    /// All three tokens resolve, including `%APPDATA%` — which no built-in rule
    /// uses, since the roaming profile is a *denylist* root rather than a
    /// candidate one. It is still a token the config documents, so a user may
    /// root a rule there, and mutation testing found nothing exercising it.
    #[test]
    fn every_documented_token_resolves_from_user_dirs() {
        let dirs = UserDirs {
            home: Some(PathBuf::from("/home/me")),
            local_app_data: Some(PathBuf::from("/local")),
            app_data: Some(PathBuf::from("/roaming")),
        };

        for (token, resolved) in [
            ("~", "/home/me"),
            ("%LOCALAPPDATA%", "/local"),
            ("%APPDATA%", "/roaming"),
        ] {
            let rules = Rules::new(
                vec![{
                    let mut base = rule("t", &["x/"]);
                    base.parts[0].root = Some(format!("{token}/inner"));
                    base
                }],
                &dirs,
            )
            .expect("compile");

            assert_eq!(
                matches(&rules, &format!("{resolved}/inner/x"), true),
                vec!["t"],
                "`{token}` must expand to `{resolved}`"
            );
        }
    }

    /// And each is dropped on its own when the frontend could not find it —
    /// never widened to some other directory that happens to be known.
    #[test]
    fn a_token_whose_directory_is_unknown_drops_its_rule() {
        for token in ["~", "%LOCALAPPDATA%", "%APPDATA%"] {
            let rules = Rules::new(
                vec![{
                    let mut base = rule("t", &["x/"]);
                    base.parts[0].root = Some(token.into());
                    base
                }],
                &UserDirs::default(),
            )
            .expect("compile");

            assert!(rules.is_empty(), "`{token}` had nothing to resolve against");
        }
    }

    /// A path whose first segment is not a token this build knows is a literal,
    /// whatever it looks like. Nothing distinguishes `%FOO%` from `Projects`
    /// here, and nothing should: inventing an expansion for an unknown variable
    /// is guessing about where a delete rule points.
    #[test]
    fn an_unknown_token_is_a_literal_path() {
        for root in ["%FOO%", "%", "%%", "~sam", "Projects"] {
            let rules = Rules::new(
                vec![{
                    let mut base = rule("literal", &["x/"]);
                    base.parts[0].root = Some(root.into());
                    base
                }],
                &dirs("/home/me"),
            )
            .expect("compile");

            assert_eq!(
                matches(&rules, &format!("{root}/x"), true),
                vec!["literal"],
                "`{root}` must be taken as written"
            );
        }
    }

    /// The one place `is_empty` is load-bearing: a config whose rules all failed
    /// to resolve leaves nothing that could ever match, and the caller has to be
    /// able to say so rather than report a silent empty plan.
    #[test]
    fn rules_that_all_drop_leave_an_empty_set() {
        let rules = Rules::new(
            vec![{
                let mut base = rule("needs-home", &[".cache/"]);
                base.parts[0].root = Some("~".into());
                base
            }],
            &UserDirs::default(),
        )
        .expect("compile");

        assert!(rules.is_empty(), "every rule was dropped");
        assert!(
            !Rules::builtin(&UserDirs::default()).is_empty(),
            "and a set with anything in it is not empty"
        );
    }

    /// Confirm tier, and stated rather than inherited: "you have not touched
    /// this in a while" is not evidence that it regenerates, and a later change
    /// to `Rule::default` must not be able to make age matches auto by accident.
    #[test]
    fn the_age_rule_is_confirm_tier_by_its_own_statement() {
        let rule = age_rule(Duration::from_secs(1));

        assert_eq!(rule.tier, Tier::Confirm);
        assert_eq!(rule.name, "old");
        let part = &rule.parts[0];
        assert_eq!(part.older_than, Some(Duration::from_secs(1)));
        assert_eq!(part.root, None, "it applies wherever the scan goes");
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
                Tier::Trash,
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

        let guarded = |name: &str| {
            rules
                .get(name)
                .expect("present")
                .parts
                .iter()
                .any(|part| part.requires_clean_repo)
        };

        assert!(guarded("rust-target"));
        for name in ["node-modules", "pycache", "user-caches"] {
            assert!(!guarded(name), "{name} is not produced from a working tree");
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

    /// A bare `~` resolves to the home directory itself, with no trailing
    /// separator — the resolved root is printed to the user before a walk.
    #[test]
    fn a_bare_token_resolves_to_the_directory_itself() {
        let rules = Rules::new(
            vec![{
                let mut base = rule("t", &["x/"]);
                base.parts[0].root = Some("~".into());
                base
            }],
            &dirs("/home/me"),
        )
        .expect("compile");

        assert_eq!(rules.scan_roots(), vec![PathBuf::from("/home/me")]);
    }

    #[test]
    fn scan_roots_lists_only_the_rooted_rules() {
        let rules = Rules::builtin(&dirs("/home/me"));

        assert_eq!(
            rules.scan_roots(),
            vec![PathBuf::from("/home/me")],
            "only user-caches is rooted when there is no %LOCALAPPDATA%"
        );
    }

    /// The reason this merges rather than merely collects: walking `~` and
    /// `~/Projects` both would put every candidate under the latter into the
    /// plan twice, and the total would claim double what removing them frees.
    #[test]
    fn a_root_inside_another_is_dropped() {
        let rules = Rules::new(
            vec![
                {
                    let mut base = rule("inner", &["**/target/"]);
                    base.parts[0].root = Some("~/Projects".into());
                    base
                },
                {
                    let mut base = rule("outer", &["**/node_modules/"]);
                    base.parts[0].root = Some("~".into());
                    base
                },
                {
                    let mut base = rule("deeper", &["**/x/"]);
                    base.parts[0].root = Some("~/Projects/github".into());
                    base
                },
            ],
            &dirs("/home/me"),
        )
        .expect("compile");

        assert_eq!(
            rules.scan_roots(),
            vec![PathBuf::from("/home/me")],
            "the outermost root covers the other two"
        );
    }

    /// The component-wise comparison matters here as much as in the denylist:
    /// `/home/mine` shares a string prefix with `/home/min` and is not inside
    /// it, so dropping it would leave a configured directory unwalked.
    #[test]
    fn a_sibling_that_merely_shares_a_prefix_is_kept() {
        let rules = Rules::new(
            vec![
                {
                    let mut base = rule("a", &["**/x/"]);
                    base.parts[0].root = Some("/home/min".into());
                    base
                },
                {
                    let mut base = rule("b", &["**/y/"]);
                    base.parts[0].root = Some("/home/mine".into());
                    base
                },
            ],
            &UserDirs::default(),
        )
        .expect("compile");

        assert_eq!(
            rules.scan_roots(),
            vec![PathBuf::from("/home/min"), PathBuf::from("/home/mine")]
        );
    }

    /// Two rules on the same directory are one directory to walk.
    #[test]
    fn duplicate_roots_collapse() {
        let rules = Rules::new(
            vec![
                {
                    let mut base = rule("a", &["**/x/"]);
                    base.parts[0].root = Some("~".into());
                    base
                },
                {
                    let mut base = rule("b", &["**/y/"]);
                    base.parts[0].root = Some("~".into());
                    base
                },
            ],
            &dirs("/home/me"),
        )
        .expect("compile");

        assert_eq!(rules.scan_roots(), vec![PathBuf::from("/home/me")]);
    }

    /// Every rule unrooted: nothing names a directory, so there is nothing to
    /// walk without a path. The caller has to say so.
    #[test]
    fn unrooted_rules_contribute_no_scan_root() {
        let rules = Rules::builtin(&UserDirs::default());

        assert!(!rules.is_empty(), "three built-ins still compiled");
        assert!(
            rules.scan_roots().is_empty(),
            "but none of them names a directory to walk"
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

    /// The four states, on one rule, one at a time.
    #[test]
    fn a_path_is_untracked_in_scope_included_or_excluded() {
        let mut rule = rooted("junk", "~/Projects", &["**/target/"]);
        rule.parts[0].excludes = vec!["**/keep/**".into()];
        let rules = Rules::new(vec![rule], &dirs("/home/me")).expect("compiles");

        assert_eq!(
            state(&rules, "/home/me/Music/target", true),
            State::Untracked,
            "outside every root"
        );
        assert_eq!(
            state(&rules, "/home/me/Projects/src", true),
            State::InScope,
            "inside the root, matched by nothing"
        );
        assert_eq!(
            state(&rules, "/home/me/Projects/app/target", true),
            State::Included
        );
        assert_eq!(
            state(&rules, "/home/me/Projects/keep/target", true),
            State::Excluded
        );
    }

    /// The state that is easiest to get wrong by folding it into a neighbour.
    /// "My rule does not cover this" and "my rule is not running" are different
    /// problems, and only one of them is the user's fault.
    #[test]
    fn in_scope_is_a_state_of_its_own() {
        let rules = Rules::new(
            vec![rooted("junk", "~/Projects", &["**/target/"])],
            &dirs("/home/me"),
        )
        .expect("compiles");

        let inside = state(&rules, "/home/me/Projects/notes.md", false);
        let outside = state(&rules, "/home/me/notes.md", false);

        assert_eq!(inside, State::InScope);
        assert_eq!(outside, State::Untracked);
        assert_ne!(inside, outside);
    }

    /// A rule that could not be compiled is not there, so nothing is watching
    /// its territory. Reading that as an error would put a red row on a screen
    /// about a directory that is perfectly ordinary.
    #[test]
    fn a_dropped_rule_leaves_its_territory_untracked() {
        // No home directory, so `~/Projects` cannot resolve and the rule goes.
        let rules = Rules::new(
            vec![rooted("junk", "~/Projects", &["**/target/"])],
            &UserDirs::default(),
        )
        .expect("compiles: an unresolvable root drops the rule, it is not an error");

        assert!(rules.is_empty());
        assert_eq!(
            state(&rules, "/home/me/Projects/app/target", true),
            State::Untracked
        );
    }

    #[test]
    fn a_disabled_rule_leaves_its_territory_untracked() {
        let rules = Rules::new(
            vec![Rule {
                enabled: false,
                ..rooted("junk", "~/Projects", &["**/target/"])
            }],
            &dirs("/home/me"),
        )
        .expect("compiles");

        assert_eq!(
            state(&rules, "/home/me/Projects/app/target", true),
            State::Untracked
        );
    }

    /// List order is precedence here as everywhere: the first rule that claims a
    /// path and does not decline it is the one that speaks.
    #[test]
    fn the_first_rule_that_claims_it_without_declining_it_wins() {
        let mut first = rooted("first", "~/Projects", &["**/target/"]);
        first.parts[0].excludes = vec!["**/target/".into()];
        let second = rooted("second", "~/Projects", &["**/target/"]);

        let rules = Rules::new(vec![first, second], &dirs("/home/me")).expect("compiles");

        assert_eq!(
            state(&rules, "/home/me/Projects/app/target", true),
            State::Included,
            "the first declined it; the second did not"
        );
    }

    /// Naming a path in `excludes` says something even when no `includes`
    /// reaches it: the user has said to leave it alone.
    #[test]
    fn an_exclude_without_a_matching_include_still_reads_as_excluded() {
        let mut rule = rooted("junk", "~/Projects", &["**/target/"]);
        rule.parts[0].excludes = vec!["**/vendor/**".into()];
        let rules = Rules::new(vec![rule], &dirs("/home/me")).expect("compiles");

        assert_eq!(
            state(&rules, "/home/me/Projects/vendor/thing.c", false),
            State::Excluded
        );
    }

    /// An unrooted rule applies wherever the scan goes, so nothing under it is
    /// untracked.
    #[test]
    fn an_unrooted_rule_puts_everything_in_scope() {
        let rules =
            Rules::new(vec![rule("junk", &["**/target/"])], &dirs("/home/me")).expect("compiles");

        assert_eq!(state(&rules, "/anywhere/at/all", true), State::InScope);
        assert_eq!(state(&rules, "/anywhere/target", true), State::Included);
    }

    /// No rules at all is not a special case: nothing is watching anything.
    #[test]
    fn without_rules_everything_is_untracked() {
        let rules = Rules::default();

        assert_eq!(state(&rules, "/home/me/Projects", true), State::Untracked);
    }

    /// The gitignore convention `matching` already honours has to hold here too,
    /// or a file called `target` would be coloured as a build directory.
    #[test]
    fn a_directory_only_pattern_does_not_claim_a_file() {
        let rules = Rules::new(
            vec![rooted("junk", "~/Projects", &["**/target/"])],
            &dirs("/home/me"),
        )
        .expect("compiles");

        assert_eq!(
            state(&rules, "/home/me/Projects/app/target", false),
            State::InScope,
            "a file of that name is not a build directory"
        );
    }

    /// The inconsistency this exists to prevent: a `target/` with no
    /// `Cargo.toml` beside it is coloured as junk while `clean` would not touch
    /// it. `Included` has to mean "detect would claim this", not "a glob
    /// matched".
    #[test]
    fn a_glob_match_whose_predicate_fails_is_in_scope_not_included() {
        let rules = Rules::new(
            vec![{
                let mut base = rooted("rust-target", "~/Projects", &["**/target/"]);
                base.parts[0].requires = vec!["Cargo.toml".into()];
                base
            }],
            &dirs("/home/me"),
        )
        .expect("compiles");

        assert_eq!(
            state_beside(&rules, "/home/me/Projects/app/target", &["Cargo.toml"]),
            State::Included
        );
        assert_eq!(
            state_beside(&rules, "/home/me/Projects/app/target", &[]),
            State::InScope,
            "the rule does not reach it — but the user never asked for it to be left alone"
        );
    }

    /// The defect: a `.csproj` has no fixed name, so an exact comparison made
    /// the predicate unusable for every build system but Cargo's.
    #[test]
    fn a_required_sibling_is_a_glob() {
        let rules = Rules::new(
            vec![{
                let mut base = rooted("csharp-bin", "~/Projects", &["**/bin/", "**/obj/"]);
                base.parts[0].requires = vec!["*.csproj".into()];
                base
            }],
            &dirs("/home/me"),
        )
        .expect("compiles");

        assert_eq!(
            state_beside(&rules, "/home/me/Projects/app/bin", &["App.csproj"]),
            State::Included
        );
        assert_eq!(
            state_beside(&rules, "/home/me/Projects/app/obj", &["App.csproj"]),
            State::Included
        );
        assert_eq!(
            state_beside(&rules, "/home/me/Projects/app/bin", &["README.md"]),
            State::InScope,
            "nothing beside it says this is build output"
        );
    }

    /// A pattern with no metacharacters matches itself, so every rule written
    /// before this went in still means what it meant.
    #[test]
    fn a_plain_name_still_means_that_name() {
        let rules = Rules::new(
            vec![{
                let mut base = rooted("rust-target", "~/Projects", &["**/target/"]);
                base.parts[0].requires = vec!["Cargo.toml".into()];
                base
            }],
            &dirs("/home/me"),
        )
        .expect("compiles");
        let beside =
            |siblings: &[&str]| state_beside(&rules, "/home/me/Projects/a/target", siblings);

        assert_eq!(beside(&["Cargo.toml"]), State::Included);
        assert_eq!(
            beside(&["NotCargo.toml"]),
            State::InScope,
            "a name is not a suffix"
        );
        assert_eq!(beside(&["Cargo.toml.bak"]), State::InScope);
    }

    /// Two required siblings are two questions. One pattern finding a match is
    /// not the other one finding one.
    #[test]
    fn every_required_sibling_needs_a_match_of_its_own() {
        let rules = Rules::new(
            vec![{
                let mut base = rooted("dotnet", "~/Projects", &["**/bin/"]);
                base.parts[0].requires = vec!["*.csproj".into(), "*.sln".into()];
                base
            }],
            &dirs("/home/me"),
        )
        .expect("compiles");
        let beside = |siblings: &[&str]| state_beside(&rules, "/home/me/Projects/a/bin", siblings);

        assert_eq!(beside(&["App.csproj", "App.sln"]), State::Included);
        assert_eq!(beside(&["App.csproj"]), State::InScope);
        assert_eq!(
            beside(&["App.sln", "Other.sln"]),
            State::InScope,
            "two matches for one pattern is still one pattern answered"
        );
    }

    /// The user's own text, so a broken one is reported rather than dropped —
    /// the same treatment `includes` and `excludes` get.
    #[test]
    fn a_malformed_required_sibling_names_the_rule_and_the_pattern() {
        let err = Rules::new(
            vec![{
                let mut base = rule("broken", &["**/bin/"]);
                base.parts[0].requires = vec!["*.[cs".into()];
                base
            }],
            &UserDirs::default(),
        )
        .expect_err("an unclosed class must not compile");

        assert_eq!(err.rule, "broken");
        assert_eq!(err.pattern, "*.[cs");
    }

    /// For callers that would have to read a directory to answer.
    #[test]
    fn a_rule_set_says_whether_anything_asks_about_siblings() {
        let asking = Rules::new(
            vec![{
                let mut base = rule("dotnet", &["**/bin/"]);
                base.parts[0].requires = vec!["*.csproj".into()];
                base
            }],
            &UserDirs::default(),
        )
        .expect("compiles");
        let quiet =
            Rules::new(vec![rule("any", &["**/bin/"])], &UserDirs::default()).expect("compiles");

        assert!(asking.wants_siblings());
        assert!(!quiet.wants_siblings());
        assert!(!Rules::default().wants_siblings());
    }

    /// An age threshold is the other predicate, and it reads the entry's own
    /// mtime rather than the clock.
    #[test]
    fn an_age_threshold_decides_the_same_way_here_as_in_detect() {
        const DAY: Duration = Duration::from_secs(24 * 60 * 60);
        let rules = Rules::new(
            vec![{
                let mut base = rooted("stale", "~/Downloads", &["**"]);
                base.parts[0].older_than = Some(30 * DAY);
                base
            }],
            &dirs("/home/me"),
        )
        .expect("compiles");

        let at = |modified: SystemTime| {
            rules.state(
                Path::new("/home/me/Downloads/thing.iso"),
                &Facts {
                    is_dir: false,
                    modified: Some(modified),
                    now: now(),
                    any_sibling: &|_| false,
                },
            )
        };

        assert_eq!(at(now() - 40 * DAY), State::Included);
        assert_eq!(at(now() - 10 * DAY), State::InScope);
    }

    /// A predicate failing must not shadow the rules below it, here for the same
    /// reason `detect::claim` says so.
    #[test]
    fn a_failed_predicate_lets_a_later_rule_claim_the_path() {
        let first = {
            let mut base = rooted("first", "~/Projects", &["**/target/"]);
            base.parts[0].requires = vec!["Cargo.toml".into()];
            base
        };
        let second = rooted("second", "~/Projects", &["**/target/"]);

        let rules = Rules::new(vec![first, second], &dirs("/home/me")).expect("compiles");

        assert_eq!(
            state_beside(&rules, "/home/me/Projects/app/target", &[]),
            State::Included,
            "the first could not claim it; the second has no such requirement"
        );
    }

    /// Colour alone excludes anyone who cannot tell the colours apart.
    #[test]
    fn every_state_has_a_word_for_it() {
        let labels: Vec<&str> = [
            State::Untracked,
            State::InScope,
            State::Included,
            State::Excluded,
        ]
        .iter()
        .map(|state| state.label())
        .collect();

        assert_eq!(labels, ["untracked", "in scope", "included", "excluded"]);
    }
}
