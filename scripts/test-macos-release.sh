#!/bin/bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
verify="$project_dir/scripts/verify-macos-release.sh"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
cd "$fixture"

ssh-keygen -q -t ed25519 -N '' -f release
ssh-keygen -q -t ed25519 -N '' -f untrusted
mkdir payload
printf 'PawCommander release fixture\n' > payload/build.txt
archive=PawCommander-9.0.1-macOS-arm64.zip
manifest=SHA256SUMS.txt
signature="$manifest.sig"
ditto -c -k payload "$archive"
shasum -a 256 "$archive" > "$manifest"
cp "$manifest" original-checksums

sign() {
  ssh-keygen -Y sign -f "$1" -n "$2" < "$manifest" > "$signature" 2>/dev/null
}

reject() {
  local scenario="$1"
  shift
  if bash "$verify" "$@" > rejected.log 2>&1; then
    printf 'Unexpectedly accepted %s\n' "$scenario" >&2
    cat rejected.log >&2
    exit 1
  fi
  printf 'Rejected %s\n' "$scenario"
}

namespace=macos-release@pawcommander.capture402.xyz
sign release "$namespace"
bash "$verify" release.pub "$archive"
reject 'an untrusted release key' untrusted.pub "$archive"

renamed=PawCommander-9.0.2-macOS-arm64.zip
cp "$archive" "$renamed"
reject 'an archive renamed to a different release' release.pub "$renamed"
cp "$archive" original-archive
printf 'different bytes\n' >> "$archive"
reject 'a changed archive' release.pub "$archive"
shasum -a 256 "$archive" > "$manifest"
reject 'a changed archive with a recomputed checksum' release.pub "$archive"
mv original-archive "$archive"
cp original-checksums "$manifest"

cp "$signature" original-signature
rm "$signature"
reject 'a missing signature' release.pub "$archive"
printf 'invalid signature\n' > "$signature"
reject 'an invalid signature' release.pub "$archive"
sign release git
reject 'a signature for another purpose' release.pub "$archive"
mv original-signature "$signature"

printf '0  missing.txt\n' > "$manifest"
reject 'a changed checksum' release.pub "$archive"
mv original-checksums "$manifest"
printf 'extra entry\n' >> "$manifest"
sign release "$namespace"
reject 'an unexpected extra record in a signed checksum' release.pub "$archive"
