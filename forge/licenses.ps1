# Collect the licence texts of every crate that ends up in the exe into
# assets\THIRD-PARTY.txt, which `proteus licenses` prints.
#   pwsh forge\licenses.ps1
# Identical texts are stored once and list every crate that uses them.
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)
$meta = cargo metadata --format-version 1 --filter-platform x86_64-pc-windows-msvc | ConvertFrom-Json
# Only what the product links: follow normal dependencies from the root, not dev or build ones.
$byId = @{}; foreach ($n in $meta.resolve.nodes) { $byId[$n.id] = $n }
$seen = [System.Collections.Generic.HashSet[string]]::new(); $todo = [System.Collections.Generic.Queue[string]]::new()
$todo.Enqueue($meta.resolve.root)
while ($todo.Count) {
    $id = $todo.Dequeue(); if (-not $seen.Add($id)) { continue }
    foreach ($d in $byId[$id].deps) { if ($d.dep_kinds | Where-Object { $null -eq $_.kind }) { $todo.Enqueue($d.pkg) } }
}
$pkgs = $meta.packages | Where-Object { $seen.Contains($_.id) -and $_.id -ne $meta.resolve.root } | Sort-Object name, version
$texts = [ordered]@{}; $missing = @()
foreach ($p in $pkgs) {
    $dir = Split-Path $p.manifest_path -Parent
    $files = Get-ChildItem $dir -File | Where-Object { $_.Name -match '^(LICEN[SC]E|COPYING|NOTICE|UNLICENSE)' } | Sort-Object Name
    if (-not $files) { $missing += $p; continue }
    foreach ($f in $files) {
        $t = ((Get-Content $f.FullName -Raw) -replace "`r`n", "`n").Trim()
        $h = [BitConverter]::ToString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes(($t -replace '\s+', ' ')))).Replace('-', '')
        if (-not $texts.Contains($h)) { $texts[$h] = @{ text = $t; users = [System.Collections.Generic.List[string]]::new() } }
        $texts[$h].users.Add("$($p.name) $($p.version) ($($f.Name))")
    }
}
$out = [System.Text.StringBuilder]::new()
[void]$out.AppendLine("== Rust crates built into this exe ($($pkgs.Count)) ==`n")
foreach ($p in $pkgs) { [void]$out.AppendLine(("{0} {1}  [{2}]  {3}" -f $p.name, $p.version, $p.license, $p.repository)) }
if ($missing) { [void]$out.AppendLine("`nNo licence file in the crate's own package (the expression above applies): " + (($missing | ForEach-Object { "$($_.name) $($_.version)" }) -join ', ')) }
$i = 0
foreach ($h in $texts.Keys) { $i++; [void]$out.AppendLine("`n-- licence text $i, used by: " + ($texts[$h].users -join '; ') + " --`n"); [void]$out.AppendLine($texts[$h].text) }
$text = $out.ToString() -replace "`r`n", "`n"
[IO.File]::WriteAllText("$PWD\assets\THIRD-PARTY.txt", $text, [Text.UTF8Encoding]::new($false))
"{0} crates, {1} distinct licence texts, {2} without a licence file, {3:N0} KB" -f $pkgs.Count, $texts.Count, $missing.Count, ($text.Length / 1kb)
