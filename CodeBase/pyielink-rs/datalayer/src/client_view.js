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
import { existsSync, openSync, writeSync, closeSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

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

// ---- mux framing (mirrors datalayer/src/mux.js) ----------------------------
const HEADER = 5;
const CH_CONTROL = 0x01;
const CH_INPUT = 0x02;
const CH_VIDEO = 0x03;

class MuxReader {
  constructor(onFrame) {
    this.buf = Buffer.alloc(0);
    this.onFrame = onFrame;
  }
  feed(chunk) {
    this.buf = this.buf.length ? Buffer.concat([this.buf, chunk]) : chunk;
    for (;;) {
      if (this.buf.length < HEADER) return;
      const channel = this.buf.readUInt8(0);
      const len = this.buf.readUInt32BE(1);
      if (this.buf.length < HEADER + len) return;
      const payload = this.buf.subarray(HEADER, HEADER + len);
      this.buf = this.buf.subarray(HEADER + len);
      try {
        this.onFrame(channel, payload);
      } catch (e) {
        log(`frame handler error: ${e.message}`);
      }
    }
  }
}

function frame(channel, payload) {
  const body = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
  const out = Buffer.allocUnsafe(HEADER + body.length);
  out.writeUInt8(channel, 0);
  out.writeUInt32BE(body.length, 1);
  body.copy(out, HEADER);
  return out;
}

// ---- viewer lifecycle ------------------------------------------------------
// Prefer ffplay (ships with ffmpeg full builds, opens its own window).
// Fall back to `ffmpeg -f sdl2`. Both take MPEG-TS on stdin.
let viewer = null;

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
  const ffLog = openSync(process.env.TEMP + "\\pyielink-ffplay.log", "a");
  try { writeSync(ffLog, `\n==== viewer spawn ${new Date().toISOString()} ====\n`); } catch {}
  const common = [
    // NOTE: all input options (and -window_title) MUST come before -i,
    // otherwise ffplay parses them as the input filename.
    "-fflags", "+nobuffer",
    "-probesize", "32",
    "-analyzeduration", "0",
    "-window_title", "Pyielink - Remote Screen",
    "-loglevel", "error",
    "-nostats",
    "-i", "pipe:0",
  ];
  try {
    proc = spawn("ffplay", common, { stdio: ["pipe", "ignore", ffLog] });
    kind = "ffplay";
  } catch {
    proc = spawn("ffmpeg", [...common, "-f", "sdl2", "Pyielink - Remote Screen"], {
      stdio: ["pipe", "ignore", ffLog],
    });
    kind = "ffmpeg-sdl2";
  }
  proc.stdin.on("drain", () => {
    if (activeWs && activeWs.readyState === WebSocket.OPEN) activeWs.resume();
  });
  proc.on("error", (e) => log(`viewer error (${kind}): ${e.message}`));
  proc.on("close", (code) => {
    log(`viewer (${kind}) exited (code ${code})`);
    try {
      writeSync(ffLog, `==== viewer exited code=${code} ${new Date().toISOString()} ====\n`);
      closeSync(ffLog);
    } catch {}
    if (viewer?.proc === proc) viewer = null;
  });
  viewer = { proc, kind };
  log(`viewer started: ${kind}`);
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
  const trace = process.env.TEMP + "\\pyielink-seticon.log";
  try {
    spawn("powershell", ["-NoProfile", "-ExecutionPolicy", "Bypass",
      "-File", ps1, "-ProcName", "ffplay", "-Image", pngPath, "-Trace", trace],
      { stdio: "ignore" });
  } catch (e) {
    log(`icon spawn failed: ${e.message}`);
  }
}

// ---- input relay -----------------------------------------------------------
// input_hook.ps1 (parked, detached) watches the player window and fires
// normalized mouse/key events at a localhost UDP socket; we forward them
// onto the INPUT channel. One helper per viewer window: kill any previous
// instance before spawning a new one so reconnects never double-hook.
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

function startInputRelay(ws) {
  stopInputRelay();
  const ps1 = fileURLToPath(new URL("./assets/input_hook.ps1", import.meta.url));
  if (!existsSync(ps1)) {
    log("input helper missing, remote control disabled");
    return;
  }
  const sock = dgram.createSocket("udp4");
  sock.on("message", (buf) => {
    const cur = activeWs;
    if (cur && cur.readyState === WebSocket.OPEN) {
      try { cur.send(frame(CH_INPUT, buf)); } catch {}
    }
  });
  sock.on("error", (e) => log(`input udp error: ${e.message}`));
  sock.bind(0, "127.0.0.1", () => {
    const port = sock.address().port;
    const helper = spawn(
      "powershell",
      ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", ps1,
       "-ProcName", "ffplay", "-UdpPort", String(port),
       "-Trace", process.env.TEMP + "\\pyielink-hook.log"],
      { stdio: "ignore" }
    );
    inputHelper = helper;
    inputUdp = sock;
    log(`input relay up (udp:${port}, pid:${helper.pid})`);
    helper.on("exit", (c) => log(`input helper exited (code ${c})`));
  });
}

// ---- session ---------------------------------------------------------------
let activeWs = null;
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

function connect() {
  attempt += 1;
  const url = `ws://${args.host}:${args.port}/`;
  log(`connecting (attempt ${attempt}/${MAX_ATTEMPTS}) …`);
  const ws = new WebSocket(url);
  activeWs = ws;

  let authed = false;
  const reader = new MuxReader((channel, payload) => {
    if (!authed) return;
    if (channel === CH_CONTROL) {
      if (payload.toString("utf8") === "PING") {
        ws.send(frame(CH_CONTROL, "PONG"));
      }
      return;
    }
    if (channel === CH_VIDEO && viewer) {
      // Backpressure-aware pipe into the player: pause the socket while the
      // OS pipe is saturated instead of buffering without bound.
      const ok = viewer.proc.stdin.write(payload);
      if (!ok && ws.readyState === WebSocket.OPEN) ws.pause();
    }
  });

  ws.on("open", () => {
    ws.send(JSON.stringify({ k: args.key }));
  });

  ws.on("message", (data, isBinary) => {
    if (!isBinary) {
      const ack = data.toString("utf8");
      if (ack.includes('"ok":true')) {
        authed = true;
        attempt = 0;
        log("data channel up");
        ensureViewer();
        ws.send(
          frame(
            CH_VIDEO,
            JSON.stringify({ t: "video_start", monitor_index: 0, offset_x: 0, offset_y: 0, width: 0, height: 0 })
          )
        );
        ws.send(frame(CH_INPUT, JSON.stringify({ t: "input_start" })));
        startInputRelay(ws);
      } else {
        log(`unexpected ack: ${ack.trim()}`);
        ws.close();
      }
      return;
    }
    reader.feed(data);
  });

  ws.on("close", (code) => {
    if (authed) {
      log(`data layer closed (code ${code})`);
      try { ws.send(frame(CH_INPUT, JSON.stringify({ t: "input_stop" }))); } catch {}
      stopInputRelay();
    }
    scheduleReconnect(`closed ${code}`);
  });

  ws.on("error", (err) => {
    if (!authed) log(`handshake failed: ${err.message}`);
    scheduleReconnect(err.code || "error");
  });
}

process.on("SIGINT", () => {
  stopInputRelay();
  process.exit(0);
});
process.on("exit", () => stopInputRelay());
process.on("uncaughtException", (e) => log(`UNCAUGHT: ${e.stack || e}`));

connect();
