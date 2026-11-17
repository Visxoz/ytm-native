# Builds icons/icon.ico from icons/icon.svg, and icon-small.svg at 16, 20 and 24 px. Needs ImageMagick 7.
$ErrorActionPreference = 'Stop'
$icons = Join-Path (Split-Path $PSScriptRoot) 'icons'
$png = Join-Path ([IO.Path]::GetTempPath()) 'lily-icon.png'

$frames = foreach ($size in 16, 20, 24, 32, 40, 48, 64, 256) {
    $svg = if ($size -le 24) { 'icon-small.svg' } else { 'icon.svg' }
    magick -background none "$icons\$svg" -filter Box -resize "${size}x$size" -strip "png32:$png"
    if ($LASTEXITCODE) { throw "magick failed at $size px" }
    @{ Size = $size; Bytes = [IO.File]::ReadAllBytes($png) }
}
Remove-Item $png

# Written by hand because magick stores every ICO frame as an uncompressed bitmap.
$ico = [IO.MemoryStream]::new()
$w = [IO.BinaryWriter]::new($ico)
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$frames.Count)
$offset = 6 + 16 * $frames.Count
foreach ($f in $frames) {
    $side = [byte]($f.Size % 256)  # 256 is written as 0
    $w.Write($side); $w.Write($side); $w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$f.Bytes.Length); $w.Write([uint32]$offset)
    $offset += $f.Bytes.Length
}
foreach ($f in $frames) { $w.Write($f.Bytes) }
[IO.File]::WriteAllBytes("$icons\icon.ico", $ico.ToArray())
