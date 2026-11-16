# Console-host flood A/B (src/conhost.rs), configurations interleaved, 3 runs each.
#   pwsh forge\conhost-matrix.ps1 -Out matrix.jsonl -Other <folder holding another conpty.dll + OpenConsole.exe>
# Runs a dist\ copy of the exe in an isolated data folder.
param([string]$Out, [string]$Other, [int]$Mb = 32, [string]$Exe = (Join-Path $PSScriptRoot '..\dist\proteus-conhost.exe'))
$d = Join-Path $env:TEMP "proteus-conhost-matrix-$PID"; New-Item -ItemType Directory -Force $d | Out-Null
$env:PROTEUS_DATA=$d; $env:PROTEUS_SOCKET="$d\sock"; $env:PROTEUS_SOUND='off'
$hosts = @('bundled') + @($Other | Where-Object { $_ })
$cfgs = @()
foreach ($h in $hosts) { foreach ($pipe in 'anon','overlapped') { foreach ($sink in 'null','parse') { foreach ($kind in 'ascii','sgr') {
  $cfgs += ,@('--host', $h, '--pipe', $pipe, '--sink', $sink, '--kind', $kind, '--mb', "$Mb")
} } } }
# Small writes, as a program printing line by line does.
foreach ($h in $hosts) { foreach ($pipe in 'anon','overlapped') {
  $cfgs += ,@('--host', $h, '--pipe', $pipe, '--sink', 'null', '--kind', 'ascii', '--mb', '8', '--chunk', '80')
} }
foreach ($run in 1..3) { foreach ($c in $cfgs) {
  $line = & $Exe conhost flood @c 2>$null | Out-String
  if ($line.Trim()) { Add-Content $Out $line.Trim() } else { Add-Content $Out ('{"error":"' + ($c -join ' ').Replace('\','/') + '"}') }
} }
