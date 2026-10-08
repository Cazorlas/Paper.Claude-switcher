// Downloads the prebuilt binary for this platform from the GitHub release
// that matches this package's version.
const fs = require("fs");
const path = require("path");
const https = require("https");
const { version } = require("./package.json");

const targets = {
  "win32-x64": "paper-claude-switch-windows-x64.exe",
  "darwin-x64": "paper-claude-switch-macos-x64",
  "darwin-arm64": "paper-claude-switch-macos-arm64",
  "linux-x64": "paper-claude-switch-linux-x64",
};
const key = `${process.platform}-${process.arch}`;
const asset = targets[key];
if (!asset) {
  console.error(`paper-claude-switch: no prebuilt binary for ${key}; build from source with cargo.`);
  process.exit(1);
}
const url = `https://github.com/Cazorlas/Paper.Claude-switcher/releases/download/v${version}/${asset}`;
const dest = path.join(__dirname, "bin", process.platform === "win32" ? "paper-claude-switch.exe" : "paper-claude-switch");

function get(u, redirects = 5) {
  https.get(u, { headers: { "User-Agent": "paper-claude-switch-installer" } }, (res) => {
    if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location && redirects > 0) {
      res.resume();
      return get(res.headers.location, redirects - 1);
    }
    if (res.statusCode !== 200) {
      console.error(`paper-claude-switch: download failed (${res.statusCode}) ${u}`);
      process.exit(1);
    }
    const tmp = dest + ".part";
    const out = fs.createWriteStream(tmp, { mode: 0o755 });
    res.pipe(out);
    out.on("finish", () => out.close(() => { fs.renameSync(tmp, dest); fs.chmodSync(dest, 0o755); }));
    out.on("error", (e) => { console.error(e.message); process.exit(1); });
  }).on("error", (e) => { console.error(e.message); process.exit(1); });
}
get(url);
