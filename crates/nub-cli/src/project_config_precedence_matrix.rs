//! Every `ConfigKey` through the full precedence chain
//! `CLI > environment > project > global > defaults`, asserting both the
//! resolved VALUE and its recorded SOURCE, plus the explicit-false / explicit-
//! empty rule (a `false`/`[]`/`{}` in a higher layer must beat a lower truthy —
//! the classic Option-vs-falsy precedence bug).
//!
//! One spec per key. Values are derived from a per-layer tag so ADJACENT layers
//! always hold distinguishable values (bools/enums alternate by parity), which
//! is what catches an off-by-one in the merge order.
//!
//! Every key is exercised at every layer, `dlx.*` included, even though a
//! project FILE may not carry `dlx`: that carve-out lives in the parser, and the
//! merge is deliberately scope-blind. Skipping layers here would trade a real
//! merge-order check for a redundant restatement of the parser's gate.

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

/// Only the two values a `nub.jsonc` may carry — `Install`/`Prompt` exist for
/// the pnpm-mirroring surfaces, not this one. Both differ from the explicit-
/// empty form (`Enabled(false)`), which is what the falsy-precedence case needs.
fn verify_deps(tag: usize) -> VerifyDeps {
    if tag.is_multiple_of(2) {
        VerifyDeps::Warn
    } else {
        VerifyDeps::Error
    }
}

/// One variant per strategy, and the two that carry a knob carry a tagged one —
/// so a layer that wins must win the whole union, tag and payload together.
fn linker(tag: usize) -> LinkerConfig {
    match tag % 4 {
        0 => LinkerConfig::Global {
            eject: Some(strings(tag)),
        },
        1 => LinkerConfig::Isolated {
            hoist: Some(Hoist::Patterns(strings(tag))),
        },
        2 => LinkerConfig::Hoisted,
        _ => LinkerConfig::Pnp,
    }
}

