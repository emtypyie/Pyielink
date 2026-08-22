# pyielink 0.1.0-alpha.4 data-layer milestone test (Phase 2: promotion -> ws 4243)
# Drives the real host binary, then a scripted Node ws client against the
# session's data port: key auth (4001 on wrong), mux echo, heartbeat
# stability, network-kill detection, and child teardown on bootstrap BYE.
# Usage:
#   powershell -ExecutionPolicy Bypass -File test-datalayer.ps1 [-Exe path\to\pyielink.exe] [-Quick]
param(
    [string]$Exe = "$PSScriptRoot\target\debug\pyielink.exe",
    [switch]$Quick
)
$ErrorActionPreference = "Stop"

$HB_MS      = if ($Quick) { 600 } else { 5000 }
$STAB_MS    = if ($Quick) { 4000 } else { 60000 }
$DETECT_MS  = if ($Quick) { 5000 } else { 18000 }
$BP         = 14243   # bootstrap listener for this test run

$HOME_DIR   = Join-Path $env:TEMP ("pyiedl-test-" + [guid]::NewGuid().ToString("N").Substring(0,8))
$STATE      = Join-Path $HOME_DIR "host_state.json"
$HOST_OUT   = Join-Path $PSScriptRoot "_dl_host_out.log"
$DL_CLIENT  = Join-Path $PSScriptRoot "datalayer\_dl_client.mjs"

$env:PYIELINK_HOME           = $HOME_DIR
$env:PYIELINK_DATALAYER      = "$PSScriptRoot\datalayer"
$env:PYIELINK_PLAINTEXT_STATE = "1"     # lets the driver read bob's stored secret; encryption covered by the other suite
$env:PYIELINK_DL_HB_MS       = $HB_MS

function Cleanup {
    if ($script:hostProc) {
        Stop-Process -Id $script:hostProc.Id -Force -ErrorAction SilentlyContinue
        $script:hostProc = $null
    }
    Remove-Item Env:\PYIELINK_HOME           -ErrorAction SilentlyContinue
    Remove-Item Env:\PYIELINK_DATALAYER      -ErrorAction SilentlyContinue
    Remove-Item Env:\PYIELINK_PLAINTEXT_STATE -ErrorAction SilentlyContinue
    Remove-Item Env:\PYIELINK_DL_HB_MS       -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $HOME_DIR    -ErrorAction SilentlyContinue
    Remove-Item -Force $HOST_OUT             -ErrorAction SilentlyContinue
    Remove-Item -Force "$PSScriptRoot\_dl_host_err.log" -ErrorAction SilentlyContinue
}
trap { Cleanup; throw $_ }

# ---- helpers (same wire helpers as the bootstrap milestone suite) ----------
function Get-Sha256Hex([string]$s) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($s)) | ForEach-Object { $_.ToString("x2") }) -join ""
    } finally { $sha.Dispose() }
}

