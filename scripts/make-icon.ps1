# Generates assets/icon.ico and assets/icon-64.png for taskbar-audio.
Add-Type -AssemblyName System.Drawing

$assetsDir = Join-Path $PSScriptRoot "..\assets"
New-Item -ItemType Directory -Force -Path $assetsDir | Out-Null

function New-AudioIconBitmap([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.Clear([System.Drawing.Color]::Transparent)

    $pad = [float]($size * 0.06)
    $w = [float]($size - $pad * 2)
    $r = [float]($size * 0.22)

    # Rounded square dark-blue/teal background
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $path.AddArc($pad, $pad, $d, $d, 180, 90)
    $path.AddArc($pad + $w - $d, $pad, $d, $d, 270, 90)
    $path.AddArc($pad + $w - $d, $pad + $w - $d, $d, $d, 0, 90)
    $path.AddArc($pad, $pad + $w - $d, $d, $d, 90, 90)
    $path.CloseFigure()

    $rect = New-Object System.Drawing.RectangleF($pad, $pad, $w, $w)
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        $rect,
        [System.Drawing.Color]::FromArgb(255, 18, 118, 212),
        [System.Drawing.Color]::FromArgb(255, 11, 74, 153),
        45.0
    )
    $g.FillPath($brush, $path)

    # Draw speaker + sound waves
    $white = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::White)
    $speakerPts = [System.Drawing.PointF[]]@(
        (New-Object System.Drawing.PointF(($size * 0.24), ($size * 0.40))),
        (New-Object System.Drawing.PointF(($size * 0.35), ($size * 0.40))),
        (New-Object System.Drawing.PointF(($size * 0.49), ($size * 0.27))),
        (New-Object System.Drawing.PointF(($size * 0.49), ($size * 0.73))),
        (New-Object System.Drawing.PointF(($size * 0.35), ($size * 0.60))),
        (New-Object System.Drawing.PointF(($size * 0.24), ($size * 0.60)))
    )
    $g.FillPolygon($white, $speakerPts)

    $penW = [Math]::Max(1.5, [float]($size * 0.065))
    $pen = New-Object System.Drawing.Pen([System.Drawing.Color]::White, $penW)
    $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round

    $g.DrawArc($pen, [float]($size * 0.43), [float]($size * 0.34), [float]($size * 0.20), [float]($size * 0.32), -45, 90)
    $g.DrawArc($pen, [float]($size * 0.38), [float]($size * 0.23), [float]($size * 0.36), [float]($size * 0.54), -48, 96)

    $pen.Dispose()
    $white.Dispose()
    $brush.Dispose()
    $path.Dispose()
    $g.Dispose()
    return $bmp
}

# Save 64x64 PNG for toast notifications
$bmp64 = New-AudioIconBitmap 64
$png64Path = Join-Path $assetsDir "icon-64.png"
$bmp64.Save($png64Path, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp64.Dispose()

# Build multi-resolution .ico (16, 32, 48, 256)
$sizes = @(16, 32, 48, 256)
$pngBuffers = @()
foreach ($s in $sizes) {
    $b = New-AudioIconBitmap $s
    $ms = New-Object System.IO.MemoryStream
    $b.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $pngBuffers += ,($ms.ToArray())
    $ms.Dispose()
    $b.Dispose()
}

$icoPath = Join-Path $assetsDir "icon.ico"
$fs = [System.IO.File]::Open($icoPath, [System.IO.FileMode]::Create)
$bw = New-Object System.IO.BinaryWriter($fs)

# ICO header
$bw.Write([uint16]0)
$bw.Write([uint16]1)
$bw.Write([uint16]$sizes.Count)

$offset = 6 + (16 * $sizes.Count)
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]
    $data = $pngBuffers[$i]
    $dim = if ($s -eq 256) { [byte]0 } else { [byte]$s }
    $bw.Write($dim)
    $bw.Write($dim)
    $bw.Write([byte]0)
    $bw.Write([byte]0)
    $bw.Write([uint16]1)
    $bw.Write([uint16]32)
    $bw.Write([uint32]$data.Length)
    $bw.Write([uint32]$offset)
    $offset += $data.Length
}

for ($i = 0; $i -lt $sizes.Count; $i++) {
    $bw.Write($pngBuffers[$i])
}

$bw.Close()
$fs.Close()
Write-Host "Generated $icoPath and $png64Path"
