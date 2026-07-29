//! Differential probe for the `init_cmd` registry-fixture flake: does an
//! ABORTIVE socket close abort a large in-flight HTTP response, and does a
//! graceful close fix it?
//!
//! The flake surfaced only on `Test (windows-latest, node 24)`: the age-floor
//! install test failed with a TRANSPORT error — not an HTTP status — while
//! fetching the tarball, the largest body the fixture serves. The mechanism
//! under test is that closing a socket whose receive buffer still holds unread
//! bytes is an ABORTIVE close, which discards whatever is still queued in the
//! SEND buffer — so a client mid-read of a multi-megabyte body never gets the
//! rest of it.
//!
//! Two servers differing in exactly ONE thing — how the connection is closed —
//! are driven by an identical client. Both leave the same unread bytes on the
//! wire, because the client deliberately sends its request in two segments; the
//! `Abortive` server reads once (the pattern the fixture used) and closes with
//! segment two unread, while `Graceful` reads the request to its end, then
//! half-closes and drains to EOF before dropping.
//!
//! What the abortive loss looks like is platform-dependent, so do not key on
//! the error. On macOS it is neither an RST nor a FIN — the transfer simply
//! stops mid-body and the client's read blocks until it times out.
//!
//! Read the run as a differential. `Graceful` failing at all is the regression
//! (and the only thing that sets a non-zero exit); `Abortive` failing is the
//! observation that the original fixture pattern was unsound on this platform.
//! `Abortive` passing on a platform means only that the platform tolerates it —
//! it is not evidence the pattern is correct.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Large enough that a substantial tail is still sitting in the kernel send
/// buffer when the server closes — which is what an abortive close discards.
const BODY_LEN: usize = 4 * 1024 * 1024;
const ITERATIONS: usize = 15;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Close {
    /// The original fixture: one bounded read, write, let the drop close it.
    Abortive,
    /// Read the request to its end, half-close (FIN), drain to EOF.
    Graceful,
}

impl Close {
    fn label(self) -> &'static str {
        match self {
            Close::Abortive => "abortive (original fixture)",
            Close::Graceful => "graceful (fixed fixture)",
        }
    }
}

fn serve(mode: Close, body: Arc<Vec<u8>>) -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?.to_string();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let body = body.clone();
            thread::spawn(move || {
                let _ = handle(mode, stream, &body);
            });
        }
    });
    Ok(addr)
}

fn handle(mode: Close, mut stream: TcpStream, body: &[u8]) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;
    let mut buf = [0_u8; 4096];

    match mode {
        Close::Abortive => {
            let _ = stream.read(&mut buf)?;
        }
        Close::Graceful => {
            let mut request = Vec::new();
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf)? {
                    0 => break,
                    size => request.extend_from_slice(&buf[..size]),
                }
            }
        }
    }

    let headers = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;

    if mode == Close::Graceful {
        stream.shutdown(Shutdown::Write)?;
        while stream.read(&mut buf)? > 0 {}
    }
    Ok(())
}

/// One request/response exchange. Returns the number of body bytes received, or
/// the transport error that ended it.
fn fetch(addr: &str) -> std::io::Result<usize> {
    let mut stream = TcpStream::connect(addr)?;
    // An abortive close can leave the client with no RST and no FIN — just
    // silence — so the read timeout, not a reset, is what ends a lost response.
    // Keep it short: it is the per-failure cost of the whole run.
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;

    // Two segments with a gap between them. This is the whole point of the
    // probe: it guarantees the `Abortive` server's single read consumes only
    // segment one, leaving segment two unread when it closes.
    stream.write_all(b"GET /tarball.tgz HTTP/1.1\r\nhost: probe\r\n")?;
    stream.flush()?;
    thread::sleep(Duration::from_millis(20));
    stream.write_all(b"user-agent: win-socket-close-probe\r\naccept: */*\r\n\r\n")?;
    stream.flush()?;

    let mut received = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        match stream.read(&mut buf)? {
            0 => break,
            size => received.extend_from_slice(&buf[..size]),
        }
    }
    let header_end = received
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(received.len());
    Ok(received.len() - header_end)
}

fn run(mode: Close, body: Arc<Vec<u8>>) -> (usize, usize, Option<String>) {
    let addr = match serve(mode, body) {
        Ok(addr) => addr,
        Err(error) => return (0, ITERATIONS, Some(format!("bind failed: {error}"))),
    };
    let (mut ok, mut bad, mut first) = (0, 0, None);
    for _ in 0..ITERATIONS {
        match fetch(&addr) {
            Ok(len) if len == BODY_LEN => ok += 1,
            Ok(len) => {
                bad += 1;
                first.get_or_insert(format!("short body: {len} of {BODY_LEN} bytes"));
            }
            Err(error) => {
                bad += 1;
                first.get_or_insert(format!("{:?}: {error}", error.kind()));
            }
        }
    }
    (ok, bad, first)
}

fn main() {
    let body = Arc::new(vec![0x5a_u8; BODY_LEN]);
    println!(
        "platform: {} / {}  body: {BODY_LEN} bytes  iterations: {ITERATIONS}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    let mut regression = false;
    for mode in [Close::Abortive, Close::Graceful] {
        let (ok, bad, first) = run(mode, body.clone());
        println!(
            "{:<28} ok={ok:<3} failed={bad:<3} {}",
            mode.label(),
            first.unwrap_or_else(|| "-".to_string())
        );
        if mode == Close::Graceful && bad > 0 {
            regression = true;
        }
    }

    if regression {
        println!("\nFAIL: the graceful close lost responses — the fixture fix does not hold here.");
        std::process::exit(1);
    }
    println!("\nOK: every response survived the graceful close.");
}
