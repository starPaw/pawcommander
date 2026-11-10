#!/usr/bin/env bash
# Build a native macOS ZIP with licenses, source attribution, and a checksum.
set -euo pipefail
project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"
about_bin="${CARGO_ABOUT_BIN:-cargo-about}"
command -v "$about_bin" >/dev/null || {
  echo 'Install cargo-about 0.9.3, or set CARGO_ABOUT_BIN to its executable.' >&2
  exit 1
}
command -v python3 >/dev/null
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
bash scripts/build-macos.sh
version="$(sed -n '/^\[package\]/,/^\[/s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml)"
architecture="$(uname -m)"
target="$(rustc -vV | sed -n 's/^host: //p')"
release_dir="$project_dir/dist/release"
mkdir -p "$release_dir"
staging="$(mktemp -d "$project_dir/dist/package.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
ditto dist/PawCommander.app "$staging/PawCommander.app"
"$about_bin" generate --locked --target "$target" --fail scripts/licenses.hbs \
  -o "$staging/THIRD_PARTY_NOTICES.txt"
python3 scripts/package-licenses.py "$staging" "$target"
cp LICENSE "$staging/LICENSE.txt"
cp docs/MACOS_INSTALL.md "$staging/READ_ME.md"
cp "$staging/THIRD_PARTY_NOTICES.txt" "$staging/PawCommander.app/Contents/Resources/"
cp LICENSE "$staging/PawCommander.app/Contents/Resources/LICENSE.txt"
source_commit="${SOURCE_COMMIT:-$(git rev-parse HEAD 2>/dev/null || printf unknown)}"
printf 'PawCommander %s\nArchitecture: %s\nSource commit: %s\nBuild macOS: %s\nMinimum macOS: %s\nSigning: ad-hoc; not notarized\n' \
  "$version" "$architecture" "$source_commit" "$(sw_vers -productVersion)" \
  "$MACOSX_DEPLOYMENT_TARGET" > "$staging/BUILD_INFO.txt"
codesign --force --sign - "$staging/PawCommander.app"
codesign --verify --strict "$staging/PawCommander.app"
archive="PawCommander-${version}-macOS-${architecture}.zip"
ditto -c -k --sequesterRsrc "$staging" "$release_dir/$archive"
(cd "$release_dir" && shasum -a 256 "$archive" > SHA256SUMS.txt)
printf 'Release archive: %s\n' "$release_dir/$archive"
