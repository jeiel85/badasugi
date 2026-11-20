# Shared by smoke.ps1 and release.ps1: where cargo builds, and the GPU (Vulkan) build environment.
#   . scripts/build-env.ps1; $targetDir = Use-BuildEnv -Gpu

# Visual Studio developer environment (x64) into this PowerShell session: cl.exe, link.exe, ninja, INCLUDE/LIB
function Import-VsDevEnv {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { throw "Visual Studio (C++ build tools) not found" }
    $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    $vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvars64.bat"
    if (-not (Test-Path $vcvars)) { throw "vcvars64.bat not found under $vs" }
    cmd /c "`"$vcvars`" >nul && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') { Set-Item "Env:$($Matches[1])" $Matches[2] }
    }
}

# Returns the cargo target directory (CARGO_TARGET_DIR, or <repo>\target).
# -Gpu: checks the Vulkan SDK (VULKAN_SDK, glslc) and that the target directory is short enough.
function Use-BuildEnv([switch]$Gpu) {
    $root = Split-Path -Parent $PSScriptRoot
    $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root "target" }
    $target = [IO.Path]::GetFullPath($target)
    if ($Gpu) {
        $sdk = $env:VULKAN_SDK
        if (-not $sdk) { $sdk = [Environment]::GetEnvironmentVariable("VULKAN_SDK", "Machine") }
        if (-not $sdk -or -not (Test-Path (Join-Path $sdk "Bin\glslc.exe"))) {
            throw "GPU build needs the Vulkan SDK (https://vulkan.lunarg.com, sets VULKAN_SDK) - install it, or run with -Cpu"
        }
        $env:VULKAN_SDK = $sdk
        $env:PATH = "$sdk\Bin;$env:PATH"
        # ggml's Vulkan shader generator is a nested CMake project ~190 characters below the target directory, and
        # MSBuild's tracking files (.tlog) there go past the 260-character limit whatever the folder. Build whisper.cpp
        # with Ninja instead (no tracking files) - it needs the Visual Studio developer environment (cl.exe, ninja).
        if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue) -or -not (Get-Command ninja.exe -ErrorAction SilentlyContinue)) {
            Import-VsDevEnv
        }
        $env:CMAKE_GENERATOR = "Ninja"
        # Even with Ninja the deepest object path is ~240 characters below a 15-character folder (D:\badasugi-gpu,
        # tested). Longer folders were not tested - keep it short, or build the CPU-only app with -Cpu.
        if ($target.Length -gt 16) {
            throw "GPU build: cargo target dir '$target' is too long ($($target.Length) chars) for the Vulkan shader build. Set CARGO_TARGET_DIR to a short path (e.g. D:\badasugi-gpu), or use -Cpu"
        }
    }
    return $target
}
