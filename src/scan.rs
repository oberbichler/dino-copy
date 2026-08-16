use crate::filter::Filter;
use crate::plan::{EntryKind, FileEntry};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Result of a tree scan.
pub struct Scan {
    pub entries: BTreeMap<PathBuf, FileEntry>,
    /// Directories whose contents could not be read, with the error text.
    /// Their contents are missing from `entries` even though they exist — so
    /// the caller must not conclude that those directories are empty.
    pub unreadable: Vec<(PathBuf, String)>,
}

/// Walks `root` in parallel and returns all entries relative to `root`.
/// `jobs` controls the thread count (0 = jwalk default).
/// Entries caught by `filter` do not appear in the map.
pub fn scan_tree(root: &Path, jobs: usize, filter: &Filter) -> anyhow::Result<Scan> {
    use jwalk::WalkDir;

    // follow_links(false): symlinks are NOT dereferenced. The walker does not
    // descend into symlinked directories and does not read link targets as
    // content.
    let mut walk = WalkDir::new(root).skip_hidden(false).follow_links(false);
    if jobs > 0 {
        walk = walk.parallelism(jwalk::Parallelism::RayonNewPool(jobs));
    }
    if !filter.is_empty() {
        let filter = filter.clone();
        let root = root.to_path_buf();
        // Apply exclusions right at readdir time: entries removed from the
        // children list are neither yielded nor entered, so an excluded
        // directory drops out together with its subtree. Compared to filtering
        // afterwards, this also saves the subtree's I/O.
        walk = walk.process_read_dir(move |_depth, dir_path, _state, children| {
            let Ok(rel_dir) = dir_path.strip_prefix(&root) else {
                return;
            };
            children.retain(|child| match child {
                Ok(entry) => !filter.excludes(&rel_dir.join(entry.file_name())),
                // Keep error entries: they are reported normally below.
                Err(_) => true,
            });
        });
    }

    // Phase 1: parallel readdir (jwalk), but only collect the paths. The
    // expensive metadata syscalls (lstat, read_link) do NOT run here in the
    // single consumer thread — they would be serialized there and cancel out
    // the walk's parallelism — but in phase 2 instead.
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut unreadable: Vec<(PathBuf, String)> = Vec::new();
    for entry in walk {
        let entry = entry?;
        let path = entry.path();
        // jwalk reports a failed readdir on the directory entry itself, not as
        // an iterator error. Without evaluating this, the tree would look as if
        // the directory were empty.
        if let Some(err) = &entry.read_children_error {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            unreadable.push((rel, err.to_string()));
        }
        if path == root {
            continue;
        }
        paths.push(path);
    }

    // Phase 2: read the metadata per entry in parallel and collect straight
    // into the map. Special files and vanished entries yield None and drop out
    // in the process.
    //
    // With an explicit --scan-jobs, cap the metadata parallelism at the same
    // value; otherwise use the global rayon pool.
    let map = if jobs > 0 {
        match rayon::ThreadPoolBuilder::new().num_threads(jobs).build() {
            Ok(pool) => pool.install(move || read_all(root, paths)),
            Err(_) => read_all(root, paths),
        }
    } else {
        read_all(root, paths)
    }?;

    Ok(Scan { entries: map, unreadable })
}

/// Reads the metadata of all `paths` in parallel and builds the map from it.
///
/// `into_par_iter` consumes the path list, so each PathBuf is freed after
/// processing instead of living until the end of the phase. Likewise, results
/// are collected straight into the BTreeMap: an intermediate Vec would hold
/// every entry in memory a second time.
fn read_all(root: &Path, paths: Vec<PathBuf>) -> anyhow::Result<BTreeMap<PathBuf, FileEntry>> {
    use rayon::prelude::*;
    paths.into_par_iter().filter_map(|p| read_entry(root, &p).transpose()).collect()
}

/// Reads the metadata of a walked path and builds the FileEntry.
///
/// `Ok(None)` for entries that do not belong in the map: special files
/// (FIFO/socket/device) as well as entries that vanished between the walk's
/// readdir and the lstat here. The latter is normal for a source that is in
/// use (lock/cache files) and must not abort the run — since the two-phase
/// optimization, that window spans the entire walk.
fn read_entry(root: &Path, path: &Path) -> anyhow::Result<Option<(PathBuf, FileEntry)>> {
    use std::os::unix::fs::MetadataExt;

    let vanished = |what: &str| {
        eprintln!("Warning: skipping {what} (vanished during the scan): {}", path.display());
    };
    let rel = path.strip_prefix(root)?.to_path_buf();
    // symlink_metadata does NOT follow the link (unlike entry.metadata()).
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            vanished("entry");
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let kind = if meta.file_type().is_symlink() {
        match std::fs::read_link(path) {
            Ok(target) => EntryKind::Symlink { target },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                vanished("symlink");
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
    } else if meta.is_dir() {
        EntryKind::Dir
    } else if meta.is_file() {
        EntryKind::File
    } else {
        // Skip special files (FIFO, socket, device): File::open on a FIFO
        // blocks until a writer connects -> risk of a deadlock.
        eprintln!("Warning: skipping special file {}", path.display());
        return Ok(None);
    };
    let size = if meta.is_file() { meta.len() } else { 0 };
    Ok(Some((rel, FileEntry { kind, size, mtime: meta.mtime(), mode: meta.mode() & 0o7777 })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scans_files_and_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::write(root.join("sub/b.txt"), b"hi").unwrap();

        let map = scan_tree(root, 0, &Filter::default()).unwrap().entries;

        let a = map.get(&PathBuf::from("a.txt")).unwrap();
        assert_eq!(a.kind, EntryKind::File);
        assert_eq!(a.size, 5);

        let sub = map.get(&PathBuf::from("sub")).unwrap();
        assert_eq!(sub.kind, EntryKind::Dir);

        let b = map.get(&PathBuf::from("sub/b.txt")).unwrap();
        assert_eq!(b.size, 2);

        // root itself is not included
        assert!(!map.contains_key(&PathBuf::from("")));
    }

    #[test]
    fn vanished_entry_is_skipped_not_fatal() {
        // Between the walk (readdir) and the metadata phase (lstat) an entry
        // can be deleted (source in use: lock/cache files). That must only be a
        // warning, not an abort of the entire scan.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("stays.txt"), b"ok").unwrap();

        let gone = read_entry(root, &root.join("already_deleted.txt"))
            .expect("a vanished entry must not be an error");
        assert!(gone.is_none(), "a vanished entry must be skipped");

        let there = read_entry(root, &root.join("stays.txt")).unwrap();
        assert!(there.is_some());
    }

    #[test]
    fn skips_special_files_like_fifos() {
        // A FIFO must not be classified as a file: File::open on it blocks
        // until a writer connects -> dino-copy would hang forever.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("normal.txt"), b"ok").unwrap();
        let fifo = root.join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo not available");
        assert!(status.success());

        let map = scan_tree(root, 0, &Filter::default()).unwrap().entries;

        assert!(map.contains_key(&PathBuf::from("normal.txt")));
        assert!(
            !map.contains_key(&PathBuf::from("pipe")),
            "FIFO must be skipped"
        );
    }
}
