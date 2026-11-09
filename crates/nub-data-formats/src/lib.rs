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

mod value;

use std::collections::{HashMap, HashSet};

use jsonc_parser::ParseOptions;
use nub_json_guard::Dialect;

pub use value::{MAX_SAFE_INTEGER, Value};

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
fn check_depth(source: &str, dialect: Dialect) -> Result<(), String> {
    let format = match dialect {
        Dialect::Jsonc => "JSONC",
        Dialect::Json5 => "JSON5",
    };
    nub_json_guard::check_nesting_depth(source, MAX_NESTING_DEPTH, dialect)
        .map_err(|e| format!("{format} parse error: {e}"))
}

/// Every YAML rejection — scanner, preflight, or loader — reaches the caller
/// under one prefix, so a document stopped by a bound reads like any other
/// malformed one.
fn yaml_error(message: impl std::fmt::Display) -> String {
    format!("YAML parse error: {message}")
}

/// Parse YAML source under the YAML 1.2 core schema, the default of `yaml`, the
/// dominant JavaScript parser. Only the first document is read.
pub fn parse_yaml(source: &str) -> Result<Value, String> {
    check_yaml_depth(source)?;
    check_yaml_alias_expansion(source)?;
    let mut builder = YamlBuilder::default();
    yaml_rust2::parser::Parser::new_from_str(source)
        .load(&mut builder, true)
        .map_err(yaml_error)?;
    if let Some(e) = builder.error {
        return Err(yaml_error(e));
    }
    Ok(builder.first.unwrap_or(Value::Null))
}

/// Parse TOML source. Integers outside the exact range of a JavaScript number
/// become `BigInt`, since the TOML spec requires 64-bit integers be kept
/// losslessly; an offset or local date-time becomes a `Date`; a local time, which
/// `Date` cannot represent, stays a string.
pub fn parse_toml(source: &str) -> Result<Value, String> {
    let root = toml::de::DeTable::parse(source).map_err(|e| format!("TOML parse error: {e}"))?;
    toml_table(root.get_ref(), source)
}

/// Parse JSON5 source.
pub fn parse_json5(source: &str) -> Result<Value, String> {
    check_depth(source, Dialect::Json5)?;
    json5::from_str(source).map_err(|e| format!("JSON5 parse error: {e}"))
}

fn jsonc_error(message: impl std::fmt::Display) -> String {
    format!("JSONC parse error: {message}")
}

