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

/// Read the yarn-style `catalog:` / `catalogs:` blocks out of the
/// `.yarnrc.yml` at `dir` (if one exists) and merge them into `out`.
///
/// Yarn shipped catalogs in 4.10.0 (the catalog plugin is bundled by
/// default from that release). The on-disk shape is byte-identical to
/// pnpm's — top-level `catalog:` for the default catalog and `catalogs:`
/// for named ones — so the same `WorkspaceConfig` deserializer reads it;
/// only the file differs (`.yarnrc.yml` rather than `pnpm-workspace.yaml`).
/// A malformed or catalog-less `.yarnrc.yml` is a no-op: `.yarnrc.yml`
/// carries unrelated yarn config (registries, plugins, …), and a parse
/// failure must not abort catalog discovery for the rest of the sources.
fn merge_yarnrc_catalogs(out: &mut CatalogMap, dir: &std::path::Path) {
    let path = dir.join(".yarnrc.yml");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    if content.trim().is_empty() {
        return;
    }
    let Ok(cfg) = aube_manifest::parse_yaml::<aube_manifest::WorkspaceConfig>(&path, content)
    else {
        return;
    };
    merge_catalog_source(out, &cfg.catalog, &cfg.catalogs);
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
/// 4. `catalog:` / `catalogs:` in the `.yarnrc.yml` at the project root
///    and (when distinct) the workspace root — yarn's catalog feature
///    (yarn 4.10+), same on-disk shape as pnpm's, different file.
/// 5. `catalog:` / `catalogs:` in the nearest `pnpm-workspace.yaml` /
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

    // (3b): yarn-style `.yarnrc.yml` catalogs (yarn 4.10+). Read at the
    // project root and, when distinct, the workspace root — yarn's catalog
    // is a project-level config that a subpackage install must still see.
    // Lower precedence than the pnpm/aube workspace yaml below (which never
    // co-exists with a yarn project in practice, but ordering keeps the
    // pnpm-native source authoritative if both somehow appear).
    merge_yarnrc_catalogs(&mut out, project_root);
    if let Some(dir) = &workspace_root_dir
        && dir != project_root
    {
        merge_yarnrc_catalogs(&mut out, dir);
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

/// pnpm's built-in `gh:` alias → the GitHub Packages npm registry. A user
/// `namedRegistries.gh` entry overrides it (GHES repoints `gh` at an
/// enterprise host).
const BUILTIN_GH_REGISTRY: &str = "https://npm.pkg.github.com/";

/// A `namedRegistries` alias URL is accepted only when it's an absolute
/// http(s) URL — mirrors pnpm's `new URL(url)` + `http:`/`https:` protocol
/// check, catching the common typo of a bare host (`npm.work.example.com`).
/// The scheme-prefix form is dependency-free and sufficient: a truly
/// malformed-but-schemed URL surfaces later at fetch time.
fn is_valid_named_registry_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    matches!(rest, Some(authority) if !authority.is_empty())
}

/// Insert one `alias → url` entry, validating the URL. pnpm hard-errors on an
/// invalid `namedRegistries` URL; nub warn-drops it instead to keep install
/// resilient.
fn merge_named_registry(
    out: &mut std::collections::BTreeMap<String, String>,
    alias: String,
    url: String,
) {
    if is_valid_named_registry_url(&url) {
        out.insert(alias, url);
    } else {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_INVALID_NAMED_REGISTRY_URL,
            "namedRegistries alias '{alias}' maps to '{url}', which is not a valid http(s) URL — dropping it",
        );
    }
}

