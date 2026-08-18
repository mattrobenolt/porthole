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
//! - Daemon shutdown kills child tunnels but leaves mux forwards: they
//!   belong to the user's ssh connection, not to the daemon.

use std::env;
use std::fs;
use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::os::unix::fs::FileTypeExt;
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
    /// Spawned `ssh -N -L`; the flag is the shutdown cancel switch.
    Child(Arc<AtomicBool>),
    /// `-O forward` on the host's control socket; owned by the master.
    Mux,
}

/// SoA, not a map: ports, hosts, kinds, and birth times in parallel
/// Vecs at the same index. The scan only touches the u16 port array —
/// host bytes stay cold unless a port matches. Entry count is the
/// number of live tunnels: single digits, ever, so a linear scan beats
/// hashing.
#[derive(Default)]
struct Table {
    // Invariant: all four Vecs have the same length, same order.
    ports: Vec<u16>,
    hosts: Vec<String>,
    kinds: Vec<Kind>,
    born: Vec<Instant>,
}

impl Table {
    fn find(&self, port: u16) -> Option<usize> {
        self.ports.iter().position(|p| *p == port)
    }

    fn push(&mut self, host: &str, port: u16, kind: Kind) {
        self.ports.push(port);
        self.hosts.push(host.to_string());
        self.kinds.push(kind);
        self.born.push(Instant::now());
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
    /// policy allows.
    pub async fn ensure(&self, host: &str, port: u16) -> io::Result<Ensure> {
        if let Some((state, born)) = self.lookup(host, port) {
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
                if let Some(state) = Self::check(&table, host, port) {
                    return Ok(state);
                }
                table.push(host, port, Kind::Mux);
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
        if let Some(state) = Self::check(&table, host, port) {
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
        table.push(host, port, Kind::Child(cancel.clone()));

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
                    eprintln!("porthole daemon: tunnel on port {port} killed on shutdown");
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

    fn check(table: &Table, host: &str, port: u16) -> Option<Ensure> {
        table.find(port).map(|idx| {
            let existing = &table.hosts[idx];
            if existing == host {
                Ensure::Ready
            } else {
                Ensure::Conflict(existing.clone())
            }
        })
    }

    fn lookup(&self, host: &str, port: u16) -> Option<(Ensure, Instant)> {
        let table = self.inner.lock().expect("registry mutex poisoned");
        let idx = table.find(port)?;
        let state = if table.hosts[idx] == host {
            Ensure::Ready
        } else {
            Ensure::Conflict(table.hosts[idx].clone())
        };
        Some((state, table.born[idx]))
    }

    fn remove(&self, port: u16) {
        let mut table = self.inner.lock().expect("registry mutex poisoned");
        if let Some(idx) = table.find(port) {
            // Order is meaningless, so removal is O(1): move the last
            // entry into the gap.
            table.ports.swap_remove(idx);
            table.hosts.swap_remove(idx);
            table.kinds.swap_remove(idx);
            table.born.swap_remove(idx);
        }
    }
}

/// The ssh control socket directory: one socket per host, named by the
/// alias (`ControlPath ~/.porthole.d/control/%n` in the generated ssh
/// config), so host → socket is a path join, not a lookup table.
pub fn control_dir() -> Option<PathBuf> {
    env::home_dir().map(|home| home.join(".porthole.d").join("control"))
}

/// The host's control socket, only when it is a live-shaped socket
/// file. A stale or absent file falls through to the child path.
fn control_socket_for(host: &str) -> Option<PathBuf> {
    let path = control_dir()?.join(host);
    match fs::metadata(&path) {
        Ok(meta) if meta.file_type().is_socket() => Some(path),
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
