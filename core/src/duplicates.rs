//! Files whose **contents** are identical.
//!
//! Every other pass in this crate answers a question about a path: is this a
//! `target/`, is it old, how big is it. A duplicate is not a property of a file
//! at all — it is a property of a *set*, and the answer is never "remove these"
//! but "keep exactly one of these". That is why this module returns groups with
//! a keeper rather than a list of candidates.
//!
//! The pipeline is staged so the expensive stage runs on almost nothing:
//!
//! | Stage | Cost |
//! |---|---|
//! | eligible files, bucketed by apparent size | none — the scan already has it |
//! | one `symlink_metadata` per survivor | one stat per file in a same-size bucket |
//! | xxh3-128 of the first 16 KiB | one short read |
//! | blake3 over the whole file | the only stage bounded by disk throughput |
//!
//! **Hardlinks cost nothing to collapse.** `dedup::attribute` has
//! already zeroed every name but one of each inode, and a zeroed entry is not
//! eligible — which is exactly right, since removing one name of a hardlinked
//! file frees no bytes at all. The concept's "hardlink-collapse" stage is a
//! stage this module never had to write.
//!
//! Like the rest of the core it reads no clock and no environment, prints
//! nothing, and returns its failures as data.

use crate::detect::{DetectOptions, detect};
use crate::dup_rules::DuplicateRules;
use crate::rules::{Facts, Tier};
use crate::tree::{ScanNode, ScanTree, SkipReason, SkippedEntry};
use crate::walk::skip_reason;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// How much of a file the cheap hash looks at.
///
/// Two files that differ at all usually differ early — a header, a magic
/// number, a first frame. 16 KiB is enough to separate almost every real
/// same-size bucket while reading one block per file.
const PREFIX: u64 = 16 * 1024;

/// Read buffer for the full hash. Fixed, so memory never scales with a file:
/// a 40 GiB disk image must not become a 40 GiB allocation.
const CHUNK: usize = 128 * 1024;

/// Which copy survives when several are identical.
///
/// The default is [`Keep::OldestCreated`]: the original is the one that existed
/// first, and copying makes a new inode with a new creation time. Modification
/// time cannot say that on its own — `cp -p`, `rsync` and unpacking an archive
/// all carry the original's mtime onto the copy, which leaves a group whose
/// members all claim the same age.
///
/// Every date-based rule **degrades rather than misleads**: a member whose date
/// the platform did not record never wins, and a group where *no* member has the
/// requested date falls back to the other date and then to
/// [`Keep::First`] — recorded in [`DuplicateGroup::keeper_fell_back`], so the
/// report can say so instead of quietly answering a different question. Linux
/// without `statx` birth times is where this happens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum Keep {
    /// The byte-lexicographically first path. The only rule that needs no
    /// metadata and so can never degrade.
    First,
    /// The earliest creation time — the original.
    #[default]
    OldestCreated,
    /// The latest creation time — the most recently made copy.
    NewestCreated,
    /// The earliest modification time.
    OldestModified,
    /// The latest modification time.
    NewestModified,
}

impl Keep {
    /// Which date this rule reads, if any.
    fn date(self) -> Option<Date> {
        match self {
            Keep::First => None,
            Keep::OldestCreated | Keep::NewestCreated => Some(Date::Created),
            Keep::OldestModified | Keep::NewestModified => Some(Date::Modified),
        }
    }

    /// Does it want the earliest or the latest?
    fn wants_earliest(self) -> bool {
        matches!(
            self,
            Keep::First | Keep::OldestCreated | Keep::OldestModified
        )
    }
}

/// Which timestamp a keeper rule reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Date {
    Created,
    Modified,
}

impl Date {
    /// The one the rule falls back to when this one is unrecorded.
    fn other(self) -> Date {
        match self {
            Date::Created => Date::Modified,
            Date::Modified => Date::Created,
        }
    }
}

/// Everything the pass needs, all of it explicit.
#[derive(Debug, Clone, Default)]
pub struct DuplicateOptions {
    /// Which rules claim subtrees this pass must not look **inside**.
    ///
    /// The rules never produce a candidate here — they only prune. A
    /// `node_modules` is a duplicate farm, and removing one file out of one is
    /// how a tree that should have gone wholesale gets broken instead.
    pub detect: DetectOptions,

    /// Where duplicates may be looked for, and what to do with the copies.
    ///
    /// Everything a rule's parts match is **one pool**, and groups form only
    /// within a pool — so a rule's keeper policy and tier are never ambiguous.
    /// Membership is exclusive: the first rule that matches a file takes it.
    pub rules: DuplicateRules,

    /// Apparent bytes. A file smaller than this is not considered, whatever its
    /// content; a zero-length one never is, at any setting. Applied **beside**
    /// each part's own floor, the larger of the two deciding.
    pub min_size: u64,

    /// `--keep`, when it was passed: it beats what every rule says.
    pub keep: Option<Keep>,

    /// `--keep-in`, when it was passed: it replaces every rule's own list.
    ///
    /// A group with a member under one of these keeps that member, whatever
    /// `keep` says — "keep whatever is in ~/Photos, remove the copies
    /// elsewhere" is the thing users actually want. `keep` then only breaks
    /// ties inside the winning root.
    pub keep_in: Option<Vec<PathBuf>>,
}

/// One redundant copy: what would go, and what that frees.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Copy {
    pub path: PathBuf,
    pub allocated: u64,
}

/// A set of files with identical contents, and the one that stays.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DuplicateGroup {
    /// One copy's logical length — the identity this group was bucketed on.
    pub apparent: u64,

    /// The duplicate rule whose pool this group formed in — the name the report
    /// carries, and the answer to "why were these compared with each other".
    pub rule: String,

    /// What `clean` does with the copies, from that same rule.
    pub tier: Tier,

    /// The keeper rule that actually decided, after the flags were applied over
    /// the pool's own. Carried per group because two pools may answer
    /// differently, and the report says what the choice was made on.
    pub keep: Keep,

    pub keeper: PathBuf,

    /// The date the keeper rule read on the winner, where it read one.
    ///
    /// `None` for [`Keep::First`], and for a group in which nothing carried
    /// either date. Kept so the report can show what the choice was made on: a
    /// rule whose basis is invisible is one the user can only trust.
    #[cfg_attr(feature = "serde", serde(with = "crate::tree::unix_seconds"))]
    pub keeper_date: Option<SystemTime>,

    /// The keeper rule asked for could not be applied here, and a weaker one
    /// chose instead.
    ///
    /// Only ever true for a date-based [`Keep`] on a group where **no** member
    /// carried that date — a Linux filesystem with no birth times, most often.
    /// Carried per group rather than counted once for the run, because that is
    /// the granularity a report can act on: this row is the one whose keeper was
    /// not chosen the way you asked.
    pub keeper_fell_back: bool,

    /// The redundant copies, ordered by path bytes. Never empty.
    pub copies: Vec<Copy>,

    /// The sum of `copies`' allocated bytes.
    ///
    /// A sum rather than `apparent * copies.len()`: two byte-identical files
    /// can occupy different amounts of disk — different volumes, different
    /// block sizes, one of them sparse or compressed.
    pub reclaimable: u64,
}

