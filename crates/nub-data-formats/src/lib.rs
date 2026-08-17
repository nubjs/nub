//! The data formats Nub's runtime accepts as imports — YAML, TOML, JSON5, JSONC.
//!
//! These live here rather than in the N-API addon because BOTH surfaces need
//! them and they must not disagree. At run time the addon exposes them to the
//! preload; at build time `nub compile` calls them directly to inline a data
//! import into the bundle. A second implementation behind the same syntax would
//! let a document mean one thing when run and another when compiled, which is
//! exactly the class of divergence a compiler must not introduce.
//!
//! Errors are plain strings already carrying their format prefix, so the addon
//! wraps them with `napi::Error::from_reason` and the compiler reports them as
//! a build diagnostic, without either side restating the message.

use std::collections::HashMap;

use jsonc_parser::ParseOptions;

/// Deepest `{`/`[` nesting the data-format loaders accept.
///
/// Higher than the CLI's bound on its own config files, because these loaders
/// take arbitrary user data — a `.json5` fixture is not a `nub.jsonc` — but far
/// enough below the measured abort thresholds (json5 dies between 2500 and 3000
/// levels on an 8 MiB stack and between 300 and 400 on Windows' 1 MiB) that the
/// margin holds on the smallest stack nub runs on.
pub const MAX_NESTING_DEPTH: usize = 128;

/// Node budget for the collections yaml-rust2 will clone. Only collection
/// aliases are charged against it — a scalar alias cannot amplify a tree — which
/// admits flat reuse of one anchor across many keys while still catching a
/// chain of anchors that each double the previous one.
const MAX_YAML_ALIAS_MATERIALIZATION: usize = 100_000;

