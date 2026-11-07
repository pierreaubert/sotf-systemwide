#Requires -Version 5.1
<#
.SYNOPSIS
    Build the SotF Systemwide daemon (sotf-daemon.exe) for Windows.

.DESCRIPTION
    Builds the `sotf-daemon` Rust binary from crates/daemon and packages it
    as a distributable .exe:

      sotf-daemon.exe -- WASAPI/cpal capture via CpalCaptureDriver
                        (SOTF_CAPTURE_DEVICE selects the endpoint, the
                        default input otherwise). The macOS-only `hal`
                        feature is never enabled on Windows.

    By default the exe is built with the `release` profile and copied,
    together with a README.txt, to:

      dist\sotf-systemwide-<version>-windows-<arch>\sotf-daemon.exe
      dist\sotf-systemwide-<version>-windows-<arch>.zip

    SOTF crates resolve through path dependencies in the sibling
    `..\sotf-daw` checkout (the all_of_sotf layout), so that checkout must
    sit next to this repo. The script refuses to run anywhere but native
    Windows.

.PARAMETER Configuration
    Cargo profile: Release (default), Dist (fat LTO, artifacts expected by
    release packaging), or Debug.

.PARAMETER Target
    Rust target triple: x86_64-pc-windows-msvc (default on x64 hosts) or
    aarch64-pc-windows-msvc (default on ARM64 hosts). The target is added
    via `rustup target add` when missing.

.PARAMETER StaticCrt
    Link the MSVC CRT statically (+crt-static) so the exe needs no Visual
    C++ Redistributable on the target machine. Uses target\static-crt as
    target dir so it does not cache-bust the dynamic-CRT build.

.PARAMETER Asio
    Enable the `asio` feature of sotf-daemon / sotf-engine.

.PARAMETER Clean
    Run `cargo clean -p sotf-daemon` before building.

.PARAMETER NoPackage
    Only build; leave the exe in target\<triple>\<profile>\ and skip the
    dist\ staging folder and zip.

.PARAMETER OutputDir
    Directory that receives the distribution folder and zip.
    Default: <repo>\dist.

.EXAMPLE
    .\scripts\build-windows.ps1
    Release sotf-daemon.exe plus dist zip.

.EXAMPLE
    .\scripts\build-windows.ps1 -Configuration Dist -StaticCrt
    Self-contained shipping exe (fat LTO, static CRT, no VCRedist needed).

.EXAMPLE
    .\scripts\build-windows.ps1 -NoPackage
    Just build target\x86_64-pc-windows-msvc\release\sotf-daemon.exe.
#>

[CmdletBinding()]
param(
    [ValidateSet("Release", "Dist", "Debug")]
    [string]$Configuration = "Release",

    [ValidateSet("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")]
    [string]$Target,

    [switch]$StaticCrt,
    [switch]$Asio,
    [switch]$Clean,
    [switch]$NoPackage,
    [string]$OutputDir
)

$ErrorActionPreference = "Stop"

# ---------------------------------------------------------------------------
# Paths / host detection
# ---------------------------------------------------------------------------

$ScriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = (Resolve-Path (Join-Path $ScriptDir "..")).Path

$HostArch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
if (-not $Target) {
    $Target = if ($HostArch -eq "arm64") { "aarch64-pc-windows-msvc" } else { "x86_64-pc-windows-msvc" }
}
$Arch = if ($Target -like "aarch64-*") { "arm64" } else { "x64" }

# Workspace (installer) version drives the distribution name, as on
# macOS/Linux (sotf_version in scripts/lib/common.sh reads the root
# Cargo.toml). The daemon crate keeps its own 0.1.x version; see the
# dual-versioning note at the top of CHANGELOG.md.
$RootToml = Get-Content (Join-Path $ProjectRoot "Cargo.toml") -Raw
$WorkspacePackage = [regex]::Match($RootToml, '(?s)\[workspace\.package\](.*?)(?=\n\[|\z)').Groups[1].Value
if ($WorkspacePackage -match '(?m)^version\s*=\s*"([^"]+)"') {
    $Version = $Matches[1]
} else {
    Write-Error "Could not extract workspace version from Cargo.toml [workspace.package]"
}

