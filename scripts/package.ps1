# Automated packaging script for "THE FREAK" (v0.2.0)
# Produces a clean distributable layout:
# dist/
# ├── THE_FREAK.exe
# ├── data/
# │   ├── config.toml
# │   ├── window.json
# │   └── assets/
# │       └── kaine-cry.gif
# └── THE_FREAK-v0.2.0.zip

$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$projectRoot = Split-Path -Parent $scriptDir
Set-Location $projectRoot

Write-Host "==> Building release binary..." -ForegroundColor Cyan
cargo build --release --bin framepacer-the-freak
if ($LASTEXITCODE -ne 0) {
    throw "Cargo release build failed with exit code $LASTEXITCODE"
}

$distDir = Join-Path $projectRoot "dist"
$dataDir = Join-Path $distDir "data"
$assetsDir = Join-Path $dataDir "assets"

Write-Host "==> Assembling dist folder..." -ForegroundColor Cyan
if (Test-Path $distDir) {
    Remove-Item -Recurse -Force $distDir
}

New-Item -ItemType Directory -Path $assetsDir -Force | Out-Null

$srcExe = Join-Path $projectRoot "target\release\framepacer-the-freak.exe"
$destExe = Join-Path $distDir "THE_FREAK.exe"
Copy-Item $srcExe $destExe

$srcConfig = Join-Path $projectRoot "config.toml"
$destConfig = Join-Path $dataDir "config.toml"
Copy-Item $srcConfig $destConfig

$srcGif = Join-Path $projectRoot "assets\kaine-cry.gif"
$destGif = Join-Path $assetsDir "kaine-cry.gif"
Copy-Item $srcGif $destGif

# Create initial window.json if not present
$windowJsonPath = Join-Path $dataDir "window.json"
if (-not (Test-Path $windowJsonPath)) {
    '{"x": 100.0, "y": 100.0}' | Out-File -FilePath $windowJsonPath -Encoding utf8
}

# Validate that initial folder structure contains exactly the exe and data folder
$preZipItems = Get-ChildItem -Path $distDir
if ($preZipItems.Count -ne 2) {
    throw "Validation failed: expected exactly 2 items before archiving, found $($preZipItems.Count)"
}

Write-Host "`nDistributable successfully assembled in $($distDir):" -ForegroundColor Green
$preZipItems | ForEach-Object { Write-Host "  - $($_.Name)" }

# Create release zip archive containing THE_FREAK.exe and data/
$zipPath = Join-Path $distDir "THE_FREAK-v0.2.0.zip"
Write-Host "`n==> Creating release archive: $zipPath..." -ForegroundColor Cyan
Compress-Archive -Path $destExe, $dataDir -DestinationPath $zipPath -Force

Write-Host "Release package successfully created at: $zipPath" -ForegroundColor Green
