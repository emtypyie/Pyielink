# pyielink 0.1.0-alpha.4 file-transfer milestone test (Phase 3.1: channels 0x04/0x05)
# Drives the real host binary + the real client binary (one-shot put/get with
# token auth), plus a scripted ws client for standard-user sandbox refusals.
# Covers: push/pull round-trips w/ sha256 verify, resume-from-partial,
# zero-byte edge, path sandbox for standard users, oversize cap, child teardown.
# Usage:
#   powershell -ExecutionPolicy Bypass -File test-filetransfer.ps1 [-Exe path\to\pyielink.exe]
param(
    [string]$Exe = "$PSScriptRoot\target\debug\pyielink.exe"
)
$ErrorActionPreference = "Stop"

$BP       = 14244   # bootstrap listener for this test run
$HOME_DIR = Join-Path $env:TEMP ("pyift-test-" + [guid]::NewGuid().ToString("N").Substring(0,8))
$STATE    = Join-Path $HOME_DIR "host_state.json"
$HOST_OUT = Join-Path $PSScriptRoot "_ft_host_out.log"
$FT_CLIENT = Join-Path $PSScriptRoot "datalayer\_dl_files.mjs"
$SCRATCH  = Join-Path $env:TEMP ("pyift-files-" + [guid]::NewGuid().ToString("N").Substring(0,8))

$env:PYIELINK_HOME            = $HOME_DIR
$env:PYIELINK_DATALAYER       = "$PSScriptRoot\datalayer"
$env:PYIELINK_PLAINTEXT_STATE = "1"
$env:PYIELINK_ACCEPT_LICENSE  = "1"
$env:PYIELINK_PORT            = $BP

function Cleanup {
    if ($script:hostProc) {
        Stop-Process -Id $script:hostProc.Id -Force -ErrorAction SilentlyContinue
        $script:hostProc = $null
    }
    Remove-Item Env:\PYIELINK_HOME            -ErrorAction SilentlyContinue
    Remove-Item Env:\PYIELINK_DATALAYER       -ErrorAction SilentlyContinue
    Remove-Item Env:\PYIELINK_PLAINTEXT_STATE -ErrorAction SilentlyContinue
    Remove-Item Env:\PYIELINK_ACCEPT_LICENSE  -ErrorAction SilentlyContinue
    Remove-Item Env:\PYIELINK_PORT            -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $HOME_DIR     -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $SCRATCH      -ErrorAction SilentlyContinue
    foreach ($f in @("_ft_host_out.log", "_ft_host_err.log")) {
        Remove-Item -Force (Join-Path $PSScriptRoot $f) -ErrorAction SilentlyContinue
    }
}
trap { Cleanup; throw $_ }

# ---- wire helpers (same as bootstrap/datalayer suites) ----------------------
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

# keepalive responder: answers the host's bootstrap PING frames so the
# scripted session survives while exe-driven transfers run in parallel
function New-Keepalive($sock) {
    $rs = [runspacefactory]::CreateRunspace()
    $rs.Open()
    $ps = [powershell]::Create()
    $ps.Runspace = $rs
    [void]$ps.AddScript({
        param($sock)
        function Read-Exact($s, [int]$n) {
            $buf = New-Object byte[] $n
            $off = 0
            while ($off -lt $n) {
                $r = $s.GetStream().Read($buf, $off, $n - $off)
                if ($r -le 0) { return $null }
                $off += $r
            }
            return ,$buf
        }
        try {
            $sock.GetStream().ReadTimeout = 1000
            while ($true) {
                try { $hdr = Read-Exact $sock 3 } catch { continue }
                if ($null -eq $hdr) { return }
                if ($hdr[0] -ne 0x0D) { continue }   # PING only
                $len = ([int]$hdr[1] -shl 8) -bor [int]$hdr[2]
                $payload = if ($len -gt 0) { Read-Exact $sock $len } else { ,@() }
                $ms = New-Object IO.MemoryStream
                $ms.WriteByte([byte]0x0E)
                $ms.WriteByte([byte](($len -shr 8) -band 0xFF))
                $ms.WriteByte([byte]($len -band 0xFF))
                if ($len -gt 0 -and $null -ne $payload) { $ms.Write($payload, 0, $len) }
                $s2 = $sock.GetStream()
                $s2.Write($ms.ToArray(), 0, $ms.Length)
                $s2.Flush()
            }
        } catch { }
    }).AddArgument($sock)
    [void]$ps.BeginInvoke()
    return @{ PS = $ps; Runspace = $rs }
}
function Stop-Keepalive($ka) {
    try { $ka.PS.Stop() } catch {}
    try { $ka.PS.Dispose() } catch {}
    try { $ka.Runspace.Close() } catch {}
}

