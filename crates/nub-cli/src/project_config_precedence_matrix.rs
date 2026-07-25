//! P5 — every `ConfigKey` through the full precedence chain
//! `CLI > environment > project > global > defaults`, asserting both the
//! resolved VALUE and its recorded SOURCE, plus the explicit-false / explicit-
//! empty rule (a `false`/`[]`/`{}` in a higher layer must beat a lower truthy —
//! the classic Option-vs-falsy precedence bug).
//!
//! One spec per key. Values are derived from a per-layer tag so ADJACENT layers
//! always hold distinguishable values (bools/enums alternate by parity), which
//! is what catches an off-by-one in the merge order.

use super::*;
use crate::config::ImplicitDlx;

struct KeySpec {
    key: ConfigKey,
    name: &'static str,
    /// Write this key's tag-derived value into a layer config.
    set: fn(&mut ProjectConfig, usize),
    /// Does the resolved config hold exactly this key's tag-derived value?
    matches: fn(&ProjectConfig, usize) -> bool,
    /// The key's explicit-false / explicit-empty form, where its value space
    /// has one. Must differ from `set(_, 0)` (the truthy lower-layer value).
    set_empty: Option<fn(&mut ProjectConfig)>,
    is_empty: Option<fn(&ProjectConfig) -> bool>,
}

fn strings(tag: usize) -> Vec<String> {
    vec![format!("value-{tag}")]
}

fn string_map(tag: usize) -> BTreeMap<String, String> {
    BTreeMap::from([("KEY".to_string(), format!("value-{tag}"))])
}

fn verify_deps(tag: usize) -> VerifyDepsBeforeRun {
    [
        VerifyDepsBeforeRun::Install,
        VerifyDepsBeforeRun::Warn,
        VerifyDepsBeforeRun::Error,
        VerifyDepsBeforeRun::Prompt,
    ][tag % 4]
        .clone()
}

fn linker(tag: usize) -> NodeLinker {
    [
        NodeLinker::Symlink,
        NodeLinker::Isolated,
        NodeLinker::Hoisted,
        NodeLinker::Pnp,
    ][tag % 4]
}

fn consent(tag: usize) -> ImplicitDlx {
    if tag.is_multiple_of(2) {
        ImplicitDlx::Prompt
    } else {
        ImplicitDlx::Never
    }
}

fn preset(tag: usize) -> SandboxSetting {
    SandboxSetting::Preset(format!("preset-{tag}"))
}

