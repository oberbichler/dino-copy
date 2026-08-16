#[cfg(not(unix))]
compile_error!("dino-copy only supports Unix systems (macOS, Linux): it requires Unix permissions and symlinks.");

mod filter;
mod plan;
mod progress;
mod scan;
mod sync;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Mirrors a source directory onto a target directory (one-way sync).
/// Afterwards the target holds exactly the same data as the source.
#[derive(Parser, Debug)]
#[command(name = "dino-copy", version, about)]
struct Cli {
    /// Source directory (the lead). Stays unchanged.
    source: PathBuf,

    /// Target directory. Becomes an exact mirror of the source.
    target: PathBuf,

    /// Additional blake3 verification (slower, 100% reliable).
    #[arg(long)]
    checksum: bool,

    /// Only show what would happen, change nothing.
    #[arg(long)]
    dry_run: bool,

    /// Skip the confirmation (non-interactive).
    #[arg(long)]
    yes: bool,

    /// Copy parallelism (0 = automatic: SSD=CPUs, HDD/unknown=2).
    #[arg(long, default_value_t = 0)]
    jobs: usize,

    /// Scan parallelism (0 = jwalk default).
    #[arg(long = "scan-jobs", default_value_t = 0)]
    scan_jobs: usize,

    /// Persist every file with fsync before the final rename
    /// (safer on power loss, but slower).
    #[arg(long)]
    fsync: bool,

    /// Permitted mtime difference in seconds for a file to still count as
    /// unchanged (FAT32 stores mtimes at 2s resolution).
    #[arg(long = "mtime-window", default_value_t = 2)]
    mtime_window: i64,

    /// Abort if more than N entries would have to be deleted in the target.
    /// Unlimited when omitted; `0` forbids any deletion.
    #[arg(long = "max-delete", value_name = "N")]
    max_delete: Option<u64>,

    /// Allow emptying the target when the source is empty. The default is to
    /// abort, because an empty source usually means an unmounted disk.
    #[arg(long = "allow-empty-source")]
    allow_empty_source: bool,

    /// Delete even when directories could not be read or copies failed. The
    /// default is to skip the deletions: after an incomplete run the
    /// extraneous target entries may be the only remaining reachable version
    /// of the data.
    #[arg(long = "ignore-errors")]
    ignore_errors: bool,

    /// Exclude paths (can be given multiple times). Patterns without `/` match
    /// the file name at any depth (`.DS_Store`, `*.tmp`), patterns with `/`
    /// match the full relative path (`sub/*.txt`). Applies to source and
    /// target: target entries excluded this way are not deleted either.
    #[arg(long, value_name = "PATTERN")]
    exclude: Vec<String>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // Abort signals (Ctrl-C, SIGTERM, SIGHUP): clean up in-flight temp files,
    // then exit cleanly. temp+rename keeps the target consistent (only fully
    // copied files are adopted). Without SIGTERM/SIGHUP a possibly huge temp
    // file would be left behind on a cron timeout or at logout.
    ctrlc::set_handler(|| {
        eprintln!("\nAborting. Cleaning up temp files ...");
        sync::cleanup_temps();
        eprintln!("The target stays consistent (half-copied files were not adopted).");
        std::process::exit(130);
    })
    .ok();

    let source_root = cli.source;
    let dest_root = cli.target;

    // 1. Validate the inputs.
    if !source_root.is_dir() {
        bail!("Source is not a directory: {}", source_root.display());
    }
    // The target must exist; otherwise we create it (empty target directory).
    // Not in a dry run: it must not change anything, and a typo in the target
    // path should not silently turn into a real directory.
    if !dest_root.exists() {
        if cli.dry_run {
            println!("Target does not exist and would be created: {}", dest_root.display());
        } else {
            std::fs::create_dir_all(&dest_root)
                .with_context(|| format!("Cannot create target: {}", dest_root.display()))?;
        }
    } else if !dest_root.is_dir() {
        bail!("Target is not a directory: {}", dest_root.display());
    }
    // Reject identical or nested paths: canonicalize resolves "..", symlinks
    // and relative paths, so aliases are recognized too. Without this guard,
    // `dino-copy a/sub a` would delete the source directory, for example.
    check_disjoint(&source_root, &dest_root)?;

