//! P5 — golden matrix over the `nub.jsonc` schema/validator. Complements the
//! per-field tests in the sibling `tests` module (unknown root/install keys,
//! type errors, duration grammar, loader vocabulary): this matrix owns the
//! `$schema` typing rule, unknown-key rejection at every REMAINING object level
//! (`dlx`, `install.sandbox` axes, `dlx.sandbox` axes), wrapper-type errors at
//! the nested positions, and one full-surface golden shape.

use super::*;

#[test]
fn golden_full_surface_config_parses_with_all_three_sandbox_positions() {
    let cfg = parse_project_config(
        r#"{
          "$schema": "https://nubjs.com/schema/nub.json",
          "nodeCompat": false,
          "sandbox": { "fs": false, "net": ["registry.npmjs.org"], "vars": true },
          "install": {
            "nodeLinker": "isolated",
            "sandbox": "./install-policy.json"
          },
          "dlx": {
            "consent": "never",
            "sandbox": "publish-jail",
            "env": false
          }
        }"#,
    )
    .expect("the full-surface golden shape is valid");
    assert_eq!(cfg.node_compat, Some(false));
    assert!(
        matches!(cfg.sandbox, Some(SandboxSetting::Granular(_))),
        "top-level sandbox: {:?}",
        cfg.sandbox
    );
    assert_eq!(
        cfg.install.sandbox,
        Some(SandboxSetting::FileRef("./install-policy.json".into()))
    );
    assert_eq!(
        cfg.dlx.sandbox,
        Some(SandboxSetting::Preset("publish-jail".into()))
    );
    assert_eq!(cfg.dlx.env, Some(EnvSetting::Disabled));
}

#[test]
fn schema_field_must_be_a_string() {
    // The one blessed non-field key is still TYPED: the published schema
    // declares `$schema: string`, and fail-loud validation applies to it like
    // any other field.
    for raw in ["42", "true", "[]", "{}"] {
        let err = parse_project_config(&format!(r#"{{ "$schema": {raw} }}"#))
            .expect_err(&format!("non-string $schema {raw} must fail loud"));
        match err {
            ConfigError::Type { path, expected } => {
                assert_eq!(path, "$schema", "for $schema value {raw}");
                assert_eq!(expected, "a string", "for $schema value {raw}");
            }
            other => panic!("$schema {raw}: expected Type error, got {other:?}"),
        }
    }
}

#[test]
fn schema_key_is_blessed_at_the_root_only() {
    for (text, path) in [
        (r#"{ "install": { "$schema": "x" } }"#, "install"),
        (r#"{ "dlx": { "$schema": "x" } }"#, "dlx"),
        (r#"{ "sandbox": { "$schema": "x" } }"#, "sandbox"),
    ] {
        let err = parse_project_config(text).expect_err("nested $schema must fail loud");
        match err {
            ConfigError::UnknownKey {
                path: got_path,
                key,
            } => {
                assert_eq!(got_path, path);
                assert_eq!(key, "$schema");
            }
            other => panic!("{text}: expected UnknownKey, got {other:?}"),
        }
    }
}

#[test]
fn unknown_keys_fail_loud_at_every_nested_object_level() {
    // Root, `install`, and top-level `sandbox` axes are covered by the sibling
    // tests module; these are the remaining object levels.
    for (text, path, key) in [
        (r#"{ "dlx": { "consnt": "never" } }"#, "dlx", "consnt"),
        (
            r#"{ "install": { "sandbox": { "disk": true } } }"#,
            "install.sandbox",
            "disk",
        ),
        (
            r#"{ "dlx": { "sandbox": { "network": [] } } }"#,
            "dlx.sandbox",
            "network",
        ),
    ] {
        let err = parse_project_config(text).expect_err(&format!("{text} must fail loud"));
        match err {
            ConfigError::UnknownKey {
                path: got_path,
                key: got_key,
            } => {
                assert_eq!(got_path, path, "{text}");
                assert_eq!(got_key, key, "{text}");
            }
            other => panic!("{text}: expected UnknownKey, got {other:?}"),
        }
    }
}

#[test]
fn wrong_wrapper_types_report_the_nested_path() {
    for (text, path, expected) in [
        (r#"{ "dlx": [] }"#, "dlx", "an object"),
        (
            r#"{ "install": { "sandbox": 5 } }"#,
            "install.sandbox",
            "a boolean, string (preset or \"./file.json\"), or object",
        ),
        (
            r#"{ "dlx": { "sandbox": 5 } }"#,
            "dlx.sandbox",
            "a boolean, string (preset or \"./file.json\"), or object",
        ),
        (
            // A string is now a valid axis shape (`vars: "*"`), so use a number to
            // exercise the axis-value type error.
            r#"{ "dlx": { "sandbox": { "fs": 5 } } }"#,
            "dlx.sandbox.fs",
            "a boolean, string, array, or object",
        ),
    ] {
        let err = parse_project_config(text).expect_err(&format!("{text} must fail loud"));
        match err {
            ConfigError::Type {
                path: got_path,
                expected: got_expected,
            } => {
                assert_eq!(got_path, path, "{text}");
                assert_eq!(got_expected, expected, "{text}");
            }
            other => panic!("{text}: expected Type error, got {other:?}"),
        }
    }
}
