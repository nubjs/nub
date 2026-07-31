//! Nub N-API addon: data-format parsers + the in-process TS/JSX transpiler,
//! exposed to the JS preload.
//!
//! The parser functions take a source string and return a parsed value as a JS
//! object (via napi's serde-json bridge). The [`transform`](transform::transform)
//! function transpiles TS/JSX, mirroring `oxc-transform@0.132.0`'s `transformSync`
//! for byte-for-byte emit parity.

// `collapsible_if` fires on intentional nested `if let { if let }` sites;
// collapsing every site is cosmetic churn (and tsconfig.rs is a verbatim
// get-tsconfig mirror), so allow it.
#![allow(clippy::collapsible_if)]

mod cache;
mod detect;
mod resolve;
mod transform;
mod tsconfig;

use jsonc_parser::ParseOptions;
use napi_derive::napi;

/// Pin JSONC acceptance to the pre-0.32 dialect.  New parser versions default
/// new extensions on, but tsconfig and data imports must not silently become
/// more permissive when this dependency moves.
pub(crate) fn jsonc_parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: true,
        allow_trailing_commas: true,
        allow_missing_commas: false,
        allow_single_quoted_strings: true,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

pub use cache::transform_cached;
pub use detect::detect_module_info;
pub use resolve::resolve_ts;
pub use transform::transform;
pub use tsconfig::load_tsconfig;

/// Parse YAML source into a JS value.
#[napi]
pub fn parse_yaml(source: String) -> napi::Result<serde_json::Value> {
    use yaml_rust2::YamlLoader;

    check_yaml_depth(&source)?;
    let docs = YamlLoader::load_from_str(&source)
        .map_err(|e| napi::Error::from_reason(format!("YAML parse error: {e}")))?;

    let doc = docs.into_iter().next().unwrap_or(yaml_rust2::Yaml::Null);
    Ok(yaml_to_json(&doc))
}

/// Parse TOML source into a JS value.
#[napi]
pub fn parse_toml(source: String) -> napi::Result<serde_json::Value> {
    let value: toml::Value = source
        .parse()
        .map_err(|e| napi::Error::from_reason(format!("TOML parse error: {e}")))?;

    serde_json::to_value(value)
        .map_err(|e| napi::Error::from_reason(format!("TOML→JSON conversion error: {e}")))
}

/// Deepest `{`/`[` nesting the data-format loaders accept.
///
/// Higher than the CLI's bound on its own config files, because these loaders
/// take arbitrary user data — a `.json5` fixture is not a `nub.jsonc` — but far
/// enough below the measured abort thresholds (json5 dies between 2500 and 3000
/// levels on an 8 MiB stack and between 300 and 400 on Windows' 1 MiB) that the
/// margin holds on the smallest stack nub runs on.
pub(crate) const MAX_NESTING_DEPTH: usize = 128;

/// Bound `source`'s nesting, reporting a violation as `format`'s parse error so a
/// hostile document reads to JS like any other malformed one.
fn check_depth(source: &str, format: &str) -> napi::Result<()> {
    nub_json_guard::check_nesting_depth(source, MAX_NESTING_DEPTH)
        .map_err(|e| napi::Error::from_reason(format!("{format} parse error: {e}")))
}

/// Bound YAML collections before `YamlLoader` constructs a recursive tree.
///
/// YAML's block collections use indentation rather than JSON's `{` / `[` syntax,
/// so the JSON-family text guard cannot see them. Its public scanner emits every
/// block and flow collection boundary iteratively, without building the `Yaml`
/// tree whose parsing, conversion, and drop can consume the Node stack.
fn check_yaml_depth(source: &str) -> napi::Result<()> {
    use yaml_rust2::scanner::{Scanner, TokenType};

    let mut scanner = Scanner::new(source.chars());
    let mut depth = 0_usize;
    loop {
        let Some(token) = scanner
            .next_token()
            .map_err(|e| napi::Error::from_reason(format!("YAML parse error: {e}")))?
        else {
            return Ok(());
        };

        match token.1 {
            TokenType::BlockSequenceStart
            | TokenType::BlockMappingStart
            | TokenType::FlowSequenceStart
            | TokenType::FlowMappingStart => {
                depth += 1;
                if depth > MAX_NESTING_DEPTH {
                    return Err(napi::Error::from_reason(format!(
                        "YAML parse error: nesting is deeper than the {MAX_NESTING_DEPTH}-level limit"
                    )));
                }
            }
            TokenType::BlockEnd | TokenType::FlowSequenceEnd | TokenType::FlowMappingEnd => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
}

/// Parse JSON5 source into a JS value.
#[napi]
pub fn parse_json5(source: String) -> napi::Result<serde_json::Value> {
    check_depth(&source, "JSON5")?;
    json5::from_str(&source)
        .map_err(|e| napi::Error::from_reason(format!("JSON5 parse error: {e}")))
}

/// Parse JSONC (JSON with comments) source into a JS value.
#[napi]
pub fn parse_jsonc(source: String) -> napi::Result<serde_json::Value> {
    check_depth(&source, "JSONC")?;
    // `Option<T>` maps both an empty document and literal `null` to `None`.
    // The data loader deliberately turns a parsed null into an undefined default
    // export, matching its JavaScript fallback, while an absent document remains
    // a parse error.
    let parsed = jsonc_parser::parse_to_serde_value::<Option<serde_json::Value>>(
        &source,
        &jsonc_parse_options(),
    )
    .map_err(|e| napi::Error::from_reason(format!("JSONC parse error: {e}")))?;
    if let Some(value) = parsed {
        return Ok(value);
    }
    let present = jsonc_parser::parse_to_value(&source, &jsonc_parse_options())
        .map_err(|e| napi::Error::from_reason(format!("JSONC parse error: {e}")))?
        .is_some();
    if present {
        Ok(serde_json::Value::Null)
    } else {
        Err(napi::Error::from_reason(
            "JSONC: empty document".to_string(),
        ))
    }
}

fn yaml_to_json(yaml: &yaml_rust2::Yaml) -> serde_json::Value {
    match yaml {
        yaml_rust2::Yaml::Real(s) => {
            if let Ok(n) = s.parse::<f64>() {
                serde_json::Value::Number(
                    serde_json::Number::from_f64(n).unwrap_or_else(|| serde_json::Number::from(0)),
                )
            } else {
                serde_json::Value::String(s.clone())
            }
        }
        yaml_rust2::Yaml::Integer(n) => serde_json::json!(*n),
        yaml_rust2::Yaml::String(s) => serde_json::Value::String(s.clone()),
        yaml_rust2::Yaml::Boolean(b) => serde_json::Value::Bool(*b),
        yaml_rust2::Yaml::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(yaml_to_json).collect())
        }
        yaml_rust2::Yaml::Hash(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| {
                    let key = match k {
                        yaml_rust2::Yaml::String(s) => s.clone(),
                        yaml_rust2::Yaml::Integer(n) => n.to_string(),
                        yaml_rust2::Yaml::Boolean(b) => b.to_string(),
                        _ => format!("{k:?}"),
                    };
                    (key, yaml_to_json(v))
                })
                .collect();
            serde_json::Value::Object(obj)
        }
        yaml_rust2::Yaml::Null | yaml_rust2::Yaml::BadValue | yaml_rust2::Yaml::Alias(_) => {
            serde_json::Value::Null
        }
    }
}
