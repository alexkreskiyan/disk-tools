//! `--json`: the answer as machine-readable JSON on stdout.
//!
//! Sizes are the raw byte counts (the human report formats the same numbers), so
//! the two outputs never disagree.
//!
//! **Display flags do not reach here.** `-n`, `-d` and `--sort` change how a
//! report is laid out for a person; a machine-readable output quietly shortened
//! by one of them is the worst kind of shortened, because nothing in the
//! document says it happened. The flags that narrow the *plan* — `--safe`,
//! `--min-size`, `--older-than`, `--purge` — are of course reflected, since they
//! change what the answer is rather than how it is shown.
//!
//! `preview --json` and `clean --json` are **different documents**, one a plan
//! and the other an outcome. Giving them one shape with half the fields null
//! would make every consumer branch on emptiness to discover which it had.

use disk_tools_core::{CleanOutcome, CleanPlan, Excluded, Kept, ScanTree};
use serde::Serialize;
use std::path::Path;

/// Serialize the whole scan (tree + skipped) as pretty JSON.
///
/// Returns `Err` only when a path is not valid UTF-8 — serde cannot encode such
/// a `PathBuf` as a JSON string. The caller turns that into a clean error and a
/// non-zero exit rather than a panic.
pub fn render_json(tree: &ScanTree) -> serde_json::Result<String> {
    serde_json::to_string_pretty(tree)
}

/// The whole plan: what `clean` would remove, and what it refused.
///
/// Every candidate carries its rule, its tier, whether it will be destroyed
/// rather than trashed, and the `shared` marker that makes its size an upper
/// bound — so a consumer can reach the same conclusions the human report does
/// without parsing it.
pub fn render_plan(plan: &CleanPlan) -> serde_json::Result<String> {
    serde_json::to_string_pretty(plan)
}

/// The same plan, shaped as the groups a `--dup` run is about.
///
/// A different document from [`render_plan`], deliberately. The flat candidate
/// list is a true description of what will be removed, but it makes a consumer
/// rebuild the groups by keeper path to answer the first question anyone asks of
/// duplicates — *what is kept and what goes with it*. Serialising what the human
/// report shows keeps the two outputs saying the same thing.
///
/// Built from the **plan**, like the human report, so a copy the denylist
/// refused is absent from both.
pub fn render_dup_plan(plan: &CleanPlan) -> serde_json::Result<String> {
    let mut groups: Vec<DupGroup<'_>> = Vec::new();
    for candidate in &plan.candidates {
        let Some(kept) = &candidate.duplicate_of else {
            continue;
        };
        let copy = DupCopy {
            path: &candidate.path,
            allocated: candidate.allocated,
            shared: candidate.shared,
        };
        match groups.iter_mut().find(|g| g.keeper.path == kept.path) {
            Some(group) => {
                group.reclaimable += candidate.allocated;
                group.copies.push(copy);
            }
            None => groups.push(DupGroup {
                keeper: kept,
                reclaimable: candidate.allocated,
                copies: vec![copy],
            }),
        }
    }

    serde_json::to_string_pretty(&DupPlan {
        groups,
        reclaimable: plan.reclaimable,
        excluded: &plan.excluded,
        filtered_out: plan.filtered_out,
        too_small: plan.too_small,
    })
}

/// `--json` for a duplicate plan. Word for word the fields of [`CleanPlan`] that
/// still mean something here — `below-rule-minimum` does not, since no rule is
/// in play.
// No `rename_all`: `CleanPlan` serializes its fields as they are written, and a
// consumer switching between the two documents must not have to switch spelling
// as well.
#[derive(Serialize)]
struct DupPlan<'a> {
    groups: Vec<DupGroup<'a>>,
    reclaimable: u64,
    excluded: &'a [Excluded],
    filtered_out: usize,
    too_small: usize,
}

#[derive(Serialize)]
struct DupGroup<'a> {
    keeper: &'a Kept,
    /// What removing every copy in this group frees.
    reclaimable: u64,
    copies: Vec<DupCopy<'a>>,
}

#[derive(Serialize)]
struct DupCopy<'a> {
    path: &'a Path,
    allocated: u64,
    /// Its inode has another name, so those bytes may not come back.
    shared: bool,
}

