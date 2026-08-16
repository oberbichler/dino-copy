//! Guarantee tests: the source is NEVER modified by dino-copy.
//!
//! dino-copy opens source files read-only; every mutating operation (copies,
//! deletes, mkdir, metadata, temp files) targets paths below the target. These
//! tests cement that with a complete before/after comparison of the source
//! (structure, contents, mtime, permissions, symlink targets) — including
//! adversarial scenarios such as symlinks in the target that point into the
//! source.
//!
//! Deliberately NOT compared: atime. Reading updates the atime depending on the
//! mount options (relatime etc.) — that is a kernel effect of any reader and
//! not a modification by dino-copy.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_dino-copy")
}

fn run(source: &Path, target: &Path, extra: &[&str]) -> Output {
    let mut args = vec![source.to_str().unwrap(), target.to_str().unwrap()];
    args.extend_from_slice(extra);
    Command::new(bin()).args(&args).output().unwrap()
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("src");
    let d = tmp.path().join("dst");
    fs::create_dir_all(&s).unwrap();
    fs::create_dir_all(&d).unwrap();
    (tmp, s, d)
}

/// Complete state of an entry, as far as dino-copy could change it.
#[derive(Debug, PartialEq, Eq)]
struct EntryState {
    /// "file" | "dir" | "symlink"
    kind: &'static str,
    mode: u32,
    mtime: i64,
    /// File contents (files), link target as bytes (symlinks), empty (dirs).
    payload: Vec<u8>,
}

/// Recursive snapshot of a tree: rel path -> state.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, EntryState> {
    let mut map = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let path = e.path();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            let meta = fs::symlink_metadata(&path).unwrap();
            let (kind, payload) = if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap();
                ("symlink", target.into_os_string().into_encoded_bytes())
            } else if meta.is_dir() {
                stack.push(path.clone());
                ("dir", Vec::new())
            } else {
                ("file", fs::read(&path).unwrap())
            };
            map.insert(
                rel,
                EntryState { kind, mode: meta.mode() & 0o7777, mtime: meta.mtime(), payload },
            );
        }
    }
    map
}

/// Assertion with useful diagnostics about which entry changed.
fn assert_source_unchanged(before: &BTreeMap<PathBuf, EntryState>, root: &Path) {
    let after = snapshot(root);
    for (rel, b) in before {
        match after.get(rel) {
            None => panic!("source modified: {} was DELETED", rel.display()),
            Some(a) if a != b => panic!(
                "source modified: {} was {:?}, is now {:?}",
                rel.display(),
                b,
                a
            ),
            _ => {}
        }
    }
    for rel in after.keys() {
        assert!(
            before.contains_key(rel),
            "source modified: {} was ADDED (stray temp file?)",
            rel.display()
        );
    }
}

/// Builds a source containing everything dino-copy knows: files (small + large
/// enough for the chunked path), subdirectories, symlinks, special modes/mtimes.
fn build_source(s: &Path) {
    fs::create_dir_all(s.join("sub/deep")).unwrap();
    fs::write(s.join("a.txt"), b"hello").unwrap();
    fs::write(s.join("sub/b.txt"), b"world").unwrap();
    // Larger than the 8 MiB chunk threshold -> double-buffered path.
    fs::write(s.join("sub/deep/big.bin"), vec![0x5Au8; 9 * 1024 * 1024]).unwrap();
    std::os::unix::fs::symlink("a.txt", s.join("link_rel")).unwrap();
    fs::set_permissions(s.join("a.txt"), std::os::unix::fs::PermissionsExt::from_mode(0o640))
        .unwrap();
    filetime::set_file_mtime(s.join("a.txt"), filetime::FileTime::from_unix_time(1_000_000, 0))
        .unwrap();
}

#[test]
fn source_is_unmodified_by_full_mirror_with_deletes_and_conflicts() {
    let (_tmp, s, d) = setup();
    build_source(&s);
    // The target provokes every action type: stale file (Delete), superfluous
    // dir (DeleteDir), type conflict (file where the source has a dir), changed
    // file (Copy over an existing one).
    fs::write(d.join("stale.txt"), b"away with it").unwrap();
    fs::create_dir_all(d.join("old_dir")).unwrap();
    fs::write(d.join("sub"), b"file instead of directory").unwrap();
    fs::write(d.join("a.txt"), b"outdated content").unwrap();

    let before = snapshot(&s);
    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success(), "dino-copy failed: {:?}", out);
    assert_source_unchanged(&before, &s);
}

