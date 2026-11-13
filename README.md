<p align="center">
  <img src="app/src-tauri/icons/128x128@2x.png" width="128" alt="">
</p>

<h1 align="center">dino-copy</h1>

<p align="center">
  <a href="https://github.com/oberbichler/dino-copy/actions/workflows/ci.yml"><img src="https://github.com/oberbichler/dino-copy/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/oberbichler/dino-copy/releases/latest"><img src="https://img.shields.io/github/v/release/oberbichler/dino-copy" alt="Release"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey" alt="macOS | Linux">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-ISC-blue" alt="ISC license"></a>
</p>

A fast one-way mirror for local disks, as a command line tool and as
**DinoCopy**, a desktop app for macOS. The target becomes an exact copy of the
source: new and changed files are copied, unchanged ones skipped, and what no
longer exists in the source is deleted. The source is only ever read.

Built for backups between external disks: files are never half-written, and
nothing is deleted that you did not confirm or that might be the only copy
([how it works](docs/how-it-works.md)).

## Install

```sh
brew install oberbichler/tap/dino-copy        # command line tool (macOS, Linux)

brew trust --cask oberbichler/tap/dinocopy    # once
brew install --cask oberbichler/tap/dinocopy  # desktop app (macOS 11+)
```

Or download a binary or `DinoCopy.dmg` from the
[latest release](https://github.com/oberbichler/dino-copy/releases/latest).
The app is not notarized: on first start, choose **Open Anyway** in System
Settings → Privacy & Security.

## Use

**App:** pick source and target, press **Mirror**. The arrow next to it
switches to **Scan**, which shows what would change without touching anything.

**Command line:**

```sh
dino-copy /Volumes/Main /Volumes/Backup            # shows the plan, asks before deleting
dino-copy /Volumes/Main /Volumes/Backup --dry-run  # only show the plan
```

| Option | Effect |
| --- | --- |
| `--yes` | Do not ask (for scripts). |
| `--exclude PATTERN` | Ignore matching entries, e.g. `.DS_Store` or `photos/*.xmp`. |
| `--max-delete N` | Abort if more than `N` entries would be deleted. |
| `--checksum` | Also compare contents (BLAKE3). |
| `--jobs N` | Parallel copies (default 2; more helps on SSDs). |

`dino-copy --help` lists all options. Exit status `0` means the target is an
exact mirror.

## License

ISC