function Assert([bool]$cond, [string]$label) {
    if (-not $cond) { throw "FAIL: $label" }
    Write-Host "  ok: $label"
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
function Wait-PortOpen([int]$port, [int]$tries) {
    for ($i = 0; $i -lt $tries; $i++) {
        try {
            $t = New-Object Net.Sockets.TcpClient
            $t.Connect("127.0.0.1", $port); $t.Close(); return $true
        } catch { Start-Sleep -Milliseconds 200 }
    }
    return $false
}
function Wait-PortClosed([int]$port, [int]$tries) {
    for ($i = 0; $i -lt $tries; $i++) {
        try {
            $t = New-Object Net.Sockets.TcpClient
            $t.Connect("127.0.0.1", $port); $t.Close()
        } catch { return $true }
        Start-Sleep -Milliseconds 250
    }
    return $false
}

# ---- scenario --------------------------------------------------------------
Write-Host "== pyielink datalayer milestone test (promotion -> node data layer) =="

Write-Host "[1] provision + enable host"
New-Item -ItemType Directory -Force -Path $HOME_DIR | Out-Null
$out = "test123`ntest123" | & $Exe /adduser -m bob -r admin 2>&1
if ($LASTEXITCODE -ne 0) { throw "/adduser failed: $out" }
$script:hostProc = Start-Process -FilePath $Exe `
    -ArgumentList "/enable","--port",$BP `
    -RedirectStandardOutput $HOST_OUT `
    -RedirectStandardError  "$PSScriptRoot\_dl_host_err.log" `
    -PassThru -NoNewWindow
Assert (Wait-PortOpen $BP 50) "bootstrap listener up on $BP"

Write-Host "[2] real auth flow yields ticket with live data port + session key"
$c = New-Object Net.Sockets.TcpClient
$c.ReceiveTimeout = 8000
$c.Connect("127.0.0.1", $BP)
Send-Frame $c 0x01 "bob`ndl-client-0.1.0-alpha.4"
$ch = Read-Frame $c
if ($ch.Type -ne 0x0B) { throw ("expected CHALLENGE, got 0x{0:X2}" -f $ch.Type) }
$nonce = (($ch.Text -split "`n")[1]).Trim()
$raw = Get-Content $STATE -Raw
if ($raw -notmatch '"bob": \{ "pw_salt": "[0-9a-f]+", "pw_hash": "([0-9a-f]{64})"') { throw "cannot read bob pw_hash from state" }
$secret = $Matches[1]
Send-Frame $c 0x0C ("p:" + (Get-Sha256Hex ($secret + $nonce)))
$lic = Read-Frame $c
if ($lic.Type -ne 0x04) { throw ("expected LICENSE_TEXT, got 0x{0:X2}" -f $lic.Type) }
Send-Frame $c 0x05 "y"
$null = Read-Frame $c   # TOKEN_ISSUED
$okf = Read-Frame $c
if ($okf.Type -ne 0x09) { throw ("expected AUTH_OK ticket, got 0x{0:X2} '{1}'" -f $okf.Type, $okf.Text) }
$parts = $okf.Text.Trim() -split "`n"
$DL_PORT = [int]$parts[0]
$SESS_KEY = $parts[1].Trim()
Assert ($DL_PORT -gt 1024 -and $DL_PORT -ne $BP) "ticket carries ephemeral data port $DL_PORT"
Assert ($SESS_KEY -match '^[0-9a-f]{64}$') "ticket carries 64-hex session key"

Write-Host "[3] host spawned node data layer on that port"
Assert (Wait-PortOpen $DL_PORT 40) "data layer accepting connections within 8 s"

Write-Host "[4] node ws client: wrong-key 4001, valid ack, mux echo, ${STAB_MS} ms stability"
& node $DL_CLIENT $DL_PORT $SESS_KEY $STAB_MS 2>&1 | ForEach-Object { Write-Host "  $_"; $_ } |
    Where-Object { $_ -match '^FAIL' } | ForEach-Object { throw $_ }
if ($LASTEXITCODE -ne 0) { throw "node dl client reported failure" }

Write-Host "[5] network kill detected by server within $($DETECT_MS) ms"
$deadline = [DateTime]::UtcNow.AddMilliseconds($DETECT_MS)
$detected = $false
while ([DateTime]::UtcNow -lt $deadline) {
    if ((Get-Content $HOST_OUT -Raw -ErrorAction SilentlyContinue) -match "heartbeat lost") { $detected = $true; break }
    Start-Sleep -Milliseconds 250
}
Assert $detected "server logged heartbeat-lost teardown"

Write-Host "[6] bootstrap BYE kills the node child"
Send-Frame $c 0x0F "bye"
Assert (Wait-PortClosed $DL_PORT 24) "data port closed after session end"

Write-Host "[7] no orphaned data-layer processes"
Start-Sleep -Milliseconds 400
$orphans = @(Get-CimInstance Win32_Process -Filter "Name='node.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -match 'datalayer' -and $_.CommandLine -match 'server\.js' })
Assert ($orphans.Count -eq 0) "no leftover node server.js processes"

try { $c.Close() } catch {}
Cleanup
Write-Host "ALL DATALAYER CHECKS PASSED"
