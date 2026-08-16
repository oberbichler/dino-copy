use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_dino-copy")
}

unsafe extern "C" {
    /// Effective user ID of the process (POSIX). 0 = root.
    safe fn geteuid() -> u32;
}

fn running_as_root() -> bool {
    geteuid() == 0
}

/// Creates a source and target directory under a fresh tempdir and returns
/// (tempdir, source, target). The tempdir has to stay alive.
fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("src");
    let d = tmp.path().join("dst");
    fs::create_dir_all(&s).unwrap();
    fs::create_dir_all(&d).unwrap();
    (tmp, s, d)
}

/// Runs dino-copy with `source target [extra...]` and returns the output.
fn run(source: &Path, target: &Path, extra: &[&str]) -> Output {
    let mut args = vec![source.to_str().unwrap(), target.to_str().unwrap()];
    args.extend_from_slice(extra);
    Command::new(bin()).args(&args).output().unwrap()
}

/// Like `run`, but with text piped to stdin (for the confirm prompt).
fn run_with_stdin(source: &Path, target: &Path, extra: &[&str], stdin_data: &[u8]) -> Output {
    let mut args = vec![source.to_str().unwrap(), target.to_str().unwrap()];
    args.extend_from_slice(extra);
    let mut child = Command::new(bin())
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin_data).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn mirrors_source_to_dest_exactly() {
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("sub")).unwrap();
    fs::write(s.join("a.txt"), b"hello").unwrap();
    fs::write(s.join("sub/b.txt"), b"world").unwrap();

    // The target has a superfluous file that must be deleted.
    fs::write(d.join("stale.txt"), b"old").unwrap();

    let mtime = filetime::FileTime::from_unix_time(1_234_567, 0);
    filetime::set_file_mtime(s.join("a.txt"), mtime).unwrap();

    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());

    // Contents mirrored.
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"hello");
    assert_eq!(fs::read(d.join("sub/b.txt")).unwrap(), b"world");
    // Superfluous entry deleted.
    assert!(!d.join("stale.txt").exists());
    // mtime applied.
    assert_eq!(
        fs::metadata(s.join("a.txt")).unwrap().mtime(),
        fs::metadata(d.join("a.txt")).unwrap().mtime()
    );
}

#[test]
fn handles_file_dir_type_change() {
    // Source: "x" is a file. Target: "x" is a directory with contents.
    let (_tmp, s, d) = setup();
    fs::write(s.join("x"), b"iam a file").unwrap();
    fs::create_dir_all(d.join("x")).unwrap();
    fs::write(d.join("x/inner.txt"), b"stale").unwrap();

    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());

    // "x" must now be a file with the source's contents in the target.
    let meta = fs::symlink_metadata(d.join("x")).unwrap();
    assert!(meta.is_file(), "x should be a file");
    assert_eq!(fs::read(d.join("x")).unwrap(), b"iam a file");
    // No stray temp files.
    assert!(!d.join(".dino-copy.tmp.x").exists());
}

#[test]
fn symlink_is_not_dereferenced() {
    use std::os::unix::fs::symlink;
    let (_tmp, s, d) = setup();
    // A real file and a symlink to it.
    fs::write(s.join("real.txt"), b"payload").unwrap();
    symlink("real.txt", s.join("link")).unwrap();

    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());

    // In the target "link" must be a symlink, not a real copy.
    let meta = fs::symlink_metadata(d.join("link")).unwrap();
    assert!(meta.file_type().is_symlink(), "link should stay a symlink");
    assert_eq!(fs::read_link(d.join("link")).unwrap(), PathBuf::from("real.txt"));
}

#[test]
fn mode_only_change_is_mirrored() {
    // After the first mirror the source file is only chmod-ed (contents, size
    // and mtime unchanged — chmod only changes the ctime). The second run must
    // align the permissions in the target without copying the file.
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"same content").unwrap();
    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());

    fs::set_permissions(s.join("a.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("copy 0"),
        "a permission alignment must not trigger a copy: {stdout}"
    );
    let dm = fs::metadata(d.join("a.txt")).unwrap();
    assert_eq!(dm.mode() & 0o7777, 0o600, "the target permissions must be aligned");
}

