//! An embedder whose `manifest_namespace` is `""` ("manifest root") writes
//! map settings (`allowBuilds`, `patchedDependencies`, …) as **top-level**
//! `package.json` keys — never nested under a namespace object, and never
//! under a foreign brand's key (`pnpm`).
//!
//! Lives in its own integration-test binary (= its own process) because the
//! active identity is once-per-process: the in-crate unit tests run under the
//! default `aube` identity (`manifest_namespace="aube"`) and can't flip it.
//!
//! This mirrors nub's profile (`manifest_namespace=""`, `compatible_names=
//! ["pnpm"]`), whose own migration writer emits these settings at the manifest
//! root and whose read side gates the `pnpm` namespace off — so a nested
//! `pnpm.*` write would be orphaned.

use aube_manifest::{
    AllowBuildRaw, PackageJson, workspace::edit_setting_map, workspace::set_allow_builds,
};
use aube_util::Embedder;

static ROOT_TOOL: Embedder = Embedder {
    name: "roottool",
    display_name: "roottool",
    vendor: None,
    version: "1.0.0",
    user_agent: "roottool/1.0.0",
    self_names: &["roottool"],
    compatible_names: &["pnpm"],
    lockfile_basename: "roottool-lock.yaml",
    workspace_yaml: None,
    manifest_namespace: "",
    env_prefix: None,
    config_env_prefix: None,
    cache_namespace: "roottool",
    data_namespace: "roottool",
    canonical_lockfile_always_wins: true,
    runtime_switching: true,
    self_engines_check: true,
    self_update_enabled: true,
    warm_store_verify: true,
    no_churn_lockfile_write: false,
    read_branded_settings_env: true,
    primer_ttl: None,
};

