#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const os = require("os");
const { execSync, spawn } = require("child_process");

// cli.js lives in the package root (sibling of package.json) both for
// local installs (node_modules/pyielink/cli.js) and global installs.
const PACKAGE_DIR = __dirname;
const PACKAGE_JSON = path.join(PACKAGE_DIR, "package.json");
const pkg = require(PACKAGE_JSON);

const isWindows = process.platform === "win32";
const isUnix = !isWindows;

const PYIELINK_DIR_NAME = ".pyielink";

// Path to the Rust binary bundled inside the npm package (bin/pyielink[.exe]).
function getBundledBinaryPath() {
  return isWindows
    ? path.join(PACKAGE_DIR, "bin", "pyielink.exe")
    : path.join(PACKAGE_DIR, "bin", "pyielink");
}

// Persistent install location (~/.pyielink or %APPDATA%/.pyielink).
function getPyielinkDir() {
  const home = os.homedir();
  if (isWindows) {
    const appData = process.env.APPDATA || os.homedir();
    return path.join(appData, PYIELINK_DIR_NAME);
  }
  return path.join(os.homedir(), PYIELINK_DIR_NAME);
}

// Prefer the bundled binary; fall back to the persistent install location.
function getPyielinkBinaryPath() {
  const bundled = getBundledBinaryPath();
  if (fs.existsSync(bundled)) return bundled;
  const dir = getPyielinkDir();
  return isWindows ? path.join(dir, "pyielink.exe") : path.join(dir, "pyielink");
}

function getConfigPath() {
  const dir = getPyielinkDir();
  return path.join(dir, "config.json");
}

function ensureDirectory(dir) {
  if (!fs.existsSync(dir)) {
    fs.mkdirSync(dir, { recursive: true });
  }
}

function loadConfig() {
  const configPath = getConfigPath();
  if (fs.existsSync(configPath)) {
    try {
      const data = fs.readFileSync(configPath, "utf8");
      return JSON.parse(data);
    } catch {
      return null;
    }
  }
  return null;
}

function saveConfig(config) {
  const dir = getPyielinkDir();
  ensureDirectory(dir);
  const configPath = getConfigPath();
  fs.writeFileSync(configPath, JSON.stringify(config, null, 2), "utf8");
}

// ── Download binary from GitHub releases ──

function downloadBinary(url, dest, callback) {
  const http = require("https");
  const file = fs.createWriteStream(dest);

  http.get(url, (response) => {
    if (response.statusCode >= 300 && response.statusCode < 400 && response.headers.location) {
      file.close();
      fs.unlinkSync(dest);
      return downloadBinary(response.headers.location, dest, callback);
    }

    if (response.statusCode !== 200) {
      file.close();
      try { fs.unlinkSync(dest); } catch {}
      return callback(new Error(`HTTP ${response.statusCode}`));
    }

    let downloaded = 0;
    const total = parseInt(response.headers["content-length"], 10);

    response.on("data", (chunk) => {
      downloaded += chunk.length;
      if (total) {
        const pct = Math.round((downloaded / total) * 100);
        process.stdout.write(`\r  Downloading... ${pct}%`);
      }
    });

    response.pipe(file);

    file.on("finish", () => {
      file.close();
      process.stdout.write("\r  Download complete.      \n");
      callback(null, dest);
    });
  }).on("error", (err) => {
    file.close();
    try { fs.unlinkSync(dest); } catch {}
    callback(err);
  });
}

