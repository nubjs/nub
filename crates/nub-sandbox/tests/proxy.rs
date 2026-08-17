//! Egress proxy — host-runnable integration tests (no OS sandbox needed).
//!
//! Each test starts a real [`EgressProxy`] on a loopback port, drives a real HTTP
//! CONNECT or SOCKS5 client through it, and asserts allowed tunnels forward while
//! denied ones drop — including the SNI gate (a denied SNI to an admitted target IP
//! is dropped). Upstreams are throwaway loopback echo servers, so the whole matrix is
//! hermetic; no external host is contacted. The "ClientHello" is a well-formed SNI
//! byte blob (the proxy does NOT terminate TLS, so the echo server just reflects it).

use base64::Engine;
use nub_sandbox::StaticDecider;
use nub_sandbox::policy::{Effect, NetPolicy, NetRule, NetTarget};
use nub_sandbox::proxy::{Decision, EgressProxy, GrantDecider, Host};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The `Proxy-Authorization: Basic <b64(token:)>` header line (token as username, empty
/// password — the shape the child's `HTTP_PROXY` URL userinfo produces).
fn basic_auth(token: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
    format!("Proxy-Authorization: Basic {b64}\r\n")
}

// ── throwaway upstream: a loopback echo server ──────────────────────────────────

/// Start a loopback server that floods each connection with more bytes than any socket
/// buffer pair can hold, so a client that never reads wedges the proxy's forwarder in a
/// blocking send. Ignores what it is sent; the accept thread is detached.
fn flood_server() -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            std::thread::spawn(move || {
                let chunk = vec![0xABu8; 64 * 1024];
                for _ in 0..512 {
                    if s.write_all(&chunk).is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

/// Start a loopback echo server that reflects bytes on each connection until EOF.
/// Returns its address; the accept thread is detached (dies with the test process).
fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

// ── a well-formed ClientHello carrying a chosen SNI ─────────────────────────────

fn client_hello(sni: &str) -> Vec<u8> {
    let host = sni.as_bytes();
    let mut sn = vec![0x00]; // name_type host_name
    sn.extend_from_slice(&(host.len() as u16).to_be_bytes());
    sn.extend_from_slice(host);
    let mut list = Vec::new();
    list.extend_from_slice(&(sn.len() as u16).to_be_bytes());
    list.extend_from_slice(&sn);
    let mut exts = Vec::new();
    exts.extend_from_slice(&0x0000u16.to_be_bytes()); // server_name ext
    exts.extend_from_slice(&(list.len() as u16).to_be_bytes());
    exts.extend_from_slice(&list);

    let mut body = Vec::new();
    body.extend_from_slice(&[0x03, 0x03]); // version
    body.extend_from_slice(&[0u8; 32]); // random
    body.push(0); // session id
    body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher suites
    body.extend_from_slice(&[0x01, 0x00]); // compression
    body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
    body.extend_from_slice(&exts);

    let mut hs = vec![0x01]; // ClientHello
    let l = body.len();
    hs.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8]);
    hs.extend_from_slice(&body);

    // one TLS record
    let mut rec = vec![0x16, 0x03, 0x01];
    rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
    rec.extend_from_slice(&hs);
    rec
}

// ── proxy client helpers ────────────────────────────────────────────────────────

/// HTTP CONNECT to `target` (a `host:port` authority) through the proxy, presenting
/// `token` as the Basic proxy credential. Returns the tunnel stream after the `200` ACK,
/// or the response's status line on a non-2xx (e.g. `407`/`403`).
fn http_connect(proxy_port: u16, target: &str, token: &str) -> Result<TcpStream, String> {
    let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{}\r\n",
        basic_auth(token)
    )
    .unwrap();
    let mut resp = Vec::new();
    let mut one = [0u8; 1];
    loop {
        match s.read(&mut one) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                resp.push(one[0]);
                if resp.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    let head = String::from_utf8_lossy(&resp);
    if head.starts_with("HTTP/1.1 200") {
        Ok(s)
    } else {
        Err(head.lines().next().unwrap_or("").to_string())
    }
}

/// SOCKS5 CONNECT to an IPv4 `addr` through the proxy, authenticating with `token` via
/// RFC 1929 user/pass (token as the username, empty password). Returns the tunnel stream
/// after a success reply, or `Err` on a non-success request reply.
fn socks5_connect_ip(proxy_port: u16, addr: SocketAddr, token: &str) -> Result<TcpStream, u8> {
    let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(&[0x05, 0x01, 0x02]).unwrap(); // greeting: 1 method (username/password)
    let mut sel = [0u8; 2];
    s.read_exact(&mut sel).unwrap();
    assert_eq!(
        sel,
        [0x05, 0x02],
        "proxy must select username/password auth"
    );
    // RFC 1929 sub-negotiation: token as username, empty password.
    let mut auth = vec![0x01, token.len() as u8];
    auth.extend_from_slice(token.as_bytes());
    auth.push(0x00);
    s.write_all(&auth).unwrap();
    let mut ar = [0u8; 2];
    s.read_exact(&mut ar).unwrap();
    assert_eq!(ar, [0x01, 0x00], "proxy must accept the token");
    let ip = match addr.ip() {
        std::net::IpAddr::V4(v4) => v4.octets(),
        _ => panic!("ipv4 only"),
    };
    let mut req = vec![0x05, 0x01, 0x00, 0x01];
    req.extend_from_slice(&ip);
    req.extend_from_slice(&addr.port().to_be_bytes());
    s.write_all(&req).unwrap();
    let mut rep = [0u8; 10];
    s.read_exact(&mut rep).unwrap();
    if rep[1] == 0x00 { Ok(s) } else { Err(rep[1]) }
}

/// Send a ClientHello with `sni` over an established tunnel and report whether the
/// upstream echo reflected it (i.e. the tunnel forwarded, not dropped).
fn tunnel_forwards(stream: &mut TcpStream, sni: &str) -> bool {
    let hello = client_hello(sni);
    if stream.write_all(&hello).is_err() {
        return false;
    }
    let mut got = vec![0u8; hello.len()];
    read_full(stream, &mut got)
        .map(|()| got == hello)
        .unwrap_or(false)
}

/// Read exactly `buf.len()` bytes or fail (EOF/timeout on a dropped tunnel → Err).
fn read_full(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), ()> {
    let mut off = 0;
    while off < buf.len() {
        match stream.read(&mut buf[off..]) {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => off += n,
        }
    }
    Ok(())
}

// ── policy helpers ──────────────────────────────────────────────────────────────

fn net(rules: Vec<NetRule>) -> NetPolicy {
    NetPolicy {
        enforce: true,
        rules,
        default_effect: Effect::Deny,
        ..Default::default()
    }
}
fn allow_host(pat: &str) -> NetRule {
    NetRule {
        target: NetTarget::Host(pat.to_string()),
        effect: Effect::Allow,
    }
}
fn allow_cidr(cidr: &str) -> NetRule {
    NetRule {
        target: NetTarget::Cidr(cidr.parse().unwrap()),
        effect: Effect::Allow,
    }
}

fn start(policy: NetPolicy) -> EgressProxy {
    EgressProxy::start(Arc::new(StaticDecider::new(policy)), None).unwrap()
}

// ── tests ────────────────────────────────────────────────────────────────────────

#[test]
fn http_connect_allowed_host_forwards() {
    let upstream = echo_server();
    // Allow the loopback CIDR (gate 1: target IP) AND the SNI host glob (gate 2).
    let proxy = start(net(vec![
        allow_cidr("127.0.0.0/8"),
        allow_host("*.allowed.example"),
    ]));
    let mut t = http_connect(
        proxy.port(),
        &format!("127.0.0.1:{}", upstream.port()),
        proxy.token(),
    )
    .unwrap();
    assert!(
        tunnel_forwards(&mut t, "api.allowed.example"),
        "an allowed SNI to an admitted target must forward end-to-end"
    );
}

#[test]
fn http_connect_denied_sni_drops() {
    let upstream = echo_server();
    // Target IP admitted (gate 1), but the SNI is NOT on the allow-list (gate 2).
    let proxy = start(net(vec![
        allow_cidr("127.0.0.0/8"),
        allow_host("*.allowed.example"),
    ]));
    let mut t = http_connect(
        proxy.port(),
        &format!("127.0.0.1:{}", upstream.port()),
        proxy.token(),
    )
    .unwrap();
    assert!(
        !tunnel_forwards(&mut t, "evil.example"),
        "a denied SNI must be dropped even when the target IP is admitted (shared-IP guard)"
    );
}

#[test]
fn http_connect_denied_target_host_refused_before_ack() {
    let upstream = echo_server();
    // Only a host glob is allowed; the loopback IP target is NOT admitted → gate 1
    // refuses with a non-200 before any tunnel is established.
    let proxy = start(net(vec![allow_host("*.allowed.example")]));
    let err = http_connect(
        proxy.port(),
        &format!("127.0.0.1:{}", upstream.port()),
        proxy.token(),
    )
    .unwrap_err();
    assert!(
        err.contains("403"),
        "denied target must get a 403, got {err:?}"
    );
}

#[test]
fn http_connect_hostname_target_resolves_and_forwards() {
    // The hostname path: `localhost` is allowed + resolves to loopback; the proxy owns
    // DNS. SNI `localhost` also admitted.
    let upstream = echo_server();
    let proxy = start(net(vec![allow_host("localhost")]));
    let mut t = http_connect(
        proxy.port(),
        &format!("localhost:{}", upstream.port()),
        proxy.token(),
    )
    .unwrap();
    assert!(
        tunnel_forwards(&mut t, "localhost"),
        "an allowed hostname target must resolve and forward"
    );
}

#[test]
fn socks5_allowed_forwards_denied_sni_drops() {
    let upstream = echo_server();
    let proxy = start(net(vec![
        allow_cidr("127.0.0.0/8"),
        allow_host("*.allowed.example"),
    ]));
    // allowed SNI over SOCKS5
    let mut ok = socks5_connect_ip(proxy.port(), upstream, proxy.token()).unwrap();
    assert!(
        tunnel_forwards(&mut ok, "cdn.allowed.example"),
        "socks5 allow forwards"
    );
    // denied SNI over SOCKS5 → dropped
    let mut bad = socks5_connect_ip(proxy.port(), upstream, proxy.token()).unwrap();
    assert!(
        !tunnel_forwards(&mut bad, "evil.example"),
        "socks5 denied SNI drops"
    );
}

#[test]
fn socks5_denied_target_ip_gets_refusal_reply() {
    let upstream = echo_server();
    // No CIDR allowed → the loopback target IP is refused at the SOCKS request reply.
    let proxy = start(net(vec![allow_host("*.allowed.example")]));
    let rep = socks5_connect_ip(proxy.port(), upstream, proxy.token()).unwrap_err();
    assert_eq!(rep, 0x02, "SOCKS5 refusal REP=2 (not allowed by ruleset)");
}

#[test]
fn non_tls_stream_to_admitted_target_forwards() {
    // A non-TLS payload (first byte != 0x16) to an admitted target has no SNI to
    // cross-route on → forwarded. Proves NotTls admits (not fail-closed).
    let upstream = echo_server();
    let proxy = start(net(vec![allow_cidr("127.0.0.0/8")]));
    let mut t = http_connect(
        proxy.port(),
        &format!("127.0.0.1:{}", upstream.port()),
        proxy.token(),
    )
    .unwrap();
    let payload = b"PING plain-tcp\n";
    t.write_all(payload).unwrap();
    let mut got = vec![0u8; payload.len()];
    assert!(read_full(&mut t, &mut got).is_ok() && got == payload);
}

#[test]
fn stalled_tls_tunnel_fails_closed() {
    // Client ACKs then sends the START of a TLS record but never completes the
    // ClientHello (a partial handshake). The proxy must NOT splice — it fails closed
    // rather than let a later denied-SNI cross-route. We assert the tunnel is dropped
    // (the read side closes without echoing our partial bytes).
    let _upstream = echo_server();
    let proxy = start(net(vec![
        allow_cidr("127.0.0.0/8"),
        allow_host("*.allowed.example"),
    ]));
    let mut t = http_connect(
        proxy.port(),
        &format!("127.0.0.1:{}", _upstream.port()),
        proxy.token(),
    )
    .unwrap();
    t.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    // A handshake record header claiming a large body, then nothing more.
    t.write_all(&[0x16, 0x03, 0x01, 0x02, 0x00, 0x01, 0x00])
        .unwrap();
    let mut got = [0u8; 16];
    // With no complete ClientHello, the proxy waits (up to its own timeout) then
    // drops — the client read returns 0/err, never an echo of our bytes.
    let dropped = matches!(t.read(&mut got), Ok(0) | Err(_));
    assert!(
        dropped,
        "a stalled TLS tunnel must fail closed (be dropped)"
    );
}

#[test]
fn decider_seam_is_consulted_for_target_and_sni() {
    // A recording decider proves BOTH gates fire: the CONNECT target AND the SNI are
    // each passed to the callback seam (the interactive-prompt swap point).
    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<String>>,
    }
    impl GrantDecider for Recorder {
        fn decide(&self, host: &Host) -> Decision {
            let key = match host {
                Host::Name(n) => n.clone(),
                Host::Ip(ip) => ip.to_string(),
            };
            self.seen.lock().unwrap().push(key.clone());
            // Allow the loopback target + the allowed SNI; deny everything else.
            if key == "127.0.0.1" || key == "keep.allowed.example" {
                Decision::Allow
            } else {
                Decision::Deny
            }
        }
    }
    let upstream = echo_server();
    let rec = Arc::new(Recorder::default());
    let proxy = EgressProxy::start(rec.clone(), None).unwrap();
    let mut t = http_connect(
        proxy.port(),
        &format!("127.0.0.1:{}", upstream.port()),
        proxy.token(),
    )
    .unwrap();
    assert!(tunnel_forwards(&mut t, "keep.allowed.example"));
    let seen = rec.seen.lock().unwrap().clone();
    assert!(
        seen.contains(&"127.0.0.1".to_string()),
        "target host consulted"
    );
    assert!(
        seen.contains(&"keep.allowed.example".to_string()),
        "SNI consulted via the same seam"
    );
}

#[test]
fn dropping_proxy_stops_the_listener() {
    let proxy = start(net(vec![allow_cidr("127.0.0.0/8")]));
    let port = proxy.port();
    // Reachable while alive.
    assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
    drop(proxy);
    // After drop the listener is closed; a connect now fails (give the accept thread a
    // moment to unwind).
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "the proxy port must be closed after the handle drops"
    );
}

