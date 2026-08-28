#!/usr/bin/env node
// Linux host-side input injection (scaffold).
//
// Spawned by datalayer/src/input.js. Reads one JSON event per line on stdin
// using the SAME schema as assets/inject.ps1:
//   {"t":"key","vk":65,"up":false}
//   {"t":"mouse","type":"move|ldown|lup|rdown|rup|mdown|mup|wheel","x":N,"y":N,"delta":N}
// Mouse x/y arrive NORMALIZED to 0..65535 across the remote screen; we scale
// them to the real display geometry (xdotool getdisplaygeometry) before replay.
//
// Requires `xdotool` on PATH (apt install xdotool / dnf install xdotool ...).
// Keys: Windows VK codes are mapped to xdotool keysyms (best-effort subset).

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

let buf = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    handle(line);
  }
});
process.stdin.on("end", () => process.exit(0));

function handle(line) {
  line = (line || "").trim();
  if (!line) return;
  let ev;
  try { ev = JSON.parse(line); } catch { return; }
  if (!ev || typeof ev !== "object") return;

  if (ev.t === "mouse") {
    const [x, y] = px(ev.x, ev.y);
    switch (ev.type) {
      case "move": run(["mousemove", String(x), String(y)]); break;
      case "ldown": run(["mousemove", String(x), String(y)]); run(["mousedown", "1"]); break;
      case "lup": run(["mousemove", String(x), String(y)]); run(["mouseup", "1"]); break;
      case "rdown": run(["mousemove", String(x), String(y)]); run(["mousedown", "3"]); break;
      case "rup": run(["mousemove", String(x), String(y)]); run(["mouseup", "3"]); break;
      case "mdown": run(["mousemove", String(x), String(y)]); run(["mousedown", "2"]); break;
      case "mup": run(["mousemove", String(x), String(y)]); run(["mouseup", "2"]); break;
      case "wheel": {
        const steps = Math.max(1, Math.min(20, Math.round(Math.abs(Number(ev.delta) || 0) / 120)));
        const btn = (Number(ev.delta) || 0) > 0 ? "5" : "4"; // down / up
        for (let k = 0; k < steps; k++) run(["click", btn]);
        break;
      }
      default: run(["mousemove", String(x), String(y)]);
    }
  } else if (ev.t === "key") {
    const ks = vkToKeysym(Number(ev.vk));
    if (ks) run(ev.up ? ["keyup", ks] : ["keydown", ks]);
  }
}
