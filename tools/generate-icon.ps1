param(
    [string]$Output = (Join-Path $PSScriptRoot '..\assets\shiyue-icon.ico')
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

function New-IconPng([int]$size) {
    $bitmap = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality

    $scale = $size / 256.0
    $graphics.Clear([System.Drawing.Color]::FromArgb(246, 239, 232))
    $outer = New-Object System.Drawing.RectangleF (24 * $scale), (24 * $scale), (208 * $scale), (208 * $scale)
    $outerPath = New-Object System.Drawing.Drawing2D.GraphicsPath
    $radius = 44 * $scale
    $outerPath.AddArc($outer.X, $outer.Y, $radius * 2, $radius * 2, 180, 90)
    $outerPath.AddArc($outer.Right - $radius * 2, $outer.Y, $radius * 2, $radius * 2, 270, 90)
    $outerPath.AddArc($outer.Right - $radius * 2, $outer.Bottom - $radius * 2, $radius * 2, $radius * 2, 0, 90)
    $outerPath.AddArc($outer.X, $outer.Bottom - $radius * 2, $radius * 2, $radius * 2, 90, 90)
    $outerPath.CloseFigure()
    $graphics.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(24, 38, 51))), $outerPath)

    $paper = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 250, 244))
    $ink = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(24, 38, 51)), (7 * $scale)
    $muted = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(169, 179, 181)), (7 * $scale)
    $coral = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(237, 107, 93))

    $graphics.FillPie($paper, 57 * $scale, 63 * $scale, 86 * $scale, 126 * $scale, 180, 180)
    $graphics.FillPie($paper, 113 * $scale, 63 * $scale, 86 * $scale, 126 * $scale, 180, 180)
    $graphics.DrawLine($ink, 128 * $scale, 76 * $scale, 128 * $scale, 177 * $scale)
    foreach ($y in @(92, 111, 130)) {
        $graphics.DrawLine($muted, 74 * $scale, $y * $scale, (116 - (($y - 92) / 19) * 7) * $scale, $y * $scale)
        $graphics.DrawLine($muted, 182 * $scale, $y * $scale, 157 * $scale, $y * $scale)
    }
    $graphics.FillRectangle($coral, 137 * $scale, 76 * $scale, 25 * $scale, 62 * $scale)
    $graphics.FillEllipse($coral, 176 * $scale, 156 * $scale, 16 * $scale, 16 * $scale)

    $stream = New-Object System.IO.MemoryStream
    $bitmap.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    $bytes = $stream.ToArray()
    $stream.Dispose(); $graphics.Dispose(); $bitmap.Dispose(); $outerPath.Dispose(); $paper.Dispose(); $ink.Dispose(); $muted.Dispose(); $coral.Dispose()
    return ,$bytes
}

$sizes = @(256, 128, 64, 48, 32, 16)
$images = @($sizes | ForEach-Object { ,(New-IconPng $_) })
$directoryBytes = New-Object System.Collections.Generic.List[byte]
function Add-UShort([System.Collections.Generic.List[byte]]$list, [int]$value) { $list.AddRange([BitConverter]::GetBytes([uint16]$value)) }
function Add-UInt([System.Collections.Generic.List[byte]]$list, [UInt64]$value) { $list.AddRange([BitConverter]::GetBytes([uint32]$value)) }
Add-UShort $directoryBytes 0; Add-UShort $directoryBytes 1; Add-UShort $directoryBytes $sizes.Count
$offset = 6 + (16 * $sizes.Count)
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $size = $sizes[$i]; $directoryBytes.Add($(if ($size -eq 256) { 0 } else { $size })); $directoryBytes.Add($(if ($size -eq 256) { 0 } else { $size })); $directoryBytes.Add(0); $directoryBytes.Add(0)
    Add-UShort $directoryBytes 1; Add-UShort $directoryBytes 32; Add-UInt $directoryBytes $images[$i].Length; Add-UInt $directoryBytes $offset
    $offset += $images[$i].Length
}
$outputBytes = New-Object System.Collections.Generic.List[byte]
$outputBytes.AddRange($directoryBytes)
foreach ($image in $images) { $outputBytes.AddRange([byte[]]$image) }
$parent = Split-Path -Parent $Output
New-Item -ItemType Directory -Force -Path $parent | Out-Null
$resolvedOutput = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Output)
[IO.File]::WriteAllBytes($resolvedOutput, $outputBytes.ToArray())
Write-Output "Generated $Output"
