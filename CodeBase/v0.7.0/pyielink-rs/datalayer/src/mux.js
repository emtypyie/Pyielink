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
    ws.on("message", (data) => this._feed(data));
  }

  on(channel, handler) {
    this.handlers.set(channel, handler);
    return this;
  }

  send(channel, payload) {
    if (!this.ws || this.ws.readyState !== 1) return false;
    if (this.ws.bufferedAmount > HIGH_WATER) return false;
    const len = Buffer.isBuffer(payload) ? payload.length : Buffer.byteLength(payload);
    if (len > MAX_PAYLOAD) return false;
    this.ws.send(frame(channel, payload));
    return true;
  }

  _feed(data) {
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
