# Builds the app and the installer wizard.
#   powershell -ExecutionPolicy Bypass -File .\build-installer.ps1
# Result: dist\Orexa-Downloader-Setup.exe
#
# To release an update: raise "version" in app\Cargo.toml (the one place the app version
# lives; the installer follows it), run this script, and send the new
# Orexa-Downloader-Setup.exe. The browser extension has its own version (extension\manifest.json)
# and only changes when the extension itself changes. Running this on a PC that already has
# Orexa Downloader updates it in place (old files replaced; downloads and settings kept).
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

# The installer takes the app's version.
$appToml = [System.IO.File]::ReadAllText((Join-Path $root 'app\Cargo.toml'))
$m = [regex]::Match($appToml, '(?m)^version = "([^"]*)"')
if (-not $m.Success) { throw 'Could not read the app version from app\Cargo.toml' }
$version = $m.Groups[1].Value
$path = Join-Path $root 'setup\Cargo.toml'
$text = [System.IO.File]::ReadAllText($path)
$updated = [regex]::Replace($text, '(?m)^version = "[^"]*"', "version = `"$version`"", 1)
if ($updated -ne $text) { [System.IO.File]::WriteAllText($path, $updated) }
Write-Host "Version $version (from app\Cargo.toml)"

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
