// VideoGrab — YouTube downloader powered by yt-dlp
// Tauri v2 backend: download queue, bundled yt-dlp management, app & yt-dlp updates.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{Emitter, Manager};

mod downloader;
mod yt_dlp;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadItem {
    pub id: String,
    pub url: String,
    pub mode: String,      // "video" | "audio"
    pub quality: String,   // "best" | "1080" | "720" | "480" | "mp3" | "m4a"
    pub save_dir: String,
    pub status: String,    // pending | running | done | error
    pub title: Option<String>,
    pub progress: f64,     // 0..100
    pub speed: Option<String>,
    pub error: Option<String>,
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
        })
        .invoke_handler(tauri::generate_handler![
            downloader::start_download,
            downloader::get_queue,
            downloader::clear_queue,
            downloader::remove_item,
            yt_dlp::check_ytdlp_update,
            yt_dlp::update_ytdlp,
            yt_dlp::get_ytdlp_status,
            yt_dlp::set_auto_check,
            yt_dlp::ytdlp_debug_info,
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
