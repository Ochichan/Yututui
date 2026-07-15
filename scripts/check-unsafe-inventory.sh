#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

pattern='\bunsafe(\s*\{|\s+fn|\s+impl|\s+extern|\s+trait)'
matches=$(mktemp)
unexpected=$(mktemp)
trap 'rm -f "$matches" "$unexpected"' EXIT

if ! rg -n --with-filename "$pattern" src crates/ratatui-image/src > "$matches"; then
  echo "unsafe inventory ok"
  exit 0
fi

is_allowed() {
  case "$1" in
    crates/ratatui-image/src/picker.rs) return 0 ;;
    crates/ratatui-image/src/protocol/halfblocks/chafa.rs) return 0 ;;
    src/bin/yututray.rs) return 0 ;;
    src/daemon/mod.rs) return 0 ;;
    src/desktop/platform/windows.rs) return 0 ;;
    src/desktop/single_instance.rs) return 0 ;;
    src/desktop/startup.rs) return 0 ;;
    src/data_ownership.rs) return 0 ;;
    src/data_export/macos_private.rs) return 0 ;;
    src/data_export/publish.rs) return 0 ;;
    src/data_export/windows_private.rs) return 0 ;;
    src/media/identity.rs) return 0 ;;
    src/media/macos.rs) return 0 ;;
    src/media/smtc.rs) return 0 ;;
    src/player/guardian.rs) return 0 ;;
    src/player/lifetime.rs) return 0 ;;
    src/test_util/env.rs) return 0 ;;
    src/util/process.rs) return 0 ;;
    src/util/runtime.rs) return 0 ;;
    src/util/safe_fs.rs) return 0 ;;
    src/util/safe_fs/pinned.rs) return 0 ;;
    *) return 1 ;;
  esac
}

while IFS=: read -r file line rest; do
  if ! is_allowed "$file"; then
    printf '%s:%s:%s\n' "$file" "$line" "$rest" >> "$unexpected"
  fi
done < "$matches"

if [ -s "$unexpected" ]; then
  echo "error: unsafe appears outside the reviewed path allowlist:" >&2
  cat "$unexpected" >&2
  exit 1
fi

echo "unsafe inventory ok"
