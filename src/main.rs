//! porthole — open URLs from remote machines in the local browser.
//! One binary, eight roles: daemon, open, herdr-open, zed, clipboard,
//! paste, status, tunnel. Symlinks named `open`, `pbcopy`, `pbpaste`,
//! `lemonade`, or `zed` dispatch to the client, busybox-style.

use std::borrow::Cow;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::iter::once;
use std::os::unix::net::UnixStream;
use std::path::{Path, absolute};
use std::process::ExitCode;
use std::time::Duration;

use url::Url;

#[cfg(feature = "daemon")]
mod connection;
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
  herdr-open open a URL from a herdr link handler (reads HERDR_PLUGIN_CLICKED_URL)
  open       send a URL to the daemon, spool it if unreachable
  paste      print the local clipboard to stdout (also: pbpaste)
  status     show daemon and tunnel state
  tunnel     inspect or kill tunnels
  zed        open a remote path in the local Zed over SSH

porthole <command> --help shows command-specific usage.
";

#[cfg(not(feature = "daemon"))]
const HELP: &str = "\
porthole — open URLs from remote machines in the local browser

usage:
  porthole open <url>         send a URL to the local browser
  porthole herdr-open <url>   open a URL from a herdr link handler
  porthole clipboard          set the local clipboard from stdin
  porthole paste              print the local clipboard to stdout
  porthole zed [<path>]       open a path on this remote in the local Zed

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

    // Multicall: the nix module symlinks `open`, `pbcopy`, `pbpaste`,
    // and `lemonade` to this binary on remotes, so macOS-style callers,
    // $BROWSER, both clipboard directions, and Neovim's clipboard
    // probe work without wrapper scripts.
    match invoked_as.as_str() {
        "open" => return open(args),
        "pbcopy" => return clipboard(args),
        "pbpaste" => return paste(args),
        "lemonade" => return lemonade(args),
        "zed" => return zed(args),
        _ => {}
    }

    let Some(command) = args.next() else {
        eprint!("{HELP}");
        return ExitCode::from(2);
    };

    match command.as_str() {
        "open" => open(args),
        "clipboard" => clipboard(args),
        "paste" => paste(args),
        "herdr-open" => herdr_open(args),
        "zed" => zed(args),
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
    Paste {
        paste: bool,
    },
    Zed {
        #[serde(borrow)]
        zed: ZedTarget<'a>,
    },
}

/// The remote half of a `zed` request: a path on this machine and the
/// account ssh is logged in as. The daemon supplies the host alias from
/// the socket name, so the remote never guesses how the Mac addresses it.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ZedTarget<'a> {
    #[serde(borrow)]
    pub(crate) path: Cow<'a, str>,
    #[serde(borrow)]
    pub(crate) user: Cow<'a, str>,
}

