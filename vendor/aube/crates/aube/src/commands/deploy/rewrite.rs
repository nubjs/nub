//! Manifest-rewrite helpers for `aube deploy`.
//!
//! After [`super::injection`] stages every bundled local-ref target under
//! `<target>/.aube-deploy-injected/<id>/`, the rewriter walks each
//! affected manifest (deployed top-level + every bundled sibling) and
//! turns `workspace:` / `file:` / `link:` / `portal:` / `catalog:`
//! specifiers into either:
//!
//!   * a relative `file:` pointer at the staged copy (regular deps that
//!     end up in the bundling plan), or
//!   * a concrete semver range (peer-dep `workspace:` specs that aren't
//!     bundled), or
//!   * a `file:` pointer at the deploy root (back-refs from a bundled
//!     sibling to the deployed package — handled here so the deployed
//!     package stays a singleton).
//!
//! `peerDependencies` is mostly left alone: peers are satisfied by the
//! consumer's installed tree, not bundled. The exception is a peer
//! `workspace:` ref that's not in the plan — we resolve it to a concrete
//! range so the deploy target (no workspace yaml) can still parse it.
use super::filtering::StripFields;
use super::injection::{Injection, InjectionPlan};
use crate::commands::CatalogMap;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where the deployed package lives in the source workspace (canonical
/// path) and the deploy target root. Used by the rewriter to recognize
/// back-refs to the deployed package and emit a `file:` pointer back at
/// the target instead of bundling a duplicate copy.
#[derive(Debug, Clone, Copy)]
pub(super) struct DeployRoot<'a> {
    pub(super) deployed_canonical: &'a Path,
    pub(super) target_root: &'a Path,
}

/// Translate a `workspace:` peer spec into a concrete semver range using
/// the sibling's pinned version. `workspace:*` / `workspace:` collapse to
/// the exact version; `^`/`~` keep their operator; any other suffix is
/// already a valid range and used verbatim. Used only for peer-dep
/// rewrites — regular deps go through bundling and become `file:` refs.
fn resolve_workspace_spec(spec: &str, concrete_version: &str) -> String {
    let suffix = spec.strip_prefix("workspace:").unwrap_or(spec);
    match suffix {
        "" | "*" => concrete_version.to_string(),
        "^" => format!("^{concrete_version}"),
        "~" => format!("~{concrete_version}"),
        other => other.to_string(),
    }
}

/// Look up `spec` (a `catalog:` / `catalog:<name>` reference) in the
/// source workspace's catalog map and return the concrete range. Mirrors
/// the resolver's [`resolve_catalog_spec`](aube_resolver) precedence:
/// bare `catalog:` maps to `default`; unknown catalog or missing entry
/// is a hard error; a catalog value that itself is another `catalog:`
/// ref errors (catalogs cannot chain).
fn resolve_catalog_for_rewrite(
    catalogs: &CatalogMap,
    pkg_name: &str,
    spec: &str,
    manifest_path: &Path,
) -> miette::Result<String> {
    let catalog_name = spec
        .strip_prefix("catalog:")
        .map(|n| if n.is_empty() { "default" } else { n })
        .ok_or_else(|| {
            miette!(
                "{}: internal error — resolve_catalog_for_rewrite called on non-catalog spec {spec:?}",
                aube_util::cmd("deploy")
            )
        })?;
    let Some(catalog) = catalogs.get(catalog_name) else {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_UNKNOWN_CATALOG,
            help = "define the catalog in `pnpm-workspace.yaml` or under `pnpm.catalog` / `workspaces.catalog` in `package.json`",
            "{}: {} declares `{pkg_name}: {spec}` but catalog `{catalog_name}` is not defined in the source workspace",
            aube_util::cmd("deploy"),
            manifest_path.display(),
        ));
    };
    let Some(value) = catalog.get(pkg_name) else {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_UNKNOWN_CATALOG_ENTRY,
            "{}: {} declares `{pkg_name}: {spec}` but catalog `{catalog_name}` has no entry for {pkg_name:?}",
            aube_util::cmd("deploy"),
            manifest_path.display(),
        ));
    };
    if aube_util::pkg::is_catalog_spec(value) {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_UNKNOWN_CATALOG_ENTRY,
            "{}: catalog `{catalog_name}` entry for {pkg_name:?} is itself a catalog reference ({value:?}); catalogs cannot chain",
            aube_util::cmd("deploy"),
        ));
    }
    Ok(value.clone())
}

