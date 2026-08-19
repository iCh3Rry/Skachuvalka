# Сервер оновлень VideoGrab

Цей каталог містить шаблон для розповсюдження оновлень програми VideoGrab
«по повітрю» через офіційний плагін self-update фреймворку Tauri v2.

## Як це працює

Програма VideoGrab має вбудований механізм перевірки оновлень
(`tauri-plugin-updater`). При натисканні кнопки **«Оновити програму»** (а також
автоматично за бажанням) вона звертається до HTTPS-ендпоінту, вказаного у
`tauri.conf.json` у полі `plugins.updater.endpoints`, і порівнює версію з
файлом `latest.json`. Якщо версія новіша — пропонує завантажити й
установити оновлення; підпис артефактів перевіряється відкритим ключем,
тож підробити оновлення неможливо.

## Структура

```
update-server/
├── README.md          — ця інструкція
├── server.mjs         — простий HTTPS-сервер (Node.js, без залежностей)
├── generate-latest.mjs— генерує latest.json для всіх платформ
├── artifacts/         — сюди кладуть файли з кожного релізу
│   └── 1.0.0/
│       ├── VideoGrab_1.0.0_x64-setup.nsis.zip
│       ├── VideoGrab_1.0.0_x64-setup.nsis.zip.sig
│       ├── VideoGrab_1.0.0_x64_en-US.msi.zip
│       ├── VideoGrab_1.0.0_x64_en-US.msi.zip.sig
│       ├── VideoGrab_1.0.0_aarch64.dmg.zip
│       ├── VideoGrab_1.0.0_aarch64.dmg.zip.sig
│       ├── VideoGrab_1.0.0_amd64.deb
│       ├── VideoGrab_1.0.0_amd64.deb.sig
│       ├── VideoGrab_1.0.0_amd64.AppImage
│       └── VideoGrab_1.0.0_amd64.AppImage.sig
└── latest.json        — генерується автоматично
```

## Розгортання крок за кроком

### 1. Підготовка артефактів

Після кожного релізу (збірка через GitHub Actions або локально на Windows
для NSIS і на macOS для DMG) скопіюйте у каталог `artifacts/<версія>/`
**всі** артефакти та їхні `.sig`-файли, які генерує Tauri:

- Windows: `*.nsis.zip` + `.sig` (EXE-інсталятор), `*.msi.zip` + `.sig`
- macOS: `*.dmg.zip` + `.sig` (DMG для Intel + Apple Silicon, універсальний)
- Linux: `*.AppImage` + `.sig`, `*.deb` + `.sig`

Важливо: оновлювач шукає саме ці назви артефактів — вони мають формат
`{ProductName}_{version}_{arch}_{target}.{ext}.sig`.

### 2. Генерація latest.json

```bash
cd update-server
node generate-latest.mjs artifacts/1.0.0 latest.json "Опис оновлення 1.0.0"
```

Команда створює `latest.json` з коректним форматом Tauri v2
(платформи `windows-x86_64`, `darwin-x86_64`, `darwin-aarch64`,
`linux-x86_64`) та підписами.

### 3. Запуск сервера

```bash
cd update-server
node server.mjs
```

Сервер роздає `latest.json` та артефакти по HTTPS. У продуктиві замініть
`server.mjs` на будь-який статичний HTTPS-хостинг (Nginx, Cloudflare,
GitHub Pages через скрипт генерації у CI тощо) — достатньо, щоб
`https://ваш-домен/updates/latest.json` віддавав файл з правильним
`Content-Type: application/json` і CORS-заголовком
`Access-Control-Allow-Origin: *`.

### 4. Налаштування програми

У `videograb/src-tauri/tauri.conf.json` замініть `endpoints`:

```json
"endpoints": [
  "https://ваш-домен/updates/latest.json"
]
```

Відкритий ключ `pubkey` у тому ж файлі вже налаштований на ключі,
згенеровані в цьому репозиторії (`sign/key.rsa`). **Зберігайте
`sign/key.rsa` (приватний ключ) у таємниці** — під ним підписуються всі
оновлення. Публічний ключ можна поширювати вільно.

### 5. Процес випуску оновлення

1. Змінити `version` у `tauri.conf.json` (наприклад, `1.0.1`).
2. Зібрати артефакти на GitHub Actions (Windows/macOS) і локально або в CI
   (Linux).
3. Скопіювати артефакти + `.sig` у `artifacts/1.0.1/`.
4. `node generate-latest.mjs artifacts/1.0.1 latest.json "…"`.
5. Залити `latest.json` та артефакти на сервер.

Надалі кожен користувач, натиснувши **«Оновити програму»**, побачить
пропозицію встановити версію 1.0.1.

## Якщо вашого сервера ще немає

Найшвидший варіант без власного сервера — GitHub Releases:

```json
"endpoints": [
  "https://github.com/<ваш-логін>/videograb-releases/releases/latest/download/latest.json"
]
```

І завантажувати `latest.json` + артефакти як ассети кожного релізу.
GitHub автоматично обслуговує HTTPS.
