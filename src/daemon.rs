//! The daemon role: one Unix socket listener per remote host.
//!
//! The listener's file name IS the host identity — no metadata, no
//! mapping tables. The ssh RemoteForward on each remote points at the
//! matching `~/.porthole.d/<host>.sock` on this machine.

use std::env;
use std::fs;
use std::io;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::runtime::Builder;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::sleep;
use url::Url;

use crate::Request;
use crate::tunnel::{self, Ensure, Registry};

/// Entry point for `porthole daemon <host>...`.
///
/// The host list arrives on argv: in production the launchd plist that
/// home-manager generates carries it, so there is no config file format
/// to design. The runtime is built by hand instead of via `#[tokio::main]`
/// so its construction stays visible: one OS thread, one I/O driver,
/// every task polled on this thread.
pub fn run(args: impl Iterator<Item = String>) -> ExitCode {
    let hosts: Vec<String> = args.collect();
    if hosts.is_empty() {
        eprintln!("usage: porthole daemon <host>...");
        return ExitCode::from(2);
    }

    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    match runtime.block_on(serve(hosts)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("porthole daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(hosts: Vec<String>) -> io::Result<()> {
    let Some(home) = env::home_dir() else {
        return Err(io::Error::other("cannot determine home directory"));
    };
    let dir = home.join(".porthole.d");
    fs::create_dir_all(&dir)?;

    let registry = Registry::new();
    let mut paths = Vec::new();
    let mut tasks = Vec::new();
    for host in hosts {
        let path = dir.join(format!("{host}.sock"));
        let listener = bind_listener(&path)?;
        eprintln!("porthole daemon: {host}: listening on {}", path.display());
        paths.push(path);
        tasks.push(tokio::spawn(accept_loop(listener, host, registry.clone())));
    }

    // Shutdown watchers, one per signal. (No tokio::select! here: the
    // macros feature is deliberately off, so each signal gets its own
    // tiny task.)
    spawn_shutdown_watcher(
        SignalKind::terminate(),
        "SIGTERM",
        registry.clone(),
        paths.clone(),
    );
    spawn_shutdown_watcher(SignalKind::interrupt(), "SIGINT", registry.clone(), paths);

    // Accept loops never return Ok; the first task to finish is the
    // first failure, and its error tears the daemon down.
    for task in tasks {
        task.await??;
    }
    Ok(())
}

/// On the watched signal, kill tunnel children, unlink our sockets so a
/// restart never meets a stale file, then exit. process::exit skips
/// destructors — the cleanup above is the destructor.
fn spawn_shutdown_watcher(
    kind: SignalKind,
    name: &'static str,
    registry: Registry,
    paths: Vec<PathBuf>,
) {
    tokio::spawn(async move {
        let mut sig = match signal(kind) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("porthole daemon: cannot watch {name}: {e}");
                return;
            }
        };
        if sig.recv().await.is_none() {
            return;
        }
        eprintln!("porthole daemon: {name}, shutting down");
        registry.cancel_all();
        for path in &paths {
            if let Err(e) = fs::remove_file(path) {
                eprintln!("porthole daemon: removing {}: {e}", path.display());
            }
        }
        // Give the reaper tasks a beat to actually kill the ssh children.
        sleep(Duration::from_millis(100)).await;
        process::exit(0);
    });
}

/// Bind the per-host socket, reclaiming it when a previous daemon died
/// and left the file behind. A socket file that refuses connections is
/// stale; one that accepts them belongs to a live daemon, which is an
/// error worth saying out loud.
fn bind_listener(path: &Path) -> io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => match StdUnixStream::connect(path) {
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{}: another daemon holds this socket", path.display()),
            )),
            Err(_) => {
                eprintln!(
                    "porthole daemon: {}: reclaiming stale socket",
                    path.display()
                );
                fs::remove_file(path)?;
                UnixListener::bind(path)
            }
        },
        Err(e) => Err(e),
    }
}

async fn accept_loop(listener: UnixListener, host: String, registry: Registry) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        // Fire-and-forget: on a current-thread runtime a spawned task is
        // a state machine on the same thread — concurrency, not
        // parallelism.
        tokio::spawn(handle_conn(stream, host.clone(), registry.clone()));
    }
}

/// One line is a URL with JSON around it; anything near 64 KiB is junk.
const MAX_LINE: usize = 64 * 1024;

