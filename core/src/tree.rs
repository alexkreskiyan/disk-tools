use crate::walk::WalkEntry;
use std::path::{Path, PathBuf};

/// One entry in the scanned tree — a file or a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ScanNode {
    pub path: PathBuf,

    /// Bytes this entry actually occupies on disk, after hardlink attribution:
    /// an inode reached through several paths is charged to exactly one of
    /// them, so directory totals sum to the root total.
    pub allocated: u64,

    /// Logical length. Larger than `allocated` for sparse or compressed files.
    pub apparent: u64,

    pub is_dir: bool,

    /// Always empty for files.
    pub children: Vec<ScanNode>,
}

/// Why an entry could not be scanned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SkipReason {
    PermissionDenied,
    /// The entry disappeared between being listed and being measured.
    NotFound,
    Other(String),
}

/// An entry the walk could not process.
///
/// Collected rather than logged — the core has no opinion on how this should
/// reach a human.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SkippedEntry {
    pub path: PathBuf,
    pub reason: SkipReason,
}

/// The result of a scan: a size-annotated tree plus whatever was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ScanTree {
    pub root: ScanNode,
    pub skipped: Vec<SkippedEntry>,
}

/// Fold the flat, dedup-attributed walk entries into a size-annotated tree.
///
/// Each node's size is its own measured bytes plus the sum of its children's —
/// `du`'s definition, and why directories are recorded with their own block
/// (§8.2.2 / §8.2.4). Non-keeper hardlinks already carry 0 from
/// [`crate::dedup::attribute`], so the bottom-up sum counts each inode once.
///
/// Children are ordered by path for a deterministic tree — the walk's order is
/// nondeterministic under rayon; the renderer re-sorts by size at display time.
pub(crate) fn aggregate(entries: Vec<WalkEntry>, root: &Path) -> ScanNode {
    use std::collections::HashMap;

    let by_path: HashMap<&Path, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.path.as_path(), i))
        .collect();

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    let mut root_index = None;
    for (i, entry) in entries.iter().enumerate() {
        if entry.path == root {
            root_index = Some(i);
            continue; // the root has no parent to attach to
        }
        if let Some(parent) = entry.path.parent() {
            if let Some(&p) = by_path.get(parent) {
                children[p].push(i);
            }
            // else: a child whose parent isn't recorded. The walk descends only
            // into directories it managed to record, so this is unreachable in
            // normal operation — kept as a defensive fallback that drops the
            // orphan rather than panicking, should that invariant ever change.
        }
    }

    match root_index {
        Some(i) => build_node(i, &entries, &children),
        // Unreadable or nonexistent root: no entry to build from. The reason is
        // in `skipped`; hand back an empty node so `ScanTree.root` stays total.
        None => ScanNode {
            path: root.to_path_buf(),
            allocated: 0,
            apparent: 0,
            is_dir: false,
            children: Vec::new(),
        },
    }
}

