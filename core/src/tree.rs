use crate::walk::WalkEntry;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

    /// This entry's own mtime — for a directory its own, never derived from its
    /// children. `None` when the platform recorded none, which callers must read
    /// as "unknown" rather than "old" or "new".
    ///
    /// Serialized as whole seconds since the Unix epoch, negative before 1970.
    #[cfg_attr(feature = "serde", serde(with = "unix_seconds"))]
    pub modified: Option<SystemTime>,

    /// How many names this inode has in total, counting any outside the scan.
    ///
    /// `Some` on Unix. **`None` on Windows and for every directory** — the
    /// Windows directory listing carries no link count and this project will not
    /// open a handle per file to obtain one, and a directory's `nlink` counts
    /// subdirectories, which is a different quantity entirely. `None` means
    /// *unknown*, never `1`.
    pub links: Option<u32>,

    /// Always empty for files.
    pub children: Vec<ScanNode>,
}

/// `Option<SystemTime>` on the wire as an optional count of whole seconds since
/// the Unix epoch, negative for anything older than 1970.
///
/// serde's own `SystemTime` impl was not usable here on two counts: it emits a
/// nested `{secs_since_epoch, nanos_since_epoch}` object where consumers want a
/// number, and it **fails to serialize any pre-1970 instant** — so one file with
/// a bad clock would turn an entire `--json` payload into an error. A scan must
/// not be that fragile about a timestamp it merely passes through.
///
/// The cost is sub-second precision, which nothing here wants: the age rule this
/// field exists for works in days.
#[cfg(feature = "serde")]
mod unix_seconds {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(
        value: &Option<SystemTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let seconds = value.and_then(|time| match time.duration_since(UNIX_EPOCH) {
            Ok(since) => i64::try_from(since.as_secs()).ok(),
            // Before 1970: the error carries the interval by which, so the sign
            // is recovered rather than the value being lost.
            Err(err) => i64::try_from(err.duration().as_secs()).ok().map(|s| -s),
        });
        serializer.serialize_some(&seconds)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<SystemTime>, D::Error> {
        let seconds = Option::<i64>::deserialize(deserializer)?;
        Ok(seconds.and_then(|seconds| match seconds.unsigned_abs() {
            magnitude if seconds < 0 => UNIX_EPOCH.checked_sub(Duration::from_secs(magnitude)),
            magnitude => UNIX_EPOCH.checked_add(Duration::from_secs(magnitude)),
        }))
    }
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

    /// Paths **within this scan** that share an inode — one inner vector per
    /// group, only groups of two or more, each sorted so the first element is
    /// the path the bytes were charged to.
    ///
    /// This is what makes "would deleting X actually free its bytes?" answerable
    /// without a filesystem-wide link census: if a group has a member outside
    /// the thing being deleted, the answer is no. Unlike [`ScanNode::links`] it
    /// works on **both** platforms, since Windows entries carry a `FileId` too.
    /// What it cannot see is sharing with paths outside the scanned tree.
    pub link_groups: Vec<Vec<PathBuf>>,
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
        // A child whose parent is not recorded falls through silently. The walk
        // descends only into directories it managed to record, so that is
        // unreachable in normal operation — dropping the orphan rather than
        // panicking is the defensive choice, should the invariant ever change.
        if let Some(parent) = entry.path.parent()
            && let Some(&p) = by_path.get(parent)
        {
            children[p].push(i);
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
            // Nothing was measured, so nothing is known — not even a guess at
            // the root's own mtime, which would invite the age rule to judge a
            // node the scan never saw.
            modified: None,
            links: None,
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
        // Facts about this entry alone, so unlike the sizes they are carried
        // across rather than folded — a directory's mtime is its own, and its
        // children's ages say nothing about it.
        modified: entry.modified,
        links: entry.links,
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
            modified: Some(SystemTime::UNIX_EPOCH),
            links: Some(1),
            children: Vec::new(),
        };

        let root = ScanNode {
            path: PathBuf::from("root"),
            allocated: 8192,
            apparent: 8000,
            is_dir: true,
            modified: None,
            links: None,
            children: vec![file.clone()],
        };

        let tree = ScanTree {
            root,
            skipped: vec![SkippedEntry {
                path: PathBuf::from("root/locked"),
                reason: SkipReason::PermissionDenied,
            }],
            link_groups: Vec::new(),
        };

        assert_eq!(tree.root.path, PathBuf::from("root"));
        assert!(tree.root.is_dir);
        assert_eq!(tree.root.allocated, 8192);
        assert_eq!(tree.root.apparent, 8000);
        assert_eq!(tree.root.children, vec![file]);

        assert!(!tree.root.children[0].is_dir);
        assert_eq!(tree.root.children[0].path, PathBuf::from("root/big.bin"));
        assert!(tree.root.children[0].children.is_empty());

        assert_eq!(tree.root.children[0].modified, Some(SystemTime::UNIX_EPOCH));
        assert_eq!(tree.root.children[0].links, Some(1));

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
        use std::time::SystemTime;

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

        /// The sharing signal has to survive the whole pipeline, not just
        /// `dedup` — this is the end of the wire, where Task 4 will read it.
        #[test]
        fn link_groups_reach_the_scan_tree() {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            fs::create_dir(root.join("a")).expect("mkdir");
            fs::create_dir(root.join("z")).expect("mkdir");
            let first = root.join("a/first.bin");
            let second = root.join("z/second.bin");
            write(&first, 4096);
            if fs::hard_link(&first, &second).is_err() {
                eprintln!("skipping: this filesystem has no hard links");
                return;
            }

            let tree = crate::scan(&opts(root));

            assert_eq!(tree.link_groups, vec![vec![first, second]]);
        }

        /// The common case must stay quiet: a tree with no hardlinks reports no
        /// groups at all, so nothing downstream is marked as shared.
        #[test]
        fn a_tree_without_hardlinks_has_no_link_groups() {
            let dir = fixture();

            let tree = crate::scan(&opts(dir.path()));

            assert!(tree.link_groups.is_empty(), "{:?}", tree.link_groups);
        }

        /// Do two readings of one path's mtime describe the same instant?
        ///
        /// Exact on Unix. On Windows a **directory's** timestamp is written back
        /// to its parent's listing lazily, so a value read from that listing can
        /// trail a direct `stat` of the same directory by milliseconds — the two
        /// really are the same field seen at two removes. Files are unaffected:
        /// their entry is flushed when the handle closes, which is why
        /// `walk::tests::modified_matches_the_files_mtime` can stay exact.
        fn same_instant(scanned: SystemTime, stated: SystemTime) -> bool {
            if !cfg!(windows) {
                return scanned == stated;
            }
            let drift = scanned
                .duration_since(stated)
                .or_else(|_| stated.duration_since(scanned))
                .expect("one of the two orderings holds");
            drift < std::time::Duration::from_secs(1)
        }

        /// Sizes fold upward; mtime does not. A directory judged by its
        /// children's timestamps would make the age rule call a freshly-written
        /// cache "old" whenever its parent had not been touched — the exact
        /// misfire Task 3's rule must avoid.
        #[test]
        fn modified_is_carried_per_node_not_aggregated() {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            let sub = root.join("sub");
            fs::create_dir(&sub).expect("mkdir");
            write(&sub.join("inner.bin"), 4096);

            let tree = crate::scan(&opts(root));

            let sub_node = tree
                .root
                .children
                .iter()
                .find(|c| c.path == sub)
                .expect("sub node present");
            let expected = fs::symlink_metadata(&sub)
                .expect("stat")
                .modified()
                .expect("mtime");

            let modified = sub_node
                .modified
                .expect("the platform records a directory mtime");
            assert!(
                same_instant(modified, expected),
                "the directory node must carry its own mtime, got {modified:?} against {expected:?}"
            );
            assert_eq!(
                sub_node.links, None,
                "a directory carries no link count, whatever its subdirectories say"
            );
            assert!(
                !sub_node.children.is_empty(),
                "the fixture must actually have a child, or this proves nothing"
            );
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
