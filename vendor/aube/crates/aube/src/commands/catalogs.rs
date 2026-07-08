//! Catalog-aware helpers shared between `aube add` and `aube install`.
//!
//! Two settings land here:
//! - `catalogMode` governs how `add` writes the manifest specifier for a
//!   package that already appears in the default workspace catalog.
//! - `cleanupUnusedCatalogs` trims workspace-yaml catalog entries that
//!   no importer references, after a successful resolve.
//!
//! Both features share the same source of truth (`WorkspaceConfig::catalog`
//! / `catalogs` maps on disk), so the edit-the-yaml plumbing lives here
//! rather than in either command module.

use aube_settings::resolved::CatalogMode;
use miette::WrapErr;
use std::collections::BTreeMap;
use std::path::Path;

/// Outcome of matching an `aube add` spec against the default catalog.
#[derive(Debug)]
pub(crate) enum CatalogRewrite {
    /// Write the user's resolved specifier verbatim — either the mode is
    /// `manual`, the package isn't in the catalog, or `prefer` decided
    /// the user's range was incompatible.
    Manual,
    /// Rewrite the manifest entry to `catalog:` (always the default
    /// catalog — named catalogs require an explicit opt-in spec).
    UseDefaultCatalog,
    /// `strict` mode saw a spec that disagrees with the catalog entry.
    /// Propagate as a hard error.
    StrictMismatch {
        pkg: String,
        catalog_range: String,
        user_range: String,
    },
}

/// Decide whether an `add` specifier should be rewritten to a
/// `catalog:` reference. See `CatalogMode` docs in `settings.toml` for
/// the full semantics; this function only considers the *default*
/// catalog since named catalogs (`catalog:<name>`) always require an
/// explicit user opt-in.
pub(crate) fn decide_add_rewrite(
    mode: CatalogMode,
    default_catalog: Option<&BTreeMap<String, String>>,
    pkg_name: &str,
    user_range: &str,
    has_explicit_range: bool,
    resolved_version: &str,
    exclude_from_catalog: bool,
) -> CatalogRewrite {
    if exclude_from_catalog {
        return CatalogRewrite::Manual;
    }
    let Some(catalog) = default_catalog else {
        return CatalogRewrite::Manual;
    };
    let Some(catalog_range) = catalog.get(pkg_name) else {
        return CatalogRewrite::Manual;
    };
    match mode {
        CatalogMode::Manual => CatalogRewrite::Manual,
        CatalogMode::Prefer => {
            if range_compatible(
                user_range,
                has_explicit_range,
                catalog_range,
                resolved_version,
            ) {
                CatalogRewrite::UseDefaultCatalog
            } else {
                CatalogRewrite::Manual
            }
        }
        CatalogMode::Strict => {
            if !has_explicit_range
                || range_compatible(
                    user_range,
                    has_explicit_range,
                    catalog_range,
                    resolved_version,
                )
            {
                CatalogRewrite::UseDefaultCatalog
            } else {
                CatalogRewrite::StrictMismatch {
                    pkg: pkg_name.to_string(),
                    catalog_range: catalog_range.to_string(),
                    user_range: user_range.to_string(),
                }
            }
        }
    }
}

/// Treat the user's range as compatible with the catalog when it is
/// either (a) the exact same string — the common case for projects
/// that already standardized on catalog ranges — or (b) the catalog's
/// range would also accept the version we just resolved, so swapping
/// `catalog:` in won't silently install a different version.
pub(crate) fn range_compatible(
    user_range: &str,
    has_explicit_range: bool,
    catalog_range: &str,
    resolved_version: &str,
) -> bool {
    if !has_explicit_range {
        return true;
    }
    if user_range == catalog_range {
        return true;
    }
    let Ok(catalog_parsed) = node_semver::Range::parse(catalog_range) else {
        return false;
    };
    let Ok(version) = node_semver::Version::parse(resolved_version) else {
        return false;
    };
    version.satisfies(&catalog_parsed)
}

