//! Client-facing proxy handshakes: HTTP `CONNECT` and SOCKS5. Each parses the
//! requested target (host + port) WITHOUT yet committing to it — the caller runs the
//! target through the grant decider, then calls [`reply_success`]/[`reply_failure`].
//!
//! Why the two-step (parse, then caller replies): the client waits for the tunnel
//! ACK (`200` / SOCKS reply) before it sends its TLS ClientHello, so the proxy MUST
//! ACK before it can read the SNI. The target-host check happens BEFORE the ACK
//! (a denied host is refused outright); the SNI check happens AFTER (read the
//! ClientHello, then connect-or-drop). Both gates must pass.
//!
//! PER-SESSION TOKEN (defense-in-depth). The listener binds loopback with no OS-level
//! caller authentication, so ANY co-resident same-user process could otherwise borrow
//! the sandboxed child's egress hole. Every handshake must therefore present the
//! per-run bearer token BEFORE any host decision: HTTP via `Proxy-Authorization: Basic`
//! (the token as the userinfo of the `HTTP_PROXY` URL), SOCKS5 via RFC 1929 user/pass.
//! A missing/mismatched token FAILS CLOSED (407 / SOCKS auth-failure, then the
//! connection is dropped) — the token is checked before the target host is even read,
//! so an unauthenticated caller cannot probe policy. The token is 256-bit CSPRNG, so
//! the compare is high-entropy; it is still done in constant time as hygiene.

use super::Host;
use base64::Engine;
use std::io::{self, Read, Write};
use std::net::IpAddr;

/// Which client protocol a connection spoke — determines the reply framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Http,
    Socks5,
}

/// A parsed tunnel request: the protocol (for reply framing) and the requested target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub proto: Proto,
    pub host: Host,
    pub port: u16,
}

/// Max bytes we read while looking for the end of an HTTP CONNECT header block. A
/// real CONNECT request is short; past this it is junk → error (fail closed).
const MAX_HTTP_HEADER: usize = 8 * 1024;

/// Read + parse the client handshake, dispatching on the first byte (`0x05` = SOCKS5,
/// otherwise HTTP). Performs the SOCKS5 greeting round-trip (method selection) inline;
/// does NOT send the final tunnel reply (the caller does, post-decision).
///
/// `token` is the per-session bearer every client must present (HTTP `Proxy-Authorization`
/// / SOCKS5 RFC-1929 user-pass). A missing/wrong token yields `Err` AFTER the appropriate
/// auth-failure reply is written — the caller drops the connection (fail-closed), and the
/// target host is never consulted.
pub fn read_request(stream: &mut (impl Read + Write), token: &str) -> io::Result<Request> {
    // Invariant: a live proxy always mints a non-empty token. An empty token would make
    // `Basic base64(":")` (empty user+pass) authenticate — so assert it never happens.
    debug_assert!(!token.is_empty(), "egress-proxy token must be non-empty");
    let mut first = [0u8; 1];
    stream.read_exact(&mut first)?;
    if first[0] == 0x05 {
        read_socks5(stream, token)
    } else {
        read_http_connect(stream, first[0], token)
    }
}

/// Send the tunnel-established ACK so the client begins its TLS handshake.
pub fn reply_success(stream: &mut impl Write, proto: Proto) -> io::Result<()> {
    match proto {
        Proto::Http => stream.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n"),
        // VER=5 REP=0(success) RSV=0 ATYP=1(IPv4) BND.ADDR=0.0.0.0 BND.PORT=0
        Proto::Socks5 => stream.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]),
    }
}

/// Refuse the tunnel (denied target or SNI). The client sees a closed/refused tunnel.
pub fn reply_failure(stream: &mut impl Write, proto: Proto) -> io::Result<()> {
    match proto {
        Proto::Http => stream.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n"),
        // REP=2 = connection not allowed by ruleset.
        Proto::Socks5 => stream.write_all(&[0x05, 0x02, 0x00, 0x01, 0, 0, 0, 0, 0, 0]),
    }
}

// ── SOCKS5 (RFC 1928) ────────────────────────────────────────────────────────────

