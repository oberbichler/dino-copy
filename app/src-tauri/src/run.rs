//! One mirror or scan at a time, on a background thread.
//!
//! The flow follows the command line tool (`src/main.rs`) step by step, with
//! the confirmation replaced by "the user pressed Start": plan (pass 1), the
//! safety checks, then execute (pass 2). Every decision about what is copied
//! or deleted is made by `dino-copy-core`.

use dino_copy_core::filter::Filter;
use dino_copy_core::guard;
use dino_copy_core::problem::{Action, Problem};
use dino_copy_core::sync::{self, ProgressSink};
use dino_copy_core::walk::{
    self, DetailKind, ExecOptions, Plan, PlanObserver, PlanOptions, Roots, SkipReason,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Runtime};

/// Folders macOS keeps on every volume. They cannot be mirrored (some are
/// not even readable) and would make every disk-to-disk run fail and withhold
/// its deletions, so the app always leaves them out — on both sides.
pub const MACOS_SYSTEM_FOLDERS: &[&str] = &[
    ".Spotlight-V100",
    ".fseventsd",
    ".Trashes",
    ".DocumentRevisions-V100",
    ".TemporaryItems",
];

/// Copy parallelism: the CLI's default, safe for hard disks.
const JOBS: usize = 2;
/// How often progress is sent to the window.
const TICK: Duration = Duration::from_millis(250);
/// Technical problem lines passed on to the window (the counts in the
/// issues are always complete).
const MAX_DETAILS: usize = 200;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Make the target an exact copy of the source.
    Mirror,
    /// Only report what a mirror would do.
    Scan,
}

/// The cancel flag of the running job, if any.
#[derive(Default)]
pub struct RunState {
    current: Mutex<Option<Arc<AtomicBool>>>,
}