fn read_manifest(dir: &std::path::Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(dir.join("package.json")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

/// A map setting written under a `manifest_namespace=""` embedder lands at the
/// manifest root, an existing root-level entry round-trips (merge), and neither
/// a `""` key nor a foreign `pnpm` namespace is created — even when `pnpm` is
/// already declared in the manifest.
#[test]
fn root_embedder_writes_map_settings_at_manifest_root() {
    aube_util::set_embedder(&ROOT_TOOL);

    let tmp = tempfile::tempdir().unwrap();
    // Pre-existing `pnpm` object present (the case that must NOT divert the
    // write into `pnpm`), plus a prior root-level `allowBuilds` entry that
    // must survive the merge.
    std::fs::write(
        tmp.path().join("package.json"),
        "{\n  \"name\": \"x\",\n  \"pnpm\": {},\n  \"allowBuilds\": { \"old\": true }\n}\n",
    )
    .unwrap();

    edit_setting_map(tmp.path(), "allowBuilds", |m| {
        m.insert("esbuild".to_string(), serde_json::Value::Bool(true));
    })
    .unwrap();

    let value = read_manifest(tmp.path());
    let obj = value.as_object().unwrap();

    // Lands at the manifest ROOT as a top-level map key.
    assert_eq!(
        obj["allowBuilds"]["esbuild"],
        serde_json::Value::Bool(true),
        "new entry must land at top-level allowBuilds, got: {obj:#?}"
    );
    // The pre-existing root-level entry round-trips via the merge.
    assert_eq!(
        obj["allowBuilds"]["old"],
        serde_json::Value::Bool(true),
        "existing root-level entry must survive the write"
    );
    // Never an empty-string namespace key.
    assert!(
        !obj.contains_key(""),
        "must never write an empty-string namespace key, got: {obj:#?}"
    );
    // Never nested under the foreign `pnpm` brand.
    assert!(
        obj.get("pnpm").and_then(|p| p.get("allowBuilds")).is_none(),
        "must never nest the setting under the pnpm namespace, got: {obj:#?}"
    );
}

#[test]
fn root_embedder_reads_neutral_top_level_allow_builds_on_every_surface() {
    aube_util::set_embedder(&ROOT_TOOL);

    let manifest = PackageJson::parse(
        std::path::Path::new("package.json"),
        r#"{
            "name": "x",
            "allowBuilds": {
                "esbuild": true,
                "sharp": false
            },
            "pnpm": {
                "allowBuilds": {
                    "left-pad": true
                }
            }
        }"#
        .to_string(),
    )
    .unwrap();

    // A non-pnpm incumbent under a manifest-root embedder: the pnpm namespace
    // is gated off, but the top-level `allowBuilds` is the embedder's own
    // un-branded key and is read on EVERY surface — so `approve-builds` heals
    // here (the npm/bun/yarn incumbent case). The pnpm-branded `left-pad` entry
    // is NOT read on this surface.
    aube_util::update_engine_context(|ctx| {
        ctx.read_branded_pnpm_config = false;
        ctx.read_manifest_root_config = false;
    });
    let compat = manifest.pnpm_allow_builds();
    assert!(matches!(
        compat.get("esbuild"),
        Some(AllowBuildRaw::Bool(true))
    ));
    assert!(matches!(
        compat.get("sharp"),
        Some(AllowBuildRaw::Bool(false))
    ));
    assert!(
        !compat.contains_key("left-pad"),
        "the pnpm-branded entry must not be read on a non-pnpm surface"
    );

    // Pnpm/fresh mode reads the pnpm-branded config; the neutral top-level key
    // is also read (it's the embedder's own un-branded key), merged with
    // later-wins so a top-level entry can override a pnpm one on key conflict.
    aube_util::update_engine_context(|ctx| {
        ctx.read_branded_pnpm_config = true;
        ctx.read_manifest_root_config = false;
    });
    let pnpm = manifest.pnpm_allow_builds();
    assert!(matches!(
        pnpm.get("left-pad"),
        Some(AllowBuildRaw::Bool(true))
    ));
    assert!(matches!(
        pnpm.get("esbuild"),
        Some(AllowBuildRaw::Bool(true))
    ));

    // NubIdentity-style mode gates pnpm off and reads root `allowBuilds` as the
    // native config surface produced by `pm use nub`.
    aube_util::update_engine_context(|ctx| {
        ctx.read_branded_pnpm_config = false;
        ctx.read_manifest_root_config = true;
    });
    let root = manifest.pnpm_allow_builds();
    assert!(matches!(
        root.get("esbuild"),
        Some(AllowBuildRaw::Bool(true))
    ));
    assert!(matches!(
        root.get("sharp"),
        Some(AllowBuildRaw::Bool(false))
    ));
    assert!(!root.contains_key("left-pad"));
}

/// The approve-builds heal path (pnpm-compat/fresh surface, no workspace
/// yaml on disk): under a manifest-root embedder with `read_branded_pnpm_config`
/// on and `read_manifest_root_config` off — the common case — an *approval*
/// (`allow=true`) lands in the nested `package.json#pnpm.onlyBuiltDependencies`
/// array (pnpm's canonical allowlist, which the read side and real pnpm 10.x
/// both honor) WITHOUT creating a `pnpm-workspace.yaml`. Round-trips through the
/// read side to prove the write is visible.
#[test]
fn set_allow_builds_writes_pnpm_only_built_deps_on_pnpm_surface_no_yaml() {
    aube_util::set_embedder(&ROOT_TOOL);
    aube_util::update_engine_context(|ctx| {
        ctx.read_branded_pnpm_config = true;
        ctx.read_manifest_root_config = false;
    });

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("package.json"),
        "{\n  \"name\": \"x\",\n  \"dependencies\": { \"core-js\": \"3.37.1\" }\n}\n",
    )
    .unwrap();

    let written = set_allow_builds(tmp.path(), &["core-js".to_string()], true).unwrap();

    // Lands in package.json — not a freshly-created workspace yaml.
    assert_eq!(
        written.file_name().and_then(|n| n.to_str()),
        Some("package.json"),
        "approval on the pnpm-compat surface (no yaml) must write package.json, got: {written:?}"
    );
    assert!(
        !tmp.path().join("pnpm-workspace.yaml").exists(),
        "must not create a pnpm-workspace.yaml where none existed"
    );
    // Nested under `pnpm.onlyBuiltDependencies`, not the unread top-level key.
    let manifest = read_manifest(tmp.path());
    assert!(
        manifest.get("allowBuilds").is_none(),
        "must not write an unread top-level allowBuilds key, got: {manifest:#?}"
    );
    assert_eq!(
        manifest["pnpm"]["onlyBuiltDependencies"],
        serde_json::json!(["core-js"]),
        "approval must land in pnpm.onlyBuiltDependencies, got: {manifest:#?}"
    );

    // The write round-trips through the read side on this surface.
    let parsed = PackageJson::parse(
        &tmp.path().join("package.json"),
        std::fs::read_to_string(tmp.path().join("package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        parsed.pnpm_only_built_dependencies(),
        vec!["core-js".to_string()],
        "read side must see the approval on the pnpm-compat surface"
    );
}

/// An *existing* `pnpm-workspace.yaml` still wins on the pnpm-compat surface:
/// the approval appends there (keeping all workspace config in one place)
/// rather than splitting it into `package.json`.
#[test]
fn set_allow_builds_appends_existing_yaml_on_pnpm_surface() {
    aube_util::set_embedder(&ROOT_TOOL);
    aube_util::update_engine_context(|ctx| {
        ctx.read_branded_pnpm_config = true;
        ctx.read_manifest_root_config = false;
    });

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("package.json"), "{\n  \"name\": \"x\"\n}\n").unwrap();
    std::fs::write(
        tmp.path().join("pnpm-workspace.yaml"),
        "packages:\n  - 'pkg/*'\n",
    )
    .unwrap();

    let written = set_allow_builds(tmp.path(), &["core-js".to_string()], true).unwrap();

    assert_eq!(
        written.file_name().and_then(|n| n.to_str()),
        Some("pnpm-workspace.yaml"),
        "an existing workspace yaml must be the write target, got: {written:?}"
    );
    let yaml = std::fs::read_to_string(tmp.path().join("pnpm-workspace.yaml")).unwrap();
    assert!(
        yaml.contains("allowBuilds:") && yaml.contains("core-js") && yaml.contains("pkg/*"),
        "existing yaml must gain the approval and keep its prior content, got:\n{yaml}"
    );
    let manifest = read_manifest(tmp.path());
    assert!(
        manifest.get("pnpm").is_none(),
        "must not also write package.json when a yaml exists, got: {manifest:#?}"
    );
}

/// A denial (`allow=false`) on the pnpm-compat surface with no yaml lands in
/// the nested `pnpm.allowBuilds` map (the array allowlist can't carry a
/// `false`), again without creating a workspace yaml.
#[test]
fn set_allow_builds_writes_pnpm_allow_builds_map_for_denial_no_yaml() {
    aube_util::set_embedder(&ROOT_TOOL);
    aube_util::update_engine_context(|ctx| {
        ctx.read_branded_pnpm_config = true;
        ctx.read_manifest_root_config = false;
    });

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("package.json"), "{\n  \"name\": \"x\"\n}\n").unwrap();

    let written = set_allow_builds(tmp.path(), &["esbuild".to_string()], false).unwrap();

    assert_eq!(
        written.file_name().and_then(|n| n.to_str()),
        Some("package.json")
    );
    assert!(!tmp.path().join("pnpm-workspace.yaml").exists());
    let manifest = read_manifest(tmp.path());
    assert_eq!(
        manifest["pnpm"]["allowBuilds"]["esbuild"],
        serde_json::Value::Bool(false),
        "denial must land in pnpm.allowBuilds as false, got: {manifest:#?}"
    );
    let parsed = PackageJson::parse(
        &tmp.path().join("package.json"),
        std::fs::read_to_string(tmp.path().join("package.json")).unwrap(),
    )
    .unwrap();
    assert!(
        matches!(
            parsed.pnpm_allow_builds().get("esbuild"),
            Some(AllowBuildRaw::Bool(false))
        ),
        "read side must see the denial on the pnpm-compat surface"
    );
}

