// Download queue: each item spawns an yt-dlp subprocess and parses the
// human-readable progress lines to report progress to the frontend.

use crate::{yt_dlp_path, AppState, DownloadItem};
use tauri::{Emitter, Manager};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Characters that are forbidden in file names on Windows (they make
/// yt-dlp fail with "[Errno 22] Invalid argument" when building the
/// output path from the video title).
#[cfg(windows)]
fn sanitize_windows_filename(name: &str) -> String {
    name.chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect()
}

/// Sanitize the save directory path on Windows: replace forbidden
/// filename characters so the -o template never produces an invalid path.
/// On non-Windows platforms this is a no-op (no forbidden characters).
#[cfg(not(windows))]
fn sanitize_dir(dir: &str) -> String {
    dir.to_string()
}

#[cfg(windows)]
fn sanitize_dir(dir: &str) -> String {
    // The directory may legally contain ':' (drive letter) and '\',
    // only the name-portion characters matter. Keep ':' after the
    // drive letter (single letter + colon) and backslashes.
    let mut out = String::with_capacity(dir.len());
    for (i, c) in dir.chars().enumerate() {
        if i == 1 && c == ':' {
            out.push(c);
        } else if "<>\"/\\|?*".contains(c) {
            out.push('_');
        } else {
            out.push(c);
        }
    }
    out
}

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
        // On Windows the title may carry forbidden filename characters
        // (':', '<', '|', '"', '?', '*'...), which made yt-dlp fail with
        // "[Errno 22] Invalid argument". sanitize_windows_filename
        // rewrites them to '_' before yt-dlp expands the template
        // characters (% is passed verbatim here, only the base path is
        // sanitized — yt-dlp itself sanitizes template output on
        // Windows too, but sanitizing the base path up front covers
        // user-chosen folders that may carry odd characters).
        format!(
            "{}/%(title).120s [%(id)s].%(ext)s",
            if cfg!(windows) {
                sanitize_dir(&item.save_dir)
            } else {
                item.save_dir.clone()
            }
        ),
    ];
    // Download the video thumbnail into the app data dir so the UI can
    // display it next to the queued item. Thumbnails are keyed by id.
    if item.mode != "audio" {
        let thumb_dir = app_thumb_dir(&app);
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



/// Remove all partial/intermediate files yt-dlp may have written for a
/// cancelled item in the chosen save directory. yt-dlp's output template
/// is "{save_dir}/{title} [{media_id}].{ext}", so matching on the id
/// inside brackets reliably covers every stage: fragment files
/// (…{id}.f123.webm), merged webm, temp files (.part) and the final mp4.
async fn remove_partial_files_for(save_dir: &str, media_id: Option<&str>) {
    let dir = std::path::Path::new(save_dir);
    let mut ids = Vec::new();
    if let Some(id) = media_id {
        ids.push(id.to_string());
    }
    let patterns: Vec<String> = ids
        .iter()
        .map(|id| format!("[{id}]"))
        .collect();
    if patterns.is_empty() {
        return;
    }
    let entries = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };
    let mut rd = entries;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if patterns.iter().any(|p| name.contains(p)) {
            // Never touch the thumbnail/media directory itself if it
            // somehow ended up here; only delete files.
            if entry.path().is_file() {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }
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

/// Fetch video metadata (id + title) with a lightweight yt-dlp invocation
/// so the queue can show the real title and thumbnail immediately.
async fn fetch_metadata(yt: &str, url: &str) -> Option<(String, String)> {
    let out = ytdlp_cmd(yt)
        .args([
            "--no-download",
            "--print",
            "%(id)s\t%(title)s",
            url,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let (first, title) = line.trim().split_once('\t')?;
    if first.len() < 5 {
        return None;
    }
    Some((first.to_string(), title.to_string()))
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
    // Keep a copy to return from the command; the spawn takes ownership.
    let item0 = item.clone();
    tauri::async_runtime::spawn(async move {
        let s = app2.state::<AppState>();
        // Fetch metadata first so the queue shows the real title and
        // thumbnail right away (fallback: parse stdout during download).
        if let Some((vid, title)) = fetch_metadata(&yt, &url).await {
            update_item(&app2, &s, &id, |i| {
                i.media_id = Some(vid);
                if i.title.is_none() {
                    i.title = Some(title);
                }
            })
            .await;
        }
        let args = build_args(&app2, &item);
        let child = match ytdlp_cmd(&yt)
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
        {
            let mut children = s.active_children.lock().await;
            children.insert(id.clone(), child);
        }
        // Wait for the process, interpret the exit, and finalize the item.
        let _ = wait_and_finalize(&app2, &s, &id).await;
        s.active_children.lock().await.remove(&id);
    });

    Ok(item0)
}

/// Wait for the yt-dlp subprocess, interpret the exit, and update the item.
async fn wait_and_finalize(
    app: &tauri::AppHandle,
    s: &tauri::State<'_, AppState>,
    id: &str,
) -> bool {
    let app_owned = app.clone();
    let id_owned = id.to_string();
    let app_spawn = app_owned.clone();
    let id_spawn = id_owned.clone();
    let (stdout, stderr) = {
        let mut children = s.active_children.lock().await;
        if let Some(c) = children.get_mut(id) {
            (c.stdout.take(), c.stderr.take())
        } else {
            return false;
        }
    };
    let reader_task = tokio::spawn(async move {
        let app = app_spawn;
        let id = id_spawn;
        let mut readers = vec![];
        if let Some(out) = stdout {
            readers.push(tokio::spawn(read_lines(app.clone(), id.clone(), out)));
        }
        if let Some(err_out) = stderr {
            readers.push(tokio::spawn(read_lines_err(app.clone(), id.clone(), err_out)));
        }
        for r in readers {
            let _ = r.await;
        }
    });

    // Poll the child with a short timeout so a cancelled (killed) process
    // does not make the finalize step hang forever.
    let ok = match timeout(
        std::time::Duration::from_secs(30),
        wait_child(&app_owned, s, &id_owned),
    )
    .await
    {
        Some(result) => result,
        None => {
            // Not finished within 30s after stdout exhausted — unlikely,
            // treat as unfinished but keep the state already set.
            false
        }
    };
    let _ = reader_task.await;

    let s = app_owned.state::<AppState>();
    update_item(&app_owned, &s, &id_owned, |i| {
        if i.status != "done" {
            // A cancelled download already marks itself as `error` with a
            // friendly message — do not overwrite it with a generic one.
            if i.status == "error" || i.status == "cancelled" {
                // nothing to do
            } else if ok {
                i.status = "done".into();
                i.progress = 100.0;
                i.stage = "done".into();
            } else if i.error.is_none() {
                i.status = "error".into();
                let s2 = app.state::<AppState>();
                let detail = s2
                    .last_line
                    .try_lock()
                    .ok()
                    .and_then(|mut l| l.take());
                i.error = Some(
                    detail.unwrap_or_else(|| "yt-dlp exited with an error".into()),
                );
            }
        }
    })
    .await;
    // Persist the queue (history) after the item settles.
    let _ = save_history(app, &s).await;
    ok
}

async fn read_lines(
    app: tauri::AppHandle,
    id: String,
    stream: tokio::process::ChildStdout,
) {
    let s = app.state::<AppState>();
    let mut reader = BufReader::new(stream).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim().to_string();
        handle_line(&app, &s, &id, &line).await;
    }
}

async fn read_lines_err(
    app: tauri::AppHandle,
    id: String,
    stream: tokio::process::ChildStderr,
) {
    let s = app.state::<AppState>();
    let mut reader = BufReader::new(stream).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim().to_string();
        handle_line(&app, &s, &id, &line).await;
    }
}

/// Wait for the process, interpreting success; returns true if the
/// download completed normally (not killed by cancel).
async fn wait_child(
    app: &tauri::AppHandle,
    s: &tauri::State<'_, AppState>,
    id: &str,
) -> bool {
    // Poll the registered child until it exits or disappears (e.g. killed
    // by cancel_download — in that case we return `false` so the caller
    // does not mark the item as successfully done).
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let mut children = s.active_children.lock().await;
        if let Some(c) = children.get_mut(id) {
            match c.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) => continue,
                Err(_) => return false,
            }
        } else {
            return false;
        }
    }
}