/// The `0x05` version byte was already consumed by [`read_request`].
fn read_socks5(stream: &mut (impl Read + Write), token: &str) -> io::Result<Request> {
    // Greeting: NMETHODS(1) + METHODS(n). We REQUIRE username/password auth (0x02, RFC
    // 1929) so the per-session token gates every SOCKS tunnel — no-auth (0x00) is refused.
    let mut nmethods = [0u8; 1];
    stream.read_exact(&mut nmethods)?;
    let mut methods = vec![0u8; nmethods[0] as usize];
    stream.read_exact(&mut methods)?;
    if !methods.contains(&0x02) {
        // Client did not offer user/pass → no acceptable method; fail closed.
        stream.write_all(&[0x05, 0xFF])?;
        return Err(bad("socks5: username/password auth required"));
    }
    stream.write_all(&[0x05, 0x02])?; // METHOD = username/password
    socks5_authenticate(stream, token)?;

    // Request: VER(1) CMD(1) RSV(1) ATYP(1) …
    let mut head = [0u8; 4];
    stream.read_exact(&mut head)?;
    if head[0] != 0x05 {
        return Err(bad("socks5: bad request version"));
    }
    if head[1] != 0x01 {
        // Only CONNECT (0x01) is supported — no BIND/UDP-ASSOCIATE in a jail.
        return Err(bad("socks5: unsupported command"));
    }
    let host = match head[3] {
        0x01 => {
            let mut a = [0u8; 4];
            stream.read_exact(&mut a)?;
            Host::Ip(IpAddr::from(a))
        }
        0x04 => {
            let mut a = [0u8; 16];
            stream.read_exact(&mut a)?;
            Host::Ip(IpAddr::from(a))
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len)?;
            let mut name = vec![0u8; len[0] as usize];
            stream.read_exact(&mut name)?;
            let name = String::from_utf8(name).map_err(|_| bad("socks5: non-utf8 host"))?;
            host_from_str(&name)?
        }
        _ => return Err(bad("socks5: unknown address type")),
    };
    let mut port = [0u8; 2];
    stream.read_exact(&mut port)?;
    Ok(Request {
        proto: Proto::Socks5,
        host,
        port: u16::from_be_bytes(port),
    })
}

/// RFC 1929 username/password sub-negotiation. VER(1)=0x01 ULEN(1) UNAME PLEN(1) PASSWD.
/// The token is accepted in EITHER field (the `HTTP_PROXY` userinfo places it as the
/// username with an empty password; a SOCKS client may split it either way). A mismatch
/// writes the `0x01 0x01` failure and fails closed.
fn socks5_authenticate(stream: &mut (impl Read + Write), token: &str) -> io::Result<()> {
    let mut ver = [0u8; 1];
    stream.read_exact(&mut ver)?;
    if ver[0] != 0x01 {
        return Err(bad("socks5: bad auth version"));
    }
    let mut ulen = [0u8; 1];
    stream.read_exact(&mut ulen)?;
    let mut uname = vec![0u8; ulen[0] as usize];
    stream.read_exact(&mut uname)?;
    let mut plen = [0u8; 1];
    stream.read_exact(&mut plen)?;
    let mut passwd = vec![0u8; plen[0] as usize];
    stream.read_exact(&mut passwd)?;
    if ct_eq(&uname, token.as_bytes()) || ct_eq(&passwd, token.as_bytes()) {
        stream.write_all(&[0x01, 0x00])?; // auth success
        Ok(())
    } else {
        stream.write_all(&[0x01, 0x01])?; // auth failure
        Err(bad("socks5: invalid proxy credentials"))
    }
}

// ── HTTP CONNECT (RFC 7231 §4.3.6) ────────────────────────────────────────────────

