#!/usr/bin/env bash
#
# ytm-tui installer (macOS / Linux) — makes `ytt` runnable from anywhere, no manual setup.
#
#   curl -fsSL https://raw.githubusercontent.com/Ochichan/ytm-tui/main/install.sh | bash
#                                 # download a prebuilt binary for this OS/arch — no clone needed
#   ./install.sh                  # from a clone: use ./dist or a release binary, else cargo build
#   ./install.sh --build          # always build from source (needs Rust + a clone)
#   ./install.sh --no-modify-path # don't touch any shell rc file
#
# Pin a version with  YTT_VERSION=v1.5.8 ./install.sh  (default: the latest release).
#
# It puts the install dir on your PATH (unless --no-modify-path) and checks that the runtime
# tools (mpv, yt-dlp, ffmpeg) are present.
#
set -euo pipefail

BIN=ytt
REPO_SLUG="Ochichan/ytm-tui"
DOWNLOAD_TMP=""
cleanup_download_tmp() {
  if [ -n "${DOWNLOAD_TMP:-}" ]; then
    rm -rf "$DOWNLOAD_TMP"
  fi
}
trap cleanup_download_tmp EXIT

# Run from the repo root regardless of where the script was invoked from.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" >/dev/null 2>&1 && pwd)"
cd "$SCRIPT_DIR"

c_info=$'\033[1;36m'; c_ok=$'\033[1;32m'; c_warn=$'\033[1;33m'; c_err=$'\033[1;31m'; c_off=$'\033[0m'
info() { printf '%s==>%s %s\n'   "$c_info" "$c_off" "$*"; }
ok()   { printf '%s\xe2\x9c\x93%s %s\n' "$c_ok" "$c_off" "$*"; }
warn() { printf '%swarn:%s %s\n' "$c_warn" "$c_off" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$c_err" "$c_off" "$*" >&2; exit 1; }

FORCE_BUILD=0
NO_MODIFY_PATH=0
for arg in "$@"; do
  case "$arg" in
    --build)          FORCE_BUILD=1 ;;
    --no-modify-path) NO_MODIFY_PATH=1 ;;
    *)                warn "ignoring unknown argument: $arg" ;;
  esac
done

OS="$(uname -s)"
ARCH="$(uname -m)"

# Map (OS, ARCH) -> the prebuilt artifact we look for in dist/, if any. The name matches
# the binary inside the CI release archive (ytm-tui-macos-arm64.tar.gz contains `ytt`), so
# extracting that archive into dist/ enables the fast path. dist/ is gitignored, so a fresh
# `git clone` won't have it and will fall through to the cargo build below.
prebuilt_for_platform() {
  case "$OS/$ARCH" in
    Darwin/arm64) echo "dist/ytt" ;;
    *)            echo "" ;;
  esac
}

INSTALL_DIR=""

install_prebuilt() {
  local src="$1"
  INSTALL_DIR="$HOME/.local/bin"
  mkdir -p "$INSTALL_DIR"
  install -m 0755 "$src" "$INSTALL_DIR/$BIN"
  # Strip the quarantine flag if the repo arrived as a downloaded archive (Gatekeeper).
  if [ "$OS" = Darwin ]; then
    xattr -d com.apple.quarantine "$INSTALL_DIR/$BIN" 2>/dev/null || true
  fi
  ok "Installed prebuilt binary -> $INSTALL_DIR/$BIN"
}

