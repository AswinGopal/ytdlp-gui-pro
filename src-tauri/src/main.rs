#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use ytdownloader::{
    config::{self, QualityMenu},
    history,
    queue::{JobId, JobView, Queue},
    types::JobSpec,
    updater, yt,
};

/// Event name the frontend listens on; the payload is a `QueueEvent`.
const QUEUE_EVENT: &str = "queue_event";

/// Runs blocking lib work off the async runtime and flattens both error
/// layers (join failure, lib failure) into the `String` Tauri sends to JS.
async fn blocking<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> ytdownloader::types::Result<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

// ── Queue ────────────────────────────────────────────────────────────────────

#[tauri::command]
fn cmd_queue_list(queue: State<'_, Queue>) -> Vec<JobView> {
    queue.list()
}

#[tauri::command]
async fn cmd_queue_add(queue: State<'_, Queue>, url: String) -> Result<JobView, String> {
    let q = queue.inner().clone();
    blocking(move || q.add(&url)).await
}

#[tauri::command]
async fn cmd_queue_set_spec(
    queue: State<'_, Queue>,
    id: JobId,
    spec: JobSpec,
) -> Result<JobView, String> {
    let q = queue.inner().clone();
    blocking(move || q.set_spec(id, spec)).await
}

/// Opens a folder picker and, if the user confirms, gives the row its own
/// download folder. Returns `None` when the dialog is cancelled.
#[tauri::command]
async fn cmd_queue_pick_item_dir(
    app: AppHandle,
    queue: State<'_, Queue>,
    id: JobId,
) -> Result<Option<JobView>, String> {
    let q = queue.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(picked) = app.dialog().file().blocking_pick_folder() else {
            return Ok(None);
        };
        let dir = picked
            .as_path()
            .ok_or("Invalid path")?
            .to_string_lossy()
            .into_owned();
        q.set_item_dir(id, Some(dir))
            .map(Some)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Drops the row's own folder so it follows the common download folder again.
#[tauri::command]
async fn cmd_queue_clear_item_dir(queue: State<'_, Queue>, id: JobId) -> Result<JobView, String> {
    let q = queue.inner().clone();
    blocking(move || q.set_item_dir(id, None)).await
}

#[tauri::command]
async fn cmd_queue_start(queue: State<'_, Queue>, id: JobId) -> Result<(), String> {
    let q = queue.inner().clone();
    blocking(move || q.start(id)).await
}

#[tauri::command]
async fn cmd_queue_pause(queue: State<'_, Queue>, id: JobId) -> Result<(), String> {
    let q = queue.inner().clone();
    blocking(move || q.pause(id)).await
}

#[tauri::command]
async fn cmd_queue_stop(queue: State<'_, Queue>, id: JobId) -> Result<(), String> {
    let q = queue.inner().clone();
    blocking(move || q.stop(id)).await
}

#[tauri::command]
async fn cmd_queue_remove(queue: State<'_, Queue>, ids: Vec<JobId>) -> Result<(), String> {
    let q = queue.inner().clone();
    blocking(move || q.remove(&ids)).await
}

#[tauri::command]
async fn cmd_redownload(
    queue: State<'_, Queue>,
    urls: Vec<String>,
) -> Result<Vec<JobView>, String> {
    let q = queue.inner().clone();
    blocking(move || q.redownload(&urls)).await
}

// ── History ──────────────────────────────────────────────────────────────────

#[tauri::command]
fn cmd_get_history() -> Vec<history::HistoryEntry> {
    history::read_history()
}

#[tauri::command]
fn cmd_delete_history_entry(url: String) {
    history::delete_entry(&url);
}

#[tauri::command]
fn cmd_clear_history() {
    history::clear_history();
}

// ── Settings ─────────────────────────────────────────────────────────────────

#[tauri::command]
fn cmd_get_quality_menu() -> QualityMenu {
    config::get_quality_menu()
}

/// Validates and persists the menu; returns the normalised version so the UI
/// can show exactly what was stored.
#[tauri::command]
fn cmd_set_quality_menu(menu: QualityMenu) -> Result<QualityMenu, String> {
    config::set_quality_menu(menu).map_err(|e| e.to_string())
}

/// Returns the common download directory path as a string (for display).
#[tauri::command]
fn cmd_get_download_dir() -> String {
    config::get_download_dir_display()
}

/// Opens a native folder-picker dialog. If the user confirms a selection,
/// saves it and returns the new path. Returns None if cancelled.
#[tauri::command]
async fn cmd_pick_download_dir(app: AppHandle) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let Some(picked) = app.dialog().file().blocking_pick_folder() else {
            return Ok(None);
        };
        let pb = picked
            .as_path()
            .map(|p| p.to_path_buf())
            .ok_or("Invalid path")?;
        config::set_download_dir(&pb).map_err(|e| e.to_string())?;
        Ok(Some(pb.to_string_lossy().to_string()))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Clears any stored override and falls back to system Downloads.
#[tauri::command]
fn cmd_reset_download_dir() -> Result<String, String> {
    config::reset_download_dir().map_err(|e| e.to_string())?;
    Ok(config::get_download_dir_display())
}

#[tauri::command]
async fn cmd_update_ytdlp() -> Result<String, String> {
    blocking(updater::check_and_update).await
}

#[tauri::command]
async fn cmd_check_deno() -> bool {
    tauri::async_runtime::spawn_blocking(yt::deno_in_path)
        .await
        .unwrap_or(false)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let queue = Queue::open(Arc::new(move |event| {
                let _ = handle.emit(QUEUE_EVENT, event);
            }))?;
            app.manage(queue);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            cmd_queue_list,
            cmd_queue_add,
            cmd_queue_set_spec,
            cmd_queue_pick_item_dir,
            cmd_queue_clear_item_dir,
            cmd_queue_start,
            cmd_queue_pause,
            cmd_queue_stop,
            cmd_queue_remove,
            cmd_redownload,
            cmd_get_history,
            cmd_delete_history_entry,
            cmd_clear_history,
            cmd_get_quality_menu,
            cmd_set_quality_menu,
            cmd_get_download_dir,
            cmd_pick_download_dir,
            cmd_reset_download_dir,
            cmd_update_ytdlp,
            cmd_check_deno,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Kill every yt-dlp process tree before the app goes away, or
            // downloads would keep running with no window to show them.
            if let tauri::RunEvent::Exit = event {
                app.state::<Queue>().shutdown();
            }
        });
}
