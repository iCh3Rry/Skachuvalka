// yt-dlp management: bundle the self-contained executable, check GitHub for
// new releases, and download a fresh binary on user request.

use crate::downloader::ytdlp_cmd;
use crate::yt_dlp_path;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tauri::{Emitter, Manager};

const YTDLP_GH_RELEASE: &str =
    "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct YtDlpStatus {
    pub bundled_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub auto_check: bool,
}

/// Asset name suffixes used by the official yt-dlp release page.
fn release_asset_name() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else if cfg!(target_arch = "aarch64") {
        "yt-dlp_aarch64"
    } else if cfg!(target_os = "macos") {
        "yt-dlp_macos"
    } else {
        "yt-dlp"
    }
}

/// Semantic comparison: returns true if `a` is newer than `b`.
pub fn newer_than(a: &str, b: &str) -> bool {
    let va = Version::parse(a.trim_start_matches('v')).ok();
    let vb = Version::parse(b.trim_start_matches('v')).ok();
    match (va, vb) {
        (Some(a), Some(b)) => a > b,
        _ => a != b,
    }
}

/// Path to the bundled executable inside the app resources.
fn bundled_resource_path(app: &tauri::AppHandle) -> PathBuf {
    let name = if cfg!(windows) {
        "yt-dlp-bundled.exe"
    } else {
        "yt-dlp-bundled"
    };
    // Tauri v2 places bundled resources under a `resources/` subdirectory
    // of the resource_dir (e.g. usr/lib/VideoGrab/resources on Linux).
    app.path()
        .resource_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("resources")
        .join(name)
}

/// Name of the bundled ffmpeg resource for the current platform.
/// For universal macOS builds the architecture is detected at runtime.
fn bundled_ffmpeg_resource_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg-bundled.exe"
    } else if cfg!(target_arch = "aarch64") {
        "ffmpeg-bundled-arm64"
    } else {
        "ffmpeg-bundled-x64"
    }
}

/// Runtime architecture check (needed for universal macOS builds where
/// target_arch is fixed at compile time to the primary build arch).
fn host_is_arm64() -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sysctl")
            .arg("-n")
            .arg("hw.optional.arm64")
            .output()
        {
            return String::from_utf8_lossy(&out.stdout).trim() == "1";
        }
    }
    #[cfg(all(target_arch = "x86_64", not(target_os = "macos")))]
    {
        if let Ok(out) = std::process::Command::new("uname")
            .arg("-m")
            .output()
        {
            return String::from_utf8_lossy(&out.stdout).trim() == "aarch64";
        }
    }
    cfg!(target_arch = "aarch64")
}

/// On a universal macOS build both ffmpeg-bundled-x64 and
/// ffmpeg-bundled-arm64 are bundled; pick the one matching the running
/// process architecture.
fn bundled_ffmpeg_candidate_paths(app: &tauri::AppHandle) -> Vec<PathBuf> {
    let base = app
        .path()
        .resource_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("resources");
    let primary = base.join(bundled_ffmpeg_resource_name());
    let mut candidates = vec![primary.clone()];
    // Prefer the binary matching the running architecture; fall back to
    // the other macOS slice or the Windows build (dev on different OS).
    let preferred = if host_is_arm64() {
        base.join("ffmpeg-bundled-arm64")
    } else {
        base.join("ffmpeg-bundled-x64")
    };
    if preferred != primary {
        candidates.insert(0, preferred);
    }
    if cfg!(not(windows)) {
        let win = base.join("ffmpeg-bundled.exe");
        if win != primary && win.exists() {
            candidates.push(win);
        }
    }
    candidates
}

/// Ensure the data-dir copy of ffmpeg exists: copy the matching platform
/// binary from app resources. yt-dlp picks it up automatically when it
/// lives in the same directory (or is on PATH).
pub async fn ensure_ffmpeg(app: &tauri::AppHandle) -> Result<(), String> {
    let target = crate::ffmpeg_path(app);
    if target.exists() && tokio::fs::metadata(&target).await.map_or(false, |m| m.len() > 1000) {
        return Ok(());
    }
    let mut candidates = bundled_ffmpeg_candidate_paths(app);
    // Dev builds: also try the resource_dir root (no `resources/` subdir).
    let base = app.path().resource_dir().unwrap_or_else(|_| PathBuf::from("."));
    let name = bundled_ffmpeg_resource_name();
    if base.join(name).exists() {
        candidates.push(base.join(name));
    }
    let res = candidates.into_iter().find(|p| p.exists());
    let Some(res) = res else {
        // ffmpeg is optional: without it yt-dlp only warns about muxing.
        eprintln!("ffmpeg resource not found, downloads will not be muxed");
        return Ok(());
    };
    tokio::fs::create_dir_all(target.parent().unwrap()).await.map_err(|e| e.to_string())?;
    tokio::fs::copy(&res, &target).await.map_err(|e| e.to_string())?;
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).await;
    }
    Ok(())
}

