//! The lockfile shape pnpmfile hooks see.
//!
//! pnpm hands `preResolution` its **in-memory** `LockfileObject`, which
//! is not the on-disk v9 file: `packages:` and `snapshots:` are merged
//! into one map keyed by the snapshot's dep path, and each importer's
//! inline `{specifier, version}` pairs are split back into a flat
//! `dependencies` map plus a sibling `specifiers` map
//! (`convertToLockfileObject` / `revertProjectSnapshot` in
//! `@pnpm/lockfile.fs`). A hook written against pnpm reads
//! `lockfile.packages[key].resolution` and
//! `lockfile.importers['.'].specifiers`, so anything else is a shape it
//! cannot use.
//!
//! Both constructors below produce that object. They go through the
//! writer's own projection ([`super::write::build`]) rather than a
//! second walk of the graph, so the hook sees the same dep-path
//! translation, patch-hash suffixes and alias recovery that land in
//! `pnpm-lock.yaml` — a hook that rewrites a resolution URL is looking
//! at the URL the lockfile will actually carry.

use aube_manifest::PackageJson;
use serde_json::{Map, Value};

use crate::{Error, LockfileGraph, LockfileSettings};

/// pnpm's own `LOCKFILE_VERSION` for the v9 schema, mirrored here so the
/// empty object below advertises the same version the writer stamps.
const LOCKFILE_VERSION: &str = "9.0";

/// Project `graph` onto the object pnpm passes a pnpmfile hook.
///
/// `lockfile_dir` is the project root: the view is always rendered with
/// pnpm's native encodings (aliases included) because its only consumer
/// is a pnpm-compatible hook, whatever lockfile format the project
/// itself is on.
pub fn lockfile_object(
    lockfile_dir: &std::path::Path,
    graph: &LockfileGraph,
    manifest: &PackageJson,
) -> Result<Value, Error> {
    let writable = super::write::build(&lockfile_dir.join("pnpm-lock.yaml"), graph, manifest)?;
    let mut value = serde_json::to_value(&writable)
        .map_err(|e| Error::parse(lockfile_dir, format!("failed to project lockfile: {e}")))?;
    let Some(root) = value.as_object_mut() else {
        return Ok(value);
    };
    merge_snapshots_into_packages(root);
    flatten_patched_dependencies(root);
    revert_importers(root);
    Ok(value)
}

/// The object pnpm synthesizes when there is no lockfile on disk
/// (`createLockfileObject`): a version, the resolved settings, and one
/// empty entry per importer. Deliberately carries **no** `packages`
/// key — pnpm only sets that field when a lockfile was read, and a hook
/// that guards on `if (!lockfile.packages) return` depends on the
/// difference.
///
/// An empty `importer_ids` falls back to the root importer alone,
/// which is what a caller with no workspace list to hand over means.
pub fn empty_lockfile_object(importer_ids: &[String], settings: &LockfileSettings) -> Value {
    let root = [".".to_string()];
    let importer_ids = if importer_ids.is_empty() {
        &root[..]
    } else {
        importer_ids
    };
    let mut importers = Map::new();
    for id in importer_ids {
        importers.insert(
            id.clone(),
            Value::Object(Map::from_iter([
                ("dependencies".to_string(), Value::Object(Map::new())),
                ("specifiers".to_string(), Value::Object(Map::new())),
            ])),
        );
    }
    let mut settings_map = Map::new();
    settings_map.insert(
        "autoInstallPeers".to_string(),
        Value::Bool(settings.auto_install_peers),
    );
    settings_map.insert(
        "excludeLinksFromLockfile".to_string(),
        Value::Bool(settings.exclude_links_from_lockfile),
    );
    Value::Object(Map::from_iter([
        (
            "lockfileVersion".to_string(),
            Value::String(LOCKFILE_VERSION.to_string()),
        ),
        ("settings".to_string(), Value::Object(settings_map)),
        ("importers".to_string(), Value::Object(importers)),
    ]))
}

/// Collapse the v9 `packages:`/`snapshots:` split the way pnpm's reader
/// does: iterate the *snapshots*, key the merged entry by the snapshot's
/// dep path (peer and patch suffixes intact), and let the `packages:`
/// entry for the suffix-less id win on any field both carry. A
/// `packages:` entry no snapshot references drops out, exactly as in
/// pnpm.
fn merge_snapshots_into_packages(root: &mut Map<String, Value>) {
    let snapshots = match root.remove("snapshots") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let packages = match root.remove("packages") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let mut merged = Map::new();
    for (dep_path, snapshot) in snapshots {
        let mut entry = match snapshot {
            Value::Object(m) => m,
            other => {
                merged.insert(dep_path, other);
                continue;
            }
        };
        if let Some(Value::Object(info)) = packages.get(remove_suffix(&dep_path)) {
            for (key, value) in info {
                entry.insert(key.clone(), value.clone());
            }
        }
        merged.insert(dep_path, Value::Object(entry));
    }
    root.insert("packages".to_string(), Value::Object(merged));
}

/// pnpm's `migratePatchedDependencies`: the in-memory map is always
/// `selector -> string`, where a v10 `{hash, path}` entry collapses to
/// its hash and a bare path string passes through.
fn flatten_patched_dependencies(root: &mut Map<String, Value>) {
    let Some(Value::Object(patched)) = root.get_mut("patchedDependencies") else {
        return;
    };
    for value in patched.values_mut() {
        if let Value::Object(entry) = value {
            let flattened = entry
                .get("hash")
                .or_else(|| entry.get("path"))
                .cloned()
                .unwrap_or(Value::Null);
            *value = flattened;
        }
    }
}

