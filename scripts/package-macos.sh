#!/usr/bin/env bash
set -euo pipefail

target="${1:?usage: package-macos.sh <target> <archive> [bin] [out-dir]}"
archive="${2:?usage: package-macos.sh <target> <archive> [bin] [out-dir]}"
bin_name="${3:-ytt}"
out_dir="${4:-${GITHUB_WORKSPACE:-$(pwd)}}"
release_dir="target/$target/release"
stage_dir="target/package/macos"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"

if [[ -z "$version" ]]; then
  echo "could not read package version from Cargo.toml" >&2
  exit 1
fi

if [[ ! -x "$release_dir/$bin_name" ]]; then
  echo "missing release binary: $release_dir/$bin_name" >&2
  exit 1
fi

rm -rf "$stage_dir"
mkdir -p "$stage_dir/YuTuTui!.app/Contents/MacOS" \
  "$stage_dir/YuTuTui!.app/Contents/Resources" \
  "$out_dir"

cp "$release_dir/$bin_name" "$stage_dir/$bin_name"
cp "$release_dir/$bin_name" "$stage_dir/YuTuTui!.app/Contents/MacOS/ytt"
cp assets/icons/yututui.icns "$stage_dir/YuTuTui!.app/Contents/Resources/yututui.icns"

cat > "$stage_dir/YuTuTui!.app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>YuTuTui!</string>
  <key>CFBundleExecutable</key>
  <string>ytt</string>
  <key>CFBundleIconFile</key>
  <string>yututui</string>
  <key>CFBundleIdentifier</key>
  <string>io.github.ochi.yututui</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>YuTuTui!</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$version</string>
  <key>CFBundleVersion</key>
  <string>$version</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.music</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
</dict>
</plist>
PLIST

printf 'APPL????' > "$stage_dir/YuTuTui!.app/Contents/PkgInfo"
chmod +x "$stage_dir/YuTuTui!.app/Contents/MacOS/ytt"

archive_items=("$bin_name" "YuTuTui!.app")

if [[ -x "$release_dir/yututray" ]]; then
  mkdir -p "$stage_dir/YuTuTray!.app/Contents/MacOS" \
    "$stage_dir/YuTuTray!.app/Contents/Resources"
  cp "$release_dir/yututray" "$stage_dir/yututray"
  cp "$release_dir/yututray" "$stage_dir/YuTuTray!.app/Contents/MacOS/yututray"
  # Keep `ytt` beside `yututray` inside the helper bundle so the tray's "Open TUI"
  # action can launch an absolute bundled path even when the archive root is moved.
  cp "$release_dir/$bin_name" "$stage_dir/YuTuTray!.app/Contents/MacOS/ytt"
  cp assets/icons/yututui.icns "$stage_dir/YuTuTray!.app/Contents/Resources/yututui.icns"

  cat > "$stage_dir/YuTuTray!.app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>YuTuTray!</string>
  <key>CFBundleExecutable</key>
  <string>yututray</string>
  <key>CFBundleIconFile</key>
  <string>yututui</string>
  <key>CFBundleIdentifier</key>
  <string>io.github.ochi.yututui.tray</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>YuTuTray!</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$version</string>
  <key>CFBundleVersion</key>
  <string>$version</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.music</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>LSUIElement</key>
  <true/>
</dict>
</plist>
PLIST

  printf 'APPL????' > "$stage_dir/YuTuTray!.app/Contents/PkgInfo"
  chmod +x "$stage_dir/yututray" \
    "$stage_dir/YuTuTray!.app/Contents/MacOS/yututray" \
    "$stage_dir/YuTuTray!.app/Contents/MacOS/ytt"
  archive_items+=("yututray" "YuTuTray!.app")
fi

tar -czf "$out_dir/$archive" -C "$stage_dir" "${archive_items[@]}"
