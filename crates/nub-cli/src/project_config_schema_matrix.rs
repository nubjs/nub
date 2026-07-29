//! Golden matrix over the `nub.jsonc` schema/validator. Complements the
//! per-field tests in the sibling `tests` module (unknown root/install keys,
//! type errors, duration grammar, loader vocabulary): this matrix owns the
//! `$schema` typing rule, unknown-key rejection in `dlx` (the one object level
//! the sibling module does not reach), wrapper-type errors reported against
//! their nested paths, and one full-surface golden shape.
//!
//! `dlx` is global-only, so every case that reaches into it goes through
//! [`parse_global_config`]; the project parser refuses the section outright.

use super::*;

/// Each row below names the parser for the file that may legally hold its key,
/// so a case keeps testing the reader it actually describes.
type Parse = fn(&str) -> Result<ProjectConfig>;
const PROJECT: Parse = parse_project_config;
const GLOBAL: Parse = parse_global_config;

#[test]
fn golden_full_surface_config_parses() {
    let cfg = parse_global_config(
        r#"{
          "$schema": "https://nubjs.com/schema/latest.json",
          "nodeCompat": false,
          "install": {
            "linker": { "strategy": "isolated", "hoist": false },
            "minimumReleaseAge": "3d"
          },
          "dlx": {
            "consent": "never"
          }
        }"#,
    )
    .expect("the full-surface golden shape is valid");
    assert_eq!(cfg.node_compat, Some(false));
    assert_eq!(
        cfg.install.linker,
        Some(LinkerConfig::Isolated {
            hoist: Some(Hoist::Bool(false))
        })
    );
    assert_eq!(
        cfg.install.minimum_release_age,
        Some(Duration::from_secs(3 * 86_400))
    );
    assert_eq!(cfg.dlx.consent, Some(ImplicitDlx::Never));
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
    for (parse, text, path) in [
        (PROJECT, r#"{ "install": { "$schema": "x" } }"#, "install"),
        (GLOBAL, r#"{ "dlx": { "$schema": "x" } }"#, "dlx"),
    ] {
        let err = parse(text).expect_err("nested $schema must fail loud");
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
fn an_unknown_dlx_key_fails_loud_against_its_own_path() {
    // Root and `install` are covered by the sibling tests module; `dlx` is the
    // only other object level with a flat key set of its own.
    let err = parse_global_config(r#"{ "dlx": { "consnt": "never" } }"#)
        .expect_err("an unknown dlx key must fail loud");
    match err {
        ConfigError::UnknownKey { path, key } => {
            assert_eq!(path, "dlx");
            assert_eq!(key, "consnt");
        }
        other => panic!("expected UnknownKey, got {other:?}"),
    }
}

#[test]
fn wrong_wrapper_types_report_the_nested_path() {
    for (parse, text, path, expected) in [
        (GLOBAL, r#"{ "dlx": [] }"#, "dlx", "an object"),
        (PROJECT, r#"{ "install": [] }"#, "install", "an object"),
        (
            PROJECT,
            r#"{ "install": { "publicHoist": 5 } }"#,
            "install.publicHoist",
            "an array of strings",
        ),
        // `publicHoist` is patterns only. The blanket boolean pnpm spells
        // `shamefully-hoist` is `["*"]` here — pnpm's own internal
        // representation of it — so a bare `true` is a type error, not a
        // second way to say the same thing.
        (
            PROJECT,
            r#"{ "install": { "publicHoist": true } }"#,
            "install.publicHoist",
            "an array of strings",
        ),
    ] {
        let err = parse(text).expect_err(&format!("{text} must fail loud"));
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
