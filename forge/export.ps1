# Publish the current commit as the public repository, without this
# checkout's history and without anything that identifies the machine it
# was built on.
#   pwsh forge\export.ps1 [-Repo OWNER/NAME] [-Message TEXT] [-DryRun]
#
# The public repository has its own linear history: each export is one
# commit on top of it. Private history (paths, machine details in old
# commits) never leaves this checkout. Files listed in forge\export\drop.txt
# are left out; forge\export\replace.txt maps private strings to neutral ones
# of the same length (so binary journals keep their record lengths); files in
# forge\export\public\ replace their private counterparts. Afterwards every
# byte of every file is searched for forge\export\forbidden.txt and the push
# is refused on any hit.
param([string]$Repo = 'karimalsalah/proteus', [string]$Message = '', [switch]$DryRun)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
if (git status --porcelain) { throw 'commit first: the export is made from HEAD' }
$cfg = Join-Path $root 'forge\export'
$work = Join-Path $env:TEMP "proteus-export-$PID"
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$work\tree" | Out-Null

# 1. The tree at HEAD.
git archive --format=tar -o "$work\head.tar" HEAD
tar -x -f "$work\head.tar" -C "$work\tree"
Remove-Item "$work\head.tar"

# 2. Leave out what is private by nature.
foreach ($line in Get-Content "$cfg\drop.txt") {
    $p = $line.Trim(); if (-not $p -or $p.StartsWith('#')) { continue }
    Remove-Item (Join-Path "$work\tree" $p) -Recurse -Force -ErrorAction SilentlyContinue
}
Remove-Item "$work\tree\forge\export" -Recurse -Force

# 3. Public versions of files that differ.
$pub = Join-Path $cfg 'public'
Get-ChildItem $pub -Recurse -File -Force | ForEach-Object {
    $rel = $_.FullName.Substring($pub.Length + 1)
    $dest = Join-Path "$work\tree" $rel
    New-Item -ItemType Directory -Force (Split-Path $dest -Parent) | Out-Null
    Copy-Item $_.FullName $dest -Force
}

# 4. Same-length replacement, byte for byte, in every file (UTF-8 and UTF-16LE).
$pairs = @(foreach ($line in Get-Content "$cfg\replace.txt") {
    if (-not $line.Trim() -or $line.StartsWith('#')) { continue }
    $from, $to = $line -split ' => ', 2
    if ($from.Length -ne $to.Length) { throw "replace.txt: '$from' and '$to' differ in length" }
    , @($from, $to)
})
# Latin-1 maps every byte to one character and back, so string replacement
# on it is byte replacement; a UTF-16LE needle is the same text with a zero
# byte after each character.
$l1 = [Text.Encoding]::Latin1
function Forms([string]$s) { , @($s, (($s.ToCharArray() | ForEach-Object { "$_`0" }) -join '')) }
$changed = 0
Get-ChildItem "$work\tree" -Recurse -File -Force | ForEach-Object {
    $text = $l1.GetString([IO.File]::ReadAllBytes($_.FullName)); $before = $text
    foreach ($p in $pairs) {
        foreach ($case in @(@($p[0], $p[1]), @($p[0].ToLower(), $p[1].ToLower()), @($p[0].ToUpper(), $p[1].ToUpper()))) {
            $f = Forms $case[0]; $r = Forms $case[1]
            $text = $text.Replace($f[0], $r[0]).Replace($f[1], $r[1])
        }
    }
    if (-not [object]::ReferenceEquals($text, $before) -and $text -ne $before) { [IO.File]::WriteAllBytes($_.FullName, $l1.GetBytes($text)); $script:changed++ }
}
"replaced private strings in $changed files"

# 5. Nothing forbidden may remain, in any file, in any encoding or case.
$bad = @()
$needles = Get-Content "$cfg\forbidden.txt" | Where-Object { $_.Trim() -and -not $_.StartsWith('#') } | ForEach-Object { $_.Trim().ToLowerInvariant() }
$keyFile = Join-Path $env:USERPROFILE '.proteus-release\signing.key'
if (Test-Path $keyFile) { $needles += (Get-Content $keyFile -Raw).Trim().ToLowerInvariant() }
Get-ChildItem "$work\tree" -Recurse -File -Force | ForEach-Object {
    $text = $l1.GetString([IO.File]::ReadAllBytes($_.FullName)).ToLowerInvariant()
    foreach ($s in $needles) { $f = Forms $s; if ($text.Contains($f[0]) -or $text.Contains($f[1])) { $bad += "$($_.FullName.Substring($work.Length + 6)): $s" } }
}
if ($bad) { $bad | ForEach-Object { "  FORBIDDEN $_" }; throw "not exported: $($bad.Count) forbidden strings remain" }
"searched every byte of {0} files: nothing forbidden" -f (Get-ChildItem "$work\tree" -Recurse -File -Force).Count

# 6. Commit on top of the public history.
$login = gh api user -q .login; $id = gh api user -q .id
$author = "$login <$id+$login@users.noreply.github.com>"
$repoDir = "$work\repo"
$exists = (gh api "repos/$Repo/commits?per_page=1" 2>$null) -and $LASTEXITCODE -eq 0
if ($exists) { git clone --quiet "https://github.com/$Repo.git" $repoDir } else { New-Item -ItemType Directory -Force $repoDir | Out-Null; git -C $repoDir init --quiet -b main }
Get-ChildItem $repoDir -Force | Where-Object { $_.Name -ne '.git' } | Remove-Item -Recurse -Force
Copy-Item "$work\tree\*" $repoDir -Recurse -Force
Get-ChildItem "$work\tree" -Force -Filter '.*' | ForEach-Object { Copy-Item $_.FullName $repoDir -Recurse -Force }
git -C $repoDir add -A
$src = (git rev-parse --short HEAD)
$build = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
if (-not $Message) { $Message = "PROTEUS $build" }
git -C $repoDir -c "user.name=$login" -c "user.email=$id+$login@users.noreply.github.com" commit --quiet -m $Message -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
if ($LASTEXITCODE -ne 0) { "nothing changed since the last export"; return }
git -C $repoDir log --oneline -1
if ($DryRun) { "dry run: not pushed. Tree at $repoDir"; return }
if (-not $exists) { git -C $repoDir remote add origin "https://github.com/$Repo.git" }
git -C $repoDir push --quiet origin main
"pushed $Repo (from $src)"
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
