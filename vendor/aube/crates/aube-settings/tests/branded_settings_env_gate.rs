//! Integration test for the `read_branded_settings_env` embedder toggle.
//!
//! Lives in its own integration-test binary (= its own process) because the
//! active [`Embedder`] is once-per-process (`set_embedder` is first-write-wins):
//! registering a non-default profile here would flip the fallback the other
//! crates' unit tests rely on. This binary registers an embedder that *hides*
//! its branded settings-env surface (`read_branded_settings_env = false`) and
//! checks that the branded `AUBE_*` settings var is ignored end-to-end through
//! settings resolution, while the neutral `NPM_CONFIG_*` alias still wins.
//!
//! The default (ON) side of the toggle — standalone aube reading every
//! `AUBE_*` settings var — is covered by `aube-util`'s
//! `env::tests::aube_profile_honors_every_settings_env_family`, which runs
//! under the default `AUBE` profile in a different binary.

use aube_settings::{ResolveCtx, resolved};
use aube_util::Embedder;
use std::collections::BTreeMap;

/// A `mytool` embedder that keeps a branded env prefix for identity but turns
/// the branded settings-env family OFF — the nub posture for Change 2.
static MYTOOL_NO_BRANDED_ENV: Embedder = Embedder {
    name: "mytool",
    display_name: "mytool",
    vendor: None,
    version: "1.0.0",
    user_agent: "mytool/1.0.0",
    self_names: &["mytool"],
    compatible_names: &["pnpm"],
    lockfile_basename: "lock.yaml",
    workspace_yaml: Some("mytool-workspace.yaml"),
    manifest_namespace: "mytool",
    // Branded prefix is *still set* — proving the toggle is independent of
    // `env_prefix`. Even AUBE_* (aube's own brand) is gated off, let alone a
    // MYTOOL_* var: with the family disabled, no branded settings var is read.
    env_prefix: Some("MYTOOL"),
    config_env_prefix: Some("MYTOOL"),
    cache_namespace: "mytool",
    data_namespace: "mytool",
    canonical_lockfile_always_wins: true,
    runtime_switching: true,
    self_engines_check: true,
    self_update_enabled: true,
    warm_store_verify: true,
    read_branded_settings_env: false,
    no_churn_lockfile_write: false,
    primer_ttl: None,
};

fn ctx<'a>(
    ws: &'a BTreeMap<String, yaml_serde::Value>,
    env: &'a [(String, String)],
) -> ResolveCtx<'a> {
    ResolveCtx {
        project_aube_config: &[],
        project_npmrc: &[],
        user_aube_config: &[],
        user_npmrc: &[],
        workspace_yaml: ws,
        global_config_yaml: aube_settings::values::empty_yaml_map(),
        env,
        cli: &[],
        embedder_defaults: &[],
    }
}

#[test]
fn branded_settings_env_off_ignores_branded_var_but_honors_neutral() {
    aube_util::set_embedder(&MYTOOL_NO_BRANDED_ENV);
    let ws = BTreeMap::new();

    // Branded `AUBE_STORE_DIR` alone: with the branded settings-env family
    // turned off, it is not read, so the setting falls through to its built-in
    // default rather than the env value.
    let branded_only = vec![("AUBE_STORE_DIR".to_string(), "/from-branded".to_string())];
    assert_ne!(
        resolved::store_dir(&ctx(&ws, &branded_only)),
        Some("/from-branded".to_string()),
        "branded AUBE_STORE_DIR must be ignored when read_branded_settings_env=false"
    );

    // The neutral npm-compat alias is never the tool's brand — it is honored
    // regardless of the toggle.
    let neutral = vec![(
        "NPM_CONFIG_STORE_DIR".to_string(),
        "/from-neutral".to_string(),
    )];
    assert_eq!(
        resolved::store_dir(&ctx(&ws, &neutral)),
        Some("/from-neutral".to_string()),
        "neutral NPM_CONFIG_STORE_DIR must still be honored"
    );
}