/// One pool, and how much of the tree fell into it.
///
/// Reported whether or not it produced a group, because "two areas, 4,120 files,
/// no group" and "one area, three files" are different problems and an empty
/// report cannot be read without knowing which one it is. That an area was never
/// searched is a **configuration** answer, and configuration is invisible unless
/// something says it out loud.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Searched {
    pub rule: String,
    /// Files that reached the comparison — after the rules, the floors and the
    /// hardlink collapse, before a single byte was read.
    pub files: usize,
}

/// What the pass found, and what it could not read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Duplicates {
    /// Ordered by `reclaimable` descending, then by keeper path bytes.
    pub groups: Vec<DuplicateGroup>,

    /// Anything unreadable, vanished, or changed under us between the scan and
    /// the hash — as data, never printed.
    pub skipped: Vec<SkippedEntry>,

    /// Every pool that was searched, in rule order, whether or not it produced
    /// anything.
    pub pools: Vec<Searched>,

    pub files_hashed: usize,
    pub bytes_read: u64,
}

/// One file finished hashing.
///
/// Mirrors [`crate::Finished`]: the core reports progress and the frontend
/// decides whether a human ever sees it.
#[derive(Debug, Clone, Copy)]
pub struct Hashed<'a> {
    pub path: &'a Path,
    /// Bytes read for this file, at this stage.
    pub bytes: u64,
    /// Bytes read by the whole pass so far, this file included.
    pub running_total: u64,
}

/// One file on its way through the pipeline.
///
/// Both dates are read from the `symlink_metadata` the pass already makes to
/// confirm the file is still there — so the keeper rules cost no extra call,
/// and nothing above this module has to start carrying a creation time it would
/// otherwise never use.
#[derive(Debug, Clone)]
struct Member {
    path: PathBuf,
    /// Which pool it belongs to. Groups form within one, so this is half the
    /// bucket key — two identical files in different pools are not duplicates
    /// of each other, and saying so is the whole point of the rules.
    pool: usize,
    allocated: u64,
    apparent: u64,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
}

impl Member {
    fn date(&self, which: Date) -> Option<SystemTime> {
        match which {
            Date::Created => self.created,
            Date::Modified => self.modified,
        }
    }
}

/// Everything the parallel stages share.
struct Progress<'a> {
    skipped: Mutex<Vec<SkippedEntry>>,
    files_hashed: AtomicU64,
    bytes_read: AtomicU64,
    report: &'a (dyn Fn(Hashed<'_>) + Sync),
}

impl Progress<'_> {
    fn skip(&self, path: &Path, reason: SkipReason) {
        self.skipped.lock().expect("skip list").push(SkippedEntry {
            path: path.to_path_buf(),
            reason,
        });
    }

    fn hashed(&self, path: &Path, bytes: u64) {
        self.files_hashed.fetch_add(1, Ordering::Relaxed);
        let running_total = self.bytes_read.fetch_add(bytes, Ordering::Relaxed) + bytes;
        (self.report)(Hashed {
            path,
            bytes,
            running_total,
        });
    }
}

/// Find every set of files with identical contents under `tree`.
///
/// Reads file contents — the only thing in this crate that does — but writes
/// nothing and never fails: a file that vanishes, refuses to open or changes
/// size mid-pass leaves a [`SkippedEntry`] and drops out of its group. A group
/// left with fewer than two members disappears with it, so **no copy is ever
/// proposed for removal against content that could not be read**.
pub fn duplicates(
    tree: &ScanTree,
    options: &DuplicateOptions,
    report: &(dyn Fn(Hashed<'_>) + Sync),
) -> Duplicates {
    let claimed: HashSet<PathBuf> = detect(tree, &options.detect)
        .into_iter()
        .map(|found| found.path)
        .collect();

    let mut eligible = Vec::new();
    collect(&tree.root, &[], &claimed, options, &mut eligible);

    // Bucketing costs nothing — the scan measured every one of these — and it is
    // what makes the whole pass affordable: a unique size is proof of a unique
    // file, and most files have one. The **pool** is part of the key: two
    // identical files the rules put in different pools are not duplicates of
    // each other, which is the answer the rules exist to give.
    let mut buckets: HashMap<(usize, u64), Vec<Member>> = HashMap::new();
    let mut population: HashMap<usize, usize> = HashMap::new();
    for member in eligible {
        *population.entry(member.pool).or_default() += 1;
        buckets
            .entry((member.pool, member.apparent))
            .or_default()
            .push(member);
    }
    let contested: Vec<((usize, u64), Vec<Member>)> = buckets
        .into_iter()
        .filter(|(_, members)| members.len() > 1)
        .collect();

    // Counted before the hashing narrows anything, so the figure answers "how
    // much was even looked at here" rather than "how much survived".
    let mut pools: Vec<Searched> = Vec::new();
    for (index, files) in &population {
        if let Some(pool) = options.rules.at(*index) {
            pools.push(Searched {
                rule: pool.name.to_owned(),
                files: *files,
            });
        }
    }
    pools.sort_by(|a, b| a.rule.cmp(&b.rule));

    let progress = Progress {
        skipped: Mutex::new(Vec::new()),
        files_hashed: AtomicU64::new(0),
        bytes_read: AtomicU64::new(0),
        report,
    };

    let mut groups: Vec<DuplicateGroup> = contested
        .into_par_iter()
        .flat_map_iter(|((pool, size), members)| resolve(pool, size, members, options, &progress))
        .collect();

    // The buckets came out of a `HashMap` and the work ran in parallel, so
    // nothing above is ordered. Everything a caller sees is ordered here.
    groups.sort_by(|a, b| {
        b.reclaimable
            .cmp(&a.reclaimable)
            .then_with(|| a.keeper.as_os_str().cmp(b.keeper.as_os_str()))
    });
    let mut skipped = progress.skipped.into_inner().expect("skip list");
    skipped.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));

    Duplicates {
        groups,
        pools,
        skipped,
        files_hashed: progress.files_hashed.load(Ordering::Relaxed) as usize,
        bytes_read: progress.bytes_read.load(Ordering::Relaxed),
    }
}

