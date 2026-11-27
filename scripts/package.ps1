# Automated packaging script for "FramePacer Companion" (v0.3.1)
# Produces a clean distributable layout:
# dist/
# ├── FramePacerCompanion.exe
# ├── data/
# │   ├── config.toml
# │   └── window.json
# └── FramePacerCompanion-v0.3.1.zip

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

Write-Host "==> Assembling dist folder..." -ForegroundColor Cyan
if (Test-Path $distDir) {
    Remove-Item -Recurse -Force $distDir
}

New-Item -ItemType Directory -Path $dataDir -Force | Out-Null

$srcExe = Join-Path $projectRoot "target\release\framepacer-the-freak.exe"
$destExe = Join-Path $distDir "FramePacerCompanion.exe"
Copy-Item $srcExe $destExe

$srcConfig = Join-Path $projectRoot "config.toml"
$destConfig = Join-Path $dataDir "config.toml"
Copy-Item $srcConfig $destConfig

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

# Create release zip archive containing FramePacerCompanion.exe and data/
$zipPath = Join-Path $distDir "FramePacerCompanion-v0.3.1.zip"
Write-Host "`n==> Creating release archive: $zipPath..." -ForegroundColor Cyan
Compress-Archive -Path $destExe, $dataDir -DestinationPath $zipPath -Force

Write-Host "Release package successfully created at: $zipPath" -ForegroundColor Green

# When living under <appRoot>/data/source, sync updated exe up to <appRoot>
$parentDataDir = Split-Path -Parent $projectRoot
if ((Split-Path -Leaf $projectRoot) -eq "source" -and (Split-Path -Leaf $parentDataDir) -eq "data") {
    $appRoot = Split-Path -Parent $parentDataDir
    Write-Host "`n==> Syncing top-level app at $appRoot (preserving user data)..." -ForegroundColor Cyan
    Copy-Item $destExe (Join-Path $appRoot "FramePacerCompanion.exe") -Force

    $legacyExe = Join-Path $appRoot "THE_FREAK.exe"
    if (Test-Path $legacyExe) {
        Remove-Item $legacyExe -Force -ErrorAction SilentlyContinue
    }

    $parentConfig = Join-Path $parentDataDir "config.toml"
    if (-not (Test-Path $parentConfig)) {
        Copy-Item $srcConfig $parentConfig
    }
    Write-Host "Top-level FramePacerCompanion.exe synced (preserved config.toml, window.json, and logs)." -ForegroundColor Green
}
