//! `disk-tools-core` — the scanning engine behind the `disk-tools` CLI.
//!
//! The library never logs or prints. Anything that goes wrong during a scan
//! (an unreadable directory, a file that vanishes mid-walk) comes back as data
//! in [`ScanTree::skipped`]; deciding how to show it is the frontend's job.
//!
//! The core also knows nothing about config files, config paths or terminals —
//! it is driven entirely by an explicit [`ScanOptions`].

// `deny` rather than `forbid`: Windows exposes neither a file's allocated size
// nor its identity through anything safe, so both must come through FFI.
// `forbid` cannot be lifted by an `#[allow]` — that is the whole difference
// between the two — so the crate denies `unsafe` and the functions that need it
// opt out one at a time, never a whole module.
//
// The exemptions, all `#[cfg(windows)]`:
//   size::allocated              GetCompressedFileSizeW  — per-file fallback
//   windows_dir::read_facts      GetFileInformationByHandleEx
//   windows_dir::collect_batch   walking that call's output buffer
//   windows_dir::volume_serial   GetFileInformationByHandle
//
// Everywhere else, including all of Unix, `unsafe` still fails the build. Any
// further exemption deserves the same scrutiny these got — the buffer walk in
// particular is the only one doing pointer arithmetic rather than a single call,
// and is bounds-checked against the capacity we passed in rather than trusting
// the OS to stay inside it.
#![deny(unsafe_code)]

mod clean;
mod dedup;
mod detect;
mod git;
mod measure;
mod options;
mod paths;
mod rules;
mod size;
#[cfg(feature = "trash")]
mod trash;
mod tree;
mod walk;
#[cfg(windows)]
mod windows_dir;

pub use clean::{Candidate, CleanOptions, CleanPlan, ExcludeReason, Excluded, plan};
pub use detect::{DetectOptions, Detection, detect};
pub use measure::{Claim, Finished, Measured, measure};
pub use options::ScanOptions;
pub use rules::{
    AnySibling, Facts, NameTest, Rule, RuleError, Rules, State, Tier, UserDirs, age_rule,
    builtin_rules,
};
pub use size::allocated_size;
#[cfg(feature = "trash")]
pub use trash::{CleanOutcome, Removal, TrashFailure, apply, move_to_trash};
pub use tree::{ScanNode, ScanTree, SkipReason, SkippedEntry};

