//! Presentation layer for engine-adjacent output. Text nub relays from the
//! engine, and the status lines family verbs print beside it, pass through
//! [`rewrite`]. The engine brands its own output through the embedder profile,
//! so what is left here is a credential scrub.

/// Info passthrough for family verbs (stderr, rewritten — stdout is data,
/// progress and status lines go to stderr).
pub(crate) fn info(msg: &str) {
    eprintln!("{}", rewrite(msg));
}

/// Defense-in-depth credential scrub over arbitrary engine prose.
///
/// The engine redacts URLs where it builds its messages, so in correct
/// operation no credential reaches here. This backstop guarantees that a
/// FUTURE un-redacted engine path cannot leak `user:pass@host` userinfo or a
/// `token`/`auth`/`api_key` query value through nub's output: every
/// whitespace-delimited URL-like token (`scheme://…` or scheme-relative
/// `//…`) is redacted, and non-URL text is returned untouched.
pub(crate) fn rewrite(text: &str) -> String {
    // Cheap pre-check: a credential can only ride on a URL token, which
    // always contains "//". Skip the allocation/scan for the common case.
    if !text.contains("//") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    // Walk the text token-by-token, preserving every whitespace separator
    // verbatim so the rendered message's spacing/newlines survive.
    let mut rest = text;
    while !rest.is_empty() {
        let ws_end = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        if ws_end > 0 {
            out.push_str(&rest[..ws_end]);
            rest = &rest[ws_end..];
            continue;
        }
        let tok_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..tok_end];
        if token.contains("//") {
            out.push_str(&redact_url_token(token));
        } else {
            out.push_str(token);
        }
        rest = &rest[tok_end..];
    }
    out
}

/// Redact a single URL-bearing token. The token may carry surrounding
/// punctuation (`(https://…)`, `https://…,`), so peel a leading `(`/`[` run
/// and a trailing run of `)`/`]`/`,`/`.`/`;` before redacting, then re-attach
/// it — [`redact_url`] expects a bare URL.
fn redact_url_token(token: &str) -> String {
    let lead_end = token
        .find(|c: char| c != '(' && c != '[' && c != '<')
        .unwrap_or(token.len());
    let (lead, after_lead) = token.split_at(lead_end);
    let trail_start = after_lead
        .rfind(|c: char| !matches!(c, ')' | ']' | '>' | ',' | '.' | ';' | '"' | '\''))
        .map(|i| i + after_lead[i..].chars().next().map_or(1, char::len_utf8))
        .unwrap_or(0);
    let (core, trail) = after_lead.split_at(trail_start);
    format!("{lead}{}{trail}", redact_url(core))
}

/// `url` with its `user:password@` userinfo replaced by `***@` and the value
/// of each well-known credential query parameter replaced by `***`.
fn redact_url(url: &str) -> String {
    redact_query_tokens(&redact_userinfo(url))
}

/// Handles both `scheme://user:pw@host` and scheme-relative `//user:pw@host`;
/// an `@` past the first `/` is path, not userinfo.
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
    let slash = tail.find('/').unwrap_or(tail.len());
    if at >= slash {
        return url.to_string();
    }
    format!("{}***@{}", &url[..after], &tail[at + 1..])
}

/// Parameter names match case-insensitively; the fragment is kept.
fn redact_query_tokens(url: &str) -> String {
    const SENSITIVE: &[&str] = &["token", "auth", "api_key", "apikey", "access_token"];
    let Some((head, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let (query, fragment) = match query.find('#') {
        Some(hash) => query.split_at(hash),
        None => (query, ""),
    };
    let mut out = String::with_capacity(url.len());
    out.push_str(head);
    out.push('?');
    for (index, pair) in query.split('&').enumerate() {
        if index > 0 {
            out.push('&');
        }
        match pair.split_once('=') {
            Some((key, _)) if SENSITIVE.contains(&key.to_ascii_lowercase().as_str()) => {
                out.push_str(key);
                out.push_str("=***");
            }
            _ => out.push_str(pair),
        }
    }
    out.push_str(fragment);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_redacts_credentials_in_registry_urls() {
        // Backstop: even if an engine path forgets to scrub a URL, no
        // userinfo or query token may reach nub's output. Covers the
        // common prose shapes — bare URL, parenthesized (reqwest's
        // ` for url (…)`), and a trailing-punctuation URL.
        let msg = "HTTP error: error sending request \
                   for url (https://alice:s3cr3t@registry.example.com/foo?token=abc123)";
        let out = rewrite(msg);
        assert!(!out.contains("s3cr3t"), "password leaked: {out}");
        assert!(!out.contains("alice:"), "userinfo leaked: {out}");
        assert!(!out.contains("abc123"), "token leaked: {out}");
        assert!(
            out.contains("registry.example.com"),
            "redacted host must survive: {out}"
        );
    }

    #[test]
    fn rewrite_leaves_credential_free_urls_intact() {
        let url = "https://registry.npmjs.org/lodash";
        assert_eq!(rewrite(url), url, "clean URL must pass through unchanged");
    }
}
