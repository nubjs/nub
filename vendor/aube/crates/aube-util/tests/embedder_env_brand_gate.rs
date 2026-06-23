//! Integration test for the env-prefix brand boundary under a nub-shaped
//! embedder — `env_prefix = None` (no branded debug-toggle family) but
//! `config_env_prefix = Some("NUB")` (the first-class config knobs read under
//! the host's own brand).
//!
//! Lives in its own integration-test binary (= its own process) because the
//! active `Embedder` is once-per-process (`set_embedder` is first-write-wins):
//! registering a non-default profile here would flip the fallback the other
//! crates' unit tests rely on. The default (AUBE) side — `embedder_env` /
//! `config_env` reading `AUBE_*` — is covered by `aube-util`'s lib unit test
//! `env::tests::embedder_and_config_env_read_aube_prefixed_under_default_profile`,
//! which runs under the default profile in a different binary.

use aube_util::Embedder;
use aube_util::env::{config_env, embedder_env};

/// A nub-shaped embedder: hides the branded debug-toggle family
/// (`env_prefix = None`) but owns the first-class config knobs under its own
/// brand (`config_env_prefix = Some("NUB")`).
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

/// Restore the previous value of an env var around a closure. Integration-test
/// binaries are single-test here, but be a good citizen anyway.
fn with_var<F: FnOnce()>(key: &str, value: &str, f: F) {
    let prev = std::env::var_os(key);
    // SAFETY: this binary runs one test, single-threaded.
    unsafe { std::env::set_var(key, value) };
    f();
    unsafe {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}

#[test]
fn nub_profile_hides_debug_toggles_and_reads_nub_config() {
    aube_util::set_embedder(&NUBLIKE);

    // Debug-toggle family (`env_prefix = None`): a branded `AUBE_*` toggle is
    // UNREADABLE under the nub-shaped profile — the brand never leaks. (And the
    // profile exposes no `NUBLIKE_*` debug family either, since `env_prefix` is
    // `None`, not `Some("NUB")`.)
    with_var("AUBE_DISABLE_CLONEDIR", "1", || {
        assert!(
            embedder_env("DISABLE_CLONEDIR").is_none(),
            "AUBE_DISABLE_CLONEDIR must be ignored under env_prefix = None"
        );
    });

    // First-class config (`config_env_prefix = Some(\"NUB\")`): the `NUB_*`
    // form is read, and the branded `AUBE_*` form is NOT — even when both are
    // set in the environment, only the host's own brand wins.
    with_var("NUB_CACHE_DIR", "/nub/cache", || {
        with_var("AUBE_CACHE_DIR", "/aube/cache", || {
            assert_eq!(
                config_env("CACHE_DIR").as_deref(),
                Some(std::ffi::OsStr::new("/nub/cache")),
                "config_env must read NUB_CACHE_DIR and ignore AUBE_CACHE_DIR under nub"
            );
        });
    });

    // The same first-class knob, with ONLY the branded `AUBE_*` form set, must
    // read nothing under nub — the AUBE brand is fully off nub's surface.
    with_var("AUBE_CONCURRENCY", "32", || {
        assert!(
            config_env("CONCURRENCY").is_none(),
            "AUBE_CONCURRENCY must be ignored under config_env_prefix = Some(\"NUB\")"
        );
    });
}
