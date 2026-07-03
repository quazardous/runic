#requires -version 5
<#
.SYNOPSIS
    Build runic-tray (release) and package it as a Windows MSI installer.
.DESCRIPTION
    Produces dist/runic-tray-<version>-windows-x64.msi via cargo-wix + WiX v3
    (candle/light). The MSI is the "installed" channel — Program Files install,
    a "runic" Start-menu shortcut, the embedded Raidho icon, and a frozen
    UpgradeCode for clean version-to-version upgrades. The portable ZIP
    (package.ps1) remains the "portable" channel.

    Requirements:
      - cargo-wix:  cargo install cargo-wix
      - WiX v3:     candle/light on PATH, or %WIX% pointing at the toolset root
                    (so %WIX%\bin\candle.exe exists). GitHub's windows-latest
                    runner ships WiX v3 with %WIX% already set; locally this
                    script falls back to .tools\wix314 if you extracted the WiX
                    binaries there (see docs/dev/windows-setup.md).

    On the GNU toolchain the exe icon embeds via windres (mingw64 bin on PATH);
    this script prepends it when present (no-op for MSVC / if absent).
#>
$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$tray = Join-Path $repo 'runic-tray'

# Icon embed on the GNU toolchain.
$mingw = 'C:\msys64\mingw64\bin'
if (Test-Path $mingw) { $env:PATH = "$mingw;$env:PATH" }

# Locate WiX: respect an existing %WIX% / candle on PATH (CI), else fall back to
# the locally-extracted v3 binaries.
$haveCandle = [bool](Get-Command candle.exe -ErrorAction SilentlyContinue) -or
              ($env:WIX -and (Test-Path (Join-Path $env:WIX 'bin\candle.exe')))
if (-not $haveCandle) {
    $local = Join-Path $repo '.tools\wix314'
    if (Test-Path (Join-Path $local 'bin\candle.exe')) {
        $env:WIX = "$local\"
        Write-Host "using local WiX: $local"
    } else {
        throw "WiX v3 not found (no candle.exe on PATH, no %WIX%, no .tools\wix314). See docs/dev/windows-setup.md."
    }
}

# Version and target dir via cargo metadata — robust to the workspace layout
# (inherited version, build output in the workspace-root `target/`).
$meta = cargo metadata --no-deps --format-version 1 --manifest-path (Join-Path $tray 'Cargo.toml') | ConvertFrom-Json
$ver = ($meta.packages | Where-Object { $_.name -eq 'runic-tray' }).version
$targetDir = $meta.target_directory
Write-Host "runic-tray $ver — building MSI..."

# cargo wix builds the release binary then runs candle/light.
Push-Location $tray
try {
    cargo wix 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "cargo wix failed" }
} finally {
    Pop-Location
}

$built = Join-Path $targetDir "wix\runic-tray-$ver-x86_64.msi"
if (-not (Test-Path $built)) { throw "expected MSI not found: $built" }

$dist = Join-Path $repo 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null
$msi = Join-Path $dist "runic-tray-$ver-windows-x64.msi"
Copy-Item $built $msi -Force
Write-Host "packaged: $msi ($([math]::Round((Get-Item $msi).Length/1KB)) KB)"
