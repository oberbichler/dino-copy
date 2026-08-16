use std::path::Path;

/// Exclusion filter for scan entries.
///
/// Applied to BOTH source and target: an excluded target entry is invisible to
/// dino-copy and is therefore neither overwritten nor deleted as extraneous.
/// Without that symmetry, mirroring two macOS volume roots would be impossible
/// — each disk has its own `.Spotlight-V100`/`.fseventsd`, and `.Trashes`
/// cannot even be deleted.
///
/// Pattern kinds (deliberately reduced rsync semantics):
/// - without `/`: matches the file name at any depth (`.DS_Store`, `*.tmp`)
/// - with `/`: matches the full relative path, and `*` does not cross a
///   directory boundary (`sub/*.txt`)
#[derive(Debug, Clone, Default)]
pub struct Filter {
    names: Option<globset::GlobSet>,
    paths: Option<globset::GlobSet>,
}

impl Filter {
    pub fn new(patterns: &[String]) -> anyhow::Result<Self> {
        let mut names = globset::GlobSetBuilder::new();
        let mut paths = globset::GlobSetBuilder::new();
        let (mut has_names, mut has_paths) = (false, false);
        for p in patterns {
            // literal_separator: in path patterns '*' must not skip over '/',
            // otherwise "sub/*" would also match "sub/deep/inside".
            let glob = globset::GlobBuilder::new(p)
                .literal_separator(true)
                .build()
                .map_err(|e| anyhow::anyhow!("Invalid --exclude pattern {p:?}: {e}"))?;
            if p.contains('/') {
                paths.add(glob);
                has_paths = true;
            } else {
                names.add(glob);
                has_names = true;
            }
        }
        Ok(Self {
            names: has_names.then(|| names.build()).transpose()?,
            paths: has_paths.then(|| paths.build()).transpose()?,
        })
    }

    /// True if no pattern is set at all. The scan then skips the filter
    /// callback entirely.
    pub fn is_empty(&self) -> bool {
        self.names.is_none() && self.paths.is_none()
    }

    /// True if `rel` (path relative to the root) is excluded.
    pub fn excludes(&self, rel: &Path) -> bool {
        if let Some(set) = &self.names {
            if let Some(name) = rel.file_name() {
                if set.is_match(Path::new(name)) {
                    return true;
                }
            }
        }
        self.paths.as_ref().is_some_and(|set| set.is_match(rel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn filter(patterns: &[&str]) -> Filter {
        Filter::new(&patterns.iter().map(|p| p.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn empty_filter_excludes_nothing() {
        let f = Filter::new(&[]).unwrap();
        assert!(f.is_empty());
        assert!(!f.excludes(Path::new("a.txt")));
    }

    #[test]
    fn name_pattern_matches_at_every_depth() {
        let f = filter(&["*.tmp"]);
        assert!(f.excludes(Path::new("x.tmp")));
        assert!(f.excludes(Path::new("sub/deep/x.tmp")));
        assert!(!f.excludes(Path::new("x.txt")));
    }

    #[test]
    fn name_pattern_matches_directories_too() {
        // Pruning the subtree is the walker's job; all that matters here is
        // that the directory entry itself counts as a match.
        let f = filter(&["cache"]);
        assert!(f.excludes(Path::new("cache")));
        assert!(f.excludes(Path::new("sub/cache")));
    }

    #[test]
    fn path_pattern_matches_the_full_relative_path_only() {
        let f = filter(&["sub/b.txt"]);
        assert!(f.excludes(Path::new("sub/b.txt")));
        assert!(!f.excludes(Path::new("other/b.txt")));
        assert!(!f.excludes(Path::new("b.txt")));
    }

    #[test]
    fn star_in_a_path_pattern_does_not_cross_separators() {
        let f = filter(&["sub/*.txt"]);
        assert!(f.excludes(Path::new("sub/direct.txt")));
        assert!(!f.excludes(Path::new("sub/deep/inside.txt")));
    }

    #[test]
    fn patterns_of_both_kinds_combine() {
        let f = filter(&[".DS_Store", "sub/b.txt"]);
        assert!(f.excludes(Path::new("deep/.DS_Store")));
        assert!(f.excludes(Path::new("sub/b.txt")));
        assert!(!f.excludes(Path::new("a.txt")));
    }

    #[test]
    fn invalid_pattern_is_rejected_with_the_pattern_in_the_message() {
        let err = Filter::new(&["[".to_string()]).unwrap_err().to_string();
        assert!(err.contains("Invalid --exclude pattern"), "{err}");
        assert!(err.contains('['), "{err}");
    }
}