// Get the GitHub repo info from package.json
function getGitHubRepo() {
  const repoUrl = pkg.repository.url;
  // Handle both git:// and https:// URLs
  const match = repoUrl.match(/github\.com[:/]([^/]+)\/([^"]+)/);
  if (match) {
    return { owner: match[1], repo: match[2] };
  }
  return null;
}

// ── Maybe download binary if not already installed ──

async function maybeDownloadBinary() {
  const binaryPath = getPyielinkBinaryPath();
  const config = loadConfig();

  // If a bundled binary ships with the package, use it directly.
  const bundled = getBundledBinaryPath();
  if (fs.existsSync(bundled)) {
    // Verify it runs, then ensure it's available at the resolved path.
    try {
      execSync(`${bundled} --version`, { stdio: "pipe" }).toString().trim();
      // Copy bundled binary to the persistent location if needed.
      if (binaryPath !== bundled && !fs.existsSync(binaryPath)) {
        ensureDirectory(path.dirname(binaryPath));
        fs.copyFileSync(bundled, binaryPath);
        fs.chmodSync(binaryPath, "755");
      }
      return true;
    } catch {
      // Bundled binary broken; fall through to download.
    }
  }

  // If binary exists and config exists, we're good
  if (fs.existsSync(binaryPath) && config) {
    // Verify the binary works by checking version
    try {
      const output = execSync(`${binaryPath} --version`, { stdio: "pipe" })
        .toString()
        .trim();
      // If binary reports a version, we're set
      return true;
    } catch {
      // Binary exists but doesn't work; remove and re-download
      fs.unlinkSync(binaryPath);
    }
  }

  // Build download URL from GitHub repo
  const github = getGitHubRepo();
  if (!github) {
    process.stdout.write("  [error] Could not determine GitHub repo from package.json\n");
    process.exit(1);
  }

  const { owner, repo } = github;
  const version = pkg.version;
  const platform = isWindows ? "windows" : isUnix ? "linux" : "darwin";
  const arch = isWindows || isUnix ? "x64" : "arm64";
  const ext = isWindows ? ".exe" : "";

  // GitHub API: latest release asset
  const apiUrl = `https://api.github.com/repos/${owner}/${repo}/releases/latest`;

  process.stdout.write("  [..] Checking for Pyielink " + version + " release...\n");

  return new Promise((resolve, reject) => {
    // Fetch latest release from GitHub
    const http = require("https");
    const req = http.get(apiUrl, (response) => {
      let data = "";

      response.on("data", (chunk) => {
        data += chunk;
      });

      response.on("end", () => {
        try {
          const release = JSON.parse(data);
          // Find the asset for our platform
          let assetUrl = null;
          let assetName = null;

          for (const a of release.assets) {
            const aName = a.name.toLowerCase();
            const isWin = a.name.includes(".exe") && platform === "windows";
            const isWinX64 = a.name.includes("x64") && !a.name.includes(".exe") && platform === "windows";
            const isLinuxX64 = a.name.includes("x64") && platform === "linux";
            const isDarwinArm = a.name.includes("arm64") && platform === "darwin";
            const isDarwinX64 = a.name.includes("x64") && !a.name.includes("arm64") && platform === "darwin";

            if ((platform === "windows" && isWinX64) ||
                (platform === "linux" && isLinuxX64) ||
                (platform === "darwin" && isDarwinArm) ||
                (platform === "darwin" && isDarwinX64)) {
              assetUrl = a.browser_download_url;
              assetName = a.name;
              break;
            }
          }

          if (!assetUrl) {
            process.stdout.write("  [error] No suitable asset found for this platform in the latest release.\n");
            process.stdout.write("Available assets:\n");
            for (const a of release.assets) {
              process.stdout.write("  - " + a.name + "\n");
            }
            reject(new Error("No suitable asset found"));
            return;
          }

          process.stdout.write("  [ok] Found " + assetName + ".\n");

          // Download the asset
          process.stdout.write("  [..] Downloading Pyielink " + version + " (" + platform + " " + arch + ")...\n");

          downloadBinary(assetUrl, path.join(os.homedir(), "pyielink-" + platform + "-" + arch + ext), (err, downloadedPath) => {
            if (err) {
              process.stdout.write("  [error] Failed to download binary.\n");
              reject(err);
              return;
            }

            // Move to persistent location
            ensureDirectory(path.dirname(binaryPath));
            fs.copyFileSync(downloadedPath, binaryPath);
            fs.chmodSync(binaryPath, "755");

            // Save config
            saveConfig({
              firstRun: true,
              downloaded: true,
              version: version
            });

            process.stdout.write("  [ok] Pyielink " + version + " installed.\n");
            resolve(true);
          });
        } catch (e) {
          process.stdout.write("  [error] Failed to fetch GitHub release.\n");
          reject(e);
        }
      });
    });

    req.on("error", (e) => {
      process.stdout.write("  [error] Network error fetching GitHub release.\n");
      reject(e);
    });
  });
}

// ── Run the Rust binary ──

function runPyielink(args) {
  const binaryPath = getPyielinkBinaryPath();

  if (!fs.existsSync(binaryPath)) {
    process.stdout.write("  [error] Pyielink binary not found. ");
    process.stdout.write("First run will download it automatically.\n");
    process.exit(1);
  }

  try {
    // Set PYIELINK_HOME and point the data layer at the package dir.
    // The Rust binary looks for <PYIELINK_DATALAYER>/datalayer/src/server.js,
    // so this must be the package root (which contains datalayer/).
    const env = {
      ...process.env,
      PYIELINK_HOME: getPyielinkDir(),
      PYIELINK_DATALAYER: PACKAGE_DIR,
    };
    // Forward all CLI args to the Rust binary. Quote each token so paths
    // and addresses with special characters are passed through intact.
    const cmd = [binaryPath, ...args].map((a) => `"${a}"`).join(" ");
    execSync(cmd, {
      cwd: process.cwd(),
      env,
      stdio: "inherit"
    });
  } catch (e) {
    process.stdout.write(`\n  [error] Pyielink exited with code ${e.status || 1}\n`);
    process.exit(e.status || 1);
  }
}

// ── CLI Handling ──

const args = process.argv.slice(2);

if (args.includes("--repl")) {
  process.env.PYIELINK_REPL = "1";
}

if (args[0] === "--help" || args[0] === "-h") {
  console.log(`
Pyielink remote access framework v` + pkg.version + `

Usage:
  pyielink <user@ip>               Connect to host (GUI mode)
  pyielink --repl user@ip          REPL terminal mode
  pyielink enable                  Enable host for connections
  pyielink enable --all            Enable host for any IP
  pyielink enable --whitelist IP   Allow specific IP only
  pyielink whitelist add IP        Add IP to whitelist
  pyielink whitelist remove IP     Remove IP from whitelist

First run: npm install -g pyielink
  This will download the Rust binary from GitHub and set up configuration.

After installation, run:
  pyielink user@192.168.1.100    # Connect GUI
  pyielink --repl user@192.168.1.100  # Connect REPL
  pyielink enable                 # Enable host on first run
`);
  process.exit(0);
}

// ── Main ──

(async () => {
  process.stdout.write("Pyielink " + pkg.version + "\n\n");

  // Step 1: Ensure binary is available (use bundled or download if needed)
  await maybeDownloadBinary();

  // Step 2: Run the Rust binary with our configured env
  runPyielink(args);
})();
