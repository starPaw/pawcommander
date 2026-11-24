#!/bin/bash
# Verify a macOS ZIP against a detached signature and a previously trusted public key.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  printf 'Usage: bash %s <trusted-release-key.pub> <PawCommander-release.zip>\n' "$0" >&2
  exit 2
fi

archive="$2"
archive_name="${archive##*/}"
release_name='^PawCommander-[0-9][0-9A-Za-z.+-]*-macOS-(arm64|x86_64)\.zip$'
if [[ ! "$archive_name" =~ $release_name ]]; then
  echo 'Use the original versioned PawCommander macOS ZIP filename.' >&2
  exit 2
fi

archive_dir="$(/usr/bin/dirname -- "$archive")"
checksums="$archive_dir/SHA256SUMS.txt"
signature="$checksums.sig"
for asset in "$1" "$archive" "$checksums" "$signature"; do
  if [[ ! -f "$asset" ]]; then
    printf 'Missing required file: %s\n' "$asset" >&2
    exit 1
  fi
done

public_key="$(/bin/cat -- "$1")"
read -r key_type key_data _ <<< "$public_key"
if [[ "$public_key" == *$'\n'* || "$key_type" != ssh-ed25519 || ! "$key_data" =~ ^[A-Za-z0-9+/]+={0,2}$ ]]; then
  echo 'The trusted release key must be a single OpenSSH Ed25519 public key.' >&2
  exit 1
fi

# Reconstruct the only accepted record so both checks use the same bytes.
checksum="$(/usr/bin/shasum -a 256 < "$archive")"
record="${checksum%% *}  $archive_name"
if ! printf '%s\n' "$record" | /usr/bin/cmp -s - "$checksums"; then
  echo 'The release checksum does not match this ZIP and filename.' >&2
  exit 1
fi

printf '%s\n' "$record" | /usr/bin/ssh-keygen -Y verify \
  -f <(printf 'pawcommander-release %s %s\n' "$key_type" "$key_data") \
  -I pawcommander-release -n macos-release@pawcommander.capture402.xyz -s "$signature"
printf 'Verified release archive: %s\n' "$archive"
