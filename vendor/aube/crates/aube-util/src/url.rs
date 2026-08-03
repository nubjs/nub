const INVALID_REGISTRY_URL: &str = "<invalid registry URL>";

/// Return a locator safe for diagnostic output.
///
/// Strip query and fragment components before parsing or considering any
/// fallback. They are never part of the stable locator and often carry signed
/// credentials. Unknown locators fail closed: this renderer is for registry
/// and tarball diagnostics, not arbitrary user prose.
pub fn redact_url(url: &str) -> String {
    let locator = strip_query_and_fragment(url);
    if let Some(mut parsed) = parse_url_like(locator) {
        if parsed.set_password(None).is_err() || parsed.set_username("").is_err() {
            return INVALID_REGISTRY_URL.to_string();
        }
        return parsed.to_string();
    }

    // `npm:@scope/pkg` includes a colon before the package's `@`; recognize
    // that exact grammar before userinfo rejection. Every other ambiguous
    // authority goes through the fail-closed path below.
    if is_npm_scoped_alias(locator) {
        return locator.to_string();
    }
    if has_authority_like_userinfo(locator) {
        return INVALID_REGISTRY_URL.to_string();
    }
    if is_scoped_package_reference(locator) {
        return locator.to_string();
    }

    INVALID_REGISTRY_URL.to_string()
}

/// Alias for [`redact_url`] at user-facing diagnostic call sites.
///
/// Keeping a single policy prevents signed remote-tarball URLs from being
/// rendered less safely by one diagnostic path than another.
pub fn display_url(url: &str) -> String {
    redact_url(url)
}

/// Render a dependency range in a diagnostic without treating it as a URL.
///
/// Ordinary semver expressions retain their useful text. Scoped package and
/// `npm:` alias syntax use the same narrow validation as [`display_url`]. All
/// URL-shaped or otherwise ambiguous values delegate to the fail-closed URL
/// renderer.
pub fn display_package_range(value: &str) -> String {
    let range = strip_query_and_fragment(value);
    if is_safe_semver_range(range)
        || is_scoped_package_reference(range)
        || is_npm_scoped_alias(range)
    {
        return range.to_string();
    }
    display_url(range)
}

/// Parse absolute network URL-like text, including special-scheme shorthand
/// such as `https:/host` and `https:////host`. Scheme-relative URLs borrow
/// `https:` for parsing and render as a canonical HTTPS locator after
/// redaction. `file:` locators always fail closed, including those with an
/// authority component.
fn parse_url_like(url: &str) -> Option<reqwest::Url> {
    if url
        .as_bytes()
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"file:"))
    {
        return None;
    }

    if url.starts_with("//") {
        return reqwest::Url::parse(&format!("https:{url}")).ok();
    }

    let parsed = reqwest::Url::parse(url).ok()?;
    (parsed.scheme() != "file" && parsed.host_str().is_some()).then_some(parsed)
}

fn strip_query_and_fragment(value: &str) -> &str {
    &value[..value.find(['?', '#']).unwrap_or(value.len())]
}

/// A pre-`@` colon indicates userinfo authority data. Do not require dotted,
/// port-bearing, or otherwise URL-like host punctuation: `user:pass@localhost`
/// is credential-bearing just as surely as `user:pass@registry.example`.
fn has_authority_like_userinfo(value: &str) -> bool {
    let Some((before_at, after_at)) = value.rsplit_once('@') else {
        return false;
    };
    !before_at.is_empty() && before_at.contains(':') && !after_at.is_empty()
}

/// Validate the conservative shared grammar for `@scope/name[@range]`.
///
/// URL-shaped syntax, a second `@`, or a colon in the range is never a
/// package-display escape hatch. It falls through to the constant locator.
fn is_scoped_package_reference(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('@') else {
        return false;
    };
    let Some((scope, name_and_range)) = rest.split_once('/') else {
        return false;
    };
    let (name, range) = name_and_range
        .split_once('@')
        .unwrap_or((name_and_range, ""));

    is_package_identifier(scope) && is_package_identifier(name) && is_safe_package_range(range)
}

