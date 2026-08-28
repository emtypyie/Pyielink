// Pyielink client-side video viewer (Node).
//
// Replaces the old in-Rust data path for GUI sessions: connects to the
// session's data layer over WebSocket, authenticates with the session key,
// starts the remote screen stream, and pipes MPEG-TS straight into ffplay,
// which renders the actual window. Only the bootstrap handshake stays in
// Rust; everything past AUTH_OK lives here.
//
// Usage: node client_view.js --host <ip> --port <dataPort> --key <sessionKey>

import { spawn } from "child_process";
import process from "node:process";
import dgram from "node:dgram";
import { existsSync, openSync, writeSync, closeSync, appendFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { Mux, CHANNELS } from "./mux.js";
import { startClientRtc, applyClientSignal } from "./rtc.js";
const LATENCY_CSV = path.join(process.env.TEMP || process.env.TMPDIR || ".", "pyielink-latency.csv");
function latencyLog(stage, ms, extra="") {
  try { appendFileSync(LATENCY_CSV, `${Date.now()},${stage},${ms.toFixed(2)},${extra}\n`); } catch {}
}

const require = createRequire(import.meta.url);
const WebSocket = require("ws");

function log(msg) {
  console.error(`[pyielink-view] ${msg}`);
}

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 2) {
    out[argv[i]?.replace(/^--/, "")] = argv[i + 1];
  }
  return out;
}

const args = parseArgs(process.argv.slice(2));
if (!args.host || !args.port || !args.key) {
  log("usage: client_view.js --host <ip> --port <p> --key <k>");
  process.exit(2);
}

const MAX_ATTEMPTS = 5;
const RECONNECT_DELAY_MS = 2000;

// ---- viewer lifecycle ------------------------------------------------------
// Prefer ffplay (ships with ffmpeg full builds, opens its own window).
// Fall back to `ffmpeg -f sdl2`. Both take MPEG-TS on stdin.
let viewer = null;
let mux = null;
let rtcPc = null;
let rtcStarted = false;
let activeWs = null;
let lastFps = 0;          // displayed decode fps (progress lines / sec)
let ffplayTicks = 0;      // ffplay progress lines seen (1 per rendered frame)
let lastFpsTick = 0;      // snapshot for per-second delta
let fpsTimer = null;      // 1Hz fps display ticker
let viewerRestarts = 0;   // bounded auto-restart after a viewer crash
let hwaccelDisabled = false; // set if GPU decode fails to open the pipe
const MAX_VIEWER_RESTARTS = 5;

// Pick a GPU decoder for the viewer. `-hwaccel auto` is NOT used: it fails to
// configure a decoder/filtergraph for a non-seekable pipe input. A specific
// backend (d3d11va on Windows) opens the pipe fine. If it still fails on a
// given machine, the stderr watcher flips hwaccelDisabled and we retry in
// software mode.
function pickHwaccel() {
  if (process.env.PYIELINK_HWACCEL) return ["-hwaccel", process.env.PYIELINK_HWACCEL];
  if (process.platform === "win32") return ["-hwaccel", "d3d11va"];
  if (process.platform === "darwin") return ["-hwaccel", "videotoolbox"];
  return []; // linux: software unless PYIELINK_HWACCEL set (vaapi/cuda/...)
}