/// What a cleanup actually did: the two halves and every failure.
pub fn render_outcome_json(outcome: &CleanOutcome) -> serde_json::Result<String> {
    serde_json::to_string_pretty(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_tools_core::{
        Candidate, ExcludeReason, Excluded, Reclaimed, ScanNode, ScanTree, SkipReason,
        SkippedEntry, Tier, TrashFailure,
    };
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    fn node(path: &str, allocated: u64, apparent: u64, children: Vec<ScanNode>) -> ScanNode {
        ScanNode {
            path: PathBuf::from(path),
            allocated,
            apparent,
            is_dir: !children.is_empty(),
            modified: None,
            links: None,
            children,
        }
    }

    /// Wraps a root, so the model can grow without every fixture below changing
    /// — `ScanTree` gained a field between v0.1 and v0.2 alone.
    fn tree(root: ScanNode) -> ScanTree {
        tree_with(root, Vec::new())
    }

    fn tree_with(root: ScanNode, skipped: Vec<SkippedEntry>) -> ScanTree {
        ScanTree {
            root,
            skipped,
            link_groups: Vec::new(),
        }
    }

    // ---- the plan and the outcome ----------------------------------------

    // ---- the duplicate plan ----------------------------------------------

    fn kept(path: &str) -> disk_tools_core::Kept {
        disk_tools_core::Kept {
            path: PathBuf::from(path),
            date: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            fell_back: false,
        }
    }

    fn duplicate(path: &str, allocated: u64, keeper: &disk_tools_core::Kept) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            rule: "duplicate".into(),
            tier: Tier::Confirm,
            purge: false,
            duplicate_of: Some(keeper.clone()),
            allocated,
            shared: false,
        }
    }

    fn dup_plan() -> CleanPlan {
        let keeper = kept("/p/keeper.bin");
        let candidates = vec![
            duplicate("/p/a.bin", 4096, &keeper),
            duplicate("/p/b.bin", 4096, &keeper),
        ];
        CleanPlan {
            reclaimable: 8192,
            candidates,
            ..CleanPlan::default()
        }
    }

    fn parsed(payload: String) -> serde_json::Value {
        serde_json::from_str(&payload).expect("parse")
    }

    /// The flat list is a true description of the removal; the groups are the
    /// question anyone actually asks of duplicates. A consumer should not have
    /// to rebuild them by keeper path.
    #[test]
    fn the_duplicate_document_is_groups() {
        let value = parsed(render_dup_plan(&dup_plan()).expect("serialize"));

        let groups = value["groups"].as_array().expect("groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["keeper"]["path"], "/p/keeper.bin");
        assert_eq!(groups[0]["reclaimable"], 8192);
        assert_eq!(groups[0]["copies"].as_array().expect("copies").len(), 2);
        assert_eq!(groups[0]["copies"][0]["allocated"], 4096);
        assert_eq!(value["reclaimable"], 8192);
    }

    /// Whatever the report shows, the document says — including the one thing a
    /// reader has to know to judge the choice.
    #[test]
    fn the_keeper_carries_its_date_and_whether_the_rule_held() {
        let value = parsed(render_dup_plan(&dup_plan()).expect("serialize"));

        assert_eq!(value["groups"][0]["keeper"]["date"], 1_700_000_000);
        assert_eq!(value["groups"][0]["keeper"]["fell_back"], false);
    }

    /// `below_rule_minimum` is absent because no rule is in play; the other two
    /// narrowings are, and spelled as `CleanPlan` spells them.
    #[test]
    fn the_narrowings_keep_the_spelling_the_other_document_uses() {
        let mut plan = dup_plan();
        plan.filtered_out = 2;
        plan.too_small = 3;

        let value = parsed(render_dup_plan(&plan).expect("serialize"));

        assert_eq!(value["filtered_out"], 2);
        assert_eq!(value["too_small"], 3);
        assert!(value.get("below_rule_minimum").is_none());
    }

    /// `-d` and `--sort` lay a report out for a person. A machine-readable
    /// document quietly shortened by one of them says nothing about having been.
    #[test]
    fn the_duplicate_document_is_whole_whatever_the_display_flags_said() {
        let value = parsed(render_dup_plan(&dup_plan()).expect("serialize"));
        assert_eq!(
            value["groups"][0]["copies"]
                .as_array()
                .expect("copies")
                .len(),
            2,
            "both copies, at any --depth"
        );
    }

    fn a_plan() -> CleanPlan {
        CleanPlan {
            candidates: vec![
                Candidate {
                    path: PathBuf::from("/p/node_modules"),
                    rule: "node-modules".into(),
                    tier: Tier::Purge,
                    purge: true,
                    duplicate_of: None,
                    allocated: 4096,
                    shared: false,
                },
                Candidate {
                    path: PathBuf::from("/p/old.bin"),
                    rule: "old".into(),
                    tier: Tier::Confirm,
                    purge: false,
                    duplicate_of: None,
                    allocated: 1024,
                    shared: true,
                },
            ],
            reclaimable: 5120,
            excluded: vec![Excluded {
                path: PathBuf::from("/Windows"),
                reason: ExcludeReason::Denylisted,
            }],
            filtered_out: 1,
            too_small: 2,
            below_rule_minimum: 3,
        }
    }

    #[test]
    fn a_plan_round_trips() {
        let plan = a_plan();

        let json = render_plan(&plan).expect("serialize");
        let parsed: CleanPlan = serde_json::from_str(&json).expect("parse back");

        assert_eq!(parsed, plan);
    }

    /// Everything the human report says about a candidate, so a consumer can
    /// reach the same conclusions without parsing prose.
    #[test]
    fn every_candidate_carries_what_the_report_shows() {
        let value: serde_json::Value =
            serde_json::from_str(&render_plan(&a_plan()).expect("serialize")).expect("parse");
        let first = &value["candidates"][0];

        assert_eq!(first["path"], "/p/node_modules");
        assert_eq!(first["rule"], "node-modules");
        assert_eq!(first["tier"], "purge");
        assert_eq!(first["purge"], true);
        assert_eq!(first["allocated"], 4096, "a raw byte count, never `4.0K`");
        assert_eq!(first["shared"], false);
        assert_eq!(value["candidates"][1]["shared"], true);
    }

    /// The two are different questions: what a rule says, and where the
    /// candidate goes. `--purge` moves the second and not the first.
    #[test]
    fn the_tier_and_the_destination_are_both_there() {
        let mut plan = a_plan();
        for candidate in &mut plan.candidates {
            candidate.purge = true;
        }
        let value: serde_json::Value =
            serde_json::from_str(&render_plan(&plan).expect("serialize")).expect("parse");

        assert_eq!(value["candidates"][1]["tier"], "confirm");
        assert_eq!(
            value["candidates"][1]["purge"], true,
            "destroyed under the flag, and still needing confirmation"
        );
    }

    /// The word in the document is the word in the config file. A consumer
    /// reading `"Trash"` and writing it back would find it refused.
    #[test]
    fn a_tier_is_spelled_as_the_config_file_spells_it() {
        for (tier, word) in [
            (Tier::Purge, "purge"),
            (Tier::Trash, "trash"),
            (Tier::Confirm, "confirm"),
        ] {
            let plan = CleanPlan {
                candidates: vec![Candidate {
                    tier,
                    ..a_plan().candidates[0].clone()
                }],
                ..CleanPlan::default()
            };
            let value: serde_json::Value =
                serde_json::from_str(&render_plan(&plan).expect("serialize")).expect("parse");

            assert_eq!(value["candidates"][0]["tier"], word);
        }
    }

    /// Why something was refused is machine-readable too — a consumer that
    /// cannot tell the denylist from the git guard cannot decide what to do.
    #[test]
    fn a_refusal_carries_its_reason() {
        let value: serde_json::Value =
            serde_json::from_str(&render_plan(&a_plan()).expect("serialize")).expect("parse");

        assert_eq!(value["excluded"][0]["path"], "/Windows");
        assert_eq!(value["excluded"][0]["reason"], "denylisted");
    }

    /// The counts the report turns into three separate sentences, each naming a
    /// different remedy.
    #[test]
    fn the_narrowings_are_counted_separately() {
        let value: serde_json::Value =
            serde_json::from_str(&render_plan(&a_plan()).expect("serialize")).expect("parse");

        assert_eq!(value["filtered_out"], 1);
        assert_eq!(value["too_small"], 2);
        assert_eq!(value["below_rule_minimum"], 3);
    }

    /// The outcome is a different document from the plan, and says which half
    /// of a mixed run can be brought back.
    #[test]
    fn an_outcome_reports_both_halves_and_every_failure() {
        let outcome = CleanOutcome {
            trashed: Reclaimed {
                paths: vec![PathBuf::from("/p/target")],
                bytes: 2048,
            },
            purged: Reclaimed {
                paths: vec![PathBuf::from("/p/node_modules")],
                bytes: 4096,
            },
            failed: vec![TrashFailure {
                path: PathBuf::from("/p/stuck"),
                reason: "Permission denied".into(),
            }],
        };

        let json = render_outcome_json(&outcome).expect("serialize");
        let parsed: CleanOutcome = serde_json::from_str(&json).expect("parse back");
        assert_eq!(parsed, outcome);

        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(value["trashed"]["bytes"], 2048);
        assert_eq!(value["purged"]["bytes"], 4096);
        assert_eq!(
            value.get("reclaimed"),
            None,
            "the two are never added up here: one figure would not say what \
             can be brought back"
        );
        assert_eq!(value["failed"][0]["reason"], "Permission denied");
    }

    #[test]
    fn json_round_trips() {
        let tree = tree(node(
            "root",
            8192,
            8000,
            vec![node("root/a.bin", 4096, 4000, vec![])],
        ));

        let json = render_json(&tree).expect("serialize");
        let parsed: ScanTree = serde_json::from_str(&json).expect("parse back");
        assert_eq!(parsed, tree, "JSON must round-trip to an identical tree");
    }

    #[test]
    fn json_carries_scan_numbers() {
        let tree = tree(node("root", 123_456, 120_000, vec![]));

        let value: serde_json::Value =
            serde_json::from_str(&render_json(&tree).expect("serialize")).expect("parse");
        // Raw byte counts, not the human report's "120.6K".
        assert_eq!(value["root"]["allocated"], 123_456);
        assert_eq!(value["root"]["apparent"], 120_000);
    }

    #[test]
    fn json_includes_skipped() {
        let tree = tree_with(
            node("root", 0, 0, vec![]),
            vec![SkippedEntry {
                path: PathBuf::from("root/locked"),
                reason: SkipReason::PermissionDenied,
            }],
        );

        let value: serde_json::Value =
            serde_json::from_str(&render_json(&tree).expect("serialize")).expect("parse");
        let skipped = value["skipped"].as_array().expect("skipped array");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["path"], "root/locked");
        assert_eq!(skipped[0]["reason"], "PermissionDenied");
    }

    /// A three-level tree with several children per level round-trips with
    /// every value intact *and* every child still in its original position —
    /// catches a dedup/reorder bug that a flat, single-child fixture couldn't.
    #[test]
    fn json_round_trips_nested_children_preserving_order_and_values() {
        let tree = tree(node(
            "root",
            90_000,
            89_000,
            vec![
                node(
                    "root/b_mid",
                    50_000,
                    49_000,
                    vec![
                        node("root/b_mid/two", 20_000, 19_500, vec![]),
                        node("root/b_mid/one", 30_000, 29_500, vec![]),
                    ],
                ),
                node("root/a_leaf", 40_000, 40_000, vec![]),
            ],
        ));

        let json = render_json(&tree).expect("serialize");
        let parsed: ScanTree = serde_json::from_str(&json).expect("parse back");

        assert_eq!(
            parsed, tree,
            "the whole nested tree must round-trip exactly"
        );
        // Order specifically, since `PartialEq` on `Vec` already implies it —
        // spelled out so a future switch to an order-blind comparison here
        // wouldn't silently stop checking this.
        let mid = &parsed.root.children[0];
        assert_eq!(mid.path, PathBuf::from("root/b_mid"));
        assert_eq!(mid.children[0].path, PathBuf::from("root/b_mid/two"));
        assert_eq!(mid.children[1].path, PathBuf::from("root/b_mid/one"));
        assert_eq!(parsed.root.children[1].path, PathBuf::from("root/a_leaf"));
    }

    /// `SkipReason` has three shapes on the wire: two bare strings and one
    /// `{"Other": "..."}` object. Only `PermissionDenied` was covered before.
    #[test]
    fn skip_reason_not_found_and_other_round_trip_with_the_right_wire_shape() {
        let tree = tree_with(
            node("root", 0, 0, vec![]),
            vec![
                SkippedEntry {
                    path: PathBuf::from("root/vanished"),
                    reason: SkipReason::NotFound,
                },
                SkippedEntry {
                    path: PathBuf::from("root/weird"),
                    reason: SkipReason::Other("disk on fire".to_owned()),
                },
            ],
        );

        let json = render_json(&tree).expect("serialize");

        let value: serde_json::Value = serde_json::from_str(&json).expect("parse as Value");
        let skipped = value["skipped"].as_array().expect("skipped array");
        assert_eq!(
            skipped[0]["reason"], "NotFound",
            "unit variant is a bare string"
        );
        assert_eq!(
            skipped[1]["reason"],
            serde_json::json!({ "Other": "disk on fire" }),
            "the tuple variant is a single-key object keyed by variant name"
        );

        let parsed: ScanTree = serde_json::from_str(&json).expect("parse back to ScanTree");
        assert_eq!(
            parsed, tree,
            "both variants must round-trip to identical values"
        );
    }

    /// A zero-byte tree (an empty root, no children, nothing skipped) must
    /// still serialize — the numeric fields are `0`, not omitted or null.
    #[test]
    fn empty_tree_serializes() {
        let tree = tree(node("root", 0, 0, vec![]));

        let json = render_json(&tree).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(value["root"]["allocated"], 0);
        assert_eq!(value["root"]["apparent"], 0);
        assert_eq!(value["root"]["children"], serde_json::json!([]));
        assert_eq!(value["skipped"], serde_json::json!([]));

        let parsed: ScanTree = serde_json::from_str(&json).expect("parse back");
        assert_eq!(parsed, tree);
    }

    /// `is_dir` is carried on the wire, not reconstructed from whether
    /// `children` is empty — an empty directory and a file must stay
    /// distinguishable after a round trip.
    #[test]
    fn is_dir_survives_the_round_trip_even_for_a_childless_directory() {
        let empty_dir = ScanNode {
            path: PathBuf::from("root/empty"),
            allocated: 0,
            apparent: 0,
            is_dir: true,
            modified: None,
            links: None,
            children: Vec::new(),
        };
        let tree = tree(node("root", 0, 0, vec![empty_dir.clone()]));

        let json = render_json(&tree).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(
            value["root"]["children"][0]["is_dir"], true,
            "a childless directory must still be tagged is_dir: true"
        );

        let parsed: ScanTree = serde_json::from_str(&json).expect("parse back");
        assert_eq!(parsed.root.children[0], empty_dir);
    }

    /// The whole point of `--json` is machine-readable numbers: a size must
    /// come back as a JSON number equal to the exact byte count, never a
    /// formatted string like the human report's "1.0M".
    #[test]
    fn sizes_are_raw_json_numbers_not_formatted_strings() {
        let tree = tree(node("root", 1_048_576, 1_048_575, vec![]));

        let value: serde_json::Value =
            serde_json::from_str(&render_json(&tree).expect("serialize")).expect("parse");

        assert!(
            value["root"]["allocated"].is_u64(),
            "allocated must be a JSON number, got {:?}",
            value["root"]["allocated"]
        );
        assert_eq!(value["root"]["allocated"], 1_048_576);
        assert_eq!(value["root"]["apparent"], 1_048_575);
        // Never the human report's rounded/unit-suffixed form.
        assert_ne!(value["root"]["allocated"].as_str(), Some("1.0M"));
    }

    /// The v0.2 additions must be on the wire in a shape a consumer can use:
    /// `modified` as a plain number of seconds (not serde's nested
    /// `{secs_since_epoch, …}` object), `links` as a number or `null`, and
    /// `link_groups` as an array of arrays of paths.
    #[test]
    fn json_carries_the_new_fields() {
        let mut file = node("root/a.bin", 4096, 4000, vec![]);
        file.modified = Some(UNIX_EPOCH + Duration::from_secs(1_750_000_000));
        file.links = Some(2);
        let mut root = node("root", 8192, 8000, vec![file]);
        root.modified = Some(UNIX_EPOCH + Duration::from_secs(1_750_000_001));

        let mut tree = tree(root);
        tree.link_groups = vec![vec![
            PathBuf::from("root/a.bin"),
            PathBuf::from("root/b.bin"),
        ]];

        let json = render_json(&tree).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");

        let child = &value["root"]["children"][0];
        assert_eq!(
            child["modified"], 1_750_000_000_i64,
            "modified is whole seconds since the Unix epoch, as a bare number"
        );
        assert_eq!(child["links"], 2);
        assert_eq!(
            value["link_groups"],
            serde_json::json!([["root/a.bin", "root/b.bin"]])
        );

        let parsed: ScanTree = serde_json::from_str(&json).expect("parse back");
        assert_eq!(parsed, tree, "the additions must round-trip");
    }

    /// An entry the platform gave no timestamp for, and one on which no link
    /// count is available (every entry on Windows), must serialize as `null` —
    /// never as `0`, which a consumer would read as 1970, nor as `1`, which
    /// would claim the content is unshared.
    #[test]
    fn unknown_signals_serialize_as_null() {
        let tree = tree(node("root", 0, 0, vec![]));

        let value: serde_json::Value =
            serde_json::from_str(&render_json(&tree).expect("serialize")).expect("parse");

        assert_eq!(value["root"]["modified"], serde_json::Value::Null);
        assert_eq!(value["root"]["links"], serde_json::Value::Null);
        assert_eq!(value["link_groups"], serde_json::json!([]));
    }

    /// serde's own `SystemTime` impl refuses to serialize anything before 1970,
    /// which would turn one file with a bad clock into a failed `--json` run for
    /// the whole scan. The custom representation must simply carry the negative
    /// second count.
    #[test]
    fn a_pre_1970_timestamp_serializes_rather_than_failing_the_scan() {
        let mut root = node("root", 0, 0, vec![]);
        let ancient = UNIX_EPOCH - Duration::from_secs(86_400);
        root.modified = Some(ancient);
        let tree = tree(root);

        let json = render_json(&tree).expect("a pre-epoch mtime must not fail the payload");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(value["root"]["modified"], -86_400_i64);

        let parsed: ScanTree = serde_json::from_str(&json).expect("parse back");
        assert_eq!(parsed.root.modified, Some(ancient));
    }

    /// Sub-second precision is deliberately dropped (the age rule works in
    /// days), so a round trip lands on the containing second rather than the
    /// original instant. Pinned here so the loss is a documented property
    /// instead of a surprise for whoever next compares two trees.
    #[test]
    fn modified_round_trips_to_whole_seconds() {
        let mut root = node("root", 0, 0, vec![]);
        root.modified = Some(UNIX_EPOCH + Duration::new(1_750_000_000, 999_000_000));
        let tree = tree(root);

        let parsed: ScanTree =
            serde_json::from_str(&render_json(&tree).expect("serialize")).expect("parse back");

        assert_eq!(
            parsed.root.modified,
            Some(UNIX_EPOCH + Duration::from_secs(1_750_000_000))
        );
    }

    /// A non-UTF-8 path can't become a JSON string; that must surface as an
    /// `Err`, never a panic.
    #[cfg(unix)]
    #[test]
    fn non_utf8_path_is_an_error_not_a_panic() {
        use std::os::unix::ffi::OsStrExt;

        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(&[0xff, 0xfe]));
        let tree = tree(ScanNode {
            path: bad,
            allocated: 0,
            apparent: 0,
            is_dir: true,
            modified: None,
            links: None,
            children: Vec::new(),
        });

        assert!(
            render_json(&tree).is_err(),
            "a non-UTF-8 path must be a serialization error, not a panic"
        );
    }
}
