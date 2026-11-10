//! What went wrong during a run, kept structured so that a front end can
//! group and explain it; [`Display`](std::fmt::Display) gives the line the
//! command line tool prints.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// One failure or warning: what was being done, on which path, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub action: Action,
    pub path: PathBuf,
    /// The reason in the system's words ("Permission denied (os error 13)"),
    /// or the engine's own for problems that are not I/O errors.
    pub detail: String,
    /// The kind of I/O error, if the problem is one.
    pub error: Option<io::ErrorKind>,
}

/// What was being done when the problem occurred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Copying a file (pass 2). `path` is relative to the roots.
    Copy,
    /// Creating a directory on the target.
    Mkdir,
    /// Creating a symlink on the target.
    Symlink,
    /// Renaming a target entry to the source's spelling.
    Rename { to: OsString },
    /// Removing a target entry of another type before replacing it.
    RemoveConflict,
    /// Removing a target entry that is no longer in the source.
    Remove,
    /// Setting a file's permissions.
    SetFileMeta,
    /// Setting a directory's time and permissions.
    SetDirMeta,
    /// Flushing a directory to disk.
    FsyncDir,
    /// A target entry changed its type after the plan; it is left alone.
    TypeChanged,
    /// Names that cannot be told apart on the target. `path` is relative.
    Collision,
    /// A directory whose contents could not be listed.
    Unreadable,
    /// An entry that disappeared while it was being read (a warning).
    Vanished { what: &'static str },
    /// A FIFO, socket or device, which is never mirrored (a warning).
    SpecialFile,
    /// Stands in for further problems once the stored list is full.
    Suppressed,
}

impl Problem {
    /// A problem caused by an I/O error.
    pub fn io(action: Action, path: impl Into<PathBuf>, e: &io::Error) -> Self {
        Self {
            action,
            path: path.into(),
            detail: e.to_string(),
            error: Some(e.kind()),
        }
    }

    /// A problem from an error chain; the I/O error in it, if any, gives the kind.
    pub fn chain(action: Action, path: impl Into<PathBuf>, e: &anyhow::Error) -> Self {
        Self {
            action,
            path: path.into(),
            detail: e.to_string(),
            error: e
                .chain()
                .find_map(|c| c.downcast_ref::<io::Error>())
                .map(io::Error::kind),
        }
    }

    /// A problem with a reason of the engine's own.
    pub fn note(action: Action, path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self {
            action,
            path: path.into(),
            detail: detail.into(),
            error: None,
        }
    }

    pub(crate) fn suppressed() -> Self {
        Self::note(Action::Suppressed, Path::new(""), "")
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (path, detail) = (self.path.display(), &self.detail);
        match &self.action {
            Action::Copy => write!(f, "copy {path}: {detail}"),
            Action::Mkdir => write!(f, "mkdir {path}: {detail}"),
            Action::Symlink => write!(f, "symlink {path}: {detail}"),
            Action::Rename { to } => write!(f, "rename {path} to {to:?}: {detail}"),
            Action::RemoveConflict => write!(f, "remove conflict {path}: {detail}"),
            Action::Remove => write!(f, "rm {path}: {detail}"),
            Action::SetFileMeta => write!(f, "set file meta {path}: {detail}"),
            Action::SetDirMeta => write!(f, "set dir meta {path}: {detail}"),
            Action::FsyncDir => write!(f, "fsync dir {path}: {detail}"),
            Action::TypeChanged => {
                write!(f, "type of {path} changed since the plan; left untouched")
            }
            Action::Collision => write!(f, "cannot mirror {path}: {detail}"),
            Action::Unreadable => write!(f, "contents not readable: {path} ({detail})"),
            Action::Vanished { what } => {
                write!(f, "skipping {what} (vanished during the scan): {path}")
            }
            Action::SpecialFile => write!(f, "skipping special file {path}"),
            Action::Suppressed => f.write_str("... further errors suppressed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command line output must not change with the structured type.
    #[test]
    fn lines_read_as_before() {
        let denied = io::Error::from_raw_os_error(13);
        let cases = [
            (
                Problem::io(Action::Copy, "a/b.txt", &denied),
                "copy a/b.txt: Permission denied (os error 13)",
            ),
            (
                Problem::io(Action::Rename { to: "B".into() }, "/t/b", &denied),
                "rename /t/b to \"B\": Permission denied (os error 13)",
            ),
            (
                Problem::note(Action::TypeChanged, "x", ""),
                "type of x changed since the plan; left untouched",
            ),
            (
                Problem::io(Action::Unreadable, "/s/d", &denied),
                "contents not readable: /s/d (Permission denied (os error 13))",
            ),
            (
                Problem::note(Action::Vanished { what: "entry" }, "/s/x", ""),
                "skipping entry (vanished during the scan): /s/x",
            ),
            (Problem::suppressed(), "... further errors suppressed"),
        ];
        for (p, line) in cases {
            assert_eq!(p.to_string(), line);
        }
        assert_eq!(
            Problem::io(Action::Copy, "a", &denied).error,
            Some(io::ErrorKind::PermissionDenied)
        );
    }
}
