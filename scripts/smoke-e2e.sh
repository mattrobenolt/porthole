#!/usr/bin/env bash
# Full end-to-end test over REAL ssh on one machine. This box plays both
# roles: the "remote" (porthole open + spool) and the "local" (daemon +
# tunnels). A user-mode sshd on 127.0.0.1:2222 provides the actual
# transport. The daemon's supervisor owns the SSH master connection
# (carrying the RemoteForward) and auto-reconnects when it drops.
# Only the browser is simulated (PORTHOLE_OPENER).
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build -q

T="/tmp/porthole-e2e-$$"
mkdir -p "$T"
PIDS=()
SSHD_PID=""
cleanup() {
    # Stop the supervisor's master via its control socket.
    PATH="$T/bin:$PATH" HOME="$T" ssh -O exit -S "$T/.porthole.d/master/remote1" remote1 2>/dev/null || true
    for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    pkill -f "nc -l 127.0.0.1 8888" 2>/dev/null || true
    rm -rf "$T"
}
trap cleanup EXIT

SSHD=$(command -v sshd)  # sshd re-execs itself; must be absolute

start_sshd() {
    "$SSHD" -f "$T/sshd_config" -E "$T/sshd.log" -D &
    SSHD_PID=$!
    PIDS+=("$SSHD_PID")
    for _ in $(seq 1 50); do nc -z 127.0.0.1 2222 && break; sleep 0.1; done
}

echo "== setup: throwaway keys, user-mode sshd with StreamLocalBindUnlink =="
ssh-keygen -q -t ed25519 -N '' -f "$T/hostkey"
ssh-keygen -q -t ed25519 -N '' -f "$T/id_ed25519"
cp "$T/id_ed25519.pub" "$T/authorized_keys"

cat > "$T/sshd_config" <<EOF
Port 2222
ListenAddress 127.0.0.1
HostKey $T/hostkey
AuthorizedKeysFile $T/authorized_keys
UsePAM no
PasswordAuthentication no
# Throwaway localhost test: /tmp is world-writable and StrictModes would
# reject the key path for it.
StrictModes no
AllowStreamLocalForwarding yes
StreamLocalBindUnlink yes
PidFile $T/sshd.pid
EOF

start_sshd

# ssh reads ~/.ssh/config from the passwd entry, not $HOME, so a wrapper
# earlier in PATH injects -F. This also covers the daemon's own ssh
# spawns (supervisor + tunnel children), which inherit PATH.
mkdir -p "$T/bin"
cat > "$T/bin/ssh" <<EOF
#!/bin/sh
exec "$(command -v ssh)" -F "$T/.ssh/config" "\$@"
EOF
chmod +x "$T/bin/ssh"

# ssh alias: in production this is home-manager's matchBlocks. Here it's
# one throwaway stanza. The daemon's supervisor overrides ControlMaster,
# ControlPath, and ControlPersist with its own -M/-S/-o flags; these
# settings are for interactive sessions only.
mkdir -p "$T/.ssh"
cat > "$T/.ssh/config" <<EOF
Host remote1
    HostName 127.0.0.1
    Port 2222
    IdentityFile $T/id_ed25519
    IdentitiesOnly yes
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    LogLevel ERROR
    ControlMaster auto
    ControlPath $T/.porthole.d/control/%n
    ControlPersist 60m
EOF
chmod 700 "$T/.ssh"; chmod 600 "$T/.ssh/config"

# A real HTTP service on the "remote" to receive tunnelled traffic.
# Output to a file: background children must never hold our stdout.
(
    while true; do
        printf 'HTTP/1.0 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nporthole-ok\n' \
            | nc -l 127.0.0.1 8888
    done
) >"$T/service.log" 2>&1 &
PIDS+=($!)

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
cat > "$T/pbpaste.sh" <<EOF
#!/bin/sh
cat "$T/clipboard.txt" 2>/dev/null
EOF
chmod +x "$T/pbpaste.sh"
cat > "$T/zed.sh" <<EOF
#!/bin/sh
echo "\$1" >> "$T/zed.log"
EOF
chmod +x "$T/zed.sh"

# From here on everything porthole sees the test home.
export HOME=$T
export USER=${USER:-$(id -un)}
export PORTHOLE_OPENER="$T/opener.sh"
export PORTHOLE_PBCOPY="$T/pbcopy.sh"
export PORTHOLE_PBPASTE="$T/pbpaste.sh"
export PORTHOLE_ZED="$T/zed.sh"
export PATH="$T/bin:$PATH"

echo "== start daemon (plays the macOS side; supervisor owns the SSH master) =="
./target/debug/porthole daemon remote1 2>"$T/daemon.log" &
PIDS+=($!)
# The supervisor connects to the sshd and creates the RemoteForward.
# Wait for the forwarded socket to appear on the "remote" side.
ok=""
for _ in $(seq 1 50); do [ -S "$T/.porthole.sock" ] && ok=1 && break; sleep 0.1; done
[ -n "$ok" ] || { echo "FAIL: supervisor's RemoteForward socket never appeared"; cat "$T/daemon.log"; cat "$T/sshd.log"; exit 1; }
# Probe until the socket actually accepts (file presence ≠ readiness).
ok=""
for _ in $(seq 1 20); do
    if python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])' "$T/.porthole.sock" 2>/dev/null; then
        ok=1; break
    fi
    sleep 0.2