/// Every file that could still be a duplicate, and the pool it is in.
///
/// Four ways not to be here, and each says something different:
///
/// - a **clean** rule claims the subtree — it goes whole, and removing one file
///   out of it is how the rest gets broken;
/// - no **duplicate** rule's parts match the file, so nothing asked for it;
/// - it is below the floor, the part's or the flag's, whichever is larger;
/// - its bytes were zeroed by hardlink attribution, which is the collapse:
///   removing one name of an inode frees nothing.
fn collect(
    node: &ScanNode,
    siblings: &[ScanNode],
    claimed: &HashSet<PathBuf>,
    options: &DuplicateOptions,
    out: &mut Vec<Member>,
) {
    if claimed.contains(&node.path) {
        return;
    }
    if node.is_dir {
        if options.rules.prunes(&node.path) {
            return;
        }
        for child in &node.children {
            collect(child, &node.children, claimed, options, out);
        }
        return;
    }
    if node.apparent == 0 {
        return;
    }

    let facts = Facts {
        is_dir: false,
        modified: node.modified,
        now: options.detect.now,
        any_sibling: &|wanted| {
            siblings
                .iter()
                .any(|sibling| sibling.path.file_name().is_some_and(wanted))
        },
    };
    let Some(pool) = options.rules.pool(&node.path, &facts) else {
        return;
    };
    // Both floors apply and the larger wins, as with the clean rules: a flag
    // narrows what a file said, it does not widen it.
    if node.apparent < pool.min_size.max(options.min_size) {
        return;
    }

    out.push(Member {
        path: node.path.clone(),
        pool: pool.index,
        allocated: node.allocated,
        apparent: node.apparent,
        // Both filled by `still_there`, from the stat it makes anyway.
        created: None,
        modified: None,
    });
}

/// Take one same-size bucket down to the groups that are genuinely identical.
fn resolve(
    pool: usize,
    size: u64,
    members: Vec<Member>,
    options: &DuplicateOptions,
    progress: &Progress<'_>,
) -> Vec<DuplicateGroup> {
    let members = still_there(members, size, progress);
    if members.len() < 2 {
        return Vec::new();
    }

    // Below the prefix length the cheap hash would read the whole file, which
    // the full hash is about to do anyway — so it would cost a second read to
    // learn something already implied.
    let buckets = if size > PREFIX {
        split(
            members,
            |member| {
                let (digest, read) = hash_prefix(&member.path)?;
                progress.hashed(&member.path, read);
                Ok(digest.to_le_bytes().to_vec())
            },
            progress,
        )
    } else {
        vec![members]
    };

    buckets
        .into_iter()
        .flat_map(|bucket| {
            split(
                bucket,
                |member| {
                    let (digest, read) = hash_full(&member.path)?;
                    progress.hashed(&member.path, read);
                    Ok(digest.as_bytes().to_vec())
                },
                progress,
            )
        })
        .filter_map(|identical| group(pool, size, identical, options))
        .collect()
}

/// Confirm each member is still the regular file of that size the scan saw, and
/// take its dates while the metadata is in hand.
///
/// One `symlink_metadata` per file in a contested bucket — cheap beside the
/// reads to come, and it is what keeps three things out of a removal plan: a
/// symlink (its content is a path, not the file it names), a file that changed
/// under us, and one that is already gone. The keeper's dates ride along on it,
/// which is why choosing by creation time costs nothing and why `ScanNode` never
/// had to grow a field for it.
fn still_there(members: Vec<Member>, size: u64, progress: &Progress<'_>) -> Vec<Member> {
    members
        .into_iter()
        .filter_map(|member| match std::fs::symlink_metadata(&member.path) {
            Ok(metadata) if metadata.is_file() && metadata.len() == size => Some(Member {
                // `created` is unsupported on some Linux filesystems and
                // `modified` on almost none; either way an error is "unknown",
                // which the keeper rules treat as never winning.
                created: metadata.created().ok(),
                modified: metadata.modified().ok(),
                ..member
            }),
            Ok(metadata) if metadata.is_file() => {
                progress.skip(
                    &member.path,
                    SkipReason::Other(format!(
                        "size changed since the scan: {size} bytes then, {} now",
                        metadata.len()
                    )),
                );
                None
            }
            // A symlink or a device node is silently not a duplicate — nothing
            // went wrong, it was simply never a candidate.
            Ok(_) => None,
            Err(err) => {
                progress.skip(&member.path, skip_reason(&err));
                None
            }
        })
        .collect()
}

/// Sub-divide a bucket by a digest, dropping whatever ends up alone.
///
/// A member whose digest cannot be read is dropped **and reported**: an
/// unreadable file is not evidence of anything, least of all that some other
/// file is a redundant copy of it.
fn split<K, F>(members: Vec<Member>, digest: F, progress: &Progress<'_>) -> Vec<Vec<Member>>
where
    K: std::hash::Hash + Eq + Ord,
    F: Fn(&Member) -> io::Result<K>,
{
    let mut by_digest: HashMap<K, Vec<Member>> = HashMap::new();
    for member in members {
        match digest(&member) {
            Ok(key) => by_digest.entry(key).or_default().push(member),
            Err(err) => progress.skip(&member.path, skip_reason(&err)),
        }
    }
    let mut buckets: Vec<(K, Vec<Member>)> = by_digest
        .into_iter()
        .filter(|(_, members)| members.len() > 1)
        .collect();
    // Ordered by digest so the pass is reproducible before the final sort, not
    // only after it.
    buckets.sort_by(|a, b| a.0.cmp(&b.0));
    buckets.into_iter().map(|(_, members)| members).collect()
}

/// Turn a set of confirmed-identical files into a group with one keeper.
fn group(
    pool: usize,
    apparent: u64,
    mut members: Vec<Member>,
    options: &DuplicateOptions,
) -> Option<DuplicateGroup> {
    if members.len() < 2 {
        return None;
    }
    members.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));

    // The pool's policy, and the flags over it. Resolved here rather than at the
    // call site so that "the rule says, the flag overrules" is one sentence in
    // one place.
    let (rule, tier, keep, keep_in) = policy(pool, options);
    let chosen = choose_keeper(&members, keep, keep_in);
    let keeper_path = members[chosen.index].path.clone();
    let copies: Vec<Copy> = members
        .into_iter()
        .enumerate()
        .filter(|(index, _)| *index != chosen.index)
        .map(|(_, member)| Copy {
            path: member.path,
            allocated: member.allocated,
        })
        .collect();

    Some(DuplicateGroup {
        apparent,
        rule,
        tier,
        keep,
        keeper: keeper_path,
        keeper_date: chosen.date,
        keeper_fell_back: chosen.fell_back,
        reclaimable: copies.iter().map(|copy| copy.allocated).sum(),
        copies,
    })
}

/// The keeper, and what decided it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Chosen {
    index: usize,
    /// The date the rule actually read on the winner.
    ///
    /// `None` where no date was read at all — `First`, or a group in which
    /// nothing carried either date. It is carried out of here because the report
    /// shows it: a keeper rule the user cannot check is one they have to trust,
    /// and this one was changed once already for choosing wrongly in a way that
    /// was invisible until the paths were read closely.
    date: Option<SystemTime>,
    fell_back: bool,
}

