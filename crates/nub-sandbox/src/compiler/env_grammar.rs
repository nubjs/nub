//! The closed env value-type grammar (Rust-parsed, NOT ArkType). Per sandbox.mdx
//! `format`:
//!
//!   value  := "string" | FORMAT | /regex/ | enum:a|b|c   (enum = literal union)
//!   FORMAT := integer | number | port                     (trimmed 2026-07-08)
//!
//! No comparison/intersection operators. String formats (email/url/…) deliberately
//! do NOT ship — `/regex/` covers them. Unrecognized → hard error naming the set
//! (closed vocab, so re-adding later is non-breaking).
//!
//! FUTURE (deferred, not implemented): `minLength`/`maxLength` value-length
//! constraints on the extras object. Left out deliberately (re-add alongside the
//! string formats if demand appears; non-breaking by the closed-vocab discipline).
//! Tracked in wiki/sandbox-config.md §5.

use crate::policy::EnvFormat;

/// A parsed env value type from the string grammar.
#[derive(Debug, Clone, PartialEq)]
pub enum EnvType {
    /// The `"string"` catch-all — any value validates.
    AnyString,
    /// One of the closed FORMAT keywords.
    Format(EnvFormat),
    /// A `/regex/` pattern (syntax-checked while compiling the config).
    Regex(String),
    /// An `enum:a|b|c` literal union — the value must be one of these exact strings.
    Union(Vec<String>),
}

/// Parse a type string from the env grammar. Errors name the supported set.
pub fn parse_env_type(spec: &str) -> Result<EnvType, String> {
    let s = spec.trim();
    // `/regex/` — a leading and trailing slash.
    if let Some(inner) = s.strip_prefix('/').and_then(|r| r.strip_suffix('/')) {
        if inner.is_empty() {
            return Err("empty /regex/ in env type".to_string());
        }
        regex::Regex::new(inner).map_err(|e| format!("invalid regex `{inner}`: {e}"))?;
        return Ok(EnvType::Regex(inner.to_string()));
    }
    // Literal union: `enum:a|b|c` (unquoted members joined by `|`).
    if let Some(list) = s.strip_prefix("enum:") {
        let members = parse_enum_list(list)?;
        return Ok(EnvType::Union(members));
    }
    match s {
        "string" => Ok(EnvType::AnyString),
        "integer" => Ok(EnvType::Format(EnvFormat::Integer)),
        "number" => Ok(EnvType::Format(EnvFormat::Number)),
        "port" => Ok(EnvType::Format(EnvFormat::Port)),
        other => Err(format!(
            "unknown env type `{other}` — supported: string, integer, number, port, /regex/, or an enum:a|b|c list"
        )),
    }
}

/// Parse an `enum:a|b|c` list body (the text after the `enum:` prefix) into its member
/// strings — unquoted, `|`-separated, each trimmed. An empty member (`enum:`, `enum:a||b`)
/// is an error.
fn parse_enum_list(list: &str) -> Result<Vec<String>, String> {
    let mut members = Vec::new();
    for part in list.split('|') {
        let p = part.trim();
        if p.is_empty() {
            return Err("empty value in an enum:a|b|c list".to_string());
        }
        members.push(p.to_string());
    }
    Ok(members)
}

impl EnvType {
    /// Return the [`EnvFormat`] this type carries, for the IR's `schema`. Regex /
    /// union / any-string have no closed format.
    pub fn format(&self) -> Option<EnvFormat> {
        match self {
            EnvType::Format(f) => Some(*f),
            _ => None,
        }
    }

    /// Validate `value` against this type; render any error message with `display` in
    /// place of the raw value. A `secrets` entry passes `"<redacted>"` so a failed-
    /// validation error never surfaces the secret; a non-sensitive `vars` entry passes
    /// the value itself (seeing the bad `PORT` is the useful, common case). `display` is
    /// cosmetic — the verdict is always computed against the real `value`.
    pub fn validate_display(&self, value: &str, display: &str) -> Result<(), String> {
        match self {
            EnvType::AnyString => Ok(()),
            EnvType::Format(EnvFormat::Integer) => value
                .parse::<i64>()
                .map(|_| ())
                .map_err(|_| format!("`{display}` is not an integer")),
            EnvType::Format(EnvFormat::Number) => match value.parse::<f64>() {
                // Reject `inf`/`nan` — a config value typed `number` means a finite
                // numeric string, not an IEEE special.
                Ok(n) if n.is_finite() => Ok(()),
                _ => Err(format!("`{display}` is not a finite number")),
            },
            EnvType::Format(EnvFormat::Port) => match value.parse::<u32>() {
                Ok(n) if (1..=65535).contains(&n) => Ok(()),
                _ => Err(format!("`{display}` is not a valid port (1–65535)")),
            },
            EnvType::Regex(pat) => {
                let re =
                    regex::Regex::new(pat).map_err(|e| format!("invalid regex `{pat}`: {e}"))?;
                if re.is_match(value) {
                    Ok(())
                } else {
                    Err(format!("`{display}` does not match /{pat}/"))
                }
            }
            EnvType::Union(members) => {
                if members.iter().any(|m| m == value) {
                    Ok(())
                } else {
                    Err(format!("`{display}` is not one of {members:?}"))
                }
            }
        }
    }
}
