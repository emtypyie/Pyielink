import { CHANNELS } from "./mux.js";
import { spawn, execSync, spawnSync } from "child_process";
import { performance } from "node:perf_hooks";
import { appendFileSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ASSETS = fileURLToPath(new URL("./assets/", import.meta.url));

// ffmpeg/nvenc preset names changed between builds: old "llhp"/"ll"/"hp" were
// replaced by the p1..p7 presets in current ffmpeg. Probe a 1-frame lavfi
// encode to pick a preset/tune combo this build actually accepts (cached).
const encodeCache = {};
function _encodeWorks(codec, preset, tune) {
    try {
        const r = spawnSync("ffmpeg", [
            "-hide_banner", "-t", "1",
            "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30",
            "-c:v", codec, "-preset", preset, "-tune", tune, "-f", "null", "-"
        ], { timeout: 20000, stdio: "ignore" });
        return r.status === 0;
    } catch (_) {
        return false;
    }
}
function resolveEncode(codec, presets, tunes, extra) {
    if (encodeCache[codec]) return encodeCache[codec];
    for (const p of presets) {
        for (const t of tunes) {
            if (_encodeWorks(codec, p, t)) {
                const cfg = { codec, preset: p, tune: t, extra };
                encodeCache[codec] = cfg;
                return cfg;
            }
        }
    }
    const cfg = { codec, preset: presets[0], tune: tunes[0], extra };
    encodeCache[codec] = cfg;
    return cfg;
}


const CHUNK_SIZE = 1200; // MTU-friendly: 1200B < 1500 MTU, vs 64K bursty (NALU slicer)
const FFMPEG_RESTART_DELAY = 2000;
const MAX_RESTARTS = 5;
const LATENCY_CSV = path.join(process.env.TEMP || process.env.TMPDIR || ".", "pyielink-latency.csv");
function latencyLog(stage, ms, extra="") {
  try {
    const line = `${Date.now()},${stage},${ms.toFixed(2)},${extra}\n`;
    appendFileSync(LATENCY_CSV, line);
  } catch {}
}
let hwProbeCache = null;
function probeHardware(log) {
  if (hwProbeCache) return hwProbeCache;
  const res = { ddagrab: false, encoders: [], hwaccels: [], captures: [] };
  const CAPS = ["ddagrab", "gdigrab", "pipewire", "x11grab", "avfoundation"];
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
    const devs = execSync("ffmpeg -hide_banner -devices 2>&1", { timeout: 3000 }).toString();
    for (const c of CAPS) if (devs.includes(c)) res.captures.push(c);
    res.ddagrab = res.captures.includes("ddagrab");
  } catch {}
  hwProbeCache = res;
  try { log(`[video] hw probe: captures=[${res.captures.join(",")}] encoders=[${res.encoders.join(",")}] hwaccels=[${res.hwaccels.join(",")}]`); } catch {}
  latencyLog("hw_probe", 0, `caps=${res.captures.join("|")},enc=${res.encoders.join("|")}`);
  return res;
}

// Choose the best screen-capture input for the current OS, based on what ffmpeg
// supports here. Cross-platform: Windows ddagrab/gdigrab, Linux pipewire/x11grab,
// macOS avfoundation.
function pickCapture(hw) {
  const p = process.platform;
  if (p === "win32") {
    if (hw.captures.includes("ddagrab")) return { fmt: "ddagrab", arg: "desktop", fr: "60" };
    return { fmt: "gdigrab", arg: "desktop", fr: "60" };
  }
  if (p === "linux") {
    if (hw.captures.includes("pipewire")) return { fmt: "pipewire", arg: "desktop", fr: "60" };
    return { fmt: "x11grab", arg: ":0.0", fr: "60" };
  }
  if (p === "darwin") {
    return { fmt: "avfoundation", arg: "1:none", fr: "60" };
  }
  return { fmt: "gdigrab", arg: "desktop", fr: "60" };
}

