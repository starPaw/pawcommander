#!/usr/bin/env bash
# Disposable loopback-only SFTP integration test. No real credentials are used.
set -euo pipefail
cd "$(dirname "$0")/.."
sshd_bin="${SSHD_BIN:-/usr/sbin/sshd}"
test_root="$(mktemp -d /tmp/pawcommander-sftp.XXXXXX)"
server_pid=""
jump_pid=""
agent_pid=""
reconnect="${PAWCOMMANDER_TEST_SFTP_RECONNECT:-0}"
proxyjump="${PAWCOMMANDER_TEST_SFTP_PROXYJUMP:-0}"
terminal_test="${PAWCOMMANDER_TEST_SFTP_TERMINAL:-0}"
test_host="${PAWCOMMANDER_TEST_SFTP_HOST:-127.0.0.1}"
# Keep this disposable server loopback-only, including the IPv6 test mode.
case "$test_host" in 127.0.0.1|::1) ;; *) exit 2 ;; esac
if [[ "$proxyjump" == "1" && ( "$(uname -s)" != Darwin || "$test_host" != 127.0.0.1 || "$reconnect" != "0" || "$terminal_test" != "0" ) ]]; then
  echo "Run the ProxyJump fixture on macOS with default SFTP test options." >&2
  exit 2
fi
cleanup() {
  if [[ -n "$jump_pid" ]]; then
    kill "$jump_pid" 2>/dev/null || true
    wait "$jump_pid" 2>/dev/null || true
  fi
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [[ -n "$agent_pid" ]]; then
    kill "$agent_pid" 2>/dev/null || true
    wait "$agent_pid" 2>/dev/null || true
  fi
  rm -rf "$test_root"
}
trap cleanup EXIT
ssh-keygen -q -t ed25519 -N '' -f "$test_root/host"
ssh-keygen -q -t ed25519 -N '' -f "$test_root/other"
if [[ "$reconnect" == "1" ]]; then
  sftp_server_bin="${SFTP_SERVER_BIN:-/usr/libexec/sftp-server}"
  test -x "$sftp_server_bin"
  cp scripts/test-fixtures/drop-sftp-reply.py "$test_root/subsystem.py"
  ssh-keygen -q -t ed25519 -N '' -f "$test_root/client"
  ssh-agent -D -a "$test_root/agent.sock" > "$test_root/agent.log" 2>&1 &
  agent_pid=$!
  for attempt in {1..50}; do
    kill -0 "$agent_pid"
    if [[ -S "$test_root/agent.sock" ]]; then break; fi
    sleep 0.1
  done
  export SSH_AUTH_SOCK="$test_root/agent.sock"
  ssh-add "$test_root/client"
elif [[ "$terminal_test" == "1" || "$terminal_test" == "embedded" ]]; then
  export PAWCOMMANDER_TEST_TERMINAL_KEY="$test_root/client key '%h \${USER}'"
  ssh-keygen -q -t ed25519 -N '' -f "$PAWCOMMANDER_TEST_TERMINAL_KEY"
  cp "$PAWCOMMANDER_TEST_TERMINAL_KEY.pub" "$test_root/client.pub"
else
  ssh-keygen -q -t ed25519 -N 'test-passphrase' -f "$test_root/client"
fi
# OS picks an unused high port. A competing bind fails the test safely.
test_port="$(python3 -c 'import socket,sys; host=sys.argv[1]; s=socket.socket(socket.AF_INET6 if ":" in host else socket.AF_INET); s.bind((host,0)); print(s.getsockname()[1])' "$test_host")"
sftp_options=""
if [[ "${PAWCOMMANDER_TEST_DISABLE_STATVFS:-0}" == "1" ]]; then
  sftp_options="-P fstatvfs"
fi
if [[ "${PAWCOMMANDER_TEST_SFTP_METADATA_DENIED:-0}" == "1" ]]; then
  sftp_options="-P setstat,fsetstat"
fi
subsystem="internal-sftp $sftp_options"
if [[ "$reconnect" == "1" ]]; then
  subsystem="$(command -v python3) $test_root/subsystem.py $test_root $sftp_server_bin"
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
Subsystem sftp $subsystem
LogLevel ERROR
CONFIG
if [[ "$terminal_test" == "1" || "$terminal_test" == "embedded" ]]; then
  cat >> "$test_root/sshd_config" <<CONFIG
PermitUserRC no
SetEnv SHELL=/bin/sh ENV=/dev/null ZDOTDIR=$test_root LC_CTYPE=UTF-8
CONFIG
fi
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
if [[ "$reconnect" == "1" ]]; then
  cargo test --locked sftp_reconnect_checks_lost_publication_replies -- --ignored --nocapture
elif [[ "$terminal_test" == "1" ]]; then
  cargo test --locked openssh_terminal_starts_interactively_in_the_exact_folder -- --ignored --nocapture
elif [[ "$terminal_test" == "embedded" ]]; then
  cargo test --locked openssh_terminal_embedded_propagates_pty_size_and_folder -- --ignored --nocapture
elif [[ "$proxyjump" == "1" ]]; then
  ssh-keygen -q -t ed25519 -N '' -f "$test_root/jump-host"
  ssh-keygen -q -t ed25519 -N 'jump-test-passphrase' -f "$test_root/jump-client"
  jump_port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')"
  cat > "$test_root/jump_config" <<CONFIG
ListenAddress 127.0.0.1
Port $jump_port
HostKey $test_root/jump-host
PidFile $test_root/jump.pid
AuthorizedKeysFile $test_root/jump-client.pub
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
PermitRootLogin prohibit-password
AllowUsers $(id -un)
AllowTcpForwarding local
PermitOpen 127.0.0.1:$test_port
LogLevel DEBUG1
CONFIG
  "$sshd_bin" -D -e -f "$test_root/jump_config" > "$test_root/jump.log" 2>&1 &
  jump_pid=$!
  for attempt in {1..50}; do
    if ! kill -0 "$jump_pid" 2>/dev/null; then cat "$test_root/jump.log" >&2; exit 1; fi
    if ssh-keyscan -T 1 -p "$jump_port" 127.0.0.1 >> "$test_root/known_hosts" 2>/dev/null; then break; fi
    sleep 0.1
  done
  export PAWCOMMANDER_TEST_SFTP_JUMP_PORT="$jump_port"
  cargo test --locked openssh_proxy_jump_verifies_both_hosts_and_forwards_sftp -- --ignored --nocapture
  grep -Eq 'server_request_direct_tcpip.*target 127\.0\.0\.1' "$test_root/jump.log"
else
  cargo test --locked openssh_round_trip_and_host_verification -- --ignored --nocapture
fi
