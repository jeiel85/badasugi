# Badasugi release: build -> sign -> latest.json -> GitHub release
#
#   pwsh scripts/release.ps1 -Notes "release notes"         # build + upload
#   pwsh scripts/release.ps1 -Notes "..." -NoUpload          # build only (dist/)
#
# Requirements
#   - Updater signing key: $HOME\.tauri\badasugi.key (cargo tauri signer generate)
#     If you lose this key, already-installed apps cannot verify new updates. Back it up.
#   - gh CLI login (for upload)
#   - The version comes from [workspace.package] version in Cargo.toml.

param(
    [string]$Notes = "",
    [string]$NotesFile = "",
    [switch]$NoUpload,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$env:PATH = "$HOME\.cargo\bin;$env:PATH"

$repo = "jeiel85/badasugi"
$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$tag = "v$version"
Write-Host "== Badasugi $tag" -ForegroundColor Cyan

$keyPath = Join-Path $HOME ".tauri\badasugi.key"
if (-not (Test-Path $keyPath)) { throw "Signing key not found: $keyPath" }

# The signing key must never be visible to the build: every dependency's build.rs / proc-macro runs
# inside `cargo tauri build`. So build WITHOUT updater artifacts and with no key in the environment,
# then sign the installer in a separate step below.
Remove-Item Env:TAURI_SIGNING_PRIVATE_KEY, Env:TAURI_SIGNING_PRIVATE_KEY_PATH, Env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD -ErrorAction SilentlyContinue
if (-not $SkipBuild) {
    $noSign = Join-Path ([IO.Path]::GetTempPath()) "badasugi-nosign.json"
    [IO.File]::WriteAllText($noSign, '{"bundle":{"createUpdaterArtifacts":false}}')
    try {
        cargo tauri build --config $noSign
        if ($LASTEXITCODE -ne 0) { throw "build failed" }
    } finally {
        Remove-Item $noSign -ErrorAction SilentlyContinue
    }
}

$bundle = Join-Path $root "target\release\bundle\nsis"
$setup = Get-ChildItem $bundle -Filter "*_${version}_x64-setup.exe" | Select-Object -First 1
if (-not $setup) { throw "installer not found in $bundle" }
$sigFile = "$($setup.FullName).sig"
# Sign only the finished installer (minisign). The key is read from the file by the signer itself.
Remove-Item $sigFile -ErrorAction SilentlyContinue
try {
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
    cargo tauri signer sign --private-key-path $keyPath --app-version $version $setup.FullName
    if ($LASTEXITCODE -ne 0) { throw "signing failed" }
} finally {
    Remove-Item Env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD -ErrorAction SilentlyContinue
}
if (-not (Test-Path $sigFile)) { throw "signature not found: $sigFile" }

$dist = Join-Path $root "dist"
if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
New-Item -ItemType Directory $dist | Out-Null

$setupName = "Badasugi_${version}_x64-setup.exe"
$portableName = "Badasugi_${version}_x64-portable.zip"
Copy-Item $setup.FullName (Join-Path $dist $setupName)
Copy-Item $sigFile (Join-Path $dist "$setupName.sig")

# Portable: the exe alone (models are downloaded on first use; WebView2 ships with Windows 10/11)
$stage = Join-Path $dist "portable"
New-Item -ItemType Directory $stage | Out-Null
Copy-Item (Join-Path $root "target\release\badasugi.exe") $stage
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath (Join-Path $dist $portableName)
Remove-Item $stage -Recurse -Force

if ($NotesFile) { $Notes = Get-Content $NotesFile -Raw -Encoding UTF8 }
if (-not $Notes) { $Notes = "Badasugi $tag" }

# Updater manifest: the installed app reads releases/latest/download/latest.json
$signature = (Get-Content $sigFile -Raw).Trim()
$url = "https://github.com/$repo/releases/download/$tag/$setupName"
$manifest = [ordered]@{
    version   = $version
    notes     = $Notes.Trim()
    pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    platforms = [ordered]@{
        "windows-x86_64"      = [ordered]@{ signature = $signature; url = $url }
        "windows-x86_64-nsis" = [ordered]@{ signature = $signature; url = $url }
    }
}
$json = $manifest | ConvertTo-Json -Depth 5
[System.IO.File]::WriteAllText((Join-Path $dist "latest.json"), $json, (New-Object System.Text.UTF8Encoding($false)))

$sums = Get-ChildItem $dist -File | Where-Object { $_.Name -ne "SHA256SUMS.txt" } | ForEach-Object {
    "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
}
[System.IO.File]::WriteAllText((Join-Path $dist "SHA256SUMS.txt"), ($sums -join "`n") + "`n", (New-Object System.Text.UTF8Encoding($false)))

Get-ChildItem $dist | Format-Table Name, Length
if ($NoUpload) { Write-Host "dist/ ready (upload skipped)"; exit 0 }

$notesPath = Join-Path $dist "notes.md"
[System.IO.File]::WriteAllText($notesPath, $Notes, (New-Object System.Text.UTF8Encoding($false)))
$assets = Get-ChildItem $dist -File | Where-Object { $_.Name -ne "notes.md" } | ForEach-Object { $_.FullName }
gh release create $tag @assets --repo $repo --title "받아쓰기 $tag" --notes-file $notesPath --latest
if ($LASTEXITCODE -ne 0) { throw "gh release failed" }
Write-Host "released: https://github.com/$repo/releases/tag/$tag" -ForegroundColor Green
