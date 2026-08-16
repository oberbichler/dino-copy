use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// A symlink. `target` is the (non-dereferenced) link target.
    Symlink { target: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub kind: EntryKind,
    pub size: u64,
    /// mtime in whole seconds since the epoch.
    pub mtime: i64,
    /// Unix permissions (mode & 0o7777).
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Remove a blocking target of the wrong type (runs before Create/Copy).
    RemoveConflict(PathBuf),
    CreateDir(PathBuf),
    Copy { rel: PathBuf, size: u64 },
    /// Create symlink `rel` pointing at `target` (fresh). `mtime` is applied.
    CreateSymlink { rel: PathBuf, target: PathBuf, mtime: i64 },
    DeleteFile(PathBuf),
    DeleteDir(PathBuf),
    /// Apply a directory's mtime + permissions from the source
    /// (runs at the very end, deepest first).
    SetDirMeta { rel: PathBuf, mtime: i64, mode: u32 },
    /// Align the permissions of a file whose contents are unchanged. chmod on
    /// the source only changes the ctime, not the mtime — without this action
    /// pure permission drift would never be mirrored.
    SetFileMeta { rel: PathBuf, mode: u32 },
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub copy_count: u64,
    pub copy_bytes: u64,
    pub delete_count: u64,
    pub mkdir_count: u64,
    pub skip_count: u64,
    /// Files whose contents are unchanged but whose permissions get aligned
    /// (SetFileMeta).
    pub meta_count: u64,
}

/// Compares source against dest entries and produces the action list + stats.
/// `mtime_tol` is the permitted tolerance in seconds.
/// Action order: CreateDir first (ascending), then Copy, then the deletes
/// (files before directories, directories descending by depth).
/// Convenience variant without forced copies (only used by tests; the
/// production path always goes through [`diff_with_forced`]).
#[cfg(test)]
pub fn diff(
    source: &BTreeMap<PathBuf, FileEntry>,
    dest: &BTreeMap<PathBuf, FileEntry>,
    mtime_tol: i64,
) -> (Vec<Action>, Stats) {
    diff_with_forced(source, dest, mtime_tol, &std::collections::BTreeSet::new())
}

