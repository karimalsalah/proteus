# The gate: a change lands only if this passes. Scripts measure; nobody argues.
#   pwsh forge\gate.ps1 [-SkipBench]
param([switch]$SkipBench)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)
$fail = @()
function Step($name, [scriptblock]$body) {
    Write-Host "== $name" -ForegroundColor Cyan
    try { & $body } catch { $script:fail += "${name}: $($_.Exception.Message)"; Write-Host "   FAIL $($_.Exception.Message)" -ForegroundColor Red }
}

Step 'unit tests' {
    $out = cargo test --release 2>&1 | Out-String
    # Several test programs run; one passing must not hide another failing.
    if ($LASTEXITCODE -ne 0 -or $out -notmatch 'test result: ok') { throw ($out -split "`n" | Select-Object -Last 15 | Out-String) }
    ($out -split "`n" | Select-String 'test result').Line
}

Step 'release build' {
    # A running copy locks the exe; move it aside rather than stopping anyone's window.
    if (Test-Path target\release\proteus.exe) {
        Remove-Item target\release\proteus.old.exe -Force -ErrorAction SilentlyContinue
        Rename-Item target\release\proteus.exe proteus.old.exe -ErrorAction SilentlyContinue
    }
    $out = cargo build --release 2>&1 | Out-String
    if (-not (Test-Path target\release\proteus.exe)) { throw ($out -split "`n" | Select-Object -Last 25 | Out-String) }
    if ($out -match 'warning: unused') { throw 'unused-code warnings' }
}

# Everything from here runs against its own data folder and its own daemon,
# so the gate can never touch a session someone is working in. Sound is off
# except in the step that checks it.
$iso = Join-Path $env:TEMP "proteus-gate-$PID"
New-Item -ItemType Directory -Force "$iso\data" | Out-Null
$env:PROTEUS_DATA = "$iso\data"
$env:PROTEUS_SOCKET = "$iso\gate.sock"
$env:PROTEUS_SOUND = 'off'
# Test windows open without taking the keyboard from whoever is typing.
$env:PROTEUS_NO_FOCUS = '1'

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$exe = "dist\proteus-$stamp.exe"
Step 'copy for testing' {
    New-Item -ItemType Directory -Force dist | Out-Null
    # Old test copies go; ones still running refuse and are tried next time.
    Get-ChildItem dist -File | ForEach-Object { try { Remove-Item $_.FullName -Force -ErrorAction Stop } catch {} }
    Copy-Item target\release\proteus.exe $exe -Force
    "built $exe ($([math]::Round((Get-Item $exe).Length/1MB,1)) MB)"
}