#[test]
fn preserves_directory_mtime_and_mode() {
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("sub")).unwrap();
    fs::write(s.join("sub/f.txt"), b"hi").unwrap();
    fs::set_permissions(s.join("sub"), fs::Permissions::from_mode(0o750)).unwrap();
    let dir_mtime = filetime::FileTime::from_unix_time(1_500_000, 0);
    filetime::set_file_mtime(s.join("sub"), dir_mtime).unwrap();

    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());

    let sm = fs::metadata(s.join("sub")).unwrap();
    let dm = fs::metadata(d.join("sub")).unwrap();
    assert_eq!(sm.permissions().mode() & 0o7777, dm.permissions().mode() & 0o7777);
    assert_eq!(sm.mtime(), dm.mtime(), "the directory mtime must be applied");
}

#[test]
fn skips_unchanged_on_second_run() {
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"data").unwrap();

    let _ = run(&s, &d, &["--yes"]);
    let out = run(&s, &d, &["--yes"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Second run: nothing to copy.
    assert!(stdout.contains("copy 0") || stdout.contains("already in sync"));
}

#[test]
fn checksum_detects_same_size_mtime_different_content() {
    // Same size AND same mtime, but different contents.
    let (_tmp, s, d) = setup();
    fs::write(s.join("f.txt"), b"AAAA").unwrap();
    fs::write(d.join("f.txt"), b"BBBB").unwrap();
    let mtime = filetime::FileTime::from_unix_time(1_000_000, 0);
    filetime::set_file_mtime(s.join("f.txt"), mtime).unwrap();
    filetime::set_file_mtime(d.join("f.txt"), mtime).unwrap();

    // Without --checksum: skipped (same size+mtime), the target stays "BBBB".
    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("copy 0"), "without checksum nothing may be copied");
    assert_eq!(fs::read(d.join("f.txt")).unwrap(), b"BBBB");

    // With --checksum: blake3 spots the difference, the target becomes "AAAA".
    let out = run(&s, &d, &["--checksum", "--yes"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("copy 1"), "checksum must detect the difference");
    assert_eq!(fs::read(d.join("f.txt")).unwrap(), b"AAAA");
}

#[test]
fn fsync_flag_still_mirrors_correctly() {
    // --fsync must not change the result, only the durability.
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("sub")).unwrap();
    fs::write(s.join("a.txt"), b"durable").unwrap();
    fs::write(s.join("sub/b.txt"), b"nested").unwrap();

    let out = run(&s, &d, &["--fsync", "--yes"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"durable");
    assert_eq!(fs::read(d.join("sub/b.txt")).unwrap(), b"nested");
}

#[test]
fn dry_run_does_not_modify_dest() {
    let (_tmp, s, d) = setup();
    fs::write(s.join("new.txt"), b"content").unwrap();
    fs::write(d.join("stale.txt"), b"remove me").unwrap();

    let out = run(&s, &d, &["--dry-run"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dry-run"));

    // The target is unchanged: the new file was NOT copied, stale NOT deleted.
    assert!(!d.join("new.txt").exists(), "a dry run must not copy anything");
    assert!(d.join("stale.txt").exists(), "a dry run must not delete anything");
}

#[test]
fn updates_changed_file() {
    // The file exists on both sides, but contents/size differ.
    let (_tmp, s, d) = setup();
    fs::write(s.join("f.txt"), b"new longer content").unwrap();
    fs::write(d.join("f.txt"), b"old").unwrap();

    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("copy 1"));
    assert_eq!(fs::read(d.join("f.txt")).unwrap(), b"new longer content");
}

#[test]
fn mirrors_empty_directories() {
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("emptydir")).unwrap();
    fs::create_dir_all(s.join("nested/deep")).unwrap();

    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());

    assert!(d.join("emptydir").is_dir(), "an empty directory must be created");
    assert!(d.join("nested/deep").is_dir(), "nested empty dirs must be created");
}

