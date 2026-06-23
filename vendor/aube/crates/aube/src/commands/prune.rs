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
use clap::Args;
use miette::{Context, IntoDiagnostic};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

pub const AFTER_LONG_HELP: &str = "\
Global store cleanup: use `aube store prune` to clean unreferenced files from the global
content-addressable store.";

#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Remove devDependencies from node_modules
    #[arg(long, short = 'P', visible_alias = "production")]
    pub prod: bool,

    /// Also remove optionalDependencies
    #[arg(long)]
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

    // Per-importer set of top-level package names that should stay
    // in `<importer>/node_modules/`.
    let allowed_top_level: BTreeMap<String, HashSet<String>> = filtered
        .importers
        .iter()
        .map(|(path, deps)| {
            let names: HashSet<String> = deps.iter().map(|d| d.name.clone()).collect();
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
        prune_top_level(&nm, allowed, preserve_leaf.as_deref(), &mut stats)?;

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
                if !allowed.contains(&full) {
                    super::remove_existing(&inner.path())?;
                    stats.top_level += 1;
                }
            }
            if std::fs::read_dir(&path).into_diagnostic()?.next().is_none() {
                let _ = std::fs::remove_dir(&path);
            }
        } else if !allowed.contains(name.as_ref()) {
            super::remove_existing(&path)?;
            stats.top_level += 1;
        }
    }
    Ok(())
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