/// The daemon's reply, one JSON line on the same connection, for the
/// requests that need one: Paste and Zed. The write requests (Open,
/// Clipboard) stay fire-and-forget with no ack.
/// Paste success carries the clipboard, Zed success carries `ok`, and
/// failure carries a reason, so the client fails fast on the daemon's
/// own terms instead of timing out.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum Response<'a> {
    Clipboard {
        #[serde(borrow)]
        clipboard: Cow<'a, str>,
    },
    Error {
        #[serde(borrow)]
        error: Cow<'a, str>,
    },
    Ok {
        ok: bool,
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

/// `porthole herdr-open`: the herdr plugin entry point. herdr passes
/// the clicked URL in `HERDR_PLUGIN_CLICKED_URL`. A positional arg is
/// accepted as a fallback for manual testing outside herdr. Delegates
/// to `open` — same validation, same socket, same spool.
fn herdr_open(mut args: impl Iterator<Item = String>) -> ExitCode {
    let url =
        env::var("HERDR_PLUGIN_CLICKED_URL").unwrap_or_else(|_| args.next().unwrap_or_default());
    if url.is_empty() {
        eprintln!(
            "porthole herdr-open: no URL (set HERDR_PLUGIN_CLICKED_URL or pass one as an argument)"
        );
        return ExitCode::from(2);
    }
    open(once(url))
}

/// `porthole zed` (also reachable as `zed`): open a path on this remote
/// in the local Zed over SSH. One positional path, or the current
/// directory. Flags are rejected rather than guessed at: this shim is a
/// one-way bridge, not a Zed CLI reimplementation.
fn zed(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut path = None;
    let mut after_separator = false;
    for arg in args.by_ref() {
        if !after_separator {
            match arg.as_str() {
                "-h" | "--help" => {
                    eprintln!("usage: zed [<path>]");
                    return ExitCode::SUCCESS;
                }
                "--" => {
                    after_separator = true;
                    continue;
                }
                _ if arg.starts_with('-') => {
                    eprintln!("zed: unsupported flag '{arg}' (this shim opens one path)");
                    return ExitCode::from(2);
                }
                _ => {}
            }
        }
        if path.is_some() {
            eprintln!("zed: one path at a time");
            return ExitCode::from(2);
        }
        path = Some(arg);
    }

    let Some(home) = env::home_dir() else {
        eprintln!("zed: cannot determine home directory");
        return ExitCode::FAILURE;
    };
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("zed: cannot determine current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let raw = path.unwrap_or_else(|| ".".to_string());
    let target = if raw == "~" {
        home.clone()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        cwd.join(raw)
    };
    let resolved = match absolute(&target) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("zed: cannot resolve {}: {e}", target.display());
            return ExitCode::FAILURE;
        }
    };
    let Some(path) = resolved.to_str() else {
        eprintln!("zed: path is not valid UTF-8");
        return ExitCode::FAILURE;
    };

    let user = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_default();
    if user.is_empty() {
        eprintln!("zed: cannot determine the remote username");
        return ExitCode::FAILURE;
    }

    let sock = home.join(".porthole.sock");
    let spool = home.join(".porthole.spool");
    match zed_inner(&sock, &spool, path, &user) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Send the request and wait for the daemon's one-line ack. No spool:
/// a stale `zed .` from hours ago would steal focus at a random later
/// moment, which is worse than a clear failure now.
fn zed_inner(sock: &Path, spool: &Path, path: &str, user: &str) -> io::Result<()> {
    let mut stream = UnixStream::connect(sock).map_err(|e| {
        if unreachable(&e) {
            io::Error::other("daemon unreachable (zed needs a live connection)")
        } else {
            e
        }
    })?;
    stream.set_write_timeout(Some(ZED_TIMEOUT))?;

    flush_spool(&mut stream, spool)?;
    write_request(
        &mut stream,
        &Request::Zed {
            zed: ZedTarget {
                path: Cow::Borrowed(path),
                user: Cow::Borrowed(user),
            },
        },
    )?;

    let line = read_reply(&mut stream, ZED_TIMEOUT)?;
    match serde_json::from_str::<Response>(line.trim_end()) {
        Ok(Response::Ok { ok: true }) => Ok(()),
        Ok(Response::Error { error }) => Err(io::Error::other(error.into_owned())),
        Ok(_) => Err(io::Error::other("unexpected reply from daemon")),
        Err(e) => Err(io::Error::other(format!(
            "malformed reply from daemon: {e}"
        ))),
    }
}

/// How long the `zed` shim waits for the daemon to hand the URL to the
/// local Zed CLI. The CLI returns as soon as it hands off; the margin is
/// for a cold app launch.
const ZED_TIMEOUT: Duration = Duration::from_secs(15);

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

/// Largest clipboard payload, raw bytes, both directions. JSON escaping
/// inflates the wire line (the daemon caps lines at 64 KiB) and terminals
/// cap OSC 52 payloads too, so text pastes only — this is a clipboard,
/// not a pipe. Visible to the daemon module for its side of the cap.
pub(crate) const CLIPBOARD_MAX: usize = 32 * 1024;

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

/// `porthole paste` (also reachable as `pbpaste`): the Mac's
/// clipboard → stdout. The read direction of the bridge.
///
/// There is no OSC 52 shortcut here: terminals almost universally
/// refuse the query form of the sequence (it would let any program
/// silently read the clipboard), so attached or detached, the request
/// rides the daemon socket and the reply rides the same connection.
///
/// A read has nothing to spool, and never blocks forever: a reply that
/// never lands times out, so an editor calling this as a clipboard
/// provider fails loudly instead of hanging its UI.
fn paste(_args: impl Iterator<Item = String>) -> ExitCode {
    let Some(home) = env::home_dir() else {
        eprintln!("porthole paste: cannot determine home directory");
        return ExitCode::FAILURE;
    };
    let sock = home.join(".porthole.sock");
    let spool = home.join(".porthole.spool");
    match paste_inner(&sock, &spool) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("porthole paste: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Paste has no tty branch, so it lives beside `open_inner`, not inside
/// `clipboard`: connect, opportunistically flush the URL spool (the
/// transport is up), ask, and read one line back. The daemon acks
/// nothing but Paste, so the first line back is the paste reply.
fn paste_inner(sock: &Path, spool: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(sock).map_err(|e| {
        if unreachable(&e) {
            io::Error::other("daemon unreachable (paste needs a live connection)")
        } else {
            e
        }
    })?;
    stream.set_read_timeout(Some(PASTE_TIMEOUT))?;
    stream.set_write_timeout(Some(PASTE_TIMEOUT))?;

    flush_spool(&mut stream, spool)?;
    write_request(&mut stream, &Request::Paste { paste: true })?;

    let line = read_reply(&mut stream, PASTE_TIMEOUT)?;
    match serde_json::from_str::<Response>(line.trim_end()) {
        Ok(Response::Clipboard { clipboard }) => {
            io::stdout().write_all(clipboard.as_bytes())?;
            io::stdout().flush()?;
            Ok(())
        }
        Ok(Response::Error { error }) => Err(io::Error::other(error.into_owned())),
        Ok(Response::Ok { .. }) => Err(io::Error::other("unexpected reply from daemon")),
        Err(e) => Err(io::Error::other(format!(
            "malformed reply from daemon: {e}"
        ))),
    }
}

/// How long `porthole paste` waits for the daemon's reply. Generous
/// against one SSH round trip plus pbpaste; short enough that an
/// editor calling this as a provider recovers before the user notices.
const PASTE_TIMEOUT: Duration = Duration::from_secs(5);

/// One bounded reply line. The daemon enforces the same cap on its
/// replies as on its reads, so a longer line means the protocol broke,
/// not that patience should increase.
fn read_reply(stream: &mut UnixStream, timeout: Duration) -> io::Result<String> {
    stream.set_read_timeout(Some(timeout))?;
    let mut line = String::new();
    let n = match io::BufReader::new(stream)
        .take(MAX_REPLY_LINE as u64 + 1)
        .read_line(&mut line)
    {
        Ok(n) => n,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            return Err(io::Error::other(format!(
                "no reply within {} s",
                timeout.as_secs()
            )));
        }
        Err(e) => return Err(e),
    };
    if n == 0 {
        return Err(io::Error::other(
            "daemon closed the connection without replying",
        ));
    }
    if line.len() > MAX_REPLY_LINE {
        return Err(io::Error::other(format!(
            "reply exceeds {MAX_REPLY_LINE} bytes"
        )));
    }
    Ok(line)
}

/// The largest reply line the client accepts, mirroring the daemon's
/// inbound line cap. (Not imported from daemon.rs: that module is
/// feature-gated off in the remote build, and the client must enforce
/// the cap in that build too.)
const MAX_REPLY_LINE: usize = 64 * 1024;

/// `lemonade` impersonation: of every clipboard provider Neovim probes
/// for, this is the only one with no environment gate — no has('mac'),
/// no $DISPLAY, no $WAYLAND_DISPLAY, just `executable('lemonade')`.
/// Under this name the bridge is auto-detected on any remote, SSH
/// sessions included, with zero editor configuration.
///
/// The dialect is two subcommands: `copy` is the clipboard role,
/// `paste` is the paste role (the Mac's clipboard on stdout).
fn lemonade(mut args: impl Iterator<Item = String>) -> ExitCode {
    match args.next().as_deref() {
        Some("copy") => clipboard(args),
        Some("paste") => paste(args),
        _ => {
            eprintln!("usage: lemonade copy|paste");
            ExitCode::from(2)
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

        let paste: Request = serde_json::from_str(r#"{"paste":true}"#).unwrap();
        assert!(matches!(paste, Request::Paste { paste: true }));
        let wire = serde_json::to_string(&Request::Paste { paste: true }).unwrap();
        assert_eq!(wire, r#"{"paste":true}"#);

        let zed: Request =
            serde_json::from_str(r#"{"zed":{"path":"/Users/matt/code/porthole","user":"matt"}}"#)
                .unwrap();
        let Request::Zed { zed } = zed else {
            panic!("expected Zed");
        };
        assert_eq!(zed.path, "/Users/matt/code/porthole");
        assert_eq!(zed.user, "matt");
    }

    #[test]
    fn reply_format_both_verbs() {
        let ok: Response = serde_json::from_str(r#"{"clipboard":"hello"}"#).unwrap();
        let Response::Clipboard { clipboard } = ok else {
            panic!("expected Clipboard reply");
        };
        assert_eq!(clipboard, "hello");

        let err: Response = serde_json::from_str(r#"{"error":"boom"}"#).unwrap();
        let Response::Error { error } = err else {
            panic!("expected Error reply");
        };
        assert_eq!(error, "boom");

        let ok: Response = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(matches!(ok, Response::Ok { ok: true }));
    }
}
