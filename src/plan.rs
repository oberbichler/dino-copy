//! What to do with one directory: the data model of an entry and the
//! per-directory comparison ([`diff_dir`]) that the walk in [`crate::walk`]
//! applies to every directory of the tree.
//!
//! [`diff_dir`] is a pure function of two sorted listings — no I/O — so the
//! comparison rules are unit-tested here in isolation.

use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// A symlink. `target` is the (non-dereferenced) link target.
    Symlink { target: PathBuf },
}

/// Identity of a file system object: device and inode number.
///
/// Used to recognize when the target file system resolves a name to an entry
/// that is listed under a different spelling (see [`crate::walk`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

impl FileId {
    pub fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self { dev: meta.dev(), ino: meta.ino() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub kind: EntryKind,
    pub size: u64,
    /// mtime in whole seconds since the epoch.
    pub mtime: i64,
    /// Unix permissions (mode & 0o7777).
    pub mode: u32,
    pub id: FileId,
}

/// The entries of ONE directory, sorted by name (byte order). The walker only
/// ever holds the listings of the directories it is currently processing, so
/// memory no longer grows with the size of the whole tree.
pub type Listing = Vec<(OsString, FileEntry)>;

/// What has to happen to one child of a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Copy the file contents (new on the target, or size/mtime differ).
    Copy { size: u64 },
    /// File counts as unchanged (size + mtime within tolerance). `mode` is set
    /// when only the permissions differ: chmod on the source only changes the
    /// ctime, not the mtime — without this pure permission drift would never
    /// be mirrored. `--checksum` may still turn this into a copy.
    Same { size: u64, mode: Option<u32> },
    /// (Re)create a symlink. Recreating is cheap and applies the mtime.
    Symlink { target: PathBuf, mtime: i64 },
    /// A source directory. `create` when it is missing on the target (or was
    /// in the way as a different type); `meta_ok` when the target directory's
    /// mtime (within tolerance) and mode already match.
    Dir { mtime: i64, mode: u32, create: bool, meta_ok: bool },
    /// Exists only on the target: remove it (a directory with its subtree).
    Remove { is_dir: bool },
    /// Symlink with the same target and mtime: nothing to do.
    Keep,
}

/// One child of a directory together with its operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub name: OsString,
    /// The target holds an entry of a different type under this name. It has
    /// to be removed (with its subtree) before `op` runs.
    pub conflict: bool,
    pub op: Op,
}

impl Child {
    /// True when the operation changes the contents of the parent directory
    /// on the target — and with it the parent's mtime, which then has to be
    /// reset afterwards. Permission changes (Same with mode) do not count.
    pub fn touches_parent(&self) -> bool {
        self.conflict
            || matches!(
                self.op,
                Op::Copy { .. } | Op::Symlink { .. } | Op::Remove { .. } | Op::Dir { create: true, .. }
            )
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    /// Copied files plus (re)created symlinks.
    pub copy_count: u64,
    pub copy_bytes: u64,
    /// Target entries of the wrong type that get replaced.
    pub conflict_count: u64,
    /// Target entries without a source counterpart. A directory counts with
    /// every entry of its subtree.
    pub remove_count: u64,
    pub mkdir_count: u64,
    pub skip_count: u64,
    /// Files whose contents are unchanged but whose permissions get aligned.
    pub meta_count: u64,
    /// Directories whose mtime/mode get (re)applied.
    pub dir_meta_count: u64,
    /// Target entries renamed to the source's spelling (case-only changes on a
    /// case-insensitive target).
    pub rename_count: u64,
}

impl Stats {
    /// Everything that deletes something on the target.
    pub fn delete_count(&self) -> u64 {
        self.conflict_count + self.remove_count
    }

    /// True when the run would not change anything on the target.
    pub fn is_noop(&self) -> bool {
        self.copy_count == 0
            && self.delete_count() == 0
            && self.mkdir_count == 0
            && self.meta_count == 0
            && self.dir_meta_count == 0
            && self.rename_count == 0
    }