async fn handle_conn(stream: UnixStream, host: String, registry: Registry) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        // take() bounds a single line: junk without a newline grows the
        // buffer to the cap and no further, then the connection is cut.
        // (A post-read length check would not work: read_line never
        // returns until it sees a newline.)
        let read = (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_line(&mut line)
            .await;
        match read {
            Ok(0) => return, // clean EOF: client closed the connection
            Ok(_) => {
                let content = line.strip_suffix('\n').unwrap_or(&line);
                if content.len() > MAX_LINE {
                    eprintln!(
                        "porthole daemon: {host}: line over {MAX_LINE} bytes, closing connection"
                    );
                    return;
                }
                handle_line(&host, content, &registry).await;
            }
            Err(e) => {
                eprintln!("porthole daemon: {host}: read error: {e}");
                return;
            }
        }
    }
}

async fn handle_line(host: &str, line: &str, registry: &Registry) {
    let request = match serde_json::from_str::<Request>(line) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("porthole daemon: {host}: malformed request: {e}");
            return;
        }
    };
    match classify(&request.url) {
        Ok(Target::Direct) => open_and_log(host, &request.url).await,
        Ok(Target::Tunnel(port)) => match registry.ensure(host, port).await {
            Ok(Ensure::Ready) | Ok(Ensure::Started) => {
                if tunnel::wait_ready(port).await {
                    open_and_log(host, &request.url).await;
                } else {
                    eprintln!(
                        "porthole daemon: {host}: tunnel on port {port} did not become ready, not opening {}",
                        request.url
                    );
                }
            }
            Ok(Ensure::LocalWins) => {
                eprintln!(
                    "porthole daemon: {host}: port {port} already bound locally, opening {} without a tunnel (local wins)",
                    request.url
                );
                open_and_log(host, &request.url).await;
            }
            Ok(Ensure::Conflict(other)) => {
                eprintln!(
                    "porthole daemon: {host}: rejected {}: port {port} is already tunneled to '{other}' (first tunnel wins)",
                    request.url
                );
            }
            Err(e) => {
                eprintln!("porthole daemon: {host}: failed to spawn tunnel for port {port}: {e}");
            }
        },
        Err(reason) => {
            eprintln!(
                "porthole daemon: {host}: rejected {}: {reason}",
                request.url
            );
        }
    }
}

async fn open_and_log(host: &str, url: &str) {
    eprintln!("porthole daemon: {host}: open {url}");
    if let Err(e) = open_url(url).await {
        eprintln!("porthole daemon: {host}: opener failed: {e}");
    }
}

enum Target {
    Direct,
    Tunnel(u16),
}

/// The daemon is the real validator; the client's scheme check is only a
/// courtesy for fast feedback. Everything arriving on the socket is
/// untrusted input.
fn classify(raw: &str) -> Result<Target, String> {
    let url = Url::parse(raw).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("scheme '{}' is not http/https", url.scheme()));
    }
    let loopback = match url.host() {
        Some(url::Host::Domain(d)) => d.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => return Err("URL has no host".to_string()),
    };
    if loopback {
        // port_or_known_default: http:// implies 80, https:// implies 443.
        let port = url
            .port_or_known_default()
            .ok_or_else(|| "cannot determine port".to_string())?;
        Ok(Target::Tunnel(port))
    } else {
        Ok(Target::Direct)
    }
}