#[test]
fn dropping_the_proxy_completes_while_a_tunnel_is_wedged() {
    // Teardown must not depend on the child cooperating. A sandboxed child picks which
    // direction it wedges: stop reading and the upstream→client forwarder blocks in
    // `send`; open a tunnel and go silent and the client→upstream one blocks in `recv`.
    // Windows wakes neither via `shutdown()`, so `EgressProxy::drop` — which joins every
    // handler — hangs unless the forwarder bounds itself. Both are exercised here at once.
    //
    // The watchdog matters as much as the scenario: asserting on a bounded channel recv
    // rather than letting `drop` block means a regression FAILS this test instead of
    // hanging the whole Windows leg for an hour, which is what the original defect did.
    let upstream = flood_server();
    let proxy = start(net(vec![
        allow_cidr("127.0.0.0/8"),
        allow_host("*.allowed.example"),
    ]));
    let target = format!("127.0.0.1:{}", upstream.port());

    // Wedge the send side: ask for the flood, then never read a byte of it.
    let mut greedy = http_connect(proxy.port(), &target, proxy.token()).unwrap();
    greedy
        .write_all(&client_hello("api.allowed.example"))
        .unwrap();
    // Wedge the recv side: a spliced tunnel whose client simply says nothing more.
    let mut silent = http_connect(proxy.port(), &target, proxy.token()).unwrap();
    silent.write_all(b"PING plain-tcp\n").unwrap();
    // Let the flood fill both socket buffers so the forwarder is genuinely blocked in a
    // send rather than merely idle — otherwise this asserts nothing.
    std::thread::sleep(Duration::from_secs(1));

    let (done, wait) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(proxy);
        let _ = done.send(());
    });
    assert!(
        wait.recv_timeout(Duration::from_secs(30)).is_ok(),
        "dropping the proxy must complete even with both splice directions blocked \
         (a wedged tunnel must not deadlock EgressProxy::drop's join)"
    );
    // Keep both tunnels open across the drop — the point is that the peers never help.
    drop((greedy, silent));
}

