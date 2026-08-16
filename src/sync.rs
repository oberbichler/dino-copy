use crate::plan::Action;
use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Registry of the temp files currently in flight. Read by the Ctrl-C handler
/// to clean up half-written temp files.
fn active_temps() -> &'static Mutex<HashSet<PathBuf>> {
    static REGISTRY: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashSet::new()))
}

fn register_temp(p: &Path) {
    active_temps().lock().unwrap().insert(p.to_path_buf());
}

fn unregister_temp(p: &Path) {
    active_temps().lock().unwrap().remove(p);
}

/// Removes every still-registered temp file. Intended for the Ctrl-C handler.
pub fn cleanup_temps() {
    if let Ok(set) = active_temps().lock() {
        for p in set.iter() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Callback invoked after each copied file chunk (for progress reporting).
pub trait ProgressSink: Sync {
    fn add_bytes(&self, n: u64);
    fn set_current(&self, name: &str);
    fn inc_files(&self);
}

/// No-op sink for tests.
#[cfg(test)]
pub struct NullSink;
#[cfg(test)]
impl ProgressSink for NullSink {
    fn add_bytes(&self, _n: u64) {}
    fn set_current(&self, _name: &str) {}
    fn inc_files(&self) {}
}

/// Copies a single file from `src` to `dst` via temp+rename and applies the
/// source's mtime/atime and permissions.
/// With `fsync`, the file contents are flushed to disk before the rename.
pub fn copy_file(src: &Path, dst: &Path, sink: &dyn ProgressSink, fsync: bool) -> anyhow::Result<u64> {
    if let Some(name) = dst.file_name().and_then(|n| n.to_str()) {
        sink.set_current(name);
    }
    // Directories are created by `execute` in phase 1; no create_dir_all per
    // file here (saves one stat per copy). copy_into_temp reads the metadata
    // via fstat from the open descriptor instead of a second path-based stat.
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));

    // Unique temp name: PID + a process-wide counter prevent collisions between
    // parallel copies (e.g. with non-UTF8 names) and with real source files that
    // happen to be called ".dino-copy.tmp.*".
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let prefix = format!(".dino-copy.tmp.{}.{}.", std::process::id(), seq);
    // The original name is pure cosmetics (uniqueness comes from PID+counter).
    // Truncate it so the temp name does not exceed NAME_MAX (255 bytes).
    let name = dst.file_name().and_then(|n| n.to_str()).unwrap_or("f");
    let name = truncate_utf8(name, 255usize.saturating_sub(prefix.len()));
    let tmp = parent.join(format!("{prefix}{name}"));

    // Register the temp file so Ctrl-C or an error can clean it up.
    register_temp(&tmp);
    let result = copy_into_temp(src, &tmp, sink, fsync);
    match result {
        Ok(total) => {
            if let Err(e) = std::fs::rename(&tmp, dst) {
                let _ = std::fs::remove_file(&tmp);
                unregister_temp(&tmp);
                return Err(e.into());
            }
            unregister_temp(&tmp);
            sink.inc_files();
            Ok(total)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            unregister_temp(&tmp);
            Err(e)
        }
    }
}

/// Truncates `s` to at most `max_bytes` bytes without cutting UTF-8 sequences.
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Threshold above which we use the chunked copy path (with progress) instead
/// of a single large read. Small files do not benefit from fine-grained
/// progress and are fastest with std::io::copy.
const CHUNK_PROGRESS_THRESHOLD: u64 = 8 * 1024 * 1024;

/// Chunk size of the double-buffered large-file copy.
const COPY_CHUNK: usize = 2 * 1024 * 1024;

/// Buffer size for files below [`CHUNK_PROGRESS_THRESHOLD`].
///
/// Measured on 200 files of 4 MB each with `--jobs 1`: 8 KiB (the default of
/// `std::io::copy`) 1343 MB/s, 64 KiB 3502 MB/s, 256 KiB 4230 MB/s,
/// 1 MiB 4380 MB/s. So the knee lies below 256 KiB; beyond that there is
/// nothing left to gain, while the buffer is paid per copy thread.
const SMALL_COPY_BUF: usize = 256 * 1024;

