# pyielink Phase 1 milestone test (hardened protocol: challenge/proof + heartbeat)
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

function Get-Sha256Hex([string]$s) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($s)) | ForEach-Object { $_.ToString("x2") }) -join ""
    } finally { $sha.Dispose() }
}

function Get-StoredSecret([string]$user, [string]$field) {
    # mirrors the host state file layout for the given user's field
    $raw = Get-Content $STATE -Raw
    if ($raw -match ('"' + $user + '": \{ "pw_salt": "[0-9a-f]+", "' + $field + '": "([0-9a-f]{64})"')) {
        return $Matches[1]
    }
    throw "could not read $field for $user"
}

function Add-User([string]$name) {
    $out = "test123`ntest123" | & $Exe /adduser -m $name 2>&1
    if ($LASTEXITCODE -ne 0) { throw "/adduser $name failed: $out" }
}

function Start-HostRun {
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

function Stop-HostRun {
    if ($script:hostProc) {
        Stop-Process -Id $script:hostProc.Id -Force -ErrorAction SilentlyContinue
        $script:hostProc = $null
        Start-Sleep -Milliseconds 300
    }
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
    $c.ReceiveTimeout = 8000
    $c.Connect("127.0.0.1", $PORT)
    return $c
}

function Hello($c, [string]$user) { Send-Frame $c 0x01 "$user`ntest-client-1.0.1" }

function Answer-Challenge($c, [string]$mode, [string]$secret) {
    # secret = the SAME stored material the host holds (pw_hash or token hash)
    $ch = Read-Frame $c
    if ($ch.Type -ne 0x0B) { throw ("expected CHALLENGE, got 0x{0:X2}" -f $ch.Type) }
    $nonce = (($ch.Text -split "`n")[1]).Trim()
    $proof = "{0}:{1}" -f $mode, (Get-Sha256Hex ($secret + $nonce))
    Send-Frame $c 0x0C $proof
    return $nonce
}

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
Write-Host "== pyielink milestone test (hardened bootstrap) =="

Reset-State

Write-Host "[1] provisioning"
Add-User "bob"
Add-User "alice"
$ErrorActionPreference = "Continue"
$dup = "x`nx" | & $Exe /adduser -m bob 2>&1
$ErrorActionPreference = "Stop"
Assert ($LASTEXITCODE -ne 0) "duplicate username rejected"
Assert ((Get-Content $STATE -Raw) -match '"enabled": false') "state starts disabled"

Write-Host "== host run #1: standard flows (fail budget kept under lockout) =="
Start-HostRun
try {
    Write-Host "[2] disabled refusal"
    Set-Enabled $false
    $c = Open-Conn; Hello $c "bob"
    $r = Read-Frame $c
    Assert (($r.Type -eq 0x0A) -and ($r.Text -eq "host disabled")) "refused while disabled"
    $c.Close()

    Write-Host "[3] unknown user"
    Set-Enabled $true
    $c = Open-Conn; Hello $c "carol"
    $r = Read-Frame $c
    Assert (($r.Type -eq 0x0A) -and ($r.Text -eq "unknown user")) "unknown-user refusal"
    $c.Close()

    Write-Host "[4] password proofs: wrong x2, correct on attempt 3, license, token, ticket"
    $bobHash = Get-StoredSecret "bob" "pw_hash"
    $fakeHash = "ff" * 32
    $c = Open-Conn; Hello $c "bob"
    $null = Answer-Challenge $c "p" $fakeHash         # fail 1
    $null = Answer-Challenge $c "p" ("ee" * 32)       # fail 2
    $null = Answer-Challenge $c "p" $bobHash          # success
    $lic = Read-Frame $c
    Expect $lic 0x04 "license text"
    Assert ($lic.Text -match "ETHICS") "license body present"
    Send-Frame $c 0x05 "y"
    $tok = Read-Frame $c
    Expect $tok 0x07 "token issued"
    Assert ($tok.Text -cmatch '^[0-9a-f]{64}$') "token is 64 hex chars"
    $ok = Read-Frame $c
    Expect $ok 0x09 "auth ok"
    $parts = $ok.Text -split "`n"
    Assert (($parts.Count -eq 2) -and ($parts[0] -eq "4243") -and ($parts[1] -cmatch '^[0-9a-f]{64}$')) "promotion ticket = data port + session key"
    $script:bobToken = $tok.Text
    $script:bobTokenHash = Get-Sha256Hex $tok.Text
    $script:keyA = $parts[1]
    $c.Close()

    Write-Host "[5] token-only reconnect via proof (no license, no password)"
    $c = Open-Conn; Hello $c "bob"
    $null = Answer-Challenge $c "t" $bobTokenHash
    $ok2 = Read-Frame $c
    Expect $ok2 0x09 "token proof accepted"
    $c.Close()

    Write-Host "[6] bad token proof falls back to password; token rotates"
    $c = Open-Conn; Hello $c "bob"
    $n1 = Answer-Challenge $c "t" ("ff" * 32)         # fail 3
    $n2 = Answer-Challenge $c "p" $bobHash
    Assert ($n1 -ne $n2) "every challenge uses a fresh nonce"
    $tok2 = Read-Frame $c
    Expect $tok2 0x07 "rotated token"
    Assert ($tok2.Text -ne $bobToken) "token actually rotated"
    $null = Read-Frame $c   # AUTH_OK
    $c.Close()
    $script:bobToken = $tok2.Text
    $script:bobTokenHash = Get-Sha256Hex $tok2.Text

    Write-Host "[7] heartbeat round-trips then clean BYE"
    $c = Open-Conn; Hello $c "bob"
    $null = Answer-Challenge $c "t" $bobTokenHash
    $null = Read-Frame $c                             # AUTH_OK
    $pings = 0
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt 12 -and $pings -lt 2) {
        $f = Read-Frame $c
        if ($f.Type -eq 0x0D) {
            $pings++
            Send-Frame $c 0x0E $f.Text                # echo payload back
        }
    }
    Assert ($pings -ge 2) "received >=2 host pings in 12s and answered each"
    Send-Frame $c 0x0F "bye"
    Start-Sleep -Milliseconds 400
    try {
        $null = Read-Exact $c 1
        throw "expected close after BYE"
    } catch [System.Management.Automation.PSInvalidOperationException] { }
    catch { if ($_.ToString() -notmatch "closed by host") { throw } }
    Assert $true "host closed cleanly after BYE"
    $c.Close()

    Write-Host "[8] license reject keeps user unlicensed"
    $aliceHash = Get-StoredSecret "alice" "pw_hash"
    $c = Open-Conn; Hello $c "alice"
    $null = Answer-Challenge $c "p" $aliceHash
    $licA = Read-Frame $c
    Expect $licA 0x04 "alice gets license text"
    Send-Frame $c 0x06 "n"
    Start-Sleep -Milliseconds 300
    Assert ((Get-Content $STATE -Raw) -notmatch '"alice".{0,200}licensed.{0,10}true') "alice still unlicensed"
    $c.Close()

    Write-Host "[9] malformed frames do not kill the listener"
    $c = Open-Conn
    Send-Frame $c 0x01 "bob`nx"
    $null = Read-Frame $c
    $c.Client.Close()   # abrupt close mid-frame
    Start-Sleep -Milliseconds 300
    $c2 = Open-Conn; Hello $c2 "bob"
    $null = Answer-Challenge $c2 "t" $bobTokenHash
    $null = Read-Frame $c2
    $c2.Close()
    Assert $true "listener survived abrupt disconnect"
} finally {
    Stop-HostRun
}