/// Parse JSONC (JSON with comments) source.
pub fn parse_jsonc(source: &str) -> Result<Value, String> {
    check_depth(source, Dialect::Jsonc)?;
    // Deserializing into `Option<T>` maps both an empty document and a literal
    // `null` to `None`, and the two must not share a fate: the data loader turns
    // a parsed null into an undefined default export, matching its JavaScript
    // fallback, while an absent document stays a parse error. The AST reports
    // presence, not value, so it separates them.
    let parsed =
        jsonc_parser::parse_to_serde_value::<Option<Value>>(source, &jsonc_parse_options())
            .map_err(jsonc_error)?;
    if let Some(value) = parsed {
        return Ok(value);
    }
    jsonc_parser::parse_to_value(source, &jsonc_parse_options())
        .map_err(jsonc_error)?
        .map(|_| Value::Null)
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

/// Builds the first document as a [`Value`], resolving each scalar itself.
///
/// `YamlLoader` cannot be used: it resolves plain scalars with Rust's float
/// parser, so `nan` and `inf` become numbers while `.NaN` and `True` stay
/// strings, and the style that separates `true` from `"true"` is gone by the
/// time its tree exists. The events still carry style and tag.
#[derive(Default)]
struct YamlBuilder {
    first: Option<Value>,
    root: Option<Value>,
    documents: usize,
    stack: Vec<(YamlCollection, usize)>,
    anchors: HashMap<usize, Value>,
    error: Option<yaml_rust2::ScanError>,
}

enum YamlCollection {
    Sequence(Vec<Value>),
    Mapping {
        entries: Vec<(Value, Value)>,
        key: Option<Value>,
        seen: HashSet<ScalarKey>,
    },
}

/// A mapping key as `yaml`'s `uniqueKeys` compares it: a scalar by value, so
/// `1` and `1.0` are one key while `1` and `"1"` are two. NaN equals nothing,
/// and neither does a collection key, which `yaml` compares by node identity.
#[derive(PartialEq, Eq, Hash)]
enum ScalarKey {
    Null,
    Bool(bool),
    Number(u64),
    String(String),
}

impl ScalarKey {
    fn of(key: &Value) -> Option<Self> {
        Some(match key {
            Value::Null => Self::Null,
            Value::Bool(b) => Self::Bool(*b),
            Value::Number(n) if n.is_nan() => return None,
            // `+ 0.0` folds -0 into 0, which `===` also treats as one value.
            Value::Number(n) => Self::Number((n + 0.0).to_bits()),
            Value::String(s) => Self::String(s.clone()),
            _ => return None,
        })
    }
}

impl yaml_rust2::parser::MarkedEventReceiver for YamlBuilder {
    fn on_event(&mut self, event: yaml_rust2::Event, mark: yaml_rust2::scanner::Marker) {
        use yaml_rust2::Event;

        // Later documents are still parsed, so a syntax error anywhere fails
        // the import as it always has, but nothing reads them.
        if self.error.is_some() || self.documents > 0 {
            return;
        }
        let (node, anchor) = match event {
            Event::DocumentEnd => {
                self.documents += 1;
                self.first = Some(self.root.take().unwrap_or(Value::Null));
                return;
            }
            Event::Scalar(text, style, anchor, tag) => (yaml_scalar(text, style, tag), anchor),
            Event::Alias(id) => (self.anchors.get(&id).cloned().unwrap_or(Value::Null), 0),
            Event::SequenceStart(anchor, _) => {
                self.stack
                    .push((YamlCollection::Sequence(Vec::new()), anchor));
                return;
            }
            Event::MappingStart(anchor, _) => {
                let mapping = YamlCollection::Mapping {
                    entries: Vec::new(),
                    key: None,
                    seen: HashSet::new(),
                };
                self.stack.push((mapping, anchor));
                return;
            }
            Event::SequenceEnd | Event::MappingEnd => match self.stack.pop() {
                Some((YamlCollection::Sequence(items), anchor)) => (Value::Array(items), anchor),
                Some((YamlCollection::Mapping { entries, .. }, anchor)) => {
                    (Value::Object(entries), anchor)
                }
                None => return,
            },
            _ => return,
        };
        if anchor > 0 {
            self.anchors.insert(anchor, node.clone());
        }
        match self.stack.last_mut() {
            None => self.root = Some(node),
            Some((YamlCollection::Sequence(items), _)) => items.push(node),
            Some((YamlCollection::Mapping { entries, key, seen }, _)) => match key.take() {
                None => *key = Some(node),
                Some(k) => {
                    // A collection key stays a structure until it is emitted.
                    // Flattening it here would quote and escape it again inside
                    // every enclosing collection key, doubling the text at each
                    // level, and would make it equal to a string that spells
                    // the same flow text.
                    if ScalarKey::of(&k).is_some_and(|id| !seen.insert(id)) {
                        self.error = Some(yaml_rust2::ScanError::new_string(
                            mark,
                            format!("{}: duplicated key in mapping", k.to_js()),
                        ));
                        return;
                    }
                    entries.push((k, node));
                }
            },
        }
    }
}

/// A collection key as flow YAML, the string `yaml` keys it by: `[ a, b ]`,
/// `{ a: 1 }`, `[]`. `yaml` re-emits each scalar in its source style; this
/// tree no longer holds that, so it departs in three places: a string needing
/// quotes is double-quoted (`yaml` keeps `'single'`), a number prints its value
/// (`-0`, not `-0.0`; `31`, not `0x1f`), and a nested collection key stays on
/// one line. Quoting any string that is not plain-safe keeps distinct keys
/// distinct.
pub(crate) fn yaml_flow_key(value: &Value) -> String {
    fn plain_safe(s: &str) -> bool {
        !s.is_empty()
            && core_scalar(s).is_none()
            && s.trim() == s
            && !s.starts_with([
                '-', '?', ':', '!', '&', '*', '|', '>', '%', '@', '`', '\'', '"',
            ])
            && !s.contains(['#', ',', '[', ']', '{', '}', '"', '\'', '\n', '\r', '\t'])
            && !s.contains(": ")
            && !s.ends_with(':')
    }
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::String(s) if plain_safe(s) => out.push_str(s),
            Value::Number(n) if n.is_nan() => out.push_str(".nan"),
            Value::Number(n) if n.is_infinite() => {
                out.push_str(if *n > 0.0 { ".inf" } else { "-.inf" })
            }
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::Array(items) if items.is_empty() => out.push_str("[]"),
            Value::Object(entries) if entries.is_empty() => out.push_str("{}"),
            Value::Array(items) => {
                out.push_str("[ ");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write(item, out);
                }
                out.push_str(" ]");
            }
            Value::Object(entries) => {
                out.push_str("{ ");
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write(k, out);
                    out.push_str(": ");
                    write(v, out);
                }
                out.push_str(" }");
            }
            // Strings needing quotes, null and booleans: their JavaScript
            // literal is also valid double-quoted or plain YAML.
            other => out.push_str(&other.to_js()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// Resolve a scalar the way the YAML 1.2 core schema does: an untagged plain
/// scalar by its text, a quoted or block one as a string, a `!!type` as that type.
fn yaml_scalar(
    text: String,
    style: yaml_rust2::scanner::TScalarStyle,
    tag: Option<yaml_rust2::parser::Tag>,
) -> Value {
    let resolved = match &tag {
        None if style == yaml_rust2::scanner::TScalarStyle::Plain => core_scalar(&text),
        Some(tag) if tag.handle == "tag:yaml.org,2002:" => match tag.suffix.as_str() {
            "null" => core_scalar(&text).filter(|v| *v == Value::Null),
            "bool" => core_scalar(&text).filter(|v| matches!(v, Value::Bool(_))),
            "int" => core_int(&text),
            "float" => core_scalar(&text).filter(|v| matches!(v, Value::Number(_))),
            _ => None,
        },
        _ => None,
    };
    resolved.unwrap_or(Value::String(text))
}

/// The core schema's non-string forms, or `None` for a string.
fn core_scalar(text: &str) -> Option<Value> {
    Some(match text {
        "" | "~" | "null" | "Null" | "NULL" => Value::Null,
        "true" | "True" | "TRUE" => Value::Bool(true),
        "false" | "False" | "FALSE" => Value::Bool(false),
        ".nan" | ".NaN" | ".NAN" => Value::Number(f64::NAN),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => Value::Number(f64::INFINITY),
        "-.inf" | "-.Inf" | "-.INF" => Value::Number(f64::NEG_INFINITY),
        _ if is_core_float(text) => {
            return core_int(text).or_else(|| text.parse().ok().map(Value::Number));
        }
        _ => return core_int(text),
    })
}

/// `[-+]?[0-9]+`, `0o[0-7]+` or `0x[0-9a-fA-F]+`, as a number: past 2^53 it
/// rounds, as `yaml` and every other JavaScript YAML parser does by default.
fn core_int(text: &str) -> Option<Value> {
    let n = if let Some(hex) = text.strip_prefix("0x") {
        radix_to_f64(hex, 16)?
    } else if let Some(octal) = text.strip_prefix("0o") {
        radix_to_f64(octal, 8)?
    } else {
        let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // Correctly rounded, as `Number(text)` is; keeps the sign of `-0`.
        text.parse().ok()?
    };
    Some(Value::Number(n))
}

/// Hex or octal digits as the nearest double, rounded once over the whole
/// integer as `Number(BigInt(text))` is. Digits accumulate exactly in a `u128`;
/// past 128 bits the rest only shift the exponent and set a sticky bit, which is
/// enough for round-to-nearest-even because 124+ bits remain above it.
fn radix_to_f64(digits: &str, radix: u32) -> Option<f64> {
    let width = radix.trailing_zeros();
    let (mut top, mut shift, mut sticky) = (0_u128, 0_i32, false);
    if digits.is_empty() {
        return None;
    }
    for c in digits.chars() {
        let d = c.to_digit(radix)?;
        if top.leading_zeros() >= width {
            top = (top << width) | u128::from(d);
        } else {
            shift = shift.saturating_add(width as i32);
            sticky |= d != 0;
        }
    }
    Some((top | u128::from(sticky)) as f64 * 2_f64.powi(shift))
}

/// `[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?`
fn is_core_float(text: &str) -> bool {
    let b = text.as_bytes();
    let digits = |mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        i
    };
    let mut i = usize::from(matches!(b.first(), Some(b'-' | b'+')));
    let int_end = digits(i);
    let whole = int_end > i;
    i = int_end;
    if b.get(i) == Some(&b'.') {
        let frac_end = digits(i + 1);
        if !whole && frac_end == i + 1 {
            return false;
        }
        i = frac_end;
    } else if !whole {
        return false;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'-' | b'+')) {
            i += 1;
        }
        let exp_end = digits(i);
        if exp_end == i {
            return false;
        }
        i = exp_end;
    }
    i == b.len()
}

