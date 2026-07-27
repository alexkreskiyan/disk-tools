//! Editing a rule, as a value.
//!
//! Nothing here draws and nothing here writes. A [`Form`] is text in nine
//! fields, and [`Form::confirm`] turns that text into a [`Rule`] or says which
//! field is wrong. That is the whole of it, which is what lets every rule of the
//! config file be tested here without a terminal or a temp directory.
//!
//! **Validation is the file's own.** Every field is checked with the function
//! `config.rs` uses for the same key — `parse_size` for `min-size`,
//! `parse_duration` for `older-than`, `Rules::new` for the globs — and
//! `a_rule_the_form_accepts_is_a_rule_the_config_parser_accepts` puts the result
//! back through the config parser to prove the two agree. A second
//! implementation would drift, and the one that drifts is the one that writes a
//! file the CLI then refuses to read.
//!
//! **A rejected form stays open.** The field is flagged and the text is left
//! alone: a dialog that clears itself on a typo makes the user retype the eight
//! fields that were right.

use crate::args::{parse_duration, parse_size};
use disk_tools_core::{Rule, Rules, Tier, UserDirs};
use std::path::Path;

/// One editable field, in the order the form shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Root,
    Includes,
    Excludes,
    RequiresSibling,
    MinSize,
    OlderThan,
    Tier,
    Enabled,
}

impl Field {
    pub const ALL: [Field; 9] = [
        Field::Name,
        Field::Root,
        Field::Includes,
        Field::Excludes,
        Field::RequiresSibling,
        Field::MinSize,
        Field::OlderThan,
        Field::Tier,
        Field::Enabled,
    ];

    /// The key this field is, spelled as the config file spells it.
    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "name",
            Field::Root => "root",
            Field::Includes => "includes",
            Field::Excludes => "excludes",
            Field::RequiresSibling => "requires-sibling",
            Field::MinSize => "min-size",
            Field::OlderThan => "older-than",
            Field::Tier => "tier",
            Field::Enabled => "enabled",
        }
    }

    /// What it accepts, for the line under the form. A field whose syntax is
    /// only in the README is a field that gets typed wrong.
    pub fn hint(self) -> &'static str {
        match self {
            Field::Name => "what the plan and the report call this rule",
            Field::Root => "a directory, or * for anywhere the scan goes",
            Field::Includes => "globs, comma-separated; a trailing / means directories only",
            Field::Excludes => "globs this rule declines, comma-separated",
            Field::RequiresSibling => "file names that must sit beside a match",
            Field::MinSize => "skip anything smaller, e.g. 10M; empty for no floor",
            Field::OlderThan => "only if untouched this long, e.g. 30d; empty for any age",
            Field::Tier => "auto removes without asking; confirm does not",
            Field::Enabled => "a disabled rule matches nothing",
        }
    }

    /// Whether this field is typed into, or cycled with one key.
    pub fn is_choice(self) -> bool {
        matches!(self, Field::Tier | Field::Enabled)
    }

    fn at(self) -> usize {
        Field::ALL
            .iter()
            .position(|field| *field == self)
            .expect("every field is in ALL")
    }
}

/// Why a form would not close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub field: Field,
    pub message: String,
}

/// What `a` opens: pick a rule, then edit it.
///
/// Two steps rather than one because the answer to "which rule" is not always
/// "a new one" — a path already claimed usually wants the rule that claims it
/// widened, not a second rule beside it.
pub enum Dialog {
    Choosing(Chooser),
    Editing(Form),
}

/// The list of rules, with "a new one" at the top.
pub struct Chooser {
    /// The row `a` was pressed on, kept for prefilling a new rule.
    pub parent: std::path::PathBuf,
    pub name: String,
    pub is_dir: bool,
    /// Existing rule names, in the order the file lists them — which is their
    /// precedence, and so the order worth showing.
    pub rules: Vec<String>,
    cursor: usize,
}

impl Chooser {
    pub fn new(parent: &Path, name: &str, is_dir: bool, rules: Vec<String>) -> Chooser {
        Chooser {
            parent: parent.to_path_buf(),
            name: name.to_owned(),
            is_dir,
            rules,
            cursor: 0,
        }
    }

