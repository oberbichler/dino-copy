//! Streaming merge walk over source and target.
//!
//! Instead of loading both trees into memory and diffing them afterwards, the
//! walker reads source and target directory by directory, compares the two
//! listings of each directory by a merge join ([`diff_dir`]) and acts on the
//! result right away. Memory is bounded by tree depth × directory width plus
//! the bounded copy queue — independent of the total number of entries. What
//! grows with the run is proportional to the *changes* only: the paths to
//! delete and the `--checksum` mismatches.
//!
//! # Two passes
//!
//! 1. [`plan`] walks both trees and only counts: the numbers for the plan
//!    line, the delete confirmation, `--max-delete` and the progress bar. With
//!    `--checksum` it also hashes every file that looks unchanged. It records
//!    which target entries would be deleted ([`Plan::removals`],
//!    [`Plan::conflicts`]).
//! 2. [`execute`] walks again and does the work. The trees may have changed in
//!    between — the user took a while to confirm, a disk was unmounted — so
//!    pass 2 re-derives every decision from the current state, but it deletes
//!    nothing that pass 1 did not plan.
//!
//! # What may be deleted
//!
//! Deleting is the one irreversible step, so it is fenced in on all sides.
//! A target entry is only removed when all of the following hold:
//!
//! - pass 1 planned it and the user confirmed (or `--yes`) — a removal outside
//!   [`Plan::removals`], a type conflict outside [`Plan::conflicts`], or a
//!   subtree that grew since pass 1, blocks it;
//! - no earlier action failed (`--ignore-errors` overrides): an extra entry
//!   might be the only copy left of data whose new copy just failed;
//! - every directory could be read (`--ignore-errors` overrides): the missing
//!   entries of an unreadable directory are unknown, not gone;
//! - its name really is extra on the target, not an alias of a source name
//!   that differs only in case or Unicode normalization
//!   ([`Walker::resolve_aliases`]).
//!
//! Removals run after all copies; type conflicts are replaced as the walk
//! reaches them (they are confirmed like removals).

use crate::filter::Filter;
use crate::plan::{Child, EntryKind, FileEntry, FileId, Listing, Op, Stats, diff_dir, hash_file};
use crate::problem::{Action, Problem};
use crate::sync::{self, ProgressSink};
use rayon::prelude::*;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Stack size of the walker threads. The walk recurses once per directory
/// level (plus rayon's own frames in between); 16 MiB of reserved virtual
/// memory covers the deepest paths a Unix `PATH_MAX` allows.
const WALK_STACK: usize = 16 * 1024 * 1024;

/// Capacity of the queue between the walker and the copy workers. It keeps
/// the workers busy while the walker reads ahead, without letting the queue
/// grow with the tree (a copy job is just a relative path).
const COPY_QUEUE: usize = 4096;

/// The two roots plus everything both passes need to compare them.
pub struct Roots<'a> {
    pub src: &'a Path,
    pub dst: &'a Path,
    pub filter: &'a Filter,
    pub mtime_tol: i64,
    /// Set to stop a run from another thread. A cancelled run starts no new
    /// work, interrupts running copies (their temp files are removed) and
    /// deletes nothing. `None` where the process itself ends on Ctrl-C.
    pub cancel: Option<&'a AtomicBool>,
}

impl Roots<'_> {
    /// True once the run has been asked to stop.
    pub fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
    }
}

/// Observer of the plan pass: progress and warnings. The command line tool
/// prints them; the desktop app shows them.
pub trait PlanObserver: Sync {
    /// `n` more entries were listed (source and target together).
    fn listed(&self, n: u64);
    /// One more file was verified by `--checksum`.
    fn hashed(&self);
    /// Something the user should know but that does not stop the run, e.g.
    /// an unreadable directory or a skipped special file.
    fn warning(&self, problem: &Problem);
}

/// Reads one directory: its entries (filtered, sorted by name) or the error
/// text when it cannot be read.
///
/// The lstat calls run in parallel: on a flat directory with many entries
/// they dominate the cost. Special files (FIFO/socket/device) and entries that
/// vanish between readdir and lstat are skipped — the latter is normal for a
/// source that is in use (lock/cache files).
pub fn read_listing(
    abs: &Path,
    rel: &Path,
    filter: &Filter,
    observer: Option<&dyn PlanObserver>,
) -> std::io::Result<Listing> {
    let rd = std::fs::read_dir(abs)?;
    let mut names: Vec<OsString> = Vec::new();
    for entry in rd {
        let name = entry?.file_name();
        // Apply exclusions before the lstat: an excluded directory is neither
        // listed nor entered, so it drops out together with its subtree.
        if !filter.is_empty() && filter.excludes(&rel.join(&name)) {
            continue;
        }
        names.push(name);
    }
    let read: Vec<std::io::Result<Option<(OsString, FileEntry)>>> = names
        .into_par_iter()
        .map(|name| read_entry(abs, name, observer))
        .collect();
    let mut out = Vec::with_capacity(read.len());
    for r in read {
        if let Some(e) = r? {
            out.push(e);
        }
    }
    out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// An error reading one entry, with its path in the message (the listing
/// error names only the directory).
fn in_entry(path: &Path, e: std::io::Error) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

/// `lstat` + `read_link` for one entry. `Ok(None)` for entries that do not take
/// part in the mirror (special files, vanished entries).
fn read_entry(
    dir: &Path,
    name: OsString,
    observer: Option<&dyn PlanObserver>,
) -> std::io::Result<Option<(OsString, FileEntry)>> {
    use std::os::unix::fs::MetadataExt;

    let path = dir.join(&name);
    let vanished = |what: &'static str| {
        if let Some(o) = observer {
            o.warning(&Problem::note(Action::Vanished { what }, &path, ""));
        }
    };
    // symlink_metadata does NOT follow the link: symlinks are mirrored as
    // links, never dereferenced.
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            vanished("entry");
            return Ok(None);
        }
        Err(e) => return Err(in_entry(&path, e)),
    };
    let kind = if meta.file_type().is_symlink() {
        match std::fs::read_link(&path) {
            Ok(target) => EntryKind::Symlink { target },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                vanished("symlink");
                return Ok(None);
            }
            Err(e) => return Err(in_entry(&path, e)),
        }
    } else if meta.is_dir() {
        EntryKind::Dir
    } else if meta.is_file() {
        EntryKind::File
    } else {
        // File::open on a FIFO blocks until a writer connects -> deadlock.
        if let Some(o) = observer {
            o.warning(&Problem::note(Action::SpecialFile, &path, ""));
        }
        return Ok(None);
    };
    let size = if meta.is_file() { meta.len() } else { 0 };
    let entry = FileEntry {
        kind,
        size,
        mtime: meta.mtime(),
        mode: meta.mode() & 0o7777,
        id: FileId::of(&meta),
    };
    Ok(Some((name, entry)))
}

/// A directory of the source tree that is being (or about to be) processed.
///
/// Nodes form a tree through their `parent` links. Whoever still has work in
/// a directory holds an `Arc` to its node: the walker while it lists the
/// directory, every subdirectory node, every queued or running copy into it.
/// When the last reference goes away the subtree is complete, and `Drop`
/// applies the directory's mtime and mode — deepest first, automatically,
/// because a child node keeps its parent alive until it is dropped itself.
pub struct DirNode<'a> {
    parent: Option<Arc<DirNode<'a>>>,
    rel: PathBuf,
    /// Source mtime and mode; None for the root (handled by main).
    meta: Option<(i64, u32)>,
    /// The target directory's metadata already matches the source.
    meta_ok: bool,
    /// An action changed this directory's contents (and thus its mtime).
    touched: AtomicBool,
    /// A deletion is pending somewhere below this directory. Its metadata has
    /// to wait until after the delete phase: removing a child changes the
    /// mtime again, and a mirrored read-only mode would block the removal.
    deferred: AtomicBool,
    /// Present in the execute pass only.
    exec: Option<&'a ExecShared<'a>>,
}

impl<'a> DirNode<'a> {
    fn child(self: &Arc<Self>, name: &OsString, mtime: i64, mode: u32, meta_ok: bool) -> Arc<Self> {
        Arc::new(DirNode {
            parent: Some(self.clone()),
            rel: self.rel.join(name),
            meta: Some((mtime, mode)),
            meta_ok,
            touched: AtomicBool::new(false),
            deferred: AtomicBool::new(false),
            exec: self.exec,
        })
    }