/// Rewrite the `dependencies` / `devDependencies` / `optionalDependencies`
/// fields of `manifest_path` so every `workspace:` / `file:` / `link:`
/// specifier becomes a relative `file:` pointer at the bundled copy
/// staged under `<target>/.aube-deploy-injected/<id>/`. `strip` names
/// any dep fields the caller wants physically removed before install
/// runs — load-bearing for `--prod` / `--dev` / `--no-optional`, since
/// install's resolver walks the full manifest before the linker applies
/// filtering, so an unstripped sibling devDep would still be fetched.
///
/// `source_pkg_dir` resolves relative `file:` / `link:` paths the same
/// way they resolve in the source workspace; `manifest_dir` is where
/// the rewritten manifest lives, used to compute the relative
/// `file:./...` path back to the staged sibling. For the deployed
/// package these are the source pkg and the target root; for a bundled
/// sibling they are the sibling's own source dir and its
/// `<target>/.aube-deploy-injected/<id>/` staging dir.
///
/// Unknown `workspace:` refs are a hard error (bundling would have
/// already inserted them into `plan` if they were valid).
/// `peerDependencies` is left untouched — peers are satisfied by the
/// consumer's installed tree, not bundled.
// 8 arguments: each is a distinct piece of context the rewriter needs.
// Bundling them into a struct would just shift the names off the
// signature without simplifying the call sites — every test already
// builds each value explicitly.
#[allow(clippy::too_many_arguments)]
pub(super) fn rewrite_local_refs(
    manifest_path: &Path,
    source_pkg_dir: &Path,
    manifest_dir: &Path,
    ws_index: &BTreeMap<String, (PathBuf, Option<String>)>,
    catalogs: &CatalogMap,
    plan: &InjectionPlan,
    strip: StripFields,
    root: DeployRoot<'_>,
) -> miette::Result<()> {
    let raw = std::fs::read_to_string(manifest_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse {}", manifest_path.display()))?;

    const DEP_FIELDS: &[&str] = &[
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ];
    let Some(obj) = doc.as_object_mut() else {
        return Err(miette!(
            "{} did not parse to a JSON object",
            manifest_path.display()
        ));
    };
    if strip.dependencies {
        obj.remove("dependencies");
    }
    if strip.dev_dependencies {
        obj.remove("devDependencies");
    }
    if strip.optional_dependencies {
        obj.remove("optionalDependencies");
    }
    for field in DEP_FIELDS {
        let Some(deps) = obj.get_mut(*field).and_then(|v| v.as_object_mut()) else {
            continue;
        };
        for (name, spec_val) in deps.iter_mut() {
            let Some(raw_spec) = spec_val.as_str() else {
                continue;
            };
            // Resolve `catalog:` first. The deploy target has no
            // workspace yaml, so any `catalog:` reference left in the
            // manifest would hit `ERR_AUBE_UNKNOWN_CATALOG` at install
            // time. We swap in the concrete range from the source
            // workspace's catalog map and re-bind `spec` so the
            // workspace/file/link branches below see the resolved
            // value (matters when a catalog entry points at a
            // `workspace:` / `file:` spec — pnpm allows that).
            let resolved_owned;
            let spec: &str = if aube_util::pkg::is_catalog_spec(raw_spec) {
                // `resolve_catalog_for_rewrite` rejects chained
                // `catalog:` -> `catalog:` values, so the resolved
                // string always differs from `raw_spec` — write
                // unconditionally.
                resolved_owned =
                    resolve_catalog_for_rewrite(catalogs, name, raw_spec, manifest_path)?;
                *spec_val = serde_json::Value::String(resolved_owned.clone());
                resolved_owned.as_str()
            } else {
                raw_spec
            };
            if aube_util::pkg::is_workspace_spec(spec) {
                let Some((sibling_dir, sibling_version)) = ws_index.get(name) else {
                    return Err(miette!(
                        "{}: {} declares `{name}: {spec}` but no workspace package named {name:?} was found",
                        aube_util::cmd("deploy"),
                        manifest_path.display()
                    ));
                };
                let canonical = super::canonicalize(sibling_dir);
                // Back-ref to the deployed package itself: a sibling B
                // depending on `@deployed-pkg` via `workspace:*` must
                // resolve to the deploy root, not a bundled copy
                // (singletons would otherwise break). Emit a `file:`
                // pointer back at `target_root` from `manifest_dir`.
                if canonical == root.deployed_canonical {
                    *spec_val =
                        serde_json::Value::String(file_spec_to_dir(manifest_dir, root.target_root));
                    continue;
                }
                let Some(inj) = plan.get(&canonical) else {
                    // Reachable when `peerDependencies` references a
                    // workspace sibling — peers aren't bundled (bundling
                    // walks dependencies/devDependencies/optionalDependencies
                    // only). Resolve the `workspace:` spec to a concrete
                    // semver range so the install layer can actually parse
                    // it; leaving raw `workspace:*` would hard-fail when
                    // the deploy target has no workspace context.
                    if *field == "peerDependencies" {
                        let Some(sibling_version) = sibling_version else {
                            return Err(miette!(
                                "{}: workspace package {name:?} has no `version` field, required to rewrite `{name}: {spec}` in {}",
                                aube_util::cmd("deploy"),
                                manifest_path.display()
                            ));
                        };
                        *spec_val = serde_json::Value::String(resolve_workspace_spec(
                            spec,
                            sibling_version,
                        ));
                        continue;
                    }
                    return Err(miette!(
                        "{}: bundling plan missing entry for workspace sibling {name:?} declared in {}",
                        aube_util::cmd("deploy"),
                        manifest_path.display()
                    ));
                };
                *spec_val = serde_json::Value::String(file_spec_for_injection(manifest_dir, inj));
            } else if let Some(local) = aube_lockfile::LocalSource::parse(spec, source_pkg_dir) {
                let abs = match &local {
                    aube_lockfile::LocalSource::Directory(rel)
                    | aube_lockfile::LocalSource::Link(rel)
                    | aube_lockfile::LocalSource::Portal(rel)
                    | aube_lockfile::LocalSource::Tarball(rel) => source_pkg_dir.join(rel),
                    aube_lockfile::LocalSource::Exec(_)
                    | aube_lockfile::LocalSource::Git(_)
                    | aube_lockfile::LocalSource::RemoteTarball(_) => continue,
                };
                let canonical = super::canonicalize(&abs);
                // Same back-ref guard as the `workspace:` branch: a sibling
                // reaching the deployed package via `file:../deployed-pkg`
                // must rewrite to a `file:` pointer at the deploy root,
                // not to a duplicate copy.
                if canonical == root.deployed_canonical {
                    *spec_val =
                        serde_json::Value::String(file_spec_to_dir(manifest_dir, root.target_root));
                    continue;
                }
                let Some(inj) = plan.get(&canonical) else {
                    // `file:`/`link:` peers are not bundled (peerDependencies
                    // is excluded from `iter_strippable_deps`), so a peer
                    // pointing at a relative local path can't be left as-is:
                    // the relative path means something else under the
                    // deploy target. Fail loudly rather than ship a manifest
                    // whose paths resolve nowhere at runtime.
                    if *field == "peerDependencies" {
                        return Err(miette!(
                            "{}: peerDependencies cannot reference a local `file:`/`link:` target ({name:?} -> {spec:?}) — peers aren't bundled into the deploy and the relative path won't resolve under the target. Promote the peer to a regular dependency or drop the local path.",
                            aube_util::cmd("deploy")
                        ));
                    }
                    return Err(miette!(
                        "{}: bundling plan missing entry for `{name}: {spec}` declared in {}",
                        aube_util::cmd("deploy"),
                        manifest_path.display()
                    ));
                };
                *spec_val = serde_json::Value::String(file_spec_for_injection(manifest_dir, inj));
            }
        }
    }

    let rewritten = serde_json::to_string_pretty(&doc)
        .into_diagnostic()
        .wrap_err("failed to serialize rewritten package.json")?;
    aube_util::fs_atomic::atomic_write(manifest_path, rewritten.as_bytes())
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to write {}", manifest_path.display()))?;
    Ok(())
}

/// Build the `file:./...` spec the rewriter writes for an injected ref.
/// For directory sources the path points at the staged package root;
/// for tarball sources it points at the staged tarball file. Always
/// emits POSIX separators so a deploy artifact built on macOS/Linux
/// installs unchanged on Windows.
fn file_spec_for_injection(manifest_dir: &Path, inj: &Injection) -> String {
    let target_path = if inj.is_tarball {
        inj.target_dir.join(&inj.tarball_filename)
    } else {
        inj.target_dir.clone()
    };
    file_spec_to_dir(manifest_dir, &target_path)
}

/// `file:` spec from `manifest_dir` to `target` as a forward-slashed
/// relative path. Used both for bundled-injection refs and for
/// back-refs from a bundled sibling to the deploy root.
fn file_spec_to_dir(manifest_dir: &Path, target: &Path) -> String {
    let rel = pathdiff::diff_paths(target, manifest_dir).unwrap_or_else(|| target.to_path_buf());
    let mut s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() {
        s = ".".to_string();
    }
    // npm/pnpm canonicalize plain `file:` refs; `file:./x` is more
    // visually obviously a relative path than `file:x`, so prefix `./`
    // when the result doesn't already start with a path-traversal or
    // absolute marker.
    if !s.starts_with("./") && !s.starts_with("../") && !s.starts_with('/') {
        s = format!("./{s}");
    }
    format!("file:{s}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_index(entries: &[(&str, &str)]) -> BTreeMap<String, (PathBuf, Option<String>)> {
        entries
            .iter()
            .map(|(n, v)| {
                (
                    (*n).to_string(),
                    (PathBuf::from("/tmp"), Some((*v).to_string())),
                )
            })
            .collect()
    }

    /// Build a catalog map matching what `discover_catalogs` would return —
    /// the outer key is the catalog name (`"default"` for the unnamed
    /// catalog), the inner map goes package → range.
    fn catalog_map(entries: &[(&str, &[(&str, &str)])]) -> CatalogMap {
        let mut m = CatalogMap::new();
        for (cat_name, pkgs) in entries {
            let mut inner = BTreeMap::new();
            for (pkg, range) in *pkgs {
                inner.insert((*pkg).to_string(), (*range).to_string());
            }
            m.insert((*cat_name).to_string(), inner);
        }
        m
    }

    #[test]
    fn rewrite_local_refs_drops_workspace_dep_when_field_stripped() {
        // `--prod` default: a workspace: devDep must be physically
        // removed from the deployed manifest before install runs (the
        // resolver walks every dep field before filtering).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"lodash":"^4"},"devDependencies":{"@test/internal":"workspace:*"}}"#,
        )
        .unwrap();

        let idx = ws_index(&[]); // empty: dev is stripped, sibling never looked up
        let plan = InjectionPlan::new();
        let stub = PathBuf::from("/nonexistent-deployed");
        rewrite_local_refs(
            &path,
            tmp.path(),
            tmp.path(),
            &idx,
            &CatalogMap::new(),
            &plan,
            StripFields {
                dependencies: false,
                dev_dependencies: true,
                optional_dependencies: false,
            },
            DeployRoot {
                deployed_canonical: &stub,
                target_root: tmp.path(),
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(out.get("devDependencies").is_none());
        assert_eq!(out["dependencies"]["lodash"], "^4");
    }

    #[test]
    fn rewrite_local_refs_writes_relative_file_spec_for_workspace_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_dir = tmp.path();
        let sibling_dir = tmp.path().join("packages/lib");
        std::fs::create_dir_all(&sibling_dir).unwrap();
        let injected_dir = manifest_dir.join(".aube-deploy-injected").join("lib");
        std::fs::create_dir_all(&injected_dir).unwrap();

        let path = manifest_dir.join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"@test/lib":"workspace:*"}}"#,
        )
        .unwrap();

        let mut idx = BTreeMap::new();
        idx.insert(
            "@test/lib".to_string(),
            (sibling_dir.clone(), Some("1.2.3".to_string())),
        );
        let mut plan = InjectionPlan::new();
        plan.insert(
            super::super::canonicalize(&sibling_dir),
            Injection {
                source_dir: sibling_dir.clone(),
                is_tarball: false,
                target_dir: injected_dir.clone(),
                tarball_filename: String::new(),
            },
        );
        let stub = PathBuf::from("/nonexistent-deployed");
        rewrite_local_refs(
            &path,
            manifest_dir,
            manifest_dir,
            &idx,
            &CatalogMap::new(),
            &plan,
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: manifest_dir,
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            out["dependencies"]["@test/lib"],
            "file:./.aube-deploy-injected/lib"
        );
    }

    #[test]
    fn rewrite_local_refs_resolves_workspace_peer_to_concrete_range() {
        // peerDependencies aren't bundled (bundling walks deps/devDeps/
        // optionalDeps only), so they hit the resolve_workspace_spec
        // path. Each spec form should land on a parseable semver range.
        let tmp = tempfile::tempdir().unwrap();
        let manifest_dir = tmp.path();
        let sibling_dir = tmp.path().join("packages/lib");
        std::fs::create_dir_all(&sibling_dir).unwrap();

        let path = manifest_dir.join("package.json");
        std::fs::write(
            &path,
            r#"{
                "name":"x",
                "version":"1.0.0",
                "peerDependencies":{
                    "@test/lib":"workspace:*",
                    "@test/lib-caret":"workspace:^",
                    "@test/lib-tilde":"workspace:~",
                    "@test/lib-literal":"workspace:^2.0.0"
                }
            }"#,
        )
        .unwrap();

        let mut idx = BTreeMap::new();
        for n in [
            "@test/lib",
            "@test/lib-caret",
            "@test/lib-tilde",
            "@test/lib-literal",
        ] {
            idx.insert(
                n.to_string(),
                (sibling_dir.clone(), Some("1.2.3".to_string())),
            );
        }
        let stub = PathBuf::from("/nonexistent-deployed");
        rewrite_local_refs(
            &path,
            manifest_dir,
            manifest_dir,
            &idx,
            &CatalogMap::new(),
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: manifest_dir,
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let peers = &out["peerDependencies"];
        assert_eq!(peers["@test/lib"], "1.2.3");
        assert_eq!(peers["@test/lib-caret"], "^1.2.3");
        assert_eq!(peers["@test/lib-tilde"], "~1.2.3");
        assert_eq!(peers["@test/lib-literal"], "^2.0.0");
    }

    #[test]
    fn rewrite_local_refs_writes_back_ref_to_target_root_for_deployed_pkg() {
        // Sibling B (staged at <target>/.aube-deploy-injected/B/)
        // declares a `workspace:*` back-dep on the deployed package.
        // We must not bundle the deployed package as a separate
        // injection (singleton would break); instead, rewrite the spec
        // to a `file:` pointer back at the deploy root.
        let tmp = tempfile::tempdir().unwrap();
        let target_root = tmp.path();
        let deployed_dir = target_root.join("source/deployed-pkg");
        std::fs::create_dir_all(&deployed_dir).unwrap();
        let deployed_canonical = super::super::canonicalize(&deployed_dir);
        let sibling_target = target_root.join(".aube-deploy-injected").join("b");
        std::fs::create_dir_all(&sibling_target).unwrap();

        let sibling_manifest = sibling_target.join("package.json");
        std::fs::write(
            &sibling_manifest,
            r#"{"name":"@test/b","version":"1.0.0","dependencies":{"@deployed/pkg":"workspace:*"}}"#,
        )
        .unwrap();

        let mut idx = BTreeMap::new();
        idx.insert(
            "@deployed/pkg".to_string(),
            (deployed_canonical.clone(), Some("9.9.9".to_string())),
        );
        rewrite_local_refs(
            &sibling_manifest,
            &deployed_canonical,
            &sibling_target,
            &idx,
            &CatalogMap::new(),
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &deployed_canonical,
                target_root,
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sibling_manifest).unwrap()).unwrap();
        // From <target>/.aube-deploy-injected/b/ back to <target>/ is
        // `../..`.
        assert_eq!(out["dependencies"]["@deployed/pkg"], "file:../..");
    }

    #[test]
    fn rewrite_local_refs_writes_back_ref_for_file_link_to_deployed_pkg() {
        // Same back-ref scenario as the workspace test, but the sibling
        // references the deployed package via `file:` instead of
        // `workspace:*`. The result must still be a relative file:
        // pointer at the deploy root, not a bundled duplicate.
        let tmp = tempfile::tempdir().unwrap();
        let target_root = tmp.path();
        let deployed_dir = target_root.join("source/deployed-pkg");
        std::fs::create_dir_all(&deployed_dir).unwrap();
        let deployed_canonical = super::super::canonicalize(&deployed_dir);
        let sibling_target = target_root.join(".aube-deploy-injected").join("b");
        std::fs::create_dir_all(&sibling_target).unwrap();

        let sibling_manifest = sibling_target.join("package.json");
        std::fs::write(
            &sibling_manifest,
            r#"{"name":"@test/b","version":"1.0.0","dependencies":{"@deployed/pkg":"file:../../source/deployed-pkg"}}"#,
        )
        .unwrap();

        rewrite_local_refs(
            &sibling_manifest,
            &deployed_canonical,
            &sibling_target,
            &BTreeMap::new(),
            &CatalogMap::new(),
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &deployed_canonical,
                target_root,
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sibling_manifest).unwrap()).unwrap();
        assert_eq!(out["dependencies"]["@deployed/pkg"], "file:../..");
    }

    #[test]
    fn rewrite_local_refs_errors_on_file_peer_dep() {
        // `file:`/`link:` peer specs aren't bundled and the relative
        // path doesn't survive the deploy. Hard-fail rather than ship a
        // manifest whose peer paths resolve nowhere.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","peerDependencies":{"vendor":"file:../local-vendor"}}"#,
        )
        .unwrap();
        let stub = PathBuf::from("/nonexistent-deployed");
        let err = rewrite_local_refs(
            &path,
            tmp.path(),
            tmp.path(),
            &BTreeMap::new(),
            &CatalogMap::new(),
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: tmp.path(),
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("peerDependencies"), "msg was: {msg}");
        assert!(msg.contains("vendor"), "msg was: {msg}");
    }

    #[test]
    fn rewrite_local_refs_errors_on_unknown_workspace_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"@test/missing":"workspace:*"}}"#,
        )
        .unwrap();
        let idx = ws_index(&[]);
        let plan = InjectionPlan::new();
        let stub = PathBuf::from("/nonexistent-deployed");
        let err = rewrite_local_refs(
            &path,
            tmp.path(),
            tmp.path(),
            &idx,
            &CatalogMap::new(),
            &plan,
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: tmp.path(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("@test/missing"));
    }

    #[test]
    fn rewrite_local_refs_resolves_catalog_default() {
        // Bare `catalog:` and explicit `catalog:default` both resolve from
        // the source workspace's `default` catalog. The deployed manifest
        // becomes self-contained — no workspace yaml needed at install
        // time.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{
                "name":"x","version":"1.0.0",
                "dependencies":{
                    "drizzle-orm":"catalog:",
                    "zod":"catalog:default"
                }
            }"#,
        )
        .unwrap();

        let cats = catalog_map(&[(
            "default",
            &[("drizzle-orm", "1.0.0-rc.1"), ("zod", "4.4.2")],
        )]);
        let stub = PathBuf::from("/nonexistent-deployed");
        rewrite_local_refs(
            &path,
            tmp.path(),
            tmp.path(),
            &BTreeMap::new(),
            &cats,
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: tmp.path(),
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(out["dependencies"]["drizzle-orm"], "1.0.0-rc.1");
        assert_eq!(out["dependencies"]["zod"], "4.4.2");
    }

    #[test]
    fn rewrite_local_refs_resolves_named_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"react":"catalog:evens"}}"#,
        )
        .unwrap();

        let cats = catalog_map(&[("evens", &[("react", "18.2.0")])]);
        let stub = PathBuf::from("/nonexistent-deployed");
        rewrite_local_refs(
            &path,
            tmp.path(),
            tmp.path(),
            &BTreeMap::new(),
            &cats,
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: tmp.path(),
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(out["dependencies"]["react"], "18.2.0");
    }

    #[test]
    fn rewrite_local_refs_errors_on_unknown_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"drizzle-orm":"catalog:"}}"#,
        )
        .unwrap();
        let stub = PathBuf::from("/nonexistent-deployed");
        let err = rewrite_local_refs(
            &path,
            tmp.path(),
            tmp.path(),
            &BTreeMap::new(),
            &CatalogMap::new(),
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: tmp.path(),
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("catalog `default`"), "msg was: {msg}");
        assert!(msg.contains("drizzle-orm"), "msg was: {msg}");
    }

    #[test]
    fn rewrite_local_refs_errors_on_missing_catalog_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"drizzle-orm":"catalog:"}}"#,
        )
        .unwrap();
        let cats = catalog_map(&[("default", &[("zod", "4.4.2")])]);
        let stub = PathBuf::from("/nonexistent-deployed");
        let err = rewrite_local_refs(
            &path,
            tmp.path(),
            tmp.path(),
            &BTreeMap::new(),
            &cats,
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: tmp.path(),
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("has no entry"), "msg was: {msg}");
        assert!(msg.contains("drizzle-orm"), "msg was: {msg}");
    }

    #[test]
    fn rewrite_local_refs_errors_on_chained_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"react":"catalog:"}}"#,
        )
        .unwrap();
        // Catalog entry whose value is itself another catalog reference —
        // pnpm rejects this; we mirror the behavior.
        let cats = catalog_map(&[("default", &[("react", "catalog:other")])]);
        let stub = PathBuf::from("/nonexistent-deployed");
        let err = rewrite_local_refs(
            &path,
            tmp.path(),
            tmp.path(),
            &BTreeMap::new(),
            &cats,
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: tmp.path(),
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("catalogs cannot chain"), "msg was: {msg}");
    }

    #[test]
    fn rewrite_local_refs_catalog_resolves_to_workspace_then_to_file_ref() {
        // A catalog entry can point at a `workspace:` spec — pnpm allows
        // this. After catalog resolution the workspace branch should
        // then rewrite to a `file:` pointer at the bundled sibling.
        let tmp = tempfile::tempdir().unwrap();
        let manifest_dir = tmp.path();
        let sibling_dir = tmp.path().join("packages/lib");
        std::fs::create_dir_all(&sibling_dir).unwrap();
        let injected_dir = manifest_dir.join(".aube-deploy-injected").join("lib");
        std::fs::create_dir_all(&injected_dir).unwrap();

        let path = manifest_dir.join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"@test/lib":"catalog:"}}"#,
        )
        .unwrap();

        let mut idx = BTreeMap::new();
        idx.insert(
            "@test/lib".to_string(),
            (sibling_dir.clone(), Some("1.2.3".to_string())),
        );
        let mut plan = InjectionPlan::new();
        plan.insert(
            super::super::canonicalize(&sibling_dir),
            Injection {
                source_dir: sibling_dir.clone(),
                is_tarball: false,
                target_dir: injected_dir.clone(),
                tarball_filename: String::new(),
            },
        );
        let cats = catalog_map(&[("default", &[("@test/lib", "workspace:*")])]);
        let stub = PathBuf::from("/nonexistent-deployed");
        rewrite_local_refs(
            &path,
            manifest_dir,
            manifest_dir,
            &idx,
            &cats,
            &plan,
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: manifest_dir,
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            out["dependencies"]["@test/lib"],
            "file:./.aube-deploy-injected/lib"
        );
    }

    #[test]
    fn file_spec_for_injection_emits_relative_directory_path() {
        let manifest_dir = PathBuf::from("/tmp/deploy/out");
        let target_dir = PathBuf::from("/tmp/deploy/out/.aube-deploy-injected/lib");
        let inj = Injection {
            source_dir: PathBuf::from("/src/lib"),
            is_tarball: false,
            target_dir,
            tarball_filename: String::new(),
        };
        assert_eq!(
            file_spec_for_injection(&manifest_dir, &inj),
            "file:./.aube-deploy-injected/lib"
        );
    }

    #[test]
    fn file_spec_for_injection_emits_relative_tarball_path() {
        let manifest_dir = PathBuf::from("/tmp/deploy/out");
        let target_dir = PathBuf::from("/tmp/deploy/out/.aube-deploy-injected/foo");
        let inj = Injection {
            source_dir: PathBuf::from("/src/foo.tgz"),
            is_tarball: true,
            target_dir,
            tarball_filename: "foo.tgz".to_string(),
        };
        assert_eq!(
            file_spec_for_injection(&manifest_dir, &inj),
            "file:./.aube-deploy-injected/foo/foo.tgz"
        );
    }

    #[test]
    fn file_spec_for_injection_emits_dotdot_for_sibling_in_injected_dir() {
        // A bundled sibling whose own manifest references another
        // bundled sibling: rewrite emits `../<id>` relative to the
        // sibling's own staging dir, not the deploy root.
        let manifest_dir = PathBuf::from("/tmp/deploy/out/.aube-deploy-injected/lib");
        let target_dir = PathBuf::from("/tmp/deploy/out/.aube-deploy-injected/core");
        let inj = Injection {
            source_dir: PathBuf::from("/src/core"),
            is_tarball: false,
            target_dir,
            tarball_filename: String::new(),
        };
        assert_eq!(file_spec_for_injection(&manifest_dir, &inj), "file:../core");
    }
}
