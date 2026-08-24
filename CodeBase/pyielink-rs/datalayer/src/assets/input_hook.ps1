param(
  [string]$ProcName = "ffplay",
  [Parameter(Mandatory = $true)][int]$UdpPort
)
# Client-side input capture for the ffplay viewer.
#
# While the player window is hovered (foreground + cursor over its CLIENT
# area), low-level hooks translate mouse/keyboard activity into normalized
# events (coords scaled to 0..65535) and fire them at a localhost UDP port,
# where client_view.js relays them onto the data layer INPUT channel.
#
# Button/wheel/key events are SWALLOWED while captured so ffplay itself
# never reacts (its hotkeys would toggle pause/fullscreen/quit). Mouse
# MOVES are never swallowed - that would break system cursor motion.

$ErrorActionPreference = 'SilentlyContinue'

Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class PIKHook {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int x, y; }
  [StructLayout(LayoutKind.Sequential)]
  public struct KBDLLHOOKSTRUCT { public uint vkCode, scanCode, flags, time; public IntPtr extra; }
  [StructLayout(LayoutKind.Sequential)]
  public struct MSLLHOOKSTRUCT { public POINT pt; public uint mouseData, flags, time; public IntPtr extra; }
  [StructLayout(LayoutKind.Sequential)]
  public struct MSG { public IntPtr hwnd; public uint message; public IntPtr wParam, lParam; public uint time; public POINT pt; }

  public delegate IntPtr HookProc(int code, IntPtr wParam, IntPtr lParam);

  [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] static extern IntPtr SetWindowsHookEx(int id, HookProc p, IntPtr mod, uint tid);
  [DllImport("user32.dll")] static extern bool UnhookWindowsHookEx(IntPtr h);
  [DllImport("user32.dll")] static extern IntPtr CallNextHookEx(IntPtr h, int c, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] static extern int GetMessage(out MSG m, IntPtr w, uint a, uint b);
  [DllImport("user32.dll")] static extern bool PostThreadMessage(uint tid, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr w, out RECT r);
  [DllImport("user32.dll")] static extern bool ClientToScreen(IntPtr w, ref POINT p);
  [DllImport("user32.dll")] static extern bool IsWindow(IntPtr w);

  const int WH_MOUSE_LL = 14, WH_KEYBOARD_LL = 13;
  const int WM_QUIT = 0x0012;
  const int WM_MOUSEMOVE = 0x200, WM_LBUTTONDOWN = 0x201, WM_LBUTTONUP = 0x202,
            WM_RBUTTONDOWN = 0x204, WM_RBUTTONUP = 0x205, WM_MOUSEWHEEL = 0x20A;
  const int WM_KEYDOWN = 0x100, WM_KEYUP = 0x101, WM_SYSKEYDOWN = 0x104, WM_SYSKEYUP = 0x105;

  static UdpClient udp;
  static IntPtr target = IntPtr.Zero;
  static IntPtr mouseHook = IntPtr.Zero, kbdHook = IntPtr.Zero;
  static int lastMoveTick = 0;

  public static void Run(string procName, int port) {
    udp = new UdpClient();
    udp.Connect("127.0.0.1", port);

    for (int i = 0; i < 120 && !FindTarget(procName); i++) Thread.Sleep(250);
    if (target == IntPtr.Zero) return;

    mouseHook = SetWindowsHookEx(WH_MOUSE_LL, MouseProc, IntPtr.Zero, 0);
    kbdHook = SetWindowsHookEx(WH_KEYBOARD_LL, KbdProc, IntPtr.Zero, 0);

    uint tid = GetCurrentThreadId();
    new Thread(() => {
      while (IsWindow(target)) Thread.Sleep(500);
      PostThreadMessage(tid, WM_QUIT, IntPtr.Zero, IntPtr.Zero);
    }) { IsBackground = true }.Start();

    MSG m;
    while (GetMessage(out m, IntPtr.Zero, 0, 0)) { }

    if (mouseHook != IntPtr.Zero) UnhookWindowsHookEx(mouseHook);
    if (kbdHook != IntPtr.Zero) UnhookWindowsHookEx(kbdHook);
    udp.Close();
  }

  static bool FindTarget(string procName) {
    foreach (Process p in Process.GetProcessesByName(procName)) {
      try {
        if (p.MainWindowHandle != IntPtr.Zero && p.MainWindowTitle.Length > 0) {
          target = p.MainWindowHandle;
          return true;
        }
      } catch {}
    }
    return false;
  }

  // True when the cursor is over the target's CLIENT area and it is focused.
  static bool Capturing(POINT pt) {
    if (target == IntPtr.Zero || !IsWindow(target)) return false;
    if (GetForegroundWindow() != target) return false;
    RECT r;
    if (!GetClientRect(target, out r)) return false;
    POINT o; o.x = 0; o.y = 0;
    if (!ClientToScreen(target, ref o)) return false;
    return pt.x >= o.x && pt.x < o.x + (r.R - r.L) &&
           pt.y >= o.y && pt.y < o.y + (r.B - r.T);
  }

  static void SendJson(string s) {
    byte[] b = Encoding.UTF8.GetBytes(s);
    try { udp.Send(b, b.Length); } catch {}
  }

  static void SendMouse(string type, int sx, int sy, int wheelDelta) {
    RECT r;
    if (!GetClientRect(target, out r)) return;
    POINT o; o.x = 0; o.y = 0;
    if (!ClientToScreen(target, ref o)) return;
    int cw = r.R - r.L, ch = r.B - r.T;
    if (cw < 1 || ch < 1) return;
    int nx = (sx - o.x) * 65535 / cw;
    int ny = (sy - o.y) * 65535 / ch;
    if (nx < 0) nx = 0; if (nx > 65535) nx = 65535;
    if (ny < 0) ny = 0; if (ny > 65535) ny = 65535;
    string j = "{\"t\":\"mouse\",\"type\":\"" + type + "\",\"x\":" + nx +
               ",\"y\":" + ny + (wheelDelta != 0 ? ",\"delta\":" + wheelDelta : "") + "}";
    SendJson(j);
  }

  static IntPtr MouseProc(int code, IntPtr wParam, IntPtr lParam) {
    if (code >= 0) {
      int msg = wParam.ToInt32();
      MSLLHOOKSTRUCT info = (MSLLHOOKSTRUCT)Marshal.PtrToStructure(lParam, typeof(MSLLHOOKSTRUCT));
      if (Capturing(info.pt)) {
        switch (msg) {
          case WM_MOUSEMOVE: {
            // Throttle to ~60 Hz; NEVER swallow moves (cursor must keep moving).
            int now = Environment.TickCount;
            if (now - lastMoveTick >= 15) {
              lastMoveTick = now;
              SendMouse("move", info.pt.x, info.pt.y, 0);
            }
            break;
          }
          case WM_LBUTTONDOWN: SendMouse("ldown", info.pt.x, info.pt.y, 0); return (IntPtr)1;
          case WM_LBUTTONUP:   SendMouse("lup",   info.pt.x, info.pt.y, 0); return (IntPtr)1;
          case WM_RBUTTONDOWN: SendMouse("rdown", info.pt.x, info.pt.y, 0); return (IntPtr)1;
          case WM_RBUTTONUP:   SendMouse("rup",   info.pt.x, info.pt.y, 0); return (IntPtr)1;
          case WM_MOUSEWHEEL: {
            short d = (short)((info.mouseData >> 16) & 0xFFFF);
            SendMouse("wheel", info.pt.x, info.pt.y, d);
            return (IntPtr)1;
          }
        }
      }
    }
    return CallNextHookEx(IntPtr.Zero, code, wParam, lParam);
  }

  static IntPtr KbdProc(int code, IntPtr wParam, IntPtr lParam) {
    if (code >= 0) {
      int msg = wParam.ToInt32();
      KBDLLHOOKSTRUCT k = (KBDLLHOOKSTRUCT)Marshal.PtrToStructure(lParam, typeof(KBDLLHOOKSTRUCT));
      // Let Alt / Win combos through untouched (Alt-Tab, Win-L, ...) - do
      // not capture, do not swallow.
      uint vk = k.vkCode;
      bool modifierPass = (vk >= 0xA0 && vk <= 0xA5) || vk == 0x5B || vk == 0x5C ||
                          vk == 0xA5 || vk == 0x38 || vk == 0xA4;
      if (!modifierPass && IsWindow(target) && GetForegroundWindow() == target) {
        bool up = (msg == WM_KEYUP || msg == WM_SYSKEYUP);
        SendJson("{\"t\":\"key\",\"vk\":" + vk + ",\"up\":" + (up ? "true" : "false") + "}");
        return (IntPtr)1;  // swallow: ffplay must never see q/space/f/esc
      }
    }
    return CallNextHookEx(IntPtr.Zero, code, wParam, lParam);
  }
}
'@

[PIKHook]::Run($ProcName, $UdpPort)
