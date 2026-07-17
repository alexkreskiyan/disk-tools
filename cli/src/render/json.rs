//! `--json`: the scan as machine-readable JSON on stdout.
//!
//! Sizes are the raw byte counts (the human report formats the same numbers), so
//! the two outputs never disagree. `skipped` is carried alongside the tree.

use disk_tools_core::ScanTree;

/// Serialize the whole scan (tree + skipped) as pretty JSON.
///
/// Returns `Err` only when a path is not valid UTF-8 — serde cannot encode such
/// a `PathBuf` as a JSON string. The caller turns that into a clean error and a
/// non-zero exit rather than a panic.
pub fn render_json(tree: &ScanTree) -> serde_json::Result<String> {
    serde_json::to_string_pretty(tree)
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_tools_core::{ScanNode, ScanTree, SkipReason, SkippedEntry};
    use std::path::PathBuf;

    fn node(path: &str, allocated: u64, apparent: u64, children: Vec<ScanNode>) -> ScanNode {
        ScanNode {
            path: PathBuf::from(path),
            allocated,
            apparent,
            is_dir: !children.is_empty(),
            children,
        }
    }

    #[test]
    fn json_round_trips() {
        let tree = ScanTree {
            root: node(
                "root",
                8192,
                8000,
                vec![node("root/a.bin", 4096, 4000, vec![])],
            ),
            skipped: Vec::new(),
        };

        let json = render_json(&tree).expect("serialize");
        let parsed: ScanTree = serde_json::from_str(&json).expect("parse back");
        assert_eq!(parsed, tree, "JSON must round-trip to an identical tree");
    }

    #[test]
    fn json_carries_scan_numbers() {
        let tree = ScanTree {
            root: node("root", 123_456, 120_000, vec![]),
            skipped: Vec::new(),
        };

        let value: serde_json::Value =
            serde_json::from_str(&render_json(&tree).expect("serialize")).expect("parse");
        // Raw byte counts, not the human report's "120.6K".
        assert_eq!(value["root"]["allocated"], 123_456);
        assert_eq!(value["root"]["apparent"], 120_000);
    }

    #[test]
    fn json_includes_skipped() {
        let tree = ScanTree {
            root: node("root", 0, 0, vec![]),
            skipped: vec![SkippedEntry {
                path: PathBuf::from("root/locked"),
                reason: SkipReason::PermissionDenied,
            }],
        };

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
        let tree = ScanTree {
            root: node(
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
            ),
            skipped: Vec::new(),
        };

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
        let tree = ScanTree {
            root: node("root", 0, 0, vec![]),
            skipped: vec![
                SkippedEntry {
                    path: PathBuf::from("root/vanished"),
                    reason: SkipReason::NotFound,
                },
                SkippedEntry {
                    path: PathBuf::from("root/weird"),
                    reason: SkipReason::Other("disk on fire".to_owned()),
                },
            ],
        };

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
        let tree = ScanTree {
            root: node("root", 0, 0, vec![]),
            skipped: Vec::new(),
        };

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
            children: Vec::new(),
        };
        let tree = ScanTree {
            root: node("root", 0, 0, vec![empty_dir.clone()]),
            skipped: Vec::new(),
        };

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
        let tree = ScanTree {
            root: node("root", 1_048_576, 1_048_575, vec![]),
            skipped: Vec::new(),
        };

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

    /// A non-UTF-8 path can't become a JSON string; that must surface as an
    /// `Err`, never a panic.
    #[cfg(unix)]
    #[test]
    fn non_utf8_path_is_an_error_not_a_panic() {
        use std::os::unix::ffi::OsStrExt;

        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(&[0xff, 0xfe]));
        let tree = ScanTree {
            root: ScanNode {
                path: bad,
                allocated: 0,
                apparent: 0,
                is_dir: true,
                children: Vec::new(),
            },
            skipped: Vec::new(),
        };

        assert!(
            render_json(&tree).is_err(),
            "a non-UTF-8 path must be a serialization error, not a panic"
        );
    }
}
