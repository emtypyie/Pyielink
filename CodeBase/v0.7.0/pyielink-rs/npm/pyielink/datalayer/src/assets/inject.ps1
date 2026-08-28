# Host-side input injection engine.
#
# Spawned by datalayer/src/input.js. Reads one JSON event per line on stdin:
#   {"t":"key","vk":65,"up":false}
#   {"t":"mouse","type":"move|ldown|lup|rdown|rup|mdown|mup|wheel","x":N,"y":N,"delta":N}
# Mouse x/y arrive NORMALIZED to 0..65535 across the remote screen, which is
# exactly what SendInput's MOUSEEVENTF_ABSOLUTE expects - pass through 1:1.
# Exits when stdin closes (input.js kills it or the session ends).

$ErrorActionPreference = 'SilentlyContinue'

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class PIKInj {
  [StructLayout(LayoutKind.Sequential)]
  struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr extra; }
  [StructLayout(LayoutKind.Sequential)]
  struct KEYBDINPUT { public ushort wVk, wScan; public uint dwFlags, time; public IntPtr extra; }
  [StructLayout(LayoutKind.Explicit)]
  struct INPUTUNION { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
  [StructLayout(LayoutKind.Sequential)]
  struct INPUT { public uint type; public INPUTUNION u; }

  [DllImport("user32.dll", SetLastError = true)]
  static extern uint SendInput(uint n, INPUT[] inputs, int size);

  const uint INPUT_MOUSE = 0, INPUT_KEYBOARD = 1;
  const uint F_MOVE = 0x0001, F_LEFTDOWN = 0x0002, F_LEFTUP = 0x0004,
             F_RIGHTDOWN = 0x0008, F_RIGHTUP = 0x0010, F_MIDDLEDOWN = 0x0020, F_MIDDLEUP = 0x0040,
             F_WHEEL = 0x0800, F_ABSOLUTE = 0x8000, F_VIRTUALDESK = 0x4000;
  const uint F_KEYUP = 0x0002;

  // Marker written into dwExtraInfo of every injected event. The client-side
  // low-level hooks check for it and ignore our own injections - otherwise
  // capture -> inject -> recapture becomes an exponential feedback loop.
  public static readonly IntPtr EXTRA_MAGIC = (IntPtr)0x50494B31;  // "PIK1"

  // flags: full SendInput flag set for this event; data: wheel delta bits.
  public static uint Mouse(int nx, int ny, uint flags, uint data) {
    INPUT[] a = new INPUT[1];
    a[0].type = INPUT_MOUSE;
    a[0].u.mi.dx = nx;
    a[0].u.mi.dy = ny;
    a[0].u.mi.mouseData = data;
    a[0].u.mi.dwFlags = flags;
    a[0].u.mi.extra = EXTRA_MAGIC;
    return SendInput(1, a, Marshal.SizeOf(typeof(INPUT)));
  }

  public static uint Key(ushort vk, bool up) {
    INPUT[] a = new INPUT[1];
    a[0].type = INPUT_KEYBOARD;
    a[0].u.ki.wVk = vk;
    a[0].u.ki.extra = EXTRA_MAGIC;
    if (up) a[0].u.ki.dwFlags = F_KEYUP;
    return SendInput(1, a, Marshal.SizeOf(typeof(INPUT)));
  }
}
'@

while ($true) {
  $line = [Console]::In.ReadLine()
  if ($null -eq $line) { break }
  $line = $line.Trim()
  if ($line.Length -eq 0) { continue }
  try {
    $ev = $line | ConvertFrom-Json
    if ($ev.t -eq 'key') {
      [PIKInj]::Key([uint16][int]$ev.vk, [bool]$ev.up) | Out-Null
    } elseif ($ev.t -eq 'mouse') {
      $x=[int]$ev.x; $y=[int]$ev.y
      switch ($ev.type) {
        'move'  { [PIKInj]::Mouse($x,$y,0xC001,0) | Out-Null } # VIRTUALDESK|ABSOLUTE|MOVE
        'ldown' { [PIKInj]::Mouse($x,$y,0xC001,0) | Out-Null; [PIKInj]::Mouse($x,$y,0xC002,0) | Out-Null } # MOVE then LEFTDOWN
        'lup'   { [PIKInj]::Mouse($x,$y,0xC001,0) | Out-Null; [PIKInj]::Mouse($x,$y,0xC004,0) | Out-Null } # MOVE then LEFTUP
        'rdown' { [PIKInj]::Mouse($x,$y,0xC001,0) | Out-Null; [PIKInj]::Mouse($x,$y,0xC008,0) | Out-Null }
        'rup'   { [PIKInj]::Mouse($x,$y,0xC001,0) | Out-Null; [PIKInj]::Mouse($x,$y,0xC010,0) | Out-Null }
        'mdown' { [PIKInj]::Mouse($x,$y,0xC001,0) | Out-Null; [PIKInj]::Mouse($x,$y,0xC020,0) | Out-Null }
        'mup'   { [PIKInj]::Mouse($x,$y,0xC001,0) | Out-Null; [PIKInj]::Mouse($x,$y,0xC040,0) | Out-Null }
        'wheel' { [PIKInj]::Mouse($x,$y,0xC801,[uint32]([int32][int]$ev.delta)) | Out-Null } # VIRTUALDESK|ABSOLUTE|MOVE|WHEEL
        default { [PIKInj]::Mouse($x,$y,0xC001,0) | Out-Null }
      }
    }
  } catch {}
}