/// Ensure the data-dir copy of yt-dlp exists: copy from resources or download.
pub async fn ensure_bundled_ytdlp(
    app: &tauri::AppHandle,
) -> Result<(), String> {
    let target = yt_dlp_path(app);
    if target.exists() && tokio::fs::metadata(&target).await.map_or(false, |m| m.len() > 1000) {
        return Ok(());
    }
    let res = bundled_resource_path(app);
    let res = if res.exists() {
        Some(res)
    } else {
        // Fallback: try resource_dir root (dev builds) or a resources sibling.
        let base = app.path().resource_dir().unwrap_or_else(|_| PathBuf::from("."));
        [base.join("resources").join(res.file_name().unwrap_or_default()), base.join(res.file_name().unwrap_or_default())]
            .into_iter()
            .find(|p| p.exists())
    };
    if let Some(res) = res {
        tokio::fs::create_dir_all(target.parent().unwrap()).await.map_err(|e| e.to_string())?;
        tokio::fs::copy(&res, &target).await.map_err(|e| e.to_string())?;
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(
                &target,
                std::fs::Permissions::from_mode(0o755),
            )
            .await;
        }
        return Ok(());
    }
    // Fallback: download the self-contained binary straight from GitHub.
    match download_latest_ytdlp_to(app, &target).await {
        Ok(()) => Ok(()),
        Err(e) => Err(format!(
            "yt-dlp не знайдено ні в ресурсах програми, ні в мережі: {e}"
        )),
    }
}

/// Read the version of the yt-dlp binary living in the data directory.
pub async fn bundled_ytdlp_version(
    app: &tauri::AppHandle,
) -> Result<String, String> {
    let target = yt_dlp_path(app);
    let candidate = if target.exists() {
        Some(target)
    } else {
        let res = bundled_resource_path(app);
        if res.exists() {
            Some(res)
        } else {
            None
        }
    };
    let Some(bin) = candidate else {
        return Ok(String::new());
    };
    let out = ytdlp_cmd(&bin.display().to_string())
        .arg("--version")
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Query GitHub for the latest yt-dlp release tag.
pub async fn fetch_latest_ytdlp_version() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent("VideoGrab/1.0")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(YTDLP_GH_RELEASE)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub API returned {}", resp.status()));
    }
    let rel: GhRelease = resp.json().await.map_err(|e| e.to_string())?;
    Ok(rel.tag_name)
}

/// Debug info: paths and raw version result.
#[tauri::command]
pub async fn ytdlp_debug_info(
    app: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    let target = crate::yt_dlp_path(&app);
    let res = bundled_resource_path(&app);
    let raw = match ytdlp_cmd(&target.display().to_string()).arg("--version").output().await {
        Ok(o) => {
            format!(
                "exit={} stdout={:?} stderr={:?}",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        }
        Err(e) => format!("spawn error: {e}"),
    };
    Ok(serde_json::json!({
        "data_path": target.display().to_string(),
        "data_exists": target.exists(),
        "resource_path": res.display().to_string(),
        "resource_exists": res.exists(),
        "raw_version_output": raw,
    }))
}

/// Get the current combined status shown in the UI.
#[tauri::command]
pub async fn get_ytdlp_status(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
) -> Result<YtDlpStatus, String> {
    let bundled = bundled_ytdlp_version(&app).await.unwrap_or_default();
    let auto = *state.auto_check_ytdlp.lock().await;
    let latest = fetch_latest_ytdlp_version().await.ok();
    let update_available = latest
        .as_deref()
        .map(|l| !bundled.is_empty() && newer_than(l, &bundled))
        .unwrap_or(false);
    Ok(YtDlpStatus {
        bundled_version: bundled,
        latest_version: latest,
        update_available,
        auto_check: auto,
    })
}

/// Set whether the app checks yt-dlp updates at startup.
#[tauri::command]
pub async fn set_auto_check(
    state: tauri::State<'_, crate::AppState>,
    enabled: bool,
) -> Result<(), String> {
    *state.auto_check_ytdlp.lock().await = enabled;
    Ok(())
}

/// Check for a yt-dlp update (manual button in the UI).
#[tauri::command]
pub async fn check_ytdlp_update(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
) -> Result<YtDlpStatus, String> {
    let bundled = bundled_ytdlp_version(&app).await.unwrap_or_default();
    let latest = fetch_latest_ytdlp_version().await?;
    let update_available = newer_than(&latest, &bundled);
    Ok(YtDlpStatus {
        bundled_version: bundled,
        latest_version: Some(latest),
        update_available,
        auto_check: *state.auto_check_ytdlp.lock().await,
    })
}

/// Download the newest self-contained yt-dlp binary into the data directory.
#[tauri::command]
pub async fn update_ytdlp(
    app: tauri::AppHandle,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent("VideoGrab/1.0")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(YTDLP_GH_RELEASE)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub API returned {}", resp.status()));
    }
    let rel: GhRelease = resp.json().await.map_err(|e| e.to_string())?;
    let wanted = release_asset_name();
    let asset = rel
        .assets
        .into_iter()
        .find(|a| a.name == wanted)
        .ok_or_else(|| format!("asset {wanted} not found in release"))?;

    let target = yt_dlp_path(&app);
    // Write to a temp file first, then atomically replace.
    let tmp = target.with_extension("tmp");
    let bytes = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::create_dir_all(target.parent().unwrap())
        .await
        .map_err(|e| e.to_string())?;
    let mut f = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
    f.write_all(&bytes).await.map_err(|e| e.to_string())?;
    f.flush().await.map_err(|e| e.to_string())?;
    drop(f);
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).await;
    }
    tokio::fs::rename(&tmp, &target)
        .await
        .map_err(|e| e.to_string())?;

    let version = rel.tag_name;
    let _ = app.emit("ytdlp-updated", version.clone());
    Ok(version)
}