impl RunState {
    pub fn cancel(&self) {
        if let Some(flag) = self.current.lock().unwrap().as_ref() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// Events to the window
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    /// Pass 1: comparing source and target.
    Scanning,
    /// Pass 2: copying (cancellable).
    Copying,
    /// Pass 2, the final removals (no longer cancellable).
    Deleting,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    bytes_done: u64,
    bytes_total: u64,
    files_done: u64,
    files_total: u64,
    current: String,
    bytes_per_sec: f64,
    /// Estimated seconds left; `None` until the rate is known.
    eta_secs: Option<f64>,
    /// Done work over all work (see [`Estimator`]), for the bar.
    fraction: f64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum Outcome {
    /// The target now mirrors the source.
    Success,
    /// Nothing had to be done.
    InSync,
    /// Scan finished (nothing was changed).
    Scanned,
    /// The run finished, but the target is not an exact mirror.
    Incomplete,
    /// The run did not start or stopped with an error.
    Failed,
    Cancelled,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    copy_count: u64,
    copy_bytes: u64,
    delete_count: u64,
    rename_count: u64,
    mkdir_count: u64,
    unchanged_count: u64,
}

/// One planned change for the scan view, kept compact (a first scan onto an
/// empty disk can list hundreds of thousands).
#[derive(Clone, Serialize)]
struct ScanItem {
    /// c = copy, m = new folder, d = delete, x = replace, r = rename.
    k: &'static str,
    /// Relative path.
    p: String,
    /// Size (copy) or number of entries (delete).
    n: u64,
    /// Is a directory (delete).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dir: bool,
    /// Current name on the target (rename).
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
}

/// Something to tell the user. The window turns it into a sentence in the
/// user's language; only [`Msg::Detail`] carries text as it is (warnings and
/// I/O errors from the engine, which name paths and system errors).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "key", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Msg {
    // Why a pair cannot be started.
    SourceMissing {
        path: String,
    },
    TargetMissing {
        path: String,
    },
    Unresolvable {
        path: String,
    },
    Identical,
    TargetInsideSource,
    SourceInsideTarget,
    RunInProgress,
    // Headlines.
    CancelledUnchanged,
    SourceEmpty {
        entries: u64,
    },
    ScanNothingToDo,
    ScanFinished,
    InSync,
    CancelledPartly,
    FinishedWithErrors {
        count: u64,
    },
    FinishedIncomplete,
    Mirrored {
        target: String,
        source: String,
    },
    // Further messages.
    DeletionsWillBeSkipped {
        count: u64,
        unreadable: u64,
        collisions: u64,
    },
    RootMetaFailed {
        error: String,
    },
    SkippedAfterFailures {
        count: u64,
    },
    SkippedUnreadable {
        count: u64,
    },
    SkippedChanged {
        count: u64,
    },
    Detail {
        text: String,
    },
}

/// A kind of problem, as the user sees it: what went wrong and what to do.
/// The order is the order of importance in the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum IssueKind {
    /// The target has no space left.
    TargetFull,
    /// The target cannot be written at all.
    ReadOnly,
    /// macOS denied access (privacy settings or permissions).
    NoPermission,
    /// A file is too large for the target's file system (FAT32: 4 GB).
    TooLarge,
    /// A name the target's file system does not allow.
    BadName,
    /// Names that differ only in case or normalization.
    NameClash,
    /// A folder that could not be read.
    Unreadable,
    /// Something changed or vanished while the run was going on.
    Changed,
    /// Anything else (I/O errors, a disk that went away, ...).
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Issue {
    kind: IssueKind,
    count: u64,
}

/// How a problem shows up in the window; `None` for harmless notes.
fn issue_kind(p: &Problem) -> Option<IssueKind> {
    use IssueKind::*;
    Some(match (&p.action, p.error) {
        (Action::Vanished { .. } | Action::SpecialFile | Action::Suppressed, _) => return None,
        (Action::Collision, _) => NameClash,
        (_, Some(ErrorKind::StorageFull | ErrorKind::QuotaExceeded)) => TargetFull,
        (_, Some(ErrorKind::ReadOnlyFilesystem)) => ReadOnly,
        (_, Some(ErrorKind::PermissionDenied)) => NoPermission,
        (_, Some(ErrorKind::FileTooLarge)) => TooLarge,
        (_, Some(ErrorKind::InvalidFilename)) => BadName,
        (Action::Unreadable, _) => Unreadable,
        (Action::TypeChanged, _) | (_, Some(ErrorKind::NotFound)) => Changed,
        _ => Other,
    })
}

/// Problems grouped by kind, most important first. The engine stores only
/// the first problems of a run; `uncounted` further failures are added to the
/// most frequent kind (mass failures are nearly always of one kind, such as a
/// full target).
fn issues(problems: &[Problem], uncounted: u64) -> Vec<Issue> {
    let mut counts: BTreeMap<IssueKind, u64> = BTreeMap::new();
    for kind in problems.iter().filter_map(issue_kind) {
        *counts.entry(kind).or_default() += 1;
    }
    if uncounted > 0 {
        let top = counts
            .iter()
            .max_by_key(|&(_, n)| *n)
            .map_or(IssueKind::Other, |(k, _)| *k);
        *counts.entry(top).or_default() += uncounted;
    }
    counts
        .into_iter()
        .map(|(kind, count)| Issue { kind, count })
        .collect()
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunResult {
    mode: Mode,
    outcome: Outcome,
    /// One line for the headline.
    headline: Msg,
    /// What went wrong, grouped for the user.
    issues: Vec<Issue>,
    /// Further notes (deletions skipped, ...).
    messages: Vec<Msg>,
    /// Every problem as a technical line, for "Details" (capped at
    /// [`MAX_DETAILS`]).
    details: Vec<String>,
    summary: Summary,
    duration_secs: f64,
    /// Planned changes (scan only).
    #[serde(skip_serializing_if = "Option::is_none")]
    items: Option<Vec<ScanItem>>,
}

// ---------------------------------------------------------------------------
// Checks and start
// ---------------------------------------------------------------------------

/// Why `source` → `target` cannot be started.
pub fn check_pair(source: &Path, target: &Path) -> Result<(), Msg> {
    if !source.is_dir() {
        return Err(Msg::SourceMissing {
            path: source.display().to_string(),
        });
    }
    if !target.is_dir() {
        return Err(Msg::TargetMissing {
            path: target.display().to_string(),
        });
    }
    // The same rules as `guard::check_disjoint`, told apart for the window.
    let resolve = |p: &Path| {
        guard::resolve_existing(p).map_err(|_| Msg::Unresolvable {
            path: p.display().to_string(),
        })
    };
    let (src, dst) = (resolve(source)?, resolve(target)?);
    if src == dst {
        Err(Msg::Identical)
    } else if dst.starts_with(&src) {
        Err(Msg::TargetInsideSource)
    } else if src.starts_with(&dst) {
        Err(Msg::SourceInsideTarget)
    } else {
        Ok(())
    }
}

pub fn start(
    app: AppHandle,
    state: &RunState,
    mode: Mode,
    source: PathBuf,
    target: PathBuf,
) -> Result<(), Msg> {
    check_pair(&source, &target)?;
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut current = state.current.lock().unwrap();
        if current.is_some() {
            return Err(Msg::RunInProgress);
        }
        *current = Some(cancel.clone());
    }
    std::thread::Builder::new()
        .name("dino-copy-run".into())
        .spawn(move || {
            let _awake = KeepAwake::start();
            let result = run(&app, mode, &source, &target, &cancel);
            use tauri::Manager;
            app.state::<RunState>().current.lock().unwrap().take();
            let _ = app.emit("run://done", result);
        })
        .map_err(|e| Msg::Detail {
            text: e.to_string(),
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

fn run<R: Runtime>(
    app: &AppHandle<R>,
    mode: Mode,
    src: &Path,
    dst: &Path,
    cancel: &AtomicBool,
) -> RunResult {
    let started = Instant::now();
    let patterns: Vec<String> = MACOS_SYSTEM_FOLDERS.iter().map(|s| s.to_string()).collect();
    let filter = Filter::new(&patterns).expect("built-in patterns are valid");
    let roots = Roots {
        src,
        dst,
        filter: &filter,
        mtime_tol: 2,
        cancel: Some(cancel),
    };
    let finish =
        |outcome, headline: Msg, messages, problems: &[Problem], uncounted: u64, summary, items| {
            RunResult {
                mode,
                outcome,
                headline,
                issues: issues(problems, uncounted),
                messages,
                details: problems
                    .iter()
                    .take(MAX_DETAILS)
                    .map(ToString::to_string)
                    .collect(),
                summary,
                duration_secs: started.elapsed().as_secs_f64(),
                items,
            }
        };

    // Pass 1.
    phase(app, Phase::Scanning);
    let observer = Observer::new(app);
    let plan = walk::plan(
        &roots,
        PlanOptions {
            scan_jobs: 0,
            checksum: false,
            hash_jobs: JOBS,
            details: mode == Mode::Scan,
        },
        Some(&observer),
    );
    let mut problems = observer.take_warnings();
    let mut messages = Vec::new();
    let summary = summary_of(&plan);
    if plan.cancelled {
        return finish(
            Outcome::Cancelled,
            Msg::CancelledUnchanged,
            messages,
            &problems,
            0,
            summary,
            None,
        );
    }

    // The CLI's safety net against an unmounted source disk.
    let target_entries = plan.stats.remove_count + plan.stats.conflict_count;
    if plan.source_empty && target_entries > 0 {
        return finish(
            Outcome::Failed,
            Msg::SourceEmpty {
                entries: target_entries,
            },
            messages,
            &problems,
            0,
            summary,
            None,
        );
    }
    let deletions_allowed = plan.deletions_allowed(false);
    if !deletions_allowed && plan.stats.remove_count > 0 {
        messages.push(Msg::DeletionsWillBeSkipped {
            count: plan.stats.remove_count,
            unreadable: plan.unreadable,
            collisions: plan.collisions,
        });
    }

    if mode == Mode::Scan {
        let items = scan_items(&plan);
        let headline = if plan.stats.is_noop() {
            Msg::ScanNothingToDo
        } else {
            Msg::ScanFinished
        };
        return finish(
            Outcome::Scanned,
            headline,
            messages,
            &problems,
            0,
            summary,
            Some(items),
        );
    }

    if plan.stats.is_noop() && !sync::root_meta_differs(src, dst, 2) {
        let outcome = if plan.unreadable + plan.collisions > 0 {
            Outcome::Incomplete
        } else {
            Outcome::InSync
        };
        return finish(outcome, Msg::InSync, messages, &problems, 0, summary, None);
    }

    // Pass 2.
    phase(app, Phase::Copying);
    let sink = Sink::new(app, plan.stats.copy_bytes, plan.stats.copy_count);
    let errors = Mutex::new(Vec::new());
    let report = std::thread::scope(|s| {
        let ticker = s.spawn(|| sink.tick_until_done(&errors));
        let report = walk::execute(
            &roots,
            &plan,
            ExecOptions {
                jobs: JOBS,
                scan_jobs: 0,
                fsync: false,
                ignore_errors: false,
                delete_budget: if deletions_allowed {
                    plan.stats.remove_count
                } else {
                    0
                },
            },
            &sink,
            &errors,
        );
        sink.done.store(true, Ordering::Relaxed);
        let _ = ticker.join();
        report
    });
    if !report.cancelled
        && let Err(e) = sync::mirror_root_meta(src, dst)
    {
        messages.push(Msg::RootMetaFailed {
            error: e.to_string(),
        });
    }

    let errs = errors.into_inner().unwrap();
    let stored = errs
        .iter()
        .filter(|p| p.action != Action::Suppressed)
        .count() as u64;
    let uncounted = report.failed.saturating_sub(stored);
    problems.extend(errs);
    if let Some(reason) = report.skip_reason
        && let Some(msg) = skip_msg(reason, report.skipped_deletes)
    {
        messages.push(msg);
    }
    let mut summary = summary;
    if report.skip_reason.is_some() {
        summary.delete_count = 0;
    }

    let (outcome, headline) = if report.cancelled {
        (Outcome::Cancelled, Msg::CancelledPartly)
    } else if report.failed > 0 {
        (
            Outcome::Incomplete,
            Msg::FinishedWithErrors {
                count: report.failed,
            },
        )
    } else if report.skip_reason.is_some()
        || plan.unreadable + plan.collisions + report.unreadable > 0
    {
        (Outcome::Incomplete, Msg::FinishedIncomplete)
    } else {
        (
            Outcome::Success,
            Msg::Mirrored {
                target: name_of(dst),
                source: name_of(src),
            },
        )
    };
    finish(
        outcome, headline, messages, &problems, uncounted, summary, None,
    )
}

fn summary_of(plan: &Plan) -> Summary {
    Summary {
        copy_count: plan.stats.copy_count,
        copy_bytes: plan.stats.copy_bytes,
        delete_count: plan.stats.delete_count(),
        rename_count: plan.stats.rename_count,
        mkdir_count: plan.stats.mkdir_count,
        unchanged_count: plan.stats.skip_count,
    }
}

fn skip_msg(reason: SkipReason, count: u64) -> Option<Msg> {
    Some(match reason {
        SkipReason::Cancelled => return None,
        SkipReason::FailedActions => Msg::SkippedAfterFailures { count },
        SkipReason::Unreadable => Msg::SkippedUnreadable { count },
        SkipReason::Unplanned { .. } => Msg::SkippedChanged { count },
    })
}

fn scan_items(plan: &Plan) -> Vec<ScanItem> {
    let mut items: Vec<ScanItem> = plan
        .details
        .iter()
        .map(|d| {
            let p = d.path.to_string_lossy().into_owned();
            match &d.kind {
                DetailKind::Copy { size } => ScanItem {
                    k: "c",
                    p,
                    n: *size,
                    dir: false,
                    from: None,
                },
                DetailKind::Mkdir => ScanItem {
                    k: "m",
                    p,
                    n: 0,
                    dir: true,
                    from: None,
                },
                DetailKind::Remove { is_dir, entries } => ScanItem {
                    k: "d",
                    p,
                    n: *entries,
                    dir: *is_dir,
                    from: None,
                },
                DetailKind::Replace => ScanItem {
                    k: "x",
                    p,
                    n: 0,
                    dir: false,
                    from: None,
                },
                DetailKind::Rename { from } => ScanItem {
                    k: "r",
                    p,
                    n: 0,
                    dir: false,
                    from: Some(from.to_string_lossy().into_owned()),
                },
            }
        })
        .collect();
    items.sort_by(|a, b| a.p.cmp(&b.p));
    items
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn phase<R: Runtime>(app: &AppHandle<R>, phase: Phase) {
    let _ = app.emit("run://phase", phase);
}

// ---------------------------------------------------------------------------
// Observers
// ---------------------------------------------------------------------------

/// Pass 1: counts listed entries (sent throttled) and collects warnings.
struct Observer<'a, R: Runtime> {
    app: &'a AppHandle<R>,
    listed: AtomicU64,
    last_emit: Mutex<Instant>,
    warnings: Mutex<Vec<Problem>>,
}

impl<'a, R: Runtime> Observer<'a, R> {
    fn new(app: &'a AppHandle<R>) -> Self {
        Self {
            app,
            listed: AtomicU64::new(0),
            last_emit: Mutex::new(Instant::now()),
            warnings: Mutex::new(Vec::new()),
        }
    }

    fn take_warnings(&self) -> Vec<Problem> {
        std::mem::take(&mut *self.warnings.lock().unwrap())
    }
}

impl<R: Runtime> PlanObserver for Observer<'_, R> {
    fn listed(&self, n: u64) {
        let total = self.listed.fetch_add(n, Ordering::Relaxed) + n;
        let mut last = self.last_emit.lock().unwrap();
        if last.elapsed() >= TICK {
            *last = Instant::now();
            let _ = self.app.emit("run://scan-progress", total);
        }
    }
    fn hashed(&self) {}
    fn warning(&self, problem: &Problem) {
        // Kept in full: the issue counts come from this list.
        self.warnings.lock().unwrap().push(problem.clone());
    }
}

/// Pass 2: progress counters, sent to the window by [`Sink::tick_until_done`].
struct Sink<'a, R: Runtime> {
    app: &'a AppHandle<R>,
    bytes_total: u64,
    files_total: u64,
    bytes: AtomicU64,
    files: AtomicU64,
    current: Mutex<String>,
    done: AtomicBool,
}

impl<'a, R: Runtime> Sink<'a, R> {
    fn new(app: &'a AppHandle<R>, bytes_total: u64, files_total: u64) -> Self {
        Self {
            app,
            bytes_total,
            files_total,
            bytes: AtomicU64::new(0),
            files: AtomicU64::new(0),
            current: Mutex::new(String::new()),
            done: AtomicBool::new(false),
        }
    }

    /// Sends progress every [`TICK`] until the run is done (see
    /// [`Estimator`]).
    fn tick_until_done(&self, errors: &Mutex<Vec<Problem>>) {
        let mut est = Estimator::default();
        let mut seen = 0;
        while !self.done.load(Ordering::Relaxed) {
            std::thread::sleep(TICK);
            // New problems: tell the window right away (a full target should
            // not wait for the end of a long run).
            let kinds = {
                let list = errors.lock().unwrap();
                (list.len() != seen).then(|| {
                    seen = list.len();
                    issues(&list, 0)
                        .into_iter()
                        .map(|i| i.kind)
                        .collect::<Vec<_>>()
                })
            };
            if let Some(kinds) = kinds {
                let _ = self.app.emit("run://issues", kinds);
            }
            let p = est.update(
                Instant::now(),
                self.bytes.load(Ordering::Relaxed),
                self.bytes_total,
                self.files.load(Ordering::Relaxed),
                self.files_total,
                self.current.lock().unwrap().clone(),
            );
            let _ = self.app.emit("run://progress", p);
        }
    }
}

impl<R: Runtime> ProgressSink for Sink<'_, R> {
    fn add_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }
    fn set_current(&self, _worker: usize, name: &str) {
        *self.current.lock().unwrap() = name.to_string();
    }
    fn set_done(&self, _worker: usize) {}
    fn inc_files(&self) {
        self.files.fetch_add(1, Ordering::Relaxed);
    }
    fn begin_removals(&self, _entries: u64) {
        phase(self.app, Phase::Deleting);
    }
}

