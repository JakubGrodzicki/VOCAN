#!/usr/bin/env bash
# ==============================================================================
# VOCAN - one-shot setup script for macOS and Linux.
#
# What this does, in order:
#   1. Detects your OS and CPU architecture.
#   2. Installs Rust (via the official rustup installer) if it's missing.
#   3. Installs ffmpeg (via Homebrew on macOS, apt/dnf/pacman on Linux) if missing.
#   4. Downloads the DeepFilterNet3 "deep-filter" binary from its official
#      GitHub releases (Rikorose/DeepFilterNet) and places it next to the
#      VOCAN binary. This step is OPTIONAL functionality in the app (only
#      needed for the "Dereverb (DeepFilterNet3)" checkbox).
#      On macOS, downloaded binaries are quarantined by Gatekeeper; this
#      script removes that flag ONLY from this specific downloaded file so
#      it can run, the same way you'd do manually via Finder/right-click.
#   5. Builds VOCAN in release mode.
#   6. Runs the automated test suite (fast tests, then ffmpeg-dependent
#      tests) to confirm everything actually works.
#
# Run it from the root of a cloned VOCAN repository:
#   chmod +x installMacLinux.sh
#   ./installMacLinux.sh
#
# Flags:
#   --no-dfn3     Skip the DeepFilterNet3 download entirely.
#   --skip-tests  Skip running the test suite after building.
# ==============================================================================

set -euo pipefail

SKIP_DFN3=false
SKIP_TESTS=false
for arg in "$@"; do
  case "$arg" in
    --no-dfn3) SKIP_DFN3=true ;;
    --skip-tests) SKIP_TESTS=true ;;
    *) echo "Unknown option: $arg" >&2; exit 1 ;;
  esac
done

# --- Pretty output helpers ---------------------------------------------------
c_reset="\033[0m"; c_bold="\033[1m"; c_green="\033[32m"; c_yellow="\033[33m"; c_red="\033[31m"; c_blue="\033[34m"
step()  { printf "\n${c_bold}${c_blue}==> %s${c_reset}\n" "$1"; }
ok()    { printf "${c_green}  OK: %s${c_reset}\n" "$1"; }
warn()  { printf "${c_yellow}  WARNING: %s${c_reset}\n" "$1"; }
fail()  { printf "${c_red}  ERROR: %s${c_reset}\n" "$1"; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

# --- 1. Detect OS and architecture -------------------------------------------
step "Detecting platform"
OS_NAME="$(uname -s)"
ARCH_NAME="$(uname -m)"

case "$OS_NAME" in
  Darwin) PLATFORM="macos" ;;
  Linux)  PLATFORM="linux" ;;
  *) fail "Unsupported OS: $OS_NAME (this script supports macOS and Linux only; use installWindows.ps1 on Windows)"; exit 1 ;;
esac

case "$ARCH_NAME" in
  arm64|aarch64) ARCH="aarch64" ;;
  x86_64|amd64)  ARCH="x86_64" ;;
  *) fail "Unsupported CPU architecture: $ARCH_NAME"; exit 1 ;;
esac

ok "Detected $PLATFORM / $ARCH"

# --- 2. Rust ------------------------------------------------------------------
step "Checking for Rust"
if command -v cargo >/dev/null 2>&1; then
  ok "Rust already installed: $(rustc --version)"
else
  warn "Rust not found. Installing via the official rustup installer (https://rustup.rs)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
  ok "Rust installed."
fi
# Make sure this shell session can see cargo, even on a fresh install.
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

if ! command -v cargo >/dev/null 2>&1; then
  fail "cargo still not on PATH after install. Open a new terminal and re-run this script."
  exit 1
fi

# --- 3. ffmpeg ------------------------------------------------------------------
step "Checking for ffmpeg"
if command -v ffmpeg >/dev/null 2>&1; then
  ok "ffmpeg already installed: $(ffmpeg -version | head -1)"