    pub fn add(&mut self, o: &Stats) {
        self.copy_count += o.copy_count;
        self.copy_bytes += o.copy_bytes;
        self.conflict_count += o.conflict_count;
        self.remove_count += o.remove_count;
        self.mkdir_count += o.mkdir_count;
        self.skip_count += o.skip_count;
        self.meta_count += o.meta_count;
        self.dir_meta_count += o.dir_meta_count;
        self.rename_count += o.rename_count;
    }
}

/// Compares the listings of one directory (source vs. target) by a merge join
/// over the sorted names. Both listings are consumed.
pub fn diff_dir(src: Listing, dst: Listing, mtime_tol: i64) -> Vec<Child> {
    let mut out = Vec::with_capacity(src.len().max(dst.len()));
    let mut s_it = src.into_iter().peekable();
    let mut d_it = dst.into_iter().peekable();
    loop {
        let order = match (s_it.peek(), d_it.peek()) {
            (None, None) => break,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some((sn, _)), Some((dn, _))) => sn.cmp(dn),
        };
        match order {
            std::cmp::Ordering::Less => {
                let (name, s) = s_it.next().unwrap();
                out.push(Child { name, conflict: false, op: op_for(s, None, mtime_tol) });
            }
            std::cmp::Ordering::Greater => {
                let (name, d) = d_it.next().unwrap();
                let op = Op::Remove { is_dir: d.kind == EntryKind::Dir };
                out.push(Child { name, conflict: false, op });
            }
            std::cmp::Ordering::Equal => {
                let (name, s) = s_it.next().unwrap();
                let (_, d) = d_it.next().unwrap();
                if same_type(&s.kind, &d.kind) {
                    out.push(Child { name, conflict: false, op: op_for(s, Some(&d), mtime_tol) });
                } else {
                    // Wrong type on the target: remove it, then create as new.
                    out.push(Child { name, conflict: true, op: op_for(s, None, mtime_tol) });
                }
            }
        }
    }
    out
}

/// Compares only the type of two kinds (ignoring the symlink target).
fn same_type(a: &EntryKind, b: &EntryKind) -> bool {
    matches!(
        (a, b),
        (EntryKind::File, EntryKind::File)
            | (EntryKind::Dir, EntryKind::Dir)
            | (EntryKind::Symlink { .. }, EntryKind::Symlink { .. })
    )
}

