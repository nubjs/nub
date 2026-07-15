//! The `hoist_explicit` companion accessor (`explicitAccessor = true` in
//! settings.toml) distinguishes an EXPLICITLY-configured `hoist` from the
//! built-in default. The `gvs_over_default_hoist` layout needs that: an
//! explicit `hoist=true` vetoes the shared virtual store, a defaulted one does
//! not.

use aube_settings::{ResolveCtx, resolved};
use std::collections::BTreeMap;

fn ctx<'a>(
    npmrc: &'a [(String, String)],
    env: &'a [(String, String)],
    cli: &'a [(String, String)],
    embedder_defaults: &'a [(String, String)],
    ws: &'a BTreeMap<String, yaml_serde::Value>,
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
        env,
        cli,
        embedder_defaults,
    }
}

#[test]
fn unset_is_none_while_normal_accessor_returns_the_default() {
    let ws = BTreeMap::new();
    let c = ctx(&[], &[], &[], &[], &ws);
    assert_eq!(
        resolved::hoist_explicit(&c),
        None,
        "no source set hoist ⇒ explicit is None"
    );
    assert!(
        resolved::hoist(&c),
        "the normal accessor still folds in the built-in default (true)"
    );
}

#[test]
fn explicit_false_and_true_are_reported_from_every_tier() {
    let ws = BTreeMap::new();

    let npmrc = vec![("hoist".to_string(), "false".to_string())];
    assert_eq!(
        resolved::hoist_explicit(&ctx(&npmrc, &[], &[], &[], &ws)),
        Some(false),
        ".npmrc hoist=false ⇒ explicit Some(false)"
    );

    let env = vec![("npm_config_hoist".to_string(), "true".to_string())];
    assert_eq!(
        resolved::hoist_explicit(&ctx(&[], &env, &[], &[], &ws)),
        Some(true),
        "env ⇒ explicit Some(true)"
    );

    // The embedder-defaults tier COUNTS as explicit — this is what lets nub's
    // injected-deps `hoist=true` push veto GVS.
    let embedder = vec![("hoist".to_string(), "true".to_string())];
    assert_eq!(
        resolved::hoist_explicit(&ctx(&[], &[], &[], &embedder, &ws)),
        Some(true),
        "embedderDefaults hoist=true ⇒ explicit Some(true)"
    );
}
