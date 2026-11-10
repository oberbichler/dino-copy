//! dino-copy: a fast one-way mirror for local disks.
//!
//! The mirror engine lives in the `dino-copy-core` library (shared with the
//! desktop app); this crate adds the terminal display ([`progress`]).
//!
//! This file holds the CLI, the safety checks before anything is touched
//! (nested paths, empty source, `--max-delete`, confirmation) and the report.

#![forbid(unsafe_code)]

mod progress;

use dino_copy_core::{filter, guard, sync, walk};

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Mirrors a source directory onto a target directory (one-way sync).
/// Afterwards the target holds exactly the same data as the source.
#[derive(Parser, Debug)]
#[command(
    name = "dino-copy",
    version,
    about,
    long_about = "Fast one-way directory mirror (e.g. for syncing two external USB HDDs): \
                  copies new/changed files, skips unchanged ones, deletes extras and \
                  preserves mtime and permissions.\n\n\
                  The source is only ever read; every write goes to the target."
)]
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

    /// Parallel copies and checksum reads.
    ///
    /// The default of 2 is safe for hard disks, where more parallel access
    /// makes the head seek back and forth, and also fastest for many tiny
    /// files. On SSDs, higher values (e.g. 8) speed up medium-sized files and
    /// --checksum.
    #[arg(long, default_value = "2")]
    jobs: std::num::NonZeroUsize,

    /// Scan parallelism (0 = one thread per CPU).
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

    /// Delete even when directories could not be read, copies failed or names
    /// could not be mirrored. The default is to skip the deletions: after an
    /// incomplete run the extraneous target entries may be the only remaining
    /// reachable version of the data.
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
    guard::check_disjoint(&source_root, &dest_root)?;

    println!("Source: {}", source_root.display());
    println!("Target: {}", dest_root.display());

    // 2. Pass 1 (plan): walk both trees directory by directory and count what
    // has to happen. Nothing is kept per entry, so memory stays bounded by the
    // tree depth instead of growing with the number of files.
    println!("Scanning directories ...");
    // The same filter for both trees: excluded target entries are invisible
    // that way and are neither overwritten nor deleted.
    let filter = filter::Filter::new(&cli.exclude)?;
    let roots = walk::Roots {
        src: &source_root,
        dst: &dest_root,
        filter: &filter,
        mtime_tol: cli.mtime_window,
        // Ctrl-C ends the process (see the handler above) instead.
        cancel: None,
    };

    // A fixed default rather than guessing the hardware: whether more
    // parallelism helps depends more on the data (tiny files get slower) than
    // on the disk type, and disk detection is unreliable exactly for external
    // disks behind USB bridges.
    let effective_jobs = cli.jobs.get();

    // With --checksum, pass 1 also verifies every file that looks unchanged by
    // size+mtime via blake3. Only the mismatches are kept (as forced copies).
    let scan_progress = progress::ScanProgressBars::new(cli.checksum);
    let planned = walk::plan(
        &roots,
        walk::PlanOptions {
            scan_jobs: cli.scan_jobs,
            checksum: cli.checksum,
            hash_jobs: effective_jobs,
            details: false,
        },
        Some(&scan_progress),
    );
    scan_progress.finish();
    let mut stats = planned.stats.clone();
    let unreadable = planned.unreadable;
    // Everything on the target that would be removed if the source is empty.
    let target_entries = stats.remove_count + stats.conflict_count;

    // After an incomplete scan, deleting is not defensible: whatever lies below
    // an unreadable directory would look like an extraneous target entry and
    // would be removed — even though the source does contain it, just
    // unreadably. Pass 2 enforces the same rule.
    if unreadable > 0 && !cli.ignore_errors && stats.remove_count > 0 {
        eprintln!(
            "Warning: {} planned deletion(s) are skipped because {} \
             directory/directories are not readable (--ignore-errors forces them).",
            stats.remove_count, unreadable
        );
        stats.remove_count = 0;
    }
    // A name collision leaves a source entry out: it fails the run (pass 2
    // reports it as an error), which in turn blocks the deletions. It does not
    // go away by itself, so say how to resolve it.
    if planned.collisions > 0 {
        eprintln!("{COLLISION_HINT}");
    }
    if planned.collisions > 0 && !cli.ignore_errors && stats.remove_count > 0 {
        eprintln!(
            "Warning: {} planned deletion(s) are skipped because {} entries cannot \
             be mirrored (--ignore-errors forces them).",
            stats.remove_count, planned.collisions
        );
        stats.remove_count = 0;
    }

    println!(
        "Plan: copy {} ({:.2} MB), delete {}, {} directories, {} file metadata, {} renamed, {} skipped.",
        stats.copy_count,
        stats.copy_bytes as f64 / 1_000_000.0,
        stats.delete_count(),
        stats.mkdir_count,
        stats.meta_count,
        stats.rename_count,
        stats.skip_count
    );

    // 3. Safety nets against mass deletion. Both take effect BEFORE the dry-run
    // exit, so that a trial run reports them too, and before the confirmation,
    // so that --yes cannot bypass them.
    if planned.source_empty && target_entries > 0 && !cli.allow_empty_source {
        bail!(
            "Source {} is empty, but the target holds {} entries. Aborting: this \
             looks like an unmounted source disk. If the target really should be \
             emptied: --allow-empty-source",
            source_root.display(),
            target_entries
        );
    }
    if let Some(max) = cli.max_delete {
        if stats.delete_count() > max {
            bail!(
                "{} deletions exceed the limit --max-delete {}. Aborting, \
                 nothing changed.",
                stats.delete_count(),
                max
            );
        }
    }

    if cli.dry_run {
        println!("(dry-run: no changes)");
        return incomplete_result(unreadable, planned.collisions);
    }

    // The root produces no action (the walk never compares it), but it has to
    // be mirrored as well — otherwise a pure metadata drift of the root would
    // be permanent.
    let root_meta_pending = sync::root_meta_differs(&source_root, &dest_root, cli.mtime_window);

    if stats.is_noop() && !root_meta_pending {
        println!("Nothing to do. Target is already in sync.");
        return incomplete_result(unreadable, planned.collisions);
    }

    // 4. Confirmation when there are deletions.
    if stats.delete_count() > 0 && !cli.yes && !confirm(stats.delete_count())? {
        println!("Aborted.");
        return Ok(());
    }

    // 5. Pass 2 (execute): walk again and mirror. Copies stream through a
    // bounded queue to the workers while the walk continues.
    let prog = progress::Progress::new(stats.copy_bytes, stats.copy_count, effective_jobs);
    let errors = Mutex::new(Vec::new());
    let report = walk::execute(
        &roots,
        &planned,
        walk::ExecOptions {
            jobs: effective_jobs,
            scan_jobs: cli.scan_jobs,
            fsync: cli.fsync,
            ignore_errors: cli.ignore_errors,
            // Never remove more than was shown and confirmed above.
            delete_budget: stats.remove_count,
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

    let mut exceeded = false;
    match report.skip_reason {
        None => {}
        Some(walk::SkipReason::FailedActions) => eprintln!(
            "Warning: {} deletion(s) skipped because {} action(s) failed \
             (--ignore-errors forces them).",
            report.skipped_deletes, report.failed
        ),
        Some(walk::SkipReason::Unreadable) => eprintln!(
            "Warning: {} deletion(s) skipped because {} directory/directories are \
             not readable (--ignore-errors forces them).",
            report.skipped_deletes, report.unreadable
        ),
        // Only the desktop app cancels in-process.
        Some(walk::SkipReason::Cancelled) => {}
        Some(walk::SkipReason::Unplanned { planned, found }) => {
            eprintln!(
                "Warning: found {found} deletion(s) while mirroring that do not match \
                 the {planned} planned. The source or target changed (or was unmounted) \
                 during the run, so nothing was deleted. Run dino-copy again."
            );
            exceeded = true;
        }
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
    let unreadable = unreadable.max(report.unreadable);
    if unreadable > 0 {
        return Err(incomplete_scan_error(unreadable));
    }
    if exceeded {
        bail!("Deletions skipped because the source changed during the run.");
    }

    println!("Done. The target now mirrors the source.");
    Ok(())
}

/// Error for an incomplete run. Unreadable directories mean that dino-copy does
/// not fully know the source inventory: the mirror cannot be exact, and that
/// must be reflected in the exit code (cron job).
/// Explanation printed once when source names collide on the target.
const COLLISION_HINT: &str = "\
Hint: the target file system cannot tell some source names apart (they differ
only in case, e.g. \"Foo\" and \"foo\"), so it can hold only one of them. Until
this is resolved, every run reports it and withholds all deletions. Either
rename one of the entries in the source, leave one out with --exclude, or use
a target volume that distinguishes case (e.g. APFS (Case-sensitive)).";

/// `Ok` for a complete mirror, otherwise the error describing what is missing.
fn incomplete_result(unreadable: u64, collisions: u64) -> Result<()> {
    if unreadable > 0 {
        return Err(incomplete_scan_error(unreadable));
    }
    if collisions > 0 {
        bail!("{collisions} entries could not be mirrored (name collisions) - the mirror is incomplete.");
    }
    Ok(())
}

fn incomplete_scan_error(count: u64) -> anyhow::Error {
    anyhow::anyhow!(
        "{count} directory/directories could not be read - the mirror is \
         incomplete."
    )
}


fn confirm(delete_count: u64) -> Result<bool> {
    print!("{delete_count} files/folders will be DELETED in the target. Continue? [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
}
