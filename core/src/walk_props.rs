//! Property-based tests of the two-pass mirror against a whole-tree model.
//!
//! Random source and target trees (files, directories, symlinks, modes,
//! mtimes, equal-size-but-different contents, type conflicts, exclusions) are
//! written to disk, mirrored with [`walk::plan`] + [`walk::execute`], and the
//! result is compared with what an independent model — working on complete
//! in-memory snapshots, not directory by directory — says it must be.
//!
//! A second property replaces the source with an arbitrary other tree between
//! the two passes (a changed or unmounted source) and checks the core safety
//! promise: nothing is deleted on the target that pass 1 did not plan.

use crate::filter::Filter;
use crate::sync::NullSink;
use crate::walk::{self, ExecOptions, Plan, PlanOptions, Roots};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Tree generation
// ---------------------------------------------------------------------------

/// Few names, so that independent trees still overlap. "e" is the name that
/// the exclude filter targets. "ab" and "a.b" sort differently by bytes than
/// by length, which keeps the merge join honest.
///
/// Generated trees use lowercase names only; [`perturb`] gives target entries
/// an uppercase spelling ([`upper`]). On a case-insensitive file system that
/// is the same entry under another name — the case-rename path — on a
/// case-sensitive one it is just another name.
const NAMES: &[&str] = &["a", "b", "e", "ab", "a.b"];

/// Uppercase spelling of a name, if it gets one. "e" does not: the filter
/// pattern is case-sensitive, so "E" would not be excluded.
fn upper(name: &'static str) -> Option<&'static str> {
    match name {
        "a" => Some("A"),
        "b" => Some("B"),
        "ab" => Some("AB"),
        "a.b" => Some("A.B"),
        _ => None,
    }
}

/// True if the temp dir's file system ignores case.
fn case_insensitive() -> bool {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("probe"), b"").unwrap();
    tmp.path().join("PROBE").exists()
}

/// On a case-insensitive file system, entries are identified by their
/// lowercase path (the generated source names are all lowercase) — up to the
/// first excluded component: below it dino-copy touches nothing, so the
/// target keeps whatever spelling it has.
fn canon_path(p: &Path, ci: bool, exclude: bool) -> PathBuf {
    if !ci {
        return p.to_path_buf();
    }
    let mut out = PathBuf::new();
    let mut inside_excluded = false;
    for c in p.components() {
        let c = c.as_os_str().to_string_lossy();
        out.push(if inside_excluded {
            c.to_string()
        } else {
            c.to_lowercase()
        });
        inside_excluded |= exclude && c == "e";
    }
    out
}

fn canon(snap: Snapshot, ci: bool, exclude: bool) -> Snapshot {
    snap.into_iter()
        .map(|(p, s)| (canon_path(&p, ci, exclude), s))
        .collect()
}
/// "x" and "y" have the same size: without `--checksum` they are
/// indistinguishable when the mtime matches as well.
const DATA: &[&[u8]] = &[b"", b"x", b"y", b"xy"];
/// Far apart (more than the tolerance), so "within tolerance" means "equal".
const MTIMES: &[i64] = &[1_000_000, 1_000_100, 1_000_200];
const FILE_MODES: &[u32] = &[0o644, 0o600, 0o755];
const DIR_MODES: &[u32] = &[0o755, 0o700];
const LINK_TARGETS: &[&str] = &["a", "../b", "/nonexistent"];
const MTIME_TOL: i64 = 2;

#[derive(Debug, Clone)]
enum Node {
    File {
        data: usize,
        mtime: i64,
        mode: u32,
    },
    Link {
        target: &'static str,
        mtime: i64,
    },
    Dir {
        children: BTreeMap<&'static str, Node>,
        mtime: i64,
        mode: u32,
    },
}

type Tree = BTreeMap<&'static str, Node>;

fn node() -> impl Strategy<Value = Node> {
    let mtime = || prop::sample::select(MTIMES);
    let leaf = prop_oneof![
        4 => (0..DATA.len(), mtime(), prop::sample::select(FILE_MODES))
            .prop_map(|(data, mtime, mode)| Node::File { data, mtime, mode }),
        1 => (prop::sample::select(LINK_TARGETS), mtime())
            .prop_map(|(target, mtime)| Node::Link { target, mtime }),
    ];
    leaf.prop_recursive(3, 32, 4, move |inner| {
        (
            prop::collection::btree_map(prop::sample::select(NAMES), inner, 0..4),
            prop::sample::select(MTIMES),
            prop::sample::select(DIR_MODES),
        )
            .prop_map(|(children, mtime, mode)| Node::Dir {
                children,
                mtime,
                mode,
            })
    })
}

