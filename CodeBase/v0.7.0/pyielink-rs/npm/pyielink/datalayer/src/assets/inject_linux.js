#!/usr/bin/env node
// Linux host-side input injection (updated with flags support).
//
// Spawned by datalayer/src/input.js. Reads one JSON event per line on stdin
// using the SAME schema as assets/inject.ps1:
//   {"t":"key","vk":65,"up":false,"flags":3}
//   {"t":"mouse","type":"move|ldown|lup|rdown|rup|mdown|mup|wheel","x":N,"y":N,"delta":N,"flags":0}
// Mouse x/y arrive NORMALIZED (0..65535 across the remote screen), we scale
// them to the real display geometry (xdotool getdisplaygeometry) before replay.
//
// Requires `xdotool` on PATH (apt install xdotool / dnf install xdotool ...).
// Keys: Windows VK codes are mapped to xdotool keysyms (best-effort subset).
// Modifier flags (Shift, Ctrl, Alt) are propagated from the remote side.

import { spawnSync } from "node:child_process";

function geom() {
  try {
    const r = spawnSync("xdotool", ["getdisplaygeometry"], { encoding: "utf8" });
    if (r.status === 0) {
      const p = (r.stdout || "").trim().split(/\s+/).map(Number);
      if (p.length === 2 && p[0] > 0 && p[1] > 0) return { w: p[0], h: p[1] };
    }
  } catch {}
  return { w: 1920, h: 1080 };
}
let G = geom();

// Windows VK -> xdotool keysym. Printable A-Z / 0-9 handled generically below.
const VK = {
  8: "BackSpace", 9: "Tab", 13: "Return", 27: "Escape", 32: "space",
  33: "Prior", 34: "Next", 35: "End", 36: "Home",
  37: "Left", 38: "Up", 39: "Right", 40: "Down",
  45: "Insert", 46: "Delete",
  112: "F1", 113: "F2", 114: "F3", 115: "F4", 116: "F5", 117: "F6",
  118: "F7", 119: "F8", 120: "F9", 121: "F10", 122: "F11", 123: "F12",
  160: "Shift_L", 161: "Shift_R", 162: "Control_L", 163: "Control_R",
  164: "Alt_L", 165: "Alt_R", 91: "Super_L", 92: "Super_R",
  186: "colon", 187: "equal", 188: "comma", 189: "minus",
  190: "period", 191: "slash", 192: "grave",
  219: "bracketleft", 220: "backslash", 221: "bracketright",
};

function vkToKeysym(vk) {
  if (VK[vk]) return VK[vk];
  if (vk >= 65 && vk <= 90) return String.fromCharCode(vk).toLowerCase();
  if (vk >= 48 && vk <= 57) return String.fromCharCode(vk);
  return null;
}

function px(nx, ny) {
  const x = Math.round((Number(nx) || 0) / 65535 * (G.w - 1));
  const y = Math.round((Number(ny) || 0) / 65535 * (G.h - 1));
  return [Math.max(0, Math.min(G.w - 1, x)), Math.max(0, Math.min(G.h - 1, y))];
}

function run(args) {
  try { spawnSync("xdotool", args, { stdio: "ignore" }); } catch {}
}

// Parse flags bitmask: bit0=Shift, bit1=Ctrl, bit2=Alt
function parseFlags(flags) {
  const s = !! (flags & 1);  // Shift
  const c = !! (flags & 2);  // Ctrl
  const a = !! (flags & 4);  // Alt
  return { shift: s, ctrl: c, alt: a };
}

function keyDownKeysym(ks, {shift, ctrl, alt}) {
  const mods = [];
  if (shift) mods.push("Shift");
  if (ctrl) mods.push("Ctrl");
  if (alt) mods.push("Alt");
  const m = mods.length > 0 ? "+" + mods.join("+") : "";
  run(["keydown", m, ks]);
  run(["keyup", m, ks]);
}

function keyDownVk(vk, {shift, ctrl, alt}) {
  const ks = vkToKeysym(Number(vk));
  if (ks) {
    keyDownKeysym(ks, {shift, ctrl, alt});
  }
}