/// `porthole status`: report the daemon's world from the outside. There
/// is no query channel yet, so the honest sources are the socket
/// directory (probe live/stale by connecting) and the process table
/// (our tunnel children match a rigid command-line shape).
pub fn status(_args: impl Iterator<Item = String>) -> ExitCode {
    let Some(home) = env::home_dir() else {
        eprintln!("porthole status: cannot determine home directory");
        return ExitCode::FAILURE;
    };
    let dir = home.join(".porthole.d");

    println!("sockets:");
    match fs::read_dir(&dir) {
        Ok(entries) => {
            let mut any = false;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                    continue;
                }
                any = true;
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let state = match StdUnixStream::connect(&path) {
                    Ok(_) => "live",
                    Err(_) => "stale",
                };
                println!("  {name}: {state}");
            }
            if !any {
                println!("  (none)");
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => println!("  (none)"),
        Err(e) => {
            eprintln!("porthole status: cannot read {}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    }

    println!("tunnels:");
    let tunnels = find_tunnels();
    if tunnels.is_empty() {
        println!("  (none)");
    }
    for t in &tunnels {
        println!("  port {} → {} (pid {})", t.port, t.host, t.pid);
    }

    ExitCode::SUCCESS
}

/// `porthole tunnel [list] | kill <port>`.
///
/// No admin channel is needed: the process table is the shared state.
/// Listing scans for tunnel children; killing signals the ssh child
/// directly, and the daemon's reaper drops the registry entry when the
/// child exits. The system converges through process death.
pub fn tunnel(args: impl Iterator<Item = String>) -> ExitCode {
    let mut args = args;
    match args.next().as_deref() {
        None | Some("list") => {
            let tunnels = find_tunnels();
            if tunnels.is_empty() {
                println!("no tunnels");
            }
            for t in &tunnels {
                println!("port {} → {} (pid {})", t.port, t.host, t.pid);
            }
            ExitCode::SUCCESS
        }
        Some("kill") => {
            let Some(port) = args.next().and_then(|p| p.parse::<u16>().ok()) else {
                eprintln!("usage: porthole tunnel kill <port>");
                return ExitCode::from(2);
            };
            let tunnels = find_tunnels();
            let Some(t) = tunnels.iter().find(|t| t.port == port) else {
                eprintln!("porthole tunnel: no tunnel on port {port}");
                return ExitCode::FAILURE;
            };
            let status = process::Command::new("kill")
                .arg(t.pid.to_string())
                .status();
            match status {
                Ok(s) if s.success() => {
                    println!(
                        "killed tunnel on port {port} (pid {}, was → {})",
                        t.pid, t.host
                    );
                    ExitCode::SUCCESS
                }
                Ok(s) => {
                    eprintln!("porthole tunnel: kill exited with {s}");
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("porthole tunnel: failed to run kill: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(other) => {
            eprintln!("porthole tunnel: unknown subcommand '{other}'");
            eprintln!("usage: porthole tunnel [list] | kill <port>");
            ExitCode::from(2)
        }
    }
}

struct TunnelInfo {
    pid: u32,
    port: u16,
    host: String,
}

/// Scan the process table for our tunnel children. Anything that is not
/// a strict match for the daemon's spawn shape is invisible — a false
/// negative means the tunnel admin commands miss it, never that we
/// touch somebody else's ssh.
fn find_tunnels() -> Vec<TunnelInfo> {
    let Ok(out) = process::Command::new("ps")
        .args(["-eo", "pid=,args="])
        .output()
    else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let (port, host) = parse_tunnel(&tokens)?;
            Some(TunnelInfo {
                pid: tokens[0].parse().ok()?,
                port,
                host: host.to_string(),
            })
        })
        .collect()
}

/// Recognize one of our tunnel children in a ps line's tokens:
/// `ssh -N -L <port>:localhost:<port> -o ExitOnForwardFailure=yes <host>`
fn parse_tunnel<'a>(tokens: &[&'a str]) -> Option<(u16, &'a str)> {
    let (_, args) = tokens.split_first()?;
    let program = args.first()?;
    if !program.ends_with("ssh") || !args.contains(&"-N") {
        return None;
    }
    let l = args.iter().position(|t| *t == "-L")?;
    let spec = args.get(l + 1)?;
    let host = args.last()?;
    let (local, rest) = spec.split_once(':')?;
    let (localhost, remote) = rest.rsplit_once(':')?;
    if localhost != "localhost" {
        return None;
    }
    let local: u16 = local.parse().ok()?;
    let remote: u16 = remote.parse().ok()?;
    (local == remote).then_some((local, *host))
}

#[cfg(test)]
mod tests {
    use super::parse_tunnel;

    #[test]
    fn parses_our_tunnel() {
        let tokens = [
            "1234",
            "ssh",
            "-N",
            "-L",
            "3000:localhost:3000",
            "-o",
            "ExitOnForwardFailure=yes",
            "dev1",
        ];
        assert_eq!(parse_tunnel(&tokens), Some((3000, "dev1")));
    }

    #[test]
    fn rejects_mismatched_or_non_localhost_specs() {
        assert_eq!(
            parse_tunnel(&["1", "ssh", "-N", "-L", "3000:localhost:3001", "dev1"]),
            None
        );
        assert_eq!(
            parse_tunnel(&["1", "ssh", "-N", "-L", "3000:example.com:3000", "dev1"]),
            None
        );
    }

    #[test]
    fn rejects_non_tunnel_processes() {
        assert_eq!(parse_tunnel(&["1", "sshd", "-D"]), None);
        assert_eq!(parse_tunnel(&["1", "ssh", "dev1", "echo", "hi"]), None);
        assert_eq!(parse_tunnel(&["1", "nc", "-k", "-l", "3000"]), None);
    }
}

/// PORTHOLE_OPENER is a test seam: on macOS this is /usr/bin/open; under
/// test it is a script that logs instead of launching a browser.
async fn open_url(url: &str) -> io::Result<()> {
    let opener = env::var("PORTHOLE_OPENER").unwrap_or_else(|_| "/usr/bin/open".to_string());
    let status = Command::new(opener).arg(url).status().await?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("opener exited with {status}")))
    }
}
