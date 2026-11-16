# Build the measurement tree: real source files from the cargo registry,
# copied (read-only use of the registry) into <temp>\proteus-chron-lab\<Name>:
# exactly -Files files totalling about -TotalMB. Ignore files are left out so
# that every file copied is tracked. The mix: files under 32 KB as they come,
# plus as many files of 32 KB to 4 MB as it takes to reach the total.
param([int]$Files = 10000, [int]$TotalMB = 200, [string]$Name = 'tree')
$ErrorActionPreference = 'Stop'
$lab = Join-Path $env:TEMP 'proteus-chron-lab'
$dst = Join-Path $lab $Name
if (Test-Path $dst) { Remove-Item $dst -Recurse -Force }
New-Item -ItemType Directory -Force $dst | Out-Null
$src = (Get-ChildItem "$env:USERPROFILE\.cargo\registry\src" -Directory | Select-Object -First 1).FullName
$small = [Collections.Generic.List[object]]::new(); $large = [Collections.Generic.List[object]]::new()
foreach ($f in [IO.Directory]::EnumerateFiles($src, '*', [IO.SearchOption]::AllDirectories)) {
    $name = [IO.Path]::GetFileName($f)
    if ($name -in '.gitignore', '.ignore', '.proteusignore', '.gitattributes') { continue }
    if ($f.Substring($src.Length) -match '\\\.git(\\|$)') { continue }
    $len = ([IO.FileInfo]$f).Length
    if ($len -lt 32KB) { $small.Add([pscustomobject]@{ P = $f; L = $len }) } elseif ($len -le 4MB) { $large.Add([pscustomobject]@{ P = $f; L = $len }) }
}
$avgS = ($small | ForEach-Object { $_.L } | Measure-Object -Average).Average
$avgL = ($large | ForEach-Object { $_.L } | Measure-Object -Average).Average
$nL = [int][Math]::Round(($TotalMB * 1MB - $Files * $avgS) / ($avgL - $avgS))
$nL = [Math]::Max(0, [Math]::Min($nL, [Math]::Min($large.Count, $Files)))
# Spread the picks over the whole registry rather than its first crates.
$pick = { param($list, $n) $step = [Math]::Max(1.0, $list.Count / [double][Math]::Max(1, $n)); 0..($n - 1) | ForEach-Object { $list[[int][Math]::Floor($_ * $step)] } }
$chosen = @(& $pick $large $nL) + @(& $pick $small ($Files - $nL))
$count = 0; [long]$bytes = 0
$sw = [Diagnostics.Stopwatch]::StartNew()
foreach ($c in $chosen) {
    $to = Join-Path $dst $c.P.Substring($src.Length + 1)
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($to)) | Out-Null
    [IO.File]::Copy($c.P, $to)
    $count++; $bytes += $c.L
}
"{0} files ({1} of 32 KB or more), {2:N1} MB, copied in {3:N1} s -> {4}" -f $count, $nL, ($bytes / 1MB), $sw.Elapsed.TotalSeconds, $dst
"folders: {0}; registry had {1} small and {2} large candidates" -f ([IO.Directory]::GetDirectories($dst, '*', [IO.SearchOption]::AllDirectories)).Count, $small.Count, $large.Count
