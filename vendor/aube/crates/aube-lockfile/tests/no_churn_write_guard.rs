//! Embedder no-churn lockfile write guard, exercised end-to-end.
//!
//! Lives in its own integration-test binary (= its own process) because
//! the active embedder identity is once-per-process: a unit test inside
//! the crate would race the default (`aube`) identity, whose default for
//! `no_churn_lockfile_write` is `false` (upstream — always write).
//!
//! Contract under test: with an embedder that opts the guard ON, a
//! second write of a graph whose resolved identity equals the on-disk
//! lockfile's leaves the file untouched (same mtime), while a write of a
//! genuinely-changed graph rewrites it.

use std::collections::BTreeMap;

use aube_lockfile::{
    DepType, DirectDep, LockedPackage, LockfileGraph, LockfileKind, write_lockfile_as,
};
use aube_manifest::PackageJson;
use aube_util::Embedder;

// Same as AUBE except the no-churn guard is ON. (Distinct lockfile
// basename keeps the debug-assert in `set_embedder` happy and avoids any
// chance of aliasing a foreign name.)
static NO_CHURN_TOOL: Embedder = Embedder {
    name: "nochurn",
    display_name: "nochurn",
    vendor: None,
    version: "1.0.0",
    user_agent: "nochurn/1.0.0",
    self_names: &["nochurn"],
    compatible_names: &["pnpm"],
    lockfile_basename: "nochurn-lock.yaml",
    workspace_yaml: Some("nochurn-workspace.yaml"),
    manifest_namespace: "nochurn",
    env_prefix: Some("NOCHURN"),
    config_env_prefix: Some("NOCHURN"),
    cache_namespace: "nochurn",
    data_namespace: "nochurn",
    canonical_lockfile_always_wins: true,
    runtime_switching: true,
    self_engines_check: true,
    self_update_enabled: true,
    warm_store_verify: true,
    no_churn_lockfile_write: true,
    read_branded_settings_env: true,
    primer_ttl: None,
};

fn pkg(name: &str, version: &str, integrity: &str) -> LockedPackage {
    LockedPackage {
        name: name.to_string(),
        version: version.to_string(),
        integrity: Some(integrity.to_string()),
        dep_path: format!("{name}@{version}"),
        ..Default::default()
    }
}

fn graph_with(packages: Vec<LockedPackage>) -> LockfileGraph {
    let mut pkg_map = BTreeMap::new();
    for p in &packages {
        pkg_map.insert(p.dep_path.clone(), p.clone());
    }
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        packages
            .iter()
            .map(|p| DirectDep {
                name: p.name.clone(),
                dep_path: p.dep_path.clone(),
                dep_type: DepType::Production,
                specifier: Some(format!("^{}", p.version)),
            })
            .collect(),
    );
    LockfileGraph {
        importers,
        packages: pkg_map,
        ..Default::default()
    }
}

