//! Integration test for the embedder-defaults tier.
//!
//! Lives in its own integration-test binary — i.e. its own process —
//! because `set_embedder_defaults` is once-per-process: registering
//! `nodeLinker` here would leak into the `values.rs` unit tests that
//! resolve the same setting from other sources.

use aube_settings::{ResolveCtx, embedder_defaults, resolved, set_embedder_defaults};
use std::collections::BTreeMap;

fn ctx<'a>(
    ws: &'a BTreeMap<String, yaml_serde::Value>,
    npmrc: &'a [(String, String)],
    env: &'a [(String, String)],
) -> ResolveCtx<'a> {
    ResolveCtx {
        project_aube_config: &[],
        project_npmrc: npmrc,
        user_aube_config: &[],
        user_npmrc: &[],
        workspace_yaml: ws,
        global_config_yaml: aube_settings::values::empty_yaml_map(),
        env,
        cli: &[],
        embedder_defaults: embedder_defaults(),
    }
}

#[test]
fn embedder_defaults_rank_below_every_user_source() {
    set_embedder_defaults(vec![("nodeLinker".to_string(), "hoisted".to_string())]);
    let ws = BTreeMap::new();

    // The default substitution applies when no user source speaks.
    let plain = ctx(&ws, &[], &[]);
    assert_eq!(
        resolved::node_linker(&plain),
        resolved::NodeLinker::Hoisted,
        "embedder default must replace the built-in default"
    );

    // The lowest-ranked *user* file source still beats it.
    let npmrc = vec![("node-linker".to_string(), "isolated".to_string())];
    let with_npmrc = ctx(&ws, &npmrc, &[]);
    assert_eq!(
        resolved::node_linker(&with_npmrc),
        resolved::NodeLinker::Isolated,
        ".npmrc must win over embedder defaults"
    );

    // So does the environment.
    let env = vec![("npm_config_node_linker".to_string(), "isolated".to_string())];
    let with_env = ctx(&ws, &[], &env);
    assert_eq!(
        resolved::node_linker(&with_env),
        resolved::NodeLinker::Isolated,
        "env must win over embedder defaults"
    );
}
