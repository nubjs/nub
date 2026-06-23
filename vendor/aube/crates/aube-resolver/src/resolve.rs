mod driver;
mod fetch;
mod seed;
mod vulnerable;

use crate::local_source::is_non_registry_specifier;
use crate::semver_util::version_satisfies;
use crate::{
    Error, FxHashMap, PeerContextOptions, ReadPackageHook, Resolver, apply_peer_contexts, catalog,
    hoist_auto_installed_peers,
};
use aube_lockfile::{DirectDep, LockedPackage, LockfileGraph};
use aube_manifest::PackageJson;
use aube_registry::VersionMetadata;
use std::collections::{BTreeMap, HashMap};

impl Resolver {
    /// Resolve all dependencies from a package.json.
    ///
    /// Uses batch-parallel BFS: each "wave" drains the queue, identifies
    /// uncached package names, fetches their packuments concurrently, then
    /// processes the entire batch before starting the next wave.
    pub async fn resolve(
        &mut self,
        manifest: &PackageJson,
        existing: Option<&LockfileGraph>,
    ) -> Result<LockfileGraph, Error> {
        self.resolve_workspace(
            &[(".".to_string(), manifest.clone())],
            existing,
            &HashMap::new(),
        )
        .await
    }

    /// Resolve all dependencies for a workspace (multiple importers).
    ///
    /// `manifests` is a list of (importer_path, PackageJson) — e.g. (".", root), ("packages/app", app).
    /// `workspace_packages` maps package name → version. Used both for
    /// explicit `workspace:` protocol resolution and for yarn/npm/bun
    /// style linkage where a bare semver range on a workspace-package
    /// name resolves to the local copy when its version satisfies the
    /// range.
    pub async fn resolve_workspace(
        &mut self,
        manifests: &[(String, PackageJson)],
        existing: Option<&LockfileGraph>,
        workspace_packages: &HashMap<String, String>,
    ) -> Result<LockfileGraph, Error> {
        // Run `readPackage` over each importer's own manifest before
        // seeding, matching pnpm — which fires the hook on workspace
        // project manifests, not just resolved registry packages. This
        // lets a pnpmfile rewrite an importer's own `dependencies` /
        // `devDependencies` / `optionalDependencies` / `peerDependencies`
        // (e.g. local `link:` wiring of monorepo packages) before the
        // resolver walks them. The registry-package hook still runs in
        // the BFS loop, so a dep *added* by the importer hook is itself
        // hooked when resolved, just like pnpm.
        let hooked_manifests = if let Some(hook) = self.read_package_hook.as_deref_mut() {
            let mut owned = manifests.to_vec();
            apply_read_package_to_importers(hook, &mut owned).await?;
            Some(owned)
        } else {
            None
        };
        let manifests = hooked_manifests.as_deref().unwrap_or(manifests);
        driver::ResolveDriver::new(self, manifests, existing, workspace_packages)
            .run()
            .await
    }

    /// Is `(name, range)` safe to speculatively prefetch against the
    /// registry?
    ///
    /// Returns false for any spec that won't go through the registry
    /// resolver at all — workspace/catalog/npm-alias/jsr ranges, local
    /// (`file:`/`link:`/`git:`) specifiers, and bare ranges that match
    /// a workspace package. Also false for any name listed in
    /// `pnpm.overrides`, since the override may rewrite the spec into
    /// one of the above and we can't cheaply tell ahead of time.
    fn is_prefetchable(
        &self,
        name: &str,
        range: &str,
        workspace_packages: &HashMap<String, String>,
    ) -> bool {
        let workspace_hit = workspace_packages
            .get(name)
            .is_some_and(|ws_v| version_satisfies(ws_v, range));
        !aube_util::pkg::is_workspace_spec(range)
            && !aube_util::pkg::is_catalog_spec(range)
            && !aube_util::pkg::is_npm_spec(range)
            && !aube_util::pkg::is_jsr_spec(range)
            && !is_non_registry_specifier(range)
            && !self.overrides.contains_key(name)
            && !workspace_hit
    }

