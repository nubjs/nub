//! Integration test for the embedder identity's lifecycle user-agent.
//!
//! Lives in its own integration-test binary — i.e. its own process —
//! because the active identity is once-per-process: registering an
//! identity here would leak into the `user_agent_tests` unit tests that
//! assert the default `aube/<version>` token.

use aube_util::Embedder;

static MYTOOL: Embedder = Embedder {
    name: "mytool",
    display_name: "mytool",
    vendor: None,
    version: "2.1.0",
    user_agent: "mytool/2.1.0",
    self_names: &["mytool"],
    compatible_names: &["pnpm"],
    lockfile_basename: "mytool-lock.yaml",
    lockfile_legacy_basenames: &[],
    workspace_yaml: Some("mytool-workspace.yaml"),
    manifest_namespace: "mytool",
    env_prefix: Some("MYTOOL"),
    config_env_prefix: Some("MYTOOL"),
    diag_env_prefix: Some("MYTOOL"),
    cache_namespace: "mytool",
    data_namespace: "mytool",
    virtual_store_subdir: "virtual-store",
    managed_config_system_dir: Some("mytool"),
    config_namespace: Some("mytool"),
    canonical_lockfile_always_wins: true,
    runtime_switching: true,
    self_engines_check: true,
    self_update_enabled: true,
    warm_store_verify: true,
    no_churn_lockfile_write: false,
    read_branded_settings_env: true,
    gvs_incompatible_warning: true,
    gvs_over_default_hoist: false,
    primer_ttl: None,
    cpu_budget: None,
    tty_progress: false,
    rich_update_picker: false,
    strict_unsupported_source: false,
    warm_trust_revalidate: true,
    trust_policy_ignore_after_default: None,
    extra_settings_fingerprint: None,
};

#[test]
fn registered_identity_replaces_the_default_token_and_keeps_the_platform_tail() {
    aube_util::set_embedder(&MYTOOL);
    let ua = aube_scripts::aube_user_agent();
    assert!(
        ua.starts_with("mytool/2.1.0 "),
        "identity user-agent must lead the UA, got: {ua}"
    );
    assert!(
        !ua.contains("aube/"),
        "default user-agent must be fully replaced, got: {ua}"
    );
    assert_eq!(
        ua.split_whitespace().count(),
        3,
        "platform/arch tail must survive the override, got: {ua}"
    );
}