    fn needs_meta(&self) -> bool {
        self.meta.is_some() && (!self.meta_ok || self.touched.load(Ordering::Relaxed))
    }

    /// Marks this node and all its ancestors as deferred.
    fn defer(&self) {
        let mut node = Some(self);
        while let Some(n) = node {
            if n.deferred.swap(true, Ordering::Relaxed) {
                break; // Ancestors were marked by the earlier call.
            }
            node = n.parent.as_deref();
        }
    }
}

impl Drop for DirNode<'_> {
    fn drop(&mut self) {
        let Some(exec) = self.exec else { return };
        let target = exec.dst_root.join(&self.rel);
        if exec.fsync && *self.touched.get_mut() {
            if let Err(e) = sync::sync_dir(&target) {
                exec.fail(Problem::io(Action::FsyncDir, &target, &e));
            }
        }
        if !self.needs_meta() {
            return;
        }
        let (mtime, mode) = self.meta.expect("needs_meta implies meta");
        if *self.deferred.get_mut() {
            exec.deferred
                .lock()
                .unwrap()
                .push((std::mem::take(&mut self.rel), mtime, mode));
        } else if let Err(e) = sync::set_dir_meta(&target, mtime, mode) {
            exec.meta_failed.fetch_add(1, Ordering::Relaxed);
            sync::record_error(exec.errors, Problem::io(Action::SetDirMeta, &target, &e));
        }
    }
}

/// A target entry to rename to the source's spelling (within one directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    pub from: OsString,
    pub to: OsString,
}

/// Whether a target name that the file system resolves to the same entry as
/// the source name should be renamed to the source's spelling.
///
/// Yes for a difference in case: case-insensitive file systems preserve the
/// case given (APFS, HFS+, exFAT, NTFS, ext4 casefold), so after one rename
/// the names match and later runs have nothing to do.
///
/// No for a pure Unicode normalization difference (NFC vs. NFD): it is
/// invisible, both spellings open the file, and HFS+ stores every name in NFD
/// anyway — renaming to NFC there would come back as NFD and repeat on every
/// run. Names that are not valid UTF-8 have no normalization; any difference
/// counts.
fn needs_rename(source: &OsStr, target: &OsStr) -> bool {
    use unicode_normalization::UnicodeNormalization;
    match (source.to_str(), target.to_str()) {
        (Some(s), Some(t)) => s.nfc().ne(t.nfc()),
        _ => source != target,
    }
}

/// Same entry type (file / directory / symlink).
fn same_kind(a: &FileEntry, b: &FileEntry) -> bool {
    std::mem::discriminant(&a.kind) == std::mem::discriminant(&b.kind)
}

/// A subdirectory the walker descends into next.
struct SubDir<'a> {
    node: Arc<DirNode<'a>>,
    /// Whether the directory exists on the target with the right type (and
    /// so has to be listed there). False for newly created ones.
    dst_exists: bool,
}

/// What differs between the plan and the execute pass. The walk itself —
/// listing, alias resolution, diffing, recursion — is shared.
trait Visitor<'a>: Sync {
    /// Handles all children of the directory `node`, returns the
    /// subdirectories to descend into. `renames` come first: the children
    /// already carry the new names.
    fn visit(
        &self,
        w: &Walker<'_>,
        node: &Arc<DirNode<'a>>,
        renames: &[Rename],
        children: Vec<Child>,
    ) -> Vec<SubDir<'a>>;

    /// The source entry `rel` cannot be mirrored because the target file
    /// system resolves its name to a different entry (see
    /// [`Walker::resolve_aliases`]). The entry is left out of the run.
    fn collision(&self, w: &Walker<'_>, rel: &Path, why: &str);
}

/// One walk over source and target, driving a [`Visitor`].
struct Walker<'r> {
    roots: &'r Roots<'r>,
    unreadable: AtomicU64,
    /// Progress and warnings; only pass 1 has one (pass 2 would repeat the
    /// warnings).
    observer: Option<&'r dyn PlanObserver>,
}

impl Walker<'_> {
    fn list(&self, root: &Path, rel: &Path) -> Listing {
        let abs = root.join(rel);
        match read_listing(&abs, rel, self.roots.filter, self.observer) {
            Ok(l) => {
                if let Some(p) = self.observer {
                    p.listed(l.len() as u64);
                }
                l
            }
            Err(e) => {
                // The contents are unknown, NOT empty: the caller must not
                // treat what is missing as deletable (see the delete gate).
                self.report_unreadable(&abs, &e);
                Vec::new()
            }
        }
    }

    fn report_unreadable(&self, abs: &Path, err: &std::io::Error) {
        self.unreadable.fetch_add(1, Ordering::Relaxed);
        if let Some(o) = self.observer {
            o.warning(&Problem::io(Action::Unreadable, abs, err));
        }
    }

    /// Number of entries in a target subtree that is going to be removed (the
    /// directory itself included). Only counts — the removal itself is one
    /// `remove_dir_all`.
    fn count_tree(&self, rel: &Path) -> u64 {
        let abs = self.roots.dst.join(rel);
        let rd = match std::fs::read_dir(&abs) {
            Ok(rd) => rd,
            Err(e) => {
                self.report_unreadable(&abs, &e);
                return 1;
            }
        };
        let mut count = 1;
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    self.report_unreadable(&abs, &e);
                    break;
                }
            };
            let child = rel.join(entry.file_name());
            if self.roots.filter.excludes(&child) {
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_dir() => count += self.count_tree(&child),
                Ok(t) if t.is_file() || t.is_symlink() => count += 1,
                _ => {}
            }
        }
        count
    }

    /// Reconciles names that the target file system treats as equal although
    /// their bytes differ.
    ///
    /// macOS volumes (APFS, HFS+) are case-insensitive and
    /// normalization-insensitive by default: `Foo.txt` and `foo.txt`, or `é`
    /// in NFC and NFD, name the same file. A byte-wise comparison would treat
    /// such a pair as "new on the source" plus "extra on the target" — then
    /// copy the new name (which overwrites the old entry, as it is the same
    /// file) and finally delete the old name, i.e. the freshly copied data.
    ///
    /// For each source name missing in the target listing, an lstat on the
    /// target tells whether the file system resolves it anyway. If it does, the
    /// device/inode pair identifies the target entry meant, which is then
    /// compared under the source's name instead of being deleted. When that is
    /// not unambiguously possible (the entry is already paired, excluded,
    /// hard-linked within the directory, or appeared meanwhile), the source
    /// entry is reported as a collision and left out: no data on the target is
    /// touched, and the run fails so the problem gets noticed.
    fn resolve_aliases<'a>(
        &self,
        visitor: &dyn Visitor<'a>,
        rel: &Path,
        mut src: Listing,
        mut dst: Listing,
    ) -> (Listing, Listing, Vec<Rename>) {
        let in_dst = |name: &OsString, dst: &Listing| {
            dst.binary_search_by(|(n, _)| n.as_os_str().cmp(name))
                .is_ok()
        };
        let dir = self.roots.dst.join(rel);
        let mut drop_src: Vec<usize> = Vec::new();
        let mut renames: Vec<Rename> = Vec::new();
        for i in 0..src.len() {
            let name = &src[i].0;
            // `dst` is sorted again after each rename, so the search stays valid.
            if in_dst(name, &dst) {
                continue;
            }
            // NotFound is the normal case: the name is new on the target. Other
            // errors are left to the action itself, which reports them.
            let Ok(meta) = std::fs::symlink_metadata(dir.join(name)) else {
                continue;
            };
            let id = FileId::of(&meta);
            let mut same = dst.iter().enumerate().filter(|(_, (_, d))| d.id == id);
            let candidate = match (same.next(), same.next()) {
                (Some((j, _)), None) => Some(j),
                _ => None, // none (excluded / appeared meanwhile) or hard links
            };
            let source_has = |j: usize| {
                src.binary_search_by(|(n, _)| n.as_os_str().cmp(&dst[j].0))
                    .is_ok()
            };
            let keep = match candidate {
                Some(j) if !source_has(j) => {
                    // A visible difference (case) is mirrored by renaming the
                    // target entry; see `needs_rename`. Not for a type
                    // conflict: that entry is replaced under the new name anyway.
                    if needs_rename(name, &dst[j].0) && same_kind(&src[i].1, &dst[j].1) {
                        renames.push(Rename {
                            from: dst[j].0.clone(),
                            to: name.clone(),
                        });
                    }
                    // Compare the target entry under the source's spelling. All
                    // later paths use that spelling, which the target resolves.
                    dst[j].0.clone_from(name);
                    dst.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                    true
                }
                Some(j) => {
                    let why = format!(
                        "the target file system treats it as the same name as {:?}",
                        dst[j].0
                    );
                    visitor.collision(self, &rel.join(name), &why);
                    false
                }
                None => {
                    visitor.collision(
                        self,
                        &rel.join(name),
                        "the target file system resolves the name to an entry that \
                         cannot be identified unambiguously (excluded, hard-linked or \
                         created during the run)",
                    );
                    false
                }
            };
            if !keep {
                drop_src.push(i);
            }
        }
        for i in drop_src.into_iter().rev() {
            src.remove(i);
        }
        (src, dst, renames)
    }

    /// Processes the directory `node` and, in parallel, everything below it.
    fn walk<'a>(&self, visitor: &dyn Visitor<'a>, node: Arc<DirNode<'a>>, dst_exists: bool) {
        if self.roots.cancelled() {
            return;
        }
        let src = self.list(self.roots.src, &node.rel);
        let dst = if dst_exists {
            self.list(self.roots.dst, &node.rel)
        } else {
            Vec::new()
        };
        let (src, dst, renames) = if dst_exists {
            self.resolve_aliases(visitor, &node.rel, src, dst)
        } else {
            (src, dst, Vec::new())
        };
        let children = diff_dir(src, dst, self.roots.mtime_tol);
        let subdirs = visitor.visit(self, &node, &renames, children);
        // Our own reference is no longer needed: the subdirectories and the
        // queued copies keep the node alive exactly as long as necessary.
        drop(node);
        subdirs
            .into_par_iter()
            .for_each(|sd| self.walk(visitor, sd.node, sd.dst_exists));
    }
}