Step 'one file, nothing beside it' {
    # The exe alone in an empty folder with an empty data directory must be
    # able to open a real terminal: that proves everything it needs is inside.
    $box = Join-Path $env:TEMP "proteus-alone-$PID"
    New-Item -ItemType Directory -Force "$box\app", "$box\data" | Out-Null
    Copy-Item target\release\proteus.exe "$box\app\PROTEUS.exe"
    $env:PROTEUS_DATA = "$box\data"
    $env:PROTEUS_SOCKET = "$box\alone.sock"
    try {
        & "$box\app\PROTEUS.exe" shot --cmd 'cmd /c echo standalone-ok' --wait 20 --out "$box\out.png" --size 1200x400 | Out-Null
        if (-not (Test-Path "$box\out.png")) { throw 'no screenshot produced' }
        if (-not (Get-ChildItem "$box\data\runtime\conpty-*\conpty.dll" -ErrorAction SilentlyContinue)) { throw 'console host was not unpacked' }
        $log = Get-Content "$box\data\proteus.log" -Raw -ErrorAction SilentlyContinue
        if ($log -match 'falling back|PANIC|fatal') { throw "log: $log" }
        if (-not (Test-Path "$box\data\config.json") -and $false) { throw 'unreachable' }
        'ran alone: unpacked its console host and opened a terminal'
    } finally {
        $env:PROTEUS_DATA = "$iso\data"
        $env:PROTEUS_SOCKET = "$iso\gate.sock"
        Remove-Item $box -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Step 'ears' {
    # Every sound rendered to a file with no sound card involved, and measured.
    New-Item -ItemType Directory -Force forge\out | Out-Null
    $e = (& $exe ears --out forge\out\ears.wav | Out-String) | ConvertFrom-Json
    if ((Get-Item forge\out\ears.wav).Length -lt 500kb) { throw 'ears.wav missing or short' }
    if ($e.peak -gt 0.95) { throw "clipping: peak $($e.peak)" }
    foreach ($part in 'keys', 'text', 'flood', 'bell', 'attach', 'exit_ok', 'exit_fail', 'text_plain', 'text_red', 'text_green', 'outcome_ok', 'outcome_fail', 'needs_you') {
        if ($e.parts.$part.rms -lt 0.002) { throw "$part is inaudible (rms $($e.parts.$part.rms))" }
    }
    if ($e.parts.after.peak -ne 0) { throw "sound continues after the last event (peak $($e.parts.after.peak))" }
    if ($e.parts.text_red.rms -le $e.parts.text_plain.rms) { throw 'red text does not sound different from plain text' }
    "peak {0:N3}, rms {1:N4}, {2} notes, silent afterwards" -f $e.peak, $e.rms, $e.notes
}

Step 'screenshots' {
    New-Item -ItemType Directory -Force forge\out | Out-Null
    & $exe shot --vt forge\samples\basic.vt --out forge\out\basic.png --size 2200x1100 | Out-Null
    & $exe shot --vt forge\samples\basic.vt --out forge\out\reveal-110.png --size 2200x1100 --reveal-ms 110 | Out-Null
    foreach ($p in 'forge\out\basic.png', 'forge\out\reveal-110.png') {
        if (-not (Test-Path $p) -or (Get-Item $p).Length -lt 50kb) { throw "$p missing or empty" }
    }
    # Box drawing, underline shapes and the conformance sample, for the same reason.
    & $exe shot --vt forge\samples\draw.vt --out forge\out\draw.png --size 2400x1250 | Out-Null
    & $exe shot --vt forge\samples\conformance.vt --out forge\out\conformance.png --size 1500x1200 | Out-Null
    foreach ($p in 'forge\out\draw.png', 'forge\out\conformance.png') {
        if (-not (Test-Path $p) -or (Get-Item $p).Length -lt 50kb) { throw "$p missing or empty" }
    }
    'rendered basic.png, reveal-110.png, draw.png, conformance.png (read them after renderer changes)'
}

Step 'chronicle' {
    # Undo for the shell with the real exe: run a command, undo it, undo the undo, restore.
    & "$PSScriptRoot\chronicle.ps1" -Exe $exe -Scratch "$iso\chronicle"
}

Step 'recall' {
    # A real program journaled with no window. Its past must be reachable,
    # searchable and drawable, and every seek must equal a replay from the start.
    $j = "$iso\recall.prtj"
    & $exe recall record --out $j --cmd 'cmd /c dir C:\Windows\System32' --size 120x36 --wait 60 | Out-Null
    $b = (& $exe recall bench --journal $j --n 12 --json | Out-String) | ConvertFrom-Json
    if ($b.instants -ne 12 -or $b.identical -ne $b.instants) { throw "seeks differ from a replay from the start: $($b.different -join '; ')" }
    if ($b.index.rejected -ne 0) { throw "a keyframe did not reproduce its terminal: $($b.index.first_reject)" }
    $f = (& $exe recall find 'notepad.exe' --journal $j --json | Out-String) | ConvertFrom-Json
    if (@($f.hits).Count -lt 1) { throw 'find did not see text that was printed' }
    $half = [math]::Round($b.session_seconds / 2, 3)
    & $exe shot --journal $j --until $half --scrub --out forge\out\scrub.png --size 2360x1520 | Out-Null
    if (-not (Test-Path forge\out\scrub.png) -or (Get-Item forge\out\scrub.png).Length -lt 50kb) { throw 'scrub.png missing or empty' }
    "{0} of {1} seeks identical to a replay from the start; p50 {2} ms with the index, {3} ms without; find saw the text {4}x in {5} ms; rendered scrub.png" -f $b.identical, $b.instants, $b.after_ms.p50, $b.before_ms.p50, @($f.hits).Count, $f.ms
}

Step 'links' {
    # The three shapes of link in the sample, hovered the way the window
    # hovers them (cell-by-cell hit-testing is a unit test). Read the PNGs.
    $root = (Get-Location).Path
    $cases = @(
        @{ at = '3,8'; want = 'https://example.com/docs | ctrl+click to open | 19 cells in 2 span'; png = 'link-osc8' },
        @{ at = '10,8'; want = "$root\src\app.rs:42:7 | ctrl+click to open"; png = 'link-path' },
        @{ at = '16,10'; want = 'https://example.com/plain?q=1 | ctrl+click to open'; png = 'link-web' },
        @{ at = '5,36'; want = 'javascript:alert(1) | not opened: javascript: links are not opened'; png = 'link-refused' },
        @{ at = '1,5'; want = 'none'; png = $null }
    )
    foreach ($c in $cases) {
        $out = Join-Path 'forge\out' "$($c.png ?? 'link-none').png"
        $said = (& $exe shot --vt forge\samples\links.vt --cwd $root --link-at $c.at --out $out --size 2200x1100 | Out-String)
        if (-not $said.Contains("link at $($c.at): $($c.want)")) { throw "at $($c.at) expected '$($c.want)', got: $said" }
        if ($c.png -and (-not (Test-Path $out) -or (Get-Item $out).Length -lt 50kb)) { throw "$out missing or empty" }
    }
    'osc 8 (one id in two places), path:line from the working directory, plain address, refused javascript:, plain text: all as expected; rendered link-*.png'
}

Step 'flash safety' {
    # WCAG 2.3.1 at the brightest light the config allows. The analyser must
    # catch the positive control and pass the plain sample, or it is not trusted.
    $run = { param($vt, $extra) (& $exe shot --flash --vt $vt --light 3.0 --seconds 4 --size 1100x650 --scale 1 @extra | Out-String) | ConvertFrom-Json }
    $ctl = & $run forge\samples\flash-4hz.vt @('--every', '7.5')
    if ($ctl.pass) { throw "the analyser did not flag a 4 Hz red/black alternation: $($ctl | ConvertTo-Json -Compress)" }
    $plain = & $run forge\samples\basic.vt @('--every', '2')
    if (-not $plain.pass) { throw "the analyser flagged basic.vt: $($plain | ConvertTo-Json -Compress)" }
    $worst = 0
    foreach ($every in 1, 2, 4, 8) {
        foreach ($lines in 1, 4) {
            $r = & $run forge\samples\red-flood.vt @('--every', $every, '--lines', $lines, '--prefill', 40)
            if (-not $r.pass) { throw "red flood flashes (every $every frames, $lines lines): $($r | ConvertTo-Json -Compress)" }
            $worst = [math]::Max($worst, [math]::Max($r.general.max_transitions_in_1s, $r.red.max_transitions_in_1s))
        }
    }
    "4 Hz control flagged ({0} general, {1} red transitions in 1 s); basic.vt {2}; red flood at 8 rates: at most {3} transitions in any 1 s (limit 6)" -f $ctl.general.max_transitions_in_1s, $ctl.red.max_transitions_in_1s, $plain.general.max_transitions_in_1s, $worst
}

if (-not $SkipBench) {
    Step 'real keys through a real window' {
        # Key messages are posted to an unfocused test window and a program
        # inside reports what it received. This is the only step that crosses
        # the window library, the key encoder and the console host together.
        # Named keys (Enter, Up, Tab, Backspace) are checked exactly. Letters
        # need a character message, which Windows sometimes also generates by
        # itself for a posted key, so a letter may arrive twice: the step
        # cannot tell that from a doubling bug and does not try to.
        Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public static class GateKeys {
  [DllImport("user32.dll")] static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] static extern uint MapVirtualKeyW(uint code, uint type);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  public static void Tap(IntPtr h, uint vk, bool ext, char ch) {
    uint sc = MapVirtualKeyW(vk, 0); long down = 1 | ((long)sc << 16) | (ext ? 1L << 24 : 0); long up = down | (1L << 30) | (1L << 31);
    PostMessageW(h, 0x0100, (IntPtr)vk, (IntPtr)down);
    if (ch != 0) PostMessageW(h, 0x0102, (IntPtr)ch, (IntPtr)down);
    PostMessageW(h, 0x0101, (IntPtr)vk, (IntPtr)up);
  }
}
"@
        $full = (Resolve-Path $exe).Path
        $report = "$iso\keys.json"
        Remove-Item $report -ErrorAction SilentlyContinue
        $p = Start-Process -FilePath $full -PassThru -ArgumentList '--local', '-e', 'cmd.exe', '/d', '/c', 'start', '/wait', '/b', $full, 'keys', '--out', $report, '--until', 'c', '--seconds', '20'
        try {
            Start-Sleep -Seconds 6
            $p.Refresh(); $h = $p.MainWindowHandle
            if ($h -eq 0) { throw 'the test window never appeared' }
            if ([GateKeys]::GetForegroundWindow() -eq $h) { throw 'the test window took the keyboard' }
            foreach ($k in @(@(0x41, $false, 'a'), @(0x0D, $false, [char]0), @(0x26, $true, [char]0), @(0x42, $false, 'b'), @(0x09, $false, [char]0), @(0x08, $false, [char]0), @(0x43, $false, 'c'))) {
                [GateKeys]::Tap($h, $k[0], $k[1], $k[2]); Start-Sleep -Milliseconds 120
            }
            $t0 = Get-Date
            while (-not (Test-Path $report) -and ((Get-Date) - $t0).TotalSeconds -lt 22) { Start-Sleep -Milliseconds 100 }
            if (-not (Test-Path $report)) { throw 'the program inside never reported' }
            $got = (Get-Content $report -Raw | ConvertFrom-Json).received
            if (($got -creplace '([a-z])\1+', '$1') -cne 'a\x0d\e[Ab\x09\x7fc') { throw "typed a Enter Up b Tab Backspace c; the program received: $got" }
            "a Enter Up b Tab Backspace c arrived as $got; the window did not take the keyboard"
        } finally { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
    }

    Step 'bench' {
        $json = "$iso\data\bench.json"
        Remove-Item $json -ErrorAction SilentlyContinue
        $p = Start-Process -FilePath (Resolve-Path $exe) -ArgumentList '--bench' -PassThru
        if (-not $p.WaitForExit(150000)) { Stop-Process -Id $p.Id -Force; throw 'bench timed out' }
        $b = Get-Content $json -Raw | ConvertFrom-Json
        $k = $b.typing_key_to_present_ms
        "typing p50={0:N1} p95={1:N1} max={2:N1} ms | flood {3:N2} MB/s {4:N0} fps | build avg {5} us" -f $k.p50, $k.p95, $k.max, $b.flood.mb_per_s, $b.flood.fps, $b.grid_build_us.avg
        if ($k.n -lt 60) { throw "only $($k.n) keystrokes measured" }
        # Measured 10-15 ms through the daemon; the budget leaves room for noise, not for regressions.
        if ($k.p95 -gt 18) { throw "typing p95 $($k.p95) ms exceeds 18 ms budget" }
        if (-not $b.flood.completed) { throw 'flood did not complete' }
        if ($b.grid_build_us.avg -gt 1000) { throw "grid build avg $($b.grid_build_us.avg) us exceeds 1000 us budget" }
    }

    Step 'idle cost, with sound' {
        # A real bell through the real audio device, almost inaudibly: the
        # device must open, be given samples, and stop again by itself, and
        # the window must then cost nothing while the device is held paused.
        # --local: the session dies with this window instead of lingering in the daemon.
        $env:PROTEUS_SOUND = '0.03'
        try {
            $p = Start-Process -FilePath (Resolve-Path $exe) -PassThru -ArgumentList '--local', '-e', 'pwsh.exe', '-NoLogo', '-NoProfile', '-Command', '[Console]::Write([char]7); Start-Sleep 120'
        } finally { $env:PROTEUS_SOUND = 'off' }
        Start-Sleep -Seconds 8
        $p.Refresh(); $t0 = $p.TotalProcessorTime.TotalSeconds
        Start-Sleep -Seconds 10
        $p.Refresh(); $pct = 100 * ($p.TotalProcessorTime.TotalSeconds - $t0) / 10
        $sound = Get-Content "$iso\data\sound-$($p.Id).json" -Raw -ErrorAction SilentlyContinue
        Stop-Process -Id $p.Id -Force
        "idle cpu = {0:N2}% of one core" -f $pct
        if ($pct -gt 1.0) { throw "idle cpu $pct% exceeds 1% budget" }
        if (-not $sound) {
            $log = Get-Content "$iso\data\proteus.log" -Raw -ErrorAction SilentlyContinue
            throw "the audio device was never opened and closed. log: $log"
        }
        $s = $sound | ConvertFrom-Json
        "device: {0} | opened {1}x in {2:N1} ms | event to callback {3:N1} ms | {4} frames delivered | peak {5:N4} | {6} while idle" -f $s.device, $s.opens, $s.open_ms_last, $s.event_to_callback_ms.max, $s.frames, $s.peak, $s.state
        if ($s.state -eq 'running' -or $s.frames -lt 1000 -or $s.peak -le 0) { throw "sound did not reach the device, or the stream never stopped: $sound" }
    }
}

Remove-Item $iso -Recurse -Force -ErrorAction SilentlyContinue
if ($fail.Count) { Write-Host "`nGATE FAILED" -ForegroundColor Red; $fail | ForEach-Object { " - $_" }; exit 1 }
Write-Host "`nGATE PASSED  ($exe)" -ForegroundColor Green
