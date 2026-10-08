#!/usr/bin/env node
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawnSync, spawn } = require("child_process");

const isWin = process.platform === "win32";
const exe = path.join(__dirname, isWin ? "paper-claude-switch.exe" : "paper-claude-switch");
const current = require("../package.json").version;
const args = process.argv.slice(2);

function newer(a, b) {
  const pa = a.split(".").map(Number), pb = b.split(".").map(Number);
  for (let i = 0; i < 3; i++) if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) > (pb[i] || 0);
  return false;
}

function latestVersion() {
  const r = spawnSync("npm", ["view", "paper-claude-switch", "version"], { encoding: "utf8", shell: isWin });
  return r.status === 0 ? r.stdout.trim() : null;
}

// `self-update [--check]`: handled here because a running binary cannot be replaced
// (Windows locks it), and a background `auto` keeps it running.
if (args[0] === "self-update") {
  const latest = latestVersion();
  if (!latest) {
    console.error("paper-claude-switch: cannot reach the npm registry (is npm on PATH?)");
    process.exit(1);
  }
  if (!newer(latest, current)) {
    console.log(`paper-claude-switch ${current} is up to date`);
    process.exit(0);
  }
  console.log(`paper-claude-switch ${current} -> ${latest}`);
  if (args.includes("--check")) process.exit(0);

  const running = isWin
    ? spawnSync("tasklist", ["/FI", "IMAGENAME eq paper-claude-switch.exe", "/NH"], { encoding: "utf8" }).stdout.includes("paper-claude-switch.exe")
    : spawnSync("pgrep", ["-x", "paper-claude-switch"]).status === 0;
  if (running) {
    console.log("stopping the background auto so the binary can be replaced...");
    if (isWin) spawnSync("taskkill", ["/IM", "paper-claude-switch.exe", "/F"], { stdio: "ignore" });
    else spawnSync("pkill", ["-x", "paper-claude-switch"]);
  }
  const r = spawnSync("npm", ["i", "-g", "paper-claude-switch@latest"], { stdio: "inherit", shell: isWin });
  if (r.status !== 0) process.exit(r.status || 1);
  if (running) console.log("start `paper-claude-switch auto` again to resume automatic switching");
  process.exit(0);
}

// `uninstall [--purge | --keep-data]`: handled here so it works even when the binary is
// missing or blocked, and because a running binary cannot delete itself.
if (args[0] === "uninstall") {
  const dataDir = process.env.PAPER_CLAUDE_SWITCH_HOME || path.join(os.homedir(), ".paper-claude-switch");
  const finish = (wipe) => {
    if (isWin) spawnSync("taskkill", ["/IM", "paper-claude-switch.exe", "/F"], { stdio: "ignore" });
    else spawnSync("pkill", ["-x", "paper-claude-switch"]);
    if (isWin) {
      const vbs = path.join(process.env.APPDATA || "", "Microsoft", "Windows", "Start Menu", "Programs", "Startup", "paper-claude-switch-auto.vbs");
      try { fs.rmSync(vbs, { force: true }); } catch {}
    }
    if (wipe) {
      try { fs.rmSync(dataDir, { recursive: true, force: true }); console.log(`deleted ${dataDir}`); }
      catch (e) { console.error(`could not delete ${dataDir}: ${e.message}`); }
    } else {
      console.log(`kept your accounts and settings in ${dataDir}`);
    }
    const r = spawnSync("npm", ["rm", "-g", "paper-claude-switch"], { stdio: "inherit", shell: isWin });
    console.log(r.status === 0 ? "paper-claude-switch uninstalled" : "npm could not remove the package; run: npm rm -g paper-claude-switch");
    process.exit(r.status || 0);
  };
  if (args.includes("--purge")) finish(true);
  else if (args.includes("--keep-data") || !process.stdin.isTTY) finish(false);
  else {
    const rl = require("readline").createInterface({ input: process.stdin, output: process.stdout });
    rl.question(`Also delete your saved accounts and settings (${dataDir})? [y/N] `, (a) => {
      rl.close();
      finish(/^y(es)?$/i.test(a.trim()));
    });
  }
} else {
// npm may skip the postinstall script (allow-scripts); fetch the binary on first run instead.
if (!fs.existsSync(exe)) {
  const r = spawnSync(process.execPath, [path.join(__dirname, "..", "install.js")], { stdio: "inherit" });
  if (r.status !== 0 || !fs.existsSync(exe)) {
    console.error("paper-claude-switch: could not download the binary; see https://github.com/Cazorlas/Paper.Claude-switcher/releases");
    process.exit(1);
  }
}

// Once a day, look for a newer version in the background and print a hint; never installs by itself.
try {
  const cache = path.join(os.tmpdir(), "paper-claude-switch-update.json");
  let info = {};
  try { info = JSON.parse(fs.readFileSync(cache, "utf8")); } catch {}
  if (!info.at || Date.now() - info.at > 24 * 3600 * 1000) {
    const code = `const {execFileSync}=require("child_process");try{const v=execFileSync("npm",["view","paper-claude-switch","version"],{encoding:"utf8",shell:process.platform==="win32"}).trim();require("fs").writeFileSync(${JSON.stringify(cache)},JSON.stringify({at:Date.now(),latest:v}))}catch{}`;
    spawn(process.execPath, ["-e", code], { detached: true, stdio: "ignore" }).unref();
  }
  if (info.latest && newer(info.latest, current) && process.stderr.isTTY && !args.includes("--json") && !args.includes("--json-pretty") && args[0] !== "auto") {
    console.error(`paper-claude-switch ${info.latest} is available (you have ${current}); run: paper-claude-switch self-update`);
  }
} catch {}

const r = spawnSync(exe, args, { stdio: "inherit" });
if (r.error) {
  console.error(`paper-claude-switch: cannot run ${exe}: ${r.error.message}`);
  process.exit(1);
}
process.exit(r.status ?? 1);
}
