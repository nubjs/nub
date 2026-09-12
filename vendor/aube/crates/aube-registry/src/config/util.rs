/// Map an empty string to `None` so a blank `.npmrc` value like
/// `https-proxy=` reliably *unsets* the field instead of installing an
/// unparseable empty URL into the reqwest builder. Trimming matches
/// npm's own line handling.
pub(super) fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Normalize the scheme-specific proxy settings. pnpm treats the
/// config-parser boolean/null spellings as unset instead of attempting
/// to parse them as proxy hostnames.
pub(super) fn proxy_url(s: String) -> Option<String> {
    non_empty(s).filter(|value| !matches!(value.as_str(), "false" | "null"))
}

pub(super) fn pem_value(s: String) -> String {
    s.replace("\\n", "\n")
}