// ── per-session token gate (defense-in-depth) ──────────────────────────────────────

#[test]
fn http_connect_without_token_is_rejected_with_407() {
    // A co-resident same-user process that does NOT know the token cannot use the proxy:
    // a CONNECT with no Proxy-Authorization is answered 407 and dropped — BEFORE the
    // (admitted) target host is consulted.
    let upstream = echo_server();
    let proxy = start(net(vec![allow_cidr("127.0.0.0/8")]));
    let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: x\r\n\r\n",
        upstream.port()
    )
    .unwrap();
    let mut resp = Vec::new();
    let _ = s.read_to_end(&mut resp);
    assert!(
        String::from_utf8_lossy(&resp).starts_with("HTTP/1.1 407"),
        "a tokenless CONNECT to an admitted target must be refused 407, got {:?}",
        String::from_utf8_lossy(&resp)
    );
}

#[test]
fn http_connect_with_wrong_token_is_rejected() {
    // A wrong token is refused exactly like a missing one (no oracle for a near-miss).
    let upstream = echo_server();
    let proxy = start(net(vec![allow_cidr("127.0.0.0/8")]));
    let wrong = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let err = http_connect(
        proxy.port(),
        &format!("127.0.0.1:{}", upstream.port()),
        wrong,
    )
    .expect_err("a wrong token must be refused");
    assert!(err.contains("407"), "wrong token must get 407, got {err:?}");
}

