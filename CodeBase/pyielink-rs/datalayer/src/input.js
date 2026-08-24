// Host-side input service: receives normalized input events on the INPUT
// channel and feeds them to assets/inject.ps1, a long-lived PowerShell
// helper holding a compiled SendInput P/Invoke. One JSON event per line on
// the injector's stdin; it dies when stdin closes.
//
// Mouse coordinates arrive NORMALIZED (0..65535 across the remote screen),
// which is exactly what MOUSEEVENTF_ABSOLUTE consumes - passed through 1:1.

import { CHANNELS } from "./mux.js";
import { spawn } from "child_process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

const INJECTOR = fileURLToPath(new URL("./assets/inject.ps1", import.meta.url));

export class InputService {
    constructor(mux, session, log) {
        this.mux = mux;
        this.session = session;
        this.log = log || (() => {});
        this.active = false;
        this.injector = null;

        mux.on(CHANNELS.INPUT, (payload) => this._handleInput(payload));
    }

    _handleInput(payload) {
        try {
            const msg = JSON.parse(payload.toString("utf8"));
            if (msg.t === "input_start") {
                this.start();
                return;
            }
            if (msg.t === "input_stop") {
                this.stop();
                return;
            }
            if (!this.active || !this.injector) return;

            // Accept a single event object, an array, or { events: [...] }.
            let events;
            if (Array.isArray(msg)) events = msg;
            else if (Array.isArray(msg.events)) events = msg.events;
            else if (msg.t === "key" || msg.t === "mouse") events = [msg];
            else return;

            for (const ev of events) {
                const line = JSON.stringify(ev);
                try {
                    this.injector.stdin.write(line + "\n");
                } catch {
                    /* injector died mid-session; stop() will clean up */
                }
            }
        } catch (e) {
            this.log(`[input] parse error: ${e.message}`);
        }
    }

    start() {
        if (this.active) return;
        if (!existsSync(INJECTOR)) {
            this.log(`[input] injector missing: ${INJECTOR}`);
            return;
        }
        this.injector = spawn(
            "powershell",
            ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", INJECTOR],
            { stdio: ["pipe", "ignore", "pipe"] }
        );
        this.injector.stderr.on("data", (d) => {
            const s = d.toString().trim();
            if (s) this.log(`[input] injector: ${s}`);
        });
        this.injector.on("exit", () => { this.injector = null; });
        this.active = true;
        this.log("[input] capture started");
    }

    stop() {
        if (!this.active) return;
        this.active = false;
        if (this.injector) {
            try { this.injector.kill(); } catch {}
            this.injector = null;
        }
        this.log("[input] capture stopped");
    }
}