    println!("Source: {}", source_root.display());
    println!("Target: {}", dest_root.display());

    // 2. Scan both trees in parallel. Source and target typically live on
    // different disks (two external HDDs), so rayon::join overlaps the I/O of
    // both scans instead of serializing them.
    println!("Scanning directories ...");
    // The same filter for both trees: excluded target entries are invisible
    // that way and are neither overwritten nor deleted.
    let filter = filter::Filter::new(&cli.exclude)?;
    let (src_res, dst_res) = rayon::join(
        || scan::scan_tree(&source_root, cli.scan_jobs, &filter),
        || {
            // In a dry run the target may still be missing (see above): then it
            // is empty by definition, rather than letting the scan fail.
            if dest_root.is_dir() {
                scan::scan_tree(&dest_root, cli.scan_jobs, &filter)
            } else {
                Ok(scan::Scan { entries: std::collections::BTreeMap::new(), unreadable: Vec::new() })
            }
        },
    );
    let src_scan = src_res.with_context(|| format!("Scan {}", source_root.display()))?;
    let dst_scan = dst_res.with_context(|| format!("Scan {}", dest_root.display()))?;

    // Unreadable directories make the comparison incomplete: their contents are
    // missing from the map even though they exist. Report that visibly instead
    // of pretending they were empty.
    let mut unreadable = 0usize;
    for (root, scan) in [(&source_root, &src_scan), (&dest_root, &dst_scan)] {
        for (rel, err) in &scan.unreadable {
            eprintln!("Warning: contents not readable: {} ({err})", root.join(rel).display());
            unreadable += 1;
        }
    }

    let src_map = src_scan.entries;
    let dst_map = dst_scan.entries;

    // Determine the copy/hash parallelism once: on macOS is_ssd spawns a
    // diskutil process, and that must not happen repeatedly.
    let effective_jobs = if cli.jobs > 0 {
        cli.jobs
    } else {
        sync::default_copy_jobs(&dest_root)
    };

