//! `aube dedupe` — collapse redundant lockfile versions by re-resolving fresh.
//!
//! The resolver's `resolve(&manifest, existing)` reuses versions from `existing`
//! when they satisfy a range. Passing `existing = None` forces a fresh resolve
//! that always picks the highest version satisfying each range, which
//! naturally collapses duplicates left over from past adds/removes/updates.

use super::install;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, usage_rs::Args)]
pub struct DedupeArgs {
    /// Check whether dedupe would change the lockfile; don't write anything.
    ///
    /// Exits non-zero when dedupe would make changes — useful in CI.
    #[usage(long)]
    pub check: bool,
    #[usage(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[usage(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[usage(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(args: DedupeArgs) -> miette::Result<()> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    let cwd = crate::dirs::workspace_or_project_root()?;
    let lock = super::take_project_lock(&cwd)?;

    let manifest = super::load_manifest(&cwd.join("package.json"))?;

    // Read the existing lockfile purely for the diff. We do NOT pass it to
    // the resolver — passing `None` is what makes this "dedupe" instead of
    // "install": the resolver won't reuse stale pinned versions.
    let existing = aube_lockfile::parse_lockfile(&cwd, &manifest).ok();

    // Discover workspace packages so we resolve every importer, not just
    // the root package.json. Without this, dedupe would produce a graph
    // missing workspace importers, diff would be wrong, and the subsequent
    // `install::run` would see drift and silently re-resolve on top.
    let workspace_packages = aube_workspace::find_workspace_packages(&cwd)
        .into_diagnostic()
        .wrap_err("failed to discover workspace packages")?;
    let is_workspace = !workspace_packages.is_empty();

    let mut manifests: Vec<(String, aube_manifest::PackageJson)> =
        vec![(".".to_string(), manifest.clone())];
    let mut ws_package_versions: HashMap<String, String> = HashMap::new();

    if is_workspace {
        for pkg_dir in &workspace_packages {
            let pkg_manifest = aube_manifest::PackageJson::from_path(&pkg_dir.join("package.json"))
                .map_err(miette::Report::new)
                .wrap_err_with(|| format!("failed to read {}/package.json", pkg_dir.display()))?;

            let rel_path = pkg_dir
                .strip_prefix(&cwd)
                .unwrap_or(pkg_dir)
                .to_string_lossy()
                .to_string();

            if let Some(name) = &pkg_manifest.name {
                let version = pkg_manifest.version.as_deref().unwrap_or("0.0.0");
                ws_package_versions.insert(name.clone(), version.to_string());
            }

            manifests.push((rel_path, pkg_manifest));
        }
    }

    let workspace_catalogs = super::load_workspace_catalogs(&cwd)?;
    let mut resolver = super::build_resolver(&cwd, &manifest, workspace_catalogs)?;
    let mut graph = resolver
        .resolve_workspace(&manifests, None, &ws_package_versions)
        .await
        .map_err(miette::Report::new)
        .wrap_err("failed to resolve dependencies")?;

    let (removed, added) = diff_graphs(existing.as_ref(), &graph);

    // No changes: report and exit cleanly.
    if removed.is_empty() && added.is_empty() {
        eprintln!(
            "Lockfile is already deduped ({} packages)",
            graph.packages.len()
        );
        return Ok(());
    }

    // Changes would happen. Report the diff.
    for dep_path in &removed {
        eprintln!("  - {dep_path}");
    }
    for dep_path in &added {
        eprintln!("  + {dep_path}");
    }
    eprintln!(
        "Dedupe: {} removed, {} added (net {} packages)",
        removed.len(),
        added.len(),
        added.len() as i64 - removed.len() as i64,
    );

    if args.check {
        return Err(miette!("dedupe --check: lockfile is not deduped"));
    }

    install::finalize_lockfile_graph(&cwd, &mut graph, &manifest, false, None).await?;
    super::write_and_log_lockfile(&cwd, &graph, &manifest)?;

    // Resync node_modules against the new lockfile.
    install::run_with_project_lock(
        install::InstallOptions::with_mode(super::chained_frozen_mode(install::FrozenMode::Prefer)),
        &lock,
    )
    .await?;

    Ok(())
}

