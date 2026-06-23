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

pub(super) fn pem_value(s: String) -> String {
    s.replace("\\n", "\n")
}