    // 3. Optional: blake3 verification for seemingly unchanged files.
    // Mismatches are fed into the diff as forced copies so that they run
    // through the same logic (statistics, SetDirMeta for touched dirs).
    let mut forced = std::collections::BTreeSet::new();
    if cli.checksum {
        use plan::EntryKind;
        use rayon::prelude::*;

        // Candidates: files that count as unchanged by size+mtime — only these
        // are additionally verified via blake3.
        let candidates: Vec<PathBuf> = src_map
            .iter()
            .filter_map(|(rel, s)| {
                if s.kind != EntryKind::File {
                    return None;
                }
                match dst_map.get(rel) {
                    Some(d)
                        if d.kind == EntryKind::File
                            && plan::unchanged(s, d, cli.mtime_window) =>
                    {
                        Some(rel.clone())
                    }
                    _ => None,
                }
            })
            .collect();

        // Hash in parallel: across the candidates (par_iter) and, per file,
        // source and target at the same time (rayon::join — they live on
        // different disks, so the reads overlap). Pick the parallelism like the
        // copy phase does (SSD: many, HDD/unknown: conservative) so rotating
        // disks do not end up seek-thrashing. To be safe, a read error counts
        // as "different" (the file is then copied).
        // Show progress: the verification reads every candidate file in full on
        // both sides and can take a long time on large data sets — without a
        // display dino-copy appears frozen meanwhile.
        let bar = indicatif::ProgressBar::new(candidates.len() as u64);
        bar.set_style(
            indicatif::ProgressStyle::with_template(
                "{spinner} Checksums [{bar:40}] {pos}/{len} files",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        let check = || {
            candidates
                .par_iter()
                .filter(|rel| {
                    let rel = rel.as_path();
                    let sp = source_root.join(rel);
                    let dp = dest_root.join(rel);
                    let (hs, hd) =
                        rayon::join(|| plan::hash_file(&sp), || plan::hash_file(&dp));
                    bar.inc(1);
                    match (hs, hd) {
                        (Ok(hs), Ok(hd)) => hs != hd,
                        _ => true,
                    }
                })
                .cloned()
                .collect::<Vec<PathBuf>>()
        };
        let differing = match rayon::ThreadPoolBuilder::new()
            .num_threads(effective_jobs)
            .build()
        {
            Ok(pool) => pool.install(check),
            Err(_) => check(),
        };
        bar.finish_and_clear();
        forced.extend(differing);
    }

    // 4. Plan the diff.
    let (mut actions, mut stats) =
        plan::diff_with_forced(&src_map, &dst_map, cli.mtime_window, &forced);

    // After an incomplete scan, deleting is not defensible: whatever lies below
    // an unreadable directory would look like an extraneous target entry and
    // would be removed — even though the source does contain it, just
    // unreadably.
    if unreadable > 0 && !cli.ignore_errors && stats.delete_count > 0 {
        eprintln!(
            "Warning: {} planned deletion(s) are skipped because {} \
             directory/directories are not readable (--ignore-errors forces them).",
            stats.delete_count, unreadable
        );
        actions.retain(|a| {
            !matches!(a, plan::Action::DeleteFile(_) | plan::Action::DeleteDir(_))
        });
        stats.delete_count = 0;
    }

    println!(
        "Plan: copy {} ({:.2} MB), delete {}, {} directories, {} file metadata, {} skipped.",
        stats.copy_count,
        stats.copy_bytes as f64 / 1_000_000.0,
        stats.delete_count,
        stats.mkdir_count,
        stats.meta_count,
        stats.skip_count
    );

    // 5. Safety nets against mass deletion. Both take effect BEFORE the dry-run
    // exit, so that a trial run reports them too, and before the confirmation,
    // so that --yes cannot bypass them.
    if src_map.is_empty() && !dst_map.is_empty() && !cli.allow_empty_source {
        bail!(
            "Source {} is empty, but the target holds {} entries. Aborting: this \
             looks like an unmounted source disk. If the target really should be \
             emptied: --allow-empty-source",
            source_root.display(),
            dst_map.len()
        );
    }
    if let Some(max) = cli.max_delete {
        if stats.delete_count > max {
            bail!(
                "{} deletions exceed the limit --max-delete {}. Aborting, \
                 nothing changed.",
                stats.delete_count,
                max
            );
        }
    }

    if cli.dry_run {
        println!("(dry-run: no changes)");
        if unreadable > 0 {
            return Err(incomplete_scan_error(unreadable));
        }
        return Ok(());
    }

    // The root produces no action (it appears in neither map), but it has to be
    // mirrored as well — otherwise a pure metadata drift of the root would be
    // permanent.
    let root_meta_pending = sync::root_meta_differs(&source_root, &dest_root, cli.mtime_window);

    if actions.is_empty() && !root_meta_pending {
        println!("Nothing to do. Target is already in sync.");
        if unreadable > 0 {
            return Err(incomplete_scan_error(unreadable));
        }
        return Ok(());
    }

    // 6. Confirmation when there are deletions.
    if stats.delete_count > 0 && !cli.yes && !confirm(stats.delete_count)? {
        println!("Aborted.");
        return Ok(());
    }

    // 7. Execute with progress.
    let prog = progress::Progress::new(stats.copy_bytes, stats.copy_count);
    let errors = Mutex::new(Vec::new());
    let report = sync::execute(
        &source_root,
        &dest_root,
        &actions,
        sync::ExecOptions {
            jobs: effective_jobs,
            fsync: cli.fsync,
            ignore_errors: cli.ignore_errors,
        },
        &prog,
        &errors,
    );
    prog.finish();

    let errs = errors.into_inner().unwrap();
    if !errs.is_empty() {
        // report.failed is the true total; the list itself is capped
        // (sync::MAX_STORED_ERRORS) so that mass failures do not blow up memory.
        eprintln!("\n{} errors:", report.failed);
        for e in &errs {
            eprintln!("  {e}");
        }
    }

    if report.skipped_deletes > 0 {
        eprintln!(
            "Warning: {} deletion(s) skipped because {} action(s) failed \
             (--ignore-errors forces them).",
            report.skipped_deletes, report.failed
        );
    }

    // The root itself comes last: every change in the target just touched its
    // mtime, so do this only after all writes.
    if let Err(e) = sync::mirror_root_meta(&source_root, &dest_root) {
        eprintln!("Warning: target directory metadata not applied: {e}");
    }

    if report.failed > 0 {
        bail!("{} actions failed.", report.failed);
    }
    // Whatever was readable has been copied — but "the target now mirrors the
    // source" would be a lie as long as directories stayed unread.
    if unreadable > 0 {
        return Err(incomplete_scan_error(unreadable));
    }

    println!("Done. The target now mirrors the source.");
    Ok(())
}

/// Error for an incomplete run. Unreadable directories mean that dino-copy does
/// not fully know the source inventory: the mirror cannot be exact, and that
/// must be reflected in the exit code (cron job).
fn incomplete_scan_error(count: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "{count} directory/directories could not be read - the mirror is \
         incomplete."
    )
}

/// Resolves `path` as far as it exists: the deepest existing ancestor is
/// canonicalized, and the not-yet-existing remainder is appended lexically.
///
/// Necessary because `--dry-run` must not create the target, yet
/// [`check_disjoint`] still needs resolved paths — otherwise `..`, a symlink or
/// a relative path could not be recognized as nesting.
///
/// Limitation: `..` inside the not-yet-existing part is not resolved. That is
/// harmless, because by definition that part is not a directory one could
/// descend into.
fn resolve_existing(path: &std::path::Path) -> Result<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&cur) {
            Ok(mut base) => {
                for name in tail.iter().rev() {
                    base.push(name);
                }
                return Ok(base);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let name = cur
                    .file_name()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Cannot resolve path: {}", path.display())
                    })?
                    .to_os_string();
                tail.push(name);
                cur.pop();
                // Relative path without a parent: resolve against the working
                // directory.
                if cur.as_os_str().is_empty() {
                    cur = PathBuf::from(".");
                }
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Cannot resolve path: {}", path.display()))
            }
        }
    }
}