# full bootstrap auth as an existing user; returns ticket parts + raw token
function Invoke-Auth([string]$user, [string]$ipPort) {
    $c = New-Object Net.Sockets.TcpClient
    $c.ReceiveTimeout = 8000
    $c.Connect("127.0.0.1", $ipPort)
    Send-Frame $c 0x01 "$user`nft-client-0.1.0-alpha.4"
    $ch = Read-Frame $c
    if ($ch.Type -ne 0x0B) { throw ("expected CHALLENGE, got 0x{0:X2}" -f $ch.Type) }
    $nonce = (($ch.Text -split "`n")[1]).Trim()
    $raw = Get-Content $STATE -Raw
    if ($raw -notmatch ('"' + $user + '": \{ "pw_salt": "[0-9a-f]+", "pw_hash": "([0-9a-f]{64})"')) {
        throw "cannot read $user pw_hash from state"
    }
    $secret = $Matches[1]
    Send-Frame $c 0x0C ("p:" + (Get-Sha256Hex ($secret + $nonce)))
    $lic = Read-Frame $c
    if ($lic.Type -ne 0x04) { throw ("expected LICENSE_TEXT, got 0x{0:X2}" -f $lic.Type) }
    Send-Frame $c 0x05 "y"
    $tok = Read-Frame $c
    if ($tok.Type -ne 0x07) { throw ("expected TOKEN_ISSUED, got 0x{0:X2}" -f $tok.Type) }
    $okf = Read-Frame $c
    if ($okf.Type -ne 0x09) { throw ("expected AUTH_OK ticket, got 0x{0:X2} '{1}'" -f $okf.Type, $okf.Text) }
    $parts = $okf.Text.Trim() -split "`n"
    return @{
        Conn  = $c
        Port  = [int]$parts[0]
        Key   = $parts[1].Trim()
        Token = $tok.Text.Trim()
    }
}

# ---- scenario ---------------------------------------------------------------
Write-Host "== pyielink file-transfer milestone test (Phase 3.1: mux 0x04/0x05) =="

# native exe runs emit shutdown-race noise on stderr; never let PowerShell
# promote those records into terminating errors mid-suite
function Invoke-Client {
    param([string[]]$ClientArgs)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $lines = & $Exe @ClientArgs 2>&1 | ForEach-Object { "$_" }
        return @{ Exit = $LASTEXITCODE; Lines = $lines }
    } finally {
        $ErrorActionPreference = $prev
    }
}

Write-Host "[0] sweep stale host + data-layer processes from earlier aborted runs"
Get-CimInstance Win32_Process -Filter "Name='pyielink.exe' OR Name='node.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -and ($_.CommandLine -match 'datalayer\\src\\server\.js' -or $_.CommandLine -match '/enable') } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }

Write-Host "[1] provision bob (admin) + enable host on $BP"
New-Item -ItemType Directory -Force -Path $HOME_DIR, $SCRATCH | Out-Null
$out = "test123`ntest123" | & $Exe /adduser -m bob -r admin 2>&1
if ($LASTEXITCODE -ne 0) { throw "/adduser bob failed: $out" }
$out = "alice123`nalice123" | & $Exe /adduser -m alice -r user 2>&1
if ($LASTEXITCODE -ne 0) { throw "/adduser alice failed: $out" }
$script:hostProc = Start-Process -FilePath $Exe `
    -ArgumentList "/enable","--port",$BP `
    -RedirectStandardOutput $HOST_OUT `
    -RedirectStandardError  "$PSScriptRoot\_ft_host_err.log" `
    -PassThru -NoNewWindow
Assert (Wait-PortOpen $BP 50) "bootstrap listener up on $BP"

Write-Host "[2] admin auth -> ticket + client token stored for one-shot runs"
$bobsess = Invoke-Auth "bob" $BP
Assert ($bobsess.Port -gt 1024 -and $bobsess.Port -ne $BP) "ticket carries ephemeral data port $($bobsess.Port)"
Assert ($bobsess.Key -cmatch '^[0-9a-f]{64}$') "ticket carries 64-hex session key"
$tokDir = Join-Path $HOME_DIR "tokens"
New-Item -ItemType Directory -Force -Path $tokDir | Out-Null
Set-Content -Path (Join-Path $tokDir "bob@127.0.0.1") -Value (Get-Sha256Hex $bobsess.Token) -NoNewline
$bobKa = New-Keepalive $bobsess.Conn

Write-Host "[3] host spawned node data layer on that port"
Assert (Wait-PortOpen $bobsess.Port 40) "data layer accepting connections within 8 s"

