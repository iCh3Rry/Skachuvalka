// Download queue: each item spawns an yt-dlp subprocess and parses the
// human-readable progress lines to report progress to the frontend.

use crate::{yt_dlp_path, AppState, DownloadItem};
use tauri::{Emitter, Manager};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Spawn yt-dlp with a clean environment so the self-contained binary is not
/// poisoned by host/launcher environment variables (e.g. PYTHONHOME injected
/// by the AppImage runtime).
pub fn ytdlp_cmd(bin: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.env_clear();
    // The bundled ffmpeg lives next to yt-dlp in the app data directory;
    // putting it on PATH lets yt-dlp find it for muxing automatically.
    let ffmpeg_dir = std::path::Path::new(bin)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_string_lossy()
        .to_string();
    if cfg!(windows) {
        // Windows needs SystemRoot for TLS; keep the essentials,
        // plus the ffmpeg directory on PATH for yt-dlp muxing.
        for (k, v) in std::env::vars_os() {
            let key = k.to_string_lossy().to_uppercase();
            if key == "SYSTEMROOT" {
                cmd.env(k, v);
            }
        }
        cmd.env("PATH", format!("{};C:\\Windows\\System32", ffmpeg_dir));
    } else {
        // Prepend the app data directory (next to yt-dlp) so yt-dlp finds
        // the bundled ffmpeg for muxing separate video/audio streams.
        cmd.env("PATH", format!("{}:/usr/local/bin:/usr/bin:/bin", ffmpeg_dir));
        // yt-dlp needs a real environment: HOME for its cache/config, TMPDIR
        // for temp files during ffmpeg muxing, USER/LANG for locale. Without
        // these (env_clear) it can fail with "yt-dlp exited with an error",
        // which happens when launched from an app bundle on macOS/Linux.
        if let Ok(home) = std::env::var("HOME") {
            cmd.env("HOME", home);
            cmd.env("USER", std::env::var("USER").unwrap_or_else(|_| "user".into()));
        } else {
            cmd.env("HOME", "/tmp");
            cmd.env("USER", "user");
        }
        cmd.env("TMPDIR", std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()));
        cmd.env("LANG", std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into()));
        // Forward the system TLS cert bundle if present (some distros).
        if let Ok(v) = std::env::var("SSL_CERT_FILE") {
            cmd.env("SSL_CERT_FILE", v);
        }
        if let Ok(v) = std::env::var("SSL_CERT_DIR") {
            cmd.env("SSL_CERT_DIR", v);
        }
    }
    cmd
}

fn build_args(app: &tauri::AppHandle, item: &DownloadItem) -> Vec<String> {
    let mut args = vec![
        "--newline".into(),
        "--no-playlist".into(),
        // --no-self-update was REMOVED in yt-dlp 2026.08; passing it makes
        // yt-dlp exit with code 2 ("no such option") on newer versions,
        // which previously surfaced as the generic
        // "yt-dlp exited with an error" in the installed app.
        "--no-update".into(),
        // Fallback player clients: when the default client is blocked
        // (bot-check / no JS runtime) yt-dlp tries these before giving up.
        "--extractor-args".into(),
        "youtube:player_client=default,web_embedded,web_safari,ios".into(),
        "--no-warnings".into(),
        "-o".into(),
        format!("{}/%(title).120s [%(id)s].%(ext)s", item.save_dir),
    ];
    // Download the video thumbnail into the app data dir so the UI can
    // display it next to the queued item. Thumbnails are keyed by id.
    if item.mode != "audio" {
        let thumb_dir = app_thumb_dir(&app);
        args.push("--write-thumbnail".into());
        args.push("--write-info-json".into());
        args.push("-o".into());
        args.push(format!(
            "thumbnail:{}/%(id)s.%(ext)s",
            thumb_dir.to_string_lossy()
        ));
        args.push("-o".into());
        args.push(format!("infojson:{}", thumb_dir.to_string_lossy()));
    }
    match item.mode.as_str() {
        "audio" => {
            let ext = match item.quality.as_str() {
                "m4a" => "m4a",
                "mp3" | _ => "mp3",
            };
            args.push("-x".into());
            args.push("--audio-format".into());
            args.push(ext.into());
            args.push("--audio-quality".into());
            args.push("0".into());
        }
        _ => match item.quality.as_str() {
            // Prefer mp4 containers (h264 video + mp4a audio) so the
            // result is a playable MP4 rather than webm/opus, which the
            // user could not open.
            "1080" | "720" | "480" => {
                args.push("-f".into());
                args.push(format!(
                    "bv*[height<={}][vcodec~=avc]+ba*[acodec~=mp4]/bv*[height<={}]/b[height<={}]/b*+ba/b",
                    item.quality, item.quality, item.quality
                ));
            }
            _ => {
                args.push("-f".into());
                args.push("bv*[vcodec~=avc]+ba*[acodec~=mp4]/bv*+ba/b".into());
            }
        }
    }
    // Remux everything into mp4 for video modes, so the user always
    // ends up with exactly one .mp4 file.
    if item.mode != "audio" {
        args.push("--merge-output-format".into());
        args.push("mp4".into());
    }
    args.push(item.url.clone());
    args
}

