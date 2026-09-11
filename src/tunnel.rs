//! The tunnel registry: one ssh port forward per host:port.
//!
//! Two mechanisms, one policy:
//!
//! - **Mux.** When the host's ssh session multiplexes — the home-manager
//!   module sets `ControlPath ~/.porthole.d/control/%n`, so the socket
//!   name is the host alias the daemon already knows — a forward is one
//!   synchronous `ssh -S <sock> -O forward -L <port>:localhost:<port>`
//!   round trip to the live master. Nothing to spawn, supervise, or
//!   reap; the forward dies with the user's connection, which is exactly
//!   the right lifetime.
//! - **Child.** Without a control socket, spawn `ssh -N -L` and reap it
//!   when it exits. Covers hand-rolled sessions with no ControlMaster.
//!
//! Policy:
//! - First tunnel wins the local port. A later request for the same
//!   port from a different host is a clear error.
//! - A local process already listening on the port wins outright: the
//!   URL opens without a tunnel and the decision is logged.
//! - Callback forwards (ports sniffed from OAuth-style query params)
//!   have a lifecycle: a connection rides the forward exactly once
//!   (the redirect), so the reaper cancels the forward once that
//!   connection has come and gone — or when no connection ever lands.
//!   A fixed callback port (AWS SSO et al.) squatted for days blocks
//!   local tools from ever binding it. Dev forwards (the URL's own
//!   loopback host) are open-ended use and are never reaped.
//! - Daemon shutdown kills child tunnels but leaves mux forwards: they
//!   belong to the user's ssh connection, not to the daemon.

use std::env;
use std::fs;
use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

/// How long to wait for a fresh tunnel to bind its local port. Also the
/// age an entry must reach before ensure() trusts-but-verifies it with
/// a probe: younger entries are still binding and must not be probed
/// into an early grave.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// A mux forward is one local round trip to the master; anything past
/// this means the master is wedged, not slow.
const MUX_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the reaper polls connection state for sniffed forwards.
const REAP_POLL: Duration = Duration::from_secs(30);

/// A sniffed forward that carried a connection and has been quiet this
/// long is reaped. Too long only delays the reap; too short kills a
/// multi-burst auth flow mid-redirect, and that flow has no self-heal
/// (the redirect lands in the browser, not through porthole).
const REAP_QUIET: Duration = Duration::from_secs(5 * 60);

/// A sniffed forward that no connection ever rode and no request
/// touched in this long: an abandoned flow. Generous on purpose —
/// this is also the MFA patience window before the redirect's first
/// connection.
const REAP_UNUSED: Duration = Duration::from_secs(30 * 60);

/// Ceiling for one lsof round trip; a wedged lsof must not stall
/// reaping forever.
const LSOF_TIMEOUT: Duration = Duration::from_secs(2);

/// `PORTHOLE_REAP_SCALE` is the test seam for the reaper: every timing
/// above multiplied by it. The smoke rig runs 200x faster (0.005) so
/// reaping is assertable in seconds. Absent or invalid → 1.0.
fn reap_scale() -> f64 {
    env::var("PORTHOLE_REAP_SCALE")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|&s| s.is_finite() && s > 0.0)
        .unwrap_or(1.0)
}

fn scaled(base: Duration, scale: f64) -> Duration {
    Duration::from_secs_f64(base.as_secs_f64() * scale)
}

#[derive(Clone)]
pub struct Registry {
    // A std Mutex, never held across an .await: on a current-thread
    // runtime another task can run at every await point, and blocking
    // the thread on a held guard would deadlock it. Mux reservation
    // below exists precisely so the process spawn/round trip happens
    // outside the lock.
    inner: Arc<Mutex<Table>>,
}

enum Kind {
    /// Spawned `ssh -N -L`; the flag is the shutdown/reap cancel switch.
    Child(Arc<AtomicBool>),
    /// `-O forward` on the host's control socket; owned by the master.
    Mux,
}