/// Remove catalog entries from the workspace yaml that the freshly
/// resolved graph didn't reference. Returns the list of `(catalog,
/// package)` pairs that were dropped so the caller can surface a
/// one-line summary.
///
/// Goes through `aube_manifest::workspace::edit_workspace_yaml`, which
/// no-ops the rewrite when the closure produces no structural change —
/// catalog cleanup runs on every install under `cleanupUnusedCatalogs`
/// and we don't want to strip user comments on the steady-state pass
/// where every declared entry is still referenced.
pub(crate) fn prune_unused_catalog_entries(
    workspace_path: &Path,
    declared: &BTreeMap<String, BTreeMap<String, String>>,
    used: &BTreeMap<String, BTreeMap<String, aube_lockfile::CatalogEntry>>,
) -> miette::Result<Vec<(String, String)>> {
    let mut unused: Vec<(String, String)> = Vec::new();
    for (cat_name, entries) in declared {
        for pkg in entries.keys() {
            let is_used = used
                .get(cat_name)
                .map(|u| u.contains_key(pkg))
                .unwrap_or(false);
            if !is_used {
                unused.push((cat_name.clone(), pkg.clone()));
            }
        }
    }
    if unused.is_empty() {
        return Ok(unused);
    }

    aube_manifest::workspace::edit_workspace_yaml(workspace_path, |root| {
        for (cat_name, pkg_name) in &unused {
            if cat_name == "default" {
                if let Some(map) = root
                    .get_mut("catalog")
                    .and_then(yaml_serde::Value::as_mapping_mut)
                {
                    map.shift_remove(pkg_name.as_str());
                }
            } else if let Some(catalogs) = root
                .get_mut("catalogs")
                .and_then(yaml_serde::Value::as_mapping_mut)
                && let Some(map) = catalogs
                    .get_mut(cat_name.as_str())
                    .and_then(yaml_serde::Value::as_mapping_mut)
            {
                map.shift_remove(pkg_name.as_str());
            }
        }
        // Drop now-empty containers so the file doesn't grow meaningless
        // `catalog: {}` / `catalogs:` headers.
        if root
            .get("catalog")
            .and_then(yaml_serde::Value::as_mapping)
            .is_some_and(yaml_serde::Mapping::is_empty)
        {
            root.shift_remove("catalog");
        }
        if let Some(catalogs) = root
            .get_mut("catalogs")
            .and_then(yaml_serde::Value::as_mapping_mut)
        {
            let to_drop: Vec<String> = catalogs
                .iter()
                .filter_map(|(k, v)| {
                    let key = k.as_str()?;
                    match v.as_mapping() {
                        Some(m) if m.is_empty() => Some(key.to_string()),
                        _ => None,
                    }
                })
                .collect();
            for key in to_drop {
                catalogs.shift_remove(key.as_str());
            }
        }
        if root
            .get("catalogs")
            .and_then(yaml_serde::Value::as_mapping)
            .is_some_and(yaml_serde::Mapping::is_empty)
        {
            root.shift_remove("catalogs");
        }
        Ok(())
    })
    .map_err(miette::Report::new)
    .wrap_err_with(|| {
        format!(
            "failed to write {} after cleanupUnusedCatalogs",
            workspace_path.display()
        )
    })?;
    Ok(unused)
}

/// One catalog entry queued by `aube add --save-catalog` /
/// `--save-catalog-name`. The writer applies these in a single
/// `edit_workspace_yaml` pass so the file is rewritten at most once
/// per command, preserving comments when nothing structural changed.
#[derive(Debug, Clone)]
pub(crate) struct CatalogUpsert {
    /// Catalog name. `"default"` writes under the top-level `catalog:`
    /// key; any other name lands under `catalogs.<name>`.
    pub catalog: String,
    /// Package name (the manifest key that will reference `catalog:` /
    /// `catalog:<catalog>`).
    pub package: String,
    /// Range to record in the catalog. Already includes any save-prefix
    /// the caller wanted (e.g. `^1.0.0`, `1.2.3`, `~1.0.0`).
    pub range: String,
}

/// Upsert a batch of catalog entries into the workspace yaml. Existing
/// entries are NEVER overwritten — pnpm's `--save-catalog` treats the
/// catalog as the source of truth and lets the caller fall back to the
/// manual specifier when the entry exists. So this function only
/// inserts; same-key skips fall through silently.
///
/// Goes through `edit_workspace_yaml`, which no-ops the rewrite when
/// the closure produces no structural change. Empty `entries` is a
/// no-op.
pub(crate) fn upsert_catalog_entries(
    workspace_path: &Path,
    entries: &[CatalogUpsert],
) -> miette::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    aube_manifest::workspace::edit_workspace_yaml(workspace_path, |root| {
        for entry in entries {
            let CatalogUpsert {
                catalog,
                package,
                range,
            } = entry;
            let map = if catalog == "default" {
                workspace_yaml_submap(root, "catalog", workspace_path)?
            } else {
                let catalogs = workspace_yaml_submap(root, "catalogs", workspace_path)?;
                workspace_yaml_submap(catalogs, catalog.as_str(), workspace_path)?
            };
            map.entry(yaml_serde::Value::String(package.clone()))
                .or_insert_with(|| yaml_serde::Value::String(range.clone()));
        }
        Ok(())
    })
    .map_err(miette::Report::new)
    .wrap_err_with(|| {
        format!(
            "failed to write {} after --save-catalog",
            workspace_path.display()
        )
    })?;
    Ok(())
}

