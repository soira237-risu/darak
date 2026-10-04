# 앱 아이콘 원본 PNG(1024)에서 크기별 PNG를 담은 .ico와 창 아이콘용 256px PNG를 만든다.
# 사용: pwsh tools/make-icon.ps1 [-Source design/icon/darak-1024.png]
param(
  [string]$Source = "design/icon/darak-1024.png",
  [string]$OutDir = "crates/darak/assets"
)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
$src = [System.Drawing.Bitmap]::FromFile((Join-Path $root $Source))
$out = Join-Path $root $OutDir
New-Item -ItemType Directory -Force $out | Out-Null

# 한 번에 크게 줄이면 작은 크기에서 선이 깨지므로 절반씩 줄여 간다.
function Resize([System.Drawing.Bitmap]$img, [int]$size) {
  $cur = $img
  while ($cur.Width / 2 -ge $size) {
    $cur = Draw $cur ([int]($cur.Width / 2))
  }
  if ($cur.Width -ne $size) { $cur = Draw $cur $size }
  return $cur
}
function Draw([System.Drawing.Bitmap]$img, [int]$size) {
  $bmp = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
  $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
  $g.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
  $g.DrawImage($img, 0, 0, $size, $size)
  $g.Dispose()
  return $bmp
}

$sizes = 16, 24, 32, 48, 64, 128, 256
$pngs = foreach ($s in $sizes) {
  $ms = New-Object System.IO.MemoryStream
  (Resize $src $s).Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
  , $ms.ToArray()
}
[System.IO.File]::WriteAllBytes((Join-Path $out "icon-256.png"), $pngs[-1])

# ICO: 헤더 6바이트 + 항목 16바이트씩 + PNG 데이터 (Vista 이후 PNG 항목 지원)
$fs = [System.IO.File]::Create((Join-Path $out "icon.ico"))
$w = New-Object System.IO.BinaryWriter $fs
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
  $d = if ($sizes[$i] -ge 256) { 0 } else { $sizes[$i] }
  $w.Write([byte]$d); $w.Write([byte]$d); $w.Write([byte]0); $w.Write([byte]0)
  $w.Write([uint16]1); $w.Write([uint16]32)
  $w.Write([uint32]$pngs[$i].Length); $w.Write([uint32]$offset)
  $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $w.Write($p) }
$w.Close()
"icon.ico ($($sizes -join ', ')) + icon-256.png -> $OutDir"