/// Operation for a source entry. `d` is the target entry of the SAME type, or
/// None when there is none (missing, or removed as a conflict).
fn op_for(s: FileEntry, d: Option<&FileEntry>, mtime_tol: i64) -> Op {
    match s.kind {
        EntryKind::Dir => Op::Dir {
            mtime: s.mtime,
            mode: s.mode,
            create: d.is_none(),
            meta_ok: d.is_some_and(|d| (s.mtime - d.mtime).abs() <= mtime_tol && s.mode == d.mode),
        },
        EntryKind::File => match d {
            Some(d) if unchanged(&s, d, mtime_tol) => {
                Op::Same { size: s.size, mode: (s.mode != d.mode).then_some(s.mode) }
            }
            _ => Op::Copy { size: s.size },
        },
        EntryKind::Symlink { target } => {
            // Equal = same target AND mtime within tolerance.
            let same = d.is_some_and(|d| {
                matches!(&d.kind, EntryKind::Symlink { target: dt } if *dt == target)
                    && (s.mtime - d.mtime).abs() <= mtime_tol
            });
            if same {
                Op::Keep
            } else {
                Op::Symlink { target, mtime: s.mtime }
            }
        }
    }
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

    fn file(name: &str, size: u64, mtime: i64) -> (OsString, FileEntry) {
        (name.into(), FileEntry { kind: EntryKind::File, size, mtime, mode: 0o644, id: FileId::default() })
    }
    fn dir(name: &str) -> (OsString, FileEntry) {
        (name.into(), FileEntry { kind: EntryKind::Dir, size: 0, mtime: 0, mode: 0o755, id: FileId::default() })
    }
    fn symlink(name: &str, target: &str) -> (OsString, FileEntry) {
        (
            name.into(),
            FileEntry {
                kind: EntryKind::Symlink { target: PathBuf::from(target) },
                size: 0,
                mtime: 0,
                mode: 0o777,
                id: FileId::default(),
            },
        )
    }
    fn child(name: &str, conflict: bool, op: Op) -> Child {
        Child { name: name.into(), conflict, op }
    }

    #[test]
    fn copies_new_file() {
        let out = diff_dir(vec![file("a.txt", 10, 100)], vec![], 1);
        assert_eq!(out, vec![child("a.txt", false, Op::Copy { size: 10 })]);
    }

    #[test]
    fn skips_unchanged_file() {
        let out = diff_dir(vec![file("a.txt", 10, 100)], vec![file("a.txt", 10, 100)], 1);
        assert_eq!(out, vec![child("a.txt", false, Op::Same { size: 10, mode: None })]);
        assert!(!out[0].touches_parent());
    }

    #[test]
    fn copies_changed_size() {
        let out = diff_dir(vec![file("a.txt", 20, 100)], vec![file("a.txt", 10, 100)], 1);
        assert_eq!(out[0].op, Op::Copy { size: 20 });
    }

    #[test]
    fn mtime_within_tolerance_skips() {
        let out = diff_dir(vec![file("a.txt", 10, 100)], vec![file("a.txt", 10, 101)], 1);
        assert!(matches!(out[0].op, Op::Same { .. }));
    }

    #[test]
    fn mtime_beyond_tolerance_copies() {
        let out = diff_dir(vec![file("a.txt", 10, 100)], vec![file("a.txt", 10, 103)], 1);
        assert_eq!(out[0].op, Op::Copy { size: 10 });
    }

    #[test]
    fn mode_only_change_is_a_meta_update_not_a_copy() {
        let mut sf = file("a.txt", 10, 100);
        sf.1.mode = 0o600;
        let out = diff_dir(vec![sf], vec![file("a.txt", 10, 100)], 1);
        assert_eq!(out[0].op, Op::Same { size: 10, mode: Some(0o600) });
        assert!(!out[0].touches_parent(), "chmod does not change the parent's mtime");
    }

    #[test]
    fn extra_entries_on_dest_are_removed() {
        let out = diff_dir(vec![], vec![dir("old"), file("old.txt", 5, 100)], 1);
        assert_eq!(
            out,
            vec![
                child("old", false, Op::Remove { is_dir: true }),
                child("old.txt", false, Op::Remove { is_dir: false }),
            ]
        );
        assert!(out.iter().all(Child::touches_parent));
    }

    #[test]
    fn creates_missing_dir() {
        let out = diff_dir(vec![dir("sub")], vec![], 1);
        assert_eq!(
            out[0].op,
            Op::Dir { mtime: 0, mode: 0o755, create: true, meta_ok: false }
        );
        assert!(out[0].touches_parent());
    }

    #[test]
    fn existing_dir_with_matching_meta_is_left_alone() {
        let out = diff_dir(vec![dir("sub")], vec![dir("sub")], 1);
        assert_eq!(out[0].op, Op::Dir { mtime: 0, mode: 0o755, create: false, meta_ok: true });
        assert!(!out[0].touches_parent());
    }

    #[test]
    fn dir_meta_differs_is_detected() {
        let mut d = dir("sub");
        d.1.mtime = 999;
        d.1.mode = 0o700;
        let out = diff_dir(vec![dir("sub")], vec![d], 1);
        assert_eq!(out[0].op, Op::Dir { mtime: 0, mode: 0o755, create: false, meta_ok: false });
    }

    #[test]
    fn file_on_source_dir_on_dest_is_a_conflict_then_copy() {
        let out = diff_dir(vec![file("x", 3, 100)], vec![dir("x")], 1);
        assert_eq!(out, vec![child("x", true, Op::Copy { size: 3 })]);
    }

    #[test]
    fn dir_on_source_file_on_dest_is_a_conflict_then_mkdir() {
        let out = diff_dir(vec![dir("y")], vec![file("y", 3, 100)], 1);
        assert!(out[0].conflict);
        assert!(matches!(out[0].op, Op::Dir { create: true, .. }));
    }

    #[test]
    fn new_symlink_is_created() {
        let out = diff_dir(vec![symlink("link", "target")], vec![], 1);
        assert_eq!(out[0].op, Op::Symlink { target: PathBuf::from("target"), mtime: 0 });
    }

    #[test]
    fn identical_symlink_is_skipped() {
        let out = diff_dir(vec![symlink("link", "target")], vec![symlink("link", "target")], 1);
        assert_eq!(out[0].op, Op::Keep);
        assert!(!out[0].touches_parent());
    }

    #[test]
    fn symlink_mtime_drift_is_recreated() {
        let mut sl = symlink("link", "target");
        sl.1.mtime = 100;
        let mut dl = symlink("link", "target");
        dl.1.mtime = 500;
        let out = diff_dir(vec![sl], vec![dl], 1);
        assert_eq!(out[0].op, Op::Symlink { target: PathBuf::from("target"), mtime: 100 });
    }

    #[test]
    fn symlink_mtime_within_tolerance_is_skipped() {
        let mut sl = symlink("link", "target");
        sl.1.mtime = 100;
        let mut dl = symlink("link", "target");
        dl.1.mtime = 101;
        let out = diff_dir(vec![sl], vec![dl], 1);
        assert_eq!(out[0].op, Op::Keep);
    }

    #[test]
    fn changed_symlink_target_is_recreated() {
        let out = diff_dir(vec![symlink("link", "new")], vec![symlink("link", "old")], 1);
        assert_eq!(out[0].op, Op::Symlink { target: PathBuf::from("new"), mtime: 0 });
    }

    #[test]
    fn merge_join_interleaves_both_sides_in_name_order() {
        let out = diff_dir(
            vec![file("a", 1, 0), file("c", 1, 0), file("e", 1, 0)],
            vec![file("b", 1, 0), file("c", 1, 0), file("d", 1, 0)],
            1,
        );
        let names: Vec<_> = out.iter().map(|c| c.name.to_str().unwrap()).collect();
        assert_eq!(names, ["a", "b", "c", "d", "e"]);
        assert!(matches!(out[1].op, Op::Remove { .. }));
        assert!(matches!(out[2].op, Op::Same { .. }));
        assert!(matches!(out[3].op, Op::Remove { .. }));
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