/// Directory where thumbnails/infojson are stored ({data_dir}/media).
fn app_thumb_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
        .join("media")
}

/// Extract the YouTube video id from the infojson path yt-dlp prints, e.g.
/// [info] Writing video metadata as JSON to: /path/media/dQw4w9WgXcQ.info.json
fn parse_media_id_line(line: &str) -> Option<String> {
    let rest = line.strip_prefix("[info] Writing video metadata as JSON to:")?.trim();
    let stem = std::path::Path::new(rest)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())?;
    if stem.len() > 5 && stem.len() < 16 {
        Some(stem)
    } else {
        None
    }
}

/// Extract percent and speed from a line like:
/// [download]  52.8% of  967.79KiB at  232.51KiB/s ETA 00:01
fn parse_progress_line(line: &str) -> Option<(f64, Option<String>)> {
    let line = line.trim();
    let after = line.strip_prefix("[download]")?;
    let rest = after.trim();
    let pct: f64 = rest.split('%').next()?.trim().parse().ok()?;
    // Clamp to a sane range: yt-dlp occasionally emits raw chunk
    // progress lines that can look like >100% in buffered reads.
    let pct = pct.max(0.0).min(100.0);
    let mut speed = None;
    if let Some(at_pos) = rest.find("at ") {
        let after_at = rest[at_pos + 3..].trim();
        if let Some(end) = after_at.find(" ETA") {
            speed = Some(after_at[..end].trim().to_string());
        } else {
            speed = Some(after_at.to_string());
        }
    }
    Some((pct, speed))
}

/// Extract a title from the [info]/[Merger] style lines, e.g.
/// [info] Title Here: Downloading 1 format(s)
fn parse_title_line(line: &str) -> Option<String> {
    let rest = line.strip_prefix("[info]")?.trim();
    let title = rest.split(':').next()?.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// Start a new download; the work happens in a spawned task so the UI stays responsive.
#[tauri::command]
pub async fn start_download(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    url: String,
    mode: String,
    quality: String,
    save_dir: String,
) -> Result<DownloadItem, String> {
    let item = DownloadItem {
        id: uuid::Uuid::new_v4().to_string(),
        url: url.clone(),
        mode,
        quality,
        save_dir,
        status: "pending".into(),
        stage: "info".into(),
        media_id: None,
        title: None,
        progress: 0.0,
        speed: None,
        error: None,
    };
    state.queue.lock().await.push(item.clone());
    let _ = app.emit("queue-changed", state.queue.lock().await.clone());

    let id = item.id.clone();
    let app2 = app.clone();
    let yt = yt_dlp_path(&app).display().to_string();
    let args = build_args(&app2, &item);
    tauri::async_runtime::spawn(async move {
        let mut child = match ytdlp_cmd(&yt)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let s = app2.state::<AppState>();
                update_item(&app2, &s, &id, |i| {
                    i.status = "error".into();
                    i.error = Some(format!("Cannot start yt-dlp ({yt}): {e}."));
                })
                .await;
                return;
            }
        };

        let s = app2.state::<AppState>();
        update_item(&app2, &s, &id, |i| i.status = "running".into()).await;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        if let Some(out) = stdout {
            let mut reader = BufReader::new(out).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let line = line.trim().to_string();
                handle_line(&app2, &s, &id, &line).await;
            }
        }
        if let Some(err_out) = stderr {
            let mut reader = BufReader::new(err_out).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let line = line.trim().to_string();
                handle_line(&app2, &s, &id, &line).await;
            }
        }

        let ok = child.wait().await.map(|x| x.success()).unwrap_or(false);
        update_item(&app2, &s, &id, |i| {
            if i.status != "done" {
                if ok {
                    i.status = "done".into();
                    i.progress = 100.0;
                    i.stage = "done".into();
                } else if i.error.is_none() {
                    i.status = "error".into();
                    // Show the last captured yt-dlp output line as the reason.
                    let s2 = app2.state::<AppState>();
                    let detail = s2
                        .last_line
                        .try_lock()
                        .ok()
                        .and_then(|mut l| l.take());
                    i.error = Some(
                        detail
                            .unwrap_or_else(|| "yt-dlp exited with an error".into()),
                    );
                }
            }
        })
        .await;
    });

    Ok(item)
}