fn consent(tag: usize) -> ImplicitDlx {
    if tag.is_multiple_of(2) {
        ImplicitDlx::Prompt
    } else {
        ImplicitDlx::Never
    }
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
            key: ConfigKey::NodeExecutable,
            name: "nodeExecutable",
            set: |c, t| c.node_executable = Some(format!("./node-{t}")),
            matches: |c, t| c.node_executable.as_deref() == Some(format!("./node-{t}").as_str()),
            // A non-empty path string has no false/empty form.
            set_empty: None,
            is_empty: None,
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
            key: ConfigKey::EnvFile,
            name: "envFile",
            set: |c, t| c.env_file = Some(EnvFileSetting::Sources(strings(t))),
            matches: |c, t| c.env_file == Some(EnvFileSetting::Sources(strings(t))),
            // `envFile: false` — the explicit disable — must beat a lower source list.
            set_empty: Some(|c| c.env_file = Some(EnvFileSetting::Disabled)),
            is_empty: Some(|c| c.env_file == Some(EnvFileSetting::Disabled)),
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
            key: ConfigKey::Jsx,
            name: "jsx",
            set: |c, t| {
                c.jsx = Some(
                    if t.is_multiple_of(2) {
                        "react"
                    } else {
                        "react-jsx"
                    }
                    .into(),
                )
            },
            matches: |c, t| {
                c.jsx.as_deref()
                    == Some(if t.is_multiple_of(2) {
                        "react"
                    } else {
                        "react-jsx"
                    })
            },
            set_empty: None,
            is_empty: None,
        },
        KeySpec {
            key: ConfigKey::JsxFactory,
            name: "jsxFactory",
            set: |c, t| c.jsx_factory = Some(format!("factory{t}")),
            matches: |c, t| c.jsx_factory.as_deref() == Some(format!("factory{t}").as_str()),
            set_empty: None,
            is_empty: None,
        },
        KeySpec {
            key: ConfigKey::JsxFragmentFactory,
            name: "jsxFragmentFactory",
            set: |c, t| c.jsx_fragment_factory = Some(format!("fragment{t}")),
            matches: |c, t| {
                c.jsx_fragment_factory.as_deref() == Some(format!("fragment{t}").as_str())
            },
            set_empty: None,
            is_empty: None,
        },
        KeySpec {
            key: ConfigKey::JsxImportSource,
            name: "jsxImportSource",
            set: |c, t| c.jsx_import_source = Some(format!("runtime-{t}")),
            matches: |c, t| c.jsx_import_source.as_deref() == Some(format!("runtime-{t}").as_str()),
            set_empty: None,
            is_empty: None,
        },
        KeySpec {
            key: ConfigKey::Decorators,
            name: "decorators",
            set: |c, _| c.decorators = Some(crate::project_config::DecoratorMode::Legacy),
            matches: |c, _| c.decorators == Some(crate::project_config::DecoratorMode::Legacy),
            set_empty: None,
            is_empty: None,
        },
        KeySpec {
            key: ConfigKey::EmitDecoratorMetadata,
            name: "emitDecoratorMetadata",
            set: |c, t| c.emit_decorator_metadata = Some(t.is_multiple_of(2)),
            matches: |c, t| c.emit_decorator_metadata == Some(t.is_multiple_of(2)),
            set_empty: Some(|c| c.emit_decorator_metadata = Some(false)),
            is_empty: Some(|c| c.emit_decorator_metadata == Some(false)),
        },
        KeySpec {
            key: ConfigKey::VerifyDeps,
            name: "verifyDeps",
            set: |c, t| c.verify_deps = Some(verify_deps(t)),
            matches: |c, t| c.verify_deps == Some(verify_deps(t)),
            set_empty: Some(|c| c.verify_deps = Some(VerifyDeps::Enabled(false))),
            is_empty: Some(|c| c.verify_deps == Some(VerifyDeps::Enabled(false))),
        },
        KeySpec {
            key: ConfigKey::InstallLinker,
            name: "install.linker",
            set: |c, t| c.install.linker = Some(linker(t)),
            matches: |c, t| c.install.linker == Some(linker(t)),
            // `hoist: false` (strict) must beat a lower pattern list, and it is
            // only reachable under `isolated` — so the empty form pins the tag too.
            set_empty: Some(|c| {
                c.install.linker = Some(LinkerConfig::Isolated {
                    hoist: Some(Hoist::Bool(false)),
                })
            }),
            is_empty: Some(|c| {
                c.install.linker
                    == Some(LinkerConfig::Isolated {
                        hoist: Some(Hoist::Bool(false)),
                    })
            }),
        },
        KeySpec {
            key: ConfigKey::InstallPublicHoist,
            name: "install.publicHoist",
            set: |c, t| c.install.public_hoist = Some(strings(t)),
            matches: |c, t| c.install.public_hoist.as_deref() == Some(strings(t).as_slice()),
            // `publicHoist: []` — the explicit opt-out — must beat a lower list.
            set_empty: Some(|c| c.install.public_hoist = Some(Vec::new())),
            is_empty: Some(|c| c.install.public_hoist.as_deref() == Some(&[][..])),
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
            key: ConfigKey::DlxConsent,
            name: "dlx.consent",
            set: |c, t| c.dlx.consent = Some(consent(t)),
            matches: |c, t| c.dlx.consent == Some(consent(t)),
            // The restrictive arm must beat a lower `prompt`.
            set_empty: Some(|c| c.dlx.consent = Some(ImplicitDlx::Never)),
            is_empty: Some(|c| c.dlx.consent == Some(ImplicitDlx::Never)),
        },
        KeySpec {
            key: ConfigKey::InstallBuildJail,
            name: "install.buildJail",
            set: |c, t| c.install.build_jail = Some(t.is_multiple_of(2)),
            matches: |c, t| c.install.build_jail == Some(t.is_multiple_of(2)),
            set_empty: Some(|c| c.install.build_jail = Some(false)),
            is_empty: Some(|c| c.install.build_jail == Some(false)),
        },
    ]
}

/// One per `ConfigKey` variant; [`ordinal`] is what keeps it honest.
const KEY_COUNT: usize = 22;

/// Exhaustive by construction: adding a `ConfigKey` variant breaks this match,
/// forcing the new key into the spec table.
fn ordinal(key: ConfigKey) -> usize {
    match key {
        ConfigKey::NodeCompat => 0,
        ConfigKey::NodeExecutable => 1,
        ConfigKey::Preload => 2,
        ConfigKey::NodeOptions => 3,
        ConfigKey::V8Flags => 4,
        ConfigKey::EnvFile => 5,
        ConfigKey::Loader => 6,
        ConfigKey::Conditions => 7,
        ConfigKey::Tsconfig => 8,
        ConfigKey::Jsx => 9,
        ConfigKey::JsxFactory => 10,
        ConfigKey::JsxFragmentFactory => 11,
        ConfigKey::JsxImportSource => 12,
        ConfigKey::Decorators => 13,
        ConfigKey::EmitDecoratorMetadata => 14,
        ConfigKey::VerifyDeps => 15,
        ConfigKey::InstallLinker => 16,
        ConfigKey::InstallPublicHoist => 17,
        ConfigKey::InstallMinimumReleaseAge => 18,
        ConfigKey::InstallMinimumReleaseAgeExclude => 19,
        ConfigKey::DlxConsent => 20,
        ConfigKey::InstallBuildJail => 21,
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
    assert_eq!(specs.len(), KEY_COUNT, "one spec per ConfigKey variant");
    let mut seen = [false; KEY_COUNT];
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
