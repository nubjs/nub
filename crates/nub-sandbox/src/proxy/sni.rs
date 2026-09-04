//! TLS ClientHello SNI extraction — pure byte parsing, NO MITM.
//!
//! The egress proxy reads the cleartext `server_name` (SNI) from the client's
//! first TLS record(s) WITHOUT terminating TLS: it never holds a private key, never
//! injects a CA, never sees plaintext beyond the ClientHello the client itself sends
//! in the clear. The extracted host gates the still-encrypted tunnel (forward or
//! drop); the bytes are then blind-piped untouched.
//!
//! FAIL-CLOSED is the governing contract. A record stream that LOOKS like TLS
//! (first byte `0x16`, handshake) but does not yield a parseable ClientHello is
//! [`SniScan::Malformed`] — the caller DENIES it, never guesses. Only a stream whose
//! first byte is not a TLS handshake is [`SniScan::NotTls`] (a plain/raw tunnel the
//! caller decides by the already-admitted CONNECT/SOCKS target). Every bounds check
//! that can't be satisfied yet is [`SniScan::Incomplete`] (read more, bounded by the
//! caller's size cap + timeout); an oversize/garbage stream trips the internal cap
//! and becomes `Malformed`.
//!
//! A ClientHello may be fragmented — split across TCP reads AND across multiple TLS
//! records (an adversary can fragment deliberately to hide the SNI). We REASSEMBLE
//! handshake payload across records before parsing, so a fragmented ClientHello is
//! resolved, never silently admitted half-read.
//!
//! ECH note: with Encrypted ClientHello the real SNI is inside an encrypted
//! extension; only the OUTER `server_name` (the public name) is visible here, and it
//! is what we match. Reaching a denied inner host requires the *allowed* outer host
//! to front it (shared ECH provider) — a bounded residual for the srt/Codex-tier
//! threat model, documented, not claimed closed (no-MITM cannot see the inner name).

/// The outcome of scanning a client byte prefix for a ClientHello SNI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SniScan {
    /// A complete ClientHello carrying this `server_name`.
    Sni(String),
    /// A complete ClientHello with no `server_name` extension. No cross-routing is
    /// possible without an SNI, so the caller admits by the CONNECT/SOCKS target.
    NoSni,
    /// Not enough bytes yet to decide — read more (caller bounds this by cap + time).
    Incomplete,
    /// The first byte is not a TLS handshake record: a plain/raw tunnel, decided by
    /// the already-admitted target.
    NotTls,
    /// TLS-shaped but unparseable — the caller FAILS CLOSED (deny).
    Malformed,
}

/// TLS record content type for the handshake protocol.
const CONTENT_HANDSHAKE: u8 = 0x16;
/// Handshake message type for ClientHello.
const HS_CLIENT_HELLO: u8 = 0x01;
/// Extension type for `server_name` (SNI).
const EXT_SERVER_NAME: u16 = 0x0000;
/// `NameType` `host_name` inside the SNI list.
const SNI_NAME_TYPE_HOST: u8 = 0x00;

/// Upper bound on reassembled handshake bytes. A real ClientHello is a few KB; past
/// this a fragmenting adversary is stalling, so we stop reassembling and fail closed
/// rather than buffer without limit.
const MAX_HANDSHAKE_LEN: usize = 16 * 1024;

/// Scan a prefix of the client→server byte stream (raw TLS record layer) for the
/// ClientHello SNI. Pure over `buf`; the caller re-invokes with more bytes on
/// [`SniScan::Incomplete`].
pub fn scan_client_hello(buf: &[u8]) -> SniScan {
    let Some(&first) = buf.first() else {
        return SniScan::Incomplete;
    };
    // A ClientHello always rides a handshake record. Anything else is a non-TLS
    // tunnel — not our to inspect (the caller already admitted the target host).
    if first != CONTENT_HANDSHAKE {
        return SniScan::NotTls;
    }

    // Reassemble handshake payload across (possibly fragmented) records.
    let hs = match reassemble_handshake(buf) {
        Ok(Some(hs)) => hs,
        Ok(None) => return SniScan::Incomplete,
        Err(()) => return SniScan::Malformed,
    };
    parse_client_hello(&hs)
}

