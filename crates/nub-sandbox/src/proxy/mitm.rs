//! The TLS-termination tier for exact-host credential brokering.
//!
//! ENGAGED per-host, ONLY where a rule demands reading inside the stream (a
//! credential-broker rule), or globally under `proxy: "terminate"`. Everything else
//! stays a blind splice ([`super::splice`]) — the default is not "MITM off", it is "MITM
//! never instantiated": with no broker + Auto mode, [`MitmEngine`] does not exist and no
//! TLS/CA code runs.
//!
//! THE FLOW for a brokered host: mint a leaf for the exact SNI host → complete TLS
//! with the child → parse one HTTP/1.1 request → replace exact opaque markers only
//! inside header values → open a second, verified TLS connection to that same host →
//! forward the request and response. Markers in a URL, body, response, or another
//! host are never touched.
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

use super::ca::{CaScope, MitmCa};
use crate::matcher::host::strip_trailing_dot;
use crate::policy::CredentialBroker;
use base64::Engine as _;
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
    brokers: Vec<RuntimeCredentialBroker>,
    /// `proxy: "terminate"` — terminate every allowed TLS host, not only brokered ones.
    terminate_all: bool,
}

#[derive(Clone)]
pub(crate) struct CredentialReplacement {
    marker: String,
    secret: Arc<Secret>,
}

impl std::fmt::Debug for CredentialReplacement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialReplacement")
            .field("marker", &self.marker)
            .field("secret", &self.secret)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeCredentialBroker {
    host: String,
    replacements: Vec<CredentialReplacement>,
}

#[derive(Clone)]
struct Secret(String);

impl Secret {
    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(\"<redacted>\")")
    }
}

/// Per-apply broker state: fresh markers for the child plus real secrets retained
/// only in redacted proxy-owned values.
pub(crate) struct BrokerSession {
    markers: Vec<(String, String)>,
    brokers: Vec<RuntimeCredentialBroker>,
}

impl BrokerSession {
    pub(crate) fn from_policy(
        configured: &[CredentialBroker],
        mut lookup: impl FnMut(&str) -> Result<Option<String>, String>,
    ) -> io::Result<Self> {
        let mut credentials: Vec<(String, String, Arc<Secret>)> = Vec::new();
        for name in configured.iter().flat_map(|broker| &broker.env) {
            if credentials
                .iter()
                .any(|(existing, _, _)| env_key_eq(existing, name))
            {
                continue;
            }
            let value = lookup(name)
                .map_err(|error| {
                    io::Error::other(format!("reading brokered env `{name}`: {error}"))
                })?
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "brokered environment variable `{name}` is not set in the parent"
                    ))
                })?;
            if value.is_empty() {
                return Err(io::Error::other(format!(
                    "brokered environment variable `{name}` is empty"
                )));
            }
            if value
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte == 0x7f)
            {
                return Err(io::Error::other(format!(
                    "brokered environment variable `{name}` contains an HTTP-unsafe control byte"
                )));
            }
            let marker = loop {
                let candidate = fresh_marker()?;
                if !credentials
                    .iter()
                    .any(|(_, existing, _)| existing == &candidate)
                {
                    break candidate;
                }
            };
            credentials.push((name.clone(), marker, Arc::new(Secret(value))));
        }

        let brokers = configured
            .iter()
            .map(|broker| RuntimeCredentialBroker {
                host: broker.host.clone(),
                replacements: broker
                    .env
                    .iter()
                    .map(|name| {
                        let (_, marker, secret) = credentials
                            .iter()
                            .find(|(existing, _, _)| env_key_eq(existing, name))
                            .expect("credential collected for every configured broker env");
                        CredentialReplacement {
                            marker: marker.clone(),
                            secret: Arc::clone(secret),
                        }
                    })
                    .collect(),
            })
            .collect();
        let markers = credentials
            .into_iter()
            .map(|(name, marker, _)| (name, marker))
            .collect();
        Ok(Self { markers, brokers })
    }

    pub(crate) fn install_markers(&self, env: &mut std::collections::BTreeMap<String, String>) {
        for (name, marker) in &self.markers {
            env.retain(|existing, _| !env_key_eq(existing, name));
            env.insert(name.clone(), marker.clone());
        }
    }

    pub(crate) fn into_brokers(self) -> Vec<RuntimeCredentialBroker> {
        self.brokers
    }
}

