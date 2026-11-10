#!/usr/bin/env bash
# Publish only the static website; keep previous releases available for rollback.
set -euo pipefail

if [[ $# -ne 1 || ! "$1" =~ ^[a-zA-Z0-9_.@-]+$ || "$1" == -* ]]; then
  echo 'Usage: bash scripts/deploy-website.sh <SSH host or user@host>' >&2
  exit 1
fi
host="$1"
project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"
node --check website/script.js

archive_dir="$(mktemp -d /tmp/pawcommander-site.XXXXXX)"
archive="$archive_dir/site.tar.gz"
trap 'rm -f "$archive"; rmdir "$archive_dir"' EXIT
COPYFILE_DISABLE=1 tar --no-xattrs -czf "$archive" -C website \
  index.html styles.css script.js robots.txt sitemap.xml \
  assets/logo.svg assets/workspace.png assets/icon.png \
  assets/fonts/manrope-latin.woff2 assets/fonts/outfit-latin.woff2 \
  assets/fonts/Manrope-OFL.txt assets/fonts/Outfit-OFL.txt
archive_sha="$(shasum -a 256 "$archive" | cut -d ' ' -f 1)"
index_sha="$(shasum -a 256 website/index.html | cut -d ' ' -f 1)"
release="$(date -u +%Y%m%dT%H%M%SZ)-${archive_sha:0:8}"
remote_archive="/tmp/pawcommander-site-${release}.tar.gz"
ssh_options=(-o BatchMode=yes -o StrictHostKeyChecking=yes -o IgnoreUnknown=UseKeychain -o UseKeychain=yes)
scp "${ssh_options[@]}" "$archive" "$host:$remote_archive"

ssh "${ssh_options[@]}" "$host" \
  "sudo -n bash -s -- '$release' '$remote_archive' '$archive_sha' '$index_sha'" <<'REMOTE'
set -euo pipefail
release="$1"
archive="$2"
archive_sha="$3"
index_sha="$4"
base=/var/www/pawcommander
domain=pawcommander.capture402.xyz
previous="$(readlink "$base/current" || true)"
activated=0
rollback() {
  status=$?
  if [[ $status -ne 0 && $activated -eq 1 ]]; then
    if [[ -n "$previous" ]]; then
      ln -s "$previous" "$base/.rollback-$release"
      mv -Tf "$base/.rollback-$release" "$base/current"
    else
      rm -f "$base/current"
    fi
    echo 'Website health check failed; previous release restored.' >&2
  fi
  rm -f "$archive"
  exit "$status"
}
trap rollback EXIT
printf '%s  %s\n' "$archive_sha" "$archive" | sha256sum --check --status
nginx -t
test -f "/etc/letsencrypt/live/$domain/fullchain.pem"
test ! -e "$base/releases/$release"
install -d -m 755 "$base/releases/$release"
tar -xzf "$archive" --no-same-owner -C "$base/releases/$release"
find "$base/releases/$release" -type d -exec chmod 755 {} +
find "$base/releases/$release" -type f -exec chmod 644 {} +
test -f "$base/releases/$release/index.html"
ln -s "$base/releases/$release" "$base/.current-$release"
mv -Tf "$base/.current-$release" "$base/current"
activated=1
served_sha="$(curl --fail --silent --show-error --resolve "$domain:443:127.0.0.1" "https://$domain/" | sha256sum | cut -d ' ' -f 1)"
test "$served_sha" = "$index_sha"
printf 'Published %s\nhttps://%s/\n' "$release" "$domain"
REMOTE
