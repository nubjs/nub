//! P5 — golden matrix over the `nub.jsonc` schema/validator. Complements the
//! per-field tests in the sibling `tests` module (unknown root/install keys,
//! type errors, duration grammar, loader vocabulary): this matrix owns the
//! `$schema` typing rule, unknown-key rejection at every REMAINING object level
//! (`dlx`), wrapper-type errors at the nested positions (including the inline-object
//! migration error at every sandbox position), and one full-surface golden shape.

use super::*;

#[test]
fn golden_full_surface_config_parses_with_all_three_sandbox_positions() {
    let cfg = parse_project_config(
        r#"{
          "$schema": "https://nubjs.com/schema/nub.json",
          "nodeCompat": false,
          "sandbox": true,
          "install": {
            "nodeLinker": "isolated",
            "buildJail": false
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
    assert_eq!(cfg.sandbox, Some(SandboxSetting::Enabled));
    assert_eq!(cfg.install.build_jail, Some(false));
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
    // `sandbox` is no longer an object level (an inline object is a migration
    // error), so `install` and `dlx` are the nested object levels that carry keys.
    for (text, path) in [
        (r#"{ "install": { "$schema": "x" } }"#, "install"),
        (r#"{ "dlx": { "$schema": "x" } }"#, "dlx"),
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
    // Root and `install` are covered by the sibling tests module; `dlx` is the
    // remaining object level. (A sandbox value is never an object anymore, so its
    // keys are not an unknown-key surface — the inline-object migration error is
    // asserted in `wrong_wrapper_types_report_the_nested_path`.)
    let text = r#"{ "dlx": { "consnt": "never" } }"#;
    match parse_project_config(text).expect_err("dlx unknown key must fail loud") {
        ConfigError::UnknownKey { path, key } => {
            assert_eq!(path, "dlx");
            assert_eq!(key, "consnt");
        }
        other => panic!("{text}: expected UnknownKey, got {other:?}"),
    }
}

#[test]
fn wrong_wrapper_types_report_the_nested_path() {
    for (text, path, expected) in [
        (r#"{ "dlx": [] }"#, "dlx", "an object"),
        (
            r#"{ "install": { "buildJail": 5 } }"#,
            "install.buildJail",
            "a boolean",
        ),
        (
            r#"{ "dlx": { "sandbox": 5 } }"#,
            "dlx.sandbox",
            "a boolean or string (preset or \"./file.json\")",
        ),
        (
            // An inline object at any sandbox position is rejected with the
            // migration error (granular policies belong in a file).
            r#"{ "dlx": { "sandbox": { "fs": 5 } } }"#,
            "dlx.sandbox",
            "a boolean or string — a preset name or a \"./file.jsonc\" \
             reference (inline sandbox objects are not accepted; move the \
             policy into a file)",
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