fn toml_table(table: &toml::de::DeTable<'_>, source: &str) -> Result<Value, String> {
    let mut entries: Vec<_> = table.iter().collect();
    // `DeTable` is keyed lexicographically; each key's span restores the
    // document order every JavaScript TOML parser keeps.
    entries.sort_by_key(|(key, _)| key.span().start);
    entries
        .into_iter()
        .map(|(key, value)| {
            Ok((
                Value::String(key.get_ref().to_string()),
                toml_value(value, source)?,
            ))
        })
        .collect::<Result<_, String>>()
        .map(Value::Object)
}

/// The checks `toml::Table` would have made while converting: an integer must
/// fit in 64 bits and a float must not overflow to infinity.
fn toml_value(value: &toml::Spanned<toml::de::DeValue<'_>>, source: &str) -> Result<Value, String> {
    use toml::de::DeValue;

    let error = |what: String| {
        let before = &source[..value.span().start.min(source.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        format!("TOML parse error at line {line}, column {column}: {what}")
    };
    Ok(match value.get_ref() {
        DeValue::String(s) => Value::String(s.to_string()),
        DeValue::Integer(int) => {
            let n = i64::from_str_radix(int.as_str(), int.radix())
                .map_err(|_| error(format!("integer {int} does not fit in 64 bits")))?;
            if n.unsigned_abs() <= MAX_SAFE_INTEGER as u64 {
                Value::Number(n as f64)
            } else {
                Value::BigInt(n)
            }
        }
        DeValue::Float(float) => {
            let n: f64 = float
                .as_str()
                .parse()
                .map_err(|_| error(format!("invalid float {float}")))?;
            if n.is_infinite() && !float.as_str().contains("inf") {
                return Err(error(format!("float {float} is out of range")));
            }
            Value::Number(n)
        }
        DeValue::Boolean(b) => Value::Bool(*b),
        DeValue::Datetime(datetime) => toml_datetime(datetime),
        DeValue::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| toml_value(item, source))
                .collect::<Result<_, _>>()?,
        ),
        DeValue::Table(table) => toml_table(table, source)?,
    })
}

