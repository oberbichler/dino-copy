//! Desktop app for dino-copy: pick two disks (or folders), press Start.
//!
//! The window is a thin layer over `dino-copy-core`; every rule about what is
//! copied and what may be deleted lives there. This crate adds:
//!
//! - [`volumes`]: the drive list for the pickers;
//! - [`run`]: one mirror or scan at a time on a background thread, reporting
//!   phases, progress, warnings and the result as events to the window.

mod run;
mod volumes;

use run::{Mode, RunState};
use tauri::Manager;

/// Volumes for the source/target pickers, read fresh on every call.
#[tauri::command]
async fn list_volumes() -> Vec<volumes::Volume> {
    // Statting a sleeping or dead disk can take a while: keep it off the
    // main thread so the window stays responsive.
    tauri::async_runtime::spawn_blocking(volumes::list)
        .await
        .unwrap_or_default()
}

/// Why `source` → `target` cannot be started, or `None` if it can.
#[tauri::command]
fn check_pair(source: String, target: String) -> Option<run::Msg> {
    run::check_pair(source.as_ref(), target.as_ref()).err()
}

/// Starts a mirror or scan. Progress arrives as `run://…` events.
#[tauri::command]
fn start(
    app: tauri::AppHandle,
    state: tauri::State<'_, RunState>,
    mode: Mode,
    source: String,
    target: String,
) -> Result<(), run::Msg> {
    run::start(app, &state, mode, source.into(), target.into())
}

/// The user's preferred languages, most preferred first ("de-AT", "en-US").
#[tauri::command]
fn locales() -> Vec<String> {
    sys_locale::get_locales().collect()
}

/// Asks the running job to stop. Has no effect once deletions have begun.
#[tauri::command]
fn cancel(state: tauri::State<'_, RunState>) {
    state.cancel();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            app.manage(RunState::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_volumes,
            check_pair,
            start,
            cancel,
            locales
        ])
        .run(tauri::generate_context!())
        .expect("error while running dino-copy");
}
