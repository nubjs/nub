//! The TLS-termination (MITM) tier — credential brokering (proposal §5, cut-1 marquee).
//!
//! ENGAGED per-host, ONLY where a rule demands reading inside the stream (a
//! credential-inject rule), or globally under `proxy: "terminate"`. Everything else
//! stays a blind splice ([`super::splice`]) — the default is not "MITM off", it is "MITM
//! never instantiated": with no broker + Auto mode, [`MitmEngine`] does not exist and no
//! TLS/CA code runs.
//!
//! THE FLOW for a terminated host: mint a leaf for the SNI host → complete the TLS
//! handshake WITH THE CHILD (which trusts the ephemeral CA via its env bundle) → read the
//! one HTTP/1.1 request in cleartext → STRIP the child's copy of each brokered header and
//! INJECT the real secret (strip-then-set, so a child-supplied value never survives) →
//! open a REAL TLS connection to the upstream (verifying the real cert against the real
//! roots — nub is never itself MITM'd) → forward the modified request → relay the
//! response back. The child NEVER holds the secret: direct injection, not a sentinel —
//! there is no dummy token for the child to log, echo, or persist.
//!
//! FAIL-CLOSED everywhere: any handshake / parse / upstream / cert error drops the
//! connection (the child sees a reset). There is no path that forwards a request WITHOUT
//! its injection, and no path that injects over an unverified channel.
//!
//! CUT-1 FRAMING: one request per terminated connection, `Connection: close` forced —
//! the response's relayed `close` makes the child reconnect for its next request, so
//! every request is its own terminated connection and every one is injected. Keep-alive
//! and request pipelining are cut-1 non-goals; a chunked REQUEST body is refused
//! (fail-closed) rather than mis-framed.

use super::ca::MitmCa;
use crate::matcher::host::host_glob_matches;
use crate::policy::{CredentialBroker, HeaderInject};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

/// Cap on the buffered request head (request line + headers). Past this a client is
/// dribbling or hostile → fail closed.
const MAX_HEAD: usize = 64 * 1024;
/// Cap on a buffered request body nub will forward. Bodies are read whole to re-frame
/// with an accurate Content-Length, so the cap bounds PARENT memory across many child
/// connections — kept small (credential brokering targets API requests, not uploads);
/// a larger body fails closed. (Streaming the forward is the follow-up that lifts this.)
const MAX_BODY: usize = 1024 * 1024;

/// The MITM engine: the ephemeral CA, a reusable upstream-verifying client config, and
/// the compiled broker set. Built ONLY when the tier is TlsInspect. `Arc`-shared across
/// tunnel threads.
pub struct MitmEngine {
    ca: MitmCa,
    /// Upstream (proxy→real-server) TLS config — verifies the real cert against the real
    /// platform roots. Reused for every upstream leg (roots don't change per connection).
    client_config: Arc<rustls::ClientConfig>,
    /// The crypto provider, reused when building each per-host server config.
    provider: Arc<rustls::crypto::CryptoProvider>,
    brokers: Vec<CredentialBroker>,
    /// `proxy: "terminate"` — terminate every allowed TLS host, not only brokered ones.
    terminate_all: bool,
}

impl MitmEngine {
    pub fn new(brokers: Vec<CredentialBroker>, terminate_all: bool) -> io::Result<Arc<MitmEngine>> {
        let ca = MitmCa::generate()?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());

        let mut roots = rustls::RootCertStore::empty();
        let (added, _) = roots.add_parsable_certificates(ca.native_roots().iter().cloned());
        if added == 0 {
            return Err(io::Error::other(
                "no usable upstream root certificates for the MITM proxy",
            ));
        }
        let mut client_config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(tls_err)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        // http/1.1 only — the proxy has an HTTP/1.1 parser, not an h2 framer (SRT makes
        // the same choice). The child's leaf ALPN is pinned to http/1.1 too, so a client
        // never negotiates h2 it would then have to parse.
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(Arc::new(MitmEngine {
            ca,
            client_config: Arc::new(client_config),
            provider,
            brokers,
            terminate_all,
        }))
    }

    /// The child-scoped CA-bundle path — wired into the child's CA-env vars.
    pub fn bundle_path(&self) -> &std::path::Path {
        self.ca.bundle_path()
    }

    #[cfg(target_os = "linux")]
    pub fn bundle_file(&self) -> std::io::Result<std::fs::File> {
        self.ca.bundle_file()
    }

    /// Whether `host` should be TLS-terminated (a broker demands it, or terminate-all).
    pub(super) fn should_terminate(&self, host: &str) -> bool {
        self.terminate_all || self.broker_for(host).is_some()
    }

    /// `proxy: "terminate"` — every allowed TLS host must be terminated. Used to FAIL
    /// CLOSED on a connection that carries no host to terminate (no SNI / IP literal),
    /// which would otherwise escape termination via a blind splice.
    pub(super) fn terminates_everything(&self) -> bool {
        self.terminate_all
    }

    fn broker_for(&self, host: &str) -> Option<&[HeaderInject]> {
        self.brokers
            .iter()
            .find(|b| host_glob_matches(&b.host, host))
            .map(|b| b.injects.as_slice())
    }
}