/// Upsert a batch of catalog entries into `package.json#workspaces.catalog(s)`.
///
/// This is the manifest-root companion to [`upsert_catalog_entries`]. It is
/// used when the active embedder reads neutral root manifest config (Nub
/// identity) and does not read pnpm-named workspace YAML. Existing entries are
/// never overwritten, matching pnpm's `--save-catalog` behavior.
pub(crate) fn upsert_catalog_entries_in_manifest(
    workspace_root: &Path,
    entries: &[CatalogUpsert],
) -> miette::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let path = workspace_root.join("package.json");
    crate::commands::update_manifest_json_object(&path, |root| {
        let workspaces = normalize_workspaces_object(root)?;
        for entry in entries {
            let CatalogUpsert {
                catalog,
                package,
                range,
            } = entry;
            let map = if catalog == "default" {
                json_submap(workspaces, "catalog")?
            } else {
                let catalogs = json_submap(workspaces, "catalogs")?;
                json_submap(catalogs, catalog)?
            };
            map.entry(package.clone())
                .or_insert_with(|| serde_json::Value::String(range.clone()));
        }
        Ok(())
    })
    .wrap_err_with(|| {
        format!(
            "failed to write {} after --save-catalog",
            path.display()
        )
    })?;
    Ok(())
}

fn normalize_workspaces_object<'a>(
    root: &'a mut serde_json::Map<String, serde_json::Value>,
) -> miette::Result<&'a mut serde_json::Map<String, serde_json::Value>> {
    let current = root.remove("workspaces");
    let workspaces = match current {
        Some(serde_json::Value::Object(obj)) => serde_json::Value::Object(obj),
        Some(serde_json::Value::Array(packages)) => {
            let mut obj = serde_json::Map::new();
            obj.insert("packages".to_string(), serde_json::Value::Array(packages));
            serde_json::Value::Object(obj)
        }
        Some(serde_json::Value::String(package)) => {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "packages".to_string(),
                serde_json::Value::Array(vec![serde_json::Value::String(package)]),
            );
            serde_json::Value::Object(obj)
        }
        Some(other) => {
            root.insert("workspaces".to_string(), other);
            return Err(miette::miette!(
                "package.json `workspaces` must be an object, array, or string to update catalogs"
            ));
        }
        None => serde_json::Value::Object(serde_json::Map::new()),
    };
    root.insert("workspaces".to_string(), workspaces);
    root.get_mut("workspaces")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| miette::miette!("package.json `workspaces` must be an object"))
}

fn json_submap<'a>(
    map: &'a mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> miette::Result<&'a mut serde_json::Map<String, serde_json::Value>> {
    map.entry(key.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| miette::miette!("package.json `workspaces.{key}` must be an object"))
}

