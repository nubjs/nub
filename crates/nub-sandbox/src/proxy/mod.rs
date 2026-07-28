//! The localhost egress proxy (design.md §2.5): the per-host policy engine for the
//! net axis. NO MITM.
//!
//! MECHANISM. The OS deny-layer (each backend) forces the sandboxed child's egress
//! to reach ONLY this proxy on loopback; direct external egress is blocked at the
//! kernel. The proxy speaks HTTP `CONNECT` and SOCKS5, and enforces per-host policy
//! in TWO gates, both of which must pass: (1) the CONNECT/SOCKS **target host**
//! (checked before the tunnel ACK), and (2) the TLS **SNI** read in the clear from
//! the client's ClientHello (checked after the ACK, before connecting upstream) —
//! [`sni`], no key, no CA. An allowed tunnel is blind-forwarded byte-for-byte; a
//! denied one is dropped before the upstream socket is ever opened.
//!
//! FAIL-CLOSED. The decision is a [`GrantDecider`] seam (`Fn(&Host) -> Decision`) —
//! wired to the STATIC policy here ([`StaticDecider`]); the build-jail thread later
//! swaps in an interactive prompt without touching this file. A TLS tunnel whose
//! ClientHello is malformed, or stalls without a checkable SNI, is DENIED — a
//! stall-then-send-denied-SNI cannot slip past (see [`read_and_check_sni`]).
//!
//! LIFECYCLE. Thread-per-connection over `std::net` — NO async runtime, NO new
//! dependency. The proxy runs in the nub PARENT process and outlives the child:
//! [`apply`](crate::apply) stashes the [`EgressProxy`] in [`Prepared`](crate::Prepared)
//! so it lives for the child's whole run and shuts down when that value drops.

mod ca;
mod handshake;
pub mod mitm;
mod sni;

use crate::matcher::HostMatcher;
use crate::policy::NetPolicy;
use handshake::{read_request, reply_failure, reply_success};
use sni::SniScan;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// A host the proxy makes an egress decision about. The seam type of [`GrantDecider`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    /// A hostname (from a CONNECT authority, a SOCKS5 domain, or a TLS SNI).
    Name(String),
    /// An IP literal (a SOCKS5 IPv4/IPv6 target or an IP-form CONNECT authority).
    Ip(IpAddr),
}

/// A grant decision for one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

/// The egress grant seam. The proxy consults it for the CONNECT/SOCKS target AND for
/// the TLS SNI; both must be [`Decision::Allow`]. This epic wires it to the static
/// policy ([`StaticDecider`]); the build-jail thread swaps in an interactive prompt.
pub trait GrantDecider: Send + Sync + 'static {
    fn decide(&self, host: &Host) -> Decision;

    /// Whether the policy EXPLICITLY opted into private-range (RFC1918 / IPv6-ULA)
    /// egress via a `<private>` target. Consulted by the SSRF guard to lift the
    /// default private-range block on the resolved upstream IP. Defaults to `false`
    /// (fail-closed: a decider that hasn't opted in keeps private egress blocked), so
    /// the interactive/build-jail decider stays safe without overriding this.
    fn allows_private(&self) -> bool {
        false
    }
}

/// The static-policy decider: evaluates a resolved [`NetPolicy`] last-match-wins via
/// the shared [`HostMatcher`], so the proxy's per-host verdict is byte-identical to
/// the IR's net matcher (one source of truth for host-glob + CIDR semantics).
pub struct StaticDecider {
    policy: NetPolicy,
}

impl StaticDecider {
    pub fn new(policy: NetPolicy) -> Self {
        Self { policy }
    }
}

impl GrantDecider for StaticDecider {
    fn decide(&self, host: &Host) -> Decision {
        let key = match host {
            Host::Name(n) => n.clone(),
            Host::Ip(ip) => ip.to_string(),
        };
        if HostMatcher::new(&self.policy).admits(&key) {
            Decision::Allow
        } else {
            Decision::Deny
        }
    }

    fn allows_private(&self) -> bool {
        crate::matcher::host::net_allows_private(&self.policy)
    }
}