/// Why a forward exists. Scopes the reaper: Sniff entries get a
/// lifecycle, Dev entries live for the connection.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// The URL's own loopback host — a dev server the user is actively
    /// clicking at, open-ended use. Never reaped.
    Dev,
    /// A callback port sniffed from a query param — an OAuth-style
    /// flow that uses the forward once (the redirect) and never again.
    Sniff,
}

/// SoA, not a map: ports, hosts, kinds, and birth times in parallel
/// Vecs at the same index. The scan only touches the u16 port array —
/// host bytes stay cold unless a port matches. Entry count is the
/// number of live tunnels: single digits, ever, so a linear scan beats
/// hashing.
#[derive(Default)]
struct Table {
    // Invariant: all seven Vecs have the same length, same order.
    ports: Vec<u16>,
    hosts: Vec<String>,
    kinds: Vec<Kind>,
    /// Creation time: staleness probe in ensure(), identity token for
    /// the reaper. Never bumped.
    born: Vec<Instant>,
    purposes: Vec<Purpose>,
    /// Last time any request asked for this port (ensure() bumps it).
    last_requested: Vec<Instant>,
    /// Some(t): a connection rode this forward, last seen at t. None:
    /// never seen, or a fresh sniff restarted the clock.
    last_connected: Vec<Option<Instant>>,
}

impl Table {
    fn find(&self, port: u16) -> Option<usize> {
        self.ports.iter().position(|p| *p == port)
    }

    fn push(&mut self, host: &str, port: u16, kind: Kind, purpose: Purpose) {
        self.ports.push(port);
        self.hosts.push(host.to_string());
        self.kinds.push(kind);
        self.born.push(Instant::now());
        self.purposes.push(purpose);
        self.last_requested.push(Instant::now());
        self.last_connected.push(None);
    }

    /// A request for an existing entry: refresh recency and reconcile
    /// purpose. A direct URL request promotes a sniffed entry to Dev:
    /// the port is now a dev server with open-ended use. A sniff
    /// request resets the connection clock: a new flow is starting,
    /// and the previous flow's use predicts nothing about this one.
    fn touch(&mut self, idx: usize, purpose: Purpose) {
        self.last_requested[idx] = Instant::now();
        match purpose {
            Purpose::Dev => self.purposes[idx] = Purpose::Dev,
            Purpose::Sniff => self.last_connected[idx] = None,
        }
    }

    fn remove_at(&mut self, idx: usize) {
        // Order is meaningless, so removal is O(1): move the last
        // entry into the gap.
        self.ports.swap_remove(idx);
        self.hosts.swap_remove(idx);
        self.kinds.swap_remove(idx);
        self.born.swap_remove(idx);
        self.purposes.swap_remove(idx);
        self.last_requested.swap_remove(idx);
        self.last_connected.swap_remove(idx);
    }
}