thread_local! {
    /// Reused copy buffer for the path without chunk progress.
    ///
    /// `std::io::copy` works with 8 KiB here: the file-to-file specialization
    /// (`copy_file_range`) only exists on Linux, and on macOS the generic loop
    /// with `DEFAULT_BUF_SIZE` takes over. A 4 MB file thus breaks down into
    /// roughly 500 read/write pairs.
    ///
    /// thread_local so that every copy thread reuses its buffer instead of
    /// allocating per file. Filled lazily so threads that never copy such a
    /// file do not occupy the memory.
    static SMALL_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// Number of buffers rotating between reader and writer.
const COPY_BUFFERS: usize = 4;

/// Writes `src` into the temp file `tmp` and applies permissions + times.
fn copy_into_temp(
    src: &Path,
    tmp: &Path,
    sink: &dyn ProgressSink,
    fsync: bool,
) -> anyhow::Result<u64> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut reader = std::fs::File::open(src)?;
    // Read the metadata from the open descriptor (fstat) instead of via an
    // additional path-based stat.
    let meta = reader.metadata()?;
    // Create the temp file restrictively (0600): with the umask default (0644)
    // the contents of a protected source would be readable by others while the
    // copy is running. The real source permissions are set after writing
    // (below). create_new (O_EXCL) instead of create+truncate: O_EXCL does NOT
    // follow a symlink pre-placed at the temp path. Without O_EXCL an attacker
    // with write access to the target could redirect the writes through such a
    // symlink (the temp name is predictable from PID+counter).
    let open_temp = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(tmp)
    };
    let mut writer = match open_temp() {
        Ok(f) => f,
        // Parent missing (execute normally creates it in phase 1): create it
        // once and retry. Also covers direct copy_file calls.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = tmp.parent() {
                std::fs::create_dir_all(parent)?;
            }
            open_temp()?
        }
        // Something already exists at the temp path: a leftover from a crashed
        // run (PID reuse) or a pre-placed symlink. Remove the entry ITSELF
        // (remove_file deletes a symlink, never its target) and retry.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(tmp)?;
            open_temp()?
        }
        Err(e) => return Err(e.into()),
    };
    let size = meta.len();

    let total = if size <= CHUNK_PROGRESS_THRESHOLD {
        // Small and medium files: one loop over the thread_local buffer.
        // Progress is reported as a single batch.
        copy_small(&mut reader, &mut writer, sink)?
    } else {
        // Large files: copy double-buffered (reading the source and writing to
        // the target overlap) and report progress per chunk.
        copy_chunked(&mut reader, &mut writer, sink)?
    };

    writer.flush()?;
    // Optional: persist the contents to disk before the atomic rename. Protects
    // against power loss, but costs throughput.
    if fsync {
        writer.sync_all()?;
    }
    drop(writer);

    // Apply the source's permissions + mtime/atime.
    std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    let atime = filetime::FileTime::from_last_access_time(&meta);
    filetime::set_file_times(tmp, atime, mtime)?;
    Ok(total)
}

/// Copies `reader` -> `writer` via the reused [`SMALL_BUF`] and reports the
/// total amount at the end as a single batch.
fn copy_small(
    reader: &mut std::fs::File,
    writer: &mut std::fs::File,
    sink: &dyn ProgressSink,
) -> std::io::Result<u64> {
    use std::io::{Read, Write};

    let total = SMALL_BUF.with(|cell| -> std::io::Result<u64> {
        let mut buf = cell.borrow_mut();
        if buf.len() < SMALL_COPY_BUF {
            buf.resize(SMALL_COPY_BUF, 0);
        }
        let mut total = 0u64;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    writer.write_all(&buf[..n])?;
                    total += n as u64;
                }
                // Like std::io::copy: EINTR is not an error, just a retry.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    })?;
    sink.add_bytes(total);
    Ok(total)
}

/// Copies `reader` -> `writer` double-buffered and reports progress per written
/// chunk.
///
/// A reader thread reads ahead from the source into recycled buffers while this
/// thread writes already-read buffers to the target. Source and target
/// typically live on different disks — this way their I/O overlaps instead of
/// one disk waiting while the other works. The buffers rotate back to the
/// reader through the `empty` channel, so there is no allocation per chunk
/// after the warm-up.
fn copy_chunked(
    reader: &mut std::fs::File,
    writer: &mut std::fs::File,
    sink: &dyn ProgressSink,
) -> std::io::Result<u64> {
    use std::io::{Read, Write};
    use std::sync::mpsc::sync_channel;

    std::thread::scope(|s| {
        // The channels MUST live in the scope body: when the writer path leaves
        // the closure (EOF or a write error), rx_full/tx_empty are dropped
        // BEFORE the implicit join of the reader thread and release it from
        // recv/send. Were they declared outside thread::scope, a write error
        // (e.g. a full target disk) would deadlock: scope joins first, the
        // channel ends would still be alive, and the reader would hang in
        // rx_empty.recv() forever.
        //
        // full:  filled buffers reader -> writer (or a read error).
        // empty: emptied buffers writer -> reader for reuse.
        let (tx_full, rx_full) = sync_channel::<std::io::Result<(Vec<u8>, usize)>>(COPY_BUFFERS);
        let (tx_empty, rx_empty) = sync_channel::<Vec<u8>>(COPY_BUFFERS);
        for _ in 0..COPY_BUFFERS {
            // Capacity == COPY_BUFFERS, so no send blocks or fails.
            tx_empty.send(vec![0u8; COPY_CHUNK]).expect("empty channel has capacity");
        }

        // Reader thread: reads ahead into recycled buffers.
        s.spawn(move || {
            while let Ok(mut buf) = rx_empty.recv() {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: tx_full is dropped on return.
                    Ok(n) => {
                        // An error means the writer is gone (error path) -> stop.
                        if tx_full.send(Ok((buf, n))).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx_full.send(Err(e));
                        break;
                    }
                }
            }
        });

        // This thread writes while the reader is already reading the next
        // chunk. When the loop is left (EOF or an error), the rx_full/tx_empty
        // declared above in the scope body are dropped and release the reader
        // from any pending recv/send -> thread::scope joins cleanly.
        let mut total = 0u64;
        loop {
            match rx_full.recv() {
                Err(_) => break,              // Reader done (EOF) -> everything written.
                Ok(Err(e)) => return Err(e),  // Pass the read error through.
                Ok(Ok((buf, n))) => {
                    writer.write_all(&buf[..n])?;
                    total += n as u64;
                    sink.add_bytes(n as u64);
                    let _ = tx_empty.send(buf); // back to the reader (ignored if done).
                }
            }
        }
        Ok(total)
    })
}