/// A TOML date-time as an ECMAScript date-time string, which `Date` reads the
/// way TOML means it: an offset date-time is an instant, a local date-time is
/// local time, and a local date is that day at UTC midnight. Precision past the
/// millisecond is truncated, as the TOML spec requires of an implementation that
/// cannot hold it.
fn toml_datetime(datetime: &toml::value::Datetime) -> Value {
    let time = datetime.time.map(|t| {
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            t.hour,
            t.minute,
            t.second.unwrap_or(0),
            t.nanosecond.unwrap_or(0) / 1_000_000
        )
    });
    let offset = datetime.offset.map(|o| o.to_string()).unwrap_or_default();
    match (datetime.date, time) {
        (Some(date), Some(time)) => Value::Date(format!("{date}T{time}{offset}")),
        (Some(date), None) => Value::Date(date.to_string()),
        (None, Some(time)) => Value::String(time),
        (None, None) => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toml_number_out_of_range_names_its_position() {
        assert_eq!(
            parse_toml("a = 1\nb = [2, 9223372036854775808]\n").unwrap_err(),
            "TOML parse error at line 2, column 9: integer 9223372036854775808 does not fit in 64 bits"
        );
    }

    /// The four formats agree on one shape, so a document that means the same
    /// thing in each emits the same JavaScript — which is the property the
    /// compiler relies on when it inlines one of these into a bundle.
    #[test]
    fn each_format_parses_to_the_same_value() {
        let want = r#"{"k":"v","n":1.0}"#;
        assert_eq!(parse_yaml("k: v\nn: 1\n").unwrap().to_js(), want);
        assert_eq!(parse_toml("k = \"v\"\nn = 1\n").unwrap().to_js(), want);
        assert_eq!(parse_json5("{k:'v',n:1}").unwrap().to_js(), want);
        assert_eq!(
            parse_jsonc("{\"k\":\"v\",/*c*/\"n\":1}").unwrap().to_js(),
            want
        );
    }

    /// YAML 1.2 core schema, as `yaml` resolves it: `.nan`/`.inf` are floats
    /// while plain `nan`/`inf` are strings, the capitalised booleans are
    /// booleans, a quoted scalar is a string whatever it spells, keys resolve
    /// like values, and an integer past 2^53 is a rounded number.
    #[test]
    fn yaml_scalars_resolve_under_the_core_schema() {
        let doc = "big: 9007199254740993\nnan: .nan\ninf: -.inf\nzero: -0.0\n\
                   plain: [nan, inf, True, NULL, 0x1F, 0o17, 1_000, 1., .5e1]\n\
                   quoted: ['true', \"1\"]\ntagged: [!!str 1, !!float 1]\n\
                   .nan: k\n1.5: k\n~: k\n__proto__: {x: 1}\n";
        assert_eq!(
            parse_yaml(doc).unwrap().to_js(),
            concat!(
                r#"{"big":9007199254740992.0,"nan":NaN,"inf":-Infinity,"zero":-0,"#,
                r#""plain":["nan","inf",true,null,31.0,15.0,"1_000",1.0,5.0],"#,
                r#""quoted":["true","1"],"tagged":["1",1.0],"#,
                r#"[NaN]:"k",[1.5]:"k","":"k",["__proto__"]:{"x":1.0}}"#
            )
        );
        assert_eq!(
            parse_yaml("a: &x [1]\nb: *x\n---\nc: 1\n").unwrap().to_js(),
            r#"{"a":[1.0],"b":[1.0]}"#
        );
    }

    /// A collection key becomes `yaml`'s flow text as a plain string key, never
    /// a computed one: JavaScript's coercion would merge every map key into
    /// "[object Object]" and throw on a `toString` key.
    #[test]
    fn yaml_collection_keys_become_their_flow_text() {
        let doc = "? {a: 1}\n: first\n? {b: 2}\n: second\n? {toString: 1}\n: t\n\
                   ? [a, b]\n: seq\n? [[1, 'x,y'], {}]\n: nested\n";
        assert_eq!(
            parse_yaml(doc).unwrap().to_js(),
            concat!(
                r#"{"{ a: 1 }":"first","{ b: 2 }":"second","{ toString: 1 }":"t","#,
                r#""[ a, b ]":"seq","[ [ 1, \"x,y\" ], {} ]":"nested"}"#
            )
        );

        // A collection key inside another is written as structure, once.
        // Quoting it at each level doubled the text per level, and made it equal
        // to a string spelling the same flow text.
        let mut key = String::from("{ a: b }");
        for _ in 0..20 {
            key = format!("{{ ? {key} : c }}");
        }
        let js = parse_yaml(&format!("? {key}\n: v\n")).unwrap().to_js();
        assert!(
            js.len() < 2 * key.len(),
            "{} bytes of output for a {}-byte key",
            js.len(),
            key.len()
        );
        assert_eq!(
            parse_yaml("? { ? [a, b] : c }\n: collection\n? { \"[ a, b ]\": c }\n: string\n")
                .unwrap()
                .to_js(),
            r#"{"{ [ a, b ]: c }":"collection","{ \"[ a, b ]\": c }":"string"}"#
        );
    }

    /// Keys collide as they do in `yaml`: a scalar by value, so `1` and `1.0`
    /// are one key but `1` and `"1"` two; NaN never; and a collection key
    /// never, so a repeated one overwrites the first.
    #[test]
    fn yaml_duplicate_keys_compare_by_value() {
        for doc in [
            "a: 1\na: 2\n",
            "1: a\n1.0: b\n",
            "0: a\n-0: b\n",
            "~: a\nnull: b\n",
        ] {
            let err = parse_yaml(doc).unwrap_err();
            assert!(err.contains("duplicated key"), "{doc:?}: {err}");
        }
        for doc in [
            "1: a\n\"1\": b\n",
            ".nan: a\n.nan: b\n",
            "? [a]\n: 1\n? [a]\n: 2\n",
        ] {
            assert!(parse_yaml(doc).is_ok(), "{doc:?} was rejected");
        }
    }

    /// Hex and octal integers round once, to the nearest double, as
    /// `Number(BigInt(text))` does; per-digit accumulation rounded at every step.
    #[test]
    fn yaml_radix_integers_round_once() {
        let n = |doc: &str| match parse_yaml(doc).unwrap() {
            Value::Object(entries) => entries[0].1.clone(),
            other => other,
        };
        assert_eq!(
            n("x: 0x2000000000000101"),
            Value::Number(2305843009213694464.0)
        );
        // Past u128: 2^160 - 1 rounds up to 2^160.
        assert_eq!(
            n(&format!("x: 0x{}", "f".repeat(40))),
            Value::Number(2_f64.powi(160))
        );
        assert_eq!(
            n(&format!("x: 0o{}", "7".repeat(400))),
            Value::Number(f64::INFINITY)
        );
    }

    /// TOML keeps what its spec keeps: 64-bit integers losslessly (a `BigInt`
    /// past 2^53), `nan`/`inf` as floats, date-times as dates, and the
    /// document's key order.
    #[test]
    fn toml_values_keep_their_type_and_order() {
        let doc = "zeta = 9007199254740993\nalpha = 9007199254740991\nnan = nan\ninf = -inf\n\
                   zero = -0.0\nwhen = 1979-05-27T00:32:00.999999-07:00\nlocal = 1979-05-27 07:32:00\n\
                   day = 1979-05-27\ntime = 07:32:00\n[__proto__]\nx = 1\n";
        assert_eq!(
            parse_toml(doc).unwrap().to_js(),
            concat!(
                r#"{"zeta":9007199254740993n,"alpha":9007199254740991.0,"nan":NaN,"inf":-Infinity,"#,
                r#""zero":-0,"when":new Date("1979-05-27T00:32:00.999-07:00"),"#,
                r#""local":new Date("1979-05-27T07:32:00.000"),"day":new Date("1979-05-27"),"#,
                r#""time":"07:32:00.000",["__proto__"]:{"x":1.0}}"#
            )
        );
    }

    /// JSON5 has `NaN` and `Infinity`, which a JSON value could not hold.
    #[test]
    fn json5_keeps_nan_and_infinity() {
        assert_eq!(
            parse_json5("{a: NaN, b: -Infinity, c: -0.0, __proto__: 1}")
                .unwrap()
                .to_js(),
            r#"{"a":NaN,"b":-Infinity,"c":-0,["__proto__"]:1.0}"#
        );
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
        // The two parsers end a `//` comment at different bytes, and each bound
        // follows its own. JSON5 ends this one at the `\r`, so the nesting after
        // it is code. JSONC keeps it open to the `\n`, so the quote after the
        // `\r` opens no string and the nesting on the next line is code.
        let over = format!("{}1{}", "[".repeat(200), "]".repeat(200));
        for refused in [
            parse_json5(&format!("// c\r{over}")),
            parse_jsonc(&format!("// c\r\"\n{over}")),
        ] {
            let error = refused.unwrap_err();
            assert!(error.contains("deeper than the 128-level limit"), "{error}");
        }

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
        assert_eq!(parse_jsonc("null").unwrap(), Value::Null);
        assert!(parse_jsonc("  \n// only a comment\n").is_err());
    }
}