/// Pin JSONC acceptance to the pre-0.32 dialect. New parser versions default
/// new extensions on, but tsconfig and data imports must not silently become
/// more permissive when this dependency moves.
pub fn jsonc_parse_options() -> ParseOptions {
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

/// Bound `source`'s nesting, reporting a violation as `format`'s parse error so a
/// hostile document reads like any other malformed one.
fn check_depth(source: &str, format: &str) -> Result<(), String> {
    nub_json_guard::check_nesting_depth(source, MAX_NESTING_DEPTH)
        .map_err(|e| format!("{format} parse error: {e}"))
}

/// Every YAML rejection — scanner, preflight, or loader — reaches the caller
/// under one prefix, so a document stopped by a bound reads like any other
/// malformed one.
fn yaml_error(message: impl std::fmt::Display) -> String {
    format!("YAML parse error: {message}")
}

/// Parse YAML source into a JSON value.
pub fn parse_yaml(source: &str) -> Result<serde_json::Value, String> {
    use yaml_rust2::YamlLoader;

    check_yaml_depth(source)?;
    check_yaml_alias_expansion(source)?;
    let docs = YamlLoader::load_from_str(source).map_err(yaml_error)?;

    let doc = docs.into_iter().next().unwrap_or(yaml_rust2::Yaml::Null);
    Ok(yaml_to_json(&doc))
}

/// Parse TOML source into a JSON value.
pub fn parse_toml(source: &str) -> Result<serde_json::Value, String> {
    let value: toml::Value = source
        .parse()
        .map_err(|e| format!("TOML parse error: {e}"))?;

    serde_json::to_value(value).map_err(|e| format!("TOML→JSON conversion error: {e}"))
}

/// Parse JSON5 source into a JSON value.
pub fn parse_json5(source: &str) -> Result<serde_json::Value, String> {
    check_depth(source, "JSON5")?;
    json5::from_str(source).map_err(|e| format!("JSON5 parse error: {e}"))
}

fn jsonc_error(message: impl std::fmt::Display) -> String {
    format!("JSONC parse error: {message}")
}

/// Parse JSONC (JSON with comments) source into a JSON value.
pub fn parse_jsonc(source: &str) -> Result<serde_json::Value, String> {
    check_depth(source, "JSONC")?;
    // Deserializing into `Option<T>` maps both an empty document and a literal
    // `null` to `None`, and the two must not share a fate: the data loader turns
    // a parsed null into an undefined default export, matching its JavaScript
    // fallback, while an absent document stays a parse error. The AST reports
    // presence, not value, so it separates them.
    let parsed = jsonc_parser::parse_to_serde_value::<Option<serde_json::Value>>(
        source,
        &jsonc_parse_options(),
    )
    .map_err(jsonc_error)?;
    if let Some(value) = parsed {
        return Ok(value);
    }
    jsonc_parser::parse_to_value(source, &jsonc_parse_options())
        .map_err(jsonc_error)?
        .map(|_| serde_json::Value::Null)
        .ok_or_else(|| "JSONC parse error: empty document".to_string())
}

/// Bound YAML collections before `YamlLoader` constructs a recursive tree.
///
/// YAML's block collections use indentation rather than JSON's `{` / `[` syntax,
/// so the JSON-family text guard cannot see them. Its public scanner emits every
/// block and flow collection boundary iteratively, without building the `Yaml`
/// tree whose parsing, conversion, and drop can consume the Node stack.
fn check_yaml_depth(source: &str) -> Result<(), String> {
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
fn check_yaml_alias_expansion(source: &str) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The four formats agree on one shape, so a document that means the same
    /// thing in each parses to the same JSON — which is the property the compiler
    /// relies on when it inlines one of these into a bundle.
    #[test]
    fn each_format_parses_to_the_same_value() {
        let want = serde_json::json!({ "k": "v", "n": 1 });
        assert_eq!(parse_yaml("k: v\nn: 1\n").unwrap(), want);
        assert_eq!(parse_toml("k = \"v\"\nn = 1\n").unwrap(), want);
        assert_eq!(parse_json5("{k:'v',n:1}").unwrap(), want);
        assert_eq!(parse_jsonc("{\"k\":\"v\",/*c*/\"n\":1}").unwrap(), want);
    }

    /// Every rejection carries its format's prefix, because the caller reports
    /// the string verbatim and a bare message would not say what failed.
    #[test]
    fn errors_name_their_format() {
        assert!(parse_yaml("\t- [").unwrap_err().starts_with("YAML"));
        assert!(parse_toml("= 1").unwrap_err().starts_with("TOML"));
        assert!(parse_json5("{,}").unwrap_err().starts_with("JSON5"));
        assert!(parse_jsonc("{").unwrap_err().starts_with("JSONC"));
    }

    /// The depth and alias bounds are the reason these are not a thin wrapper
    /// over the parser crates, so they get a test that fails without them.
    #[test]
    fn hostile_documents_are_refused_before_the_parser_allocates() {
        let deep = format!("{}{}", "[".repeat(600), "]".repeat(600));
        assert!(parse_json5(&deep).unwrap_err().contains("JSON5"));
        assert!(parse_jsonc(&deep).unwrap_err().contains("JSONC"));

        let nested = format!(
            "a:\n{}",
            (0..600)
                .map(|i| format!("{}b:\n", " ".repeat(i + 1)))
                .collect::<String>()
        );
        assert!(parse_yaml(&nested).unwrap_err().contains("YAML"));

        // Anchors that each double the previous one: bounded by node budget, not depth.
        let mut bomb = String::from("a: &a [x,x,x,x,x,x,x,x,x,x]\n");
        for i in 0..9 {
            bomb.push_str(&format!(
                "{c}: &{c} [{p},{p},{p},{p},{p},{p},{p},{p},{p},{p}]\n",
                c = (b'b' + i) as char,
                p = format!("*{}", (b'a' + i) as char)
            ));
        }
        assert!(parse_yaml(&bomb).unwrap_err().contains("materialize"));
    }

    /// An empty JSONC document and a literal `null` must not share a fate — the
    /// loader turns a parsed null into an undefined default export, while an
    /// absent document stays an error.
    #[test]
    fn an_empty_jsonc_document_is_not_a_null_one() {
        assert_eq!(parse_jsonc("null").unwrap(), serde_json::Value::Null);
        assert!(parse_jsonc("  \n// only a comment\n").is_err());
    }
}
