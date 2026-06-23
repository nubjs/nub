use std::collections::BTreeMap;

/// Extract the scope from a package name (e.g., "@myorg/pkg" -> "@myorg").
pub(super) fn package_scope(name: &str) -> Option<&str> {
    if name.starts_with('@') {
        name.find('/').map(|idx| &name[..idx])
    } else {
        None
    }
}

/// Convert a registry URL to the URI key used in .npmrc for auth lookup.
/// "https://registry.example.com/" -> "//registry.example.com/"
///
/// Strips *only the scheme's own default port* (`:443` for https, `:80`
/// for http) so `https://host:443/x/` collapses to the same key as
/// `https://host/x/`, matching npm's nerf-dart behavior. The unusual
/// case of `https://host:80/` (https on the http default port) is
/// deliberately *not* collapsed — that's a different server.
pub(super) fn registry_uri_key(url: &str) -> String {
    let (rest, default_port) = if let Some(rest) = url.strip_prefix("https:") {
        (rest, ":443")
    } else if let Some(rest) = url.strip_prefix("http:") {
        (rest, ":80")
    } else {
        return url.to_string();
    };
    strip_authority_port_suffix(rest, default_port)
}

/// Normalize an `//host[:port]/path...` key from `.npmrc` so it matches
/// what `registry_uri_key` produces on the lookup side.
///
/// Ingest can't know the scheme the user intended (`.npmrc` keys are
/// scheme-less), so we strip both `:443` and `:80` — in practice
/// nobody writes either explicitly unless they meant the default for
/// the corresponding scheme. The lookup side is stricter: it only
/// strips the matching default, so an `//host:80/x/` key will still
/// not authenticate an `https://host:80/x/` request, and vice versa.
pub(super) fn normalize_npmrc_uri_key(key: &str) -> String {
    let stripped = strip_authority_port_suffix(key, ":443");
    if stripped != key {
        return stripped;
    }
    strip_authority_port_suffix(key, ":80")
}

/// Strip a trailing `:N` from the authority of an `//host[:N]/path...`
/// key. Returns the key unchanged when the prefix isn't `//` or the
/// authority doesn't end with the requested port suffix.
fn strip_authority_port_suffix(key: &str, port_suffix: &str) -> String {
    let Some(after) = key.strip_prefix("//") else {
        return key.to_string();
    };
    let (authority, path) = match after.find('/') {
        Some(idx) => (&after[..idx], &after[idx..]),
        None => (after, ""),
    };
    let Some(authority) = authority.strip_suffix(port_suffix) else {
        return key.to_string();
    };
    format!("//{authority}{path}")
}

/// Look up `key` in `map`, falling back to longest-prefix matching by
/// trimming path segments from the right. Mirrors npm/pnpm's auth
/// resolution: a tarball at `//host/a/b/c-1.0.0.tgz` finds an auth
/// entry registered at `//host/a/`, while `//other/` does not match a
/// `//host/` entry. Stops before falling all the way to the bare `//`
/// host-less prefix.
pub(crate) fn lookup_by_uri_prefix<'a, V>(
    map: &'a BTreeMap<String, V>,
    key: &str,
) -> Option<&'a V> {
    if let Some(v) = map.get(key) {
        return Some(v);
    }
    let trimmed = key.trim_end_matches('/');
    if !trimmed.is_empty()
        && trimmed != key
        && let Some(v) = map.get(trimmed)
    {
        return Some(v);
    }
    let mut cursor = trimmed;
    while let Some(idx) = cursor.rfind('/') {
        cursor = &cursor[..idx];
        // Stop at or before the leading "//" — anything that short is a
        // host-less prefix that could match arbitrary registries.
        if cursor.len() <= 2 {
            break;
        }
        let with_slash = format!("{cursor}/");
        if let Some(v) = map.get(&with_slash) {
            return Some(v);
        }
        if let Some(v) = map.get(cursor) {
            return Some(v);
        }
    }
    None
}

/// Public wrapper for normalize_registry_url.
pub fn normalize_registry_url_pub(url: &str) -> String {
    normalize_registry_url(url)
}

/// Public wrapper for [`registry_uri_key`], so callers outside the
/// crate can convert a full registry URL into the `//host[:port]/path/`
/// key `.npmrc` uses for per-registry auth entries without reimplementing
/// the scheme-stripping logic.
pub fn registry_uri_key_pub(url: &str) -> String {
    registry_uri_key(url)
}

/// True when `url` points at `registry.npmjs.org` (the public npm
/// registry). Lowercased + trailing-slash-tolerant so different
/// equivalent spellings (`https://Registry.NPMJS.org/`, no slash,
/// scheme-relative `//registry.npmjs.org/`) all resolve the same way.
/// Scheme matching is case-insensitive per RFC 3986; `https`/`http`
/// pass and anything else (mirrors, replays, transports we don't
/// speak) is by definition not the public registry.
pub(super) fn is_public_npmjs_url(url: &str) -> bool {
    let url = url.trim();
    let after_scheme = strip_prefix_ignore_ascii_case(url, "https://")
        .or_else(|| strip_prefix_ignore_ascii_case(url, "http://"))
        .or_else(|| url.strip_prefix("//"))
        .unwrap_or(url);
    // No scheme stripped AND no scheme-relative `//` prefix means a
    // bare authority like `registry.npmjs.org/`. We accept that, but
    // reject anything whose prefix *looks* like a scheme we didn't
    // recognise (`ftp:`, `file:`) — `unwrap_or(url)` would otherwise
    // happily split `ftp://registry.npmjs.org/` on `/` and walk away
    // believing the host matched.
    if after_scheme == url && url.contains("://") {
        return false;
    }
    let host = after_scheme
        .split_once('/')
        .map(|(h, _)| h)
        .unwrap_or(after_scheme);
    let host = host.split_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = host.split_once(':').map(|(h, _)| h).unwrap_or(host);
    host.eq_ignore_ascii_case("registry.npmjs.org")
}

/// Strip a literal ASCII prefix from `s` ignoring case, returning the
/// remainder. Matches the semantics of [`str::strip_prefix`] but
/// folds case before comparing — used by [`is_public_npmjs_url`]
/// so a user-supplied `.npmrc` entry like `HTTPS://...` doesn't
/// fall through and accidentally disable the supply-chain gates.
fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    // `split_at_checked` returns `None` rather than panicking when the
    // byte offset isn't a UTF-8 char boundary, so a malformed
    // `.npmrc` value like `https:/ñ...` (multi-byte char straddling
    // the prefix length) gracefully fails the prefix match instead
    // of crashing `aube add`.
    let (head, tail) = s.split_at_checked(prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then_some(tail)
}

/// Ensure registry URL has a trailing slash.
pub(super) fn normalize_registry_url(url: &str) -> String {
    let url = url.trim();
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}
