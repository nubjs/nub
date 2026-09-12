//! `aube prune` — remove extraneous packages from `node_modules/`.
//!
//! Matches pnpm's semantics:
//! - `aube prune` removes orphaned entries (anything in `node_modules/` or
//!   `node_modules/.aube/` that isn't reachable from the lockfile).
//! - `aube prune --prod` additionally drops `devDependencies`.
//! - `aube prune --no-optional` additionally drops `optionalDependencies`.
//!
//! **Does not modify the lockfile.** Only removes files from `node_modules/`.
//!
//! The heavy lifting is done by `LockfileGraph::filter_deps`, which runs the
//! BFS across all workspace importers and returns a reachable-set
//! `LockfileGraph` given a predicate. We then walk `node_modules/` and delete
//! anything outside that set.

use aube_lockfile::DepType;
use aube_lockfile::dep_path_filename::{
    DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH, dep_path_to_filename,
};
use miette::{Context, IntoDiagnostic};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

pub const AFTER_LONG_HELP: &str = "\
Global store cleanup: use `aube store prune` to clean unreferenced files from the global
content-addressable store.";

#[derive(Debug, usage_rs::Args)]
pub struct PruneArgs {
    /// Remove devDependencies from node_modules
    #[usage(long, long = "production", short = 'P')]
    pub prod: bool,

    /// Also remove optionalDependencies
    #[usage(long)]
    pub no_optional: bool,
}

