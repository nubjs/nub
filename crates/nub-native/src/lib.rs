//! Nub N-API addon: data-format parsers + the in-process TS/JSX transpiler,
//! exposed to the JS preload.
//!
//! The parser functions take a source string and return a parsed value as a JS
//! object (via napi's serde-json bridge). The [`transform`](transform::transform)
//! function transpiles TS/JSX, mirroring `oxc-transform@0.132.0`'s `transformSync`
//! for byte-for-byte emit parity.

mod detect;
mod transform;

use napi_derive::napi;

pub use detect::detect_module_info;
pub use transform::transform;

/// Parse YAML source into a JS value.
#[napi]
pub fn parse_yaml(source: String) -> napi::Result<serde_json::Value> {
    use yaml_rust2::YamlLoader;

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

/// Parse JSON5 source into a JS value.
#[napi]
pub fn parse_json5(source: String) -> napi::Result<serde_json::Value> {
    json5::from_str(&source)
        .map_err(|e| napi::Error::from_reason(format!("JSON5 parse error: {e}")))
}

/// Parse JSONC (JSON with comments) source into a JS value.
#[napi]
pub fn parse_jsonc(source: String) -> napi::Result<serde_json::Value> {
    jsonc_parser::parse_to_serde_value(&source, &Default::default())
        .map_err(|e| napi::Error::from_reason(format!("JSONC parse error: {e}")))?
        .ok_or_else(|| napi::Error::from_reason("JSONC: empty document".to_string()))
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
        yaml_rust2::Yaml::Null | yaml_rust2::Yaml::BadValue => serde_json::Value::Null,
        yaml_rust2::Yaml::Alias(_) => serde_json::Value::Null,
    }
}
