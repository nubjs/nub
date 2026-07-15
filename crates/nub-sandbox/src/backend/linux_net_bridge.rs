//! Linux per-host egress bridge — the HOST-side half (C3 / design.md §2.5).
//!
//! MECHANISM. A `--unshare-net` child lives in an EMPTY network namespace: that empty
//! netns IS the egress boundary — nothing the child does reaches off-box except through
//! this bridge, and the bridge's far end is the parent-loopback [`EgressProxy`], which
//! still authenticates every tunnel (per-session token) and enforces the host
//! allow-list. A filesystem-path AF_UNIX socket is the one channel that crosses the
//! netns wall (abstract AF_UNIX is netns-scoped; a PATH socket is mount-ns scoped), so
//! the bridge is: a parent [`UnixListener`] whose every connection is spliced to a fresh
//! `TcpStream` to the proxy's loopback port. Bubblewrap read-only bind-mounts this
//! socket dir into the sandbox; the monitor's in-netns half (a loopback `TcpListener`)
//! connects to the UDS. The bridge itself is a DUMB byte pump — it makes no policy
//! decision (the proxy does).
//!
//! LIFECYCLE. Runs as parent THREADS (mirroring the [`EgressProxy`](crate::proxy) it
//! feeds), owned by the returned handle and torn down on Drop AFTER the child exits —
//! the parent owns them so they can never be orphaned; there is no bridge process to
//! outlive the app. The per-run socket dir is 0700 and removed on Drop; a dir left by a
//! crashed prior run is GC'd by dead-pid at [`start`].

#![cfg(target_os = "linux")]

use std::fs;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

/// The sandbox-visible mount point the socket dir is bind-mounted at, and the socket
/// file name inside it. The monitor connects to `<PRIVATE_NET_ROOT>/<SOCK_NAME>`.
pub(crate) const PRIVATE_NET_ROOT: &str = "/dev/.nub-sandbox/net";
pub(crate) const SOCK_NAME: &str = "s";

/// The running host-side egress bridge. Dropping it stops accepting, joins the pump
/// threads, and removes the per-run socket dir.
pub(crate) struct HostNetBridge {
    dir: PathBuf,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

impl HostNetBridge {
    /// The host-side 0700 socket dir Bubblewrap read-only bind-mounts into the sandbox at
    /// [`PRIVATE_NET_ROOT`]. The dir stays alive for the handle's whole life (removed on
    /// Drop), so a descriptor the backend opens against it is valid through the launch.
    pub(crate) fn socket_dir(&self) -> &Path {
        &self.dir
    }
}

/// Start the host bridge feeding `proxy_port`. Creates a fresh 0700 socket dir, binds the
/// listener, and spawns the accept loop. Returns once the socket exists (so the caller can
/// bind-mount it before launching the child).
pub(crate) fn start(proxy_port: u16) -> io::Result<HostNetBridge> {
    gc_stale_dirs();
    let dir = make_socket_dir()?;
    let sock_path = dir.join(SOCK_NAME);
    let listener = match UnixListener::bind(&sock_path) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = fs::remove_dir_all(&dir);
            return Err(error);
        }
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    let sh = shutdown.clone();
    let accept_thread = std::thread::Builder::new()
        .name("nub-net-bridge".into())
        .spawn(move || accept_loop(listener, sh, proxy_port))?;
    Ok(HostNetBridge {
        dir,
        shutdown,
        accept_thread: Some(accept_thread),
    })
}

impl Drop for HostNetBridge {
    fn drop(&mut self) {
        // Signal the accept loop, then wake its blocked `accept()` with a throwaway
        // self-connection. In-flight pump threads are detached; they end when their
        // sockets close (the child — and thus the in-netns half — is already gone).
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(self.dir.join(SOCK_NAME));
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn accept_loop(listener: UnixListener, shutdown: Arc<AtomicBool>, proxy_port: u16) {
    for conn in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let Ok(uds) = conn else { continue };
        // Best-effort spawn; if the OS refuses a thread the connection is dropped
        // (fail-closed — no unproxied path opens).
        let _ = std::thread::Builder::new()
            .name("nub-net-bridge-conn".into())
            .spawn(move || {
                if let Ok(tcp) =
                    std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, proxy_port))
                {
                    splice_unix_tcp(uds, tcp);
                }
            });
    }
}

/// Blind bidirectional forward between the UDS (in-netns half) and the TCP conn (proxy).
/// One thread copies each direction; each shuts down its peer's write half on EOF so the
/// other copy unblocks and the tunnel tears down cleanly.
pub(crate) fn splice_unix_tcp(uds: UnixStream, tcp: std::net::TcpStream) {
    use std::io::copy;
    use std::net::Shutdown;
    let (Ok(mut uds_rd), Ok(mut tcp_wr)) = (uds.try_clone(), tcp.try_clone()) else {
        return;
    };
    let up = std::thread::spawn(move || {
        let _ = copy(&mut uds_rd, &mut tcp_wr);
        let _ = tcp_wr.shutdown(Shutdown::Write);
    });
    let mut tcp_rd = tcp;
    let mut uds_wr = uds;
    let _ = copy(&mut tcp_rd, &mut uds_wr);
    let _ = uds_wr.shutdown(Shutdown::Write);
    let _ = up.join();
}

/// A fresh per-run 0700 socket dir under the OS temp root. Named `nub-net-<pid>-<rand>`
/// so a crash-orphaned dir is GC-able by dead-pid. The short name keeps the UDS path well
/// under the 108-byte `sun_path` limit.
fn make_socket_dir() -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let mut rand = [0u8; 8];
    getrandom::getrandom(&mut rand)
        .map_err(|error| io::Error::other(format!("socket-dir nonce: {error}")))?;
    let nonce = rand.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64);
    let dir = std::env::temp_dir().join(format!("nub-net-{}-{:016x}", std::process::id(), nonce));
    // Create 0700 ATOMICALLY (mode set at mkdir, before the socket is bound), so no window
    // exists where a co-resident user could traverse the dir to the relay socket.
    fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

/// Remove `nub-net-<pid>-*` dirs whose owning pid is no longer alive — cleaning up after a
/// prior nub process that crashed before its Drop ran. Best-effort; any error is ignored.
fn gc_stale_dirs() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix("nub-net-") else {
            continue;
        };
        let Some((pid, _)) = rest.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<libc::pid_t>() else {
            continue;
        };
        if pid_is_dead(pid) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn pid_is_dead(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    // signal 0 probes existence without delivering; ESRCH => the pid is gone.
    let rc = unsafe { libc::kill(pid, 0) };
    rc != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}
