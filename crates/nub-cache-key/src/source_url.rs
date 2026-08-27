//! The `//# sourceURL` a transpiled body carries, spelled exactly as the HOST
//! Node's `pathToFileURL(path).href` would spell it.
//!
//! Node's loader computes a module's own URL with `pathToFileURL`, so a sourceURL
//! that disagrees with it gives one file two identities inside a single process:
//! `Debugger.scriptParsed` reports nub's spelling while a breakpoint set against
//! the module URL carries Node's, and the two never match. That is why the
//! encoding is pinned to the host rather than to the URL spec.
//!
//! The URL FORM is not host-gated. Node's own type stripping only started
//! reporting a `file:` URL in v26.4.0 (backported to v24.19.0); before that it
//! reported the bare path, and v25.x never got the change. Reproducing that would
//! flip nub's script identity on a patch bump, so nub always emits the URL — the
//! form upstream is migrating toward. Only the ENCODING, which decides whether
//! two URLs for one file are equal, follows the host.

/// Whether the host's `pathToFileURL` percent-encodes the widened characters
/// `[`, `]`, `^`, `|` and `~`.
///
/// nodejs/node#54545 widened the escape set, and it reached the release lines out
/// of order, so this cannot be reduced to a single floor. All five characters move
/// together in one step — verified by sweeping every printable byte through real
/// binaries at each boundary tag:
///
/// | Host band | Widened |
/// | --- | --- |
/// | 18.x (through 18.20.8) | no |
/// | 20.0.0 – 20.18.2 | no |
/// | 20.18.3+ | yes |
/// | 21.x (through 21.7.3) | no |
/// | 22.0.0 – 22.11.x | no |
/// | 22.12.0+ | yes |
/// | 23.0.x | no |
/// | 23.1.0+ | yes |
/// | 24.0.0+ | yes |
///
/// v20.18.3 and v22.12.0 are the same behavior by two different mechanisms —
/// v20/v22 got widened regexes in the pure-JS `pathToFileURL`, and the move to
/// ada's native `href_from_file` came later — so the band is about the escape
/// set, not about which implementation produces it.
///
/// An unparseable or absent version reads as WIDENED: every release line still
/// receiving patches is above its boundary, so that is the answer that is right
/// for anything nub will actually meet.
pub fn host_widens_url_path(node_version: &str) -> bool {
    let Some((major, minor, patch)) = parse_version(node_version) else {
        return true;
    };
    match major {
        0..=19 => false,
        20 => (minor, patch) >= (18, 3),
        21 => false,
        22 => minor >= 12,
        23 => minor >= 1,
        _ => true,
    }
}

/// `v22.12.0` / `22.12.0` → `(22, 12, 0)`. Trailing prerelease or build metadata
/// is ignored; anything that does not start with three numeric components is
/// rejected so the caller can apply its own default.
fn parse_version(raw: &str) -> Option<(u32, u32, u32)> {
    let trimmed = raw.strip_prefix('v').unwrap_or(raw);
    let mut parts = trimmed.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let tail = parts.next()?;
    let end = tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    let patch = tail[..end].parse().ok()?;
    Some((major, minor, patch))
}

/// The cache-key component naming the encoding band, so a body emitted for one
/// band is never served to a host on the other.
pub fn band_tag(widened: bool) -> &'static str {
    if widened { "url-wide" } else { "url-narrow" }
}

/// Whether a byte must be percent-encoded inside a `file:` URL path.
///
/// The always-escaped set is Node's `encodePathChars` plus the WHATWG path
/// percent-encode set, which between them cover the C0 controls, space, `"`, `#`,
/// `%`, `<`, `>`, `?`, `\`, backtick, `{`, `}`, DEL and every non-ASCII byte. The
/// `widened` five ride on [`host_widens_url_path`]. Both halves were read off real
/// binaries rather than off the URL spec, because the spec set matches neither
/// band exactly.
fn must_percent_encode(b: u8, widened: bool) -> bool {
    if !b.is_ascii() {
        return true;
    }
    if widened && matches!(b, b'[' | b']' | b'^' | b'|' | b'~') {
        return true;
    }
    matches!(
        b,
        0x00..=0x20 | b'"' | b'#' | b'%' | b'<' | b'>' | b'?' | b'\\' | b'`' | b'{' | b'}' | 0x7f
    )
}

fn push_encoded(out: &mut String, s: &str, widened: bool) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for b in s.bytes() {
        if must_percent_encode(b, widened) {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        } else {
            out.push(b as char);
        }
    }
}

/// The `file:` URL spelling of `path`, byte-identical to the host's
/// `pathToFileURL(path).href`.
///
/// `path` always arrives absolute in production (the loader derives it with
/// `fileURLToPath`); anything else is passed through unconverted, matching Node's
/// `convertCJSFilenameToURL` fallback. `windows` is a parameter rather than a
/// `cfg!` so both branches are exercisable from either host.
pub fn file_url(path: &str, windows: bool, widened: bool) -> String {
    let mut url = String::with_capacity(path.len() + 16);
    if !windows {
        if !path.starts_with('/') {
            return path.to_string();
        }
        url.push_str("file://");
        push_encoded(&mut url, path, widened);
        return url;
    }
    let forward = path.replace('\\', "/");
    if let Some(rest) = forward.strip_prefix("//") {
        // UNC `\\server\share\…` → `file://server/share/…`. The hostname is a URL
        // authority rather than a path segment, so it is not percent-encoded.
        let host_end = rest.find('/').unwrap_or(rest.len());
        url.push_str("file://");
        url.push_str(&rest[..host_end]);
        push_encoded(&mut url, &rest[host_end..], widened);
        return url;
    }
    let bytes = forward.as_bytes();
    let drive_absolute =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
    if !drive_absolute {
        return path.to_string();
    }
    url.push_str("file:///");
    push_encoded(&mut url, &forward, widened);
    url
}

