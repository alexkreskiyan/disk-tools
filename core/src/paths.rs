//! Comparing paths, carefully.
//!
//! These decide whether something is a deletion candidate — whether a directory
//! *is* a user cache, whether it sits under a never-touch root — so the two ways
//! of getting it wrong are not symmetric. Matching too little costs a directory
//! that could have been cleaned; matching too much costs one that should never
//! have been touched.
//!
//! Three rules follow from that, and all of them apply to every function here:
//!
//! - **Component-wise**, never string comparison, so `Path`'s own normalisation
//!   of separators and `.` components applies.
//! - **ASCII-case-insensitive on Windows only**, where the filesystem is.
//!   Elsewhere case is significant: APFS can be formatted either way and Linux
//!   is case-sensitive outright, so folding there would make two genuinely
//!   different directories compare equal.
//! - **Never [`Path::canonicalize`]** — a project-wide constraint. It touches
//!   the filesystem, and resolving symlinks would let a link point a rule at
//!   something the user never named.

use std::path::{Component, Path, PathBuf};

/// `path` with `.` dropped and `..` resolved against the component before it —
/// **lexically**, touching no filesystem.
///
/// This is not a stand-in for [`Path::canonicalize`] and does not try to be:
/// where a symlink sits in the path the two disagree, which is exactly why the
/// project bans the latter. It exists for one reason. `Path` does not resolve
/// `..` and cannot — whether `a/b/..` is `a` depends on whether `b` is a link —
/// so a denylist comparing components literally would let
/// `/home/me/../../System` walk straight past an entry for `System`.
///
/// Callers must check **both** forms rather than replacing the raw path with
/// this one. Normalising alone can *weaken* a check: `/System/../Users/x`
/// matches `System` literally and stops matching once resolved. A denylist may
/// only ever grow.
pub(crate) fn normalize_lexically(path: &Path) -> PathBuf {
    let mut resolved: Vec<Component<'_>> = Vec::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match resolved.last() {
                // Step back out of a real directory name.
                Some(Component::Normal(_)) => {
                    resolved.pop();
                }
                // `/..` is `/`: the root has no parent to reach.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                // A leading `..` in a relative path has nothing to cancel, so
                // it stays and the path stays relative — which `under_root`
                // then refuses outright.
                _ => resolved.push(component),
            },
            _ => resolved.push(component),
        }
    }

    resolved.iter().collect()
}

/// Is `path` `root` itself, or somewhere beneath it?
///
/// Component-wise, so `/home/mine` is **not** within `/home/min` — the trap a
/// `starts_with` on strings would fall into, and an expensive one when `root` is
/// a never-touch entry.
pub(crate) fn is_within(path: &Path, root: &Path) -> bool {
    let mut inner = path.components();
    for component in root.components() {
        match inner.next() {
            Some(c) if same_component(c, component) => {}
            _ => return false,
        }
    }
    true
}

/// Is `path` at, or under, `<filesystem root>/denied…`?
///
/// The denylist is expressed this way rather than as absolute paths so that
/// `Windows` catches a system installed on `D:` as well as on `C:` — a
/// hardcoded `C:\Windows` would quietly protect only the common case. On Unix
/// the anchor is simply `/`.
///
/// A relative path matches nothing: with no root to anchor to, "immediately
/// below the root" has no meaning, and guessing would be guessing about
/// deletion.
pub(crate) fn under_root(path: &Path, denied: &[&str]) -> bool {
    let mut components = path.components().peekable();

    // Step over the drive letter and the leading separator, which precede the
    // part being matched; on Unix only the latter exists.
    let mut rooted = false;
    while matches!(
        components.peek(),
        Some(Component::Prefix(_) | Component::RootDir)
    ) {
        rooted = true;
        components.next();
    }
    if !rooted {
        return false;
    }

    for name in denied {
        match components.next() {
            Some(Component::Normal(component)) if eq_name(component, name) => {}
            _ => return false,
        }
    }
    true
}

fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
    eq_os(a.as_os_str(), b.as_os_str())
}

fn eq_name(component: &std::ffi::OsStr, name: &str) -> bool {
    eq_os(component, std::ffi::OsStr::new(name))
}