/// Concatenate the handshake-record payloads in `buf` until the first complete
/// handshake message is buffered. `Ok(Some(bytes))` = the full handshake message;
/// `Ok(None)` = need more record bytes; `Err(())` = a non-handshake record appeared
/// mid-stream or the cap was exceeded (fail closed).
fn reassemble_handshake(buf: &[u8]) -> Result<Option<Vec<u8>>, ()> {
    let mut hs: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    loop {
        // Do we already have a complete handshake message? Its 4-byte header gives
        // the body length; stop as soon as header+body is buffered.
        if hs.len() >= 4 {
            let body_len = u24(&hs[1..4]);
            let need = 4 + body_len;
            if need > MAX_HANDSHAKE_LEN {
                return Err(());
            }
            if hs.len() >= need {
                hs.truncate(need);
                return Ok(Some(hs));
            }
        }
        if hs.len() > MAX_HANDSHAKE_LEN {
            return Err(());
        }
        // Otherwise pull the next record's payload.
        if pos + 5 > buf.len() {
            return Ok(None); // record header not fully arrived
        }
        let content = buf[pos];
        if content != CONTENT_HANDSHAKE {
            // A change-cipher-spec/alert/app-data record before the ClientHello
            // completes is not something a benign TLS client does — fail closed.
            return Err(());
        }
        let rec_len = u16be(&buf[pos + 3..pos + 5]) as usize;
        if rec_len == 0 {
            return Err(());
        }
        let start = pos + 5;
        let end = start + rec_len;
        if end > buf.len() {
            return Ok(None); // record payload not fully arrived
        }
        hs.extend_from_slice(&buf[start..end]);
        pos = end;
    }
}

/// Parse a complete ClientHello handshake message for its SNI.
fn parse_client_hello(hs: &[u8]) -> SniScan {
    let mut c = Cursor::new(hs);
    // handshake header: type(1) + length(3)
    let Some(msg_type) = c.u8() else {
        return SniScan::Incomplete;
    };
    if msg_type != HS_CLIENT_HELLO {
        return SniScan::Malformed;
    }
    if c.skip(3).is_none() {
        return SniScan::Incomplete;
    }
    // body: legacy_version(2) + random(32)
    if c.skip(2 + 32).is_none() {
        return SniScan::Malformed;
    }
    // session_id: u8 length + bytes
    if c.skip_vec8().is_none() {
        return SniScan::Malformed;
    }
    // cipher_suites: u16 length + bytes
    if c.skip_vec16().is_none() {
        return SniScan::Malformed;
    }
    // compression_methods: u8 length + bytes
    if c.skip_vec8().is_none() {
        return SniScan::Malformed;
    }
    // extensions: u16 total length + the extension list. A ClientHello with no
    // extensions block (legacy) has no SNI.
    let Some(ext_total) = c.u16() else {
        return SniScan::NoSni;
    };
    let Some(exts) = c.take(ext_total as usize) else {
        return SniScan::Malformed;
    };
    scan_extensions(exts)
}

/// Walk the extension list for `server_name`; return the host_name if present.
fn scan_extensions(exts: &[u8]) -> SniScan {
    let mut c = Cursor::new(exts);
    while !c.at_end() {
        let (Some(ext_type), Some(ext_len)) = (c.u16(), c.u16()) else {
            return SniScan::Malformed;
        };
        let Some(ext_data) = c.take(ext_len as usize) else {
            return SniScan::Malformed;
        };
        if ext_type == EXT_SERVER_NAME {
            return parse_server_name(ext_data);
        }
    }
    SniScan::NoSni
}