/// Time budget for reading the client's first bytes (the TLS ClientHello) after the
/// tunnel ACK. A legit HTTPS client sends it immediately; a client that stalls past
/// this is denied (the stall-bypass guard).
const CLIENT_HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Connect timeout to the upstream target.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(15);
/// Cap on buffered client prelude bytes while scanning for the SNI (mirrors the SNI
/// reassembly cap): past this a client is dribbling → fail closed.
const MAX_PRELUDE: usize = 16 * 1024;
/// Bound parent threads and socket state a sandboxed child can create at once.
const MAX_ACTIVE_CONNECTIONS: usize = 128;
/// Wake interval for a blocked send OR recv inside a blind splice — the tick that lets a
/// forwarder observe the teardown flag.
///
/// PROVENANCE. Teardown ([`EgressProxy::drop`]) signals `shutdown` and then
/// `shutdown(Shutdown::Both)`s every registered socket to wake the handlers it is about
/// to `join()`. On POSIX that works: `shutdown(SHUT_RDWR)` both sends the FIN and wakes
/// an operation already pending on another thread. WINSOCK DIVERGES on both halves — a
/// `shutdown()` does not cancel a `recv()`/`send()` pending elsewhere (only `closesocket`
/// does, which we cannot call while other threads hold clones of the handle), and the
/// `SD_BOTH` suppresses the FIN that would have EOF'd the loopback peer. So a splice
/// blocking with no timeout never unblocks and `drop` deadlocks on the join (measured on
/// `windows-latest`: the test thread inside `Drop::drop`, the handler inside
/// `WS2_32!recv`). Do NOT "simplify" either direction back to a `None` timeout — BOTH
/// need it, and a sandboxed child picks which one it wedges: stop reading and the
/// upstream→client `send` blocks; send nothing and the client→upstream `recv` blocks.
///
/// Correctness does not depend on the value: a timeout is never treated as EOF, only as
/// "re-check the flag", so a legitimately idle long-lived tunnel survives any number of
/// ticks. It governs teardown responsiveness alone — keep it short.
///
/// ACCEPTED CAVEAT. POSIX guarantees a timed-out `recv`/`send` reports any bytes it did
/// transfer (`EAGAIN` only when none were), so retrying is lossless. Windows does not:
/// MSDN calls the socket "indeterminate" after a blocking `SO_RCVTIMEO`/`SO_SNDTIMEO`
/// expiry and advises closing. We retry anyway — the alternative is a guaranteed,
/// child-triggerable teardown deadlock, and the failure mode if the caveat ever bites is
/// a truncated TLS record the peer rejects, i.e. a dropped tunnel, never a forged or
/// downgraded one. (These options are also silently inert on a socket not created with
/// `WSA_FLAG_OVERLAPPED`; Rust's std sets that flag, so the fix is not a no-op — but that
/// is a dependency on a std implementation detail worth knowing before porting this.)
const SPLICE_POLL: Duration = Duration::from_millis(250);

type ActiveSockets = Arc<Mutex<std::collections::BTreeMap<u64, Vec<TcpStream>>>>;

/// The sockets owned by one handler. Clones are kept in a shared registry so proxy
/// teardown can `shutdown(Both)` every in-flight client/upstream before joining the
/// handler threads. That makes the per-apply MITM engine (and its secrets) end with
/// the proxy rather than surviving in detached threads.
pub(super) struct ActiveConnection {
    id: u64,
    sockets: ActiveSockets,
    shutdown: Arc<AtomicBool>,
}

impl ActiveConnection {
    fn new(
        stream: &TcpStream,
        sockets: ActiveSockets,
        shutdown: Arc<AtomicBool>,
        next_id: &AtomicU64,
    ) -> io::Result<Self> {
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let tracked = stream.try_clone()?;
        let mut active = sockets
            .lock()
            .map_err(|_| io::Error::other("egress proxy socket registry poisoned"))?;
        if active.len() >= MAX_ACTIVE_CONNECTIONS {
            return Err(io::Error::other(
                "egress proxy active-connection cap reached",
            ));
        }
        if shutdown.load(Ordering::SeqCst) {
            let _ = tracked.shutdown(Shutdown::Both);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "egress proxy is shutting down",
            ));
        }
        active.insert(id, vec![tracked]);
        drop(active);
        Ok(Self {
            id,
            sockets,
            shutdown,
        })
    }

    pub(super) fn track(&self, stream: &TcpStream) -> io::Result<()> {
        let tracked = stream.try_clone()?;
        if self.shutdown.load(Ordering::SeqCst) {
            let _ = tracked.shutdown(Shutdown::Both);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "egress proxy is shutting down",
            ));
        }
        let mut active = self
            .sockets
            .lock()
            .map_err(|_| io::Error::other("egress proxy socket registry poisoned"))?;
        // Re-check under the same mutex Drop uses for its shutdown sweep. Without
        // this check, an upstream could be inserted after the sweep and keep a
        // joined handler blocked forever.
        if self.shutdown.load(Ordering::SeqCst) {
            let _ = tracked.shutdown(Shutdown::Both);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "egress proxy is shutting down",
            ));
        }
        let Some(sockets) = active.get_mut(&self.id) else {
            let _ = tracked.shutdown(Shutdown::Both);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "egress proxy connection is no longer active",
            ));
        };
        sockets.push(tracked);
        Ok(())
    }
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        if let Ok(mut active) = self.sockets.lock() {
            active.remove(&self.id);
        }
    }
}

