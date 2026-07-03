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

# Version and target dir via cargo metadata — robust to the workspace layout:
# the version is inherited (`version.workspace = true`, no literal line to
# grep) and build output lands in the workspace-root `target/`, not
# `runic-tray/target/`.
$meta = cargo metadata --no-deps --format-version 1 --manifest-path (Join-Path $tray 'Cargo.toml') | ConvertFrom-Json
$ver = ($meta.packages | Where-Object { $_.name -eq 'runic-tray' }).version
$targetDir = $meta.target_directory
Write-Host "runic-tray $ver — building release..."

cargo build --release --manifest-path (Join-Path $tray 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$stage = Join-Path $env:TEMP 'runic-tray-pkg'
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item (Join-Path $targetDir 'release\runic-tray.exe') $stage
Copy-Item (Join-Path $PSScriptRoot 'runic.yaml.example')     $stage
Copy-Item (Join-Path $PSScriptRoot 'README-dist.txt') (Join-Path $stage 'README.txt')

$dist = Join-Path $repo 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null
$zip = Join-Path $dist "runic-tray-$ver-windows-x64.zip"
Remove-Item $zip -Force -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip

Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "packaged: $zip ($([math]::Round((Get-Item $zip).Length/1KB)) KB)"
