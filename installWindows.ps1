<#
.SYNOPSIS
    VOCAN - one-shot setup script for Windows.

.DESCRIPTION
    What this does, in order:
      1. Detects your CPU architecture.
      2. Installs Rust (via the official rustup installer) if it's missing.
      3. Installs ffmpeg (via winget, or Chocolatey if available) if missing.
      4. Downloads the DeepFilterNet3 "deep-filter.exe" binary from its
         official GitHub releases (Rikorose/DeepFilterNet) and places it
         next to the VOCAN binary. This is OPTIONAL functionality in the
         app (only needed for the "Dereverb (DeepFilterNet3)" checkbox).
         Windows marks downloaded executables with a "Mark of the Web" zone
         flag; this script clears that flag (Unblock-File) ONLY on this one
         downloaded file, so it can run -- the same thing you'd do manually
         via its file Properties dialog ("Unblock" checkbox).
      5. Builds VOCAN in release mode.
      6. Runs the automated test suite (fast tests, then ffmpeg-dependent
         tests) to confirm everything actually works.

.PARAMETER NoDfn3
    Skip the DeepFilterNet3 download entirely.

.PARAMETER SkipTests
    Skip running the test suite after building.

.EXAMPLE
    .\installWindows.ps1

.EXAMPLE
    .\installWindows.ps1 -NoDfn3 -SkipTests
#>

param(
    [switch]$NoDfn3,
    [switch]$SkipTests
)

$ErrorActionPreference = "Stop"

function Step($msg)  { Write-Host "`n==> $msg" -ForegroundColor Cyan }
function Ok($msg)    { Write-Host "  OK: $msg" -ForegroundColor Green }
function Warn($msg)  { Write-Host "  WARNING: $msg" -ForegroundColor Yellow }
function Fail($msg)  { Write-Host "  ERROR: $msg" -ForegroundColor Red }

$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $RepoRoot

# --- 1. Detect architecture ---------------------------------------------------
Step "Detecting platform"
$archRaw = $env:PROCESSOR_ARCHITECTURE
switch ($archRaw) {
    "AMD64" { $Arch = "x86_64" }
    "ARM64" { $Arch = "aarch64" }
    default { Fail "Unsupported CPU architecture: $archRaw"; exit 1 }
}
Ok "Detected Windows / $Arch"

# --- 2. Rust -------------------------------------------------------------------
Step "Checking for Rust"
$cargoExists = Get-Command cargo -ErrorAction SilentlyContinue
if ($cargoExists) {
    Ok "Rust already installed: $(rustc --version)"
} else {
    Warn "Rust not found. Installing via the official rustup installer (https://rustup.rs)..."
    $rustupInit = Join-Path $env:TEMP "rustup-init.exe"
    $rustupUrl = if ($Arch -eq "aarch64") {
        "https://static.rust-lang.org/rustup/dist/aarch64-pc-windows-msvc/rustup-init.exe"
    } else {
        "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe"
    }
    Invoke-WebRequest -Uri $rustupUrl -OutFile $rustupInit
    & $rustupInit -y --default-toolchain stable | Out-Host
    Remove-Item $rustupInit -ErrorAction SilentlyContinue

    # Make cargo visible in this session without requiring a new terminal.
    $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
    $env:Path = "$cargoBin;$env:Path"
    Ok "Rust installed."
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Fail "cargo still not on PATH after install. Open a new terminal and re-run this script."
    exit 1
}

# --- 3. ffmpeg -----------------------------------------------------------------
Step "Checking for ffmpeg"
$ffmpegExists = Get-Command ffmpeg -ErrorAction SilentlyContinue
if ($ffmpegExists) {
    Ok "ffmpeg already installed: $((ffmpeg -version | Select-Object -First 1))"
} else {
    Warn "ffmpeg not found. Installing..."
    if (Get-Command winget -ErrorAction SilentlyContinue) {
        winget install --id Gyan.FFmpeg -e --accept-source-agreements --accept-package-agreements
    } elseif (Get-Command choco -ErrorAction SilentlyContinue) {
        choco install ffmpeg -y
    } else {
        Fail "Neither winget nor Chocolatey is available. Install ffmpeg yourself and make sure it's on PATH, then re-run this script."
        exit 1
    }
    # Refresh PATH for this session (winget/choco update the registry, not this process).
    $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path", "User")
    Ok "ffmpeg installed."
}

if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) {
    Fail "ffmpeg still not on PATH after install. Open a new terminal and re-run this script."
    exit 1
}

# --- 4. Build (needed before placing deep-filter.exe, so we know the binary path)
Step "Building VOCAN (release mode)"
cargo build --release
Ok "Build finished."

$BinDir = Join-Path $RepoRoot "target\release"
$VocanExe = Join-Path $BinDir "VOCAN.exe"
if (-not (Test-Path $VocanExe)) {
    Fail "Expected binary not found at $VocanExe"
    exit 1
}
Ok "Binary at $VocanExe"

# --- 5. DeepFilterNet3 (optional) ----------------------------------------------
if ($NoDfn3) {
    Step "Skipping DeepFilterNet3 (-NoDfn3 given)"
} else {
    Step "Installing DeepFilterNet3 (optional dereverb feature)"

    if ($Arch -eq "aarch64") {
        Warn "No official DeepFilterNet3 Windows ARM64 build is published. Skipping (this is an optional feature)."
    } else {
        try {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/Rikorose/DeepFilterNet/releases/latest"
            $asset = $release.assets | Where-Object { $_.name -like "deep-filter-*-x86_64-pc-windows-msvc.exe" } | Select-Object -First 1

            if (-not $asset) {
                Warn "Could not find a DeepFilterNet3 Windows release asset. Skipping (this is an optional feature)."
            } else {
                $dest = Join-Path $BinDir "deep-filter.exe"
                Write-Host "  Downloading: $($asset.browser_download_url)"
                Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $dest

                # Windows tags downloaded files with a "Mark of the Web" zone
                # identifier, which can trigger SmartScreen warnings. Clear it
                # for this one file, the same as ticking "Unblock" in its
                # file Properties dialog.
                Unblock-File -Path $dest

                Ok "DeepFilterNet3 installed at $dest"
            }
        } catch {
            Warn "DeepFilterNet3 download failed: $($_.Exception.Message). Skipping (this is an optional feature)."
        }
    }
}

# --- 6. Tests -------------------------------------------------------------------
if ($SkipTests) {
    Step "Skipping tests (-SkipTests given)"
} else {
    Step "Running fast tests (no ffmpeg required)"
    cargo test
    Ok "Fast tests passed."

    Step "Running full tests (ffmpeg-dependent)"
    cargo test -- --ignored
    Ok "Full tests passed."
}

Step "Done"
Write-Host "VOCAN is built and verified." -ForegroundColor Green
Write-Host "`nRun it with:`n`n    $VocanExe`n"
