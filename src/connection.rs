//! Persistent SSH master connections, one per host.
//!
//! The daemon owns a `ssh -N -M` master per host on a dedicated
//! control socket at `~/.porthole.d/master/<host>`. This master
//! carries the RemoteForward (request delivery from the remote) and
//! serves as the control socket for on-demand `-L` tunnel forwards.
//!
//! When the connection dies — laptop sleep, network loss — a
//! supervisor task respawns it with exponential backoff. The daemon
//! never depends on externally-managed SSH sessions (herdr, Zed,
//! terminals); those keep their own multiplexing on a separate
//! ControlPath and do not interfere.
//!
//! On startup, any master left behind by a previous daemon (crash,
//! kill -9) is stopped via `ssh -O exit` before the new one spawns.
//! The stale control socket is also unlinked, so `-M` never meets a
//! dead file.

use std::cmp::min;
use std::env;
use std::fs;
use std::io;
use std::mem;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

/// Base delay before the first reconnect attempt.
const BACKOFF_START: Duration = Duration::from_secs(1);
/// Maximum delay between reconnect attempts.
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long `shutdown()` waits for supervisors to see the cancel flag
/// and call `ssh -O exit` before returning.
const EXIT_GRACE: Duration = Duration::from_millis(200);
/// How long to wait for a master to exit after `ssh -O exit` before
/// falling back to `kill`.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval for `ssh -O check` when the master daemonized.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Cancel-flag check interval during backoff sleep.
const CANCEL_TICK: Duration = Duration::from_millis(100);

/// The master control socket directory, separate from the user's
/// interactive-session ControlPath (`~/.porthole.d/control/%n` in the
/// generated ssh config). Herdr, Zed, and terminal sessions keep their
/// own multiplexing; porthole's master is independent and does not
/// interfere.
pub fn master_dir() -> Option<PathBuf> {
    env::home_dir().map(|home| home.join(".porthole.d").join("master"))
}

/// The host's master control socket path.
pub fn master_socket(host: &str) -> Option<PathBuf> {
    master_dir().map(|dir| dir.join(host))
}

/// One persistent SSH master per host, supervised and respawned on exit.
#[derive(Clone)]
pub struct Supervisor {
    // A std Mutex, never held across an .await: the only operation is
    // pushing cancel flags at startup and taking them at shutdown.
    cancels: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            cancels: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Start a supervised master per host. Each runs in its own task;
    /// this returns immediately.
    pub fn start(&self, hosts: Vec<String>) {
        for host in hosts {
            let cancel = Arc::new(AtomicBool::new(false));
            self.cancels
                .lock()
                .expect("supervisor mutex poisoned")
                .push(cancel.clone());
            tokio::spawn(supervise(host, cancel));
        }
    }

    /// Stop all masters: set cancel flags so the supervisors stop
    /// respawning, then give them a beat to `ssh -O exit` their
    /// masters. After this returns, `process::exit` is safe.
    pub async fn shutdown(&self) {
        let cancels = {
            let mut guard = self.cancels.lock().expect("supervisor mutex poisoned");
            mem::take(&mut *guard)
        };
        for cancel in &cancels {
            cancel.store(true, Ordering::Relaxed);
        }
        sleep(EXIT_GRACE).await;
    }
}

/// Supervise one host's master: spawn, monitor, respawn with backoff.
async fn supervise(host: String, cancel: Arc<AtomicBool>) {
    // If a previous daemon left a master running (crash, kill -9),
    // stop it. Unlinking the socket alone leaves the process alive;
    // `ssh -O exit` kills it.
    stop_existing_master(&host).await;

    let mut backoff = BACKOFF_START;
    loop {
        if cancel.load(Ordering::Relaxed) {
            eprintln!("porthole daemon: {host}: supervisor stopped");
            return;
        }

        // Unlink any stale control socket so `-M` never meets a dead
        // file. The supervisor owns this path exclusively.
        if let Some(socket) = master_socket(&host) {
            let _ = fs::remove_file(&socket);
        }

        match spawn_master(&host).await {
            Ok(child) => {
                eprintln!("porthole daemon: {host}: master connection established");
                backoff = BACKOFF_START;
                monitor_master(&host, child, &cancel).await;
            }
            Err(e) => {
                eprintln!(
                    "porthole daemon: {host}: master spawn failed ({e}), retrying in {backoff:?}"
                );
            }
        }

        if cancel.load(Ordering::Relaxed) {
            return;
        }

        if cancellable_sleep(backoff, &cancel).await {
            return;
        }
        backoff = min(backoff * 2, BACKOFF_MAX);
    }
}

