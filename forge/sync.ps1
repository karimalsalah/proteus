# Bring changes made on the public repository (merged pull requests, Dependabot
# updates, edits on GitHub) back into this checkout, so the next export does
# not overwrite them.
#   pwsh forge\sync.ps1 [-Repo OWNER/NAME]
# The public commit this checkout last exported or synced is remembered in
# .git\proteus-public-head. Everything after it on the public main branch is
# applied here as one commit. Files whose public version lives in
# forge\export\public\ are patched there. Run by forge\export.ps1 first.
param([string]$Repo = 'karimalsalah/proteus')
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
if (git status --porcelain) { throw 'commit first: sync applies public changes as their own commit' }
$marker = Join-Path (git rev-parse --git-dir) 'proteus-public-head'
$head = (gh api "repos/$Repo/commits/main" -q .sha 2>$null)
if (-not $head) { 'no public repository yet: nothing to sync'; return }
if (-not (Test-Path $marker)) { Set-Content $marker $head; "remembered public head $($head.Substring(0, 7))"; return }
$last = (Get-Content $marker -Raw).Trim()
if ($last -eq $head) { 'public main has nothing new'; return }
$work = Join-Path $env:TEMP "proteus-sync-$PID"
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
git clone --quiet "https://github.com/$Repo.git" $work
$subjects = git -C $work log --format='- %s (%an)' "$last..$head"
$patch = git -C $work diff --binary "$last" "$head" | Out-String
if (-not $patch.Trim()) { Set-Content $marker $head; 'public commits changed nothing in the tree'; Remove-Item $work -Recurse -Force; return }
# Public files that are private files' public versions.
$pub = Join-Path $root 'forge\export\public'
foreach ($f in Get-ChildItem $pub -Recurse -File -Force) {
    $rel = $f.FullName.Substring($pub.Length + 1).Replace('\', '/')
    $patch = $patch.Replace(" a/$rel", " a/forge/export/public/$rel").Replace(" b/$rel", " b/forge/export/public/$rel")
}
$pf = Join-Path $work 'public.patch'
[IO.File]::WriteAllText($pf, ($patch -replace "`r`n", "`n"), [Text.UTF8Encoding]::new($false))
git apply --3way --index --whitespace=nowarn $pf
if ($LASTEXITCODE -ne 0) { throw "the public changes did not apply cleanly; resolve by hand from $pf, then set $marker to $head" }
git commit --quiet -m "Sync from public main ($($last.Substring(0, 7))..$($head.Substring(0, 7)))" -m ($subjects -join "`n") -m 'Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>'
Set-Content $marker $head
"synced {0} public commit(s) into {1}" -f @($subjects).Count, (git log --oneline -1)
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
