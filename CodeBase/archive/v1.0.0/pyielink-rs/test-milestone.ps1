# pyielink Phase 1 milestone test
# Drives the real host + client binaries over localhost TCP.
# Usage:  powershell -ExecutionPolicy Bypass -File test-milestone.ps1 [-Exe path\to\pyielink.exe]
param(
    [string]$Exe = "$PSScriptRoot\target\debug\pyielink.exe"
)
$ErrorActionPreference = "Stop"

$PYIELINK_DIR = Join-Path $env:USERPROFILE ".pyielink"
$STATE = Join-Path $PYIELINK_DIR "host_state.json"
$PORT = 4242

# ---- helpers -------------------------------------------------------------

function Reset-State {
    if (Test-Path $PYIELINK_DIR) { Remove-Item -Recurse -Force $PYIELINK_DIR }
}

function Add-User([string]$name) {
    $out = "test123`ntest123" | & $Exe /adduser -m $name 2>&1
    if ($LASTEXITCODE -ne 0) { throw "/adduser $name failed: $out" }
}

function Start-Host {
    $script:hostProc = Start-Process -FilePath $Exe `
        -ArgumentList "/enable","--port",$PORT `
        -RedirectStandardOutput "$PSScriptRoot\_host_out.log" `
        -RedirectStandardError  "$PSScriptRoot\_host_err.log" `
        -PassThru -NoNewWindow
    for ($i = 0; $i -lt 50; $i++) {
        try {
            $t = New-Object Net.Sockets.TcpClient
            $t.Connect("127.0.0.1", $PORT)
            $t.Close()
            return
        } catch { Start-Sleep -Milliseconds 100 }
    }
    throw "host never opened port $PORT"
}

function Set-Enabled([bool]$on) {
    $val = "false"
    if ($on) { $val = "true" }
    $raw = Get-Content $STATE -Raw
    $raw = $raw -replace '"enabled": (true|false)', ('"enabled": ' + $val)
    Set-Content -Path $STATE -Value $raw -NoNewline
}

function Read-Exact($s, [int]$n) {
    $buf = New-Object byte[] $n
    $off = 0
    while ($off -lt $n) {
        $r = $s.GetStream().Read($buf, $off, $n - $off)
        if ($r -le 0) { throw "connection closed by host" }
        $off += $r
    }
    return ,$buf
}

function Read-Frame($s) {
    $hdr = Read-Exact $s 3
    $len = ([int]$hdr[1] -shl 8) -bor [int]$hdr[2]
    $payload = if ($len -gt 0) { Read-Exact $s $len } else { ,@() }
    [pscustomobject]@{
        Type    = [int]$hdr[0]
        Payload = $payload
        Text    = [Text.Encoding]::UTF8.GetString($payload)
    }
}

function Send-Frame($s, [int]$type, [string]$text) {
    $p = [Text.Encoding]::UTF8.GetBytes($text)
    $ms = New-Object IO.MemoryStream
    $ms.WriteByte([byte]$type)
    $ms.WriteByte([byte](($p.Length -shr 8) -band 0xFF))
    $ms.WriteByte([byte]($p.Length -band 0xFF))
    if ($p.Length -gt 0) { $ms.Write($p, 0, $p.Length) }
    $s.GetStream().Write($ms.ToArray(), 0, $ms.Length)
    $s.GetStream().Flush()
}

function Open-Conn {
    $c = New-Object Net.Sockets.TcpClient
    $c.ReceiveTimeout = 5000
    $c.Connect("127.0.0.1", $PORT)
    return $c
}

function Hello($c, [string]$user) { Send-Frame $c 0x01 "$user`ntest-client-0.1.0" }

function Expect($frame, [int]$type, [string]$label) {
    if ($frame.Type -ne $type) {
        throw ("{0}: expected frame 0x{1:X2}, got 0x{2:X2} ('{3}')" -f $label, $type, $frame.Type, $frame.Text)
    }
}

function Assert([bool]$cond, [string]$label) {
    if (-not $cond) { throw "FAIL: $label" }
    Write-Host "  ok: $label"
}

# ---- scenario ------------------------------------------------------------
Write-Host "== pyielink milestone test =="

Reset-State

Write-Host "[1] provisioning"
Add-User "bob"
Add-User "alice"
$ErrorActionPreference = "Continue"
$dup = "x`nx" | & $Exe /adduser -m bob 2>&1
$ErrorActionPreference = "Stop"
Assert ($LASTEXITCODE -ne 0) "duplicate username rejected"
Assert ((Get-Content $STATE -Raw) -match '"enabled": false') "state starts disabled"

