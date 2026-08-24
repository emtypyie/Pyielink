import { CHANNELS } from "./mux.js";
import { spawn } from "child_process";

const CHUNK_SIZE = 4 * 1024;
const FFMPEG_RESTART_DELAY = 2000;
const MAX_RESTARTS = 5;

export class AudioService {
    constructor(mux, session, log) {
        this.mux = mux;
        this.session = session;
        this.log = log || (() => {});
        this.active = false;
        this.ffmpeg = null;
        this.restartCount = 0;
        this.restartTimer = null;
        this.buffer = Buffer.alloc(0);

        mux.on(CHANNELS.AUDIO, (payload) => this._handleControl(payload));
    }

    _handleControl(payload) {
        try {
            const msg = JSON.parse(payload.toString("utf8"));
            if (msg.t === "audio_start") {
                this.start();
            } else if (msg.t === "audio_stop") {
                this.stop();
            }
        } catch (e) {
            this.log(`[audio] control parse error: ${e.message}`);
        }
    }

    start() {
        if (this.active) return;
        this.active = true;
        this.restartCount = 0;
        this._spawnFFmpeg();
        this.log("[audio] stream started");
    }

    stop() {
        if (!this.active) return;
        this.active = false;
        this._killFFmpeg();
        if (this.restartTimer) {
            clearTimeout(this.restartTimer);
            this.restartTimer = null;
        }
        this.log("[audio] stream stopped");
    }

    _spawnFFmpeg() {
        if (!this.active) return;


        const args = [
            "-f", "dshow",
            "-i", "audio=Microphone",
            "-c:a", "libopus",
            "-b:a", "64k",
            "-application", "voip",
            "-frame_duration", "20",
            "-f", "opus",
            "pipe:1"
        ];

        this.log(`[audio] spawning ffmpeg: ${args.join(" ")}`);

        this.ffmpeg = spawn("ffmpeg", args, {
            stdio: ["ignore", "pipe", "pipe"]
        });

        this.ffmpeg.stdout.on("data", (chunk) => this._onAudioData(chunk));
        this.ffmpeg.stderr.on("data", (chunk) => {
            const msg = chunk.toString().trim();
            if (msg) this.log(`[audio] ffmpeg: ${msg}`);
        });

        this.ffmpeg.on("error", (err) => {
            this.log(`[audio] ffmpeg spawn error: ${err.message}`);
            this._scheduleRestart();
        });

        this.ffmpeg.on("close", (code, signal) => {
            this.log(`[audio] ffmpeg exited: code=${code}, signal=${signal}`);
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
            this.log(`[audio] max restarts (${MAX_RESTARTS}) reached, giving up`);
            this.stop();
            return;
        }
        this.restartCount++;
        this.log(`[audio] restarting in ${FFMPEG_RESTART_DELAY}ms (attempt ${this.restartCount}/${MAX_RESTARTS})`);
        this.restartTimer = setTimeout(() => this._spawnFFmpeg(), FFMPEG_RESTART_DELAY);
    }

    _onAudioData(chunk) {
        this.buffer = Buffer.concat([this.buffer, chunk]);

        while (this.buffer.length >= CHUNK_SIZE) {
            const frame = this.buffer.subarray(0, CHUNK_SIZE);
            this.buffer = this.buffer.subarray(CHUNK_SIZE);
            this.mux.send(CHANNELS.AUDIO, frame);
        }
    }
}