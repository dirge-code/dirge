#!/usr/bin/env pwsh
#Requires -Version 5.1
[CmdletBinding()]
param(
    # Build profile. Release is the default; pass -Configuration Debug for a
    # debug build. ('Debug' as a switch collides with the built-in -Debug.)
    [ValidateSet('Release', 'Debug')]
    [string]$Configuration = 'Release',

    # Cargo feature set to build. Defaults to `windows-default`, which is the
    # only supported set on Windows MSVC. The `plugin` feature (Janet runtime)
    # is intentionally excluded: `evil-janet`'s build script forces bindgen to
    # target `x86_64-pc-windows-gnu`, so clang looks for a MinGW sysroot and
    # cannot find `stdlib.h` under an MSVC toolchain. See Cargo.toml (the
    # `windows-default` comment) for details. Override via the FEATURES env var.
    [string]$Features = $(if ($env:FEATURES) { $env:FEATURES } else { 'windows-default' })
)

$ErrorActionPreference = 'Stop'

# --- Set up the MSVC developer environment (best-effort) -------------------
# Plain `cargo build` auto-detects the MSVC linker, so this is not strictly
# required, but importing vcvars makes the script work from any shell and
# keeps INCLUDE/LIB/PATH consistent for the C dependencies (rusqlite, aws-lc,
# tree-sitter, etc.).
function Import-VsDevEnv {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) {
        Write-Warning 'vswhere.exe not found; skipping MSVC env setup (relying on cargo auto-detect).'
        return
    }
    $vsPath = & $vswhere -latest -products * -property installationPath 2>$null
    if (-not $vsPath) {
        Write-Warning 'No Visual Studio installation found; skipping MSVC env setup.'
        return
    }
    $vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) {
        Write-Warning "vcvars64.bat not found at '$vcvars'; skipping MSVC env setup."
        return
    }
    Write-Host "==> Importing MSVC environment from: $vsPath"
    # Run vcvars in a child cmd, then dump the resulting environment and apply
    # each variable to the current PowerShell session.
    cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            Set-Item -Path "env:$($matches[1])" -Value $matches[2]
        }
    }
}

Import-VsDevEnv

# --- Build ------------------------------------------------------------------
$isRelease = $Configuration -eq 'Release'
$profileArg = if ($isRelease) { @('--release') } else { @() }
$profileName = if ($isRelease) { 'release' } else { 'debug' }

Write-Host "==> Building dirge ($profileName) with features: $Features"
$cargoArgs = @('build') + $profileArg + @('--no-default-features', '--features', $Features)
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}

$binary = "target\$profileName\dirge.exe"
Write-Host "==> Binary: $binary"
Get-Item $binary | Select-Object Name, @{N = 'Size'; E = { '{0:N1} MB' -f ($_.Length / 1MB) } }, LastWriteTime
