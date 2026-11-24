#!/usr/bin/env bash
# Build a native macOS ZIP with licenses, source attribution, and optional release signing.
set -euo pipefail
project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"
about_bin="${CARGO_ABOUT_BIN:-cargo-about}"
command -v "$about_bin" >/dev/null || {
  echo 'Install cargo-about 0.9.3, or set CARGO_ABOUT_BIN to its executable.' >&2
  exit 1
}
command -v python3 >/dev/null
signing_key="${PAWCOMMANDER_RELEASE_SIGNING_KEY:-}"
if [[ -n "$signing_key" ]]; then
  fingerprint="$(/usr/bin/ssh-keygen -lf "$signing_key")"
  if [[ "$fingerprint" != *' (ED25519)' ]]; then
    echo 'Use an Ed25519 release signing key.' >&2
    exit 1
  fi
fi
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
bash scripts/build-macos.sh
version="$(sed -n '/^\[package\]/,/^\[/s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml)"
architecture="$(uname -m)"
target="$(rustc -vV | sed -n 's/^host: //p')"
release_dir="${PAWCOMMANDER_RELEASE_DIR:-$project_dir/dist/release}"
mkdir -p -- "$release_dir"
release_dir="$(cd -- "$release_dir" && pwd -P)"
work_dir="$(mktemp -d "$release_dir/.package.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT
staging="$work_dir/payload"
mkdir "$staging"
ditto dist/PawCommander.app "$staging/PawCommander.app"
"$about_bin" generate --locked --target "$target" --fail scripts/licenses.hbs \
  -o "$staging/THIRD_PARTY_NOTICES.txt"
python3 scripts/package-licenses.py "$staging" "$target"
cp LICENSE "$staging/LICENSE.txt"
cp docs/MACOS_INSTALL.md "$staging/READ_ME.md"
if [[ -f "docs/releases/$version.md" ]]; then
  sed 's@(../MACOS_INSTALL.md)@(READ_ME.md)@g' "docs/releases/$version.md" > "$staging/RELEASE_NOTES.md"
fi
cp "$staging/THIRD_PARTY_NOTICES.txt" "$staging/PawCommander.app/Contents/Resources/"
cp LICENSE "$staging/PawCommander.app/Contents/Resources/LICENSE.txt"
source_commit="${SOURCE_COMMIT:-$(git rev-parse HEAD 2>/dev/null || printf unknown)}"
printf 'PawCommander %s\nArchitecture: %s\nSource commit: %s\nBuild macOS: %s\nMinimum macOS: %s\nSigning: ad-hoc; not notarized\n' \
  "$version" "$architecture" "$source_commit" "$(sw_vers -productVersion)" \
  "$MACOSX_DEPLOYMENT_TARGET" > "$staging/BUILD_INFO.txt"
codesign --force --sign - "$staging/PawCommander.app"
codesign --verify --strict "$staging/PawCommander.app"
archive="PawCommander-${version}-macOS-${architecture}.zip"
ditto -c -k --sequesterRsrc "$staging" "$work_dir/$archive"
(cd "$work_dir" && /usr/bin/shasum -a 256 "$archive" > SHA256SUMS.txt)
if [[ -n "$signing_key" ]]; then
  /usr/bin/ssh-keygen -Y sign -f "$signing_key" -n macos-release@pawcommander.capture402.xyz \
    < "$work_dir/SHA256SUMS.txt" > "$work_dir/SHA256SUMS.txt.sig"
fi

# Withdraw the previous signature before changing the files it authenticates.
rm -f "$release_dir/SHA256SUMS.txt.sig"
mv -f "$work_dir/$archive" "$release_dir/$archive"
mv -f "$work_dir/SHA256SUMS.txt" "$release_dir/SHA256SUMS.txt"
if [[ -n "$signing_key" ]]; then
  mv -f "$work_dir/SHA256SUMS.txt.sig" "$release_dir/SHA256SUMS.txt.sig"
fi
printf 'Release archive: %s\n' "$release_dir/$archive"
