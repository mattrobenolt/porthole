//! porthole — open URLs from remote machines in the local browser.
//! One binary, five roles: daemon, open, clipboard, status, tunnel.
//! Symlinks named `open` or `pbcopy` dispatch to the client, busybox-style.

use std::borrow::Cow;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Read, Write};
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
  clipboard  set the local clipboard from stdin (also: pbcopy)
  daemon     run the local daemon (owned by launchd)
  open       send a URL to the daemon, spool it if unreachable
  status     show daemon and tunnel state
  tunnel     inspect or kill tunnels

porthole <command> --help shows command-specific usage.
";

#[cfg(not(feature = "daemon"))]
const HELP: &str = "\
porthole — open URLs from remote machines in the local browser

usage:
  porthole open <url>   send a URL to the local browser
  porthole clipboard    set the local clipboard from stdin

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

    // Multicall: the nix module symlinks `open` and `pbcopy` to this
    // binary on remotes, so macOS-style callers and $BROWSER work
    // without wrapper scripts.
    match invoked_as.as_str() {
        "open" => return open(args),
        "pbcopy" => return clipboard(args),
        _ => {}
    }

    let Some(command) = args.next() else {
        eprint!("{HELP}");
        return ExitCode::from(2);
    };

    match command.as_str() {
        "open" => open(args),
        "clipboard" => clipboard(args),
        #[cfg(feature = "daemon")]
        "daemon" => daemon::run(args),
        #[cfg(feature = "daemon")]
        "status" => daemon::status(args),
        #[cfg(feature = "daemon")]
        "tunnel" => daemon::tunnel(args),
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

/// The wire protocol: one JSON object per line, untagged — the key is
/// the verb. Existing spooled lines ({"url": ...}) parse as Open, so
/// the format is backward compatible. Cow borrows from the input buffer
/// when the string needs no unescaping and owns it only when it does.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum Request<'a> {
    Open {
        #[serde(borrow)]
        url: Cow<'a, str>,
    },
    Clipboard {
        #[serde(borrow)]
        clipboard: Cow<'a, str>,
    },
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
            write_request(
                &mut stream,
                &Request::Open {
                    url: Cow::Borrowed(url),
                },
            )
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
    write_request(
        &mut file,
        &Request::Open {
            url: Cow::Borrowed(url),
        },
    )?;
    eprintln!("porthole open: daemon unreachable, URL spooled");
    Ok(())
}

/// Generic over Write: the socket and the spool file share one code path.
fn write_request(w: &mut impl Write, request: &Request) -> io::Result<()> {
    let line = serde_json::to_string(request).expect("serializing strings cannot fail");
    writeln!(w, "{line}")
}

/// Largest clipboard payload, raw bytes. JSON escaping inflates the wire
/// line (the daemon caps lines at 64 KiB) and terminals cap OSC 52
/// payloads too, so text pastes only — this is a clipboard, not a pipe.
const CLIPBOARD_MAX: usize = 32 * 1024;

/// `porthole clipboard` (also reachable as `pbcopy`): stdin → the local
/// clipboard.
///
/// Attached to a terminal, speak OSC 52 directly: no daemon, no attach,
/// no socket — the terminal is the transport. Detached (agent, cron, a
/// pipe), go through the daemon socket.
///
/// Clipboard writes are never spooled. A late paste would clobber the
/// current clipboard with stale content; losing is the safer failure.
fn clipboard(_args: impl Iterator<Item = String>) -> ExitCode {
    let mut input = String::new();
    if let Err(e) = io::stdin()
        .take(CLIPBOARD_MAX as u64 + 1)
        .read_to_string(&mut input)
    {
        eprintln!("porthole clipboard: stdin is not valid UTF-8: {e}");
        return ExitCode::FAILURE;
    }
    if input.len() > CLIPBOARD_MAX {
        eprintln!("porthole clipboard: input exceeds {CLIPBOARD_MAX} bytes");
        return ExitCode::FAILURE;
    }

    if io::stdout().is_terminal() {
        print!("\x1b]52;c;{}\x07", base64_encode(input.as_bytes()));
        if let Err(e) = io::stdout().flush() {
            eprintln!("porthole clipboard: writing OSC 52 failed: {e}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    let Some(home) = env::home_dir() else {
        eprintln!("porthole clipboard: cannot determine home directory");
        return ExitCode::FAILURE;
    };
    let sock = home.join(".porthole.sock");
    let spool = home.join(".porthole.spool");
    match UnixStream::connect(&sock) {
        Ok(mut stream) => {
            // The transport is up; opportunistically flush the URL spool.
            let result = flush_spool(&mut stream, &spool).and_then(|()| {
                write_request(
                    &mut stream,
                    &Request::Clipboard {
                        clipboard: Cow::Owned(input),
                    },
                )
            });
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("porthole clipboard: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Err(e) if unreachable(&e) => {
            eprintln!(
                "porthole clipboard: daemon unreachable (clipboard is never spooled; needs a live attach)"
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("porthole clipboard: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Standard-alphabet base64, hand-rolled: twenty lines over a dependency.
fn base64_encode(data: &[u8]) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"hello\nworld\n"), "aGVsbG8Kd29ybGQK");
    }

    #[test]
    fn wire_format_both_verbs() {
        let open: Request = serde_json::from_str(r#"{"url":"http://localhost:8888"}"#).unwrap();
        let Request::Open { url } = open else {
            panic!("expected Open");
        };
        assert_eq!(url, "http://localhost:8888");

        let clip: Request = serde_json::from_str(r#"{"clipboard":"hello"}"#).unwrap();
        let Request::Clipboard { clipboard } = clip else {
            panic!("expected Clipboard");
        };
        assert_eq!(clipboard, "hello");
    }
}