if (-not $OutputDir) { $OutputDir = Join-Path $ProjectRoot "dist" }

function Write-Info    { param($Message) Write-Host "[INFO] $Message"    -ForegroundColor Blue }
function Write-Success { param($Message) Write-Host "[SUCCESS] $Message" -ForegroundColor Green }
function Write-Warn    { param($Message) Write-Host "[WARNING] $Message" -ForegroundColor Yellow }

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

function Test-Prerequisites {
    if ($env:OS -ne "Windows_NT") {
        Write-Error "This script requires native Windows."
    }

    # Path dependencies (sotf-engine, sotf-plugins, driver-common) live here.
    $dawRoot = Join-Path (Split-Path $ProjectRoot -Parent) "sotf-daw"
    foreach ($crate in @("sotf-engine", "sotf-plugins", "driver-common")) {
        if (-not (Test-Path (Join-Path $dawRoot "crates\$crate\Cargo.toml"))) {
            Write-Error "Missing sibling checkout: $dawRoot\crates\$crate (all_of_sotf layout required)."
        }
    }

    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Error "Rust/Cargo is not installed. Install from https://rustup.rs"
    }

    # MSVC linker / Windows SDK, needed by the *-windows-msvc targets.
    $vsWhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vsWhere) {
        $vcTools = & $vsWhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if (-not $vcTools) {
            Write-Warn "Visual Studio C++ Build Tools not found; the MSVC link step will fail."
            Write-Warn "Install from https://visualstudio.microsoft.com/visual-cpp-build-tools/"
        }
    } else {
        Write-Warn "vswhere.exe not found; cannot verify Visual Studio C++ Build Tools."
    }

    $installed = & rustup target list --installed
    if ($LASTEXITCODE -eq 0 -and $installed -notcontains $Target) {
        Write-Info "Adding Rust target $Target ..."
        & rustup target add $Target
        if ($LASTEXITCODE -ne 0) { Write-Error "rustup target add $Target failed." }
    }

    Write-Success "Prerequisites check passed (host: $HostArch, target: $Target)"
}

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

function Build-Daemon {
    $profileDir = $Configuration.ToLowerInvariant()  # debug | release | dist

    $cargoArgs = @("build", "-p", "sotf-daemon", "--bin", "sotf-daemon", "--target", $Target)
    switch ($Configuration) {
        "Release" { $cargoArgs += "--release" }
        "Dist"    { $cargoArgs += @("--profile", "dist") }
        "Debug"   { }
    }
    if ($Asio) { $cargoArgs += @("--features", "asio") }

    # Static CRT goes to a separate target dir so RUSTFLAGS changes do not
    # cache-bust the dynamic-CRT build (same policy as ../sotf).
    $targetDir = Join-Path $ProjectRoot "target"
    if ($StaticCrt) {
        $targetDir = Join-Path $targetDir "static-crt"
        $cargoArgs += @("--target-dir", $targetDir)
    }

    Push-Location $ProjectRoot
    try {
        if ($Clean) {
            & cargo clean -p sotf-daemon --target $Target --target-dir $targetDir
            if ($LASTEXITCODE -ne 0) { Write-Error "cargo clean failed." }
        }

        if ($StaticCrt) {
            # Setting RUSTFLAGS replaces the rustflags in .cargo/config.toml,
            # so restate that block's CPU features next to +crt-static.
            $cpuFeatures = if ($Arch -eq "arm64") {
                "+neon"
            } else {
                "+sse,+sse2,+sse3,+ssse3,+sse4.1,+sse4.2,+avx,+avx2"
            }
            $env:RUSTFLAGS = "-C target-feature=+crt-static,$cpuFeatures"
            Write-Info "RUSTFLAGS: $env:RUSTFLAGS"
        }

        $linkage = if ($StaticCrt) { "static-CRT" } else { "dynamic-CRT" }
        Write-Info "Building sotf-daemon ($Configuration, $Target, $linkage)..."
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) { Write-Error "cargo build failed (exit $LASTEXITCODE)." }
    }
    finally {
        $env:RUSTFLAGS = $null
        Pop-Location
    }

    $exe = Join-Path $targetDir "$Target\$profileDir\sotf-daemon.exe"
    if (-not (Test-Path $exe)) { Write-Error "Binary not found at $exe" }
    return $exe
}

