# What a person sees when `proteus run` is typed at a prompt with nothing
# piped. Run inside a real pseudo-terminal by `proteus shot --cmd`, which
# renders the screen to a PNG (no window is opened):
#   $lab = "$env:TEMP\proteus-chron-lab"   (with PROTEUS_DATA and PROTEUS_SOCKET set to folders inside it)
#   proteus chronicle track --root $lab\unpiped      (a temp folder with any file in it)
#   proteus shot --cmd "pwsh -NoProfile -File <this file> <exe> $lab" --wait 40 --out unpiped.png --size 2400x1000
# Result on 2026-11-16: see known_issues in forge\lanes\chronicle.json.
param([string]$Exe, [string]$Lab)
$env:PROTEUS_DATA = "$Lab\data"
$env:PROTEUS_SOCKET = "$Lab\lab.sock"
$env:PROTEUS_SOUND = 'off'
$root = "$Lab\unpiped"
Set-Location $root
'--- 1. typed directly, nothing piped'
& $Exe run --root $root -- cmd /c 'echo child-says-hi& exit 7'
"code=[$LASTEXITCODE]   (asked for before the command had finished?)"
Start-Sleep -Seconds 3
'--- 2. through cmd /c'
cmd /c "$Exe run --root $root -- cmd /c exit 7"
"code=[$LASTEXITCODE]"
'--- 3. Start-Process -Wait'
$p = Start-Process -FilePath $Exe -ArgumentList 'run', '--root', $root, '--', 'cmd', '/c', 'exit 7' -Wait -NoNewWindow -PassThru
"code=[$($p.ExitCode)]"
'--- 4. piped to Out-Host'
& $Exe run --root $root -- cmd /c 'echo child-says-hi-again& exit 7' | Out-Host
"code=[$LASTEXITCODE]"
Start-Sleep -Seconds 1
'--- done'