/// What a pool says about its copies, with the flags applied over it.
fn policy(pool: usize, options: &DuplicateOptions) -> (String, Tier, Keep, &[PathBuf]) {
    // The pool is looked up by the index the member carried out of `collect`,
    // which is the rule's own position — so this cannot name a different rule
    // than the one that admitted the file.
    let found = options.rules.at(pool);
    let keep = options
        .keep
        .or_else(|| found.as_ref().and_then(|rule| rule.keep))
        .unwrap_or_default();
    let keep_in: &[PathBuf] = match &options.keep_in {
        Some(flagged) => flagged,
        None => found.as_ref().map_or(&[][..], |rule| rule.keep_in),
    };
    (
        found
            .as_ref()
            .map_or_else(String::new, |rule| rule.name.to_owned()),
        found.as_ref().map_or(Tier::Confirm, |rule| rule.tier),
        keep,
        keep_in,
    )
}

/// Which member stays, and what decided it.
///
/// `members` is already sorted by path bytes, so every "first" below is the
/// byte-lexicographic one and every tie breaks that way.
fn choose_keeper(members: &[Member], keep: Keep, keep_in: &[PathBuf]) -> Chosen {
    // A preferred root beats the policy outright; the policy then only chooses
    // among the members inside it.
    let inside: Vec<usize> = keep_in
        .iter()
        .find_map(|root| {
            let matching: Vec<usize> = members
                .iter()
                .enumerate()
                .filter(|(_, member)| member.path.starts_with(root))
                .map(|(index, _)| index)
                .collect();
            (!matching.is_empty()).then_some(matching)
        })
        .unwrap_or_else(|| (0..members.len()).collect());

    let Some(date) = keep.date() else {
        // `First` reads nothing, so it cannot degrade and has nothing to show.
        return Chosen {
            index: inside[0],
            date: None,
            fell_back: false,
        };
    };
    let earliest = keep.wants_earliest();

    let by_date = |which: Date| {
        // An unknown date never wins: `None` is "we do not know", and reading it
        // as the epoch would hand the keeper to whichever file the platform
        // happened to be quiet about.
        let mut best: Option<(usize, SystemTime)> = None;
        for &index in &inside {
            let Some(time) = members[index].date(which) else {
                continue;
            };
            let better = match best {
                None => true,
                Some((_, current)) if earliest => time < current,
                Some((_, current)) => time > current,
            };
            if better {
                best = Some((index, time));
            }
        }
        best
    };

    match by_date(date) {
        Some((index, time)) => Chosen {
            index,
            date: Some(time),
            fell_back: false,
        },
        // No member carries the date asked for. Degrade rather than mislead:
        // the other date, then the path — and say so, because "kept the oldest"
        // and "kept the first path" are different claims about the same run.
        None => match by_date(date.other()) {
            Some((index, time)) => Chosen {
                index,
                date: Some(time),
                fell_back: true,
            },
            None => Chosen {
                index: inside[0],
                date: None,
                fell_back: true,
            },
        },
    }
}

/// xxh3-128 of the first [`PREFIX`] bytes, and how many were read.
///
/// Never the last word on identity: it is not a cryptographic hash, and nothing
/// is removed on its word alone. All it does is keep [`hash_full`] off files
/// that already differ.
fn hash_prefix(path: &Path) -> io::Result<(u128, u64)> {
    use xxhash_rust::xxh3::Xxh3;

    let mut file = File::open(path)?;
    let mut buffer = vec![0u8; PREFIX as usize];
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    let mut hasher = Xxh3::new();
    hasher.update(&buffer[..filled]);
    Ok((hasher.digest128(), filled as u64))
}