fn specs() -> Vec<KeySpec> {
    vec![
        KeySpec {
            key: ConfigKey::NodeCompat,
            name: "nodeCompat",
            set: |c, t| c.node_compat = Some(t.is_multiple_of(2)),
            matches: |c, t| c.node_compat == Some(t.is_multiple_of(2)),
            set_empty: Some(|c| c.node_compat = Some(false)),
            is_empty: Some(|c| c.node_compat == Some(false)),
        },
        KeySpec {
            key: ConfigKey::Preload,
            name: "preload",
            set: |c, t| c.preload = Some(strings(t)),
            matches: |c, t| c.preload == Some(strings(t)),
            set_empty: Some(|c| c.preload = Some(Vec::new())),
            is_empty: Some(|c| c.preload == Some(Vec::new())),
        },
        KeySpec {
            key: ConfigKey::NodeOptions,
            name: "nodeOptions",
            set: |c, t| c.node_options = Some(strings(t)),
            matches: |c, t| c.node_options == Some(strings(t)),
            set_empty: Some(|c| c.node_options = Some(Vec::new())),
            is_empty: Some(|c| c.node_options == Some(Vec::new())),
        },
        KeySpec {
            key: ConfigKey::V8Flags,
            name: "v8Flags",
            set: |c, t| c.v8_flags = Some(strings(t)),
            matches: |c, t| c.v8_flags == Some(strings(t)),
            set_empty: Some(|c| c.v8_flags = Some(Vec::new())),
            is_empty: Some(|c| c.v8_flags == Some(Vec::new())),
        },
        KeySpec {
            key: ConfigKey::Env,
            name: "env",
            set: |c, t| c.env = Some(EnvSetting::Sources(strings(t))),
            matches: |c, t| c.env == Some(EnvSetting::Sources(strings(t))),
            // `env: false` — the explicit disable — must beat a lower source list.
            set_empty: Some(|c| c.env = Some(EnvSetting::Disabled)),
            is_empty: Some(|c| c.env == Some(EnvSetting::Disabled)),
        },
        KeySpec {
            key: ConfigKey::Define,
            name: "define",
            set: |c, t| c.define = Some(string_map(t)),
            matches: |c, t| c.define == Some(string_map(t)),
            set_empty: Some(|c| c.define = Some(BTreeMap::new())),
            is_empty: Some(|c| c.define == Some(BTreeMap::new())),
        },
        KeySpec {
            key: ConfigKey::Loader,
            name: "loader",
            set: |c, t| c.loader = Some(string_map(t)),
            matches: |c, t| c.loader == Some(string_map(t)),
            set_empty: Some(|c| c.loader = Some(BTreeMap::new())),
            is_empty: Some(|c| c.loader == Some(BTreeMap::new())),
        },
        KeySpec {
            key: ConfigKey::Conditions,
            name: "conditions",
            set: |c, t| c.conditions = Some(strings(t)),
            matches: |c, t| c.conditions == Some(strings(t)),
            set_empty: Some(|c| c.conditions = Some(Vec::new())),
            is_empty: Some(|c| c.conditions == Some(Vec::new())),
        },
        KeySpec {
            key: ConfigKey::Tsconfig,
            name: "tsconfig",
            set: |c, t| c.tsconfig = Some(format!("./tsconfig.{t}.json")),
            matches: |c, t| c.tsconfig.as_deref() == Some(format!("./tsconfig.{t}.json").as_str()),
            // A string path has no meaningful false/empty form.
            set_empty: None,
            is_empty: None,
        },
        KeySpec {
            key: ConfigKey::VerifyDepsBeforeRun,
            name: "verifyDepsBeforeRun",
            set: |c, t| c.verify_deps_before_run = Some(verify_deps(t)),
            matches: |c, t| c.verify_deps_before_run == Some(verify_deps(t)),
            set_empty: Some(|c| {
                c.verify_deps_before_run = Some(VerifyDepsBeforeRun::Enabled(false))
            }),
            is_empty: Some(|c| {
                c.verify_deps_before_run == Some(VerifyDepsBeforeRun::Enabled(false))
            }),
        },
        KeySpec {
            key: ConfigKey::Sandbox,
            name: "sandbox",
            set: |c, t| c.sandbox = Some(preset(t)),
            matches: |c, t| c.sandbox == Some(preset(t)),
            // `sandbox: false` — the explicit unjail — must beat a lower preset.
            set_empty: Some(|c| c.sandbox = Some(SandboxSetting::Disabled)),
            is_empty: Some(|c| c.sandbox == Some(SandboxSetting::Disabled)),
        },
        KeySpec {
            key: ConfigKey::InstallNodeLinker,
            name: "install.nodeLinker",
            set: |c, t| c.install.node_linker = Some(linker(t)),
            matches: |c, t| c.install.node_linker == Some(linker(t)),
            set_empty: None,
            is_empty: None,
        },
        KeySpec {
            key: ConfigKey::InstallSymlinkDisablePattern,
            name: "install.symlinkDisablePattern",
            set: |c, t| c.install.symlink_disable_pattern = Some(strings(t)),
            matches: |c, t| c.install.symlink_disable_pattern == Some(strings(t)),
            set_empty: Some(|c| c.install.symlink_disable_pattern = Some(Vec::new())),
            is_empty: Some(|c| c.install.symlink_disable_pattern == Some(Vec::new())),
        },
        KeySpec {
            key: ConfigKey::InstallHoist,
            name: "install.hoist",
            set: |c, t| c.install.hoist = Some(Hoist::Patterns(strings(t))),
            matches: |c, t| c.install.hoist == Some(Hoist::Patterns(strings(t))),
            // `hoist: false` (strict) must beat a lower pattern list.
            set_empty: Some(|c| c.install.hoist = Some(Hoist::Bool(false))),
            is_empty: Some(|c| c.install.hoist == Some(Hoist::Bool(false))),
        },
        KeySpec {
            key: ConfigKey::InstallMinimumReleaseAge,
            name: "install.minimumReleaseAge",
            set: |c, t| {
                c.install.minimum_release_age = Some(Duration::from_secs((t as u64 + 1) * 60))
            },
            matches: |c, t| {
                c.install.minimum_release_age == Some(Duration::from_secs((t as u64 + 1) * 60))
            },
            // An explicit zero age must beat a lower non-zero age.
            set_empty: Some(|c| c.install.minimum_release_age = Some(Duration::ZERO)),
            is_empty: Some(|c| c.install.minimum_release_age == Some(Duration::ZERO)),
        },
        KeySpec {
            key: ConfigKey::InstallMinimumReleaseAgeExclude,
            name: "install.minimumReleaseAgeExclude",
            set: |c, t| c.install.minimum_release_age_exclude = Some(strings(t)),
            matches: |c, t| c.install.minimum_release_age_exclude == Some(strings(t)),
            set_empty: Some(|c| c.install.minimum_release_age_exclude = Some(Vec::new())),
            is_empty: Some(|c| c.install.minimum_release_age_exclude == Some(Vec::new())),
        },
        KeySpec {
            key: ConfigKey::InstallNodeOptions,
            name: "install.nodeOptions",
            set: |c, t| c.install.node_options = Some(strings(t)),
            matches: |c, t| c.install.node_options == Some(strings(t)),
            set_empty: Some(|c| c.install.node_options = Some(Vec::new())),
            is_empty: Some(|c| c.install.node_options == Some(Vec::new())),
        },
        KeySpec {
            key: ConfigKey::InstallSandbox,
            name: "install.sandbox",
            set: |c, t| c.install.sandbox = Some(preset(t)),
            matches: |c, t| c.install.sandbox == Some(preset(t)),
            set_empty: Some(|c| c.install.sandbox = Some(SandboxSetting::Disabled)),
            is_empty: Some(|c| c.install.sandbox == Some(SandboxSetting::Disabled)),
        },
        KeySpec {
            key: ConfigKey::DlxConsent,
            name: "dlx.consent",
            set: |c, t| c.dlx.consent = Some(consent(t)),
            matches: |c, t| c.dlx.consent == Some(consent(t)),
            // The restrictive arm must beat a lower `prompt`.
            set_empty: Some(|c| c.dlx.consent = Some(ImplicitDlx::Never)),
            is_empty: Some(|c| c.dlx.consent == Some(ImplicitDlx::Never)),
        },
        KeySpec {
            key: ConfigKey::DlxSandbox,
            name: "dlx.sandbox",
            set: |c, t| c.dlx.sandbox = Some(preset(t)),
            matches: |c, t| c.dlx.sandbox == Some(preset(t)),
            set_empty: Some(|c| c.dlx.sandbox = Some(SandboxSetting::Disabled)),
            is_empty: Some(|c| c.dlx.sandbox == Some(SandboxSetting::Disabled)),
        },
        KeySpec {
            key: ConfigKey::DlxEnv,
            name: "dlx.env",
            set: |c, t| c.dlx.env = Some(EnvSetting::Sources(strings(t))),
            matches: |c, t| c.dlx.env == Some(EnvSetting::Sources(strings(t))),
            // The explicit-empty source list (distinct from `false`, covered on
            // the top-level `env`) must beat a lower non-empty list.
            set_empty: Some(|c| c.dlx.env = Some(EnvSetting::Sources(Vec::new()))),
            is_empty: Some(|c| c.dlx.env == Some(EnvSetting::Sources(Vec::new()))),
        },
    ]
}