/// A running egress proxy bound to `127.0.0.1:<port>`. Dropping it stops accepting new
/// connections (the parent owns this; it drops after the sandboxed child exits).
pub struct EgressProxy {
    port: u16,
    /// The per-session bearer every client must present (HTTP `Proxy-Authorization` /
    /// SOCKS5 user-pass) — the defense-in-depth guard against a co-resident same-user
    /// process borrowing the child's loopback egress hole. Minted per [`start`] from the
    /// OS CSPRNG; delivered to the child as the `HTTP_PROXY` URL userinfo.
    token: Arc<str>,
    shutdown: Arc<AtomicBool>,
    active_sockets: ActiveSockets,
    accept_thread: Option<JoinHandle<()>>,
    /// The MITM engine, when the policy engages TLS termination (credential brokering /
    /// `proxy: "terminate"`). `None` for a host-only (connection-tier) policy — in which
    /// case NO CA exists and NO TLS code runs (the default is "MITM never instantiated").
    /// Held here so the ephemeral CA + its child bundle live for the child's whole run.
    mitm: Option<Arc<mitm::MitmEngine>>,
}

/// Walk `[low, high]` for a free loopback port. An in-use port is skipped rather than fatal
/// (a sibling nub run legitimately holds one); exhausting the window is an error naming it,
/// because silently falling back to an ephemeral port would bind OUTSIDE the range the
/// Windows WFP permit covers and leave the child unable to reach the proxy at all.
fn bind_in_range(low: u16, high: u16) -> io::Result<TcpListener> {
    let mut last: Option<io::Error> = None;
    for port in low..=high {
        match TcpListener::bind((IpAddr::from([127, 0, 0, 1]), port)) {
            Ok(l) => return Ok(l),
            Err(e) => last = Some(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        format!(
            "every loopback port in the sandbox proxy window {low}-{high} is in use{}",
            last.map(|e| format!(" (last error: {e})"))
                .unwrap_or_default()
        ),
    ))
}

impl EgressProxy {
    /// Bind a loopback listener and start the accept loop. `decider` gates every tunnel;
    /// `mitm` (when present) terminates the hosts whose rules demand inspection. Returns
    /// once the port is bound (so a caller can wire the port into the backend deny-layer
    /// before spawning the child).
    pub fn start(
        decider: Arc<dyn GrantDecider>,
        mitm: Option<Arc<mitm::MitmEngine>>,
    ) -> io::Result<EgressProxy> {
        Self::start_in_range(decider, mitm, None)
    }

    /// As [`EgressProxy::start`], but constrained to bind inside `[low, high]` when a range is
    /// given.
    ///
    /// WHY A RANGE EXISTS AT ALL: Windows' dedicated-account backend fences egress with WFP
    /// filters keyed on the account SID, and every WFP write needs administrator. Baking the
    /// run's ephemeral port into a filter would therefore mean a UAC prompt per run, so the
    /// one-time elevated setup pre-authorizes a narrow loopback WINDOW instead and the proxy
    /// binds into it. mac/Linux carve the exact port at launch and pass `None`.
    pub fn start_in_range(
        decider: Arc<dyn GrantDecider>,
        mitm: Option<Arc<mitm::MitmEngine>>,
        range: Option<(u16, u16)>,
    ) -> io::Result<EgressProxy> {
        // Loopback only — the sandboxed child reaches us via 127.0.0.1; nothing off-box
        // should ever see this listener.
        let listener = match range {
            None => TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0))?,
            Some((low, high)) => bind_in_range(low, high)?,
        };
        let port = listener.local_addr()?.port();
        let token: Arc<str> = Arc::from(mint_token());
        let shutdown = Arc::new(AtomicBool::new(false));
        let active_sockets = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
        let sh = shutdown.clone();
        let active = active_sockets.clone();
        let engine = mitm.clone();
        let tok = token.clone();
        let accept_thread = std::thread::Builder::new()
            .name("nub-egress-proxy".into())
            .spawn(move || accept_loop(listener, sh, active, decider, engine, tok))?;
        Ok(EgressProxy {
            port,
            token,
            shutdown,
            active_sockets,
            accept_thread: Some(accept_thread),
            mitm,
        })
    }

    /// The loopback port the child must be pointed at (env hint + OS carve-out).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The per-session bearer token the child presents to the proxy — delivered via the
    /// `HTTP_PROXY` URL userinfo so ordinary proxy-honoring clients send it automatically.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The child-scoped CA-bundle path, when TLS termination is engaged — the value the
    /// CA-env vars point the child at so it trusts the minted leaves. `None` for a
    /// connection-tier policy (no CA exists).
    pub fn ca_bundle_path(&self) -> Option<&std::path::Path> {
        self.mitm.as_ref().map(|m| m.bundle_path())
    }

    /// Immutable Linux launch authority for the child trust bundle.
    #[cfg(target_os = "linux")]
    pub(crate) fn ca_bundle_file(&self) -> io::Result<Option<std::fs::File>> {
        self.mitm.as_ref().map(|m| m.bundle_file()).transpose()
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        // Signal the accept loop, close every in-flight socket, then wake `accept()`.
        // The accept thread owns and joins every handler before returning, so dropping
        // the proxy also drops every clone of the per-apply MITM engine and its secrets.
        self.shutdown.store(true, Ordering::SeqCst);
        if let Ok(active) = self.active_sockets.lock() {
            for stream in active.values().flatten() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        let _ = TcpStream::connect((IpAddr::from([127, 0, 0, 1]), self.port));
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
    }
}