export class VideoService {
    constructor(mux, session, log) {
        this.mux = mux;
        this.session = session;
        this.log = log || (() => {});
        this.active = false;
        this.ffmpeg = null;
        this.cap = null; // DXGI capture helper (when used instead of gdigrab)
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
        const cap = pickCapture(hw);
        const inputFormat = cap.fmt;
        let inputArg = cap.arg;
        const framerate = cap.fr;
        const inputOpts = [];
        if (this.monitorWidth > 0 && this.monitorHeight > 0) {
            if (inputFormat === "x11grab") {
                inputArg = `:0.0+${this.monitorOffsetX},${this.monitorOffsetY}`;
                inputOpts.push("-video_size", `${this.monitorWidth}x${this.monitorHeight}`);
            } else if (inputFormat === "avfoundation") {
                inputOpts.push("-capture_cursor", "0");
                inputOpts.push("-video_size", `${this.monitorWidth}x${this.monitorHeight}`);
            } else {
                inputOpts.push("-offset_x", String(this.monitorOffsetX));
                inputOpts.push("-offset_y", String(this.monitorOffsetY));
                inputOpts.push("-video_size", `${this.monitorWidth}x${this.monitorHeight}`);
            }
        }

        const bitrateKbps = this.currentBitrate;
        const maxrateKbps = Math.round(bitrateKbps * 1.2); // 20% headroom
        // Low-latency: 0.4× duration, not 2× (10M → 2s VBV → 10-20ms queue)
        const bufsizeKbps = Math.max(1500, Math.round(bitrateKbps * 0.4));

        // Pick best encoder: NVENC > QSV > AMF > libx264
        // Preset/tune are auto-detected per ffmpeg build (see resolveEncode).
        let codec = "libx264";
        let preset = "ultrafast";
        let tune = "zerolatency";
        let extraCodecOpts = [];
        if (hw.encoders.includes("h264_nvenc")) {
            const e = resolveEncode("h264_nvenc",
                ["p1", "llhp", "ll", "hp", "fast"],
                ["ull", "zerolatency", "ll"],
                ["-rc", "cbr", "-bf", "0", "-g", "60", "-forced-idr", "1"]);
            codec = e.codec; preset = e.preset; tune = e.tune; extraCodecOpts = e.extra;
        } else if (hw.encoders.includes("h264_qsv")) {
            const e = resolveEncode("h264_qsv",
                ["veryfast", "fast", "medium"],
                ["zerolatency", "ull"],
                ["-bf", "0", "-g", "60"]);
            codec = e.codec; preset = e.preset; tune = e.tune; extraCodecOpts = e.extra;
        } else if (hw.encoders.includes("h264_amf")) {
            const e = resolveEncode("h264_amf",
                ["speed", "fast"],
                ["zerolatency", "ull"],
                ["-bf", "0", "-g", "60"]);
            codec = e.codec; preset = e.preset; tune = e.tune; extraCodecOpts = e.extra;
        } else {
            // libx264 fallback tightened
            extraCodecOpts = ["-bf", "0", "-refs", "1", "-g", "60"];
        }

        const inputFramerate = framerate;
        const gop = "60"; // 1s at 60fps or 1.3s at 45fps → faster IDR recovery

        // Optional downscale: trade resolution for encode/decode headroom so
        // weaker clients (or single-box test setups) can still sustain 60fps.
        // PYIELINK_VIDEO_SCALE=1280:720  (WxH or W:H; aspect preserved)
        const outPre = [];
        const scaleEnv = process.env.PYIELINK_VIDEO_SCALE;
        if (scaleEnv && !this.monitorWidth) {
            const m = String(scaleEnv).match(/(\d+)[x:](\d+)/);
            if (m) outPre.push("-vf", `scale=${m[1]}:${m[2]}:force_original_aspect_ratio=decrease`);
        }

        // --- GPU Desktop Duplication capture (DXGI) when available ---
        // gdigrab (GDI BitBlt) tops out ~30-45 fps on a single box; DXGI
        // Desktop Duplication runs at the display refresh (60/120/144) on the
        // GPU. The C++ helper (assets/dxgi_capture.exe) grabs frames and
        // pipes raw BGRA to ffmpeg, which only encodes (NVENC/QSV/...).
        const dxgiExe = path.join(ASSETS, "dxgi_capture.exe");
        if (this.dxgiForcedOff === undefined) this.dxgiForcedOff = false;
        let useDxgi = false;
        if (this.dxgiForcedOff) {
            this.log("[video] DXGI disabled after previous failure; using gdigrab");
        } else if (process.env.PYIELINK_CAPTURE === "dxgi") {
            useDxgi = true;
        } else if (process.env.PYIELINK_CAPTURE === "gdigrab") {
            useDxgi = false;
        } else {
            useDxgi = existsSync(dxgiExe);
        }
        let dxgiW = 0, dxgiH = 0;
        if (useDxgi) {
            try {
                const probe = execSync(`"${dxgiExe}" 0 --probe`, { timeout: 8000 }).toString();
                const mw = probe.match(/WIDTH=(\d+)/), mh = probe.match(/HEIGHT=(\d+)/);
                if (mw && mh) { dxgiW = parseInt(mw[1], 10); dxgiH = parseInt(mh[1], 10); }
            } catch (e) {
                this.log(`[video] dxgi probe failed (${e.message}); falling back to ${inputFormat}`);
                useDxgi = false; this.dxgiForcedOff = true;
            }
            if (dxgiW < 1 || dxgiH < 1) { useDxgi = false; this.dxgiForcedOff = true; }
        }

        let args;
        if (useDxgi) {
            args = [
                "-f", "rawvideo",
                "-pix_fmt", "bgra",
                "-s", `${dxgiW}x${dxgiH}`,
                "-r", "60",
                "-i", "pipe:0",
                ...outPre,
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
                "pipe:1"
            ];
            this.log(`[video] using DXGI Desktop Duplication ${dxgiW}x${dxgiH} @60fps + ${codec} ${preset}/${tune}${outPre.length ? " + scale=" + scaleEnv : ""}`);
            this.cap = spawn(dxgiExe, ["0"], { stdio: ["ignore", "pipe", "pipe"] });
            this.cap.stderr.on("data", (d) => { const m = d.toString().trim(); if (m) this.log(`[video] dxgi: ${m}`); });
            this.cap.on("error", (e) => this.log(`[video] dxgi spawn error: ${e.message}`));
            this.cap.on("close", (code) => {
                if (code !== 0) {
                    this.dxgiForcedOff = true;
                    this.log(`[video] dxgi helper exited (code ${code}); falling back to gdigrab`);
                }
                if (this.active) this._scheduleRestart();
            });
        } else {
            args = [
                "-f", inputFormat,
                // Input options MUST precede -i: this sets how fast capture polls
                "-framerate", inputFramerate,
                ...inputOpts,
                "-i", inputArg,
                ...outPre,
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
            this.log(`[video] using ${inputFormat} @${framerate}fps + ${codec} ${preset}/${tune}${outPre.length ? " + scale=" + scaleEnv : ""}`);
        }

        this.log(`[video] spawning ffmpeg: ${args.join(" ")}`);

        this.ffmpeg = spawn("ffmpeg", args, {
            stdio: useDxgi ? ["pipe", "pipe", "pipe"] : ["ignore", "pipe", "pipe"]
        });
        if (useDxgi && this.cap) {
            // Helper raw BGRA stdout -> ffmpeg rawvideo stdin.
            this.cap.stdout.pipe(this.ffmpeg.stdin);
        }
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
        if (this.cap && !this.cap.killed) {
            try { this.cap.kill("SIGKILL"); } catch (_) {}
        }
        this.cap = null;
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

        // Drain in MTU-sized frames. If a send fails (DC closed mid-frame,
        // backpressure, network drop) we just drop that frame and keep going
        // with the next one — never let a transient error kill the encoder.
        try {
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
                } else if (ok) {
                    this._sendDropped = false;
                }
            }
        } catch (e) {
            this.log(`[video] send loop error (dropping frame, continuing): ${e.message}`);
        }
    }
}