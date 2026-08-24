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
  const common = [
    // NOTE: all input options (and -window_title) MUST come before -i,
    // otherwise ffplay parses the title string as the input filename.
    "-fflags", "+nobuffer",
    "-probesize", "32",
    "-analyzeduration", "0",
    "-window_title", "Pyielink — Remote Screen",
    "-loglevel", "error",
    "-nostats",
    "-i", "pipe:0",
  ];
  let proc;
  let kind;
  try {
    proc = spawn("ffplay", common, { stdio: ["pipe", "ignore", "inherit"] });
    kind = "ffplay";
  } catch {
    proc = spawn("ffmpeg", [...common, "-f", "sdl2", "Pyielink — Remote Screen"], {
      stdio: ["pipe", "ignore", "inherit"],
    });
    kind = "ffmpeg-sdl2";
  }
  proc.stdin.on("drain", () => {
    if (activeWs && activeWs.readyState === WebSocket.OPEN) activeWs.resume();
  });
  proc.on("error", (e) => log(`viewer error (${kind}): ${e.message}`));
  proc.on("close", (code) => {
    log(`viewer (${kind}) exited (code ${code})`);
    if (viewer?.proc === proc) viewer = null;
  });
  viewer = { proc, kind };
  log(`viewer started: ${kind}`);
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
      } else {
        log(`unexpected ack: ${ack.trim()}`);
        ws.close();
      }
      return;
    }
    reader.feed(data);
  });

  ws.on("close", (code) => {
    if (authed) log(`data layer closed (code ${code})`);
    scheduleReconnect(`closed ${code}`);
  });

  ws.on("error", (err) => {
    if (!authed) log(`handshake failed: ${err.message}`);
    scheduleReconnect(err.code || "error");
  });
}

process.on("SIGINT", () => process.exit(0));
process.on("uncaughtException", (e) => log(`UNCAUGHT: ${e.stack || e}`));

connect();
