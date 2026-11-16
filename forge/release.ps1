# Make a signed release from the current build.
#   pwsh forge\release.ps1 [-Key FILE] [-Publish] [-SkipGate] [-Notes TEXT]
# Writes dist\release\PROTEUS.exe, PROTEUS.manifest and PROTEUS.manifest.sig.
# The manifest states the build, size and SHA-256 of the exe and is signed
# with the release key, which stays on this machine. -Publish uploads the
# three files as a GitHub release of the repository this checkout pushes to.
param([string]$Repo = 'karimalsalah/proteus', [string]$Key = (Join-Path $env:USERPROFILE '.proteus-release\signing.key'), [switch]$Publish, [switch]$SkipGate, [string]$Notes = '')
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)
if (-not (Test-Path $Key)) { throw "no release key at $Key (create one with: cargo run --release --manifest-path forge\sign\Cargo.toml -- keygen `"$Key`")" }
if (git status --porcelain) { throw 'the working tree has uncommitted changes; a release is built from a commit' }
pwsh -NoProfile -File forge\licenses.ps1
if (git status --porcelain) { throw 'assets\THIRD-PARTY.txt was out of date; commit it and run again' }
if (-not $SkipGate) {
    pwsh -NoProfile -File forge\gate.ps1
    if ($LASTEXITCODE -ne 0) { throw 'not released: the gate failed' }
}
$exe = 'target\release\proteus.exe'
if (-not (Test-Path $exe)) { throw 'no build to release' }
$out = 'dist\release'; Remove-Item $out -Recurse -Force -ErrorAction SilentlyContinue; New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item $exe "$out\PROTEUS.exe"
$build = ((& "$out\PROTEUS.exe" --version | Out-String).Trim() -replace '^proteus ', '')
if ($build -notmatch '^\d+\.\d+\.\d+-\d+$') { throw "unexpected build id: $build" }
$sha = (Get-FileHash "$out\PROTEUS.exe" -Algorithm SHA256).Hash.ToLower()
$manifest = "proteus-release 1`nbuild $build`nsize $((Get-Item "$out\PROTEUS.exe").Length)`nsha256 $sha`n"
[IO.File]::WriteAllText("$PWD\$out\PROTEUS.manifest", $manifest, [Text.UTF8Encoding]::new($false))
$sign = { cargo run --quiet --release --manifest-path forge\sign\Cargo.toml -- @args }
& $sign sign $Key "$out\PROTEUS.manifest" | Out-Null
$public = (& $sign public $Key | Out-String).Trim()
$embedded = (Select-String -Path src\update.rs -Pattern 'pub const PUBLIC_KEY: &str = "([0-9a-f]{64})"').Matches[0].Groups[1].Value
if ($public -ne $embedded) { throw "this key is not the one the exe trusts (key $public, exe $embedded)" }
if ((& $sign verify $public "$out\PROTEUS.manifest" | Out-String).Trim() -ne 'ok') { throw 'the signature just made does not verify' }
# The bundle Claude Desktop installs with a double click, and the MCP Registry lists.
$ver = $build -replace '-.*$', ''
$b = "$out\mcpb"; New-Item -ItemType Directory -Force "$b\server" | Out-Null
Copy-Item "$out\PROTEUS.exe" "$b\server\proteus.exe"; Copy-Item packaging\mcpb\icon.png "$b\icon.png"
[IO.File]::WriteAllText("$PWD\$b\manifest.json", ((Get-Content packaging\mcpb\manifest.json.in -Raw) -replace '@VERSION@', $ver), [Text.UTF8Encoding]::new($false))
Compress-Archive -Path "$b\manifest.json", "$b\icon.png", "$b\server" -DestinationPath "$out\proteus-mcp.zip" -Force
Move-Item "$out\proteus-mcp.zip" "$out\proteus-mcp.mcpb" -Force; Remove-Item $b -Recurse -Force
# Scoop installs from the manifest in the repository; it names this release.
$scoop = (Get-Content packaging\scoop\proteus.json.in -Raw) -replace '@VERSION@', $ver -replace '@SHA256@', $sha
[IO.File]::WriteAllText("$PWD\packaging\scoop\proteus.json", $scoop, [Text.UTF8Encoding]::new($false))
"release $build`n  sha256 $sha`n  signed with $public`n  files in $out (commit packaging\scoop\proteus.json after publishing)"
if ($Publish) {
    $tag = 'v' + ($build -replace '-.*$', '')
    if (-not $Notes) { $Notes = "PROTEUS $build. Signed: verify with ``proteus update --check``. What is measured and what is not done: forge/ledger.json." }
    gh release create $tag -R $Repo --target main --latest "$out\PROTEUS.exe" "$out\PROTEUS.manifest" "$out\PROTEUS.manifest.sig" "$out\proteus-mcp.mcpb" --title "PROTEUS $($build -replace '-.*$', '')" --notes $Notes
    if ($LASTEXITCODE -ne 0) { throw 'upload failed' }
    "published $tag"
}
