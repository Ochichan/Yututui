#!/usr/bin/env bash
#
# ytm-tui installer (macOS / Linux) — makes `ytt` runnable from anywhere, no manual setup.
#
#   ./install.sh                  # use a prebuilt binary if one ships for this platform,
#                                 # otherwise build from source with cargo
#   ./install.sh --build          # always build from source (needs Rust)
#   ./install.sh --no-modify-path # don't touch any shell rc file
#
# It also puts the install dir on your PATH (if it isn't already, and unless
# --no-modify-path is given) and checks that the two runtime tools (mpv, yt-dlp) are present.
#
set -euo pipefail

BIN=ytt

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

# --- choose a strategy -----------------------------------------------------------------
prebuilt="$(prebuilt_for_platform)"
if [ "$FORCE_BUILD" -eq 0 ] && [ -n "$prebuilt" ] && [ -f "$prebuilt" ]; then
  install_prebuilt "$prebuilt"
else
  install_via_cargo
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
missing=()
for t in mpv yt-dlp; do command -v "$t" >/dev/null 2>&1 || missing+=("$t"); done
if [ "${#missing[@]}" -gt 0 ]; then
  if [ "$OS" = Darwin ] && command -v brew >/dev/null 2>&1; then
    warn "Missing runtime tools: ${missing[*]} — install with:  brew install ${missing[*]}"
  elif [ "$OS" = Darwin ]; then
    warn "Missing runtime tools: ${missing[*]} — install Homebrew (https://brew.sh), then:  brew install ${missing[*]}"
  else
    warn "Missing runtime tools: ${missing[*]} — install them via your package manager"
  fi
else
  ok "Runtime tools present (mpv, yt-dlp)"
fi

printf '\n'
ok "Done. Start it with:  ${c_info}${BIN}${c_off}"
