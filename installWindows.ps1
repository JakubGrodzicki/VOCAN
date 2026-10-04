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
      5. Builds VOCAN in release mode. The build downloads a prebuilt
         ONNX Runtime from the network and links it into VOCAN.exe, so
         there is nothing extra to ship for it.
      6. Downloads the Silero VAD model (silero_vad.onnx, about 2.3 MB, MIT
         licensed, github.com/snakers4/silero-vad) into a "models" folder
         next to VOCAN.exe and checks its SHA-256. This is OPTIONAL
         functionality in the app (only needed for the "Detect speech with
         Silero VAD" checkbox). If the download or the check fails, the
         file is deleted, the checkbox stays disabled and the install
         carries on.
      7. Runs the automated test suite (fast tests, then ffmpeg-dependent
         tests) to confirm everything actually works.
      8. Copies just the files needed to run VOCAN (VOCAN.exe and, if
         installed, deep-filter.exe and the "models" folder) into a clean
         "VOCAN-App" folder, then deletes the "target" build folder (many
         hundreds of MB of intermediate build files you don't need to just
         run the app). Your source code and this script are never touched
         by this step.
      9. Prints a summary and keeps this window open, so you can read it.

.PARAMETER NoDfn3
    Skip the DeepFilterNet3 download entirely.

.PARAMETER NoVad
    Skip the Silero VAD model download entirely. The "Detect speech with
    Silero VAD" checkbox in VOCAN then stays disabled.

.PARAMETER SkipTests
    Skip running the test suite after building.

.PARAMETER KeepBuild
    Do not delete the "target" build folder at the end. Use this if you
    plan to keep developing/rebuilding VOCAN from this folder.

.PARAMETER NoPause
    Do not wait for a key press before closing the window at the end.
    Useful when running this script from an existing terminal or from
    another automated script.

.EXAMPLE
    .\installWindows.ps1

.EXAMPLE
    .\installWindows.ps1 -NoDfn3 -NoVad -SkipTests
#>

param(
    [switch]$NoDfn3,
    [switch]$NoVad,
    [switch]$SkipTests,
    [switch]$KeepBuild,
    [switch]$NoPause
)

$ErrorActionPreference = "Stop"

# Silero VAD model, pinned to a release tag and a SHA-256 so a changed or
# corrupted download is rejected.
$ModelUrl    = 'https://github.com/snakers4/silero-vad/raw/v6.2.2/src/silero_vad/data/silero_vad.onnx'
$ModelSha256 = '1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3'

function Step($msg)  { Write-Host "`n==> $msg" -ForegroundColor Cyan }
function Ok($msg)    { Write-Host "  OK: $msg" -ForegroundColor Green }
function Warn($msg)  { Write-Host "  WARNING: $msg" -ForegroundColor Yellow }
function Fail($msg)  { Write-Host "  ERROR: $msg" -ForegroundColor Red }

function FailExit($msg) {
    Fail $msg
    if (-not $NoPause) {
        Read-Host "`nPress Enter to close this window"
    }
    exit 1
}

$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $RepoRoot

# --- 1. Detect architecture ---------------------------------------------------
Step "Detecting platform"
$archRaw = $env:PROCESSOR_ARCHITECTURE
switch ($archRaw) {
    "AMD64" { $Arch = "x86_64" }
    "ARM64" { $Arch = "aarch64" }
    default { FailExit "Unsupported CPU architecture: $archRaw" }
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
    FailExit "cargo still not on PATH after install. Open a new terminal and re-run this script."
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
        FailExit "Neither winget nor Chocolatey is available. Install ffmpeg yourself and make sure it's on PATH, then re-run this script."
    }
    # Refresh PATH for this session (winget/choco update the registry, not this process).
    $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path", "User")
    Ok "ffmpeg installed."
}

if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) {
    FailExit "ffmpeg still not on PATH after install. Open a new terminal and re-run this script."
}

# --- 4. Build (needed before placing deep-filter.exe, so we know the binary path)
# NOTE: the build downloads a prebuilt ONNX Runtime (cdn.pyke.io) for the `ort`
# crate and links it statically, so this step needs network access.
Step "Building VOCAN (release mode)"
cargo build --release
if ($LASTEXITCODE -ne 0) {
    FailExit "Build failed. See the output above."
}
Ok "Build finished."

