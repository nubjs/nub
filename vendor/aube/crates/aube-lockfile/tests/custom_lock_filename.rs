//! Embedder-configured lockfile filename (via the active identity),
//! exercised end-to-end.
//!
//! Lives in its own integration-test binary (= its own process) because
//! the active identity is once-per-process: unit tests inside the crate
//! would race the default (`aube`) identity.

use aube_lockfile::{
    LockfileKind, aube_lock_filename, detect_existing_lockfile_kind, pnpm_lock_filename,
    write_lockfile_as,
};
use aube_util::Embedder;

static MYTOOL: Embedder = Embedder {
    name: "mytool",
    display_name: "mytool",
    vendor: None,
    version: "2.1.0",
    user_agent: "mytool/2.1.0",
    self_names: &["mytool"],
    compatible_names: &["pnpm"],
    lockfile_basename: "lock.yaml",
    workspace_yaml: Some("mytool-workspace.yaml"),
    manifest_namespace: "mytool",
    env_prefix: Some("MYTOOL"),
    config_env_prefix: Some("MYTOOL"),
    cache_namespace: "mytool",
    data_namespace: "mytool",
    canonical_lockfile_always_wins: true,
    runtime_switching: true,
    self_engines_check: true,
    self_update_enabled: true,
    warm_store_verify: true,
    no_churn_lockfile_write: false,
    read_branded_settings_env: true,
    primer_ttl: None,
};

#[test]
fn identity_lockfile_basename_drives_naming_detection_and_writes() {
    aube_util::set_embedder(&MYTOOL);

    assert_eq!(LockfileKind::Aube.filename(), "lock.yaml");

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("package.json"), r#"{"name":"t"}"#).unwrap();

    // Naming: branch lockfiles are off, so the resolved filename is the
    // configured basename; the pnpm mapping keeps pnpm's own name.
    assert_eq!(aube_lock_filename(dir.path()), "lock.yaml");
    assert_eq!(pnpm_lock_filename(dir.path()), "pnpm-lock.yaml");

    // Write path: the Aube kind lands at the configured name.
    let graph = aube_lockfile::LockfileGraph::default();
    let manifest = aube_manifest::PackageJson::default();
    let written = write_lockfile_as(dir.path(), &graph, &manifest, LockfileKind::Aube).unwrap();
    assert_eq!(written, dir.path().join("lock.yaml"));
    assert!(written.exists(), "lock.yaml must exist after write");
    assert!(
        !dir.path().join("aube-lock.yaml").exists(),
        "the default filename must not be written once the identity overrides it"
    );

    // Detection: the configured basename ranks top of the candidate
    // order — above pnpm-lock.yaml — when canonicalLockfileAlwaysWins
    // (the default) holds.
    std::fs::write(
        dir.path().join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\n",
    )
    .unwrap();
    assert_eq!(
        detect_existing_lockfile_kind(dir.path()),
        Some(LockfileKind::Aube),
        "configured lock.yaml must outrank pnpm-lock.yaml"
    );
}