#[test]
fn confirm_prompt_aborts_on_no() {
    // Without --yes and with deletions: "n" aborts, nothing is deleted.
    let (_tmp, s, d) = setup();
    fs::write(s.join("keep.txt"), b"x").unwrap();
    fs::write(d.join("stale.txt"), b"remove me").unwrap();

    let out = run_with_stdin(&s, &d, &[], b"n\n");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Aborted"), "'n' must abort the run");
    // Nothing was changed.
    assert!(d.join("stale.txt").exists(), "an abort must not delete anything");
    assert!(!d.join("keep.txt").exists(), "an abort must not copy anything");
}

#[test]
fn confirm_prompt_proceeds_on_yes() {
    let (_tmp, s, d) = setup();
    fs::write(s.join("keep.txt"), b"x").unwrap();
    fs::write(d.join("stale.txt"), b"remove me").unwrap();

    let out = run_with_stdin(&s, &d, &[], b"y\n");
    assert!(out.status.success());
    // The sync ran through.
    assert!(!d.join("stale.txt").exists(), "'y' must delete");
    assert_eq!(fs::read(d.join("keep.txt")).unwrap(), b"x");
}

#[test]
fn missing_source_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("does_not_exist");
    let d = tmp.path().join("dst");
    fs::create_dir_all(&d).unwrap();

    let out = run(&s, &d, &["--yes"]);
    assert!(!out.status.success(), "a missing source must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Source is not a directory"), "stderr: {stderr}");
}

#[test]
fn source_equals_target_errors() {
    let (_tmp, s, _d) = setup();
    let out = run(&s, &s, &["--yes"]);
    assert!(!out.status.success(), "identical paths must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("identical"), "stderr: {stderr}");
}

#[test]
fn target_is_created_if_missing() {
    // The target directory does not exist yet -> it is created and mirrored.
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("src");
    let d = tmp.path().join("brandnew/target");
    fs::create_dir_all(&s).unwrap();
    fs::write(s.join("a.txt"), b"hi").unwrap();

    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(d.is_dir(), "the target must have been created");
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"hi");
}

#[test]
fn target_is_a_file_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("src");
    let d = tmp.path().join("target_file");
    fs::create_dir_all(&s).unwrap();
    fs::write(&d, b"i am a file, not a dir").unwrap();

    let out = run(&s, &d, &["--yes"]);
    assert!(!out.status.success(), "a target that is a file must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Target is not a directory"), "stderr: {stderr}");
}

#[test]
fn copy_failure_yields_nonzero_exit() {
    // Provoke an error: the target holds a write-protected, non-empty directory
    // where the source has a file -> the conflict removal fails.
    use std::os::unix::fs::PermissionsExt;

    // root bypasses directory permission checks, so the error injection via
    // 0o500 would not take effect. Skip in that case (e.g. Docker CI as root).
    if running_as_root() {
        eprintln!("skipped: running as root, permission-based error injection does not apply");
        return;
    }

    let (_tmp, s, d) = setup();
    fs::write(s.join("sub"), b"file-on-source").unwrap();
    // In the target "sub" is a non-empty, write-protected directory whose
    // contents cannot be deleted during the conflict removal.
    fs::create_dir_all(d.join("sub/locked")).unwrap();
    fs::write(d.join("sub/locked/inner.txt"), b"x").unwrap();
    // Make sub/locked read-only -> remove_dir_all fails on inner.
    fs::set_permissions(d.join("sub/locked"), fs::Permissions::from_mode(0o500)).unwrap();

    let out = run(&s, &d, &["--yes"]);
    // Restore the permissions so the tempdir can be deleted.
    let _ = fs::set_permissions(d.join("sub/locked"), fs::Permissions::from_mode(0o700));

    assert!(!out.status.success(), "a copy/remove error must yield exit != 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Error"), "stderr: {stderr}");
}