$BinDir = Join-Path $RepoRoot "target\release"
$VocanExe = Join-Path $BinDir "VOCAN.exe"
if (-not (Test-Path $VocanExe)) {
    FailExit "Expected binary not found at $VocanExe"
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

# --- 6. Silero VAD model (optional) --------------------------------------------
$ModelDir  = Join-Path $BinDir "models"
$ModelDest = Join-Path $ModelDir "silero_vad.onnx"
if ($NoVad) {
    Step "Skipping Silero VAD model (-NoVad given)"
} else {
    Step "Installing Silero VAD model (optional speech detection for Trim silence)"
    try {
        New-Item -ItemType Directory -Path $ModelDir -Force | Out-Null
        Write-Host "  Downloading: $ModelUrl"
        Invoke-WebRequest -Uri $ModelUrl -OutFile $ModelDest -UseBasicParsing

        $gotHash = (Get-FileHash -Path $ModelDest -Algorithm SHA256).Hash
        if ($gotHash -ine $ModelSha256) {
            Remove-Item $ModelDest -Force -ErrorAction SilentlyContinue
            Warn "Silero VAD model failed the SHA-256 check (got $gotHash). Deleted it. Speech detection stays disabled (this is an optional feature)."
        } else {
            Ok "Silero VAD model installed at $ModelDest"
        }
    } catch {
        Remove-Item $ModelDest -Force -ErrorAction SilentlyContinue
        Warn "Silero VAD model download failed: $($_.Exception.Message). Speech detection stays disabled (this is an optional feature)."
    }
}

# --- 7. Tests -------------------------------------------------------------------
$FastTestsOk = $true
$FullTestsOk = $true
$FastPassed = 0; $FastFailed = 0
$FullPassed = 0; $FullFailed = 0

function ParseTestCounts($outputLines) {
    $passed = 0; $failed = 0
    foreach ($line in $outputLines) {
        if ($line -match 'test result:.*?(\d+) passed; (\d+) failed') {
            $passed += [int]$Matches[1]
            $failed += [int]$Matches[2]
        }
    }
    return @($passed, $failed)
}

if ($SkipTests) {
    Step "Skipping tests (-SkipTests given)"
} else {
    # The tests run from target\release\deps, not from next to VOCAN.exe, so
    # tell the VAD tests where the model is (they skip themselves without it).
    if (Test-Path $ModelDest) {
        $env:VOCAN_SILERO_MODEL = $ModelDest
    }

    Step "Running fast tests (no ffmpeg required)"
    # NOTE: cargo writes normal progress (e.g. "Compiling", "Finished") to stderr.
    # Under Windows PowerShell 5.1, capturing a native command's stderr via 2>&1
    # wraps each line in an ErrorRecord; with $ErrorActionPreference = "Stop"
    # (set at the top of this script) that terminates the whole script on the
    # very first such line, before we ever get to check $LASTEXITCODE. Relax
    # it just for these two calls, where we intentionally inspect the exit
    # code ourselves instead of letting PowerShell treat stderr as fatal.
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $fastOutput = & cargo test 2>&1
    $ErrorActionPreference = $prevEap
    $fastOutput | ForEach-Object { Write-Host $_ }
    $FastTestsOk = ($LASTEXITCODE -eq 0)
    $FastPassed, $FastFailed = ParseTestCounts $fastOutput
    if ($FastTestsOk) {
        Ok "Fast tests passed ($FastPassed passed, $FastFailed failed)."
    } else {
        Fail "Fast tests reported failures ($FastPassed passed, $FastFailed failed). See the output above."
    }

    Step "Running full tests (ffmpeg-dependent)"
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $fullOutput = & cargo test -- --ignored 2>&1
    $ErrorActionPreference = $prevEap
    $fullOutput | ForEach-Object { Write-Host $_ }
    $FullTestsOk = ($LASTEXITCODE -eq 0)
    $FullPassed, $FullFailed = ParseTestCounts $fullOutput
    if ($FullTestsOk) {
        Ok "Full tests passed ($FullPassed passed, $FullFailed failed)."
    } else {
        Fail "Full tests reported failures ($FullPassed passed, $FullFailed failed). See the output above."
    }
}

# --- 8. Package a clean, ready-to-run folder and remove build litter ----------
Step "Packaging a clean, ready-to-run folder"
$AppDir = Join-Path $RepoRoot "VOCAN-App"
if (Test-Path $AppDir) {
    Remove-Item $AppDir -Recurse -Force
}
New-Item -ItemType Directory -Path $AppDir | Out-Null
Copy-Item $VocanExe (Join-Path $AppDir "VOCAN.exe")

$DfnInBin = Join-Path $BinDir "deep-filter.exe"
if (Test-Path $DfnInBin) {
    Copy-Item $DfnInBin (Join-Path $AppDir "deep-filter.exe")
}

if (Test-Path $ModelDir) {
    Copy-Item $ModelDir (Join-Path $AppDir "models") -Recurse
}
Ok "Ready-to-run files copied to $AppDir"

if ($KeepBuild) {
    Step "Keeping build files (-KeepBuild given)"
} else {
    Step "Cleaning up build files (this can take a moment)"
    Remove-Item (Join-Path $RepoRoot "target") -Recurse -Force -ErrorAction SilentlyContinue
    Ok "Removed the target folder. Your source code is untouched; rebuild any time with 'cargo build --release'."
}

# --- 9. Summary ------------------------------------------------------------------
Step "Done"
$OverallOk = $FastTestsOk -and $FullTestsOk
if ($SkipTests) {
    Write-Host "`nVOCAN was built. Tests were skipped (-SkipTests)." -ForegroundColor Green
} elseif ($OverallOk) {
    $TotalPassed = $FastPassed + $FullPassed
    $TotalFailed = $FastFailed + $FullFailed
    Write-Host "`nVOCAN installed and verified successfully." -ForegroundColor Green
    Write-Host "Test summary: $TotalPassed passed, $TotalFailed failed."
} else {
    $TotalPassed = $FastPassed + $FullPassed
    $TotalFailed = $FastFailed + $FullFailed
    Write-Host "`nVOCAN was built, but some tests reported failures." -ForegroundColor Yellow
    Write-Host "Test summary: $TotalPassed passed, $TotalFailed failed. See the output above for details."
}

Write-Host ""
Write-Host "Now go to this folder:" -ForegroundColor Cyan
Write-Host "    $AppDir"
Write-Host "and run VOCAN.exe."
Write-Host ""

if (-not $NoPause) {
    Read-Host "Press Enter to close this window"
}
