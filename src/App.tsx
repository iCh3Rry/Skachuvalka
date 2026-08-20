import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { homeDir } from "@tauri-apps/api/path";
import { relaunch } from "@tauri-apps/plugin-process";
import { check as checkForUpdates } from "@tauri-apps/plugin-updater";
import "./App.css";

interface DownloadItem {
  id: string;
  url: string;
  mode: "video" | "audio";
  quality: string;
  save_dir: string;
  status: "pending" | "running" | "done" | "error";
  stage: string;
  media_id?: string;
  title?: string;
  progress: number;
  speed?: string;
  error?: string;
}

interface YtDlpStatus {
  bundled_version: string;
  latest_version?: string;
  update_available: boolean;
  auto_check: boolean;
}

type Mode = "video" | "audio";

const VIDEO_QUALITIES = [
  { value: "best", label: "Найкраща якість" },
  { value: "1080", label: "1080p" },
  { value: "720", label: "720p" },
  { value: "480", label: "480p" },
];
const AUDIO_QUALITIES = [
  { value: "mp3", label: "MP3" },
  { value: "m4a", label: "M4A" },
];

function formatProgress(p: number) {
  return Number.isFinite(p) ? `${Math.round(p)}%` : "";
}

/** Human-readable stage label while a download is running. */
function stageLabel(item: DownloadItem): string {
  if (item.status === "pending") return "Очікує…";
  if (item.status === "done") return "Готово";
  if (item.status === "error") return item.error || "Помилка";
  switch (item.stage) {
    case "info":
      return "Отримання інформації…";
    case "merging":
      return "Обробка відео…";
    case "downloading":
    default:
      return "Завантаження…";
  }
}

type SettingsTab = "updates" | "about";