/// Runs `f` in a dedicated pool with `threads` threads (0 = one per CPU).
fn in_pool<R: Send>(threads: usize, f: impl FnOnce() -> R + Send) -> R {
    match rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(WALK_STACK)
        .build()
    {
        Ok(pool) => pool.install(f),
        Err(_) => f(),
    }
}

fn root_node<'a>(exec: Option<&'a ExecShared<'a>>) -> Arc<DirNode<'a>> {
    Arc::new(DirNode {
        parent: None,
        rel: PathBuf::new(),
        meta: None,
        meta_ok: true,
        touched: AtomicBool::new(false),
        deferred: AtomicBool::new(false),
        exec,
    })
}

// --------------------------------------------------------------------------
// Pass 1: plan
// --------------------------------------------------------------------------

/// Options of [`plan`].
pub struct PlanOptions {
    /// Walk parallelism (0 = one thread per CPU).
    pub scan_jobs: usize,
    /// Verify unchanged-looking files by blake3.
    pub checksum: bool,
    /// Parallelism of the checksum pass (it reads every byte twice, so it
    /// follows the copy parallelism instead of the scan parallelism).
    pub hash_jobs: usize,
    /// Collect every planned change with its path ([`Plan::details`]) — for
    /// showing a scan in detail. Off for a plain run: the list grows with the
    /// number of changes.
    pub details: bool,
}

/// One planned change, see [`PlanOptions::details`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    /// Relative path, in the source's spelling.
    pub path: PathBuf,
    pub kind: DetailKind,
}

/// What happens to the entry at [`Detail::path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailKind {
    /// A file is copied (new, changed, or a `--checksum` mismatch). Symlinks
    /// appear here too, with size 0.
    Copy { size: u64 },
    /// A directory is created.
    Mkdir,
    /// A target entry without a source counterpart is deleted; `entries`
    /// counts it with its whole subtree.
    Remove { is_dir: bool, entries: u64 },
    /// A target entry of another type is deleted (with its subtree) and
    /// replaced by the source entry; that entry has its own `Copy`/`Mkdir`.
    Replace,
    /// The target entry is renamed to the source's spelling; `from` is its
    /// current name.
    Rename { from: OsString },
}

/// Result of [`plan`]: what a run would do, and the limits for [`execute`].
pub struct Plan {
    pub stats: Stats,
    /// Files that look unchanged but whose contents differ (`--checksum`).
    /// Only the mismatches are kept, not the verified files.
    pub forced: BTreeSet<PathBuf>,
    /// Directories that could not be read (source and target).
    pub unreadable: u64,
    /// The source root has no entries (after filtering).
    pub source_empty: bool,
    /// Target entries to remove: the top of each removed subtree. Pass 2
    /// removes nothing outside this set, so only what was planned (and
    /// confirmed) can ever be deleted.
    pub removals: BTreeSet<PathBuf>,
    /// Target entries of the wrong type that get replaced (with their
    /// subtree). Pass 2 resolves no conflict outside this set.
    pub conflicts: BTreeSet<PathBuf>,
    /// Source entries left out because of a name collision on the target.
    pub collisions: u64,
    /// Every planned change, if [`PlanOptions::details`] asked for it (in no
    /// particular order).
    pub details: Vec<Detail>,
    /// The plan was cut short by [`Roots::cancel`]: its numbers are incomplete.
    pub cancelled: bool,
}

impl Plan {
    /// Whether deletions may run after this plan. They are withheld when the
    /// plan is incomplete — directories could not be read, or source names
    /// collide on the target — unless `ignore_errors` overrides that.
    pub fn deletions_allowed(&self, ignore_errors: bool) -> bool {
        ignore_errors || (self.unreadable == 0 && self.collisions == 0)
    }
}

struct Planner {
    checksum: bool,
    stats: Mutex<Stats>,
    forced: Mutex<BTreeSet<PathBuf>>,
    removals: Mutex<BTreeSet<PathBuf>>,
    conflicts: Mutex<BTreeSet<PathBuf>>,
    collisions: AtomicU64,
    details: Option<Mutex<Vec<Detail>>>,
}

impl<'a> Visitor<'a> for Planner {
    fn visit(
        &self,
        w: &Walker<'_>,
        node: &Arc<DirNode<'a>>,
        renames: &[Rename],
        children: Vec<Child>,
    ) -> Vec<SubDir<'a>> {
        let roots = w.roots;
        // --checksum: hash the files that look unchanged, in parallel.
        let forced: Vec<bool> = if self.checksum {
            children
                .par_iter()
                .map(|c| {
                    if !matches!(c.op, Op::Same { .. }) || c.conflict {
                        return false;
                    }
                    let rel = node.rel.join(&c.name);
                    let (hs, hd) = rayon::join(
                        || hash_file(&roots.src.join(&rel)),
                        || hash_file(&roots.dst.join(&rel)),
                    );
                    if let Some(p) = w.observer {
                        p.hashed();
                    }
                    !matches!((hs, hd), (Ok(a), Ok(b)) if a == b)
                })
                .collect()
        } else {
            vec![false; children.len()]
        };

