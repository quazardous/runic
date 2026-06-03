#requires -version 5
<#
.SYNOPSIS
    Build runic-tray (release) and package it as a portable Windows ZIP.
.DESCRIPTION
    Produces dist/runic-tray-<version>-windows-x64.zip containing the exe (with
    the embedded Raidho icon), a config template and a README. No MSI tooling
    required — uses the built-in Compress-Archive. MSI packaging is tracked
    separately.

    On the GNU toolchain the executable icon is embedded via windres, which
    needs the mingw64 bin (gcc + windres) ahead on PATH; this script prepends it
    when present.
#>
$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$tray = Join-Path $repo 'runic-tray'

# Ensure the icon resource embeds on the GNU toolchain (no-op for MSVC / if absent).
$mingw = 'C:\msys64\mingw64\bin'
if (Test-Path $mingw) { $env:PATH = "$mingw;$env:PATH" }

# Version from the crate manifest ([package] version, anchored to line start).
$ver = (Select-String -Path (Join-Path $tray 'Cargo.toml') -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
Write-Host "runic-tray $ver — building release..."

cargo build --release --manifest-path (Join-Path $tray 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$stage = Join-Path $env:TEMP 'runic-tray-pkg'
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item (Join-Path $tray 'target\release\runic-tray.exe') $stage
Copy-Item (Join-Path $PSScriptRoot 'runic.yaml.example')     $stage
Copy-Item (Join-Path $PSScriptRoot 'README-dist.txt') (Join-Path $stage 'README.txt')

$dist = Join-Path $repo 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null
$zip = Join-Path $dist "runic-tray-$ver-windows-x64.zip"
Remove-Item $zip -Force -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip

Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "packaged: $zip ($([math]::Round((Get-Item $zip).Length/1KB)) KB)"
