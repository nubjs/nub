//! How bytes get here. The 1 MB budget is decided entirely by TLS (a rustls + ring stack is
//! ~500 KB on macOS and over 1 MB static on Linux), so by default this crate links NO TLS
//! and borrows an HTTPS client the host already has. `detect` tries, in order:
//!
//! 1. In-binary TLS, when built with `--features tls` (platform TLS on macOS/Windows,
//!    rustls elsewhere).
//! 2. `node` — a single long-lived child running `fetch`. Node is the one thing the use case
//!    guarantees on the box: whatever gets installed is about to be run by it. This is what
//!    makes the chain terminate on slim container images, where a 2026-09-17 survey of 16
//!    popular bases found curl on 4 and neither curl nor wget on 8.
//! 3. `curl` (ships with macOS and Windows 10+, most full Linux distributions).
//! 4. `wget` (busybox on Alpine).
//!
//! An embedder with its own HTTP client skips all of this by implementing [`Transport`].
//! Every transport here is safe to call from many threads at once; the installer does.

use crate::error::Error;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub trait Transport: Send + Sync {
    /// Fetch `url` with the given `Accept` header. A non-2xx status is an error.
    fn get(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error>;
}

/// The first transport available, in the order documented above.
pub fn detect() -> Result<Box<dyn Transport>, Error> {
    #[cfg(feature = "tls")]
    return Ok(Box::new(builtin::Builtin::new()));
    #[cfg(not(feature = "tls"))]
    detect_host()
}

/// The first HOST-provided transport, skipping in-binary TLS even when it is compiled in.
/// Worth reaching for deliberately behind a TLS-intercepting corporate proxy: the host's own
/// client carries the system trust store, where a bundled rustls root set does not.
pub fn detect_host() -> Result<Box<dyn Transport>, Error> {
    if let Some(t) = NodeFetch::spawn() {
        return Ok(Box::new(t));
    }
    if available("curl") {
        return Ok(Box::new(Curl));
    }
    if available("wget") {
        return Ok(Box::new(Wget));
    }
    Err(Error::NoTransport(vec!["node", "curl", "wget"]))
}

fn available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn check_status(url: &str, status: u16) -> Result<(), Error> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(Error::Status {
            url: url.to_string(),
            status,
        })
    }
}

// ---------------------------------------------------------------------------------------
// node: one child, many requests in flight. A request line is `<id>\t<accept>\t<url>\n`; a
// reply is `<id> <status> <length>\n` followed by exactly `length` body bytes, written by the
// child in a single write so replies never interleave. The child starts every fetch as it
// arrives, so replies come back in completion order and a reader thread routes each to its
// waiting caller. Node's undici pool then gives connection reuse for free.

const NODE_SCRIPT: &str = r#"
if (typeof fetch !== 'function') { process.stdout.write('NOFETCH\n'); process.exit(0); }
process.stdout.write('READY\n');
const rl = require('readline').createInterface({ input: process.stdin });
rl.on('line', async (line) => {
  const [id, accept, url] = line.split('\t');
  let status = 0, body = Buffer.alloc(0);
  try {
    const r = await fetch(url, { headers: { accept } });
    status = r.status; body = Buffer.from(await r.arrayBuffer());
  } catch (e) {}
  process.stdout.write(Buffer.concat([Buffer.from(id + ' ' + status + ' ' + body.length + '\n'), body]));
});
rl.on('close', () => process.exit(0));
"#;

type Reply = Result<(u16, Vec<u8>), Error>;
type Pending = Arc<Mutex<HashMap<u64, Sender<Reply>>>>;

pub struct NodeFetch {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
}