#[test]
fn many_files_parallel_jobs() {
    // Many files across several directories, with increased parallelism.
    let (_tmp, s, d) = setup();
    for dir in 0..5 {
        let sub = s.join(format!("d{dir}"));
        fs::create_dir_all(&sub).unwrap();
        for f in 0..40 {
            fs::write(sub.join(format!("f{f}.txt")), format!("content-{dir}-{f}")).unwrap();
        }
    }

    let out = run(&s, &d, &["--jobs", "8", "--scan-jobs", "4", "--yes"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    // Spot checks + total count.
    assert_eq!(fs::read(d.join("d0/f0.txt")).unwrap(), b"content-0-0");
    assert_eq!(fs::read(d.join("d4/f39.txt")).unwrap(), b"content-4-39");
    let count: usize = (0..5)
        .map(|dir| fs::read_dir(d.join(format!("d{dir}"))).unwrap().count())
        .sum();
    assert_eq!(count, 200, "all 200 files must be copied");
}

// --- Path validation: source/target must not be nested ---

#[test]
fn rejects_source_inside_target() {
    // dino-copy a/sub a would delete the source itself -> must be rejected.
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let sub = a.join("sub");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("data.txt"), b"important").unwrap();
    fs::write(a.join("other.txt"), b"also important").unwrap();

    let out = run(&sub, &a, &["--yes"]);
    assert!(!out.status.success(), "nested paths must be rejected");
    // Nothing was changed.
    assert_eq!(fs::read(sub.join("data.txt")).unwrap(), b"important");
    assert_eq!(fs::read(a.join("other.txt")).unwrap(), b"also important");
}

#[test]
fn rejects_target_inside_source() {
    // dino-copy a a/sub would copy recursively into itself -> reject.
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    fs::create_dir_all(&a).unwrap();
    fs::write(a.join("data.txt"), b"x").unwrap();

    let out = run(&a, &a.join("backup"), &["--yes"]);
    assert!(!out.status.success(), "a target inside the source must be rejected");
}

#[test]
fn rejects_same_path_via_dotdot() {
    // /a and /a/../a are the same path, just not textually equal.
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    fs::create_dir_all(&a).unwrap();
    fs::write(a.join("data.txt"), b"x").unwrap();
    let aliased = a.join("..").join("a");

    let out = run(&a, &aliased, &["--yes"]);
    assert!(!out.status.success(), "identical paths via .. must be detected");
    assert_eq!(fs::read(a.join("data.txt")).unwrap(), b"x");
}

// --- mtime tolerance: FAT32 stores mtimes at 2s resolution ---

#[test]
fn mtime_diff_of_two_seconds_is_skipped_by_default() {
    // FAT32 rounds mtimes to 2 seconds. The same size + a 2s difference must
    // count as unchanged by default (like rsync --modify-window=2).
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"aa").unwrap();
    fs::write(d.join("a.txt"), b"bb").unwrap();
    filetime::set_file_mtime(s.join("a.txt"), filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
    filetime::set_file_mtime(d.join("a.txt"), filetime::FileTime::from_unix_time(1_000_002, 0)).unwrap();

    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());
    // Not copied -> the target contents are unchanged.
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"bb");
}

#[test]
fn mtime_window_zero_forces_copy_on_any_diff() {
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"aa").unwrap();
    fs::write(d.join("a.txt"), b"bb").unwrap();
    filetime::set_file_mtime(s.join("a.txt"), filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
    filetime::set_file_mtime(d.join("a.txt"), filetime::FileTime::from_unix_time(1_000_001, 0)).unwrap();

    let out = run(&s, &d, &["--yes", "--mtime-window", "0"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"aa");
}

#[test]
fn second_run_with_subdirs_is_noop() {
    // Regression: SetDirMeta used to be emitted unconditionally, which made the
    // "nothing to do" path unreachable for trees with subdirectories.
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("sub/deep")).unwrap();
    fs::write(s.join("sub/a.txt"), b"x").unwrap();
    fs::write(s.join("sub/deep/b.txt"), b"y").unwrap();

    let out1 = run(&s, &d, &["--yes"]);
    assert!(out1.status.success());

    let out2 = run(&s, &d, &["--yes"]);
    assert!(out2.status.success());
    let stdout = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout.contains("Nothing to do"),
        "the second run must be a no-op, stdout: {stdout}"
    );
}

#[test]
fn symlink_mtime_is_preserved() {
    let (_tmp, s, d) = setup();
    fs::write(s.join("real.txt"), b"x").unwrap();
    std::os::unix::fs::symlink("real.txt", s.join("link")).unwrap();
    let lt = filetime::FileTime::from_unix_time(1_111_111, 0);
    filetime::set_symlink_file_times(s.join("link"), lt, lt).unwrap();

    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());

    assert_eq!(
        fs::symlink_metadata(d.join("link")).unwrap().mtime(),
        1_111_111,
        "the symlink mtime must be applied"
    );
}