pub async fn run(args: PruneArgs) -> miette::Result<()> {
    let cwd = crate::dirs::project_root()?;
    let _lock = super::take_project_lock(&cwd)?;

    let manifest = super::load_manifest(&cwd.join("package.json"))?;

    let graph = aube_lockfile::parse_lockfile(&cwd, &manifest)
        .map_err(miette::Report::new)
        .wrap_err(format!(
            "failed to read lockfile — run `{}` first",
            aube_util::cmd("install")
        ))?;

    // Build the filtered graph via the existing BFS helper.
    let filtered = graph.filter_deps(|dep| {
        if args.prod && dep.dep_type == DepType::Dev {
            return false;
        }
        if args.no_optional && dep.dep_type == DepType::Optional {
            return false;
        }
        true
    });

    // Set of on-disk `.aube/` entry names that should stay. Built by
    // routing each reachable dep_path through the same filename
    // encoder the linker uses, so the directory names on disk match
    // what we're comparing against here.
    let allowed_dep_paths: HashSet<String> = filtered
        .packages
        .keys()
        .map(|dp| dep_path_to_filename(dp, DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH))
        .collect();

    // Whether the layout on disk flattens transitive deps into the
    // importer's top-level `node_modules/`. The hoisted linker
    // materializes the whole reachable closure at top level;
    // `shamefully-hoist` promotes every package there under the isolated
    // linker. In both, a direct-deps-only keep-set deletes reachable prod
    // transitives that legitimately live at top level (issue #241). Plain
    // isolated keeps only direct-dep symlinks at top level, so its keep-set
    // stays direct-deps-only — matching pnpm exactly (a package that is
    // both a direct devDep and a prod transitive loses its top-level
    // symlink on `prune --prod` there, rather than being over-retained).
    //
    // The install records the node-linker it actually used in the layout
    // state; prefer it, because it reflects a `--node-linker=hoisted` CLI
    // override that was never written to `.npmrc` (re-resolving settings
    // here would miss it). The state does not record `shamefully-hoist`,
    // so resolve that from settings. Fall back to full settings resolution
    // when there is no state (a tree installed by another tool).
    let flattens_transitives = match crate::state::read_state_layout_linker(&cwd) {
        Some(crate::state::InstallLayoutMode::Hoisted) => true,
        Some(crate::state::InstallLayoutMode::Isolated) => {
            super::with_settings_ctx(&cwd, aube_settings::resolved::shamefully_hoist)
        }
        None => super::with_settings_ctx(&cwd, |ctx| {
            matches!(
                aube_settings::resolved::node_linker(ctx),
                aube_settings::resolved::NodeLinker::Hoisted
            ) || aube_settings::resolved::shamefully_hoist(ctx)
        }),
    };

    // Per-importer set of top-level package names that should stay in
    // `<importer>/node_modules/`. Under a flattening layout, union the full
    // reachable-closure NAME set (the same set the store sweep keeps) so
    // hoisted transitives survive. A name-keyed keep-set can still retain a
    // top-level symlink that points at a virtual-store *version* the store
    // sweep removed (a direct devDep and a prod transitive sharing a name
    // at different versions); the dangling-symlink sweep in
    // `prune_top_level` cleans that, so the keep-set never leaves a dangler.
    let closure_names: HashSet<String> = if flattens_transitives {
        filtered.packages.values().map(|p| p.name.clone()).collect()
    } else {
        HashSet::new()
    };
    let allowed_top_level: BTreeMap<String, HashSet<String>> = filtered
        .importers
        .iter()
        .map(|(path, deps)| {
            let mut names: HashSet<String> = deps.iter().map(|d| d.name.clone()).collect();
            names.extend(closure_names.iter().cloned());
            (path.clone(), names)
        })
        .collect();

    let mut stats = PruneStats::default();

    // Walk the resolved virtualStoreDir. Root importer's store is
    // shared by the whole workspace, so this only needs to happen once.
    // `resolve_virtual_store_dir_for_cwd` honors the setting (or falls
    // back to `<modulesDir>/.aube` when unset) so prune lands on the
    // same directory the linker wrote to.
    let modules_dir_name = super::resolve_modules_dir_name_for_cwd(&cwd);
    let aube_dir = super::resolve_virtual_store_dir_for_cwd(&cwd);
    if aube_dir.is_dir() {
        prune_aube_store(&aube_dir, &allowed_dep_paths, &mut stats)?;
    }

    // Walk each importer's top-level node_modules/ and remove stale direct
    // entries. `filtered.importers` has `"."` for the root; workspace entries
    // are relative paths like `"packages/app"`.
    for (importer_path, allowed) in &allowed_top_level {
        let importer_dir = if importer_path == "." {
            cwd.clone()
        } else {
            cwd.join(importer_path)
        };
        let nm = importer_dir.join(&modules_dir_name);
        if !nm.is_dir() {
            continue;
        }
        // When `virtualStoreDir` lives directly under this importer's
        // `modulesDir` with a non-dotfile name (e.g. `vstore`), the
        // `starts_with('.')` short-circuit in `prune_top_level` won't
        // cover it and the sweep would delete the whole virtual store.
        // Mirror the `aube_dir_leaf` guard the linker already has for
        // the same scenario.
        let preserve_leaf: Option<std::ffi::OsString> = if aube_dir.parent() == Some(nm.as_path()) {
            aube_dir.file_name().map(|s| s.to_owned())
        } else {
            None
        };
        // The virtual-store leaf (e.g. `.store`) scopes the dangling-symlink
        // sweep to store-pointing symlinks, sparing `link:`/`portal:` deps.
        prune_top_level(
            &nm,
            allowed,
            preserve_leaf.as_deref(),
            aube_dir.file_name(),
            &mut stats,
        )?;

        // Clean any .bin/ entries that now point at nothing.
        let bin = nm.join(".bin");
        if bin.is_dir() {
            prune_dangling_bins(&bin, &mut stats)?;
        }
    }

    // Summary
    if stats.is_empty() {
        eprintln!("Nothing to prune");
    } else {
        eprintln!(
            "Pruned {} entr{}: {} top-level, {} from .aube, {} dangling .bin",
            stats.total(),
            if stats.total() == 1 { "y" } else { "ies" },
            stats.top_level,
            stats.aube_store,
            stats.bins,
        );
    }

    Ok(())
}

#[derive(Default, Debug)]
struct PruneStats {
    top_level: usize,
    aube_store: usize,
    bins: usize,
}

