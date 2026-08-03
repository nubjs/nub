/// Return a URL safe for diagnostic output.
///
/// URLs are transport identity, not a safe diagnostic payload: userinfo and
/// every query or fragment component can carry bearer credentials, signatures,
/// or opaque capability data. Keep only the authority and path so callers can
/// identify the endpoint without leaking its request credentials. A malformed
/// authority-like value with userinfo is not safe to show at all.
pub fn redact_url(url: &str) -> String {
    let locator_end = url.find(['?', '#']).unwrap_or(url.len());
    let locator = &url[..locator_end];
    if has_unparseable_authority_userinfo(locator) {
        return "<invalid registry URL>".to_string();
    }
    redact_userinfo(locator)
}

/// Alias for [`redact_url`] at user-facing diagnostic call sites.
///
/// Keeping a single policy prevents signed remote-tarball URLs from being
/// rendered less safely by one diagnostic path than another.
pub fn display_url(url: &str) -> String {
    redact_url(url)
}

/// Whether an otherwise scheme-less value has userinfo-shaped authority data.
/// Registry URLs require an authority marker (`://` or `//`). A value such as
/// `user:password@host/path` is invalid but contains credentials; emitting it
/// verbatim would leak them. Recognized package-source protocols deliberately
/// remain displayable, because their `@` characters are package syntax rather
/// than URL userinfo.
fn has_unparseable_authority_userinfo(locator: &str) -> bool {
    if locator.contains("://") || locator.starts_with("//") || is_package_source(locator) {
        return false;
    }

    let authority_end = locator.find('/').unwrap_or(locator.len());
    let authority = &locator[..authority_end];
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return false;
    };
    !userinfo.is_empty() && !host.is_empty() && userinfo.contains(':')
}

fn is_package_source(specifier: &str) -> bool {
    [
        "npm:",
        "file:",
        "link:",
        "portal:",
        "workspace:",
        "catalog:",
        "patch:",
        "exec:",
        "git:",
        "github:",
        "gitlab:",
        "bitbucket:",
    ]
    .iter()
    .any(|prefix| specifier.starts_with(prefix))
}

/**
 * Redact only the `user:password@` portion of `url`, if any.
 *
 * Handles both fully-qualified (`scheme://user:pw@host`) and
 * scheme-relative (`//user:pw@host`) inputs.
 */
fn redact_userinfo(url: &str) -> String {
    let after = if let Some(scheme_end) = url.find("://") {
        scheme_end + 3
    } else if url.starts_with("//") {
        2
    } else {
        return url.to_string();
    };
    let tail = &url[after..];
    let authority_end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
    let Some(at) = tail[..authority_end].rfind('@') else {
        return url.to_string();
    };
    format!("{}***@{}", &url[..after], &tail[at + 1..])
}

#[cfg(test)]
mod tests {
    use super::{display_url, redact_url};

    #[test]
    fn passthrough_when_no_userinfo() {
        assert_eq!(
            redact_url("https://registry.example.com/foo"),
            "https://registry.example.com/foo"
        );
    }

    #[test]
    fn redacts_user_and_password() {
        let input = format!("https://user:hunter2{}host.example.com/x", '\u{40}');
        let expected = format!("https://***{}host.example.com/x", '\u{40}');
        assert_eq!(redact_url(&input), expected);
    }

    #[test]
    fn redacts_all_multi_at_userinfo_before_the_authority() {
        let input =
            "https://user:pass@password-tail@registry.example:4873/npm?signature=signed#fragment";
        let display = display_url(input);

        assert_eq!(display, "https://***@registry.example:4873/npm");
        for leaked in [
            "user",
            "pass",
            "password-tail",
            "signature",
            "signed",
            "fragment",
        ] {
            assert!(
                !display.contains(leaked),
                "display URL leaked {leaked:?}: {display}"
            );
        }
    }

    #[test]
    fn replaces_scheme_less_userinfo_shaped_registry_with_constant() {
        let input = "user:pass@password-tail@registry.example/npm?token=opaque@token#fragment";
        let display = display_url(input);

        assert_eq!(display, "<invalid registry URL>");
        for leaked in ["user", "pass", "password-tail", "token", "fragment", "@"] {
            assert!(
                !display.contains(leaked),
                "display URL leaked {leaked:?}: {display}"
            );
        }
    }

    #[test]
    fn preserves_non_url_package_aliases() {
        assert_eq!(
            display_url("npm:@scope/package@1.2.3"),
            "npm:@scope/package@1.2.3"
        );
    }

    #[test]
    fn does_not_redact_at_in_path() {
        let input = format!("https://host/foo{}1.0.0/bar", '\u{40}');
        assert_eq!(redact_url(&input), input);
    }

    #[test]
    fn redacts_userinfo_with_ipv6_host() {
        let input = format!("https://tok{}[::1]:8443/x", '\u{40}');
        let expected = format!("https://***{}[::1]:8443/x", '\u{40}');
        assert_eq!(redact_url(&input), expected);
    }

    #[test]
    fn redacts_scheme_relative_userinfo() {
        let input = format!("//user:pw{}host.example.com/x", '\u{40}');
        let expected = format!("//***{}host.example.com/x", '\u{40}');
        assert_eq!(redact_url(&input), expected);
    }

    #[test]
    fn strips_query_and_fragment_from_diagnostic_urls() {
        assert_eq!(
            redact_url("https://reg.example.com/x?token=abc123&v=1#section"),
            "https://reg.example.com/x"
        );
        assert_eq!(
            redact_url("https://reg.example.com/x?signature=signed-url-secret"),
            "https://reg.example.com/x"
        );
        assert_eq!(
            redact_url("https://reg.example.com/x#opaque-fragment"),
            "https://reg.example.com/x"
        );
    }

    #[test]
    fn strips_suffixes_before_redacting_authority_userinfo() {
        assert_eq!(
            redact_url("https://registry.example?signature=prefix@query-secret"),
            "https://registry.example"
        );
        assert_eq!(
            redact_url("ftp://registry.example/npm#fragment@fragment-secret"),
            "ftp://registry.example/npm"
        );
    }

    #[test]
    fn display_url_treats_at_signs_in_query_and_fragment_as_suffix_data() {
        let cases = [
            (
                "https://registry.example?token=prefix@query-secret",
                "https://registry.example",
            ),
            (
                "ftp://user:password@registry.example/npm?token=prefix@query-secret",
                "ftp://***@registry.example/npm",
            ),
            (
                "https://registry.example#fragment@fragment-secret",
                "https://registry.example",
            ),
            (
                "file://user:password@registry.example/npm#fragment@fragment-secret",
                "file://***@registry.example/npm",
            ),
        ];

        for (input, expected) in cases {
            let display = display_url(input);
            assert_eq!(display, expected, "unexpected display URL for {input}");
            for leaked in ["query-secret", "fragment-secret", "?", "#"] {
                assert!(
                    !display.contains(leaked),
                    "display URL leaked {leaked:?}: {display}"
                );
            }
        }
    }
}