/// pnpm's `revertProjectSnapshot`: hoist every dependency block's
/// inline `specifier` into one `specifiers` map on the importer and
/// leave the block itself as a flat `name -> version` map. `specifiers`
/// is always present, even when empty, because pnpm's reader always
/// sets it.
///
/// `skippedOptionalDependencies` is left out on purpose: it is aube's
/// own drift-detection record, and its `version` is the sentinel
/// `0.0.0` rather than an installed version, so flattening it into
/// pnpm's `name -> version` shape would state something untrue.
fn revert_importers(root: &mut Map<String, Value>) {
    const BLOCKS: [&str; 3] = ["dependencies", "devDependencies", "optionalDependencies"];
    let Some(Value::Object(importers)) = root.get_mut("importers") else {
        return;
    };
    for importer in importers.values_mut() {
        let Value::Object(importer) = importer else {
            continue;
        };
        importer.remove("skippedOptionalDependencies");
        let mut specifiers = Map::new();
        for block in BLOCKS {
            let Some(Value::Object(deps)) = importer.get(block) else {
                continue;
            };
            let mut flat = Map::new();
            for (name, spec) in deps {
                let Value::Object(spec) = spec else {
                    continue;
                };
                if let Some(specifier) = spec.get("specifier") {
                    specifiers.insert(name.clone(), specifier.clone());
                }
                flat.insert(
                    name.clone(),
                    spec.get("version").cloned().unwrap_or(Value::Null),
                );
            }
            importer.insert(block.to_string(), Value::Object(flat));
        }
        importer.insert("specifiers".to_string(), Value::Object(specifiers));
    }
}

/// pnpm's `removeSuffix`: a dep path's identity ends at the first
/// top-level `(`, whether that opens a `(patch_hash=…)` marker or a
/// peer-suffix segment.
fn remove_suffix(dep_path: &str) -> &str {
    match dep_path.find('(') {
        Some(i) => &dep_path[..i],
        None => dep_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_suffix_trims_peer_and_patch_segments() {
        assert_eq!(remove_suffix("react-dom@18.2.0"), "react-dom@18.2.0");
        assert_eq!(
            remove_suffix("react-dom@18.2.0(react@18.2.0)"),
            "react-dom@18.2.0"
        );
        assert_eq!(
            remove_suffix("lodash@4.17.21(patch_hash=abc)"),
            "lodash@4.17.21"
        );
    }

    #[test]
    fn empty_object_has_no_packages_key() {
        let value = empty_lockfile_object(
            &[".".to_string()],
            &LockfileSettings {
                auto_install_peers: true,
                ..Default::default()
            },
        );
        let root = value.as_object().unwrap();
        assert!(
            !root.contains_key("packages"),
            "pnpm's createLockfileObject sets no packages key; hooks guard on that"
        );
        assert_eq!(root["lockfileVersion"], Value::String("9.0".into()));
        assert_eq!(root["settings"]["autoInstallPeers"], Value::Bool(true));
        assert_eq!(root["importers"]["."]["specifiers"], Value::Object(Map::new()));
        assert_eq!(
            root["importers"]["."]["dependencies"],
            Value::Object(Map::new())
        );
    }

    #[test]
    fn packages_carry_resolution_and_snapshot_dependencies() {
        let mut graph = LockfileGraph::default();
        let mut pkg = crate::LockedPackage {
            name: "is-obj".into(),
            version: "3.0.0".into(),
            integrity: Some("sha512-deadbeef".into()),
            dep_path: "is-obj@3.0.0".into(),
            ..Default::default()
        };
        pkg.dependencies.insert("tslib".into(), "2.6.2".into());
        pkg.engines.insert("node".into(), ">=12".into());
        graph.packages.insert("is-obj@3.0.0".into(), pkg);
        graph.packages.insert(
            "tslib@2.6.2".into(),
            crate::LockedPackage {
                name: "tslib".into(),
                version: "2.6.2".into(),
                integrity: Some("sha512-tslib".into()),
                dep_path: "tslib@2.6.2".into(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".into(),
            vec![crate::DirectDep {
                name: "is-obj".into(),
                dep_path: "is-obj@3.0.0".into(),
                dep_type: crate::DepType::Production,
                specifier: Some("^3.0.0".into()),
            }],
        );

        let dir = std::env::temp_dir().join("aube-hook-view-test");
        let value =
            lockfile_object(&dir, &graph, &PackageJson::default()).expect("projection succeeds");

        let entry = &value["packages"]["is-obj@3.0.0"];
        assert_eq!(
            entry["resolution"]["integrity"],
            Value::String("sha512-deadbeef".into()),
            "a pnpm hook reads packages[key].resolution; it must be present"
        );
        assert_eq!(entry["engines"]["node"], Value::String(">=12".into()));
        assert_eq!(
            entry["dependencies"]["tslib"],
            Value::String("2.6.2".into()),
            "the snapshot's dependency map merges into the same entry"
        );
        assert!(
            value.get("snapshots").is_none(),
            "pnpm's in-memory object has no snapshots key"
        );

        let importer = &value["importers"]["."];
        assert_eq!(
            importer["dependencies"]["is-obj"],
            Value::String("3.0.0".into()),
            "the inline {{specifier, version}} pair must be flattened to the version"
        );
        assert_eq!(
            importer["specifiers"]["is-obj"],
            Value::String("^3.0.0".into())
        );
    }
}
