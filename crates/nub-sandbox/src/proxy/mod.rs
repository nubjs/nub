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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
            last.map(|e| format!(" (last error: {e})")).unwrap_or_default()
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
        let sh = shutdown.clone();
        let engine = mitm.clone();
        let tok = token.clone();
        let accept_thread = std::thread::Builder::new()
            .name("nub-egress-proxy".into())
            .spawn(move || accept_loop(listener, sh, decider, engine, tok))?;
        Ok(EgressProxy {
            port,
            token,
            shutdown,
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
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        // Signal the accept loop, then wake its blocked `accept()` with a throwaway
        // self-connection so it observes the flag and exits. In-flight tunnel threads
        // are detached; they end when their sockets close (the child is already gone).
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect((IpAddr::from([127, 0, 0, 1]), self.port));
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
    }
}

/// Accept loop: one detached handler thread per connection. Any handler error just
/// closes that connection — a single malformed client never takes down the proxy.
fn accept_loop(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    decider: Arc<dyn GrantDecider>,
    mitm: Option<Arc<mitm::MitmEngine>>,
    token: Arc<str>,
) {
    for conn in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = conn else { continue };
        let d = decider.clone();
        let m = mitm.clone();
        let tok = token.clone();
        // Best-effort spawn; if the OS refuses a thread we simply drop the connection
        // (fail-closed — no unproxied path opens).
        let _ = std::thread::Builder::new()
            .name("nub-egress-tunnel".into())
            .spawn(move || {
                let _ = handle_conn(stream, d, m, &tok);
            });
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
) -> io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_HELLO_TIMEOUT))?;
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
                let _ = mitm::terminate(engine, stream, prelude, host, req.port);
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
    let upstream = connect_upstream(&req.host, req.port)?;
    stream.set_read_timeout(None)?;
    upstream.set_read_timeout(None)?;
    let mut up = upstream;
    if !prelude.is_empty() {
        up.write_all(&prelude)?;
    }
    splice(stream, up);
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
/// SSRF / DNS-rebinding guard. An allowed hostname that resolves — or an attacker's DNS
/// rebinds — to the cloud-metadata / link-local surface is refused at the connect. It
/// covers IPv4 link-local `169.254.0.0/16` (incl. the `169.254.169.254` IMDS endpoint),
/// IPv6 link-local `fe80::/10`, and the AWS IPv6 IMDS `fd00:ec2::254`; an IPv4-in-IPv6
/// form (`::ffff:169.254.169.254`, `::169.254.169.254`) is unmapped to its embedded v4
/// FIRST so the encoding can't smuggle a metadata address past as an IPv6 literal. All
/// integer/octal/hex host encodings are already normalized away here because we classify
/// the RESOLVED [`IpAddr`], not the child-supplied token. Loopback is deliberately NOT
/// blocked (the proxy's own carve is loopback, and a legit upstream may be); broad RFC1918
/// private-range blocking is a separate maintainer posture call (see LIMITATIONS.md).
fn is_blocked_egress_ip(ip: IpAddr) -> bool {
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
fn connect_upstream(host: &Host, port: u16) -> io::Result<TcpStream> {
    let addrs: Vec<SocketAddr> = match host {
        Host::Ip(ip) => vec![SocketAddr::new(*ip, port)],
        Host::Name(name) => (name.as_str(), port).to_socket_addrs()?.collect(),
    };
    let mut last_err = io::Error::other("no address resolved");
    for addr in addrs {
        if is_blocked_egress_ip(addr.ip()) {
            last_err = io::Error::new(
                io::ErrorKind::PermissionDenied,
                "egress to a link-local/metadata address is blocked",
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
fn splice(client: TcpStream, upstream: TcpStream) {
    let Ok(mut client_rd) = client.try_clone() else {
        return;
    };
    let Ok(mut up_wr) = upstream.try_clone() else {
        return;
    };
    let c2u = std::thread::spawn(move || {
        let _ = io::copy(&mut client_rd, &mut up_wr);
        let _ = up_wr.shutdown(Shutdown::Write);
    });
    let mut up_rd = upstream;
    let mut client_wr = client;
    let _ = io::copy(&mut up_rd, &mut client_wr);
    let _ = client_wr.shutdown(Shutdown::Write);
    let _ = c2u.join();
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
    use crate::policy::{Effect, NetRule, NetTarget};

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
            Decision::Allow // apex matches *.allowed.example
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
        let blocked = [
            "169.254.169.254",        // AWS/GCP/Azure IMDS (IPv4 link-local)
            "169.254.0.1",            // link-local edge
            "fe80::1",                // IPv6 link-local
            "fe80::a9fe:a9fe",        // IPv6 link-local, arbitrary suffix
            "febf::1",                // fe80::/10 upper edge
            "fd00:ec2::254",          // AWS IPv6 IMDS
            "::ffff:169.254.169.254", // IPv4-mapped metadata (encoding smuggle)
            "::169.254.169.254",      // IPv4-compat metadata (encoding smuggle)
        ];
        for ip in blocked {
            assert!(
                is_blocked_egress_ip(ip.parse().unwrap()),
                "{ip} must be classified as blocked egress"
            );
        }
        // NOT blocked: loopback (the proxy carve + loopback upstreams), public, and —
        // deliberately, pending the maintainer posture call — RFC1918 private ranges.
        let allowed = [
            "127.0.0.1",
            "::1",
            "8.8.8.8",
            "203.0.113.10",
            "2606:4700:4700::1111",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1", // RFC1918: NOT blocked in this change
        ];
        for ip in allowed {
            assert!(
                !is_blocked_egress_ip(ip.parse().unwrap()),
                "{ip} must NOT be classified as blocked egress"
            );
        }
    }

    #[test]
    fn connect_upstream_denies_link_local_but_reaches_allowed_target() {
        // Negative control: an allowed (non-blocked) target actually connects.
        let echo = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0)).unwrap();
        let port = echo.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = echo.accept();
        });
        assert!(
            connect_upstream(&Host::Ip(IpAddr::from([127, 0, 0, 1])), port).is_ok(),
            "an allowed target must still connect through the guard"
        );

        // The guard denies a metadata target immediately (PermissionDenied), without
        // attempting the connect — so a live metadata endpoint would never be reached.
        let err = connect_upstream(&Host::Ip("169.254.169.254".parse().unwrap()), 80)
            .expect_err("link-local egress must be blocked");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
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
}