#[test]
fn correct_token_still_forwards() {
    // Positive control paired with the two negatives: the CHILD's own token forwards.
    let upstream = echo_server();
    let proxy = start(net(vec![
        allow_cidr("127.0.0.0/8"),
        allow_host("*.allowed.example"),
    ]));
    let mut t = http_connect(
        proxy.port(),
        &format!("127.0.0.1:{}", upstream.port()),
        proxy.token(),
    )
    .unwrap();
    assert!(
        tunnel_forwards(&mut t, "api.allowed.example"),
        "the correct token must forward end-to-end"
    );
}

#[test]
fn socks5_without_userpass_auth_is_refused() {
    // A SOCKS client offering only no-auth (0x00) gets `0x05 0xFF` (no acceptable method)
    // — the tokenless SOCKS path is closed just like the HTTP one.
    let proxy = start(net(vec![allow_cidr("127.0.0.0/8")]));
    let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(&[0x05, 0x01, 0x00]).unwrap(); // greeting: only no-auth offered
    let mut sel = [0u8; 2];
    let _ = s.read_exact(&mut sel);
    assert_eq!(
        sel,
        [0x05, 0xFF],
        "a no-auth-only SOCKS greeting must be refused"
    );
}

// ── the build jail's package-identity egress gate ───────────────────────────────

