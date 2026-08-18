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
- SSH carries the socket through a `RemoteForward` from your Mac.
- The daemon on macOS validates the URL, manages tunnels, and calls `open(1)`.
- Tunnels ride the session's own multiplexed SSH connection: the home-manager
  module sets `ControlMaster auto` and `ControlPath ~/.porthole.d/control/%n`
  per host, so a tunnel is one `ssh -O forward` round trip — no extra ssh
  processes, and forwards die with the connection. Sessions without a control
  socket get a spawned `ssh -N -L` instead.
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
};
```

The module runs the daemon under launchd and declares one ssh `RemoteForward`
per host.

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
- a plain `open <url>` shim for macOS-style callers
- `porthole open <url>` directly
- **Ctrl+click on a loopback URL in a herdr pane** — the home-manager
  module links a plugin manifest that routes loopback URLs to
  `porthole open` instead of a dead browser tab. This is automatic
  when herdr is installed; without herdr, every other entry point
  still works.

The clipboard bridges too: `cat foo.txt | pbcopy` puts stdin on the Mac's
clipboard. Attached to a terminal this speaks OSC 52 directly and needs
nothing else running. Detached, it goes through the daemon. Clipboard
writes are never spooled — a late paste would clobber the current
clipboard with stale content.

Note: on a headless remote, xdg-open consults the MIME database only when
a display is present (`has_display` in xdg-open). The reliable route there
is `$BROWSER`. home-manager sets it, but only if your shell sources
home-manager's session variables file.

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