/// Discover the `namedRegistries` alias→URL map the resolver routes
/// `<alias>:<spec>` dependencies through.
///
/// GATED on `engine_context().named_registries_enabled` — a separate posture
/// from the pnpm-branded config reads because that one defaults `true` in
/// standalone aube, where activating this feature would break
/// default-preservation. Empty map = feature off; every `<alias>:` spec falls
/// through to its existing resolver, so standalone aube stays byte-identical.
///
/// Sources, ascending precedence (later overrides earlier per-alias):
/// 1. the built-in `gh:` → GitHub Packages;
/// 2. `namedRegistries` in pnpm's GLOBAL `config.yaml` (via
///    [`super::settings_context::load_global_config_yaml`], itself gated by the
///    global-scope pnpm posture);
/// 3. `namedRegistries` in the nearest workspace yaml
///    (`pnpm-workspace.yaml` / `aube-workspace.yaml`) — the project surface
///    wins over global, mirroring the settings layer's precedence.
///
/// Each URL is validated http(s); an invalid entry is warn-dropped (nub keeps
/// install resilient rather than hard-erroring like pnpm).
pub(crate) fn discover_named_registries(
    project_root: &std::path::Path,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::<String, String>::new();
    if !aube_util::engine_context().named_registries_enabled {
        return out;
    }

    // (1) built-in gh alias.
    out.insert("gh".to_string(), BUILTIN_GH_REGISTRY.to_string());

    // (2) global config.yaml, lower precedence than the workspace yaml.
    let global = super::settings_context::load_global_config_yaml();
    if let Some(value) = global.get("namedRegistries")
        && let Ok(map) =
            yaml_serde::from_value::<std::collections::BTreeMap<String, String>>(value.clone())
    {
        for (alias, url) in map {
            merge_named_registry(&mut out, alias, url);
        }
    }

    // (3) workspace yaml, highest precedence. Loaded from the walk-up dir when
    // present, else project_root — mirroring discover_catalogs.
    let workspace_yaml_dir = crate::dirs::find_workspace_yaml_root(project_root);
    let yaml_dir = workspace_yaml_dir.as_deref().unwrap_or(project_root);
    if let Ok((ws_config, _raw)) = aube_manifest::workspace::load_both(yaml_dir) {
        for (alias, url) in ws_config.named_registries {
            merge_named_registry(&mut out, alias, url);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.yarnrc.yml`'s default + named catalogs (yarn 4.10+) are discovered,
    /// keyed identically to the pnpm dialect (`default` for the unnamed
    /// catalog), so the resolver resolves `catalog:` / `catalog:<name>` deps
    /// in a yarn project without a `pnpm-workspace.yaml` in sight.
    #[test]
    fn discovers_yarnrc_default_and_named_catalogs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"y","dependencies":{"react":"catalog:","lodash":"catalog:legacy"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".yarnrc.yml"),
            "catalog:\n  react: ^18.3.1\ncatalogs:\n  legacy:\n    lodash: ^4.17.21\n",
        )
        .unwrap();

        let cats = discover_catalogs(dir.path()).unwrap();
        assert_eq!(
            cats.get("default").unwrap().get("react").unwrap(),
            "^18.3.1"
        );
        assert_eq!(
            cats.get("legacy").unwrap().get("lodash").unwrap(),
            "^4.17.21"
        );
    }

    /// A `.yarnrc.yml` with only non-catalog yarn config (the common case —
    /// registries, plugins) contributes nothing and never aborts discovery.
    #[test]
    fn yarnrc_without_catalogs_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"y"}"#).unwrap();
        std::fs::write(
            dir.path().join(".yarnrc.yml"),
            "nodeLinker: node-modules\nnpmRegistryServer: \"https://registry.npmjs.org\"\n",
        )
        .unwrap();

        assert!(discover_catalogs(dir.path()).unwrap().is_empty());
    }

    /// Serializes the tests that toggle the process-global
    /// `named_registries_enabled` gate so they can't observe each other's
    /// setting under cargo's parallel runner.
    static NAMED_REGISTRY_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct NamedRegistryGate {
        old: bool,
    }

    impl NamedRegistryGate {
        fn set(enabled: bool) -> Self {
            let old = aube_util::engine_context().named_registries_enabled;
            aube_util::update_engine_context(|c| c.named_registries_enabled = enabled);
            Self { old }
        }
    }

    impl Drop for NamedRegistryGate {
        fn drop(&mut self) {
            let old = self.old;
            aube_util::update_engine_context(|c| c.named_registries_enabled = old);
        }
    }

    #[test]
    fn named_registries_empty_when_gate_off() {
        let _serial = NAMED_REGISTRY_GATE.lock().unwrap();
        let _gate = NamedRegistryGate::set(false);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "namedRegistries:\n  work: https://npm.work.net/\n",
        )
        .unwrap();
        assert!(discover_named_registries(dir.path()).is_empty());
    }

    #[test]
    fn named_registries_builtin_gh_workspace_override_and_validation() {
        let _serial = NAMED_REGISTRY_GATE.lock().unwrap();
        let _gate = NamedRegistryGate::set(true);
        let dir = tempfile::tempdir().unwrap();
        // Workspace yaml overrides the built-in `gh` (GHES repoint), adds a
        // valid alias, and carries an invalid URL that must be warn-dropped.
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "namedRegistries:\n  \
             work: https://npm.work.net/\n  \
             gh: https://ghe.example/\n  \
             bad: npm.work.example.com\n",
        )
        .unwrap();

        let map = discover_named_registries(dir.path());
        // Workspace `gh` wins over the built-in default.
        assert_eq!(
            map.get("gh").map(String::as_str),
            Some("https://ghe.example/")
        );
        assert_eq!(
            map.get("work").map(String::as_str),
            Some("https://npm.work.net/")
        );
        // Invalid (scheme-less) URL was dropped, not inserted.
        assert!(!map.contains_key("bad"));
    }

    #[test]
    fn named_registries_builtin_gh_present_without_workspace_config() {
        let _serial = NAMED_REGISTRY_GATE.lock().unwrap();
        let _gate = NamedRegistryGate::set(true);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        // No project namedRegistries → the built-in gh default is still seeded.
        // (Assert presence, not the exact URL: a global pnpm config.yaml on the
        // host could legitimately repoint gh, which would still leave it keyed.)
        let map = discover_named_registries(dir.path());
        assert!(map.contains_key("gh"));
    }
}