/// Monitor the master child. When the child exits, decide whether to
/// respawn (connection died) or poll (ssh daemonized the master into
/// the background and the pre-fork parent exited 0).
async fn monitor_master(host: &str, mut child: Child, cancel: &AtomicBool) {
    let exit_status = loop {
        if cancel.load(Ordering::Relaxed) {
            stop_master(host).await;
            match timeout(SHUTDOWN_TIMEOUT, child.wait()).await {
                Ok(Ok(_)) => {
                    eprintln!("porthole daemon: {host}: master stopped on shutdown");
                }
                Ok(Err(e)) => {
                    eprintln!("porthole daemon: {host}: master wait on shutdown: {e}");
                }
                Err(_) => {
                    let _ = child.kill().await;
                    eprintln!(
                        "porthole daemon: {host}: master did not exit in {SHUTDOWN_TIMEOUT:?}, killed"
                    );
                }
            }
            return;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                eprintln!("porthole daemon: {host}: master wait failed: {e}");
                return;
            }
        }
        sleep(Duration::from_millis(50)).await;
    };

    // Exit 0 can mean ssh daemonized the master (ControlPersist caused
    // a background fork despite our `no`). If the master is still
    // alive, switch to polling `ssh -O check` until it dies. Any other
    // exit status is a connection error — the supervise loop respawns.
    if exit_status.success() && master_check(host).await {
        eprintln!("porthole daemon: {host}: master daemonized, polling until exit");
        poll_master(host, cancel).await;
    } else {
        eprintln!("porthole daemon: {host}: master exited ({exit_status}), will reconnect");
    }
}

/// Poll `ssh -O check` until the master dies or cancel is set. Used
/// when ssh daemonized the master and the child handle is no longer
/// meaningful.
async fn poll_master(host: &str, cancel: &AtomicBool) {
    loop {
        if cancel.load(Ordering::Relaxed) {
            stop_master(host).await;
            eprintln!("porthole daemon: {host}: master stopped on shutdown");
            return;
        }
        if !master_check(host).await {
            eprintln!("porthole daemon: {host}: master connection lost, will reconnect");
            return;
        }
        if cancellable_sleep(POLL_INTERVAL, cancel).await {
            stop_master(host).await;
            eprintln!("porthole daemon: {host}: master stopped on shutdown");
            return;
        }
    }
}

/// Check if the master is alive via `ssh -O check`.
async fn master_check(host: &str) -> bool {
    let Some(socket) = master_socket(host) else {
        return false;
    };
    match timeout(
        Duration::from_secs(5),
        Command::new("ssh")
            .arg("-S")
            .arg(&socket)
            .arg("-O")
            .arg("check")
            .arg(host)
            .output(),
    )
    .await
    {
        Ok(Ok(o)) => o.status.success(),
        _ => false,
    }
}

/// Send `ssh -O exit` to the master on the control socket, then wait
/// for the child to exit. Falls back to `kill` if the master doesn't
/// exit within `SHUTDOWN_TIMEOUT`.
async fn stop_master(host: &str) {
    let Some(socket) = master_socket(host) else {
        return;
    };
    let _ = Command::new("ssh")
        .arg("-S")
        .arg(&socket)
        .arg("-O")
        .arg("exit")
        .arg(host)
        .output()
        .await;
}

/// Stop any master left behind by a previous daemon instance. Called
/// once at supervisor startup, before the first spawn.
async fn stop_existing_master(host: &str) {
    let Some(socket) = master_socket(host) else {
        return;
    };
    if !fs::metadata(&socket)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
    {
        return;
    }
    eprintln!("porthole daemon: {host}: stopping orphaned master from previous daemon");
    stop_master(host).await;
}

/// Spawn the ssh master for one host.
///
/// `-M` makes this process the master (ControlMaster=yes).
/// `ControlPersist=no` prevents daemonization — the process stays in
/// the foreground and exits when the connection dies, so the
/// supervisor can detect the exit and respawn. `ExitOnForwardFailure`
/// makes the process exit loudly if the RemoteForward fails (stale
/// remote socket, permission error) instead of running with a dead
/// forward.
async fn spawn_master(host: &str) -> io::Result<Child> {
    let Some(home) = env::home_dir() else {
        return Err(io::Error::other("cannot determine home directory"));
    };
    let Some(socket) = master_socket(host) else {
        return Err(io::Error::other("cannot determine master socket path"));
    };
    let remote_sock = home.join(".porthole.sock");
    let local_sock = home.join(".porthole.d").join(format!("{host}.sock"));

    Command::new("ssh")
        .arg("-N")
        .arg("-M")
        .arg("-S")
        .arg(&socket)
        .arg("-o")
        .arg("ControlPersist=no")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=2")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-R")
        .arg(format!(
            "{}:{}",
            remote_sock.display(),
            local_sock.display()
        ))
        .arg(host)
        .kill_on_drop(true)
        .spawn()
}

/// Sleep for `duration`, but wake every `CANCEL_TICK` to check the
/// cancel flag. Returns `true` if cancelled, `false` if the full
/// duration elapsed. Without tokio::select! (the macros feature is
/// off), this is how we make backoff sleeps interruptible.
async fn cancellable_sleep(duration: Duration, cancel: &AtomicBool) -> bool {
    let mut elapsed = Duration::ZERO;
    while elapsed < duration {
        if cancel.load(Ordering::Relaxed) {
            return true;
        }
        sleep(CANCEL_TICK).await;
        elapsed += CANCEL_TICK;
    }
    false
}
