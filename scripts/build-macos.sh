#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  printf '%s\n' 'This script creates a macOS app bundle and must run on macOS.' >&2
  exit 1
fi

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
app_dir="$project_dir/dist/PawCommander.app"
target_dir="$project_dir/target"
cd "$project_dir"

cargo build --release --locked --target-dir "$target_dir"

# Extract the package version without requiring Python or another packaging tool.
app_version="$(sed -n '/^\[package\]/,/^\[/s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml)"
if [[ -z "$app_version" ]]; then
  printf '%s\n' 'Unable to read the package version from Cargo.toml.' >&2
  exit 1
fi

mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources"
cp "$target_dir/release/pawcommander" "$app_dir/Contents/MacOS/pawcommander"
cp "$project_dir/assets/PawCommander.icns" "$app_dir/Contents/Resources/PawCommander.icns"
chmod 755 "$app_dir/Contents/MacOS/pawcommander"

cat > "$app_dir/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleLocalizations</key>
    <array><string>en</string><string>ru</string></array>
    <key>CFBundleIconFile</key>
    <string>PawCommander.icns</string>
    <key>CFBundleExecutable</key>
    <string>pawcommander</string>
    <key>CFBundleIdentifier</key>
    <string>dev.pawcommander.filemanager</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>PawCommander</string>
    <key>CFBundleDisplayName</key>
    <string>PawCommander</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$app_version</string>
    <key>CFBundleVersion</key>
    <string>$app_version</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

plutil -lint "$app_dir/Contents/Info.plist"
if command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - "$app_dir"
  codesign --verify "$app_dir"
fi

printf 'Created %s\n' "$app_dir"