/// Transfer rates and the remaining time, measured over the last
/// [`Estimator::WINDOW`].
///
/// The remaining work counts every byte plus [`Estimator::PER_FILE`] for every
/// file, so that many tiny files are not "almost done" when their few bytes
/// are, while a large file moves the bar as its bytes go. Measured over a
/// window rather than tick by tick, neither the system's write cache (data
/// arrives in bursts) nor large files (one completes every now and then) throw
/// the speed, the estimate or the bar around; and the bar, being done work
/// over all work, never moves back.
#[derive(Default)]
struct Estimator {
    /// (time, bytes, work) per tick, oldest first, spanning the window.
    samples: VecDeque<(Instant, u64, u64)>,
}

impl Estimator {
    /// How far back the rates look.
    const WINDOW: Duration = Duration::from_secs(20);
    /// How much has to be seen before there is a rate at all.
    const MIN_SPAN: Duration = Duration::from_secs(3);
    /// The work of one file besides its bytes (opening, creating, metadata),
    /// in bytes: roughly what an external disk transfers in that time.
    const PER_FILE: u64 = 512 * 1024;

    fn update(
        &mut self,
        now: Instant,
        bytes: u64,
        bytes_total: u64,
        files: u64,
        files_total: u64,
        current: String,
    ) -> Progress {
        let work =
            |bytes: u64, files: u64| bytes.saturating_add(files.saturating_mul(Self::PER_FILE));
        let (done, total) = (work(bytes, files), work(bytes_total, files_total));

        self.samples.push_back((now, bytes, done));
        // Keep one sample at or beyond the window's start.
        while self.samples.len() > 2 && now.duration_since(self.samples[1].0) >= Self::WINDOW {
            self.samples.pop_front();
        }
        let (t0, bytes0, done0) = self.samples[0];
        let span = now.duration_since(t0);
        let rate = |from: u64, to: u64| {
            (span >= Self::MIN_SPAN).then(|| to.saturating_sub(from) as f64 / span.as_secs_f64())
        };
        let work_rate = rate(done0, done).filter(|r| *r > 0.0);
        let left = total.saturating_sub(done);

        Progress {
            bytes_done: bytes,
            bytes_total,
            files_done: files,
            files_total,
            current,
            bytes_per_sec: rate(bytes0, bytes).unwrap_or(0.0),
            eta_secs: if total > 0 && left == 0 {
                Some(0.0)
            } else {
                work_rate.map(|r| left as f64 / r)
            },
            fraction: if total == 0 {
                1.0
            } else {
                done as f64 / total as f64
            },
        }
    }
}

