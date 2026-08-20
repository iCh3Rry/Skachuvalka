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

export default function App() {
  const [url, setUrl] = useState("");
  const [mode, setMode] = useState<Mode>("video");
  const [quality, setQuality] = useState("best");
  const [saveDir, setSaveDir] = useState("");
  const [queue, setQueue] = useState<DownloadItem[]>([]);
  const [ytdlp, setYtdlp] = useState<YtDlpStatus | null>(null);
  const [checkingAppUpdate, setCheckingAppUpdate] = useState(false);
  const [debugInfo, setDebugInfo] = useState<string | null>(null);

  async function fetchDebug() {
    try {
      setDebugInfo(JSON.stringify(await invoke("ytdlp_debug_info")));
    } catch (e) {
      setDebugInfo(JSON.stringify({ error: String(e) }));
    }
  }
  const [appUpdateStatus, setAppUpdateStatus] = useState("");

  useEffect(() => {
    let alive = true;
    listen<DownloadItem>("item-updated", (e) => {
      setQueue((q) =>
        q.map((item) => (item.id === e.payload.id ? e.payload : item))
      );
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
    if (dir) setSaveDir(Array.isArray(dir) ? dir[0] : dir);
  }

  async function handleDownload() {
    if (!url.trim()) return;
    try {
      await invoke("start_download", {
        url: url.trim(),
        mode,
        quality,
        saveDir: saveDir || (await homeDir()) + "Downloads",
      });
      setUrl("");
    } catch (e) {
      alert(`Помилка: ${e}`);
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
      <header>
        <h1>VideoGrab</h1>
        <p className="subtitle">
          Завантажуйте відео та аудіо з YouTube за допомогою yt-dlp
        </p>
        <div className="header-actions">
          <button
            className="secondary"
            onClick={handleCheckAppUpdate}
            disabled={checkingAppUpdate}
          >
            {checkingAppUpdate ? "Перевірка..." : "Оновити програму"}
          </button>
          <button className="secondary" onClick={handleCheckYtdlpUpdate}>
            Оновити yt-dlp
          </button>
        </div>
          {appUpdateStatus && <p className="status-line">{appUpdateStatus}</p>}
        {debugInfo && (
          <p className="version-line" style={{ color: "#a0aec0", fontSize: 11 }}>
            DEBUG: {debugInfo}{" "}
            <button className="link-btn" onClick={() => setDebugInfo(null)}>
              закрити
            </button>
          </p>
        )}
        {ytdlp && (
          <p className="version-line">
            yt-dlp: <strong>{ytdlp.bundled_version || "не встановлено"}</strong>
            {ytdlp.latest_version &&
              ytdlp.latest_version !== ytdlp.bundled_version && (
                <button className="link-btn" onClick={handleCheckYtdlpUpdate}>
                  {" "}
                  — доступна версія {ytdlp.latest_version}, натисніть, щоб
                  оновити
                </button>
              )}{" "}
            <button className="link-btn" onClick={() => fetchDebug()}>
              (debug)
            </button>
          </p>
        )}
      </header>

      <main>
        <section className="card">
          <h2>Нове завантаження</h2>
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
          {saveDir && <p className="path-line">Зберігати в: {saveDir}</p>}
          <button className="primary" onClick={handleDownload}>
            Завантажити
          </button>
        </section>

        <section className="card">
          <h2>Черга завантажень</h2>
          {queue.length === 0 && <p className="empty">Черга порожня</p>}
          {[...queue].reverse().map((item) => (
            <div key={item.id} className={`queue-item ${item.status}`}>
              <div className="qi-title">
                <span className="qi-name">{item.title || item.url}</span>
                <span className="qi-meta">
                  {item.mode === "video" ? "відео" : "аудіо"} · {item.quality}
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
                <span>
                  {item.status === "pending" && "Очікує"}
                  {item.status === "running" && "Завантаження..."}
                  {item.status === "done" && "Готово"}
                  {item.status === "error" && (item.error || "Помилка")}
                </span>
              </div>
            </div>
          ))}
        </section>
      </main>
    </div>
  );
}
