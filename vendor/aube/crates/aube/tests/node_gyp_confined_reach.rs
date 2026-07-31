//! An embedder that confines dependency lifecycle scripts resolves the
//! CONFINED node-gyp reach, so the bootstrap runs instead of deferring to
//! whatever `node-gyp` happens to sit on the host's `PATH`.
//!
//! The confined script cannot read an arbitrary ambient `PATH` entry, so an
//! ambient hit that suppressed the bootstrap would leave it with no node-gyp
//! at all. The unconfined default is pinned as a unit test next to the
//! implementation; this file covers the opted-in half, which needs a
//! registered profile.
//!
//! Single test on purpose: `set_embedder` is a first-write-wins `OnceLock`, so
//! a sibling here could not re-point it. Cargo gives this file its own
//! process, which is the whole isolation.

use aube::commands::install::node_gyp_bootstrap::ScriptReach;
use aube_util::Embedder;

static CONFINING: Embedder = Embedder {
    name: "confiner",
    display_name: "confiner",
    vendor: None,
    version: "0.0.0",
    user_agent: "confiner/0.0.0",
    self_names: &["confiner"],
    compatible_names: &[],
    lockfile_basename: "confiner-lock.yaml",
    lockfile_legacy_basenames: &[],
    workspace_yaml: None,
    manifest_namespace: "",
    env_prefix: None,
    config_env_prefix: None,
    diag_env_prefix: None,
    internal_env_prefix: "AUBE",
    cache_namespace: "confiner",
    data_namespace: "confiner",
    virtual_store_subdir: "virtual-store",
    managed_config_system_dir: None,
    config_namespace: None,
    canonical_lockfile_always_wins: false,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
    warm_store_verify: false,
    no_churn_lockfile_write: true,
    read_branded_settings_env: false,
    gvs_incompatible_warning: false,
    gvs_over_default_hoist: true,
    primer_ttl: None,
    cpu_budget: None,
    tty_progress: false,
    rich_update_picker: false,
    strict_unsupported_source: false,
    warm_trust_revalidate: false,
    trust_policy_ignore_after_default: None,
    extra_settings_fingerprint: None,
    embedder_owns_lifecycle_sandbox: true,
};

#[test]
fn embedder_owned_lifecycle_confinement_takes_the_bootstrapped_node_gyp() {
    aube_util::set_embedder(&CONFINING);
    assert_eq!(
        ScriptReach::for_active_embedder(),
        ScriptReach::Confined,
        "a confining embedder must not defer to an ambient node-gyp its scripts cannot read"
    );
}