/// Like [`diff`], but files in `forced` are copied even when they count as
/// unchanged (e.g. because `--checksum` found a mismatch). This makes forced
/// copies run through the same logic as normal ones (statistics, ordering,
/// SetDirMeta for touched directories).
pub fn diff_with_forced(
    source: &BTreeMap<PathBuf, FileEntry>,
    dest: &BTreeMap<PathBuf, FileEntry>,
    mtime_tol: i64,
    forced: &std::collections::BTreeSet<PathBuf>,
) -> (Vec<Action>, Stats) {
    let mut stats = Stats::default();
    // A target entry "blocks" when it exists but has a different type than the
    // source entry: it must be removed before Create/Copy.
    let mut conflicts: Vec<PathBuf> = Vec::new();
    let mut mkdirs: Vec<PathBuf> = Vec::new();
    let mut copies: Vec<Action> = Vec::new();
    let mut symlinks: Vec<Action> = Vec::new();
    let mut file_metas: Vec<Action> = Vec::new();
    // All source directories with a flag for whether their metadata already
    // matches on the target. SetDirMeta is only emitted when the metadata
    // differs or an action changes the directory's contents (and thus its
    // mtime).
    let mut dirs: Vec<(PathBuf, i64, u32, bool)> = Vec::new();

    /// Compares only the type of two kinds (ignoring the symlink target).
    fn same_type(a: &EntryKind, b: &EntryKind) -> bool {
        matches!(
            (a, b),
            (EntryKind::File, EntryKind::File)
                | (EntryKind::Dir, EntryKind::Dir)
                | (EntryKind::Symlink { .. }, EntryKind::Symlink { .. })
        )
    }

    for (rel, s) in source {
        let dest_entry = dest.get(rel);
        // Wrong type on the target → remove it as a conflict.
        if let Some(d) = dest_entry {
            if !same_type(&s.kind, &d.kind) {
                conflicts.push(rel.clone());
                stats.delete_count += 1;
            }
        }
        let type_matches = dest_entry.map(|d| same_type(&s.kind, &d.kind)).unwrap_or(false);

        match &s.kind {
            EntryKind::Dir => {
                if !type_matches {
                    mkdirs.push(rel.clone());
                    stats.mkdir_count += 1;
                }
                // Metadata counts as in sync when the target dir exists and
                // mtime (within tolerance) + mode match.
                let meta_ok = type_matches
                    && dest_entry.is_some_and(|d| {
                        (s.mtime - d.mtime).abs() <= mtime_tol && s.mode == d.mode
                    });
                dirs.push((rel.clone(), s.mtime, s.mode, meta_ok));
            }
            EntryKind::File => match dest_entry {
                Some(d)
                    if type_matches && unchanged(s, d, mtime_tol) && !forced.contains(rel) =>
                {
                    stats.skip_count += 1;
                    // Contents unchanged but permissions differ (chmod only
                    // changes the ctime): align the metadata instead of copying.
                    if s.mode != d.mode {
                        file_metas.push(Action::SetFileMeta { rel: rel.clone(), mode: s.mode });
                        stats.meta_count += 1;
                    }
                }
                _ => {
                    copies.push(Action::Copy { rel: rel.clone(), size: s.size });
                    stats.copy_count += 1;
                    stats.copy_bytes += s.size;
                }
            },
            EntryKind::Symlink { target } => {
                // Equal = same target AND mtime within tolerance. Recreating is
                // cheap and sets the mtime along the way.
                let same_link = match dest_entry {
                    Some(d) => match &d.kind {
                        EntryKind::Symlink { target: dt } => {
                            dt == target && (s.mtime - d.mtime).abs() <= mtime_tol
                        }
                        _ => false,
                    },
                    None => false,
                };
                if !same_link {
                    symlinks.push(Action::CreateSymlink {
                        rel: rel.clone(),
                        target: target.clone(),
                        mtime: s.mtime,
                    });
                    stats.copy_count += 1;
                }
            }
        }
    }

    // CreateDir ascending (shallow before deep) so that parents come first.
    mkdirs.sort_by_key(|p| p.components().count());

    // Set of the conflict paths: RemoveConflict removes the entire subtree, so
    // descendants must not be deleted on top of that.
    let conflict_set: std::collections::BTreeSet<PathBuf> = conflicts.iter().cloned().collect();
    let under_conflict = |rel: &std::path::Path| {
        rel.ancestors().skip(1).any(|anc| conflict_set.contains(anc))
    };

    // Deletes: everything on dest that does not exist on source (as the same
    // type).
    let mut del_files: Vec<PathBuf> = Vec::new();
    let mut del_dirs: Vec<PathBuf> = Vec::new();
    for (rel, d) in dest {
        let keep = match source.get(rel) {
            Some(s) => same_type(&s.kind, &d.kind),
            None => false,
        };
        if keep {
            continue;
        }
        // Type conflicts are already covered by RemoveConflict.
        if source.contains_key(rel) {
            continue;
        }
        // Skip descendants of a removed conflict subtree.
        if under_conflict(rel) {
            continue;
        }
        match d.kind {
            EntryKind::File | EntryKind::Symlink { .. } => {
                del_files.push(rel.clone());
                stats.delete_count += 1;
            }
            EntryKind::Dir => {
                del_dirs.push(rel.clone());
                stats.delete_count += 1;
            }
        }
    }
    // Directories deepest first.
    del_dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));

    // Directories whose contents are changed by an action: that changes their
    // mtime, so the metadata has to be reset afterwards. What matters is the
    // direct parent of the changed entry.
    let mut touched: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut touch_parent = |rel: &std::path::Path| {
        if let Some(parent) = rel.parent() {
            if !parent.as_os_str().is_empty() {
                touched.insert(parent.to_path_buf());
            }
        }
    };
    for rel in conflicts.iter().chain(&mkdirs).chain(&del_files).chain(&del_dirs) {
        touch_parent(rel);
    }
    for a in copies.iter().chain(&symlinks) {
        match a {
            Action::Copy { rel, .. } | Action::CreateSymlink { rel, .. } => touch_parent(rel),
            _ => {}
        }
    }

    // Dir metadata deepest first (child writes would otherwise change the
    // parent's mtime).
    let mut set_dir_meta: Vec<Action> = dirs
        .into_iter()
        .filter(|(rel, _, _, meta_ok)| !meta_ok || touched.contains(rel))
        .map(|(rel, mtime, mode, _)| Action::SetDirMeta { rel, mtime, mode })
        .collect();
    set_dir_meta.sort_by_key(|a| match a {
        Action::SetDirMeta { rel, .. } => std::cmp::Reverse(rel.components().count()),
        _ => std::cmp::Reverse(0),
    });

    let mut actions = Vec::new();
    // 1. Remove conflicts before targets of the same name are created/copied.
    actions.extend(conflicts.into_iter().map(Action::RemoveConflict));
    actions.extend(mkdirs.into_iter().map(Action::CreateDir));
    actions.extend(copies);
    actions.extend(symlinks);
    // Permission alignment of unchanged files (changes no dir mtimes).
    actions.extend(file_metas);
    actions.extend(del_files.into_iter().map(Action::DeleteFile));
    actions.extend(del_dirs.into_iter().map(Action::DeleteDir));
    // Last: directory metadata (deepest first).
    actions.extend(set_dir_meta);
    (actions, stats)
}