/// Accept loop: one tracked handler thread per connection. Any handler error just
/// closes that connection — a single malformed client never takes down the proxy.
fn accept_loop(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    active_sockets: ActiveSockets,
    decider: Arc<dyn GrantDecider>,
    mitm: Option<Arc<mitm::MitmEngine>>,
    token: Arc<str>,
) {
    let next_id = AtomicU64::new(1);
    let mut handlers: Vec<JoinHandle<()>> = Vec::new();
    for conn in listener.incoming() {
        // A long-running child may churn many short connections. Reap completed
        // handlers continuously instead of retaining one JoinHandle per historical
        // connection until shutdown.
        let mut index = 0;
        while index < handlers.len() {
            if handlers[index].is_finished() {
                let handler = handlers.swap_remove(index);
                let _ = handler.join();
            } else {
                index += 1;
            }
        }
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = conn else { continue };
        let Ok(active) =
            ActiveConnection::new(&stream, active_sockets.clone(), shutdown.clone(), &next_id)
        else {
            continue;
        };
        let d = decider.clone();
        let m = mitm.clone();
        let tok = token.clone();
        let sh = shutdown.clone();
        // Best-effort spawn; if the OS refuses a thread we simply drop the connection
        // (fail-closed — no unproxied path opens).
        if let Ok(handler) = std::thread::Builder::new()
            .name("nub-egress-tunnel".into())
            .spawn(move || {
                let _ = handle_conn(stream, d, m, &tok, sh, active);
            })
        {
            handlers.push(handler);
        }
    }
    for handler in handlers {
        let _ = handler.join();
    }
}

/// Mint the per-session bearer token: 256 bits from the OS CSPRNG, hex-encoded (64
/// URL-safe chars, so it drops into the `HTTP_PROXY` userinfo with no escaping). A
/// getrandom failure is unrecoverable for a security token → panic rather than fall
/// back to a weak source (fail-closed: no proxy, no egress).
fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable for egress-proxy token");
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Handle one client tunnel: parse the request, gate the target host, ACK, gate the
/// SNI, then EITHER blind-splice (connection tier) OR terminate + inject (MITM tier).
/// Returns `Ok(())` on any clean refusal.
fn handle_conn(
    mut stream: TcpStream,
    decider: Arc<dyn GrantDecider>,
    mitm: Option<Arc<mitm::MitmEngine>>,
    token: &str,
    shutdown: Arc<AtomicBool>,
    active: ActiveConnection,
) -> io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_HELLO_TIMEOUT))?;
    // Whether the policy opted into private-range egress (governs the SSRF guard's
    // private tier only). Computed once per connection from the static decider.
    let allow_private = decider.allows_private();
    // Token gate FIRST: an unauthenticated caller is answered (407 / SOCKS auth-fail)
    // and dropped before any host decision. `?` propagates the auth error → connection
    // closed (fail-closed).
    let req = read_request(&mut stream, token)?;

    // Gate 1 — the CONNECT/SOCKS target host (before the ACK).
    if decider.decide(&req.host) == Decision::Deny {
        let _ = reply_failure(&mut stream, req.proto);
        return Ok(());
    }
    reply_success(&mut stream, req.proto)?;

    // Gate 2 — the TLS SNI, read no-MITM from the client's first bytes.
    let (prelude, allowed, sni_host) = read_and_check_sni(&mut stream, decider.as_ref())?;
    if !allowed {
        return Ok(()); // drop — the client sees a reset tunnel
    }

    // The host the leaf is minted for + the broker is matched on: the SNI the client
    // asked for (so its TLS hostname check passes), else the CONNECT/SOCKS authority.
    let terminate_host = sni_host.or_else(|| match &req.host {
        Host::Name(n) => Some(n.clone()),
        Host::Ip(_) => None, // an IP-literal target carries no name to mint a leaf for
    });

    // MITM tier: terminate + inject ONLY the hosts whose rules demand it; everything else
    // (and every host under a connection-tier policy) stays a blind splice. Any error on
    // the terminate path is a fail-closed drop — never a splice fallback that would send
    // the request un-injected or expose the secret.
    if let Some(engine) = mitm.as_ref() {
        match terminate_host.as_deref() {
            Some(host) if engine.should_terminate(host) => {
                let _ = mitm::terminate(
                    engine,
                    stream,
                    prelude,
                    host,
                    req.port,
                    allow_private,
                    &active,
                );
                return Ok(());
            }
            // `proxy: "terminate"` but this connection carries no host to terminate (no
            // SNI / IP literal) — FAIL CLOSED rather than blind-splice past the
            // terminate-everything guarantee.
            None if engine.terminates_everything() => return Ok(()),
            // A connection-tier host, or a broker that didn't match this SNI → splice.
            _ => {}
        }
    }

    // Connection tier — connect upstream, replay the buffered prelude, blind-splice.
    if shutdown.load(Ordering::SeqCst) {
        return Ok(());
    }
    let upstream = connect_upstream(&req.host, req.port, allow_private)?;
    active.track(&upstream)?;
    let mut up = upstream;
    if !prelude.is_empty() {
        up.write_all(&prelude)?;
    }
    // Both legs' read timeouts are owned by `pump` from here on (SPLICE_POLL).
    splice(stream, up, shutdown);
    Ok(())
}

