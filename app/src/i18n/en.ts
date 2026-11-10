// English: the reference language. Its keys are the keys of all others.

export const en = {
  // Setup
  source: "Source",
  target: "Target",
  choose: "Choose…",
  diskOrFolder: "a disk or a folder",
  mirror: "Mirror",
  scan: "Scan",
  modeMenu: "Choose what Start does",
  freeOf: "{free} free of {total}",
  used: "{used} used",
  noDisks: "No external disks found",
  chooseFolder: "Choose Folder…",
  chooseSourceFolder: "Choose the source folder",
  chooseTargetFolder: "Choose the target folder",
  hintChoose: "Choose a source and a target.",
  hintMirror: "The target will become an exact copy of the source.",
  hintScan: "Shows what a mirror would do. Nothing is changed.",

  // Running
  mirroring: "Mirroring",
  scanning: "Scanning",
  comparing: "Comparing…",
  phaseScanning: "Looking for what changed since the last mirror",
  estimating: "Estimating time left…",
  phaseCopying: "Copying new and changed files",
  almostDone: "Almost done",
  phaseDeleting: "Removing what is no longer in the source",
  finishing: "Finishing…",
  cancelling: "Cancelling…",
  copied: "Copied",
  files: "Files",
  speed: "Speed",
  compared: "{n} compared",
  xOfY: "{done} of {total}",
  etaUnderMinute: "Less than a minute left",
  etaMinutes: { one: "About {n} minute left", other: "About {n} minutes left" },
  etaHours: "About {h} h left",
  etaHoursMinutes: "About {h} h {m} min left",
  cancel: "Cancel",

  // Done
  done: "Done",
  took: "Took {duration}.",
  cancelledAfter: "Cancelled after {duration}.",
  filesCopied: { one: "{n} file copied ({bytes})", other: "{n} files copied ({bytes})" },
  deleted: "{n} deleted",
  renamed: "{n} renamed",

  // Scan
  catCopy: "Copy",
  catDelete: "Delete",
  catRename: "Rename",
  search: "Search",
  tree: "Tree",
  list: "List",
  close: "Close",
  mirrorNow: "Mirror Now",
  scanWould: "A mirror would copy {files} ({bytes}), delete {deleted} and rename {renamed}.",
  fileCount: { one: "{n} file", other: "{n} files" },
  unchanged: { one: "{n} file unchanged.", other: "{n} files unchanged." },
  changes: { one: "{n} change", other: "{n} changes" },
  shownOf: "{shown} of {total}",
  noChanges: "No changes.",
  nothingMatches: "Nothing matches.",
  newFolder: "new folder",
  replacesFile: "replaces a file",
  renamedFrom: "renamed from {name}",
  wholeFolder: "whole folder, {entries}",
  entries: { one: "{n} entry", other: "{n} entries" },
  replacedOther: "replaced (other type on the target)",

  // Units
  bytes: { one: "{n} byte", other: "{n} bytes" },
  units: "KB MB GB TB",

  // Messages from the backend
  sourceMissing: "The source is not available: {path}",
  targetMissing: "The target is not available: {path}",
  unresolvable: "Cannot access {path}",
  identical: "Source and target are the same.",
  targetInsideSource: "The target lies inside the source.",
  sourceInsideTarget: "The source lies inside the target.",
  runInProgress: "A run is already in progress.",
  cancelledUnchanged: "Cancelled. Nothing was changed.",
  sourceEmpty: {
    one: "The source is empty, but the target holds {n} entry. Nothing was changed: this looks like a disk that is not mounted.",
    other: "The source is empty, but the target holds {n} entries. Nothing was changed: this looks like a disk that is not mounted.",
  },
  scanNothingToDo: "Nothing to do: the target already mirrors the source.",
  scanFinished: "Scan finished. Nothing was changed.",
  inSync: "Already in sync. Nothing to do.",
  cancelledPartly: "Cancelled. Files copied so far are complete; nothing was deleted.",
  finishedWithErrors: {
    one: "Finished with {n} error. The target is not a complete mirror.",
    other: "Finished with {n} errors. The target is not a complete mirror.",
  },
  finishedIncomplete: "Finished, but the target is not a complete mirror.",
  mirrored: "{target} now mirrors {source}.",
  deletionsWillBeSkipped: {
    one: "{n} deletion will be skipped: {reason}.",
    other: "{n} deletions will be skipped: {reason}.",
  },
  reasonUnreadable: { one: "{n} folder could not be read", other: "{n} folders could not be read" },
  reasonCollisions: {
    one: "{n} name cannot be told apart on the target",
    other: "{n} names cannot be told apart on the target",
  },
  rootMetaFailed: "Target folder metadata not applied: {error}",
  skippedAfterFailures: {
    one: "{n} deletion skipped because other actions failed.",
    other: "{n} deletions skipped because other actions failed.",
  },
  skippedUnreadable: {
    one: "{n} deletion skipped because folders could not be read.",
    other: "{n} deletions skipped because folders could not be read.",
  },
  skippedChanged: {
    one: "{n} deletion skipped: the source or target changed during the run. Run again.",
    other: "{n} deletions skipped: the source or target changed during the run. Run again.",
  },
};

type Plural = { one?: string; two?: string; few?: string; many?: string; other: string };
/** Any language may use a plural where English does not, and vice versa. */
export type Messages = Record<keyof typeof en, string | Plural>;
