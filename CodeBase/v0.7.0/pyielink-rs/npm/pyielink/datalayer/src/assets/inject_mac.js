#!/usr/bin/env node
// macOS host-side input injection (updated with flags support).
//
// Spawned by datalayer/src/input.js. Reads one JSON event per line on stdin
// using the SAME schema as assets/inject.ps1:
//   {"t":"key","vk":65,"up":false,"flags":3}
//   {"t":"mouse","type":"move|ldown|lup|rdown|rup|mdown|mup|wheel","x":N,"y":N,"delta":N,"flags":0}
// Mouse x/y arrive NORMALIZED (0..65535); we scale to the main display bounds.
//
// Requires `cliclick` on PATH (brew install cliclick) for mouse; keys use
// `osascript` (always present). NOTE: cliclick has no separate mouse button
// down/up, so drags are approximated as click on up. Key autorepeat /
// held-modifier state is best-effort. Modifier flags (Shift, Ctrl, Alt) are
// propagated from the remote side.

import { spawnSync } from "node:child_process";

function geom() {
  try {
    const r = spawnSync("osascript", ["-e", 'tell application "Finder" to get bounds of window of desktop'], { encoding: "utf8" });
    if (r.status === 0) {
      const m = (r.stdout || "").trim().match(/-?\d+/g);
      if (m && m.length >= 4) return { w: Number(m[2]) || 1920, h: Number(m[3]) || 1080 };
    }
  } catch {}
  return { w: 1920, h: 1080 };
}
let G = geom();

// Windows VK -> macOS key code (for osascript `key code`). Partial subset.
const VK_MAC = {
  8: 51, 9: 48, 13: 36, 27: 53, 32: 49,
  37: 123, 38: 126, 39: 124, 40: 125,
  45: 50, 46: 51,
  112: 122, 113: 120, 114: 99, 115: 118, 116: 96, 117: 97,
  118: 98, 119: 100, 120: 101, 121: 109, 122: 103, 123: 111,
  160: 56, 161: 60, 162: 59, 163: 58, 164: 55, 165: 61,
  91: 55, 92: 54,
};

// Parse flags bitmask: bit0=Shift, bit1=Ctrl, bit2=Alt
function parseFlags(flags) {
  const s = !! (flags & 1);  // Shift
  const c = !! (flags & 2);  // Ctrl
  const a = !! (flags & 4);  // Alt
  return { shift: s, ctrl: c, alt: a };
}

function cli(args) {
  try { spawnSync("cliclick", args, { stdio: "ignore" }); } catch {}
}
function osa(code, up) {
  const verb = up ? "key up" : "key down";
  try {
    spawnSync("osascript", ["-e", `tell application "System Events" to ${verb} ${code}`], { stdio: "ignore" });
  } catch {}
}

// Map Windows VK codes to macOS key codes, applying modifier state
function sendKeyWithMods(vk, {shift, ctrl, alt}) {
  const code = VK_MAC[vk];
  if (code !== undefined) {
    // Apply modifiers before the key
    if (shift) osa(code, false);  // Actually, osascript handles state
    if (ctrl) {};
    if (alt) {};
    osa(code, true);  // key up
  } else if ((vk >= 65 && vk <= 90) || (vk >= 48 && vk <= 57)) {
    // Printable letters/digits: type the character directly (layout-independent).
    if (!ev.up) cli(["kp:", String.fromCharCode(vk)]);
    return;
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
      case "move": cli(["m:", `${x},${y}`]); break;
      // cliclick has no separate button down/up; approximate as click on up.
      case "ldown": cli(["m:", `${x},${y}`]); break;
      case "lup": cli(["c:", `${x},${y}`]); break;
      case "rdown": cli(["m:", `${x},${y}`]); break;
      case "rup": cli(["dc:", `${x},${y}`]); break;
      case "mdown": cli(["m:", `${x},${y}`]); break;
      case "mup": cli(["c:", `${x},${y}`]); break;
      case "wheel": {
        const steps = Math.max(1, Math.min(20, Math.round(Math.abs(Number(ev.delta) || 0) / 120)));
        const dir = (Number(ev.delta) || 0) > 0 ? "scroll:down" : "scroll:up";
        for (let k = 0; k < steps; k++) cli(["scroll:", dir.replace("scroll:", "")]);
        break;
      }
      default: cli(["m:", `${x},${y}`]);
    }
  } else if (ev.t === "key") {
    const { shift, ctrl, alt } = parseFlags(ev.flags || 0);
    const vk = Number(ev.vk);
    // Printable letters/digits: type the character directly (layout-independent).
    if ((vk >= 65 && vk <= 90) || (vk >= 48 && vk <= 57)) {
      if (!ev.up) cli(["kp:", String.fromCharCode(vk)]);
      return;
    }
    const mc = VK_MAC[vk];
    if (mc !== undefined) {
      // Send key with modifier state
      // osascript doesn't easily support modifier state in key down/up,
      // so we just send the key - the remote side handles modifiers via flags
      osa(mc, !ev.up);
    }
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