/// Keeps the Mac from idle-sleeping while a run is in progress (displays may
/// still sleep). Ends with the guard, or with this process.
struct KeepAwake(Option<std::process::Child>);

impl KeepAwake {
    fn start() -> Self {
        Self(
            std::process::Command::new("/usr/bin/caffeinate")
                .args(["-i", "-w", &std::process::id().to_string()])
                .spawn()
                .ok(),
        )
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_or_missing_pairs_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(a.join("inner")).unwrap();

        assert_eq!(
            check_pair(&a, &a.join("inner")),
            Err(Msg::TargetInsideSource)
        );
        assert_eq!(
            check_pair(&a.join("inner"), &a),
            Err(Msg::SourceInsideTarget)
        );
        assert_eq!(check_pair(&a, &a), Err(Msg::Identical));
        assert!(check_pair(&a, &tmp.path().join("missing")).is_err());
        let b = tmp.path().join("b");
        std::fs::create_dir(&b).unwrap();
        assert!(check_pair(&a, &b).is_ok());
    }

    /// Runs the estimator over a copy that moves `bytes_at(t)` bytes and
    /// `files_at(t)` files by second `t`, ticking like the app, and returns
    /// every report with its time.
    fn simulate(
        secs: f64,
        bytes_total: u64,
        files_total: u64,
        bytes_at: impl Fn(f64) -> u64,
        files_at: impl Fn(f64) -> u64,
    ) -> Vec<(f64, Progress)> {
        let start = Instant::now();
        let mut e = Estimator::default();
        let mut out = Vec::new();
        let mut t = 0.0;
        while t <= secs {
            let now = start + Duration::from_secs_f64(t);
            let p = e.update(
                now,
                bytes_at(t),
                bytes_total,
                files_at(t),
                files_total,
                String::new(),
            );
            out.push((t, p));
            t += TICK.as_secs_f64();
        }
        out
    }

