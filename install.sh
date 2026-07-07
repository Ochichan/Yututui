#!/usr/bin/env bash
#
# YuTuTui! installer (macOS / Linux) — makes `ytt` runnable from anywhere, no manual setup.
# macOS release/source installs also place the `yututray` menu-bar companion beside it.
#
#   curl -fsSL https://raw.githubusercontent.com/Ochichan/Yututui/main/install.sh | bash
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
DESKTOP_BIN=yututray
REPO_SLUG="Ochichan/Yututui"
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
# the binary inside the CI release archive (yututui-macos-arm64.tar.gz contains `ytt`), so
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
  install_macos_desktop_companion "$(dirname "$src")" "$INSTALL_DIR"
}

install_macos_desktop_companion() {
  local src_dir="$1" dest_dir="$2" companion src dest
  [ "$OS" = Darwin ] || return 0

  if [ -f "$src_dir/$DESKTOP_BIN" ]; then
    companion="$DESKTOP_BIN"
  elif [ -f "$src_dir/yututray" ]; then
    # Backward-compatible with older pinned release archives.
    companion="yututray"
  else
    return 0
  fi

  src="$src_dir/$companion"
  dest="$dest_dir/$companion"
  if [ "$src" != "$dest" ]; then
    install -m 0755 "$src" "$dest"
  else
    chmod 0755 "$dest"
  fi
  xattr -d com.apple.quarantine "$dest" 2>/dev/null || true
  ok "Installed menu-bar companion -> $dest  (keep it at login: $companion --install-startup)"
}

# Install the Linux launcher entry + icon into the XDG user dirs so `ytt` appears in app
# menus and MPRIS media widgets (KDE/GNOME) resolve its icon by the "yututui" theme name.
# User-level, matching the ~/.local/bin binary install; the cache refreshes are best-effort.
install_linux_desktop() {
  local desktop_src="$1" icon_src="$2"
  local data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
  local apps_dir="$data_dir/applications"
  # The freedesktop icon theme spec only searches size dirs that hicolor's index.theme
  # declares. 1024x1024 is NOT a standard entry, so an icon dropped there is never resolved
  # by name and the launcher shows a blank icon. 512x512 IS standard on every modern hicolor
  # theme, and icon loaders downscale our 1024px source cleanly. We also drop a copy into the
  # pixmaps dir, which implementations scan as a fallback regardless of the active theme.
  local icon_dir="$data_dir/icons/hicolor/512x512/apps"
  local pixmaps_dir="$data_dir/pixmaps"
  mkdir -p "$apps_dir" "$icon_dir" "$pixmaps_dir"
  install -m 0644 "$desktop_src" "$apps_dir/yututui.desktop"
  install -m 0644 "$icon_src" "$icon_dir/yututui.png"
  install -m 0644 "$icon_src" "$pixmaps_dir/yututui.png"
  # Sweep away an icon left in the old non-standard 1024x1024 dir by earlier installers so
  # the stale copy doesn't linger (it was never resolvable anyway).
  rm -f "$data_dir/icons/hicolor/1024x1024/apps/yututui.png" 2>/dev/null || true
  update-desktop-database "$apps_dir" >/dev/null 2>&1 || true
  gtk-update-icon-cache -q -t "$data_dir/icons/hicolor" >/dev/null 2>&1 || true
  ok "Installed launcher + icon -> $apps_dir/yututui.desktop"
}

install_via_cargo() {
  if [ ! -f Cargo.toml ]; then
    die "No prebuilt for $OS/$ARCH on the Releases page, and this isn't a YuTuTui! checkout.
  Clone it and re-run, or build with cargo:
    git clone https://github.com/$REPO_SLUG && cd Yututui && ./install.sh"
  fi
  command -v cargo >/dev/null 2>&1 || die \
"No prebuilt binary for $OS/$ARCH and Rust isn't installed.
  Install Rust (one line):
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  then re-run ./install.sh   (or grab a prebuilt binary from the project's Releases page)."
  info "Building from source with cargo — this can take a few minutes the first time…"
  if [ "$OS" = Darwin ]; then
    cargo install --path . --force --features desktop --bin "$BIN" --bin "$DESKTOP_BIN"
  else
    cargo install --path . --force
  fi
  # cargo installs into \$CARGO_HOME/bin (default ~/.cargo/bin), already on PATH via rustup.
  INSTALL_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
  ok "Built and installed -> $INSTALL_DIR/$BIN"
  install_macos_desktop_companion "$INSTALL_DIR" "$INSTALL_DIR"

  # If an older HOME-local install appears earlier on PATH than cargo's bin dir, refresh
  # that visible command too; otherwise `ytt` would still launch the stale binary.
  local visible cargo_install_dir
  visible="$(command -v "$BIN" 2>/dev/null || true)"
  if [ -n "$visible" ] && [ "$visible" != "$INSTALL_DIR/$BIN" ]; then
    case "$visible" in
      "$HOME"/*)
        install -m 0755 "$INSTALL_DIR/$BIN" "$visible"
        if [ "$OS" = Darwin ]; then
          xattr -d com.apple.quarantine "$visible" 2>/dev/null || true
        fi
        cargo_install_dir="$INSTALL_DIR"
        INSTALL_DIR="$(dirname "$visible")"
        install_macos_desktop_companion "$cargo_install_dir" "$INSTALL_DIR"
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
    Darwin/arm64)              echo "yututui-macos-arm64.tar.gz" ;;
    Darwin/x86_64)             echo "yututui-macos-x64.tar.gz" ;;
    Linux/x86_64)              echo "yututui-linux-x64.tar.gz" ;;
    Linux/aarch64|Linux/arm64) echo "yututui-linux-arm64.tar.gz" ;;
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
  if [ "$OS" = Darwin ]; then
    tar -xzf "$tmp/$archive" -C "$tmp" "$DESKTOP_BIN" 2>/dev/null \
      || tar -xzf "$tmp/$archive" -C "$tmp" yututray 2>/dev/null \
      || true
  fi
  install_prebuilt "$tmp/$BIN"
  # Linux archives also carry a .desktop entry + icon (releases after v1.5.9); place them in
  # the XDG user dirs. Older archives just skip this quietly.
  if [ "$OS" = Linux ] && tar -xzf "$tmp/$archive" -C "$tmp" yututui.desktop yututui.png 2>/dev/null; then
    install_linux_desktop "$tmp/yututui.desktop" "$tmp/yututui.png"
  fi
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
  # Replace any previous yututui block first, so re-runs (even with a different INSTALL_DIR,
  # e.g. switching between the prebuilt and cargo paths) never accumulate stale entries.
  if [ -f "$rc" ] && grep -qsF '# >>> yututui >>>' "$rc"; then
    tmp="$(mktemp)"
    awk '/# >>> yututui >>>/{skip=1} skip && /# <<< yututui <<</{skip=0; next} !skip' "$rc" > "$tmp" \
      && cat "$tmp" > "$rc" && rm -f "$tmp"
  fi
  {
    printf '\n# >>> yututui >>>\n'
    printf '# added by YuTuTui! install.sh — remove this block to undo\n'
    printf '%s\n' "$line"
    printf '# <<< yututui <<<\n'
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
