use std::path::PathBuf;

/// Everything a scan needs to know, supplied by the frontend.
///
/// The core reads no config files and consults no environment: whatever the
/// user configured has already been resolved into this struct by the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanOptions {
    /// Where to start. Always explicit — the CLI never falls back to the
    /// working directory, so a bare `disk-tools` can't scan something huge by
    /// accident.
    pub root: PathBuf,

    /// Hide entries smaller than this many bytes.
    ///
    /// A *display* filter: parent totals still include what it hides.
    /// Filtering during aggregation would make the totals lie.
    pub min_size: u64,

    /// How deep to *print*, `None` for unlimited.
    ///
    /// Traversal always runs to the bottom regardless — same as
    /// `du --max-depth`, so a shown directory's total still covers its whole
    /// subtree.
    pub depth: Option<usize>,

    /// Rank and report apparent size rather than allocated size.
    pub apparent: bool,

    /// Stop at filesystem boundaries instead of descending into other mounts.
    pub one_file_system: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn options_default_values() {
        let opts = ScanOptions::default();

        assert_eq!(opts.min_size, 0);
        assert_eq!(opts.depth, None);
        assert!(!opts.apparent);
        assert!(!opts.one_file_system);
        assert_eq!(opts.root, PathBuf::new());
    }
}