Write-Host "== host run #2: exhaustion, real binaries, lockout (fresh fail budget) =="
Start-HostRun
try {
    Write-Host "[10] auth exhaustion (3 bad proofs)"
    $badA = "a" * 32
    $badB = "b" * 32
    $badC = "c" * 32
    $c = Open-Conn; Hello $c "alice"
    foreach ($p in @($badA, $badB, $badC)) { $null = Answer-Challenge $c "p" $p }
    $fail = Read-Frame $c                              # fails 1,2,3
    Expect $fail 0x0A "exhaustion failure"
    Assert ($fail.Text -eq "authentication failed") "exhaustion reason"
    $c.Close()

    Write-Host "[11] real client binary end-to-end (token path, zero input)"
    # the real client stores sha256(token) in its credential file; mirror that
    $tokDir = Join-Path $PYIELINK_DIR "tokens"
    New-Item -ItemType Directory -Path $tokDir -Force | Out-Null
    Set-Content -Path (Join-Path $tokDir "bob@127.0.0.1") -Value $bobTokenHash -NoNewline
    $cl = Start-Process -FilePath $Exe -ArgumentList "bob@127.0.0.1" `
        -RedirectStandardOutput "$PSScriptRoot\_client_out.log" `
        -RedirectStandardError  "$PSScriptRoot\_client_err.log" `
        -PassThru -NoNewWindow
    Start-Sleep -Seconds 7
    Stop-Process -Id $cl.Id -Force -ErrorAction SilentlyContinue
    $joined = (Get-Content "$PSScriptRoot\_client_out.log" -Raw -ErrorAction SilentlyContinue) + (Get-Content "$PSScriptRoot\_client_err.log" -Raw -ErrorAction SilentlyContinue)
    Assert ($joined -match "session promoted") "real client promoted with stored token only"
    Assert ($joined -notmatch "password:") "no password prompt on token path"
    Assert ($joined -match "rtt \d+ms") "real client sees heartbeats"

    Write-Host "[12] IP lockout after 5 consecutive failures"
    # note: successful auths reset the fail counter, so land 5 fails in a row
    $badX = "d" * 32
    $badY = "e" * 32
    $c = Open-Conn; Hello $c "bob"                     # fails 1-3
    foreach ($p in @($badA, $badB, $badC)) { $null = Answer-Challenge $c "p" $p }
    $null = Read-Frame $c
    $c.Close()
    $c = Open-Conn; Hello $c "bob"                     # fails 4-5 -> locked
    $null = Answer-Challenge $c "p" $badX
    $null = Answer-Challenge $c "p" $badY
    Start-Sleep -Milliseconds 200
    $c.Close()
    $c2 = Open-Conn; Hello $c2 "bob"
    $lockmsg = ""
    try { $ch = Read-Frame $c2; if ($ch.Type -eq 0x0B) { $lockmsg = "<challenge still sent>" } elseif ($ch.Type -eq 0x0A) { $lockmsg = $ch.Text } } catch { $lockmsg = "<closed>" }
    Assert ($lockmsg -eq "too many failures, try later") "further connections refused while locked (got: $lockmsg)"
    $c2.Close()
} finally {
    Stop-HostRun
}

Write-Host ""
Write-Host "ALL MILESTONE CHECKS PASSED" -ForegroundColor Green
