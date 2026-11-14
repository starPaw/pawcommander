#!/usr/bin/env bash
# Disposable loopback-only SFTP integration test. No real credentials are used.
set -euo pipefail
cd "$(dirname "$0")/.."
sshd_bin="${SSHD_BIN:-/usr/sbin/sshd}"
test_root="$(mktemp -d /tmp/pawcommander-sftp.XXXXXX)"
server_pid=""
test_host="${PAWCOMMANDER_TEST_SFTP_HOST:-127.0.0.1}"
# Keep this disposable server loopback-only, including the IPv6 test mode.
case "$test_host" in 127.0.0.1|::1) ;; *) exit 2 ;; esac
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$test_root"
}
trap cleanup EXIT
ssh-keygen -q -t ed25519 -N '' -f "$test_root/host"
ssh-keygen -q -t ed25519 -N '' -f "$test_root/other"
ssh-keygen -q -t ed25519 -N 'test-passphrase' -f "$test_root/client"
# OS picks an unused high port. A competing bind fails the test safely.
test_port="$(python3 -c 'import socket,sys; host=sys.argv[1]; s=socket.socket(socket.AF_INET6 if ":" in host else socket.AF_INET); s.bind((host,0)); print(s.getsockname()[1])' "$test_host")"
sftp_options=""
if [[ "${PAWCOMMANDER_TEST_DISABLE_STATVFS:-0}" == "1" ]]; then
  sftp_options="-P fstatvfs"
fi
cat > "$test_root/sshd_config" <<CONFIG
ListenAddress $test_host
Port $test_port
HostKey $test_root/host
PidFile $test_root/sshd.pid
AuthorizedKeysFile $test_root/client.pub
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
PermitRootLogin prohibit-password
AllowUsers $(id -un)
Subsystem sftp internal-sftp $sftp_options
LogLevel ERROR
CONFIG
"$sshd_bin" -D -e -f "$test_root/sshd_config" > "$test_root/sshd.log" 2>&1 &
server_pid=$!
for attempt in {1..50}; do
  if ! kill -0 "$server_pid" 2>/dev/null; then cat "$test_root/sshd.log" >&2; exit 1; fi
  if ssh-keyscan -T 1 -p "$test_port" "$test_host" > "$test_root/known_hosts" 2>/dev/null; then break; fi
  sleep 0.1
done
export PAWCOMMANDER_TEST_SFTP_ROOT="$test_root"
export PAWCOMMANDER_TEST_SFTP_HOST="$test_host"
export PAWCOMMANDER_TEST_SFTP_PORT="$test_port"
export PAWCOMMANDER_TEST_SFTP_USER="$(id -un)"
cargo test --locked openssh_round_trip_and_host_verification -- --ignored --nocapture
