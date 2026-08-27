#!/usr/bin/env bash
# Tunnel registry test. A stub `ssh` (found first in PATH) simulates a
# real ssh: master spawn (-M) creates a control socket, mux forward
# (-O forward -L) binds the port, child tunnel (-N -L) binds in the
# foreground, and -O check/exit/cancel handle the control protocol.
# Two hosts on one daemon exercise the first-tunnel-wins conflict
# policy. The supervisor starts a master for testhost (stub creates
# the control socket → mux path); for otherhost the stub simulates
# spawn failure → child fallback path.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build -q

T=$(mktemp -d)
DPID=""
cleanup() {
    [ -n "$DPID" ] && kill "$DPID" 2>/dev/null || true
    pkill -f "nc -k -l 127.0.0.1" 2>/dev/null || true
    pkill -f "porthole.d/master" 2>/dev/null || true
    rm -rf "$T"
}
trap cleanup EXIT

mkdir -p "$T/bin"
cat > "$T/bin/ssh" <<'EOF'
#!/bin/sh
# Stub ssh: simulates a real ssh for the tunnel registry + supervisor.
# Handles master spawn (-M), mux forward (-O forward -L), child tunnel
# (-N -L), master check (-O check), master exit (-O exit), and
# forward cancel (-O cancel).
#
# For -M: creates the control socket (python unix listener) and exits 0
# (daemonizes). For otherhost: exits 1 (spawn failure → child fallback).
# STUB_SSH_FAIL=<port> exits 1 for that port on both mux and child paths.

mode=""
spec=""
socket=""
host=""
mux=""
while [ $# -gt 0 ]; do
    case "$1" in
        -M) mode=master ;;
        -N) [ "$mode" != "master" ] && mode=child ;;
        -O) shift
            case "${1:-}" in
                forward) mode=mux ;;
                check)   mode=check ;;
                exit)    mode=exit ;;
                cancel)  mode=cancel ;;
            esac
            ;;
        -L) shift; spec="${1:-}" ;;
        -S) shift; socket="${1:-}" ;;
        -R) shift ;;
        -o) shift ;;
        *) host="$1" ;;
    esac
    shift
done

case "$mode" in
    master)
        # Supervisor's master spawn. Create the control socket and
        # exit 0 (daemonize). For otherhost, simulate failure.
        [ -n "$socket" ] || exit 1
        [ "$host" = "otherhost" ] && exit 1
        python3 -c '
import socket, sys
s = socket.socket(socket.AF_UNIX)
try:
    s.bind(sys.argv[1])
except OSError:
    sys.exit(0)
s.listen(128)
while True:
    conn, _ = s.accept()
    conn.close()
' "$socket" >/dev/null 2>&1 &
        # Give the listener a moment to bind before we exit (simulates
        # the master creating the control socket before the parent
        # forks into the background).
        for _ in $(seq 1 10); do [ -S "$socket" ] && break; sleep 0.05; done
        exit 0
        ;;
    mux)
        # -O forward -L <port>: bind with nc, exit 0 (master's ack).
        [ -n "$spec" ] || exit 1
        port="${spec%%:*}"
        if [ "${STUB_SSH_FAIL:-}" = "$port" ]; then exit 1; fi
        nc -k -l 127.0.0.1 "$port" >/dev/null 2>&1 &
        exit 0
        ;;
    child)
        # -N -L <port>: bind in foreground.
        [ -n "$spec" ] || exit 1
        port="${spec%%:*}"
        if [ "${STUB_SSH_FAIL:-}" = "$port" ]; then exit 1; fi
        exec nc -k -l 127.0.0.1 "$port"
        ;;
    check)
        # -O check: is the master alive?
        [ -S "$socket" ] && exit 0 || exit 1
        ;;
    exit)
        # -O exit: stop the master (unlink socket).
        rm -f "$socket" 2>/dev/null
        exit 0
        ;;
    cancel)
        # -O cancel -L <port>: cancel a forward (no-op for the stub).
        exit 0
        ;;
    *)
        exit 1
        ;;
esac
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
# Give the supervisor time to spawn the master for testhost. The stub
# creates the control socket (~/.porthole.d/master/testhost) and exits
# 0 (daemonizes); the supervisor detects this and switches to polling.
sleep 0.5
for _ in $(seq 1 30); do [ -S "$T/.porthole.d/master/testhost" ] && break; sleep 0.1; done
[ -S "$T/.porthole.d/master/testhost" ] || { echo "FAIL: supervisor master socket never appeared"; cat "$T/daemon.log"; exit 1; }

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

echo "== 7. host without a master: spawned-child fallback =="
ln -sf "$T/.porthole.d/otherhost.sock" "$T/.porthole.sock"
./target/debug/porthole open http://localhost:3001
sleep 0.3
nc -z 127.0.0.1 3001 || { echo "FAIL: child tunnel port 3001 not listening"; exit 1; }
echo "   child tunnel on 3001 up"

echo "== 8. porthole status =="
./target/debug/porthole status

echo "== 8.5 tunnel subcommand =="
./target/debug/porthole tunnel list
rc=0; ./target/debug/porthole tunnel kill 3999 || rc=$?; echo "kill-missing exit=$rc"
rc=0; ./target/debug/porthole tunnel frobnicate || rc=$?; echo "unknown exit=$rc"

echo "== 9. SIGTERM: sockets removed, child tunnels killed =="
kill -TERM "$DPID"
DPID=""
sleep 0.7
echo "--- socket dir after shutdown (expect control/ and master/ dirs):"
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