/// A lightweight poll-with-timeout helper.
async fn timeout<F: std::future::Future>(
    dur: std::time::Duration,
    fut: F,
) -> Option<F::Output> {
    tokio::time::timeout(dur, fut).await.ok()
}

/// Cancel a running (or pending) download: kills the yt-dlp subprocess and
/// deletes everything yt-dlp managed to write into the save directory for
/// this video.
#[tauri::command]
pub async fn cancel_download(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let item = state
        .queue
        .lock()
        .await
        .iter()
        .find(|i| i.id == id)
        .cloned();
    let Some(item) = item else {
        return Ok(());
    };
    // Kill the subprocess if it is still registered as active.
    let was_running = {
        let mut children = state.active_children.lock().await;
        if let Some(mut child) = children.remove(&id) {
            let _ = child.kill().await;
            true
        } else {
            false
        }
    };
    // Clean up any partial/intermediate output files yt-dlp wrote.
    remove_partial_files_for(&item.save_dir, item.media_id.as_deref()).await;

    update_item(&app, &state, &id, |i| {
        if i.status == "pending" || i.status == "running" {
            i.status = "error".into();
            i.error = Some(if was_running {
                "Скасування завантаження".into()
            } else {
                "Очікує скасування".into()
            });
        }
    })
    .await;
    // Also remove leftover app-data media files for this video.
    if let Some(vid) = item.media_id.as_deref() {
        let _ = crate::cleanup_media(&app, vid).await;
    }
    // Sync the queue to disk (history persistence).
    let _ = save_history(&app, &state).await;
    Ok(())
}

