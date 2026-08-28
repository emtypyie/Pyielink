#!/usr/bin/env python3
# Cross-platform client-side input capture for the pyielink ffplay viewer.
#
# Mirrors assets/input_hook.ps1: while the cursor is over the ffplay window we
# translate mouse/keyboard activity into NORMALIZED events (coords 0..65535)
# and fire them as one JSON object per UDP datagram at 127.0.0.1:<port>, where
# client_view.js relays them onto the data layer INPUT channel.
#
#   Linux  : X11 global tap via Xlib + the XRecord extension (pip install xlib)
#   macOS  : Quartz CGEvent tap                    (pip install pyobjc)
#
# Key events are translated to Windows VK codes so the host injector (which
# speaks VK on Windows, and maps VK->keysym on Linux/mac) can replay them.
# Only a common subset is mapped; unhandled keys are logged and dropped.
#
# NOTE: this is a scaffold. Swallowing captured keys from the local ffplay is
# NOT done cross-platform yet (so ffplay may also react to q/space/f); the
# events are still forwarded to the host.

import argparse
import json
import socket
import sys
import time
import threading

TRACE = ""


def trace(msg):
    if TRACE:
        try:
            with open(TRACE, "a") as f:
                f.write(time.strftime("%H:%M:%S.fff") + " " + msg + "\n")
        except Exception:
            pass


def send_udp(sock, obj):
    try:
        sock.sendto(json.dumps(obj).encode("utf-8"), ("127.0.0.1", UDP_PORT))
    except Exception as e:
        trace("udp send: " + str(e))


UDP_PORT = 0
WIN = {"x": 0, "y": 0, "w": 1, "h": 1, "ok": False}
WIN_LOCK = threading.Lock()


# ---- window discovery -------------------------------------------------------
def refresh_window_linux(proc):
    try:
        import subprocess
        out = subprocess.run(["xdotool", "search", "--class", proc],
                             capture_output=True, text=True, timeout=2)
        wid = (out.stdout or "").strip().split("\n")[0]
        if not wid:
            return
        geo = subprocess.run(["xdotool", "getwindowgeometry", "--shell", wid],
                             capture_output=True, text=True, timeout=2).stdout
        d = {}
        for line in geo.splitlines():
            if "=" in line:
                k, v = line.split("=", 1)
                d[k.strip()] = v.strip()
        with WIN_LOCK:
            WIN["x"] = int(d.get("X", 0))
            WIN["y"] = int(d.get("Y", 0))
            WIN["w"] = max(1, int(d.get("WIDTH", 1)))
            WIN["h"] = max(1, int(d.get("HEIGHT", 1)))
            WIN["ok"] = True
    except Exception as e:
        trace("linux win: " + str(e))


def refresh_window_mac(proc):
    try:
        from Quartz import CGWindowListCopyWindowInfo, kCGWindowListOptionOnScreenOnly
        wins = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, 0)
        for w in wins:
            owner = (w.get("kCGWindowOwnerName") or "")
            if proc.lower() in owner.lower() or "ffplay" in owner.lower():
                b = w.get("kCGWindowBounds") or {}
                with WIN_LOCK:
                    WIN["x"] = int(b.get("X", 0))
                    WIN["y"] = int(b.get("Y", 0))
                    WIN["w"] = max(1, int(b.get("Width", 1)))
                    WIN["h"] = max(1, int(b.get("Height", 1)))
                    WIN["ok"] = True
                return
    except Exception as e:
        trace("mac win: " + str(e))


def refresh_loop(proc):
    while True:
        try:
            if sys.platform.startswith("linux"):
                refresh_window_linux(proc)
            elif sys.platform == "darwin":
                refresh_window_mac(proc)
        except Exception:
            pass
        time.sleep(0.5)


# ---- keycode -> Windows VK (common subset) ----------------------------------
VK_MAP_LINUX = {
    24: 0x4F, 25: 0x50, 26: 0x51,  # O P [
    27: 0xDB, 28: 0xDC, 29: 0xDD, 30: 0x41, 31: 0x42,  # ] \ A B
    32: 0x43, 33: 0x44, 34: 0x45, 35: 0x46, 36: 0x47, 37: 0x48, 38: 0x49,
    39: 0x4A, 40: 0x4B, 41: 0x4C, 42: 0x4D, 43: 0x4E, 44: 0x4D,  # M
    45: 0x4E, 46: 0x4B, 47: 0x49, 48: 0x4F,  # N K I O
    # digits row
    10: 0x31, 11: 0x32, 12: 0x33, 13: 0x34, 14: 0x35,
    15: 0x36, 16: 0x37, 17: 0x38, 18: 0x39, 19: 0x30,
    22: 0x51,  # backspace
    36: 0x0D,  # enter
    37: 0x5A, 38: 0x58, 39: 0x43, 40: 0x56, 41: 0x42,  # z x c v b
    44: 0x20,  # space
    50: 0x10, 62: 0x10,  # shift L/R
    37: 0x5A,  # (dup)
    64: 0x14,  # ctrl (altgr handled separately)
    108: 0x12,  # alt gr
    113: 0x71,  # f2 ... (rough)
    9: 0x1B,   # esc
    111: 0x27, 114: 0x28, 113: 0x25, 116: 0x26,  # ins/end/vol/up etc (approx)
    23: 0x54,  # tab
}