pub enum Ensure {
    /// A tunnel for this host:port already exists (or a racing task
    /// just created it). The caller should still wait for readiness.
    Ready,
    /// We established the forward just now.
    Started,
    /// A local process already listens on the port; open directly.
    LocalWins,
    /// The port is already tunneled to a different host.
    Conflict(String),
}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Table::default())),
        }
    }

    /// Ensure a tunnel from `host` to local `port`, creating one if the
    /// policy allows. `purpose` says why: Dev forwards live for the
    /// connection, Sniff forwards until the reaper decides the flow is
    /// done.
    pub async fn ensure(&self, host: &str, port: u16, purpose: Purpose) -> io::Result<Ensure> {
        if let Some((state, born)) = self.lookup(host, port, purpose) {
            match state {
                Ensure::Ready => {
                    // An entry is a claim, not proof: mux forwards die
                    // with their ssh connection and nobody tells us.
                    // Probe entries old enough to have bound; recreate
                    // the dead ones.
                    if born.elapsed() >= READY_TIMEOUT && !local_listening(port).await {
                        eprintln!(
                            "porthole daemon: {host}: tunnel on port {port} is stale, recreating"
                        );
                        self.remove(port);
                    } else {
                        return Ok(Ensure::Ready);
                    }
                }
                other => return Ok(other),
            }
        }

        // No tunnel of ours on this port. If something local is already
        // listening, local wins and no tunnel is created.
        if local_listening(port).await {
            return Ok(Ensure::LocalWins);
        }

        // Mux fast path. Reserve the entry under the lock, then await
        // the ssh round trip outside it; a loser of the reservation
        // race sees Ready on its own re-check.
        if let Some(socket) = control_socket_for(host) {
            {
                let mut table = self.inner.lock().expect("registry mutex poisoned");
                if let Some(state) = Self::check(&mut table, host, port, purpose) {
                    return Ok(state);
                }
                table.push(host, port, Kind::Mux, purpose);
            }
            match mux_forward(&socket, host, port).await {
                Ok(()) => {
                    eprintln!(
                        "porthole daemon: {host}: forwarded -L {port}:localhost:{port} via {}",
                        socket.display()
                    );
                    return Ok(Ensure::Started);
                }
                Err(e) => {
                    eprintln!(
                        "porthole daemon: {host}: mux forward for port {port} failed ({e}), falling back to a child tunnel"
                    );
                    self.remove(port);
                }
            }
        }

        // Child path: spawn is a syscall, so check + spawn + register
        // stays atomic under the lock without holding it across an
        // await.
        let mut table = self.inner.lock().expect("registry mutex poisoned");
        if let Some(state) = Self::check(&mut table, host, port, purpose) {
            return Ok(state);
        }

        let mut child = Command::new("ssh")
            .arg("-N")
            .arg("-L")
            .arg(format!("{port}:localhost:{port}"))
            // Exit loudly instead of living on with a failed local bind.
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg(host)
            .spawn()?;

        eprintln!(
            "porthole daemon: {host}: spawned tunnel: ssh -N -L {port}:localhost:{port} {host}"
        );
        let cancel = Arc::new(AtomicBool::new(false));
        table.push(host, port, Kind::Child(cancel.clone()), purpose);

        // Reaper: poll the child every 50ms; whichever happens first —
        // the ssh child dies (entry drops, next request recreates it on
        // demand) or the cancel flag is set (shutdown). A poll loop
        // instead of select! because the tokio macros feature is
        // deliberately off.
        let registry = self.clone();
        tokio::spawn(async move {
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        eprintln!("porthole daemon: tunnel on port {port} exited ({status})");
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("porthole daemon: tunnel on port {port}: wait failed: {e}");
                        break;
                    }
                }
                if cancel.load(Ordering::Relaxed) {
                    let _ = child.kill().await;
                    eprintln!(
                        "porthole daemon: tunnel on port {port} canceled (shutdown or idle reap)"
                    );
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
            registry.remove(port);
        });

        Ok(Ensure::Started)
    }

    /// Kill every child tunnel on shutdown. Mux forwards are left
    /// running: they belong to the user's ssh connection, and a daemon
    /// restart must not tear down a live session's forwards.
    pub fn cancel_all(&self) {
        let mut table = self.inner.lock().expect("registry mutex poisoned");
        // Clearing the tables makes the reapers' remove() calls no-ops.
        table.ports.clear();
        table.hosts.clear();
        table.born.clear();
        table.purposes.clear();
        table.last_requested.clear();
        table.last_connected.clear();
        let mut mux = 0;
        for kind in mem::take(&mut table.kinds) {
            match kind {
                Kind::Child(cancel) => cancel.store(true, Ordering::Relaxed),
                Kind::Mux => mux += 1,
            }
        }
        if mux > 0 {
            eprintln!(
                "porthole daemon: leaving {mux} mux forwards in place (owned by their ssh connections)"
            );
        }
    }

    fn check(table: &mut Table, host: &str, port: u16, purpose: Purpose) -> Option<Ensure> {
        let idx = table.find(port)?;
        if table.hosts[idx] == host {
            table.touch(idx, purpose);
            Some(Ensure::Ready)
        } else {
            Some(Ensure::Conflict(table.hosts[idx].clone()))
        }
    }

    fn lookup(&self, host: &str, port: u16, purpose: Purpose) -> Option<(Ensure, Instant)> {
        let mut table = self.inner.lock().expect("registry mutex poisoned");
        let idx = table.find(port)?;
        let state = if table.hosts[idx] == host {
            table.touch(idx, purpose);
            Ensure::Ready
        } else {
            Ensure::Conflict(table.hosts[idx].clone())
        };
        Some((state, table.born[idx]))
    }

    /// Start the callback reaper: one task that gives sniffed forwards
    /// the lifecycle described in the module docs. Runs until the
    /// daemon exits (shutdown's process::exit takes it down with
    /// everything else) or until lsof is missing, which disables
    /// reaping rather than risk killing forwards whose connections it
    /// cannot see.
    pub fn spawn_reaper(&self) {
        let registry = self.clone();
        tokio::spawn(async move {
            let scale = reap_scale();
            let poll = scaled(REAP_POLL, scale);
            let quiet = scaled(REAP_QUIET, scale);
            let unused = scaled(REAP_UNUSED, scale);
            loop {
                sleep(poll).await;
                if !registry.sweep(quiet, unused).await {
                    return;
                }
            }
        });
    }

    /// One reaper pass. Returns false when reaping must stop (no
    /// lsof). Three phases, and the mutex is only ever held for memory
    /// work: snapshot under the lock, observe (lsof, ssh round trips)
    /// outside it, write back and act.
    async fn sweep(&self, quiet: Duration, unused: Duration) -> bool {
        // Phase 1: snapshot the sniffed entries.
        let snaps: Vec<SweepEntry> = {
            let table = self.inner.lock().expect("registry mutex poisoned");
            (0..table.ports.len())
                .filter(|&i| table.purposes[i] == Purpose::Sniff)
                .map(|i| SweepEntry {
                    port: table.ports[i],
                    host: table.hosts[i].clone(),
                    born: table.born[i],
                    child: match &table.kinds[i] {
                        Kind::Child(c) => Some(c.clone()),
                        Kind::Mux => None,
                    },
                })
                .collect()
        };
        if snaps.is_empty() {
            return true;
        }

        // Phase 2: connection state, outside the lock.
        let mut connected = Vec::with_capacity(snaps.len());
        for snap in &snaps {
            match established_on_port(snap.port).await {
                Ok(c) => connected.push(c),
                // No lsof: without connection sight the unused timer
                // would kill forwards in active use. Disable reaping.
                Err(e) => {
                    eprintln!("porthole daemon: lsof unavailable ({e}), callback reaping disabled");
                    return false;
                }
            }
        }

        // Phase 3: write back sightings, decide condemnations. A
        // snapshot whose entry vanished or was replaced mid-sweep (a
        // request recreated it) is skipped — born is the identity token.
        let mut condemned = Vec::new();
        {
            let mut table = self.inner.lock().expect("registry mutex poisoned");
            for (snap, &connected) in snaps.iter().zip(&connected) {
                let Some(idx) = table.find(snap.port) else {
                    continue;
                };
                if table.born[idx] != snap.born {
                    continue;
                }
                if connected {
                    table.last_connected[idx] = Some(Instant::now());
                    continue;
                }
                match table.last_connected[idx] {
                    Some(t) if t.elapsed() >= quiet => condemned.push(snap.clone()),
                    None if table.last_requested[idx].elapsed() >= unused => {
                        condemned.push(snap.clone())
                    }
                    _ => {}
                }
            }
        }

        // Phase 4: tear down, outside the lock. Children are flagged,
        // not killed here — their own reaper polls the flag, kills the
        // ssh child, and drops the entry. Mux forwards get one
        // `-O cancel` round trip, then the entry goes; a cancel failure
        // (master gone) still drops the entry, since the forward died
        // with its master and a lingering claim would squat the port.
        for snap in condemned {
            match snap.child {
                Some(cancel) => {
                    cancel.store(true, Ordering::Relaxed);
                    eprintln!(
                        "porthole daemon: reaped callback tunnel on port {} (child)",
                        snap.port
                    );
                }
                None => {
                    if let Some(socket) = control_socket_for(&snap.host)
                        && let Err(e) = mux_cancel(&socket, &snap.host, snap.port).await
                    {
                        eprintln!(
                            "porthole daemon: cancel for reaped port {} failed: {e}",
                            snap.port
                        );
                    }
                    self.remove_if(snap.port, snap.born);
                    eprintln!(
                        "porthole daemon: reaped callback tunnel on port {} (mux)",
                        snap.port
                    );
                }
            }
        }
        true
    }

    fn remove(&self, port: u16) {
        let mut table = self.inner.lock().expect("registry mutex poisoned");
        if let Some(idx) = table.find(port) {
            table.remove_at(idx);
        }
    }

    /// Remove by port, but only the entry we condemned — born is the
    /// identity token. A request that recreated the entry while the
    /// cancel round trip was in flight wins and keeps its tunnel.
    fn remove_if(&self, port: u16, born: Instant) {
        let mut table = self.inner.lock().expect("registry mutex poisoned");
        if let Some(idx) = table.find(port)
            && table.born[idx] == born
        {
            table.remove_at(idx);
        }
    }
}

