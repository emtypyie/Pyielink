import { createHash } from "node:crypto";
import {
  copyFileSync,
  createReadStream,
  createWriteStream,
  existsSync,
  mkdirSync,
  renameSync,
  statSync,
  unlinkSync,
} from "node:fs";
import { homedir, tmpdir } from "node:os";
import path from "node:path";
import { CHANNELS } from "./mux.js";

const CHUNK = 64 * 1024;
const MAX_FILE = 2 * 1024 * 1024 * 1024;
const MAX_ACTIVE = 3;

function safeBaseName(name) {
  const base = path.basename(String(name || ""));
  if (!base || base === "." || base === ".." || /[<>:"|?*\u0000]/.test(base)) return null;
  return base;
}

export class FileService {
  constructor(mux, session, log) {
    this.mux = mux;
    this.session = session;
    this.log = log || (() => {});
    this.transfers = new Map();
    this.nextId = 1;
    this.landing = path.join(homedir(), "pyielink-files");
    try {
      mkdirSync(this.landing, { recursive: true });
    } catch {}
    mux.on(CHANNELS.FILE_META, (p) => this._meta(p));
    mux.on(CHANNELS.FILE_CHUNK, (p) => this._chunk(p));
  }

  _sendMeta(obj) {
    this.mux.send(CHANNELS.FILE_META, Buffer.from(JSON.stringify(obj)));
  }

  _error(id, code, msg) {
    this._sendMeta({ t: "error", id, code, msg });
    const x = this.transfers.get(id);
    if (x) this._abort(id);
  }

  _abort(id) {
    const x = this.transfers.get(id);
    if (!x) return;
    clearTimeout(x.stallTimer);
    if (x.ws && !x.ws.destroyed) x.ws.destroy();
    if (x.dir === "push") {
      try {
        unlinkSync(x.tmpPath);
      } catch {}
    }
    this.transfers.delete(id);
    this.log(`[files] transfer ${id} aborted (${x.dir} ${x.name})`);
  }

  // resolve a requested host-side path against the role sandbox:
  // standard users are locked to the landing dir; admins may use any
  // absolute path. Relative names always land in the sandbox root.
  _resolveTarget(name) {
    const requested = String(name || "").trim();
    if (!requested) return { err: "empty path" };
    const absolute = path.isAbsolute(requested) || /^[a-zA-Z]:/.test(requested);
    if (this.session.role !== "admin") {
      if (absolute || requested.split(/[\\/]/).includes("..")) {
        return { err: "standard users may only write inside their home landing folder" };
      }
      const base = safeBaseName(requested);
      if (!base) return { err: "invalid file name" };
      return { abs: path.join(this.landing, base) };
    }
    let candidate;
    if (absolute) {
      candidate = path.normalize(requested);
    } else {
      const base = safeBaseName(requested);
      if (!base) return { err: "invalid file name" };
      candidate = path.join(this.landing, base);
    }
    const parsed = path.parse(candidate);
    for (const seg of parsed.dir.slice(parsed.root.length).split(path.sep)) {
      if (seg === "..") return { err: "path traversal rejected" };
    }
    if (!parsed.base || /[<>:"|?*\u0000]/.test(parsed.base)) return { err: "invalid file name" };
    return { abs: candidate };
  }

  _meta(payload) {
    let msg;
    try {
      msg = JSON.parse(payload.toString("utf8"));
    } catch {
      return;
    }
    if (msg.t === "pull") return this._pull(msg);
    if (msg.t === "push") return this._push(msg);
    if (msg.t === "eof") {
      // client finished streaming its push; required for zero-byte files
      // where no FILE_CHUNK ever arrives to trigger completion
      const id = Number(msg.id);
      const x = this.transfers.get(id);
      if (!x || x.dir !== "push") return;
      const claimed = Number(msg.bytes) || 0;
      if (claimed !== x.written || x.written !== x.size) {
        return this._error(id, "bad", `eof claims ${claimed} bytes but ${x.written}/${x.size} arrived`);
      }
      return this._finishPush(id);
    }
    if (msg.t === "done-ack") {
      const x = this.transfers.get(msg.id);
      if (x) this._abort(msg.id); // ack received -> cleanup local handle
      return;
    }
  }

  _pull({ id, name, have }) {
    id = Number(id) || this.nextId++;
    if (this.transfers.size >= MAX_ACTIVE) return this._error(id, "busy", "too many concurrent transfers");
    const target = this._resolveTarget(name);
    if (target.err) return this._error(id, "denied", target.err);
    let st;
    try {
      st = statSync(target.abs);
    } catch {
      return this._error(id, "notfound", `no such file: ${name}`);
    }
    if (!st.isFile()) return this._error(id, "notfound", `not a regular file: ${name}`);
    if (st.size > MAX_FILE) return this._error(id, "toobig", "file exceeds 2 GiB cap");

    const sha = createHash("sha256");
    const rs = createReadStream(target.abs);
    rs.on("data", (b) => sha.update(b));
    rs.on("error", () => this._error(id, "io", "read failed"));
    rs.on("end", () => {
      const digest = sha.digest("hex");
      const offset = Math.max(0, Math.min(Number(have) || 0, st.size));
      this.transfers.set(id, { dir: "pull", name, ws: rs });
      this._sendMeta({ t: "meta", id, size: st.size, sha256: digest });
      this._streamOut(id, target.abs, offset);
    });
  }

  _streamOut(id, filePath, offset) {
    const total = statSync(filePath).size;
    const rs = createReadStream(filePath, { start: offset });
    const x = this.transfers.get(id);
    if (x) {
      x.ws = rs;
      x.startedAt = Date.now();
      x.lastActivity = Date.now();
    }
    let sent = offset;
    let readEnd = false;
    let eofSent = false;
    const queue = [];
    let retryTimer = null;

    const tryEof = () => {
      if (!eofSent && readEnd && queue.length === 0) {
        eofSent = true;
        this._sendMeta({ t: "eof", id, bytes: sent - offset });
        this.log(`[files] pull ${id} streamed ${sent - offset} of ${total} bytes (${this.session.user})`);
        // registry entry cleaned when client confirms via done-ack or on teardown
      }
    };
    const flush = () => {
      if (retryTimer) {
        clearTimeout(retryTimer);
        retryTimer = null;
      }
      while (queue.length > 0) {
        if (this.mux.send(CHANNELS.FILE_CHUNK, queue[0])) {
          queue.shift();
          if (x) x.lastActivity = Date.now();
        } else {
          // socket saturated: pause source, retry same chunk shortly
          retryTimer = setTimeout(flush, 50);
          return;
        }
      }
      if (!readEnd) rs.resume();
      tryEof();
    };
    rs.on("data", (buf) => {
      const head = Buffer.allocUnsafe(12);
      head.writeUInt32BE(id, 0);
      head.writeBigUInt64BE(BigInt(sent), 4);
      sent += buf.length;
      queue.push(Buffer.concat([head, buf]));
      rs.pause();
      flush();
    });
    rs.on("end", () => {
      readEnd = true;
      tryEof();
    });
    rs.on("error", () => this._error(id, "io", "read failed mid-stream"));
  }

  _push({ id, name, size, sha256 }) {
    id = Number(id) || this.nextId++;
    if (this.transfers.size >= MAX_ACTIVE) return this._error(id, "busy", "too many concurrent transfers");
    const target = this._resolveTarget(name);
    if (target.err) return this._error(id, "denied", target.err);
    size = Number(size);
    if (!Number.isFinite(size) || size < 0) return this._error(id, "bad", "bad size");
    if (size > MAX_FILE) return this._error(id, "toobig", "file exceeds 2 GiB cap");
    if (typeof sha256 !== "string" || !/^[0-9a-f]{64}$/.test(sha256)) {
      return this._error(id, "bad", "sha256 must be 64 hex chars");
    }

    const stagingDir = path.join(tmpdir(), "pyielink-dl");
    try {
      mkdirSync(stagingDir, { recursive: true });
    } catch {}
    const tmpPath = path.join(stagingDir, `${id}-${Date.now()}.part`);
    const existing = existsSync(target.abs) ? statSync(target.abs).size : 0;
    const resume = Math.max(0, Math.min(existing, size));

    const ws = createWriteStream(tmpPath, { flags: resume > 0 ? "a" : "w" });
    const sha = createHash("sha256");
    // hash only the appended region; seed nothing for full files
    const x = {
      dir: "push",
      name,
      size,
      sha256,
      ws,
      sha,
      written: resume,
      tmpPath,
      abs: target.abs,
      startedAt: Date.now(),
      lastActivity: Date.now(),
      stallTimer: null,
    };
    this.transfers.set(id, x);
    x.stallTimer = setTimeout(() => this._stalled(id), 30000);
    ws.on("error", () => this._error(id, "io", "write failed"));
    this._sendMeta({ t: "ready", id, resume, size });
    this.log(`[files] push ${id} accepted: ${target.abs} (resume at ${resume})`);
  }

  _stalled(id) {
    const x = this.transfers.get(id);
    if (x && Date.now() - (x.lastActivity || x.startedAt || Date.now()) > 25000) {
      this._error(id, "stall", "transfer stalled");
    } else if (x) {
      x.stallTimer.refresh();
    }
  }

  _chunk(payload) {
    if (payload.length < 12) return;
    const id = payload.readUInt32BE(0);
    const offset = Number(payload.readBigUInt64BE(4));
    const data = payload.subarray(12);
    const x = this.transfers.get(id);
    if (!x || x.dir !== "push") return; // unknown/stale chunk: drop silently
    if (offset !== x.written) return; // out-of-order: drop; sender retries by offset policy
    if (x.written + data.length > x.size) return this._error(id, "bad", "exceeds announced size");
    x.sha.update(data);
    x.written += data.length;
    x.lastActivity = Date.now();
    x.ws.write(data);
    if (x.written === x.size) this._finishPush(id);
  }

  _finishPush(id) {
    const x = this.transfers.get(id);
    clearTimeout(x.stallTimer);
    const digest = x.sha.digest("hex");
    const ok = digest === x.sha256.toLowerCase();
    if (!ok) {
      this._error(id, "hashmismatch", `sha256 mismatch got=${digest}`);
      return;
    }
    try {
      mkdirSync(path.dirname(x.abs), { recursive: true });
      // move may cross volumes; fall back to copy+unlink
      try {
        renameSync(x.tmpPath, x.abs);
      } catch {
        copyFileSync(x.tmpPath, x.abs);
        unlinkSync(x.tmpPath);
      }
    } catch (e) {
      this._error(id, "io", `finalize failed: ${e.message}`);
      return;
    }
    this._sendMeta({ t: "done", id, ok: true, bytes: x.written });
    this.log(`[files] push ${id} complete: ${x.abs} (${x.written} bytes verified)`);
    this.transfers.delete(id);
  }

  teardownAll() {
    for (const id of [...this.transfers.keys()]) this._abort(id);
  }
}