/// `approve-builds` HEALS on the NonPnpmCompat (npm/bun/yarn incumbent)
/// surface: with both gates off, `set_allow_builds` writes the top-level
/// `package.json#allowBuilds` key, and the read side now consults that neutral,
/// un-branded key on every surface — so the approval takes effect and the next
/// install runs the script. (Previously a documented no-op; the gap is closed.)
#[test]
fn set_allow_builds_heals_on_non_pnpm_compat_surface() {
    aube_util::set_embedder(&ROOT_TOOL);
    // npm/bun/yarn incumbent: the pnpm namespace is gated off and this is not
    // nub identity, but the neutral top-level key is still read.
    aube_util::update_engine_context(|ctx| {
        ctx.read_branded_pnpm_config = false;
        ctx.read_manifest_root_config = false;
    });

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("package.json"), "{\n  \"name\": \"x\"\n}\n").unwrap();

    let written = set_allow_builds(tmp.path(), &["core-js".to_string()], true).unwrap();
    assert_eq!(
        written.file_name().and_then(|n| n.to_str()),
        Some("package.json"),
        "approval lands in package.json, not a fresh pnpm-workspace.yaml, got: {written:?}"
    );
    assert!(
        !tmp.path().join("pnpm-workspace.yaml").exists(),
        "must not create a pnpm-workspace.yaml on a non-pnpm surface"
    );

    // The approval is written at the top level…
    let manifest = read_manifest(tmp.path());
    assert_eq!(
        manifest["allowBuilds"]["core-js"],
        serde_json::Value::Bool(true)
    );
    // …and the read side on this surface now honors it: the approval takes
    // effect, so the next install runs the dep's lifecycle script.
    let parsed = PackageJson::parse(
        &tmp.path().join("package.json"),
        std::fs::read_to_string(tmp.path().join("package.json")).unwrap(),
    )
    .unwrap();
    assert!(
        matches!(
            parsed.pnpm_allow_builds().get("core-js"),
            Some(AllowBuildRaw::Bool(true))
        ),
        "approve-builds must heal on a non-pnpm-compat surface — the top-level \
         write is read back here. got: {:#?}",
        parsed.pnpm_allow_builds()
    );
}

