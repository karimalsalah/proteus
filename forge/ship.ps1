# Put the current build on the Desktop as one file: PROTEUS.exe.
#   pwsh forge\ship.ps1 [-SkipGate]
# Runs the gate first; nothing ships if it fails.
param([switch]$SkipGate)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)

if (-not $SkipGate) {
    pwsh -NoProfile -File forge\gate.ps1
    if ($LASTEXITCODE -ne 0) { Write-Host 'Not shipped: the gate failed.' -ForegroundColor Red; exit 1 }
}
$src = 'target\release\proteus.exe'
if (-not (Test-Path $src)) { throw 'no build to ship' }
$dest = Join-Path ([Environment]::GetFolderPath('Desktop')) 'PROTEUS.exe'

if (Test-Path $dest) {
    try { Remove-Item $dest -Force -ErrorAction Stop }
    catch {
        # A window started from the Desktop file is open, which locks it.
        # Windows still lets a running program be moved, so move it out of
        # sight; that window keeps working and the file is cleaned up later.
        $runtime = Join-Path $env:LOCALAPPDATA 'proteus\runtime'
        New-Item -ItemType Directory -Force $runtime | Out-Null
        Move-Item $dest (Join-Path $runtime "retired-$(Get-Date -Format yyyyMMdd-HHmmss).exe") -Force
    }
}
Copy-Item $src $dest -Force
$v = (& $dest --version | Out-String).Trim()
Write-Host "Shipped $dest  ($([math]::Round((Get-Item $dest).Length/1MB,1)) MB, $v)" -ForegroundColor Green
