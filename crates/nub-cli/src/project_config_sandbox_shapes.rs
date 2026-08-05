//! P6 — sandbox config values are LOSSLESS through parse and INERT in every
//! Phase-0 consumer projection. One four-shape table (the wrapper's full value
//! space: `false | true | preset | file-ref`) is shared across the top-level and
//! `dlx.sandbox` positions, and reused by pm_engine's install-lowering test.
//! Top-level parse + snapshot retention is covered by the sibling
//! `sandbox_four_shapes_are_lossless_and_inert_in_the_snapshot`; this module
//! owns the nested positions and the projection-equality (inertness) proofs.

use super::*;
use crate::config::ImplicitDlx;

/// The four policy shapes, as (raw JSONC value, expected parse). An inline
/// granular object is no longer a shape (rejected by `validate_sandbox`); a
/// granular policy lives in a file, covered by the `FileRef` shape.
pub(crate) fn four_shapes() -> Vec<(String, SandboxSetting)> {
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
    ]
}

fn project_layer(values: ProjectConfig) -> Option<LoadedConfig> {
    Some(LoadedConfig {
        source: ConfigSource::file(ConfigSourceKind::Project, Path::new("/project/nub.jsonc")),
        values,
    })
}

/// `dlx` is global-only, so anything reaching into it is layered here rather
/// than through [`project_layer`].
fn global_layer(values: ProjectConfig) -> Option<LoadedConfig> {
    Some(LoadedConfig {
        source: ConfigSource::file(ConfigSourceKind::Global, Path::new("/config/nub/nub.jsonc")),
        values,
    })
}

#[test]
fn four_shapes_round_trip_losslessly_at_the_nested_positions() {
    for (raw, expected) in four_shapes() {
        let dlx = parse_global_config(&format!(r#"{{ "dlx": {{ "sandbox": {raw} }} }}"#))
            .expect("dlx.sandbox shape parses");
        assert_eq!(
            dlx.dlx.sandbox,
            Some(expected.clone()),
            "dlx.sandbox: {raw}"
        );
        let _ = expected;
    }
}

#[test]
fn runtime_projection_with_any_sandbox_shape_equals_the_no_sandbox_baseline() {
    let base_values = ProjectConfig {
        preload: Some(vec!["./preload.mjs".into()]),
        node_options: Some(vec!["--stack-trace-limit=7".into()]),
        env_file: Some(EnvFileSetting::Disabled),
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

    for (raw, shape) in four_shapes() {
        let mut values = base_values.clone();
        values.sandbox = Some(shape.clone());
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
        env: Some(EnvFileSetting::Disabled),
        sandbox: None,
    };
    let baseline_snapshot = resolve_effective_config(
        Path::new("/cwd"),
        global_layer(ProjectConfig {
            dlx: base_dlx.clone(),
            ..ProjectConfig::default()
        }),
        None,
        ConfigOverlays::default(),
    );
    let baseline_env = dlx_env_for(&baseline_snapshot);
    let baseline_consent = dlx_consent_for(&baseline_snapshot, ImplicitDlx::Prompt);
    assert_eq!(baseline_env, Some(BTreeMap::new()), "baseline is live");
    assert_eq!(baseline_consent, ImplicitDlx::Never, "baseline is live");

    for (raw, shape) in four_shapes() {
        let mut dlx = base_dlx.clone();
        dlx.sandbox = Some(shape);
        let snapshot = resolve_effective_config(
            Path::new("/cwd"),
            global_layer(ProjectConfig {
                dlx,
                ..ProjectConfig::default()
            }),
            None,
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