done
[ -n "$ok" ] || { echo "FAIL: forwarded socket appeared but never accepted"; cat "$T/daemon.log"; cat "$T/sshd.log"; exit 1; }
echo "   forwarded socket is live (supervisor master connected)"

echo "== 1. direct URL through the real RemoteForward =="
./target/debug/porthole open https://e2e.example.com
sleep 0.3

echo "== 1b. clipboard copy + paste round-trip over the real RemoteForward =="
# stdout to /dev/null: force the daemon path even when the rig itself
# runs on a tty (an attached clipboard would speak OSC 52 and skip the
# socket entirely).
printf 'e2e clipboard' | ./target/debug/porthole clipboard >/dev/null
sleep 0.3
round=$(./target/debug/porthole paste 2>"$T/paste.err")
[ "$round" = "e2e clipboard" ] || { echo "FAIL: paste round-trip got '$round'"; cat "$T/paste.err"; cat "$T/daemon.log"; exit 1; }
echo "   copy → daemon → paste round-tripped through the forwarded socket"

echo "== 1c. zed shim over the real RemoteForward =="
ln -s "$PWD/target/debug/porthole" "$T/zed"
mkdir -p "$T/project"
project=$(cd "$T/project" && pwd -P)
(cd "$T/project" && "$T/zed" .)
sleep 0.3
[ "$(cat "$T/zed.log" 2>/dev/null)" = "ssh://$USER@remote1$project" ] || {
    echo "FAIL: zed got '$(cat "$T/zed.log" 2>/dev/null)'"; cat "$T/daemon.log"; exit 1
}
echo "   zed received the ssh URL with the daemon's host alias"

echo "== 2. loopback URL, one box shares loopback: expect 'local wins' =="
echo "   (a REAL tunnel carrying traffic needs two machines — this box's =="
echo "    remote and local share 127.0.0.1, so the probe is right to fire) =="
./target/debug/porthole open http://localhost:8888
sleep 0.5
echo "==   service sanity check: $(curl -s http://localhost:8888/) =="

echo "== 3. kill sshd: supervisor's master dies, client spools =="
# Kill the sshd's child processes FIRST (while they're still parented
# to the sshd), then the parent. If the parent dies first, the
# children are reparented to init and pkill -P can't find them — the
# TCP connection stays alive and the ssh client never exits.
pkill -P "$SSHD_PID" 2>/dev/null || true
kill "$SSHD_PID" 2>/dev/null || true
# Give the ssh client time to detect the closed TCP connection and
# exit. The supervisor will then detect the exit and start backoff.
sleep 1
# Force-remove the forwarded socket to simulate the connection being
# fully gone (sshd may leave the file behind on abrupt exit).
rm -f "$T/.porthole.sock"
./target/debug/porthole open https://spooled.example.com
echo "   spool contents: $(cat "$T/.porthole.spool" 2>/dev/null || echo '(empty)')"
grep -q 'spooled.example.com' "$T/.porthole.spool" 2>/dev/null || { echo "FAIL: URL was not spooled"; exit 1; }
echo "   URL spooled successfully"

echo "== 4. restart sshd: supervisor reconnects, spool flushes =="
start_sshd
# Wait for the supervisor to reconnect (backoff is 1-2s) and the
# forwarded socket to reappear.
ok=""
for _ in $(seq 1 50); do [ -S "$T/.porthole.sock" ] && ok=1 && break; sleep 0.1; done
[ -n "$ok" ] || { echo "FAIL: supervisor did not reconnect after sshd restart"; cat "$T/daemon.log"; cat "$T/sshd.log"; exit 1; }
# Probe until the new socket accepts.
ok=""
for _ in $(seq 1 20); do
    if python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])' "$T/.porthole.sock" 2>/dev/null; then
        ok=1; break
    fi
    sleep 0.2
done
[ -n "$ok" ] || { echo "FAIL: re-attached socket never accepted"; exit 1; }
echo "   supervisor reconnected, forwarded socket live"
./target/debug/porthole open https://flush.example.com
sleep 0.5
[ ! -f "$T/.porthole.spool" ] || { echo "FAIL: spool was not flushed after reconnect"; exit 1; }
echo "   spool flushed successfully"

echo "== 5. oauth-style URL: sniffed callback port forwarded via the supervisor's master =="
./target/debug/porthole open 'https://login.example.com/oauth2/authorize?client_id=x&redirect_uri=http%3A%2F%2Flocalhost%3A8899%2Fcallback&state=y'
ok=""
for _ in $(seq 1 30); do nc -z 127.0.0.1 8899 && ok=1 && break; sleep 0.2; done
[ -n "$ok" ] || { echo "FAIL: sniffed callback port never forwarded"; cat "$T/daemon.log"; exit 1; }
echo "   callback port 8899 accepts (mux forward on the supervisor's master)"

echo "== 6. status sees the mux forward (lsof on the master pid) =="
./target/debug/porthole status | tee "$T/status.out"
grep -q 'port 8899 → remote1 (mux' "$T/status.out" || { echo "FAIL: status missed the mux forward"; exit 1; }

echo "== 7. tunnel kill cancels the mux forward =="
./target/debug/porthole tunnel kill 8899
sleep 0.3
if nc -z 127.0.0.1 8899 2>/dev/null; then
    echo "FAIL: forward survived tunnel kill"
    exit 1
fi
echo "   forward gone"

echo "== opened.log (expect: e2e, localhost:8888, spooled, flush, authorize — in order) =="
cat "$T/opened.log"
echo "== daemon log =="
cat "$T/daemon.log"
echo "== e2e smoke test done =="