/// Scan `options.root` and return a size-annotated tree plus whatever was
/// skipped.
///
/// The one public entry point. Runs the three internal phases in order — walk
/// the tree in parallel, resolve hardlink attribution, then aggregate bottom-up
/// — because attribution must settle before any directory total is summed.
/// Never fails: an unreadable root (or anything else that goes wrong) comes back
/// as a [`SkippedEntry`] in [`ScanTree::skipped`], not an error.
pub fn scan(options: &ScanOptions) -> ScanTree {
    let mut walked = walk::walk(options);
    // Attribution both zeroes the duplicate links and reports which paths shared
    // them — the second half is the only record of that sharing left once the
    // sizes are folded away.
    let link_groups = dedup::attribute(&mut walked.entries);
    let root = tree::aggregate(walked.entries, options.root.as_path());
    ScanTree {
        root,
        skipped: walked.skipped,
        link_groups,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant, SystemTime};

    /// Diagnostic, not an assertion: prints where `scan` spends its time.
    ///
    /// The three phases are private, so nothing outside this crate can time
    /// them separately — hence a test rather than an example or a bench.
    /// Ignored by default because it needs a real tree and asserts nothing.
    ///
    /// ```text
    /// just bench-phases ~/.cargo
    /// RAYON_NUM_THREADS=1 just bench-phases ~/.cargo   # what actually scales
    /// ```
    #[test]
    #[ignore = "diagnostic: needs DT_PHASE_PATH, prints timings, asserts nothing"]
    fn phase_split() {
        let root = std::env::var("DT_PHASE_PATH")
            .expect("set DT_PHASE_PATH to the tree to scan, e.g. DT_PHASE_PATH=~/.cargo");
        let options = ScanOptions {
            root: root.clone().into(),
            ..ScanOptions::default()
        };

        let start = Instant::now();
        let mut walked = walk::walk(&options);
        let walk = start.elapsed();
        let entries = walked.entries.len();

        let start = Instant::now();
        let link_groups = dedup::attribute(&mut walked.entries);
        let dedup = start.elapsed();

        let start = Instant::now();
        let root_node = tree::aggregate(walked.entries, options.root.as_path());
        let aggregate = start.elapsed();

        let skipped = walked.skipped.len();
        let groups = link_groups.len();
        let allocated = root_node.allocated;
        let tree = ScanTree {
            root: root_node,
            skipped: walked.skipped,
            link_groups,
        };

        // The detection pass, which the three phases above never included. It is
        // timed with the defaults a first `clean` run gets — every built-in rule
        // on, the age rule off — and with a real home, so the rooted cache rules
        // are exercised rather than dropped.
        let dirs = UserDirs {
            home: std::env::var_os("HOME").map(Into::into),
            ..UserDirs::default()
        };
        let detect_options = DetectOptions {
            rules: Rules::builtin(&dirs),
            now: SystemTime::now(),
        };
        let start = Instant::now();
        let found = detect::detect(&tree, &detect_options);
        let detect = start.elapsed();

        // The upper bound, and the number a rule engine must be measured
        // against. Above, a claimed `node_modules` is never descended into, so
        // the pass skips whole subtrees and its time is not per-node over the
        // whole tree. Here one rule matches everything the walk can reach and
        // claims none of it — an impossible `older_than` — so the DFS visits
        // every node and pays a full glob match on each.
        let none = DetectOptions {
            rules: Rules::new(vec![age_rule(Duration::from_secs(1))], &dirs).expect("compile"),
            now: SystemTime::UNIX_EPOCH,
        };
        let start = Instant::now();
        let full = detect::detect(&tree, &none);
        let detect_full = start.elapsed();
        assert!(full.is_empty(), "the upper-bound pass must claim nothing");

        let total = walk + dedup + aggregate + detect;
        let share = |d: std::time::Duration| 100.0 * d.as_secs_f64() / total.as_secs_f64();

        println!("\n{root} — {entries} entries, {skipped} skipped, {groups} hardlink groups");
        println!("  walk       {:>9.1?}  {:>5.1}%", walk, share(walk));
        println!("  dedup      {:>9.1?}  {:>5.1}%", dedup, share(dedup));
        println!(
            "  aggregate  {:>9.1?}  {:>5.1}%",
            aggregate,
            share(aggregate)
        );
        println!(
            "  detect     {:>9.1?}  {:>5.1}%   {} candidates",
            detect,
            share(detect),
            found.len()
        );
        println!(
            "  detect*    {:>9.1?}          every node visited, nothing claimed",
            detect_full
        );
        println!("  total      {total:>9.1?}");
        println!(
            "  threads    {}\n  root total {allocated} bytes",
            rayon::current_num_threads(),
        );
    }

    /// Diagnostic: does stat-ing the entries of **one** directory scale?
    ///
    /// The walk parallelises across subdirectories but not within one, so a wide
    /// flat directory runs single-threaded. Before changing that, this asks
    /// whether the kernel would even allow a speed-up, or whether the metadata
    /// path serialises anyway — in which case parallelising the loop buys
    /// nothing and costs contention.
    ///
    /// Passes alternate (seq, par, seq, par, …) so cache warming cannot bias
    /// whichever runs second, and the best of each is reported.
    ///
    /// ```text
    /// just bench-stat /tmp/flat-dir
    /// ```
    #[test]
    #[ignore = "diagnostic: needs DT_PHASE_PATH, prints timings, asserts nothing"]
    fn dir_stat_scaling() {
        use rayon::prelude::*;

        let root = std::env::var("DT_PHASE_PATH")
            .expect("set DT_PHASE_PATH to a directory holding many files");
        let paths: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();

        // Summing the lengths keeps the optimiser from eliding the stat calls.
        let sequential = || {
            let start = Instant::now();
            let total: u64 = paths
                .iter()
                .filter_map(|p| std::fs::symlink_metadata(p).ok())
                .map(|m| m.len())
                .sum();
            (start.elapsed(), total)
        };
        let parallel = || {
            let start = Instant::now();
            let total: u64 = paths
                .par_iter()
                .filter_map(|p| std::fs::symlink_metadata(p).ok())
                .map(|m| m.len())
                .sum();
            (start.elapsed(), total)
        };

        let mut best_seq = std::time::Duration::MAX;
        let mut best_par = std::time::Duration::MAX;
        for _ in 0..5 {
            let (seq, a) = sequential();
            let (par, b) = parallel();
            assert_eq!(a, b, "both passes must see the same bytes");
            best_seq = best_seq.min(seq);
            best_par = best_par.min(par);
        }

        println!("\n{root} — {} entries in one directory", paths.len());
        println!("  sequential {best_seq:>9.1?}");
        println!("  parallel   {best_par:>9.1?}");
        println!(
            "  speed-up   {:.2}x on {} threads",
            best_seq.as_secs_f64() / best_par.as_secs_f64(),
            rayon::current_num_threads()
        );
    }
}
