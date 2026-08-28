param(
  [string]$ProcName = "ffplay",
  [Parameter(Mandatory = $true)][int]$UdpPort,
  [string]$Trace = ""
)
# Transparent overlay over ffplay's CLIENT area.
# Shows red crosshair + bottom-left coords. Forwards all mouse input
# via UDP to client_view.js (same CH_INPUT path as input_hook).
# Visual distinction: overlay crosshair is red '+' (instant local),
# host arrow in video stays white (delayed) — no confusion.
$ErrorActionPreference = 'SilentlyContinue'
function Tr([string]$m){ if($Trace){ try{ Add-Content -LiteralPath $Trace -Value "$(Get-Date -Format HH:mm:ss.fff) $m" }catch{} } }
Tr "crosshair start pid=$PID proc=$ProcName udp=$UdpPort"
try { Add-Type -AssemblyName PresentationFramework -EA Stop; Add-Type -AssemblyName PresentationCore -EA Stop; Add-Type -AssemblyName WindowsBase -EA Stop } catch { Tr "FATAL WPF load: $_"; exit 1 }

Add-Type -TypeDefinition @'
using System;using System.Diagnostics;using System.Runtime.InteropServices;
public static class PIKOver {
  [StructLayout(LayoutKind.Sequential)] public struct RECT{public int L,T,R,B;}
  [StructLayout(LayoutKind.Sequential)] public struct POINT{public int x,y;}
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h,out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h,ref POINT p);
  [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern int GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h,out RECT r);
  public static IntPtr FindPlayer(string name,string title){
    for(int i=0;i<240;i++){
      foreach(Process p in Process.GetProcessesByName(name)){
        try{
          IntPtr h=p.MainWindowHandle;
          string t=p.MainWindowTitle;
          if(h!=IntPtr.Zero && t.Length>0 && t.Contains("Pyielink")) return h;
          if(h!=IntPtr.Zero && t.Length>0 && title.Length==0) return h;
        }catch{}
      }
      System.Threading.Thread.Sleep(250);
    }
    return IntPtr.Zero;
  }
}
'@ -ErrorAction Stop
Tr "types loaded"

# UDP
$udp=$null
try{ $udp=New-Object System.Net.Sockets.UdpClient; $udp.Connect("127.0.0.1",$UdpPort); Tr "udp connected" }catch{ Tr "FATAL udp: $_"; exit 1 }

$hwnd=[PIKOver]::FindPlayer($ProcName,"Pyielink - Remote Screen")
if($hwnd -eq [IntPtr]::Zero){ Tr "FATAL window never appeared"; exit 1 }
Tr "window found hwnd=$hwnd"

# Build WPF window on STA
$win=$null; $canvas=$null; $hLine=$null; $vLine=$null; $coordText=$null; $dot=$null
$udpRef=$udp; $traceRef=$Trace; $hwndRef=$hwnd

# Shared state for throttling
$script:lastMoveTick=0
$script:mouseSent=0

function SendJson([string]$s){
  try{ $b=[Text.Encoding]::UTF8.GetBytes($s); $udpRef.Send($b,$b.Length)|Out-Null; $script:mouseSent++ }catch{ Tr "udp send err: $_" }
}
function SendMouse([string]$type,[double]$rx,[double]$ry,[double]$w,[double]$h,[int]$delta){
  if($w -lt 1 -or $h -lt 1){return}
  $nx=[int]($rx*65535/$w); $ny=[int]($ry*65535/$h)
  if($nx -lt 0){$nx=0} if($nx -gt 65535){$nx=65535}
  if($ny -lt 0){$ny=0} if($ny -gt 65535){$ny=65535}
  $j='{"t":"mouse","type":"'+$type+'","x":'+$nx+',"y":'+$ny
  if($delta -ne 0){ $j+=',"delta":'+$delta }
  $j+='}'
  SendJson $j
}

