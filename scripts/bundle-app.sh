#!/usr/bin/env bash
# Builds mine.app: a release build of the desktop app, with `mine` next to it in Contents/MacOS so the
# app starts the daemon with that `mine` (the second place `find_mine` looks).
# usage: scripts/bundle-app.sh [--dest DIR]   (default: $HOME/Applications)
# ponytail: no icon and no code signing yet (stage 8, with the Homebrew install).
set -euo pipefail

usage() { echo "usage: scripts/bundle-app.sh [--dest DIR]" >&2; exit 2; }
dest="$HOME/Applications"
case "$#:${1:-}" in
  0:) ;;
  2:--dest) dest=$2 ;;
  *) usage ;;
esac
[ -n "$dest" ] || usage

root=$(cd "$(dirname "$0")/.." && pwd)
manifest="$root/Cargo.toml"
cargo build --release --manifest-path "$manifest" -p mine-app -p mine-cli
version=$(cargo pkgid --manifest-path "$manifest" -p mine-app | sed 's/.*[#@]//')
# Ask cargo where it built, so CARGO_TARGET_DIR and a `build.target-dir` in a cargo config both work.
target=$(cargo metadata --format-version 1 --no-deps --manifest-path "$manifest" |
  plutil -extract target_directory raw -o - -)
bin="$target/release"

# The bundle's `mine` is what the running daemon starts every new holder with and what every session's hook
# runs, so the old bundle stays in place until the new one is complete, and then the two are swapped.
# ponytail: two renames leave a moment with no bundle; renamex_np(RENAME_SWAP) if that ever matters.
mkdir -p "$dest"
app="$dest/mine.app"
tmp=$(mktemp -d "$dest/.mine.app.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
new="$tmp/mine.app"
mkdir -p "$new/Contents/MacOS"
cp "$bin/mine-app" "$bin/mine" "$new/Contents/MacOS/"
for f in mine-app mine; do
  [ -x "$new/Contents/MacOS/$f" ] || { echo "Couldn't copy $f into the bundle" >&2; exit 1; }
done

cat > "$new/Contents/Info.plist" <<PLIST
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

# The old bundle moves into the temp dir, which the trap removes once the new one is in place.
old="$tmp/old.app"
if [ -e "$app" ]; then mv "$app" "$old"; fi
if ! mv "$new" "$app"; then
  if [ -e "$old" ]; then mv "$old" "$app"; fi
  echo "Couldn't move the new bundle into $app" >&2
  exit 1
fi

echo "Built $app (version $version)"