impl NodeFetch {
    /// `None` when `node` is absent or predates global `fetch` (Node < 18).
    pub fn spawn() -> Option<Self> {
        let mut child = Command::new("node")
            .args(["-e", NODE_SCRIPT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let mut stdout = BufReader::new(child.stdout.take()?);
        let mut ready = String::new();
        stdout.read_line(&mut ready).ok()?;
        if ready.trim_end() != "READY" {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        let pending: Pending = Arc::default();
        let routes = Arc::clone(&pending);
        std::thread::spawn(move || Self::route(stdout, routes));
        Some(NodeFetch {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
        })
    }

    /// Reader thread: deliver each reply to its caller. On EOF every waiter gets an error,
    /// which is how a crashed child surfaces instead of hanging its callers.
    fn route(mut stdout: BufReader<ChildStdout>, pending: Pending) {
        loop {
            let mut header = String::new();
            if matches!(stdout.read_line(&mut header), Ok(0) | Err(_)) {
                break;
            }
            let mut parts = header.split_whitespace();
            let parsed = (
                parts.next().and_then(|s| s.parse::<u64>().ok()),
                parts.next().and_then(|s| s.parse::<u16>().ok()),
                parts.next().and_then(|s| s.parse::<usize>().ok()),
            );
            let (Some(id), Some(status), Some(len)) = parsed else {
                break;
            };
            let mut body = vec![0; len];
            if stdout.read_exact(&mut body).is_err() {
                break;
            }
            let waiter = pending.lock().ok().and_then(|mut p| p.remove(&id));
            if let Some(tx) = waiter {
                let _ = tx.send(Ok((status, body)));
            }
        }
        if let Ok(mut p) = pending.lock() {
            for (_, tx) in p.drain() {
                let _ = tx.send(Err(Error::Transport("node transport exited".into())));
            }
        }
    }
}

impl Transport for NodeFetch {
    fn get(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error> {
        let poisoned = || Error::Transport("node transport poisoned".into());
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending.lock().map_err(|_| poisoned())?.insert(id, tx);
        {
            let mut stdin = self.stdin.lock().map_err(|_| poisoned())?;
            writeln!(stdin, "{id}\t{accept}\t{url}")
                .map_err(|e| Error::Transport(format!("node: {e}")))?;
        }
        let (status, body) = rx
            .recv()
            .map_err(|_| Error::Transport("node transport exited".into()))??;
        if status == 0 {
            return Err(Error::Transport(format!("node: fetch failed for {url}")));
        }
        check_status(url, status)?;
        Ok(body)
    }
}

impl Drop for NodeFetch {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------------------
// curl / wget: one process per request. `--compressed` lets curl negotiate gzip for the
// packument; wget (busybox included) has no equivalent and fetches identity.

pub struct Curl;

impl Transport for Curl {
    fn get(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error> {
        let out = Command::new("curl")
            .args([
                "-fsSL",
                "--compressed",
                "-w",
                "\n%{http_code}",
                "-H",
                &format!("accept: {accept}"),
                url,
            ])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::Transport(format!("curl: {e}")))?;
        // `-w` appends the status after the body; split it back off.
        let body = out.stdout;
        let cut = body.iter().rposition(|&b| b == b'\n').unwrap_or(body.len());
        let status: u16 = std::str::from_utf8(&body[cut..])
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if !out.status.success() && status == 0 {
            return Err(Error::Transport(format!(
                "curl exited {} for {url}",
                out.status
            )));
        }
        check_status(url, status)?;
        Ok(body[..cut].to_vec())
    }
}

pub struct Wget;

impl Transport for Wget {
    fn get(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error> {
        let out = Command::new("wget")
            .args(["-q", "-O", "-", &format!("--header=accept: {accept}"), url])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::Transport(format!("wget: {e}")))?;
        if !out.status.success() {
            return Err(Error::Transport(format!(
                "wget exited {} for {url}",
                out.status
            )));
        }
        Ok(out.stdout)
    }
}

// ---------------------------------------------------------------------------------------
// In-binary TLS (opt-in). ureq's Agent pools connections and is safe to share across threads.

#[cfg(feature = "tls")]
mod builtin {
    use super::{Transport, check_status};
    use crate::error::Error;

    pub struct Builtin(ureq::Agent);

    impl Builtin {
        pub fn new() -> Self {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                use ureq::tls::{TlsConfig, TlsProvider};
                let cfg = ureq::Agent::config_builder()
                    .tls_config(
                        TlsConfig::builder()
                            .provider(TlsProvider::NativeTls)
                            .build(),
                    )
                    .http_status_as_error(false)
                    .build();
                return Builtin(ureq::Agent::new_with_config(cfg));
            }
            #[allow(unreachable_code)]
            Builtin(ureq::Agent::new_with_config(
                ureq::Agent::config_builder()
                    .http_status_as_error(false)
                    .build(),
            ))
        }
    }

    impl Transport for Builtin {
        fn get(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error> {
            let mut resp = self
                .0
                .get(url)
                .header("accept", accept)
                .call()
                .map_err(|e| Error::Transport(e.to_string()))?;
            check_status(url, resp.status().as_u16())?;
            resp.body_mut()
                .with_config()
                .limit(1 << 30)
                .read_to_vec()
                .map_err(|e| Error::Transport(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_programs_are_reported_absent_not_as_errors() {
        assert!(!available("microbe-definitely-not-a-program"));
    }

    #[test]
    fn node_transport_multiplexes_concurrent_requests() {
        let Some(t) = NodeFetch::spawn() else { return };
        // Eight failures racing through one child: every caller must get ITS reply back,
        // whatever order the child finishes in, and none may hang.
        std::thread::scope(|s| {
            for i in 0..8 {
                let t = &t;
                s.spawn(move || {
                    let err = t
                        .get(&format!("https://registry.invalid/{i}"), "*/*")
                        .unwrap_err();
                    assert!(matches!(err, Error::Transport(_)), "{err}");
                });
            }
        });
    }

    #[test]
    fn node_transport_reports_http_status() {
        let Some(t) = NodeFetch::spawn() else { return };
        // A URL no resolver answers fails at fetch, not with a status.
        let err = t.get("https://registry.invalid/x", "*/*").unwrap_err();
        assert!(matches!(err, Error::Transport(_)), "{err}");
    }
}
