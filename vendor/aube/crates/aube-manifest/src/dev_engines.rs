//! `devEngines` field (OpenJS package-metadata interoperability spec).
//!
//! Shape per the spec (and pnpm 10.14+/11):
//!
//! ```json
//! {
//!   "devEngines": {
//!     "runtime": { "name": "node", "version": "^24.4.0", "onFail": "download" }
//!   }
//! }
//! ```
//!
//! Every slot (`runtime`, `packageManager`, `os`, `cpu`, `libc`) accepts a
//! single object or an array of objects. aube only acts on `runtime`
//! entries named `node`; everything else is preserved untouched in
//! `extra` so format round-trips don't drop fields.
//!
//! Parsing is tolerant in the same spirit as `engines_tolerant`: this
//! field appears in arbitrary published packages, and a malformed
//! `devEngines` in some transitive dep must not fail an install. A
//! shape we don't understand deserializes to "absent".

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

/// Parsed `devEngines` map. Only `runtime` is interpreted; other keys
/// ride along in `extra`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DevEngines {
    /// `devEngines.runtime` entries, normalized to a list (the spec's
    /// single-object form parses as a one-element list and serializes
    /// back as a single object).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "one_or_many_tolerant",
        serialize_with = "one_or_many_serialize"
    )]
    pub runtime: Vec<DevEngineDependency>,
    /// `devEngines.packageManager` entries — same object-or-array
    /// tolerance as `runtime`. aube acts on the entry named `aube`
    /// (self-version switching); other package managers are validated
    /// territory for the startup guard.
    #[serde(
        default,
        rename = "packageManager",
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "one_or_many_tolerant",
        serialize_with = "one_or_many_serialize"
    )]
    pub package_manager: Vec<DevEngineDependency>,
    /// Unrecognized `devEngines` slots (`os`, `cpu`, `libc`, future
    /// additions) — preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl DevEngines {
    /// The `runtime` entry named `node`, if any. The spec allows one
    /// entry per runtime name; if a manifest repeats `node`, the first
    /// entry wins (matching pnpm).
    pub fn node_runtime(&self) -> Option<&DevEngineDependency> {
        self.runtime.iter().find(|r| r.name == "node")
    }

    /// The `packageManager` entry naming this tool itself, if any.
    pub fn aube_package_manager(&self) -> Option<&DevEngineDependency> {
        let self_names = aube_util::embedder().self_names;
        self.package_manager
            .iter()
            .find(|r| self_names.contains(&r.name.as_str()))
    }

    /// Names of `runtime` entries aube does not act on (anything but
    /// `node`), for a caller that wants to surface a warning.
    pub fn unsupported_runtimes(&self) -> Vec<&str> {
        self.runtime
            .iter()
            .filter(|r| r.name != "node")
            .map(|r| r.name.as_str())
            .collect()
    }
}

/// A single `devEngines.<slot>` dependency entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevEngineDependency {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, rename = "onFail", skip_serializing_if = "Option::is_none")]
    pub on_fail: Option<OnFail>,
    /// Forward-compat passthrough for fields the spec may grow.
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// `onFail` policy when the active environment doesn't satisfy the
/// declared engine. Spec default (entry without `onFail`) is `error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnFail {
    Ignore,
    Warn,
    Error,
    Download,
}

impl std::str::FromStr for OnFail {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ignore" => Ok(Self::Ignore),
            "warn" => Ok(Self::Warn),
            "error" => Ok(Self::Error),
            "download" => Ok(Self::Download),
            other => Err(format!(
                "invalid onFail value {other:?} (expected ignore|warn|error|download)"
            )),
        }
    }
}

impl std::fmt::Display for OnFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Ignore => "ignore",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Download => "download",
        })
    }
}

/// Deserialize the whole `devEngines` field tolerantly: a non-object
/// (or otherwise unintelligible) value parses as absent rather than
/// failing the manifest, mirroring `engines_tolerant`.
pub fn dev_engines_tolerant<'de, D>(de: D) -> Result<Option<DevEngines>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(de)?;
    Ok(match value {
        Some(v @ serde_json::Value::Object(_)) => {
            serde_json::from_value::<DevEngines>(v).ok().filter(|d| {
                // An object that parsed but carries nothing we (or a
                // round-trip) care about is equivalent to absent.
                !d.runtime.is_empty() || !d.package_manager.is_empty() || !d.extra.is_empty()
            })
        }
        _ => None,
    })
}

