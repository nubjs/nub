//! Property tests that pin down the tolerant-shape behavior of the
//! packument deserializers in `aube-registry`.
//!
//! These deserializers all accept JSON that doesn't match the strict
//! "expected" shape (object instead of string, array instead of map,
//! null, etc.) and degrade to a safe default rather than failing the
//! whole packument parse. That tolerance is load-bearing — a strict
//! parse would block install of any range that even *lists* an
//! affected version. Each test fixes the expected behavior against a
//! reference implementation that walks `serde_json::Value` directly,
//! so any Visitor-based rewrite can prove parity over a wide input
//! space.
//!
//! The reference implementations are intentionally written
//! independently of the production deserializers — they encode the
//! *spec*, not the impl. Keep them in sync with intentional behavior
//! changes; never edit one to match a regressed production output.

use aube_registry::{Packument, VersionMetadata};
use proptest::prelude::*;
use serde_json::Value;
use std::collections::BTreeMap;

// === Reference (oracle) impls — walk `Value`, encode the spec. ===

fn ref_non_string_tolerant_map(v: &Value) -> BTreeMap<String, String> {
    match v {
        Value::Object(m) => m
            .iter()
            .filter_map(|(k, v)| match v {
                Value::String(s) => Some((k.clone(), s.clone())),
                _ => None,
            })
            .collect(),
        // Null degrades to empty; any other top-level shape is a hard
        // parse error and never reaches this oracle (we only generate
        // null/object inputs at the field level — see arb_obj_or_null).
        _ => BTreeMap::new(),
    }
}

fn ref_deprecated_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn ref_license_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(m) => m
            .get("type")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from),
        _ => None,
    }
}

fn ref_funding_url(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(m) => m
            .get("url")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from),
        Value::Array(arr) => arr.iter().find_map(|v| match v {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Object(m) => m
                .get("url")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from),
            _ => None,
        }),
        _ => None,
    }
}

fn ref_bin_map(v: &Value) -> BTreeMap<String, String> {
    match v {
        Value::Null => BTreeMap::new(),
        Value::String(s) if s.is_empty() => BTreeMap::new(),
        Value::String(s) => {
            let mut m = BTreeMap::new();
            m.insert(String::new(), s.clone());
            m
        }
        Value::Object(m) => m
            .iter()
            .filter_map(|(k, v)| match v {
                Value::String(s) => Some((k.clone(), s.clone())),
                _ => None,
            })
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// Production `npm_user_tolerant` produces `Some(NpmUser { trusted_publisher })`
/// when the input is a JSON object, `None` otherwise. We can't compare
/// `NpmUser` directly (no `PartialEq`), so we pluck out the only field
/// the trust-policy check actually reads.
fn ref_npm_user_trusted_publisher(v: &Value) -> Option<Option<Value>> {
    match v {
        Value::Object(m) => Some(match m.get("trustedPublisher") {
            None | Some(Value::Null) => None,
            Some(other) => Some(other.clone()),
        }),
        _ => None,
    }
}

// === Generators ===

fn arb_json_leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        // f64 NaN/Inf don't round-trip through JSON; restrict to finite.
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(Value::from),
        ".*".prop_map(Value::from),
    ]
}

/// Arbitrary JSON value, bounded recursion depth.
fn arb_json() -> impl Strategy<Value = Value> {
    arb_json_leaf().prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(".*", inner, 0..4)
                .prop_map(|m| { Value::Object(m.into_iter().collect()) }),
        ]
    })
}

/// Arbitrary `null | object<string, json>` — the only shapes the
/// `non_string_tolerant_map` deserializer accepts at the field level.
fn arb_obj_or_null() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        prop::collection::btree_map(".*", arb_json(), 0..6)
            .prop_map(|m| Value::Object(m.into_iter().collect())),
    ]
}

// === Plumbing: wrap an arbitrary value into a minimal Packument /
// VersionMetadata JSON and parse it through the production path. ===

fn parse_packument_with(field: &str, value: &Value) -> Packument {
    let mut m = serde_json::Map::new();
    m.insert("name".into(), Value::String("pkg".into()));
    m.insert(field.into(), value.clone());
    serde_json::from_value(Value::Object(m)).expect("packument parses")
}

fn parse_version_with(field: &str, value: &Value) -> VersionMetadata {
    let mut m = serde_json::Map::new();
    m.insert("name".into(), Value::String("x".into()));
    m.insert("version".into(), Value::String("1.0.0".into()));
    m.insert(field.into(), value.clone());
    serde_json::from_value(Value::Object(m)).expect("version parses")
}

// === Properties ===

proptest! {
    // 1024 cases per property — the default 256 is fine for parity but
    // the visitor rewrite is invasive enough to warrant heavier
    // sampling. Total runtime stays under a second on a warm cache.
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    #[test]
    fn dist_tags_filters_to_string_entries(v in arb_obj_or_null()) {
        let parsed = parse_packument_with("dist-tags", &v);
        prop_assert_eq!(parsed.dist_tags, ref_non_string_tolerant_map(&v));
    }

    #[test]
    fn time_filters_to_string_entries(v in arb_obj_or_null()) {
        let parsed = parse_packument_with("time", &v);
        prop_assert_eq!(parsed.time, ref_non_string_tolerant_map(&v));
    }

    #[test]
    fn dependencies_filter_to_string_entries(v in arb_obj_or_null()) {
        let parsed = parse_version_with("dependencies", &v);
        prop_assert_eq!(parsed.dependencies, ref_non_string_tolerant_map(&v));
    }

    #[test]
    fn dev_dependencies_filter_to_string_entries(v in arb_obj_or_null()) {
        let parsed = parse_version_with("devDependencies", &v);
        prop_assert_eq!(parsed.dev_dependencies, ref_non_string_tolerant_map(&v));
    }

    #[test]
    fn peer_dependencies_filter_to_string_entries(v in arb_obj_or_null()) {
        let parsed = parse_version_with("peerDependencies", &v);
        prop_assert_eq!(parsed.peer_dependencies, ref_non_string_tolerant_map(&v));
    }

    #[test]
    fn optional_dependencies_filter_to_string_entries(v in arb_obj_or_null()) {
        let parsed = parse_version_with("optionalDependencies", &v);
        prop_assert_eq!(parsed.optional_dependencies, ref_non_string_tolerant_map(&v));
    }

    #[test]
    fn deprecated_matches_reference(v in arb_json()) {
        let parsed = parse_version_with("deprecated", &v);
        prop_assert_eq!(parsed.deprecated, ref_deprecated_string(&v));
    }

    #[test]
    fn license_matches_reference(v in arb_json()) {
        let parsed = parse_version_with("license", &v);
        prop_assert_eq!(parsed.license, ref_license_string(&v));
    }

    #[test]
    fn funding_url_matches_reference(v in arb_json()) {
        let parsed = parse_version_with("funding", &v);
        prop_assert_eq!(parsed.funding_url, ref_funding_url(&v));
    }

    #[test]
    fn bin_matches_reference(v in arb_json()) {
        let parsed = parse_version_with("bin", &v);
        prop_assert_eq!(parsed.bin, ref_bin_map(&v));
    }

    #[test]
    fn npm_user_trusted_publisher_matches_reference(v in arb_json()) {
        let parsed = parse_version_with("_npmUser", &v);
        let expected = ref_npm_user_trusted_publisher(&v);
        let actual = parsed.npm_user.as_ref().map(|u| u.trusted_publisher.clone());
        prop_assert_eq!(actual, expected);
    }
}