Write-Host "[4] PUT round-trip via real client (admin absolute target)"
$srcBytes = New-Object byte[] ((1MB) + 777)   # odd tail crosses chunk boundaries
$rng = [System.Security.Cryptography.RNGCryptoServiceProvider]::Create()
$rng.GetBytes($srcBytes); $rng.Dispose()
$src = Join-Path $SCRATCH "source.bin"
[IO.File]::WriteAllBytes($src, $srcBytes)
$srcHash = (Get-FileHash $src -Algorithm SHA256).Hash
$dst = Join-Path $SCRATCH "up-test.bin"
$r = Invoke-Client @("put", "bob@127.0.0.1", $src, $dst)
$r.Lines | ForEach-Object { Write-Host "  $_" }
if ($r.Exit -ne 0) { throw "put exited $($r.Exit)" }
$putOut = $r.Lines
Assert (($putOut -join "`n") -match "sha256 verified") "client reported host sha256 verification"
Assert ((Test-Path $dst) -and ((Get-FileHash $dst -Algorithm SHA256).Hash -eq $srcHash)) "uploaded bytes match source"

Write-Host "[5] GET round-trip via real client"
$back = Join-Path $SCRATCH "back.bin"
$r = Invoke-Client @("get", "bob@127.0.0.1", $dst, $back)
$r.Lines | ForEach-Object { Write-Host "  $_" }
if ($r.Exit -ne 0) { throw "get exited $($r.Exit)" }
Assert ((Get-FileHash $back -Algorithm SHA256).Hash -eq $srcHash) "downloaded bytes match source"

Write-Host "[6] resume-from-partial GET"
$half = Join-Path $SCRATCH "resume.bin"
[IO.File]::WriteAllBytes($half, $srcBytes[0..([int]($srcBytes.Length / 2) - 1)])
$r = Invoke-Client @("get", "bob@127.0.0.1", $dst, $half)
$r.Lines | ForEach-Object { Write-Host "  $_" }
if ($r.Exit -ne 0) { throw "resume get exited $($r.Exit)" }
$resOut = $r.Lines
Assert (($resOut -join "`n") -match "resuming at") "client announced resume offset"
Assert ((Get-FileHash $half -Algorithm SHA256).Hash -eq $srcHash) "assembled file matches after resume"

Write-Host "[7] zero-byte file round-trip"
$empty = Join-Path $SCRATCH "empty.bin"
[IO.File]::WriteAllBytes($empty, @())
$zDst = Join-Path $SCRATCH "zero-up.bin"
$r = Invoke-Client @("put", "bob@127.0.0.1", $empty, $zDst)
if ($r.Exit -ne 0) { throw "zero-byte put failed (exit $($r.Exit))" }
$zBack = Join-Path $SCRATCH "zero-back.bin"
$r = Invoke-Client @("get", "bob@127.0.0.1", $zDst, $zBack)
if ($r.Exit -ne 0) { throw "zero-byte get failed (exit $($r.Exit))" }
Assert (((Get-Item $zBack).Length -eq 0)) "zero-byte file survived round-trip"

Write-Host "[8] standard-user sandbox: traversal/absolute denied, oversize rejected"
$alicesess = Invoke-Auth "alice" $BP
$aliceKa = New-Keepalive $alicesess.Conn
Assert (Wait-PortOpen $alicesess.Port 40) "alice data layer up on port $($alicesess.Port)"
$prevEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
foreach ($probe in @(
    @{ op="deny";   arg="..\escape.txt";                          label="traversal name refused" },
    @{ op="deny";   arg="$SCRATCH\absolute.txt";                  label="absolute path refused for standard user" },
    @{ op="toobig"; arg="big.bin";                                label="oversize announce refused (toobig)" }
)) {
    & node $FT_CLIENT $alicesess.Port $alicesess.Key $probe.op $probe.arg 2>&1 |
        ForEach-Object { Write-Host "  $_" }
    if ($LASTEXITCODE -ne 0) { $ErrorActionPreference = $prevEap; throw "sandbox probe failed: $($probe.label)" }
    Assert (-not (Test-Path (Join-Path $HOME_DIR "escape.txt"))) $probe.label
}
$ErrorActionPreference = $prevEap

Write-Host "[9] BYE tears down both node children"
Send-Frame $alicesess.Conn 0x0F "bye"
Assert (Wait-PortClosed $alicesess.Port 24) "alice data port closed after BYE"
Stop-Keepalive $aliceKa
try { $alicesess.Conn.Close() } catch {}
Send-Frame $bobsess.Conn 0x0F "bye"
Assert (Wait-PortClosed $bobsess.Port 24) "bob data port closed after BYE"

Write-Host "[10] no orphaned data-layer processes"
Start-Sleep -Milliseconds 400
$orphans = @(Get-CimInstance Win32_Process -Filter "Name='node.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -match 'datalayer' -and $_.CommandLine -match 'server\.js' })
Assert ($orphans.Count -eq 0) "no leftover node server.js processes"

Stop-Keepalive $bobKa
try { $bobsess.Conn.Close() } catch {}
Cleanup
Write-Host "ALL FILETRANSFER CHECKS PASSED"