fn tree() -> impl Strategy<Value = Tree> {
    prop::collection::btree_map(prop::sample::select(NAMES), node(), 0..5)
}

/// A source tree and a target tree. Mostly the target is an older, drifted
/// copy of the source — like a real backup, where the interesting cases
/// (same path, same size and mtime, other contents) actually occur — and
/// sometimes an unrelated tree.
fn tree_pair() -> impl Strategy<Value = (Tree, Tree)> {
    (tree(), tree(), any::<u64>(), 0..4u8).prop_map(|(src, other, seed, mode)| {
        let dst = if mode == 0 {
            other
        } else {
            perturb(&src, &mut Rng(seed | 1))
        };
        (src, dst)
    })
}

/// Tiny deterministic generator (xorshift) so `perturb` stays a pure function
/// of the seed and proptest can replay failures.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

/// The target as it might look after the source moved on: entries dropped,
/// added, retyped, with other contents (same size and mtime included), other
/// mtimes or other modes.
fn perturb(tree: &Tree, rng: &mut Rng) -> Tree {
    let mut out = Tree::new();
    for (&name, node) in tree {
        let changed = match (rng.below(12), node) {
            (0, _) => continue,
            (1, _) => Node::File {
                data: 1,
                mtime: MTIMES[0],
                mode: 0o644,
            },
            (2, _) => Node::Dir {
                children: Tree::new(),
                mtime: MTIMES[1],
                mode: 0o755,
            },
            // Same size, same mtime, other bytes: only --checksum sees it.
            (3, Node::File { data, mtime, mode }) => Node::File {
                data: match data {
                    1 => 2,
                    2 => 1,
                    d => *d,
                },
                mtime: *mtime,
                mode: *mode,
            },
            (4, Node::File { data, mode, .. }) => Node::File {
                data: *data,
                mtime: rng.pick(MTIMES),
                mode: *mode,
            },
            (5, Node::File { data, mtime, .. }) => Node::File {
                data: *data,
                mtime: *mtime,
                mode: rng.pick(FILE_MODES),
            },
            (6, Node::Link { mtime, .. }) => Node::Link {
                target: rng.pick(LINK_TARGETS),
                mtime: *mtime,
            },
            (7, Node::Dir { children, mode, .. }) => Node::Dir {
                children: perturb(children, rng),
                mtime: rng.pick(MTIMES),
                mode: *mode,
            },
            (
                8,
                Node::Dir {
                    children, mtime, ..
                },
            ) => Node::Dir {
                children: perturb(children, rng),
                mtime: *mtime,
                mode: rng.pick(DIR_MODES),
            },
            (
                _,
                Node::Dir {
                    children,
                    mtime,
                    mode,
                },
            ) => Node::Dir {
                children: perturb(children, rng),
                mtime: *mtime,
                mode: *mode,
            },
            (_, n) => n.clone(),
        };
        // Sometimes the target spells the name in uppercase.
        let key = match upper(name) {
            Some(up) if rng.below(5) == 0 => up,
            _ => name,
        };
        out.insert(key, changed);
    }
    // Entries that only the target has (never a second spelling of an
    // existing name: a case-insensitive file system could not hold both).
    if rng.below(3) == 0 {
        let name = rng.pick(NAMES);
        if !out.keys().any(|k| k.eq_ignore_ascii_case(name)) {
            out.insert(
                name,
                Node::File {
                    data: 3,
                    mtime: MTIMES[2],
                    mode: 0o600,
                },
            );
        }
    }
    out
}