        let mut s = Stats {
            rename_count: renames.len() as u64,
            ..Stats::default()
        };
        let mut touched = !renames.is_empty();
        let mut subdirs = Vec::new();
        // Collected locally, appended under one lock per directory.
        let mut details: Option<Vec<Detail>> = self.details.as_ref().map(|_| Vec::new());
        let mut detail = |path: PathBuf, kind: DetailKind| {
            if let Some(d) = details.as_mut() {
                d.push(Detail { path, kind });
            }
        };
        for r in renames {
            detail(
                node.rel.join(&r.to),
                DetailKind::Rename {
                    from: r.from.clone(),
                },
            );
        }
        for (c, forced) in children.into_iter().zip(forced) {
            if c.conflict {
                detail(node.rel.join(&c.name), DetailKind::Replace);
                s.conflict_count += 1;
                self.conflicts
                    .lock()
                    .unwrap()
                    .insert(node.rel.join(&c.name));
            }
            touched |= c.touches_parent() || forced;
            match c.op {
                Op::Copy { size } => {
                    s.copy_count += 1;
                    s.copy_bytes += size;
                    detail(node.rel.join(&c.name), DetailKind::Copy { size });
                }
                Op::Same { size, .. } if forced => {
                    s.copy_count += 1;
                    s.copy_bytes += size;
                    self.forced.lock().unwrap().insert(node.rel.join(&c.name));
                    detail(node.rel.join(&c.name), DetailKind::Copy { size });
                }
                Op::Same { mode, .. } => {
                    s.skip_count += 1;
                    if mode.is_some() {
                        s.meta_count += 1;
                    }
                }
                Op::Symlink { .. } => {
                    s.copy_count += 1;
                    detail(node.rel.join(&c.name), DetailKind::Copy { size: 0 });
                }
                Op::Keep => {}
                Op::Dir {
                    mtime,
                    mode,
                    create,
                    meta_ok,
                } => {
                    if create {
                        s.mkdir_count += 1;
                        detail(node.rel.join(&c.name), DetailKind::Mkdir);
                    }
                    subdirs.push(SubDir {
                        node: node.child(&c.name, mtime, mode, meta_ok),
                        dst_exists: !create,
                    });
                }
                Op::Remove { is_dir } => {
                    let rel = node.rel.join(&c.name);
                    let entries = if is_dir { w.count_tree(&rel) } else { 1 };
                    s.remove_count += entries;
                    detail(rel.clone(), DetailKind::Remove { is_dir, entries });
                    self.removals.lock().unwrap().insert(rel);
                }
            }
        }
        node.touched.store(touched, Ordering::Relaxed);
        if node.needs_meta() {
            s.dir_meta_count += 1;
        }
        self.stats.lock().unwrap().add(&s);
        if let (Some(all), Some(mine)) = (&self.details, details) {
            all.lock().unwrap().extend(mine);
        }
        subdirs
    }

    fn collision(&self, w: &Walker<'_>, rel: &Path, why: &str) {
        self.collisions.fetch_add(1, Ordering::Relaxed);
        if let Some(o) = w.observer {
            o.warning(&Problem::note(Action::Collision, rel, why));
        }
    }
}

/// Pass 1: walks both trees and counts what a run would do.
pub fn plan(roots: &Roots, opts: PlanOptions, observer: Option<&dyn PlanObserver>) -> Plan {
    let walker = Walker {
        roots,
        unreadable: AtomicU64::new(0),
        observer,
    };
    let planner = Planner {
        checksum: opts.checksum,
        stats: Mutex::new(Stats::default()),
        forced: Mutex::new(BTreeSet::new()),
        removals: Mutex::new(BTreeSet::new()),
        conflicts: Mutex::new(BTreeSet::new()),
        collisions: AtomicU64::new(0),
        details: opts.details.then(|| Mutex::new(Vec::new())),
    };
    let source_empty = match read_listing(roots.src, Path::new(""), roots.filter, None) {
        Ok(l) => l.is_empty(),
        Err(_) => true,
    };
    let threads = if opts.checksum {
        opts.hash_jobs
    } else {
        opts.scan_jobs
    };
    let dst_exists = roots.dst.is_dir();
    in_pool(threads, || {
        walker.walk(&planner, root_node(None), dst_exists)
    });
    Plan {
        stats: planner.stats.into_inner().unwrap(),
        forced: planner.forced.into_inner().unwrap(),
        unreadable: walker.unreadable.into_inner(),
        source_empty,
        removals: planner.removals.into_inner().unwrap(),
        conflicts: planner.conflicts.into_inner().unwrap(),
        collisions: planner.collisions.into_inner(),
        details: planner
            .details
            .map(|d| d.into_inner().unwrap())
            .unwrap_or_default(),
        cancelled: roots.cancelled(),
    }
}

// --------------------------------------------------------------------------
// Pass 2: execute
// --------------------------------------------------------------------------

/// Options of [`execute`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ExecOptions {
    /// Copy parallelism (values below 1 count as 1).
    pub jobs: usize,
    /// Walk parallelism (0 = one thread per CPU).
    pub scan_jobs: usize,
    /// fsync every copied file and every changed directory.
    pub fsync: bool,
    /// Remove even after failed actions or unreadable directories.
    pub ignore_errors: bool,
    /// Number of removals the user agreed to (pass 1's count, subtrees
    /// included). Together with [`Plan::removals`] this bounds what pass 2 may
    /// delete.
    pub delete_budget: u64,
}

/// Why the deletions of a run were not carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Earlier actions failed: the extra entries might be the only copy left.
    FailedActions,
    /// Directories could not be read: missing entries are unknown, not gone.
    Unreadable,
    /// Pass 2 found removals that pass 1 did not plan (or more entries than
    /// were confirmed): the trees changed in between — for instance, the
    /// source was unmounted. Nothing is removed then.
    Unplanned { planned: u64, found: u64 },
    /// The run was cancelled ([`Roots::cancel`]) before the removals.
    Cancelled,
}

/// Outcome of [`execute`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExecReport {
    /// Failed actions (copies, mkdir, metadata, removals, collisions, ...).
    pub failed: u64,
    /// Directories pass 2 could not read.
    pub unreadable: u64,
    /// Entries (subtrees included) that were not removed, see `skip_reason`.
    pub skipped_deletes: u64,
    /// Why the removals were withheld; `None` if they ran (or none were due).
    pub skip_reason: Option<SkipReason>,
    /// The run was cancelled ([`Roots::cancel`]): the target is incomplete.
    pub cancelled: bool,
}

/// Wraps the caller's sink so that copies notice [`Roots::cancel`].
struct CancelAware<'s> {
    inner: &'s dyn ProgressSink,
    cancel: Option<&'s AtomicBool>,
}

impl ProgressSink for CancelAware<'_> {
    fn add_bytes(&self, n: u64) {
        self.inner.add_bytes(n);
    }
    fn set_current(&self, worker: usize, name: &str) {
        self.inner.set_current(worker, name);
    }
    fn set_done(&self, worker: usize) {
        self.inner.set_done(worker);
    }
    fn inc_files(&self) {
        self.inner.inc_files();
    }
    fn begin_removals(&self, entries: u64) {
        self.inner.begin_removals(entries);
    }
    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) || self.inner.cancelled()
    }
}

struct ExecShared<'a> {
    dst_root: &'a Path,
    fsync: bool,
    errors: &'a Mutex<Vec<Problem>>,
    /// Failures before the delete phase. They block the deletions.
    failed: AtomicU64,
    /// Failures while applying directory metadata. They make the run fail but
    /// do not block deletions (that phase used to run after them).
    meta_failed: AtomicU64,
    /// Directory metadata that has to wait until after the deletions.
    deferred: Mutex<Vec<(PathBuf, i64, u32)>>,
    /// Top-level entries to remove, with whether they are directories.
    removes: Mutex<Vec<(PathBuf, bool)>>,
    remove_count: AtomicU64,
}

impl ExecShared<'_> {
    fn fail(&self, problem: Problem) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        sync::record_error(self.errors, problem);
    }
}

struct CopyJob<'a> {
    rel: PathBuf,
    /// Keeps the parent directory's node alive until the copy is done.
    _parent: Arc<DirNode<'a>>,
}

struct Executor<'a> {
    shared: &'a ExecShared<'a>,
    /// The plan from pass 1: forced copies and the confirmed conflicts.
    plan: &'a Plan,
    copies: std::sync::mpsc::SyncSender<CopyJob<'a>>,
    sink: &'a dyn ProgressSink,
}

