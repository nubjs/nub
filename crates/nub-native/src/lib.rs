//! Nub N-API addon: data-format parsers + the in-process TS/JSX transpiler,
//! exposed to the JS preload.
//!
//! The parser functions take a source string and return a parsed value as a JS
//! object (via napi's serde-json bridge). The [`transform`](transform::transform)
//! function transpiles TS/JSX, mirroring `oxc-transform@0.132.0`'s `transformSync`
//! for byte-for-byte emit parity.

// `collapsible_if` fires on intentional nested `if let { if let }` sites;
// collapsing every site is cosmetic churn, so allow it.
#![allow(clippy::collapsible_if)]

mod cache;
mod detect;
mod resolve;
mod transform;
mod tsconfig;

use std::collections::HashMap;

use napi_derive::napi;

// The pinned JSONC dialect and the nesting bound both live in `nub_tsconfig`: the
// tsconfig reader and this addon's `.jsonc`/`.json5` data importers must accept exactly
// the same dialect and the same depth, so there is ONE definition rather than two that
// can drift. See that crate for why each value is what it is.
use nub_tsconfig::{MAX_NESTING_DEPTH, jsonc_parse_options};

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
    check_yaml_alias_expansion(&source)?;
    let docs = YamlLoader::load_from_str(&source).map_err(yaml_error)?;

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

/// Bound `source`'s nesting, reporting a violation as `format`'s parse error so a
/// hostile document reads to JS like any other malformed one.
fn check_depth(source: &str, format: &str) -> napi::Result<()> {
    nub_json_guard::check_nesting_depth(source, MAX_NESTING_DEPTH)
        .map_err(|e| napi::Error::from_reason(format!("{format} parse error: {e}")))
}

/// Every YAML rejection — scanner, preflight, or loader — reaches JS under one
/// prefix, so a document stopped by a bound reads like any other malformed one.
fn yaml_error(message: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(format!("YAML parse error: {message}"))
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
        let Some(token) = scanner.next_token().map_err(yaml_error)? else {
            return Ok(());
        };

        match token.1 {
            TokenType::BlockSequenceStart
            | TokenType::BlockMappingStart
            | TokenType::FlowSequenceStart
            | TokenType::FlowMappingStart => {
                depth += 1;
                if depth > MAX_NESTING_DEPTH {
                    return Err(yaml_error(format!(
                        "nesting is deeper than the {MAX_NESTING_DEPTH}-level limit"
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

/// Node budget for the collections yaml-rust2 will clone. Only collection
/// aliases are charged against it — a scalar alias cannot amplify a tree — which
/// admits flat reuse of one anchor across many keys while still catching a
/// chain of anchors that each double the previous one.
const MAX_YAML_ALIAS_MATERIALIZATION: usize = 100_000;

#[derive(Clone, Copy)]
struct YamlAnchorSize {
    nodes: usize,
    collection: bool,
}

struct YamlCollectionFrame {
    nodes: usize,
    anchor: Option<String>,
}

/// Bound alias expansion before `YamlLoader` allocates it.
///
/// yaml-rust2 clones the referenced collection at every alias, so a document
/// whose anchors each double the previous one costs allocation exponential in
/// its own length. Estimating that from scanner tokens keeps the preflight
/// iterative and ahead of the loader; a malformed document remains the loader's
/// error to report, not this scan's.
fn check_yaml_alias_expansion(source: &str) -> napi::Result<()> {
    use yaml_rust2::scanner::{Scanner, TokenType};

    fn finish_node(
        nodes: usize,
        collection: bool,
        anchor: Option<String>,
        frames: &mut [YamlCollectionFrame],
        anchors: &mut HashMap<String, YamlAnchorSize>,
    ) {
        if let Some(anchor) = anchor {
            anchors.insert(anchor, YamlAnchorSize { nodes, collection });
        }
        if let Some(parent) = frames.last_mut() {
            parent.nodes = parent.nodes.saturating_add(nodes);
        }
    }

    let mut scanner = Scanner::new(source.chars());
    let mut anchors = HashMap::new();
    let mut frames = Vec::new();
    let mut pending_anchor = None;
    let mut materialized_nodes = 0_usize;

    loop {
        let Some(token) = scanner.next_token().map_err(yaml_error)? else {
            return Ok(());
        };

        match token.1 {
            TokenType::DocumentStart => {
                // YAML anchors are document-local. A malformed document may
                // leave a frame open; let YamlLoader report that syntax error.
                anchors.clear();
                frames.clear();
                pending_anchor = None;
            }
            TokenType::Anchor(name) => pending_anchor = Some(name),
            TokenType::BlockSequenceStart
            | TokenType::BlockMappingStart
            | TokenType::FlowSequenceStart
            | TokenType::FlowMappingStart => frames.push(YamlCollectionFrame {
                nodes: 1,
                anchor: pending_anchor.take(),
            }),
            TokenType::BlockEnd | TokenType::FlowSequenceEnd | TokenType::FlowMappingEnd => {
                if let Some(frame) = frames.pop() {
                    finish_node(frame.nodes, true, frame.anchor, &mut frames, &mut anchors);
                }
            }
            TokenType::Alias(name) => {
                let size = anchors.get(&name).copied().unwrap_or(YamlAnchorSize {
                    nodes: 1,
                    collection: false,
                });
                if size.collection {
                    materialized_nodes = materialized_nodes.saturating_add(size.nodes);
                    if materialized_nodes > MAX_YAML_ALIAS_MATERIALIZATION {
                        return Err(yaml_error(format!(
                            "alias expansion would materialize more than the {MAX_YAML_ALIAS_MATERIALIZATION}-node limit"
                        )));
                    }
                }
                // The parser rejects an anchor applied to an alias; retaining it
                // here only makes this conservative preflight complete the node
                // accounting before the loader returns that syntax error.
                finish_node(
                    size.nodes,
                    false,
                    pending_anchor.take(),
                    &mut frames,
                    &mut anchors,
                );
            }
            TokenType::Scalar(..) => {
                finish_node(1, false, pending_anchor.take(), &mut frames, &mut anchors)
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

fn jsonc_error(message: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(format!("JSONC parse error: {message}"))
}

/// Parse JSONC (JSON with comments) source into a JS value.
#[napi]
pub fn parse_jsonc(source: String) -> napi::Result<serde_json::Value> {
    check_depth(&source, "JSONC")?;
    // Deserializing into `Option<T>` maps both an empty document and a literal
    // `null` to `None`, and the two must not share a fate: the data loader turns
    // a parsed null into an undefined default export, matching its JavaScript
    // fallback, while an absent document stays a parse error. The AST reports
    // presence, not value, so it separates them.
    let parsed = jsonc_parser::parse_to_serde_value::<Option<serde_json::Value>>(
        &source,
        &jsonc_parse_options(),
    )
    .map_err(jsonc_error)?;
    if let Some(value) = parsed {
        return Ok(value);
    }
    jsonc_parser::parse_to_value(&source, &jsonc_parse_options())
        .map_err(jsonc_error)?
        .map(|_| serde_json::Value::Null)
        .ok_or_else(|| napi::Error::from_reason("JSONC: empty document"))
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
