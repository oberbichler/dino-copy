//! Checks on the source and target paths before anything is touched.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

/// Resolves `path` as far as it exists: the deepest existing ancestor is
/// canonicalized, and the not-yet-existing remainder is appended lexically.
///
/// Necessary because `--dry-run` must not create the target, yet
/// [`check_disjoint`] still needs resolved paths — otherwise `..`, a symlink or
/// a relative path could not be recognized as nesting.
///
/// Limitation: `..` inside the not-yet-existing part is not resolved. That is
/// harmless, because by definition that part is not a directory one could
/// descend into.
pub fn resolve_existing(path: &std::path::Path) -> Result<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&cur) {
            Ok(mut base) => {
                for name in tail.iter().rev() {
                    base.push(name);
                }
                return Ok(base);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let name = cur
                    .file_name()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Cannot resolve path: {}", path.display())
                    })?
                    .to_os_string();
                tail.push(name);
                cur.pop();
                // Relative path without a parent: resolve against the working
                // directory.
                if cur.as_os_str().is_empty() {
                    cur = PathBuf::from(".");
                }
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Cannot resolve path: {}", path.display()))
            }
        }
    }
}

/// Makes sure that source and target are neither identical nor nested inside
/// one another. The target may still be missing (dry run).
pub fn check_disjoint(source: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let src = resolve_existing(source)
        .with_context(|| format!("Cannot resolve source: {}", source.display()))?;
    let dst = resolve_existing(dest)
        .with_context(|| format!("Cannot resolve target: {}", dest.display()))?;
    if src == dst {
        bail!("Source and target are identical: {}", src.display());
    }
    if dst.starts_with(&src) {
        bail!(
            "Target lies inside the source: {} contains {}",
            src.display(),
            dst.display()
        );
    }
    if src.starts_with(&dst) {
        bail!(
            "Source lies inside the target: {} contains {} (would delete the source!)",
            dst.display(),
            src.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn resolve_existing_matches_canonicalize_for_existing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("there");
        std::fs::create_dir(&dir).unwrap();

        assert_eq!(resolve_existing(&dir).unwrap(), std::fs::canonicalize(&dir).unwrap());
    }

    #[test]
    fn resolve_existing_appends_the_missing_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();

        let got = resolve_existing(&tmp.path().join("missing")).unwrap();

        assert_eq!(got, base.join("missing"));
    }

    #[test]
    fn resolve_existing_handles_several_missing_components() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();

        let got = resolve_existing(&tmp.path().join("a/b/c")).unwrap();

        assert_eq!(got, base.join("a").join("b").join("c"));
    }

    #[test]
    fn resolve_existing_resolves_a_bare_relative_name_against_the_cwd() {
        // Without the special handling of the empty parent, the loop would spin
        // forever here: cur.pop() yields "" and canonicalize("") is NotFound
        // again.
        let got = resolve_existing(Path::new("doesnotexistxyz")).unwrap();

        assert_eq!(got, std::fs::canonicalize(".").unwrap().join("doesnotexistxyz"));
    }

    #[test]
    fn resolve_existing_follows_symlinks_in_the_existing_part() {
        // The nesting guard depends on this: a symlink must not be able to
        // bypass the detection.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let got = resolve_existing(&link.join("missing")).unwrap();

        assert_eq!(got, std::fs::canonicalize(&real).unwrap().join("missing"));
    }
}