#[test]
fn source_is_unmodified_when_dest_symlink_points_into_source_dir() {
    // Adversarial: where the source has a real directory, the target holds a
    // SYMLINK to exactly that source directory. Were dino-copy to write through
    // the link (instead of removing it upfront as a type conflict), copies and
    // deletes would land in the SOURCE.
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("data")).unwrap();
    fs::write(s.join("data/precious.txt"), b"untouchable").unwrap();
    std::os::unix::fs::symlink(s.join("data"), d.join("data")).unwrap();
    // Additionally make a "stale" file visible in the target under the link:
    // dst/data/stale.txt physically lives in the SOURCE. A naive delete of
    // dst/data/stale.txt would hit the source. (The scan does not follow links,
    // so dino-copy must not even see this path as target content.)
    fs::write(s.join("data/stale_in_source.txt"), b"belongs to the source").unwrap();

    let before = snapshot(&s);
    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success(), "dino-copy failed: {:?}", out);
    assert_source_unchanged(&before, &s);

    // And the target is now a REAL directory (link replaced), with copies.
    let dmeta = fs::symlink_metadata(d.join("data")).unwrap();
    assert!(dmeta.is_dir() && !dmeta.file_type().is_symlink(), "the link must be replaced");
    assert_eq!(fs::read(d.join("data/precious.txt")).unwrap(), b"untouchable");
}

#[test]
fn source_is_unmodified_when_dest_symlink_to_source_file_is_deleted() {
    // The target has a superfluous symlink to a source file. The delete must
    // remove the LINK, never the source file behind it.
    let (_tmp, s, d) = setup();
    fs::write(s.join("keep.txt"), b"stays").unwrap();
    std::os::unix::fs::symlink(s.join("keep.txt"), d.join("extra_link")).unwrap();
    // keep.txt also exists as a regular copy in the target, so that only the
    // link is superfluous.
    fs::write(d.join("keep.txt"), b"stays").unwrap();

    let before = snapshot(&s);
    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success(), "dino-copy failed: {:?}", out);
    assert_source_unchanged(&before, &s);
    assert!(!d.join("extra_link").exists() && fs::symlink_metadata(d.join("extra_link")).is_err());
    assert_eq!(fs::read(s.join("keep.txt")).unwrap(), b"stays");
}

#[test]
fn source_is_unmodified_by_checksum_run_with_forced_copies() {
    // --checksum reads every candidate file of the source in full and forces
    // copies on a mismatch — even then the source may only be read.
    let (_tmp, s, d) = setup();
    build_source(&s);
    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());
    // Same size+mtime, different contents -> only --checksum detects that.
    fs::write(d.join("a.txt"), b"HELLO").unwrap();
    let src_meta = fs::metadata(s.join("a.txt")).unwrap();
    filetime::set_file_mtime(
        d.join("a.txt"),
        filetime::FileTime::from_unix_time(src_meta.mtime(), 0),
    )
    .unwrap();

    let before = snapshot(&s);
    let out = run(&s, &d, &["--yes", "--checksum"]);
    assert!(out.status.success(), "dino-copy failed: {:?}", out);
    assert_source_unchanged(&before, &s);
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"hello", "expected a forced copy");
}

#[test]
fn source_is_unmodified_even_when_actions_fail() {
    // Error path: a target file lies in a write-protected directory, so the
    // copy fails. Error handling/cleanup (removing temps) must not touch the
    // source either.
    unsafe extern "C" {
        safe fn geteuid() -> u32;
    }
    if geteuid() == 0 {
        // As root permissions do not apply -> the scenario cannot be set up.
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, s, d) = setup();
    build_source(&s);
    fs::create_dir_all(d.join("sub/deep")).unwrap();
    fs::set_permissions(d.join("sub/deep"), fs::Permissions::from_mode(0o555)).unwrap();

    let before = snapshot(&s);
    let out = run(&s, &d, &["--yes"]);
    assert!(!out.status.success(), "the run should fail because of EACCES");
    assert_source_unchanged(&before, &s);

    // Clean up so the tempdir can be deleted.
    fs::set_permissions(d.join("sub/deep"), fs::Permissions::from_mode(0o755)).unwrap();
}
