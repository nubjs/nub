//! P6 — sandbox config values are LOSSLESS through parse and INERT in every
//! Phase-0 consumer projection. One five-shape table (the trichotomy's full
//! value space) is shared across the top-level, `install.sandbox`, and
//! `dlx.sandbox` positions, and reused by pm_engine's install-lowering test.
//! Top-level parse + snapshot retention is covered by the sibling
//! `sandbox_five_shapes_are_lossless_and_inert_in_the_snapshot`; this module
//! owns the nested positions and the projection-equality (inertness) proofs.

use super::*;
use crate::config::ImplicitDlx;

/// The five policy shapes, as (raw JSONC value, expected parse). The granular
/// arm exercises all four axis forms (object / array / string / bool) so
/// losslessness covers the raw-value retention path, not just the wrapper
/// classification.
pub(crate) fn five_shapes() -> Vec<(String, SandboxSetting)> {
    let granular_raw = r#"{ "fs": { "./data": { "read": true, "write": false } }, "net": [], "vars": "*", "secrets": false }"#;
    let granular = SandboxSetting::Granular(SandboxAxes {
        fs: Some(SandboxAxis::Object(BTreeMap::from([(
            "./data".into(),
            serde_json::json!({ "read": true, "write": false }),
        )]))),
        net: Some(SandboxAxis::Array(Vec::new())),
        vars: Some(SandboxAxis::String("*".into())),
        secrets: Some(SandboxAxis::Bool(false)),
    });
    vec![
        ("false".into(), SandboxSetting::Disabled),
        ("true".into(), SandboxSetting::Enabled),
        (
            r#""build-jail""#.into(),
            SandboxSetting::Preset("build-jail".into()),
        ),
        (
            r#""./team.json""#.into(),
            SandboxSetting::FileRef("./team.json".into()),
        ),
        (granular_raw.into(), granular),
    ]
}

fn project_layer(values: ProjectConfig) -> Option<LoadedConfig> {
    Some(LoadedConfig {
        source: ConfigSource::file(ConfigSourceKind::Project, Path::new("/project/nub.jsonc")),
        values,
    })
}

#[test]
fn five_shapes_round_trip_losslessly_at_the_nested_positions() {
    for (raw, expected) in five_shapes() {
        let install = parse_project_config(&format!(r#"{{ "install": {{ "sandbox": {raw} }} }}"#))
            .expect("install.sandbox shape parses");
        assert_eq!(
            install.install.sandbox,
            Some(expected.clone()),
            "install.sandbox: {raw}"
        );

        let dlx = parse_project_config(&format!(r#"{{ "dlx": {{ "sandbox": {raw} }} }}"#))
            .expect("dlx.sandbox shape parses");
        assert_eq!(dlx.dlx.sandbox, Some(expected), "dlx.sandbox: {raw}");
    }
}

#[test]
fn runtime_projection_with_any_sandbox_shape_equals_the_no_sandbox_baseline() {
    let base_values = ProjectConfig {
        preload: Some(vec!["./preload.mjs".into()]),
        node_options: Some(vec!["--stack-trace-limit=7".into()]),
        env: Some(EnvSetting::Disabled),
        ..ProjectConfig::default()
    };
    let baseline = resolve_effective_config(
        Path::new("/cwd"),
        None,
        project_layer(base_values.clone()),
        ConfigOverlays::default(),
    )
    .runtime_config()
    .expect("baseline runtime projection");

    for (raw, shape) in five_shapes() {
        let mut values = base_values.clone();
        values.sandbox = Some(shape.clone());
        values.install.sandbox = Some(shape.clone());
        values.dlx.sandbox = Some(shape);
        let projected = resolve_effective_config(
            Path::new("/cwd"),
            None,
            project_layer(values),
            ConfigOverlays::default(),
        )
        .runtime_config()
        .expect("sandboxed runtime projection");
        assert_eq!(
            projected, baseline,
            "runtime projection must not react to sandbox shape {raw}"
        );
    }
}

#[test]
fn dlx_projection_with_any_sandbox_shape_equals_the_no_sandbox_baseline() {
    // A real dlx layer (env disabled ⇒ Some(empty), consent set) so the equality
    // compares live projections, not two Nones.
    let base_dlx = DlxConfig {
        consent: Some(ImplicitDlx::Never),
        env: Some(EnvSetting::Disabled),
        sandbox: None,
    };
    let baseline_snapshot = resolve_effective_config(
        Path::new("/cwd"),
        None,
        project_layer(ProjectConfig {
            dlx: base_dlx.clone(),
            ..ProjectConfig::default()
        }),
        ConfigOverlays::default(),
    );
    let baseline_env = dlx_env_for(&baseline_snapshot);
    let baseline_consent = dlx_consent_for(&baseline_snapshot, ImplicitDlx::Prompt);
    assert_eq!(baseline_env, Some(BTreeMap::new()), "baseline is live");
    assert_eq!(baseline_consent, ImplicitDlx::Never, "baseline is live");

    for (raw, shape) in five_shapes() {
        let mut dlx = base_dlx.clone();
        dlx.sandbox = Some(shape);
        let snapshot = resolve_effective_config(
            Path::new("/cwd"),
            None,
            project_layer(ProjectConfig {
                dlx,
                ..ProjectConfig::default()
            }),
            ConfigOverlays::default(),
        );
        assert_eq!(
            dlx_env_for(&snapshot),
            baseline_env,
            "dlx env projection must not react to sandbox shape {raw}"
        );
        assert_eq!(
            dlx_consent_for(&snapshot, ImplicitDlx::Prompt),
            baseline_consent,
            "dlx consent must not react to sandbox shape {raw}"
        );
    }
}
