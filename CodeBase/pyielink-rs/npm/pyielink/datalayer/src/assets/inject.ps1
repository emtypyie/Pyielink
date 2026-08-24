# Host-side input injection engine.
#
# Spawned by datalayer/src/input.js. Reads one JSON event per line on stdin:
#   {"t":"key","vk":65,"up":false}
#   {"t":"mouse","type":"move|ldown|lup|rdown|rup|wheel","x":N,"y":N,"delta":N}
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
             F_RIGHTDOWN = 0x0008, F_RIGHTUP = 0x0010, F_WHEEL = 0x0800,
             F_ABSOLUTE = 0x8000;
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

function ConvertTo-UInt16([short]$v) {
  [BitConverter]::ToUInt32([BitConverter]::GetBytes($v), 0)
}

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
      # Base: absolute move to the event position.
      $flags = [uint32]0x8001   # ABSOLUTE | MOVE
      $data = [uint32]0
      switch ($ev.type) {
        'ldown' { $flags = $flags -bor 0x0002 }  # LEFTDOWN
        'lup'   { $flags = $flags -bor 0x0004 }  # LEFTUP
        'rdown' { $flags = $flags -bor 0x0008 }  # RIGHTDOWN
        'rup'   { $flags = $flags -bor 0x0010 }  # RIGHTUP
        'wheel' { $flags = [uint32]0x0800; $data = ConvertTo-UInt16 ([int16][int]$ev.delta) }
      }
      [PIKInj]::Mouse([int]$ev.x, [int]$ev.y, $flags, $data) | Out-Null
    }
  } catch {}
}
