//! porthole — open URLs from remote machines in the local browser.
//! One binary, four roles: daemon, open, status, tunnel. A symlink
//! named `open` dispatches straight to the client (busybox-style).

use std::borrow::Cow;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;

use url::Url;

#[cfg(feature = "daemon")]
mod daemon;
#[cfg(feature = "daemon")]
mod tunnel;

#[cfg(feature = "daemon")]
const HELP: &str = "\
porthole — open URLs from remote machines in the local browser

usage: porthole <command> [<args>]

commands:
  daemon    run the local daemon (owned by launchd)
  open      send a URL to the daemon, spool it if unreachable
  status    show daemon and tunnel state
  tunnel    inspect or kill tunnels

porthole <command> --help shows command-specific usage.
";

#[cfg(not(feature = "daemon"))]
const HELP: &str = "\
porthole — open URLs from remote machines in the local browser

usage: porthole open <url>

This build carries the remote client only. The daemon, status, and
tunnel roles exist in the macOS build.
";

fn main() -> ExitCode {
    let mut args = env::args();
    let invoked_as = args
        .next()
        .and_then(|a| {
            Path::new(&a)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    // Multicall: the nix module symlinks `open` to this binary on remotes,
    // so macOS-style callers and $BROWSER work without a wrapper script.
    if invoked_as == "open" {
        return open(args);
    }

    let Some(command) = args.next() else {
        eprint!("{HELP}");
        return ExitCode::from(2);
    };

    match command.as_str() {
        "open" => open(args),
        #[cfg(feature = "daemon")]
        "daemon" => daemon::run(args),
        #[cfg(feature = "daemon")]
        "status" => daemon::status(args),
        #[cfg(feature = "daemon")]
        "tunnel" => tunnel(args),
        "-h" | "--help" | "help" => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("porthole: unknown command '{command}'");
            eprint!("{HELP}");
            ExitCode::from(2)
        }
    }
}

/// The wire protocol: one JSON object per line. Cow borrows from the
/// input buffer when the string needs no unescaping and owns it only
/// when it does — the common case allocates nothing on either side.
#[derive(serde::Serialize, serde::Deserialize)]
struct Request<'a> {
    #[serde(borrow)]
    url: Cow<'a, str>,
}

fn open(args: impl Iterator<Item = String>) -> ExitCode {
    let mut args = args;
    // Exactly one positional argument.
    let (Some(url), None) = (args.next(), args.next()) else {
        eprintln!("usage: porthole open <url>");
        return ExitCode::from(2);
    };

    // Cheap client-side gate so typos fail loudly instead of spooling.
    // The daemon remains the real validator; it trusts nothing.
    let scheme_ok = Url::parse(&url)
        .map(|u| matches!(u.scheme(), "http" | "https"))
        .unwrap_or(false);
    if !scheme_ok {
        eprintln!("porthole open: only http and https URLs are accepted");
        return ExitCode::from(2);
    }

    let Some(home) = env::home_dir() else {
        eprintln!("porthole open: cannot determine home directory");
        return ExitCode::FAILURE;
    };
    let sock = home.join(".porthole.sock");
    let spool = home.join(".porthole.spool");

    match open_inner(&sock, &spool, &url) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("porthole open: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Deliberately blocking std I/O: one connect and a few writes, no
/// concurrency, in a short-lived process. Tokio earns its keep in the
/// daemon, not here.
fn open_inner(sock: &Path, spool: &Path, url: &str) -> io::Result<()> {
    match UnixStream::connect(sock) {
        Ok(mut stream) => {
            flush_spool(&mut stream, spool)?;
            write_request(&mut stream, url)
        }
        Err(e) if unreachable(&e) => append_spool(spool, url),
        Err(e) => Err(e),
    }
}

/// A missing socket file (NotFound) or a stale one nobody listens on
/// (ConnectionRefused) both mean "no ssh session up right now".
fn unreachable(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// The spool stores serialized requests verbatim, one JSON line per URL,
/// so flushing is a single write of the file contents. Ordering is FIFO:
/// spooled URLs reach the browser before the current one.
fn flush_spool(stream: &mut UnixStream, spool: &Path) -> io::Result<()> {
    let contents = match fs::read(spool) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if contents.is_empty() {
        return Ok(());
    }
    // If this fails partway, the spool is left in place, so a retry can
    // resend lines the daemon already saw. A duplicate opens a tab twice;
    // silence would be worse.
    stream.write_all(&contents)?;
    fs::remove_file(spool)
}

fn append_spool(spool: &Path, url: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(spool)?;
    write_request(&mut file, url)?;
    eprintln!("porthole open: daemon unreachable, URL spooled");
    Ok(())
}

/// Generic over Write: the socket and the spool file share one code path.
fn write_request(w: &mut impl Write, url: &str) -> io::Result<()> {
    let line = serde_json::to_string(&Request {
        url: Cow::Borrowed(url),
    })
    .expect("serializing a string field cannot fail");
    writeln!(w, "{line}")
}

#[cfg(feature = "daemon")]
fn tunnel(_args: impl Iterator<Item = String>) -> ExitCode {
    eprintln!("porthole tunnel: not implemented yet");
    ExitCode::FAILURE
}
