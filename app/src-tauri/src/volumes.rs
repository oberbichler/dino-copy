//! The drive list: mounted volumes the user can pick as source or target.

use serde::Serialize;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    /// Display name, e.g. "Photos-A".
    pub name: String,
    /// Mount point, e.g. "/Volumes/Photos-A".
    pub path: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// External or removable disk (USB, Thunderbolt, SD card).
    pub removable: bool,
}

/// Mounted volumes under `/Volumes`, sorted by name.
///
/// The system volume (`/`) is left out on purpose: every external disk is
/// mounted inside it, so it could never be paired with one (source and
/// target must not be nested). Internal folders are picked via "Choose
/// folder…" instead.
pub fn list() -> Vec<Volume> {
    // Only the space: the I/O statistics of a full refresh are not needed and
    // cost several times as much.
    let refresh = sysinfo::DiskRefreshKind::nothing().with_storage();
    let Ok(disks) = sysinfo::Disks::new_with_refreshed_list_specifics(refresh) else {
        return Vec::new();
    };
    let mut out: Vec<Volume> = disks
        .iter()
        .filter(|d| is_user_volume(d.mount_point()))
        .map(|d| Volume {
            name: d
                .mount_point()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| d.name().to_string_lossy().into_owned()),
            path: d.mount_point().to_string_lossy().into_owned(),
            total_bytes: d.total_space(),
            available_bytes: d.available_space(),
            removable: d.is_removable(),
        })
        .collect();
    out.sort_by_key(|v| v.name.to_lowercase());
    out.dedup_by(|a, b| a.path == b.path);
    out
}

/// Calls `on_change` whenever an entry appears in or disappears from `dir`
/// (`/Volumes`: a disk was mounted or ejected), checking every `every`.
/// Never returns.
///
/// Reading the directory touches none of the disks, so this costs next to
/// nothing and never wakes a sleeping disk.
pub fn watch(dir: &Path, every: Duration, on_change: impl Fn()) -> ! {
    let names = || -> Vec<OsString> {
        let mut names: Vec<OsString> = std::fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        names.sort();
        names
    };
    let mut seen = names();
    loop {
        std::thread::sleep(every);
        let now = names();
        if now != seen {
            seen = now;
            on_change();
        }
    }
}

/// Volumes a user would mirror: directly under `/Volumes`, not the system's
/// own helper mounts (`/System/Volumes/...`, `/private/var/vm`, ...).
fn is_user_volume(mount_point: &Path) -> bool {
    mount_point.parent() == Some(Path::new("/Volumes"))
        && mount_point
            .file_name()
            .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_direct_children_of_volumes_are_offered() {
        assert!(is_user_volume(Path::new("/Volumes/Photos-A")));
        assert!(is_user_volume(Path::new("/Volumes/My Disk")));
        assert!(!is_user_volume(Path::new("/")));
        assert!(!is_user_volume(Path::new("/System/Volumes/Data")));
        assert!(!is_user_volume(Path::new("/Volumes")));
        assert!(!is_user_volume(Path::new("/Volumes/A/nested")));
        assert!(!is_user_volume(Path::new("/Volumes/.timemachine")));
    }

    #[test]
    fn plugging_in_and_ejecting_a_disk_is_noticed() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = tmp.path().to_path_buf();
        std::thread::spawn(move || {
            watch(&dir, Duration::from_millis(20), || {
                let _ = tx.send(());
            })
        });
        let wait = || rx.recv_timeout(Duration::from_secs(2));

        std::thread::sleep(Duration::from_millis(100));
        assert!(rx.try_recv().is_err(), "no change, no notice");
        std::fs::create_dir(tmp.path().join("Photos-A")).unwrap();
        assert!(wait().is_ok(), "plugged in");
        std::fs::remove_dir(tmp.path().join("Photos-A")).unwrap();
        assert!(wait().is_ok(), "ejected");
    }

    #[test]
    fn listing_does_not_panic_and_offers_no_system_volume() {
        let vols = list();
        assert!(
            vols.iter().all(|v| v.path.starts_with("/Volumes/")),
            "{vols:?}"
        );
    }
}