fn write_tree(dir: &Path, tree: &Tree) {
    for (name, node) in tree {
        let p = dir.join(name);
        match node {
            Node::File { data, mtime, mode } => {
                fs::write(&p, DATA[*data]).unwrap();
                fs::set_permissions(&p, fs::Permissions::from_mode(*mode)).unwrap();
                filetime::set_file_mtime(&p, filetime::FileTime::from_unix_time(*mtime, 0))
                    .unwrap();
            }
            Node::Link { target, mtime } => {
                std::os::unix::fs::symlink(target, &p).unwrap();
                let t = filetime::FileTime::from_unix_time(*mtime, 0);
                filetime::set_symlink_file_times(&p, t, t).unwrap();
            }
            Node::Dir {
                children,
                mtime,
                mode,
            } => {
                fs::create_dir(&p).unwrap();
                write_tree(&p, children);
                // After the children (they change the mtime), mtime before mode.
                filetime::set_file_mtime(&p, filetime::FileTime::from_unix_time(*mtime, 0))
                    .unwrap();
                fs::set_permissions(&p, fs::Permissions::from_mode(*mode)).unwrap();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshots and the model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    File(Vec<u8>),
    Link(PathBuf),
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Snap {
    kind: Kind,
    mode: u32,
    mtime: i64,
}

impl Snap {
    fn same_type(&self, o: &Snap) -> bool {
        std::mem::discriminant(&self.kind) == std::mem::discriminant(&o.kind)
    }
}

type Snapshot = BTreeMap<PathBuf, Snap>;

/// Every entry below `root` (root excluded), keyed by relative path.
fn snapshot(root: &Path) -> Snapshot {
    fn rec(root: &Path, rel: &Path, out: &mut Snapshot) {
        let Ok(rd) = fs::read_dir(root.join(rel)) else {
            return;
        };
        for e in rd {
            let rel = rel.join(e.unwrap().file_name());
            let p = root.join(&rel);
            let m = fs::symlink_metadata(&p).unwrap();
            let kind = if m.file_type().is_symlink() {
                Kind::Link(fs::read_link(&p).unwrap())
            } else if m.is_dir() {
                Kind::Dir
            } else {
                Kind::File(fs::read(&p).unwrap())
            };
            let is_dir = kind == Kind::Dir;
            out.insert(
                rel.clone(),
                Snap {
                    kind,
                    mode: m.mode() & 0o7777,
                    mtime: m.mtime(),
                },
            );
            if is_dir {
                rec(root, &rel, out);
            }
        }
    }
    let mut out = Snapshot::new();
    rec(root, Path::new(""), &mut out);
    out
}

/// True if a component of `p` is excluded by the filter under test.
fn excluded(p: &Path, exclude: bool) -> bool {
    exclude && p.components().any(|c| c.as_os_str() == "e")
}

/// The target as it must look after a complete, error-free run.
fn expected(src: &Snapshot, dst: &Snapshot, checksum: bool, exclude: bool) -> Snapshot {
    let mut out = Snapshot::new();
    for (p, s) in src.iter().filter(|(p, _)| !excluded(p, exclude)) {
        let mut e = s.clone();
        // Same size and mtime: only --checksum looks at the contents.
        if let (
            Kind::File(sd),
            Some(Snap {
                kind: Kind::File(dd),
                mtime,
                ..
            }),
        ) = (&s.kind, dst.get(p))
        {
            if !checksum && sd.len() == dd.len() && (s.mtime - mtime).abs() <= MTIME_TOL {
                e.kind = Kind::File(dd.clone());
            }
        }
        out.insert(p.clone(), e);
    }
    // Excluded target entries survive as long as their parent directory does,
    // i.e. every ancestor is a directory on both sides.
    for (p, d) in dst.iter().filter(|(p, _)| excluded(p, exclude)) {
        let parents_survive = p
            .ancestors()
            .skip(1)
            .filter(|a| !a.as_os_str().is_empty())
            .all(|a| {
                excluded(a, exclude)
                    || (matches!(src.get(a), Some(s) if s.kind == Kind::Dir)
                        && matches!(dst.get(a), Some(d) if d.kind == Kind::Dir))
            });
        if parents_survive {
            out.insert(p.clone(), d.clone());
        }
    }
    out
}

/// Number of target entries a run deletes: everything without a same-typed
/// source counterpart, except below a type conflict (that subtree goes with
/// its root, which counts once).
fn expected_deletes(src: &Snapshot, dst: &Snapshot, exclude: bool) -> u64 {
    let conflict =
        |a: &Path| matches!((src.get(a), dst.get(a)), (Some(s), Some(d)) if !s.same_type(d));
    dst.iter()
        .filter(|(p, _)| !excluded(p, exclude))
        .filter(|(p, d)| !matches!(src.get(*p), Some(s) if s.same_type(d)))
        .filter(|(p, _)| !p.ancestors().skip(1).any(conflict))
        .count() as u64
}

// ---------------------------------------------------------------------------
// Running the mirror
// ---------------------------------------------------------------------------

struct Setup {
    _tmp: tempfile::TempDir,
    src: PathBuf,
    dst: PathBuf,
    filter: Filter,
}

fn setup(src: &Tree, dst: &Tree, exclude: bool) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("s");
    let d = tmp.path().join("d");
    fs::create_dir(&s).unwrap();
    fs::create_dir(&d).unwrap();
    write_tree(&s, src);
    write_tree(&d, dst);
    let filter = if exclude {
        Filter::new(&["e".to_string()]).unwrap()
    } else {
        Filter::default()
    };
    Setup {
        _tmp: tmp,
        src: s,
        dst: d,
        filter,
    }
}

impl Setup {
    fn roots(&self) -> Roots<'_> {
        Roots {
            src: &self.src,
            dst: &self.dst,
            filter: &self.filter,
            mtime_tol: MTIME_TOL,
            cancel: None,
        }
    }

    fn plan(&self, checksum: bool) -> Plan {
        let opts = PlanOptions {
            scan_jobs: 2,
            checksum,
            hash_jobs: 2,
            details: false,
        };
        walk::plan(&self.roots(), opts, None)
    }

    fn execute(
        &self,
        plan: &Plan,
        jobs: usize,
    ) -> (walk::ExecReport, Vec<crate::problem::Problem>) {
        let errors = Mutex::new(Vec::new());
        let opts = ExecOptions {
            jobs,
            scan_jobs: 2,
            fsync: false,
            ignore_errors: false,
            delete_budget: plan.stats.remove_count,
        };
        let report = walk::execute(&self.roots(), plan, opts, &NullSink, &errors);
        (report, errors.into_inner().unwrap())
    }
}

/// True if `p` is `r` or lies below it.
fn within(p: &Path, r: &Path) -> bool {
    p.starts_with(r)
}

/// Sink that raises the cancel flag when the `after`-th copy starts.
struct CancelAfter<'a> {
    cancel: &'a std::sync::atomic::AtomicBool,
    after: usize,
    seen: std::sync::atomic::AtomicUsize,
}

impl crate::sync::ProgressSink for CancelAfter<'_> {
    fn add_bytes(&self, _n: u64) {}
    fn set_current(&self, _worker: usize, _name: &str) {
        use std::sync::atomic::Ordering;
        if self.seen.fetch_add(1, Ordering::Relaxed) + 1 >= self.after {
            self.cancel.store(true, Ordering::Relaxed);
        }
    }
    fn set_done(&self, _worker: usize) {}
    fn inc_files(&self) {}
}