    /// Build the final `LockfileGraph` from accumulated resolver state.
    ///
    /// Runs the catalog-pick materialization, temporarily hoists
    /// auto-installed peers when `auto_install_peers` is on, applies
    /// peer-context suffixes, then strips the hoisted importer entries
    /// again. Returns the post-peer-context graph ready for lockfile
    /// emission, with `importers` mirroring the manifests.
    fn finalize_resolved_graph(
        &self,
        importers: BTreeMap<String, Vec<DirectDep>>,
        resolved: BTreeMap<String, LockedPackage>,
        resolved_versions: &FxHashMap<String, Vec<String>>,
        resolved_times: BTreeMap<String, String>,
        skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>>,
        catalog_picks: BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<LockfileGraph, Error> {
        let resolved_catalogs =
            catalog::materialize_catalog_picks(catalog_picks, resolved_versions);

        let canonical = LockfileGraph {
            importers,
            packages: resolved,
            settings: aube_lockfile::LockfileSettings {
                auto_install_peers: self.auto_install_peers,
                exclude_links_from_lockfile: self.exclude_links_from_lockfile,
                // Tarball-URL recording is a lockfile-writer concern; the
                // resolver never populates URLs itself. Install flips this
                // on after the graph is built when the setting is active.
                lockfile_include_tarball_url: false,
            },
            // Stamp the resolver's overrides into the output graph so the
            // lockfile writer can round-trip them and the next install's
            // drift check can compare them against the manifest.
            overrides: self.overrides.clone(),
            ignored_optional_dependencies: self.ignored_optional_dependencies.clone(),
            times: resolved_times,
            skipped_optional_dependencies,
            catalogs: resolved_catalogs,
            // Resolver output is format-agnostic; the bun writer layer
            // defaults `configVersion` to 1 when emitting a fresh
            // lockfile.
            bun_config_version: None,
            // Fresh resolves don't carry over unknown blocks; the
            // install-side merge (`overlay_metadata_from`) copies
            // them back from the prior lockfile when round-tripping.
            patched_dependencies: BTreeMap::new(),
            patched_dependency_hashes: BTreeMap::new(),
            trusted_dependencies: Vec::new(),
            runtimes: BTreeMap::new(),
            extra_fields: BTreeMap::new(),
            workspace_extra_fields: BTreeMap::new(),
            // pnpm config checksums are an install-flow concern, stamped
            // onto the graph just before a pnpm-lock.yaml is written.
            // A fresh resolve leaves them unset.
            package_extensions_checksum: None,
            pnpmfile_checksum: None,
        };

        // Second pass: temporarily hoist every auto-installed peer to its
        // importer's direct deps so the peer-context pass below resolves
        // direct deps' peers from the importer scope — the same view
        // pnpm's `auto-install-peers=true` resolution has. The additions
        // are stripped again after `apply_peer_contexts`: pnpm keeps
        // auto-installed peers in the resolved graph/snapshots but never
        // writes them as importer specifiers or links them at the top
        // level of `node_modules/`. Skipped entirely when the setting is
        // off — matches pnpm, which leaves the importer's `dependencies`
        // untouched in that mode.
        let (hoisted, auto_installed_peers) = if self.auto_install_peers {
            hoist_auto_installed_peers(canonical)
        } else {
            (canonical, crate::AutoInstalledPeers::new())
        };

        // Third pass: compute peer-context suffixes for every reachable
        // package. See `apply_peer_contexts` for the details.
        let peer_options = PeerContextOptions {
            dedupe_peer_dependents: self.dedupe_peer_dependents,
            dedupe_peers: self.dedupe_peers,
            resolve_from_workspace_root: self.resolve_peers_from_workspace_root,
            peers_suffix_max_length: self.peers_suffix_max_length,
        };
        let _diag_peer =
            aube_util::diag::Span::new(aube_util::diag::Category::Resolver, "peer_context_apply");
        let mut contextualized = apply_peer_contexts(hoisted, &peer_options)?;
        crate::remove_auto_installed_peers(&mut contextualized, &auto_installed_peers);
        drop(_diag_peer);
        tracing::debug!(
            "peer-context pass produced {} contextualized packages",
            contextualized.packages.len()
        );
        Ok(contextualized)
    }
}

/// Apply the project's `readPackage` hook to each importer manifest in
/// place. Mirrors pnpm, which fires the hook on workspace-project
/// manifests, not just resolved registry packages. Honored edits are the
/// dependency maps (`dependencies`, `devDependencies`,
/// `optionalDependencies`, `peerDependencies`, and `peerDependenciesMeta`);
/// identity (`name`/`version`) edits are ignored — and, like the
/// registry-package path in the BFS loop, an identity rewrite emits a
/// `WARN_AUBE_HOOK_IDENTITY_REWRITTEN` warning so the discarded edit isn't
/// silent.
async fn apply_read_package_to_importers(
    hook: &mut dyn ReadPackageHook,
    manifests: &mut [(String, PackageJson)],
) -> Result<(), Error> {
    for (importer_path, manifest) in manifests.iter_mut() {
        let input = importer_to_version_metadata(manifest, importer_path)?;
        // Capture the (possibly synthesized) identity we hand the hook so an
        // attempted rewrite can be reported rather than dropped silently.
        let before_name = input.name.clone();
        let before_version = input.version.clone();
        let after = hook.read_package(input).await.map_err(|e| {
            Error::Registry(
                importer_label(importer_path, manifest),
                format!("readPackage hook: {e}"),
            )
        })?;
        if after.name != before_name || after.version != before_version {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_HOOK_IDENTITY_REWRITTEN,
                "[pnpmfile] readPackage rewrote importer {}@{} identity to {}@{}; \
                         aube ignores identity edits",
                before_name,
                before_version,
                after.name,
                after.version,
            );
        }
        apply_version_metadata_to_importer(manifest, after);
    }
    Ok(())
}