/// Update a queue item and emit the change to the frontend.
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

/// Persist the whole queue to disk (download history).
async fn save_history(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
) -> Result<(), String> {
    let queue = state.queue.lock().await;
    crate::save_history(app, &queue).await
}

#[tauri::command]
pub async fn get_queue(state: tauri::State<'_, AppState>) -> Result<Vec<DownloadItem>, String> {
    Ok(state.queue.lock().await.clone())
}

#[tauri::command]
pub async fn clear_queue(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    state.queue.lock().await.clear();
    let _ = save_history(&app, &state).await;
    Ok(())
}

/// Remove a queue item. For finished entries the actual downloaded file
/// (and leftover thumbnails/infojson) is deleted from the device too —
/// the single-button "delete downloaded video" feature.
#[tauri::command]
pub async fn remove_item(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let target = state
        .queue
        .lock()
        .await
        .iter()
        .find(|i| i.id == id)
        .cloned();
    state.queue.lock().await.retain(|i| i.id != id);
    let snapshot = state.queue.lock().await.clone();
    let _ = app.emit("queue-changed", snapshot);
    let _ = save_history(&app, &state).await;
    if let Some(item) = target {
        if item.status == "done" {
            // Delete the final video/audio file. The output template is
            // "{title} [{media_id}].{ext}", so matching the bracketed id
            // inside the file name covers the result regardless of the
            // title's characters.
            if let Some(vid) = &item.media_id {
                let pattern = format!("[{vid}]");
                let mut rd = match tokio::fs::read_dir(&item.save_dir).await {
                    Ok(rd) => rd,
                    Err(_) => return Ok(()),
                };
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let is_file = entry
                        .file_type()
                        .await
                        .map(|t| t.is_file())
                        .unwrap_or(false);
                    if is_file
                        && entry
                            .file_name()
                            .to_string_lossy()
                            .contains(&pattern)
                    {
                        let _ = tokio::fs::remove_file(entry.path()).await;
                    }
                }
                // Leftover thumbnails/infojson files too.
                let _ = crate::cleanup_media(&app, vid).await;
            }
        }
    }
    Ok(())
}