/// Makes sure that source and target are neither identical nor nested inside
/// one another. The target may still be missing (dry run).
fn check_disjoint(source: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let src = resolve_existing(source)
        .with_context(|| format!("Cannot resolve source: {}", source.display()))?;
    let dst = resolve_existing(dest)
        .with_context(|| format!("Cannot resolve target: {}", dest.display()))?;
    if src == dst {
        bail!("Source and target are identical: {}", src.display());
    }
    if dst.starts_with(&src) {
        bail!(
            "Target lies inside the source: {} contains {}",
            src.display(),
            dst.display()
        );
    }
    if src.starts_with(&dst) {
        bail!(
            "Source lies inside the target: {} contains {} (would delete the source!)",
            dst.display(),
            src.display()
        );
    }
    Ok(())
}

fn confirm(delete_count: u64) -> Result<bool> {
    print!("{delete_count} files/folders will be DELETED in the target. Continue? [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn resolve_existing_matches_canonicalize_for_existing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("there");
        std::fs::create_dir(&dir).unwrap();

        assert_eq!(resolve_existing(&dir).unwrap(), std::fs::canonicalize(&dir).unwrap());
    }

    #[test]
    fn resolve_existing_appends_the_missing_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();

        let got = resolve_existing(&tmp.path().join("missing")).unwrap();

        assert_eq!(got, base.join("missing"));
    }

    #[test]
    fn resolve_existing_handles_several_missing_components() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();

        let got = resolve_existing(&tmp.path().join("a/b/c")).unwrap();

        assert_eq!(got, base.join("a").join("b").join("c"));
    }

    #[test]
    fn resolve_existing_resolves_a_bare_relative_name_against_the_cwd() {
        // Without the special handling of the empty parent, the loop would spin
        // forever here: cur.pop() yields "" and canonicalize("") is NotFound
        // again.
        let got = resolve_existing(Path::new("doesnotexistxyz")).unwrap();

        assert_eq!(got, std::fs::canonicalize(".").unwrap().join("doesnotexistxyz"));
    }

    #[test]
    fn resolve_existing_follows_symlinks_in_the_existing_part() {
        // The nesting guard depends on this: a symlink must not be able to
        // bypass the detection.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let got = resolve_existing(&link.join("missing")).unwrap();

        assert_eq!(got, std::fs::canonicalize(&real).unwrap().join("missing"));
    }
}
