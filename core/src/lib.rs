//! The mirror engine behind dino-copy, shared by the command line tool and the
//! desktop app.
//!
//! - [`walk`]: the streaming two-pass walk (plan, then execute) and the rules
//!   for what may be deleted;
//! - [`plan`]: the entry model and the per-directory comparison;
//! - [`sync`]: the file system operations on the target;
//! - [`filter`]: exclusion patterns;
//! - [`guard`]: checks on the source and target paths before a run.

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!(
    "dino-copy only supports Unix systems (macOS, Linux): it requires Unix permissions and symlinks."
);

pub mod filter;
pub mod guard;
pub mod plan;
pub mod problem;
pub mod sync;
pub mod walk;
#[cfg(test)]
mod walk_props;
