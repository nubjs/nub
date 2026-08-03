use std::collections::BTreeMap;

/// Extract the scope from a package name (e.g., "@myorg/pkg" -> "@myorg").
pub(super) fn package_scope(name: &str) -> Option<&str> {
    if name.starts_with('@') {
        name.find('/').map(|idx| &name[..idx])
    } else {
        None
    }
}

/// Convert a registry URL to the URI key used in `.npmrc` for auth lookup.
/// `https://registry.example.com/` becomes `//registry.example.com/`.
///
/// The key is a credential-free nerf-dart: userinfo, queries, and fragments
/// are never part of it. The path is always slash-terminated. An explicitly
/// configured port remains part of the key: without a scheme, normalizing it
/// away would conflate distinct Yarn and npm credential selectors.

/// Parse a registry base accepted for HTTP request routing.
///
/// Request URLs and credential lookup keys have intentionally different
/// representations. This parser accepts only explicit HTTP(S) authorities;
/// its result retains valid userinfo so reqwest can issue the configured Basic
/// authorization header. A one-slash `https:/…` spelling is never silently
/// promoted to a trusted base.
fn parse_routable_registry_url(url: &str) -> Option<reqwest::Url> {
    let url = url.trim();
    let authority = strip_prefix_ignore_ascii_case(url, "https://")
        .or_else(|| strip_prefix_ignore_ascii_case(url, "http://"))?;
    // The URL parser deliberately repairs `https:////host`; registry bases
    // require exactly the two authority slashes and must not do so.
    if authority.is_empty() || matches!(authority.as_bytes().first(), Some(b'/' | b'?' | b'#')) {
        return None;
    }

    let parsed = reqwest::Url::parse(url).ok()?;
    matches!(parsed.scheme(), "https" | "http")
        .then_some(parsed)
        .filter(|parsed| parsed.host_str().is_some())
}

/// Serialize an HTTP(S) registry base for request routing.
///
/// Query and fragment components are not a base path and must not be appended
/// to package routes. Unlike [`registry_uri_key`], this preserves valid
/// authority userinfo: reqwest derives Basic authorization from it.
fn normalize_routable_registry_url(mut url: reqwest::Url) -> String {
    url.set_query(None);
    url.set_fragment(None);
    if !url.path().ends_with('/') {
        let mut path = url.path().to_string();
        path.push('/');
        url.set_path(&path);
    }
    url.to_string()
}

/// Convert a validated request URL into its credential-free nerf-dart key.
fn registry_uri_key_from_routable_url(url: &reqwest::Url) -> Option<String> {
    let mut authority = url.host()?.to_string();
    if let Some(port) = url.port() {
        authority.push(':');
        authority.push_str(&port.to_string());
    }
    Some(normalized_uri_key(&authority, url.path()))
}

/// Convert a valid registry URL to its credential-free nerf-dart key.
///
/// Invalid registry bases have no key. Callers must propagate that absence or
/// use a safe fallback; an empty key is never a credential target.
pub(super) fn registry_uri_key(url: &str) -> Option<String> {
    registry_uri_key_from_routable_url(&parse_routable_registry_url(url)?)
}

/// Normalize an `//host[:port]/path...` key from `.npmrc` for auth lookup.
///
/// Ingest is scheme-agnostic: it cannot safely infer whether `:80` or `:443`
/// is default for the configured endpoint, so explicit ports remain part of
/// the credential selector. It removes malformed userinfo, queries, and
/// fragments before the key can enter the auth map.
pub(super) fn normalize_npmrc_uri_key(key: &str) -> String {
    let Some(rest) = key.strip_prefix("//") else {
        return key.to_string();
    };
    let (authority, path) = normalized_authority_and_path(rest);
    let mut key = normalized_uri_key(authority, path);
    lowercase_uri_key_host(&mut key);
    key
}

/// Lowercase only the hostname in an already-normalized `//host[:port]/path/`
/// key. Ports and paths are intentionally byte-for-byte specific.
///
/// An unbracketed authority with multiple colons is malformed, so leave it
/// unchanged rather than guessing where a hostname ends.
fn lowercase_uri_key_host(key: &mut str) {
    let host_end = {
        let authority = &key[2..];
        let authority_end = authority.find('/').unwrap_or(authority.len());
        match authority.as_bytes().first() {
            Some(b'[') => authority
                .find(']')
                .filter(|&close| close < authority_end)
                .map(|close| close + 1),
            _ => match authority[..authority_end].split_once(':') {
                None => Some(authority_end),
                Some((host, port)) if !port.contains(':') => Some(host.len()),
                Some(_) => None,
            },
        }
    };

    if let Some(host_end) = host_end.filter(|&end| end > 0) {
        key[2..2 + host_end].make_ascii_lowercase();
    }
}

/// Extract a credential-free authority and query/fragment-free path.
///
/// The authority ends at the first `/`, `?`, or `#`. Splitting userinfo at
/// the final `@` keeps only the host/port even when malformed raw userinfo
/// contains multiple `@` characters.
fn normalized_authority_and_path(rest: &str) -> (&str, &str) {
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host_and_port)| host_and_port);
    let suffix = &rest[authority_end..];
    let path_end = suffix.find(['?', '#']).unwrap_or(suffix.len());
    let path = if suffix.starts_with('/') {
        &suffix[..path_end]
    } else {
        ""
    };
    (authority, path)
}

/// Build a credential-free `.npmrc` URI key with a slash-terminated path.
fn normalized_uri_key(authority: &str, path: &str) -> String {
    let needs_trailing_slash = path.is_empty() || !path.ends_with('/');
    let mut key =
        String::with_capacity(2 + authority.len() + path.len() + usize::from(needs_trailing_slash));
    key.push_str("//");
    key.push_str(authority);
    if path.is_empty() {
        key.push('/');
    } else {
        key.push_str(path);
        if needs_trailing_slash {
            key.push('/');
        }
    }
    key
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

/// Public wrapper for [`normalize_registry_url`].
pub fn normalize_registry_url_pub(url: &str) -> Option<String> {
    normalize_registry_url(url)
}

/// Public wrapper for [`registry_uri_key`], so callers outside the crate can
/// convert a valid full registry URL into the `//host[:port]/path/` key
/// `.npmrc` uses for per-registry auth entries.
pub fn registry_uri_key_pub(url: &str) -> Option<String> {
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
/// Normalize a valid registry value for request routing.
///
/// Valid absolute HTTP(S) URLs are parsed and serialized before use. That
/// preserves authority userinfo for reqwest Basic auth while dropping query
/// and fragment components that cannot be part of a package route. Credential
/// key normalization is intentionally separate in [`registry_uri_key`].
pub(super) fn normalize_registry_url(url: &str) -> Option<String> {
    parse_routable_registry_url(url).map(normalize_routable_registry_url)
}