impl PruneStats {
    fn total(&self) -> usize {
        self.top_level + self.aube_store + self.bins
    }
    fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// Walk `node_modules/.aube/` and remove any entry whose name isn't in
/// `allowed`. All entries live as single flat directories under
/// `.aube/` — scoped packages are encoded as `@scope+name@version`
/// rather than nested under `@scope/`, matching `dep_path_to_filename`.
fn prune_aube_store(
    aube_dir: &Path,
    allowed: &HashSet<String>,
    stats: &mut PruneStats,
) -> miette::Result<()> {
    for entry in std::fs::read_dir(aube_dir).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        // `.aube/node_modules/` is the hidden-hoist tree populated by
        // the linker's `hoist` + `hoistPattern` pass. It's rebuilt
        // from scratch on every install, so prune should leave it
        // alone — treating its root as a stale dep_path would delete
        // every hidden-hoist symlink for the current graph.
        if name == "node_modules" {
            continue;
        }
        if !allowed.contains(name.as_ref()) {
            super::remove_existing(&entry.path())?;
            stats.aube_store += 1;
        }
    }
    Ok(())
}

/// Walk a `node_modules/` directory and remove top-level entries that
/// aren't in `allowed`. Skips all dotfile/dotdir internals. When
/// `preserve_leaf` is `Some`, any entry whose name matches is also
/// preserved — this is how prune avoids deleting a non-dotfile
/// `virtualStoreDir` (e.g. `node_modules/vstore`) that sits directly
/// under the walked `nm`.
fn prune_top_level(
    nm: &Path,
    allowed: &HashSet<String>,
    preserve_leaf: Option<&std::ffi::OsStr>,
    store_leaf: Option<&std::ffi::OsStr>,
    stats: &mut PruneStats,
) -> miette::Result<()> {
    for entry in std::fs::read_dir(nm).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let name = entry.file_name();
        if Some(name.as_os_str()) == preserve_leaf {
            continue;
        }
        let name = name.to_string_lossy();

        // Skip aube/pnpm internals
        if name.starts_with('.') {
            continue;
        }

        let path = entry.path();

        if name.starts_with('@') && path.is_dir() && !path.is_symlink() {
            // Scoped: iterate one level deeper.
            for inner in std::fs::read_dir(&path).into_diagnostic()? {
                let inner = inner.into_diagnostic()?;
                let inner_name = inner.file_name();
                let full = format!("{name}/{}", inner_name.to_string_lossy());
                if !allowed.contains(&full) || is_dangling_store_symlink(&inner.path(), store_leaf)
                {
                    super::remove_existing(&inner.path())?;
                    stats.top_level += 1;
                }
            }
            if std::fs::read_dir(&path).into_diagnostic()?.next().is_none() {
                let _ = std::fs::remove_dir(&path);
            }
        } else if !allowed.contains(name.as_ref()) || is_dangling_store_symlink(&path, store_leaf) {
            super::remove_existing(&path)?;
            stats.top_level += 1;
        }
    }
    Ok(())
}

/// A top-level entry that is a broken symlink INTO the virtual store.
/// The keep-set is name-keyed, so a name-matched top-level symlink can
/// still point at a virtual-store version the store sweep just removed
/// (a direct devDep and a prod transitive sharing a name at different
/// versions). Such an entry is swept regardless of the keep-set so prune
/// never leaves a dangler. The store-leaf check is load-bearing: a
/// `link:`/`portal:` dep is a bare symlink to an arbitrary external path
/// (often a sibling build output that may be absent) — it is a
/// legitimately installed direct dep and must survive even while
/// dangling, so only danglers pointing into the virtual store are swept.
/// Real directories (the hoisted layout) are not symlinks, so they are
/// unaffected.
fn is_dangling_store_symlink(path: &Path, store_leaf: Option<&std::ffi::OsStr>) -> bool {
    let Some(leaf) = store_leaf else { return false };
    let Ok(meta) = path.symlink_metadata() else {
        return false;
    };
    if !meta.file_type().is_symlink() || path.exists() {
        return false;
    }
    matches!(std::fs::read_link(path), Ok(t) if t.components().any(|c| c.as_os_str() == leaf))
}

/// Remove any `.bin/` entry whose symlink target no longer resolves.
fn prune_dangling_bins(bin: &Path, stats: &mut PruneStats) -> miette::Result<()> {
    for entry in std::fs::read_dir(bin).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();

        // Only touch symlinks — some installs leave real files in .bin/
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if !meta.file_type().is_symlink() {
            continue;
        }

        // `.exists()` follows the link; returns false for dangling ones.
        if !path.exists() && std::fs::remove_file(&path).is_ok() {
            stats.bins += 1;
        }
    }
    Ok(())
}
