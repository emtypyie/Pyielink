param(
  [string]$ProcName = "ffplay",
  [Parameter(Mandatory = $true)][string]$Image,
  [string]$Trace = ""
)
# Titlebar icon setter for the ffplay player window.
#
# Strategy:
#  1. Locate the player's main window through the process table
#     (FindWindow is unreliable against SDL2 windows).
#  2. Apply WM_SETICON with icons built from $Image.
#  3. REAPPLY several times over the first ~12s: SDL re-asserts its own
#     class icon shortly after window creation and would stomp a one-shot.
#  4. Park forever: HICONS are owned by THIS process; exiting destroys them.
#
# Every step appends to $Trace (if given) so failures are never silent.

function Tr([string]$m) {
  if ($Trace) { try { Add-Content -LiteralPath $Trace -Value "$(Get-Date -Format HH:mm:ss.fff) $m" } catch {} }
}

Tr "start proc=$ProcName image=$Image"

try {
  Add-Type -AssemblyName System.Drawing | Out-Null
  Add-Type -TypeDefinition @"
using System;
using System.Diagnostics;
using System.Runtime.InteropServices;
public class PIKIcon {
  public static IntPtr Found = IntPtr.Zero;
  public static IntPtr FindPlayerWindow(string procName) {
    for (int i = 0; i < 80; i++) {
      Process[] ps = Process.GetProcessesByName(procName);
      foreach (Process p in ps) {
        try {
          IntPtr h = p.MainWindowHandle;
          if (h != IntPtr.Zero && p.MainWindowTitle.Length > 0) { Found = h; return h; }
        } catch {}
      }
      System.Threading.Thread.Sleep(250);
    }
    return IntPtr.Zero;
  }
  [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr w);
  [DllImport("user32.dll")] public static extern bool DestroyIcon(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr w, uint m, IntPtr a, IntPtr l);
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr w);
  [DllImport("user32.dll", EntryPoint = "SetClassLongPtrW")]
  public static extern IntPtr SetClassLongPtr(IntPtr w, int idx, IntPtr val);
  [DllImport("user32.dll")]
  public static extern bool SetWindowPos(IntPtr w, IntPtr a, int x, int y, int cx, int cy, uint f);
}
"@ | Out-Null
} catch {
  Tr "FATAL Add-Type: $_"
  exit 1
}
Tr "types loaded"

$hwnd = [PIKIcon]::FindPlayerWindow($ProcName)
if ($hwnd -eq [IntPtr]::Zero) { Tr "no window found after 20s"; exit 1 }
Tr "hwnd=$hwnd"

if (-not (Test-Path $Image)) { Tr "image missing"; exit 1 }

try {
  $src = [System.Drawing.Image]::FromFile($Image)
} catch {
  Tr "FATAL FromFile: $_"
  exit 1
}

$dpi = [PIKIcon]::GetDpiForWindow($hwnd)
if ($dpi -eq 0) { $dpi = 96 }
$bigSide   = [Math]::Round(([PIKIcon]::GetSystemMetrics(11)) * $dpi / 96)
$smallSide = [Math]::Round(([PIKIcon]::GetSystemMetrics(49)) * $dpi / 96)
if ($bigSide -lt 16) { $bigSide = 48 }
if ($smallSide -lt 12) { $smallSide = 16 }
Tr "dpi=$dpi big=$bigSide small=$smallSide"

function New-Icon([int]$side) {
  $bmp = New-Object System.Drawing.Bitmap $side, $side
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
  $g.DrawImage($src, 0, 0, $side, $side)
  return $bmp.GetHicon()
}

$WM_SETICON_BIG   = 0x0080  # wParam 1 handled below
$applied = 0

# Reapply loop: defeat SDL's late class-icon stomp, then park forever.
for ($round = 0; $round -lt 7; $round++) {
  if (-not [PIKIcon]::IsWindow($hwnd)) { Tr "window gone round=$round"; break }
  try {
    $big = New-Icon $bigSide
    $sml = New-Icon $smallSide
    [PIKIcon]::SendMessage($hwnd, 0x80, [IntPtr]1, $big) | Out-Null   # WM_SETICON ICON_BIG
    [PIKIcon]::SendMessage($hwnd, 0x80, [IntPtr]0, $sml) | Out-Null   # WM_SETICON ICON_SMALL
    # Also swap the CLASS icons (GCLP_HICON=-14, GCLP_HICONSM=-3): SDL
    # registered its own, and DWM falls back to them in some states.
    [PIKIcon]::SetClassLongPtr($hwnd, -14, $big) | Out-Null
    [PIKIcon]::SetClassLongPtr($hwnd, -3, $sml) | Out-Null
    # Kick the non-client area so the titlebar repaints immediately.
    # SWP_NOMOVE|SWP_NOSIZE|SWP_NOZORDER|SWP_NOACTIVATE|SWP_FRAMECHANGED
    [PIKIcon]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, 0, 0, 0x63) | Out-Null
    $applied++
    Tr "applied round=$round big=$big sml=$sml"
    Start-Sleep -Seconds 2
  } catch {
    Tr "apply error round=${round}: $_"
  }
}

Tr "done applied=$applied - parking"
while ([PIKIcon]::IsWindow($hwnd)) { Start-Sleep -Seconds 3 }
Tr "window closed, exiting"
