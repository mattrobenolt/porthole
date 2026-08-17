#!/usr/bin/env bash
# Milestone-3 tunnel registry test. A stub `ssh` (found first in PATH)
# binds the forwarded port with nc instead of opening a connection, so
# the whole registry runs without a remote. Two hosts on one daemon
# exercise the first-tunnel-wins conflict policy.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build -q

T=$(mktemp -d)
DPID=""
cleanup() {
    [ -n "$DPID" ] && kill "$DPID" 2>/dev/null || true
    pkill -f "nc -k -l 127.0.0.1" 2>/dev/null || true
    rm -rf "$T"
}
trap cleanup EXIT

mkdir -p "$T/bin"
cat > "$T/bin/ssh" <<'EOF'
#!/bin/sh
# Stub ssh: parse -L <port>:localhost:<port> from the arguments, then
# listen on that local port forever. Exits 1 immediately for the port
# named in STUB_SSH_FAIL to simulate an ssh failure.
while [ $# -gt 0 ]; do
    if [ "$1" = "-L" ]; then
        shift
        spec="$1"
    fi
    shift
done
port="${spec%%:*}"
if [ "${STUB_SSH_FAIL:-}" = "$port" ]; then
    exit 1
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

echo "== start daemon with two hosts =="
./target/debug/porthole daemon testhost otherhost 2>"$T/daemon.log" &
DPID=$!
sleep 0.5

echo "== 1. first loopback URL: spawns tunnel, waits for readiness, opens =="
ln -sf "$T/.porthole.d/testhost.sock" "$T/.porthole.sock"
./target/debug/porthole open http://localhost:3000
sleep 0.3

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

echo "== 5. ssh fails immediately: readiness timeout, no open =="
./target/debug/porthole open http://localhost:3999
sleep 6

echo "== 6. porthole status (tunnels show none here: the stub ssh execs into nc) =="
./target/debug/porthole status

echo "== 6.5 tunnel subcommand (rig covers only the negative paths) =="
./target/debug/porthole tunnel list
rc=0; ./target/debug/porthole tunnel kill 3000 || rc=$?; echo "kill-missing exit=$rc"
rc=0; ./target/debug/porthole tunnel frobnicate || rc=$?; echo "unknown exit=$rc"

echo "== 7. SIGTERM: sockets removed, tunnel children killed =="
kill -TERM "$DPID"
DPID=""
sleep 0.7
echo "--- socket dir after shutdown (expect empty):"
ls -A "$T/.porthole.d" || true
if pgrep -f "nc -k -l 127.0.0.1 3000" >/dev/null; then
    echo "FAIL: tunnel child survived SIGTERM"
    exit 1
fi
echo "   tunnel children gone"

echo "== opened.log (expect: 3000, 3000/path2, 4000 — nothing else) =="
cat "$T/opened.log"
echo "== daemon log (expect SIGTERM + killed-on-shutdown lines) =="
cat "$T/daemon.log"
echo "== tunnel smoke test done =="
