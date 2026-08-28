import { timingSafeEqual } from "node:crypto";

export const CHANNELS = Object.freeze({
  CONTROL: 0x01,
  INPUT: 0x02,
  VIDEO: 0x03,
  FILE_META: 0x04,
  FILE_CHUNK: 0x05,
  AUDIO: 0x06,
});

const HEADER = 5;
const MAX_PAYLOAD = 8 * 1024 * 1024;
export const HIGH_WATER = 256 * 1024; // 256K, not 4M: backpressure early, keep queue <15ms at 5Mbps

export function frame(channel, payload) {
  const body = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
  const out = Buffer.allocUnsafe(HEADER + body.length);
  out.writeUInt8(channel, 0);
  out.writeUInt32BE(body.length, 1);
  body.copy(out, HEADER);
  return out;
}

export class Mux {
  constructor(ws) {
    this.ws = ws;
    this.handlers = new Map();
    this.droppedUnknown = 0;
    this.droppedOversize = 0;
    this.buf = Buffer.alloc(0);
    this.dcMap = null; // channel -> RTCDataChannel when WebRTC is active
    ws.on("message", (data) => this._feed(data));
  }

  // Attach a live RTCDataChannel for `channel`. When set, send() prefers it
  // over the WebSocket. Pass null to clear and fall back entirely to ws.
  // One underlying RTCDataChannel may carry several mux channels (e.g. the
  // "file" DC carries FILE_META + FILE_CHUNK), so we attach the message/close
  // listeners exactly once per DC, not once per channel.
  useRtcChannel(channel, dc) {
    if (!this.dcMap) this.dcMap = new Map();
    if (!this._dcListeners) this._dcListeners = new Map(); // RTCDataChannel -> Set<channel>
    if (dc) {
      this.dcMap.set(channel, dc);
      let set = this._dcListeners.get(dc);
      if (!set) {
        set = new Set();
        this._dcListeners.set(dc, set);
        const sink = (e) => {
          const raw = e && e.data !== undefined ? e.data : e;
          this._feed(Buffer.isBuffer(raw) ? raw : Buffer.from(raw));
        };
        dc.on("message", sink);
        dc.on("close", () => {
          const chs = this._dcListeners.get(dc);
          if (chs) {
            for (const c of chs) this.dcMap.delete(c);
            this._dcListeners.delete(dc);
          }
        });
      }
      set.add(channel);
    } else if (this.dcMap) {
      this.dcMap.delete(channel);
      for (const [dc2, set] of this._dcListeners) {
        if (set.delete(channel) && set.size === 0) this._dcListeners.delete(dc2);
      }
    }
  }

  on(channel, handler) {
    this.handlers.set(channel, handler);
    return this;
  }

  send(channel, payload) {
    // Prefer a live WebRTC data channel for this channel when one is attached.
    const dc = this.dcMap && this.dcMap.get(channel);
    if (dc && dc.readyState === "open") {
      const body = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
      if (body.length > MAX_PAYLOAD) return false;
      try {
        dc.send(frame(channel, body));
        return true;
      } catch {
        return false;
      }
    }
    if (!this.ws || this.ws.readyState !== 1) return false;
    if (this.ws.bufferedAmount > HIGH_WATER) return false;
    const len = Buffer.isBuffer(payload) ? payload.length : Buffer.byteLength(payload);
    if (len > MAX_PAYLOAD) return false;
    this.ws.send(frame(channel, payload));
    return true;
  }

  _feed(data) {
    // Text messages on the ws are WebRTC signaling, never mux frames.
    if (typeof data === "string") return;
    const chunk = Buffer.isBuffer(data) ? data : Buffer.from(data);
    this.buf = this.buf.length ? Buffer.concat([this.buf, chunk]) : chunk;
    while (true) {
      if (this.buf.length < HEADER) return;
      const channel = this.buf.readUInt8(0);
      const len = this.buf.readUInt32BE(1);
      if (len > MAX_PAYLOAD) {
        this.droppedOversize += 1;
        this.buf = Buffer.alloc(0);
        return;
      }
      if (this.buf.length < HEADER + len) return;
      const payload = this.buf.subarray(HEADER, HEADER + len);
      this.buf = this.buf.subarray(HEADER + len);
      const handler = this.handlers.get(channel);
      if (handler) {
        try {
          handler(payload);
        } catch {
          this.droppedUnknown += 1;
        }
      } else {
        this.droppedUnknown += 1;
      }
    }
  }
}

export function keysMatch(a, b) {
  const ha = Buffer.from(String(a), "utf8");
  const hb = Buffer.from(String(b), "utf8");
  if (ha.length !== hb.length) return false;
  return timingSafeEqual(ha, hb);
}