impl<'a> Visitor<'a> for Executor<'a> {
    fn visit(
        &self,
        w: &Walker<'_>,
        node: &Arc<DirNode<'a>>,
        renames: &[Rename],
        children: Vec<Child>,
    ) -> Vec<SubDir<'a>> {
        let shared = self.shared;
        let is_forced = |c: &Child| {
            matches!(c.op, Op::Same { .. })
                && !self.plan.forced.is_empty()
                && self.plan.forced.contains(&node.rel.join(&c.name))
        };
        // Set before the first copy is queued: a copy that finishes quickly
        // may already drop the last other reference to this node.
        let touched =
            !renames.is_empty() || children.iter().any(|c| c.touches_parent() || is_forced(c));
        node.touched.store(touched, Ordering::Relaxed);

        // A rename only changes the name of the same file; nothing is copied
        // or lost. If it fails, the entry is still kept in sync under its old
        // spelling (the target resolves both).
        let dir = w.roots.dst.join(&node.rel);
        for r in renames {
            if let Err(e) = std::fs::rename(dir.join(&r.from), dir.join(&r.to)) {
                shared.fail(Problem::io(
                    Action::Rename { to: r.to.clone() },
                    dir.join(&r.from),
                    &e,
                ));
            }
        }

        let mut subdirs = Vec::new();
        for c in children {
            if self.sink.cancelled() {
                break;
            }
            let forced = is_forced(&c);
            let rel = node.rel.join(&c.name);
            let target = w.roots.dst.join(&rel);
            if c.conflict {
                // Replacing a target entry of another type deletes it (with its
                // subtree), so it is held to the same rule as the removals:
                // only what pass 1 planned and the user confirmed.
                if !self.plan.conflicts.contains(&rel) {
                    shared.fail(Problem::note(Action::TypeChanged, &rel, ""));
                    continue;
                }
                if let Err(e) = sync::remove_any(&target) {
                    shared.fail(Problem::io(Action::RemoveConflict, &target, &e));
                    continue;
                }
            }
            match c.op {
                Op::Copy { .. } => self.queue_copy(rel, node),
                Op::Same { .. } if forced => self.queue_copy(rel, node),
                Op::Same {
                    mode: Some(mode), ..
                } => {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(e) =
                        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                    {
                        shared.fail(Problem::io(Action::SetFileMeta, &target, &e));
                    }
                }
                Op::Same { mode: None, .. } | Op::Keep => {}
                Op::Symlink {
                    target: link,
                    mtime,
                } => match sync::create_symlink(&link, &target, mtime) {
                    Ok(()) => self.sink.inc_files(),
                    Err(e) => shared.fail(Problem::io(Action::Symlink, &target, &e)),
                },
                Op::Dir {
                    mtime,
                    mode,
                    create,
                    meta_ok,
                } => {
                    if create {
                        if let Err(e) = std::fs::create_dir_all(&target) {
                            shared.fail(Problem::io(Action::Mkdir, &target, &e));
                        }
                    }
                    subdirs.push(SubDir {
                        node: node.child(&c.name, mtime, mode, meta_ok),
                        dst_exists: !create,
                    });
                }
                Op::Remove { is_dir } => {
                    let n = if is_dir { w.count_tree(&rel) } else { 1 };
                    shared.remove_count.fetch_add(n, Ordering::Relaxed);
                    shared.removes.lock().unwrap().push((rel, is_dir));
                    node.defer();
                }
            }
        }
        subdirs
    }

    fn collision(&self, _: &Walker<'_>, rel: &Path, why: &str) {
        self.shared.fail(Problem::note(Action::Collision, rel, why));
    }
}

impl<'a> Executor<'a> {
    fn queue_copy(&self, rel: PathBuf, node: &Arc<DirNode<'a>>) {
        if self.sink.cancelled() {
            return;
        }
        // Blocks while the queue is full: the walk never runs further ahead
        // of the copies than COPY_QUEUE entries.
        let _ = self.copies.send(CopyJob {
            rel,
            _parent: node.clone(),
        });
    }
}

