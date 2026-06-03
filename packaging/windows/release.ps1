#requires -version 5
<#
.SYNOPSIS
    Cut a runic-tray Windows release: build, package, checksum, and (optionally)
    publish a GitHub Release.
.DESCRIPTION
    Tag scheme: tray releases use `tray-vX.Y.Z` (decoupled from the core/lib/CLI
    which uses a bare `vX.Y.Z`). The version comes from runic-tray/Cargo.toml.

    Builds the portable ZIP via package.ps1, writes a SHA256 sidecar, then:
      - default (DRY RUN): prints exactly what it WOULD publish and stops.
      - with -Publish:      runs `gh release create tray-v<ver> ...`.

    Publishing is gated behind -Publish on purpose: creating a public GitHub
    Release is irreversible, so it never happens by merely running the script.
.EXAMPLE
    .\packaging\windows\release.ps1            # dry run — build + checksum only
    .\packaging\windows\release.ps1 -Publish   # actually create the release
#>
param([switch]$Publish)
$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$tray = Join-Path $repo 'runic-tray'
$ver = (Select-String -Path (Join-Path $tray 'Cargo.toml') -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
$tag = "tray-v$ver"

# Build + package the portable ZIP.
& (Join-Path $PSScriptRoot 'package.ps1')

$zip = Join-Path $repo "dist\runic-tray-$ver-windows-x64.zip"
if (-not (Test-Path $zip)) { throw "expected artifact not found: $zip" }

# SHA256 sidecar (`<hash>  <filename>` — the sha256sum convention).
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
$sha = "$zip.sha256"
"$hash  runic-tray-$ver-windows-x64.zip" | Set-Content -Path $sha -Encoding ascii

$notes = "runic-tray $ver — portable Windows build (SOCKS5 proxy tray). SHA256: $hash"

if ($Publish) {
    Write-Host "Publishing GitHub release $tag ..."
    gh release create $tag $zip $sha --title "runic-tray $ver" --notes $notes
    if ($LASTEXITCODE -ne 0) { throw "gh release create failed" }
    Write-Host "published: $tag"
} else {
    Write-Host ""
    Write-Host "DRY RUN — would publish GitHub release:" -ForegroundColor Yellow
    Write-Host "  tag    : $tag"
    Write-Host "  assets : $zip"
    Write-Host "           $sha"
    Write-Host "  sha256 : $hash"
    Write-Host ""
    Write-Host "Re-run with -Publish to create the release." -ForegroundColor Yellow
}