else
  warn "ffmpeg not found. Installing..."
  if [ "$PLATFORM" = "macos" ]; then
    if command -v brew >/dev/null 2>&1; then
      brew install ffmpeg
    else
      fail "Homebrew is not installed. Install it yourself from https://brew.sh, then re-run this script (or install ffmpeg by any other means and make sure it's on PATH)."
      exit 1
    fi
  else
    if command -v apt-get >/dev/null 2>&1; then
      sudo apt-get update && sudo apt-get install -y ffmpeg
    elif command -v dnf >/dev/null 2>&1; then
      sudo dnf install -y ffmpeg
    elif command -v pacman >/dev/null 2>&1; then
      sudo pacman -Sy --noconfirm ffmpeg
    else
      fail "No supported package manager found (apt-get/dnf/pacman). Install ffmpeg yourself and make sure it's on PATH, then re-run this script."
      exit 1
    fi
  fi
  ok "ffmpeg installed."
fi

FFMPEG_BIN="$(command -v ffmpeg)"

# --- 4. Build (needed before placing deep-filter, so we know the binary path) -
step "Building VOCAN (release mode)"
cargo build --release
ok "Build finished."

BIN_DIR="$REPO_ROOT/target/release"
if [ ! -f "$BIN_DIR/VOCAN" ]; then
  fail "Expected binary not found at $BIN_DIR/VOCAN"
  exit 1
fi
ok "Binary at $BIN_DIR/VOCAN"

# --- 5. DeepFilterNet3 (optional) ---------------------------------------------
if [ "$SKIP_DFN3" = true ]; then
  step "Skipping DeepFilterNet3 (--no-dfn3 given)"
else
  step "Installing DeepFilterNet3 (optional dereverb feature)"

  if [ "$PLATFORM" = "macos" ]; then
    ASSET_SUFFIX="${ARCH}-apple-darwin"
  else
    ASSET_SUFFIX="${ARCH}-unknown-linux-gnu"
  fi

  API_JSON="$(curl -fsSL https://api.github.com/repos/Rikorose/DeepFilterNet/releases/latest || true)"
  ASSET_URL="$(printf '%s' "$API_JSON" \
    | grep -o "\"browser_download_url\": *\"[^\"]*deep-filter-[^\"]*${ASSET_SUFFIX}\"" \
    | head -1 \
    | sed -E 's/.*"(https[^"]+)"/\1/')"

  if [ -z "$ASSET_URL" ]; then
    warn "Could not find a DeepFilterNet3 release asset for ${ASSET_SUFFIX}. Skipping (this is an optional feature)."
  else
    TMP_BIN="$(mktemp)"
    echo "  Downloading: $ASSET_URL"
    curl -fsSL -o "$TMP_BIN" "$ASSET_URL"

    if ! file "$TMP_BIN" | grep -qE "Mach-O|ELF"; then
      fail "Downloaded file doesn't look like a valid executable. Skipping DeepFilterNet3 install."
      rm -f "$TMP_BIN"
    else
      DEST="$BIN_DIR/deep-filter"
      cp "$TMP_BIN" "$DEST"
      rm -f "$TMP_BIN"
      chmod +x "$DEST"

      if [ "$PLATFORM" = "macos" ]; then
        # Downloaded executables are quarantined by Gatekeeper on macOS and
        # refuse to run until this flag is cleared. This only affects the
        # single file we just downloaded, not any system-wide setting.
        xattr -d com.apple.quarantine "$DEST" 2>/dev/null || true
      fi

      # Also place a copy next to ffmpeg, in case VOCAN resolves ffmpeg to an
      # absolute path in some environment (belt-and-suspenders; the primary
      # lookup VOCAN uses is next to its own executable, handled above).
      FFMPEG_DIR="$(dirname "$FFMPEG_BIN")"
      if [ -w "$FFMPEG_DIR" ]; then
        cp "$DEST" "$FFMPEG_DIR/deep-filter" 2>/dev/null || true
      fi

      ok "DeepFilterNet3 installed at $DEST"
    fi
  fi
fi

# --- 6. Tests ------------------------------------------------------------------
if [ "$SKIP_TESTS" = true ]; then
  step "Skipping tests (--skip-tests given)"
else
  step "Running fast tests (no ffmpeg required)"
  cargo test
  ok "Fast tests passed."

  step "Running full tests (ffmpeg-dependent)"
  cargo test -- --ignored
  ok "Full tests passed."
fi

step "Done"
printf "${c_bold}${c_green}VOCAN is built and verified.${c_reset}\n"
printf "Run it with:\n\n    %s\n\n" "$BIN_DIR/VOCAN"