/// Diff two lockfile graphs by dep_path. Returns `(removed, added)` — packages
/// present in `old` but not `new`, and vice versa. Versions that are in both
/// are omitted (they're untouched).
///
/// Only packages the lockfile writer actually serializes participate:
/// `link:`/`exec:` entries are excluded, exactly mirroring the writer's
/// skip set for the `packages:`/`snapshots:` sections (`pnpm/write.rs`),
/// so a key that can never reach the lockfile can never be reported as a
/// change. The case that made this bite is workspace-member links, where
/// the two graphs are also asymmetric: the parser synthesizes a
/// `<name>@link+<hash>` package for every workspace link it reads back,
/// while a fresh resolve binds members by version and records no package
/// entry at all — so dedupe reported them as "removed" on every run
/// while the lockfile stayed byte-identical, and `--check` failed
/// forever. (`exec:` and non-member `link:` deps resolve symmetrically;
/// they are excluded on the writer-parity ground alone.)
fn diff_graphs(
    existing: Option<&aube_lockfile::LockfileGraph>,
    new: &aube_lockfile::LockfileGraph,
) -> (Vec<String>, Vec<String>) {
    fn serialized_keys(pkgs: &BTreeMap<String, aube_lockfile::LockedPackage>) -> BTreeSet<&String> {
        use aube_lockfile::LocalSource;
        pkgs.iter()
            .filter(|(_, pkg)| {
                !matches!(
                    pkg.local_source,
                    Some(LocalSource::Link(_) | LocalSource::Exec(_))
                )
            })
            .map(|(key, _)| key)
            .collect()
    }

    let empty: BTreeMap<String, aube_lockfile::LockedPackage> = BTreeMap::new();
    let old_keys = serialized_keys(existing.map(|g| &g.packages).unwrap_or(&empty));
    let new_keys = serialized_keys(&new.packages);

    let removed: Vec<String> = old_keys
        .difference(&new_keys)
        .map(|s| s.to_string())
        .collect();
    let added: Vec<String> = new_keys
        .difference(&old_keys)
        .map(|s| s.to_string())
        .collect();
    (removed, added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{LockedPackage, LockfileGraph};

    fn pkg(dep_path: &str, name: &str, version: &str, alias_of: Option<&str>) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            dep_path: dep_path.to_string(),
            alias_of: alias_of.map(str::to_string),
            ..Default::default()
        }
    }

    fn graph(pkgs: Vec<LockedPackage>) -> LockfileGraph {
        let mut g = LockfileGraph::default();
        for p in pkgs {
            g.packages.insert(p.dep_path.clone(), p);
        }
        g
    }

    /// Parse `yaml` as a pnpm lockfile, so the fixture below exercises the
    /// real reader rather than a hand-built stand-in — `diff_graphs` never
    /// reads `alias_of`, so only the reader's own output can demonstrate
    /// that the orphan sweep happened.
    fn parse_pnpm(yaml: &str) -> LockfileGraph {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(&path, yaml).unwrap();
        aube_lockfile::pnpm::parse(&path).unwrap()
    }

    /// The #578 contract, end to end from the reader: an `npm:` alias whose
    /// target nothing else reaches must parse to the same key set a fresh
    /// resolve emits, so `dedupe` sees no change. The reader used to leave
    /// the canonical `is-number@7.0.0` beside the alias clone, which made
    /// this diff report a removal on every run and `--check` fail forever
    /// on a byte-identical lockfile. Fails if that sweep regresses.
    #[test]
    fn an_orphan_alias_target_is_swept_so_dedupe_sees_no_change() {
        let parsed = parse_pnpm(
            r#"
lockfileVersion: '9.0'

importers:

  .:
    dependencies:
      number-alias:
        specifier: npm:is-number@7.0.0
        version: is-number@7.0.0

packages:

  is-number@7.0.0:
    resolution: {integrity: sha512-fake}

snapshots:

  is-number@7.0.0: {}
"#,
        );
        // What the resolver emits for this manifest: the alias key only.
        let fresh = graph(vec![pkg(
            "number-alias@7.0.0",
            "number-alias",
            "7.0.0",
            Some("is-number"),
        )]);
        assert_eq!(
            diff_graphs(Some(&parsed), &fresh),
            (Vec::new(), Vec::new()),
            "parsed keys {:?} must match a fresh resolve",
            parsed.packages.keys().collect::<Vec<_>>()
        );
    }

    /// The shape the reader must never hand back: a lingering canonical
    /// entry the fresh resolve has no counterpart for is exactly what
    /// reopens #578, so pin that this diff would in fact report it.
    #[test]
    fn a_lingering_alias_target_would_report_a_phantom_removal() {
        let parsed = graph(vec![
            pkg("is-number@7.0.0", "is-number", "7.0.0", None),
            pkg(
                "number-alias@7.0.0",
                "number-alias",
                "7.0.0",
                Some("is-number"),
            ),
        ]);
        let fresh = graph(vec![pkg(
            "number-alias@7.0.0",
            "number-alias",
            "7.0.0",
            Some("is-number"),
        )]);
        let (removed, added) = diff_graphs(Some(&parsed), &fresh);
        assert_eq!(removed, vec!["is-number@7.0.0".to_string()]);
        assert!(added.is_empty());
    }

    /// The counterpart the sweep must NOT touch: `wrap-ansi` depends on
    /// `string-width@4.2.3` directly, so the canonical entry is live even
    /// though `string-width-cjs` also aliases it (the real `@isaacs/cliui`
    /// shape). An over-eager sweep drops it here and the diff reports an
    /// addition the lockfile never makes.
    #[test]
    fn an_alias_target_a_second_consumer_needs_survives_the_sweep() {
        let parsed = parse_pnpm(
            r#"
lockfileVersion: '9.0'

importers:

  .:
    dependencies:
      string-width-cjs:
        specifier: npm:string-width@^4.2.0
        version: string-width@4.2.3
      wrap-ansi:
        specifier: ^7.0.0
        version: wrap-ansi@7.0.0

packages:

  string-width@4.2.3:
    resolution: {integrity: sha512-fake1}

  wrap-ansi@7.0.0:
    resolution: {integrity: sha512-fake2}

snapshots:

  string-width@4.2.3: {}

  wrap-ansi@7.0.0:
    dependencies:
      string-width: 4.2.3
"#,
        );
        let fresh = graph(vec![
            pkg("string-width@4.2.3", "string-width", "4.2.3", None),
            pkg(
                "string-width-cjs@4.2.3",
                "string-width-cjs",
                "4.2.3",
                Some("string-width"),
            ),
            pkg("wrap-ansi@7.0.0", "wrap-ansi", "7.0.0", None),
        ]);
        assert_eq!(
            diff_graphs(Some(&parsed), &fresh),
            (Vec::new(), Vec::new()),
            "parsed keys {:?} must match a fresh resolve",
            parsed.packages.keys().collect::<Vec<_>>()
        );
    }
}