    #[test]
    fn the_estimate_waits_until_the_rate_is_known() {
        let r = simulate(1.0, 1000, 10, |t| (t * 100.0) as u64, |_| 0);
        assert!(r.iter().all(|(_, p)| p.eta_secs.is_none()));
        assert!(r.iter().all(|(_, p)| p.bytes_per_sec == 0.0));
        let done = simulate(0.0, 0, 0, |_| 0, |_| 0);
        assert_eq!(done[0].1.eta_secs, None);
        assert_eq!(done[0].1.fraction, 1.0);
    }

    /// Four 2 GB videos at a steady 100 MB/s: a file completes only every
    /// 20 s, which must not throw the estimate or the bar around.
    #[test]
    fn large_files_give_a_steady_estimate_and_bar() {
        const GB: u64 = 1_000_000_000;
        let rate = 100e6;
        let total = 4 * 2 * GB;
        let r = simulate(
            79.0,
            total,
            4,
            |t| (t * rate) as u64,
            |t| (t * rate) as u64 / (2 * GB),
        );
        let mut last_fraction = 0.0;
        for (t, p) in &r {
            assert!(p.fraction >= last_fraction, "bar went back at {t} s");
            last_fraction = p.fraction;
            if let Some(eta) = p.eta_secs {
                let actual = total as f64 / rate - t;
                assert!(
                    (eta - actual).abs() <= 0.1 * actual + 3.0,
                    "at {t} s: {eta} s left, actually {actual} s"
                );
            } else {
                assert!(*t < 5.0, "no estimate at {t} s");
            }
        }
    }

