//! `--explain`: what this command is about to do, and under which rules.
//!
//! It prints and **stops**. Explaining and then acting would make this a log
//! line rather than a check, and the point of a check is that it happens before
//! anything is walked, read or removed.
//!
//! What it exists for: a configuration is invisible. A rule dropped for a `~`
//! this machine cannot resolve matches nothing and says nothing; a pool that
//! covers one directory and not its neighbour produces an empty report that
//! reads exactly like a disk with no duplicates. Every one of those is a fact
//! the tool already has and never had a reason to say out loud.

use crate::args::{Cleanup, Intent, Mode};
use disk_tools_core::{Rules, ScanOptions};
use std::fmt::Write;
use std::path::Path;

/// Everything the frontend resolved, said plainly.
///
/// Built from the resolved [`Mode`], so it describes the run that *would* have
/// happened rather than a second reading of the same arguments.
pub fn explain(mode: &Mode, config: Option<&Path>) -> String {
    let mut out = String::new();

    match config {
        Some(path) => {
            let _ = writeln!(out, "Configuration: {}", path.display());
        }
        // Not a footnote: with no file the built-in rules are in force, and a
        // user wondering why their rule does nothing may simply be editing a
        // file this run never found.
        None => {
            let _ = writeln!(
                out,
                "Configuration: none found — the built-in rules are in force"
            );
        }
    }
    let _ = writeln!(out);

    match mode {
        Mode::Scan { options, .. } => scan(options, &mut out),
        Mode::Clean(cleanup) => cleanup_mode(cleanup, &mut out),
        Mode::Ui { root, rules, .. } => {
            let _ = writeln!(out, "Would open the browser at {}.", root.display());
            let _ = writeln!(
                out,
                "It reads the rules and colours by them; it removes nothing.\n"
            );
            write_rules("Clean rules", rules, &mut out);
        }
        Mode::ConfigInit { target, force } => {
            let _ = writeln!(
                out,
                "Would write the default configuration to {}.",
                target.display()
            );
            if !*force {
                let _ = writeln!(out, "It refuses an existing file without -f.");
            }
        }
    }

    out
}

fn scan(options: &ScanOptions, out: &mut String) {
    let _ = writeln!(out, "Would measure {}.", options.root.display());
    let _ = writeln!(
        out,
        "Nothing is removed and no rule is consulted: `scan` only measures.\n"
    );
    let _ = writeln!(
        out,
        "  sizes          {}",
        if options.apparent {
            "apparent (--apparent)"
        } else {
            "allocated on disk"
        }
    );
    if options.one_file_system {
        let _ = writeln!(out, "  boundaries     stops at other filesystems");
    }
}

fn cleanup_mode(cleanup: &Cleanup, out: &mut String) {
    let verb = match cleanup.intent {
        Intent::Preview => "Would examine",
        Intent::Removing => "Would examine, and then remove from",
    };
    let source = if cleanup.roots_from_rules {
        match cleanup.duplicates {
            Some(_) => " (from the roots of your duplicate rules)",
            None => " (from the roots of your clean rules)",
        }
    } else {
        " (the path you named)"
    };

    if cleanup.roots.is_empty() {
        let _ = writeln!(
            out,
            "Nothing would be examined: no rule names a directory, and no path was given."
        );
        return;
    }
    let _ = writeln!(out, "{verb}:{source}");
    for root in &cleanup.roots {
        let _ = writeln!(out, "  {}", root.display());
    }
    let _ = writeln!(out);

    match &cleanup.duplicates {
        Some(duplicating) => {
            let _ = writeln!(
                out,
                "Candidates come from **file contents**: identical files, grouped, one copy kept."
            );
            let _ = writeln!(
                out,
                "Your clean rules are still consulted, but only to keep the search out of what\n\
                 they claim — a node_modules goes whole, and is not a place to remove one file from.\n"
            );
            let _ = writeln!(out, "  floor          {}", size(duplicating.min_size));
            // "--keep or the file" rather than "--keep": both arrive here as
            // the same `Some`, and naming the flag for a value that came from
            // the file would send a user to change something they never passed.
            match duplicating.keep {
                Some(keep) => {
                    let _ = writeln!(
                        out,
                        "  keeper         {keep:?}, over every rule (--keep, or [duplicates] keep)"
                    );
                }
                None => {
                    let _ = writeln!(out, "  keeper         each rule's own");
                }
            }
            if let Some(keep_in) = &duplicating.keep_in {
                for root in keep_in {
                    let _ = writeln!(out, "  prefer to keep {}", root.display());
                }
            }
            let _ = writeln!(out);
            write_rules("Pools", duplicating.rules.compiled(), out);
            let _ = writeln!(
                out,
                "Copies are only ever compared **within one pool**. Two identical files the rules\n\
                 put in different pools are never offered as duplicates of each other."
            );
        }
        None => {
            let _ = writeln!(
                out,
                "Candidates come from **your clean rules**: what they claim, whole.\n"
            );
            if cleanup.options.min_size > 0 {
                let _ = writeln!(
                    out,
                    "  floor          {} (--min-size)",
                    size(cleanup.options.min_size)
                );
            }
            let _ = writeln!(out);
            write_rules("Clean rules", &cleanup.options.detect.rules, out);
        }
    }

    write_fate(cleanup, out);
}

