// VideoGrab — YouTube downloader powered by yt-dlp
// Tauri v2 backend: download queue, bundled yt-dlp management, app & yt-dlp updates.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{Emitter, Manager};

mod downloader;
mod yt_dlp;

/// The file where the download history is persisted between sessions.
fn history_path(app: &tauri::AppHandle) -> PathBuf {
    data_dir(app).join("history.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadItem {
    pub id: String,
    pub url: String,
    pub mode: String,      // "video" | "audio"
    pub quality: String,   // "best" | "1080" | "720" | "480" | "mp3" | "m4a"
    pub save_dir: String,
    pub status: String,    // pending | running | done | error
    pub stage: String,     // info | downloading | merging
    pub media_id: Option<String>,
    pub title: Option<String>,
    pub progress: f64,     // 0..100
    pub speed: Option<String>,
    pub error: Option<String>,
}

/// Persist the current queue (download history) to disk so it survives
/// restarts. Entries whose finished file no longer exists on disk are
/// dropped during loading, so manually deleted files disappear from the
/// queue automatically.
pub async fn save_history(app: &tauri::AppHandle, items: &[DownloadItem]) -> Result<(), String> {
    let dir = data_dir(app);
    let _ = tokio::fs::create_dir_all(&dir).await;
    let content = serde_json::to_string_pretty(items).map_err(|e| e.to_string())?;
    tokio::fs::write(history_path(app), content).await.map_err(|e| e.to_string())
}

/// Restore the download history from disk; keep only entries whose final
/// file still exists (or pending/running items, which are discarded — a
/// running download cannot survive a restart anyway).
pub async fn load_history(
    app: &tauri::AppHandle,
) -> Result<Vec<DownloadItem>, String> {
    let path = history_path(app);
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(_) => return Ok(Vec::new()),
    };
    let items: Vec<DownloadItem> =
        serde_json::from_str(&content).unwrap_or_default();
    let mut kept = Vec::with_capacity(items.len());
    for mut item in items.clone() {
        if item.status == "done" {
            if let Some(id) = &item.media_id {
                // The output template is "{title} [{id}].{ext}" — matching
                // the bracketed id inside the file name covers the final
                // mp4 regardless of what characters the title carries.
                let pattern = format!("[{id}]");
                let found = match tokio::fs::read_dir(&item.save_dir).await {
                    Ok(mut rd) => {
                        let mut found = false;
                        while let Ok(Some(entry)) = rd.next_entry().await {
                            if entry.file_type().await.map(|t| t.is_file()).unwrap_or(false)
                                && entry
                                    .file_name()
                                    .to_string_lossy()
                                    .contains(&pattern)
                            {
                                found = true;
                                break;
                            }
                        }
                        found
                    }
                    Err(_) => false,
                };
                if found {
                    kept.push(item);
                } else {
                    // File was deleted manually from the device — drop it.
                    let _ = cleanup_media(app, id).await;
                }
            }
        } else {
            // Errors are restored too (their partial files already gone).
            kept.push(item);
        }
    }
    // Drop the saved history if it changed (removed entries).
    let changed = kept.len() != items.len();
    let _ = save_history(app, &kept).await;
    if changed {
        let _ = app.emit("queue-changed", kept.clone());
    }
    Ok(kept)
}

/// Remove leftover thumbnail/infojson files for a media id.
pub async fn cleanup_media(app: &tauri::AppHandle, id: &str) -> Result<(), String> {
    let dir = app.path().app_data_dir().unwrap_or_default().join("media");
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(_) => return Ok(()),
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        if entry.file_type().await.map(|t| t.is_file()).unwrap_or(false)
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{id}."))
        {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YtDlpStatus {
    pub bundled_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub auto_check: bool,
}

/// App-wide state shared between commands.
pub struct AppState {
    pub queue: tokio::sync::Mutex<Vec<DownloadItem>>,
    pub auto_check_ytdlp: tokio::sync::Mutex<bool>,
    pub ytdlp_version: tokio::sync::Mutex<String>,
    pub last_line: tokio::sync::Mutex<Option<String>>,
    /// Active yt-dlp subprocesses, keyed by queue item id — needed so
    /// cancel_download can terminate a running download.
    pub active_children: tokio::sync::Mutex<std::collections::HashMap<String, tokio::process::Child>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            queue: tokio::sync::Mutex::new(Vec::new()),
            auto_check_ytdlp: tokio::sync::Mutex::new(true),
            ytdlp_version: tokio::sync::Mutex::new(String::new()),
            last_line: tokio::sync::Mutex::new(None),
            active_children: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        })
        .invoke_handler(tauri::generate_handler![
            downloader::start_download,
            downloader::cancel_download,
            downloader::get_queue,
            downloader::clear_queue,
            downloader::remove_item,
            yt_dlp::thumbnail_for,
            yt_dlp::check_ytdlp_update,
            yt_dlp::update_ytdlp,
            yt_dlp::get_ytdlp_status,
            yt_dlp::set_auto_check,
            yt_dlp::ytdlp_debug_info,
            yt_dlp::load_last_dir,
            yt_dlp::save_last_dir,
            yt_dlp::get_app_version,
        ])
        .setup(|app| {
            // Initialize bundled yt-dlp if missing, then optionally check for yt-dlp updates.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppState>();
                if let Err(e) = yt_dlp::ensure_bundled_ytdlp(&handle).await {
                    eprintln!("yt-dlp bundle init failed: {e}");
                }
                if let Err(e) = yt_dlp::ensure_ffmpeg(&handle).await {
                    eprintln!("ffmpeg bundle init failed: {e}");
                }
                if let Ok(version) = yt_dlp::bundled_ytdlp_version(&handle).await {
                    *state.ytdlp_version.lock().await = version;
                }
                // Restore the download history from disk so the queue
                // survives restarts. Entries whose file was manually
                // deleted from the device are dropped automatically.
                if let Ok(history) = crate::load_history(&handle).await {
                    if !history.is_empty() {
                        let mut queue = state.queue.lock().await;
                        queue.extend(history);
                        let snapshot = queue.clone();
                        drop(queue);
                        let _ = handle.emit("queue-changed", snapshot);
                    }
                }
                let auto = *state.auto_check_ytdlp.lock().await;
                if auto {
                    match yt_dlp::fetch_latest_ytdlp_version().await {
                        Ok(latest) => {
                            let ver = state.ytdlp_version.lock().await.clone();
                            let available = !ver.is_empty()
                                && latest != ver
                                && yt_dlp::newer_than(&latest, &ver);
                            if available {
                                let _ = handle.emit("ytdlp-update-available", latest);
                            }
                        }
                        Err(e) => eprintln!("yt-dlp latest version check failed: {e}"),
                    }
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

pub fn data_dir(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("videograb")
}

pub fn yt_dlp_path(app: &tauri::AppHandle) -> PathBuf {
    let dir = data_dir(app);
    if cfg!(windows) {
        dir.join("yt-dlp.exe")
    } else {
        dir.join("yt-dlp")
    }
}

/// Path to the bundled ffmpeg binary in the app data directory.
/// yt-dlp uses it automatically for muxing separate video/audio streams.
pub fn ffmpeg_path(app: &tauri::AppHandle) -> PathBuf {
    let dir = data_dir(app);
    if cfg!(windows) {
        dir.join("ffmpeg.exe")
    } else {
        dir.join("ffmpeg")
    }
}