/// `first` is the already-consumed first byte of the request line.
fn read_http_connect(
    stream: &mut (impl Read + Write),
    first: u8,
    token: &str,
) -> io::Result<Request> {
    let mut buf = vec![first];
    // Read until the header terminator `\r\n\r\n`, bounded.
    let mut one = [0u8; 1];
    loop {
        if buf.len() >= MAX_HTTP_HEADER {
            return Err(bad("http connect: header too large"));
        }
        let n = stream.read(&mut one)?;
        if n == 0 {
            return Err(bad("http connect: eof before header end"));
        }
        buf.push(one[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    // Token gate BEFORE any host decision: an unauthenticated caller gets 407 and is
    // dropped, so it cannot even probe the host policy. `Proxy-Authenticate: Basic`
    // lets a reactive client re-offer creds on a fresh connection.
    if !http_proxy_auth_ok(&buf, token) {
        let _ = stream.write_all(
            b"HTTP/1.1 407 Proxy Authentication Required\r\n\
              Proxy-Authenticate: Basic realm=\"nub\"\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return Err(bad("http connect: proxy authentication required"));
    }
    // First line: `CONNECT host:port HTTP/1.1`.
    let line_end = buf
        .windows(2)
        .position(|w| w == b"\r\n")
        .ok_or_else(|| bad("http connect: no request line"))?;
    let line = std::str::from_utf8(&buf[..line_end]).map_err(|_| bad("http connect: non-utf8"))?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    if !method.eq_ignore_ascii_case("CONNECT") {
        return Err(bad("http connect: not a CONNECT request"));
    }
    let authority = parts
        .next()
        .ok_or_else(|| bad("http connect: missing authority"))?;
    let (host, port) = split_authority(authority)?;
    Ok(Request {
        proto: Proto::Http,
        host,
        port,
    })
}

/// Split a `host:port` authority (with `[v6]:port` support) into host + port.
fn split_authority(authority: &str) -> io::Result<(Host, u16)> {
    let (host_str, port_str) = if let Some(rest) = authority.strip_prefix('[') {
        // [IPv6]:port
        let close = rest
            .find(']')
            .ok_or_else(|| bad("http connect: unterminated IPv6 authority"))?;
        let host = &rest[..close];
        let after = &rest[close + 1..];
        let port = after
            .strip_prefix(':')
            .ok_or_else(|| bad("http connect: missing port"))?;
        (host, port)
    } else {
        let (h, p) = authority
            .rsplit_once(':')
            .ok_or_else(|| bad("http connect: missing port"))?;
        (h, p)
    };
    let port: u16 = port_str
        .parse()
        .map_err(|_| bad("http connect: bad port"))?;
    Ok((host_from_str(host_str)?, port))
}

/// Classify a host token as an IP literal or a name, rejecting control characters.
///
/// A CR/LF/NUL (or any other control char) in a host token is never legitimate; allowing
/// it invites request-smuggling / parse-confusion against the upstream resolver and the
/// SOCKS/CONNECT framing, so it fails closed at the parse boundary.
fn host_from_str(s: &str) -> io::Result<Host> {
    if s.chars().any(|c| c.is_control()) {
        return Err(bad("host contains control characters"));
    }
    match s.parse::<IpAddr>() {
        Ok(ip) => Ok(Host::Ip(ip)),
        Err(_) => Ok(Host::Name(s.to_string())),
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

// ── per-session token auth ─────────────────────────────────────────────────────────

/// Validate the HTTP `Proxy-Authorization: Basic <base64>` credential against the
/// per-session token. The token is accepted in EITHER the username or the password
/// field of the decoded `user:pass` — the `HTTP_PROXY` userinfo places it as the
/// username with an empty password, but a client that splits it otherwise still passes.
/// Any missing/malformed piece is a fail-closed `false`.
fn http_proxy_auth_ok(header_block: &[u8], token: &str) -> bool {
    let Some(value) = header_value(header_block, b"proxy-authorization") else {
        return false;
    };
    // `Basic <b64>` — scheme is case-insensitive, one SP separator.
    let Some(sp) = value.iter().position(|&b| b == b' ') else {
        return false;
    };
    let (scheme, creds) = value.split_at(sp);
    if !scheme.eq_ignore_ascii_case(b"basic") {
        return false;
    }
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(creds.trim_ascii()) else {
        return false;
    };
    let colon = decoded
        .iter()
        .position(|&b| b == b':')
        .unwrap_or(decoded.len());
    let user = &decoded[..colon];
    let pass = decoded.get(colon + 1..).unwrap_or(&[]);
    ct_eq(user, token.as_bytes()) || ct_eq(pass, token.as_bytes())
}

/// Find a header's value in a raw `\r\n`-delimited header block, case-insensitive on the
/// name, leading whitespace trimmed. `name` is the lowercase header name.
fn header_value<'a>(block: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut start = 0;
    while start < block.len() {
        let end = block[start..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .map(|p| start + p)
            .unwrap_or(block.len());
        let line = &block[start..end];
        if let Some(colon) = line.iter().position(|&b| b == b':')
            && line[..colon].eq_ignore_ascii_case(name)
        {
            return Some(line[colon + 1..].trim_ascii());
        }
        start = end + 2;
    }
    None
}

/// Constant-time byte equality. The token is 256-bit CSPRNG so timing is moot, but a
/// data-independent compare is the correct default for a credential check. Length is
/// public (fixed-width hex token), so an early length reject is fine.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A duplex fake: `read` drains `input`, `write` appends to `output`.
    struct Duplex {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Duplex {
        fn new(input: Vec<u8>) -> Self {
            Self {
                input: Cursor::new(input),
                output: Vec::new(),
            }
        }
    }
    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }
    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A fixed high-entropy-shaped session token for the fixtures.
    const TOK: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// The `Proxy-Authorization: Basic <b64(token:)>` header line (token as username,
    /// empty password — the `HTTP_PROXY` userinfo shape).
    fn basic_auth_line(token: &str) -> Vec<u8> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
        format!("Proxy-Authorization: Basic {b64}\r\n").into_bytes()
    }

    /// A full HTTP CONNECT header block carrying a valid Basic proxy-auth line.
    fn http_connect_req(authority: &str, token: &str) -> Vec<u8> {
        let mut v = format!("CONNECT {authority} HTTP/1.1\r\nHost: x\r\n").into_bytes();
        v.extend_from_slice(&basic_auth_line(token));
        v.extend_from_slice(b"\r\n");
        v
    }

    /// SOCKS5 greeting (offering user/pass) + the RFC 1929 sub-negotiation carrying
    /// `token` as the username with an empty password.
    fn socks5_greeting_auth(token: &str) -> Vec<u8> {
        let mut v = vec![0x05, 0x01, 0x02]; // ver, 1 method, username/password
        v.push(0x01); // auth version
        v.push(token.len() as u8);
        v.extend_from_slice(token.as_bytes());
        v.push(0x00); // empty password
        v
    }

    #[test]
    fn parses_http_connect_hostname() {
        let mut d = Duplex::new(http_connect_req("example.com:443", TOK));
        let r = read_request(&mut d, TOK).unwrap();
        assert_eq!(r.proto, Proto::Http);
        assert_eq!(r.host, Host::Name("example.com".into()));
        assert_eq!(r.port, 443);
    }

    #[test]
    fn parses_http_connect_ipv6() {
        let mut d = Duplex::new(http_connect_req("[::1]:8443", TOK));
        let r = read_request(&mut d, TOK).unwrap();
        assert_eq!(r.host, Host::Ip("::1".parse().unwrap()));
        assert_eq!(r.port, 8443);
    }

    #[test]
    fn parses_socks5_domain_request_and_authenticates() {
        // greeting offers user/pass; request: connect, domain "a.example", :443
        let mut bytes = socks5_greeting_auth(TOK);
        bytes.extend_from_slice(&[0x05, 0x01, 0x00, 0x03]);
        let host = b"a.example";
        bytes.push(host.len() as u8);
        bytes.extend_from_slice(host);
        bytes.extend_from_slice(&443u16.to_be_bytes());
        let mut d = Duplex::new(bytes);
        let r = read_request(&mut d, TOK).unwrap();
        assert_eq!(r.proto, Proto::Socks5);
        assert_eq!(r.host, Host::Name("a.example".into()));
        assert_eq!(r.port, 443);
        // method-selection (user/pass = 0x02) then auth-success (0x01 0x00).
        assert_eq!(&d.output, &[0x05, 0x02, 0x01, 0x00]);
    }

    #[test]
    fn parses_socks5_ipv4_request() {
        let mut bytes = socks5_greeting_auth(TOK);
        bytes.extend_from_slice(&[0x05, 0x01, 0x00, 0x01, 93, 184, 216, 34]);
        bytes.extend_from_slice(&443u16.to_be_bytes());
        let mut d = Duplex::new(bytes);
        let r = read_request(&mut d, TOK).unwrap();
        assert_eq!(r.host, Host::Ip("93.184.216.34".parse().unwrap()));
    }

    #[test]
    fn socks5_rejects_non_connect_command() {
        // cmd 0x02 = BIND — unsupported in a jail (checked after auth passes).
        let mut bytes = socks5_greeting_auth(TOK);
        bytes.extend_from_slice(&[0x05, 0x02, 0x00, 0x01, 1, 2, 3, 4, 0, 80]);
        let mut d = Duplex::new(bytes);
        assert!(read_request(&mut d, TOK).is_err());
    }

    #[test]
    fn http_non_connect_is_rejected() {
        // Valid auth so the request reaches (and is rejected at) the method check.
        let mut req = b"GET http://x/ HTTP/1.1\r\nHost: x\r\n".to_vec();
        req.extend_from_slice(&basic_auth_line(TOK));
        req.extend_from_slice(b"\r\n");
        let mut d = Duplex::new(req);
        assert!(read_request(&mut d, TOK).is_err());
    }

    #[test]
    fn socks5_rejects_control_char_in_hostname() {
        // A NUL embedded in the SOCKS5 domain (request-smuggling / parse-confusion) is
        // rejected — a legit host token never carries a control char.
        let mut bytes = socks5_greeting_auth(TOK);
        bytes.extend_from_slice(&[0x05, 0x01, 0x00, 0x03]);
        let host = b"evil\0.example";
        bytes.push(host.len() as u8);
        bytes.extend_from_slice(host);
        bytes.extend_from_slice(&443u16.to_be_bytes());
        let mut d = Duplex::new(bytes);
        assert!(read_request(&mut d, TOK).is_err());
    }

    #[test]
    fn http_connect_rejects_control_char_in_authority() {
        // A NUL inside the CONNECT authority survives `split_whitespace` (it is not
        // whitespace) and reaches the host token, where the control-char check rejects
        // it — the parse-confusion hardening. (A whitespace-class control like \x0b would
        // instead be swallowed by the tokenizer, so NUL is the faithful exercise.)
        let mut req = b"CONNECT ex\x00ample.com:443 HTTP/1.1\r\nHost: x\r\n".to_vec();
        req.extend_from_slice(&basic_auth_line(TOK));
        req.extend_from_slice(b"\r\n");
        let mut d = Duplex::new(req);
        assert!(read_request(&mut d, TOK).is_err());
    }

    #[test]
    fn http_connect_missing_token_gets_407_and_fails() {
        // No Proxy-Authorization → 407 challenge + fail-closed, BEFORE the host is read.
        let mut d = Duplex::new(b"CONNECT example.com:443 HTTP/1.1\r\nHost: x\r\n\r\n".to_vec());
        assert!(read_request(&mut d, TOK).is_err());
        assert!(
            d.output.starts_with(b"HTTP/1.1 407"),
            "a tokenless CONNECT must be answered 407, got {:?}",
            String::from_utf8_lossy(&d.output)
        );
    }

    #[test]
    fn http_connect_wrong_token_rejected() {
        let wrong = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let mut d = Duplex::new(http_connect_req("example.com:443", wrong));
        assert!(read_request(&mut d, TOK).is_err());
        assert!(d.output.starts_with(b"HTTP/1.1 407"));
    }

    #[test]
    fn socks5_without_userpass_method_is_refused() {
        // Client offers only no-auth (0x00) → no acceptable method (0x05 0xFF), fail closed.
        let mut d = Duplex::new(vec![0x05, 0x01, 0x00]);
        assert!(read_request(&mut d, TOK).is_err());
        assert_eq!(&d.output, &[0x05, 0xFF]);
    }

    #[test]
    fn socks5_wrong_token_rejected() {
        let wrong = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let mut bytes = socks5_greeting_auth(wrong);
        bytes.extend_from_slice(&[0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0, 80]);
        let mut d = Duplex::new(bytes);
        assert!(read_request(&mut d, TOK).is_err());
        // user/pass selected (0x05 0x02) then auth failure (0x01 0x01).
        assert_eq!(&d.output, &[0x05, 0x02, 0x01, 0x01]);
    }

    #[test]
    fn success_and_failure_replies_are_protocol_shaped() {
        let mut d = Duplex::new(vec![]);
        reply_success(&mut d, Proto::Http).unwrap();
        assert!(d.output.starts_with(b"HTTP/1.1 200"));
        let mut d = Duplex::new(vec![]);
        reply_failure(&mut d, Proto::Socks5).unwrap();
        assert_eq!(d.output[0..2], [0x05, 0x02]);
    }
}