/// Temp files of interrupted copies anywhere below `root`.
fn temp_files(root: &Path) -> Vec<PathBuf> {
    snapshot(root)
        .into_keys()
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".dino-copy.tmp."))
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// A complete run turns the target into the model's expectation, counts
    /// its deletions exactly, leaves the source alone and is idempotent.
    #[test]
    fn mirror_matches_the_model(
        (src, dst) in tree_pair(),
        checksum in any::<bool>(),
        exclude in any::<bool>(),
        jobs in 1usize..4,
    ) {
        let t = setup(&src, &dst, exclude);
        let ci = case_insensitive();
        let src_before = snapshot(&t.src);
        // By identity: an uppercase target name aliases the source's entry.
        let dst_before = canon(snapshot(&t.dst), ci, exclude);

        let plan = t.plan(checksum);
        prop_assert_eq!(plan.unreadable, 0);
        prop_assert_eq!(plan.collisions, 0);
        prop_assert_eq!(plan.stats.delete_count(), expected_deletes(&src_before, &dst_before, exclude));

        let (report, errors) = t.execute(&plan, jobs);
        prop_assert_eq!(report.failed, 0, "errors: {:?}", errors);
        prop_assert_eq!(report.skip_reason, None);

        prop_assert_eq!(&snapshot(&t.src), &src_before, "the source must not change");
        let want = expected(&src_before, &dst_before, checksum, exclude);
        prop_assert_eq!(snapshot(&t.dst), want);

        let again = t.plan(checksum);
        prop_assert!(again.stats.is_noop(), "second run not a no-op: {:?}", again.stats);
    }

    /// The source is replaced by an arbitrary other tree between the passes
    /// (changed, partially or fully unmounted). Whatever pass 2 then does, it
    /// must not delete or replace a target entry that pass 1 did not plan.
    #[test]
    fn nothing_unplanned_is_deleted_when_the_source_changes_between_passes(
        (src, dst) in tree_pair(),
        src2 in tree(),
        exclude in any::<bool>(),
        jobs in 1usize..4,
    ) {
        let t = setup(&src, &dst, exclude);
        let plan = t.plan(false);
        let ci = case_insensitive();
        let dst_before = canon(snapshot(&t.dst), ci, exclude);

        fs::remove_dir_all(&t.src).unwrap();
        fs::create_dir(&t.src).unwrap();
        write_tree(&t.src, &src2);
        let _ = t.execute(&plan, jobs);

        // A rename is not a deletion: compare by identity (canonical path).
        let dst_after = canon(snapshot(&t.dst), ci, exclude);
        let allowed: Vec<PathBuf> = plan
            .removals
            .iter()
            .chain(&plan.conflicts)
            .map(|p| canon_path(p, ci, exclude))
            .collect();
        for (p, before) in &dst_before {
            let gone = match dst_after.get(p) {
                None => true,
                Some(after) => !after.same_type(before),
            };
            if gone {
                prop_assert!(
                    allowed.iter().any(|r| within(p, r)),
                    "{} was deleted although pass 1 did not plan it (planned: {:?})",
                    p.display(),
                    allowed
                );
            }
        }
    }

    /// Cancelling at an arbitrary point (after the n-th copy has started)
    /// deletes nothing that was planned for removal and leaves no temp files.
    #[test]
    fn a_cancel_at_any_point_deletes_nothing_and_leaves_no_temp_files(
        (src, dst) in tree_pair(),
        exclude in any::<bool>(),
        after in 0usize..12,
        jobs in 1usize..4,
    ) {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        let t = setup(&src, &dst, exclude);
        let plan = t.plan(false);
        let cancel = AtomicBool::new(after == 0);
        let roots = Roots { cancel: Some(&cancel), ..t.roots() };
        let sink = CancelAfter { cancel: &cancel, after, seen: AtomicUsize::new(0) };
        let errors = Mutex::new(Vec::new());
        let opts = ExecOptions {
            jobs,
            scan_jobs: 2,
            fsync: false,
            ignore_errors: false,
            delete_budget: plan.stats.remove_count,
        };

        let report = walk::execute(&roots, &plan, opts, &sink, &errors);

        prop_assert!(temp_files(&t.dst).is_empty(), "temp files left: {:?}", temp_files(&t.dst));
        if report.cancelled {
            for r in &plan.removals {
                prop_assert!(
                    fs::symlink_metadata(t.dst.join(r)).is_ok(),
                    "{} was removed although the run was cancelled",
                    r.display()
                );
            }
            prop_assert_eq!(report.failed, 0, "a cancel is not a failure: {:?}", errors);
        }
    }
}