/// Parse a `server_name` extension body for the first `host_name`.
fn parse_server_name(data: &[u8]) -> SniScan {
    let mut c = Cursor::new(data);
    // server_name_list: u16 length prefix.
    let Some(list_len) = c.u16() else {
        return SniScan::Malformed;
    };
    let Some(list) = c.take(list_len as usize) else {
        return SniScan::Malformed;
    };
    let mut lc = Cursor::new(list);
    while !lc.at_end() {
        let Some(name_type) = lc.u8() else {
            return SniScan::Malformed;
        };
        let Some(name_len) = lc.u16() else {
            return SniScan::Malformed;
        };
        let Some(name) = lc.take(name_len as usize) else {
            return SniScan::Malformed;
        };
        if name_type == SNI_NAME_TYPE_HOST {
            // SNI is ASCII (A-label) per RFC 6066; reject non-UTF-8 rather than guess.
            return match std::str::from_utf8(name) {
                Ok(s) if !s.is_empty() => SniScan::Sni(s.to_string()),
                _ => SniScan::Malformed,
            };
        }
    }
    SniScan::NoSni
}

fn u16be(b: &[u8]) -> u16 {
    ((b[0] as u16) << 8) | (b[1] as u16)
}

fn u24(b: &[u8]) -> usize {
    ((b[0] as usize) << 16) | ((b[1] as usize) << 8) | (b[2] as usize)
}

