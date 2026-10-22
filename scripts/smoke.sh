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

# The pbpaste stand-in is mode-switchable by marker file: the daemon
# reads PORTHOLE_PBPASTE from its own env at startup, so per-command
# env overrides on the client would change nothing. One daemon, one
# stub, three behaviors: normal, always-fail, always-oversized.
cat > "$T/pbpaste.sh" <<EOF
#!/bin/sh
if [ -f "$T/pbpaste-fail" ]; then exit 3; fi
if [ -f "$T/pbpaste-big" ]; then head -c 40000 /dev/zero | tr '\0' 'a'; exit 0; fi
cat "$T/clipboard.txt" 2>/dev/null
EOF
chmod +x "$T/pbpaste.sh"

cat > "$T/zed.sh" <<EOF
#!/bin/sh
echo "\$1" >> "$T/zed.log"
EOF
chmod +x "$T/zed.sh"

export HOME=$T
export USER=${USER:-$(id -un)}
export PORTHOLE_OPENER="$T/opener.sh"
export PORTHOLE_PBCOPY="$T/pbcopy.sh"
export PORTHOLE_PBPASTE="$T/pbpaste.sh"
export PORTHOLE_ZED="$T/zed.sh"

echo "== start daemon for host 'testhost' =="
./target/debug/porthole daemon testhost 2>"$T/daemon.log" &
DPID=$!
sleep 0.5

# The ssh RemoteForward, in miniature: the client's well-known socket
# path lands on the daemon's per-host listener.
ln -s "$T/.porthole.d/testhost.sock" "$T/.porthole.sock"

echo "== 1. direct URL via the real client =="
./target/debug/porthole open https://example.com

echo "== 1b. zed shim: remote cwd becomes an ssh URL for the local Zed =="
ln -s "$PWD/target/debug/porthole" "$T/zed"
mkdir -p "$T/project"
(cd "$T/project" && "$T/zed" .)
(cd "$T/project" && "$T/zed" "two words")
sleep 0.2
expected="ssh://$USER@testhost$T/project"
expected_spaced="ssh://$USER@testhost$T/project/two%20words"
if [ "$(sed -n 1p "$T/zed.log")" = "$expected" ] &&
   [ "$(sed -n 2p "$T/zed.log")" = "$expected_spaced" ]; then
    echo "   zed received both URLs"
else
    echo "FAIL: zed received:"; cat "$T/zed.log"; exit 1
fi

rc=0; "$T/zed" a b >/dev/null 2>&1 || rc=$?
[ "$rc" = 2 ] && echo "   multiple paths rejected (exit=2)" || { echo "FAIL: multiple paths exit=$rc"; exit 1; }

rc=0; "$T/zed" --foreground >/dev/null 2>&1 || rc=$?
[ "$rc" = 2 ] && echo "   unsupported flags rejected (exit=2)" || { echo "FAIL: unsupported flag exit=$rc"; exit 1; }

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

echo "== 7b. lemonade impersonation (Neovim's ungated probe) =="
ln -s "$PWD/target/debug/porthole" "$T/lemonade"
ln -s "$PWD/target/debug/porthole" "$T/pbpaste"
# stdout redirected to a file: forces the detached path even when the
# script itself runs on a tty.
printf 'via lemonade' | "$T/lemonade" copy > "$T/lemonade.out"
sleep 0.3
if [ "$(cat "$T/clipboard.txt")" = "via lemonade" ]; then
    echo "   lemonade copy reached the daemon"
else
    echo "FAIL: lemonade copy did not reach the daemon"; exit 1
fi
# The paste round-trip: the same content back through the daemon's
# reply path, via Neovim's probe name.
round=$("$T/lemonade" paste 2>"$T/paste.err")
if [ "$round" = "via lemonade" ]; then
    echo "   lemonade paste round-tripped through the daemon"
else
    echo "FAIL: lemonade paste got '$round'"; cat "$T/paste.err"; exit 1
fi
round=$("$T/pbpaste" 2>>"$T/paste.err")
if [ "$round" = "via lemonade" ]; then
    echo "   pbpaste symlink round-tripped"
else
    echo "FAIL: pbpaste got '$round'"; cat "$T/paste.err"; exit 1
fi
# A failing clipboard tool surfaces the daemon's error reply, not a hang.
touch "$T/pbpaste-fail"
rc=0; "$T/pbpaste" >"$T/paste-fail.out" 2>"$T/paste-fail.err" || rc=$?
if [ "$rc" = 1 ] && grep -q "exited" "$T/paste-fail.err"; then
    echo "   failing pbpaste tool fails loudly with the daemon's reason"
else
    echo "FAIL: failing tool exit=$rc"; cat "$T/paste-fail.err"; exit 1
fi
# Over the cap: refused, not truncated.
rm "$T/pbpaste-fail"; touch "$T/pbpaste-big"
rc=0; "$T/pbpaste" >"$T/paste-big.out" 2>"$T/paste-big.err" || rc=$?
rm "$T/pbpaste-big"
if [ "$rc" = 1 ] && grep -q "exceeds 32768 bytes" "$T/paste-big.err"; then
    echo "   oversized clipboard refused with the cap in the message"
else
    echo "FAIL: oversized exit=$rc"; cat "$T/paste-big.err"; exit 1
fi
rc=0; "$T/lemonade" >/dev/null 2>&1 || rc=$?
[ "$rc" = 2 ] && echo "   bare lemonade prints usage (exit=2)" || { echo "FAIL: bare exit=$rc"; exit 1; }

echo "== 8. clipboard, attached (stdout is a tty → OSC 52, no daemon) =="
# tr over base64: GNU base64 needs -w 0 to stop wrapping, BSD base64
# has no -w at all — stripping newlines is correct on both.
expected=$(printf 'term-test' | base64 | tr -d '\n')
if command -v script >/dev/null; then
    if script -qec true /dev/null >/dev/null 2>&1; then
        # GNU script: the command rides the -c flag.
        script -qec "printf 'term-test' | ./target/debug/porthole clipboard" "$T/tty.log" >/dev/null
    else
        # BSD script (macOS): the command is positional.
        script -q "$T/tty.log" sh -c "printf 'term-test' | ./target/debug/porthole clipboard" >/dev/null
    fi
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

rc=0; (cd "$T/project" && "$T/zed" .) >/dev/null 2>"$T/zed-down.err" || rc=$?
[ "$rc" = 1 ] && grep -q "daemon unreachable" "$T/zed-down.err" && \
    echo "   zed fails fast with the daemon down" || { echo "FAIL: zed down exit=$rc"; cat "$T/zed-down.err"; exit 1; }
[ ! -e "$T/.porthole.spool" ] && echo "   zed was not spooled" || { echo "FAIL: zed request was spooled"; exit 1; }

echo "== smoke test passed =="