function ensureViewer() {
  if (viewer) return;
  // Branding ships next to this script (datalayer/src/assets/).
  // PNG renders most reliably through System.Drawing; ICO is the fallback.
  const a = p => fileURLToPath(new URL(`./assets/${p}`, import.meta.url));
  const iconImg = existsSync(a("PyieLink1.png")) ? a("PyieLink1.png") : a("PyieLink.ico");
  let proc;
  let kind;
  // Player stderr goes to a rolling temp file - keeps field debugging
  // possible without spamming the console.
  const ffLog = openSync(path.join(process.env.TEMP || process.env.TMPDIR || ".", "pyielink-ffplay.log"), "a");
  try { writeSync(ffLog, `\n==== viewer spawn ${new Date().toISOString()} ====\n`); } catch {}
  // GPU decode where available (d3d11va on Windows — opens the pipe cleanly,
  // unlike `-hwaccel auto`). Falls back to software if it can't open.
  let hwaccelArgs = hwaccelDisabled ? [] : pickHwaccel();
  const common = [
    // Input options MUST come before -i; output options (-window_title) after.
    "-fflags", "+nobuffer",
    "-probesize", "100000",
    "-analyzeduration", "1000000",
    ...hwaccelArgs,
    "-f", "mpegts",
    "-i", "pipe:0",
    "-window_title", "Pyielink - Remote Screen",
    // NOTE: stats (not -nostats) so we can read the live decode fps; the
    // progress lines go to stderr which we parse below.
  ];
  try {
    proc = spawn("ffplay", common, { stdio: ["pipe", "ignore", "pipe"] });
    kind = "ffplay";
  } catch {
    proc = spawn("ffmpeg", [...common, "-f", "sdl2", "Pyielink - Remote Screen"], {
      stdio: ["pipe", "ignore", "pipe"],
    });
    kind = "ffmpeg-sdl2";
  }
  proc.stdin.on("drain", () => {
    try {
      if (activeWs) {
        if (activeWs._socket && activeWs._socket.resume) activeWs._socket.resume();
        else if (activeWs.resume) activeWs.resume();
      }
    } catch {}
  });
  proc.stdin.on("error", (e) => log(`viewer stdin error: ${e.message}`));
  proc.on("error", (e) => log(`viewer error (${kind}): ${e.message}`));
  // Capture ffplay stderr: mirror to the log file AND derive the live fps.
  // This ffmpeg build's stats line has no `fps=` field; it emits one line per
  // rendered frame ("  21.75 M-V: 0.063 fd= 29 ..."), so we count those.
  proc.stderr.on("data", (d) => {
    try { writeSync(ffLog, d); } catch {}
    const s = d.toString();
    // ffplay progress line (one per rendered frame). Format differs across
    // builds: old "21.75 M-V: 0.063 fd= 29 ..." vs new "nan : 0.000 fd= 0 ...".
    if (/^\s*[\d.nan+-]+\s*(?::|M-V:)\s*[\d.]+\s+fd=\s*\d+/.test(s)) ffplayTicks += 1;
    // GPU decode couldn't open the pipe → drop to software and respawn once.
    if (!hwaccelDisabled && /Failed to open file|configure filtergraph/.test(s)) {
      hwaccelDisabled = true;
      log("hwaccel decode failed to open pipe; restarting viewer in software mode");
      try { proc.kill("SIGKILL"); } catch {}
    }
  });
  proc.on("close", (code) => {
    log(`viewer (${kind}) exited (code ${code})`);
    try {
      writeSync(ffLog, `==== viewer exited code=${code} ${new Date().toISOString()} ====\n`);
      closeSync(ffLog);
    } catch {}
    if (viewer?.proc === proc) viewer = null;
    if (fpsTimer) { clearInterval(fpsTimer); fpsTimer = null; }
    // Stay alive across a network blip / viewer crash: if we're still
    // connected, restart the viewer and keep rendering the next frames.
    if (activeWs && activeWs.readyState === 1 && code !== 0 && viewerRestarts < MAX_VIEWER_RESTARTS) {
      viewerRestarts += 1;
      log(`restarting viewer (${viewerRestarts}/${MAX_VIEWER_RESTARTS})`);
      setTimeout(ensureViewer, 500);
    } else if (viewerRestarts >= MAX_VIEWER_RESTARTS) {
      log(`viewer restart cap reached; leaving it stopped`);
    }
  });
  viewer = { proc, kind };
  log(`viewer started: ${kind}`);
  // 1Hz fps readout (only while connected + a viewer is live).
  if (fpsTimer) clearInterval(fpsTimer);
  fpsTimer = setInterval(() => {
    if (viewer && activeWs && activeWs.readyState === 1) {
      const fps = ffplayTicks - lastFpsTick;
      lastFpsTick = ffplayTicks;
      if (fps > 0) lastFps = fps; // smooth: keep last good value across gaps
      log(`fps: ${lastFps.toFixed(1)}`);
      // Mirror FPS into the player window title so it's visible on-screen.
      if (process.platform === "win32") {
        try {
          const t = `Pyielink - Remote Screen (FPS: ${lastFps.toFixed(0)}${hwaccelDisabled ? "" : " GPU"})`;
          const ps1 = fileURLToPath(new URL("./assets/set_title.ps1", import.meta.url));
          spawn("powershell", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", ps1, "-ProcName", "ffplay", "-Title", t], { stdio: "ignore" }).unref();
        } catch {}
      }
    } else {
      lastFpsTick = ffplayTicks;
    }
  }, 1000);
  applyWindowIcon(iconImg);
}

// ffplay has no -window_icon option, so set the titlebar icon afterwards via
// a small PowerShell helper: it finds the player window through the process
// table, applies WM_SETICON repeatedly (SDL re-stomps early one-shots), then
// parks for the window's lifetime (HICONS die with their creator process).
function applyWindowIcon(pngPath) {
  const ps1 = fileURLToPath(new URL("./assets/set_icon.ps1", import.meta.url));
  if (!existsSync(ps1) || !existsSync(pngPath)) {
    log(`icon skip: ps1=${existsSync(ps1)} img=${existsSync(pngPath)}`);
    return;
  }
  const trace = path.join(process.env.TEMP || process.env.TMPDIR || ".", "pyielink-seticon.log");
  try {
    spawn("powershell", ["-NoProfile", "-ExecutionPolicy", "Bypass",
      "-File", ps1, "-ProcName", "ffplay", "-Image", pngPath, "-Trace", trace],
      { stdio: "ignore" });
  } catch (e) {
    log(`icon spawn failed: ${e.message}`);
  }
}

// ---- input relay -----------------------------------------------------------
// A platform capture helper watches the player window and fires normalized
// mouse/key events at a localhost UDP socket; we forward them onto the INPUT
// channel. One helper per viewer window: kill any previous instance before
// spawning a new one so reconnects never double-hook.
//
//   win32 : assets/input_hook.ps1 (low-level hooks) / crosshair.ps1 (overlay)
//   linux : assets/input_hook.py (X11 XRecord tap)
//   darwin: assets/input_hook.py (Quartz CGEvent tap)
let inputUdp = null;
let inputHelper = null;

function stopInputRelay() {
  if (inputHelper) {
    try { process.kill(inputHelper.pid); } catch {}
    try { spawn("taskkill", ["/PID", String(inputHelper.pid), "/T", "/F"], { stdio: "ignore" }).unref(); } catch {}
    inputHelper = null;
  }
  if (inputUdp) {
    try { inputUdp.close(); } catch {}
    inputUdp = null;
  }
}

// Resolve the capture helper for this platform. kind: "crosshair" | "input".
function captureHelper(kind) {
  const a = fileURLToPath(new URL("./assets/", import.meta.url));
  if (process.platform === "win32") {
    if (kind === "crosshair") {
      const cs = path.join(a, "crosshair.ps1");
      if (existsSync(cs)) return { cmd: "powershell", script: cs };
    }
    const ps1 = path.join(a, "input_hook.ps1");
    if (existsSync(ps1)) return { cmd: "powershell", script: ps1 };
    return null;
  }
  if (process.platform === "linux" || process.platform === "darwin") {
    const py = path.join(a, "input_hook.py");
    if (existsSync(py)) return { cmd: "python3", script: py };
    return null;
  }
  return null;
}

function buildHelperArgs(pick, port) {
  const trace = path.join(process.env.TEMP || process.env.TMPDIR || ".", "pyielink-hook.log");
  if (pick.script.endsWith(".ps1"))
    return ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", pick.script,
            "-ProcName", "ffplay", "-UdpPort", String(port), "-Trace", trace];
  return [pick.script, "--port", String(port), "--proc", "ffplay", "--trace", trace];
}