/// A bounds-checked forward cursor over a byte slice. Every accessor returns `None`
/// on underflow so a truncated/hostile buffer can never index out of range.
struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }
    fn at_end(&self) -> bool {
        self.pos >= self.b.len()
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let hi = self.u8()? as u16;
        let lo = self.u8()? as u16;
        Some((hi << 8) | lo)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        let end = self.pos.checked_add(n)?;
        if end > self.b.len() {
            return None;
        }
        self.pos = end;
        Some(())
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.b.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    /// Skip a u8-length-prefixed vector.
    fn skip_vec8(&mut self) -> Option<()> {
        let n = self.u8()? as usize;
        self.skip(n)
    }
    /// Skip a u16-length-prefixed vector.
    fn skip_vec16(&mut self) -> Option<()> {
        let n = self.u16()? as usize;
        self.skip(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but well-formed TLS ClientHello record carrying `sni` (or none
    /// when empty). Single record, real field framing — the parser must accept it.
    fn client_hello(sni: Option<&str>) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]); // legacy_version TLS1.2
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(0); // session_id len 0
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher_suites: len2 + one suite
        body.extend_from_slice(&[0x01, 0x00]); // compression_methods: len1 + null

        let mut exts = Vec::new();
        if let Some(host) = sni {
            let host = host.as_bytes();
            let mut sn = Vec::new();
            sn.push(SNI_NAME_TYPE_HOST);
            sn.extend_from_slice(&(host.len() as u16).to_be_bytes());
            sn.extend_from_slice(host);
            let mut list = Vec::new();
            list.extend_from_slice(&(sn.len() as u16).to_be_bytes());
            list.extend_from_slice(&sn);
            exts.extend_from_slice(&EXT_SERVER_NAME.to_be_bytes());
            exts.extend_from_slice(&(list.len() as u16).to_be_bytes());
            exts.extend_from_slice(&list);
        }
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);

        let mut hs = Vec::new();
        hs.push(HS_CLIENT_HELLO);
        let l = body.len();
        hs.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8]);
        hs.extend_from_slice(&body);

        framed_records(&hs, hs.len())
    }

    /// Wrap handshake bytes into TLS records of at most `chunk` payload bytes each
    /// (chunk < hs.len() exercises multi-record fragmentation).
    fn framed_records(hs: &[u8], chunk: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for part in hs.chunks(chunk.max(1)) {
            out.push(CONTENT_HANDSHAKE);
            out.extend_from_slice(&[0x03, 0x01]); // record version
            out.extend_from_slice(&(part.len() as u16).to_be_bytes());
            out.extend_from_slice(part);
        }
        out
    }

    #[test]
    fn extracts_sni_from_single_record() {
        let rec = client_hello(Some("example.com"));
        assert_eq!(scan_client_hello(&rec), SniScan::Sni("example.com".into()));
    }

    #[test]
    fn extracts_sni_across_fragmented_records() {
        // Fragment the SAME ClientHello across 4-byte records — the reassembly must
        // recover the SNI (the deliberate-fragmentation evasion).
        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]);
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]);
        body.extend_from_slice(&[0x01, 0x00]);
        let host = b"split.example.com";
        let mut sn = vec![SNI_NAME_TYPE_HOST];
        sn.extend_from_slice(&(host.len() as u16).to_be_bytes());
        sn.extend_from_slice(host);
        let mut list = Vec::new();
        list.extend_from_slice(&(sn.len() as u16).to_be_bytes());
        list.extend_from_slice(&sn);
        let mut exts = Vec::new();
        exts.extend_from_slice(&EXT_SERVER_NAME.to_be_bytes());
        exts.extend_from_slice(&(list.len() as u16).to_be_bytes());
        exts.extend_from_slice(&list);
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);
        let mut hs = vec![HS_CLIENT_HELLO];
        let l = body.len();
        hs.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8]);
        hs.extend_from_slice(&body);

        let rec = framed_records(&hs, 4);
        assert_eq!(
            scan_client_hello(&rec),
            SniScan::Sni("split.example.com".into())
        );
    }

    #[test]
    fn complete_client_hello_without_sni_is_nosni() {
        let rec = client_hello(None);
        assert_eq!(scan_client_hello(&rec), SniScan::NoSni);
    }

    #[test]
    fn non_handshake_first_byte_is_not_tls() {
        // A plain HTTP request byte, or any non-0x16 lead — not TLS.
        assert_eq!(scan_client_hello(b"GET / HTTP/1.1\r\n"), SniScan::NotTls);
    }

    #[test]
    fn truncated_record_is_incomplete() {
        let rec = client_hello(Some("example.com"));
        // Only the record header + a few payload bytes → need more.
        assert_eq!(scan_client_hello(&rec[..8]), SniScan::Incomplete);
        // Even an empty buffer is Incomplete (read more), never a false NotTls.
        assert_eq!(scan_client_hello(&[]), SniScan::Incomplete);
    }

    #[test]
    fn handshake_but_not_client_hello_is_malformed() {
        // A ServerHello (type 0x02) framed as a handshake record must fail closed.
        let hs = vec![0x02, 0x00, 0x00, 0x00];
        let rec = framed_records(&hs, hs.len());
        assert_eq!(scan_client_hello(&rec), SniScan::Malformed);
    }

    #[test]
    fn oversize_dribble_fails_closed() {
        // A handshake header claiming a body larger than the cap, never completing:
        // the reassembler must trip the cap → Malformed (never buffer unbounded nor
        // hang the caller in Incomplete forever once the cap is hit).
        let big = MAX_HANDSHAKE_LEN + 100;
        let mut hs = vec![HS_CLIENT_HELLO];
        hs.extend_from_slice(&[(big >> 16) as u8, (big >> 8) as u8, big as u8]);
        // one record whose declared body length is past the cap once the header is read
        let rec = framed_records(&hs, hs.len());
        assert_eq!(scan_client_hello(&rec), SniScan::Malformed);
    }

    #[test]
    fn non_handshake_record_mid_stream_fails_closed() {
        // First record is a handshake fragment; a second record switches content type
        // (0x17 app-data) before the ClientHello completes → fail closed, never admit
        // a half-read handshake.
        let mut out = Vec::new();
        out.push(CONTENT_HANDSHAKE);
        out.extend_from_slice(&[0x03, 0x01, 0x00, 0x02, HS_CLIENT_HELLO, 0x00]);
        out.push(0x17); // app-data record
        out.extend_from_slice(&[0x03, 0x01, 0x00, 0x01, 0xff]);
        assert_eq!(scan_client_hello(&out), SniScan::Malformed);
    }

    #[test]
    fn truncated_extension_length_fails_closed() {
        // Well-formed up to the extensions block, then an extension whose declared
        // length runs past the buffer → Malformed, not a silent NoSni.
        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]);
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]);
        body.extend_from_slice(&[0x01, 0x00]);
        // extensions total len = 4, but the single extension claims a 0xffff body.
        let mut exts = Vec::new();
        exts.extend_from_slice(&EXT_SERVER_NAME.to_be_bytes());
        exts.extend_from_slice(&[0xff, 0xff]);
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);
        let mut hs = vec![HS_CLIENT_HELLO];
        let l = body.len();
        hs.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8]);
        hs.extend_from_slice(&body);
        let rec = framed_records(&hs, hs.len());
        assert_eq!(scan_client_hello(&rec), SniScan::Malformed);
    }
}