/// blake3 over the whole file, and how many bytes that took.
///
/// What "identical" finally means here.
///
/// A 256-bit digest from a cryptographic hash: two different files sharing one
/// is not a thing that happens, which is why there is no byte-for-byte pass
/// after it. Doubling every read to defend against that would be a real cost
/// against an unreachable one.
fn hash_full(path: &Path) -> io::Result<(blake3::Hash, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut total = 0;
    loop {
        match file.read(&mut buffer)? {
            0 => break,
            read => {
                hasher.update(&buffer[..read]);
                total += read as u64;
            }
        }
    }
    Ok((hasher.finalize(), total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ScanOptions, scan};
    use std::fs;
    use std::time::Duration;

    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).expect("write file");
    }

    /// Content of `len` bytes that differs per `seed` — so two files are
    /// identical only when both arguments match.
    fn content(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
    }

    /// The options a search gets before any config narrows it: the built-in
    /// duplicate rule, which searches everywhere but a repository's own store.
    fn searching() -> DuplicateOptions {
        DuplicateOptions {
            rules: crate::dup_rules::DuplicateRules::builtin(&crate::rules::UserDirs::default()),
            ..DuplicateOptions::default()
        }
    }

    fn found(root: &Path, options: &DuplicateOptions) -> Duplicates {
        let tree = scan(&ScanOptions {
            root: root.to_path_buf(),
            ..ScanOptions::default()
        });
        duplicates(&tree, options, &|_| {})
    }

    fn plain(root: &Path) -> Duplicates {
        found(root, &searching())
    }

    /// Create a hard link, or report that this filesystem cannot — the same
    /// degradation `dedup.rs` uses, for the same reason.
    fn try_hard_link(original: &Path, link: &Path) -> bool {
        match fs::hard_link(original, link) {
            Ok(()) => true,
            Err(err) => {
                eprintln!("skipping: this filesystem has no hard links ({err})");
                false
            }
        }
    }

    #[test]
    fn identical_files_form_one_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(&root.join("a.bin"), &content(1, 8192));
        write(&root.join("b.bin"), &content(1, 8192));

        let found = plain(root);

        assert_eq!(found.groups.len(), 1, "{:?}", found.groups);
        let group = &found.groups[0];
        assert_eq!(
            group.keeper,
            root.join("a.bin"),
            "the file written first stays"
        );
        assert_eq!(group.copies.len(), 1);
        assert_eq!(group.copies[0].path, root.join("b.bin"));
        assert_eq!(
            group.reclaimable, group.copies[0].allocated,
            "reclaimable is the sum of what goes, not of the whole group"
        );
        assert!(group.reclaimable > 0);
    }

    #[test]
    fn same_size_different_content_is_not_a_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(&root.join("a.bin"), &content(1, 8192));
        write(&root.join("b.bin"), &content(2, 8192));

        assert!(
            plain(root).groups.is_empty(),
            "same size is not same content"
        );
    }

    /// The whole point of the second stage: the prefix hash agrees and must not
    /// be the last word. The fixture is deliberately larger than `PREFIX`, or it
    /// would never reach that stage and would prove nothing.
    #[test]
    fn a_difference_past_the_prefix_is_still_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let len = (PREFIX as usize) * 2;
        let mut first = content(3, len);
        let mut second = first.clone();
        *second.last_mut().expect("non-empty") ^= 0xff;
        first.truncate(len);

        write(&root.join("a.bin"), &first);
        write(&root.join("b.bin"), &second);

        assert!(
            plain(root).groups.is_empty(),
            "files sharing their first 16 KiB are not duplicates"
        );

        // …and the same fixture with the tail restored *is* a group, so the
        // assertion above cannot pass by never reaching the full hash at all.
        write(&root.join("b.bin"), &first);
        assert_eq!(plain(root).groups.len(), 1);
    }

    #[test]
    fn hardlinked_names_are_not_duplicates_of_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(&root.join("a.bin"), &content(4, 8192));
        if !try_hard_link(&root.join("a.bin"), &root.join("b.bin")) {
            return;
        }

        assert!(
            plain(root).groups.is_empty(),
            "removing one name of an inode frees nothing, so it is no candidate"
        );
    }

    #[test]
    fn a_separate_copy_of_a_hardlinked_file_is_a_duplicate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let bytes = content(5, 8192);
        write(&root.join("a.bin"), &bytes);
        if !try_hard_link(&root.join("a.bin"), &root.join("b.bin")) {
            return;
        }
        write(&root.join("c.bin"), &bytes); // its own inode

        let found = plain(root);

        assert_eq!(found.groups.len(), 1, "{:?}", found.groups);
        let group = &found.groups[0];
        assert_eq!(group.keeper, root.join("a.bin"));
        assert_eq!(
            group.copies.iter().map(|c| &c.path).collect::<Vec<_>>(),
            vec![&root.join("c.bin")],
            "the zeroed second name of the inode is not a copy; the separate file is"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_never_a_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        // Two symlinks with identical targets have identical *link* contents and
        // identical sizes — the pair a size bucket would otherwise offer up.
        write(&root.join("target.bin"), &content(6, 8192));
        std::os::unix::fs::symlink(root.join("target.bin"), root.join("one.link"))
            .expect("symlink");
        std::os::unix::fs::symlink(root.join("target.bin"), root.join("two.link"))
            .expect("symlink");

        let found = plain(root);

        assert!(found.groups.is_empty(), "{:?}", found.groups);
        assert!(
            found.skipped.is_empty(),
            "a symlink is not a failure, it was simply never a candidate"
        );
    }

    #[test]
    fn a_vanished_file_is_skipped_and_its_group_survives_without_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let bytes = content(7, 8192);
        for name in ["a.bin", "b.bin", "c.bin"] {
            write(&root.join(name), &bytes);
        }

        let tree = scan(&ScanOptions {
            root: root.to_path_buf(),
            ..ScanOptions::default()
        });
        fs::remove_file(root.join("c.bin")).expect("remove");
        let found = duplicates(&tree, &searching(), &|_| {});

        assert_eq!(found.groups.len(), 1);
        assert_eq!(found.groups[0].copies.len(), 1, "two of three remain");
        assert_eq!(
            found.skipped,
            vec![SkippedEntry {
                path: root.join("c.bin"),
                reason: SkipReason::NotFound,
            }]
        );
    }

    /// A copy is never proposed against content that could not be read.
    #[test]
    fn a_group_that_loses_its_partner_disappears() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let bytes = content(8, 8192);
        write(&root.join("a.bin"), &bytes);
        write(&root.join("b.bin"), &bytes);

        let tree = scan(&ScanOptions {
            root: root.to_path_buf(),
            ..ScanOptions::default()
        });
        fs::remove_file(root.join("b.bin")).expect("remove");
        let found = duplicates(&tree, &searching(), &|_| {});

        assert!(found.groups.is_empty(), "{:?}", found.groups);
        assert_eq!(found.skipped.len(), 1);
    }

    #[test]
    fn empty_files_are_never_members() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(&root.join("a.bin"), b"");
        write(&root.join("b.bin"), b"");

        assert!(
            plain(root).groups.is_empty(),
            "every empty file is identical to every other and removing one frees nothing"
        );
    }

    #[test]
    fn min_size_keeps_small_files_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(&root.join("a.bin"), &content(9, 1024));
        write(&root.join("b.bin"), &content(9, 1024));

        let options = DuplicateOptions {
            min_size: 4096,
            ..searching()
        };
        assert!(found(root, &options).groups.is_empty());
        assert_eq!(plain(root).groups.len(), 1, "and they are a group below it");
    }

    #[test]
    fn a_claimed_subtree_is_never_looked_inside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let bytes = content(10, 8192);
        fs::create_dir(root.join("node_modules")).expect("mkdir");
        write(&root.join("node_modules/a.bin"), &bytes);
        write(&root.join("node_modules/b.bin"), &bytes);

        assert!(
            plain(root).groups.is_empty(),
            "a directory that goes wholesale is not a place to remove single files from"
        );

        // The same two files outside it are a group, so the rule above is what
        // suppressed them rather than the fixture.
        write(&root.join("a.bin"), &bytes);
        write(&root.join("b.bin"), &bytes);
        assert_eq!(plain(root).groups.len(), 1);
    }

    #[test]
    fn groups_are_deterministic_across_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for (seed, len) in [(11u8, 4096), (12, 8192), (13, 20480)] {
            for name in ["one", "two", "three"] {
                write(
                    &root.join(format!("{seed}-{name}.bin")),
                    &content(seed, len),
                );
            }
        }

        let first = plain(root);
        assert_eq!(first.groups.len(), 3);
        assert_eq!(first.groups, plain(root).groups, "order and content both");
    }

    #[test]
    fn groups_are_ordered_by_what_they_free() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(&root.join("small-a.bin"), &content(14, 4096));
        write(&root.join("small-b.bin"), &content(14, 4096));
        write(&root.join("big-a.bin"), &content(15, 65536));
        write(&root.join("big-b.bin"), &content(15, 65536));

        let found = plain(root);

        assert_eq!(found.groups.len(), 2);
        assert!(
            found.groups[0].reclaimable > found.groups[1].reclaimable,
            "the biggest reclaim comes first, not the first path"
        );
        assert_eq!(found.groups[0].keeper, root.join("big-a.bin"));
    }

    /// The point of the default, on a real filesystem: the original is the file
    /// that existed first, and its path has nothing to do with it.
    #[test]
    fn the_default_keeps_the_original_not_the_first_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let bytes = content(19, 8192);
        // Written first, but sorts last.
        write(&root.join("z-original.bin"), &bytes);
        std::thread::sleep(Duration::from_millis(20));
        write(&root.join("a-copy.bin"), &bytes);

        let dates = |name: &str| {
            let metadata = fs::metadata(root.join(name)).expect("stat");
            (metadata.created().ok(), metadata.modified().ok())
        };
        if dates("z-original.bin") == dates("a-copy.bin") {
            eprintln!("skipping: this filesystem cannot tell the two writes apart");
            return;
        }

        let group = &plain(root).groups[0];
        assert_eq!(
            group.keeper,
            root.join("z-original.bin"),
            "the earlier file stays even though the other sorts first"
        );
        assert_eq!(group.copies[0].path, root.join("a-copy.bin"));
    }

    #[test]
    fn keep_in_beats_the_policy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let bytes = content(16, 8192);
        fs::create_dir(root.join("keep")).expect("mkdir");
        fs::create_dir(root.join("elsewhere")).expect("mkdir");
        // "elsewhere/…" sorts first, so the default policy would keep it.
        write(&root.join("elsewhere/a.bin"), &bytes);
        write(&root.join("keep/z.bin"), &bytes);

        assert_eq!(
            plain(root).groups[0].keeper,
            root.join("elsewhere/a.bin"),
            "without a preferred root the default rule decides, and that file was written first"
        );

        let options = DuplicateOptions {
            keep_in: Some(vec![root.join("keep")]),
            ..searching()
        };
        let found = found(root, &options);
        assert_eq!(found.groups[0].keeper, root.join("keep/z.bin"));
        assert_eq!(found.groups[0].copies[0].path, root.join("elsewhere/a.bin"));
    }

    #[test]
    fn progress_reports_every_file_it_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(&root.join("a.bin"), &content(17, 8192));
        write(&root.join("b.bin"), &content(17, 8192));

        let seen = Mutex::new(Vec::new());
        let tree = scan(&ScanOptions {
            root: root.to_path_buf(),
            ..ScanOptions::default()
        });
        let found = duplicates(&tree, &searching(), &|hashed| {
            seen.lock().expect("seen").push(hashed.running_total);
        });

        let seen = seen.into_inner().expect("seen");
        assert_eq!(seen.len(), 2, "one report per file hashed");
        assert_eq!(found.files_hashed, 2);
        assert_eq!(found.bytes_read, 2 * 8192);
        assert_eq!(
            *seen.iter().max().expect("reports"),
            found.bytes_read,
            "the running total ends where the pass does"
        );
    }

    /// Files no larger than the prefix are read once, not twice: the cheap hash
    /// would read the whole file only to be superseded by the full one.
    #[test]
    fn small_files_skip_the_prefix_stage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let len = PREFIX as usize;
        write(&root.join("a.bin"), &content(18, len));
        write(&root.join("b.bin"), &content(18, len));

        let found = plain(root);

        assert_eq!(found.groups.len(), 1);
        assert_eq!(found.files_hashed, 2, "two files, one read each");
        assert_eq!(found.bytes_read, 2 * PREFIX);
    }

    /// A part rooted at `root`, matching everything under it.
    fn part(root: std::path::PathBuf) -> crate::rules::Part {
        crate::rules::Part {
            root: Some(root.to_string_lossy().into_owned()),
            includes: vec!["**".into()],
            ..crate::rules::Part::default()
        }
    }

    /// One pool per name, each rooted where it is named.
    fn pools(areas: &[(&str, std::path::PathBuf)]) -> DuplicateOptions {
        DuplicateOptions {
            rules: crate::dup_rules::DuplicateRules::new(
                areas
                    .iter()
                    .map(|(name, root)| crate::dup_rules::DuplicateRule {
                        name: (*name).to_owned(),
                        parts: vec![part(root.clone())],
                        ..crate::dup_rules::DuplicateRule::default()
                    })
                    .collect(),
                &crate::rules::UserDirs::default(),
            )
            .expect("compiles"),
            ..DuplicateOptions::default()
        }
    }

    /// One pool over the whole fixture, with a keeper policy of its own.
    fn one_pool(root: &Path, keep: Option<Keep>) -> crate::dup_rules::DuplicateRules {
        crate::dup_rules::DuplicateRules::new(
            vec![crate::dup_rules::DuplicateRule {
                name: "here".into(),
                keep,
                parts: vec![part(root.to_path_buf())],
                ..crate::dup_rules::DuplicateRule::default()
            }],
            &crate::rules::UserDirs::default(),
        )
        .expect("compiles")
    }

    // ---- the rules, and the flags over them ------------------------------

    /// Two pools, and files in different ones are not duplicates of each other
    /// however identical they are. That is the answer the rules exist to give.
    #[test]
    fn identical_files_in_different_pools_are_not_a_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let bytes = content(30, 8192);
        fs::create_dir(root.join("photos")).expect("mkdir");
        fs::create_dir(root.join("downloads")).expect("mkdir");
        write(&root.join("photos/a.bin"), &bytes);
        write(&root.join("downloads/a.bin"), &bytes);

        let split = pools(&[
            ("photos", root.join("photos")),
            ("downloads", root.join("downloads")),
        ]);
        assert!(
            found(root, &split).groups.is_empty(),
            "one pool each, so nothing to pair with"
        );

        // The same two files under one rule with two parts *are* a group — the
        // difference is the configuration, not the disk.
        let together = DuplicateOptions {
            rules: crate::dup_rules::DuplicateRules::new(
                vec![crate::dup_rules::DuplicateRule {
                    name: "both".into(),
                    parts: vec![part(root.join("photos")), part(root.join("downloads"))],
                    ..crate::dup_rules::DuplicateRule::default()
                }],
                &crate::rules::UserDirs::default(),
            )
            .expect("compiles"),
            ..DuplicateOptions::default()
        };
        assert_eq!(found(root, &together).groups.len(), 1);
    }

    /// The pool's own policy decides, so two areas may answer differently in one
    /// run.
    #[test]
    fn each_pool_keeps_by_its_own_rule() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for (area, seed) in [("first", 31u8), ("second", 32)] {
            fs::create_dir(root.join(area)).expect("mkdir");
            // Written in this order, so `z` is the older of the two.
            write(&root.join(format!("{area}/z.bin")), &content(seed, 8192));
            std::thread::sleep(Duration::from_millis(20));
            write(&root.join(format!("{area}/a.bin")), &content(seed, 8192));
        }

        let mut rules = vec![
            crate::dup_rules::DuplicateRule {
                name: "first".into(),
                keep: Some(Keep::OldestCreated),
                parts: vec![part(root.join("first"))],
                ..crate::dup_rules::DuplicateRule::default()
            },
            crate::dup_rules::DuplicateRule {
                name: "second".into(),
                keep: Some(Keep::First),
                parts: vec![part(root.join("second"))],
                ..crate::dup_rules::DuplicateRule::default()
            },
        ];
        rules.sort_by(|a, b| a.name.cmp(&b.name));
        let options = DuplicateOptions {
            rules: crate::dup_rules::DuplicateRules::new(rules, &crate::rules::UserDirs::default())
                .expect("compiles"),
            ..DuplicateOptions::default()
        };

        let found = found(root, &options);
        let keeper = |area: &str| {
            found
                .groups
                .iter()
                .find(|group| group.rule == area)
                .map(|group| group.keeper.file_name().expect("name").to_owned())
                .expect(area)
        };
        assert_eq!(keeper("first"), "z.bin", "the earlier file, by creation");
        assert_eq!(keeper("second"), "a.bin", "the earlier path, by bytes");
    }

    /// The flag replaces what every rule said — that is what a flag is for.
    #[test]
    fn the_keep_flag_overrules_every_pool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(&root.join("z.bin"), &content(33, 8192));
        std::thread::sleep(Duration::from_millis(20));
        write(&root.join("a.bin"), &content(33, 8192));

        let by_rule = DuplicateOptions {
            rules: one_pool(root, Some(Keep::OldestCreated)),
            ..DuplicateOptions::default()
        };
        assert_eq!(
            found(root, &by_rule).groups[0]
                .keeper
                .file_name()
                .expect("name"),
            "z.bin"
        );

        let flagged = DuplicateOptions {
            keep: Some(Keep::First),
            ..by_rule
        };
        assert_eq!(
            found(root, &flagged).groups[0]
                .keeper
                .file_name()
                .expect("name"),
            "a.bin",
            "the flag beat the rule"
        );
    }

    /// Both floors apply and the larger decides: a flag narrows what a part
    /// said, it never widens it.
    #[test]
    fn the_larger_of_the_two_floors_decides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(&root.join("a.bin"), &content(34, 4096));
        write(&root.join("b.bin"), &content(34, 4096));

        let with_floor = |rule_floor: u64, flag: u64| {
            let rules = crate::dup_rules::DuplicateRules::new(
                vec![crate::dup_rules::DuplicateRule {
                    name: "here".into(),
                    parts: vec![crate::rules::Part {
                        root: Some(root.to_string_lossy().into_owned()),
                        includes: vec!["**".into()],
                        min_size: rule_floor,
                        ..crate::rules::Part::default()
                    }],
                    ..crate::dup_rules::DuplicateRule::default()
                }],
                &crate::rules::UserDirs::default(),
            )
            .expect("compiles");
            found(
                root,
                &DuplicateOptions {
                    rules,
                    min_size: flag,
                    ..DuplicateOptions::default()
                },
            )
            .groups
            .len()
        };

        assert_eq!(with_floor(0, 0), 1, "neither floor keeps them out");
        assert_eq!(with_floor(8192, 0), 0, "the part's floor is the larger");
        assert_eq!(with_floor(0, 8192), 0, "the flag's floor is the larger");
        assert_eq!(
            with_floor(1024, 1024),
            1,
            "and equal floors let them through"
        );
    }

    // ---- the keeper rules, as a pure function -----------------------------

    /// `created` and `modified`, in that order — the two the rules read.
    fn member(path: &str, created: Option<SystemTime>, modified: Option<SystemTime>) -> Member {
        Member {
            path: PathBuf::from(path),
            pool: 0,
            allocated: 4096,
            apparent: 4096,
            created,
            modified,
        }
    }

    fn at(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    /// `choose_keeper` is documented as taking its input already sorted by path
    /// bytes — the same order `group` puts it in.
    fn sorted(mut members: Vec<Member>) -> Vec<Member> {
        members.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
        members
    }

    fn choose(members: &[Member], keep: Keep, keep_in: Vec<PathBuf>) -> (&Path, bool) {
        let chosen = choose_keeper(members, keep, &keep_in);
        (&members[chosen.index].path, chosen.fell_back)
    }

    /// The date the rule read, which is what the report shows beside the keeper.
    fn decided_on(members: &[Member], keep: Keep) -> Option<SystemTime> {
        choose_keeper(members, keep, &[]).date
    }

    fn keeper(members: &[Member], keep: Keep, keep_in: Vec<PathBuf>) -> &Path {
        choose(members, keep, keep_in).0
    }

    #[test]
    fn the_default_keeps_the_earliest_created() {
        assert_eq!(Keep::default(), Keep::OldestCreated);
    }

    #[test]
    fn keep_first_takes_the_byte_first_path() {
        let members = sorted(vec![
            member("/z/early.bin", at(10), at(10)),
            member("/a/late.bin", at(99), at(99)),
        ]);
        let (keeper, fell_back) = choose(&members, Keep::First, vec![]);
        assert_eq!(keeper, Path::new("/a/late.bin"));
        assert!(!fell_back, "a rule that reads no date can never degrade");
    }

    #[test]
    fn created_and_modified_are_different_questions() {
        // The copy was made later (created 300) but carries the original's
        // mtime, exactly as `cp -p` and unpacking an archive leave it.
        let members = sorted(vec![
            member("/copy.bin", at(300), at(100)),
            member("/original.bin", at(100), at(200)),
        ]);

        assert_eq!(
            keeper(&members, Keep::OldestCreated, vec![]),
            Path::new("/original.bin"),
            "creation time sees which file existed first"
        );
        assert_eq!(
            keeper(&members, Keep::OldestModified, vec![]),
            Path::new("/copy.bin"),
            "modification time can be carried onto the copy, and then says the opposite"
        );
    }

    #[test]
    fn oldest_and_newest_pick_opposite_ends() {
        let members = sorted(vec![
            member("/a.bin", at(300), at(30)),
            member("/b.bin", at(100), at(10)),
            member("/c.bin", at(200), at(20)),
        ]);
        assert_eq!(
            keeper(&members, Keep::OldestCreated, vec![]),
            Path::new("/b.bin")
        );
        assert_eq!(
            keeper(&members, Keep::NewestCreated, vec![]),
            Path::new("/a.bin")
        );
        assert_eq!(
            keeper(&members, Keep::OldestModified, vec![]),
            Path::new("/b.bin")
        );
        assert_eq!(
            keeper(&members, Keep::NewestModified, vec![]),
            Path::new("/a.bin")
        );
    }

    /// An unknown date is "we do not know", not "the epoch" — reading it as a
    /// time would hand the keeper to whichever file the platform was quiet about.
    #[test]
    fn an_unknown_date_never_wins() {
        let members = sorted(vec![
            member("/a.bin", None, None),
            member("/b.bin", at(500), at(500)),
        ]);
        for keep in [Keep::OldestCreated, Keep::NewestCreated] {
            let (keeper, fell_back) = choose(&members, keep, vec![]);
            assert_eq!(keeper, Path::new("/b.bin"), "{keep:?}");
            assert!(
                !fell_back,
                "one member with the date is enough to apply the rule asked for"
            );
        }
    }

    /// The filesystem with no birth times — where a rule about creation has to
    /// degrade rather than mislead.
    #[test]
    fn no_creation_time_anywhere_falls_back_to_modified() {
        let members = sorted(vec![
            member("/a.bin", None, at(900)),
            member("/b.bin", None, at(100)),
        ]);

        let (keeper, fell_back) = choose(&members, Keep::OldestCreated, vec![]);
        assert_eq!(
            keeper,
            Path::new("/b.bin"),
            "the same end of the other date, not a different question"
        );
        assert!(fell_back, "and the report has to be able to say so");
    }

    #[test]
    fn no_date_at_all_falls_back_to_the_path() {
        let members = sorted(vec![
            member("/z.bin", None, None),
            member("/a.bin", None, None),
        ]);
        let (keeper, fell_back) = choose(&members, Keep::OldestCreated, vec![]);
        assert_eq!(keeper, Path::new("/a.bin"));
        assert!(fell_back);
    }

    /// The report shows what the choice was made on, so the value has to be the
    /// winner's own and the date the rule actually read — not the other one.
    #[test]
    fn the_date_that_decided_comes_back_with_the_keeper() {
        let members = sorted(vec![
            member("/a.bin", at(300), at(30)),
            member("/b.bin", at(100), at(10)),
        ]);

        assert_eq!(decided_on(&members, Keep::OldestCreated), at(100));
        assert_eq!(decided_on(&members, Keep::NewestCreated), at(300));
        assert_eq!(decided_on(&members, Keep::OldestModified), at(10));
        assert_eq!(
            decided_on(&members, Keep::First),
            None,
            "a rule that reads no date has none to show"
        );
    }

    /// Degraded, the date shown must be the one that actually decided — showing
    /// a creation time that no member had would be the misleading this whole
    /// fallback exists to avoid.
    #[test]
    fn a_degraded_rule_shows_the_date_it_fell_back_to() {
        let members = sorted(vec![
            member("/a.bin", None, at(900)),
            member("/b.bin", None, at(100)),
        ]);

        assert_eq!(decided_on(&members, Keep::OldestCreated), at(100));
    }

    #[test]
    fn keep_in_tries_its_roots_in_order() {
        let members = sorted(vec![
            member("/first/a.bin", at(10), at(10)),
            member("/second/b.bin", at(20), at(20)),
        ]);
        assert_eq!(
            keeper(
                &members,
                Keep::NewestCreated,
                vec![PathBuf::from("/nowhere"), PathBuf::from("/first")],
            ),
            Path::new("/first/a.bin"),
            "a root no member lies under is passed over, not fatal"
        );
    }

    /// The policy still chooses *within* the winning root.
    #[test]
    fn keep_in_narrows_and_the_policy_decides() {
        let members = sorted(vec![
            member("/keep/a.bin", at(10), at(10)),
            member("/keep/b.bin", at(90), at(90)),
            member("/other/c.bin", at(99), at(99)),
        ]);
        assert_eq!(
            keeper(&members, Keep::NewestCreated, vec![PathBuf::from("/keep")]),
            Path::new("/keep/b.bin")
        );
    }
}