/// Pass 2: walks both trees again and mirrors the source onto the target.
///
/// Order of effects: confirmed type conflicts, mkdir, symlinks and permission
/// fixes happen as the walk reaches them; copies run in `opts.jobs` worker
/// threads fed by a bounded queue; directory metadata is applied as soon as a
/// directory's subtree is complete. Removals come last, under the rules in the
/// [module documentation](self); directories with removals below them get
/// their metadata after that.
///
/// Errors do not abort the run: they are collected in `errors` (capped) and
/// counted in [`ExecReport::failed`].
pub fn execute(
    roots: &Roots,
    plan: &Plan,
    opts: ExecOptions,
    sink: &dyn ProgressSink,
    errors: &Mutex<Vec<Problem>>,
) -> ExecReport {
    let sink = &CancelAware {
        inner: sink,
        cancel: roots.cancel,
    };
    let shared = ExecShared {
        dst_root: roots.dst,
        fsync: opts.fsync,
        errors,
        failed: AtomicU64::new(0),
        meta_failed: AtomicU64::new(0),
        deferred: Mutex::new(Vec::new()),
        removes: Mutex::new(Vec::new()),
        remove_count: AtomicU64::new(0),
    };
    let walker = Walker {
        roots,
        unreadable: AtomicU64::new(0),
        observer: None,
    };
    let jobs = opts.jobs.max(1);

    let (tx, rx) = std::sync::mpsc::sync_channel::<CopyJob>(COPY_QUEUE);
    let rx = Mutex::new(rx);
    std::thread::scope(|s| {
        for worker in 0..jobs {
            let (rx, shared) = (&rx, &shared);
            s.spawn(move || {
                loop {
                    // The lock is only held for the recv, not for the copy.
                    let job = rx.lock().unwrap().recv();
                    let Ok(job) = job else { break };
                    // After a cancel the queue is drained without copying.
                    if sink.cancelled() {
                        continue;
                    }
                    sink.set_current(worker, &job.rel.to_string_lossy());
                    let src = roots.src.join(&job.rel);
                    let dst = roots.dst.join(&job.rel);
                    if let Err(e) = sync::copy_file(&src, &dst, sink, opts.fsync) {
                        // A copy interrupted by the cancel is not a failure.
                        if !sink.cancelled() {
                            shared.fail(Problem::chain(Action::Copy, &job.rel, &e));
                        }
                    }
                    // Dropping the job may complete its directory (metadata).
                    drop(job);
                }
                // Queue closed and drained: this worker gets no further work.
                sink.set_done(worker);
            });
        }

        let executor = Executor {
            shared: &shared,
            plan,
            copies: tx,
            sink,
        };
        let dst_exists = roots.dst.is_dir();
        in_pool(opts.scan_jobs, || {
            walker.walk(&executor, root_node(Some(&shared)), dst_exists)
        });
        // Closes the queue: the workers finish the remaining jobs and exit.
        drop(executor);
    });

    let unreadable = walker.unreadable.into_inner();
    let failed_before = shared.failed.load(Ordering::Relaxed);
    let removes = std::mem::take(&mut *shared.removes.lock().unwrap());
    let found = shared.remove_count.load(Ordering::Relaxed);

    let mut report = ExecReport {
        unreadable,
        cancelled: sink.cancelled(),
        ..Default::default()
    };
    if !removes.is_empty() {
        report.skip_reason = if report.cancelled {
            Some(SkipReason::Cancelled)
        } else if failed_before > 0 && !opts.ignore_errors {
            Some(SkipReason::FailedActions)
        } else if unreadable > 0 && !opts.ignore_errors {
            Some(SkipReason::Unreadable)
        } else if found > opts.delete_budget
            || removes.iter().any(|(rel, _)| !plan.removals.contains(rel))
        {
            Some(SkipReason::Unplanned {
                planned: opts.delete_budget,
                found,
            })
        } else {
            None
        };
        if report.skip_reason.is_some() {
            report.skipped_deletes = found;
        } else {
            // From here on the run is no longer cancellable: a half-done
            // removal phase is worse than a finished one.
            sink.begin_removals(found);
            for (rel, is_dir) in removes {
                let target = roots.dst.join(&rel);
                let res = if is_dir {
                    std::fs::remove_dir_all(&target)
                } else {
                    std::fs::remove_file(&target)
                };
                if let Err(e) = res {
                    shared.fail(Problem::io(Action::Remove, &target, &e));
                }
            }
        }
    }

    // Deferred directory metadata, deepest first (a child's update would
    // otherwise change the parent's mtime again).
    let mut deferred = std::mem::take(&mut *shared.deferred.lock().unwrap());
    deferred.sort_by_key(|(rel, _, _)| std::cmp::Reverse(rel.components().count()));
    for (rel, mtime, mode) in deferred {
        let target = roots.dst.join(&rel);
        if let Err(e) = sync::set_dir_meta(&target, mtime, mode) {
            shared.meta_failed.fetch_add(1, Ordering::Relaxed);
            sync::record_error(errors, Problem::io(Action::SetDirMeta, &target, &e));
        }
    }

    report.failed =
        shared.failed.load(Ordering::Relaxed) + shared.meta_failed.load(Ordering::Relaxed);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::NullSink;
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    fn roots<'a>(s: &'a Path, d: &'a Path, filter: &'a Filter) -> Roots<'a> {
        Roots {
            src: s,
            dst: d,
            filter,
            mtime_tol: 2,
            cancel: None,
        }
    }

    fn plan_of(s: &Path, d: &Path) -> Plan {
        let f = Filter::default();
        plan(
            &roots(s, d, &f),
            PlanOptions {
                scan_jobs: 2,
                checksum: false,
                hash_jobs: 2,
                details: false,
            },
            None,
        )
    }

    /// Runs pass 2 against an existing plan.
    fn exec_plan(
        s: &Path,
        d: &Path,
        plan: &Plan,
        opts: ExecOptions,
        sink: &dyn ProgressSink,
    ) -> (ExecReport, Vec<Problem>) {
        let f = Filter::default();
        let errors = Mutex::new(Vec::new());
        let opts = ExecOptions {
            delete_budget: plan.stats.remove_count,
            ..opts
        };
        let report = execute(&roots(s, d, &f), plan, opts, sink, &errors);
        (report, errors.into_inner().unwrap())
    }

    /// Plans and executes, like a confirmed run of the binary.
    fn exec(
        s: &Path,
        d: &Path,
        opts: ExecOptions,
        sink: &dyn ProgressSink,
    ) -> (ExecReport, Vec<Problem>) {
        exec_plan(s, d, &plan_of(s, d), opts, sink)
    }

    fn opts(jobs: usize) -> ExecOptions {
        ExecOptions {
            jobs,
            scan_jobs: 2,
            ..Default::default()
        }
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().join("s");
        let d = tmp.path().join("d");
        fs::create_dir_all(&s).unwrap();
        fs::create_dir_all(&d).unwrap();
        (tmp, s, d)
    }

    #[test]
    fn listing_is_sorted_and_carries_the_metadata() {
        let (_tmp, s, _d) = setup();
        fs::create_dir(s.join("sub")).unwrap();
        fs::write(s.join("b.txt"), b"hello").unwrap();
        fs::write(s.join("a.txt"), b"hi").unwrap();

        let l = read_listing(&s, Path::new(""), &Filter::default(), None).unwrap();

        let names: Vec<_> = l.iter().map(|(n, _)| n.to_str().unwrap()).collect();
        assert_eq!(names, ["a.txt", "b.txt", "sub"]);
        assert_eq!(l[1].1.size, 5);
        assert_eq!(l[2].1.kind, EntryKind::Dir);
    }

    #[test]
    fn vanished_entry_is_skipped_not_fatal() {
        // Between readdir and lstat an entry can be deleted (source in use:
        // lock/cache files). That must only be a warning.
        let (_tmp, s, _d) = setup();
        let gone = read_entry(&s, "already_deleted.txt".into(), None)
            .expect("a vanished entry must not be an error");
        assert!(gone.is_none());
    }

    #[test]
    fn skips_special_files_like_fifos() {
        // File::open on a FIFO blocks until a writer connects.
        let (_tmp, s, _d) = setup();
        fs::write(s.join("normal.txt"), b"ok").unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(s.join("pipe"))
            .status()
            .unwrap();
        assert!(status.success());

        let l = read_listing(&s, Path::new(""), &Filter::default(), None).unwrap();

        let names: Vec<_> = l.iter().map(|(n, _)| n.to_str().unwrap()).collect();
        assert_eq!(names, ["normal.txt"], "FIFO must be skipped");
    }

    #[test]
    fn plan_counts_a_whole_removed_subtree() {
        let (_tmp, s, d) = setup();
        fs::create_dir_all(d.join("old/deep")).unwrap();
        fs::write(d.join("old/deep/x"), b"x").unwrap();
        fs::write(d.join("stale.txt"), b"x").unwrap();

        let p = plan_of(&s, &d);

        // old, old/deep, old/deep/x, stale.txt
        assert_eq!(p.stats.remove_count, 4);
        assert!(p.source_empty);
    }

    #[test]
    fn identical_trees_with_subdirs_plan_nothing() {
        // A no-op run must plan nothing — otherwise "nothing to do" is
        // unreachable and dir metadata gets rewritten on every run.
        let (_tmp, s, d) = setup();
        fs::create_dir_all(s.join("sub")).unwrap();
        fs::write(s.join("sub/a.txt"), b"a").unwrap();
        let (r, errs) = exec(&s, &d, opts(2), &NullSink);
        assert_eq!(r.failed, 0, "{errs:?}");

        let p = plan_of(&s, &d);

        assert!(p.stats.is_noop(), "expected no-op, got {:?}", p.stats);
        assert_eq!(p.stats.skip_count, 1);
    }

    #[test]
    fn execute_mirrors_creates_copies_and_removes() {
        let (_tmp, s, d) = setup();
        fs::create_dir_all(s.join("keep")).unwrap();
        fs::write(s.join("keep/a.txt"), b"aaa").unwrap();
        fs::create_dir_all(d.join("old/inner")).unwrap();
        fs::write(d.join("stale.txt"), b"x").unwrap();

        let (r, errs) = exec(&s, &d, opts(2), &NullSink);

        assert_eq!(r.failed, 0, "{errs:?}");
        assert_eq!(fs::read(d.join("keep/a.txt")).unwrap(), b"aaa");
        assert!(!d.join("stale.txt").exists());
        assert!(!d.join("old").exists());
    }

    #[test]
    fn dir_meta_is_applied_after_the_copies_into_it() {
        // A copy renames into the directory and changes its mtime. The mtime
        // has to be set after the LAST copy, even with parallel workers.
        let (_tmp, s, d) = setup();
        fs::create_dir_all(s.join("sub/deep")).unwrap();
        for i in 0..50 {
            fs::write(s.join(format!("sub/f{i}")), b"x").unwrap();
            fs::write(s.join(format!("sub/deep/f{i}")), b"x").unwrap();
        }
        for dir in ["sub/deep", "sub"] {
            filetime::set_file_mtime(
                s.join(dir),
                filetime::FileTime::from_unix_time(1_000_000, 0),
            )
            .unwrap();
        }

        let (r, errs) = exec(&s, &d, opts(4), &NullSink);

        assert_eq!(r.failed, 0, "{errs:?}");
        assert_eq!(fs::metadata(d.join("sub")).unwrap().mtime(), 1_000_000);
        assert_eq!(fs::metadata(d.join("sub/deep")).unwrap().mtime(), 1_000_000);
    }

    #[test]
    fn dir_meta_is_reapplied_after_a_deletion_inside() {
        let (_tmp, s, d) = setup();
        fs::create_dir_all(s.join("sub")).unwrap();
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub/stale.txt"), b"x").unwrap();
        filetime::set_file_mtime(
            s.join("sub"),
            filetime::FileTime::from_unix_time(1_000_000, 0),
        )
        .unwrap();

        let (r, errs) = exec(&s, &d, opts(1), &NullSink);

        assert_eq!(r.failed, 0, "{errs:?}");
        assert!(!d.join("sub/stale.txt").exists());
        assert_eq!(fs::metadata(d.join("sub")).unwrap().mtime(), 1_000_000);
    }

    #[test]
    fn read_only_dir_mode_does_not_block_a_deletion_inside() {
        // The mode is mirrored only after the delete phase — otherwise a
        // source dir with 0o555 would lock the target before the removal.
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, s, d) = setup();
        fs::create_dir_all(s.join("ro")).unwrap();
        fs::create_dir_all(d.join("ro")).unwrap();
        fs::write(d.join("ro/stale.txt"), b"x").unwrap();
        fs::set_permissions(s.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();

        let (r, errs) = exec(&s, &d, opts(1), &NullSink);

        let mode = fs::metadata(d.join("ro")).unwrap().mode() & 0o7777;
        fs::set_permissions(d.join("ro"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(s.join("ro"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(r.failed, 0, "{errs:?}");
        assert!(!d.join("ro/stale.txt").exists());
        assert_eq!(mode, 0o555);
    }

    #[test]
    fn deletions_beyond_the_confirmed_plan_are_skipped() {
        // Pass 1 saw no deletions (the source was mounted), pass 2 finds some
        // (the source vanished meanwhile): nothing may be removed.
        let (_tmp, s, d) = setup();
        fs::write(s.join("precious.txt"), b"x").unwrap();
        fs::write(d.join("precious.txt"), b"x").unwrap();
        let p = plan_of(&s, &d);
        assert_eq!(p.stats.remove_count, 0);
        fs::remove_file(s.join("precious.txt")).unwrap();

        let (r, _) = exec_plan(&s, &d, &p, opts(1), &NullSink);

        assert!(d.join("precious.txt").exists());
        assert_eq!(
            r.skip_reason,
            Some(SkipReason::Unplanned {
                planned: 0,
                found: 1
            })
        );
        assert_eq!(r.skipped_deletes, 1);
    }

    #[test]
    fn a_different_removal_with_the_same_count_is_still_refused() {
        // The count alone is not enough: pass 1 planned to remove "a", pass 2
        // would remove "b" instead. Only confirmed paths may be deleted.
        let (_tmp, s, d) = setup();
        fs::write(s.join("b"), b"x").unwrap();
        fs::write(d.join("a"), b"x").unwrap();
        fs::write(d.join("b"), b"x").unwrap();
        let p = plan_of(&s, &d);
        assert_eq!(p.removals, BTreeSet::from([PathBuf::from("a")]));
        fs::rename(s.join("b"), s.join("a")).unwrap();

        let (r, _) = exec_plan(&s, &d, &p, opts(1), &NullSink);

        assert!(
            d.join("b").exists(),
            "an unconfirmed path must not be deleted"
        );
        assert!(matches!(r.skip_reason, Some(SkipReason::Unplanned { .. })));
    }

    #[test]
    fn a_type_conflict_that_appeared_after_the_plan_is_not_resolved() {
        // Between the passes a source directory turns into a file. Replacing
        // the target directory would delete a subtree nobody confirmed.
        let (_tmp, s, d) = setup();
        fs::create_dir_all(s.join("photos")).unwrap();
        fs::create_dir_all(d.join("photos")).unwrap();
        fs::write(d.join("photos/1.jpg"), b"jpeg").unwrap();
        fs::write(s.join("photos/1.jpg"), b"jpeg").unwrap();
        let p = plan_of(&s, &d);
        fs::remove_dir_all(s.join("photos")).unwrap();
        fs::write(s.join("photos"), b"now a file").unwrap();

        let (r, errs) = exec_plan(&s, &d, &p, opts(1), &NullSink);

        assert_eq!(fs::read(d.join("photos/1.jpg")).unwrap(), b"jpeg");
        assert_eq!(r.failed, 1, "the skipped conflict must fail the run");
        assert_eq!(errs[0].action, Action::TypeChanged, "{errs:?}");
    }

    #[test]
    fn a_removed_subtree_that_grew_since_the_plan_is_not_deleted() {
        // The user confirmed "old" with 2 entries; by now it holds more.
        let (_tmp, s, d) = setup();
        fs::create_dir(d.join("old")).unwrap();
        fs::write(d.join("old/1"), b"x").unwrap();
        let p = plan_of(&s, &d);
        assert_eq!(p.stats.remove_count, 2);
        fs::write(d.join("old/2"), b"x").unwrap();

        let (r, _) = exec_plan(&s, &d, &p, opts(1), &NullSink);

        assert!(d.join("old/2").exists());
        assert_eq!(
            r.skip_reason,
            Some(SkipReason::Unplanned {
                planned: 2,
                found: 3
            })
        );
    }

    #[test]
    fn source_file_named_like_temp_is_not_clobbered() {
        // With deterministic temp names the temp path of the "foo" copy would
        // be the target path of ".dino-copy.tmp.foo".
        let (_tmp, s, d) = setup();
        fs::write(s.join(".dino-copy.tmp.foo"), b"i am a real file").unwrap();
        fs::write(s.join("foo"), b"foo-content").unwrap();

        let (r, errs) = exec(&s, &d, opts(1), &NullSink);

        assert_eq!(r.failed, 0, "{errs:?}");
        assert_eq!(fs::read(d.join("foo")).unwrap(), b"foo-content");
        assert_eq!(
            fs::read(d.join(".dino-copy.tmp.foo")).unwrap(),
            b"i am a real file"
        );
    }

    #[test]
    fn pass_2_itself_refuses_deletions_after_an_unreadable_directory() {
        // main already withholds the deletions after pass 1. This checks the
        // second line of defence: pass 2 on its own, with a full budget.
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, s, d) = setup();
        fs::create_dir_all(s.join("secret")).unwrap();
        fs::create_dir_all(d.join("secret")).unwrap();
        fs::write(d.join("secret/backup.txt"), b"only copy").unwrap();
        fs::set_permissions(s.join("secret"), fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable_for_us = fs::read_dir(s.join("secret")).is_err();

        let (r, _) = exec(&s, &d, opts(1), &NullSink);

        fs::set_permissions(s.join("secret"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(d.join("secret"), fs::Permissions::from_mode(0o755)).unwrap();
        if !unreadable_for_us {
            return; // running as root: permissions do not apply
        }
        assert!(d.join("secret/backup.txt").exists());
        assert_eq!(r.skip_reason, Some(SkipReason::Unreadable));
    }

    #[test]
    fn case_differences_are_renamed_normalization_differences_are_not() {
        let os = |s: &str| OsString::from(s);
        assert!(needs_rename(&os("Foo.txt"), &os("foo.txt")));
        assert!(
            needs_rename(&os("CAF\u{c9}"), &os("cafe\u{301}")),
            "case plus normalization"
        );
        assert!(
            !needs_rename(&os("Caf\u{e9}"), &os("Cafe\u{301}")),
            "NFC vs NFD only"
        );
        assert!(!needs_rename(&os("same"), &os("same")));
    }

    #[test]
    fn non_utf8_names_are_compared_by_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let a = OsStr::from_bytes(b"caf\xe9");
        let b = OsStr::from_bytes(b"CAF\xe9");
        assert!(needs_rename(a, b));
        assert!(!needs_rename(a, a));
    }

    /// Visitor that only records collisions (for `resolve_aliases`).
    #[derive(Default)]
    struct CollisionSpy(Mutex<Vec<PathBuf>>);
    impl<'a> Visitor<'a> for CollisionSpy {
        fn visit(
            &self,
            _: &Walker<'_>,
            _: &Arc<DirNode<'a>>,
            _: &[Rename],
            _: Vec<Child>,
        ) -> Vec<SubDir<'a>> {
            Vec::new()
        }
        fn collision(&self, _: &Walker<'_>, rel: &Path, _why: &str) {
            self.0.lock().unwrap().push(rel.to_path_buf());
        }
    }

    fn file_entry() -> FileEntry {
        FileEntry {
            kind: EntryKind::File,
            size: 1,
            mtime: 0,
            mode: 0o644,
            id: FileId::default(),
        }
    }

    #[test]
    fn two_source_names_for_one_target_entry_are_a_collision() {
        // A case-sensitive source holds "Foo" and "foo", the target can only
        // hold one of them. "foo" pairs by exact name; "Foo" must not be
        // copied over it — it is reported and left out.
        let (_tmp, s, d) = setup();
        fs::write(d.join("foo"), b"x").unwrap();
        if !d.join("FOO").exists() {
            return; // the temp file system is case-sensitive: no aliasing
        }
        let f = Filter::default();
        let r = roots(&s, &d, &f);
        let w = Walker {
            roots: &r,
            unreadable: AtomicU64::new(0),
            observer: None,
        };
        let src: Listing = vec![("Foo".into(), file_entry()), ("foo".into(), file_entry())];
        let dst = read_listing(&d, Path::new(""), &f, None).unwrap();
        let spy = CollisionSpy::default();

        let (src, dst, renames) = w.resolve_aliases(&spy, Path::new(""), src, dst);

        assert_eq!(*spy.0.lock().unwrap(), [PathBuf::from("Foo")]);
        assert_eq!(
            src.iter()
                .map(|(n, _)| n.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["foo"]
        );
        assert_eq!(dst.len(), 1);
        assert!(
            renames.is_empty(),
            "a collision is never resolved by renaming"
        );
    }

    // ---- cancellation -----------------------------------------------------

    fn temp_files(d: &Path) -> Vec<PathBuf> {
        fn rec(dir: &Path, out: &mut Vec<PathBuf>) {
            for e in fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if e.file_name()
                    .to_string_lossy()
                    .starts_with(".dino-copy.tmp.")
                {
                    out.push(p.clone());
                }
                if p.is_dir() && !p.is_symlink() {
                    rec(&p, out);
                }
            }
        }
        let mut out = Vec::new();
        rec(d, &mut out);
        out
    }

    #[test]
    fn a_run_cancelled_up_front_changes_nothing() {
        let (_tmp, s, d) = setup();
        fs::write(s.join("new.txt"), b"x").unwrap();
        fs::write(d.join("stale.txt"), b"x").unwrap();
        let p = plan_of(&s, &d);
        let cancel = AtomicBool::new(true);
        let f = Filter::default();
        let r = Roots {
            cancel: Some(&cancel),
            ..roots(&s, &d, &f)
        };
        let errors = Mutex::new(Vec::new());
        let opts = ExecOptions {
            delete_budget: p.stats.remove_count,
            ..opts(2)
        };

        let report = execute(&r, &p, opts, &NullSink, &errors);

        assert!(report.cancelled);
        assert_eq!(report.failed, 0, "a cancel is not a failure: {errors:?}");
        assert!(!d.join("new.txt").exists());
        assert!(
            d.join("stale.txt").exists(),
            "a cancelled run deletes nothing"
        );
    }

    /// Sink that raises the cancel flag on the first chunk of copied data.
    struct CancelOnFirstBytes<'a>(&'a AtomicBool);
    impl ProgressSink for CancelOnFirstBytes<'_> {
        fn add_bytes(&self, _n: u64) {
            self.0.store(true, Ordering::Relaxed);
        }
        fn set_current(&self, _worker: usize, _name: &str) {}
        fn set_done(&self, _worker: usize) {}
        fn inc_files(&self) {}
    }

    #[test]
    fn a_cancel_interrupts_a_large_copy_and_leaves_no_temp_file() {
        let (_tmp, s, d) = setup();
        // Above the chunked-copy threshold, so the copy checks between chunks.
        fs::write(s.join("big.bin"), vec![7u8; 24 * 1024 * 1024]).unwrap();
        fs::write(d.join("stale.txt"), b"x").unwrap();
        let p = plan_of(&s, &d);
        let cancel = AtomicBool::new(false);
        let f = Filter::default();
        let r = Roots {
            cancel: Some(&cancel),
            ..roots(&s, &d, &f)
        };
        let errors = Mutex::new(Vec::new());
        let opts = ExecOptions {
            delete_budget: p.stats.remove_count,
            ..opts(1)
        };

        let report = execute(&r, &p, opts, &CancelOnFirstBytes(&cancel), &errors);

        assert!(report.cancelled);
        assert_eq!(report.failed, 0, "{errors:?}");
        assert!(
            !d.join("big.bin").exists(),
            "no half-written file under its real name"
        );
        assert!(
            temp_files(&d).is_empty(),
            "temp files left: {:?}",
            temp_files(&d)
        );
        assert!(d.join("stale.txt").exists());
        assert_eq!(
            report.skip_reason,
            Some(SkipReason::Cancelled),
            "the removal found before the cancel is withheld"
        );
    }

    #[test]
    fn a_cancelled_plan_says_so() {
        let (_tmp, s, d) = setup();
        fs::write(s.join("a"), b"x").unwrap();
        let cancel = AtomicBool::new(true);
        let f = Filter::default();
        let r = Roots {
            cancel: Some(&cancel),
            ..roots(&s, &d, &f)
        };
        let opts = PlanOptions {
            scan_jobs: 1,
            checksum: false,
            hash_jobs: 1,
            details: false,
        };

        let p = plan(&r, opts, None);

        assert!(p.cancelled);
        assert_eq!(p.stats, Stats::default(), "nothing was walked");
    }

    // ---- scan details -----------------------------------------------------

    #[test]
    fn details_list_every_planned_change_with_its_path() {
        let (_tmp, s, d) = setup();
        fs::create_dir_all(s.join("new/deep")).unwrap();
        fs::write(s.join("new/deep/f.txt"), b"12345").unwrap();
        fs::write(s.join("x"), b"now a file").unwrap();
        fs::create_dir_all(d.join("x/inner")).unwrap();
        fs::create_dir_all(d.join("old/sub")).unwrap();
        fs::write(d.join("old/sub/g"), b"g").unwrap();
        let f = Filter::default();
        let opts = PlanOptions {
            scan_jobs: 2,
            checksum: false,
            hash_jobs: 2,
            details: true,
        };

        let mut p = plan(&roots(&s, &d, &f), opts, None);
        p.details.sort_by(|a, b| a.path.cmp(&b.path));

        let got: Vec<(&str, &DetailKind)> = p
            .details
            .iter()
            .map(|d| (d.path.to_str().unwrap(), &d.kind))
            .collect();
        assert_eq!(
            got,
            [
                ("new", &DetailKind::Mkdir),
                ("new/deep", &DetailKind::Mkdir),
                ("new/deep/f.txt", &DetailKind::Copy { size: 5 }),
                (
                    "old",
                    &DetailKind::Remove {
                        is_dir: true,
                        entries: 3
                    }
                ),
                ("x", &DetailKind::Replace),
                ("x", &DetailKind::Copy { size: 10 }),
            ]
        );
    }

    #[test]
    fn details_are_off_unless_asked_for() {
        let (_tmp, s, d) = setup();
        fs::write(s.join("a"), b"x").unwrap();

        assert!(plan_of(&s, &d).details.is_empty());
    }

    #[test]
    fn deletions_are_withheld_after_an_incomplete_plan() {
        let (_tmp, s, d) = setup();
        let mut p = plan_of(&s, &d);
        assert!(p.deletions_allowed(false));
        p.unreadable = 1;
        assert!(!p.deletions_allowed(false));
        assert!(p.deletions_allowed(true), "--ignore-errors overrides");
        p.unreadable = 0;
        p.collisions = 1;
        assert!(!p.deletions_allowed(false));
    }

    /// Sink that records the (worker, name) pairs passed to set_current and
    /// which workers were reported as done.
    #[derive(Default)]
    struct NameSpy {
        names: Mutex<Vec<(usize, String)>>,
        done: Mutex<BTreeSet<usize>>,
    }
    impl ProgressSink for NameSpy {
        fn add_bytes(&self, _n: u64) {}
        fn set_current(&self, worker: usize, name: &str) {
            self.names.lock().unwrap().push((worker, name.to_string()));
        }
        fn set_done(&self, worker: usize) {
            self.done.lock().unwrap().insert(worker);
        }
        fn inc_files(&self) {}
    }

    fn twelve_files(s: &Path) {
        for i in 0..12 {
            fs::write(s.join(format!("f{i}.txt")), b"x").unwrap();
        }
    }

    #[test]
    fn every_busy_worker_is_reported_done_once_the_copies_are_over() {
        let (_tmp, s, d) = setup();
        twelve_files(&s);
        let spy = NameSpy::default();

        let (r, errs) = exec(&s, &d, opts(2), &spy);

        assert_eq!(r.failed, 0, "{errs:?}");
        let busy: BTreeSet<usize> = spy.names.lock().unwrap().iter().map(|(w, _)| *w).collect();
        let done = spy.done.lock().unwrap().clone();
        assert!(!busy.is_empty());
        assert!(busy.is_subset(&done), "busy={busy:?} done={done:?}");
    }

    #[test]
    fn each_copy_is_announced_with_its_worker_index() {
        let (_tmp, s, d) = setup();
        twelve_files(&s);
        let spy = NameSpy::default();

        let (r, errs) = exec(&s, &d, opts(2), &spy);

        assert_eq!(r.failed, 0, "{errs:?}");
        let seen = spy.names.lock().unwrap();
        assert_eq!(seen.len(), 12, "every copy must be announced");
        assert!(seen.iter().all(|(w, _)| *w < 2), "{seen:?}");
    }

    #[test]
    fn progress_reports_the_path_relative_to_the_source_root() {
        let (_tmp, s, d) = setup();
        fs::create_dir_all(s.join("sub/deep")).unwrap();
        fs::write(s.join("sub/deep/b.txt"), b"x").unwrap();
        let spy = NameSpy::default();

        let (r, errs) = exec(&s, &d, opts(1), &spy);

        assert_eq!(r.failed, 0, "{errs:?}");
        let seen = spy.names.lock().unwrap();
        assert_eq!(
            seen.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(),
            ["sub/deep/b.txt"]
        );
    }
}
