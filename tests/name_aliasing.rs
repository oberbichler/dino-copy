//! Names that differ byte-wise but that the target file system treats as the
//! same name: case (`Foo` vs `foo`) and Unicode normalization (NFC vs NFD).
//!
//! macOS volumes (APFS, HFS+) behave like that by default. A byte-wise
//! comparison sees "new on the source" plus "extra on the target", copies the
//! new name — which lands on the very same file — and then deletes the old name,
//! i.e. the freshly copied backup. These tests pin down that this never happens.
//!
//! A difference in case is mirrored by renaming the target entry; a pure
//! normalization difference is left alone (it is invisible, and HFS+ would
//! turn every rename back into NFD).
//!
//! They need such a file system for the temp dir and are skipped (with a
//! message) elsewhere, e.g. on Linux CI with ext4.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NFC: &str = "Caf\u{e9}.txt";
const NFD: &str = "Cafe\u{301}.txt";

fn run(source: &Path, target: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dino-copy"))
        .arg(source)
        .arg(target)
        .args(extra)
        .output()
        .unwrap()
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("src");
    let d = tmp.path().join("dst");
    fs::create_dir_all(&s).unwrap();
    fs::create_dir_all(&d).unwrap();
    (tmp, s, d)
}

/// True if `a` and `b` resolve to the same entry in a fresh temp dir.
fn aliases(a: &str, b: &str) -> bool {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join(a), b"x").unwrap();
    tmp.path().join(b).exists()
}

macro_rules! require_aliasing {
    ($a:expr, $b:expr) => {
        if !aliases($a, $b) {
            eprintln!(
                "skipped: the temp file system distinguishes {:?} and {:?}",
                $a, $b
            );
            return;
        }
    };
}