/// Exhaustive by construction: adding a `ConfigKey` variant breaks this match,
/// forcing the new key into the spec table.
fn ordinal(key: ConfigKey) -> usize {
    match key {
        ConfigKey::NodeCompat => 0,
        ConfigKey::Preload => 1,
        ConfigKey::NodeOptions => 2,
        ConfigKey::V8Flags => 3,
        ConfigKey::Env => 4,
        ConfigKey::Define => 5,
        ConfigKey::Loader => 6,
        ConfigKey::Conditions => 7,
        ConfigKey::Tsconfig => 8,
        ConfigKey::VerifyDepsBeforeRun => 9,
        ConfigKey::Sandbox => 10,
        ConfigKey::InstallNodeLinker => 11,
        ConfigKey::InstallSymlinkDisablePattern => 12,
        ConfigKey::InstallHoist => 13,
        ConfigKey::InstallMinimumReleaseAge => 14,
        ConfigKey::InstallMinimumReleaseAgeExclude => 15,
        ConfigKey::InstallNodeOptions => 16,
        ConfigKey::InstallSandbox => 17,
        ConfigKey::DlxConsent => 18,
        ConfigKey::DlxSandbox => 19,
        ConfigKey::DlxEnv => 20,
    }
}

/// Layers ordered weakest → strongest, with the winner's `ConfigSourceKind`.
const LAYERS: [ConfigSourceKind; 5] = [
    ConfigSourceKind::Defaults,
    ConfigSourceKind::Global,
    ConfigSourceKind::Project,
    ConfigSourceKind::Environment,
    ConfigSourceKind::Cli,
];

