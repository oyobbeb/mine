#!/usr/bin/env bash
# Builds mine.app: a release build of the desktop app, with `mine` next to it in Contents/MacOS so the
# app starts the daemon with that `mine` (the second place `find_mine` looks).
# usage: scripts/bundle-app.sh [--dest DIR]   (default: $HOME/Applications)
# ponytail: no icon and no code signing yet (stage 8, with the Homebrew install).
set -euo pipefail

dest="$HOME/Applications"
case "$#:${1:-}" in
  0:) ;;
  2:--dest) dest=$2 ;;
  *) echo "usage: scripts/bundle-app.sh [--dest DIR]" >&2; exit 2 ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
manifest="$root/Cargo.toml"
cargo build --release --manifest-path "$manifest" -p mine-app -p mine-cli
version=$(cargo pkgid --manifest-path "$manifest" -p mine-app | sed 's/.*[#@]//')

app="$dest/mine.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
bin="${CARGO_TARGET_DIR:-$root/target}/release"
cp "$bin/mine-app" "$bin/mine" "$app/Contents/MacOS/"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>mine</string>
  <key>CFBundleIdentifier</key><string>dev.mine.app</string>
  <key>CFBundleExecutable</key><string>mine-app</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "Built $app (version $version)"
