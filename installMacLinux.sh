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
#   7. Copies just the files needed to run VOCAN (the VOCAN binary and, if
#      installed, deep-filter) into a clean "VOCAN-App" folder, then deletes
#      the "target" build folder (many hundreds of MB of intermediate build
#      files you don't need to just run the app). Your source code and this
#      script are never touched by this step.
#   8. Prints a summary and, if run in an interactive terminal, waits for a
#      key press before the window closes.
#
# Run it from the root of a cloned VOCAN repository:
#   chmod +x installMacLinux.sh
#   ./installMacLinux.sh
#
# Flags:
#   --no-dfn3      Skip the DeepFilterNet3 download entirely.
#   --skip-tests   Skip running the test suite after building.
#   --keep-build   Do not delete the "target" build folder at the end.
#                  Use this if you plan to keep developing/rebuilding VOCAN.
#   --no-pause     Do not wait for a key press before exiting.
# ==============================================================================

set -euo pipefail

SKIP_DFN3=false
SKIP_TESTS=false
KEEP_BUILD=false
NO_PAUSE=false
for arg in "$@"; do
  case "$arg" in
    --no-dfn3) SKIP_DFN3=true ;;
    --skip-tests) SKIP_TESTS=true ;;
    --keep-build) KEEP_BUILD=true ;;
    --no-pause) NO_PAUSE=true ;;
    *) echo "Unknown option: $arg" >&2; exit 1 ;;
  esac
done

# --- Pretty output helpers ---------------------------------------------------
c_reset="\033[0m"; c_bold="\033[1m"; c_green="\033[32m"; c_yellow="\033[33m"; c_red="\033[31m"; c_blue="\033[34m"
step()  { printf "\n${c_bold}${c_blue}==> %s${c_reset}\n" "$1"; }
ok()    { printf "${c_green}  OK: %s${c_reset}\n" "$1"; }
warn()  { printf "${c_yellow}  WARNING: %s${c_reset}\n" "$1"; }
fail()  { printf "${c_red}  ERROR: %s${c_reset}\n" "$1"; }

pause_before_exit() {
  if [ "$NO_PAUSE" != true ] && [ -t 0 ]; then
    read -rp "Press Enter to close this window..." _ignored
  fi
}

fail_exit() {
  fail "$1"
  pause_before_exit
  exit 1
}

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

# --- 1. Detect OS and architecture -------------------------------------------
step "Detecting platform"
OS_NAME="$(uname -s)"
ARCH_NAME="$(uname -m)"

case "$OS_NAME" in
  Darwin) PLATFORM="macos" ;;
  Linux)  PLATFORM="linux" ;;
  *) fail_exit "Unsupported OS: $OS_NAME (this script supports macOS and Linux only; use installWindows.ps1 on Windows)" ;;
esac

case "$ARCH_NAME" in
  arm64|aarch64) ARCH="aarch64" ;;
  x86_64|amd64)  ARCH="x86_64" ;;
  *) fail_exit "Unsupported CPU architecture: $ARCH_NAME" ;;
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
  fail_exit "cargo still not on PATH after install. Open a new terminal and re-run this script."
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
      fail_exit "Homebrew is not installed. Install it yourself from https://brew.sh, then re-run this script (or install ffmpeg by any other means and make sure it's on PATH)."
    fi
  else
    if command -v apt-get >/dev/null 2>&1; then
      sudo apt-get update && sudo apt-get install -y ffmpeg
    elif command -v dnf >/dev/null 2>&1; then
      sudo dnf install -y ffmpeg
    elif command -v pacman >/dev/null 2>&1; then
      sudo pacman -Sy --noconfirm ffmpeg
    else
      fail_exit "No supported package manager found (apt-get/dnf/pacman). Install ffmpeg yourself and make sure it's on PATH, then re-run this script."
    fi
  fi
  ok "ffmpeg installed."
fi

FFMPEG_BIN="$(command -v ffmpeg)"

# --- 4. Build (needed before placing deep-filter, so we know the binary path) -
step "Building VOCAN (release mode)"
if ! cargo build --release; then
  fail_exit "Build failed. See the output above."
fi
ok "Build finished."