fn env_key_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn fresh_marker() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| io::Error::other(format!("generating credential marker: {error}")))?;
    Ok(format!(
        "nub-credential-v1-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}

impl MitmEngine {
    pub(crate) fn new(
        brokers: Vec<RuntimeCredentialBroker>,
        terminate_all: bool,
    ) -> io::Result<Arc<MitmEngine>> {
        let ca = MitmCa::generate(ca_scope(&brokers, terminate_all))?;
        let roots = ca.native_roots().to_vec();
        Self::with_ca_and_roots(ca, brokers, terminate_all, roots)
    }

    #[cfg(test)]
    pub(super) fn new_for_test(
        brokers: Vec<RuntimeCredentialBroker>,
        upstream_root: rustls::pki_types::CertificateDer<'static>,
    ) -> io::Result<Arc<MitmEngine>> {
        let ca = MitmCa::generate(ca_scope(&brokers, false))?;
        Self::with_ca_and_roots(ca, brokers, false, vec![upstream_root])
    }

    fn with_ca_and_roots(
        ca: MitmCa,
        brokers: Vec<RuntimeCredentialBroker>,
        terminate_all: bool,
        upstream_roots: Vec<rustls::pki_types::CertificateDer<'static>>,
    ) -> io::Result<Arc<MitmEngine>> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());

        let mut roots = rustls::RootCertStore::empty();
        let (added, _) = roots.add_parsable_certificates(upstream_roots);
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

    #[cfg(test)]
    pub(super) fn child_ca_der(&self) -> rustls::pki_types::CertificateDer<'static> {
        self.ca.ca_der()
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

    fn broker_for(&self, host: &str) -> Option<&[CredentialReplacement]> {
        broker_for_host(&self.brokers, host)
    }
}

fn broker_for_host<'a>(
    brokers: &'a [RuntimeCredentialBroker],
    host: &str,
) -> Option<&'a [CredentialReplacement]> {
    let host = strip_trailing_dot(host);
    brokers
        .iter()
        .find(|broker| strip_trailing_dot(&broker.host).eq_ignore_ascii_case(host))
        .map(|broker| broker.replacements.as_slice())
}

