//! The daemon's accept loop, hand-written on mio. No async/await anywhere.
//!
//! Companion to src/daemon.rs: same sockets, same wire protocol, same
//! behavior. With `async fn`, the compiler generates the state machine
//! this file writes out by hand — read them side by side.
//!
//! Run: cargo run --example mio_accept

use std::io::{self, Read};

use mio::net::{UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token};

const LISTENER: Token = Token(0);

/// One connection's state. In the async version this struct is generated
/// by the compiler: its fields are exactly the locals that live across
/// an .await point in handle_conn.
struct Conn {
    stream: UnixStream,
    buf: Vec<u8>,
}

fn main() -> io::Result<()> {
    let path = "/tmp/porthole-mio-accept.sock";
    let _ = std::fs::remove_file(path); // reclaim a stale socket, if any

    let mut listener = UnixListener::bind(path)?;
    let mut poll = Poll::new()?;
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)?;

    eprintln!("mio accept demo: listening on {path}");

    let mut conns: Vec<(Token, Conn)> = Vec::new();
    let mut next = 1usize;
    let mut events = Events::with_capacity(128);

    loop {
        // This is the entire runtime: park in epoll_wait/kevent until
        // something is readable, then advance each state machine by hand.
        poll.poll(&mut events, None)?;

        for event in events.iter() {
            if event.token() == LISTENER {
                // mio is edge-triggered: drain accept() to WouldBlock or
                // you never get woken for the next connection. Tokio does
                // this loop for you, invisibly, inside accept().await.
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let token = Token(next);
                            next += 1;
                            let mut conn = Conn {
                                stream,
                                buf: Vec::new(),
                            };
                            poll.registry().register(
                                &mut conn.stream,
                                token,
                                Interest::READABLE,
                            )?;
                            conns.push((token, conn));
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                }
                continue;
            }

            let token = event.token();
            let Some(idx) = conns.iter().position(|(t, _)| *t == token) else {
                continue;
            };

            // Same edge-triggered rule for reads: drain to WouldBlock or
            // EOF (0 bytes). Partial reads land in the connection's
            // buffer — the hand-written equivalent of BufReader.
            let mut eof = false;
            {
                let conn = &mut conns[idx].1;
                loop {
                    let mut chunk = [0u8; 4096];
                    match conn.stream.read(&mut chunk) {
                        Ok(0) => {
                            eof = true;
                            break;
                        }
                        Ok(n) => conn.buf.extend_from_slice(&chunk[..n]),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                }
            }

            if eof {
                // Our client writes its lines and closes; EOF completes
                // the request. Process whole lines, then drop the conn.
                let (_, mut conn) = conns.swap_remove(idx);
                for line in conn.buf.split(|b| *b == b'\n') {
                    if !line.is_empty() {
                        eprintln!("would open: {}", String::from_utf8_lossy(line));
                    }
                }
                poll.registry().deregister(&mut conn.stream)?;
            }
        }
    }
}
