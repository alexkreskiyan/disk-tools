use std::path::PathBuf;

/// One entry in the scanned tree — a file or a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
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
pub struct SkippedEntry {
    pub path: PathBuf,
    pub reason: SkipReason,
}

/// The result of a scan: a size-annotated tree plus whatever was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanTree {
    pub root: ScanNode,
    pub skipped: Vec<SkippedEntry>,
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
}