/// Read the client's first bytes and decide the SNI gate. Returns the buffered prelude
/// (to replay), whether the tunnel is allowed, and the SNI hostname when one was present.
///
/// The rule closes the SNI-evasion vectors: a complete ClientHello's SNI is checked;
/// a ClientHello with no SNI, or a non-TLS stream, admits (the target host already
/// passed gate 1, and without an SNI a shared-IP host cannot cross-route); a TLS
/// ClientHello that is malformed, oversize, or stalls without completing (incl. the
/// client ACKing then sending nothing) FAILS CLOSED — so a "send a partial hello, then
/// send a denied SNI after we splice" attack cannot bypass gate 2.
fn read_and_check_sni(
    stream: &mut TcpStream,
    decider: &dyn GrantDecider,
) -> io::Result<(Vec<u8>, bool, Option<String>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => return Ok(finalize_scan(&buf, decider)),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                match sni::scan_client_hello(&buf) {
                    SniScan::Sni(host) => {
                        let ok = decider.decide(&Host::Name(host.clone())) == Decision::Allow;
                        return Ok((buf, ok, Some(host)));
                    }
                    // Admitted target + no SNI to cross-route on → allow.
                    SniScan::NoSni | SniScan::NotTls => return Ok((buf, true, None)),
                    // TLS-shaped but broken → fail closed.
                    SniScan::Malformed => return Ok((buf, false, None)),
                    SniScan::Incomplete => {
                        if buf.len() > MAX_PRELUDE {
                            return Ok((buf, false, None)); // dribbling past the cap → fail closed
                        }
                        // else read more
                    }
                }
            }
            Err(e) if is_timeout(&e) => return Ok(finalize_scan(&buf, decider)),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Decide on whatever prelude arrived when the read ends (EOF or timeout). A complete
/// hello is honored; an incomplete/empty TLS stream (the stall) fails closed.
fn finalize_scan(buf: &[u8], decider: &dyn GrantDecider) -> (Vec<u8>, bool, Option<String>) {
    match sni::scan_client_hello(buf) {
        SniScan::Sni(host) => {
            let ok = decider.decide(&Host::Name(host.clone())) == Decision::Allow;
            (buf.to_vec(), ok, Some(host))
        }
        SniScan::NoSni | SniScan::NotTls => (buf.to_vec(), true, None),
        // Incomplete (incl. an empty buffer — client ACK'd then sent nothing) or
        // Malformed → the SNI could not be verified → deny.
        SniScan::Incomplete | SniScan::Malformed => (buf.to_vec(), false, None),
    }
}

/// Egress addresses the proxy must NEVER connect to, even when policy admits the host.
///
/// SSRF / DNS-rebinding guard, two tiers on the RESOLVED [`IpAddr`] (so integer/octal/hex
/// host encodings are already normalized away — we classify the address, not the token):
///
/// - HARD block (never opt-out-able, [`is_hard_blocked_ip`]): the cloud-metadata /
///   link-local surface — IPv4 link-local `169.254.0.0/16` (incl. the `169.254.169.254`
///   IMDS endpoint), IPv6 link-local `fe80::/10`, and the AWS IPv6 IMDS `fd00:ec2::254`.
///   An IPv4-in-IPv6 form (`::ffff:169.254.169.254`, `::169.254.169.254`) is unmapped to
///   its embedded v4 first so the encoding can't smuggle a metadata address past.
/// - PRIVATE block (default-on, lifted by an explicit `<private>` allow): RFC1918
///   `10/8`+`172.16/12`+`192.168/16` and IPv6 ULA `fc00::/7` ([`is_private_range`]).
///   `allow_private` is `true` only when the policy names `<private>` — a bare `*` does
///   not lift it. The AWS IPv6 IMDS lives inside ULA but is caught by the HARD tier first,
///   so `<private>` re-opens the private ranges WITHOUT re-opening the metadata endpoint.
///
/// Loopback (`127/8`, `::1`) is deliberately in NEITHER tier: it is the proxy's own carve
/// and a legit upstream may be loopback, so it is always reachable.
fn is_blocked_egress_ip(ip: IpAddr, allow_private: bool) -> bool {
    if is_hard_blocked_ip(ip) {
        return true;
    }
    !allow_private && crate::matcher::host::is_private_range(ip)
}

/// The always-blocked cloud-metadata / link-local surface (no policy opt-out).
fn is_hard_blocked_ip(ip: IpAddr) -> bool {
    const AWS_IMDS_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4() {
                return v4.is_link_local();
            }
            // fe80::/10 hand-rolled (`Ipv6Addr::is_unicast_link_local` is still unstable).
            (v6.segments()[0] & 0xffc0) == 0xfe80 || v6 == AWS_IMDS_V6
        }
    }
}

