import { CHANNELS } from "./mux.js";
import { spawn, execSync } from "child_process";
import { performance } from "node:perf_hooks";
import { appendFileSync } from "node:fs";

const CHUNK_SIZE = 1200; // MTU-friendly: 1200B < 1500 MTU, vs 64K bursty (NALU slicer)
const FFMPEG_RESTART_DELAY = 2000;
const MAX_RESTARTS = 5;
const LATENCY_CSV = (process.env.TEMP || ".") + "\\pyielink-latency.csv";
function latencyLog(stage, ms, extra="") {
  try {
    const line = `${Date.now()},${stage},${ms.toFixed(2)},${extra}\n`;
    appendFileSync(LATENCY_CSV, line);
  } catch {}
}
let hwProbeCache = null;
function probeHardware(log) {
  if (hwProbeCache) return hwProbeCache;
  const res = { ddagrab: false, encoders: [], hwaccels: [] };
  try {
    const enc = execSync("ffmpeg -hide_banner -encoders 2>&1", { timeout: 3000 }).toString();
    if (enc.includes("h264_nvenc")) res.encoders.push("h264_nvenc");
    if (enc.includes("hevc_nvenc")) res.encoders.push("hevc_nvenc");
    if (enc.includes("h264_qsv")) res.encoders.push("h264_qsv");
    if (enc.includes("h264_amf")) res.encoders.push("h264_amf");
  } catch {}
  try {
    const hw = execSync("ffmpeg -hide_banner -hwaccels 2>&1", { timeout: 3000 }).toString();
    res.hwaccels = hw.split(/\W+/).filter(Boolean);
  } catch {}
  try {
    const fmts = execSync("ffmpeg -hide_banner -formats 2>&1", { timeout: 3000 }).toString();
    if (fmts.includes("ddagrab")) res.ddagrab = true;
  } catch {}
  // Also check gdigrab always available on Windows
  hwProbeCache = res;
  try { log(`[video] hw probe: ddagrab=${res.ddagrab} encoders=[${res.encoders.join(",")}] hwaccels=[${res.hwaccels.join(",")}]`); } catch {}
  latencyLog("hw_probe", 0, `ddagrab=${res.ddagrab},enc=${res.encoders.join("|")}`);
  return res;
}

export class VideoService {
    constructor(mux, session, log) {
        this.mux = mux;
        this.session = session;
        this.log = log || (() => {});
        this.active = false;
        this.ffmpeg = null;
        this.restartCount = 0;
        this.restartTimer = null;
        this.buffer = Buffer.alloc(0);
        this.monitorIndex = 0;
        this.monitorOffsetX = 0;
        this.monitorOffsetY = 0;
        this.monitorWidth = 0;
        this.monitorHeight = 0;
        this.paused = false;
        this._spawnTime = 0;
        this._frameCount = 0;
        // latency CSV header
        try { appendFileSync(LATENCY_CSV, "ts,stage,ms,extra\n"); } catch {}
        // Adaptive bitrate
        this.currentBitrate = 5000; // kbps
        this.minBitrate = 500; // kbps
        this.maxBitrate = 20000; // kbps
        this.targetLatency = 100; // ms
        this.lastBitrateRequest = Date.now();
        this.bitrateRequestInterval = 5000; // ms

        mux.on(CHANNELS.VIDEO, (payload) => this._handleControl(payload));
    }

    _handleControl(payload) {
        try {
            const msg = JSON.parse(payload.toString("utf8"));
            if (msg.t === "video_start") {
                this.monitorIndex = msg.monitor_index ?? 0;
                this.monitorOffsetX = msg.offset_x ?? 0;
                this.monitorOffsetY = msg.offset_y ?? 0;
                this.monitorWidth = msg.width ?? 0;
                this.monitorHeight = msg.height ?? 0;
                this.start();
            } else if (msg.t === "video_stop") {
                this.stop();
            } else if (msg.t === "video_pause") {
                this.pause();
            } else if (msg.t === "video_resume") {
                this.resume();
            } else if (msg.t === "bitrate_request") {
                this._handleBitrateRequest(msg);
            }
        } catch (e) {
            this.log(`[video] control parse error: ${e.message}`);
        }
    }

