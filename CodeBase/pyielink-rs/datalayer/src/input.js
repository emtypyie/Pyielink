import { CHANNELS } from "./mux.js";
import { execSync } from "child_process";

const MOUSEEVENTF_MOVE = 0x0001;
const MOUSEEVENTF_LEFTDOWN = 0x0002;
const MOUSEEVENTF_LEFTUP = 0x0004;
const MOUSEEVENTF_RIGHTDOWN = 0x0008;
const MOUSEEVENTF_RIGHTUP = 0x0010;
const MOUSEEVENTF_WHEEL = 0x0800;
const MOUSEEVENTF_ABSOLUTE = 0x8000;

const KEYEVENTF_KEYUP = 0x0002;
const KEYEVENTF_SCANCODE = 0x0008;
const KEYEVENTF_UNICODE = 0x0004;

let screenWidth = 1920;
let screenHeight = 1080;

try {
    const out = execSync('wmic path Win32_VideoController get CurrentHorizontalResolution,CurrentVerticalResolution /value', { encoding: 'utf8' });
    const match = out.match(/CurrentHorizontalResolution=(\d+).*CurrentVerticalResolution=(\d+)/s);
    if (match) {
        screenWidth = parseInt(match[1], 10);
        screenHeight = parseInt(match[2], 10);
    }
} catch {}

function clamp(n, min, max) {
    return Math.max(min, Math.min(max, n));
}

function toAbsolute(x, y) {
    return [
        Math.round((x / screenWidth) * 65535),
        Math.round((y / screenHeight) * 65535)
    ];
}

function sendInputStub(input) {
    console.log(`[input] stub SendInput: type=${input.type}`, input.mi ? `mouse` : `keyboard`);
}

export class InputService {
    constructor(mux, session, log) {
        this.mux = mux;
        this.session = session;
        this.log = log || (() => {});
        this.active = false;
        this.indicatorTimer = null;

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
            if (!this.active) return;

            const events = Array.isArray(msg) ? msg : (msg.events || []);
            if (!Array.isArray(events)) return;

            for (const ev of events) {
                this._injectEvent(ev);
            }
        } catch (e) {
            this.log(`[input] parse error: ${e.message}`);
        }
    }

    _injectEvent(ev) {
        const type = ev.type || (ev.vk !== undefined ? "key" : "mouse");

        if (type === "key" || ev.vk !== undefined) {
            this._injectKey(ev);
        } else if (type === "mouse" || ev.button !== undefined || ev.delta !== undefined) {
            this._injectMouse(ev);
        }
    }

    _injectKey(ev) {
        const vk = ev.vk || 0;
        const scan = ev.scan || 0;
        const flags = ev.flags || 0;

        this.log(`[input] key event: vk=${vk}, scan=${scan}, flags=${flags}`);
    }

    _injectMouse(ev) {
        const flags = ev.flags || 0;
        const isAbsolute = (flags & MOUSEEVENTF_ABSOLUTE) !== 0;

        let x = ev.x || 0;
        let y = ev.y || 0;

        if (isAbsolute) {
            [x, y] = toAbsolute(x, y);
        }

        this.log(`[input] mouse event: type=${ev.type || "move"}, x=${x}, y=${y}, button=${ev.button}, delta=${ev.delta}, flags=${flags}`);
    }

    start() {
        if (this.active) return;
        this.active = true;
        this._showIndicator();
        this.log("[input] capture started");
    }

    stop() {
        if (!this.active) return;
        this.active = false;
        this._hideIndicator();
        this.log("[input] capture stopped");
    }

    _showIndicator() {
        this.log("[input] indicator: input capture active");
    }

    _hideIndicator() {
        this.log("[input] indicator: input capture inactive");
    }
}