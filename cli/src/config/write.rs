//! Putting a rule back into the config file.
//!
//! **The file is edited, not regenerated.** A config that is worth writing by
//! hand is a config full of comments explaining why each rule is there, and
//! serialising the parsed form would throw every one of them away. `toml_edit`
//! keeps the whole document — comments, spacing, key order — and lets one table
//! be changed inside it.
//!
//! The whole of it is [`upsert`], a function from TOML text to TOML text, so
//! "every comment survives" is asserted directly rather than inferred from a
//! file on disk.
//!
//! One trap this has to avoid. **An absent `[[rules]]` means "leave the
//! built-ins alone", not "there are no rules."** Appending a single table to
//! such a file would turn five rules into one without a word, so the built-ins
//! are written out first and the new rule joins them.

use super::DEFAULT_CONFIG;
use disk_tools_core::{Rule, Tier, builtin_rules};
use std::path::Path;
use toml_edit::{Array, DocumentMut, Item, Table, value};

/// What a write did, for the sentence the browser prints afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrote {
    Added,
    Changed,
    /// Added, and the built-in rules were written out alongside it because the
    /// file had said nothing about rules and would otherwise have lost them.
    AddedWithBuiltins,
}

/// Put `rule` into `text`, replacing the table of the same name or appending one.
///
/// Returns the whole file. Fails only on TOML the parser cannot read at all —
/// which is a file the program should not be rewriting.
pub fn upsert(text: &str, rule: &Rule) -> Result<(String, Wrote), String> {
    let mut doc: DocumentMut = text.parse().map_err(|err| format!("{err}"))?;

    // Absent, not empty: the difference is the whole point. `[[rules]]` with no
    // entries is a user saying "no rules"; no `[[rules]]` at all is a user
    // saying nothing, which leaves the built-ins in force.
    // `rules = []` is an inline array, and an inline array is not somewhere a
    // `[[rules]]` table can be appended. Same statement, different spelling.
    if doc
        .get("rules")
        .and_then(Item::as_array)
        .is_some_and(Array::is_empty)
    {
        doc["rules"] = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }

    let mut carried = false;
    if doc.get("rules").is_none() {
        let mut builtins = toml_edit::ArrayOfTables::new();
        for builtin in builtin_rules() {
            if builtin.name == rule.name {
                continue;
            }
            let mut table = Table::new();
            apply(&mut table, &builtin);
            builtins.push(table);
        }
        doc["rules"] = Item::ArrayOfTables(builtins);
        carried = true;
    }

    let rules = doc["rules"]
        .as_array_of_tables_mut()
        .ok_or_else(|| "`rules` is not a list of tables".to_owned())?;

    let existing = rules
        .iter_mut()
        .find(|table| table.get("name").and_then(Item::as_str) == Some(rule.name.as_str()));

    let wrote = match existing {
        // Edited in place, so every comment and every space this table already
        // had survives — the neighbours are not even looked at.
        Some(table) => {
            apply(table, rule);
            Wrote::Changed
        }
        None => {
            let mut table = Table::new();
            apply(&mut table, rule);
            rules.push(table);
            if carried {
                Wrote::AddedWithBuiltins
            } else {
                Wrote::Added
            }
        }
    };

    Ok((doc.to_string(), wrote))
}

/// Write `rule` into the file at `path`, creating it if it is not there.
///
/// A missing file starts from the template `config init` writes, so the rules a
/// user adds arrive in a file that explains itself rather than a bare list.
pub fn to_file(path: &Path, rule: &Rule) -> Result<Wrote, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => DEFAULT_CONFIG.to_owned(),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };

    let (updated, wrote) = upsert(&text, rule)?;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
    }

    // Through a neighbouring file and a rename: a crash part-way through a
    // direct write leaves a truncated config, and the next run of any verb
    // refuses to start.
    let temp = path.with_extension("toml.new");
    std::fs::write(&temp, &updated).map_err(|err| format!("{}: {err}", temp.display()))?;
    std::fs::rename(&temp, path).map_err(|err| format!("{}: {err}", path.display()))?;

    Ok(wrote)
}

