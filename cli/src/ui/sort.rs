//! Putting a directory's entries in an order.
//!
//! Two properties carry the weight here, and neither is about comparison.
//!
//! **Directories stay above files**, as in `mc`, and reversing does not change
//! that: "by name, descending" is a statement about names, not an invitation to
//! interleave two kinds of thing.
//!
//! **The order applied is reported back.** Creation time is not universal —
//! Linux keeps it only through `statx`, on filesystems that have it — so asking
//! for it can be impossible. Falling back to name silently would leave a user
//! looking at an order they did not choose and cannot account for.

use super::listing::Entry;
use std::cmp::Ordering;

/// What the entries are sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Name,
    Size,
    Created,
    Modified,
}

impl Order {
    /// For the header.
    pub fn label(self) -> &'static str {
        match self {
            Order::Name => "name",
            Order::Size => "size",
            Order::Created => "created",
            Order::Modified => "modified",
        }
    }
}

/// What actually happened, which is not always what was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub order: Order,
    /// The requested order was impossible here, and `order` is the substitute.
    pub fell_back: bool,
}

/// Sort in place, and say what order came out.
///
/// `Created` falls back to `Name` when **no** entry has a creation time: on such
/// a platform every comparison would be between two absences, so the result
/// would be arbitrary rather than sorted. One entry having it is enough to
/// proceed — the rest sort to the end, which is where "unknown" belongs.
pub fn sort(entries: &mut [Entry], wanted: Order, reverse: bool) -> Applied {
    let possible = match wanted {
        Order::Created if entries.iter().all(|entry| entry.created.is_none()) => Order::Name,
        other => other,
    };

    entries.sort_by(|a, b| {
        // Directories first, whichever way the rest is going. `sort_by` is
        // stable, so equal keys keep the order `read_dir` gave them.
        match b.is_dir.cmp(&a.is_dir) {
            Ordering::Equal => {}
            kind => return kind,
        }

        let by_name = flip(a.name.cmp(&b.name), reverse);
        match key(a, b, possible, reverse) {
            // `Equal` on the key falls through to the name, which is what makes
            // two files of the same size sit in a readable order.
            Some(keyed) => keyed.then(by_name),
            None => by_name,
        }
    });

    Applied {
        order: possible,
        fell_back: possible != wanted,
    }
}

/// Compare on the chosen key, or `None` when neither entry has one.
///
/// **`reverse` applies only between two known values.** An unknown sorts last in
/// both directions: it is not "small", and letting the flip float unknowns to the
/// top would put the least informative rows where the eye goes first. Reversing
/// the whole comparison — the obvious way to write this — does exactly that.
fn key(a: &Entry, b: &Entry, order: Order, reverse: bool) -> Option<Ordering> {
    match order {
        Order::Name => None,
        Order::Size => option_cmp(a.size, b.size, reverse),
        Order::Created => option_cmp(a.created, b.created, reverse),
        Order::Modified => option_cmp(a.modified, b.modified, reverse),
    }
}

fn option_cmp<T: Ord>(a: Option<T>, b: Option<T>, reverse: bool) -> Option<Ordering> {
    match (a, b) {
        (Some(a), Some(b)) => Some(flip(a.cmp(&b), reverse)),
        (Some(_), None) => Some(Ordering::Less),
        (None, Some(_)) => Some(Ordering::Greater),
        // No opinion: the caller falls back to the name.
        (None, None) => None,
    }
}

