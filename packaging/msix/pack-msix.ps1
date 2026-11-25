<#
.SYNOPSIS
    Builds the Microsoft Store compliant binary (--no-default-features --features store),
    renders the required MSIX Store PNG assets from assets/icon.svg, packages a signed-ready
    .msix via Windows SDK makeappx.exe, and immediately wipes all build caches.
.EXAMPLE
    .\packaging\msix\pack-msix.ps1 -IdentityName "12345Vinny.TaskbarAudio" -Publisher "CN=XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX"
#>
param(
    [string]$IdentityName = "VinnySantos.TaskbarAudio",
    [string]$Publisher = "CN=73B1EB55-0637-4699-9B59-496D26478B09",
    [string]$PublisherDisplayName = "Vinny Santos",
    [ValidateSet("x64", "arm64")]
    [string]$Arch = "x64",
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $repoRoot
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

if (-not $Version) {
    $cargoToml = Get-Content (Join-Path $repoRoot "Cargo.toml") -Raw
    if ($cargoToml -match '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"') {
        $Version = "$($Matches[1]).0"
    } else {
        $Version = "0.1.0.0"
    }
}

$targetTriple = if ($Arch -eq "arm64") { "aarch64-pc-windows-msvc" } else { "x86_64-pc-windows-msvc" }
$stageDir = Join-Path $repoRoot "packaging\msix\stage-$Arch"
$outMsix = Join-Path $repoRoot "taskbar-audio-$Arch.msix"

function Find-MakeAppx {
    $cmd = Get-Command makeappx.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $sdkBin = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
    if (Test-Path $sdkBin) {
        $found = Get-ChildItem $sdkBin -Recurse -Filter makeappx.exe -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\makeappx\.exe$' } |
            Sort-Object FullName -Descending |
            Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    throw "makeappx.exe not found. Please install the Windows 10/11 SDK."
}

function Write-StorePngs([string]$assetsOut) {
    Add-Type -AssemblyName System.Drawing
    New-Item -ItemType Directory -Force -Path $assetsOut | Out-Null

    $specs = @(
        @{ Name = "StoreLogo.png"; Size = 50 },
        @{ Name = "Square44x44Logo.png"; Size = 44 },
        @{ Name = "Square150x150Logo.png"; Size = 150 }
    )

    foreach ($spec in $specs) {
        $sz = $spec.Size
        $bmp = New-Object System.Drawing.Bitmap($sz, $sz, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $g.Clear([System.Drawing.Color]::Transparent)

        $pad = [float]($sz * 0.06)
        $w = [float]($sz - $pad * 2)
        $r = [float]($sz * 0.20)
        $d = $r * 2

        $path = New-Object System.Drawing.Drawing2D.GraphicsPath
        $path.AddArc($pad, $pad, $d, $d, 180, 90)
        $path.AddArc($pad + $w - $d, $pad, $d, $d, 270, 90)
        $path.AddArc($pad + $w - $d, $pad + $w - $d, $d, $d, 0, 90)
        $path.AddArc($pad, $pad + $w - $d, $d, $d, 90, 90)
        $path.CloseFigure()

        $bg = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(255, 31, 32, 36))
        $g.FillPath($bg, $path)

        $white = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::White)
        $pts = [System.Drawing.PointF[]]@(
            (New-Object System.Drawing.PointF(($sz * 0.24), ($sz * 0.40))),
            (New-Object System.Drawing.PointF(($sz * 0.35), ($sz * 0.40))),
            (New-Object System.Drawing.PointF(($sz * 0.49), ($sz * 0.27))),
            (New-Object System.Drawing.PointF(($sz * 0.49), ($sz * 0.73))),
            (New-Object System.Drawing.PointF(($sz * 0.35), ($sz * 0.60))),
            (New-Object System.Drawing.PointF(($sz * 0.24), ($sz * 0.60)))
        )
        $g.FillPolygon($white, $pts)

        $penW = [Math]::Max(1.5, [float]($sz * 0.062))
        $pen = New-Object System.Drawing.Pen([System.Drawing.Color]::White, $penW)
        $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
        $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
        $g.DrawArc($pen, [float]($sz * 0.43), [float]($sz * 0.35), [float]($sz * 0.21), [float]($sz * 0.30), -45, 90)
        $g.DrawArc($pen, [float]($sz * 0.38), [float]($sz * 0.24), [float]($sz * 0.36), [float]($sz * 0.52), -48, 96)

        $outPath = Join-Path $assetsOut $spec.Name
        $bmp.Save($outPath, [System.Drawing.Imaging.ImageFormat]::Png)

        $pen.Dispose()
        $white.Dispose()
        $bg.Dispose()
        $path.Dispose()
        $g.Dispose()
        $bmp.Dispose()
    }
}

try {
    $makeAppx = Find-MakeAppx
    Write-Host "[msix] Building Microsoft Store compliant binary ($targetTriple)..." -ForegroundColor Cyan
    & cargo build --release --target $targetTriple --no-default-features --features store
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed for $targetTriple" }

    if (Test-Path $stageDir) { Remove-Item -Recurse -Force $stageDir }
    New-Item -ItemType Directory -Force -Path $stageDir | Out-Null

    $builtExe = Join-Path $repoRoot "target\$targetTriple\release\taskbar-audio.exe"
    Copy-Item $builtExe (Join-Path $stageDir "taskbar-audio.exe") -Force
    Write-StorePngs (Join-Path $stageDir "assets")

    $manifest = Get-Content (Join-Path $repoRoot "packaging\msix\AppxManifest.xml") -Raw
    $manifest = $manifest `
        -replace '(<Identity[\s\S]*?Name=")[^"]+(")', "`${1}$IdentityName`${2}" `
        -replace '(<Identity[\s\S]*?Publisher=")[^"]+(")', "`${1}$Publisher`${2}" `
        -replace '(<Identity[\s\S]*?Version=")[^"]+(")', "`${1}$Version`${2}" `
        -replace '(<Identity[\s\S]*?ProcessorArchitecture=")[^"]+(")', "`${1}$Arch`${2}" `
        -replace '(<PublisherDisplayName>)[^<]+(</PublisherDisplayName>)', "`${1}$PublisherDisplayName`${2}"
    Set-Content -Path (Join-Path $stageDir "AppxManifest.xml") -Value $manifest -Encoding utf8

    if (Test-Path $outMsix) { Remove-Item -Force $outMsix }
    Write-Host "[msix] Packing $outMsix..." -ForegroundColor Cyan
    & $makeAppx pack /d $stageDir /p $outMsix /o
    if ($LASTEXITCODE -ne 0) { throw "makeappx pack failed" }

    Write-Host "[msix] Created $outMsix" -ForegroundColor Green
}
finally {
    if (Test-Path $stageDir) { Remove-Item -Recurse -Force $stageDir -ErrorAction SilentlyContinue }
    & powershell -ExecutionPolicy Bypass -File (Join-Path $repoRoot "build-clean.ps1") -CleanOnly
}