/// Build the `readPackage` hook input for an importer manifest. The hook
/// wire is [`VersionMetadata`] (the same shape the resolver hands the hook
/// for registry packages), so the manifest is round-tripped through JSON.
/// `name`/`version` are required by `VersionMetadata` yet optional on a
/// manifest (workspace roots routinely omit both) — inject inert defaults
/// so the conversion can't fail on a nameless root.
fn importer_to_version_metadata(
    manifest: &PackageJson,
    importer_path: &str,
) -> Result<VersionMetadata, Error> {
    let mut value = serde_json::to_value(manifest).map_err(|e| {
        Error::Registry(
            importer_path.to_string(),
            format!("readPackage hook: failed to serialize importer manifest: {e}"),
        )
    })?;
    if !value.get("name").is_some_and(serde_json::Value::is_string) {
        value["name"] = serde_json::Value::String(String::new());
    }
    if !value
        .get("version")
        .is_some_and(serde_json::Value::is_string)
    {
        value["version"] = serde_json::Value::String("0.0.0".to_string());
    }
    serde_json::from_value(value).map_err(|e| {
        Error::Registry(
            importer_path.to_string(),
            format!("readPackage hook: failed to build hook input from importer manifest: {e}"),
        )
    })
}

/// Copy the honored dependency-map edits from the hook's returned manifest
/// back onto the importer. Identity and registry-only fields are ignored.
fn apply_version_metadata_to_importer(manifest: &mut PackageJson, after: VersionMetadata) {
    manifest.dependencies = after.dependencies;
    manifest.dev_dependencies = after.dev_dependencies;
    manifest.optional_dependencies = after.optional_dependencies;
    manifest.peer_dependencies = after.peer_dependencies;
    // `peerDependenciesMeta` has no typed slot on `PackageJson`; it lives
    // in the flattened `extra` map. Reflect hook edits there so downstream
    // peer handling sees them, and drop the key when the hook cleared it so
    // a removal round-trips.
    if after.peer_dependencies_meta.is_empty() {
        manifest.extra.remove("peerDependenciesMeta");
    } else if let Ok(v) = serde_json::to_value(&after.peer_dependencies_meta) {
        manifest.extra.insert("peerDependenciesMeta".to_string(), v);
    }
}