VK_MAP_MAC = {
    0: 0x41, 1: 0x53, 2: 0x44, 3: 0x46, 4: 0x48, 5: 0x47, 6: 0x5A, 7: 0x58,
    8: 0x43, 9: 0x56, 11: 0x42, 12: 0x51, 13: 0x57, 14: 0x45, 15: 0x52,
    16: 0x59, 17: 0x55, 18: 0x54, 19: 0x5A, 20: 0x58, 21: 0x58,  # approx
    49: 0x20,  # space
    36: 0x0D,  # return
    48: 0x09,  # tab
    51: 0x08,  # delete (backspace)
    53: 0x1B,  # esc
    123: 0x25, 124: 0x27, 125: 0x28, 126: 0x26,  # arrows
    55: 0x5B, 56: 0xA0, 58: 0xA4, 59: 0xA2,  # cmd/shift/opt/ctrl
}


def linux_keycode_to_vk(code):
    return VK_MAP_LINUX.get(code)


def mac_keycode_to_vk(code):
    return VK_MAP_MAC.get(code)


def in_window(px, py):
    with WIN_LOCK:
        if not WIN["ok"]:
            return False
        return (WIN["x"] <= px < WIN["x"] + WIN["w"] and
                WIN["y"] <= py < WIN["y"] + WIN["h"])


def normalize(px, py):
    with WIN_LOCK:
        x = WIN["x"]; y = WIN["y"]; w = WIN["w"]; h = WIN["h"]
    nx = int((px - x) * 65535 / w)
    ny = int((py - y) * 65535 / h)
    return max(0, min(65535, nx)), max(0, min(65535, ny))


# ---- Linux capture (XRecord) ------------------------------------------------
def capture_linux(sock):
    from Xlib import display
    from Xlib.ext import record
    from Xlib.protocol import rq

    local = display.Display()
    rec = display.Display()

    def handler(reply):
        if reply.category != record.FromServer or reply.client_swapped:
            return
        data = reply.data
        if not data:
            return
        # Parse a single XRecord event (8-byte header + body).
        ev_type = data[0]
        if ev_type == 2:  # KeyPress
            code = data[1]
            vk = linux_keycode_to_vk(code)
            if vk is not None:
                send_udp(sock, {"t": "key", "vk": vk, "up": False})
        elif ev_type == 3:  # KeyRelease
            code = data[1]
            vk = linux_keycode_to_vk(code)
            if vk is not None:
                send_udp(sock, {"t": "key", "vk": vk, "up": True})
        elif ev_type == 4:  # ButtonPress
            code = data[1]
            px = int.from_bytes(data[2:4], "little")
            py = int.from_bytes(data[4:6], "little")
            if not in_window(px, py):
                return
            nx, ny = normalize(px, py)
            mtype = {1: "ldown", 3: "rdown", 2: "mdown"}.get(code, "ldown")
            send_udp(sock, {"t": "mouse", "type": mtype, "x": nx, "y": ny})
        elif ev_type == 5:  # ButtonRelease
            code = data[1]
            px = int.from_bytes(data[2:4], "little")
            py = int.from_bytes(data[4:6], "little")
            nx, ny = normalize(px, py) if in_window(px, py) else (0, 0)
            mtype = {1: "lup", 3: "rup", 2: "mup"}.get(code, "lup")
            send_udp(sock, {"t": "mouse", "type": mtype, "x": nx, "y": ny})
        elif ev_type == 6:  # MotionNotify
            px = int.from_bytes(data[2:4], "little")
            py = int.from_bytes(data[4:6], "little")
            if not in_window(px, py):
                return
            nx, ny = normalize(px, py)
            send_udp(sock, {"t": "mouse", "type": "move", "x": nx, "y": ny})

    ctx = rec.record_create_context(
        0, [record.AllClients],
        [{"core_requests": (0, 0), "core_events": (0, 65535), "errors": (0, 0), "client_started": False, "client_died": False}])
    rec.record_enable_context(ctx, handler)
    rec.record_free_context(ctx)