/// The net axis of the real build-jail policy — compiled through the production entry so
/// this exercises what a dependency lifecycle spawn actually gets, not a hand-built
/// stand-in that could drift from it.
///
/// `package` is the identity the gate turns on, and it is the ONLY thing it turns on: the
/// catalog decides whether the spawn gets coarse egress or none at all. There is no host
/// dimension left to vary, so a caller wanting the admitted policy just has to name an
/// admitted package.
fn build_jail_net(package: Option<&str>) -> NetPolicy {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let package_dir = project.join("node_modules/pkg");
    nub_sandbox::compile_build_jail(
        nub_sandbox::Homes {
            home: root.path().join("home"),
            tmp: root.path().join("tmp"),
            cache: root.path().join("cache"),
            project: project.clone(),
        },
        &package_dir,
        package,
        Some("1.0.0"),
        Vec::new(),
        Vec::new(),
        Default::default(),
    )
    .expect("compile build-jail")
    .net
}

#[test]
fn build_jail_egress_admits_any_host_and_does_not_gate_on_package_identity() {
    // The decider IS the proxy's gate (it consults this seam for both the CONNECT authority and
    // the SNI), so asserting on it covers both gates without needing any host to be reachable
    // from wherever the suite runs. It is also the seam that would show a per-host gate coming
    // back, which is why the coarse contract is pinned here rather than only at the compiler.
    //
    // `cypress` is catalogued, so this is the ADMITTED arm — and its grant is COARSE. Per-host was
    // withdrawn because only macOS could enforce it (Linux has no netns to route a child through,
    // Windows' loopback exemption is admin-only), so a list that gated the platform most
    // developers use meant an incomplete list erroring for them alone.
    let decider = StaticDecider::new(build_jail_net(Some("cypress")));
    let decide = |h: &str| decider.decide(&Host::Name(h.to_string()));

    for admitted in [
        // Hosts the withdrawn `$downloads` list carried: still reachable, as they always were.
        "nodejs.org",
        "binaries.prisma.sh",
        "cdn.cypress.io",
        // Hosts it did NOT carry — this is the behaviour change, stated plainly. The first was a
        // recorded refusal and the second an attacker-chosen label under a listed host; both are
        // now admitted for a catalogued package, because the host dimension no longer gates.
        // The catalog's per-package `hosts` arrays are retained as PROVENANCE for exactly this
        // reason: a changing list is a detection signal in a PR diff, not a runtime gate.
        "www.google-analytics.com",
        "leak.cdn.cypress.io",
        "evil.test",
    ] {
        assert_eq!(
            decide(admitted),
            Decision::Allow,
            "`{admitted}`: a catalogued package's grant is coarse, so every host passes. \
             A Deny here means per-host enforcement was restored"
        );
    }

    // ⛔⛔ THE IDENTITY DIFFERENTIAL NO LONGER EXISTS ON THIS AXIS, AND THIS BLOCK ASSERTED THAT IT DID.
    // It read "an uncatalogued package must reach nothing at all", which was the posture until
    // `4001cec5c5 sandbox: give an uncatalogued package a baseline grant instead of nothing`
    // (2026-08-16) set `baseline_caps().network = true`. `left-pad` — the Shai-Hulud shape, a package
    // no catalog admits — now takes the baseline and reaches every host, exactly like a catalogued one.
    //
    // So egress is NOT what the jail withholds from an unknown package, and no test in this file should
    // imply otherwise. The defense is on the FILESYSTEM axis: no read of the real `$HOME`, no write to
    // the project — i.e. the script cannot obtain anything worth exfiltrating, rather than being unable
    // to send it. `build_jail_enforcement.rs` carries that half.
    //
    // What is still worth pinning here is UNIFORMITY: the uncatalogued decision must match the
    // catalogued one host for host, so a future change that re-introduces per-host gating for either
    // one fails here instead of passing quietly.
    let unvetted = StaticDecider::new(build_jail_net(Some("left-pad")));
    for host in [
        "nodejs.org",
        "binaries.prisma.sh",
        "cdn.cypress.io",
        "evil.test",
    ] {
        assert_eq!(
            unvetted.decide(&Host::Name(host.to_string())),
            decide(host),
            "`{host}`: catalogued and uncatalogued must decide identically — the net axis does not \
             gate on package identity, and a difference here means per-host gating came back"
        );
    }
}