async fn handle_line(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    id: &str,
    line: &str,
) {
    if line.starts_with("[download]") {
        if let Some((pct, speed)) = parse_progress_line(line) {
            update_item(app, state, id, |i| {
                i.progress = (pct * 100.0).round();
                i.speed = speed;
                if i.stage == "info" {
                    i.stage = "downloading".into();
                }
            })
            .await;
        } else if line.contains("Destination:") {
            // keep as-is: download starting
        } else if line.contains("100%") || line.contains("Download completed") {
            update_item(app, state, id, |i| {
                i.status = "done".into();
                i.progress = 100.0;
                i.stage = "done".into();
            })
            .await;
        }
    } else if line.starts_with("[info]") {
        if let Some(title) = parse_title_line(line) {
            update_item(app, state, id, |i| {
                if i.title.is_none() {
                    i.title = Some(title);
                }
            })
            .await;
        }
        if let Some(vid) = parse_media_id_line(line) {
            update_item(app, state, id, |i| {
                if i.media_id.is_none() {
                    i.media_id = Some(vid);
                }
            })
            .await;
        }
        // Remember the last line so a detailed reason can be shown on failure.
        if !line.is_empty() {
            if let Ok(mut last) = state.last_line.try_lock() {
                *last = Some(line.to_string());
            }
        }
    } else if line.starts_with("[Merger]") {
        update_item(app, state, id, |i| {
            i.stage = "merging".into();
            i.progress = 100.0;
        })
        .await;
        if let Some(title) = parse_title_line(line) {
            update_item(app, state, id, |i| {
                if i.title.is_none() {
                    i.title = Some(title);
                }
            })
            .await;
        }
    } else if line.starts_with("[error]") || line.starts_with("ERROR:") {
        let msg = line
            .strip_prefix("[error]")
            .or_else(|| line.strip_prefix("ERROR:"))
            .unwrap_or(line)
            .trim()
            .to_string();
        update_item(app, state, id, |i| {
            if i.status != "done" {
                i.status = "error".into();
                if i.error.is_none() {
                    i.error = Some(msg);
                }
            }
        })
        .await;
    }
}

async fn update_item<F: FnOnce(&mut DownloadItem)>(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    id: &str,
    f: F,
) {
    let mut queue = state.queue.lock().await;
    if let Some(item) = queue.iter_mut().find(|i| i.id == id) {
        f(item);
        let snapshot = item.clone();
        drop(queue);
        let _ = app.emit("item-updated", snapshot);
    }
}

#[tauri::command]
pub async fn get_queue(state: tauri::State<'_, AppState>) -> Result<Vec<DownloadItem>, String> {
    Ok(state.queue.lock().await.clone())
}

#[tauri::command]
pub async fn clear_queue(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.queue.lock().await.clear();
    Ok(())
}

#[tauri::command]
pub async fn remove_item(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    state.queue.lock().await.retain(|i| i.id != id);
    Ok(())
}