/// Accept the spec's single-object or array form for an entry list.
/// Malformed entries inside an array are dropped (one junk entry in a
/// published package must not fail the install); a malformed scalar
/// parses as the empty list.
fn one_or_many_tolerant<'de, D>(de: D) -> Result<Vec<DevEngineDependency>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(de)?;
    Ok(match value {
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect(),
        Some(v @ serde_json::Value::Object(_)) => {
            serde_json::from_value(v).ok().into_iter().collect()
        }
        _ => Vec::new(),
    })
}

/// Serialize a one-element list back as the single-object form the
/// manifest most likely used, and longer lists as arrays.
fn one_or_many_serialize<S>(entries: &[DevEngineDependency], ser: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match entries {
        [single] => single.serialize(ser),
        many => many.serialize(ser),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Option<DevEngines> {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default, deserialize_with = "dev_engines_tolerant")]
            dev_engines: Option<DevEngines>,
        }
        serde_json::from_str::<Wrap>(&format!(r#"{{"dev_engines": {json}}}"#))
            .unwrap()
            .dev_engines
    }

    #[test]
    fn object_form_parses() {
        let d =
            parse(r#"{"runtime": {"name": "node", "version": "^24.4.0", "onFail": "download"}}"#)
                .unwrap();
        let node = d.node_runtime().unwrap();
        assert_eq!(node.version.as_deref(), Some("^24.4.0"));
        assert_eq!(node.on_fail, Some(OnFail::Download));
    }

    #[test]
    fn array_form_parses_and_first_node_wins() {
        let d = parse(
            r#"{"runtime": [
                {"name": "bun", "version": "^1.2.0"},
                {"name": "node", "version": ">=22"},
                {"name": "node", "version": ">=99"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(d.node_runtime().unwrap().version.as_deref(), Some(">=22"));
        assert_eq!(d.unsupported_runtimes(), vec!["bun"]);
    }

    #[test]
    fn missing_on_fail_is_none() {
        let d = parse(r#"{"runtime": {"name": "node", "version": "^22"}}"#).unwrap();
        assert_eq!(d.node_runtime().unwrap().on_fail, None);
    }

    #[test]
    fn junk_shapes_parse_as_absent() {
        assert!(parse(r#""totally a string""#).is_none());
        assert!(parse("42").is_none());
        assert!(parse("[]").is_none());
        assert!(parse("null").is_none());
        assert!(parse("{}").is_none());
    }

    #[test]
    fn junk_entry_in_array_is_dropped() {
        let d = parse(r#"{"runtime": ["nonsense", {"name": "node", "version": "^20"}]}"#).unwrap();
        assert_eq!(d.runtime.len(), 1);
        assert_eq!(d.node_runtime().unwrap().version.as_deref(), Some("^20"));
    }

    #[test]
    fn invalid_on_fail_drops_the_entry() {
        // Unknown onFail values fail that entry's parse; tolerance
        // drops it rather than failing the manifest. A typo'd policy
        // should not be silently coerced into some default here —
        // the entry vanishing means the field has no effect, the
        // same outcome npm gives unknown garbage.
        let d = parse(r#"{"runtime": [{"name": "node", "version": "^20", "onFail": "explode"}]}"#);
        assert!(d.is_none());
    }

    #[test]
    fn package_manager_slot_is_typed() {
        let d = parse(
            r#"{"runtime": {"name": "node"}, "packageManager": {"name": "pnpm", "version": "^10"}}"#,
        )
        .unwrap();
        assert!(d.aube_package_manager().is_none());
        assert_eq!(d.package_manager[0].name, "pnpm");
        let d = parse(r#"{"packageManager": {"name": "aube", "version": "1.18.2"}}"#).unwrap();
        assert_eq!(
            d.aube_package_manager().unwrap().version.as_deref(),
            Some("1.18.2")
        );
    }

    #[test]
    fn unknown_slots_are_preserved() {
        let d = parse(r#"{"runtime": {"name": "node"}, "os": {"name": "linux"}}"#).unwrap();
        assert!(d.extra.contains_key("os"));
    }

    #[test]
    fn single_entry_round_trips_as_object() {
        let d = parse(r#"{"runtime": {"name": "node", "version": "^24"}}"#).unwrap();
        let back = serde_json::to_value(&d).unwrap();
        assert!(back["runtime"].is_object(), "got {back}");
    }

    #[test]
    fn multi_entry_round_trips_as_array() {
        let d = parse(r#"{"runtime": [{"name": "node"}, {"name": "deno"}]}"#).unwrap();
        let back = serde_json::to_value(&d).unwrap();
        assert!(back["runtime"].is_array(), "got {back}");
    }

    #[test]
    fn extra_entry_fields_are_preserved() {
        let d = parse(r#"{"runtime": {"name": "node", "version": "^24", "futureField": true}}"#)
            .unwrap();
        assert!(d.node_runtime().unwrap().extra.contains_key("futureField"));
    }
}
