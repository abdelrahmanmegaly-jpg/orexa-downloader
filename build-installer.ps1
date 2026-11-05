# Builds the app and the installer wizard.
#   powershell -ExecutionPolicy Bypass -File .\build-installer.ps1
# Result: dist\Orexa-Downloader-Setup.exe
#
# To release an update: raise "version" in extension\manifest.json (the one place the version
# lives; the app and the installer always follow the extension), run this script, and send
# the new Orexa-Downloader-Setup.exe. Running it on a PC that already has Orexa
# Downloader updates it in place (old files replaced; downloads and settings kept).
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

# The app and installer take the extension's version.
$manifest = Get-Content -Raw -Encoding UTF8 (Join-Path $root 'extension\manifest.json') | ConvertFrom-Json
$version = $manifest.version
foreach ($toml in @('app\Cargo.toml', 'setup\Cargo.toml')) {
    $path = Join-Path $root $toml
    $text = [System.IO.File]::ReadAllText($path)
    $updated = [regex]::Replace($text, '(?m)^version = "[^"]*"', "version = `"$version`"", 1)
    if ($updated -ne $text) { [System.IO.File]::WriteAllText($path, $updated) }
}
Write-Host "Version $version (from the extension)"

Push-Location (Join-Path $root 'app')
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'App build failed' }
Pop-Location

# The setup program embeds the app, so it is built second.
Push-Location (Join-Path $root 'setup')
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'Setup build failed' }
Pop-Location

$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
Copy-Item -Force (Join-Path $root 'setup\target\release\orexa-setup.exe') (Join-Path $dist 'Orexa-Downloader-Setup.exe')
Get-Item (Join-Path $dist 'Orexa-Downloader-Setup.exe') | Select-Object FullName, @{n='SizeMB';e={[math]::Round($_.Length / 1MB, 1)}}