/// One sniffed entry as the reaper sees it between lock scopes. Also
/// the condemnation record: everything phase 4 needs to tear it down.
/// The lifecycle clocks (last_connected, last_requested) are
/// deliberately NOT snapshotted: phase 3 reads them from the live
/// table, so a request that touched the entry mid-sweep wins.
#[derive(Clone)]
struct SweepEntry {
    port: u16,
    host: String,
    /// Identity token: creation time. Guards against acting on an
    /// entry that was removed and recreated mid-sweep.
    born: Instant,
    /// Some: the entry is a child tunnel and this is its cancel flag.
    child: Option<Arc<AtomicBool>>,
}

/// The ssh control socket directory: one socket per host, owned by
/// the connection supervisor (`~/.porthole.d/master/<host>`). This is
/// separate from the user's interactive-session ControlPath
/// (`~/.porthole.d/control/%n`), so herdr, Zed, and terminal sessions
/// keep their own multiplexing without interference.
pub fn control_dir() -> Option<PathBuf> {
    env::home_dir().map(|home| home.join(".porthole.d").join("master"))
}

/// The host's control socket, only when it is a live master. A socket
/// file that exists but nobody is listening on (stale master from a
/// dead connection) is unlinked so it never clogs the mux path or child
/// tunnels with "ControlSocket already exists, disabling multiplexing."
fn control_socket_for(host: &str) -> Option<PathBuf> {
    let path = control_dir()?.join(host);
    match fs::metadata(&path) {
        Ok(meta) if meta.file_type().is_socket() => {
            // A socket file exists, but the master might be dead. A
            // stale file makes every mux forward fail (the round trip
            // to the dead master times out) and blocks child tunnels
            // from creating a new master. Probe with a connect:
            // ConnectionRefused means nobody is listening.
            if StdUnixStream::connect(&path).is_ok() {
                Some(path)
            } else {
                eprintln!(
                    "porthole daemon: {host}: stale control socket {}, removing",
                    path.display()
                );
                let _ = fs::remove_file(&path);
                None
            }
        }
        _ => None,
    }
}