    /// 100,000 files of 4 KB at 500 files/s: the files, not the few bytes,
    /// decide how long the copy takes.
    #[test]
    fn many_tiny_files_are_not_almost_done_when_the_bytes_are() {
        let files = 100_000;
        let r = simulate(
            60.0,
            files * 4096,
            files,
            |t| (t * 500.0) as u64 * 4096,
            |t| (t * 500.0) as u64,
        );
        let (t, p) = r.last().unwrap();
        let actual = files as f64 / 500.0 - t;
        let eta = p.eta_secs.unwrap();
        assert!(
            (eta - actual).abs() <= 0.1 * actual,
            "{eta} s left, actually {actual} s"
        );
        assert!(
            (p.fraction - t / (files as f64 / 500.0)).abs() < 0.05,
            "bar at {}",
            p.fraction
        );
    }

    /// The disk takes data in bursts (the system's write cache): 400 MB/s for
    /// one second, nothing the next. Speed and estimate show the average.
    #[test]
    fn bursts_are_averaged_out() {
        let total = 100_000_000_000;
        let bytes_at = |t: f64| {
            let whole = t.floor();
            let in_burst = if (whole as u64).is_multiple_of(2) {
                t - whole
            } else {
                1.0
            };
            ((whole / 2.0).floor() * 400e6 + in_burst.min(1.0) * 400e6) as u64
        };
        let r = simulate(120.0, total, 10_000, bytes_at, |t| bytes_at(t) / 10_000_000);
        for (t, p) in r.iter().filter(|(t, _)| *t >= 30.0) {
            assert!(
                (p.bytes_per_sec - 200e6).abs() < 0.15 * 200e6,
                "at {t} s: {} B/s",
                p.bytes_per_sec
            );
            let actual = total as f64 / 200e6 - t;
            let eta = p.eta_secs.unwrap();
            assert!(
                (eta - actual).abs() <= 0.1 * actual,
                "at {t} s: {eta} s left, actually {actual} s"
            );
        }
    }

