# Undo for the shell, end to end: the real exe, real files, a real command.
# Called by gate.ps1; also runs alone:
#   pwsh forge\chronicle.ps1 -Exe dist\proteus-<stamp>.exe
# It works only in a folder it makes under %TEMP%, with its own data folder,
# and refuses to start anywhere else. No window is opened.
param([Parameter(Mandatory)][string]$Exe, [string]$Scratch = (Join-Path $env:TEMP "proteus-chronicle-check-$PID"))
$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path $Exe).Path
$temp = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
$Scratch = [IO.Path]::GetFullPath($Scratch)
if (-not $Scratch.StartsWith($temp, [StringComparison]::OrdinalIgnoreCase)) { throw "refused: $Scratch is not under $temp" }
$ownData = -not $env:PROTEUS_DATA
if ($ownData) { $env:PROTEUS_DATA = "$Scratch\data"; $env:PROTEUS_SOCKET = "$Scratch\check.sock"; $env:PROTEUS_SOUND = 'off' }
if (-not [IO.Path]::GetFullPath($env:PROTEUS_DATA).StartsWith($temp, [StringComparison]::OrdinalIgnoreCase)) { throw "refused: PROTEUS_DATA ($env:PROTEUS_DATA) is not under $temp" }

$root = "$Scratch\tree"
Remove-Item $Scratch -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$root\dir", $env:PROTEUS_DATA | Out-Null
Set-Content "$root\keep.txt" 'kept' -NoNewline
Set-Content "$root\edit.txt" 'old' -NoNewline
Set-Content "$root\victim.txt" 'doomed' -NoNewline
Set-Content "$root\dir\gone.txt" 'in a folder the command removes' -NoNewline

# Everything under the tree, as path -> content hash.
function Picture {
    $p = @{}
    Get-ChildItem $root -Recurse -File | ForEach-Object { $p[$_.FullName.Substring($root.Length + 1)] = (Get-FileHash $_.FullName).Hash }
    Get-ChildItem $root -Recurse -Directory | ForEach-Object { $p[$_.FullName.Substring($root.Length + 1) + '\'] = 'folder' }
    ($p.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join '|'
}
# Run the exe on the tree and read its JSON. Piping makes PowerShell wait for it.
function Chronicle {
    $out = & $Exe chronicle @args --root $root --json 2>&1 | Out-String
    $script:code = $LASTEXITCODE
    try { $out | ConvertFrom-Json } catch { throw "chronicle $args (exit $script:code) did not print JSON: $out" }
}
function Need($ok, $what) { if (-not $ok) { throw $what } }

try {
    $t = Chronicle track
    Need ($t.files -eq 4 -and $t.failures.Count -eq 0) "track: $($t | ConvertTo-Json -Compress -Depth 5)"
    $before = Picture

    # One command that adds, edits and deletes files, removes a folder, and exits with 7.
    Push-Location $root
    try {
        $out = & $Exe run --root $root --json -m 'the gate' -- cmd /c 'echo new> created.txt& echo changed> edit.txt& del victim.txt& rmdir /s /q dir& mkdir made& echo x> made\inner.txt& exit 7' 2>&1 | Out-String
        $exit = $LASTEXITCODE
    } finally { Pop-Location }
    Need ($exit -eq 7) "proteus run returned $exit, not the command's 7. Output: $out"
    $step = ($out -split "`n" | Where-Object { $_.TrimStart().StartsWith('{') } | Select-Object -Last 1) | ConvertFrom-Json
    Need ($step.kind -eq 'run' -and $step.exit -eq 7 -and $step.added -eq 3 -and $step.removed -eq 3 -and $step.modified -eq 1) "the step was recorded as: $out"
    $after = Picture
    Need ($after -ne $before) 'the command changed nothing?'

    $dry = Chronicle undo
    Need ((-not $dry.applied) -and $dry.plan.write -eq 3 -and $dry.plan.delete -eq 2 -and (Picture) -eq $after) "dry run: $($dry.plan | ConvertTo-Json -Compress -Depth 5)"

    $undo = Chronicle undo --apply
    Need ($script:code -eq 0 -and $undo.complete -and $undo.verified -and $undo.back_to_before) "undo: $($undo | ConvertTo-Json -Compress -Depth 6)"
    Need ((Picture) -eq $before) 'after undo the folder is not what it was before the command'

    $redo = Chronicle undo --apply
    Need ($redo.complete -and $redo.back_to_before -and (Picture) -eq $after) 'undoing the undo did not bring the result of the command back'

    $first = Chronicle restore 1 --apply
    Need ($first.complete -and $first.reached_target -and (Picture) -eq $before) 'restore to the first snapshot did not bring the first state back'

    $log = Chronicle log
    $steps = @($log.entries | Where-Object { $_.t -eq 'step' } | ForEach-Object { $_.kind }) -join ','
    Need ($steps -eq 'run,undo,undo,restore') "steps in the log: $steps"
    $s = Chronicle status
    Need ($s.clean -and $s.files -eq 4) "status: $($s | ConvertTo-Json -Compress -Depth 4)"

    "run (exit 7, {0} ms) -> undo ({1} ms) -> undo again -> restore: folder identical each time; {2} log entries, store {3:N0} bytes" -f $step.ms, $undo.apply_ms, $log.total, $s.stored_bytes
} finally {
    if ($ownData) { Remove-Item Env:PROTEUS_DATA, Env:PROTEUS_SOCKET, Env:PROTEUS_SOUND -ErrorAction SilentlyContinue }
    Remove-Item $Scratch -Recurse -Force -ErrorAction SilentlyContinue
}
