# Badasugi smoke test: run the REAL app in an isolated "smoke mode" and check the full path
#   live recording (test WAV instead of the mic) -> pause -> resume -> playback while recording -> stop ->
#   saved transcript/MD/Opus recording -> playback in the UI, then file transcription -> saved transcript,
#   then the diagnostic log.
#
#   pwsh scripts/smoke.ps1              # unit tests + build (GPU/Vulkan) + smoke, then smoke again with no GPU driver
#   pwsh scripts/smoke.ps1 -SkipBuild   # use the existing release\badasugi.exe
#   pwsh scripts/smoke.ps1 -Cpu         # CPU-only build (no Vulkan SDK needed), one smoke run
#
# GPU build: needs the Vulkan SDK (VULKAN_SDK) and a SHORT cargo target dir - ggml's Vulkan shader generator is a
# nested CMake project ~190 characters deep, and MSBuild fails past 260 (e.g. CARGO_TARGET_DIR=D:\badasugi-gpu).
# The second run hides the GPU driver from the Vulkan loader (VK_DRIVER_FILES -> a missing file): the app must
# still work and transcribe on the CPU.
#
# Safe to run while the installed app is in use: smoke mode uses a temp folder for settings, sessions,
# logs and WebView data, skips single-instance/tray/hotkey/updater, and only READS the installed models.
# No sound is played and the room microphone is not recorded.

param(
    [switch]$SkipBuild,
    [switch]$SkipTests,
    [switch]$Cpu
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$env:PATH = "$HOME\.cargo\bin;$env:PATH"
. (Join-Path $PSScriptRoot "build-env.ps1")
$targetDir = Use-BuildEnv -Gpu:(-not $Cpu)

if (-not $SkipTests) {
    if ($Cpu) { cargo test --release -p badasugi -p badasugi-core }
    else { cargo test --release -p badasugi -p badasugi-core --features badasugi-core/vulkan }
    if ($LASTEXITCODE -ne 0) { throw "unit tests failed" }
}
if (-not $SkipBuild) {
    if ($Cpu) { cargo build --release -p badasugi }
    else { cargo build --release -p badasugi --features vulkan }
    if ($LASTEXITCODE -ne 0) { throw "build failed" }
}

$fixture = Join-Path $root "tests\fixtures\ko_meeting.wav"
if (-not (Test-Path $fixture)) { throw "fixture missing: $fixture" }
$exe = Join-Path $targetDir "release\badasugi.exe"
if (-not (Test-Path $exe)) { throw "app not built: $exe" }

# Run the app once in smoke mode with extra environment variables; throws when a check fails
function Invoke-Smoke([string]$Label, [hashtable]$Extra) {
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
    foreach ($k in $Extra.Keys) { $vars[$k] = $Extra[$k] -replace '\{work\}', $work }
    $passed = $false
    try {
        foreach ($k in $vars.Keys) { Set-Item "Env:$k" $vars[$k] }
        Write-Host "== smoke ($Label): running the app in smoke mode ($work)" -ForegroundColor Cyan
        $p = Start-Process $exe -PassThru
        if (-not $p.WaitForExit(15 * 60 * 1000)) { $p.Kill(); throw "smoke timed out (15 min)" }
        $report = Join-Path $work "smoke-report.json"
        if (-not (Test-Path $report)) { throw "no smoke report (exit code $($p.ExitCode)); see $work\logs" }
        $r = Get-Content $report -Raw -Encoding UTF8 | ConvertFrom-Json
        foreach ($c in $r.checks) {
            $mark = if ($c.ok) { "OK  " } else { "FAIL" }
            $color = if ($c.ok) { "Green" } else { "Red" }
            Write-Host ("{0} {1} - {2}" -f $mark, $c.name, $c.detail) -ForegroundColor $color
        }
        if (-not $r.ok) { throw "smoke checks failed ($Label, log: $work\logs)" }
        Write-Host "== smoke ($Label): all $($r.checks.Count) checks passed (v$($r.version))" -ForegroundColor Green
        $passed = $true
    } finally {
        foreach ($k in $vars.Keys) { Remove-Item "Env:$k" -ErrorAction SilentlyContinue }
        # keep the work folder when something failed, for inspection
        if ($passed) { Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue }
    }
}

Invoke-Smoke "default" @{}
if (-not $Cpu) {
    # No GPU driver: point the Vulkan loader at a driver list that does not exist -> no Vulkan device -> CPU
    Invoke-Smoke "no GPU driver" @{
        VK_DRIVER_FILES               = "{work}\no-gpu-driver.json"
        VK_ICD_FILENAMES              = "{work}\no-gpu-driver.json"
        BADASUGI_SMOKE_EXPECT_DEVICE  = "cpu"
    }
}