    _handleBitrateRequest(msg) {
        const requestedKbps = Math.max(this.minBitrate, Math.min(this.maxBitrate, msg.kbps));
        const delta = Math.abs(requestedKbps - this.currentBitrate) / this.currentBitrate;
        if (delta < 0.25 || Date.now() - (this._lastRestart || 0) < 15000) return;
        this._lastRestart = Date.now();
        this.log(`[video] bitrate change requested: ${this.currentBitrate} -> ${requestedKbps} kbps`);
        this.currentBitrate = requestedKbps;
        this._restartWithNewBitrate();
    }

    _restartWithNewBitrate() {
        if (!this.active) return;
        this.log(`[video] restarting with new bitrate: ${this.currentBitrate} kbps`);
        this._killFFmpeg();
        // Small delay to ensure clean shutdown
        setTimeout(() => this._spawnFFmpeg(), 500);
    }

    pause() {
        if (this.paused) return;
        this.paused = true;
        this.log("[video] stream paused");
    }

    resume() {
        if (!this.paused) return;
        this.paused = false;
        this.log("[video] stream resumed");
    }

    start() {
        if (this.active) return;
        this.active = true;
        this.restartCount = 0;
        this._spawnFFmpeg();
        this.log("[video] stream started");
    }

    stop() {
        if (!this.active) return;
        this.active = false;
        this._killFFmpeg();
        if (this.restartTimer) {
            clearTimeout(this.restartTimer);
            this.restartTimer = null;
        }
        this.log("[video] stream stopped");
    }

    _spawnFFmpeg() {
        if (!this.active) return;

        const hw = probeHardware(this.log);
        const useDDAGrab = hw.ddagrab && process.platform === "win32";
        const inputFormat = useDDAGrab ? "ddagrab" : "gdigrab";
        const inputArg = "desktop";
        const inputOpts = [];
        if (this.monitorWidth > 0 && this.monitorHeight > 0) {
            inputOpts.push("-offset_x", String(this.monitorOffsetX));
            inputOpts.push("-offset_y", String(this.monitorOffsetY));
            inputOpts.push("-video_size", `${this.monitorWidth}x${this.monitorHeight}`);
        }

        const bitrateKbps = this.currentBitrate;
        const maxrateKbps = Math.round(bitrateKbps * 1.2); // 20% headroom
        // Low-latency: 0.4× duration, not 2× (10M → 2s VBV → 10-20ms queue)
        const bufsizeKbps = Math.max(1500, Math.round(bitrateKbps * 0.4));

        // Pick best encoder: NVENC > QSV > AMF > libx264
        let codec = "libx264";
        let preset = "ultrafast";
        let tune = "zerolatency";
        let extraCodecOpts = [];
        if (hw.encoders.includes("h264_nvenc")) {
            codec = "h264_nvenc"; preset = "llhp"; tune = "ull"; // NVIDIA low-latency HP + ultra-low
            extraCodecOpts = ["-rc", "cbr", "-bf", "0", "-g", "60", "-forced-idr", "1"];
        } else if (hw.encoders.includes("h264_qsv")) {
            codec = "h264_qsv"; preset = "veryfast"; tune = "zerolatency";
            extraCodecOpts = ["-bf", "0", "-g", "60"];
        } else if (hw.encoders.includes("h264_amf")) {
            codec = "h264_amf"; preset = "speed"; tune = "zerolatency";
            extraCodecOpts = ["-bf", "0", "-g", "60"];
        } else {
            // libx264 fallback tightened
            extraCodecOpts = ["-bf", "0", "-refs", "1", "-g", "60"];
        }

        const framerate = useDDAGrab ? "60" : "45";
        const inputFramerate = framerate;
        const gop = "60"; // 1s at 60fps or 1.3s at 45fps → faster IDR recovery

        const args = [
            "-f", inputFormat,
            // Input options MUST precede -i: this sets how fast capture polls
            "-framerate", inputFramerate,
            ...inputOpts,
            "-i", inputArg,
            "-f", "mpegts",
            "-codec:v", codec,
            "-preset", preset,
            "-tune", tune,
            "-b:v", `${bitrateKbps}k`,
            "-maxrate", `${maxrateKbps}k`,
            "-bufsize", `${bufsizeKbps}k`,
            "-g", gop,
            ...extraCodecOpts,
            "-pix_fmt", "yuv420p",
            "-fflags", "nobuffer+genpts",
            "-flags", "low_delay",
            "-probesize", "32",
            "-analyzeduration", "0",
            "-copyts",
            "-start_at_zero",
            "pipe:1"
        ];
        // ddagrab benefits from vsync/fps filter, but keep minimal
        if (useDDAGrab) {
            this.log(`[video] using DXGI ddagrab @${framerate}fps + ${codec} ${preset}/${tune}`);
        }

        this.log(`[video] spawning ffmpeg: ${args.join(" ")}`);

        this.ffmpeg = spawn("ffmpeg", args, {
            stdio: ["ignore", "pipe", "pipe"]
        });
        this._spawnTime = performance.now();
        this._frameCount = 0;
        latencyLog("capture_spawn", 0, `bitrate=${this.currentBitrate}`);

        this.ffmpeg.stdout.on("data", (chunk) => this._onVideoData(chunk));
        this.ffmpeg.stderr.on("data", (chunk) => {
            const msg = chunk.toString().trim();
            if (msg) this.log(`[video] ffmpeg: ${msg}`);
        });

        this.ffmpeg.on("error", (err) => {
            this.log(`[video] ffmpeg spawn error: ${err.message}`);
            this._scheduleRestart();
        });

        this.ffmpeg.on("close", (code, signal) => {
            this.log(`[video] ffmpeg exited: code=${code}, signal=${signal}`);
            if (this.active) this._scheduleRestart();
        });
    }