    /// Rows as shown: the new-rule row, then every existing rule.
    pub fn rows(&self) -> Vec<String> {
        let mut rows = vec![format!("new rule for {}", self.name)];
        rows.extend(self.rules.iter().cloned());
        rows
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn down(&mut self) {
        if self.cursor + 1 < self.rows().len() {
            self.cursor += 1;
        }
    }

    pub fn up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Which rule was picked, or `None` for a new one.
    pub fn picked(&self) -> Option<&str> {
        // Row zero is the new-rule row, so every other index is one further
        // along than its rule.
        self.cursor
            .checked_sub(1)
            .map(|index| self.rules[index].as_str())
    }
}

/// A rule being written or rewritten.
pub struct Form {
    /// The name the rule had when the form opened. `None` for a new rule.
    ///
    /// Kept so an edit can keep its own name without colliding with itself —
    /// the one case where a name already in use is not a duplicate.
    editing: Option<String>,
    values: [String; 9],
    focus: usize,
    problem: Option<Problem>,
}

impl Form {
    /// A new rule for `name` inside `parent`.
    ///
    /// The root is the directory being browsed and the include is the entry
    /// itself, which is the rule a user opening this dialog on a `node_modules`
    /// is almost always about to write. `~` is used where it applies: a rule
    /// with a home-relative root keeps working on another machine, and the
    /// resolved form silently does not.
    pub fn for_new(parent: &Path, name: &str, is_dir: bool, dirs: &UserDirs) -> Form {
        let mut values = empty();
        values[Field::Name.at()] = name.to_owned();
        values[Field::Root.at()] = shorten(parent, dirs);
        values[Field::Includes.at()] = format!("**/{name}{}", if is_dir { "/" } else { "" });
        values[Field::Tier.at()] = tier_word(Tier::Confirm);
        values[Field::Enabled.at()] = yes_no(true);

        Form {
            editing: None,
            values,
            focus: 0,
            problem: None,
        }
    }

    /// An existing rule, opened as it stands.
    pub fn for_existing(rule: &Rule) -> Form {
        let mut values = empty();
        values[Field::Name.at()] = rule.name.clone();
        // The file's spelling of "no root". A rule that applies everywhere has
        // to say so in the same word the config would.
        values[Field::Root.at()] = rule.root.clone().unwrap_or_else(|| "*".to_owned());
        values[Field::Includes.at()] = join(&rule.includes);
        values[Field::Excludes.at()] = join(&rule.excludes);
        values[Field::RequiresSibling.at()] = join(&rule.requires_sibling);
        // An unset threshold is an empty field, not a `0` or a `0s` — those are
        // values, and would read as ones the user chose.
        values[Field::MinSize.at()] = match rule.min_size {
            0 => String::new(),
            bytes => bytes.to_string(),
        };
        // Days without loss: `parse_duration`'s finest unit is `d`, so every
        // threshold that can reach a `Rule` is a whole number of them.
        values[Field::OlderThan.at()] = match rule.older_than {
            Some(older_than) => format!("{}d", older_than.as_secs() / 86_400),
            None => String::new(),
        };
        // `Rule`'s own default is `Confirm`, so a rule whose file said nothing
        // about the tier shows the safe answer rather than a blank.
        values[Field::Tier.at()] = tier_word(rule.tier);
        values[Field::Enabled.at()] = yes_no(rule.enabled);

        Form {
            editing: Some(rule.name.clone()),
            values,
            focus: 0,
            problem: None,
        }
    }

    pub fn value(&self, field: Field) -> &str {
        &self.values[field.at()]
    }

    pub fn focus(&self) -> Field {
        Field::ALL[self.focus]
    }

    pub fn problem(&self) -> Option<&Problem> {
        self.problem.as_ref()
    }

    /// Whether this form is rewriting a rule rather than adding one.
    pub fn is_edit(&self) -> bool {
        self.editing.is_some()
    }

    pub fn next_field(&mut self) {
        self.focus = (self.focus + 1) % Field::ALL.len();
    }

    pub fn previous_field(&mut self) {
        self.focus = (self.focus + Field::ALL.len() - 1) % Field::ALL.len();
    }

    /// Type a character into the focused field.
    ///
    /// A choice field ignores it: `tier` has two values and letters are not how
    /// you pick between them.
    pub fn push(&mut self, ch: char) {
        if self.focus().is_choice() {
            return;
        }
        self.values[self.focus].push(ch);
        self.problem = None;
    }