fn names(dir: &Path) -> Vec<String> {
    let mut n: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    n.sort();
    n
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_noop(s: &Path, d: &Path) {
    let again = run(s, d, &["--yes"]);
    assert!(
        stdout(&again).contains("Nothing to do"),
        "second run must be a no-op: {}",
        stdout(&again)
    );
}

#[test]
fn case_only_rename_of_a_file_keeps_the_backup() {
    require_aliasing!("x", "X");
    let (_tmp, s, d) = setup();
    fs::write(s.join("Foo.txt"), b"current").unwrap();
    fs::write(d.join("foo.txt"), b"old").unwrap();

    let out = run(&s, &d, &["--yes"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("1 renamed"), "{}", stdout(&out));
    assert_eq!(
        names(&d),
        ["Foo.txt"],
        "the target takes the source's spelling"
    );
    assert_eq!(fs::read(d.join("Foo.txt")).unwrap(), b"current");
    assert_noop(&s, &d);
}

#[test]
fn case_only_rename_of_a_directory_keeps_and_updates_its_contents() {
    require_aliasing!("x", "X");
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("Docs")).unwrap();
    fs::write(s.join("Docs/a.txt"), b"new content").unwrap();
    fs::write(s.join("Docs/b.txt"), b"unchanged").unwrap();
    fs::create_dir_all(d.join("docs")).unwrap();
    fs::write(d.join("docs/a.txt"), b"old").unwrap();
    fs::write(d.join("docs/stale.txt"), b"gone in source").unwrap();

    let out = run(&s, &d, &["--yes"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(names(&d), ["Docs"]);
    assert_eq!(
        names(&d.join("Docs")),
        ["a.txt", "b.txt"],
        "real extras are still removed"
    );
    assert_eq!(fs::read(d.join("Docs/a.txt")).unwrap(), b"new content");
    assert_eq!(fs::read(d.join("Docs/b.txt")).unwrap(), b"unchanged");
    assert_noop(&s, &d);
}

#[test]
fn case_renames_at_several_levels_are_all_mirrored() {
    require_aliasing!("x", "X");
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("Photos/2024/Trip")).unwrap();
    fs::write(s.join("Photos/2024/Trip/IMG_1.JPG"), b"jpeg").unwrap();
    fs::create_dir_all(d.join("photos/2024/trip")).unwrap();
    fs::write(d.join("photos/2024/trip/img_1.jpg"), b"jpeg").unwrap();

    let out = run(&s, &d, &["--yes"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("3 renamed"), "{}", stdout(&out));
    assert_eq!(names(&d), ["Photos"]);
    assert_eq!(names(&d.join("Photos/2024")), ["Trip"]);
    assert_eq!(names(&d.join("Photos/2024/Trip")), ["IMG_1.JPG"]);
    assert_noop(&s, &d);
}

#[test]
fn a_rename_alone_is_not_reported_as_in_sync() {
    require_aliasing!("x", "X");
    let (_tmp, s, d) = setup();
    fs::write(s.join("README"), b"same").unwrap();
    fs::write(d.join("readme"), b"same").unwrap();
    let mtime = filetime::FileTime::from_unix_time(1_000_000, 0);
    filetime::set_file_mtime(s.join("README"), mtime).unwrap();
    filetime::set_file_mtime(d.join("readme"), mtime).unwrap();

    let dry = run(&s, &d, &["--dry-run"]);
    assert!(stdout(&dry).contains("copy 0"), "{}", stdout(&dry));
    assert!(stdout(&dry).contains("1 renamed"), "{}", stdout(&dry));
    assert_eq!(names(&d), ["readme"], "--dry-run must not rename");

    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(names(&d), ["README"]);
}

#[test]
fn unicode_normalization_difference_keeps_the_backup() {
    require_aliasing!(NFC, NFD);
    let (_tmp, s, d) = setup();
    fs::write(s.join(NFC), b"current").unwrap();
    fs::write(d.join(NFD), b"old").unwrap();

    let out = run(&s, &d, &["--yes"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("0 renamed"), "{}", stdout(&out));
    assert_eq!(
        names(&d),
        [NFD],
        "a normalization-only difference is not renamed"
    );
    assert_eq!(fs::read(d.join(NFC)).unwrap(), b"current");
    assert_noop(&s, &d);
}

#[test]
fn alias_with_another_type_is_replaced_as_a_conflict() {
    require_aliasing!("x", "X");
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("Foo")).unwrap();
    fs::write(s.join("Foo/inner.txt"), b"inner").unwrap();
    fs::write(d.join("foo"), b"a file in the way").unwrap();

    let out = run(&s, &d, &["--yes"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("0 renamed"), "replaced, not renamed: {}", stdout(&out));
    assert_eq!(names(&d), ["Foo"]);
    assert_eq!(fs::read(d.join("Foo/inner.txt")).unwrap(), b"inner");
}

#[test]
fn alias_of_an_excluded_target_entry_is_not_overwritten() {
    // Excluded target entries are invisible to dino-copy and must never be
    // overwritten — not even through a name that the file system aliases.
    require_aliasing!("x", "X");
    let (_tmp, s, d) = setup();
    fs::write(s.join("Notes"), b"from the source").unwrap();
    fs::write(d.join("notes"), b"protected").unwrap();

    let out = run(&s, &d, &["--yes", "--exclude", "notes"]);

    assert_eq!(fs::read(d.join("notes")).unwrap(), b"protected");
    assert_eq!(
        names(&d),
        ["notes"],
        "an excluded entry is not renamed either"
    );
    assert!(
        !out.status.success(),
        "the left-out entry must fail the run"
    );
    assert!(stderr(&out).contains("cannot mirror"), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--exclude"),
        "the hint must name a way out: {}",
        stderr(&out)
    );
}

#[test]
fn ambiguous_hard_links_are_reported_and_nothing_is_deleted() {
    // "a" and "b" are the same inode on the target; the source has "A" only.
    // Which target name "A" refers to is ambiguous, so neither may go.
    require_aliasing!("x", "X");
    let (_tmp, s, d) = setup();
    fs::write(s.join("A"), b"source").unwrap();
    fs::write(d.join("a"), b"linked").unwrap();
    fs::hard_link(d.join("a"), d.join("b")).unwrap();

    let out = run(&s, &d, &["--yes"]);

    assert!(!out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("cannot mirror"), "{}", stderr(&out));
    assert_eq!(names(&d), ["a", "b"]);
    assert_eq!(fs::read(d.join("a")).unwrap(), b"linked");
}