fn flip(ordering: Ordering, reverse: bool) -> Ordering {
    if reverse {
        ordering.reverse()
    } else {
        ordering
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::time::{Duration, SystemTime};

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn file(name: &str, size: u64) -> Entry {
        Entry {
            name: OsString::from(name),
            is_dir: false,
            size: Some(size),
            modified: Some(at(1_000)),
            created: Some(at(1_000)),
        }
    }

    fn dir(name: &str) -> Entry {
        Entry {
            name: OsString::from(name),
            is_dir: true,
            size: None,
            modified: Some(at(1_000)),
            created: Some(at(1_000)),
        }
    }

    fn names(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| entry.name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn by_name_ascending_and_descending() {
        let mut entries = vec![file("c", 1), file("a", 1), file("b", 1)];

        assert_eq!(sort(&mut entries, Order::Name, false).order, Order::Name);
        assert_eq!(names(&entries), ["a", "b", "c"]);

        sort(&mut entries, Order::Name, true);
        assert_eq!(names(&entries), ["c", "b", "a"]);
    }

    #[test]
    fn by_size_puts_the_biggest_last_until_reversed() {
        let mut entries = vec![file("big", 900), file("small", 10), file("mid", 100)];

        sort(&mut entries, Order::Size, false);
        assert_eq!(names(&entries), ["small", "mid", "big"]);

        sort(&mut entries, Order::Size, true);
        assert_eq!(names(&entries), ["big", "mid", "small"]);
    }

    #[test]
    fn by_each_timestamp() {
        let mut entries = vec![
            Entry {
                created: Some(at(300)),
                modified: Some(at(100)),
                ..file("older-created", 1)
            },
            Entry {
                created: Some(at(100)),
                modified: Some(at(300)),
                ..file("newer-created", 1)
            },
        ];

        sort(&mut entries, Order::Created, false);
        assert_eq!(names(&entries), ["newer-created", "older-created"]);

        sort(&mut entries, Order::Modified, false);
        assert_eq!(names(&entries), ["older-created", "newer-created"]);
    }

    /// The platform keeps no birth times. Every comparison would be between two
    /// absences, so the result would be arbitrary — name is the honest answer,
    /// and saying so is the rest of it.
    #[test]
    fn creation_time_falls_back_to_name_when_nothing_has_one() {
        let mut entries = vec![
            Entry {
                created: None,
                ..file("b", 1)
            },
            Entry {
                created: None,
                ..file("a", 1)
            },
        ];

        let applied = sort(&mut entries, Order::Created, false);

        assert_eq!(applied.order, Order::Name);
        assert!(applied.fell_back, "and the header has to say so");
        assert_eq!(names(&entries), ["a", "b"]);
    }

    /// One entry with a birth time is enough to sort by it. The rest go to the
    /// end, which is where unknown belongs.
    #[test]
    fn one_entry_with_a_birth_time_is_enough() {
        let mut entries = vec![
            Entry {
                created: None,
                ..file("unknown", 1)
            },
            Entry {
                created: Some(at(500)),
                ..file("known", 1)
            },
        ];

        let applied = sort(&mut entries, Order::Created, false);

        assert_eq!(applied.order, Order::Created);
        assert!(!applied.fell_back);
        assert_eq!(names(&entries), ["known", "unknown"]);
    }

    /// Unknown is not "small". Reversing the order must not float the least
    /// informative rows to where the eye goes first.
    #[test]
    fn an_unknown_key_sorts_last_in_both_directions() {
        let mut entries = vec![
            Entry {
                size: None,
                ..file("unknown", 0)
            },
            file("known", 500),
        ];

        sort(&mut entries, Order::Size, false);
        assert_eq!(names(&entries), ["known", "unknown"]);

        sort(&mut entries, Order::Size, true);
        assert_eq!(
            names(&entries),
            ["known", "unknown"],
            "reversing the sizes must not reorder what has none"
        );
    }

    /// `mc`'s habit, and the reason it is a habit: reversing "by name" is a
    /// statement about names, not an invitation to interleave two kinds of thing.
    #[test]
    fn directories_stay_above_files_whichever_way_the_rest_goes() {
        let mut entries = vec![
            file("a-file", 1),
            dir("z-dir"),
            file("z-file", 1),
            dir("a-dir"),
        ];

        for reverse in [false, true] {
            sort(&mut entries, Order::Name, reverse);
            let (dirs, files): (Vec<_>, Vec<_>) = entries.iter().partition(|e| e.is_dir);
            assert_eq!(dirs.len(), 2);
            assert_eq!(
                names(&entries)[..2].to_vec(),
                names(&dirs.into_iter().cloned().collect::<Vec<_>>()),
                "reverse = {reverse}"
            );
            assert_eq!(files.len(), 2);
        }
    }

    /// Directories have no size until they are measured, so sorting by size has
    /// to leave them somewhere sensible rather than collapse them together at
    /// random.
    #[test]
    fn sorting_by_size_keeps_unmeasured_directories_in_name_order() {
        let mut entries = vec![dir("z"), dir("a"), file("f", 10)];

        sort(&mut entries, Order::Size, false);

        assert_eq!(names(&entries), ["a", "z", "f"]);
    }

    #[test]
    fn sorting_nothing_is_not_an_error() {
        let mut entries: Vec<Entry> = Vec::new();

        let applied = sort(&mut entries, Order::Created, false);

        assert_eq!(
            applied.order,
            Order::Name,
            "an empty listing has no birth times, so the rule applies unchanged"
        );
    }
}
