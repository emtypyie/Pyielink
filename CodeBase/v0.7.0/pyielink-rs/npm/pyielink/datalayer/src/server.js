import { readFileSync, rmSync, existsSync, appendFileSync } from "node:fs";
import process from "node:process";
import os from "node:os";
import path from "node:path";
import { WebSocketServer } from "ws";
import { Mux, keysMatch, CHANNELS } from "./mux.js";
import { Heartbeat } from "./heartbeat.js";
import { FileService } from "./files.js";
import { InputService } from "./input.js";
import { VideoService } from "./video.js";
import { AudioService } from "./audio.js";
import { startHostRtc, applyHostSignal } from "./rtc.js";

const _port = process.env.PYIELINK_DL_PORT || "unknown";
const LOGF = path.join(os.tmpdir(), `pyielink-dl-${_port}.log`);
function flog(...a) {
  try {
    appendFileSync(LOGF, a.map((x) => (x && x.stack) ? x.stack : String(x)).join(" ") + "\n");
  } catch {}
}
flog("=== server.js start pid=" + process.pid + " node=" + process.version + " ===");
process.on("uncaughtException", (e) => {
  flog("UNCAUGHT:", e && e.stack ? e.stack : e);
  console.error(`[pyielink-dl] UNCAUGHT: ${e && e.stack ? e.stack : e}`);
});
process.on("exit", (code, sig) => {
  flog("EXIT code=" + code + " signal=" + sig);
});
process.on("SIGTERM", () => flog("SIGTERM received"));
process.on("SIGINT", () => flog("SIGINT received"));

const DEFAULT_PORT = 4243;
const AUTH_WINDOW_MS = 10000;

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--port") {
      const n = Number(argv[i + 1]);
      if (!Number.isInteger(n) || n <= 0 || n > 65535) {
        console.error(`[pyielink-dl] bad --port value: ${argv[i + 1]}`);
        process.exit(2);
      }
      out.port = n;
      i++;
    }
  }
  return out;
}

function loadHandoff() {
  const path = process.env.PYIELINK_SESSION;
  if (!path || !existsSync(path)) {
    throw new Error("PYIELINK_SESSION handoff file missing or already consumed");
  }
  const raw = readFileSync(path, "utf8");
  try {
    rmSync(path, { force: true });
  } catch {}
  const [key, user, role] = raw.split(/\r?\n/);
  if (!key) throw new Error("handoff file has no session key");
  return { key, user: user || "", role: role === "admin" ? "admin" : "user" };
}

const args = parseArgs(process.argv.slice(2));
const port = args.port ?? (Number(process.env.PYIELINK_DL_PORT) || DEFAULT_PORT);

let session;
try {
  session = loadHandoff();
} catch (err) {
  console.error(`[pyielink-dl] fatal: ${err.message}`);
  process.exit(2);
}

const wss = new WebSocketServer({ port });
flog("listening on port " + port + " user=" + session.user + " role=" + session.role);
console.log(
  `[pyielink-dl] listening on ws://127.0.0.1:${port} user=${session.user} role=${session.role}`
);

wss.on("connection", (ws) => {
  flog("client connection received");
  console.log(`[pyielink-dl] client connection received`);
  let mux = null;
  let hb = null;
  let authed = false;
  let rtcPc = null;

  const authTimer = setTimeout(() => {
    if (!authed && ws.readyState === 1) ws.close(4001, "auth timeout");
  }, AUTH_WINDOW_MS);
  if (authTimer.unref) authTimer.unref();

  const sendSignal = (msg) => {
    if (ws.readyState === 1) ws.send(JSON.stringify(msg));
  };
  const handleSignal = (msg) => {
    if (!rtcPc) return;
    applyHostSignal(rtcPc, msg, (m) => flog("[rtc] host signal: " + m));
  };

  ws.on("message", (data, isBinary) => {
    if (authed) {
      // After auth: text = WebRTC signaling, binary = mux frames (handled by Mux).
      if (!isBinary) {
        try {
          const msg = JSON.parse(Buffer.from(data).toString("utf8"));
          if (msg && typeof msg.t === "string" && msg.t.startsWith("rtc_")) handleSignal(msg);
        } catch {}
      }
      return;
    }
    clearTimeout(authTimer);
    let hello = null;
    try {
      hello = JSON.parse(Buffer.from(data).toString("utf8"));
    } catch {
      hello = null;
    }
    if (!hello || typeof hello.k !== "string" || !keysMatch(hello.k, session.key)) {
      flog("session key mismatch - closing 4001 (got key len=" + (hello && hello.k ? hello.k.length : 0) + ")");
      console.warn("[pyielink-dl] session key mismatch - closing 4001");
      ws.close(4001, "bad session key");
      return;
    }
    authed = true;
    flog("client authenticated (" + session.user + ")");
    console.log(`[pyielink-dl] client authenticated (${session.user})`);
    ws.send(JSON.stringify({ ok: true, user: session.user, role: session.role }));
    mux = new Mux(ws);
    mux.on(CHANNELS.CONTROL, () => {});
    hb = new Heartbeat(mux, {
      onLost: () => {
        console.warn("[pyielink-dl] heartbeat lost - closing 4002");
        files.teardownAll();
        ws.close(4002, "heartbeat lost");
      },
      onRtt: (ms) => console.log(`[pyielink-dl] rtt ${ms} ms`),
    });
    const svcLog = (m) => { flog(m); console.log(m); };
    const files = new FileService(mux, session, svcLog);
    const input = new InputService(mux, session, svcLog);
    const video = new VideoService(mux, session, svcLog);
    const audio = new AudioService(mux, session, svcLog);
    hb.start();
    if (process.env.PYIELINK_TRANSPORT !== "tcp") {
      startHostRtc({ mux, onSignal: sendSignal, log: (m) => flog(m) })
        .then((pc) => { rtcPc = pc; })
        .catch((e) => flog("[rtc] host setup failed, ws fallback: " + (e && e.message)));
    }
    ws.on("close", (code) => {
      if (rtcPc) { try { rtcPc.close(); } catch {} rtcPc = null; }
      if (code === 1000 || code === 1005) {
        hb.stop();
      }
      files.teardownAll();
      input.stop();
      video.stop();
      audio.stop();
      console.log(`[pyielink-dl] client disconnected (code ${code})`);
    });
  });

  ws.on("error", () => {});
});

wss.on("error", (err) => {
  console.error(`[pyielink-dl] server error: ${err.message}`);
  process.exitCode = 1;
});