/// Set every key this rule states, and remove the ones it does not.
///
/// Removing matters as much as setting: a rule that no longer needs
/// `requires-sibling` must not keep the key from before, or the file will say
/// something the program does not do.
fn apply(table: &mut Table, rule: &Rule) {
    set(table, "name", value(rule.name.as_str()));
    // The file's spelling of "no root", and the reason `root` can be required
    // while an unrooted rule remains expressible.
    set(table, "root", value(rule.root.as_deref().unwrap_or("*")));
    set(table, "includes", value(strings(&rule.includes)));

    set_or_clear(
        table,
        "excludes",
        (!rule.excludes.is_empty()).then(|| value(strings(&rule.excludes))),
    );
    set_or_clear(
        table,
        "requires-sibling",
        (!rule.requires_sibling.is_empty()).then(|| value(strings(&rule.requires_sibling))),
    );
    set_or_clear(
        table,
        "requires-clean-repo",
        rule.requires_clean_repo.then(|| value(true)),
    );
    set_or_clear(
        table,
        "min-size",
        (rule.min_size > 0).then(|| value(rule.min_size.to_string())),
    );
    set_or_clear(
        table,
        "older-than",
        rule.older_than
            .map(|older| value(format!("{}d", older.as_secs() / 86_400))),
    );

    // Always written, unlike the rest. The tier decides whether `--apply` takes
    // something without being asked, and a file that leaves it to a default is a
    // file whose most consequential setting is invisible.
    set(
        table,
        "tier",
        value(match rule.tier {
            Tier::Purge => "purge",
            Tier::Trash => "trash",
            Tier::Confirm => "confirm",
        }),
    );
    set_or_clear(table, "enabled", (!rule.enabled).then(|| value(false)));
}

/// Set a key, or take it out when there is nothing to say.
fn set_or_clear(table: &mut Table, key: &str, item: Option<Item>) {
    match item {
        Some(item) => set(table, key, item),
        None => {
            table.remove(key);
        }
    }
}

/// Set a key, keeping whatever was written around the old value.
///
/// A trailing comment belongs to the *value*, not the key, so a plain assignment
/// takes it with it — `name = "keeper"  # why` becomes `name = "keeper"`. That
/// is precisely the thing this module exists not to do.
fn set(table: &mut Table, key: &str, mut item: Item) {
    if let (Some(Item::Value(old)), Item::Value(new)) = (table.get(key), &mut item) {
        *new.decor_mut() = old.decor().clone();
    }
    table[key] = item;
}

