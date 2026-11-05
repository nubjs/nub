//! The value a data document parses to, and the JavaScript that recreates it.
//!
//! JSON cannot be the interchange type. It has no NaN, no ±Infinity, no -0, no
//! integer past 2^53, no date, and a `{"__proto__": …}` literal sets a prototype
//! instead of a key — every one of which a YAML or TOML document can hold. So the
//! parsers produce this tree and both surfaces emit it as a JavaScript expression:
//! the runtime evaluates it as a module's default export, and `nub compile`
//! inlines the same text into the bundle.

use std::fmt::Write as _;

/// Largest integer a JavaScript number holds exactly (`Number.MAX_SAFE_INTEGER`).
pub const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    /// An integer a JavaScript number cannot hold exactly. Only TOML produces
    /// one: its spec requires 64-bit integers be kept losslessly.
    BigInt(i64),
    String(String),
    /// An ECMAScript date-time string, emitted as `new Date("…")`. A string with
    /// no offset is a local time to `Date`, which is what a TOML local date-time
    /// means.
    Date(String),
    Array(Vec<Value>),
    /// Entries in document order. A YAML key may be any node, so keys are
    /// values: a number or boolean is emitted as a computed key, whose
    /// `String()` conversion is the one `yaml` applies, and a collection as a
    /// string holding its flow text, which is how `yaml` keys one.
    Object(Vec<(Value, Value)>),
}

impl Value {
    /// This value as a JavaScript expression.
    pub fn to_js(&self) -> String {
        let mut out = String::new();
        self.write_js(&mut out);
        out
    }

    /// The expression a data module's default export is. A document that parses
    /// to null exports `undefined`, on every surface.
    pub fn to_default_export(&self) -> String {
        match self {
            Value::Null => "undefined".to_string(),
            value => value.to_js(),
        }
    }

    fn write_js(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => write_number(*n, out),
            Value::BigInt(n) => {
                let _ = write!(out, "{n}n");
            }
            Value::String(s) => write_string(s, out),
            Value::Date(s) => {
                out.push_str("new Date(");
                write_string(s, out);
                out.push(')');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_js(out);
                }
                out.push(']');
            }
            Value::Object(entries) => {
                out.push('{');
                for (i, (key, value)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    match key {
                        // A literal `"__proto__":` key sets the prototype; only
                        // the computed form defines an own property.
                        Value::String(s) if s == "__proto__" => out.push_str("[\"__proto__\"]"),
                        Value::String(s) => write_string(s, out),
                        // `yaml` keys a null to "", where String(null) is "null".
                        Value::Null => out.push_str("\"\""),
                        Value::Number(_) | Value::Bool(_) => {
                            out.push('[');
                            key.write_js(out);
                            out.push(']');
                        }
                        // A collection, which only YAML has. Never computed:
                        // coercing an object key runs its `toString`, which can
                        // collide or throw.
                        key => write_string(&crate::yaml_flow_key(key), out),
                    }
                    out.push(':');
                    value.write_js(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_number(n: f64, out: &mut String) {
    if n.is_nan() {
        out.push_str("NaN");
    } else if n.is_infinite() {
        out.push_str(if n > 0.0 { "Infinity" } else { "-Infinity" });
    } else if n == 0.0 && n.is_sign_negative() {
        out.push_str("-0");
    } else {
        // Rust's shortest round-trip form (`1.5`, `1e21`, `100.0`) is a valid
        // JavaScript literal for the same double.
        let _ = write!(out, "{n:?}");
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// For the serde-driven parsers (JSON5, JSONC). Implemented by hand rather than
/// borrowed from `serde_json::Value`, whose numbers cannot hold JSON5's `NaN`
/// and `Infinity`.
impl<'de> serde::Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Value;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a data value")
            }
            fn visit_unit<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_none<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
                serde::Deserialize::deserialize(d)
            }
            fn visit_bool<E>(self, b: bool) -> Result<Value, E> {
                Ok(Value::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<Value, E> {
                Ok(Value::Number(n as f64))
            }
            fn visit_u64<E>(self, n: u64) -> Result<Value, E> {
                Ok(Value::Number(n as f64))
            }
            fn visit_f64<E>(self, n: f64) -> Result<Value, E> {
                Ok(Value::Number(n))
            }
            fn visit_str<E>(self, s: &str) -> Result<Value, E> {
                Ok(Value::String(s.to_owned()))
            }
            fn visit_string<E>(self, s: String) -> Result<Value, E> {
                Ok(Value::String(s))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Value, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Value::Array(items))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Value, A::Error> {
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    entries.push((Value::String(key), value));
                }
                Ok(Value::Object(entries))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(entries: Vec<(&str, Value)>) -> Value {
        Value::Object(
            entries
                .into_iter()
                .map(|(k, v)| (Value::String(k.into()), v))
                .collect(),
        )
    }

    /// Each value JSON would lose or misread has its own JavaScript spelling.
    #[test]
    fn emits_the_values_json_cannot_carry() {
        let v = obj(vec![
            ("nan", Value::Number(f64::NAN)),
            ("inf", Value::Number(f64::NEG_INFINITY)),
            ("zero", Value::Number(-0.0)),
            ("big", Value::BigInt(9007199254740993)),
            ("when", Value::Date("1979-05-27T07:32:00.000Z".into())),
            ("__proto__", obj(vec![("x", Value::Number(1.0))])),
            ("s", Value::String("a\"\\\n\u{1}é".into())),
        ]);
        assert_eq!(
            v.to_js(),
            r#"{"nan":NaN,"inf":-Infinity,"zero":-0,"big":9007199254740993n,"when":new Date("1979-05-27T07:32:00.000Z"),["__proto__"]:{"x":1.0},"s":"a\"\\\n\u0001é"}"#
        );
    }

    #[test]
    fn non_string_keys_are_computed_and_a_null_key_is_empty() {
        let v = Value::Object(vec![
            (Value::Number(f64::NAN), Value::Bool(true)),
            (Value::Null, Value::Bool(false)),
        ]);
        assert_eq!(v.to_js(), r#"{[NaN]:true,"":false}"#);
    }

    #[test]
    fn a_null_document_exports_undefined() {
        assert_eq!(Value::Null.to_default_export(), "undefined");
        assert_eq!(
            Value::Array(vec![Value::Null]).to_default_export(),
            "[null]"
        );
    }
}