/// One synchronous round trip to the ssh master: by the time the client
/// exits, the forward is bound or the attempt has failed. No readiness
/// polling needed — the ack is the master's bind result.
async fn mux_forward(socket: &Path, host: &str, port: u16) -> io::Result<()> {
    let output = timeout(
        MUX_TIMEOUT,
        Command::new("ssh")
            .arg("-S")
            .arg(socket)
            .arg("-O")
            .arg("forward")
            .arg("-L")
            .arg(format!("{port}:localhost:{port}"))
            .arg(host)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "ssh -O forward timed out"))??;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ssh -O forward exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

/// Tear down a mux forward: the mirror of `mux_forward`, one round
/// trip to the master. A failure here still warrants dropping the
/// entry: if the master is gone the forward died with it, and a
/// lingering claim would squat the local port.
async fn mux_cancel(socket: &Path, host: &str, port: u16) -> io::Result<()> {
    let output = timeout(
        MUX_TIMEOUT,
        Command::new("ssh")
            .arg("-S")
            .arg(socket)
            .arg("-O")
            .arg("cancel")
            .arg("-L")
            .arg(format!("{port}:localhost:{port}"))
            .arg(host)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "ssh -O cancel timed out"))??;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ssh -O cancel exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

/// Is a connection riding the forward right now? lsof lists both
/// endpoints of every connection touching the port, so the filter
/// matters: only sockets whose LOCAL side is the forwarded port count
/// as use — the daemon's own readiness probes (and any port scanner)
/// appear with the port on their remote side.
async fn established_on_port(port: u16) -> io::Result<bool> {
    let output = timeout(
        LSOF_TIMEOUT,
        Command::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:ESTABLISHED"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "lsof timed out"))??;
    // Exit 1 (no matches) is the common idle answer; parse stdout only.
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| parse_lsof_local_port(line) == Some(port)))
}