/// A writer keeps changing the source while pass 2 runs (files deleted,
/// renamed, created, directories replaced by files). The run may fail or skip
/// work, but it must finish, and every target entry it deleted must have been
/// planned by pass 1.
#[test]
fn source_churn_during_execute_deletes_only_planned_entries() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let tmp = tempfile::tempdir().unwrap();
    let (s, d) = (tmp.path().join("s"), tmp.path().join("d"));
    for dir in 0..20 {
        fs::create_dir_all(s.join(format!("d{dir}"))).unwrap();
        fs::create_dir_all(d.join(format!("d{dir}"))).unwrap();
        for f in 0..100 {
            fs::write(s.join(format!("d{dir}/f{f}")), format!("{dir}/{f}")).unwrap();
            // Every 3rd file exists in the target only: planned removals.
            if f % 3 == 0 {
                fs::write(d.join(format!("d{dir}/old{f}")), b"old").unwrap();
            }
        }
    }
    let filter = Filter::default();
    let roots = Roots {
        src: &s,
        dst: &d,
        filter: &filter,
        mtime_tol: MTIME_TOL,
        cancel: None,
    };
    let plan = walk::plan(
        &roots,
        PlanOptions {
            scan_jobs: 4,
            checksum: false,
            hash_jobs: 2,
            details: false,
        },
        None,
    );
    let dst_before = snapshot(&d);

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
            while !stop.load(Ordering::Relaxed) {
                let dir = s.join(format!("d{}", rng.below(20)));
                let f = dir.join(format!("f{}", rng.below(100)));
                let _ = match rng.below(4) {
                    0 => fs::remove_file(&f),
                    1 => fs::rename(&f, dir.join(format!("old{}", rng.below(100)))),
                    2 => fs::write(&f, b"rewritten"),
                    _ => fs::remove_dir_all(&dir).and_then(|()| fs::write(&dir, b"now a file")),
                };
            }
        });
        let errors = Mutex::new(Vec::new());
        let opts = ExecOptions {
            jobs: 4,
            scan_jobs: 4,
            fsync: false,
            ignore_errors: false,
            delete_budget: plan.stats.remove_count,
        };
        let _ = walk::execute(&roots, &plan, opts, &NullSink, &errors);
        stop.store(true, Ordering::Relaxed);
    });

    let dst_after = snapshot(&d);
    let allowed: Vec<&PathBuf> = plan.removals.iter().chain(&plan.conflicts).collect();
    for (p, before) in &dst_before {
        let gone = dst_after
            .get(p)
            .is_none_or(|after| !after.same_type(before));
        assert!(
            !gone || allowed.iter().any(|r| within(p, r)),
            "{} was deleted although pass 1 did not plan it",
            p.display()
        );
    }
}