    // ---- whole runs against a mock app --------------------------------------

    use std::fs;
    use tauri::Listener;

    struct Trees {
        _tmp: tempfile::TempDir,
        src: PathBuf,
        dst: PathBuf,
    }

    fn trees() -> Trees {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
        fs::create_dir_all(src.join("photos")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("photos/a.jpg"), b"aaaa").unwrap();
        fs::write(src.join("notes.txt"), b"notes").unwrap();
        fs::write(dst.join("stale.txt"), b"gone in source").unwrap();
        Trees {
            _tmp: tmp,
            src,
            dst,
        }
    }

    /// Runs like the Start button, and records the phase events.
    fn run_mock(t: &Trees, mode: Mode, cancelled: bool) -> (RunResult, Vec<String>) {
        let app = tauri::test::mock_app();
        let phases = Arc::new(Mutex::new(Vec::new()));
        let seen = phases.clone();
        app.listen_any("run://phase", move |e| {
            seen.lock().unwrap().push(e.payload().to_string())
        });
        let cancel = AtomicBool::new(cancelled);
        let result = run(app.handle(), mode, &t.src, &t.dst, &cancel);
        let phases = phases.lock().unwrap().clone();
        (result, phases)
    }

    #[test]
    fn mirror_makes_the_target_an_exact_copy_and_reports_its_phases() {
        let t = trees();

        let (r, phases) = run_mock(&t, Mode::Mirror, false);

        assert!(
            matches!(r.outcome, Outcome::Success),
            "{:?}: {:?}",
            r.headline,
            r.messages
        );
        assert_eq!(fs::read(t.dst.join("photos/a.jpg")).unwrap(), b"aaaa");
        assert!(!t.dst.join("stale.txt").exists());
        assert_eq!(r.summary.copy_count, 2);
        assert_eq!(r.summary.delete_count, 1);
        assert_eq!(phases, [r#""scanning""#, r#""copying""#, r#""deleting""#]);
    }

    #[test]
    fn a_second_mirror_finds_nothing_to_do() {
        let t = trees();
        run_mock(&t, Mode::Mirror, false);

        let (r, _) = run_mock(&t, Mode::Mirror, false);

        assert!(matches!(r.outcome, Outcome::InSync), "{:?}", r.headline);
    }

    #[test]
    fn scan_lists_the_changes_and_touches_nothing() {
        let t = trees();

        let (r, _) = run_mock(&t, Mode::Scan, false);

        assert!(matches!(r.outcome, Outcome::Scanned));
        let items = r.items.expect("a scan returns its items");
        let listed: Vec<(&str, &str)> = items.iter().map(|i| (i.k, i.p.as_str())).collect();
        assert_eq!(
            listed,
            [
                ("c", "notes.txt"),
                ("m", "photos"),
                ("c", "photos/a.jpg"),
                ("d", "stale.txt")
            ]
        );
        assert!(!t.dst.join("notes.txt").exists());
        assert!(t.dst.join("stale.txt").exists());
    }

    #[test]
    fn a_cancelled_run_changes_nothing() {
        let t = trees();

        let (r, _) = run_mock(&t, Mode::Mirror, true);

        assert!(matches!(r.outcome, Outcome::Cancelled));
        assert!(t.dst.join("stale.txt").exists());
        assert!(!t.dst.join("notes.txt").exists());
    }

    #[test]
    fn an_empty_source_never_empties_the_target() {
        let t = trees();
        fs::remove_dir_all(&t.src).unwrap();
        fs::create_dir(&t.src).unwrap();

        let (r, _) = run_mock(&t, Mode::Mirror, false);

        assert!(matches!(r.outcome, Outcome::Failed), "{:?}", r.headline);
        assert!(
            matches!(r.headline, Msg::SourceEmpty { .. }),
            "{:?}",
            r.headline
        );
        assert!(t.dst.join("stale.txt").exists());
    }

    #[test]
    fn macos_system_folders_are_left_alone_on_both_sides() {
        let t = trees();
        fs::create_dir_all(t.src.join(".fseventsd")).unwrap();
        fs::write(t.src.join(".fseventsd/log"), b"source only").unwrap();
        fs::create_dir_all(t.dst.join(".Spotlight-V100")).unwrap();
        fs::write(t.dst.join(".Spotlight-V100/index"), b"target only").unwrap();

        let (r, _) = run_mock(&t, Mode::Mirror, false);

        assert!(
            matches!(r.outcome, Outcome::Success),
            "{:?}: {:?}",
            r.headline,
            r.messages
        );
        assert!(!t.dst.join(".fseventsd").exists(), "not copied");
        assert!(t.dst.join(".Spotlight-V100/index").exists(), "not deleted");
    }

    #[test]
    fn system_folders_are_valid_patterns() {
        let patterns: Vec<String> = MACOS_SYSTEM_FOLDERS.iter().map(|s| s.to_string()).collect();
        let f = Filter::new(&patterns).unwrap();
        assert!(f.excludes(Path::new(".Spotlight-V100")));
        assert!(!f.excludes(Path::new("Photos")));
    }

    /// The window translates messages by `key` and reads their fields by
    /// these names (see `msgText` in `app/src/main.ts`).
    #[test]
    fn messages_reach_the_window_as_key_and_camel_case_fields() {
        let json = |m: Msg| serde_json::to_value(m).unwrap();
        assert_eq!(
            json(Msg::Identical),
            serde_json::json!({ "key": "identical" })
        );
        assert_eq!(
            json(Msg::Mirrored {
                target: "B".into(),
                source: "A".into()
            }),
            serde_json::json!({ "key": "mirrored", "target": "B", "source": "A" })
        );
        assert_eq!(
            json(Msg::DeletionsWillBeSkipped {
                count: 3,
                unreadable: 1,
                collisions: 0
            }),
            serde_json::json!({
                "key": "deletionsWillBeSkipped", "count": 3, "unreadable": 1, "collisions": 0
            })
        );
        assert_eq!(
            json(Msg::SourceEmpty { entries: 2 }),
            serde_json::json!({ "key": "sourceEmpty", "entries": 2 })
        );
    }

    #[test]
    fn problems_are_grouped_by_what_the_user_can_do() {
        let os = |code| std::io::Error::from_raw_os_error(code);
        let io = |action, code| Problem::io(action, "x", &os(code));
        let kinds = |ps: &[Problem]| ps.iter().map(issue_kind).collect::<Vec<_>>();
        assert_eq!(
            kinds(&[
                io(Action::Copy, 28),       // ENOSPC
                io(Action::Copy, 69),       // EDQUOT
                io(Action::Mkdir, 30),      // EROFS
                io(Action::Unreadable, 13), // EACCES: privacy settings
                io(Action::Copy, 1),        // EPERM
                io(Action::Copy, 27),       // EFBIG: FAT32
                io(Action::Copy, 63),       // ENAMETOOLONG
                io(Action::Unreadable, 5),  // EIO
                io(Action::Copy, 2),        // ENOENT: vanished meanwhile
                io(Action::Copy, 5),        // EIO
            ]),
            [
                Some(IssueKind::TargetFull),
                Some(IssueKind::TargetFull),
                Some(IssueKind::ReadOnly),
                Some(IssueKind::NoPermission),
                Some(IssueKind::NoPermission),
                Some(IssueKind::TooLarge),
                Some(IssueKind::BadName),
                Some(IssueKind::Unreadable),
                Some(IssueKind::Changed),
                Some(IssueKind::Other),
            ]
        );
        assert_eq!(
            kinds(&[
                Problem::note(Action::Collision, "a", "x"),
                Problem::note(Action::TypeChanged, "a", ""),
                Problem::note(Action::Vanished { what: "entry" }, "a", ""),
                Problem::note(Action::SpecialFile, "a", ""),
            ]),
            [
                Some(IssueKind::NameClash),
                Some(IssueKind::Changed),
                None,
                None
            ]
        );
    }

    #[test]
    fn failures_beyond_the_stored_ones_count_towards_the_most_frequent_kind() {
        let full = Problem::io(Action::Copy, "a", &std::io::Error::from_raw_os_error(28));
        let eio = Problem::io(Action::Copy, "b", &std::io::Error::from_raw_os_error(5));
        let list = [full.clone(), full, eio];
        assert_eq!(
            issues(&list, 500),
            [
                Issue {
                    kind: IssueKind::TargetFull,
                    count: 502
                },
                Issue {
                    kind: IssueKind::Other,
                    count: 1
                },
            ]
        );
        assert_eq!(
            issues(&[], 3),
            [Issue {
                kind: IssueKind::Other,
                count: 3
            }]
        );
    }

    #[test]
    fn an_unreadable_source_folder_is_reported_as_an_issue() {
        let t = trees();
        let locked = t.src.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("x"), b"x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let (r, _) = run_mock(&t, Mode::Mirror, false);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(r.outcome, Outcome::Incomplete), "{:?}", r.headline);
        assert_eq!(
            r.issues,
            [Issue {
                kind: IssueKind::NoPermission,
                count: 1
            }]
        );
        assert!(
            r.details[0].starts_with("contents not readable: "),
            "{:?}",
            r.details
        );
        // Deletions are withheld while a folder is unreadable.
        assert!(t.dst.join("stale.txt").exists());
    }
}
