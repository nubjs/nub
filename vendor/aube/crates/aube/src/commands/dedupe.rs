//! `aube dedupe` — collapse redundant lockfile versions by re-resolving fresh.
//!
//! The resolver's `resolve(&manifest, existing)` reuses versions from `existing`
//! when they satisfy a range. Passing `existing = None` forces a fresh resolve
//! that always picks the highest version satisfying each range, which
//! naturally collapses duplicates left over from past adds/removes/updates.

use super::install;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Args)]
pub struct DedupeArgs {
    /// Check whether dedupe would change the lockfile; don't write anything.
    ///
    /// Exits non-zero when dedupe would make changes — useful in CI.
    #[arg(long)]
    pub check: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
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
    fn serialized_keys(
        pkgs: &BTreeMap<String, aube_lockfile::LockedPackage>,
    ) -> BTreeSet<&String> {
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
