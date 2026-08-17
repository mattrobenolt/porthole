#!/usr/bin/env bash
# Full end-to-end test over REAL ssh on one machine. This box plays both
# roles: the "remote" (porthole open + spool) and the "local" (daemon +
# tunnels). A user-mode sshd on 127.0.0.1:2222 provides the actual
# transport: the RemoteForward socket is created by sshd for real, the
# daemon's tunnel is a real `ssh -N -L`, and curl proves bytes flow.
# Only the browser is simulated (PORTHOLE_OPENER).
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build -q

T=$(mktemp -d)
PIDS=()
ATTACH=""
cleanup() {
    [ -n "$ATTACH" ] && kill "$ATTACH" 2>/dev/null || true
    for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    # The trap kills the service's loop subshell; this kills the nc child
    # it may have spawned mid-listen. Orphans hold our stdout pipe open
    # and the caller never sees EOF.
    pkill -f "nc -l 127.0.0.1 8888" 2>/dev/null || true
    rm -rf "$T"
}
trap cleanup EXIT

SSHD=$(command -v sshd)  # sshd re-execs itself; must be absolute

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

"$SSHD" -f "$T/sshd_config" -E "$T/sshd.log" -D &
PIDS+=($!)
for _ in $(seq 1 50); do nc -z 127.0.0.1 2222 && break; sleep 0.1; done

# ssh reads ~/.ssh/config from the passwd entry, not $HOME, so a wrapper
# earlier in PATH injects -F. This also covers the daemon's own tunnel
# spawns, which inherit PATH.
mkdir -p "$T/bin"
cat > "$T/bin/ssh" <<EOF
#!/bin/sh
exec "$(command -v ssh)" -F "$T/.ssh/config" "\$@"
EOF
chmod +x "$T/bin/ssh"

# ssh alias: in production this is home-manager's matchBlocks + the
# per-host RemoteForward. Here it's one throwaway stanza.
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

# From here on everything porthole sees the test home.
export HOME=$T
export PORTHOLE_OPENER="$T/opener.sh"
export PATH="$T/bin:$PATH"

echo "== start daemon (plays the macOS side) =="
./target/debug/porthole daemon remote1 2>"$T/daemon.log" &
PIDS+=($!)
sleep 0.5

echo "== attach: the RemoteForward session (production: mac -> remote) =="
ssh -N -R "$T/.porthole.sock:$T/.porthole.d/remote1.sock" remote1 &
ATTACH=$!
for _ in $(seq 1 50); do [ -S "$T/.porthole.sock" ] && break; sleep 0.1; done
[ -S "$T/.porthole.sock" ] || { echo "FAIL: forwarded socket never appeared"; cat "$T/sshd.log"; exit 1; }
echo "   forwarded socket is live"

echo "== 1. direct URL through the real RemoteForward =="
./target/debug/porthole open https://e2e.example.com
sleep 0.3

echo "== 2. loopback URL, one box shares loopback: expect 'local wins' =="
echo "   (a REAL tunnel carrying traffic needs two machines — this box's =="
echo "    remote and local share 127.0.0.1, so the probe is right to fire) =="
./target/debug/porthole open http://localhost:8888
sleep 0.5
echo "==   service sanity check: $(curl -s http://localhost:8888/) =="

echo "== 3. drop the session: client spools =="
kill "$ATTACH"
ATTACH=""
# Wait for sshd to tear down the dead session's forward. Racing this is
# how you connect to a stale socket file.
for _ in $(seq 1 30); do [ ! -S "$T/.porthole.sock" ] && break; sleep 0.1; done
./target/debug/porthole open https://spooled.example.com
echo "   spool contents: $(cat "$T/.porthole.spool")"

echo "== 4. re-attach (StreamLocalBindUnlink replaces the stale socket) =="
ssh -N -R "$T/.porthole.sock:$T/.porthole.d/remote1.sock" remote1 2>"$T/reattach.log" &
ATTACH=$!
ok=""
for _ in $(seq 1 50); do [ -S "$T/.porthole.sock" ] && ok=1 && break; sleep 0.1; done
[ -n "$ok" ] || { echo "FAIL: re-attach"; cat "$T/reattach.log"; exit 1; }
# Presence is not readiness: probe until the new socket accepts.
ok=""
for _ in $(seq 1 20); do
    if python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])' "$T/.porthole.sock" 2>/dev/null; then
        ok=1; break
    fi
    sleep 0.2
done
[ -n "$ok" ] || { echo "FAIL: re-attached socket never accepted"; exit 1; }
./target/debug/porthole open https://flush.example.com
sleep 0.5

echo "== opened.log (expect: e2e, localhost:8888, spooled, flush — in order) =="
cat "$T/opened.log"
echo "== daemon log =="
cat "$T/daemon.log"
echo "== e2e smoke test done =="