export default function App() {
  const [url, setUrl] = useState("");
  const [mode, setMode] = useState<Mode>("video");
  const [quality, setQuality] = useState("best");
  const [saveDir, setSaveDir] = useState("");
  const [queue, setQueue] = useState<DownloadItem[]>([]);
  const [ytdlp, setYtdlp] = useState<YtDlpStatus | null>(null);
  const [checkingAppUpdate, setCheckingAppUpdate] = useState(false);
  const [appUpdateStatus, setAppUpdateStatus] = useState("");
  const [appVersion, setAppVersion] = useState("");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [settingsTab, setSettingsTab] = useState<SettingsTab>("updates");
  const [thumbnails, setThumbnails] = useState<Record<string, string>>({});
  const [saveBusy, setSaveBusy] = useState(false);

  useEffect(() => {
    let alive = true;
    listen<DownloadItem>("item-updated", (e) => {
      setQueue((q) =>
        q.map((item) => (item.id === e.payload.id ? e.payload : item))
      );
      // Thumbnail becomes available once yt-dlp writes the media id.
      if (e.payload.media_id) {
        void loadThumbnail(e.payload.media_id);
      }
    });
    listen<DownloadItem[]>("queue-changed", (e) => {
      if (alive) setQueue(e.payload);
    });
    listen<string>("ytdlp-update-available", (e) => {
      setYtdlp((s) =>
        s ? { ...s, latest_version: e.payload, update_available: true } : s
      );
    });
    listen<string>("ytdlp-updated", () => refreshYtdlp());
    void (async () => {
      try {
        const dir = await invoke<string>("load_last_dir");
        if (dir && alive) setSaveDir(dir);
        const ver = await invoke<string>("get_app_version");
        if (alive) setAppVersion(ver);
      } catch (e) {
        console.error("settings load error", e);
      }
    })();
    refreshYtdlp();
    // The backend copies the bundled yt-dlp and checks for updates asynchronously
    // at startup; poll briefly so the UI catches the result even if it races.
    let polls = 0;
    const timer = setInterval(() => {
      polls += 1;
      refreshYtdlp();
      if (polls >= 10) clearInterval(timer);
    }, 1000);
    return () => {
      alive = false;
    };
  }, []);

  async function loadThumbnail(mediaId: string) {
    // yt-dlp writes the thumbnail file AFTER emitting the media id, so retry
    // a few times with backoff until it arrives or we give up.
    const delays = [1500, 3000, 6000, 12000, 25000];
    for (const delay of delays) {
      try {
        const data = await invoke<string>("thumbnail_for", { id: mediaId });
        setThumbnails((t) => ({
          ...t,
          [mediaId]: `data:image/jpeg;base64,${data}`,
        }));
        return;
      } catch {
        // Thumbnail not ready yet; wait and try again.
      }
      await new Promise((r) => setTimeout(r, delay));
    }
  }

  async function refreshYtdlp() {
    try {
      const status = await invoke<YtDlpStatus>("get_ytdlp_status");
      setYtdlp(status);
    } catch (e) {
      console.error("yt-dlp status error", e);
    }
  }

  async function handleChooseDir() {
    const dir = await openDialog({ directory: true, multiple: false });
    if (dir) {
      const chosen = Array.isArray(dir) ? dir[0] : dir;
      setSaveDir(chosen);
      try {
        await invoke("save_last_dir", { dir: chosen });
      } catch (e) {
        console.error("save dir error", e);
      }
    }
  }

  async function handleDownload() {
    if (!url.trim()) return;
    setSaveBusy(true);
    try {
      const target = saveDir || (await homeDir()) + "Downloads";
      await invoke("start_download", {
        url: url.trim(),
        mode,
        quality,
        saveDir: target,
      });
      setUrl("");
      try {
        await invoke("save_last_dir", { dir: target });
      } catch (e) {
        console.error("save dir error", e);
      }
    } catch (e) {
      alert(`Помилка: ${e}`);
    } finally {
      setSaveBusy(false);
    }
  }

  async function handleCheckAppUpdate() {
    setCheckingAppUpdate(true);
    setAppUpdateStatus("");
    try {
      const update = await checkForUpdates();
      if (update) {
        setAppUpdateStatus(
          `Доступна нова версія ${update.version}. ${update.body ?? ""}`
        );
        if (
          confirm(
            `Оновити програму до версії ${update.version}?\n\n${update.body ?? ""}`
          )
        ) {
          setAppUpdateStatus("Встановлення оновлення...");
          await update.downloadAndInstall((event) => {
            if (event.event === "Finished") {
              setAppUpdateStatus("Оновлення встановлено. Перезапуск...");
            }
          });
          await relaunch();
        }
      } else {
        setAppUpdateStatus("Ви користуєтесь найновішою версією програми.");
      }
    } catch (e) {
      setAppUpdateStatus(`Перевірка оновлень програми: ${e}`);
    } finally {
      setCheckingAppUpdate(false);
    }
  }

  async function handleCheckYtdlpUpdate() {
    try {
      const status = await invoke<YtDlpStatus>("check_ytdlp_update");
      setYtdlp(status);
      if (status.update_available) {
        if (
          confirm(
            `Доступна нова версія yt-dlp ${status.latest_version} (у вас ${status.bundled_version}). Встановити?`
          )
        ) {
          const ver = await invoke<string>("update_ytdlp");
          alert(`yt-dlp оновлено до версії ${ver}.`);
          await refreshYtdlp();
        }
      } else {
        alert(`У вас найновіша версія yt-dlp (${status.bundled_version}).`);
      }
    } catch (e) {
      alert(`Перевірка оновлення yt-dlp: ${e}`);
    }
  }

  const qualities = mode === "video" ? VIDEO_QUALITIES : AUDIO_QUALITIES;

  return (
    <div className="app">
      <header className="compact-header">
        <div className="header-left">
          <h1>VideoGrab</h1>
          <p className="subtitle">Завантажуйте відео та аудіо з YouTube</p>
        </div>
        <button
          className="secondary settings-btn"
          onClick={() => setSettingsOpen(true)}
          title="Налаштування"
        >
          ⚙ Налаштування
        </button>
      </header>

      <main className="compact-main">
        <section className="card compact-card">
          <input
            className="url-input"
            placeholder="Вставте посилання YouTube..."
            value={url}
            onChange={(e) => setUrl(e.target.value)}
          />
          <div className="row">
            <div className="seg">
              <button
                className={mode === "video" ? "active" : ""}
                onClick={() => {
                  setMode("video");
                  setQuality("best");
                }}
              >
                Відео
              </button>
              <button
                className={mode === "audio" ? "active" : ""}
                onClick={() => {
                  setMode("audio");
                  setQuality("mp3");
                }}
              >
                Аудіо
              </button>
            </div>
            <select
              value={quality}
              onChange={(e) => setQuality(e.target.value)}
              className="quality-select"
            >
              {qualities.map((q) => (
                <option key={q.value} value={q.value}>
                  {q.label}
                </option>
              ))}
            </select>
            <button className="secondary" onClick={handleChooseDir}>
              {saveDir ? "Змінити папку" : "Обрати папку"}
            </button>
          </div>
          <div className="row">
            <p className="path-line">
              Зберігати в: {saveDir || "Завантаження"}
            </p>
            <button className="primary" onClick={handleDownload} disabled={saveBusy}>
              {saveBusy ? "Додавання…" : "Завантажити"}
            </button>
          </div>
        </section>

        <section className="card">
          <h2>Черга завантажень</h2>
          {queue.length === 0 && <p className="empty">Черга порожня</p>}
          {[...queue].reverse().map((item) => (
            <div key={item.id} className={`queue-item ${item.status}`}>
              <div className="qi-thumb">
                {thumbnails[item.media_id || ""] ? (
                  <img src={thumbnails[item.media_id || ""]} alt="" />
                ) : item.media_id ? (
                  <img
                    src={`https://i.ytimg.com/vi/${item.media_id}/default.jpg`}
                    alt=""
                    onError={(e) => {
                      (e.target as HTMLImageElement).style.display = "none";
                    }}
                  />
                ) : null}
              </div>
              <div className="qi-body">
                <div className="qi-title">
                  <span className="qi-name">{item.title || item.url}</span>
                  <span className="qi-meta">
                    {item.mode === "video" ? "відео" : "аудіо"} ·{" "}
                    {item.quality}
                  </span>
                </div>
                <div className="qi-bar-wrap">
                  <div
                    className="qi-bar"
                    style={{
                      width: `${Math.min(100, Math.max(0, item.progress))}%`,
                    }}
                  />
                </div>
                <div className="qi-status">
                  <span>
                    {formatProgress(item.progress)}
                    {item.speed ? ` · ${item.speed}` : ""}
                  </span>
                  <span>{stageLabel(item)}</span>
                </div>
              </div>
            </div>
          ))}
        </section>
      </main>

      {settingsOpen && (
        <div className="modal-backdrop" onClick={() => setSettingsOpen(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-header">
              <h2>Налаштування</h2>
              <button
                className="link-btn"
                onClick={() => setSettingsOpen(false)}
              >
                ✕
              </button>
            </div>
            <div className="tabs">
              <button
                className={settingsTab === "updates" ? "active" : ""}
                onClick={() => setSettingsTab("updates")}
              >
                Оновлення
              </button>
              <button
                className={settingsTab === "about" ? "active" : ""}
                onClick={() => setSettingsTab("about")}
              >
                Про програму
              </button>
            </div>
            <div className="modal-body">
              {settingsTab === "updates" && (
                <div className="settings-section">
                  <p>
                    yt-dlp: <strong>{ytdlp?.bundled_version || "…"}</strong>
                    {ytdlp?.latest_version &&
                      ytdlp.latest_version !== ytdlp.bundled_version && (
                        <>
                          {" "}
                          — доступна версія{" "}
                          <strong>{ytdlp.latest_version}</strong>
                        </>
                      )}
                  </p>
                  <button
                    className="primary"
                    onClick={handleCheckAppUpdate}
                    disabled={checkingAppUpdate}
                  >
                    {checkingAppUpdate ? "Перевірка..." : "Оновити програму"}
                  </button>
                  <button className="primary" onClick={handleCheckYtdlpUpdate}>
                    Оновити yt-dlp
                  </button>
                  {appUpdateStatus && (
                    <p className="status-line">{appUpdateStatus}</p>
                  )}
                </div>
              )}
              {settingsTab === "about" && (
                <div className="settings-section">
                  <p>
                    <strong>VideoGrab</strong>
                    {appVersion ? ` ${appVersion}` : ""}
                  </p>
                  <p className="muted">
                    Завантажуйте відео та аудіо з YouTube у форматі MP4.
                  </p>
                </div>
              )}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