    pub fn pop(&mut self) {
        if self.focus().is_choice() {
            return;
        }
        self.values[self.focus].pop();
        self.problem = None;
    }

    /// Flip the focused choice field.
    pub fn toggle(&mut self) {
        let focus = self.focus();
        if !focus.is_choice() {
            return;
        }
        let value = &mut self.values[self.focus];
        *value = match focus {
            Field::Tier if value == "auto" => tier_word(Tier::Confirm),
            Field::Tier => tier_word(Tier::Auto),
            _ if value == "yes" => yes_no(false),
            _ => yes_no(true),
        };
        self.problem = None;
    }

    /// Turn the form into a rule, or flag the field that stopped it.
    ///
    /// `taken` is every other rule's name. Names are what the plan and the
    /// report refer to, so two rules cannot share one — but a rule keeping its
    /// own name is not a collision, which is why the name it opened with is
    /// remembered.
    pub fn confirm(&mut self, taken: &[String], dirs: &UserDirs) -> Option<Rule> {
        match self.build(taken, dirs) {
            Ok(rule) => {
                self.problem = None;
                Some(rule)
            }
            Err(problem) => {
                // The field is flagged and every other field is left alone: a
                // dialog that clears on a typo makes the user retype the eight
                // that were right.
                self.focus = problem.field.at();
                self.problem = Some(problem);
                None
            }
        }
    }

    /// Flag the form with a reason it could not have found itself.
    ///
    /// A rule compiles on its own and still clashes with the set around it —
    /// that verdict belongs to whoever holds the other rules, and it has to be
    /// able to say so without the form guessing.
    pub fn reject(&mut self, message: String) {
        self.problem = Some(Problem {
            field: self.focus(),
            message,
        });
    }