#[test]
fn a_deny_all_net_axis_refuses_every_tunnel() {
    // End-to-end through a real proxy carrying the real policy: the CONNECT is refused before any
    // tunnel exists. The second half is the one-variable control — the same client, the same
    // upstream, a policy that admits it — so the refusal above is the policy's doing and not a
    // probe that never connects.
    //
    // ⛔ THE DENY-ALL AXIS IS NOW BUILT DIRECTLY, BECAUSE NO PACKAGE IDENTITY PRODUCES ONE. This fed
    // `build_jail_net(Some("left-pad"))` and relied on an uncatalogued package compiling to deny-all;
    // since `4001cec5c5` (2026-08-16) it compiles to the baseline's coarse ALLOW, so the policy under
    // test admitted everything and the refusal could never happen. `net(vec![])` is deny-all by
    // construction — enforce on, no rules, default Deny — which is the thing this test is actually
    // about: that a deny-all axis refuses every CONNECT when fed through the real proxy the
    // `nub sandbox` path runs. In production no proxy is started for a build-jail policy at all
    // (coarse `net: true` derives `ProxyMode::Disabled`), so the sandbox path is the only consumer.
    let upstream = echo_server();
    let target = format!("127.0.0.1:{}", upstream.port());

    let jailed = start(net(vec![]));
    assert!(
        http_connect(jailed.port(), &target, jailed.token()).is_err(),
        "a deny-all net axis must not tunnel to any upstream"
    );

    // The control admits the SNI as well as the target: the proxy gates both, and only
    // the target-authority gate is what the assertion above turns on.
    let permissive = start(net(vec![
        allow_cidr("127.0.0.0/8"),
        allow_host("cdn.cypress.io"),
    ]));
    let mut tunnel = http_connect(permissive.port(), &target, permissive.token())
        .expect("control: the same upstream must tunnel under a policy that allows it");
    assert!(
        tunnel_forwards(&mut tunnel, "cdn.cypress.io"),
        "control: the tunnel must actually carry bytes"
    );
}