#[test]
fn checksum_repair_resets_parent_dir_mtime() {
    // --checksum forces copies that plan::diff did not know about. The parent
    // dir is touched by the rename -> its mtime must still match the source
    // afterwards (and the following run must be a no-op).
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("sub")).unwrap();
    fs::write(s.join("sub/a.txt"), b"good").unwrap();
    // An old, fixed dir mtime in the source: otherwise "now" (falsified by the
    // rename) happens to equal the source mtime and the test sees nothing.
    filetime::set_file_mtime(s.join("sub"), filetime::FileTime::from_unix_time(1_000_000, 0))
        .unwrap();

    // First sync, then simulate bit rot in the target: same name, same size,
    // same mtime, different contents.
    let out = run(&s, &d, &["--yes"]);
    assert!(out.status.success());
    fs::write(d.join("sub/a.txt"), b"rot!").unwrap();
    let src_meta = fs::metadata(s.join("sub/a.txt")).unwrap();
    filetime::set_file_mtime(
        d.join("sub/a.txt"),
        filetime::FileTime::from_unix_time(src_meta.mtime(), 0),
    )
    .unwrap();
    // Restore the target's dir mtime to the source's state (as after a run).
    filetime::set_file_mtime(
        d.join("sub"),
        filetime::FileTime::from_unix_time(fs::metadata(s.join("sub")).unwrap().mtime(), 0),
    )
    .unwrap();

    let out = run(&s, &d, &["--yes", "--checksum"]);
    assert!(out.status.success());

    // The repair happened ...
    assert_eq!(fs::read(d.join("sub/a.txt")).unwrap(), b"good");
    // ... and the parent dir mtime matches the source again.
    assert_eq!(
        fs::metadata(d.join("sub")).unwrap().mtime(),
        fs::metadata(s.join("sub")).unwrap().mtime(),
        "the dir mtime must match the source after a checksum repair"
    );
}

#[test]
fn empty_source_does_not_wipe_target() {
    // The most common total-loss case: the source disk is not mounted, so
    // /Volumes/Source is an empty directory. Without a guard, --yes deletes the
    // entire backup and reports success.
    let (_tmp, s, d) = setup();
    fs::write(d.join("important.txt"), b"backup").unwrap();

    let out = run(&s, &d, &["--yes"]);

    assert!(!out.status.success(), "an empty source must abort");
    assert!(d.join("important.txt").exists(), "the target must not be emptied");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--allow-empty-source"),
        "the error message must name the way out: {stderr}"
    );
}

