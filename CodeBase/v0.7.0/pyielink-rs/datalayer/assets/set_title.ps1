param([string]$ProcName, [string]$Title)
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinTitle {
    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool SetWindowText(IntPtr hWnd, string lpString);
}
"@
try {
    $p = Get-Process -Name $ProcName -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero } | Select-Object -First 1
    if ($p) { [WinTitle]::SetWindowText($p.MainWindowHandle, $Title) | Out-Null }
} catch {}