/// Download the latest yt-dlp binary to an arbitrary path (used by the fallback).
async fn download_latest_ytdlp_to(
    _app: &tauri::AppHandle,
    target: &Path,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent("VideoGrab/1.0")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(YTDLP_GH_RELEASE)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let rel: GhRelease = resp.json().await.map_err(|e| e.to_string())?;
    let wanted = release_asset_name();
    let asset = rel
        .assets
        .into_iter()
        .find(|a| a.name == wanted)
        .ok_or_else(|| format!("asset {wanted} not found in release"))?;
    let bytes = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::create_dir_all(target.parent().unwrap())
        .await
        .map_err(|e| e.to_string())?;
    let mut f = tokio::fs::File::create(target).await.map_err(|e| e.to_string())?;
    f.write_all(&bytes).await.map_err(|e| e.to_string())?;
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755)).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Thumbnails & persistent settings
// ---------------------------------------------------------------------------

/// Read the thumbnail image for a given YouTube video id and return it as
/// base64-encoded data. yt-dlp writes {data_dir}/media/{id}.jpg during the
/// download (see downloader::app_thumb_dir).
#[tauri::command]
pub async fn thumbnail_for(
    app: tauri::AppHandle,
    id: String,
) -> Result<String, String> {
    let base = crate::data_dir(&app);
    let mut target: Option<PathBuf> = None;
    for ext in &["jpg", "jpeg", "png", "webp"] {
        let p = base.join("media").join(format!("{id}.{ext}"));
        if p.exists() {
            target = Some(p);
            break;
        }
    }
    let Some(path) = target else {
        return Err("no thumbnail".into());
    };
    let bytes = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
    Ok(base64_encode(&bytes))
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(triple >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6 & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Load the last chosen download directory (persisted across launches).
#[tauri::command]
pub async fn load_last_dir(app: tauri::AppHandle) -> Result<String, String> {
    let settings = settings_path(&app);
    let content = match tokio::fs::read_to_string(&settings).await {
        Ok(c) => c,
        Err(_) => return Ok(String::new()),
    };
    if let Ok(map) = serde_json::from_str::<serde_json::Value>(&content) {
        if let Some(dir) = map.get("last_dir").and_then(|v| v.as_str()) {
            return Ok(dir.to_string());
        }
    }
    Ok(String::new())
}

/// Persist the chosen download directory so it is restored on next launch.
#[tauri::command]
pub async fn save_last_dir(
    app: tauri::AppHandle,
    dir: String,
) -> Result<(), String> {
    let settings = settings_path(&app);
    tokio::fs::create_dir_all(settings.parent().unwrap())
        .await
        .map_err(|e| e.to_string())?;
    let content = match tokio::fs::read_to_string(&settings).await {
        Ok(c) => c,
        Err(_) => "{}".into(),
    };
    let mut map: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));
    map["last_dir"] = serde_json::json!(dir);
    tokio::fs::write(&settings, map.to_string())
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// {data_dir}/settings.json
fn settings_path(app: &tauri::AppHandle) -> PathBuf {
    crate::data_dir(app).join("settings.json")
}

/// Returns the app version from the bundle (shown in Settings → About).
#[tauri::command]
pub fn get_app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}
