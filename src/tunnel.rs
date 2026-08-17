//! The tunnel registry: one `ssh -N -L` child per host:port.
//!
//! Policy:
//! - First tunnel wins the local port. A later request for the same
//!   port from a different host is a clear error.
//! - A local process already listening on the port wins outright: the
//!   URL opens without a tunnel and the decision is logged.
//! - Tunnels are children of the daemon. When one dies, its registry
//!   entry is reaped so the next request recreates it on demand.

use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::{Instant, sleep};

/// How long to wait for a fresh tunnel to bind its local port.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Registry {
    // A std Mutex, never held across an .await: on a current-thread
    // runtime no other task can run while a guard is live, so the lock
    // is uncontended by construction.
    inner: Arc<Mutex<Table>>,
}

/// SoA, not a map: ports, hosts, and cancel switches in parallel Vecs
/// at the same index. The scan only touches the u16 port array — host
/// bytes stay cold unless a port matches. Entry count is the number of
/// live tunnels: single digits, ever, so a linear scan beats hashing.
#[derive(Default)]
struct Table {
    // Invariant: all three Vecs have the same length, same order.
    ports: Vec<u16>,
    hosts: Vec<String>,
    cancels: Vec<Arc<AtomicBool>>,
}

impl Table {
    fn find(&self, port: u16) -> Option<usize> {
        self.ports.iter().position(|p| *p == port)
    }
}

pub enum Ensure {
    /// A tunnel for this host:port already exists (or a racing task
    /// just created it). The caller should still wait for readiness.
    Ready,
    /// We spawned the ssh child just now.
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
    /// policy allows. The two lock sections are synchronous and never
    /// span an .await; Command::spawn is a syscall, so check + spawn +
    /// register stays atomic.
    pub async fn ensure(&self, host: &str, port: u16) -> io::Result<Ensure> {
        if let Some(state) = self.lookup(host, port) {
            return Ok(state);
        }

        // No tunnel of ours on this port. If something local is already
        // listening, local wins and no tunnel is created.
        if local_listening(port).await {
            return Ok(Ensure::LocalWins);
        }

        // Re-check under the lock: another task may have won the race
        // while we were probing.
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
        table.ports.push(port);
        table.hosts.push(host.to_string());
        table.cancels.push(cancel.clone());

        // Reaper: poll the child every 50ms; whichever happens first —
        // the ssh child dies (entry drops, next request recreates it on
        // demand) or the cancel flag is set (shutdown / a future
        // `tunnel kill` verb). A poll loop instead of select! because
        // the tokio macros feature is deliberately off.
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

    /// Kill every tunnel child. Used on graceful shutdown; also the
    /// mechanism a `tunnel kill` admin verb will use.
    pub fn cancel_all(&self) {
        let mut table = self.inner.lock().expect("registry mutex poisoned");
        // Clearing the tables makes the reapers' remove() calls no-ops.
        table.ports.clear();
        table.hosts.clear();
        for cancel in mem::take(&mut table.cancels) {
            cancel.store(true, Ordering::Relaxed);
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

    fn lookup(&self, host: &str, port: u16) -> Option<Ensure> {
        let table = self.inner.lock().expect("registry mutex poisoned");
        Self::check(&table, host, port)
    }

    fn remove(&self, port: u16) {
        let mut table = self.inner.lock().expect("registry mutex poisoned");
        if let Some(idx) = table.find(port) {
            // Order is meaningless, so removal is O(1): move the last
            // entry into the gap.
            table.ports.swap_remove(idx);
            table.hosts.swap_remove(idx);
            table.cancels.swap_remove(idx);
        }
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