#[cfg(test)]
mod tests {
    use super::{file_url, host_widens_url_path};

    /// The band boundaries, taken from running `pathToFileURL('/x<c>y')` over
    /// every printable ASCII byte on real binaries at each tag — including the
    /// three that pin the v20 boundary (20.18.2 narrow, 20.18.3 wide) and the last
    /// release of each line that never widened (18.20.8, 21.7.3).
    #[test]
    fn band_boundaries_match_the_measured_releases() {
        for narrow in [
            "v18.19.0", "v18.20.8", "v20.10.0", "v20.18.0", "v20.18.1", "v20.18.2", "v21.1.0",
            "v21.7.3", "v22.6.0", "v22.11.0", "v23.0.0",
        ] {
            assert!(
                !host_widens_url_path(narrow),
                "{narrow} does not escape [ ] ^ | ~"
            );
        }
        for wide in [
            "v20.18.3", "v20.19.0", "v22.12.0", "v22.15.0", "v23.1.0", "v23.3.0", "v24.0.0",
            "v26.7.0",
        ] {
            assert!(host_widens_url_path(wide), "{wide} escapes [ ] ^ | ~");
        }
    }

    /// A version nub cannot parse must not silently pick the legacy spelling: a
    /// nightly, a `-pre` build, or a future line is far likelier to be widened.
    #[test]
    fn unparseable_version_defaults_to_widened() {
        for odd in ["", "not-a-version", "v99", "v27.0.0-pre"] {
            assert!(host_widens_url_path(odd), "{odd:?} must read as widened");
        }
    }

    /// The characters that move with the band, and the ones that must not.
    #[test]
    fn widened_characters_follow_the_band() {
        assert_eq!(
            file_url("/w[x]y^z~q|r", false, true),
            "file:///w%5Bx%5Dy%5Ez%7Eq%7Cr"
        );
        assert_eq!(
            file_url("/w[x]y^z~q|r", false, false),
            "file:///w[x]y^z~q|r"
        );
        // Escaped in BOTH bands — captured from v18.19.0 and v26.7.0 alike.
        for band in [true, false] {
            assert_eq!(
                file_url("/a b%c#d?e", false, band),
                "file:///a%20b%25c%23d%3Fe"
            );
            // On POSIX a backslash is an ordinary filename byte, never a separator.
            assert_eq!(file_url("/x\\y", false, band), "file:///x%5Cy");
        }
    }

    /// Non-ASCII goes out as percent-encoded UTF-8, uppercase hex, in both bands.
    #[test]
    fn non_ascii_is_percent_encoded_utf8() {
        for band in [true, false] {
            assert_eq!(file_url("/xé€y", false, band), "file:///x%C3%A9%E2%82%ACy");
        }
    }

    /// Windows drive paths, against `pathToFileURL(p, { windows: true }).href`
    /// captured on v26.7.0: backslashes become separators (never `%5C`, unlike
    /// POSIX), and the drive letter sits in the path, not the authority.
    #[test]
    fn windows_drive_paths_match_captured_output() {
        assert_eq!(
            file_url(r"C:\a b\x~y.ts", true, true),
            "file:///C:/a%20b/x%7Ey.ts"
        );
        assert_eq!(
            file_url(r"C:\a b\x~y.ts", true, false),
            "file:///C:/a%20b/x~y.ts"
        );
        assert_eq!(
            file_url(r"C:\a%b\x.ts", true, true),
            "file:///C:/a%25b/x.ts"
        );
        assert_eq!(
            file_url(r"C:\a\x#y?z.ts", true, true),
            "file:///C:/a/x%23y%3Fz.ts"
        );
        assert_eq!(
            file_url(r"C:\a\x[y]^z|w.ts", true, true),
            "file:///C:/a/x%5By%5D%5Ez%7Cw.ts"
        );
        assert_eq!(
            file_url(r"C:\a\x[y]^z|w.ts", true, false),
            "file:///C:/a/x[y]^z|w.ts"
        );
        // An already-forward-slashed drive path is accepted unchanged.
        assert_eq!(file_url("C:/a/b.ts", true, true), "file:///C:/a/b.ts");
    }

    /// UNC paths, same capture: the share host is the URL authority and stays
    /// unencoded, while the path after it is encoded normally.
    #[test]
    fn windows_unc_paths_keep_the_host_unencoded() {
        assert_eq!(
            file_url(r"\\server\share\a b\x.ts", true, true),
            "file://server/share/a%20b/x.ts"
        );
        assert_eq!(
            file_url(r"\\server\share\w[x]~y.ts", true, true),
            "file://server/share/w%5Bx%5D%7Ey.ts"
        );
        assert_eq!(
            file_url(r"\\server\share\w[x]~y.ts", true, false),
            "file://server/share/w[x]~y.ts"
        );
    }

    /// A non-absolute input is passed through untouched on either platform,
    /// matching Node's `convertCJSFilenameToURL`, which only converts when
    /// `isAbsolute(filename)`.
    #[test]
    fn non_absolute_input_is_left_alone() {
        assert_eq!(file_url("[eval]", false, true), "[eval]");
        assert_eq!(file_url("relative/x.ts", false, true), "relative/x.ts");
        assert_eq!(file_url(r"relative\x.ts", true, true), r"relative\x.ts");
        // Rooted but not drive-qualified is not absolute on Windows.
        assert_eq!(file_url("/rooted/x.ts", true, true), "/rooted/x.ts");
    }
}