install_via_cargo() {
  if [ ! -f Cargo.toml ]; then
    die "No prebuilt for $OS/$ARCH on the Releases page, and this isn't a ytm-tui checkout.
  Clone it and re-run, or build with cargo:
    git clone https://github.com/$REPO_SLUG && cd ytm-tui && ./install.sh"
  fi
  command -v cargo >/dev/null 2>&1 || die \
"No prebuilt binary for $OS/$ARCH and Rust isn't installed.
  Install Rust (one line):
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  then re-run ./install.sh   (or grab a prebuilt binary from the project's Releases page)."
  info "Building from source with cargo — this can take a few minutes the first time…"
  cargo install --path . --force
  # cargo installs into \$CARGO_HOME/bin (default ~/.cargo/bin), already on PATH via rustup.
  INSTALL_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
  ok "Built and installed -> $INSTALL_DIR/$BIN"

  # If an older HOME-local install appears earlier on PATH than cargo's bin dir, refresh
  # that visible command too; otherwise `ytt` would still launch the stale binary.
  visible="$(command -v "$BIN" 2>/dev/null || true)"
  if [ -n "$visible" ] && [ "$visible" != "$INSTALL_DIR/$BIN" ]; then
    case "$visible" in
      "$HOME"/*)
        install -m 0755 "$INSTALL_DIR/$BIN" "$visible"
        if [ "$OS" = Darwin ]; then
          xattr -d com.apple.quarantine "$visible" 2>/dev/null || true
        fi
        INSTALL_DIR="$(dirname "$visible")"
        ok "Updated PATH-visible binary -> $visible"
        ;;
      *)
        warn "'$visible' appears earlier on PATH than $INSTALL_DIR/$BIN; update or remove it if `ytt` still starts an older build."
        ;;
    esac
  fi
}

# --- download a prebuilt release archive (the `curl | bash` path) ----------------------
# Map (OS, ARCH) -> the release archive CI publishes (see .github/workflows/build.yml).
# Empty when we don't ship a prebuilt for this platform.
release_archive_for_platform() {
  case "$OS/$ARCH" in
    Darwin/arm64)              echo "ytm-tui-macos-arm64.tar.gz" ;;
    Darwin/x86_64)             echo "ytm-tui-macos-x64.tar.gz" ;;
    Linux/x86_64)              echo "ytm-tui-linux-x64.tar.gz" ;;
    Linux/aarch64|Linux/arm64) echo "ytm-tui-linux-arm64.tar.gz" ;;
    *)                         echo "" ;;
  esac
}

# curl or wget, whichever exists.  $1 = url, $2 = output path.
fetch() {
  if   command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -qO "$2" "$1"
  else die "need curl or wget to download a prebuilt binary"; fi
}

# Verify archive $2 (in dir $1) against the matching line in $1/checksums.txt.
verify_sha256() {
  local dir="$1" archive="$2" want got
  want="$(awk -v f="$archive" '$2 == f || $2 == "*" f {print $1}' "$dir/checksums.txt" | head -n1)"
  [ -n "$want" ] || return 1
  if   command -v sha256sum >/dev/null 2>&1; then got="$(sha256sum "$dir/$archive" | awk '{print $1}')"
  elif command -v shasum    >/dev/null 2>&1; then got="$(shasum -a 256 "$dir/$archive" | awk '{print $1}')"
  else return 2; fi
  [ "$want" = "$got" ]
}

# Download the prebuilt for this platform, verify it, extract `ytt`, and install it.
download_release() {
  local archive="$1" ver base url cks_url tmp
  ver="${YTT_VERSION:-latest}"
  base="https://github.com/$REPO_SLUG/releases"
  if [ "$ver" = latest ]; then
    url="$base/latest/download/$archive";  cks_url="$base/latest/download/checksums.txt"
  else
    url="$base/download/$ver/$archive";     cks_url="$base/download/$ver/checksums.txt"
  fi
  tmp="$(mktemp -d)"
  DOWNLOAD_TMP="$tmp"

  info "Downloading $archive ($ver)…"
  fetch "$url" "$tmp/$archive" || die "download failed: $url"
  fetch "$cks_url" "$tmp/checksums.txt" 2>/dev/null || die "release has no checksums.txt — aborting"
  verify_sha256 "$tmp" "$archive" || die "checksum verification failed for $archive — aborting"
  ok "Checksum verified"
  info "Extracting…"
  tar -xzf "$tmp/$archive" -C "$tmp" "$BIN" || die "archive did not contain $BIN"
  install_prebuilt "$tmp/$BIN"
  rm -rf "$tmp"
  DOWNLOAD_TMP=""
}

# --- choose a strategy -----------------------------------------------------------------
# Order: explicit --build -> a local dist/ prebuilt (repo-dev fast path) -> a release download
# (works with no clone) -> cargo build from a checkout.
if [ "$FORCE_BUILD" -eq 1 ]; then
  install_via_cargo
else
  local_prebuilt="$(prebuilt_for_platform)"
  release_archive="$(release_archive_for_platform)"
  if [ -n "$local_prebuilt" ] && [ -f "$local_prebuilt" ]; then
    install_prebuilt "$local_prebuilt"
  elif [ -n "$release_archive" ]; then
    download_release "$release_archive"
  else
    install_via_cargo
  fi
fi

# --- make sure the install dir is on PATH ----------------------------------------------
on_path() { case ":${PATH:-}:" in *":$1:"*) return 0 ;; *) return 1 ;; esac; }

line="export PATH=\"$INSTALL_DIR:\$PATH\""
if on_path "$INSTALL_DIR"; then
  ok "$INSTALL_DIR is on your PATH"
elif [ "$NO_MODIFY_PATH" -eq 1 ]; then
  warn "$INSTALL_DIR is not on your PATH (--no-modify-path set). Add it yourself:"
  printf '    %s\n' "$line"
else
  warn "$INSTALL_DIR is not on your PATH yet."
  # Pick the rc file the user's shell actually sources on a NEW terminal. On macOS, Terminal
  # opens bash as a LOGIN shell, which reads ~/.bash_profile, NOT ~/.bashrc. (Default SHELL via
  # an intermediate var so `set -u` doesn't trip on an unset SHELL under bash 4+.)
  _shell="${SHELL:-/bin/sh}"
  case "${_shell##*/}" in
    zsh)  rc="$HOME/.zshrc" ;;
    bash) [ "$OS" = Darwin ] && rc="$HOME/.bash_profile" || rc="$HOME/.bashrc" ;;
    *)    rc="$HOME/.profile" ;;
  esac
  # Replace any previous ytm-tui block first, so re-runs (even with a different INSTALL_DIR,
  # e.g. switching between the prebuilt and cargo paths) never accumulate stale entries.
  if [ -f "$rc" ] && grep -qsF '# >>> ytm-tui >>>' "$rc"; then
    tmp="$(mktemp)"
    awk '/# >>> ytm-tui >>>/{skip=1} skip && /# <<< ytm-tui <<</{skip=0; next} !skip' "$rc" > "$tmp" \
      && cat "$tmp" > "$rc" && rm -f "$tmp"
  fi
  {
    printf '\n# >>> ytm-tui >>>\n'
    printf '# added by ytm-tui install.sh — remove this block to undo\n'
    printf '%s\n' "$line"
    printf '# <<< ytm-tui <<<\n'
  } >> "$rc"
  ok "Added $INSTALL_DIR to PATH in $rc"
  info "Activate it now (or just open a new terminal):"
  printf '    %s\n' "$line"
fi

# --- preflight the runtime tools -------------------------------------------------------
# mpv + yt-dlp are required for playback/search; ffmpeg is needed for downloads.
missing=()
for t in mpv yt-dlp ffmpeg; do command -v "$t" >/dev/null 2>&1 || missing+=("$t"); done
if [ "${#missing[@]}" -gt 0 ]; then
  if [ "$OS" = Darwin ] && command -v brew >/dev/null 2>&1; then
    warn "Missing runtime tools: ${missing[*]} — install with:  brew install ${missing[*]}"
  elif [ "$OS" = Darwin ]; then
    warn "Missing runtime tools: ${missing[*]} — install Homebrew (https://brew.sh), then:  brew install ${missing[*]}"
  else
    warn "Missing runtime tools: ${missing[*]} — install them via your package manager"
  fi
else
  ok "Runtime tools present (mpv, yt-dlp, ffmpeg)"
fi

printf '\n'
ok "Done. Start it with:  ${c_info}${BIN}${c_off}"
