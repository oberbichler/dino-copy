# Changelog

## 0.3.1

### DinoCopy

- The progress bar, the time left and the speed stay steady while copying.
  They are measured over the last 20 seconds; large files no longer throw
  the estimate around, and the bar never moves back.
- The disk chosen on one side is greyed out in the other side's list, so
  that source and target cannot be the same disk.
- Disks plugged in or ejected while the window is open show up right away.

### Command line tool

- No change.

## 0.3.0

### New: DinoCopy, the desktop app for macOS

- Pick the source and target disks from a list (or any folder), press
  **Mirror**, and watch the progress and the time left. **Scan** shows every
  change a mirror would make, as a tree or a list, without changing anything.
- Same engine and safety rules as the command line tool; the folders macOS
  keeps on every volume are left out.
- Problems are explained by cause, with the next step; the full error lines
  are under Details. A remembered disk that is not connected says so.
- English, German, French, Spanish, Italian, Dutch, Polish and Portuguese.
- Install with `brew install --cask oberbichler/tap/dinocopy` or from
  `DinoCopy.dmg` (universal; not notarized, see the README).

### Command line tool

- No change in behaviour or output. The engine now lives in its own library
  (`dino-copy-core`), shared with the app.