/// Terminate a client tunnel to `host:port`, inject the broker's credential, forward to
/// the real upstream, and relay the response. `prelude` is the ClientHello bytes already
/// read during the SNI gate — replayed into the TLS state machine.
///
/// Returns `Ok(())` on a clean completion OR a clean fail-closed drop; an `Err` is an
/// unexpected IO failure the caller also treats as a dropped connection. In NO case does
/// this forward an un-injected request or expose the secret to the child.
pub(super) fn terminate(
    engine: &MitmEngine,
    client: TcpStream,
    prelude: Vec<u8>,
    host: &str,
    port: u16,
    allow_private: bool,
) -> io::Result<()> {
    // A brokered host reached over a NON-TLS or unmintable channel must fail closed —
    // never inject a credential onto an unverified wire (SRT's allowPlaintextInject
    // default-false; the whole point is the secret only ever crosses a verified channel).
    let (chain, key) = engine.ca.leaf_for(host)?;
    let server_config = rustls::ServerConfig::builder_with_provider(engine.provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(tls_err)?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(tls_err)?;
    let mut server_config = server_config;
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let mut sconn = rustls::ServerConnection::new(Arc::new(server_config)).map_err(tls_err)?;
    // No read timeout for the terminated leg: a client may pause between handshake and
    // request; the parent reaps the whole proxy when the child exits.
    client.set_read_timeout(None)?;
    let mut client_io = ReplayIo::new(prelude, client);
    let mut client_tls = rustls::Stream::new(&mut sconn, &mut client_io);

    // Read the one request in cleartext, broker it, normalize its framing.
    let mut req = http1::read_request(&mut client_tls)?;
    if let Some(injects) = engine.broker_for(host) {
        http1::apply_injects(&mut req, injects);
    }
    http1::normalize_for_forward(&mut req);

    // The upstream leg: REAL TLS to the REAL server, verified against REAL roots.
    let upstream_tcp =
        super::connect_upstream(&super::Host::Name(host.to_string()), port, allow_private)?;
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| io::Error::other("invalid upstream server name for TLS termination"))?;
    let mut uconn = rustls::ClientConnection::new(engine.client_config.clone(), server_name)
        .map_err(tls_err)?;
    let mut up_io = upstream_tcp;
    let mut upstream_tls = rustls::Stream::new(&mut uconn, &mut up_io);
    upstream_tls.write_all(&req.serialize())?;
    upstream_tls.flush()?;

    // Relay the response back to the child. We forced `Connection: close` upstream, so
    // the server closes after the response body — copy-until-EOF frames it correctly.
    io::copy(&mut upstream_tls, &mut client_tls)?;
    // Clean TLS teardown both ways so a client doesn't report truncation.
    let _ = client_tls.flush();
    Ok(())
}

fn tls_err(e: rustls::Error) -> io::Error {
    io::Error::other(format!("MITM TLS error: {e}"))
}

/// A Read+Write that first replays the buffered ClientHello prelude, then reads/writes
/// the live socket. rustls consumes the prelude as if it had just arrived on the wire.
struct ReplayIo {
    prelude: io::Cursor<Vec<u8>>,
    sock: TcpStream,
    prelude_done: bool,
}

impl ReplayIo {
    fn new(prelude: Vec<u8>, sock: TcpStream) -> Self {
        Self {
            prelude: io::Cursor::new(prelude),
            sock,
            prelude_done: false,
        }
    }
}

impl Read for ReplayIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.prelude_done {
            let n = self.prelude.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            self.prelude_done = true;
        }
        self.sock.read(buf)
    }
}

impl Write for ReplayIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sock.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

/// A hand-rolled minimal HTTP/1.1 request model — enough to broker headers and re-frame,
/// no more. Deliberately NOT a general HTTP stack: cut-1 forwards one request per
/// connection with an explicit Content-Length, which keeps framing unambiguous.
pub(super) mod http1 {
    use super::{HeaderInject, MAX_BODY, MAX_HEAD, Read};
    use std::io;