/// Where each tier sends things, and whether the run would stop.
fn write_fate(cleanup: &Cleanup, out: &mut String) {
    let _ = writeln!(out);
    if cleanup.options.safe_only {
        let _ = writeln!(
            out,
            "--safe: only what needs no confirmation is offered. Under --dup that is nothing at\n\
             all, since every duplicate needs confirming."
        );
    }
    if cleanup.options.purge_all {
        let _ = writeln!(
            out,
            "--purge: everything in the plan is destroyed rather than trashed. Nothing comes back."
        );
    } else {
        let _ = writeln!(
            out,
            "Anything removed goes to the OS trash, unless a rule says `tier: purge`."
        );
    }

    match cleanup.intent {
        Intent::Preview => {
            let _ = writeln!(out, "\n`preview` changes nothing whatever the flags say.");
        }
        Intent::Removing if cleanup.confirm_tier_allowed => {
            let _ = writeln!(
                out,
                "\n--yes: confirm-tier candidates would be removed too, without being asked about."
            );
        }
        Intent::Removing => {
            let _ = writeln!(
                out,
                "\n`clean` would stop and remove nothing if the plan holds anything needing\n\
                 confirmation. Add --safe to take only the rest, or --yes to take all of it."
            );
        }
    }
}

/// Every rule in force, with its parts — and everything that was dropped.
fn write_rules(title: &str, rules: &Rules, out: &mut String) {
    let in_force = rules.rules();
    if in_force.is_empty() {
        let _ = writeln!(out, "{title}: none in force.");
    } else {
        let _ = writeln!(out, "{title} in force, in precedence order:");
        for rule in in_force {
            let _ = writeln!(out, "  {} ({:?})", rule.name, rule.tier);
            for (index, part) in rule.parts.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "    part {}  root {}  includes {}{}{}{}",
                    index + 1,
                    part.root.as_deref().unwrap_or("* (wherever the scan goes)"),
                    part.includes.join(", "),
                    list("  excludes ", &part.excludes),
                    list("  requires ", &part.requires),
                    if part.min_size > 0 {
                        format!("  min-size {}", size(part.min_size))
                    } else {
                        String::new()
                    },
                );
            }
        }
    }

    // The half a user cannot see any other way: a dropped rule matches nothing
    // and says nothing, which is indistinguishable from a rule that is simply
    // wrong about the disk.
    let dropped = rules.dropped();
    if !dropped.is_empty() {
        let _ = writeln!(out, "\nDropped, and matching nothing:");
        for gone in dropped {
            match gone.part {
                Some(part) => {
                    let _ = writeln!(out, "  {} part {part} — {}", gone.rule, gone.why);
                }
                None => {
                    let _ = writeln!(out, "  {} — {}", gone.rule, gone.why);
                }
            }
        }
    }
    let _ = writeln!(out);
}

fn list(label: &str, patterns: &[String]) -> String {
    if patterns.is_empty() {
        String::new()
    } else {
        format!("{label}{}", patterns.join(", "))
    }
}