/// Maximum number of stored error messages. The total count keeps rising in
/// `failed` independently — this prevents unbounded memory when, say, a
/// read-only target makes millions of actions fail.
const MAX_STORED_ERRORS: usize = 100;

fn record_error(errors: &std::sync::Mutex<Vec<String>>, msg: String) {
    let mut list = errors.lock().unwrap();
    if list.len() < MAX_STORED_ERRORS {
        list.push(msg);
    } else if list.len() == MAX_STORED_ERRORS {
        list.push("... further errors suppressed".to_string());
    }
}

/// Runtime options for [`execute`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ExecOptions {
    /// Copy parallelism (0 = automatic, based on the target volume).
    pub jobs: usize,
    /// Write every file with fsync before the rename.
    pub fsync: bool,
    /// Delete even when actions failed beforehand.
    pub ignore_errors: bool,
}

/// Result of an execution run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExecReport {
    /// Number of failed actions.
    pub failed: u64,
    /// Deletions that were skipped because of earlier errors.
    pub skipped_deletes: u64,
}

/// Runs all actions on source_root/dest_root. Copying happens in parallel.
/// Errors are collected, not fatal; the report states how many there were.
pub fn execute(
    source_root: &Path,
    dest_root: &Path,
    actions: &[Action],
    opts: ExecOptions,
    sink: &dyn ProgressSink,
    errors: &std::sync::Mutex<Vec<String>>,
) -> ExecReport {
    use rayon::prelude::*;

    let failed = AtomicU64::new(0);

    // Phase 0: remove type conflicts on the target (file/dir/symlink of the
    // wrong type) so that CreateDir/Copy/Symlink do not collide afterwards.
    for a in actions {
        if let Action::RemoveConflict(rel) = a {
            let target = dest_root.join(rel);
            if let Err(e) = remove_any(&target) {
                failed.fetch_add(1, Ordering::Relaxed);
                record_error(errors, format!("remove conflict {}: {e}", target.display()));
            }
        }
    }

    // Phase 1: create directories (serially, respecting the order).
    for a in actions {
        if let Action::CreateDir(rel) = a {
            let target = dest_root.join(rel);
            if let Err(e) = std::fs::create_dir_all(&target) {
                failed.fetch_add(1, Ordering::Relaxed);
                record_error(errors, format!("mkdir {}: {e}", target.display()));
            }
        }
    }

    // Phase 2: copy files in parallel.
    let copies: Vec<&Action> = actions
        .iter()
        .filter(|a| matches!(a, Action::Copy { .. }))
        .collect();

    let effective_jobs =
        if opts.jobs > 0 { opts.jobs } else { default_copy_jobs(dest_root) };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(effective_jobs)
        .build();

    let run_copies = |a: &&Action| {
        if let Action::Copy { rel, .. } = a {
            let src = source_root.join(rel);
            let dst = dest_root.join(rel);
            if let Err(e) = copy_file(&src, &dst, sink, opts.fsync) {
                failed.fetch_add(1, Ordering::Relaxed);
                record_error(errors, format!("copy {}: {e}", rel.display()));
            }
        }
    };

    match pool {
        Ok(p) => p.install(|| copies.par_iter().for_each(run_copies)),
        Err(_) => copies.par_iter().for_each(run_copies),
    }

    // Phase 2b: create symlinks (serially). An existing target is replaced.
    for a in actions {
        if let Action::CreateSymlink { rel, target, mtime } = a {
            let dst = dest_root.join(rel);
            if let Err(e) = create_symlink(target, &dst, *mtime) {
                failed.fetch_add(1, Ordering::Relaxed);
                record_error(errors, format!("symlink {}: {e}", dst.display()));
            } else {
                sink.inc_files();
            }
        }
    }

    // Phase 2c: align the permissions of files whose contents are unchanged
    // (serially; changes no directory mtimes).
    for a in actions {
        if let Action::SetFileMeta { rel, mode } = a {
            let target = dest_root.join(rel);
            if let Err(e) =
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(*mode))
            {
                failed.fetch_add(1, Ordering::Relaxed);
                record_error(errors, format!("set file meta {}: {e}", target.display()));
            }
        }
    }

    // Phase 2d: with --fsync, synchronize the changed directories themselves.
    // Without that, the rename from phase 2 does not survive a power loss: the
    // file would be written in full but its directory entry would not — it
    // would simply be missing.
    if opts.fsync {
        for dir in dirs_to_sync(dest_root, actions) {
            if let Err(e) = sync_dir(&dir) {
                failed.fetch_add(1, Ordering::Relaxed);
                record_error(errors, format!("fsync dir {}: {e}", dir.display()));
            }
        }
    }

    // Phase 3: deletes (files before directories, order taken from actions).
    //
    // If something failed earlier, the mirror is incomplete: then nothing may
    // be removed in the target. A file whose source could not be read right now
    // may be the only remaining reachable version in the target — deleting it
    // would be the worst possible outcome.
    let mut skipped_deletes = 0u64;
    let is_delete = |a: &&Action| matches!(a, Action::DeleteFile(_) | Action::DeleteDir(_));
    if failed.load(Ordering::Relaxed) > 0 && !opts.ignore_errors {
        skipped_deletes = actions.iter().filter(is_delete).count() as u64;
    } else {
        for a in actions {
            match a {
                Action::DeleteFile(rel) => {
                    let target = dest_root.join(rel);
                    if let Err(e) = std::fs::remove_file(&target) {
                        failed.fetch_add(1, Ordering::Relaxed);
                        record_error(errors, format!("rm {}: {e}", target.display()));
                    }
                }
                Action::DeleteDir(rel) => {
                    let target = dest_root.join(rel);
                    if let Err(e) = std::fs::remove_dir_all(&target) {
                        failed.fetch_add(1, Ordering::Relaxed);
                        record_error(errors, format!("rmdir {}: {e}", target.display()));
                    }
                }
                _ => {}
            }
        }
    }

    // Phase 4: set directory metadata (deepest first, order taken from actions).
    for a in actions {
        if let Action::SetDirMeta { rel, mtime, mode } = a {
            let target = dest_root.join(rel);
            if let Err(e) = set_dir_meta(&target, *mtime, *mode) {
                failed.fetch_add(1, Ordering::Relaxed);
                record_error(errors, format!("set dir meta {}: {e}", target.display()));
            }
        }
    }

    ExecReport { failed: failed.load(Ordering::Relaxed), skipped_deletes }
}

