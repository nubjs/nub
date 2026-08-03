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
            let key = maybe_substitute_env(key.trim(), expand_env);
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
