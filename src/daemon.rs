//! The daemon role: one Unix socket listener per remote host.
//!
//! The listener's file name IS the host identity — no metadata, no
//! mapping tables. The ssh RemoteForward on each remote points at the
//! matching `~/.porthole.d/<host>.sock` on this machine.

use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
    // ControlPath points into this directory; ssh creates the socket
    // file but never its parents.
    fs::create_dir_all(dir.join("control"))?;

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
    match request {
        Request::Open { url } => handle_open(host, &url, registry).await,
        Request::Clipboard { clipboard } => set_clipboard(host, &clipboard).await,
    }
}

async fn handle_open(host: &str, url: &str, registry: &Registry) {
    // OAuth-style flows: the authorize URL names the loopback callback
    // in a query param, and the browser is redirected there without
    // ever asking us. This is the only chance to tunnel it. Best-effort:
    // a failure here must not keep the authorize page from loading.
    for port in sniff_callback_ports(url) {
        match registry.ensure(host, port).await {
            Ok(Ensure::Started) => {
                eprintln!("porthole daemon: {host}: pre-tunneled callback port {port}")
            }
            Ok(Ensure::Ready) | Ok(Ensure::LocalWins) => {}
            Ok(Ensure::Conflict(other)) => eprintln!(
                "porthole daemon: {host}: callback port {port} already tunneled to '{other}'"
            ),
            Err(e) => {
                eprintln!(
                    "porthole daemon: {host}: pre-tunnel for callback port {port} failed: {e}"
                )
            }
        }
    }
    match classify(url) {
        Ok(Target::Direct) => open_and_log(host, url).await,
        Ok(Target::Tunnel(port)) => match registry.ensure(host, port).await {
            Ok(Ensure::Ready) | Ok(Ensure::Started) => {
                if tunnel::wait_ready(port).await {
                    open_and_log(host, url).await;
                } else {
                    eprintln!(
                        "porthole daemon: {host}: tunnel on port {port} did not become ready, not opening {url}"
                    );
                }
            }
            Ok(Ensure::LocalWins) => {
                eprintln!(
                    "porthole daemon: {host}: port {port} already bound locally, opening {url} without a tunnel (local wins)"
                );
                open_and_log(host, url).await;
            }
            Ok(Ensure::Conflict(other)) => {
                eprintln!(
                    "porthole daemon: {host}: rejected {url}: port {port} is already tunneled to '{other}' (first tunnel wins)"
                );
            }
            Err(e) => {
                eprintln!("porthole daemon: {host}: failed to spawn tunnel for port {port}: {e}");
            }
        },
        Err(reason) => {
            eprintln!("porthole daemon: {host}: rejected {url}: {reason}");
        }
    }
}