/// Picks a sensible default copy parallelism depending on the target volume.
///
/// Rotating HDDs have a physical write head: too many parallel writers cause
/// seek thrashing and are often slower than 1–2. SSDs, in contrast, benefit
/// from high parallelism. If the type cannot be determined, the more
/// conservative HDD value is chosen (safer for the expected use case of
/// "external disks").
pub fn default_copy_jobs(dest_root: &Path) -> usize {
    match is_ssd(dest_root) {
        Some(true) => std::thread::available_parallelism().map_or(4, |n| n.get()),
        // HDD or unknown: conservative, but enough for read/write pipelining.
        _ => 2,
    }
}

/// Determines via `diskutil info` whether the volume under `path` is an SSD.
/// `Some(true)`/`Some(false)` on a clear answer, `None` when unknown.
#[cfg(target_os = "macos")]
fn is_ssd(path: &Path) -> Option<bool> {
    let out = std::process::Command::new("diskutil")
        .arg("info")
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        // A line looks like "   Solid State:               Yes"
        if let Some(rest) = line.trim().strip_prefix("Solid State:") {
            let v = rest.trim().to_ascii_lowercase();
            if v.starts_with("yes") {
                return Some(true);
            }
            if v.starts_with("no") {
                return Some(false);
            }
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn is_ssd(_path: &Path) -> Option<bool> {
    // No detection on other platforms – the conservative default applies.
    None
}

/// Removes a target regardless of its type (file, symlink or directory).
fn remove_any(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Creates a symlink `dst` -> `target` and applies the source's mtime.
/// An existing `dst` is removed beforehand.
fn create_symlink(target: &Path, dst: &Path, mtime: i64) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::symlink_metadata(dst).is_ok() {
        remove_any(dst)?;
    }
    std::os::unix::fs::symlink(target, dst)?;
    // Set the mtime of the link itself (not of the link target).
    let ft = filetime::FileTime::from_unix_time(mtime, 0);
    filetime::set_symlink_file_times(dst, ft, ft)
}

/// Directories whose entries are changed by `actions`.
///
/// With `--fsync` these directories have to be synchronized themselves:
/// `sync_all()` on the temp file only makes its *contents* durable, and the
/// subsequent `rename` is a directory update. Without an fsync on the
/// directory, a copied file can be missing after a power loss.
///
/// Deletions are left out: if a deleted entry reappears after a crash, the next
/// run removes it again — that is not data loss.
fn dirs_to_sync(dest_root: &Path, actions: &[Action]) -> std::collections::BTreeSet<PathBuf> {
    let mut dirs = std::collections::BTreeSet::new();
    for a in actions {
        let rel = match a {
            Action::Copy { rel, .. }
            | Action::CreateSymlink { rel, .. }
            | Action::CreateDir(rel) => rel,
            _ => continue,
        };
        if let Some(parent) = dest_root.join(rel).parent() {
            dirs.insert(parent.to_path_buf());
        }
    }
    dirs
}

/// fsync on a directory: makes the `rename` and `mkdir` operations performed
/// inside it durable.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Applies the mode and mtime of the source root directory to the target.
///
/// The root appears in neither map — `scan_tree` skips it — and therefore has
/// no `SetDirMeta` action. The call has to happen after all writes, because
/// every change in the target touches its mtime.
pub fn mirror_root_meta(source_root: &Path, dest_root: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(source_root)?;
    set_dir_meta(dest_root, meta.mtime(), meta.mode() & 0o7777)
}

/// True when the mode or mtime of the two root directories differ.
///
/// Necessary so that pure metadata drift of the root does not pass as "nothing
/// to do": it produces no action at all and would otherwise persist forever. If
/// one side cannot be read (target missing in a dry run), that counts as a
/// difference.
pub fn root_meta_differs(source_root: &Path, dest_root: &Path, mtime_tol: i64) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(src), Ok(dst)) =
        (std::fs::metadata(source_root), std::fs::metadata(dest_root))
    else {
        return true;
    };
    (src.mtime() - dst.mtime()).abs() > mtime_tol
        || (src.mode() & 0o7777) != (dst.mode() & 0o7777)
}