/// Decide the ephemeral CA's [`CaScope`] from what this engine can ever mint a leaf
/// for. `should_terminate` mints for a broker match OR (when `terminate_all`) for ANY
/// allowed host — and the full allowlist behind `terminate_all` never reaches
/// `MitmEngine::new` (only the compiled `RuntimeCredentialBroker`s do), so there is no
/// finite host set to constrain against on that path without a blind guess. Constrain
/// only the one case where the mintable set is exactly known: brokers-only (today's
/// only reachable posture — `terminate_all` is derived from the dormant
/// `ProxyMode::Terminate`, see `policy.rs`).
fn ca_scope(brokers: &[RuntimeCredentialBroker], terminate_all: bool) -> CaScope {
    if terminate_all {
        return CaScope::Unconstrained;
    }
    let mut hosts: Vec<String> = brokers.iter().map(|broker| broker.host.clone()).collect();
    hosts.sort();
    hosts.dedup();
    if hosts.is_empty() {
        CaScope::Unconstrained
    } else {
        CaScope::Hosts(hosts)
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
    active: &super::ActiveConnection,
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
    // No read timeout for the terminated leg: a client may legitimately pause between
    // handshake and request, and rustls's blocking `Stream` treats a WouldBlock from the
    // socket as a hard error rather than retrying, so the SPLICE_POLL tick the blind
    // splice uses cannot simply be applied here.
    //
    // KNOWN GAP. That leaves this leg unbounded under `EgressProxy::drop`, which joins
    // every handler: on Windows a `shutdown()` does not cancel a pending `recv()` (see
    // `super::SPLICE_POLL`), so a child that terminates a brokered handshake and then
    // stalls mid-request wedges teardown exactly as the blind splice used to. Closing it
    // needs a retry-aware IO wrapper that absorbs the tick beneath rustls and surfaces
    // only teardown as an error — tracked separately, not solved by the splice fix.
    client.set_read_timeout(None)?;
    let mut client_io = ReplayIo::new(prelude, client);
    let mut client_tls = rustls::Stream::new(&mut sconn, &mut client_io);

    // Read the one request in cleartext, broker it, normalize its framing.
    let mut req = http1::read_request(&mut client_tls)?;
    http1::validate_host_boundary(&req, host, port)?;
    http1::normalize_for_forward(&mut req);
    if let Some(replacements) = engine.broker_for(host) {
        http1::apply_replacements(&mut req, replacements)?;
    }

    // The upstream leg: REAL TLS to the REAL server, verified against REAL roots.
    let upstream_tcp =
        super::connect_upstream(&super::Host::Name(host.to_string()), port, allow_private)?;
    active.track(&upstream_tcp)?;
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
    // Send an explicit close_notify so strict clients do not classify a complete HTTP
    // response as a truncated TLS session.
    client_tls.conn.send_close_notify();
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
    use super::{CredentialReplacement, MAX_BODY, MAX_HEAD, Read};
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
        let parts = request_line.split(' ').collect::<Vec<_>>();
        if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
            return Err(io::Error::other("malformed HTTP/1.1 request line"));
        }
        let method = parts[0].to_string();
        if !method.bytes().all(is_token_byte) {
            return Err(io::Error::other("invalid HTTP method token"));
        }
        let target = parts[1].to_string();
        if target != "*" && !target.starts_with('/') {
            return Err(io::Error::other("unsupported HTTP/1.1 request-target form"));
        }
        if target.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(io::Error::other("invalid HTTP request target"));
        }
        let version = parts[2].to_string();
        if version != "HTTP/1.1" {
            return Err(io::Error::other(
                "only HTTP/1.1 is supported for credential brokering",
            ));
        }

        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| io::Error::other("malformed header line"))?;
            if name.is_empty() || !name.bytes().all(is_token_byte) {
                return Err(io::Error::other("invalid HTTP header name"));
            }
            let value = value.trim();
            if value
                .bytes()
                .any(|byte| (byte.is_ascii_control() && byte != b'\t') || byte == 0x7f)
            {
                return Err(io::Error::other("invalid HTTP header value"));
            }
            headers.push((name.to_string(), value.to_string()));
        }

        // Body framing.
        let mut body = buf[head_end + 4..].to_vec();
        if headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        {
            return Err(io::Error::other(
                "Transfer-Encoding is not supported for credential brokering",
            ));
        }
        if headers.iter().any(|(name, _)| {
            name.eq_ignore_ascii_case("expect") || name.eq_ignore_ascii_case("upgrade")
        }) {
            return Err(io::Error::other(
                "HTTP expectation and protocol upgrade are not supported for credential brokering",
            ));
        }
        let lengths = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>();
        if lengths.len() > 1 {
            return Err(io::Error::other(
                "multiple Content-Length headers are not supported",
            ));
        }
        if let Some(cl) = lengths.first() {
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
            if body.len() != len {
                return Err(io::Error::other(
                    "pipelined bytes after the framed request are not supported",
                ));
            }
        } else if !body.is_empty() {
            return Err(io::Error::other(
                "unframed request body or pipelined bytes are not supported",
            ));
        }

        Ok(Request {
            method,
            target,
            version,
            headers,
            body,
        })
    }

    pub(super) fn validate_host_boundary(req: &Request, host: &str, port: u16) -> io::Result<()> {
        let hosts = req
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>();
        if hosts.len() != 1 {
            return Err(io::Error::other(
                "a brokered HTTP/1.1 request must carry exactly one Host header",
            ));
        }
        let expected = super::strip_trailing_dot(host);
        let value = hosts[0];
        let (actual, actual_port) = match value.rsplit_once(':') {
            Some((candidate, suffix))
                if !candidate.contains(':') && suffix.parse::<u16>().is_ok() =>
            {
                (
                    candidate,
                    Some(suffix.parse::<u16>().expect("checked above")),
                )
            }
            _ => (value, None),
        };
        if !super::strip_trailing_dot(actual).eq_ignore_ascii_case(expected)
            || actual_port.is_some_and(|actual| actual != port)
        {
            return Err(io::Error::other(
                "HTTP Host does not match the brokered TLS host boundary",
            ));
        }
        Ok(())
    }

    /// Replace all exact marker occurrences in every header value. Header names,
    /// field count, ordering, the request target, and the body are untouched.
    ///
    /// A literal marker substitution cannot CORRUPT a `Basic base64(user:pass)` header:
    /// either the client passes the brokered value through unencoded — copying it
    /// verbatim into a header field, as npm's legacy `_auth` config does — in which case
    /// the marker occupies the whole (or a clean, self-contained) substring and swaps in
    /// safely; or the client base64-ENCODES the marker itself (`curl -u marker:`, most
    /// Basic-auth libraries), in which case encoding repacks bytes into 6-bit groups and
    /// the plaintext marker never appears in the output at all — no match, no edit, the
    /// header passes through unchanged (wrong credentials reach the upstream host, not a
    /// malformed request). See `client_side_base64_encoded_basic_auth_is_left_untouched_not_corrupted`
    /// and `basic_auth_marker_used_as_the_whole_parameter_substitutes_cleanly` below.
    pub(super) fn apply_replacements(
        req: &mut Request,
        replacements: &[CredentialReplacement],
    ) -> io::Result<()> {
        // Preflight the entire expanded head before allocating any replacement. The
        // child controls marker repetition and a parent secret can be large, so the
        // inbound 64-KiB cap alone does not bound the outbound allocation.
        let mut projected = req
            .method
            .len()
            .checked_add(req.target.len())
            .and_then(|size| size.checked_add(req.version.len()))
            .and_then(|size| size.checked_add(4))
            .ok_or_else(|| io::Error::other("expanded request head size overflow"))?;
        for (name, value) in &req.headers {
            projected = projected
                .checked_add(name.len())
                .and_then(|size| size.checked_add(2))
                .and_then(|size| {
                    projected_replaced_len(value, replacements)
                        .and_then(|value_len| size.checked_add(value_len))
                })
                .and_then(|size| size.checked_add(2))
                .ok_or_else(|| io::Error::other("expanded request head size overflow"))?;
            if projected > MAX_HEAD {
                return Err(io::Error::other("expanded request head exceeds cap"));
            }
        }
        projected = projected
            .checked_add(2)
            .ok_or_else(|| io::Error::other("expanded request head size overflow"))?;
        if projected > MAX_HEAD {
            return Err(io::Error::other("expanded request head exceeds cap"));
        }

        for (_, value) in &mut req.headers {
            if let Some(replaced) = replace_markers_once(value, replacements) {
                *value = replaced;
            }
        }
        Ok(())
    }

    fn projected_replaced_len(
        value: &str,
        replacements: &[CredentialReplacement],
    ) -> Option<usize> {
        let mut cursor = 0;
        let mut projected = 0usize;
        loop {
            let next = replacements
                .iter()
                .filter_map(|replacement| {
                    value[cursor..]
                        .find(&replacement.marker)
                        .map(|offset| (cursor + offset, replacement))
                })
                .min_by_key(|(offset, _)| *offset);
            let Some((offset, replacement)) = next else {
                return projected.checked_add(value.len() - cursor);
            };
            projected = projected
                .checked_add(offset - cursor)?
                .checked_add(replacement.secret.expose().len())?;
            cursor = offset + replacement.marker.len();
        }
    }

    fn replace_markers_once(value: &str, replacements: &[CredentialReplacement]) -> Option<String> {
        let mut cursor = 0;
        let mut out: Option<String> = None;
        loop {
            let next = replacements
                .iter()
                .filter_map(|replacement| {
                    value[cursor..]
                        .find(&replacement.marker)
                        .map(|offset| (cursor + offset, replacement))
                })
                .min_by_key(|(offset, _)| *offset);
            let Some((offset, replacement)) = next else {
                break;
            };
            let output = out.get_or_insert_with(|| String::with_capacity(value.len()));
            output.push_str(&value[cursor..offset]);
            output.push_str(replacement.secret.expose());
            cursor = offset + replacement.marker.len();
        }
        out.map(|mut output| {
            output.push_str(&value[cursor..]);
            output
        })
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

    #[cfg(test)]
    fn header_get<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn is_token_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
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
        use std::io::Cursor;
        use std::sync::Arc;

        fn replacement(marker: &str, secret: &str) -> CredentialReplacement {
            CredentialReplacement {
                marker: marker.to_string(),
                secret: Arc::new(super::super::Secret(secret.to_string())),
            }
        }

        #[test]
        fn replaces_multiple_markers_in_multiple_header_values_without_restructuring() {
            let raw = "GET /repos HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: Bearer marker-a.marker-b.marker-a\r\nX-Credential: marker-b\r\nX-Unchanged: child-value\r\n\r\n";
            let mut req = read_request(&mut Cursor::new(raw.as_bytes())).unwrap();
            apply_replacements(
                &mut req,
                &[
                    replacement("marker-a", "SECRET-A"),
                    replacement("marker-b", "SECRET-B"),
                ],
            )
            .unwrap();
            assert_eq!(req.header_count("authorization"), 1);
            assert_eq!(
                req.header("Authorization"),
                Some("Bearer SECRET-A.SECRET-B.SECRET-A")
            );
            assert_eq!(req.header("X-Credential"), Some("SECRET-B"));
            assert_eq!(req.header("X-Unchanged"), Some("child-value"));
        }

        #[test]
        fn no_marker_means_no_replacement_and_url_body_are_never_scanned() {
            let raw = "POST /marker HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: Bearer child-value\r\nContent-Length: 6\r\n\r\nmarker";
            let mut req = read_request(&mut Cursor::new(raw.as_bytes())).unwrap();
            apply_replacements(&mut req, &[replacement("marker", "REAL-SECRET")]).unwrap();
            let wire = String::from_utf8(req.serialize()).unwrap();
            assert!(wire.starts_with("POST /marker HTTP/1.1\r\n"));
            assert!(wire.contains("Authorization: Bearer child-value\r\n"));
            assert!(wire.ends_with("\r\n\r\nmarker"));
            assert!(!wire.contains("REAL-SECRET"));
        }

        /// D2 reachability finding: a client that builds Basic auth the standard way
        /// (base64-encoding "user:password" ITSELF, e.g. `curl -u marker:`) never exposes
        /// the marker's literal ASCII text on the wire — base64 repacks bytes into 6-bit
        /// groups, so the encoded output contains no substring equal to the plaintext
        /// input. `apply_replacements` finds no match and leaves the header byte-for-byte
        /// unchanged: the request goes out carrying the WRONG (marker-derived) credential
        /// and fails auth upstream, but the header is never corrupted or malformed, and
        /// the real secret is never exposed either way. Pins the current safe-by-construction
        /// behavior rather than a hypothesized "corrupt header" outcome, which is not
        /// reachable: there is no code path where a base64 ENCODER preserves a literal
        /// substring of its input in its output.
        #[test]
        fn client_side_base64_encoded_basic_auth_is_left_untouched_not_corrupted() {
            use base64::Engine as _;
            let marker = "nub-credential-v1-marker";
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("user:{marker}"));
            let raw = format!(
                "GET / HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: Basic {encoded}\r\n\r\n"
            );
            let mut req = read_request(&mut Cursor::new(raw.as_bytes())).unwrap();
            apply_replacements(&mut req, &[replacement(marker, "REAL-SECRET")]).unwrap();
            assert_eq!(
                req.header("Authorization"),
                Some(format!("Basic {encoded}").as_str()),
                "a client-side-encoded Basic header must pass through byte-identical, not corrupted"
            );
            let wire = String::from_utf8(req.serialize()).unwrap();
            assert!(
                !wire.contains("REAL-SECRET"),
                "the real secret must never appear when the marker could not be matched"
            );
        }

        /// The one Basic-auth shape where a marker DOES land as an exact match: a client
        /// that treats the brokered env var as an ALREADY-encoded blob and copies it
        /// verbatim into the header (e.g. npm's legacy `_auth` config, which is sent as-is
        /// in `Authorization: Basic <_auth>` with no additional encoding). There the marker
        /// IS the entire parameter, so the literal substitution is a correct 1:1 swap, not
        /// a partial in-place edit of a larger base64 blob — no corruption risk.
        #[test]
        fn basic_auth_marker_used_as_the_whole_parameter_substitutes_cleanly() {
            let marker = "nub-credential-v1-marker";
            let raw = format!(
                "GET / HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: Basic {marker}\r\n\r\n"
            );
            let mut req = read_request(&mut Cursor::new(raw.as_bytes())).unwrap();
            apply_replacements(&mut req, &[replacement(marker, "dXNlcjpyZWFsLXNlY3JldA==")])
                .unwrap();
            assert_eq!(
                req.header("Authorization"),
                Some("Basic dXNlcjpyZWFsLXNlY3JldA==")
            );
        }

        #[test]
        fn replacement_does_not_rescan_inserted_secret_bytes() {
            let raw = "GET / HTTP/1.1\r\nHost: api.example.com\r\nX-Credential: marker-a\r\n\r\n";
            let mut req = read_request(&mut Cursor::new(raw.as_bytes())).unwrap();
            apply_replacements(
                &mut req,
                &[
                    replacement("marker-a", "secret-containing-marker-b"),
                    replacement("marker-b", "SECRET-B"),
                ],
            )
            .unwrap();
            assert_eq!(
                req.header("X-Credential"),
                Some("secret-containing-marker-b")
            );
        }

        #[test]
        fn replacement_rejects_expansion_beyond_the_head_cap_before_allocating() {
            let marker = "marker";
            let repeated = marker.repeat((MAX_HEAD / marker.len()) - 32);
            let raw = format!(
                "GET / HTTP/1.1\r\nHost: api.example.com\r\nX-Credential: {repeated}\r\n\r\n"
            );
            let mut req = read_request(&mut Cursor::new(raw.as_bytes())).unwrap();
            let err =
                apply_replacements(&mut req, &[replacement(marker, &"S".repeat(MAX_HEAD / 2))])
                    .unwrap_err();
            assert!(
                err.to_string()
                    .contains("expanded request head exceeds cap")
            );
            assert_eq!(
                req.header("X-Credential"),
                Some(repeated.as_str()),
                "preflight failure must not partially mutate the request"
            );
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
        fn broker_selection_and_http_host_boundary_are_exact() {
            let broker = super::super::RuntimeCredentialBroker {
                host: "api.example.com".to_string(),
                replacements: vec![replacement("marker", "REAL-SECRET")],
            };
            let brokers = [broker];
            assert!(super::super::broker_for_host(&brokers, "api.example.com").is_some());
            assert!(super::super::broker_for_host(&brokers, "API.EXAMPLE.COM.").is_some());
            assert!(super::super::broker_for_host(&brokers, "sub.api.example.com").is_none());
            assert!(super::super::broker_for_host(&brokers, "evil.com").is_none());

            let ok = "GET / HTTP/1.1\r\nHost: api.example.com:443\r\n\r\n";
            let req = read_request(&mut Cursor::new(ok.as_bytes())).unwrap();
            validate_host_boundary(&req, "api.example.com", 443).unwrap();

            let wrong = "GET / HTTP/1.1\r\nHost: evil.example.com\r\n\r\n";
            let req = read_request(&mut Cursor::new(wrong.as_bytes())).unwrap();
            assert!(validate_host_boundary(&req, "api.example.com", 443).is_err());

            let duplicate =
                "GET / HTTP/1.1\r\nHost: api.example.com\r\nHost: evil.example.com\r\n\r\n";
            let req = read_request(&mut Cursor::new(duplicate.as_bytes())).unwrap();
            assert!(validate_host_boundary(&req, "api.example.com", 443).is_err());
        }

        #[test]
        fn chunked_request_body_is_refused() {
            let raw = "POST /x HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
            assert!(read_request(&mut Cursor::new(raw.as_bytes())).is_err());
        }

        #[test]
        fn unsupported_http_shapes_fail_closed() {
            for raw in [
                "PRI * HTTP/2.0\r\nHost: api.example.com\r\n\r\n",
                "GET https://api.example.com/x HTTP/1.1\r\nHost: api.example.com\r\n\r\n",
                "GET / HTTP/1.0\r\nHost: api.example.com\r\n\r\n",
                "GET / HTTP/1.1\r\nHost: api.example.com\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
                "GET / HTTP/1.1\r\nHost: api.example.com\r\nExpect: 100-continue\r\n\r\n",
                "GET / HTTP/1.1\r\nHost: api.example.com\r\n\r\nGET /second HTTP/1.1\r\n\r\n",
                "POST / HTTP/1.1\r\nHost: api.example.com\r\nContent-Length: 1\r\n\r\nxy",
            ] {
                assert!(
                    read_request(&mut Cursor::new(raw.as_bytes())).is_err(),
                    "unsupported request unexpectedly parsed: {raw:?}"
                );
            }
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

#[cfg(test)]
mod session_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn broker(names: &[&str]) -> CredentialBroker {
        CredentialBroker {
            host: "api.example.com".to_string(),
            env: names.iter().map(|name| (*name).to_string()).collect(),
        }
    }

    #[test]
    fn each_session_installs_fresh_markers_and_redacts_real_secrets() {
        let configured = [broker(&["API_TOKEN", "SECOND_TOKEN"])];
        let make = || {
            BrokerSession::from_policy(&configured, |name| {
                Ok(Some(format!("real-secret-for-{name}")))
            })
            .unwrap()
        };
        let first = make();
        let second = make();
        let mut first_env = BTreeMap::from([
            ("KEEP".to_string(), "yes".to_string()),
            ("API_TOKEN".to_string(), "must-be-overwritten".to_string()),
        ]);
        let mut second_env = BTreeMap::new();
        first.install_markers(&mut first_env);
        second.install_markers(&mut second_env);

        let first_marker = first_env.get("API_TOKEN").unwrap();
        let second_marker = second_env.get("API_TOKEN").unwrap();
        assert!(first_marker.starts_with("nub-credential-v1-"));
        assert_ne!(
            first_marker, second_marker,
            "each apply gets a fresh marker"
        );
        assert!(
            !first_env
                .values()
                .any(|value| value.contains("real-secret"))
        );
        assert_eq!(first_env.get("KEEP").map(String::as_str), Some("yes"));

        let debug = format!("{:?}", first.brokers);
        assert!(!debug.contains("real-secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn missing_empty_and_multiline_parent_values_fail_closed() {
        let configured = [broker(&["API_TOKEN"])];
        assert!(BrokerSession::from_policy(&configured, |_| Ok(None)).is_err());
        assert!(BrokerSession::from_policy(&configured, |_| Ok(Some(String::new()))).is_err());
        assert!(
            BrokerSession::from_policy(&configured, |_| Ok(Some("bad\r\nvalue".to_string())))
                .is_err()
        );
    }
}
