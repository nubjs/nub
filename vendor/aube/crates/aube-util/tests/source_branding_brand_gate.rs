//! Integration test for the source-branding helpers (`prog` / `cmd`) under a
//! non-aube embedder. These are the helpers jdx approved over post-processing
//! rendered output: a user-facing string composes the program name at the
//! source, so a library consumer (nub) gets its own brand in errors/banners
//! without any post-pass.
//!
//! Lives in its own integration-test binary (= its own process) because the
//! active `Embedder` is once-per-process (`set_embedder` is first-write-wins):
//! registering a non-default profile here would flip the fallback the other
//! crates' unit tests rely on. The default (AUBE) side — `prog()` == `"aube"`,
//! `cmd("install")` == `"aube install"` — is covered by `aube-util`'s lib unit
//! test `identity::tests::prog_and_cmd_render_aube_under_default_profile`,
//! which runs under the default profile in a different binary.

use aube_util::{Embedder, cmd, prog};

/// A nub-shaped embedder: its own brand name flows through the source-branding
/// helpers.
static NUBLIKE: Embedder = Embedder {
    name: "nublike",
    display_name: "nublike",
    vendor: None,
    version: "1.0.0",
    user_agent: "nublike/1.0.0",
    self_names: &["nublike"],
    compatible_names: &["pnpm"],
    lockfile_basename: "lock.yaml",
    workspace_yaml: None,
    manifest_namespace: "",
    env_prefix: None,
    config_env_prefix: Some("NUB"),
    cache_namespace: "nublike",
    data_namespace: "nublike",
    canonical_lockfile_always_wins: false,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
    warm_store_verify: false,
    read_branded_settings_env: false,
    no_churn_lockfile_write: true,
    primer_ttl: None,
};

#[test]
fn prog_and_cmd_follow_the_embedder_brand() {
    aube_util::set_embedder(&NUBLIKE);

    // The bare program name is the host's brand, not "aube".
    assert_eq!(prog(), "nublike");

    // A command reference brands the program prefix and leaves the verb verbatim
    // — so a user under the nub-shaped embedder is told to run the host command,
    // never `aube install`.
    assert_eq!(cmd("install"), "nublike install");
    assert_eq!(cmd("patch-commit"), "nublike patch-commit");
    assert_eq!(cmd("store prune"), "nublike store prune");

    // No "aube" leaks through the composed command reference.
    assert!(
        !cmd("install").contains("aube"),
        "a user-facing command reference must carry the host brand, not aube"
    );
}
