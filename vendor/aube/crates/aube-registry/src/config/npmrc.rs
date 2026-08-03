use std::path::Path;

/// Parse a trusted .npmrc file into key=value pairs.
///
/// User/global config may use npm's environment variable substitution
/// (`${VAR}`) for dynamic registry hosts or tokens. Project-controlled
/// files must use [`parse_npmrc_untrusted`] so a cloned repository
/// cannot expand the caller's environment into registry destinations
/// or credentials.
pub(super) fn parse_npmrc(path: &Path) -> Result<Vec<(String, String)>, std::io::Error> {
    parse_npmrc_inner(path, true)
}

/// Parse a repository-controlled .npmrc file without `${VAR}` expansion.
pub(super) fn parse_npmrc_untrusted(path: &Path) -> Result<Vec<(String, String)>, std::io::Error> {
    parse_npmrc_inner(path, false)
}

/// Parse a .npmrc file into key=value pairs.
/// Supports backslash line continuation. npm's `ini` parser treats a
/// trailing `\` as "continue value on next physical line", used for
/// long auth tokens or multi-value arrays. Without this aube would
/// silently truncate the value at the first line break and reparse the
/// continuation as a bogus key.
fn parse_npmrc_inner(
    path: &Path,
    expand_env: bool,
) -> Result<Vec<(String, String)>, std::io::Error> {
    let raw_content = std::fs::read_to_string(path)?;
    let content = raw_content.strip_prefix('\u{feff}').unwrap_or(&raw_content);
    let mut entries = Vec::new();

    // Fold backslash-continuation before line iteration. Trailing
    // `\` plus newline gets joined with the next line verbatim.
    // Same as npm's `ini` semantics.
    let mut logical: Vec<String> = Vec::new();
    let mut acc = String::new();
    for raw in content.lines() {
        if let Some(stripped) = raw.strip_suffix('\\') {
            acc.push_str(stripped);
            continue;
        }
        acc.push_str(raw);
        logical.push(std::mem::take(&mut acc));
    }
    if !acc.is_empty() {
        logical.push(acc);
    }

    for line in &logical {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = canonicalize_auth_authority(maybe_substitute_env(key.trim(), expand_env));
            let value = maybe_substitute_env(strip_matched_quotes(value.trim()), expand_env);
            entries.push((key, value));
        }
    }

    Ok(entries)
}

fn maybe_substitute_env(value: &str, expand_env: bool) -> String {
    if expand_env {
        substitute_env(value)
    } else {
        value.to_string()
    }
}

/// Lowercase only the host portion of a scheme-less URI-scoped auth key.
///
/// `.npmrc` auth keys are nerf-darts (`//host[:port]/path/:_authToken`).
/// Request URL parsing normalizes DNS-host case, so matching requires the
/// equivalent normalization at ingest. Userinfo, the explicit port, and path
/// stay byte-for-byte intact: credentials and paths are not case-insensitive.
fn canonicalize_auth_authority(mut key: String) -> String {
    if !is_uri_scoped_auth_key(&key) || !key.starts_with("//") {
        return key;
    }

    let (host_start, host_end) = {
        let authority_and_rest = &key[2..];
        let authority_end = authority_and_rest
            .find(['/', '?', '#'])
            .unwrap_or(authority_and_rest.len());
        let authority = &authority_and_rest[..authority_end];
        let host_start = 2 + authority.rfind('@').map_or(0, |at| at + 1);
        let host_and_port = &key[host_start..2 + authority_end];
        // Untrusted project files retain `${VAR}` references literally so
        // later validation can reject them; do not rewrite a variable name as
        // though it were a DNS host.
        if host_and_port.contains("${") {
            return key;
        }

        let host_end = if host_and_port.starts_with('[') {
            let Some(close) = host_and_port.find(']') else {
                return key;
            };
            host_start + close + 1
        } else {
            match host_and_port.split_once(':') {
                None => host_start + host_and_port.len(),
                Some((host, port)) => {
                    // An unbracketed authority with multiple colons is
                    // malformed; leave it for normal fail-closed validation.
                    if port.contains(':') {
                        return key;
                    }
                    host_start + host.len()
                }
            }
        };
        (host_start, host_end)
    };

    if host_start < host_end {
        key[host_start..host_end].make_ascii_lowercase();
    }
    key
}

fn is_uri_scoped_auth_key(key: &str) -> bool {
    key.rsplit_once(':').is_some_and(|(_, suffix)| {
        matches!(
            suffix,
            "_authToken"
                | "_auth"
                | "username"
                | "_password"
                | "tokenHelper"
                | "token-helper"
                | "always-auth"
                | "always_auth"
                | "ca"
                | "ca[]"
                | "cafile"
                | "caFile"
                | "cert"
                | "key"
        )
    })
}

/// Strip a single layer of matched surrounding `"` or `'` from `value`.
/// Mirrors npm's `ini` parser, which lets users quote values like
/// `_auth="abc=="` to make the `=` padding survive editors that trim
/// trailing chars. The token contents (including any inner `=` chars)
/// pass through verbatim — only the outer quote pair is removed.
fn strip_matched_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Substitute ${VAR} references with environment variable values.
pub(super) fn substitute_env(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var_name.push(c);
            }
            if let Ok(val) = std::env::var(&var_name) {
                result.push_str(&val);
            }
        } else {
            result.push(c);
        }
    }

    result
}