fn strings(parts: &[String]) -> Array {
    parts.iter().map(String::as_str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rule(name: &str) -> Rule {
        Rule {
            name: name.into(),
            root: Some("~/Projects".into()),
            includes: vec!["**/target/".into()],
            ..Rule::default()
        }
    }

    /// Read the result back with the real parser: a file this writes and the CLI
    /// cannot read is the failure mode the whole design is arranged against.
    fn reparse(text: &str) -> Vec<Rule> {
        super::super::parse_for_test(text).rules
    }

    const COMMENTED: &str = r#"# The whole point of editing rather than regenerating.
[scan]
one-file-system = false

# Why this rule is here, in the user's own words.
[[rules]]
name = "keeper"            # and a trailing one
root = "*"
includes = ["**/node_modules/"]
tier = "trash"

# A comment between rules.
[[rules]]
name = "other"
root = "*"
includes = ["**/.cache/"]
tier = "confirm"
"#;

    #[test]
    fn a_new_rule_is_appended_and_every_comment_survives() {
        let (updated, wrote) = upsert(COMMENTED, &rule("fresh")).expect("valid");

        assert_eq!(wrote, Wrote::Added);
        for comment in [
            "# The whole point of editing rather than regenerating.",
            "# Why this rule is here, in the user's own words.",
            "# and a trailing one",
            "# A comment between rules.",
        ] {
            assert!(updated.contains(comment), "lost {comment:?}:\n{updated}");
        }
        assert_eq!(
            reparse(&updated)
                .iter()
                .map(|r| &r.name)
                .collect::<Vec<_>>(),
            ["keeper", "other", "fresh"]
        );
    }

    /// An edit touches one table. Its neighbours are not rewritten, reformatted
    /// or reordered.
    #[test]
    fn an_edit_leaves_the_other_rules_exactly_as_they_were() {
        let edited = Rule {
            includes: vec!["**/node_modules/".into(), "**/bower_components/".into()],
            ..rule("keeper")
        };

        let (updated, wrote) = upsert(COMMENTED, &edited).expect("valid");

        assert_eq!(wrote, Wrote::Changed);
        assert!(
            updated.contains("# A comment between rules.\n[[rules]]\nname = \"other\"\nroot = \"*\"\nincludes = [\"**/.cache/\"]\ntier = \"confirm\"\n"),
            "the neighbour moved:\n{updated}"
        );
        let rules = reparse(&updated);
        assert_eq!(rules.len(), 2);
        assert_eq!(
            rules[0].includes,
            ["**/node_modules/", "**/bower_components/"]
        );
    }

    /// The edited table keeps its own comments too, because the keys are set
    /// rather than the table replaced.
    #[test]
    fn an_edit_keeps_the_comments_on_the_rule_it_edits() {
        let (updated, _) = upsert(COMMENTED, &rule("keeper")).expect("valid");

        assert!(updated.contains("# Why this rule is here"), "{updated}");
        assert!(updated.contains("# and a trailing one"), "{updated}");
    }

    /// The trap: an absent `[[rules]]` leaves the built-ins in force, so
    /// appending one table would turn five rules into one without a word.
    #[test]
    fn adding_to_a_file_with_no_rules_carries_the_builtins_across() {
        let plain = "[scan]\none-file-system = false\n";

        let (updated, wrote) = upsert(plain, &rule("mine")).expect("valid");

        assert_eq!(wrote, Wrote::AddedWithBuiltins);
        let names: Vec<String> = reparse(&updated).into_iter().map(|r| r.name).collect();
        for builtin in builtin_rules() {
            assert!(
                names.contains(&builtin.name),
                "lost {}:\n{updated}",
                builtin.name
            );
        }
        assert_eq!(names.last().expect("the new one"), "mine");
    }

    /// A file that says `rules = []` says "no rules", and that is a statement
    /// the program has no business overruling.
    #[test]
    fn an_explicitly_empty_rule_list_is_not_refilled() {
        let empty = "rules = []\n";

        let (updated, wrote) = upsert(empty, &rule("mine")).expect("valid");

        assert_eq!(wrote, Wrote::Added);
        let names: Vec<String> = reparse(&updated).into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["mine"]);
    }

    /// A built-in of the same name is not written twice.
    #[test]
    fn carrying_the_builtins_skips_the_one_being_replaced() {
        let plain = "[scan]\none-file-system = false\n";
        let mine = Rule {
            includes: vec!["**/target/".into()],
            ..rule("rust-target")
        };

        let (updated, _) = upsert(plain, &mine).expect("valid");

        let names: Vec<String> = reparse(&updated).into_iter().map(|r| r.name).collect();
        assert_eq!(
            names.iter().filter(|name| *name == "rust-target").count(),
            1,
            "{updated}"
        );
    }

    /// Everything the form can set has to come back out of the parser unchanged,
    /// or the program writes files it then misreads.
    #[test]
    fn every_field_round_trips_through_the_file() {
        let full = Rule {
            name: "full".into(),
            root: Some("~/Projects".into()),
            includes: vec!["**/target/".into(), "**/build/".into()],
            excludes: vec!["**/vendor/**".into()],
            requires_sibling: vec!["Cargo.toml".into()],
            requires_clean_repo: true,
            older_than: Some(Duration::from_secs(30 * 86_400)),
            min_size: 10 * 1024 * 1024,
            tier: Tier::Trash,
            enabled: false,
        };

        let (updated, _) = upsert("rules = []\n", &full).expect("valid");

        assert_eq!(reparse(&updated), vec![full]);
    }

    /// `*` is how a required key says "no root", and it has to survive as an
    /// absence rather than as a directory called `*`.
    #[test]
    fn an_unrooted_rule_is_written_as_a_star() {
        let unrooted = Rule {
            root: None,
            ..rule("anywhere")
        };

        let (updated, _) = upsert("rules = []\n", &unrooted).expect("valid");

        assert!(updated.contains(r#"root = "*""#), "{updated}");
        assert_eq!(reparse(&updated)[0].root, None);
    }

    /// A key the rule no longer states must go. Left behind, the file would say
    /// something the program does not do.
    #[test]
    fn a_setting_that_is_dropped_leaves_the_file() {
        let before = r#"[[rules]]
name = "junk"
root = "*"
includes = ["**/target/"]
excludes = ["**/keep/**"]
requires-sibling = ["Cargo.toml"]
requires-clean-repo = true
min-size = "1024"
older-than = "30d"
tier = "trash"
enabled = false
"#;
        let stripped = Rule {
            name: "junk".into(),
            root: None,
            includes: vec!["**/target/".into()],
            tier: Tier::Trash,
            enabled: true,
            ..Rule::default()
        };

        let (updated, _) = upsert(before, &stripped).expect("valid");

        for gone in [
            "excludes",
            "requires-sibling",
            "requires-clean-repo",
            "min-size",
            "older-than",
            "enabled",
        ] {
            assert!(!updated.contains(gone), "{gone} survived:\n{updated}");
        }
        assert_eq!(reparse(&updated), vec![stripped]);
    }

    /// The tier decides whether `--apply` takes something without being asked. A
    /// file that leaves it to a default is a file whose most consequential
    /// setting is invisible.
    #[test]
    fn the_tier_is_always_written_even_when_it_is_the_default() {
        let (updated, _) = upsert("rules = []\n", &rule("junk")).expect("valid");

        assert!(updated.contains(r#"tier = "confirm""#), "{updated}");
    }

    /// TOML the parser cannot read is a file this has no business rewriting.
    #[test]
    fn a_broken_file_is_refused_rather_than_replaced() {
        let broken = "[[rules]\nname =";

        assert!(upsert(broken, &rule("junk")).is_err());
    }

    #[test]
    fn writing_creates_the_file_from_the_template_when_there_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/config.toml");

        let wrote = to_file(&path, &rule("mine")).expect("written");

        let text = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(wrote, Wrote::Added, "the template already lists rules");
        assert!(
            text.contains("# The never-touch denylist is NOT here"),
            "a bare list rather than the template that explains itself"
        );
        let names: Vec<String> = reparse(&text).into_iter().map(|r| r.name).collect();
        assert!(names.contains(&"mine".to_owned()), "{names:?}");
        assert!(names.contains(&"rust-target".to_owned()), "{names:?}");
    }

    #[test]
    fn writing_twice_edits_rather_than_duplicates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        to_file(&path, &rule("mine")).expect("written");
        let second = to_file(
            &path,
            &Rule {
                includes: vec!["**/build/".into()],
                ..rule("mine")
            },
        )
        .expect("written");

        assert_eq!(second, Wrote::Changed);
        let text = std::fs::read_to_string(&path).expect("read back");
        let rules = reparse(&text);
        assert_eq!(rules.iter().filter(|r| r.name == "mine").count(), 1);
        assert_eq!(
            rules
                .iter()
                .find(|r| r.name == "mine")
                .expect("there")
                .includes,
            ["**/build/"]
        );
    }

    /// A crash part-way through a direct write leaves a truncated config that
    /// every verb then refuses to start on.
    #[test]
    fn writing_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        to_file(&path, &rule("mine")).expect("written");

        let left: Vec<String> = std::fs::read_dir(dir.path())
            .expect("list")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["config.toml"]);
    }
}