#[cfg(test)]
mod diagnostics {
    use super::*;
    use crate::rules::{Rules, UserDirs};
    use crate::{ScanOptions, scan};
    use std::time::Instant;

    /// Run the pipeline over a real tree and print what it found.
    ///
    /// **Reads only** — nothing here removes, trashes or writes anything; the
    /// pass itself cannot, and neither can this. It exists because the CLI has
    /// no `--dup` yet (Task 3), and a pipeline that has only ever seen
    /// hand-built fixtures has not really been seen at all.
    ///
    /// ```text
    /// just bench-dup ~/Downloads
    /// DT_DUP_MIN=104857600 just bench-dup ~            # 100 MiB and up
    /// ```
    ///
    /// Ignored by default: it needs a path, prints, and asserts nothing.
    #[test]
    #[ignore = "diagnostic: needs DT_PHASE_PATH, prints findings, asserts nothing"]
    fn over_a_real_tree() {
        let root = std::env::var("DT_PHASE_PATH")
            .expect("set DT_PHASE_PATH to the tree to search, e.g. DT_PHASE_PATH=~/Downloads");
        let min_size: u64 = std::env::var("DT_DUP_MIN")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1024 * 1024);

        let started = Instant::now();
        let tree = scan(&ScanOptions {
            root: root.clone().into(),
            ..ScanOptions::default()
        });
        let scanned = started.elapsed();

