# dino-copy

A fast one-way mirror for local disks. dino-copy makes the target directory an
exact copy of the source: it copies new and changed files, skips unchanged
ones, deletes what no longer exists in the source, and preserves mtimes,
permissions and symlinks. The source is only ever read.

It is built for backups between external disks (USB HDDs and SSDs), where two
things matter most: **the backup is complete**, and **nothing is deleted that
should not be**.

## Installation

Homebrew (macOS, Linux):

```sh
brew install oberbichler/tap/dino-copy
```

Prebuilt binaries and a shell installer are attached to every
[GitHub release](https://github.com/oberbichler/dino-copy/releases). From
source (Rust 1.85 or newer):

```sh
cargo install --git https://github.com/oberbichler/dino-copy
```

dino-copy runs on macOS and Linux.

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
| `--jobs N` | Parallel copies (default: one per CPU on SSDs, otherwise 2; the disk type is only detected on macOS). |

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

## Development

```sh
cargo test      # unit, integration and property-based tests
cargo clippy --all-targets
```

The core guarantees are checked by property-based tests (`src/walk_props.rs`):
random source and target trees are mirrored and compared against an
independent model, and the source is swapped for an arbitrary other tree
between the two passes to check that nothing unplanned is ever deleted. Raise
`PROPTEST_CASES` for a deeper search.

## License

ISC
