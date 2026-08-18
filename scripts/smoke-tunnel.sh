#!/usr/bin/env bash
# Tunnel registry test. A stub `ssh` (found first in PATH) binds the
# forwarded port with nc instead of opening a connection, so the whole
# registry runs without a remote. Two hosts on one daemon exercise the
# first-tunnel-wins conflict policy. testhost has a fake control socket
# (~/.porthole.d/control/testhost, a python unix listener), so it takes
# the mux path; otherhost has none and takes the spawned-child path.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build -q

T=$(mktemp -d)
DPID=""
CTRL=""
cleanup() {
    [ -n "$DPID" ] && kill "$DPID" 2>/dev/null || true
    [ -n "$CTRL" ] && kill "$CTRL" 2>/dev/null || true
    pkill -f "nc -k -l 127.0.0.1" 2>/dev/null || true
    rm -rf "$T"
}
trap cleanup EXIT

mkdir -p "$T/bin"
cat > "$T/bin/ssh" <<'EOF'
#!/bin/sh
# Stub ssh: parse -L <port>:localhost:<port> and bind it with nc.
# "-O forward" (mux mode) backgrounds nc and exits 0, like a real
# master's synchronous ack; anything else execs nc in the foreground
# (child mode). STUB_SSH_FAIL=<port> exits 1 for that port. No -L at
# all ("-O check", etc.) exits 1: this stub is nobody's master.
mux=""
spec=""
while [ $# -gt 0 ]; do
    case "$1" in
        -O) shift; [ "${1:-}" = "forward" ] && mux=1 ;;
        -L) shift; spec="${1:-}" ;;
    esac
    shift
done
[ -n "$spec" ] || exit 1
port="${spec%%:*}"
if [ "${STUB_SSH_FAIL:-}" = "$port" ]; then
    exit 1
fi
if [ -n "$mux" ]; then
    # Redirect: a backgrounded child holding our pipes would keep the
    # caller's output() waiting for EOF forever.
    nc -k -l 127.0.0.1 "$port" >/dev/null 2>&1 &
    exit 0
fi
exec nc -k -l 127.0.0.1 "$port"
EOF
chmod +x "$T/bin/ssh"

cat > "$T/opener.sh" <<EOF
#!/bin/sh
echo "\$1" >> "$T/opened.log"
EOF
chmod +x "$T/opener.sh"

export HOME=$T
export PORTHOLE_OPENER="$T/opener.sh"
export PATH="$T/bin:$PATH"
export STUB_SSH_FAIL=3999

# A real socket file must sit in the control dir for the mux path — a
# plain file fails the check. Start it before the first open: the
# daemon creates ~/.porthole.d/control, so make the dir first.
mkdir -p "$T/.porthole.d/control"
python3 -c '
import socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
s.listen(1)
time.sleep(3600)
' "$T/.porthole.d/control/testhost" &
CTRL=$!
for _ in $(seq 1 30); do [ -S "$T/.porthole.d/control/testhost" ] && break; sleep 0.1; done

echo "== start daemon with two hosts =="
./target/debug/porthole daemon testhost otherhost 2>"$T/daemon.log" &
DPID=$!
sleep 0.5

echo "== 1. first loopback URL: mux forward, waits for readiness, opens =="
ln -sf "$T/.porthole.d/testhost.sock" "$T/.porthole.sock"
./target/debug/porthole open http://localhost:3000
sleep 0.3
nc -z 127.0.0.1 3000 || { echo "FAIL: mux-forwarded port not listening"; exit 1; }

echo "== 2. same host, same port: reuses the tunnel =="
./target/debug/porthole open http://localhost:3000/path2
sleep 0.3

echo "== 3. different host, same port: conflict, no open =="
ln -sf "$T/.porthole.d/otherhost.sock" "$T/.porthole.sock"
./target/debug/porthole open http://localhost:3000
sleep 0.3

echo "== 4. port bound by a real local process: local wins, no tunnel =="
nc -k -l 127.0.0.1 4000 2>/dev/null &
sleep 0.2
ln -sf "$T/.porthole.d/testhost.sock" "$T/.porthole.sock"
./target/debug/porthole open http://localhost:4000
sleep 0.3

echo "== 5. ssh fails immediately (both paths): readiness timeout, no open =="
./target/debug/porthole open http://localhost:3999
sleep 6

echo "== 6. oauth-style URL: callback port sniffed and pre-tunneled =="
./target/debug/porthole open 'https://login.example.com/oauth2/authorize?client_id=x&redirect_uri=http%3A%2F%2Flocalhost%3A3100%2Fcallback&state=y'
sleep 0.5
nc -z 127.0.0.1 3100 || { echo "FAIL: sniffed callback port 3100 not forwarded"; exit 1; }
echo "   callback port 3100 forwarded"

echo "== 7. host without a control socket: spawned-child fallback =="
ln -sf "$T/.porthole.d/otherhost.sock" "$T/.porthole.sock"
./target/debug/porthole open http://localhost:3001
sleep 0.3
nc -z 127.0.0.1 3001 || { echo "FAIL: child tunnel port 3001 not listening"; exit 1; }
echo "   child tunnel on 3001 up"

echo "== 8. porthole status (stub masters fail -O check; informational) =="
./target/debug/porthole status

echo "== 8.5 tunnel subcommand (rig covers only the negative paths) =="
./target/debug/porthole tunnel list
rc=0; ./target/debug/porthole tunnel kill 3999 || rc=$?; echo "kill-missing exit=$rc"
rc=0; ./target/debug/porthole tunnel frobnicate || rc=$?; echo "unknown exit=$rc"

echo "== 9. SIGTERM: sockets removed, child tunnels killed =="
kill -TERM "$DPID"
DPID=""
sleep 0.7
echo "--- socket dir after shutdown (expect only control/):"
ls -A "$T/.porthole.d" || true
if pgrep -f "nc -k -l 127.0.0.1 3001" >/dev/null; then
    echo "FAIL: child tunnel survived SIGTERM"
    exit 1
fi
echo "   child tunnel gone"
# Mux forwards (3000, 3100) intentionally survive daemon shutdown: they
# belong to the ssh connection, not the daemon. The stub's nc stand-ins
# are reaped by the trap.

echo "== opened.log (expect: 3000, 3000/path2, 4000, authorize URL, 3001 — nothing else) =="
cat "$T/opened.log"
echo "== daemon log =="
cat "$T/daemon.log"
echo "== tunnel smoke test done =="
