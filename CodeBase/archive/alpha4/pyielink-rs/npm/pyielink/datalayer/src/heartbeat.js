import { CHANNELS } from "./mux.js";

const INTERVAL_MS = Math.max(250, Number(process.env.PYIELINK_DL_HB_MS) || 5000);
const MAX_MISSES = 3;
const PING = Buffer.from("PING");
const PONG = Buffer.from("PONG");

export class Heartbeat {
  constructor(mux, { onLost, onRtt } = {}) {
    this.mux = mux;
    this.onLost = onLost;
    this.onRtt = onRtt;
    this.misses = 0;
    this.rttMs = null;
    this.timer = null;
    this.awaiting = false;

    mux.on(CHANNELS.CONTROL, (payload) => this._onControl(payload));
  }

  start() {
    this.stop();
    this.timer = setInterval(() => this._tick(), INTERVAL_MS);
    if (this.timer.unref) this.timer.unref();
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }

  _onControl(payload) {
    const text = payload.toString("utf8");
    if (text === "PING") {
      this.misses = 0;
      this.mux.send(CHANNELS.CONTROL, PONG);
      return;
    }
    if (text === "PONG" && this.awaiting) {
      this.awaiting = false;
      this.rttMs = Date.now() - this.lastSentAt;
      this.misses = 0;
      if (this.onRtt) this.onRtt(this.rttMs);
      return;
    }
    this.mux.send(CHANNELS.CONTROL, payload);
  }

  _tick() {
    const ws = this.mux.ws;
    if (!ws || ws.readyState === 2 || ws.readyState === 3) {
      this._registerMiss();
      return;
    }
    if (ws.readyState !== 1) return;
    if (!this.mux.send(CHANNELS.CONTROL, PING)) {
      this._registerMiss();
      return;
    }
    if (this.awaiting) this._registerMiss();
    this.awaiting = true;
    this.lastSentAt = Date.now();
  }

  _registerMiss() {
    this.misses += 1;
    if (this.misses >= MAX_MISSES) {
      this.stop();
      if (this.onLost) this.onLost();
      else if (this.mux.ws) this.mux.ws.close(4002, "heartbeat lost");
    }
  }
}