/// Inner-mapping accessor mirroring `aube-manifest::workspace::workspace_yaml_submap`,
/// duplicated here so this crate doesn't have to re-export the private helper.
/// Errors when the key exists but isn't a mapping.
fn workspace_yaml_submap<'a>(
    map: &'a mut yaml_serde::Mapping,
    key: &str,
    path: &Path,
) -> Result<&'a mut yaml_serde::Mapping, aube_manifest::Error> {
    let entry = map
        .entry(yaml_serde::Value::String(key.to_string()))
        .or_insert_with(|| yaml_serde::Value::Mapping(yaml_serde::Mapping::new()));
    entry.as_mapping_mut().ok_or_else(|| {
        aube_manifest::Error::YamlParse(path.to_path_buf(), format!("`{key}` must be a mapping"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_catalog() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("lodash".into(), "^4.17.0".into());
        m.insert("react".into(), "^18.2.0".into());
        m
    }

    #[test]
    fn manual_mode_never_rewrites() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Manual,
            Some(&cat),
            "lodash",
            "^4.17.0",
            true,
            "4.17.21",
            false,
        );
        assert!(matches!(r, CatalogRewrite::Manual));
    }

    #[test]
    fn prefer_rewrites_matching_range() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Prefer,
            Some(&cat),
            "lodash",
            "^4.17.0",
            true,
            "4.17.21",
            false,
        );
        assert!(matches!(r, CatalogRewrite::UseDefaultCatalog));
    }

    #[test]
    fn manifest_catalog_upsert_writes_default_and_named_without_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"root","workspaces":{"packages":["packages/*"]}}"#,
        )
        .unwrap();

        upsert_catalog_entries_in_manifest(
            dir.path(),
            &[
                CatalogUpsert {
                    catalog: "default".into(),
                    package: "react".into(),
                    range: "^19.0.0".into(),
                },
                CatalogUpsert {
                    catalog: "server".into(),
                    package: "@types/node".into(),
                    range: "^26.1.0".into(),
                },
            ],
        )
        .unwrap();

        assert!(
            !dir.path().join("pnpm-workspace.yaml").exists(),
            "manifest catalog writes must not create a workspace yaml"
        );
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("package.json")).unwrap())
                .unwrap();
        assert_eq!(json["workspaces"]["catalog"]["react"], "^19.0.0");
        assert_eq!(
            json["workspaces"]["catalogs"]["server"]["@types/node"],
            "^26.1.0"
        );
    }

    #[test]
    fn manifest_catalog_upsert_preserves_existing_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"workspaces":{"packages":["apps/*"],"catalog":{"react":"^18.0.0"}}}"#,
        )
        .unwrap();

        upsert_catalog_entries_in_manifest(
            dir.path(),
            &[CatalogUpsert {
                catalog: "default".into(),
                package: "react".into(),
                range: "^19.0.0".into(),
            }],
        )
        .unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("package.json")).unwrap())
                .unwrap();
        assert_eq!(json["workspaces"]["catalog"]["react"], "^18.0.0");
    }

    #[test]
    fn manifest_catalog_upsert_converts_workspace_array() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        )
        .unwrap();

        upsert_catalog_entries_in_manifest(
            dir.path(),
            &[CatalogUpsert {
                catalog: "default".into(),
                package: "react".into(),
                range: "^19.0.0".into(),
            }],
        )
        .unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("package.json")).unwrap())
                .unwrap();
        assert_eq!(json["workspaces"]["packages"][0], "packages/*");
        assert_eq!(json["workspaces"]["catalog"]["react"], "^19.0.0");
    }

    #[test]
    fn prefer_falls_back_on_incompatible_range() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Prefer,
            Some(&cat),
            "lodash",
            "^3.0.0",
            true,
            "3.10.0",
            false,
        );
        assert!(matches!(r, CatalogRewrite::Manual));
    }

    #[test]
    fn strict_errors_on_conflicting_range() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Strict,
            Some(&cat),
            "lodash",
            "^3.0.0",
            true,
            "3.10.0",
            false,
        );
        assert!(matches!(r, CatalogRewrite::StrictMismatch { .. }));
    }

    #[test]
    fn prefer_rewrites_when_range_implicit() {
        // `aube add lodash` with no version: `range_compatible`
        // short-circuits on `!has_explicit_range`, so `prefer` should
        // rewrite to `catalog:` the same way `strict` does. Captured so
        // a future change to `range_compatible` can't silently flip the
        // bare-add case back to manual mode.
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Prefer,
            Some(&cat),
            "lodash",
            "latest",
            false,
            "4.17.21",
            false,
        );
        assert!(matches!(r, CatalogRewrite::UseDefaultCatalog));
    }

    #[test]
    fn strict_rewrites_when_range_implicit() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Strict,
            Some(&cat),
            "lodash",
            "latest",
            false,
            "4.17.21",
            false,
        );
        assert!(matches!(r, CatalogRewrite::UseDefaultCatalog));
    }

    #[test]
    fn no_catalog_entry_always_manual() {
        let cat = default_catalog();
        for mode in [
            CatalogMode::Manual,
            CatalogMode::Prefer,
            CatalogMode::Strict,
        ] {
            let r = decide_add_rewrite(mode, Some(&cat), "axios", "^1.0.0", true, "1.6.0", false);
            assert!(matches!(r, CatalogRewrite::Manual), "mode={mode:?}");
        }
    }

    #[test]
    fn exclude_flag_short_circuits() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Strict,
            Some(&cat),
            "lodash",
            "^4.17.0",
            true,
            "4.17.21",
            true,
        );
        assert!(matches!(r, CatalogRewrite::Manual));
    }

    #[test]
    fn prune_drops_unused_default_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(
            &path,
            "catalog:\n  is-odd: ^3.0.1\n  is-even: ^1.0.0\ncatalogs:\n  evens:\n    is-even: ^1.0.0\n",
        )
        .unwrap();

        let mut declared = BTreeMap::new();
        let mut default = BTreeMap::new();
        default.insert("is-odd".to_string(), "^3.0.1".to_string());
        default.insert("is-even".to_string(), "^1.0.0".to_string());
        declared.insert("default".to_string(), default);
        let mut evens = BTreeMap::new();
        evens.insert("is-even".to_string(), "^1.0.0".to_string());
        declared.insert("evens".to_string(), evens);

        let mut used: BTreeMap<String, BTreeMap<String, aube_lockfile::CatalogEntry>> =
            BTreeMap::new();
        used.entry("default".to_string()).or_default().insert(
            "is-odd".to_string(),
            aube_lockfile::CatalogEntry {
                specifier: "^3.0.1".into(),
                version: "3.0.1".into(),
            },
        );

        let dropped = prune_unused_catalog_entries(&path, &declared, &used).unwrap();
        assert_eq!(
            dropped,
            vec![
                ("default".to_string(), "is-even".to_string()),
                ("evens".to_string(), "is-even".to_string()),
            ]
        );

        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("is-odd"), "expected is-odd retained");
        assert!(
            !rewritten.contains("is-even"),
            "expected is-even pruned from {rewritten}"
        );
        assert!(
            !rewritten.contains("catalogs:"),
            "empty named catalog container should be removed: {rewritten}"
        );
    }

    #[test]
    fn prune_preserves_comments_when_dropping_one_entry() {
        // Cleanup of an unused catalog entry must keep `# ...`
        // annotations on the catalog entries that survive — the whole
        // reason aube routes catalog rewrites through
        // `edit_workspace_yaml` is so the daily install (which runs
        // `cleanupUnusedCatalogs`) doesn't silently strip comments off
        // the entries the user took the time to document.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(
            &path,
            "\
# default catalog for the monorepo
catalog:
  # is-odd: keep, used by tooling
  is-odd: ^3.0.1
  # is-even: legacy, slated for removal
  is-even: ^1.0.0
",
        )
        .unwrap();

        let mut declared = BTreeMap::new();
        let mut default = BTreeMap::new();
        default.insert("is-odd".to_string(), "^3.0.1".to_string());
        default.insert("is-even".to_string(), "^1.0.0".to_string());
        declared.insert("default".to_string(), default);

        let mut used: BTreeMap<String, BTreeMap<String, aube_lockfile::CatalogEntry>> =
            BTreeMap::new();
        used.entry("default".to_string()).or_default().insert(
            "is-odd".to_string(),
            aube_lockfile::CatalogEntry {
                specifier: "^3.0.1".into(),
                version: "3.0.1".into(),
            },
        );

        prune_unused_catalog_entries(&path, &declared, &used).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("# default catalog for the monorepo"),
            "header comment lost:\n{written}"
        );
        assert!(
            written.contains("# is-odd: keep, used by tooling"),
            "surviving annotation lost:\n{written}"
        );
        // Entry's key:value line is gone. yamlpatch may leave the
        // orphaned `# is-even: legacy ...` annotation behind on its own
        // line; we accept that — preserving stray user text is a
        // feature, not a bug, and the alternative (heuristically
        // hunting for "owner" comments) would risk eating real ones.
        assert!(
            !written.contains("is-even: ^1.0.0"),
            "pruned entry value still present:\n{written}"
        );
    }

    #[test]
    fn prune_noop_when_all_entries_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        let original = "catalog:\n  is-odd: ^3.0.1\n";
        std::fs::write(&path, original).unwrap();

        let mut declared = BTreeMap::new();
        let mut default = BTreeMap::new();
        default.insert("is-odd".to_string(), "^3.0.1".to_string());
        declared.insert("default".to_string(), default);

        let mut used: BTreeMap<String, BTreeMap<String, aube_lockfile::CatalogEntry>> =
            BTreeMap::new();
        used.entry("default".to_string()).or_default().insert(
            "is-odd".to_string(),
            aube_lockfile::CatalogEntry {
                specifier: "^3.0.1".into(),
                version: "3.0.1".into(),
            },
        );

        let dropped = prune_unused_catalog_entries(&path, &declared, &used).unwrap();
        assert!(dropped.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
