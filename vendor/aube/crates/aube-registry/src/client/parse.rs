use crate::Error;

/// Bytes of surrounding payload to quote on either side of a decode
/// failure. Wide enough to carry the offending key and its value,
/// narrow enough that a 50 MB packument can't turn one error into a
/// screenful.
const EXCERPT_RADIUS: usize = 60;

/// Quote the payload around `offset` so a decode failure names the
/// field it actually tripped on. serde's message says only what the
/// shape was and what was wanted (`invalid type: map, expected u64`)
/// — with 300 packages in flight and no key in that text, the reader
/// has no way to tell WHICH field of WHICH package was malformed.
fn excerpt(bytes: &[u8], offset: usize) -> String {
    let start = offset.saturating_sub(EXCERPT_RADIUS);
    let end = offset.saturating_add(EXCERPT_RADIUS).min(bytes.len());
    if start >= end {
        return String::new();
    }
    let window = String::from_utf8_lossy(&bytes[start..end]);
    // Packument JSON is single-line in practice, but a mirror may pretty-print
    // it; collapse so the excerpt stays one line inside the error.
    let flattened = window
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    format!(
        "near: {}{flattened}{}",
        if start == 0 { "" } else { "…" },
        if end == bytes.len() { "" } else { "…" }
    )
}

/// Read a response body and decode it, attributing any failure to
/// `context` (e.g. `"packument for lodash"`).
///
/// `context` exists because packument fetches run concurrently: the
/// resolver surfaces whichever decode failed first, so an unattributed
/// message gets blamed on an unrelated package and reads as a different
/// random package on every run.
///
/// Every failure reaching here came off the WIRE, never off disk — all
/// four cached-packument readers in `client/cache.rs` return `Option`
/// and swallow a parse error into a refetch, so a corrupt cache entry
/// cannot surface as a decode error. Callers may state that in
/// `context` without qualification.
pub(super) async fn parse_full_response<T>(
    resp: reqwest::Response,
    context: &str,
) -> Result<T, Error>
where
    T: serde::de::DeserializeOwned,
{
    let body_t0 = std::time::Instant::now();
    let bytes = resp.bytes().await?;
    let body_size = bytes.len();
    aube_util::diag::event_lazy(
        aube_util::diag::Category::Registry,
        "http_body_read",
        body_t0.elapsed(),
        || format!(r#"{{"bytes":{}}}"#, body_size),
    );
    // sonic-rs takes an immutable `&[u8]`, so we don't need to convert
    // `Bytes` into `BytesMut` (which previously cost a 5-50 MB to_vec
    // when the buffer wasn't exclusively owned). `Bytes::deref` is
    // already `&[u8]`, zero-copy regardless of refcount state.
    // sonic-rs is a strict superset of RFC 8259 for valid JSON; the
    // earlier serde_json fallback was dead weight on the happy path
    // and only collapsed into the same `Error::Io(InvalidData)` for
    // the user, so we keep the single-parser shape.
    let parse_t0 = std::time::Instant::now();
    let result = sonic_rs::from_slice::<T>(&bytes).map_err(|e| Error::Decode {
        context: context.to_string(),
        locator: excerpt(&bytes, e.offset()),
        message: e.to_string(),
    });
    aube_util::diag::event_lazy(
        aube_util::diag::Category::Registry,
        "json_parse_sonic_rs",
        parse_t0.elapsed(),
        || format!(r#"{{"bytes":{}}}"#, body_size),
    );
    result
}

#[cfg(test)]
mod tests {
    use super::excerpt;

    #[test]
    fn excerpt_quotes_the_field_around_the_failure_offset() {
        let body = br#"{"name":"pkg","versions":{"1.0.0":{"dist":{"unpackedSize":{"bytes":1}}}}}"#;
        let offset = body
            .windows(14)
            .position(|w| w == b"\"unpackedSize\"")
            .expect("fixture contains the key");
        let quoted = excerpt(body, offset);
        assert!(
            quoted.contains("\"unpackedSize\""),
            "excerpt must name the offending field, got: {quoted}"
        );
    }

    #[test]
    fn excerpt_is_single_line_and_bounded_for_a_large_pretty_printed_body() {
        let mut body = b"{\n  \"a\": 1,\n".to_vec();
        body.extend(std::iter::repeat_n(b' ', 10_000));
        body.extend_from_slice(b"\"unpackedSize\": {}\n}");
        let offset = body.len() - 20;
        let quoted = excerpt(&body, offset);
        assert!(!quoted.contains('\n'), "excerpt must stay on one line");
        // Two radii of payload plus the two ellipsis marks.
        assert!(
            quoted.chars().count() <= 60 * 2 + 2,
            "excerpt must stay bounded, got {} chars",
            quoted.chars().count()
        );
    }

    #[test]
    fn excerpt_of_an_offset_past_the_body_is_empty_rather_than_panicking() {
        assert_eq!(excerpt(b"{}", 9_999), "");
    }
}
