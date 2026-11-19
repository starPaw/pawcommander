#!/usr/bin/env bash
# Disposable keyboard-interactive SSH/SFTP server with synthetic answers.
set -euo pipefail
cd "$(dirname "$0")/.."
test_root="$(mktemp -d /tmp/pawcommander-interactive.XXXXXX)"
server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$test_root"
}
trap cleanup EXIT
ssh-keygen -q -t ed25519 -N '' -f "$test_root/host"
ssh-keygen -q -t ed25519 -N '' -f "$test_root/rotated"
ssh-keygen -q -t ed25519 -N '' -f "$test_root/client"
cc -Wall -Wextra -Werror -Wno-deprecated-declarations -DWITH_SERVER \
  scripts/test-fixtures/ssh-interactive.c $(pkg-config --cflags --libs libssh) \
  -o "$test_root/server"
"$test_root/server" "$test_root/host" "$test_root/client.pub" "$test_root/rotated" > "$test_root/port" &
server_pid=$!
for attempt in {1..50}; do
  kill -0 "$server_pid"
  if [[ -s "$test_root/port" ]]; then break; fi
  sleep 0.1
done
read -r test_port < "$test_root/port"
read -r key_type public_key _ < "$test_root/host.pub"
printf '[127.0.0.1]:%s %s %s\n' "$test_port" "$key_type" "$public_key" > "$test_root/known_hosts"
ssh-keygen -H -f "$test_root/known_hosts" > "$test_root/ssh-keygen-hash.log" 2>&1
for key_name in host rotated; do
  ssh-keygen -E sha256 -lf "$test_root/$key_name.pub" | awk '{print $2}'
done > "$test_root/fingerprints"
export PAWCOMMANDER_TEST_INTERACTIVE_ROOT="$test_root"
export PAWCOMMANDER_TEST_INTERACTIVE_PORT="$test_port"
cargo test --locked ssh_interactive_round_trip -- --ignored --nocapture
wait "$server_pid"
server_pid=""
