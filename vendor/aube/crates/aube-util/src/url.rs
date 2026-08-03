const INVALID_REGISTRY_URL: &str = "<invalid registry URL>";

/// Return a locator safe for diagnostic output.
///
/// Request routing, auth-key construction, and diagnostics deliberately use
/// different URL representations. Diagnostics parse an absolute or
/// scheme-relative URL before rendering, then remove authority userinfo,
/// queries, and fragments. Parsing first makes one- and extra-slash HTTP(S)
/// spellings canonical before any confidential component can reach output.
pub fn redact_url(url: &str) -> String {
    if let Some(mut parsed) = parse_url_like(url) {
        parsed
            .set_password(None)
            .expect("parsed URL accepts cleared password");
        parsed
            .set_username("")
            .expect("parsed URL accepts cleared username");
        parsed.set_query(None);
        parsed.set_fragment(None);
        return parsed.to_string();
    }

    if is_non_url_display_value(url) {
        return url.to_string();
    }

    if is_http_url_like(url)
        || has_scheme_relative_authority(url)
        || has_authority_like_userinfo(url)
    {
        return INVALID_REGISTRY_URL.to_string();
    }

    url.to_string()
}

/// Alias for [`redact_url`] at user-facing diagnostic call sites.
///
/// Keeping a single policy prevents signed remote-tarball URLs from being
/// rendered less safely by one diagnostic path than another.
pub fn display_url(url: &str) -> String {
    redact_url(url)
}

/// Parse absolute URL-like text, including special-scheme shorthand such as
/// `https:/host` and `https:////host`. Scheme-relative URLs borrow `https:`
/// for parsing and render as a canonical HTTPS locator after redaction.
fn parse_url_like(url: &str) -> Option<reqwest::Url> {
    if url.starts_with("//") {
        return reqwest::Url::parse(&format!("https:{url}")).ok();
    }

    let parsed = reqwest::Url::parse(url).ok()?;
    parsed.host_str().is_some().then_some(parsed)
}

fn is_http_url_like(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
}

fn has_scheme_relative_authority(value: &str) -> bool {
    value.starts_with("//")
}

/// A scheme-less value with nonempty data before its final `@` is ambiguous
/// credential-bearing authority data, not a safe locator. Package references
/// get a deliberately narrow escape hatch in [`is_non_url_display_value`].
fn has_authority_like_userinfo(value: &str) -> bool {
    let locator_end = value.find(['?', '#']).unwrap_or(value.len());
    let locator = &value[..locator_end];
    let Some((before_at, after_at)) = locator.rsplit_once('@') else {
        return false;
    };
    !before_at.is_empty()
        && !after_at.is_empty()
        && (after_at.contains('/') || after_at.contains('.') || after_at.contains(':'))
}

/// Values that are package syntax rather than URLs must retain their `@`s.
/// Do not broaden this list: URL-shaped input must go through the parser or
/// fail closed instead of becoming a new diagnostic disclosure path.
fn is_non_url_display_value(value: &str) -> bool {
    (value.starts_with('@') && value.contains('/'))
        || [
            "npm:",
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
        .any(|prefix| value.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::{INVALID_REGISTRY_URL, display_url, redact_url};

    #[test]
    fn canonicalizes_safe_http_locator() {
        assert_eq!(
            redact_url("https://registry.example.com/foo"),
            "https://registry.example.com/foo"
        );
    }

    #[test]
    fn clears_userinfo_query_and_fragment_before_rendering() {
        let display =
            display_url("https://user:password@registry.example.com/npm?signature=signed#fragment");

        assert_eq!(display, "https://registry.example.com/npm");
        for secret in [
            "user",
            "password",
            "signature",
            "signed",
            "fragment",
            "@",
            "?",
            "#",
        ] {
            assert!(
                !display.contains(secret),
                "display URL leaked {secret:?}: {display}"
            );
        }
    }

    #[test]
    fn canonicalizes_one_and_extra_slash_http_authorities_before_clearing_credentials() {
        for input in [
            "https:/user:password@registry.example.com/npm?token=one#fragment",
            "https:////user:password@registry.example.com/npm?token=two#fragment",
        ] {
            let display = display_url(input);
            assert_eq!(display, "https://registry.example.com/npm", "for {input}");
            for secret in ["user", "password", "token", "fragment", "@", "?", "#"] {
                assert!(
                    !display.contains(secret),
                    "display URL leaked {secret:?}: {display}"
                );
            }
        }
    }

    #[test]
    fn clears_username_only_userinfo_before_rendering() {
        let display = display_url("http://token@registry.example.com/npm");
        assert_eq!(display, "http://registry.example.com/npm");
        assert!(!display.contains("token"));
    }

    #[test]
    fn fails_closed_for_scheme_less_authority_like_userinfo() {
        for input in [
            "user:pass@registry.example.com/npm?token=opaque#fragment",
            "opaque-token@host/path",
        ] {
            let display = display_url(input);
            assert_eq!(display, INVALID_REGISTRY_URL);
            for secret in ["user", "pass", "opaque", "token", "host", "@"] {
                assert!(
                    !display.contains(secret),
                    "display URL leaked {secret:?}: {display}"
                );
            }
        }
    }

    #[test]
    fn preserves_scoped_package_syntax_outside_url_display_path() {
        for specifier in ["@scope/package@1.2.3", "npm:@scope/package@1.2.3"] {
            assert_eq!(display_url(specifier), specifier);
        }
    }

    #[test]
    fn clears_absolute_url_credentials_or_fails_closed_when_invalid() {
        assert_eq!(
            display_url("ftp://user:password@registry.example/npm?token=opaque#fragment"),
            "ftp://registry.example/npm"
        );
        assert_eq!(
            display_url("file://user:password@registry.example/npm#fragment"),
            INVALID_REGISTRY_URL
        );
    }
}