/// Validate the one alias form that must precede authority rejection.
fn is_npm_scoped_alias(value: &str) -> bool {
    value
        .strip_prefix("npm:")
        .is_some_and(is_scoped_package_reference)
}

fn is_package_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn is_safe_package_range(value: &str) -> bool {
    value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'.' | b'-' | b'+' | b'^' | b'~' | b'<' | b'>' | b'=' | b'*' | b'|' | b' '
            )
    })
}

fn is_safe_semver_range(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'.' | b'-' | b'+' | b'^' | b'~' | b'<' | b'>' | b'=' | b'*' | b'|' | b' '
                )
        })
}

#[cfg(test)]
mod tests {
    use super::{INVALID_REGISTRY_URL, display_package_range, display_url, redact_url};

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
    fn strips_suffixes_and_fails_closed_for_scheme_less_bad_ranges() {
        let input = "@user:pass@localhost/path?token=opaque#fragment";
        let display = display_url(input);

        assert_eq!(display, INVALID_REGISTRY_URL);
        for secret in [
            "user",
            "pass",
            "localhost",
            "token",
            "opaque",
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
    fn scheme_less_locator_never_falls_back_with_query_or_fragment() {
        let display = display_url("registry.internal/pkg?token=opaque#fragment");
        assert_eq!(display, INVALID_REGISTRY_URL);
        for secret in [
            "registry.internal",
            "pkg",
            "token",
            "opaque",
            "fragment",
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
    fn package_range_renderer_preserves_semver_but_rejects_bad_authority_ranges() {
        assert_eq!(display_package_range("^1.2.3 || >=2"), "^1.2.3 || >=2");
        assert_eq!(
            display_package_range("@user:pass@localhost/path?token=opaque#fragment"),
            INVALID_REGISTRY_URL
        );
    }
    #[test]
    fn preserves_only_valid_npm_scoped_alias_after_suffix_stripping() {
        assert_eq!(
            display_url("npm:@scope/package?token=opaque#fragment"),
            "npm:@scope/package"
        );
        assert_eq!(
            display_url("npm:@scope/package@user:pass@localhost"),
            INVALID_REGISTRY_URL
        );
    }

    #[test]
    fn fails_closed_for_nested_transport_and_hostless_file_locators() {
        for input in [
            "git:https://user:pass@host/repo?token=opaque#fragment",
            "file:///private/path?token=opaque#fragment",
        ] {
            let display = display_url(input);
            assert_eq!(display, INVALID_REGISTRY_URL, "for {input}");
            for secret in [
                "user", "pass", "host", "private", "token", "opaque", "fragment", "?", "#",
            ] {
                assert!(
                    !display.contains(secret),
                    "display URL leaked {secret:?}: {display}"
                );
            }
        }
    }

    #[test]
    fn fails_closed_for_file_locators_without_leaking_authority_or_suffixes() {
        for input in [
            "file:///private/path?token=opaque#fragment",
            "file:/private/path?token=opaque#fragment",
            "file://host/private/path?token=opaque#fragment",
            "file://token@host/private/path?token=opaque#fragment",
            "file://user:password@host/private/path?token=opaque#fragment",
        ] {
            let display = display_url(input);
            assert_eq!(display, INVALID_REGISTRY_URL, "for {input}");
            for secret in [
                "user", "password", "host", "private", "path", "token", "opaque", "fragment", "@",
                "?", "#",
            ] {
                assert!(
                    !display.contains(secret),
                    "display URL leaked {secret:?}: {display}"
                );
            }
        }
    }

    #[test]
    fn hosted_file_locator_never_panics_or_renders_sensitive_components() {
        let input = "file://host/private/path?token=opaque#fragment";
        let rendered = std::panic::catch_unwind(|| display_url(input));
        let Ok(display) = rendered else {
            panic!("hosted file locator must not panic while redacting");
        };

        assert_eq!(display, INVALID_REGISTRY_URL);
        for secret in [
            "host", "private", "path", "token", "opaque", "fragment", "?", "#",
        ] {
            assert!(
                !display.contains(secret),
                "display URL leaked {secret:?}: {display}"
            );
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