function startCaptureRelay(state, helperKind) {
  const pick = captureHelper(helperKind);
  if (!pick) {
    log("capture helper missing for " + process.platform + ", remote control disabled");
    return null;
  }
  const sock = dgram.createSocket("udp4");
  sock.on("message", (buf) => {
    if (mux) { try { mux.send(CHANNELS.INPUT, buf); } catch {} }
  });
  sock.on("error", (e) => log(`capture udp error: ${e.message}`));
  sock.bind(0, "127.0.0.1", () => {
    const port = sock.address().port;
    const helper = spawn(pick.cmd, buildHelperArgs(pick, port), { stdio: "ignore" });
    state.helper = helper;
    state.udp = sock;
    log(`capture relay up (${helperKind}, udp:${port}, pid:${helper.pid})`);
    helper.on("exit", (c) => {
      log(`capture helper exited (code ${c})`);
      if (c !== 0 && helperKind === "crosshair") {
        log("crosshair failed, falling back to hook");
        if (state.udp) { try { state.udp.close(); } catch {} state.udp = null; }
        startInputRelay();
      }
    });
  });
  return sock;
}

function startInputRelay() {
  stopInputRelay();
  startCaptureRelay({ get helper() { return inputHelper; }, set helper(v) { inputHelper = v; }, get udp() { return inputUdp; }, set udp(v) { inputUdp = v; } }, "input");
}

// ---- crosshair overlay (Option A: distinct double cursor) ------------------
// Transparent WPF window over ffplay's client area (Windows only). Red '+'
// follows the mouse instantly (local); the host's white arrow follows after
// latency in the video. On Linux/mac the crosshair overlay isn't available,
// so captureHelper("crosshair") falls through to the same input_hook.py path.
let crosshairUdp = null;
let crosshairHelper = null;

function stopCrosshairRelay() {
  if (crosshairHelper) {
    try { process.kill(crosshairHelper.pid); } catch {}
    try { spawn("taskkill", ["/PID", String(crosshairHelper.pid), "/T", "/F"], { stdio: "ignore" }).unref(); } catch {}
    crosshairHelper = null;
  }
  if (crosshairUdp) {
    try { crosshairUdp.close(); } catch {}
    crosshairUdp = null;
  }
}

function startCrosshairRelay() {
  stopCrosshairRelay();
  stopInputRelay();
  startCaptureRelay({ get helper() { return crosshairHelper; }, set helper(v) { crosshairHelper = v; }, get udp() { return crosshairUdp; }, set udp(v) { crosshairUdp = v; } }, "crosshair");
}

