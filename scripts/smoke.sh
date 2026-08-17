#!/usr/bin/env bash
# End-to-end smoke test for daemon + client. No ssh and no macOS needed:
# a symlink plays the role of the ssh RemoteForward, and PORTHOLE_OPENER
# points at a logging script instead of /usr/bin/open.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build -q

T=$(mktemp -d)
DPID=""
cleanup() {
    [ -n "$DPID" ] && kill "$DPID" 2>/dev/null || true
    rm -rf "$T"
}
trap cleanup EXIT

cat > "$T/opener.sh" <<EOF
#!/bin/sh
echo "\$1" >> "$T/opened.log"
EOF
chmod +x "$T/opener.sh"

export HOME=$T
export PORTHOLE_OPENER="$T/opener.sh"

echo "== start daemon for host 'testhost' =="
./target/debug/porthole daemon testhost 2>"$T/daemon.log" &
DPID=$!
sleep 0.5

# The ssh RemoteForward, in miniature: the client's well-known socket
# path lands on the daemon's per-host listener.
ln -s "$T/.porthole.d/testhost.sock" "$T/.porthole.sock"

echo "== 1. direct URL via the real client =="
./target/debug/porthole open https://example.com

echo "== 2. loopback URL (tunnel deferred to milestone 3) =="
./target/debug/porthole open http://localhost:3000

echo "== 3. raw injection: ftp URL (bypasses the client-side gate) =="
printf '{"url":"ftp://definitely-not-http"}\n' | nc -N -U "$T/.porthole.sock"

echo "== 4. raw injection: not JSON =="
printf 'this is not json\n' | nc -N -U "$T/.porthole.sock"

sleep 0.4
echo "== opener received =="
cat "$T/opened.log"
echo "== daemon log =="
cat "$T/daemon.log"

echo "== 5. kill -9 the daemon, restart over the stale socket =="
kill -9 "$DPID"
sleep 0.2
./target/debug/porthole daemon testhost 2>"$T/daemon2.log" &
DPID=$!
sleep 0.5
./target/debug/porthole open https://after-restart.example.com
sleep 0.3
echo "== opener received (total) =="
cat "$T/opened.log"
echo "== daemon2 log =="
cat "$T/daemon2.log"

echo "== 6. junk flood: 200KB without a newline gets the connection cut =="
head -c 200000 /dev/zero | nc -N -U "$T/.porthole.sock"
sleep 0.3
echo "== daemon survives and still serves =="
./target/debug/porthole open https://after-junk.example.com
sleep 0.3
cat "$T/opened.log"
echo "== daemon2 log (expect the oversized-line rejection) =="
cat "$T/daemon2.log"

echo "== smoke test passed =="