#[test]
fn empty_source_wipes_target_when_explicitly_allowed() {
    // The guard must be switchable: a genuinely emptied source should be able
    // to produce an empty target too.
    let (_tmp, s, d) = setup();
    fs::write(d.join("important.txt"), b"backup").unwrap();

    let out = run(&s, &d, &["--yes", "--allow-empty-source"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!d.join("important.txt").exists(), "the target must be emptied");
}

#[test]
fn empty_source_and_empty_target_is_not_an_error() {
    // Nothing to delete -> the guard must not kick in.
    let (_tmp, s, d) = setup();

    let out = run(&s, &d, &["--yes"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn max_delete_aborts_before_any_change() {
    // --max-delete limits the deletions even where --yes switches off the
    // confirmation (cron job). The abort must happen before any change.
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"new").unwrap();
    fs::write(d.join("old1.txt"), b"1").unwrap();
    fs::write(d.join("old2.txt"), b"2").unwrap();

    let out = run(&s, &d, &["--yes", "--max-delete", "1"]);

    assert!(!out.status.success(), "exceeding the limit must abort");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("exceed the limit"),
        "the abort must state the reason: {stderr}"
    );
    assert!(d.join("old1.txt").exists(), "nothing may be deleted");
    assert!(d.join("old2.txt").exists(), "nothing may be deleted");
    assert!(!d.join("a.txt").exists(), "nothing may be copied either");
}

#[test]
fn max_delete_allows_deletions_up_to_the_limit() {
    // Exactly at the limit the run must go through normally.
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"new").unwrap();
    fs::write(d.join("old1.txt"), b"1").unwrap();
    fs::write(d.join("old2.txt"), b"2").unwrap();

    let out = run(&s, &d, &["--yes", "--max-delete", "2"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!d.join("old1.txt").exists());
    assert!(!d.join("old2.txt").exists());
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"new");
}

#[test]
fn exclude_skips_matching_names_at_any_depth() {
    // A pattern without '/' matches the file name at any depth.
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("sub")).unwrap();
    fs::write(s.join("a.txt"), b"payload").unwrap();
    fs::write(s.join("whatever.tmp"), b"junk").unwrap();
    fs::write(s.join("sub/also.tmp"), b"junk").unwrap();

    let out = run(&s, &d, &["--yes", "--exclude", "*.tmp"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"payload");
    assert!(!d.join("whatever.tmp").exists(), "match at the top level");
    assert!(!d.join("sub/also.tmp").exists(), "match in a subdirectory");
}

#[test]
fn exclude_skips_the_whole_subtree_of_a_matching_directory() {
    // If a directory matches, its contents must not be mirrored either.
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("cache/deep")).unwrap();
    fs::write(s.join("cache/x.bin"), b"junk").unwrap();
    fs::write(s.join("cache/deep/y.bin"), b"junk").unwrap();
    fs::write(s.join("a.txt"), b"payload").unwrap();

    let out = run(&s, &d, &["--yes", "--exclude", "cache"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"payload");
    assert!(!d.join("cache").exists(), "the excluded subtree may be missing");
}

#[test]
fn excluded_entries_in_target_are_not_deleted() {
    // The actual purpose: when mirroring two volume roots, the target has its
    // own .Spotlight-V100 etc. Those must neither be overwritten nor deleted as
    // "extraneous".
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"payload").unwrap();
    fs::create_dir_all(d.join(".Spotlight-V100")).unwrap();
    fs::write(d.join(".Spotlight-V100/index"), b"target-owned index").unwrap();

    let out = run(&s, &d, &["--yes", "--exclude", ".Spotlight-V100"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        fs::read(d.join(".Spotlight-V100/index")).unwrap(),
        b"target-owned index",
        "an excluded target entry must stay untouched"
    );
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"payload");
}

#[test]
fn exclude_with_slash_matches_only_that_relative_path() {
    // A pattern with '/' matches the full relative path, not the name.
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("sub")).unwrap();
    fs::create_dir_all(s.join("other")).unwrap();
    fs::write(s.join("sub/b.txt"), b"out").unwrap();
    fs::write(s.join("other/b.txt"), b"stays").unwrap();

    let out = run(&s, &d, &["--yes", "--exclude", "sub/b.txt"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!d.join("sub/b.txt").exists(), "the exact path must be excluded");
    assert_eq!(
        fs::read(d.join("other/b.txt")).unwrap(),
        b"stays",
        "a file of the same name at another path must not be excluded too"
    );
}

#[test]
fn exclude_star_does_not_cross_directory_boundaries() {
    // In path patterns '*' must not skip over '/': 'sub/*' only matches the
    // direct children of sub, not their descendants.
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("sub/deep")).unwrap();
    fs::write(s.join("sub/direct.txt"), b"out").unwrap();
    fs::write(s.join("sub/deep/inside.txt"), b"stays").unwrap();

    let out = run(&s, &d, &["--yes", "--exclude", "sub/*.txt"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!d.join("sub/direct.txt").exists());
    assert_eq!(fs::read(d.join("sub/deep/inside.txt")).unwrap(), b"stays");
}

