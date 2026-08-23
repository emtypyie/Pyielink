import { CHANNELS } from "./mux.js";

const CHUNK_SIZE = 64 * 1024;
const FFMPEG_RESTART_DELAY = 2000;
const MAX_RESTARTS = 5;

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
        if (requestedKbps !== this.currentBitrate) {
            this.log(`[video] bitrate change requested: ${this.currentBitrate} -> ${requestedKbps} kbps`);
            this.currentBitrate = requestedKbps;
            this._restartWithNewBitrate();
        }
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

        const { spawn } = require("child_process");

        let inputArg = "video=screen-capture-recorder";
        if (this.monitorWidth > 0 && this.monitorHeight > 0) {
            inputArg = `video=screen-capture-recorder:offset_x=${this.monitorOffsetX}:offset_y=${this.monitorOffsetY}:video_size=${this.monitorWidth}x${this.monitorHeight}`;
        }

        const bitrateKbps = this.currentBitrate;
        const maxrateKbps = Math.round(bitrateKbps * 1.2); // 20% headroom
        const bufsizeKbps = bitrateKbps * 2;

        const args = [
            "-f", "dshow",
            "-i", inputArg,
            "-f", "mpegts",
            "-codec:v", "libx264",
            "-preset", "ultrafast",
            "-tune", "zerolatency",
            "-b:v", `${bitrateKbps}k`,
            "-maxrate", `${maxrateKbps}k`,
            "-bufsize", `${bufsizeKbps}k`,
            "-g", "30",
            "-framerate", "30",
            "-pix_fmt", "yuv420p",
            "-fflags", "nobuffer+genpts",
            "-flags", "low_delay",
            "-probesize", "32",
            "-analyzeduration", "0",
            "-copyts",
            "-start_at_zero",
            "pipe:1"
        ];

        this.log(`[video] spawning ffmpeg: ${args.join(" ")}`);

        this.ffmpeg = spawn("ffmpeg", args, {
            stdio: ["ignore", "pipe", "pipe"]
        });

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
        
        this.buffer = Buffer.concat([this.buffer, chunk]);

        while (this.buffer.length >= CHUNK_SIZE) {
            const frame = this.buffer.subarray(0, CHUNK_SIZE);
            this.buffer = this.buffer.subarray(CHUNK_SIZE);
            this.mux.send(CHANNELS.VIDEO, frame);
        }
    }
}