BIN_DIR="$REPO_ROOT/target/release"
if [ ! -f "$BIN_DIR/VOCAN" ]; then
  fail_exit "Expected binary not found at $BIN_DIR/VOCAN"
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
FAST_EXIT=0
FULL_EXIT=0
FAST_PASSED=0; FAST_FAILED=0
FULL_PASSED=0; FULL_FAILED=0

parse_test_counts() {
  # Sums "test result: ok. N passed; M failed" across every binary in a run.
  local log_file="$1"
  local passed=0 failed=0 p m
  while IFS= read -r line; do
    if [[ "$line" =~ test\ result:.*\ ([0-9]+)\ passed\;\ ([0-9]+)\ failed ]]; then
      p="${BASH_REMATCH[1]}"
      m="${BASH_REMATCH[2]}"
      passed=$((passed + p))
      failed=$((failed + m))
    fi
  done < "$log_file"
  echo "$passed $failed"
}

if [ "$SKIP_TESTS" = true ]; then
  step "Skipping tests (--skip-tests given)"
else
  step "Running fast tests (no ffmpeg required)"
  FAST_LOG="$(mktemp)"
  cargo test 2>&1 | tee "$FAST_LOG" || true
  FAST_EXIT=${PIPESTATUS[0]}
  read -r FAST_PASSED FAST_FAILED <<< "$(parse_test_counts "$FAST_LOG")"
  rm -f "$FAST_LOG"
  if [ "$FAST_EXIT" -eq 0 ]; then
    ok "Fast tests passed ($FAST_PASSED passed, $FAST_FAILED failed)."
  else
    fail "Fast tests reported failures ($FAST_PASSED passed, $FAST_FAILED failed). See the output above."
  fi

  step "Running full tests (ffmpeg-dependent)"
  FULL_LOG="$(mktemp)"
  cargo test -- --ignored 2>&1 | tee "$FULL_LOG" || true
  FULL_EXIT=${PIPESTATUS[0]}
  read -r FULL_PASSED FULL_FAILED <<< "$(parse_test_counts "$FULL_LOG")"
  rm -f "$FULL_LOG"
  if [ "$FULL_EXIT" -eq 0 ]; then
    ok "Full tests passed ($FULL_PASSED passed, $FULL_FAILED failed)."
  else
    fail "Full tests reported failures ($FULL_PASSED passed, $FULL_FAILED failed). See the output above."
  fi
fi

# --- 7. Package a clean, ready-to-run folder and remove build litter ---------
step "Packaging a clean, ready-to-run folder"
APP_DIR="$REPO_ROOT/VOCAN-App"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR"
cp "$BIN_DIR/VOCAN" "$APP_DIR/VOCAN"
chmod +x "$APP_DIR/VOCAN"
if [ -f "$BIN_DIR/deep-filter" ]; then
  cp "$BIN_DIR/deep-filter" "$APP_DIR/deep-filter"
  chmod +x "$APP_DIR/deep-filter"
fi
ok "Ready-to-run files copied to $APP_DIR"

if [ "$KEEP_BUILD" = true ]; then
  step "Keeping build files (--keep-build given)"
else
  step "Cleaning up build files (this can take a moment)"
  rm -rf "$REPO_ROOT/target"
  ok "Removed target/. Your source code is untouched; rebuild any time with 'cargo build --release'."
fi

# --- 8. Summary ------------------------------------------------------------------
step "Done"
if [ "$SKIP_TESTS" = true ]; then
  printf "${c_bold}${c_green}VOCAN was built. Tests were skipped (--skip-tests).${c_reset}\n"
else
  TOTAL_PASSED=$((FAST_PASSED + FULL_PASSED))
  TOTAL_FAILED=$((FAST_FAILED + FULL_FAILED))
  if [ "$FAST_EXIT" -eq 0 ] && [ "$FULL_EXIT" -eq 0 ]; then
    printf "${c_bold}${c_green}VOCAN installed and verified successfully.${c_reset}\n"
  else
    printf "${c_bold}${c_yellow}VOCAN was built, but some tests reported failures.${c_reset}\n"
  fi
  printf "Test summary: %s passed, %s failed.\n" "$TOTAL_PASSED" "$TOTAL_FAILED"
fi

printf "\n${c_bold}Now go to this folder:${c_reset}\n    %s\nand run ./VOCAN.\n\n" "$APP_DIR"

pause_before_exit
