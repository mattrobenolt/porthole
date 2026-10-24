# porthole

porthole opens URLs from remote machines in your local browser. A tool on a
NixOS remote calls `xdg-open http://localhost:8888`. The URL arrives in the
default browser on your Mac.

If the URL points at a loopback port on the remote, porthole first creates an
SSH tunnel for that port. The browser then reaches the remote service through
the tunnel. You do nothing.

OAuth flows work too. An authorize URL carries its loopback callback in the
query string (`redirect_uri` and friends), and the browser is redirected
there without porthole ever seeing that URL. The daemon sniffs the callback
port out of every URL it opens and pre-tunnels it, so the identity provider's
redirect back to localhost lands on a live tunnel.

## How it works

- One binary serves every role: client, daemon, and admin tool.
- The remote runs no daemon. The client writes one JSON line to a Unix socket.
- The daemon owns a persistent SSH master per host (via launchd): it carries the
  `RemoteForward` for request delivery and serves as the control socket for
  tunnel forwards. When the connection dies (laptop sleep, network loss), the
  supervisor auto-reconnects with backoff.
- The daemon on macOS validates the URL, manages tunnels, and calls `open(1)`.
- Tunnels ride the supervisor's multiplexed SSH connection: a tunnel is one
  `ssh -O forward` round trip on the master's control socket — no extra ssh
  processes, and forwards die with the connection. When the master is down
  (reconnecting), a spawned `ssh -N -L` child provides the forward instead.
- OAuth callback tunnels don't linger. The reaper watches them with lsof
  and tears one down once the flow's redirect has ridden it and gone quiet
  (or the flow never lands at all), so a fixed callback port — AWS SSO's is
  fixed — is never squatted for days, blocking local tools from binding it.
- If no SSH session is up, the client spools the URL to disk. The next
  successful call flushes the spool, oldest first.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full design.

## Install

Everything is a Nix flake. Both sides come from this repository.

### The macOS side (home-manager)

```nix
imports = [ inputs.porthole.homeModules.porthole-daemon ];

programs.porthole = {
  enable = true;
  hosts = [ "dev1" "launchpad" ];  # must match your ssh Host aliases
  # zedCommand = "/Applications/Zed.app/Contents/MacOS/cli";
};
```

The module runs the daemon under launchd. It probes the usual Zed CLI
locations; set `zedCommand` or the daemon's `PORTHOLE_ZED` environment
variable for a non-standard install. The daemon owns its SSH master
connections — you no longer need an interactive SSH session for porthole to
work. Your existing `ssh`, `herdr --remote`, and Zed sessions are unaffected
(they keep their own multiplexing).

### The remote side (NixOS home-manager)

```nix
imports = [ inputs.porthole.homeModules.porthole-remote ];

programs.porthole.enable = true;
```

The module installs the client, registers it as the MIME handler for HTTP and
HTTPS, and sets `$BROWSER`. Also set this in the NixOS configuration of each
remote:

```nix
services.openssh.settings.StreamLocalBindUnlink = true;
```

## Usage

On the remote, nothing changes. Every entry point routes to the client:

- `xdg-open <url>`, which honors `$BROWSER`
- tools that read `$BROWSER` directly
- `gio open <url>` through the registered MIME handler
- a plain `open` shim for macOS-style callers. `open <url>` reaches the
  browser. `open <path>` opens the same path in its default Mac
  application when that path exists on the Mac. Nothing is transferred:
  porthole relies on your trees matching, which they do for synced homes.
  A path with no local counterpart fails with a clear error and is never
  spooled. The daemon checks before it opens anything.
- `porthole open <url>` directly
- `zed .` (or `zed <path>`) opens that path on the remote in the local
  Zed over SSH. The remote module installs a `zed` shim by default;
  set `programs.porthole.zedShim = false` if the remote has its own
  `zed` binary. The daemon uses the ssh Host alias porthole already
  knows for that remote, so the URL matches your existing ssh config.
- **Ctrl+click on a loopback URL in a herdr pane** — the home-manager
  module links a plugin manifest that routes loopback URLs to
  `porthole open` instead of a dead browser tab. This is automatic
  when herdr is installed; without herdr, every other entry point
  still works.

The clipboard bridges both ways. `cat foo.txt | pbcopy` puts stdin on
the Mac's clipboard; `pbpaste` prints the Mac's clipboard back on the
remote — one request line over the same socket, one reply line back.
The binary also answers to `lemonade` — the one clipboard provider Neovim
probes for with no display-server gate — so on remotes every Neovim yank
and paste rides the Mac's clipboard with zero editor configuration.
Copy speaks OSC 52 directly when attached to a terminal and needs nothing
else running; detached copies and every paste go through the daemon.
Clipboard traffic is never spooled — a late write would clobber the
current clipboard with stale content. Note the trust direction: paste
lets anything running as you on a remote read the Mac's clipboard, and
the socket only exists behind your own SSH masters, so this is exactly
as trustworthy as the remotes you point at it.

Note: some tools refuse to open a browser when no display is present.
xdg-open consults the MIME database only under `has_display`, and gcloud
(`check_browser.py`) skips the browser entirely unless `DISPLAY`,
`WAYLAND_DISPLAY`, or `MIR_SOCKET` is set — it never reads `$BROWSER`.
The module sets `DISPLAY=:0` to pass these gates; the browser they reach
is porthole. Your shell must source home-manager's session variables file
for any of this to take effect.

On the Mac, `porthole status` shows the listening sockets and live tunnels.
`ph` is a short symlink for the impatient.

## Development

`nix develop` enters the dev shell. The system tests are shell rigs, not
cargo tests:

- `scripts/smoke.sh` — client, daemon, junk flood, stale socket reclaim
- `scripts/smoke-tunnel.sh` — tunnel policy, status, graceful shutdown
- `scripts/smoke-e2e.sh` — the full system over real SSH on one box

The remote build ships without the daemon and never compiles tokio:

```
nix build .#porthole-remote
```

The `daemon` cargo feature gates the daemon, status, and tunnel subcommands
plus the tokio dependency.

## License

MIT. See [LICENSE](LICENSE).