/// Human-readable label for an importer in hook error messages: its
/// package name when present, else the importer path (`.` for the root).
fn importer_label(importer_path: &str, manifest: &PackageJson) -> String {
    match manifest.name.as_deref() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => importer_path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;

    /// Minimal in-process `readPackage` hook driven by a closure, so the
    /// importer-hook plumbing can be exercised without spawning a `node`
    /// child (the real host).
    struct MockHook<F>(F);

    impl<F> ReadPackageHook for MockHook<F>
    where
        F: FnMut(VersionMetadata) -> Result<VersionMetadata, String> + Send,
    {
        fn read_package<'a>(
            &'a mut self,
            pkg: VersionMetadata,
        ) -> Pin<Box<dyn Future<Output = Result<VersionMetadata, String>> + Send + 'a>> {
            let out = (self.0)(pkg);
            Box::pin(async move { out })
        }
    }

    fn manifest(name: Option<&str>) -> PackageJson {
        PackageJson {
            name: name.map(str::to_string),
            ..PackageJson::default()
        }
    }

    #[tokio::test]
    async fn applies_hook_edits_to_importer_self_manifest() {
        let mut manifests = vec![(".".to_string(), manifest(Some("root-pkg")))];
        let mut hook = MockHook(|mut pkg: VersionMetadata| {
            if pkg.name == "root-pkg" {
                pkg.dependencies
                    .insert("is-odd".to_string(), "3.0.1".to_string());
            }
            Ok(pkg)
        });
        apply_read_package_to_importers(&mut hook, &mut manifests)
            .await
            .unwrap();
        assert_eq!(
            manifests[0]
                .1
                .dependencies
                .get("is-odd")
                .map(String::as_str),
            Some("3.0.1")
        );
    }

    #[tokio::test]
    async fn applies_hook_per_importer_in_a_workspace() {
        // Each workspace member's own manifest is hooked independently —
        // the rewrite is keyed on the package name the hook is called with.
        let mut manifests = vec![
            (".".to_string(), manifest(Some("root"))),
            ("packages/app".to_string(), manifest(Some("app"))),
            ("packages/lib".to_string(), manifest(Some("lib"))),
        ];
        let mut hook = MockHook(|mut pkg: VersionMetadata| {
            // Only `app` links a local dep; the others are untouched.
            if pkg.name == "app" {
                pkg.dependencies
                    .insert("@scope/lib".to_string(), "link:../lib".to_string());
            }
            Ok(pkg)
        });
        apply_read_package_to_importers(&mut hook, &mut manifests)
            .await
            .unwrap();
        assert_eq!(
            manifests[1]
                .1
                .dependencies
                .get("@scope/lib")
                .map(String::as_str),
            Some("link:../lib")
        );
        assert!(manifests[0].1.dependencies.is_empty());
        assert!(manifests[2].1.dependencies.is_empty());
    }

    #[tokio::test]
    async fn nameless_root_is_still_passed_to_hook() {
        // Workspace roots routinely omit `name`/`version`; the hook must
        // still see (and be able to mutate) the manifest.
        let mut manifests = vec![(".".to_string(), manifest(None))];
        let mut hook = MockHook(|mut pkg: VersionMetadata| {
            pkg.dependencies
                .insert("marker".to_string(), "1.0.0".to_string());
            Ok(pkg)
        });
        apply_read_package_to_importers(&mut hook, &mut manifests)
            .await
            .unwrap();
        assert!(manifests[0].1.dependencies.contains_key("marker"));
    }

    #[tokio::test]
    async fn hook_error_surfaces_as_registry_error() {
        let mut manifests = vec![(".".to_string(), manifest(Some("x")))];
        let mut hook = MockHook(|_pkg: VersionMetadata| Err("boom".to_string()));
        let err = apply_read_package_to_importers(&mut hook, &mut manifests)
            .await
            .unwrap_err();
        match err {
            Error::Registry(name, msg) => {
                assert_eq!(name, "x");
                assert!(msg.contains("readPackage hook"), "got: {msg}");
                assert!(msg.contains("boom"), "got: {msg}");
            }
            other => panic!("expected Registry error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn importer_identity_rewrite_is_ignored_but_deps_apply() {
        // A hook that rewrites the importer's identity (name/version) while
        // also editing deps: the identity edit is discarded (and warned
        // about, mirroring the registry path), but the dep edit still lands.
        let mut manifests = vec![(".".to_string(), manifest(Some("orig")))];
        let mut hook = MockHook(|mut pkg: VersionMetadata| {
            pkg.name = format!("{}-local", pkg.name);
            pkg.version = "9.9.9".to_string();
            pkg.dependencies
                .insert("is-odd".to_string(), "3.0.1".to_string());
            Ok(pkg)
        });
        apply_read_package_to_importers(&mut hook, &mut manifests)
            .await
            .unwrap();
        // Identity rewrite is ignored — the importer keeps its own name.
        assert_eq!(manifests[0].1.name.as_deref(), Some("orig"));
        // The dependency edit is still honored.
        assert_eq!(
            manifests[0]
                .1
                .dependencies
                .get("is-odd")
                .map(String::as_str),
            Some("3.0.1")
        );
    }

    #[test]
    fn importer_to_version_metadata_injects_defaults_for_nameless_root() {
        let vm = importer_to_version_metadata(&manifest(None), ".").unwrap();
        assert_eq!(vm.name, "");
        assert_eq!(vm.version, "0.0.0");
    }

    #[test]
    fn importer_to_version_metadata_carries_all_dep_maps() {
        let mut m = manifest(Some("p"));
        m.dependencies.insert("a".into(), "1.0.0".into());
        m.dev_dependencies.insert("b".into(), "^2".into());
        m.optional_dependencies.insert("c".into(), "*".into());
        m.peer_dependencies.insert("d".into(), ">=3".into());
        let vm = importer_to_version_metadata(&m, ".").unwrap();
        assert_eq!(vm.dependencies.get("a").map(String::as_str), Some("1.0.0"));
        assert_eq!(vm.dev_dependencies.get("b").map(String::as_str), Some("^2"));
        assert_eq!(
            vm.optional_dependencies.get("c").map(String::as_str),
            Some("*")
        );
        assert_eq!(
            vm.peer_dependencies.get("d").map(String::as_str),
            Some(">=3")
        );
    }

    #[test]
    fn apply_version_metadata_keeps_dep_edits_and_ignores_identity() {
        let mut m = manifest(Some("orig"));
        let mut after = importer_to_version_metadata(&m, ".").unwrap();
        after.name = "changed".into();
        after.version = "9.9.9".into();
        after.dependencies.insert("x".into(), "1".into());
        apply_version_metadata_to_importer(&mut m, after);
        // We never copy identity back, so the importer keeps its own name.
        assert_eq!(m.name.as_deref(), Some("orig"));
        assert_eq!(m.dependencies.get("x").map(String::as_str), Some("1"));
    }
}