# Create window
try{
  $win=New-Object System.Windows.Window
  $win.WindowStyle=[System.Windows.WindowStyle]::None
  $win.AllowsTransparency=$true
  # Hit-test requires non-null background with alpha >0, even if visually transparent
  $hitBrush=New-Object System.Windows.Media.SolidColorBrush([System.Windows.Media.Color]::FromArgb(1,0,0,0))
  $win.Background=$hitBrush
  $win.Topmost=$true
  $win.ShowInTaskbar=$false
  $win.Focusable=$false
  # WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW set via P/Invoke after Show
  $win.SizeToContent=[System.Windows.SizeToContent]::Manual
  $win.ResizeMode=[System.Windows.ResizeMode]::NoResize
  $win.ShowActivated=$false
  $canvas=New-Object System.Windows.Controls.Canvas
  $canvas.Background=$hitBrush
  $win.Content=$canvas

  $hLine=New-Object System.Windows.Shapes.Line
  $hLine.Stroke=[System.Windows.Media.Brushes]::Red
  $hLine.StrokeThickness=1
  $hLine.Opacity=0.9
  $hLine.Visibility=[System.Windows.Visibility]::Hidden
  $vLine=New-Object System.Windows.Shapes.Line
  $vLine.Stroke=[System.Windows.Media.Brushes]::Red
  $vLine.StrokeThickness=1
  $vLine.Opacity=0.9
  $vLine.Visibility=[System.Windows.Visibility]::Hidden
  $dot=New-Object System.Windows.Shapes.Ellipse
  $dot.Width=5; $dot.Height=5
  $dot.Fill=[System.Windows.Media.Brushes]::Red
  $dot.Stroke=[System.Windows.Media.Brushes]::White
  $dot.StrokeThickness=1
  $dot.Visibility=[System.Windows.Visibility]::Hidden
  $coordText=New-Object System.Windows.Controls.TextBlock
  $coordText.Foreground=[System.Windows.Media.Brushes]::White
  $coordText.Background=(New-Object System.Windows.Media.SolidColorBrush([System.Windows.Media.Color]::FromArgb(128,0,0,0)))
  $coordText.Padding=[System.Windows.Thickness]::new(4,1,4,1)
  $coordText.FontSize=11
  $coordText.FontFamily=[System.Windows.Media.FontFamily]::new("Consolas")
  $coordText.Text="0,0"

  $canvas.Children.Add($hLine)|Out-Null
  $canvas.Children.Add($vLine)|Out-Null
  $canvas.Children.Add($dot)|Out-Null
  $canvas.Children.Add($coordText)|Out-Null

  # Position text bottom-left (updated dynamically in MouseMove)
  [System.Windows.Controls.Canvas]::SetLeft($coordText,4)

  # Mouse handlers (Canvas + Window fallback)
  $onMove={
    param($s,$e)
    $p=$e.GetPosition($canvas)
    $w=$canvas.ActualWidth; $h=$canvas.ActualHeight
    if($w -lt 1 -or $h -lt 1){ $w=$win.Width; $h=$win.Height }
    # Update crosshair
    try{
      $hLine.X1=0; $hLine.Y1=$p.Y; $hLine.X2=$w; $hLine.Y2=$p.Y
      $vLine.X1=$p.X; $vLine.Y1=0; $vLine.X2=$p.X; $vLine.Y2=$h
      [System.Windows.Controls.Canvas]::SetLeft($dot,$p.X-2.5)
      [System.Windows.Controls.Canvas]::SetTop($dot,$p.Y-2.5)
      # Coords bottom-left
      $nx=[int]($p.X*65535/$w); $ny=[int]($p.Y*65535/$h)
      $hostX=[int]($p.X); $hostY=[int]($p.Y)
      $coordText.Text="$hostX,$hostY  ($nx,$ny)"
      [System.Windows.Controls.Canvas]::SetTop($coordText,$h-22)
    }catch{}
    # Throttle move to ~60Hz
    $now=[Environment]::TickCount
    if($now - $script:lastMoveTick -ge 15){
      $script:lastMoveTick=$now
      SendMouse "move" $p.X $p.Y $w $h 0
      if($script:mouseSent -eq 1){ Tr "first move $hostX,$hostY -> $nx,$ny" }
    }
  }
  $canvas.Add_MouseMove($onMove)
  $onDown={
    param($s,$e)
    $p=$e.GetPosition($canvas); $w=$canvas.ActualWidth; $h=$canvas.ActualHeight
    if($w -lt 1 -or $h -lt 1){ $w=$win.Width; $h=$win.Height }
    $btn=$e.ChangedButton
    if($btn -eq [System.Windows.Input.MouseButton]::Left){ $t="ldown" }
    elseif($btn -eq [System.Windows.Input.MouseButton]::Right){ $t="rdown" }
    elseif($btn -eq [System.Windows.Input.MouseButton]::Middle){ $t="mdown" }
    else { return }
    SendMouse $t $p.X $p.Y $w $h 0
    Tr "click $t $p"
    $e.Handled=$true
  }
  $onUp={
    param($s,$e)
    $p=$e.GetPosition($canvas); $w=$canvas.ActualWidth; $h=$canvas.ActualHeight
    if($w -lt 1 -or $h -lt 1){ $w=$win.Width; $h=$win.Height }
    $btn=$e.ChangedButton
    if($btn -eq [System.Windows.Input.MouseButton]::Left){ $t="lup" }
    elseif($btn -eq [System.Windows.Input.MouseButton]::Right){ $t="rup" }
    elseif($btn -eq [System.Windows.Input.MouseButton]::Middle){ $t="mup" }
    else { return }
    SendMouse $t $p.X $p.Y $w $h 0
    $e.Handled=$true
  }
  $onWheel={
    param($s,$e)
    $p=$e.GetPosition($canvas); $w=$canvas.ActualWidth; $h=$canvas.ActualHeight
    if($w -lt 1 -or $h -lt 1){ $w=$win.Width; $h=$win.Height }
    $d=$e.Delta
    SendMouse "wheel" $p.X $p.Y $w $h $d
    Tr "wheel $d at $p"
    $e.Handled=$true
  }
  $canvas.Add_MouseDown($onDown)
  $canvas.Add_MouseUp($onUp)
  $canvas.Add_MouseWheel($onWheel)
  # Show crosshair only when mouse is inside the overlay (i.e., over ffplay client area)
  $canvas.Add_MouseEnter({
    $hLine.Visibility=[System.Windows.Visibility]::Visible
    $vLine.Visibility=[System.Windows.Visibility]::Visible
    $dot.Visibility=[System.Windows.Visibility]::Visible
  })
  $canvas.Add_MouseLeave({
    $hLine.Visibility=[System.Windows.Visibility]::Hidden
    $vLine.Visibility=[System.Windows.Visibility]::Hidden
    $dot.Visibility=[System.Windows.Visibility]::Hidden
    $coordText.Text="0,0"
  })

  # Show window off-screen first
  $win.Left=-10000; $win.Top=-10000; $win.Width=200; $win.Height=200
  $win.Show()|Out-Null
  # Make it NOACTIVATE so it doesn't steal focus from ffplay
  Add-Type -TypeDefinition @'
using System;using System.Runtime.InteropServices;
public class PIKWin { [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h,int n); [DllImport("user32.dll")] public static extern int SetWindowLong(IntPtr h,int n,int v); [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h,IntPtr a,int x,int y,int w,int h_,uint f); }
'@ | Out-Null
  $hwndOver=(New-Object System.Windows.Interop.WindowInteropHelper($win)).Handle
  $ex=[PIKWin]::GetWindowLong($hwndOver,-20)
  [PIKWin]::SetWindowLong($hwndOver,-20,($ex -bor 0x08000000 -bor 0x00000080))|Out-Null # WS_EX_NOACTIVATE|TOOLWINDOW
  Tr "overlay hwnd=$hwndOver shown"
}catch{ Tr "FATAL window create: $_"; exit 1 }