Start-Host
try {
    Write-Host "[2] disabled refusal (state flipped while listener runs)"
    Set-Enabled $false
    $c = Open-Conn; Hello $c "bob"
    $r = Read-Frame $c
    Expect $r 0x0A "disabled refusal"
    Assert ($r.Text -eq "host disabled") "refusal reason is 'host disabled'"
    $c.Close()

    Write-Host "[3] unknown user"
    Set-Enabled $true
    $c = Open-Conn; Hello $c "carol"
    $r = Read-Frame $c
    Expect $r 0x0A "unknown user refusal"
    Assert ($r.Text -eq "unknown user") "unknown-user reason"
    $c.Close()

    Write-Host "[4] wrong password x2, correct on attempt 3, license accept, token issued"
    $c = Open-Conn; Hello $c "bob"
    Expect (Read-Frame $c) 0x02 "passwd req"
    Send-Frame $c 0x03 "wrong1"
    Expect (Read-Frame $c) 0x02 "retry req 1"
    Send-Frame $c 0x03 "wrong2"
    Expect (Read-Frame $c) 0x02 "retry req 2"
    Send-Frame $c 0x03 "test123"
    $lic = Read-Frame $c
    Expect $lic 0x04 "license text"
    Assert ($lic.Text -match "ETHICS") "license body present"
    Send-Frame $c 0x05 "y"
    $tok = Read-Frame $c
    Expect $tok 0x07 "token issued"
    Assert ($tok.Text -cmatch '^[0-9a-f]{64}$') "token is 64 hex chars"
    $ok = Read-Frame $c
    Expect $ok 0x09 "auth ok"
    Assert ($ok.Text -eq "4243") "data port advertised"
    $c.Close()
    $script:bobToken = $tok.Text

    Write-Host "[5] returning session authenticates via token alone"
    $c = Open-Conn; Hello $c "bob"
    Expect (Read-Frame $c) 0x02 "passwd req"
    Send-Frame $c 0x08 $bobToken
    $ok2 = Read-Frame $c
    Expect $ok2 0x09 "token auth ok"
    $c.Close()

    Write-Host "[6] bad token falls back to password, new token rotated"
    $c = Open-Conn; Hello $c "bob"
    Expect (Read-Frame $c) 0x02 "passwd req"
    Send-Frame $c 0x08 ("ff" * 32)
    Expect (Read-Frame $c) 0x02 "fallback req"
    Send-Frame $c 0x03 "test123"
    $tok2 = Read-Frame $c
    Expect $tok2 0x07 "rotated token"
    Assert ($tok2.Text -ne $bobToken) "token actually rotated"
    Expect (Read-Frame $c) 0x09 "ok after rotation"
    $c.Close()
    $script:bobToken = $tok2.Text

    Write-Host "[7] license reject keeps user unlicensed"
    $c = Open-Conn; Hello $c "alice"
    Expect (Read-Frame $c) 0x02 "passwd req"
    Send-Frame $c 0x03 "test123"
    $licA = Read-Frame $c
    Expect $licA 0x04 "alice gets license text"
    Send-Frame $c 0x06 "n"
    Start-Sleep -Milliseconds 300
    Assert ((Get-Content $STATE -Raw) -notmatch '"alice".{0,200}licensed.{0,10}true') "alice still unlicensed"
    $c.Close()

    Write-Host "[8] auth exhaustion"
    $c = Open-Conn; Hello $c "alice"
    Expect (Read-Frame $c) 0x02 "passwd req"
    foreach ($p in @("a", "b", "c")) { Send-Frame $c 0x03 $p; $null = Read-Frame $c }
    $fail = Read-Frame $c
    Expect $fail 0x0A "exhaustion failure"
    Assert ($fail.Text -eq "authentication failed") "exhaustion reason"
    $c.Close()

    Write-Host "[9] malformed frames do not kill the listener"
    $c = Open-Conn
    Send-Frame $c 0x01 "bob`nx"
    Expect (Read-Frame $c) 0x02 "req before abuse"
    $c.Client.Close()   # abrupt close mid-frame
    Start-Sleep -Milliseconds 300
    $c2 = Open-Conn; Hello $c2 "bob"
    Expect (Read-Frame $c2) 0x02 "listener alive after abuse"
    $c2.Close()
    Assert $true "listener survived abrupt disconnect"

    Write-Host "[10] real client binary end-to-end (password path)"
    $out = "test123" | & $Exe bob@127.0.0.1 2>&1
    Assert ($LASTEXITCODE -eq 0) "client exit code 0"
    Assert (($out -join "`n") -match "session promoted") "client reports promotion"
    $tokPath = Join-Path $PYIELINK_DIR "tokens\bob@127.0.0.1"
    Assert (Test-Path $tokPath) "client stored token file"

    Write-Host "[11] real client reconnects with zero input (token path)"
    $out2 = "" | & $Exe bob@127.0.0.1 2>&1
    Assert ($LASTEXITCODE -eq 0) "second run exit code 0"
    $joined = $out2 -join "`n"
    Assert ($joined -match "presenting stored connection token") "client used stored token"
    Assert ($joined -notmatch "password:") "no password prompt on token path"
    Assert ($joined -match "session promoted") "second session promoted"
} finally {
    if ($script:hostProc) { Stop-Process -Id $script:hostProc.Id -Force -ErrorAction SilentlyContinue }
}

Write-Host ""
Write-Host "ALL MILESTONE CHECKS PASSED" -ForegroundColor Green