    _killFFmpeg() {
        if (this.ffmpeg && !this.ffmpeg.killed) {
            this.ffmpeg.kill("SIGTERM");
            setTimeout(() => {
                if (this.ffmpeg && !this.ffmpeg.killed) {
                    this.ffmpeg.kill("SIGKILL");
                }
            }, 1000);
        }
        this.ffmpeg = null;
    }

    _scheduleRestart() {
        if (!this.active) return;
        if (this.restartCount >= MAX_RESTARTS) {
            this.log(`[video] max restarts (${MAX_RESTARTS}) reached, giving up`);
            this.stop();
            return;
        }
        this.restartCount++;
        this.log(`[video] restarting in ${FFMPEG_RESTART_DELAY}ms (attempt ${this.restartCount}/${MAX_RESTARTS})`);
        this.restartTimer = setTimeout(() => this._spawnFFmpeg(), FFMPEG_RESTART_DELAY);
    }

    _onVideoData(chunk) {
        if (this.paused) return;
        const now = performance.now();
        this._totalSent = (this._totalSent || 0) + chunk.length;
        if (!this._dataSeen) {
            this._dataSeen = true;
            const firstMs = now - this._spawnTime;
            this.log(`[video] first stdout data (${chunk.length} bytes) after ${firstMs.toFixed(1)}ms`);
            latencyLog("capture_encode", firstMs, `first_chunk=${chunk.length}`);
        }
        if (this._totalSent - (this._lastLogged || 0) >= 1024 * 1024) {
            this._lastLogged = this._totalSent;
            this.log(`[video] encoder produced ${(this._totalSent / 1048576).toFixed(1)} MiB`);
        }

        this.buffer = Buffer.concat([this.buffer, chunk]);

        while (this.buffer.length >= CHUNK_SIZE) {
            const frame = this.buffer.subarray(0, CHUNK_SIZE);
            this.buffer = this.buffer.subarray(CHUNK_SIZE);
            const sendStart = performance.now();
            const ok = this.mux.send(CHANNELS.VIDEO, frame);
            this._frameCount++;
            latencyLog("network_send", performance.now() - sendStart, `frame=${this._frameCount},ok=${ok},buf=${this.mux.ws?.bufferedAmount||0}`);
            if (!ok && !this._sendDropped) {
                this._sendDropped = true;
                this.log(`[video] mux.send DROPPED (readyState=${this.mux.ws?.readyState}, buffered=${this.mux.ws?.bufferedAmount})`);
            }
        }
    }
}