    pub(super) struct Request {
        method: String,
        target: String,
        version: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    /// Read one request: head (request-line + headers) then a Content-Length body. A
    /// chunked request body is REFUSED (fail-closed) rather than risk mis-framing.
    pub(super) fn read_request(r: &mut impl Read) -> io::Result<Request> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let head_end = loop {
            if let Some(pos) = find_crlf_crlf(&buf) {
                break pos;
            }
            if buf.len() > MAX_HEAD {
                return Err(io::Error::other("request head exceeds cap"));
            }
            let n = r.read(&mut tmp)?;
            if n == 0 {
                return Err(io::Error::other(
                    "client closed before a complete request head",
                ));
            }
            buf.extend_from_slice(&tmp[..n]);
        };
        // STRICT CRLF FRAMING (request-smuggling guard). Without this a child could embed
        // a bare `\n` inside a header value; the split-on-`\r\n` parse would fold the
        // remainder INTO that value, and on re-serialization the bare LF re-materializes
        // as a separate header upstream — smuggling a header (e.g. its own `Authorization`)
        // past strip-then-set and desyncing the request. Reject any bare CR or LF in the
        // head; every CR must be followed by LF and every LF preceded by CR.
        if has_bare_crlf(&buf[..head_end]) {
            return Err(io::Error::other(
                "request head contains a bare CR or LF (framing guard)",
            ));
        }
        let head =
            std::str::from_utf8(&buf[..head_end]).map_err(|_| io::Error::other("non-UTF8 head"))?;
        let mut lines = head.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| io::Error::other("empty request"))?;
        let mut parts = request_line.splitn(3, ' ');
        let method = parts
            .next()
            .ok_or_else(|| io::Error::other("no method"))?
            .to_string();
        let target = parts
            .next()
            .ok_or_else(|| io::Error::other("no request target"))?
            .to_string();
        let version = parts
            .next()
            .ok_or_else(|| io::Error::other("no HTTP version"))?
            .to_string();

        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| io::Error::other("malformed header line"))?;
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }

        // Body framing.
        let mut body = buf[head_end + 4..].to_vec();
        if header_get(&headers, "transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
        {
            return Err(io::Error::other(
                "chunked request bodies are not supported in the MITM cut-1 (fail-closed)",
            ));
        }
        if let Some(cl) = header_get(&headers, "content-length") {
            let len: usize = cl
                .trim()
                .parse()
                .map_err(|_| io::Error::other("invalid Content-Length"))?;
            if len > MAX_BODY {
                return Err(io::Error::other("request body exceeds cap"));
            }
            while body.len() < len {
                let n = r.read(&mut tmp)?;
                if n == 0 {
                    return Err(io::Error::other("client closed mid-body"));
                }
                body.extend_from_slice(&tmp[..n]);
            }
            body.truncate(len);
        }

        Ok(Request {
            method,
            target,
            version,
            headers,
            body,
        })
    }

    /// Strip-then-set each brokered header: remove EVERY existing copy (case-insensitive),
    /// then append the injected value. A child-supplied — possibly leaked-real — header
    /// can never survive alongside the injected one.
    pub(super) fn apply_injects(req: &mut Request, injects: &[HeaderInject]) {
        for inj in injects {
            req.headers
                .retain(|(n, _)| !n.eq_ignore_ascii_case(&inj.header));
            req.headers
                .push((inj.header.clone(), inj.value.expose().to_string()));
        }
    }

    /// Normalize framing for a single-request forward: drop hop-by-hop headers, set an
    /// accurate Content-Length, force `Connection: close`.
    pub(super) fn normalize_for_forward(req: &mut Request) {
        req.headers.retain(|(n, _)| {
            !n.eq_ignore_ascii_case("connection")
                && !n.eq_ignore_ascii_case("proxy-connection")
                && !n.eq_ignore_ascii_case("keep-alive")
                && !n.eq_ignore_ascii_case("transfer-encoding")
                && !n.eq_ignore_ascii_case("content-length")
        });
        if !req.body.is_empty() {
            req.headers
                .push(("Content-Length".to_string(), req.body.len().to_string()));
        }
        req.headers
            .push(("Connection".to_string(), "close".to_string()));
    }

    /// Serialize the request onto the wire (request-line + headers + CRLF + body).
    pub(super) fn serialize(req: &Request) -> Vec<u8> {
        let mut out = Vec::with_capacity(256 + req.body.len());
        out.extend_from_slice(
            format!("{} {} {}\r\n", req.method, req.target, req.version).as_bytes(),
        );
        for (name, value) in &req.headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&req.body);
        out
    }

    impl Request {
        pub(super) fn serialize(&self) -> Vec<u8> {
            serialize(self)
        }
        #[cfg(test)]
        pub(super) fn header(&self, name: &str) -> Option<&str> {
            header_get(&self.headers, name)
        }
        #[cfg(test)]
        pub(super) fn header_count(&self, name: &str) -> usize {
            self.headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case(name))
                .count()
        }
    }

    fn header_get<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn find_crlf_crlf(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    /// True if `head` contains a bare CR (not followed by LF) or a bare LF (not part of a
    /// preceding CRLF) — the request-smuggling framing violation. `\r\n` pairs are
    /// consumed as a unit; anything else that is a CR/LF is bare.
    fn has_bare_crlf(head: &[u8]) -> bool {
        let mut i = 0;
        while i < head.len() {
            match head[i] {
                b'\r' => {
                    if head.get(i + 1) != Some(&b'\n') {
                        return true; // bare CR
                    }
                    i += 2;
                }
                b'\n' => return true, // an LF reached outside a CRLF pair → bare LF
                _ => i += 1,
            }
        }
        false
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::policy::Secret;
        use std::io::Cursor;

        fn inject(header: &str, value: &str) -> HeaderInject {
            HeaderInject {
                header: header.to_string(),
                value: Secret::new(value.to_string()),
            }
        }

        #[test]
        fn strips_child_auth_then_injects_the_real_one() {
            let raw = "GET /repos HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: Bearer child-guess\r\n\r\n";
            let mut req = read_request(&mut Cursor::new(raw.as_bytes())).unwrap();
            apply_injects(&mut req, &[inject("Authorization", "Bearer REAL-SECRET")]);
            // The child's value is gone; exactly one Authorization remains, the real one.
            assert_eq!(req.header_count("authorization"), 1);
            assert_eq!(req.header("Authorization"), Some("Bearer REAL-SECRET"));
        }

        #[test]
        fn forwarded_request_forces_close_and_reframes_body() {
            let raw = "POST /x HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\nConnection: keep-alive\r\n\r\nhello";
            let mut req = read_request(&mut Cursor::new(raw.as_bytes())).unwrap();
            normalize_for_forward(&mut req);
            let wire = String::from_utf8(req.serialize()).unwrap();
            assert!(wire.contains("Connection: close"));
            assert!(!wire.to_ascii_lowercase().contains("keep-alive"));
            assert!(wire.contains("Content-Length: 5"));
            assert!(wire.ends_with("\r\n\r\nhello"));
        }

        #[test]
        fn wildcard_broker_terminates_and_injects_only_for_matching_hosts() {
            // A `*.example.com` broker fires `broker_for`/`should_terminate` for the apex
            // and any-depth subdomain, and NOT for a sibling — the runtime selection uses
            // the same universal host-glob matcher as net allow/deny.
            let broker = crate::policy::CredentialBroker {
                host: "*.example.com".to_string(),
                injects: vec![inject("Authorization", "Bearer REAL-SECRET")],
            };
            let engine = super::super::MitmEngine::new(vec![broker], false)
                .expect("MITM engine needs a populated native root store");
            assert!(engine.should_terminate("example.com"));
            assert!(engine.should_terminate("api.example.com"));
            assert!(engine.should_terminate("a.b.example.com"));
            assert_eq!(
                engine
                    .broker_for("api.example.com")
                    .map(|i| i[0].value.expose()),
                Some("Bearer REAL-SECRET")
            );
            assert!(!engine.should_terminate("evil.com"));
            assert!(engine.broker_for("example.com.evil.com").is_none());
        }

        #[test]
        fn chunked_request_body_is_refused() {
            let raw = "POST /x HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
            assert!(read_request(&mut Cursor::new(raw.as_bytes())).is_err());
        }

        #[test]
        fn bare_lf_header_smuggling_is_refused() {
            // A child embeds a bare LF in a header value to smuggle its own Authorization
            // past strip-then-set. The framing guard must reject the whole request.
            let raw =
                "GET / HTTP/1.1\r\nHost: h\r\nX-Foo: a\nAuthorization: child-smuggled\r\n\r\n";
            assert!(read_request(&mut Cursor::new(raw.as_bytes())).is_err());
            // A bare CR is likewise rejected.
            let raw_cr = "GET / HTTP/1.1\r\nHost: h\rX-Evil: 1\r\n\r\n";
            assert!(read_request(&mut Cursor::new(raw_cr.as_bytes())).is_err());
            // A well-formed request with only CRLF pairs is accepted.
            let ok = "GET / HTTP/1.1\r\nHost: h\r\nX-Foo: a\r\n\r\n";
            assert!(read_request(&mut Cursor::new(ok.as_bytes())).is_ok());
        }
    }
}
