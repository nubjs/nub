/**
 * Redact bearer credentials from a URL before it lands in error
 * messages, trace logs, or diagnostic output.
 *
 * Three credential shapes are scrubbed:
 *   - `user:password@host` userinfo (Artifactory, Nexus, JFrog,
 *     GitHub Packages, scoped npm registries with embedded auth).
 *   - Sensitive query parameters (`token`, `auth`, `api_key`,
 *     `apikey`, `access_token`) — values are replaced with `***`.
 *   - Returns the input unchanged when no credential pattern is
 *     present.
 */
pub fn redact_url(url: &str) -> String {
    let after_userinfo = redact_userinfo(url);
    redact_query_tokens(&after_userinfo)
}

/// Redact credentials and omit query/fragment components for diagnostic URLs.
///
/// Registry URLs may carry read credentials as query parameters or userinfo.
/// A diagnostic does not need either component to identify the endpoint, so
/// remove them rather than merely masking individual query values.
pub fn display_url(url: &str) -> String {
    // The locator ends before a query or fragment. Split the original string
    // first: an `@` inside either suffix is data, never URL userinfo.
    let locator_end = url.find(['?', '#']).unwrap_or(url.len());
    redact_userinfo(&url[..locator_end])
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
    let Some(at) = tail.find('@') else {
        return url.to_string();
    };
    let authority_end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
    if at >= authority_end {
        return url.to_string();
    }
    format!("{}***@{}", &url[..after], &tail[at + 1..])
}

/**
 * Replace the value of any well-known credential query parameter with
 * `***`. Matching is case-insensitive on the parameter name.
 */
fn redact_query_tokens(url: &str) -> String {
    let Some(qpos) = url.find('?') else {
        return url.to_string();
    };
    let (head, query_full) = url.split_at(qpos);
    let query = &query_full[1..];
    // Keep the optional fragment intact.
    let (query_only, fragment) = match query.find('#') {
        Some(h) => (&query[..h], &query[h..]),
        None => (query, ""),
    };
    const SENSITIVE: &[&str] = &["token", "auth", "api_key", "apikey", "access_token"];
    let mut out = String::with_capacity(url.len());
    out.push_str(head);
    out.push('?');
    let mut first = true;
    for pair in query_only.split('&') {
        if !first {
            out.push('&');
        }
        first = false;
        if let Some(eq) = pair.find('=') {
            let (k, v) = pair.split_at(eq);
            let lower = k.to_ascii_lowercase();
            if SENSITIVE.iter().any(|s| lower == *s) {
                out.push_str(k);
                out.push_str("=***");
                let _ = v;
            } else {
                out.push_str(pair);
            }
        } else {
            out.push_str(pair);
        }
    }
    out.push_str(fragment);
    out
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
    fn redacts_query_token() {
        assert_eq!(
            redact_url("https://reg.example.com/x?token=abc123&v=1"),
            "https://reg.example.com/x?token=***&v=1"
        );
    }

    #[test]
    fn redacts_query_auth_case_insensitive() {
        assert_eq!(
            redact_url("https://reg.example.com/x?Auth=secret"),
            "https://reg.example.com/x?Auth=***"
        );
    }

    #[test]
    fn redacts_query_apikey_alias() {
        assert_eq!(
            redact_url("https://reg.example.com/x?apikey=abc&api_key=def"),
            "https://reg.example.com/x?apikey=***&api_key=***"
        );
    }

    #[test]
    fn preserves_fragment_when_redacting_query() {
        assert_eq!(
            redact_url("https://reg.example.com/x?token=abc#section"),
            "https://reg.example.com/x?token=***#section"
        );
    }

    #[test]
    fn passthrough_when_query_has_no_sensitive_keys() {
        assert_eq!(
            redact_url("https://reg.example.com/x?foo=1&bar=2"),
            "https://reg.example.com/x?foo=1&bar=2"
        );
    }

    #[test]
    fn display_url_omits_query_fragment_and_userinfo_credentials() {
        let input = format!(
            "https://registry-user:registry-pass{}registry.example.com/npm?token=transport-secret#fragment",
            '\u{40}'
        );
        assert_eq!(
            display_url(&input),
            format!("https://***{}registry.example.com/npm", '\u{40}')
        );
    }

    #[test]
    fn generic_redactor_only_treats_at_signs_in_authorities_as_userinfo() {
        assert_eq!(
            redact_url("https://registry.example?token=prefix@query-secret"),
            "https://registry.example?token=***"
        );
        assert_eq!(
            redact_url("ftp://registry.example/npm#fragment@fragment-secret"),
            "ftp://registry.example/npm#fragment@fragment-secret"
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