/// Under nub identity (`read_manifest_root_config` on, pnpm surface off), the
/// reader DOES read the top-level key, so `set_allow_builds` writes it there —
/// no spurious pnpm-workspace.yaml emitted (which would be a brand leak on the
/// nub-identity surface).
#[test]
fn set_allow_builds_writes_root_key_under_nub_identity() {
    aube_util::set_embedder(&ROOT_TOOL);
    aube_util::update_engine_context(|ctx| {
        ctx.read_branded_pnpm_config = false;
        ctx.read_manifest_root_config = true;
    });

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("package.json"), "{\n  \"name\": \"x\"\n}\n").unwrap();

    let written = set_allow_builds(tmp.path(), &["core-js".to_string()], true).unwrap();

    assert_eq!(
        written.file_name().and_then(|n| n.to_str()),
        Some("package.json"),
        "nub identity reads the top-level key, so it must write package.json, got: {written:?}"
    );
    assert!(
        !tmp.path().join("pnpm-workspace.yaml").exists(),
        "must not emit a pnpm-workspace.yaml under nub identity"
    );
    let manifest = read_manifest(tmp.path());
    assert_eq!(
        manifest["allowBuilds"]["core-js"],
        serde_json::Value::Bool(true),
        "approval must land in the top-level allowBuilds the nub-identity reader consults"
    );
}