# ---------------------------------------------------------------------------
# Packaging
# ---------------------------------------------------------------------------

function New-Package {
    param([string]$ExePath)

    $staticSuffix = if ($StaticCrt) { "-static" } else { "" }
    $distName     = "sotf-systemwide-$Version-windows-$Arch$staticSuffix"
    $stagingDir   = Join-Path $OutputDir $distName

    New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
    if (Test-Path $stagingDir) { Remove-Item -Recurse -Force $stagingDir }
    New-Item -ItemType Directory -Force -Path $stagingDir | Out-Null

    Copy-Item $ExePath -Destination $stagingDir
    $pdb = [IO.Path]::ChangeExtension($ExePath, "pdb")
    if (Test-Path $pdb) { Copy-Item $pdb -Destination $stagingDir }

    $crtReq = if ($StaticCrt) {
        "  sotf-daemon.exe : self-contained (static CRT, no extra runtime needed)"
    } else {
        "  sotf-daemon.exe : needs Visual C++ Redistributable 2015-2022 (usually pre-installed on Windows 10/11)"
    }
    $readme = @"
SotF Systemwide v$Version (Windows $Arch)
=========================================

System-wide audio capture, plugin-chain processing, and hardware output.

Included
--------
- sotf-daemon.exe : background daemon (cpal/WASAPI capture, JSON IPC over
  loopback TCP with a per-user port file)

Running
-------
Run sotf-daemon.exe from a terminal. It keeps running until Ctrl-C.
System-audio capture needs a virtual-cable (loopback) device; select it
with the SOTF_CAPTURE_DEVICE environment variable. Without one, the
default input device is captured. Set SOTF_SYSTEMWIDE_DRIVER=null for the
status-only fallback driver.

Requirements
------------
- Windows 10/11 $Arch
$crtReq
"@
    $readme | Out-File -FilePath (Join-Path $stagingDir "README.txt") -Encoding UTF8

    $zipPath = Join-Path $OutputDir "$distName.zip"
    if (Test-Path $zipPath) { Remove-Item $zipPath }
    Compress-Archive -Path (Join-Path $stagingDir "*") -DestinationPath $zipPath -CompressionLevel Optimal
    Remove-Item -Recurse -Force $stagingDir

    Write-Success "Distribution created: $zipPath"
    return $zipPath
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

Write-Info "=========================================="
Write-Info "Building SotF Systemwide v$Version for Windows $Arch"
Write-Info "  daemon : sotf-daemon.exe ($Configuration, $(if ($StaticCrt) { 'static-CRT' } else { 'dynamic-CRT' }))"
Write-Info "=========================================="

Test-Prerequisites
$exe = Build-Daemon

# The daemon has no --help: running it starts the IPC accept loop, so the
# smoke check is existence + PE signature, never execution.
$bytes = [IO.File]::ReadAllBytes($exe)
if ($bytes.Length -lt 2 -or $bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A) {
    Write-Error "$exe is not a Windows PE executable (bad MZ signature)."
}
$sizeMb = (Get-Item $exe).Length / 1MB
$sha256 = [Security.Cryptography.SHA256]::Create()
try {
    $stream = [IO.File]::OpenRead($exe)
    try { $hash = [BitConverter]::ToString($sha256.ComputeHash($stream)) -replace "-", "" }
    finally { $stream.Dispose() }
} finally { $sha256.Dispose() }
Write-Success ("Built: {0} ({1:N2} MB)" -f $exe, $sizeMb)
Write-Info "SHA-256: $hash"

if (-not $NoPackage) {
    $zipPath = New-Package $exe
    Write-Info ("Package size: {0:N2} MB" -f ((Get-Item $zipPath).Length / 1MB))
}

Write-Info "=========================================="
Write-Success "Build complete!"