/// Connect to the upstream target with a timeout. A hostname is resolved here (the
/// proxy owns DNS — a child-supplied IP for a hostname is never trusted).
///
/// ANTI-REBINDING PIN: the name is resolved exactly ONCE into a fixed address list, and
/// each address is SSRF-classified and connected to as the SAME `SocketAddr` — there is
/// no second resolution between the check and the connect, so DNS cannot swap in a
/// metadata IP after validation. A resolved address on the blocked surface is skipped
/// (fail-closed); a host that resolves ONLY to blocked addresses yields the block error.
/// `allow_private` (derived from the policy's `<private>` opt-in) governs only the
/// private-range tier; the metadata/link-local block is unconditional.
fn connect_upstream(host: &Host, port: u16, allow_private: bool) -> io::Result<TcpStream> {
    let addrs: Vec<SocketAddr> = match host {
        Host::Ip(ip) => vec![SocketAddr::new(*ip, port)],
        Host::Name(name) => (name.as_str(), port).to_socket_addrs()?.collect(),
    };
    let mut last_err = io::Error::other("no address resolved");
    for addr in addrs {
        if is_blocked_egress_ip(addr.ip(), allow_private) {
            last_err = io::Error::new(
                io::ErrorKind::PermissionDenied,
                "egress to a metadata/link-local or private-range address is blocked",
            );
            continue;
        }
        match TcpStream::connect_timeout(&addr, UPSTREAM_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Blind bidirectional forward. One thread copies client→upstream; this thread copies
/// upstream→client. Each direction shuts down the peer's write half on EOF so the
/// other copy unblocks and the tunnel tears down cleanly.
fn splice(client: TcpStream, upstream: TcpStream, shutdown: Arc<AtomicBool>) {
    let Ok(mut client_rd) = client.try_clone() else {
        return;
    };
    let Ok(mut up_wr) = upstream.try_clone() else {
        return;
    };
    let sh = shutdown.clone();
    let c2u = std::thread::spawn(move || {
        pump(&mut client_rd, &mut up_wr, &sh);
        let _ = up_wr.shutdown(Shutdown::Write);
    });
    let mut up_rd = upstream;
    let mut client_wr = client;
    pump(&mut up_rd, &mut client_wr, &shutdown);
    let _ = client_wr.shutdown(Shutdown::Write);
    let _ = c2u.join();
}

/// One direction of [`splice`]: copy until EOF, an IO error, or teardown.
///
/// The `io::copy` this replaces cannot be interrupted; the timeout+flag loop is what
/// makes a blocked forwarder reachable by [`EgressProxy::drop`] on Windows (see
/// [`SPLICE_POLL`]). A timeout is NOT end-of-stream, so the loop re-checks the flag and
/// retries. `shutdown` is written by nothing but teardown, so an exit on it only ever
/// abandons a tunnel `drop` is concurrently tearing down anyway.
fn pump(from: &mut TcpStream, to: &mut TcpStream, shutdown: &AtomicBool) {
    if from.set_read_timeout(Some(SPLICE_POLL)).is_err()
        || to.set_write_timeout(Some(SPLICE_POLL)).is_err()
    {
        return;
    }
    let mut buf = [0u8; 16 * 1024];
    while !shutdown.load(Ordering::SeqCst) {
        match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if !send_all(to, &buf[..n], shutdown) {
                    return;
                }
            }
            Err(e) if is_timeout(&e) || e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// The send half of [`pump`], interruptible on the same tick. `write_all` cannot be used:
/// it reports no byte count on error, so retrying it after a partial timed-out write
/// would duplicate bytes into the tunnel — tracking the offset off each `write` is what
/// makes the retry safe.
fn send_all(to: &mut TcpStream, buf: &[u8], shutdown: &AtomicBool) -> bool {
    let mut sent = 0;
    while sent < buf.len() {
        if shutdown.load(Ordering::SeqCst) {
            return false;
        }
        match to.write(&buf[sent..]) {
            Ok(0) => return false,
            Ok(n) => sent += n,
            Err(e) if is_timeout(&e) || e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{CredentialBroker, Effect, NetRule, NetTarget};

    fn net(rules: Vec<NetRule>, default_effect: Effect) -> NetPolicy {
        NetPolicy {
            enforce: true,
            rules,
            default_effect,
            ..Default::default()
        }
    }
    fn host(pat: &str, effect: Effect) -> NetRule {
        NetRule {
            target: NetTarget::Host(pat.to_string()),
            effect,
        }
    }

    #[test]
    fn static_decider_matches_host_glob_and_cidr() {
        let policy = net(
            vec![
                host("*.allowed.example", Effect::Allow),
                NetRule {
                    target: NetTarget::Cidr("10.0.0.0/8".parse().unwrap()),
                    effect: Effect::Allow,
                },
            ],
            Effect::Deny,
        );
        let d = StaticDecider::new(policy);
        assert_eq!(
            d.decide(&Host::Name("api.allowed.example".into())),
            Decision::Allow
        );
        assert_eq!(
            d.decide(&Host::Name("allowed.example".into())),
            Decision::Deny // `*.` is subdomains-only — the apex is NOT matched
        );
        assert_eq!(d.decide(&Host::Name("evil.example".into())), Decision::Deny);
        assert_eq!(
            d.decide(&Host::Ip("10.1.2.3".parse().unwrap())),
            Decision::Allow
        );
        assert_eq!(
            d.decide(&Host::Ip("8.8.8.8".parse().unwrap())),
            Decision::Deny
        );
    }

    #[test]
    fn blocks_metadata_and_link_local_egress() {
        // The HARD tier — always blocked, regardless of the private opt-in.
        let hard = [
            "169.254.169.254",        // AWS/GCP/Azure IMDS (IPv4 link-local)
            "169.254.0.1",            // link-local edge
            "fe80::1",                // IPv6 link-local
            "fe80::a9fe:a9fe",        // IPv6 link-local, arbitrary suffix
            "febf::1",                // fe80::/10 upper edge
            "fd00:ec2::254",          // AWS IPv6 IMDS (inside ULA, but hard-blocked)
            "::ffff:169.254.169.254", // IPv4-mapped metadata (encoding smuggle)
            "::169.254.169.254",      // IPv4-compat metadata (encoding smuggle)
        ];
        for ip in hard {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(
                is_blocked_egress_ip(ip, false),
                "{ip} must be blocked (private disallowed)"
            );
            assert!(
                is_blocked_egress_ip(ip, true),
                "{ip} must STAY blocked even with the private opt-in (hard tier)"
            );
        }
        // Loopback + public are in NEITHER tier — always reachable.
        for ip in [
            "127.0.0.1",
            "::1",
            "8.8.8.8",
            "203.0.113.10",
            "2606:4700:4700::1111",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!is_blocked_egress_ip(ip, false), "{ip} must be reachable");
            assert!(!is_blocked_egress_ip(ip, true), "{ip} must be reachable");
        }
    }

    #[test]
    fn rfc1918_and_ula_blocked_by_default_but_opt_in_reachable() {
        // The PRIVATE tier: blocked when `<private>` is NOT opted in, reachable when it is.
        let private = [
            "10.0.0.1",           // 10/8
            "10.255.255.254",     // 10/8 edge
            "172.16.0.1",         // 172.16/12 lower edge
            "172.31.255.254",     // 172.16/12 upper edge
            "192.168.1.1",        // 192.168/16
            "fc00::1",            // ULA lower edge
            "fdff:ffff::1",       // ULA upper edge (fc00::/7)
            "::ffff:10.0.0.1",    // IPv4-mapped RFC1918 (encoding smuggle)
            "::ffff:192.168.0.1", // IPv4-mapped RFC1918
        ];
        for ip in private {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(
                is_blocked_egress_ip(ip, false),
                "{ip} must be BLOCKED by default (no <private> opt-in)"
            );
            assert!(
                !is_blocked_egress_ip(ip, true),
                "{ip} must be REACHABLE once <private> is opted in"
            );
        }
        // Boundary negatives: 172.15/172.32 are OUTSIDE 172.16/12 → public, never private.
        for ip in ["172.15.255.255", "172.32.0.0", "11.0.0.1", "192.167.0.1"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(
                !is_blocked_egress_ip(ip, false),
                "{ip} is public, not private"
            );
        }
    }

    #[test]
    fn connect_upstream_blocks_by_tier_but_reaches_allowed_target() {
        // Negative control: an allowed (non-blocked) target actually connects.
        let echo = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0)).unwrap();
        let port = echo.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = echo.accept();
        });
        assert!(
            connect_upstream(&Host::Ip(IpAddr::from([127, 0, 0, 1])), port, false).is_ok(),
            "loopback must still connect through the guard"
        );

        // Metadata is denied immediately (PermissionDenied), even with the private opt-in.
        let err = connect_upstream(&Host::Ip("169.254.169.254".parse().unwrap()), 80, true)
            .expect_err("metadata egress must be blocked even with <private>");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        // RFC1918 is denied without the opt-in, allowed with it.
        let err = connect_upstream(&Host::Ip("192.168.1.1".parse().unwrap()), 80, false)
            .expect_err("RFC1918 egress must be blocked by default");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn allows_private_only_on_explicit_target_not_wildcard() {
        // A bare `*` allow-all does NOT re-open the private ranges.
        let d = StaticDecider::new(net(vec![host("*", Effect::Allow)], Effect::Deny));
        assert!(!d.allows_private(), "`*` must NOT set the private opt-in");

        // An explicit `<private>` allow does.
        let priv_rule = |e| NetRule {
            target: NetTarget::Private,
            effect: e,
        };
        let d = StaticDecider::new(net(vec![priv_rule(Effect::Allow)], Effect::Deny));
        assert!(d.allows_private(), "`<private>` must set the opt-in");

        // Last-match-wins: `<private>` then `!<private>` turns it back off.
        let d = StaticDecider::new(net(
            vec![priv_rule(Effect::Allow), priv_rule(Effect::Deny)],
            Effect::Deny,
        ));
        assert!(
            !d.allows_private(),
            "a later `!<private>` must reclose the opt-in"
        );

        // The `<private>` target admits an RFC1918 IP literal at gate 1 (matcher).
        let d = StaticDecider::new(net(vec![priv_rule(Effect::Allow)], Effect::Deny));
        assert_eq!(
            d.decide(&Host::Ip("10.1.2.3".parse().unwrap())),
            Decision::Allow
        );
        assert_eq!(
            d.decide(&Host::Ip("8.8.8.8".parse().unwrap())),
            Decision::Deny,
            "a public IP is NOT admitted by <private>"
        );
    }

    #[test]
    fn static_decider_last_match_wins() {
        // `["*", "!*.evil.example"]`: allow-all then deny a subtree.
        let policy = net(
            vec![
                host("*", Effect::Allow),
                host("*.evil.example", Effect::Deny),
            ],
            Effect::Deny,
        );
        let d = StaticDecider::new(policy);
        assert_eq!(d.decide(&Host::Name("ok.example".into())), Decision::Allow);
        assert_eq!(
            d.decide(&Host::Name("x.evil.example".into())),
            Decision::Deny
        );
    }

    #[test]
    fn live_tls_broker_releases_marker_only_to_the_exact_verified_upstream() {
        use base64::Engine as _;
        use rcgen::{CertifiedKey, generate_simple_self_signed};
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
        use std::collections::BTreeMap;

        const SECRET: &str = "real-secret-never-given-to-client";
        let configured = [CredentialBroker {
            host: "localhost".to_string(),
            env: vec!["API_TOKEN".to_string()],
        }];
        let session =
            mitm::BrokerSession::from_policy(&configured, |_| Ok(Some(SECRET.to_string())))
                .unwrap();
        let mut child_env = BTreeMap::new();
        session.install_markers(&mut child_env);
        let marker = child_env["API_TOKEN"].clone();
        assert_ne!(marker, SECRET);

        // A real local TLS upstream with its own root. The proxy must verify this cert;
        // the child separately trusts only the proxy's ephemeral CA.
        let CertifiedKey { cert, key_pair } =
            generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let upstream_root = cert.der().clone();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let upstream_config = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der())),
            )
            .unwrap();
        let listener = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0)).unwrap();
        let upstream_port = listener.local_addr().unwrap().port();
        let marker_for_upstream = marker.clone();
        let upstream = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut conn = rustls::ServerConnection::new(Arc::new(upstream_config)).unwrap();
            let mut tls = rustls::Stream::new(&mut conn, &mut socket);
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                tls.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() <= 64 * 1024);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.contains(&format!("Authorization: Bearer {SECRET}\r\n")));
            assert!(!request.contains(&marker_for_upstream));
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
            tls.conn.send_close_notify();
            tls.flush().unwrap();
        });

        let engine = mitm::MitmEngine::new_for_test(session.into_brokers(), upstream_root)
            .expect("test MITM engine");
        let mut child_roots = rustls::RootCertStore::empty();
        child_roots.add(engine.child_ca_der()).unwrap();
        let mut child_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(child_roots)
            .with_no_client_auth();
        child_config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let policy = net(vec![host("localhost", Effect::Allow)], Effect::Deny);
        let proxy = EgressProxy::start(
            Arc::new(StaticDecider::new(policy)),
            Some(Arc::clone(&engine)),
        )
        .unwrap();
        let auth = base64::engine::general_purpose::STANDARD.encode(format!("{}:", proxy.token()));
        let mut socket = TcpStream::connect((IpAddr::from([127, 0, 0, 1]), proxy.port())).unwrap();
        write!(
            socket,
            "CONNECT localhost:{upstream_port} HTTP/1.1\r\nHost: localhost:{upstream_port}\r\nProxy-Authorization: Basic {auth}\r\n\r\n"
        )
        .unwrap();
        let mut connect_reply = Vec::new();
        let mut byte = [0u8; 1];
        while !connect_reply.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).unwrap();
            connect_reply.push(byte[0]);
        }
        assert!(connect_reply.starts_with(b"HTTP/1.1 200"));

        let name = ServerName::try_from("localhost").unwrap();
        let mut conn = rustls::ClientConnection::new(Arc::new(child_config), name).unwrap();
        let mut tls = rustls::Stream::new(&mut conn, &mut socket);
        write!(
            tls,
            "GET / HTTP/1.1\r\nHost: localhost:{upstream_port}\r\nAuthorization: Bearer {marker}\r\n\r\n"
        )
        .unwrap();
        tls.flush().unwrap();
        let mut response = Vec::new();
        tls.read_to_end(&mut response).unwrap();
        assert!(response.ends_with(b"\r\n\r\nok"));

        drop(proxy);
        upstream.join().unwrap();
    }
}