#[cfg(windows)]
fn eq_os(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    // ASCII-only rather than NTFS's full Unicode upcase tables, so a non-ASCII
    // name differing in case still misses. That is the direction to miss in.
    a.eq_ignore_ascii_case(b)
}

#[cfg(not(windows))]
fn eq_os(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Comparison is component-wise, so `Path`'s own handling of separators and
    /// `.` applies and a path spelled two ways still compares equal. String
    /// comparison would see two different things.
    #[test]
    fn comparison_ignores_separator_and_dot_noise() {
        assert!(is_within(
            &PathBuf::from("/a").join("./b"),
            Path::new("/a/b")
        ));
        assert!(!is_within(Path::new("/a/c"), Path::new("/a/b")));
    }

    /// The prefix trap: `/home/mine` shares a string prefix with `/home/min`
    /// but is not inside it. Getting this wrong on a denylist entry would let a
    /// neighbouring directory inherit protection — or, the other way round,
    /// deny something innocent.
    #[test]
    fn is_within_compares_whole_components() {
        assert!(is_within(Path::new("/home/min"), Path::new("/home/min")));
        assert!(is_within(
            Path::new("/home/min/deep"),
            Path::new("/home/min")
        ));
        assert!(!is_within(Path::new("/home/mine"), Path::new("/home/min")));
        assert!(!is_within(Path::new("/home"), Path::new("/home/min")));
    }

    #[test]
    fn under_root_anchors_to_the_filesystem_root() {
        assert!(under_root(Path::new("/System"), &["System"]));
        assert!(under_root(Path::new("/System/Library"), &["System"]));
        assert!(under_root(
            Path::new("/Library/Caches/x"),
            &["Library", "Caches"]
        ));

        // The same names deeper in the tree are ordinary directories.
        assert!(!under_root(Path::new("/home/me/System"), &["System"]));
        assert!(!under_root(
            Path::new("/home/me/Library/Caches"),
            &["Library", "Caches"]
        ));
        // A shorter path cannot contain the whole sequence.
        assert!(!under_root(Path::new("/Library"), &["Library", "Caches"]));
    }

    /// The gap `Path` leaves: it never resolves `..`, so a denylist comparing
    /// components literally would let a traversal walk straight through it.
    #[test]
    fn normalize_lexically_resolves_parent_components() {
        let cases = [
            ("/home/me/../../System", "/System"),
            ("/a/./b/../c", "/a/c"),
            // The root has no parent to climb to.
            ("/../System", "/System"),
            ("/a/../..", "/"),
            // Nothing to resolve: unchanged.
            ("/System/Library", "/System/Library"),
        ];

        for (input, expected) in cases {
            assert_eq!(
                normalize_lexically(Path::new(input)),
                PathBuf::from(expected),
                "{input}"
            );
        }
    }

    /// A leading `..` has nothing to cancel, so the path stays relative — and
    /// `under_root` refuses relative paths outright, which is the safe end.
    #[test]
    fn a_leading_parent_component_survives_normalization() {
        assert_eq!(
            normalize_lexically(Path::new("../System")),
            PathBuf::from("../System")
        );
        assert!(!under_root(
            &normalize_lexically(Path::new("../System")),
            &["System"]
        ));
    }

    /// A relative path has no root to anchor against, so it must match nothing
    /// rather than being treated as if it started at `/`.
    #[test]
    fn under_root_rejects_relative_paths() {
        assert!(!under_root(Path::new("System"), &["System"]));
        assert!(!under_root(Path::new("./System"), &["System"]));
        assert!(!under_root(Path::new("../System"), &["System"]));
    }

    #[cfg(windows)]
    #[test]
    fn windows_folds_case_and_ignores_the_drive_letter() {
        assert!(is_within(
            Path::new(r"C:\Users\Me\Deep"),
            Path::new(r"c:\users\me")
        ));
        // A system on D: must be protected exactly as one on C: is.
        assert!(under_root(Path::new(r"D:\Windows\System32"), &["Windows"]));
        assert!(under_root(
            Path::new(r"c:\program files"),
            &["Program Files"]
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn case_is_significant_off_windows() {
        assert!(!is_within(Path::new("/a/B/c"), Path::new("/a/b")));
        assert!(!under_root(Path::new("/system"), &["System"]));
    }
}