# ---- macOS capture (Quartz CGEvent tap) ------------------------------------
def capture_mac(sock):
    from Quartz import (CGEventTapCreate, kCGSessionEventTap, kCGHeadInsertEventTap,
                        CGEventTapEnable, CFRunLoopRun, kCGHIDEventTap,
                        kCGEventMaskMouse, kCGEventMaskKeyDown, kCGEventMaskKeyUp, kCGEventMaskFlagsChanged,
                        CGEventGetLocation, CGEventGetIntegerValueField, kCGKeyboardEventKeycode,
                        kCGEventMouseMoved, kCGEventLeftMouseDown, kCGEventLeftMouseUp,
                        kCGEventRightMouseDown, kCGEventRightMouseUp, kCGEventOtherMouseDown,
                        kCGEventOtherMouseUp, kCGEventScrollWheel)
    mask = kCGEventMaskMouse | kCGEventMaskKeyDown | kCGEventMaskKeyUp | kCGEventMaskFlagsChanged

    def cb(event, kind):
        try:
            loc = CGEventGetLocation(event)
            px, py = int(loc.x), int(loc.y)
            if kind in (kCGEventMouseMoved, kCGEventLeftMouseDown, kCGEventLeftMouseUp,
                        kCGEventRightMouseDown, kCGEventRightMouseUp, kCGEventOtherMouseDown,
                        kCGEventOtherMouseUp, kCGEventScrollWheel):
                if not in_window(px, py) and kind != kCGEventScrollWheel:
                    return event
                nx, ny = normalize(px, py) if in_window(px, py) else (0, 0)
                if kind == kCGEventMouseMoved:
                    send_udp(sock, {"t": "mouse", "type": "move", "x": nx, "y": ny})
                elif kind == kCGEventLeftMouseDown:
                    send_udp(sock, {"t": "mouse", "type": "ldown", "x": nx, "y": ny})
                elif kind == kCGEventLeftMouseUp:
                    send_udp(sock, {"t": "mouse", "type": "lup", "x": nx, "y": ny})
                elif kind == kCGEventRightMouseDown:
                    send_udp(sock, {"t": "mouse", "type": "rdown", "x": nx, "y": ny})
                elif kind == kCGEventRightMouseUp:
                    send_udp(sock, {"t": "mouse", "type": "rup", "x": nx, "y": ny})
                elif kind == kCGEventOtherMouseDown:
                    send_udp(sock, {"t": "mouse", "type": "mdown", "x": nx, "y": ny})
                elif kind == kCGEventOtherMouseUp:
                    send_udp(sock, {"t": "mouse", "type": "mup", "x": nx, "y": ny})
                elif kind == kCGEventScrollWheel:
                    d = CGEventGetIntegerValueField(event, 1) if False else 0
                    try:
                        d = CGEventGetIntegerValueField(event, 11)  # kCGScrollWheelEventDeltaAxis1
                    except Exception:
                        d = 0
                    if d != 0:
                        # delta sign: macOS up is positive; map to host delta
                        send_udp(sock, {"t": "mouse", "type": "wheel", "x": nx, "y": ny, "delta": -int(d) * 120})
            else:
                vk = mac_keycode_to_vk(CGEventGetIntegerValueField(event, kCGKeyboardEventKeycode))
                if vk is not None:
                    up = kind in (kCGEventKeyUp,)
                    send_udp(sock, {"t": "key", "vk": vk, "up": up})
        except Exception as e:
            trace("mac cb: " + str(e))
        return event

    tap = CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap,
                          0, kCGEventMaskMouse | kCGEventMaskKeyDown | kCGEventMaskKeyUp | kCGEventMaskFlagsChanged,
                          cb, None)
    if not tap:
        trace("mac: CGEventTapCreate failed (need accessibility permission)")
        return
    CGEventTapEnable(tap, True)
    CFRunLoopRun()


def main():
    global UDP_PORT, TRACE
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--proc", default="ffplay")
    ap.add_argument("--trace", default="")
    args = ap.parse_args()
    UDP_PORT = args.port
    TRACE = args.trace
    trace("start py=%d proc=%s" % (UDP_PORT, args.proc))

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

    # Wait for the viewer window.
    for _ in range(60):
        if sys.platform.startswith("linux"):
            refresh_window_linux(args.proc)
        elif sys.platform == "darwin":
            refresh_window_mac(args.proc)
        with WIN_LOCK:
            ok = WIN["ok"]
        if ok:
            break
        time.sleep(0.25)
    if not ok:
        trace("window never appeared")
        return

    threading.Thread(target=refresh_loop, args=(args.proc,), daemon=True).start()

    if sys.platform.startswith("linux"):
        capture_linux(sock)
    elif sys.platform == "darwin":
        capture_mac(sock)
    else:
        trace("unsupported platform")


if __name__ == "__main__":
    main()
