#!/usr/bin/env node
// Простий HTTPS-сервер для розповсюдження оновлень VideoGrab.
// Роздає latest.json і артефакти з каталога artifacts/.
//
// Використання:
//   node server.mjs [порт=4430] [cert.pem] [key.pem]
//
// Без сертифікатів сервер працює по HTTP (тільки для локального тесту;
// Tauri-оновлювач вимагає HTTPS у продакшені).

import fs from "node:fs";
import http from "node:http";
import https from "node:https";
import path from "node:path";

const port = Number(process.argv[2]) || 4430;
const cert = process.argv[3];
const key = process.argv[4];

const DIR = path.resolve(import.meta.dirname);

const MIME = {
  ".json": "application/json",
  ".zip": "application/zip",
  ".deb": "application/vnd.debian.binary-package",
  ".dmg": "application/x-apple-diskimage",
  ".AppImage": "application/x-executable",
  ".sig": "text/plain",
  ".exe": "application/x-msdownload",
  ".msi": "application/x-msi",
};

const server = http.createServer((req, res) => {
  const urlPath = path.normalize(decodeURIComponent(req.url));
  if (urlPath.includes("..")) {
    res.writeHead(403);
    return res.end("forbidden");
  }
  const file = path.join(DIR, urlPath);
  if (!fs.existsSync(file) || !fs.statSync(file).isFile()) {
    res.writeHead(404);
    return res.end("not found");
  }
  const ext = path.extname(file).toLowerCase();
  res.writeHead(200, {
    "Content-Type": MIME[ext] || "application/octet-stream",
    "Access-Control-Allow-Origin": "*",
  });
  fs.createReadStream(file).pipe(res);
});

if (cert && key) {
  https
    .createServer(
      { cert: fs.readFileSync(cert), key: fs.readFileSync(key) },
      server,
    )
    .listen(port, () => console.log(`HTTPS-сервер оновлень на порту ${port}`));
} else {
  server.listen(port, () =>
    console.log(
      `HTTP-сервер оновлень на порту ${port} (для продакшену підключіть TLS: node server.mjs ${port} cert.pem key.pem)`,
    ),
  );
}
