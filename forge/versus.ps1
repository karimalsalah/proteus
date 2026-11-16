<#
VERSUS: PROTEUS against the other terminals installed on this machine.

  pwsh forge\versus.ps1 [-Runs 3] [-Terminals proteus,wt,...] [-Workloads ascii,...]
                        [-SkipIdle] [-Proteus dist\proteus-x.exe] [-Out forge\out\versus\<stamp>]

Each terminal is started with a workload as its program, using the
terminal's own command-line way to run one. The workload (forge\versus,
`versus emit`) writes to its stdout and times its own writes: a terminal that
consumes slowly makes the writes block, so the figure is the throughput the
terminal imposes. Every terminal runs inside a fresh job object
(`versus run`), which gives CPU seconds of the whole process tree (including
processes that have exited) and lets the harness stop exactly the processes
it started and nothing else. Nothing is ever stopped by name.

Windows open while this runs. They are started with SW_SHOWNOACTIVATE and
PROTEUS with PROTEUS_NO_FOCUS=1; none is minimised, because a minimised
terminal may skip drawing, which would flatter it.

Runs are interleaved: run 1 of every terminal for a workload, then run 2,
with the terminal order rotated, so a change in machine load falls on every
terminal rather than on one.

Output: <Out>\results.json (every raw record and the medians) and
<Out>\table.md. forge\versus.md cites a results.json by path.
#>
param(
    [int]$Runs = 3,
    [string[]]$Terminals = @('proteus', 'proteus-local', 'proteus-nojournal', 'wt', 'wtp', 'wezterm', 'ghostty', 'conhost'),
    [string[]]$Workloads = @('ascii', 'sgr', 'unicode', 'cargo', 'tui', 'drip'),
    [switch]$SkipIdle,
    [switch]$SkipWorkloads,
    [string]$Proteus = '',
    [string]$Out = '',
    [int]$Cols = 123,
    [int]$Rows = 30,
    [int]$IdleSeconds = 20,
    # The console-host stage alone (versus sink): each bundled ConPTY with a
    # reader that discards, no terminal. Separates ConPTY from the terminal.
    [switch]$Stages,
    # Rebuild results.json and table.md from the run folders already in -Out
    # (for instance a campaign that was stopped part-way), running nothing.
    [switch]$Summarize
)
$ErrorActionPreference = 'Stop'
# `pwsh -File` passes a list as one comma-separated string.
$Terminals = @($Terminals | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$Workloads = @($Workloads | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
$env:CARGO_BUILD_JOBS = '2'

# --- the tool ---------------------------------------------------------------
$env:CARGO_TARGET_DIR = Join-Path $root 'forge\out\versus-target'
Push-Location (Join-Path $root 'forge\versus')
cargo build --release --offline -q
if ($LASTEXITCODE -ne 0) { throw 'building forge\versus failed' }
Pop-Location
Remove-Item Env:CARGO_TARGET_DIR
$v = Join-Path $root 'forge\out\versus-target\release\versus.exe'
$cargoLog = Join-Path $root 'forge\samples\cargo-build.log'

if (-not $Proteus) {
    $Proteus = Get-ChildItem (Join-Path $root 'dist') -Filter 'proteus-*.exe' | Sort-Object LastWriteTime | Select-Object -Last 1 -ExpandProperty FullName
}
if (-not $Out) { $Out = Join-Path $root ('forge\out\versus\' + (Get-Date -Format 'yyyyMMdd-HHmmss')) }
New-Item -ItemType Directory -Force $Out | Out-Null

# --- isolation for PROTEUS (never the person's live data) -------------------
$pdata = Join-Path $env:TEMP ('versus-proteus-' + (Get-Random))
New-Item -ItemType Directory -Force $pdata | Out-Null
$env:PROTEUS_DATA = $pdata
$env:PROTEUS_NO_FOCUS = '1'
$env:PROTEUS_SOUND = 'off'

# --- the terminals ----------------------------------------------------------
$apps = "$env:LOCALAPPDATA\Microsoft\WindowsApps"
$tools = "$env:LOCALAPPDATA\tools"
$defs = [ordered]@{
    proteus = @{ name = 'PROTEUS'; launch = { param($c) "`"$Proteus`" -e $c" } }
    # The same exe with the session in the window process (--local: no daemon
    # hop, journal kept) and with no journal at all (--no-journal, which is
    # also in-process). Against the default row they put a loss on a stage.
    'proteus-local' = @{ name = 'PROTEUS --local'; launch = { param($c) "`"$Proteus`" --local -e $c" } }
    'proteus-nojournal' = @{ name = 'PROTEUS --no-journal'; launch = { param($c) "`"$Proteus`" --no-journal -e $c" } }
    wt      = @{ name = 'Windows Terminal'; launch = { param($c) "$apps\Microsoft.WindowsTerminal_8wekyb3d8bbwe\wt.exe -w new --size $Cols,$Rows new-tab -- $c" } }
    wtp     = @{ name = 'Windows Terminal Preview'; launch = { param($c) "$apps\Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe\wt.exe -w new --size $Cols,$Rows new-tab -- $c" } }
    # -n: no config file (the defaults), so the owner's settings do not decide the result.
    wezterm = @{ name = 'WezTerm'; launch = { param($c) "$tools\wezterm\wezterm-gui.exe -n --config initial_cols=$Cols --config initial_rows=$Rows start --always-new-process -- $c" } }
    ghostty = @{ name = 'Ghostty'; launch = { param($c) "$tools\ghostty\bin\ghostty.exe --config-default-files=false --window-width=$Cols --window-height=$Rows -e $c" } }
    conhost = @{ name = 'conhost'; launch = { param($c) "$env:windir\System32\conhost.exe $c" } }
}

function Workload-Args([string]$w) {
    switch ($w) {
        'ascii'   { '--kind ascii --bytes 16777216' }
        'sgr'     { '--kind sgr --bytes 16777216' }
        'unicode' { '--kind unicode --bytes 16777216' }
        'cargo'   { "--kind cargo --bytes 8388608 --log `"$cargoLog`"" }
        'tui'     { '--kind tui --frames 600' }
        'drip'    { '--kind drip --lines 500 --interval-ms 10' }
        'warm'    { '--kind ascii --bytes 1048576' }
    }
}

function Invoke-One([string]$term, [string]$work, [int]$n) {
    $dir = Join-Path $Out "$term\$work\$n"
    New-Item -ItemType Directory -Force $dir | Out-Null
    $rpt = Join-Path $dir 'workload.json'
    $run = Join-Path $dir 'run.json'
    # A fresh daemon socket per run: each PROTEUS run starts its own daemon
    # inside the job, so the daemon's CPU is counted and it is stopped after.
    $env:PROTEUS_SOCKET = Join-Path $env:TEMP ('versus-sock-' + (Get-Random))
    $cmd = "`"$v`" emit $(Workload-Args $work) --report `"$rpt`" --t0 {T0}"
    $line = & $defs[$term].launch $cmd
    & $v run --out $run --report $rpt --timeout 240 --grace 3 -- $line
    if ($LASTEXITCODE -ne 0) { Write-Warning "$term $work $n : runner exit $LASTEXITCODE" }
    $r = [ordered]@{ terminal = $term; workload = $work; run = $n }
    if (Test-Path $run) { $r.runner = Get-Content $run -Raw | ConvertFrom-Json }
    if (Test-Path $rpt) { $r.workload_report = Get-Content $rpt -Raw | ConvertFrom-Json }
    $r
}

function Invoke-Idle([string]$term, [int]$n) {
    $dir = Join-Path $Out "$term\idle\$n"
    New-Item -ItemType Directory -Force $dir | Out-Null
    $run = Join-Path $dir 'run.json'
    $env:PROTEUS_SOCKET = Join-Path $env:TEMP ('versus-sock-' + (Get-Random))
    $line = & $defs[$term].launch "$env:windir\System32\cmd.exe"
    & $v run --out $run --idle $IdleSeconds --settle 10 -- $line
    $r = [ordered]@{ terminal = $term; workload = 'idle'; run = $n }
    if (Test-Path $run) { $r.runner = Get-Content $run -Raw | ConvertFrom-Json }
    $r
}

# --- machine and versions ---------------------------------------------------
$verOf = [ordered]@{
    proteus = (Split-Path $Proteus -Leaf)
    'proteus-local' = (Split-Path $Proteus -Leaf)
    'proteus-nojournal' = (Split-Path $Proteus -Leaf)
    wt      = (Get-AppxPackage Microsoft.WindowsTerminal).Version
    wtp     = (Get-AppxPackage Microsoft.WindowsTerminalPreview).Version
    wezterm = (& "$tools\wezterm\wezterm.exe" --version)
    ghostty = ((& "$tools\ghostty\bin\ghostty.exe" --version 2>&1 | Select-String 'version:' | Select-Object -First 1) -replace '.*version:\s*', '')
    conhost = (Get-Item "$env:windir\System32\conhost.exe").VersionInfo.ProductVersion
}
$os = Get-CimInstance Win32_OperatingSystem
$machine = [ordered]@{
    cpu = (Get-CimInstance Win32_Processor).Name
    gpu = (Get-CimInstance Win32_VideoController | Select-Object -First 1 | ForEach-Object { "$($_.Name) driver $($_.DriverVersion)" })
    os = "$($os.Caption) build $($os.BuildNumber)"
    logical_cpus = [Environment]::ProcessorCount
    ram_gb = [math]::Round($os.TotalVisibleMemorySize / 1MB, 1)
    started = (Get-Date -Format 's')
    other_processes = (Get-Process).Count
}

# --- runs -------------------------------------------------------------------
$records = New-Object System.Collections.ArrayList
$order = @($Terminals)
if ($Summarize) { $SkipWorkloads = $true; $SkipIdle = $true; $Stages = $false; $machine.note = 'summarised from disk; machine fields describe the summary time, not the runs' }
else {
    "warm-up (discarded): one 1 MB run per terminal"
    foreach ($t in $order) { [void](Invoke-One $t 'warm' 0) }
}
if (-not $SkipWorkloads) {
    foreach ($w in $Workloads) {
        for ($n = 1; $n -le $Runs; $n++) {
            $rot = ($n - 1) % $order.Count
            $seq = @($order[$rot..($order.Count - 1)]) + @(if ($rot -gt 0) { $order[0..($rot - 1)] })
            foreach ($t in $seq) {
                $rec = Invoke-One $t $w $n
                [void]$records.Add($rec)
                $wr = $rec.workload_report
                "{0,-8} {1,-8} run {2}: {3}" -f $t, $w, $n, $(if ($wr) { "$($wr.mb_per_s) MB/s, $($wr.fps) fps, cpu $($rec.runner.cpu_s) s" } else { 'NO REPORT' })
            }
        }
    }
}
if (-not $SkipIdle) {
    for ($n = 1; $n -le $Runs; $n++) {
        foreach ($t in $order) {
            $rec = Invoke-Idle $t $n
            [void]$records.Add($rec)
            "{0,-8} idle     run {1}: {2}% of a core, {3} MB" -f $t, $n, $rec.runner.idle.cpu_pct_of_one_core, $rec.runner.idle.ws_mb_avg
        }
    }
}
$stageRecs = New-Object System.Collections.ArrayList
if ($Stages) {
    $hosts = [ordered]@{
        'proteus-1.24' = Join-Path $root 'vendor\conpty'
        'wezterm-1.22' = "$tools\wezterm"
        'ghostty-1.25' = "$tools\ghostty\bin"
        'inbox' = 'system'
    }
    foreach ($w in @('ascii', 'sgr', 'unicode')) {
        for ($n = 1; $n -le $Runs; $n++) {
            foreach ($k in $hosts.Keys) {
                $dir = Join-Path $Out "stage\$k\$w\$n"
                New-Item -ItemType Directory -Force $dir | Out-Null
                $rpt = Join-Path $dir 'workload.json'
                & $v sink --conpty $hosts[$k] --out (Join-Path $dir 'sink.json') -- @((Workload-Args $w) -split ' ') --report $rpt
                $r = Get-Content $rpt -Raw | ConvertFrom-Json
                [void]$stageRecs.Add([ordered]@{ host = $k; workload = $w; run = $n; workload_report = $r; sink = (Get-Content (Join-Path $dir 'sink.json') -Raw | ConvertFrom-Json) })
                "{0,-13} {1,-8} run {2}: {3} MB/s (second half {4})" -f $k, $w, $n, $r.mb_per_s, $r.second_half_mb_per_s
            }
        }
    }
}
$machine.finished = (Get-Date -Format 's')

# Every record is read back from disk, so a stopped campaign can be summarised
# with -Summarize and the table never depends on what happened to be in memory.
$records = New-Object System.Collections.ArrayList
foreach ($t in $order) {
    $td = Join-Path $Out $t
    if (-not (Test-Path $td)) { continue }
    foreach ($wd in Get-ChildItem $td -Directory) {
        if ($wd.Name -eq 'warm') { continue }
        foreach ($nd in Get-ChildItem $wd.FullName -Directory) {
            $r = [ordered]@{ terminal = $t; workload = $wd.Name; run = [int]$nd.Name }
            $run = Join-Path $nd.FullName 'run.json'
            $rpt = Join-Path $nd.FullName 'workload.json'
            if (Test-Path $run) { $r.runner = Get-Content $run -Raw | ConvertFrom-Json }
            if (Test-Path $rpt) { $r.workload_report = Get-Content $rpt -Raw | ConvertFrom-Json }
            # A run is invalid if the terminal took the keyboard, or if Windows
            # Terminal handed the tab to a WT process outside this job.
            $r.invalid = @()
            if ($r.runner.took_foreground) { $r.invalid += 'took_foreground' }
            if ($r.runner.session_locked_at_some_point) { $r.invalid += 'session locked' }
            if ($t -in @('wt', 'wtp') -and $r.runner -and -not ($r.runner.procs | Where-Object { $_.name -eq 'WindowsTerminal.exe' })) { $r.invalid += 'no WindowsTerminal.exe in the job' }
            [void]$records.Add($r)
        }
    }
}

# --- summary ----------------------------------------------------------------
function Stat($xs) {
    $s = @($xs | Where-Object { $_ -ne $null } | Sort-Object)
    if ($s.Count -eq 0) { return $null }
    [ordered]@{ median = $s[[int][math]::Floor(($s.Count - 1) / 2)] ; min = $s[0]; max = $s[-1]; n = $s.Count }
}
$summary = [ordered]@{}
foreach ($t in $order) {
    $summary[$t] = [ordered]@{ name = $defs[$t].name; version = "$($verOf[$t])" }
    foreach ($w in $Workloads + @('idle')) {
        $rs = @($records | Where-Object { $_.terminal -eq $t -and $_.workload -eq $w -and $_.invalid.Count -eq 0 })
        if ($rs.Count -eq 0) { continue }
        if ($w -eq 'idle') {
            $summary[$t].idle = [ordered]@{
                cpu_pct = Stat ($rs | ForEach-Object { $_.runner.idle.cpu_pct_of_one_core })
                cpu_s = Stat ($rs | ForEach-Object { $_.runner.idle.cpu_s })
                visible_share_min = Stat ($rs | ForEach-Object { $_.runner.visible_share_min })
                invalid_runs = @($records | Where-Object { $_.terminal -eq $t -and $_.workload -eq 'idle' -and $_.invalid.Count -gt 0 }).Count
                ws_mb = Stat ($rs | ForEach-Object { $_.runner.idle.ws_mb_avg })
            }
            continue
        }
        # Terminal CPU: the job's total less the workload's own, when the
        # workload ran inside the job (some terminals start it outside).
        $cpu = $rs | ForEach-Object {
            if (-not $_.runner -or -not $_.workload_report) { return }
            $c = [double]$_.runner.cpu_s
            if ($_.workload_report.in_runner_job -eq $true) { $c -= [double]$_.workload_report.self_cpu_s }
            $c
        }
        $summary[$t][$w] = [ordered]@{
            mb_per_s = Stat ($rs | ForEach-Object { $_.workload_report.mb_per_s })
            sustained_mb_per_s = Stat ($rs | ForEach-Object { $_.workload_report.second_half_mb_per_s })
            machine_cpu_busy_pct = Stat ($rs | ForEach-Object { $_.runner.machine_cpu_busy_pct })
            min_free_ram_mb = Stat ($rs | ForEach-Object { $_.runner.min_free_ram_mb })
            seconds = Stat ($rs | ForEach-Object { $_.workload_report.seconds })
            fps = if ($w -eq 'tui') { Stat ($rs | ForEach-Object { $_.workload_report.fps }) }
            first_write_ms = Stat ($rs | ForEach-Object { $_.workload_report.first_write_ms })
            tree_cpu_s = Stat $cpu
            # CPU of the job between the payload's first and last write: no
            # start-up, no shut-down, no harness grace period.
            payload_cpu_s = Stat ($rs | ForEach-Object {
                    if ($null -eq $_.runner.payload_cpu_s) { return }
                    $c = [double]$_.runner.payload_cpu_s
                    # Less the workload's own CPU over the same span (not its
                    # payload building before the marker).
                    $own = if ($null -ne $_.workload_report.payload_self_cpu_s) { $_.workload_report.payload_self_cpu_s } else { $_.workload_report.self_cpu_s }
                    if ($_.workload_report.in_runner_job -eq $true) { $c -= [double]$own }
                    $c })
            # 1 = the terminal's window was uncovered at every look, 0 = hidden.
            visible_share_min = Stat ($rs | ForEach-Object { $_.runner.visible_share_min })
            invalid_runs = @($records | Where-Object { $_.terminal -eq $t -and $_.workload -eq $w -and $_.invalid.Count -gt 0 }).Count
            peak_ws_mb = Stat ($rs | ForEach-Object { $_.runner.peak_ws_mb })
            drip_write_p99_ms = if ($w -eq 'drip') { Stat ($rs | ForEach-Object { $_.workload_report.write_ms.p99 }) }
            drip_late_p99_ms = if ($w -eq 'drip') { Stat ($rs | ForEach-Object { $_.workload_report.late_ms.p99 }) }
            grid = (($rs | ForEach-Object { "$($_.workload_report.cols)x$($_.workload_report.rows)" }) | Sort-Object -Unique) -join ','
            failures = @($rs | Where-Object { -not $_.workload_report }).Count
        }
    }
}
$doc = [ordered]@{ machine = $machine; versions = $verOf; cols = $Cols; rows = $Rows; runs = $Runs; summary = $summary; records = $records; stages = $stageRecs }
$doc | ConvertTo-Json -Depth 12 | Set-Content (Join-Path $Out 'results.json') -Encoding utf8

# --- table ------------------------------------------------------------------
function Cell($s, [string]$fmt) { if (-not $s) { return 'n/a' } ; ("{0:$fmt} ({1:$fmt}-{2:$fmt})" -f $s.median, $s.min, $s.max) }
$md = New-Object System.Collections.Generic.List[string]
$md.Add('| metric | ' + (($order | ForEach-Object { $defs[$_].name }) -join ' | ') + ' |')
$md.Add('|---|' + (($order | ForEach-Object { '---' }) -join '|') + '|')
foreach ($w in $Workloads) {
    if (-not ($order | Where-Object { $summary[$_].Contains($w) })) { continue }
    $rowsOut = switch ($w) {
        'tui'  { @(@('fps', 'fps', 'N1'), @('payload_cpu_s', 'payload CPU s', 'N2'), @('tree_cpu_s', 'launch-to-close CPU s', 'N2'), @('peak_ws_mb', 'peak MB', 'N0')) }
        'drip' { @(@('drip_write_p99_ms', 'write p99 ms', 'N2'), @('payload_cpu_s', 'payload CPU s', 'N2'), @('tree_cpu_s', 'launch-to-close CPU s', 'N2'), @('peak_ws_mb', 'peak MB', 'N0')) }
        default { @(@('sustained_mb_per_s', 'MB/s (second half)', 'N2'), @('mb_per_s', 'MB/s (whole)', 'N2'), @('payload_cpu_s', 'payload CPU s', 'N2'), @('tree_cpu_s', 'launch-to-close CPU s', 'N2'), @('peak_ws_mb', 'peak MB', 'N0')) }
    }
    foreach ($m in $rowsOut) {
        $md.Add("| $w $($m[1]) | " + (($order | ForEach-Object { if ($summary[$_].Contains($w)) { Cell $summary[$_][$w][$m[0]] $m[2] } else { 'n/a' } }) -join ' | ') + ' |')
    }
    $md.Add("| $w window uncovered share, least | " + (($order | ForEach-Object { if ($summary[$_].Contains($w)) { Cell $summary[$_][$w].visible_share_min 'N2' } else { 'n/a' } }) -join ' | ') + ' |')
    $md.Add("| $w machine busy % / least free RAM MB | " + (($order | ForEach-Object { $x = if ($summary[$_].Contains($w)) { $summary[$_][$w] }; if ($x -and $x.machine_cpu_busy_pct) { '{0:N0}% / {1:N0}' -f $x.machine_cpu_busy_pct.max, $x.min_free_ram_mb.min } else { 'n/a' } }) -join ' | ') + ' |')
}
if ($order | Where-Object { $summary[$_].Contains('ascii') }) { $md.Add('| launch to first write ms (ascii) | ' + (($order | ForEach-Object { Cell $summary[$_]['ascii'].first_write_ms 'N0' }) -join ' | ') + ' |') }
if ($order | Where-Object { $summary[$_].Contains('idle') }) {
    $md.Add('| idle CPU s over 20 s | ' + (($order | ForEach-Object { Cell $summary[$_].idle.cpu_s 'N3' }) -join ' | ') + ' |')
    $md.Add('| idle runs dropped (took foreground / locked) | ' + (($order | ForEach-Object { $summary[$_].idle.invalid_runs }) -join ' | ') + ' |')
    $md.Add('| idle CPU % of a core | ' + (($order | ForEach-Object { Cell $summary[$_].idle.cpu_pct 'N3' }) -join ' | ') + ' |')
    $md.Add('| idle working set MB | ' + (($order | ForEach-Object { Cell $summary[$_].idle.ws_mb 'N0' }) -join ' | ') + ' |')
}
if ($Stages) {
    $md.Add('')
    $md.Add('| ConPTY alone, null reader, MB/s (second half) | ascii | sgr | unicode |')
    $md.Add('|---|---|---|---|')
    foreach ($k in ($stageRecs | ForEach-Object { $_.host } | Select-Object -Unique)) {
        $md.Add("| $k | " + ((@('ascii', 'sgr', 'unicode') | ForEach-Object { $w = $_; Cell (Stat ($stageRecs | Where-Object { $_.host -eq $k -and $_.workload -eq $w } | ForEach-Object { $_.workload_report.second_half_mb_per_s })) 'N2' }) -join ' | ') + ' |')
    }
}
$md | Set-Content (Join-Path $Out 'table.md') -Encoding utf8
$md
"results: $Out\results.json"