// ---- session ---------------------------------------------------------------
let attempt = 0;
let reconnectTimer = null;

function scheduleReconnect(reason) {
  if (reconnectTimer || process.exitCode) return;
  if (attempt >= MAX_ATTEMPTS) {
    log(`giving up after ${MAX_ATTEMPTS} attempts (${reason})`);
    process.exitCode = 1;
    return;
  }
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, RECONNECT_DELAY_MS);
}

function signalHost(msg) {
  if (activeWs && activeWs.readyState === 1) {
    try { activeWs.send(JSON.stringify(msg)); } catch {}
  }
}

function ensureRtc() {
  if (rtcStarted) return;
  rtcStarted = true;
  if (process.env.PYIELINK_TRANSPORT === "tcp") {
    log("[rtc] disabled (PYIELINK_TRANSPORT=tcp), using ws");
    return;
  }
  startClientRtc({ mux, onSignal: signalHost, log: (m) => log(m) })
    .then((pc) => { rtcPc = pc; })
    .catch((e) => log(`[rtc] init failed, ws fallback: ${e.message}`));
}

async function handleOffer(msg) {
  await ensureRtc();
  if (!rtcPc) return;
  await applyClientSignal(rtcPc, msg, signalHost, (m) => log(m));
}

function connect() {
  attempt += 1;
  const url = `ws://${args.host}:${args.port}/`;
  log(`connecting (attempt ${attempt}/${MAX_ATTEMPTS}) …`);
  const ws = new WebSocket(url);
  activeWs = ws;

  let authed = false;

  ws.on("open", () => {
    ws.send(JSON.stringify({ k: args.key }));
  });

  ws.on("message", (data, isBinary) => {
    if (isBinary) return; // mux frames handled by Mux's own listener
    const text = data.toString("utf8");
    let msg = null;
    try { msg = JSON.parse(text); } catch {}
    if (msg && msg.ok === true) {
      authed = true;
      attempt = 0;
      viewerRestarts = 0;
      log("data channel up");
      mux = new Mux(ws);
      mux.on(CHANNELS.CONTROL, (payload) => {
        if (payload.toString("utf8") === "PING") mux.send(CHANNELS.CONTROL, "PONG");
      });
      mux.on(CHANNELS.VIDEO, (payload) => {
        if (!viewer) return;
        try {
          const ok = viewer.proc.stdin.write(payload);
          // Only back-pressure the WebSocket when this channel is actually
          // riding it; if the data channel is attached, DC has its own buffering
          // and pausing the ws would stall CONTROL/heartbeat.
          const onWs = !mux.dcMap || !mux.dcMap.get(CHANNELS.VIDEO);
          if (!ok && onWs && ws.readyState === WebSocket.OPEN) {
            try {
              if (ws._socket && ws._socket.pause) ws._socket.pause();
              else if (ws.pause) ws.pause();
            } catch {}
          }
        } catch (e) { log(`viewer write error: ${e.message}`); }
      });
      ensureViewer();
      mux.send(CHANNELS.VIDEO, JSON.stringify({ t: "video_start", monitor_index: 0, offset_x: 0, offset_y: 0, width: 0, height: 0 }));
      mux.send(CHANNELS.INPUT, JSON.stringify({ t: "input_start" }));
      const noCross = args.nocrosshair !== undefined || args["no-crosshair"] !== undefined || args.nocrosshair === "" || process.env.PYIELINK_NO_CROSSHAIR === "1";
      if (noCross) startInputRelay();
      else startCrosshairRelay();
      ensureRtc();
    } else if (msg && msg.t === "rtc_offer") {
      handleOffer(msg);
    } else if (!authed) {
      log(`unexpected message: ${text.trim().slice(0, 80)}`);
    }
  });

  ws.on("close", (code) => {
    if (authed) {
      log(`data layer closed (code ${code})`);
      try { if (mux) mux.send(CHANNELS.INPUT, JSON.stringify({ t: "input_stop" })); } catch {}
      stopInputRelay();
      stopCrosshairRelay();
    }
    if (rtcPc) { try { rtcPc.close(); } catch {} rtcPc = null; rtcStarted = false; }
    scheduleReconnect(`closed ${code}`);
  });

  ws.on("error", (err) => {
    if (!authed) log(`handshake failed: ${err.message}`);
    scheduleReconnect(err.code || "error");
  });
}

process.on("SIGINT", () => {
  stopInputRelay();
  stopCrosshairRelay();
  process.exit(0);
});
process.on("exit", () => { stopInputRelay(); stopCrosshairRelay(); });
process.on("uncaughtException", (e) => log(`UNCAUGHT: ${e.stack || e}`));

connect();