fn size(bytes: u64) -> String {
    crate::render::tree::format_size(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Args, Environment};
    use clap::Parser;
    use disk_tools_core::UserDirs;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn resolved(args: &[&str], config: crate::config::Config) -> Mode {
        Args::try_parse_from(std::iter::once("disk-tools").chain(args.iter().copied()))
            .expect("parse")
            .resolve(Environment {
                now: SystemTime::UNIX_EPOCH,
                user_dirs: UserDirs::default(),
                config,
                config_path: Some(PathBuf::from("/cfg/config.yml")),
            })
            .expect("resolve")
    }

    /// The explanation for `args`, against a config built from `text`.
    fn explaining(args: &[&str], text: &str) -> String {
        let config = crate::config::parse_for_test(text);
        explain(&resolved(args, config), Some(Path::new("/cfg/config.yml")))
    }

    fn rooted(name: &str) -> String {
        if cfg!(windows) {
            format!("C:\\{name}")
        } else {
            format!("/{name}")
        }
    }

    #[test]
    fn it_names_the_file_it_read() {
        let said = explaining(&["preview", &rooted("x")], "");
        assert!(said.contains("/cfg/config.yml"), "{said}");
    }

    /// A user wondering why their rule does nothing may be editing a file this
    /// run never found.
    #[test]
    fn no_file_is_said_out_loud() {
        let config = crate::config::Config::default();
        let said = explain(&resolved(&["preview", &rooted("x")], config), None);

        assert!(said.contains("none found"), "{said}");
        assert!(said.contains("built-in"), "{said}");
    }

    #[test]
    fn it_names_the_roots_and_where_they_came_from() {
        let said = explaining(&["preview", &rooted("x")], "");
        assert!(said.contains("the path you named"), "{said}");

        let from_rules = explaining(
            &["preview"],
            "clean-rules:\n  - name: junk\n    parts:\n      - root: \"/tmp/junk\"\n        includes: [\"**/target/\"]\n",
        );
        assert!(from_rules.contains("your clean rules"), "{from_rules}");
        assert!(from_rules.contains("/tmp/junk"), "{from_rules}");
    }

    #[test]
    fn it_lists_every_rule_with_its_parts() {
        let said = explaining(
            &["preview", &rooted("x")],
            "clean-rules:\n  - name: dotnet\n    tier: trash\n    parts:\n      - root: \"*\"\n        includes: [\"**/bin/\"]\n        requires: [\"*.csproj\"]\n      - root: \"*\"\n        includes: [\"**/bin/\"]\n        requires: [\"*.fsproj\"]\n",
        );

        assert!(said.contains("dotnet"), "{said}");
        assert!(said.contains("part 1"), "{said}");
        assert!(said.contains("part 2"), "and both of them: {said}");
        assert!(said.contains("*.fsproj"), "{said}");
    }

    /// The half a configuration cannot show on its own: a rule that matches
    /// nothing looks exactly like a rule that is wrong about the disk.
    #[test]
    fn it_says_what_was_dropped_and_why() {
        let said = explaining(
            &["preview", &rooted("x")],
            "clean-rules:\n  - name: caches\n    parts:\n      - root: \"~\"\n        includes: [\"**/x/\"]\n  - name: off\n    enabled: false\n    parts:\n      - root: \"*\"\n        includes: [\"**/y/\"]\n",
        );

        assert!(said.contains("Dropped"), "{said}");
        assert!(said.contains("caches"), "{said}");
        assert!(
            said.contains("no value for"),
            "and why — an unresolvable token: {said}"
        );
        assert!(said.contains("off"), "{said}");
        assert!(said.contains("disabled"), "{said}");
    }

    /// The mode decides which half of the file was consulted, and saying so is
    /// what keeps a user from editing the wrong one.
    #[test]
    fn it_says_which_source_the_candidates_come_from() {
        let rules = explaining(&["preview", &rooted("x")], "");
        assert!(rules.contains("your clean rules"), "{rules}");

        let dup = explaining(&["preview", "--dup", &rooted("x")], "");
        assert!(dup.contains("file contents"), "{dup}");
        assert!(dup.contains("Pools"), "{dup}");
        assert!(
            dup.contains("within one pool"),
            "and the thing an empty report cannot say for itself: {dup}"
        );
    }

    #[test]
    fn it_names_the_floor_and_the_keeper_rule() {
        let said = explaining(&["preview", "--dup", "--keep", "first", &rooted("x")], "");

        assert!(
            said.contains("1.0M"),
            "the default floor under --dup: {said}"
        );
        assert!(said.contains("First"), "{said}");
        assert!(said.contains("over every rule"), "{said}");
        assert!(
            said.contains("[duplicates] keep"),
            "and it does not claim the flag for a value the file may have set: {said}"
        );

        let by_rule = explaining(&["preview", "--dup", &rooted("x")], "");
        assert!(by_rule.contains("each rule's own"), "{by_rule}");
    }

    #[test]
    fn it_says_whether_the_run_would_stop() {
        let clean = explaining(&["clean", &rooted("x")], "");
        assert!(clean.contains("would stop"), "{clean}");
        assert!(clean.contains("--yes"), "{clean}");

        let yes = explaining(&["clean", "--yes", &rooted("x")], "");
        assert!(yes.contains("without being asked"), "{yes}");

        let preview = explaining(&["preview", &rooted("x")], "");
        assert!(preview.contains("changes nothing"), "{preview}");
    }

    #[test]
    fn it_says_where_things_go() {
        let trashed = explaining(&["clean", &rooted("x")], "");
        assert!(trashed.contains("OS trash"), "{trashed}");

        let purged = explaining(&["clean", "--purge", &rooted("x")], "");
        assert!(purged.contains("Nothing comes back"), "{purged}");
    }

    #[test]
    fn a_scan_says_it_consults_no_rule() {
        let said = explaining(&["scan", &rooted("x")], "");
        assert!(said.contains("only measures"), "{said}");
    }
}
