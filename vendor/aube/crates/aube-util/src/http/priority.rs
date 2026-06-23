//! RFC 9218 HTTP priority signal builder.
//!
//! Cold install issues two request classes against the same origin:
//! tiny critical packuments (resolver-blocking) and large tarballs
//! (fetch-phase, can stream lazily). Marking packuments urgent lets
//! HTTP/2-aware origins schedule them ahead of pending tarball frames
//! on the same connection.

use std::fmt::Write;

/// RFC 9218 §4.1 — request urgency, 0 = highest, 7 = lowest. Default
/// 3 matches RFC §4.1 when no header is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Urgency {
    /// Resolver-blocking packument metadata.
    Critical = 0,
    /// Lockfile-driven tarball reads.
    High = 1,
    /// Default per RFC 9218 §4.1.
    Default = 3,
    /// Background prefetch / speculative fetches.
    Background = 7,
}

/// Build the `Priority:` header value per RFC 9218 §4. `incremental`
/// signals the server may deliver the response in chunks (true for
/// tarballs, false for packuments where the consumer parses JSON whole).
pub fn header_value(urgency: Urgency, incremental: bool) -> String {
    let mut out = String::with_capacity(16);
    let _ = write!(out, "u={}", urgency as u8);
    if incremental {
        out.push_str(", i");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_default_is_u3() {
        assert_eq!(header_value(Urgency::Default, false), "u=3");
    }

    #[test]
    fn header_critical_packument() {
        assert_eq!(header_value(Urgency::Critical, false), "u=0");
    }

    #[test]
    fn header_tarball_incremental() {
        assert_eq!(header_value(Urgency::High, true), "u=1, i");
    }

    #[test]
    fn header_background_streaming() {
        assert_eq!(header_value(Urgency::Background, true), "u=7, i");
    }
}
