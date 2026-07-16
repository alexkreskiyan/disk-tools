//! Hardlink attribution: charge each inode's bytes to exactly one path.

use crate::walk::{FileId, WalkEntry};
use std::collections::HashMap;

/// Charge every inode reached through several paths to just one of them, so
/// directory totals sum to the root total instead of double-counting hardlinks.
///
/// The keeper is the lexicographically-first path in each identity group — a
/// fixed choice, so two runs attribute identically regardless of the walk's
/// (parallel, nondeterministic) order. Every other link in the group is zeroed,
/// both `allocated` and `apparent`, so `--apparent` totals dedup too.
///
/// Entries with no identity — directories, and every file on Windows — are left
/// untouched: each is treated as unique, so hardlinks double-count on Windows,
/// the documented v0.1 behaviour (§8.2.3).
pub(crate) fn attribute(entries: &mut [WalkEntry]) {
    let mut groups: HashMap<FileId, Vec<usize>> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(id) = entry.id {
            groups.entry(id).or_default().push(index);
        }
    }

    for indices in groups.values() {
        if indices.len() < 2 {
            continue; // a lone link owns its bytes already
        }
        // Compare the raw path bytes, not `Path::cmp`, which orders
        // component-wise and disagrees whenever a component holds a byte below
        // `/` (0x2F) — e.g. `a-b` vs `a/c`. On Unix `OsStr::cmp` is a byte
        // compare, matching `sort(1)` and the "lexicographically first" rule.
        // (Dedup only ever fires on Unix; elsewhere every `id` is `None`.)
        let keeper = *indices
            .iter()
            .min_by(|&&a, &&b| entries[a].path.as_os_str().cmp(entries[b].path.as_os_str()))
            .expect("group is non-empty");
        for &index in indices {
            if index != keeper {
                entries[index].allocated = 0;
                entries[index].apparent = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScanOptions;
    use crate::walk::walk;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn opts(root: &Path) -> ScanOptions {
        ScanOptions {
            root: root.to_path_buf(),
            ..ScanOptions::default()
        }
    }

    /// Content big enough that `allocated` is a nonzero block on any filesystem,
    /// so "counted once" is a claim about real bytes, not about zero.
    fn write(path: &Path, bytes: usize) {
        fs::write(path, vec![b'x'; bytes]).expect("write file");
    }

    // Only the hardlink fixtures (all `#[cfg(unix)]`) inspect file entries.
    #[cfg(unix)]
    fn file_entries(entries: &[WalkEntry]) -> impl Iterator<Item = &WalkEntry> {
        entries.iter().filter(|e| !e.is_dir)
    }

    /// Map of every entry's path to its `allocated`, for comparing whole trees.
    fn allocated_by_path(entries: &[WalkEntry]) -> BTreeMap<PathBuf, u64> {
        entries
            .iter()
            .map(|e| (e.path.clone(), e.allocated))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_counted_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        fs::create_dir(root.join("b")).expect("mkdir");
        write(&root.join("a/original.bin"), 4096);
        fs::hard_link(root.join("a/original.bin"), root.join("b/link.bin")).expect("hard_link");

        let mut walked = walk(&opts(root));
        // Both links measured the same bytes before attribution.
        let one_link = file_entries(&walked.entries)
            .map(|e| e.allocated)
            .max()
            .expect("at least one file");
        let before: u64 = file_entries(&walked.entries).map(|e| e.allocated).sum();
        assert_eq!(before, 2 * one_link, "walk sees the shared bytes twice");

        attribute(&mut walked.entries);

        let after: u64 = file_entries(&walked.entries).map(|e| e.allocated).sum();
        assert_eq!(
            after, one_link,
            "an inode's bytes must be counted exactly once after attribution"
        );

        // Exactly one of the two links keeps the bytes; its twin is zeroed.
        let nonzero = file_entries(&walked.entries)
            .filter(|e| e.allocated > 0)
            .count();
        assert_eq!(nonzero, 1, "one link keeps the bytes, the other is zeroed");
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_attributed_to_lexicographically_first_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        // "a/first" sorts before "z/second", whichever order the walk visits them.
        fs::create_dir(root.join("a")).expect("mkdir");
        fs::create_dir(root.join("z")).expect("mkdir");
        let first = root.join("a/first.bin");
        let second = root.join("z/second.bin");
        write(&first, 4096);
        fs::hard_link(&first, &second).expect("hard_link");

        let mut walked = walk(&opts(root));
        attribute(&mut walked.entries);

        let entry_of = |path: &Path| {
            walked
                .entries
                .iter()
                .find(|e| e.path == path)
                .unwrap_or_else(|| panic!("entry for {path:?}"))
        };
        let kept = entry_of(&first);
        let zeroed = entry_of(&second);

        assert!(
            kept.allocated > 0 && kept.apparent > 0,
            "the lexicographically-first path keeps the bytes"
        );
        assert_eq!(zeroed.allocated, 0, "the later path is zeroed (allocated)");
        assert_eq!(zeroed.apparent, 0, "the later path is zeroed (apparent)");
    }

    /// Two links whose paths diverge at a `-` (0x2D) vs `/` (0x2F) boundary:
    /// raw byte order and `Path::cmp`'s component order disagree here. The keeper
    /// must be the byte-lexicographically-first (`a-b.bin`), matching `sort(1)`
    /// and the documented rule — not `a/c.bin`, which component-wise `Path::cmp`
    /// would wrongly pick.
    #[cfg(unix)]
    #[test]
    fn keeper_uses_byte_order_not_component_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        let shallow = root.join("a-b.bin");
        let deep = root.join("a/c.bin");
        write(&shallow, 4096);
        fs::hard_link(&shallow, &deep).expect("hard_link");

        let mut walked = walk(&opts(root));
        attribute(&mut walked.entries);

        let entry_of = |path: &Path| {
            walked
                .entries
                .iter()
                .find(|e| e.path == path)
                .unwrap_or_else(|| panic!("entry for {path:?}"))
        };
        assert!(
            entry_of(&shallow).allocated > 0,
            "`a-b.bin` is byte-lexicographically first and must keep the bytes"
        );
        assert_eq!(
            entry_of(&deep).allocated,
            0,
            "`a/c.bin` sorts later under raw byte order and must be zeroed"
        );
    }

    /// The attribution must not depend on the walk's parallel, nondeterministic
    /// order — two runs of the same tree must produce identical per-path sizes.
    #[test]
    fn dir_totals_deterministic_across_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        fs::create_dir(root.join("b")).expect("mkdir");
        write(&root.join("a/one.bin"), 4096);
        write(&root.join("a/two.bin"), 8192);
        write(&root.join("b/three.bin"), 2048);
        #[cfg(unix)]
        fs::hard_link(root.join("a/one.bin"), root.join("b/one-link.bin")).expect("hard_link");

        let run = || {
            let mut walked = walk(&opts(root));
            attribute(&mut walked.entries);
            allocated_by_path(&walked.entries)
        };

        assert_eq!(
            run(),
            run(),
            "attribution must be byte-identical across runs"
        );
    }

    /// A tree of distinct files has no group of size >1, so attribution must
    /// leave every byte where it was — guards against over-zealous zeroing.
    #[test]
    fn unlinked_files_keep_their_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        write(&root.join("a/one.bin"), 4096);
        write(&root.join("two.bin"), 8192);

        let mut walked = walk(&opts(root));
        let before = allocated_by_path(&walked.entries);

        attribute(&mut walked.entries);

        assert_eq!(
            before,
            allocated_by_path(&walked.entries),
            "with no hardlinks, attribution changes nothing"
        );
    }

    /// A group of size >2 must reduce to exactly one survivor, not just "fewer
    /// than before" — guards an off-by-one that a two-link test can't catch
    /// (e.g. a bug that zeroes every index but the *last* one visited, which
    /// would coincidentally pass a two-link test half the time).
    #[cfg(unix)]
    #[test]
    fn three_way_hardlink_all_but_one_zeroed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        fs::create_dir(root.join("m")).expect("mkdir");
        fs::create_dir(root.join("z")).expect("mkdir");
        let first = root.join("a/first.bin");
        write(&first, 4096);
        fs::hard_link(&first, root.join("m/middle.bin")).expect("hard_link");
        fs::hard_link(&first, root.join("z/last.bin")).expect("hard_link");

        let mut walked = walk(&opts(root));
        let one_link = file_entries(&walked.entries)
            .map(|e| e.allocated)
            .max()
            .expect("at least one file");

        attribute(&mut walked.entries);

        let total: u64 = file_entries(&walked.entries).map(|e| e.allocated).sum();
        assert_eq!(total, one_link, "three links to one inode count once");

        let nonzero: Vec<&WalkEntry> = file_entries(&walked.entries)
            .filter(|e| e.allocated > 0)
            .collect();
        assert_eq!(
            nonzero.len(),
            1,
            "exactly one of three links keeps the bytes, got {:?}",
            nonzero.iter().map(|e| &e.path).collect::<Vec<_>>()
        );
        assert_eq!(
            nonzero[0].path, first,
            "the lexicographically-first of the three paths is the survivor"
        );
    }

    /// A sparse file makes `allocated` and `apparent` genuinely different
    /// numbers, so this catches a bug that zeroes only one field (e.g. a typo
    /// that zeroes `allocated` twice instead of both fields) — a same-valued
    /// fixture like `write(.., 4096)` can't tell that apart from correct code.
    #[cfg(unix)]
    #[test]
    fn sparse_hardlink_zeroes_both_fields_when_they_differ() {
        const LOGICAL: u64 = 4 * 1024 * 1024;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        fs::create_dir(root.join("z")).expect("mkdir");
        let first = root.join("a/first.bin");
        write(&first, 1); // one real byte, so allocated stays nonzero pre-dedup
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&first)
            .expect("reopen for truncate");
        file.set_len(LOGICAL).expect("set_len");
        file.sync_all().expect("sync");
        let second = root.join("z/second.bin");
        fs::hard_link(&first, &second).expect("hard_link");

        let mut walked = walk(&opts(root));
        let kept_before = walked
            .entries
            .iter()
            .find(|e| e.path == first)
            .expect("entry for first");
        assert!(
            kept_before.allocated < kept_before.apparent,
            "fixture must be genuinely sparse: allocated={} apparent={}",
            kept_before.allocated,
            kept_before.apparent
        );
        assert_eq!(kept_before.apparent, LOGICAL);

        attribute(&mut walked.entries);

        let entry_of = |path: &Path| {
            walked
                .entries
                .iter()
                .find(|e| e.path == path)
                .unwrap_or_else(|| panic!("entry for {path:?}"))
        };
        let kept = entry_of(&first);
        let zeroed = entry_of(&second);
        assert!(kept.allocated > 0 && kept.apparent > 0);
        assert_eq!(
            zeroed.allocated, 0,
            "sparse link's allocated must be zeroed"
        );
        assert_eq!(zeroed.apparent, 0, "sparse link's apparent must be zeroed");
    }

    /// Two unrelated hardlink pairs in the same tree must be attributed
    /// independently — guards against a bug that merges groups by iteration
    /// order rather than keying strictly on identity.
    #[cfg(unix)]
    #[test]
    fn two_independent_hardlink_groups_do_not_interfere() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        fs::create_dir(root.join("b")).expect("mkdir");
        let group1_first = root.join("a/g1-first.bin");
        let group1_second = root.join("b/g1-second.bin");
        write(&group1_first, 4096);
        fs::hard_link(&group1_first, &group1_second).expect("hard_link");

        let group2_first = root.join("a/g2-first.bin");
        let group2_second = root.join("b/g2-second.bin");
        write(&group2_first, 8192);
        fs::hard_link(&group2_first, &group2_second).expect("hard_link");

        let mut walked = walk(&opts(root));
        attribute(&mut walked.entries);

        let entry_of = |path: &Path| {
            walked
                .entries
                .iter()
                .find(|e| e.path == path)
                .unwrap_or_else(|| panic!("entry for {path:?}"))
        };
        assert!(entry_of(&group1_first).allocated > 0, "group 1 keeper kept");
        assert_eq!(entry_of(&group1_second).allocated, 0, "group 1 twin zeroed");
        assert!(entry_of(&group2_first).allocated > 0, "group 2 keeper kept");
        assert_eq!(entry_of(&group2_second).allocated, 0, "group 2 twin zeroed");

        // Each group's survivor must keep its own bytes, not the other group's.
        assert_ne!(
            entry_of(&group1_first).allocated,
            entry_of(&group2_first).allocated,
            "the two groups have distinct sizes, so their survivors must too"
        );
    }

    /// The lexicographic pick must not depend on where in the slice each link
    /// happens to sit — the walk's parallelism means array order is not
    /// guaranteed to agree with path order. Forces the later path to sit
    /// first in the slice and checks the earlier path still wins, rather than
    /// relying on the walk's own (incidental) order to exercise this.
    #[cfg(unix)]
    #[test]
    fn keeper_selection_is_independent_of_entry_array_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        fs::create_dir(root.join("z")).expect("mkdir");
        let first = root.join("a/first.bin");
        let second = root.join("z/second.bin");
        write(&first, 4096);
        fs::hard_link(&first, &second).expect("hard_link");

        let mut walked = walk(&opts(root));
        // Force descending path order, so the lexicographically-later path
        // sits before the earlier one in the slice regardless of walk order.
        walked.entries.sort_by(|a, b| b.path.cmp(&a.path));
        assert!(
            walked
                .entries
                .iter()
                .position(|e| e.path == second)
                .expect("second present")
                < walked
                    .entries
                    .iter()
                    .position(|e| e.path == first)
                    .expect("first present"),
            "fixture must place the later path earlier in the slice"
        );

        attribute(&mut walked.entries);

        let entry_of = |path: &Path| {
            walked
                .entries
                .iter()
                .find(|e| e.path == path)
                .unwrap_or_else(|| panic!("entry for {path:?}"))
        };
        assert!(
            entry_of(&first).allocated > 0,
            "the lexicographically-first path must win regardless of slice order"
        );
        assert_eq!(entry_of(&second).allocated, 0);
    }

    /// Directories carry no identity, so they must never be swept into a group
    /// and zeroed — even when a real hardlink pair is being deduped nearby.
    #[cfg(unix)]
    #[test]
    fn directories_are_left_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        fs::create_dir(root.join("b")).expect("mkdir");
        write(&root.join("a/original.bin"), 4096);
        fs::hard_link(root.join("a/original.bin"), root.join("b/link.bin")).expect("hard_link");

        let mut walked = walk(&opts(root));
        let dirs_before: BTreeMap<PathBuf, u64> = walked
            .entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| (e.path.clone(), e.allocated))
            .collect();

        attribute(&mut walked.entries);

        let dirs_after: BTreeMap<PathBuf, u64> = walked
            .entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| (e.path.clone(), e.allocated))
            .collect();
        assert_eq!(
            dirs_before, dirs_after,
            "directories must be untouched by hardlink attribution"
        );

        // Prove attribution actually ran, so the check above isn't vacuous.
        let zeroed = walked
            .entries
            .iter()
            .filter(|e| !e.is_dir && e.allocated == 0)
            .count();
        assert_eq!(zeroed, 1, "the hardlink twin should have been zeroed");
    }
}
