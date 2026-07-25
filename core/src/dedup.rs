//! Hardlink attribution: charge each inode's bytes to exactly one path.

use crate::walk::{FileId, WalkEntry};
use std::collections::HashMap;
use std::path::PathBuf;

/// Charge every inode reached through several paths to just one of them, so
/// directory totals sum to the root total instead of double-counting hardlinks.
///
/// The keeper is the lexicographically-first path in each identity group — a
/// fixed choice, so two runs attribute identically regardless of the walk's
/// (parallel, nondeterministic) order. Every other link in the group is zeroed,
/// both `allocated` and `apparent`, so `--apparent` totals dedup too.
///
/// Entries with no identity — directories, and any entry whose platform
/// declines to give one — are left untouched: each is treated as unique.
///
/// **Returns the groups it found**, sorted, so the caller can answer a question
/// the zeroed sizes no longer can: *which* paths share content. Nothing extra is
/// computed for it — the grouping happens either way, and only groups of two or
/// more are kept, so a tree without hardlinks pays a single empty `Vec`.
pub(crate) fn attribute(entries: &mut [WalkEntry]) -> Vec<Vec<PathBuf>> {
    let mut groups: HashMap<FileId, Vec<usize>> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(id) = entry.id {
            groups.entry(id).or_default().push(index);
        }
    }

    let mut shared = Vec::new();
    for indices in groups.values() {
        if indices.len() < 2 {
            continue; // a lone link owns its bytes already
        }
        // Compare the raw path bytes, not `Path::cmp`, which orders
        // component-wise and disagrees whenever a component holds a byte below
        // `/` (0x2F) — e.g. `a-b` vs `a/c`. On Unix `OsStr::cmp` is a byte
        // compare, matching `sort(1)` and the "lexicographically first" rule.
        let mut paths: Vec<PathBuf> = indices.iter().map(|&i| entries[i].path.clone()).collect();
        paths.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));

        // The first path is therefore the keeper, by the same comparison.
        let keeper = paths[0].clone();
        for &index in indices {
            if entries[index].path != keeper {
                entries[index].allocated = 0;
                entries[index].apparent = 0;
            }
        }
        shared.push(paths);
    }

    // `HashMap` iteration order is deliberately unpredictable, so the groups
    // themselves need ordering too — the same reason the keeper is a fixed
    // choice rather than whichever link the walk reached first.
    shared.sort_by(|a, b| a[0].as_os_str().cmp(b[0].as_os_str()));
    shared
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
    fn file_entries(entries: &[WalkEntry]) -> impl Iterator<Item = &WalkEntry> {
        entries.iter().filter(|e| !e.is_dir)
    }

    /// Create a hard link, or report that this filesystem cannot.
    ///
    /// NTFS supports hard links without special privileges, but ReFS and FAT do
    /// not — and a test that quietly passes because its fixture failed to build
    /// is worse than no test.
    fn try_hard_link(original: &Path, link: &Path) -> bool {
        match fs::hard_link(original, link) {
            Ok(()) => true,
            Err(err) => {
                eprintln!("skipping: this filesystem has no hard links ({err})");
                false
            }
        }
    }

    /// Map of every entry's path to its `allocated`, for comparing whole trees.
    fn allocated_by_path(entries: &[WalkEntry]) -> BTreeMap<PathBuf, u64> {
        entries
            .iter()
            .map(|e| (e.path.clone(), e.allocated))
            .collect()
    }

    /// Runs on **both** platforms since Windows entries gained an identity —
    /// this is the end-to-end proof that dedup fires there, where the walk-level
    /// `windows_hardlinks_share_an_identity` only proves the input.
    #[test]
    fn hardlink_counted_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        fs::create_dir(root.join("b")).expect("mkdir");
        write(&root.join("a/original.bin"), 4096);
        if !try_hard_link(&root.join("a/original.bin"), &root.join("b/link.bin")) {
            return;
        }

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

    /// Also cross-platform now: the lex-first rule is what makes directory
    /// totals reproducible under a parallel walk, and it must hold wherever
    /// identity does.
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
        if !try_hard_link(&first, &second) {
            return;
        }

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

    /// The groups are the sharing signal Task 4 plans with: the zeroed sizes
    /// alone cannot say *which* paths share content, only that some do.
    ///
    /// Cross-platform, because `FileId` exists on Windows too — the one half of
    /// the sharing question that is answerable on both platforms.
    #[test]
    fn link_groups_hold_both_paths_of_a_hardlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        fs::create_dir(root.join("z")).expect("mkdir");
        let first = root.join("a/first.bin");
        let second = root.join("z/second.bin");
        write(&first, 4096);
        if !try_hard_link(&first, &second) {
            return;
        }
        // An unlinked file must not appear in any group.
        write(&root.join("lone.bin"), 4096);

        let mut walked = walk(&opts(root));
        let groups = attribute(&mut walked.entries);

        assert_eq!(groups.len(), 1, "one shared inode, one group: {groups:?}");
        assert_eq!(
            groups[0],
            vec![first.clone(), second.clone()],
            "the group holds both names, keeper first"
        );
    }

    /// Groups must be ordered as firmly as the keeper is — the walk is parallel
    /// and `HashMap` iteration is deliberately unpredictable, so two runs would
    /// otherwise disagree on the order of a plan built from them.
    #[cfg(unix)]
    #[test]
    fn link_groups_are_deterministic_across_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for name in ["a", "m", "z"] {
            fs::create_dir(root.join(name)).expect("mkdir");
            let original = root.join(name).join("original.bin");
            write(&original, 4096);
            fs::hard_link(&original, root.join(name).join("link.bin")).expect("hard_link");
        }

        let run = || {
            let mut walked = walk(&opts(root));
            attribute(&mut walked.entries)
        };

        let groups = run();
        assert_eq!(groups.len(), 3, "three independent pairs");
        assert_eq!(
            groups,
            run(),
            "group order must be byte-identical across runs"
        );

        let mut sorted = groups.clone();
        sorted.sort_by(|a, b| a[0].as_os_str().cmp(b[0].as_os_str()));
        assert_eq!(groups, sorted, "groups are ordered by their first path");
    }

    /// The common case: no hardlinks anywhere means nothing to report. A signal
    /// that fired on ordinary files would mark every candidate in Task 4 as
    /// shared and make the reclaimable total useless.
    #[test]
    fn no_hardlinks_yields_no_groups() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        write(&root.join("a/one.bin"), 4096);
        write(&root.join("two.bin"), 8192);

        let mut walked = walk(&opts(root));

        assert!(
            attribute(&mut walked.entries).is_empty(),
            "distinct files share nothing"
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
