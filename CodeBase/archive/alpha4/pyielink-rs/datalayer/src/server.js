import { readFileSync, rmSync, existsSync } from "node:fs";
import process from "node:process";
import { WebSocketServer } from "ws";
import { Mux, keysMatch, CHANNELS } from "./mux.js";
import { Heartbeat } from "./heartbeat.js";

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
console.log(
  `[pyielink-dl] listening on ws://127.0.0.1:${port} user=${session.user} role=${session.role}`
);

wss.on("connection", (ws) => {
  let mux = null;
  let hb = null;
  let authed = false;

  const authTimer = setTimeout(() => {
    if (!authed && ws.readyState === 1) ws.close(4001, "auth timeout");
  }, AUTH_WINDOW_MS);
  if (authTimer.unref) authTimer.unref();

  ws.on("message", (data, isBinary) => {
    if (authed) return;
    clearTimeout(authTimer);
    let hello = null;
    try {
      hello = JSON.parse(Buffer.from(data).toString("utf8"));
    } catch {
      hello = null;
    }
    if (!hello || typeof hello.k !== "string" || !keysMatch(hello.k, session.key)) {
      console.warn("[pyielink-dl] session key mismatch - closing 4001");
      ws.close(4001, "bad session key");
      return;
    }
    authed = true;
    console.log(`[pyielink-dl] client authenticated (${session.user})`);
    ws.send(JSON.stringify({ ok: true, user: session.user, role: session.role }));
    mux = new Mux(ws);
    mux.on(CHANNELS.CONTROL, () => {});
    hb = new Heartbeat(mux, {
      onLost: () => {
        console.warn("[pyielink-dl] heartbeat lost - closing 4002");
        ws.close(4002, "heartbeat lost");
      },
      onRtt: (ms) => console.log(`[pyielink-dl] rtt ${ms} ms`),
    });
    hb.start();
    ws.on("close", (code) => {
      if (code === 1000 || code === 1005) {
        hb.stop();
      }
      console.log(`[pyielink-dl] client disconnected (code ${code})`);
    });
  });

  ws.on("error", () => {});
});

wss.on("error", (err) => {
  console.error(`[pyielink-dl] server error: ${err.message}`);
  process.exitCode = 1;
});