        // The rules a first run gets, so the pruning this pass depends on is
        // exercised rather than skipped.
        let dirs = UserDirs {
            home: std::env::var_os("HOME").map(Into::into),
            ..UserDirs::default()
        };
        // The rules a first `--dup` run gets. Without them nothing is in a pool
        // and the whole pass finds nothing — which it would do **silently**, so
        // the report below says how many pools there were.
        let pool_root = std::env::var("DT_DUP_ROOT").ok();
        let dup_rules = match &pool_root {
            Some(root) => crate::dup_rules::DuplicateRules::new(
                vec![crate::dup_rules::DuplicateRule {
                    name: "rooted".into(),
                    parts: vec![crate::rules::Part {
                        root: Some(root.clone()),
                        includes: vec!["**".into()],
                        excludes: vec!["**/.git/".into(), "**/.git/**".into()],
                        ..crate::rules::Part::default()
                    }],
                    ..crate::dup_rules::DuplicateRule::default()
                }],
                &dirs,
            )
            .expect("compiles"),
            None => crate::dup_rules::DuplicateRules::builtin(&dirs),
        };

        let options = DuplicateOptions {
            detect: DetectOptions {
                rules: Rules::builtin(&dirs),
                now: SystemTime::now(),
            },
            rules: dup_rules,
            min_size,
            ..DuplicateOptions::default()
        };

