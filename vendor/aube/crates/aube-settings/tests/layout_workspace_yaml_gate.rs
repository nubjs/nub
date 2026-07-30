//! Integration test for the `read_layout_from_workspace_yaml` engine-context
//! posture.
//!
//! Its own integration-test binary (= its own process) because the posture is
//! process-global: flipping it inside the crate's unit-test binary would leak
//! into every sibling test running in parallel. Both directions live in ONE
//! test function for the same reason — tests within a binary share the context.

use aube_settings::{ResolveCtx, resolved};
use std::collections::BTreeMap;

fn yaml(pairs: &[(&str, &str)]) -> BTreeMap<String, yaml_serde::Value> {
    pairs
        .iter()
        .map(|(k, v)| {
            (
                (*k).to_string(),
                yaml_serde::Value::String((*v).to_string()),
            )
        })
        .collect()
}

fn ctx<'a>(
    ws: &'a BTreeMap<String, yaml_serde::Value>,
    npmrc: &'a [(String, String)],
) -> ResolveCtx<'a> {
    ResolveCtx {
        managed_aube_config: &[],
        project_aube_config: &[],
        project_npmrc: npmrc,
        project_config: &[],
        user_aube_config: &[],
        user_npmrc: &[],
        workspace_yaml: ws,
        global_config_yaml: aube_settings::values::empty_yaml_map(),
        env: &[],
        cli: &[],
        embedder_defaults: &[],
    }
}

#[test]
fn the_posture_drops_only_layout_and_only_from_workspace_yaml() {
    // Three layout settings, one per helper type (string / bool / list), plus
    // `autoInstallPeers` — resolution config declaring the same source, so a
    // divergence between them isolates the axis rather than the file.
    let ws = yaml(&[
        ("modulesDir", "deps"),
        ("shamefullyHoist", "true"),
        ("publicHoistPattern", "@types/*"),
        ("autoInstallPeers", "false"),
    ]);
    let npmrc = [("modulesDir".to_string(), "deps".to_string())];
    let no_npmrc: [(String, String); 0] = [];

    // Default posture (standalone aube): every declared source applies to every
    // setting, exactly as upstream.
    assert_eq!(resolved::modules_dir(&ctx(&ws, &no_npmrc)), "deps");
    assert!(resolved::shamefully_hoist(&ctx(&ws, &no_npmrc)));
    assert_eq!(
        resolved::public_hoist_pattern(&ctx(&ws, &no_npmrc)),
        vec!["@types/*".to_string()]
    );

    aube_util::update_engine_context(|c| c.read_layout_from_workspace_yaml = false);

    assert_eq!(
        resolved::modules_dir(&ctx(&ws, &no_npmrc)),
        "node_modules",
        "a workspace-YAML layout key no longer resolves — the built-in default stands"
    );
    assert!(
        !resolved::shamefully_hoist(&ctx(&ws, &no_npmrc)),
        "same for the bool helper"
    );
    assert!(
        resolved::public_hoist_pattern(&ctx(&ws, &no_npmrc)).is_empty(),
        "same for the list helper"
    );
    assert!(
        !resolved::auto_install_peers(&ctx(&ws, &no_npmrc)),
        "resolution config in the same file is untouched"
    );
    assert_eq!(
        resolved::modules_dir(&ctx(&ws, &npmrc)),
        "deps",
        "the neutral .npmrc surface still sets the same layout key"
    );

    aube_util::update_engine_context(|c| c.read_layout_from_workspace_yaml = true);
}