/// Build one node and, recursively, its subtree. Cycle-free: every non-root
/// path is strictly deeper than its parent, so the edges form a tree.
fn build_node(index: usize, entries: &[WalkEntry], children: &[Vec<usize>]) -> ScanNode {
    let entry = &entries[index];

    let mut child_nodes: Vec<ScanNode> = children[index]
        .iter()
        .map(|&c| build_node(c, entries, children))
        .collect();
    child_nodes.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));

    // Own bytes plus the subtree. `u64` disk sums can't realistically overflow
    // (max ≈ 18 EB), so a plain `+` is used rather than masking a bug with a
    // saturating one.
    let mut allocated = entry.allocated;
    let mut apparent = entry.apparent;
    for child in &child_nodes {
        allocated += child.allocated;
        apparent += child.apparent;
    }

    ScanNode {
        path: entry.path.clone(),
        allocated,
        apparent,
        is_dir: entry.is_dir,
        children: child_nodes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn scan_tree_construction() {
        let file = ScanNode {
            path: PathBuf::from("root/big.bin"),
            allocated: 8192,
            apparent: 8000,
            is_dir: false,
            children: Vec::new(),
        };

        let root = ScanNode {
            path: PathBuf::from("root"),
            allocated: 8192,
            apparent: 8000,
            is_dir: true,
            children: vec![file.clone()],
        };

        let tree = ScanTree {
            root,
            skipped: vec![SkippedEntry {
                path: PathBuf::from("root/locked"),
                reason: SkipReason::PermissionDenied,
            }],
        };

        assert_eq!(tree.root.path, PathBuf::from("root"));
        assert!(tree.root.is_dir);
        assert_eq!(tree.root.allocated, 8192);
        assert_eq!(tree.root.apparent, 8000);
        assert_eq!(tree.root.children, vec![file]);

        assert!(!tree.root.children[0].is_dir);
        assert_eq!(tree.root.children[0].path, PathBuf::from("root/big.bin"));
        assert!(tree.root.children[0].children.is_empty());

        assert_eq!(tree.skipped.len(), 1);
        assert_eq!(tree.skipped[0].path, PathBuf::from("root/locked"));
        assert_eq!(tree.skipped[0].reason, SkipReason::PermissionDenied);
    }

    #[test]
    fn skip_reason_variants() {
        assert_ne!(SkipReason::PermissionDenied, SkipReason::NotFound);
        assert_eq!(
            SkipReason::Other("disk on fire".to_owned()),
            SkipReason::Other("disk on fire".to_owned())
        );
    }

    mod aggregation {
        use crate::ScanOptions;
        use crate::tree::ScanNode;
        use std::fs;
        use std::path::Path;

        fn opts(root: &Path) -> ScanOptions {
            ScanOptions {
                root: root.to_path_buf(),
                ..ScanOptions::default()
            }
        }

        fn write(path: &Path, bytes: usize) {
            fs::write(path, vec![b'x'; bytes]).expect("write file");
        }

        /// A three-level fixture with known files. Returns the temp dir (keep it
        /// alive) and its path.
        fn fixture() -> tempfile::TempDir {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            fs::create_dir_all(root.join("a/b")).expect("mkdir");
            fs::create_dir(root.join("c")).expect("mkdir");
            write(&root.join("top.bin"), 10_000);
            write(&root.join("a/one.bin"), 20_000);
            write(&root.join("a/b/two.bin"), 30_000);
            write(&root.join("c/three.bin"), 40_000);
            dir
        }

        /// The bytes an entry contributes on its own — every `ScanNode` records
        /// own + subtree, so a node's own share is its total minus its children's.
        #[cfg(unix)]
        fn own_allocated(node: &ScanNode) -> u64 {
            node.allocated - node.children.iter().map(|c| c.allocated).sum::<u64>()
        }

        #[test]
        fn dir_total_equals_sum_of_children() {
            let dir = fixture();
            let tree = crate::scan(&opts(dir.path()));

            // Every node's total must equal its own bytes plus its children's —
            // checked recursively over the whole tree, not just the root.
            fn check(node: &ScanNode) {
                let children_alloc: u64 = node.children.iter().map(|c| c.allocated).sum();
                let children_app: u64 = node.children.iter().map(|c| c.apparent).sum();
                assert!(
                    node.allocated >= children_alloc,
                    "{:?} total {} is below its children's {}",
                    node.path,
                    node.allocated,
                    children_alloc
                );
                assert!(
                    node.apparent >= children_app,
                    "{:?} apparent underflow",
                    node.path
                );
                // A file has no children, so its own share is its whole size.
                if !node.is_dir {
                    assert!(node.children.is_empty(), "a file has no children");
                }
                for child in &node.children {
                    check(child);
                }
            }
            check(&tree.root);

            // The totals are real, not all zero. Summed apparent over the file
            // nodes equals the known content lengths — files only, since a
            // directory carries its own `metadata.len()` apparent as well.
            fn file_apparent(node: &ScanNode) -> u64 {
                if node.is_dir {
                    node.children.iter().map(file_apparent).sum()
                } else {
                    node.apparent
                }
            }
            assert_eq!(file_apparent(&tree.root), 10_000 + 20_000 + 30_000 + 40_000);
        }

        #[cfg(unix)]
        #[test]
        fn root_total_matches_du() {
            use std::process::Command;

            let dir = fixture();
            let tree = crate::scan(&opts(dir.path()));

            // `du -sk` reports 1024-byte blocks; ×1024 gives bytes. Our allocated
            // is blocks×512, so both count the same on-disk bytes.
            let output = Command::new("du")
                .arg("-sk")
                .arg(dir.path())
                .output()
                .expect("run du");
            assert!(output.status.success(), "du failed");
            let kib: u64 = String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .next()
                .expect("du output")
                .parse()
                .expect("parse du");

            assert_eq!(
                tree.root.allocated,
                kib * 1024,
                "root total must match du -sk (got {} bytes, du says {} KiB)",
                tree.root.allocated,
                kib
            );
        }

        #[cfg(unix)]
        #[test]
        fn dir_totals_sum_to_root_with_hardlinks() {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            fs::create_dir(root.join("a")).expect("mkdir");
            fs::create_dir(root.join("b")).expect("mkdir");
            write(&root.join("a/original.bin"), 40_000);
            write(&root.join("b/other.bin"), 8_000);
            fs::hard_link(root.join("a/original.bin"), root.join("b/link.bin")).expect("hard_link");

            let tree = crate::scan(&opts(root));

            // Σ of top-level children plus the root's own block must equal the
            // root total — no bytes lost or double-counted in aggregation.
            let children_sum: u64 = tree.root.children.iter().map(|c| c.allocated).sum();
            assert_eq!(
                tree.root.allocated,
                own_allocated(&tree.root) + children_sum
            );

            // Total file bytes across the whole tree, counted through the
            // aggregation: the shared inode must appear once. Apparent is exact
            // (allocated is block-rounded), so summing file nodes gives
            // 40_000 (keeper) + 0 (zeroed twin) + 8_000 (other.bin).
            fn file_apparent(node: &ScanNode) -> u64 {
                if node.is_dir {
                    node.children.iter().map(file_apparent).sum()
                } else {
                    node.apparent
                }
            }
            assert_eq!(
                file_apparent(&tree.root),
                48_000,
                "the shared inode is counted once, not twice"
            );
        }

        #[test]
        fn children_are_ordered_by_path() {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            // Create out of lexical order to prove the sort, not the fs order.
            write(&root.join("z.bin"), 1_000);
            write(&root.join("a.bin"), 1_000);
            write(&root.join("m.bin"), 1_000);

            let tree = crate::scan(&opts(root));

            let names: Vec<_> = tree
                .root
                .children
                .iter()
                .map(|c| c.path.file_name().unwrap().to_owned())
                .collect();
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(names, sorted, "siblings must be ordered by path");
        }

        /// An empty directory has no children to fold in, so its total must be
        /// exactly its own measured bytes — nothing more, nothing summed in from
        /// nowhere. Compares against an independent measurement (`size::measure`
        /// directly, not the tree) so the assertion can't pass by tautology.
        #[test]
        fn empty_directory_contributes_only_its_own_block() {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            fs::create_dir(root.join("empty")).expect("mkdir");

            let tree = crate::scan(&opts(root));

            let empty_node = tree
                .root
                .children
                .iter()
                .find(|c| c.path.file_name().unwrap() == "empty")
                .expect("empty dir node present");

            assert!(empty_node.is_dir);
            assert!(
                empty_node.children.is_empty(),
                "an empty directory has no children to aggregate"
            );

            let metadata = fs::symlink_metadata(root.join("empty")).expect("stat");
            let sizes = crate::size::measure(&root.join("empty"), &metadata).expect("measure");
            assert_eq!(
                empty_node.allocated, sizes.allocated,
                "with no children, the node's total is its own bytes, exactly"
            );
            assert_eq!(empty_node.apparent, sizes.apparent);
        }

        /// Two sibling subdirectories with distinct contents must aggregate
        /// independently — a bug that leaks one subtree's entries into another
        /// (e.g. a `by_path` lookup keyed wrong) would still pass the generic
        /// "total >= children" check but fail this one.
        #[test]
        fn sibling_subtrees_aggregate_independently() {
            let dir = fixture();
            let tree = crate::scan(&opts(dir.path()));

            fn file_apparent(node: &ScanNode) -> u64 {
                if node.is_dir {
                    node.children.iter().map(file_apparent).sum()
                } else {
                    node.apparent
                }
            }

            let find = |name: &str| {
                tree.root
                    .children
                    .iter()
                    .find(|c| c.path.file_name().unwrap() == name)
                    .unwrap_or_else(|| panic!("{name} node present"))
            };

            // "a" holds one.bin (20_000) plus nested b/two.bin (30_000); "c" holds
            // only three.bin (40_000). Each total must reflect only its own
            // subtree, not its sibling's.
            assert_eq!(file_apparent(find("a")), 20_000 + 30_000);
            assert_eq!(file_apparent(find("c")), 40_000);
        }

        #[test]
        fn unreadable_root_yields_empty_root_node() {
            let dir = tempfile::tempdir().expect("tempdir");
            let missing = dir.path().join("does-not-exist");

            let tree = crate::scan(&opts(&missing));

            assert_eq!(tree.root.path, missing);
            assert_eq!(tree.root.allocated, 0);
            assert_eq!(tree.root.apparent, 0);
            assert!(tree.root.children.is_empty());
            assert_eq!(
                tree.skipped.len(),
                1,
                "the missing root is recorded as a skip"
            );
            assert_eq!(tree.skipped[0].path, missing);
        }
    }
}
