<#
.SYNOPSIS
    Compiles taskbar-audio.exe, copies the standalone binary to the project root
    (and %LOCALAPPDATA%\Programs\taskbar-audio\ if installed there), and immediately
    wipes target/, temporary build folders, and Cargo registry cache so zero disk space is wasted.
#>
param(
    [switch]$SkipChecks,
    [switch]$CleanOnly,
    [switch]$Restart
)

$ErrorActionPreference = "Stop"
$projectRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
if ((Split-Path -Leaf $projectRoot) -eq "scripts") {
    $projectRoot = Split-Path -Parent $projectRoot
}
Set-Location $projectRoot

$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
$outExe = Join-Path $projectRoot "taskbar-audio.exe"
$installDir = Join-Path $env:LOCALAPPDATA "Programs\taskbar-audio"
$installedExe = Join-Path $installDir "taskbar-audio.exe"

function Clear-BuildCaches {
    Write-Host "[clean] Removing target/ and temporary build artifacts..." -ForegroundColor Cyan

    # If a previous instance was running from inside target/, stop it so target/ can be deleted cleanly
    $runningFromTarget = Get-Process taskbar-audio -ErrorAction SilentlyContinue | Where-Object {
        $_.Path -and $_.Path.StartsWith((Join-Path $projectRoot "target"), [System.StringComparison]::OrdinalIgnoreCase)
    }
    if ($runningFromTarget) {
        $runningFromTarget | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 200
    }

    if (Test-Path (Join-Path $projectRoot "target")) {
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        & cargo clean *>$null
        $ErrorActionPreference = $prevEap
        Remove-Item -Recurse -Force (Join-Path $projectRoot "target") -ErrorAction SilentlyContinue
    }

    # Remove any temp taskbar-audio build directories in %TEMP%
    Get-ChildItem $env:TEMP -Filter "*taskbar-audio*" -ErrorAction SilentlyContinue | ForEach-Object {
        Remove-Item -Recurse -Force $_.FullName -ErrorAction SilentlyContinue
    }
    Get-ChildItem $env:TEMP -Filter "rustc*" -ErrorAction SilentlyContinue | ForEach-Object {
        Remove-Item -Recurse -Force $_.FullName -ErrorAction SilentlyContinue
    }

    # Clean unpacked crate sources and downloaded .crate archives in ~/.cargo/registry
    $registrySrc = Join-Path $env:USERPROFILE ".cargo\registry\src"
    $registryCache = Join-Path $env:USERPROFILE ".cargo\registry\cache"
    if (Test-Path $registrySrc) {
        Remove-Item -Recurse -Force $registrySrc -ErrorAction SilentlyContinue
    }
    if (Test-Path $registryCache) {
        Remove-Item -Recurse -Force $registryCache -ErrorAction SilentlyContinue
    }
}

if ($CleanOnly) {
    # Rescue existing release binary before wiping target/ if taskbar-audio.exe isn't at root yet
    $targetExe = Join-Path $projectRoot "target\release\taskbar-audio.exe"
    if ((Test-Path $targetExe) -and -not (Test-Path $outExe)) {
        Copy-Item $targetExe $outExe -Force
    }
    Clear-BuildCaches
    Write-Host "[clean] All build caches removed." -ForegroundColor Green
    exit 0
}

try {
    if (-not $SkipChecks) {
        Write-Host "[build] Running clippy and unit tests..." -ForegroundColor Cyan
        & cargo clippy --all-targets -- -D warnings
        if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed" }

        & cargo test
        if ($LASTEXITCODE -ne 0) { throw "cargo test failed" }
    }

    Write-Host "[build] Compiling optimized release binary..." -ForegroundColor Cyan
    & cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed" }

    $builtExe = Join-Path $projectRoot "target\release\taskbar-audio.exe"
    if (-not (Test-Path $builtExe)) {
        throw "Expected binary not found at $builtExe"
    }

    $wasRunning = @(Get-Process taskbar-audio -ErrorAction SilentlyContinue).Count -gt 0
    if ($wasRunning) {
        Stop-Process -Name taskbar-audio -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 250
    }

    Copy-Item $builtExe $outExe -Force
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item $builtExe $installedExe -Force

    $sizeKb = [Math]::Round((Get-Item $outExe).Length / 1KB, 1)
    Write-Host "[build] Saved standalone binary ($sizeKb KB) to:" -ForegroundColor Green
    Write-Host "        - $outExe"
    Write-Host "        - $installedExe"

    if ($wasRunning -or $Restart) {
        Start-Process $installedExe
    }
}
finally {
    Clear-BuildCaches
}
