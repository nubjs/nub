use miette::{Context, IntoDiagnostic};

/// Type alias for the catalog map the resolver consumes — outer key is
/// the catalog name (`default` for the unnamed catalog), inner map goes
/// from package name to version range.
pub(crate) type CatalogMap =
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>;

/// Merge `default_cat` / `named_cats` into `out`. Later calls overwrite
/// earlier entries — callers invoke this in ascending precedence order
/// so the highest-priority source lands last.
fn merge_catalog_source(
    out: &mut CatalogMap,
    default_cat: &std::collections::BTreeMap<String, String>,
    named_cats: &CatalogMap,
) {
    if !default_cat.is_empty() {
        let entry = out.entry("default".to_string()).or_default();
        for (k, v) in default_cat {
            entry.insert(k.clone(), v.clone());
        }
    }
    for (name, entries) in named_cats {
        let bucket = out.entry(name.clone()).or_default();
        for (k, v) in entries {
            bucket.insert(k.clone(), v.clone());
        }
    }
}

/// Pull the bun-style `workspaces.catalog` / `workspaces.catalogs` and
/// pnpm-style `pnpm.catalog` / `pnpm.catalogs` out of a single
/// package.json and merge them into `out`. Precedence within one
/// manifest: `pnpm.*` wins over `workspaces.*`.
fn merge_manifest_catalogs(out: &mut CatalogMap, manifest: &aube_manifest::PackageJson) {
    if let Some(ws) = &manifest.workspaces {
        merge_catalog_source(out, ws.catalog(), ws.catalogs());
    }
    merge_catalog_source(out, &manifest.pnpm_catalog(), &manifest.pnpm_catalogs());
}

/// Discover catalog entries from every supported source and merge them
/// into a single map for the resolver.
///
/// Sources, in ascending precedence (later overrides earlier on a per-
/// entry basis):
/// 1. `workspaces.catalog` / `workspaces.catalogs` in the project-root
///    `package.json` (bun style).
/// 2. `pnpm.catalog` / `pnpm.catalogs` in the project-root `package.json`.
/// 3. Same two fields from the workspace-root `package.json` when it's
///    a different file (monorepo subpackage installs). The workspace
///    root is the nearest ancestor with either a `pnpm-workspace.yaml` /
///    `aube-workspace.yaml` or a `package.json` carrying a `workspaces`
///    field — bun / npm / yarn projects use the latter and have no yaml.
/// 4. `catalog:` / `catalogs:` in the nearest `pnpm-workspace.yaml` /
///    `aube-workspace.yaml` walking up from `project_root`.
///
/// Walking up matters for monorepos where `aube install` runs from a
/// subpackage — without it, the loader only looks at `project_root`
/// and misses the root workspace's catalogs entirely.
///
/// Every command that builds a `Resolver` threads this map through
/// `Resolver::with_catalogs`; otherwise the resolver hard-fails any
/// `catalog:` dep with `UnknownCatalog(Entry)`.
pub(crate) fn discover_catalogs(project_root: &std::path::Path) -> miette::Result<CatalogMap> {
    let mut out = CatalogMap::new();

    // (1)+(2): project-root package.json catalogs.
    let project_manifest_path = project_root.join("package.json");
    let project_manifest = aube_manifest::PackageJson::from_path(&project_manifest_path).ok();
    if let Some(m) = &project_manifest {
        merge_manifest_catalogs(&mut out, m);
    }

    // (3): workspace-root package.json catalogs, if the workspace root
    // sits above the project root. We resolve the workspace root from
    // either marker — yaml first (pnpm convention), then `workspaces`
    // field (bun / npm / yarn convention) — so a subpackage install in
    // a non-pnpm monorepo still picks up the root catalog.
    let workspace_yaml_dir = crate::dirs::find_workspace_yaml_root(project_root);
    let workspace_root_dir = crate::dirs::find_workspace_root(project_root);
    if let Some(dir) = &workspace_root_dir
        && dir != project_root
        && let Ok(m) = aube_manifest::PackageJson::from_path(&dir.join("package.json"))
    {
        merge_manifest_catalogs(&mut out, &m);
    }

    // (4): workspace yaml catalogs, highest precedence. Loaded from the
    // walk-up directory when present, else from `project_root`.
    let yaml_dir = workspace_yaml_dir.as_deref().unwrap_or(project_root);
    let (ws_config, _raw) = aube_manifest::workspace::load_both(yaml_dir)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    merge_catalog_source(&mut out, &ws_config.catalog, &ws_config.catalogs);

    out.retain(|_, v| !v.is_empty());
    Ok(out)
}

/// Convenience alias preserved for existing call sites; forwards to
/// [`discover_catalogs`] so every command sees the same merged view.
pub(crate) fn load_workspace_catalogs(cwd: &std::path::Path) -> miette::Result<CatalogMap> {
    discover_catalogs(cwd)
}
