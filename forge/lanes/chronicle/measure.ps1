# The numbers in forge\lanes\chronicle.json, measured again.
#   pwsh forge\lanes\chronicle\corpus.ps1                      (once: builds the tree)
#   pwsh forge\lanes\chronicle\measure.ps1 -Exe dist\<copy>.exe [-What cold|warm|one|restore|run|watch|all]
# Everything runs headless against <temp>\proteus-chron-lab: its own data
# folder, its own socket, sound off. Nothing outside that folder is tracked,
# snapshotted or restored. Wall time is measured around the exe itself.
# "cold" deletes the lab's own data folder first. The only process stopped
# is the watch this script starts, by its id.
param([Parameter(Mandatory)][string]$Exe, [string]$What = 'all', [string]$Tree = 'tree', [int]$Runs = 5, [int]$IdleSeconds = 30, [int]$Debounce = 200)
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path $Exe).Path
$lab = Join-Path $env:TEMP 'proteus-chron-lab'
$env:PROTEUS_DATA = "$lab\data"
$env:PROTEUS_SOCKET = "$lab\lab.sock"
$env:PROTEUS_SOUND = 'off'
$root = Join-Path $lab $Tree
if (-not ([IO.Path]::GetFullPath($root)).StartsWith($lab + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'the tree must be inside the lab folder' }
if (-not (Test-Path $root)) { throw "no $root. Build it first: pwsh forge\lanes\chronicle\corpus.ps1" }

# Run the exe on the lab tree; returns the parsed JSON and the wall time in ms.
function Px {
    param([string[]]$A)
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $out = & $exe @A --root $root --json 2>&1 | Out-String
    $ms = $sw.Elapsed.TotalMilliseconds
    $code = $LASTEXITCODE
    try { $j = $out | ConvertFrom-Json } catch { throw "not JSON (exit $code): $out" }
    [pscustomobject]@{ j = $j; wall = [math]::Round($ms, 1); code = $code }
}
function Line($name, $r) {
    "{0,-22} ms={1,8} walk={2,7} read={3,8} wall={4,8}  read_files={5} new_objects={6} stored={7:N2} MB" -f $name, $r.j.ms, $r.j.walk_ms, $r.j.read_ms, $r.wall, $r.j.read_files, $r.j.new_objects, ($r.j.stored_bytes / 1MB)
}
$median = { param($a) ($a | Sort-Object)[[int](@($a).Count / 2)] }

if ($What -in 'cold', 'all') {
    # Cold: no store and no cache. Whether the files themselves were ever read
    # since they were written decides the first run (see chronicle.json).
    1..3 | ForEach-Object {
        Remove-Item "$lab\data" -Recurse -Force -ErrorAction SilentlyContinue
        Line "cold track #$_" (Px @('chronicle', 'track'))
    }
    $s = (Px @('chronicle', 'status')).j
    "tree {0:N1} MB in {1} files, {2} folders; stored {3:N1} MB in {4} objects = {5:P1} of the tree" -f ($s.bytes / 1MB), $s.files, $s.folders, ($s.stored_bytes / 1MB), $s.objects, ($s.stored_bytes / $s.bytes)
}
if ($What -in 'warm', 'all') {
    Start-Sleep -Seconds 3
    Px @('chronicle', 'snap') | Out-Null
    1..$Runs | ForEach-Object { Line "warm snap #$_" (Px @('chronicle', 'snap')) }
    1..2 | ForEach-Object { Line "rehash #$_" (Px @('chronicle', 'snap', '--rehash')) }
    $sw = [Diagnostics.Stopwatch]::StartNew(); & $exe --version | Out-Null; "process start (--version): {0:N1} ms" -f $sw.Elapsed.TotalMilliseconds
}
if ($What -in 'one', 'all') {
    # One file edited: what a single change costs to record.
    $f = Get-ChildItem $root -Recurse -File -Filter *.rs | Select-Object -First 1
    1..3 | ForEach-Object {
        Add-Content -LiteralPath $f.FullName -Value "// edit $_ $(Get-Date -Format o)"
        Line "one file edited #$_" (Px @('chronicle', 'snap'))
    }
}
if ($What -in 'restore', 'all') {
    $head = (Px @('chronicle', 'status')).j.last
    # 1. 100 files changed, then put back.
    Get-ChildItem $root -Recurse -File | Select-Object -First 100 | ForEach-Object { Add-Content -LiteralPath $_.FullName -Value 'changed' }
    $r = Px @('chronicle', 'restore', $head, '--apply')
    "restore 100 changed files: apply {0} ms, wall {1} ms, wrote {2}, complete={3} verified={4}" -f $r.j.apply_ms, $r.wall, $r.j.result.written, $r.j.complete, $r.j.verified
    # 2. Every file deleted, then the whole tree written back.
    Get-ChildItem $root | Remove-Item -Recurse -Force
    $r = Px @('chronicle', 'restore', $head, '--apply')
    "restore the whole tree:    apply {0} ms, wall {1} ms, wrote {2} files ({3:N1} MB), made {4} folders, complete={5} verified={6}" -f $r.j.apply_ms, $r.wall, $r.j.result.written, ($r.j.result.bytes / 1MB), $r.j.result.folders_made, $r.j.complete, $r.j.verified
    Line 'snap after restore' (Px @('chronicle', 'snap'))
}
if ($What -in 'run', 'all') {
    # What wrapping a command costs: `proteus run -- cmd /c exit 0` against `cmd /c exit 0`.
    Start-Sleep -Seconds 3
    Px @('chronicle', 'snap') | Out-Null
    Push-Location $root
    try {
        $with = @(); $snaps = @()
        1..7 | ForEach-Object {
            $sw = [Diagnostics.Stopwatch]::StartNew()
            $out = & $exe run --root $root --json -- cmd /c exit 0 2>&1 | Out-String
            $with += $sw.Elapsed.TotalMilliseconds
            $j = ($out -split "`n" | Where-Object { $_.TrimStart().StartsWith('{') } | Select-Object -Last 1) | ConvertFrom-Json
            $snaps += ($j.snapshot_ms[0] + $j.snapshot_ms[1])
        }
        $bare = 1..7 | ForEach-Object { $sw = [Diagnostics.Stopwatch]::StartNew(); cmd /c exit 0 | Out-Null; $sw.Elapsed.TotalMilliseconds }
        "proteus run -- cmd /c exit 0: median {0:N0} ms wall (its two snapshots: {1:N0} ms); cmd /c exit 0 alone: median {2:N0} ms" -f (& $median $with), (& $median $snaps), (& $median $bare)
        # The same with every file read before and after (what it takes to see a write made through a mapped view).
        $with = @(); $snaps = @()
        1..5 | ForEach-Object {
            $sw = [Diagnostics.Stopwatch]::StartNew()
            $out = & $exe run --root $root --json --rehash -- cmd /c exit 0 2>&1 | Out-String
            $with += $sw.Elapsed.TotalMilliseconds
            $j = ($out -split "`n" | Where-Object { $_.TrimStart().StartsWith('{') } | Select-Object -Last 1) | ConvertFrom-Json
            $snaps += ($j.snapshot_ms[0] + $j.snapshot_ms[1])
        }
        "proteus run --rehash -- cmd /c exit 0: median {0:N0} ms wall (its two snapshots: {1:N0} ms)" -f (& $median $with), (& $median $snaps)
    } finally { Pop-Location }
}
if ($What -in 'watch', 'all') {
    # `chronicle watch`: CPU while nothing changes, then the time from a file
    # being written to its snapshot being on disk.
    $out = "$lab\watch-$PID.out"
    $p = Start-Process -FilePath $exe -ArgumentList 'chronicle', 'watch', '--root', $root, '--json', '--debounce', "$Debounce" -RedirectStandardOutput $out -RedirectStandardError "$out.err" -NoNewWindow -PassThru
    try {
        $lines = { @(Get-Content $out -ErrorAction SilentlyContinue | Where-Object { $_ }) }
        $deadline = (Get-Date).AddSeconds(60)
        while (@(& $lines).Count -lt 1) { if ((Get-Date) -gt $deadline) { throw "watch printed nothing: $(Get-Content "$out.err" -Raw)" }; Start-Sleep -Milliseconds 100 }
        $first = @(& $lines)[0] | ConvertFrom-Json
        "watching {0} files (pid {1}); first capture {2} ms, read {3} files" -f $first.files, $p.Id, $first.ms, $first.read_files
        Start-Sleep -Seconds 4
        if ($IdleSeconds -gt 0) {
            $p.Refresh(); $c0 = $p.TotalProcessorTime.TotalMilliseconds; $n0 = @(& $lines).Count
            $sw = [Diagnostics.Stopwatch]::StartNew()
            Start-Sleep -Seconds $IdleSeconds
            $p.Refresh(); $c1 = $p.TotalProcessorTime.TotalMilliseconds; $secs = $sw.Elapsed.TotalSeconds
            "idle: {0:N1} ms of CPU in {1:N1} s = {2:N3}% of one core; {3} captures in that time; working set {4:N1} MB, threads {5}" -f ($c1 - $c0), $secs, (100 * ($c1 - $c0) / ($secs * 1000)), (@(& $lines).Count - $n0), ($p.WorkingSet64 / 1MB), $p.Threads.Count
        }
        $total = @(); $inner = @(); $snap = @()
        $file = Join-Path $root 'latency-probe.txt'
        for ($i = 1; $i -le 10; $i++) {
            $n = @(& $lines).Count
            $t = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
            [IO.File]::WriteAllText($file, "edit $i at $t")
            $deadline = (Get-Date).AddSeconds(30)
            while (@(& $lines).Count -le $n) { if ((Get-Date) -gt $deadline) { throw 'no capture within 30 s' }; Start-Sleep -Milliseconds 10 }
            $c = @(& $lines)[$n] | ConvertFrom-Json
            $total += ($c.recorded_ms - $t); $inner += $c.latency_ms; $snap += $c.ms
            Start-Sleep -Milliseconds 700
        }
        $stat = { param($a) $s = $a | Sort-Object; "min {0:N0}, median {1:N0}, max {2:N0}" -f $s[0], $s[[int]($s.Count / 2)], $s[-1] }
        "debounce {0} ms, 10 edits of one file, tree of {1} files:" -f $Debounce, $first.files
        "  file written -> snapshot recorded (ms): $(& $stat $total)"
        "  first event read -> recorded (ms):      $(& $stat $inner)"
        "  of which the snapshot itself (ms):      $(& $stat $snap)"
        Remove-Item $file -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 800
    } finally {
        Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 300
        Remove-Item $out, "$out.err" -ErrorAction SilentlyContinue
    }
}