# Tracking loop (runs on UI thread via DispatcherTimer)
$timer=New-Object System.Windows.Threading.DispatcherTimer
$timer.Interval=[TimeSpan]::FromMilliseconds(16)
$timer.Add_Tick({
  if(-not [PIKOver]::IsWindow($hwndRef)){
    Tr "target gone, exiting"
    $timer.Stop(); $win.Close(); [System.Windows.Threading.Dispatcher]::CurrentDispatcher.InvokeShutdown()
    return
  }
  $r=New-Object PIKOver+RECT
  if(-not [PIKOver]::GetClientRect($hwndRef,[ref]$r)){return}
  $o=New-Object PIKOver+POINT; $o.x=0; $o.y=0
  if(-not [PIKOver]::ClientToScreen($hwndRef,[ref]$o)){return}
  $cw=$r.R-$r.L; $ch=$r.B-$r.T
  if($cw -lt 1 -or $ch -lt 1){return}
   # DPI scaling
   $dpi=[PIKOver]::GetDpiForWindow($hwndRef); if($dpi -eq 0){$dpi=96}
   $scale=$dpi/96.0
   # Hide only when minimized or the target window is gone. We deliberately do
   # NOT require ffplay to be the foreground window: hovering the viewer is
   # enough to capture (RDP-like). Requiring foreground made the remote cursor
   # appear "stuck" because hovering never focuses the window.
   $isMin=[PIKOver]::IsIconic($hwndRef)
   if($isMin){
     if($win.Visibility -ne [System.Windows.Visibility]::Hidden){
       $win.Visibility=[System.Windows.Visibility]::Hidden
       $hLine.Visibility=[System.Windows.Visibility]::Hidden
       $vLine.Visibility=[System.Windows.Visibility]::Hidden
       $dot.Visibility=[System.Windows.Visibility]::Hidden
     }
     return
   } else {
     if($win.Visibility -ne [System.Windows.Visibility]::Visible){ $win.Visibility=[System.Windows.Visibility]::Visible }
   }
  $win.Left=$o.x/$scale
  $win.Top=$o.y/$scale
  $win.Width=$cw/$scale
  $win.Height=$ch/$scale
  # Keep topmost while foreground
  try{ [PIKWin]::SetWindowPos($hwndOver,[IntPtr]-1,0,0,0,0,0x0013) | Out-Null }catch{}
})
$timer.Start()
Tr "tracking started"

# Periodic log
$logTimer=New-Object System.Windows.Threading.DispatcherTimer
$logTimer.Interval=[TimeSpan]::FromSeconds(2)
$lastLog=-1
$logTimer.Add_Tick({
  if($script:mouseSent -ne $lastLog){ $script:lastLog=$script:mouseSent; Tr "sent: mouse=$script:mouseSent" }
})
$logTimer.Start()

# Run dispatcher
try{ [System.Windows.Threading.Dispatcher]::Run() }catch{ Tr "dispatcher exit: $_" }
Tr "run returned: mouse=$script:mouseSent"
$udp.Close()