    fn build(&self, taken: &[String], dirs: &UserDirs) -> Result<Rule, Problem> {
        let flag = |field: Field, message: &str| Problem {
            field,
            message: message.to_owned(),
        };

        let name = self.value(Field::Name).trim().to_owned();
        if name.is_empty() {
            return Err(flag(Field::Name, "a rule needs a name"));
        }
        if taken
            .iter()
            .any(|other| *other == name && self.editing.as_deref() != Some(other.as_str()))
        {
            return Err(flag(
                Field::Name,
                "another rule already has this name, and names are what the plan refers to",
            ));
        }

        let root = self.value(Field::Root).trim();
        if root.is_empty() {
            return Err(flag(
                Field::Root,
                "a root is required — use * for a rule that applies wherever the scan goes",
            ));
        }

        let includes = split(self.value(Field::Includes));
        if includes.is_empty() {
            return Err(flag(
                Field::Includes,
                "a rule that matches nothing is not a rule",
            ));
        }

        let min_size = match self.value(Field::MinSize).trim() {
            "" => 0,
            text => parse_size(text).map_err(|err| flag(Field::MinSize, &err))?,
        };
        let older_than = match self.value(Field::OlderThan).trim() {
            "" => None,
            text => Some(parse_duration(text).map_err(|err| flag(Field::OlderThan, &err))?),
        };

        let rule = Rule {
            name,
            // The file's spelling of "no root": the core distinguishes an
            // unrooted rule from one rooted at `/`, and `*` is how a required
            // field expresses the former.
            root: (root != "*").then(|| root.to_owned()),
            includes,
            excludes: split(self.value(Field::Excludes)),
            requires_sibling: split(self.value(Field::RequiresSibling)),
            requires_clean_repo: false,
            older_than,
            min_size,
            tier: match self.value(Field::Tier) {
                "auto" => Tier::Auto,
                _ => Tier::Confirm,
            },
            enabled: self.value(Field::Enabled) == "yes",
        };

        // The globs are only known to be globs once something compiles them, and
        // the thing that will compile them is this. A pattern accepted here and
        // rejected on the next run would be a file the program wrote and cannot
        // read.
        Rules::new(vec![rule.clone()], dirs)
            .map_err(|err| flag(Field::Includes, &err.to_string()))?;

        Ok(rule)
    }
}

fn empty() -> [String; 9] {
    std::array::from_fn(|_| String::new())
}

/// Several patterns on one line. Comma-separated, because a glob may contain
/// spaces and a path certainly may.
fn split(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn join(parts: &[String]) -> String {
    parts.join(", ")
}

fn tier_word(tier: Tier) -> String {
    match tier {
        Tier::Auto => "auto",
        Tier::Confirm => "confirm",
    }
    .to_owned()
}

fn yes_no(yes: bool) -> String {
    if yes { "yes" } else { "no" }.to_owned()
}

/// Write a path back through `~` where it applies.
///
/// A rule rooted at `~/Projects` keeps working on another machine and after the
/// user is renamed; one rooted at `/Users/alex/Projects` silently does not.
fn shorten(path: &Path, dirs: &UserDirs) -> String {
    let text = path.display().to_string();
    let Some(home) = dirs.home.as_deref() else {
        return text;
    };
    let Ok(rest) = path.strip_prefix(home) else {
        return text;
    };
    if rest.as_os_str().is_empty() {
        return "~".to_owned();
    }
    format!("~/{}", rest.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn dirs() -> UserDirs {
        UserDirs {
            home: Some(PathBuf::from("/home/me")),
            ..UserDirs::default()
        }
    }

    fn set(form: &mut Form, field: Field, text: &str) {
        while form.focus() != field {
            form.next_field();
        }
        for _ in 0..form.value(field).chars().count() {
            form.pop();
        }
        for ch in text.chars() {
            form.push(ch);
        }
    }

    /// Opening on a directory offers the rule the user is almost always about to
    /// write: rooted where they are, matching what they marked.
    #[test]
    fn a_new_rule_arrives_prefilled_from_the_path() {
        let form = Form::for_new(
            Path::new("/home/me/Projects/app"),
            "node_modules",
            true,
            &dirs(),
        );

        assert_eq!(form.value(Field::Name), "node_modules");
        assert_eq!(form.value(Field::Root), "~/Projects/app");
        assert_eq!(form.value(Field::Includes), "**/node_modules/");
        assert!(!form.is_edit());
    }

    /// A rule rooted at `/Users/alex/...` stops working the moment the config is
    /// used anywhere else; `~` does not.
    #[test]
    fn a_prefilled_root_uses_the_home_shorthand() {
        assert_eq!(shorten(Path::new("/home/me"), &dirs()), "~");
        assert_eq!(shorten(Path::new("/home/me/x"), &dirs()), "~/x");
        assert_eq!(shorten(Path::new("/etc"), &dirs()), "/etc");
        assert_eq!(
            shorten(Path::new("/home/me/x"), &UserDirs::default()),
            "/home/me/x",
            "with no home there is no shorthand to use"
        );
    }

    /// A trailing `/` is the gitignore convention the rules already honour, and
    /// it is wrong for a file.
    #[test]
    fn a_new_rule_for_a_file_does_not_ask_for_a_directory() {
        let form = Form::for_new(Path::new("/tmp"), "core.dump", false, &dirs());

        assert_eq!(form.value(Field::Includes), "**/core.dump");
    }

    #[test]
    fn an_existing_rule_opens_as_it_stands() {
        let rule = Rule {
            name: "junk".into(),
            root: Some("~/Projects".into()),
            includes: vec!["**/target/".into(), "**/build/".into()],
            excludes: vec!["**/keep/**".into()],
            requires_sibling: vec!["Cargo.toml".into()],
            older_than: Some(Duration::from_secs(30 * 86_400)),
            min_size: 1024,
            tier: Tier::Auto,
            enabled: false,
            ..Rule::default()
        };

        let form = Form::for_existing(&rule);

        assert_eq!(form.value(Field::Includes), "**/target/, **/build/");
        assert_eq!(form.value(Field::Excludes), "**/keep/**");
        assert_eq!(form.value(Field::RequiresSibling), "Cargo.toml");
        assert_eq!(form.value(Field::MinSize), "1024");
        assert_eq!(form.value(Field::OlderThan), "30d");
        assert_eq!(form.value(Field::Tier), "auto");
        assert_eq!(form.value(Field::Enabled), "no");
        assert!(form.is_edit());
    }

    /// An unstated threshold is an empty field. `0` and `0s` are values, and
    /// would read as ones the user chose.
    #[test]
    fn an_unset_threshold_is_blank_rather_than_zero() {
        let form = Form::for_existing(&Rule {
            name: "junk".into(),
            ..Rule::default()
        });

        assert_eq!(form.value(Field::MinSize), "");
        assert_eq!(form.value(Field::OlderThan), "");
    }

    /// The file's own safe default, shown rather than left blank.
    #[test]
    fn a_rule_with_no_stated_tier_shows_confirm() {
        let form = Form::for_existing(&Rule {
            name: "junk".into(),
            ..Rule::default()
        });

        assert_eq!(form.value(Field::Tier), "confirm");
    }

    #[test]
    fn a_filled_form_becomes_a_rule() {
        let mut form = Form::for_new(Path::new("/home/me/Projects"), "target", true, &dirs());
        set(&mut form, Field::MinSize, "10M");
        set(&mut form, Field::OlderThan, "30d");
        set(&mut form, Field::RequiresSibling, "Cargo.toml");

        let rule = form.confirm(&[], &dirs()).expect("valid");

        assert_eq!(rule.name, "target");
        assert_eq!(rule.root.as_deref(), Some("~/Projects"));
        assert_eq!(rule.includes, ["**/target/"]);
        assert_eq!(rule.requires_sibling, ["Cargo.toml"]);
        assert_eq!(rule.min_size, 10 * 1024 * 1024);
        assert_eq!(rule.older_than, Some(Duration::from_secs(30 * 86_400)));
        assert_eq!(rule.tier, Tier::Confirm);
        assert!(rule.enabled);
        assert!(form.problem().is_none());
    }

    /// `*` is how a required field says "no root", and the core has to receive
    /// it as an absence rather than as a directory called `*`.
    #[test]
    fn a_star_root_becomes_no_root_at_all() {
        let mut form = Form::for_new(Path::new("/home/me"), "target", true, &dirs());
        set(&mut form, Field::Root, "*");

        let rule = form.confirm(&[], &dirs()).expect("valid");

        assert_eq!(rule.root, None);
    }

    #[test]
    fn several_patterns_share_a_line() {
        let mut form = Form::for_new(Path::new("/home/me"), "junk", true, &dirs());
        set(
            &mut form,
            Field::Includes,
            "**/target/, **/build/ ,,  **/out/",
        );

        let rule = form.confirm(&[], &dirs()).expect("valid");

        assert_eq!(rule.includes, ["**/target/", "**/build/", "**/out/"]);
    }

    #[test]
    fn a_choice_field_cycles_rather_than_being_typed() {
        let mut form = Form::for_new(Path::new("/home/me"), "junk", true, &dirs());
        while form.focus() != Field::Tier {
            form.next_field();
        }

        form.push('x');
        assert_eq!(
            form.value(Field::Tier),
            "confirm",
            "letters do nothing here"
        );

        form.toggle();
        assert_eq!(form.value(Field::Tier), "auto");
        form.toggle();
        assert_eq!(form.value(Field::Tier), "confirm");
    }

    #[test]
    fn enabled_cycles_too() {
        let mut form = Form::for_new(Path::new("/home/me"), "junk", true, &dirs());
        while form.focus() != Field::Enabled {
            form.next_field();
        }

        assert_eq!(form.value(Field::Enabled), "yes");
        form.toggle();
        assert_eq!(form.value(Field::Enabled), "no");

        assert!(!form.confirm(&[], &dirs()).expect("valid").enabled);
    }

    #[test]
    fn the_fields_wrap_in_both_directions() {
        let mut form = Form::for_new(Path::new("/home/me"), "junk", true, &dirs());

        form.previous_field();
        assert_eq!(form.focus(), Field::Enabled);
        form.next_field();
        assert_eq!(form.focus(), Field::Name);
    }

    /// Every rejection names its field, so the cursor can go there and the
    /// message can sit beside the thing it is about.
    #[test]
    fn a_bad_value_flags_its_own_field_and_yields_nothing() {
        let cases = [
            (Field::Name, ""),
            (Field::Root, "   "),
            (Field::Includes, ""),
            (Field::MinSize, "10 potatoes"),
            (Field::OlderThan, "a while"),
            (Field::Includes, "**/[unclosed"),
        ];

        for (field, text) in cases {
            let mut form = Form::for_new(Path::new("/home/me"), "junk", true, &dirs());
            set(&mut form, field, text);

            assert!(
                form.confirm(&[], &dirs()).is_none(),
                "{field:?} = {text:?} must not produce a rule"
            );
            let problem = form.problem().expect("a reason");
            assert_eq!(problem.field, field, "{text:?}: {}", problem.message);
            assert_eq!(form.focus(), field, "and the cursor goes to it");
            assert!(!problem.message.is_empty());
        }
    }

    /// A dialog that clears itself on a typo makes the user retype the eight
    /// fields that were right.
    #[test]
    fn a_rejected_form_keeps_everything_the_user_typed() {
        let mut form = Form::for_new(Path::new("/home/me/Projects"), "target", true, &dirs());
        set(&mut form, Field::MinSize, "nonsense");

        assert!(form.confirm(&[], &dirs()).is_none());

        assert_eq!(form.value(Field::Name), "target");
        assert_eq!(form.value(Field::Root), "~/Projects");
        assert_eq!(form.value(Field::MinSize), "nonsense", "including the typo");
    }

    #[test]
    fn typing_clears_the_flag() {
        let mut form = Form::for_new(Path::new("/home/me"), "junk", true, &dirs());
        set(&mut form, Field::MinSize, "nonsense");
        form.confirm(&[], &dirs());
        assert!(form.problem().is_some());

        form.pop();

        assert!(form.problem().is_none());
    }

    /// Names are what the plan and the report refer to, so two rules cannot
    /// share one.
    #[test]
    fn a_name_another_rule_has_is_refused() {
        let mut form = Form::for_new(Path::new("/home/me"), "junk", true, &dirs());

        assert!(form.confirm(&["junk".to_owned()], &dirs()).is_none());
        assert_eq!(form.problem().expect("a reason").field, Field::Name);
    }

    /// The one case where a name already in use is not a duplicate.
    #[test]
    fn a_rule_may_keep_its_own_name() {
        let mut form = Form::for_existing(&Rule {
            name: "junk".into(),
            includes: vec!["**/target/".into()],
            ..Rule::default()
        });
        set(&mut form, Field::Root, "*");

        assert!(form.confirm(&["junk".to_owned()], &dirs()).is_some());
    }

    /// Days are `parse_duration`'s finest unit, so a threshold that came from a
    /// config or a flag survives being shown and confirmed unchanged.
    #[test]
    fn a_threshold_survives_a_round_trip_through_the_form() {
        for text in ["30d", "2w", "6m", "1y"] {
            let original = parse_duration(text).expect("valid");
            let mut form = Form::for_existing(&Rule {
                name: "junk".into(),
                includes: vec!["**/target/".into()],
                older_than: Some(original),
                ..Rule::default()
            });
            set(&mut form, Field::Root, "*");

            let rule = form.confirm(&[], &dirs()).expect("valid");

            assert_eq!(rule.older_than, Some(original), "{text}");
        }
    }

    /// The anti-drift check the spec asks for: a rule this form produces has to
    /// be one the config parser would read back. If the two ever disagree, the
    /// form is writing a file the CLI refuses.
    #[test]
    fn a_rule_the_form_accepts_is_a_rule_the_config_parser_accepts() {
        let mut form = Form::for_new(Path::new("/home/me/Projects"), "target", true, &dirs());
        set(&mut form, Field::Excludes, "**/vendor/**");
        set(&mut form, Field::RequiresSibling, "Cargo.toml");
        set(&mut form, Field::MinSize, "10M");
        set(&mut form, Field::OlderThan, "30d");
        let rule = form.confirm(&[], &dirs()).expect("valid");

        let text = format!(
            "[[rules]]\n\
             name = \"{}\"\n\
             root = \"{}\"\n\
             includes = [\"{}\"]\n\
             excludes = [\"{}\"]\n\
             requires-sibling = [\"{}\"]\n\
             min-size = \"{}\"\n\
             older-than = \"{}d\"\n\
             tier = \"confirm\"\n\
             enabled = true\n",
            rule.name,
            rule.root.as_deref().expect("rooted"),
            rule.includes.join("\", \""),
            rule.excludes.join("\", \""),
            rule.requires_sibling.join("\", \""),
            rule.min_size,
            rule.older_than.expect("set").as_secs() / 86_400,
        );

        let parsed = crate::config::parse_for_test(&text);

        assert_eq!(parsed.rules.len(), 1);
        assert_eq!(parsed.rules[0], rule);
    }
}
