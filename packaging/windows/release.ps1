#requires -version 5
<#
.SYNOPSIS
    Cut a runic-tray Windows release: build, package, checksum, and (optionally)
    publish a GitHub Release.
.DESCRIPTION
    Tag scheme: tray releases use `tray-vX.Y.Z` (decoupled from the core/lib/CLI
    which uses a bare `vX.Y.Z`). The version comes from runic-tray/Cargo.toml.

    Builds both distribution channels — the portable ZIP (package.ps1) and the
    MSI installer (msi.ps1) — writes a SHA256 sidecar for each, then:
      - default (DRY RUN): prints exactly what it WOULD publish and stops.
      - with -Publish:      runs `gh release create tray-v<ver> ...`.

    Publishing is gated behind -Publish on purpose: creating a public GitHub
    Release is irreversible, so it never happens by merely running the script.
    The MSI needs WiX v3 (see msi.ps1 / docs/dev/windows-setup.md).
.EXAMPLE
    .\packaging\windows\release.ps1            # dry run — build + checksum only
    .\packaging\windows\release.ps1 -Publish   # actually create the release
#>
param([switch]$Publish)
$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$tray = Join-Path $repo 'runic-tray'
# Version via cargo metadata — robust to the workspace-inherited version.
$meta = cargo metadata --no-deps --format-version 1 --manifest-path (Join-Path $tray 'Cargo.toml') | ConvertFrom-Json
$ver = ($meta.packages | Where-Object { $_.name -eq 'runic-tray' }).version
$tag = "tray-v$ver"

# Build both channels: portable ZIP + MSI installer.
& (Join-Path $PSScriptRoot 'package.ps1')
& (Join-Path $PSScriptRoot 'msi.ps1')

# Each artifact gets a `<hash>  <filename>` SHA256 sidecar (sha256sum convention).
function New-Sha256([string]$path) {
    if (-not (Test-Path $path)) { throw "expected artifact not found: $path" }
    $hash = (Get-FileHash $path -Algorithm SHA256).Hash.ToLower()
    $sidecar = "$path.sha256"
    "$hash  $(Split-Path -Leaf $path)" | Set-Content -Path $sidecar -Encoding ascii
    [pscustomobject]@{ Path = $path; Sha = $sidecar; Hash = $hash }
}

$zip = New-Sha256 (Join-Path $repo "dist\runic-tray-$ver-windows-x64.zip")
$msi = New-Sha256 (Join-Path $repo "dist\runic-tray-$ver-windows-x64.msi")
$assets = @($zip.Path, $zip.Sha, $msi.Path, $msi.Sha)

$notes = @"
runic-tray $ver — Windows SOCKS5 proxy tray.

Two channels:
- **Portable ZIP** — ``runic-tray-$ver-windows-x64.zip`` (unzip & run).
- **MSI installer** — ``runic-tray-$ver-windows-x64.msi`` (Program Files + Start-menu shortcut).

SHA256:
- ZIP: ``$($zip.Hash)``
- MSI: ``$($msi.Hash)``
"@

if ($Publish) {
    Write-Host "Publishing GitHub release $tag ..."
    gh release create $tag @assets --target main --title "runic-tray $ver" --notes $notes
    if ($LASTEXITCODE -ne 0) { throw "gh release create failed" }
    Write-Host "published: $tag"
} else {
    Write-Host ""
    Write-Host "DRY RUN — would publish GitHub release:" -ForegroundColor Yellow
    Write-Host "  tag    : $tag (target main)"
    Write-Host "  assets :"
    $assets | ForEach-Object { Write-Host "           $_" }
    Write-Host "  sha256 : ZIP $($zip.Hash)"
    Write-Host "           MSI $($msi.Hash)"
    Write-Host ""
    Write-Host "Re-run with -Publish to create the release." -ForegroundColor Yellow
}