/// Resolve with the given per-layer configs (index = position in [`LAYERS`];
/// `None` = layer absent).
fn resolve(layers: [Option<ProjectConfig>; 5]) -> EffectiveConfig {
    let [defaults, global, project, environment, cli] = layers;
    let file_layer = |kind, name: &str, values| {
        Some(LoadedConfig {
            source: ConfigSource::file(kind, Path::new(&format!("/{name}/nub.jsonc"))),
            values: values?,
        })
    };
    resolve_effective_config(
        Path::new("/cwd"),
        file_layer(ConfigSourceKind::Global, "global", global),
        file_layer(ConfigSourceKind::Project, "project", project),
        ConfigOverlays {
            cli: cli.unwrap_or_default(),
            environment: environment.unwrap_or_default(),
            defaults: defaults.unwrap_or_default(),
        },
    )
}

#[test]
fn spec_table_covers_every_config_key_exactly_once() {
    let specs = specs();
    assert_eq!(specs.len(), 21, "one spec per ConfigKey variant");
    let mut seen = [false; 21];
    for spec in &specs {
        let idx = ordinal(spec.key);
        assert!(!seen[idx], "duplicate spec for {}", spec.name);
        seen[idx] = true;
    }
}

#[test]
fn every_key_resolves_the_strongest_populated_layer_and_records_its_source() {
    for spec in specs() {
        // Round `w`: layers 0..=w populated (each with its own tag), layers
        // above absent — layer `w` must win on both value and source.
        for (w, winner_kind) in LAYERS.iter().enumerate() {
            let mut layers: [Option<ProjectConfig>; 5] = Default::default();
            for (tag, slot) in layers.iter_mut().enumerate().take(w + 1) {
                let mut config = ProjectConfig::default();
                (spec.set)(&mut config, tag);
                *slot = Some(config);
            }
            let snapshot = resolve(layers);
            assert!(
                (spec.matches)(&snapshot.values, w),
                "{}: layer {winner_kind:?} must supply the resolved value",
                spec.name
            );
            let source = snapshot
                .sources
                .get(&spec.key)
                .unwrap_or_else(|| panic!("{}: winning source must be recorded", spec.name));
            assert_eq!(
                source.kind, *winner_kind,
                "{}: recorded source must be the winning layer",
                spec.name
            );
        }
    }
}

#[test]
fn absent_everywhere_resolves_to_no_value_and_no_source() {
    let snapshot = resolve(Default::default());
    for spec in specs() {
        assert!(
            !snapshot.sources.contains_key(&spec.key),
            "{}: no layer set it, so no source may be recorded",
            spec.name
        );
    }
    assert_eq!(snapshot.values, ProjectConfig::default());
}

#[test]
fn explicit_false_or_empty_in_a_higher_layer_beats_a_lower_truthy_value() {
    for spec in specs() {
        let (Some(set_empty), Some(is_empty)) = (spec.set_empty, spec.is_empty) else {
            continue;
        };
        // Project holds the truthy value, CLI the explicit false/empty form.
        let mut truthy = ProjectConfig::default();
        (spec.set)(&mut truthy, 0);
        assert!(
            !is_empty(&truthy),
            "{}: the truthy fixture value must be distinguishable from empty",
            spec.name
        );
        let mut empty = ProjectConfig::default();
        set_empty(&mut empty);

        let snapshot = resolve([None, None, Some(truthy), None, Some(empty)]);
        assert!(
            is_empty(&snapshot.values),
            "{}: an explicit false/empty CLI value must beat the project's truthy value",
            spec.name
        );
        assert_eq!(
            snapshot.sources.get(&spec.key).map(|source| source.kind),
            Some(ConfigSourceKind::Cli),
            "{}: the explicit false/empty layer must be recorded as the source",
            spec.name
        );
    }
}