/// PORTHOLE_PBCOPY is the same kind of test seam as PORTHOLE_OPENER:
/// on macOS this is /usr/bin/pbcopy; under test it captures to a file.
async fn set_clipboard(host: &str, text: &str) {
    let tool = env::var("PORTHOLE_PBCOPY").unwrap_or_else(|_| "/usr/bin/pbcopy".to_string());
    let spawned = Command::new(tool).stdin(process::Stdio::piped()).spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            eprintln!("porthole daemon: {host}: clipboard tool failed to spawn: {e}");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(text.as_bytes()).await {
            eprintln!("porthole daemon: {host}: writing to clipboard tool: {e}");
        }
        drop(stdin);
    }
    match child.wait().await {
        Ok(s) if s.success() => {
            eprintln!(
                "porthole daemon: {host}: clipboard set ({} bytes)",
                text.len()
            );
        }
        Ok(s) => eprintln!("porthole daemon: {host}: clipboard tool exited with {s}"),
        Err(e) => eprintln!("porthole daemon: {host}: waiting for clipboard tool: {e}"),
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

/// Loopback ports named by any query param value that parses as a
/// loopback URL. OAuth redirect_uri is the case that matters: the
/// browser is redirected to the callback without passing through us,
/// so the authorize URL's query is the only place the port is visible.
/// Name-agnostic (redirect_uri, callback, next, ...) — a spurious
/// tunnel costs one ssh round trip, a missed one strands the flow.
fn sniff_callback_ports(raw: &str) -> Vec<u16> {
    let Ok(url) = Url::parse(raw) else {
        return Vec::new();
    };
    let mut ports = Vec::new();
    for (_, value) in url.query_pairs() {
        if let Ok(Target::Tunnel(port)) = classify(&value)
            && !ports.contains(&port)
        {
            ports.push(port);
        }
    }
    ports
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
    let muxes = find_mux();
    let mut any = false;
    for t in &tunnels {
        any = true;
        println!("  port {} → {} (child pid {})", t.port, t.host, t.pid);
    }
    for m in &muxes {
        for port in &m.ports {
            any = true;
            println!("  port {} → {} (mux, master pid {})", port, m.host, m.pid);
        }
    }
    if !any {
        println!("  (none)");
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
            // Child tunnels: signal the ssh child directly; the daemon's
            // reaper drops the registry entry when it exits.
            let tunnels = find_tunnels();
            if let Some(t) = tunnels.iter().find(|t| t.port == port) {
                let status = process::Command::new("kill")
                    .arg(t.pid.to_string())
                    .status();
                return match status {
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
                };
            }
            // Mux forwards live inside the ssh master; cancel through
            // its control socket.
            if let Some(dir) = tunnel::control_dir() {
                for m in find_mux() {
                    if !m.ports.contains(&port) {
                        continue;
                    }
                    let status = process::Command::new("ssh")
                        .arg("-S")
                        .arg(dir.join(&m.host))
                        .arg("-O")
                        .arg("cancel")
                        .arg("-L")
                        .arg(format!("{port}:localhost:{port}"))
                        .arg(&m.host)
                        .status();
                    return match status {
                        Ok(s) if s.success() => {
                            println!(
                                "canceled mux forward on port {port} (→ {}, master pid {})",
                                m.host, m.pid
                            );
                            ExitCode::SUCCESS
                        }
                        Ok(s) => {
                            eprintln!("porthole tunnel: ssh -O cancel exited with {s}");
                            ExitCode::FAILURE
                        }
                        Err(e) => {
                            eprintln!("porthole tunnel: failed to run ssh -O cancel: {e}");
                            ExitCode::FAILURE
                        }
                    };
                }
            }
            eprintln!("porthole tunnel: no tunnel on port {port}");
            ExitCode::FAILURE
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

struct MuxInfo {
    host: String,
    pid: u32,
    ports: Vec<u16>,
}

/// Mux forwards live inside the user's ssh master, invisible to the
/// process-table shape find_tunnels matches. Ground truth is the
/// control socket directory (one socket per host, named by alias) plus
/// lsof on the master's pid — the same "the process table is the shared
/// state" philosophy, one level up. User-added `-L` forwards on the
/// same master are indistinguishable from ours and are listed too.
fn find_mux() -> Vec<MuxInfo> {
    let Some(dir) = tunnel::control_dir() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let is_socket = entry.file_type().map(|t| t.is_socket()).unwrap_or(false);
        if !is_socket {
            continue;
        }
        let host = entry.file_name().to_string_lossy().into_owned();
        let check = process::Command::new("ssh")
            .arg("-S")
            .arg(entry.path())
            .arg("-O")
            .arg("check")
            .arg(&host)
            .output();
        let Ok(check) = check else { continue };
        if !check.status.success() {
            continue;
        }
        // "Master running (pid=...)" goes to stderr; be liberal about it.
        let text = String::from_utf8_lossy(&check.stderr).into_owned()
            + &String::from_utf8_lossy(&check.stdout);
        let Some(pid) = parse_master_pid(&text) else {
            continue;
        };
        let ports = lsof_listen_ports(pid);
        out.push(MuxInfo { host, pid, ports });
    }
    out
}

/// "Master running (pid=12345)" — the payload of `ssh -O check`.
fn parse_master_pid(text: &str) -> Option<u32> {
    let start = text.find("pid=")? + 4;
    let end = text[start..].find(')')? + start;
    text[start..end].parse().ok()
}

/// Loopback TCP listen ports owned by pid, via lsof. The master's
/// listening sockets are exactly its local forwards.
fn lsof_listen_ports(pid: u32) -> Vec<u16> {
    let Ok(out) = process::Command::new("lsof")
        .args(["-nP", "-a", "-p", &pid.to_string(), "-iTCP", "-sTCP:LISTEN"])
        .output()
    else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.lines().filter_map(parse_lsof_listen).collect()
}

/// One listening loopback port from an lsof output line, or None.
/// Lines look like:
///   ssh  1234  matt  9u  IPv4  0x...  0t0  TCP 127.0.0.1:3000 (LISTEN)
///   ssh  1234  matt  9u  IPv6  0x...  0t0  TCP [::1]:3000 (LISTEN)
fn parse_lsof_listen(line: &str) -> Option<u16> {
    let line = line.trim();
    if !line.ends_with("(LISTEN)") {
        return None;
    }
    let mut tokens = line.split_whitespace();
    let addr = tokens.find(|t| *t == "TCP").and_then(|_| tokens.next())?;
    let (host, port) = addr.rsplit_once(':')?;
    if !(host == "127.0.0.1" || host == "[::1]" || host == "localhost") {
        return None;
    }
    port.parse().ok()
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
    use super::{parse_lsof_listen, parse_master_pid, parse_tunnel, sniff_callback_ports};

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

    #[test]
    fn sniff_finds_callback_ports() {
        // Azure CLI authorize URL: RFC 8252 loopback redirect_uri.
        assert_eq!(
            sniff_callback_ports(
                "https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize?client_id=xxx&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A38947&scope=https%3A%2F%2Fmanagement.core.windows.net%2F%2F.default+offline_access&state=xxx&code_challenge=xxx&code_challenge_method=S256"
            ),
            vec![38947]
        );
        // gcloud authorize URL.
        assert_eq!(
            sniff_callback_ports(
                "https://accounts.google.com/o/oauth2/auth?response_type=code&client_id=x.apps.googleusercontent.com&redirect_uri=http%3A%2F%2Flocalhost%3A8085%2F&scope=openid&state=xxx&access_type=offline&code_challenge=xxx&code_challenge_method=S256"
            ),
            vec![8085]
        );
        // Name-agnostic: any param value that is a loopback URL counts.
        assert_eq!(
            sniff_callback_ports(
                "https://accounts.google.com/o/oauth2?callback=http%3A%2F%2F127.0.0.1%3A12345%2Fcb"
            ),
            vec![12345]
        );
        // IPv6 loopback, percent-encoded brackets and all.
        assert_eq!(
            sniff_callback_ports(
                "https://idp.example.com/auth?redirect_uri=http%3A%2F%2F%5B%3A%3A1%5D%3A9999%2Fcb"
            ),
            vec![9999]
        );
        // Distinct loopback params all tunnel, first-seen order, deduped.
        assert_eq!(
            sniff_callback_ports(
                "https://example.com/?a=http%3A%2F%2Flocalhost%3A9999&b=http%3A%2F%2Flocalhost%3A8888&c=http%3A%2F%2Flocalhost%3A9999"
            ),
            vec![9999, 8888]
        );
    }

    #[test]
    fn sniff_ignores_non_loopback_and_junk() {
        // Hosted redirect: nothing to tunnel.
        assert!(
            sniff_callback_ports(
                "https://example.com/?redirect_uri=https%3A%2F%2Fexample.com%2Fcallback"
            )
            .is_empty()
        );
        // Relative values are not URLs.
        assert!(sniff_callback_ports("https://example.com/?next=%2Flocal%2Fpath").is_empty());
        // Private-use schemes fail validation; out of scope.
        assert!(
            sniff_callback_ports("https://example.com/?redirect_uri=com.example.app%3A%2Fcb")
                .is_empty()
        );
        assert!(sniff_callback_ports("https://example.com/no-query").is_empty());
        assert!(sniff_callback_ports("not a url").is_empty());
        assert!(sniff_callback_ports("").is_empty());
    }

    #[test]
    fn master_pid_parses() {
        assert_eq!(
            parse_master_pid("Master running (pid=12345)\r\n"),
            Some(12345)
        );
        assert_eq!(parse_master_pid("Master running (pid=7)"), Some(7));
        assert_eq!(parse_master_pid("No master"), None);
        assert_eq!(parse_master_pid("pid=abc)"), None);
    }

    #[test]
    fn lsof_listen_parses() {
        assert_eq!(
            parse_lsof_listen("sshd 1234 matt 9u IPv4 0x1 0t0 TCP 127.0.0.1:3000 (LISTEN)"),
            Some(3000)
        );
        assert_eq!(
            parse_lsof_listen("sshd 1234 matt 9u IPv6 0x2 0t0 TCP [::1]:8085 (LISTEN)"),
            Some(8085)
        );
        // Wildcard and non-loopback binds are not porthole forwards.
        assert_eq!(
            parse_lsof_listen("sshd 1234 matt 9u IPv4 0x1 0t0 TCP *:3000 (LISTEN)"),
            None
        );
        assert_eq!(
            parse_lsof_listen("sshd 1234 matt 9u IPv4 0x1 0t0 TCP 10.0.0.2:3000 (LISTEN)"),
            None
        );
        // Non-listen lines and the header never match.
        assert_eq!(
            parse_lsof_listen(
                "sshd 1234 matt 3u IPv4 0x3 0t0 TCP 127.0.0.1:22->127.0.0.1:55555 (ESTABLISHED)"
            ),
            None
        );
        assert_eq!(
            parse_lsof_listen("COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME"),
            None
        );
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