/// True when the dest file counts as unchanged (same size, mtime within
/// tolerance).
pub fn unchanged(s: &FileEntry, d: &FileEntry, mtime_tol: i64) -> bool {
    s.size == d.size && (s.mtime - d.mtime).abs() <= mtime_tol
}

/// blake3 hash of a file.
///
/// Returns the hash as a `blake3::Hash` (32 bytes on the stack, with
/// `PartialEq`), not as a hex string: with `--checksum` over millions of files
/// a 64-character allocation per file would be pure waste.
pub fn hash_file(path: &std::path::Path) -> anyhow::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The helpers return (relative path, entry): the path is the map key and
    /// therefore no longer lives in the FileEntry itself.
    fn file(rel: &str, size: u64, mtime: i64) -> (PathBuf, FileEntry) {
        (PathBuf::from(rel), FileEntry { kind: EntryKind::File, size, mtime, mode: 0o644 })
    }
    fn dir(rel: &str) -> (PathBuf, FileEntry) {
        (PathBuf::from(rel), FileEntry { kind: EntryKind::Dir, size: 0, mtime: 0, mode: 0o755 })
    }
    fn symlink(rel: &str, target: &str) -> (PathBuf, FileEntry) {
        (
            PathBuf::from(rel),
            FileEntry {
                kind: EntryKind::Symlink { target: PathBuf::from(target) },
                size: 0,
                mtime: 0,
                mode: 0o777,
            },
        )
    }
    fn map(entries: Vec<(PathBuf, FileEntry)>) -> BTreeMap<PathBuf, FileEntry> {
        entries.into_iter().collect()
    }

    /// Index of an action in the list (for ordering assertions).
    fn pos(actions: &[Action], pred: impl Fn(&Action) -> bool) -> Option<usize> {
        actions.iter().position(pred)
    }

    #[test]
    fn copies_new_file() {
        let s = map(vec![file("a.txt", 10, 100)]);
        let d = map(vec![]);
        let (actions, stats) = diff(&s, &d, 1);
        assert!(actions.contains(&Action::Copy { rel: PathBuf::from("a.txt"), size: 10 }));
        assert_eq!(stats.copy_count, 1);
        assert_eq!(stats.copy_bytes, 10);
    }

    #[test]
    fn skips_unchanged_file() {
        let s = map(vec![file("a.txt", 10, 100)]);
        let d = map(vec![file("a.txt", 10, 100)]);
        let (actions, stats) = diff(&s, &d, 1);
        assert!(!actions.iter().any(|a| matches!(a, Action::Copy { .. })));
        assert_eq!(stats.skip_count, 1);
    }

    #[test]
    fn copies_changed_size() {
        let s = map(vec![file("a.txt", 20, 100)]);
        let d = map(vec![file("a.txt", 10, 100)]);
        let (actions, _) = diff(&s, &d, 1);
        assert!(actions.contains(&Action::Copy { rel: PathBuf::from("a.txt"), size: 20 }));
    }

    #[test]
    fn mtime_within_tolerance_skips() {
        let s = map(vec![file("a.txt", 10, 100)]);
        let d = map(vec![file("a.txt", 10, 101)]);
        let (_, stats) = diff(&s, &d, 1);
        assert_eq!(stats.skip_count, 1);
    }

    #[test]
    fn mtime_beyond_tolerance_copies() {
        let s = map(vec![file("a.txt", 10, 100)]);
        let d = map(vec![file("a.txt", 10, 103)]);
        let (_, stats) = diff(&s, &d, 1);
        assert_eq!(stats.copy_count, 1);
    }

    #[test]
    fn mode_only_change_emits_set_file_meta_not_copy() {
        // chmod on the source only changes the ctime: size+mtime stay the same.
        // The drift has to be aligned via SetFileMeta, not by copying.
        let mut sf = file("a.txt", 10, 100);
        sf.1.mode = 0o600;
        let df = file("a.txt", 10, 100); // helper: mode 0o644
        let s = map(vec![sf]);
        let d = map(vec![df]);
        let (actions, stats) = diff(&s, &d, 1);
        assert!(!actions.iter().any(|a| matches!(a, Action::Copy { .. })));
        assert!(actions.contains(&Action::SetFileMeta {
            rel: PathBuf::from("a.txt"),
            mode: 0o600,
        }));
        assert_eq!(stats.meta_count, 1);
        assert_eq!(stats.skip_count, 1, "the file still counts as not copied");
    }

    #[test]
    fn equal_mode_emits_no_set_file_meta() {
        let s = map(vec![file("a.txt", 10, 100)]);
        let d = map(vec![file("a.txt", 10, 100)]);
        let (actions, stats) = diff(&s, &d, 1);
        assert!(actions.is_empty(), "a no-op run must produce no actions: {actions:?}");
        assert_eq!(stats.meta_count, 0);
    }

    #[test]
    fn deletes_extra_file_on_dest() {
        let s = map(vec![]);
        let d = map(vec![file("old.txt", 5, 100)]);
        let (actions, stats) = diff(&s, &d, 1);
        assert!(actions.contains(&Action::DeleteFile(PathBuf::from("old.txt"))));
        assert_eq!(stats.delete_count, 1);
    }

    #[test]
    fn creates_missing_dir() {
        let s = map(vec![dir("sub")]);
        let d = map(vec![]);
        let (actions, stats) = diff(&s, &d, 1);
        assert!(actions.contains(&Action::CreateDir(PathBuf::from("sub"))));
        assert_eq!(stats.mkdir_count, 1);
    }

    #[test]
    fn deletes_extra_dir_deepest_first() {
        let s = map(vec![]);
        let d = map(vec![dir("a"), dir("a/b")]);
        let (actions, _) = diff(&s, &d, 1);
        let del: Vec<&Action> = actions.iter().filter(|a| matches!(a, Action::DeleteDir(_))).collect();
        assert_eq!(del[0], &Action::DeleteDir(PathBuf::from("a/b")));
        assert_eq!(del[1], &Action::DeleteDir(PathBuf::from("a")));
    }

    #[test]
    fn file_on_source_dir_on_dest_removes_conflict_before_copy() {
        // A has file "x", B has directory "x".
        let s = map(vec![file("x", 3, 100)]);
        let d = map(vec![dir("x")]);
        let (actions, _) = diff(&s, &d, 1);
        let rm = pos(&actions, |a| a == &Action::RemoveConflict(PathBuf::from("x")));
        let cp = pos(&actions, |a| matches!(a, Action::Copy { rel, .. } if rel == &PathBuf::from("x")));
        assert!(rm.is_some(), "expected RemoveConflict for x");
        assert!(cp.is_some(), "expected Copy for x");
        assert!(rm.unwrap() < cp.unwrap(), "RemoveConflict must run before Copy");
        // There must be NO DeleteDir(x) (conflict instead of a normal delete).
        assert!(!actions.contains(&Action::DeleteDir(PathBuf::from("x"))));
    }

    #[test]
    fn dir_on_source_file_on_dest_removes_conflict_before_mkdir() {
        // A has directory "y", B has file "y".
        let s = map(vec![dir("y")]);
        let d = map(vec![file("y", 3, 100)]);
        let (actions, _) = diff(&s, &d, 1);
        let rm = pos(&actions, |a| a == &Action::RemoveConflict(PathBuf::from("y")));
        let mk = pos(&actions, |a| a == &Action::CreateDir(PathBuf::from("y")));
        assert!(rm.is_some() && mk.is_some());
        assert!(rm.unwrap() < mk.unwrap(), "RemoveConflict must run before CreateDir");
    }

    #[test]
    fn new_symlink_is_created() {
        let s = map(vec![symlink("link", "target")]);
        let d = map(vec![]);
        let (actions, stats) = diff(&s, &d, 1);
        assert!(actions.contains(&Action::CreateSymlink {
            rel: PathBuf::from("link"),
            target: PathBuf::from("target"),
            mtime: 0,
        }));
        assert_eq!(stats.copy_count, 1);
    }

    #[test]
    fn identical_symlink_is_skipped() {
        let s = map(vec![symlink("link", "target")]);
        let d = map(vec![symlink("link", "target")]);
        let (actions, _) = diff(&s, &d, 1);
        assert!(!actions.iter().any(|a| matches!(a, Action::CreateSymlink { .. })));
    }

    #[test]
    fn symlink_mtime_drift_is_recreated() {
        // Same link target, but mtime outside the tolerance -> recreate it
        // (recreating applies the source mtime).
        let mut sl = symlink("link", "target");
        sl.1.mtime = 100;
        let mut dl = symlink("link", "target");
        dl.1.mtime = 500;
        let s = map(vec![sl]);
        let d = map(vec![dl]);
        let (actions, _) = diff(&s, &d, 1);
        assert!(actions.contains(&Action::CreateSymlink {
            rel: PathBuf::from("link"),
            target: PathBuf::from("target"),
            mtime: 100,
        }));
    }

    #[test]
    fn symlink_mtime_within_tolerance_is_skipped() {
        let mut sl = symlink("link", "target");
        sl.1.mtime = 100;
        let mut dl = symlink("link", "target");
        dl.1.mtime = 101;
        let s = map(vec![sl]);
        let d = map(vec![dl]);
        let (actions, _) = diff(&s, &d, 1);
        assert!(actions.is_empty(), "within the tolerance: no action, was {actions:?}");
    }

    #[test]
    fn changed_symlink_target_is_recreated() {
        let s = map(vec![symlink("link", "new")]);
        let d = map(vec![symlink("link", "old")]);
        let (actions, _) = diff(&s, &d, 1);
        assert!(actions.contains(&Action::CreateSymlink {
            rel: PathBuf::from("link"),
            target: PathBuf::from("new"),
            mtime: 0,
        }));
    }

    #[test]
    fn dir_meta_emitted_deepest_first_and_last() {
        let mut a_root = dir("a");
        a_root.1.mtime = 500;
        a_root.1.mode = 0o700;
        let mut a_b = dir("a/b");
        a_b.1.mtime = 600;
        a_b.1.mode = 0o750;
        let s = map(vec![a_root, a_b]);
        let d = map(vec![]);
        let (actions, _) = diff(&s, &d, 1);

        let metas: Vec<&Action> = actions
            .iter()
            .filter(|a| matches!(a, Action::SetDirMeta { .. }))
            .collect();
        assert_eq!(metas.len(), 2);
        // Deepest first.
        assert_eq!(
            metas[0],
            &Action::SetDirMeta { rel: PathBuf::from("a/b"), mtime: 600, mode: 0o750 }
        );
        assert_eq!(
            metas[1],
            &Action::SetDirMeta { rel: PathBuf::from("a"), mtime: 500, mode: 0o700 }
        );
        // SetDirMeta runs after CreateDir.
        let last_mkdir = actions.iter().rposition(|a| matches!(a, Action::CreateDir(_))).unwrap();
        let first_meta = pos(&actions, |a| matches!(a, Action::SetDirMeta { .. })).unwrap();
        assert!(last_mkdir < first_meta);
    }

    #[test]
    fn identical_trees_with_subdirs_produce_no_actions() {
        // A no-op run must produce no actions - otherwise the "nothing to do"
        // path is unreachable and dir metadata gets rewritten needlessly on
        // every run (writes on HDDs).
        let mut sd = dir("sub");
        sd.1.mtime = 500;
        sd.1.mode = 0o755;
        let s = map(vec![sd.clone(), file("sub/a.txt", 10, 100)]);
        let d = map(vec![sd, file("sub/a.txt", 10, 100)]);
        let (actions, _) = diff(&s, &d, 1);
        assert!(actions.is_empty(), "expected no actions, was: {actions:?}");
    }

    #[test]
    fn dir_meta_emitted_when_meta_differs() {
        let mut src_dir = dir("sub");
        src_dir.1.mtime = 500;
        src_dir.1.mode = 0o700;
        let mut dst_dir = dir("sub");
        dst_dir.1.mtime = 999; // differs
        dst_dir.1.mode = 0o755; // differs
        let s = map(vec![src_dir]);
        let d = map(vec![dst_dir]);
        let (actions, _) = diff(&s, &d, 1);
        assert!(actions.contains(&Action::SetDirMeta {
            rel: PathBuf::from("sub"),
            mtime: 500,
            mode: 0o700,
        }));
    }

    #[test]
    fn dir_meta_emitted_when_copy_touches_dir() {
        // A copy into "sub" changes its mtime -> the metadata has to be reset
        // afterwards, even though it currently matches.
        let mut sd = dir("sub");
        sd.1.mtime = 500;
        let s = map(vec![sd.clone(), file("sub/new.txt", 10, 100)]);
        let d = map(vec![sd]);
        let (actions, _) = diff(&s, &d, 1);
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::SetDirMeta { rel, .. } if rel == &PathBuf::from("sub")
            )),
            "expected SetDirMeta for sub, was: {actions:?}"
        );
    }

    #[test]
    fn dir_meta_emitted_when_delete_touches_dir() {
        let mut sd = dir("sub");
        sd.1.mtime = 500;
        let s = map(vec![sd.clone()]);
        let d = map(vec![sd, file("sub/stale.txt", 10, 100)]);
        let (actions, _) = diff(&s, &d, 1);
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::SetDirMeta { rel, .. } if rel == &PathBuf::from("sub")
            )),
            "expected SetDirMeta for sub, was: {actions:?}"
        );
    }

    #[test]
    fn forced_copy_emits_set_dir_meta_for_parent() {
        let mut sd = dir("sub");
        sd.1.mtime = 500;
        let s = map(vec![sd.clone(), file("sub/a.txt", 10, 100)]);
        let d = map(vec![sd, file("sub/a.txt", 10, 100)]);
        let forced: std::collections::BTreeSet<PathBuf> =
            [PathBuf::from("sub/a.txt")].into_iter().collect();
        let (actions, stats) = diff_with_forced(&s, &d, 1, &forced);
        assert_eq!(stats.copy_count, 1, "expected a forced copy: {actions:?}");
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::SetDirMeta { rel, .. } if rel == &PathBuf::from("sub")
            )),
            "expected SetDirMeta for sub: {actions:?}"
        );
    }

    #[test]
    fn file_hashes_differ_detected() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::File::create(&a).unwrap().write_all(b"aaaa").unwrap();
        std::fs::File::create(&b).unwrap().write_all(b"bbbb").unwrap();
        assert_ne!(hash_file(&a).unwrap(), hash_file(&b).unwrap());
        assert_eq!(hash_file(&a).unwrap(), hash_file(&a).unwrap());
    }
}