/// The local port of an lsof ESTABLISHED line:
/// `ssh 123 matt 5u IPv4 0x… 0t0 TCP 127.0.0.1:8888->127.0.0.1:49152 (ESTABLISHED)`
/// `ssh 123 matt 6u IPv6 0x… 0t0 TCP6 [::1]:8888->[::1]:49152 (ESTABLISHED)`
/// "TCP6" is the IPv6 protocol column; the established socket may be
/// a client's, whose remote side is this port (the daemon's own probe).
fn parse_lsof_local_port(line: &str) -> Option<u16> {
    let mut tokens = line.split_whitespace();
    tokens.find(|t| *t == "TCP" || *t == "TCP6")?;
    let name = tokens.next()?;
    let (local, _peer) = name.split_once("->")?;
    local
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
}

/// Something is listening on 127.0.0.1:port.
async fn local_listening(port: u16) -> bool {
    TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .is_ok()
}

/// Poll until the tunnel's local port accepts connections. Connecting to
/// a forwarded port opens one throwaway channel to the remote service —
/// harmless, and the only honest readiness signal ssh gives us.
pub async fn wait_ready(port: u16) -> bool {
    let start = Instant::now();
    loop {
        if local_listening(port).await {
            return true;
        }
        if start.elapsed() >= READY_TIMEOUT {
            return false;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::parse_lsof_local_port;

    #[test]
    fn lsof_local_side_is_the_forward_port() {
        // Genuine use: the master's accepted socket, local side 8888.
        let traffic = "ssh  123  matt  5u  IPv4  0xabc  0t0  TCP 127.0.0.1:8888->127.0.0.1:49152 (ESTABLISHED)";
        assert_eq!(parse_lsof_local_port(traffic), Some(8888));

        // IPv6: TCP6 protocol column, bracketed local address.
        let v6 = "ssh  123  matt  6u  IPv6  0xabd  0t0  TCP6 [::1]:8888->[::1]:49152 (ESTABLISHED)";
        assert_eq!(parse_lsof_local_port(v6), Some(8888));

        // The daemon's own readiness probe: local side is the ephemeral
        // port, the forward port sits on the remote side. established_
        // on_port(8888) must not count this line as use.
        let probe = "porthole  999  matt  8u  IPv4  0xabe  0t0  TCP 127.0.0.1:49152->127.0.0.1:8888 (ESTABLISHED)";
        assert_eq!(parse_lsof_local_port(probe), Some(49152));

        // Header and LISTEN lines never parse.
        assert_eq!(
            parse_lsof_local_port("COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME"),
            None
        );
    }
}
