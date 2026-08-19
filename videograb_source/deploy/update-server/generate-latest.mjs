#!/usr/bin/env node
// generate-latest.mjs — генерує latest.json у форматі Tauri v2 Updater.
//
// Використання:
//   node generate-latest.mjs <каталог-артефактів> <вихід/latest.json> <опис-релізу>
//
// Каталог артефактів має містити файли з .sig-підписами:
//   VideoGrab_<версія>_x64-setup.nsis.zip + .sig   (Windows)
//   VideoGrab_<версія>_x64_en-US.msi.zip  + .sig   (Windows MSI)
//   VideoGrab_<версія>_aarch64.dmg.zip    + .sig   (macOS universal)
//   VideoGrab_<версія>_amd64.AppImage     + .sig   (Linux AppImage)
//   VideoGrab_<версія>_amd64.deb          + .sig   (Linux DEB)
//
// У поля url вказано відносний шлях; залиште baseUrl як
// https://ваш-домен/updates/ і підставте його перед завантаженням,
// або запустіть script з --base-url=https://ваш-домен/updates/.

import fs from "node:fs";
import path from "node:path";

const BASE_URL = process.argv.includes("--base-url=")
  ? process.argv
      .find((a) => a.startsWith("--base-url="))
      .slice("--base-url=".length)
  : "https://ваш-домен/updates/";

const args = process.argv.slice(2).filter((a) => !a.startsWith("--"));
if (args.length < 3) {
  console.error(
    "Використання: node generate-latest.mjs <artifacts-dir> <out/latest.json> <release notes>"
  );
  process.exit(1);
}

const [artifactsDir, outFile, notes] = args;

const files = new Set(fs.readdirSync(artifactsDir));

function findUrl(patterns) {
  for (const p of patterns) {
    if (files.has(p)) {
      const sig = `${p}.sig`;
      if (!files.has(sig)) {
        console.warn(`Попередження: для ${p} відсутній підпис ${sig}`);
      }
      return `${BASE_URL}${p}`;
    }
  }
  return null;
}

const version = path.basename(path.resolve(artifactsDir));

const latest = {
  version,
  notes,
  pub_date: new Date().toISOString(),
  platforms: {
    "windows-x86_64": {
      signature: readSig(findUrl([
        `VideoGrab_${version}_x64-setup.nsis.zip`,
        `VideoGrab_${version}_x64_en-US.msi.zip`,
      ])),
      url: findUrl([
        `VideoGrab_${version}_x64-setup.nsis.zip`,
        `VideoGrab_${version}_x64_en-US.msi.zip`,
      ]),
    },
    "darwin-x86_64": {
      signature: readSig(findUrl([`VideoGrab_${version}_x86_64.dmg.zip`])),
      url: findUrl([`VideoGrab_${version}_x86_64.dmg.zip`]),
    },
    "darwin-aarch64": {
      signature: readSig(findUrl([
        `VideoGrab_${version}_aarch64.dmg.zip`,
        `VideoGrab_${version}_universal.dmg.zip`,
      ])),
      url: findUrl([
        `VideoGrab_${version}_aarch64.dmg.zip`,
        `VideoGrab_${version}_universal.dmg.zip`,
      ]),
    },
    "linux-x86_64": {
      signature: readSig(findUrl([`VideoGrab_${version}_amd64.AppImage`])),
      url: findUrl([`VideoGrab_${version}_amd64.AppImage`]),
    },
  },
};

function readSig(url) {
  if (!url) return "";
  const file = path.join(artifactsDir, path.basename(url)) + ".sig";
  return fs.existsSync(file) ? fs.readFileSync(file, "utf8").trim() : "";
}

fs.mkdirSync(path.dirname(path.resolve(outFile)), { recursive: true });
fs.writeFileSync(path.resolve(outFile), JSON.stringify(latest, null, 2));
console.log(`latest.json для версії ${version} створено: ${path.resolve(outFile)}`);
console.log("Перед завантаженням на сервер замініть BASE_URL у сгенерованому файлі!");
