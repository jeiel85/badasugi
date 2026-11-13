# Badasugi smoke test: run the REAL app in an isolated "smoke mode" and check the full path
#   live recording (test WAV instead of the mic) -> pause -> resume -> stop -> saved transcript/MD/WAV,
#   then file transcription -> saved transcript, then the diagnostic log.
#
#   pwsh scripts/smoke.ps1              # unit tests + build + smoke
#   pwsh scripts/smoke.ps1 -SkipBuild   # use the existing target\release\badasugi.exe
#
# Safe to run while the installed app is in use: smoke mode uses a temp folder for settings, sessions,
# logs and WebView data, skips single-instance/tray/hotkey/updater, and only READS the installed models.
# No sound is played and the room microphone is not recorded.

param(
    [switch]$SkipBuild,
    [switch]$SkipTests
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$env:PATH = "$HOME\.cargo\bin;$env:PATH"

if (-not $SkipTests) {
    cargo test --release -p badasugi -p badasugi-core
    if ($LASTEXITCODE -ne 0) { throw "unit tests failed" }
}
if (-not $SkipBuild) {
    cargo build --release -p badasugi
    if ($LASTEXITCODE -ne 0) { throw "build failed" }
}

$fixture = Join-Path $root "tests\fixtures\ko_meeting.wav"
if (-not (Test-Path $fixture)) { throw "fixture missing: $fixture" }
$work = Join-Path ([IO.Path]::GetTempPath()) ("badasugi-smoke-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory $work | Out-Null

$vars = @{
    BADASUGI_SMOKE            = "1"
    BADASUGI_HOME             = $work
    BADASUGI_TEST_WAV         = $fixture
    BADASUGI_SMOKE_FILE       = $fixture
    # words spoken OUTSIDE the paused 4-6 s window of the fixture
    BADASUGI_SMOKE_EXPECT     = "안녕하세요|금요일까지|매장"
    WEBVIEW2_USER_DATA_FOLDER = (Join-Path $work "webview")
}
$passed = $false
try {
    foreach ($k in $vars.Keys) { Set-Item "Env:$k" $vars[$k] }
    Write-Host "== smoke: running the app in smoke mode ($work)" -ForegroundColor Cyan
    $p = Start-Process (Join-Path $root "target\release\badasugi.exe") -PassThru
    if (-not $p.WaitForExit(15 * 60 * 1000)) { $p.Kill(); throw "smoke timed out (15 min)" }
    $report = Join-Path $work "smoke-report.json"
    if (-not (Test-Path $report)) { throw "no smoke report (exit code $($p.ExitCode)); see $work\logs" }
    $r = Get-Content $report -Raw -Encoding UTF8 | ConvertFrom-Json
    foreach ($c in $r.checks) {
        $mark = if ($c.ok) { "OK  " } else { "FAIL" }
        $color = if ($c.ok) { "Green" } else { "Red" }
        Write-Host ("{0} {1} - {2}" -f $mark, $c.name, $c.detail) -ForegroundColor $color
    }
    if (-not $r.ok) { throw "smoke checks failed (log: $work\logs)" }
    Write-Host "== smoke: all $($r.checks.Count) checks passed (v$($r.version))" -ForegroundColor Green
    $passed = $true
} finally {
    foreach ($k in $vars.Keys) { Remove-Item "Env:$k" -ErrorAction SilentlyContinue }
    # keep the work folder when something failed, for inspection
    if ($passed) { Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue }
}
