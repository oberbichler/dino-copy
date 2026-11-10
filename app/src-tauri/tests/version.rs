//! The app and the command line tool are released together, under one tag:
//! they must carry the same version. The release workflow checks the tag
//! against it; this catches a bump that missed a file.

/// The `version = "…"` of a manifest's `[package]` table.
fn package_version(manifest: &str) -> &str {
    let package = manifest
        .split_once("[package]")
        .expect("a [package] table")
        .1;
    package
        .lines()
        .find_map(|l| l.strip_prefix("version = \""))
        .and_then(|v| v.strip_suffix('"'))
        .expect("a version in [package]")
}

#[test]
fn app_command_line_tool_and_engine_share_one_version() {
    let app = env!("CARGO_PKG_VERSION");
    let cli = package_version(include_str!("../../../Cargo.toml"));
    let core = package_version(include_str!("../../../core/Cargo.toml"));
    assert_eq!(app, cli, "app/src-tauri/Cargo.toml and Cargo.toml");
    assert_eq!(app, core, "app/src-tauri/Cargo.toml and core/Cargo.toml");

    let npm = include_str!("../../package.json");
    assert!(
        npm.contains(&format!("\"version\": \"{app}\"")),
        "app/package.json must have version {app}"
    );
}
