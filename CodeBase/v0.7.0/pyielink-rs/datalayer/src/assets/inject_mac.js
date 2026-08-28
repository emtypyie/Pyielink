#!/usr/bin/env node
// macOS host-side input injection (scaffold, best-effort).
//
// Spawned by datalayer/src/input.js. Reads one JSON event per line on stdin
// using the SAME schema as assets/inject.ps1:
//   {"t":"key","vk":65,"up":false}
//   {"t":"mouse","type":"move|ldown|lup|rdown|rup|mdown|mup|wheel","x":N,"y":N,"delta":N}
// Mouse x/y arrive NORMALIZED (0..65535); we scale to the main display bounds.
//
// Requires `cliclick` on PATH (brew install cliclick) for mouse; keys use
// `osascript` (always present). NOTE: cliclick has no separate mouse button
// down/up, so drags are approximated as click on button-up. Key autorepeat /
// held-modifier state is best-effort.

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

function px(nx, ny) {
  const x = Math.round((Number(nx) || 0) / 65535 * (G.w - 1));
  const y = Math.round((Number(ny) || 0) / 65535 * (G.h - 1));
  return [Math.max(0, Math.min(G.w - 1, x)), Math.max(0, Math.min(G.h - 1, y))];
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

let buf = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (d) => { buf += d; let i; while ((i = buf.indexOf("\n")) >= 0) { const l = buf.slice(0, i); buf = buf.slice(i + 1); handle(l); } });
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
    const vk = Number(ev.vk);
    // Printable letters/digits: type the character directly (layout-independent).
    if ((vk >= 65 && vk <= 90) || (vk >= 48 && vk <= 57)) {
      if (!ev.up) cli(["kp:", String.fromCharCode(vk)]);
      return;
    }
    const mc = VK_MAC[vk];
    if (mc !== undefined) osa(mc, ev.up);
  }
}