/// Sets the mtime + permissions of a directory.
///
/// The order matters: the mtime MUST be set before the permissions. filetime
/// opens the path (read-only first, write-only as a fallback); a restrictive
/// mode such as 0o000 — `.Trashes` on macOS volumes, for instance — would
/// otherwise lock us out of our own target, and the write-only fallback returns
/// EISDIR on a directory. The reverse ordering is unproblematic because
/// SetDirMeta runs as the last phase anyway, and within it deepest-first:
/// nothing needs to be written into an already sealed directory afterwards.
fn set_dir_meta(dir: &Path, mtime: i64, mode: u32) -> std::io::Result<()> {
    let ft = filetime::FileTime::from_unix_time(mtime, 0);
    filetime::set_file_mtime(dir, ft)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Action;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[test]
    fn copy_file_preserves_mtime_and_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.txt");
        let dst = tmp.path().join("dst.txt");
        fs::write(&src, b"content").unwrap();
        // set the mtime
        let ft = filetime::FileTime::from_unix_time(1_000_000, 0);
        filetime::set_file_mtime(&src, ft).unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).unwrap();

        let n = copy_file(&src, &dst, &NullSink, false).unwrap();
        assert_eq!(n, 7);
        assert_eq!(fs::read(&dst).unwrap(), b"content");

        let sm = fs::metadata(&src).unwrap();
        let dm = fs::metadata(&dst).unwrap();
        assert_eq!(sm.mtime(), dm.mtime());
        assert_eq!(sm.mode() & 0o7777, dm.mode() & 0o7777);
    }

    #[test]
    fn copy_file_handles_large_file_chunked_path() {
        // File larger than CHUNK_PROGRESS_THRESHOLD -> the chunked path is used.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("big.bin");
        let dst = tmp.path().join("big.out");
        let size = (CHUNK_PROGRESS_THRESHOLD + 3) as usize;
        let data = vec![0xABu8; size];
        fs::write(&src, &data).unwrap();

        let n = copy_file(&src, &dst, &NullSink, false).unwrap();
        assert_eq!(n, size as u64);
        assert_eq!(fs::read(&dst).unwrap(), data);
    }

    #[test]
    fn chunked_copy_write_error_fails_fast_without_deadlock() {
        // A write error in the middle of the large-file path (e.g. a full
        // target disk, ENOSPC) must come back as an error. Regression:
        // copy_chunked used to deadlock because the channels lived outside
        // thread::scope and the reader thread waited forever for recycled
        // buffers after the writer returned an error.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("big.bin");
        // More content than all buffers hold, so the reader is guaranteed to
        // still be reading/waiting when the writer fails.
        fs::write(&src, vec![1u8; COPY_CHUNK * (COPY_BUFFERS + 2)]).unwrap();

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let src2 = src.clone();
        std::thread::spawn(move || {
            let mut reader = fs::File::open(&src2).unwrap();
            // A read-only handle as the "writer": write_all fails immediately.
            let mut writer = fs::File::open(&src2).unwrap();
            let res = copy_chunked(&mut reader, &mut writer, &NullSink);
            let _ = done_tx.send(res.is_err());
        });
        match done_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(was_err) => assert!(was_err, "a write error must yield Err"),
            Err(_) => panic!("copy_chunked hangs on a write error (deadlock regression)"),
        }
    }

    #[test]
    fn copy_file_large_preserves_byte_order() {
        // Double-buffered path across many chunks: a position-dependent pattern
        // exposes any swapping/losing of chunks that a test with all-identical
        // bytes would not notice.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("pattern.bin");
        let dst = tmp.path().join("pattern.out");
        // Several COPY_CHUNKs large and not a clean multiple -> a final partial
        // chunk.
        let size = COPY_CHUNK * 5 + 12345;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &data).unwrap();

        let n = copy_file(&src, &dst, &NullSink, false).unwrap();
        assert_eq!(n, size as u64);
        assert_eq!(fs::read(&dst).unwrap(), data, "byte order must be preserved");
    }

    #[test]
    fn copy_file_below_threshold_preserves_byte_order() {
        // The "small" path (<= CHUNK_PROGRESS_THRESHOLD) has so far only been
        // tested with tiny files. This file is several MB but below the
        // threshold, and not a clean multiple of any buffer size — so it covers
        // several copy iterations plus a remainder block. Position-dependent
        // pattern so a swapped or lost block stands out.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("mid.bin");
        let dst = tmp.path().join("mid.out");
        let size = 3 * 1024 * 1024 + 4321;
        assert!((size as u64) < CHUNK_PROGRESS_THRESHOLD, "must take the small path");
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &data).unwrap();

        let n = copy_file(&src, &dst, &NullSink, false).unwrap();

        assert_eq!(n, size as u64);
        assert_eq!(fs::read(&dst).unwrap(), data, "byte order must be preserved");
    }

    #[test]
    fn copy_file_reports_all_bytes_to_the_progress_sink() {
        // The progress must not get lost when the copy path is reworked.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("s.bin");
        let dst = tmp.path().join("d.bin");
        let size = 1024 * 1024 + 7;
        fs::write(&src, vec![3u8; size]).unwrap();
        let spy = ByteSpy::default();

        copy_file(&src, &dst, &spy, false).unwrap();

        assert_eq!(
            spy.bytes.load(Ordering::Relaxed),
            size as u64,
            "the reported bytes must match the file size"
        );
    }

    #[test]
    fn default_copy_jobs_is_sane() {
        // For a tempdir the detection yields either SSD (CPU count) or the
        // conservative fallback (2) – in any case >= 1.
        let tmp = tempfile::tempdir().unwrap();
        let jobs = default_copy_jobs(tmp.path());
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        assert!(jobs >= 1, "jobs must be at least 1, was {jobs}");
        assert!(jobs <= cpus.max(2), "jobs unexpectedly high: {jobs}");
    }

    #[test]
    fn execute_runs_all_action_types() {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().join("s");
        let d = tmp.path().join("d");
        fs::create_dir_all(s.join("keep")).unwrap();
        fs::write(s.join("keep/a.txt"), b"aaa").unwrap();
        fs::create_dir_all(d.join("old")).unwrap();
        fs::write(d.join("stale.txt"), b"x").unwrap();

        let actions = vec![
            Action::CreateDir(PathBuf::from("keep")),
            Action::Copy { rel: PathBuf::from("keep/a.txt"), size: 3 },
            Action::DeleteFile(PathBuf::from("stale.txt")),
            Action::DeleteDir(PathBuf::from("old")),
        ];
        let errors = Mutex::new(Vec::new());
        let report = execute(&s, &d, &actions, ExecOptions { jobs: 2, ..Default::default() }, &NullSink, &errors);

        assert_eq!(report.failed, 0);
        assert_eq!(fs::read(d.join("keep/a.txt")).unwrap(), b"aaa");
        assert!(!d.join("stale.txt").exists());
        assert!(!d.join("old").exists());
    }

    /// Sink that sums up the reported bytes.
    #[derive(Default)]
    struct ByteSpy {
        bytes: AtomicU64,
    }
    impl ProgressSink for ByteSpy {
        fn add_bytes(&self, n: u64) {
            self.bytes.fetch_add(n, Ordering::Relaxed);
        }
        fn set_current(&self, _name: &str) {}
        fn inc_files(&self) {}
    }

    /// Sink that collects the temp file's permissions on the first chunk.
    struct TempModeSpy {
        dir: PathBuf,
        seen_mode: Mutex<Option<u32>>,
    }
    impl ProgressSink for TempModeSpy {
        fn add_bytes(&self, _n: u64) {
            let mut seen = self.seen_mode.lock().unwrap();
            if seen.is_some() {
                return;
            }
            if let Ok(rd) = fs::read_dir(&self.dir) {
                for e in rd.filter_map(|e| e.ok()) {
                    if e.file_name().to_string_lossy().starts_with(".dino-copy.tmp.") {
                        if let Ok(m) = e.metadata() {
                            *seen = Some(m.mode() & 0o7777);
                        }
                    }
                }
            }
        }
        fn set_current(&self, _name: &str) {}
        fn inc_files(&self) {}
    }

    #[test]
    fn temp_file_is_not_world_readable_during_copy() {
        // A 0600 source file must not be briefly readable with the umask
        // default (0644) while being copied (info leak on multi-user systems).
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("secret.bin");
        let dst = tmp.path().join("out").join("secret.bin");
        fs::create_dir_all(tmp.path().join("out")).unwrap();
        // Large enough for the chunked path so the sink can check meanwhile.
        let data = vec![7u8; (CHUNK_PROGRESS_THRESHOLD + 1) as usize];
        fs::write(&src, &data).unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o600)).unwrap();

        let spy = TempModeSpy {
            dir: tmp.path().join("out"),
            seen_mode: Mutex::new(None),
        };
        copy_file(&src, &dst, &spy, false).unwrap();

        let seen = spy.seen_mode.lock().unwrap().expect("temp file not observed");
        assert_eq!(
            seen, 0o600,
            "the temp file must be created restrictively (0600), was {seen:o}"
        );
        // The final permissions match the source.
        assert_eq!(fs::metadata(&dst).unwrap().mode() & 0o7777, 0o600);
    }

    #[test]
    fn copies_file_with_name_near_name_max() {
        // A 250-character file name is legal (NAME_MAX=255). The temp prefix
        // ".dino-copy.tmp.{pid}.{seq}." must not push the temp name past the
        // limit, otherwise the copy fails with ENAMETOOLONG.
        let tmp = tempfile::tempdir().unwrap();
        let long_name = "x".repeat(250);
        let src = tmp.path().join(&long_name);
        let dst_dir = tmp.path().join("out");
        fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join(&long_name);
        fs::write(&src, b"payload").unwrap();

        let n = copy_file(&src, &dst, &NullSink, false)
            .expect("a copy with a long name must succeed");
        assert_eq!(n, 7);
        assert_eq!(fs::read(&dst).unwrap(), b"payload");
    }

    #[test]
    fn copies_file_with_long_multibyte_name() {
        // Like above, but with multibyte characters: truncating the name suffix
        // must not cut UTF-8 sequences in the middle.
        let tmp = tempfile::tempdir().unwrap();
        let long_name = "ü".repeat(120); // 240 bytes, 120 characters
        let src = tmp.path().join(&long_name);
        let dst_dir = tmp.path().join("out");
        fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join(&long_name);
        fs::write(&src, b"payload").unwrap();

        let n = copy_file(&src, &dst, &NullSink, false)
            .expect("a copy with a long multibyte name must succeed");
        assert_eq!(n, 7);
        assert_eq!(fs::read(&dst).unwrap(), b"payload");
    }

    #[test]
    fn preplaced_symlink_at_temp_path_is_not_followed() {
        // Attacker scenario: a symlink to a foreign file already sits at the
        // (PID+counter-predictable) temp path. Without O_EXCL, opening would
        // follow the link and redirect the writes into the link target.
        // We cover all plausible sequence numbers, since the counter is
        // process-global and shared with tests running in parallel.
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        fs::create_dir_all(&out).unwrap();
        let victim = tmp.path().join("victim.txt");
        fs::write(&victim, b"untouchable").unwrap();
        let src = tmp.path().join("src.txt");
        fs::write(&src, b"new content").unwrap();
        let dst = out.join("target.txt");
        let pid = std::process::id();
        for seq in 0..4096u64 {
            let _ = std::os::unix::fs::symlink(
                &victim,
                out.join(format!(".dino-copy.tmp.{pid}.{seq}.target.txt")),
            );
        }

        copy_file(&src, &dst, &NullSink, false).unwrap();

        assert_eq!(
            fs::read(&victim).unwrap(),
            b"untouchable",
            "writes must never be redirected through a pre-placed symlink"
        );
        assert_eq!(fs::read(&dst).unwrap(), b"new content");
    }

    #[test]
    fn source_file_named_like_temp_is_not_clobbered() {
        // The source contains a file that looks like a dino-copy temp name.
        // With deterministic temp names the temp path of the "foo" copy would
        // be identical to the target path of ".dino-copy.tmp.foo" -> the file
        // would disappear.
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().join("s");
        let d = tmp.path().join("d");
        fs::create_dir_all(&s).unwrap();
        fs::create_dir_all(&d).unwrap();
        fs::write(s.join(".dino-copy.tmp.foo"), b"i am a real file").unwrap();
        fs::write(s.join("foo"), b"foo-content").unwrap();

        let actions = vec![
            Action::Copy { rel: PathBuf::from(".dino-copy.tmp.foo"), size: 16 },
            Action::Copy { rel: PathBuf::from("foo"), size: 11 },
        ];
        let errors = Mutex::new(Vec::new());
        let report = execute(&s, &d, &actions, ExecOptions { jobs: 1, ..Default::default() }, &NullSink, &errors);

        assert_eq!(report.failed, 0, "errors: {:?}", errors.lock().unwrap());
        assert_eq!(fs::read(d.join("foo")).unwrap(), b"foo-content");
        assert_eq!(
            fs::read(d.join(".dino-copy.tmp.foo")).unwrap(),
            b"i am a real file",
            "a file with a temp-like name must not be overwritten/moved"
        );
    }

    #[test]
    fn failed_copy_leaves_no_temp_file() {
        // The dst parent is a file → create_dir_all(parent) fails, no temp file
        // may be left behind and the registry must be empty.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.txt");
        fs::write(&src, b"data").unwrap();
        // "blocker" is a file; we try to copy to "blocker/child.txt".
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let dst = blocker.join("child.txt");

        let res = copy_file(&src, &dst, &NullSink, false);
        assert!(res.is_err(), "a copy into a file-as-directory must fail");
        // No temp file in the tmp root.
        let leftover: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".dino-copy.tmp."))
            .collect();
        assert!(leftover.is_empty(), "no stray temp files allowed");
        // Note: the global temp registry is not checked, since it is shared
        // with tests running in parallel (race). The disk check above is the
        // meaningful invariant.
    }

    #[test]
    fn sync_dir_succeeds_on_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), b"x").unwrap();

        sync_dir(tmp.path()).expect("fsync on a directory must work");
    }

    #[test]
    fn dirs_to_sync_collects_parents_of_created_entries() {
        let dest = PathBuf::from("/target");
        let actions = vec![
            Action::Copy { rel: PathBuf::from("sub/a.txt"), size: 1 },
            Action::Copy { rel: PathBuf::from("b.txt"), size: 1 },
            Action::CreateSymlink {
                rel: PathBuf::from("sub/link"),
                target: PathBuf::from("a.txt"),
                mtime: 0,
            },
            Action::CreateDir(PathBuf::from("new")),
        ];

        let dirs = dirs_to_sync(&dest, &actions);

        // sub appears twice and must only be synchronized once.
        assert_eq!(
            dirs.into_iter().collect::<Vec<_>>(),
            vec![PathBuf::from("/target"), PathBuf::from("/target/sub")]
        );
    }

    #[test]
    fn dirs_to_sync_ignores_actions_that_create_nothing() {
        let dest = PathBuf::from("/target");
        let actions = vec![
            Action::DeleteFile(PathBuf::from("gone.txt")),
            Action::DeleteDir(PathBuf::from("gone")),
            Action::SetFileMeta { rel: PathBuf::from("a.txt"), mode: 0o644 },
            Action::SetDirMeta { rel: PathBuf::from("sub"), mtime: 0, mode: 0o755 },
        ];

        assert!(dirs_to_sync(&dest, &actions).is_empty());
    }
}
