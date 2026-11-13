# dino-copy

A fast one-way mirror for local disks. dino-copy makes the target directory an
exact copy of the source: it copies new and changed files, skips unchanged
ones, deletes what no longer exists in the source, and preserves mtimes,
permissions and symlinks. The source is only ever read.

It is built for backups between external disks (USB HDDs and SSDs), where two
things matter most: **the backup is complete**, and **nothing is deleted that
should not be**.

dino-copy comes as a command line tool for macOS and Linux, and as
**DinoCopy**, a desktop app for macOS (see [Desktop app](#desktop-app)). Both
use the same engine and are released together.

## Installation

Command line tool, with Homebrew (macOS, Linux):

```sh
brew install oberbichler/tap/dino-copy
```

Prebuilt binaries and a shell installer are attached to every
[GitHub release](https://github.com/oberbichler/dino-copy/releases). From
source (Rust 1.85 or newer):

```sh
cargo install --git https://github.com/oberbichler/dino-copy
```

Desktop app (macOS 11 or newer, Apple Silicon and Intel):

```sh
brew trust --cask oberbichler/tap/dinocopy   # once: Homebrew asks before loading casks from other taps
brew install --cask oberbichler/tap/dinocopy
```

or download `DinoCopy.dmg` from the
[latest release](https://github.com/oberbichler/dino-copy/releases/latest).
The app is not notarized by Apple, so macOS refuses the first start of a
downloaded copy: open it once, then choose **Open Anyway** in System Settings
→ Privacy & Security. (Homebrew installs it without that step.)

## Usage

```sh
dino-copy /Volumes/Main /Volumes/Backup
```

dino-copy scans both sides, prints a plan and asks for confirmation before it
deletes anything:

```text
Plan: copy 1204 (3812.55 MB), delete 17, 3 directories, 0 file metadata, 482113 skipped.
17 files/folders will be DELETED in the target. Continue? [y/N]
```

Useful options:

| Option | Effect |
| --- | --- |
| `--dry-run` | Show the plan, change nothing. |
| `--yes` | Do not ask (for cron jobs and scripts). |
| `--max-delete N` | Abort before changing anything if more than `N` entries would be deleted. |
| `--exclude PATTERN` | Ignore matching entries on both sides (repeatable). Without `/` the pattern matches a name at any depth (`.DS_Store`, `*.tmp`); with `/` it matches the relative path (`photos/*.xmp`). |
| `--checksum` | Also compare the contents (BLAKE3) of files whose size and mtime match. Reads everything on both sides. |
| `--fsync` | Flush every file and directory to the disk before moving on. Slower, safer on power loss. |
| `--mtime-window S` | Treat mtimes up to `S` seconds apart as equal (default 2, for FAT32/exFAT). |
| `--jobs N` | Parallel copies and checksum reads (default 2). Keep 2 for hard disks; on SSDs, `--jobs 8` speeds up medium-sized files and `--checksum` (see [Performance](#performance)). |

Mirroring two macOS volume roots typically wants:

```sh
dino-copy /Volumes/Main /Volumes/Backup \
  --exclude .Spotlight-V100 --exclude .fseventsd --exclude .Trashes \
  --exclude .DocumentRevisions-V100 --exclude .TemporaryItems
```

`dino-copy --help` lists all options.

## What is compared

A file counts as unchanged when its size and mtime (within `--mtime-window`)
match. Then only differing permissions are fixed. With `--checksum`, such
files are additionally compared byte for byte via BLAKE3. Directories and
symlinks are compared by type, mtime and permissions (and, for symlinks, the
link target). Symlinks are copied as links, never followed. FIFOs, sockets and
device files are skipped with a warning.

## Safety

Every write goes to the target. Files are copied into a temporary file next to
their destination and renamed into place once complete, so a file on the
target is never half-written under its real name — not even after Ctrl-C or a
crash. Ctrl-C removes the temporary files; after a crash, the next run deletes
the leftovers like any other extra entry.

Deleting is the only irreversible step, so it is fenced in on all sides. An
entry on the target is only deleted if:

- **it was in the plan you confirmed.** The run walks the trees twice: once to
  plan, once to act. Anything that would be deleted in the second walk but was
  not planned in the first — because the source changed or got unmounted in
  between — is left alone, and the run fails so you notice.
- **nothing failed.** If a copy fails, extra entries on the target might be the
  only copy of that data, so no deletion happens (`--ignore-errors` overrides).
- **every directory could be read.** The contents of an unreadable directory
  are unknown, not gone (`--ignore-errors` overrides).
- **it is really extra.** macOS volumes treat `Foo` and `foo`, or `é` in its two
  Unicode forms, as the same name. dino-copy recognizes such pairs and never
  deletes one name of a file under the other.

Before anything is touched, dino-copy also refuses to run when source and
target are the same or nested in each other, and when the source is empty but
the target is not — the classic sign of an unmounted disk
(`--allow-empty-source` overrides).

Excluded entries on the target are never overwritten or deleted, with one
exception: when a whole directory is removed from the target because it no
longer exists in the source (or a file of the same name replaces it), its
excluded contents (such as `.DS_Store`) go with it.

On case-insensitive targets (the macOS default), an entry renamed only in case
in the source is renamed on the target as well. A difference only in Unicode
normalization (NFC vs. NFD, which look identical) is left as is. If the source
holds two names that differ only in case — possible on a case-sensitive
source — the target can store just one of them: dino-copy mirrors one, reports
the other and withholds all deletions until you resolve it (rename one, use
`--exclude`, or a case-sensitive target volume).

### Exit status

`0` means the target is now an exact mirror. Anything else means it is not:
an action failed, a directory could not be read, a name could not be mirrored,
or deletions were withheld. The reason is printed, and the next run picks up
where this one stopped.

## How it works

dino-copy walks source and target side by side, one directory at a time: it
lists both versions of a directory, compares them by name in a single merge
pass, acts on the differences and moves on to the subdirectories, in parallel.
Copies stream through a bounded queue to a pool of copy workers, so scanning
and copying overlap. A directory's mtime and permissions are applied as soon as
everything inside it is done.

Because nothing is collected up front, memory does not grow with the size of
the source: in a benchmark with 300,000 files it stayed at about 15 MB (the
previous whole-tree design needed 230 MB, growing linearly). It grows only
with the amount of *change* — the list of planned deletions and `--checksum`
mismatches.

## Performance

Measured against `rsync -a --delete` on an M1 Pro (internal SSD, source and
target on the same disk, warm cache, dino-copy with its defaults):

| Workload | Run | dino-copy | rsync 3.4.4 | openrsync (macOS) |
| --- | --- | --- | --- | --- |
| 300,000 empty files | first | **39.6 s** | 57.9 s | 71.7 s |
| | nothing changed | **2.0 s** | 8.0 s | 28.1 s |
| | 1 % changed | **4.6 s** | 8.9 s | 28.8 s |
| 20,000 × 64 KB (1.3 GB) | first | **5.5 s** | 6.4 s | 8.9 s |
| | nothing changed | **0.07 s** | 0.15 s | 1.52 s |
| 8 × 512 MB (4 GB) | first | **1.7 s** | 5.2 s | 15.5 s |
| 1.3 GB, `--checksum` / `-c` | nothing changed | 1.56 s | **0.78 s** | 9.48 s |

With `--jobs 10` on the same SSD, the 1.3 GB copy takes 4.1 s and the
checksum run 0.72 s, while the 300,000 tiny files get slower (53 s): parallel
creation in one directory contends. On USB hard disks the disk dominates the
copy times, and the differences shrink; the faster scan remains.

## Desktop app

DinoCopy does the same job in a window: pick the source and target disks
from a list (or any folder), press **Mirror**, and watch the progress and the
time left. The arrow next to the button switches to **Scan**, which shows
every change a mirror would make as a tree or a list, with filters and a
search, without changing anything. The app uses the same engine and the same
safety rules as the command line tool, and it leaves out the folders macOS
keeps on every volume (`.Spotlight-V100`, `.fseventsd`, `.Trashes`, …). It
keeps the Mac awake during a run and notifies when it is done.

When something cannot be mirrored, the result says what happened and what to
do, grouped by cause (the target is full, a file is too large for the target's
format, no access, …); the full error lines are under **Details**. A
remembered disk that is not connected says so, and the window notices disks
being plugged in or ejected while it is open. The disk chosen on one side is
greyed out in the other side's list, so that source and target cannot be the
same disk.

The window follows the system language: English, German, French, Spanish,
Italian, Dutch, Polish and Portuguese.

## Development

| Path | What |
| --- | --- |
| `core/` | `dino-copy-core`, the engine: walk, plan, copy, delete. |
| `src/`, `tests/` | The command line tool and its integration tests. |
| `app/` | DinoCopy: the window (`index.html`, `src/`, TypeScript with Vite) and the Tauri backend (`src-tauri/`, its own Cargo workspace and lockfile). |
| `app/src/i18n/` | Translations. English defines the keys; the type check fails if a language misses one. |
| `app/tests/` | UI tests of the window in every state, language and colour scheme. |
| `app/icon/` | The icon's source (`icon.svg`, rendered by `render.mjs`). |
| `app/packaging/` | The Homebrew cask, filled in by the release. |

Command line tool and engine (Rust 1.85 or newer):

```sh
cargo test --workspace   # unit, integration and property-based tests
cargo clippy --workspace --all-targets
```

The app (Node.js 22 and Rust 1.88 or newer):

```sh
cd app
npm install
npm run tauri dev        # the app, reloading on changes
npm run typecheck
npm run test:ui          # UI tests in WebKit (once: npx playwright install webkit)
cd src-tauri && cargo test
```

The UI tests replace the backend with a stand-in (`app/tests/tauri.ts`) and
check each state's behaviour and that no text is cut off. With
`SCREENSHOTS=<dir>` they also save a screenshot of every state.

The core guarantees are checked by property-based tests (`core/src/walk_props.rs`):
random source and target trees are mirrored and compared against an
independent model, and the source is swapped for an arbitrary other tree
between the two passes to check that nothing unplanned is ever deleted. Raise
`PROPTEST_CASES` for a deeper search.

### Releasing

The command line tool and the app share one version (`Cargo.toml`,
`core/Cargo.toml`, `app/src-tauri/Cargo.toml`, `app/package.json`; a test
checks that they match). Pushing a tag `vX.Y.Z` runs `release.yml`:
[dist](https://github.com/axodotdev/cargo-dist) builds the command line tool
and updates its formula in
[oberbichler/homebrew-tap](https://github.com/oberbichler/homebrew-tap);
`build-app.yml` builds `DinoCopy.dmg` (universal, signed ad hoc) for the same
GitHub release, and `publish-app.yml` updates the `dinocopy` cask. `release.yml`
is generated: change `dist-workspace.toml` and run `dist generate`.

## License

ISC