#[test]
fn multiple_exclude_flags_are_combined() {
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"payload").unwrap();
    fs::write(s.join(".DS_Store"), b"junk").unwrap();
    fs::write(s.join("x.tmp"), b"junk").unwrap();

    let out = run(&s, &d, &["--yes", "--exclude", ".DS_Store", "--exclude", "*.tmp"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(d.join("a.txt").exists());
    assert!(!d.join(".DS_Store").exists());
    assert!(!d.join("x.tmp").exists());
}

#[test]
fn mirrors_root_directory_mode_and_mtime() {
    // The root appears in neither map (scan_tree skips it) and therefore has no
    // SetDirMeta action — yet it belongs to the mirror.
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"x").unwrap();
    fs::set_permissions(&s, fs::Permissions::from_mode(0o750)).unwrap();
    filetime::set_file_mtime(&s, filetime::FileTime::from_unix_time(1_600_000, 0)).unwrap();

    let out = run(&s, &d, &["--yes"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let sm = fs::metadata(&s).unwrap();
    let dm = fs::metadata(&d).unwrap();
    assert_eq!(dm.permissions().mode() & 0o7777, 0o750, "mode of the root");
    assert_eq!(dm.mtime(), sm.mtime(), "mtime of the root");
}

#[test]
fn root_directory_mode_change_alone_is_mirrored() {
    // If only the root's mode changes, there is not a single action. The run
    // must not dismiss that as "nothing to do", otherwise the drift would
    // persist forever.
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"x").unwrap();
    assert!(run(&s, &d, &["--yes"]).status.success());

    fs::set_permissions(&s, fs::Permissions::from_mode(0o700)).unwrap();
    let out = run(&s, &d, &["--yes"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        fs::metadata(&d).unwrap().permissions().mode() & 0o7777,
        0o700,
        "pure mode drift of the root must be aligned"
    );
}

#[test]
fn deletions_are_skipped_when_copies_fail() {
    // If a copy fails, the mirror is incomplete. Then nothing may be removed in
    // the target: the extraneous entries could be the only reachable version of
    // the data.
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        return;
    }
    let (_tmp, s, d) = setup();
    fs::write(s.join("secret.txt"), b"unreadable").unwrap();
    fs::set_permissions(s.join("secret.txt"), fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(d.join("old.txt"), b"backup").unwrap();

    let out = run(&s, &d, &["--yes"]);

    assert!(!out.status.success(), "a failed copy must yield exit != 0");
    assert!(
        d.join("old.txt").exists(),
        "nothing may be deleted after copy errors"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--ignore-errors"),
        "the way out must be named: {stderr}"
    );
}

#[test]
fn ignore_errors_forces_deletions_despite_copy_failures() {
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        return;
    }
    let (_tmp, s, d) = setup();
    fs::write(s.join("secret.txt"), b"unreadable").unwrap();
    fs::set_permissions(s.join("secret.txt"), fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(d.join("old.txt"), b"backup").unwrap();

    let out = run(&s, &d, &["--yes", "--ignore-errors"]);

    assert!(!out.status.success(), "the copy error stays an error");
    assert!(
        !d.join("old.txt").exists(),
        "--ignore-errors must force the deletion: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn successful_run_still_deletes_extras() {
    // Counter-check: without errors, deletion still happens.
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"new").unwrap();
    fs::write(d.join("old.txt"), b"gone").unwrap();

    let out = run(&s, &d, &["--yes"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!d.join("old.txt").exists());
}

#[test]
fn unreadable_source_directory_does_not_silently_delete_the_backup() {
    // If a source directory is not readable, dino-copy does not see its
    // contents. Without a guard, an older backup lying in the target counts as
    // extraneous and gets deleted without comment — even though the source file
    // still exists and merely was not readable.
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        return;
    }
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"payload").unwrap();
    fs::create_dir_all(s.join("secret")).unwrap();
    fs::write(s.join("secret/data.txt"), b"important source file").unwrap();
    fs::create_dir_all(d.join("secret")).unwrap();
    fs::write(d.join("secret/data.txt"), b"older backup").unwrap();
    fs::set_permissions(s.join("secret"), fs::Permissions::from_mode(0o000)).unwrap();

    let out = run(&s, &d, &["--yes"]);

    fs::set_permissions(s.join("secret"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(d.join("secret"), fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        d.join("secret/data.txt").exists(),
        "the backup must not be deleted when the source is unreadable"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("secret"),
        "the unreadable directory must be reported: {stderr}"
    );
    assert!(!out.status.success(), "an incomplete scan must not report exit 0");
}

#[test]
fn ignore_errors_allows_deletion_despite_an_unreadable_source_directory() {
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        return;
    }
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("secret")).unwrap();
    fs::create_dir_all(d.join("secret")).unwrap();
    fs::write(d.join("secret/old.txt"), b"old").unwrap();
    fs::set_permissions(s.join("secret"), fs::Permissions::from_mode(0o000)).unwrap();

    let out = run(&s, &d, &["--yes", "--ignore-errors"]);

    fs::set_permissions(s.join("secret"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(d.join("secret"), fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        !d.join("secret/old.txt").exists(),
        "--ignore-errors must force the deletion: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn dry_run_does_not_create_the_target_directory() {
    // A typo in the target path used to silently become a real directory: the
    // trial run created it instead of failing.
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("src");
    fs::create_dir_all(&s).unwrap();
    fs::write(s.join("a.txt"), b"x").unwrap();
    let d = tmp.path().join("typo");

    let out = run(&s, &d, &["--dry-run"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!d.exists(), "--dry-run must not create the target");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("copy 1"), "the plan must show the copy: {stdout}");
}

#[test]
fn dry_run_still_rejects_target_inside_source() {
    // The nesting guard needs resolved paths. It must take effect even when the
    // target does not exist yet.
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("src");
    fs::create_dir_all(&s).unwrap();
    let d = s.join("not/yet/here");

    let out = run(&s, &d, &["--dry-run"]);

    assert!(!out.status.success(), "a target inside the source must be rejected");
    assert!(!d.exists());
}

#[test]
fn real_run_still_creates_the_target_directory() {
    // Counter-check: without --dry-run the target is still created.
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("src");
    fs::create_dir_all(&s).unwrap();
    fs::write(s.join("a.txt"), b"content").unwrap();
    let d = tmp.path().join("new");

    let out = run(&s, &d, &["--yes"]);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"content");
}

#[test]
fn directory_without_read_permission_is_mirrored() {
    // A source directory with mode 0o000 (e.g. .Trashes on macOS volumes) must
    // be mirrorable. set_dir_meta must not lock itself out by setting the
    // permissions before the mtime.
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        return;
    }
    let (_tmp, s, d) = setup();
    fs::create_dir_all(s.join("locked")).unwrap();
    filetime::set_file_mtime(s.join("locked"), filetime::FileTime::from_unix_time(1_700_000, 0))
        .unwrap();
    fs::set_permissions(s.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

    let out = run(&s, &d, &["--yes"]);

    // Collect the metadata, then reopen both sides so the tempdir can be
    // cleaned up — before the assertions.
    let dm = fs::metadata(d.join("locked")).unwrap();
    fs::set_permissions(s.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(d.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();

    // The run reports the unreadable directory (its own test), but it must no
    // longer fail in set_dir_meta: mode and mtime are mirrored.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("set dir meta"), "set_dir_meta must not fail: {stderr}");
    assert_eq!(dm.permissions().mode() & 0o7777, 0o000, "the mode must be mirrored");
    assert_eq!(dm.mtime(), 1_700_000, "the mtime must be mirrored");
}

#[test]
fn excluded_unreadable_directory_is_not_entered() {
    // The real volume-root case: .Trashes is not readable for normal users. A
    // filter that only takes effect after the readdir would enter the folder
    // anyway and abort the scan with an error. Without --exclude the run
    // demonstrably fails here; with --exclude it must run through cleanly.
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        return;
    }
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"payload").unwrap();
    fs::create_dir_all(s.join(".Trashes/inside")).unwrap();
    fs::set_permissions(s.join(".Trashes"), fs::Permissions::from_mode(0o000)).unwrap();

    let out = run(&s, &d, &["--yes", "--exclude", ".Trashes"]);

    // Reset before the assertions, otherwise the tempdir cannot be cleaned up.
    fs::set_permissions(s.join(".Trashes"), fs::Permissions::from_mode(0o755)).unwrap();

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"payload");
    assert!(!d.join(".Trashes").exists());
}

#[test]
fn invalid_exclude_pattern_is_reported() {
    let (_tmp, s, d) = setup();
    fs::write(s.join("a.txt"), b"x").unwrap();

    let out = run(&s, &d, &["--yes", "--exclude", "["]);

    assert!(!out.status.success(), "an invalid pattern must abort");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Invalid --exclude pattern"),
        "the error message must name the pattern: {stderr}"
    );
}
