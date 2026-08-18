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

cat > "$T/pbcopy.sh" <<EOF
#!/bin/sh
cat > "$T/clipboard.txt"
EOF
chmod +x "$T/pbcopy.sh"

export HOME=$T
export PORTHOLE_OPENER="$T/opener.sh"
export PORTHOLE_PBCOPY="$T/pbcopy.sh"

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
# Client-side write errors are expected: the daemon cuts the connection
# mid-flood and which pipe end notices first is a race. The assertions
# live in the daemon's log and its survival, not the client's exit code.
head -c 200000 /dev/zero | nc -N -U "$T/.porthole.sock" || true
sleep 0.3
if ! grep -q "line over 65536 bytes" "$T/daemon2.log"; then
    echo "FAIL: daemon never logged the oversized-line cut"; exit 1
fi
echo "   daemon logged the cut"
echo "== daemon survives and still serves =="
./target/debug/porthole open https://after-junk.example.com
sleep 0.3
cat "$T/opened.log"
echo "== daemon2 log (expect the oversized-line rejection) =="
cat "$T/daemon2.log"

echo "== 7. clipboard, detached (stdout is a pipe → daemon socket) =="
printf 'hello clipboard' | ./target/debug/porthole clipboard
sleep 0.3
echo "--- clipboard capture:" && cat "$T/clipboard.txt"

echo "== 8. clipboard, attached (stdout is a tty → OSC 52, no daemon) =="
expected=$(printf 'term-test' | base64 -w 0)
if command -v script >/dev/null; then
    script -qec "printf 'term-test' | ./target/debug/porthole clipboard" "$T/tty.log" >/dev/null
    if grep -qF "$(printf '\033]52;c;')${expected}" "$T/tty.log"; then
        echo "   OSC 52 sequence emitted on the tty"
    else
        echo "FAIL: no OSC 52 sequence in tty output"; exit 1
    fi
else
    echo "   (script(1) unavailable, skipping)"
fi

echo "== 9. clipboard with daemon down: fails fast, never spools =="
kill -TERM "$DPID"; DPID=""
sleep 0.7
rc=0; printf 'never' | ./target/debug/porthole clipboard || rc=$?
echo "   exit=$rc (expect 1)"
[ ! -e "$T/.porthole.spool" ] && echo "   no spool written" || { echo "FAIL: clipboard was spooled"; exit 1; }

echo "== smoke test passed =="
