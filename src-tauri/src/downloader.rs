// Download queue: each item spawns an yt-dlp subprocess and parses the
// human-readable progress lines to report progress to the frontend.

use crate::yt_dlp_path;
use crate::{AppState, DownloadItem};
use tauri::{Emitter, Manager};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Spawn yt-dlp with a clean environment so the self-contained binary is not
/// poisoned by host/launcher environment variables (e.g. PYTHONHOME injected
/// by the AppImage runtime).
pub fn ytdlp_cmd(bin: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.env_clear();
    if cfg!(windows) {
        // Windows needs SystemRoot for TLS; keep the essentials.
        for (k, v) in std::env::vars_os() {
            let key = k.to_string_lossy().to_uppercase();
            if key == "SYSTEMROOT" {
                cmd.env(k, v);
            }
        }
    } else {
        cmd.env("PATH", "/usr/local/bin:/usr/bin:/bin");
        // Home may be needed for certificate/config paths on some distros.
        if let Ok(home) = std::env::var("HOME") {
            cmd.env("HOME", home);
        }
    }
    cmd
}

fn build_args(item: &DownloadItem) -> Vec<String> {
    let mut args = vec![
        "--newline".into(),
        "--no-playlist".into(),
        "--no-self-update".into(),
        "--no-warnings".into(),
        "-o".into(),
        format!("{}/%(title).120s [%(id)s].%(ext)s", item.save_dir),
    ];
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
            "1080" | "720" | "480" => {
                args.push("-f".into());
                args.push(format!(
                    "bv*[height<={}]+ba/b[height<={}]/b*+ba/b",
                    item.quality, item.quality
                ));
            }
            _ => {
                args.push("-f".into());
                args.push("bv*+ba/b".into());
            }
        },
    }
    args.push(item.url.clone());
    args
}

/// Extract percent and speed from a line like:
/// [download]  52.8% of  967.79KiB at  232.51KiB/s ETA 00:01
fn parse_progress_line(line: &str) -> Option<(f64, Option<String>)> {
    let line = line.trim();
    let after = line.strip_prefix("[download]")?;
    let rest = after.trim();
    let pct: f64 = rest.split('%').next()?.trim().parse().ok()?;
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
    let args = build_args(&item);
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
                } else if i.error.is_none() {
                    i.status = "error".into();
                    i.error = Some("yt-dlp exited with an error".into());
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
            })
            .await;
        } else if line.contains("Destination:") {
            // keep as-is: download starting
        } else if line.contains("100%") || line.contains("Download completed") {
            update_item(app, state, id, |i| {
                i.status = "done".into();
                i.progress = 100.0;
            })
            .await;
        }
    } else if line.starts_with("[info]") || line.starts_with("[Merger]") {
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