        // The funnel, measured with the module's own two stages so that the
        // report below is about this code rather than about a re-implementation
        // of it. Stage 2 costs nothing beyond the scan; stage 3 is where the
        // reads begin.
        let claimed: HashSet<PathBuf> = detect(&tree, &options.detect)
            .into_iter()
            .map(|found| found.path)
            .collect();
        let mut eligible = Vec::new();
        collect(&tree.root, &[], &claimed, &options, &mut eligible);
        let mut buckets: HashMap<u64, usize> = HashMap::new();
        for member in &eligible {
            *buckets.entry(member.apparent).or_default() += 1;
        }
        let contested: usize = buckets.values().filter(|&&n| n > 1).sum();

        let started = Instant::now();
        let found = duplicates(&tree, &options, &|_| {});
        let hashing = started.elapsed();

        let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
        let reclaimable: u64 = found.groups.iter().map(|group| group.reclaimable).sum();
        let copies: usize = found.groups.iter().map(|group| group.copies.len()).sum();

        println!("\n{root} — scanned in {scanned:.1?}, searched in {hashing:.1?}");
        println!("  minimum      {:.1} MiB", mib(min_size));
        println!(
            "  eligible     {} files ({:.1} MiB)",
            eligible.len(),
            mib(eligible.iter().map(|m| m.apparent).sum())
        );
        println!(
            "  contested    {contested} files share a size with something ({:.0}%)",
            100.0 * contested as f64 / eligible.len().max(1) as f64
        );
        println!("  groups       {}", found.groups.len());
        println!("  copies       {copies}");
        println!("  reclaimable  {:.1} MiB", mib(reclaimable));
        println!(
            "  read         {:.1} MiB across {} files",
            mib(found.bytes_read),
            found.files_hashed
        );
        println!("  skipped      {}", found.skipped.len());
        println!(
            "  pools        {}",
            if found.pools.is_empty() {
                "none — nothing was searched at all".to_owned()
            } else {
                found
                    .pools
                    .iter()
                    .map(|pool| format!("{} ({} files)", pool.rule, pool.files))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );

        for group in found.groups.iter().take(20) {
            println!(
                "\n  {:.1} MiB  x{}  frees {:.1} MiB",
                mib(group.apparent),
                group.copies.len() + 1,
                mib(group.reclaimable)
            );
            println!("    keep    {}", group.keeper.display());
            for copy in &group.copies {
                println!("    remove  {}", copy.path.display());
            }
        }
        if found.groups.len() > 20 {
            println!("\n  … and {} more groups", found.groups.len() - 20);
        }
    }
}