function handle(line) {
  line = (line || "").trim();
  if (!line) return;
  let ev;
  try { ev = JSON.parse(line); } catch { return; }
  if (!ev || typeof ev !== "object") return;

  if (ev.t === "mouse") {
    const [x, y] = px(ev.x, ev.y);
    const { shift: haveShift, ctrl: haveCtrl, alt: haveAlt } = parseFlags(ev.flags || 0);
    switch (ev.type) {
      case "move": run(["mousemove", String(x), String(y)]); break;
      case "ldown": run(["mousemove", String(x), String(y)]); 
                    if (haveCtrl) run(["key", "Ctrl_L"]); 
                    run(["mousedown", "1"]); break;
      case "lup": run(["mousemove", String(x), String(y)]); 
                if (haveCtrl) run(["key", "Ctrl_L"]); 
                run(["mouseup", "1"]); break;
      case "rdown": run(["mousemove", String(x), String(y)]); 
                    if (haveShift) run(["key", "Shift_L"]); 
                    run(["mousedown", "3"]); break;
      case "rup": run(["mousemove", String(x), String(y)]); 
                if (haveShift) run(["key", "Shift_L"]); 
                run(["mouseup", "3"]); break;
      case "mdown": run(["mousemove", String(x), String(y)]); 
                    if (haveAlt) run(["key", "Alt_L"]); 
                    run(["mousedown", "2"]); break;
      case "mup": run(["mousemove", String(x), String(y)]); 
                if (haveAlt) run(["key", "Alt_L"]); 
                run(["mouseup", "2"]); break;
      case "wheel": {
        const steps = Math.max(1, Math.min(20, Math.round(Math.abs(Number(ev.delta) || 0) / 120)));
        const btn = (Number(ev.delta) || 0) > 0 ? "5" : "4"; // down / up
        for (let k = 0; k < steps; k++) run(["click", btn]);
        break;
      }
      default: run(["mousemove", String(x), String(y)]);
    }
  } else if (ev.t === "key") {
    const { shift, ctrl, alt } = parseFlags(ev.flags || 0);
    keyDownVk(ev.vk, {shift, ctrl, alt});
  }
}

// Per-platform injector scripts. All speak the SAME JSON-line protocol on stdin
// (see assets/inject.ps1 for the schema) so InputService just writes lines.
function injectorForPlatform() {
  switch (process.platform) {
    case "win32":
      return {
        label: "windows(SendInput)",
        script: path.join(ASSETS, "inject.ps1"),
        cmd: "powershell",
        args: ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", path.join(ASSETS, "inject.ps1")],
      };
    case "linux":
      return {
        label: "linux(xdotool)",
        script: path.join(ASSETS, "inject_linux.js"),
        cmd: "node",
        args: [path.join(ASSETS, "inject_linux.js")],
      };
    case "darwin":
      return {
        label: "macos(cliclick)",
        script: path.join(ASSETS, "inject_mac.js"),
        cmd: "node",
        args: [path.join(ASSETS, "inject_mac.js")],
      };
    default:
      return null;
  }
}

export class InputService {
    constructor(mux, session, log) {
        this.mux = mux;
        this.session = session;
        this.log = log || (() => {});
        this.active = false;
        this.injector = null;
        this.eventCount = 0;

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
                this.eventCount += 1;
                if (this.eventCount === 1 || this.eventCount % 50 === 0) {
                    this.log(`[input] received ${this.eventCount} events (last: ${line.slice(0, 80)})`);
                }
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
        const pick = injectorForPlatform();
        if (!pick) {
            this.log(`[input] no injector for platform ${process.platform}`);
            return;
        }
        if (!existsSync(pick.script)) {
            this.log(`[input] injector missing: ${pick.script} (install ${pick.label})`);
            return;
        }
        this.injector = spawn(pick.cmd, pick.args, { stdio: ["pipe", "ignore", "pipe"] });
        this.injector.stderr.on("data", (d) => {
            const s = d.toString().trim();
            if (s) this.log(`[input] injector: ${s}`);
        });
        this.injector.on("exit", () => { this.injector = null; });
        this.active = true;
        this.log(`[input] capture started (${pick.label})`);
    }

    stop() {
        if (!this.active) return;
        this.active = false;
        if (this.eventCount > 0) this.log(`[input] session total: ${this.eventCount} events`);
        if (this.injector) {
            try { this.injector.kill(); } catch {}
            this.injector = null;
        }
        this.log("[input] capture stopped");
    }
}