fn mtime(path: &std::path::Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

#[test]
fn guard_skips_rewrite_when_graph_unchanged_and_writes_when_changed() {
    aube_util::set_embedder(&NO_CHURN_TOOL);
    assert!(
        aube_util::embedder().no_churn_lockfile_write,
        "this test binary must run under the no-churn embedder"
    );

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("package.json"), r#"{"name":"t"}"#).unwrap();
    let manifest = PackageJson::default();

    let graph = graph_with(vec![pkg("foo", "1.0.0", "sha512-AAA==")]);

    // First write creates the lockfile.
    let path = write_lockfile_as(dir.path(), &graph, &manifest, LockfileKind::Pnpm).unwrap();
    assert!(path.exists(), "first write must create the lockfile");
    let first = mtime(&path);

    // mtime resolution on some filesystems is coarse; sleep so a real
    // rewrite would be observable as a distinct timestamp.
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Second write of the SAME graph must be skipped — the resolved
    // graph identity equals what's on disk, so the file is untouched.
    let path2 = write_lockfile_as(dir.path(), &graph, &manifest, LockfileKind::Pnpm).unwrap();
    assert_eq!(path2, path, "skipped write still reports the target path");
    assert_eq!(
        mtime(&path),
        first,
        "no-churn guard must not rewrite a graph-equal lockfile"
    );

    std::thread::sleep(std::time::Duration::from_millis(20));

    // A genuinely changed graph (new package + new integrity) must be
    // written — the guard only suppresses no-ops.
    let changed = graph_with(vec![
        pkg("foo", "1.0.0", "sha512-AAA=="),
        pkg("bar", "2.0.0", "sha512-BBB=="),
    ]);
    write_lockfile_as(dir.path(), &changed, &manifest, LockfileKind::Pnpm).unwrap();
    assert!(
        mtime(&path) > first,
        "a changed graph must rewrite the lockfile"
    );
}

/// Regression: adding `patchedDependencies` over an otherwise-identical
/// package set must NOT be treated as a no-op. The guard hashes each
/// graph's own patch fingerprints into its identity, so a freshly-patched
/// graph never collapses onto the unpatched lockfile already on disk.
/// Without this, `patch-commit`'s re-install silently skips the rewrite,
/// the lockfile never records `patchedDependencies` + `(patch_hash=…)`,
/// and real pnpm rejects the frozen install with
/// ERR_PNPM_LOCKFILE_CONFIG_MISMATCH (and aube frozen-fails its own lock).
#[test]
fn guard_rewrites_when_only_patch_config_is_added() {
    aube_util::set_embedder(&NO_CHURN_TOOL);
    assert!(aube_util::embedder().no_churn_lockfile_write);

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("package.json"), r#"{"name":"t"}"#).unwrap();
    let manifest = PackageJson::default();

    // Unpatched graph hits disk first.
    let graph = graph_with(vec![pkg("ms", "2.1.3", "sha512-AAA==")]);
    let path = write_lockfile_as(dir.path(), &graph, &manifest, LockfileKind::Pnpm).unwrap();
    let first = mtime(&path);

    std::thread::sleep(std::time::Duration::from_millis(20));

    // Same package set, but now the project declares a patch for it.
    let mut patched = graph.clone();
    patched
        .patched_dependencies
        .insert("ms@2.1.3".to_string(), "patches/ms@2.1.3.patch".to_string());
    patched.patched_dependency_hashes.insert(
        "ms@2.1.3".to_string(),
        "82ff0b4d1c20272cdb11684045f28947472d5b8a10a04c0d972102d14815e536".to_string(),
    );
    write_lockfile_as(dir.path(), &patched, &manifest, LockfileKind::Pnpm).unwrap();
    assert!(
        mtime(&path) > first,
        "adding patchedDependencies must rewrite the lockfile, not be skipped as a no-op"
    );

    // And the written lockfile actually carries the patch block + suffix —
    // the records real pnpm reads to apply the patch under --frozen.
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains("patchedDependencies:"),
        "rewritten lockfile must record the patchedDependencies block:\n{written}"
    );
    assert!(
        written.contains(
            "(patch_hash=82ff0b4d1c20272cdb11684045f28947472d5b8a10a04c0d972102d14815e536)"
        ),
        "rewritten lockfile must stamp the (patch_hash=…) suffix:\n{written}"
    );

    std::thread::sleep(std::time::Duration::from_millis(20));
    let after_patch = mtime(&path);

    // Re-writing the now-on-disk patched lockfile back (parse → write of
    // the same file's graph) is a no-op: the existing file already carries
    // the matching patch hash, so the two identities agree and the guard
    // suppresses the rewrite. (Parsing first is what the install pipeline
    // does — a hand-built graph isn't byte-faithful to a parsed one.)
    let reparsed = aube_lockfile::parse_lockfile_with_kind(dir.path(), &manifest)
        .unwrap()
        .0;
    write_lockfile_as(dir.path(), &reparsed, &manifest, LockfileKind::Pnpm).unwrap();
    assert_eq!(
        mtime(&path),
        after_patch,
        "an unchanged patched lockfile must stay a no-op (zero churn)"
    );
}
