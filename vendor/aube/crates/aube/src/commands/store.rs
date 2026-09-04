//! `aube store` — inspect and manage the global content-addressable store.
//!
//! Mirrors `pnpm store`:
//!
//! - `aube store path` — print the store-version directory (aube-owned
//!   by default: `$XDG_DATA_HOME/aube/store/v1/`, falling back to
//!   `~/.local/share/aube/store/v1/`). This directory contains both
//!   `files/` (the CAS shards) and `index/` (the cached package
//!   indexes), so a single backup or Docker BuildKit cache mount of
//!   this path captures the whole store — matching `pnpm store path`'s
//!   granularity (which prints e.g. `~/.pnpm-store/v11/`).
//! - `aube store add <pkg>…` — resolve each spec against the registry, fetch
//!   the tarball, and import it into the global CAS. Pre-warms the store
//!   without touching any project's `node_modules/`.
//! - `aube store prune` — mark global virtual-store entries reachable from
//!   registered projects, remove the rest, sweep the extracted-tree tier at
//!   `<store>/v1/trees/` and the global virtual store's pre-`v1` layout the
//!   same way, then remove unreferenced CAS files. CAS pruning uses hardlink
//!   counts where available and cached package indexes on reflink
//!   filesystems. `--dry-run` reports the same totals without deleting
//!   anything.
//! - `aube store status` — verify every file referenced by a cached package
//!   index still exists in the store and its BLAKE3 hash matches. Exits 0
//!   when everything is consistent, 1 when any corruption is found.
//!
//! None of these subcommands touch `node_modules/`, the lockfile, or the
//! project manifest, so they deliberately skip the project lock and the
//! auto-install check.

use crate::commands::{make_client, packument_full_cache_dir, resolve_version, split_name_spec};
use miette::{IntoDiagnostic, miette};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, usage_rs::Args)]
pub struct StoreArgs {
    #[usage(subcommand)]
    pub command: StoreCommand,
}

#[derive(Debug, usage_rs::Subcommands)]
pub enum StoreCommand {
    /// Add one or more packages to the global store without linking them
    /// into any project.
    ///
    /// Each argument is a package spec: `lodash`, `lodash@4.17.21`,
    /// `react@next`, or `express@^4`.
    Add {
        /// Package specs to fetch into the store.
        #[usage(arg, required)]
        packages: Vec<String>,
    },
    /// Show the store path.
    Path,
    /// Remove unreferenced packages from the global store.
    ///
    /// Operates on the store printed by `aube store path`; it does not touch
    /// project node_modules directories, manifests, or lockfiles.
    ///
    /// It removes global virtual-store graph entries not referenced by any
    /// registered project. Entries older releases wrote outside the versioned
    /// namespace are swept too, against the project registry those releases
    /// kept, and held for a grace period first. It sweeps the extracted-tree
    /// cache on the same evidence and with the same hold. It then prunes
    /// content-store files.
    ///
    /// On reflink filesystems such as APFS or btrfs, link counts cannot prove
    /// project reachability, so content-store pruning relies on cached package
    /// indexes. Global virtual-store reachability comes from project links.
    Prune(PruneCliArgs),
    /// Verify the store against cached package indexes.
    ///
    /// Confirms every file referenced by a cached package index is
    /// still present in the store and that its BLAKE3 hash matches.
    /// Exits non-zero when any corruption is detected.
    Status,
}

// `PruneArgs` is part of the published Rust API, so keep its original
// constructible shape while exposing JSON as CLI-only state.
static PRUNE_JSON_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, usage_rs::Args)]
pub struct PruneCliArgs {
    /// Do not actually delete anything; report what would be pruned.
    #[usage(long)]
    dry_run: bool,
    /// Emit the dry-run plan as one machine-readable JSON document.
    #[usage(long, requires = "--dry-run")]
    json: bool,
}

#[derive(Debug)]
pub struct PruneArgs {
    /// Do not actually delete anything; report what would be pruned.
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PruneReport {
    schema_version: u32,
    dry_run: bool,
    mutation_roots: Vec<MutationRoot>,
    actions: Vec<PlannedAction>,
    global_virtual_store: GvsStats,
    legacy_global_virtual_store: LegacyGvsStats,
    extracted_trees: TreesStats,
    content_store: CasStats,
    reclaimable_bytes_upper_bound: u64,
    warnings: Vec<StructuredWarning>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationRoot {
    kind: &'static str,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannedAction {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GvsStats {
    entries: usize,
    bytes_upper_bound: u64,
    stale_project_records: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyGvsStats {
    entries: usize,
    bytes_upper_bound: u64,
    /// Unreferenced but still inside the grace window, so held rather than
    /// planned for removal.
    deferred_entries: usize,
    /// The old registry and its sweep state, retired once the layout holds no
    /// entry left to protect. Zero until then.
    bookkeeping_paths: usize,
    /// The old registry names no project that still resolves while the layout
    /// holds entries, so it was not swept at all.
    skipped_no_records: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TreesStats {
    entries: usize,
    bytes_upper_bound: u64,
    /// Unreferenced but still inside the grace window, so held rather than
    /// planned for removal. Always the whole unreferenced set on a `--dry-run`
    /// that no real prune preceded: the window's clock only starts when a real
    /// prune writes the state file.
    deferred_entries: usize,
    /// The registry named no project that still resolves while the tier holds
    /// entries, so it was not swept at all.
    skipped_no_projects: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CasStats {
    files: usize,
    bytes_upper_bound: u64,
}

#[derive(Debug, Serialize)]
struct StructuredWarning {
    code: &'static str,
    message: String,
}

#[derive(Debug, Default)]
struct CasPrunePlan {
    paths: Vec<std::path::PathBuf>,
    files: Vec<super::gvs_registry::CandidateFile>,
}

/// One entry that has been unreferenced long enough to remove. Shared by the
/// two tiers that hold before deleting: the extracted-tree cache and the
/// global virtual store's pre-`v1` layout.
#[derive(Debug)]
struct ExpiredEntry {
    path: std::path::PathBuf,
    name: String,
    /// When the entry was FIRST seen unreferenced. Kept so a removal that
    /// fails re-records the original sighting instead of restarting the
    /// window, which would make an undeletable entry immortal.
    first_seen: u64,
}

/// Plan for the global virtual store's pre-`v1` layout: the entries earlier
/// releases wrote directly under the store root, before
/// `GVS_REGISTRY_NAMESPACE_VERSION` moved the whole tier into a versioned
/// child. Nothing writes there any more and no other sweep can see it.
#[derive(Debug, Default)]
struct LegacyGvsPrunePlan {
    /// The pre-`v1` root. `None` when the global virtual store does not sit in
    /// a versioned child, in which case there is no earlier layout and — the
    /// point of the option — no licence to sweep the store's parent.
    root: Option<std::path::PathBuf>,
    entries: Vec<ExpiredEntry>,
    /// Unreferenced entries still inside the grace window, name → first seen.
    deferred: HashMap<String, u64>,
    /// The old project registry, its sweep state, and any crashed install's
    /// leftovers. Populated only when every entry of the layout is planned for
    /// removal: the records are the only evidence protecting what remains.
    bookkeeping: Vec<std::path::PathBuf>,
    files: Vec<super::gvs_registry::CandidateFile>,
    vanished_files: Vec<std::path::PathBuf>,
    swept: bool,
    /// Reported, never acted on: no record of the old registry names a project
    /// that still resolves, while the layout holds entries.
    skipped_no_records: bool,
}

#[derive(Debug, Default)]
struct TreesPrunePlan {
    entries: Vec<ExpiredEntry>,
    /// Unreferenced entries still inside the grace window, name → first seen.
    /// Written back verbatim as the next state file.
    deferred: HashMap<String, u64>,
    files: Vec<super::gvs_registry::CandidateFile>,
    vanished_files: Vec<std::path::PathBuf>,
    /// A sweep actually ran, so applying this plan must rewrite the state
    /// file even when it is empty — that rewrite is what stops the file
    /// growing without bound.
    swept: bool,
    /// Reported, never acted on: the registry knows of no live project while
    /// the tier holds entries.
    skipped_no_projects: bool,
}

pub async fn run(args: StoreArgs) -> miette::Result<()> {
    match args.command {
        StoreCommand::Add { packages } => add(packages).await,
        StoreCommand::Path => path(),
        StoreCommand::Prune(a) => {
            PRUNE_JSON_REQUESTED.store(a.json, Ordering::Relaxed);
            prune(PruneArgs { dry_run: a.dry_run })
        }
        StoreCommand::Status => status(),
    }
}

/// Anchored at the WORKSPACE root, matching what `install` resolves its store
/// against — `.npmrc` and `pnpm-workspace.yaml` discovery does not walk up, so
/// anchoring at the nearest `package.json` made `store path` report the DEFAULT
/// store from inside a workspace member while installs from that same member
/// used the root's `store-dir` override. Real pnpm reports the override from
/// both, so the narrower anchor was also a parity gap. Falls back to
/// `project_root_or_cwd` so these commands still work outside a package tree,
/// where the workspace walk-up has nothing to find.
fn open_store() -> miette::Result<aube_store::Store> {
    let cwd = crate::dirs::workspace_or_project_root()
        .or_else(|_| crate::dirs::project_root_or_cwd())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::commands::open_store(&cwd)
}

/// Same WORKSPACE-root anchoring as [`open_store`], for the same reason:
/// `path` and `prune` must resolve the store an install from a workspace
/// member would use, not the default one.
fn open_store_for_maintenance() -> miette::Result<aube_store::Store> {
    let cwd = crate::dirs::workspace_or_project_root()
        .or_else(|_| crate::dirs::project_root_or_cwd())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::commands::open_store_for_maintenance(&cwd)
}

fn path() -> miette::Result<()> {
    let store = open_store_for_maintenance()?;
    println!("{}", store.store_v1_dir().display());
    Ok(())
}

async fn add(specs: Vec<String>) -> miette::Result<()> {
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = make_client(&cwd);
    let store = crate::commands::open_store(&cwd)?;
    // Same exposure as an install: this writes content into the CAS before
    // it writes the index that makes the content reachable, so a concurrent
    // sweep collects it as unreferenced. Measured on an unlocked build, a
    // prune deleted 218 of this command's own files while it exited 0.
    let _sweep_guard = store.lock_for_link();

    let mut added = 0usize;
    for spec in &specs {
        let (name, version_spec) = split_name_spec(spec);
        let packument = client
            .fetch_packument_full_cached(name, &packument_full_cache_dir())
            .await
            .map_err(|e| match e {
                aube_registry::Error::NotFound(n) => miette!("package not found: {n}"),
                other => miette!("failed to fetch {name}: {other}"),
            })?;

        let version = resolve_version(&packument, version_spec).ok_or_else(|| {
            miette!(
                "no matching version for {name}@{}",
                version_spec.unwrap_or("latest")
            )
        })?;

        let tarball_url = packument
            .get("versions")
            .and_then(|v| v.get(&version))
            .and_then(|v| v.get("dist"))
            .and_then(|d| d.get("tarball"))
            .and_then(|t| t.as_str())
            .map(String::from)
            .unwrap_or_else(|| client.tarball_url(name, &version));
        let integrity = packument
            .get("versions")
            .and_then(|v| v.get(&version))
            .and_then(|v| v.get("dist"))
            .and_then(|d| d.get("integrity"))
            .and_then(|i| i.as_str())
            .map(String::from);

        let bytes = client
            .fetch_tarball_bytes(&tarball_url)
            .await
            .map_err(|e| miette!("failed to fetch {name}@{version}: {e}"))?;

        if let Some(expected) = integrity.as_deref() {
            aube_store::verify_integrity(&bytes, expected)
                .map_err(|e| miette!("{name}@{version}: {e}"))?;
        }

        let index = store
            .import_tarball(&bytes)
            .map_err(|e| miette!("failed to import {name}@{version}: {e}"))?;
        // When the packument shipped a `dist.integrity`, the cache
        // filename carries a `+<hex>` suffix that discriminates
        // same-(name, version) tarballs from different sources.
        // Otherwise we fall back to the plain key (proxies that strip
        // integrity still get a warm cache).
        if let Err(e) = store.save_index(name, &version, integrity.as_deref(), &index) {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_CACHE_WRITE_FAILED,
                "failed to cache index for {name}@{version}: {e}"
            );
        }

        println!("+ {name}@{version}");
        added += 1;
    }

    eprintln!(
        "Added {} to the store",
        pluralizer::pluralize("package", added as isize, true)
    );
    Ok(())
}

/// Collect the set of hex hashes referenced by every cached package index.
/// Pruning must fail closed if this scan is incomplete: a skipped index would
/// otherwise make its live CAS files look unreferenced.
fn referenced_hashes(index_dir: &Path) -> miette::Result<std::collections::HashSet<String>> {
    let mut seen = std::collections::HashSet::new();
    visit_cached_indices_at(index_dir, |_, index| {
        for stored in index.values() {
            seen.insert(stored.hex_hash.clone());
        }
    })?;
    Ok(seen)
}

/// Visit every JSON index at the root and in integrity-keyed subdirectories.
/// A missing index root is an empty cache; every other scan failure is fatal.
fn visit_cached_indices(
    store: &aube_store::Store,
    visit: impl FnMut(&Path, aube_store::PackageIndex),
) -> miette::Result<()> {
    visit_cached_indices_at(&store.index_dir(), visit)
}

fn visit_cached_indices_at(
    index_dir: &Path,
    mut visit: impl FnMut(&Path, aube_store::PackageIndex),
) -> miette::Result<()> {
    if !index_dir.try_exists().map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
            "failed to inspect store index directory {}: {e}",
            index_dir.display()
        )
    })? {
        return Ok(());
    }
    visit_indices_in_dir(index_dir, true, &mut visit)
}

fn visit_indices_in_dir(
    dir: &Path,
    visit_subdirs: bool,
    visit: &mut impl FnMut(&Path, aube_store::PackageIndex),
) -> miette::Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
            "failed to list store index directory {}: {e}",
            dir.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
                "failed to read an entry in store index directory {}: {e}",
                dir.display()
            )
        })?;
        let path = entry.path();
        let metadata = entry.metadata().map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
                "failed to inspect store index path {}: {e}",
                path.display()
            )
        })?;
        if metadata.is_dir() {
            if visit_subdirs {
                visit_indices_in_dir(&path, false, visit)?;
            }
            continue;
        }
        if !metadata.is_file() || path.extension() != Some(std::ffi::OsStr::new("json")) {
            continue;
        }
        let content = std::fs::read(&path).map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
                "failed to read store index {}: {e}",
                path.display()
            )
        })?;
        let index = serde_json::from_slice(&content).map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
                "failed to parse store index {}: {e}",
                path.display()
            )
        })?;
        visit(&path, index);
    }
    Ok(())
}

fn prune(args: PruneArgs) -> miette::Result<()> {
    let json = PRUNE_JSON_REQUESTED.swap(false, Ordering::Relaxed);
    let store = open_store_for_maintenance()?;
    let maintenance_lock = store
        .lock_for_maintenance()
        .into_diagnostic()
        .map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_STORE_PRUNE_LOCK_FAILED,
                "failed to lock the store for pruning: {e}"
            )
        })?;
    let _gvs_lock = super::gvs_registry::lock_for_prune(&store.virtual_store_dir(), json)?;
    let gvs_plan = super::gvs_registry::plan_prune(&store.virtual_store_dir())?;
    let current_index_dir = store.index_dir();
    let legacy_index_dir = store.legacy_index_dir();
    let mut referenced = referenced_hashes(&current_index_dir)?;
    if legacy_index_dir != current_index_dir {
        referenced.extend(referenced_hashes(&legacy_index_dir)?);
    }
    let legacy_plan = plan_legacy_gvs_prune(&store.virtual_store_dir());
    let cas_plan = plan_cas_prune(store.root(), &referenced, &gvs_plan, &legacy_plan)?;
    let trees = store.trees_dir();
    let trees_plan = plan_trees_prune(&trees, &store.virtual_store_dir(), &gvs_plan.live_projects);
    let report = build_prune_report(&store, &gvs_plan, &legacy_plan, &trees_plan, &cas_plan);

    if json {
        let output = serde_json::to_string_pretty(&report).into_diagnostic()?;
        println!("{output}");
        return Ok(());
    }

    if !args.dry_run {
        if store.legacy_index_migration_needed() {
            store.migrate_legacy_index_for_maintenance(&maintenance_lock);
        }
        super::gvs_registry::apply_prune(&store.virtual_store_dir(), &gvs_plan)?;
        apply_legacy_gvs_prune(&legacy_plan);
        apply_trees_prune(&trees, &trees_plan);
        for path in &cas_plan.paths {
            std::fs::remove_file(path).map_err(|e| {
                miette!(
                    code = aube_codes::errors::ERR_AUBE_STORE_PRUNE_FAILED,
                    "failed to prune store file {}: {e}",
                    path.display()
                )
            })?;
        }
    }

    let verb = if args.dry_run {
        "Would prune"
    } else {
        "Pruned"
    };
    if !gvs_plan.entries.is_empty() {
        eprintln!(
            "{verb} {} ({:.1} MB) from the global virtual store",
            pluralizer::pluralize("package", gvs_plan.entries.len() as isize, true),
            gvs_plan.bytes() as f64 / 1_048_576.0
        );
    }
    if !gvs_plan.stale_records.is_empty() {
        eprintln!(
            "{verb} {} from the global virtual store registry",
            pluralizer::pluralize(
                "stale project record",
                gvs_plan.stale_records.len() as isize,
                true
            )
        );
    }
    if !legacy_plan.entries.is_empty() {
        eprintln!(
            "{verb} {} ({:.1} MB) from the previous global virtual store layout",
            pluralizer::pluralize("package", legacy_plan.entries.len() as isize, true),
            candidate_bytes(&legacy_plan.files) as f64 / 1_048_576.0
        );
    }
    if !trees_plan.entries.is_empty() {
        eprintln!(
            "{verb} {} ({:.1} MB) from the extracted-tree cache",
            pluralizer::pluralize("entry", trees_plan.entries.len() as isize, true),
            candidate_bytes(&trees_plan.files) as f64 / 1_048_576.0
        );
    }
    if !cas_plan.files.is_empty() {
        let size_prefix = if args.dry_run { "up to " } else { "" };
        eprintln!(
            "{verb} {} ({size_prefix}{:.1} MB) from the store",
            pluralizer::pluralize("file", cas_plan.files.len() as isize, true),
            candidate_bytes(&cas_plan.files) as f64 / 1_048_576.0
        );
    }
    if gvs_plan.entries.is_empty()
        && gvs_plan.stale_records.is_empty()
        && cas_plan.files.is_empty()
        && trees_plan.entries.is_empty()
        && legacy_plan.entries.is_empty()
        && legacy_plan.bookkeeping.is_empty()
    {
        eprintln!("Nothing to prune");
    }
    if !trees_plan.deferred.is_empty() {
        // The number that matters on the first prune after an upgrade: every
        // project that has not reinstalled yet looks exactly like garbage, and
        // this is what stops us acting on that.
        eprintln!(
            "Holding {} of the extracted-tree cache unreferenced for {} days before removal.\n\
             Install in any project that still needs them and they are kept.",
            pluralizer::pluralize("entry", trees_plan.deferred.len() as isize, true),
            GRACE.as_secs() / 86_400,
        );
    }
    if trees_plan.skipped_no_projects {
        eprintln!(
            "No projects are registered against the store; skipping the extracted-tree cache.\n\
             Run an install in each project you want kept, then prune again."
        );
    }
    if !legacy_plan.deferred.is_empty() {
        // No "install to keep them" remedy here, unlike the tiers above: an
        // install re-links the project into the versioned store, which makes
        // the old entry MORE collectable, not less.
        eprintln!(
            "Holding {} of the previous global virtual store layout unreferenced for {} days before removal.",
            pluralizer::pluralize("entry", legacy_plan.deferred.len() as isize, true),
            GRACE.as_secs() / 86_400,
        );
    }
    if legacy_plan.skipped_no_records {
        eprintln!(
            "No project record of the previous global virtual store layout names a directory that still exists; skipping it."
        );
    }
    for path in &gvs_plan.vanished_files {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_STORE_PRUNE_ENTRY_DISAPPEARED,
            path = %path.display(),
            "global virtual-store file disappeared while building the prune plan"
        );
    }
    for path in &legacy_plan.vanished_files {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_STORE_PRUNE_ENTRY_DISAPPEARED,
            path = %path.display(),
            "previous-layout global virtual-store file disappeared while building the prune plan"
        );
    }
    for path in &trees_plan.vanished_files {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_STORE_PRUNE_ENTRY_DISAPPEARED,
            path = %path.display(),
            "extracted-tree file disappeared while building the prune plan"
        );
    }
    Ok(())
}

fn plan_cas_prune(
    root: &Path,
    referenced: &HashSet<String>,
    gvs_plan: &super::gvs_registry::GvsPrunePlan,
    legacy_plan: &LegacyGvsPrunePlan,
) -> miette::Result<CasPrunePlan> {
    if !root.try_exists().into_diagnostic()? {
        return Ok(CasPrunePlan::default());
    }
    // The pre-`v1` entries count here too: earlier releases hardlinked them out
    // of this same CAS, so releasing one drops a link exactly as a versioned
    // entry does. Extracted trees deliberately do not — they are reflink
    // clones, and their blocks are not what a link count measures.
    let mut removed_gvs_links: HashMap<super::gvs_registry::FileIdentity, u64> = HashMap::new();
    for file in gvs_plan.files.iter().chain(&legacy_plan.files) {
        *removed_gvs_links.entry(file.identity.clone()).or_default() += 1;
    }
    let mut plan = CasPrunePlan::default();
    let mut content_paths = HashSet::new();
    let mut markers = Vec::new();
    let root_entries = read_dir_complete(root)?;
    for entry in &root_entries {
        let path = entry.path();
        let is_stream_temp = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(&format!(".{}-stream-", aube_util::prog())));
        if !is_stream_temp {
            continue;
        }
        let metadata = entry.metadata().into_diagnostic()?;
        if metadata.is_file() {
            plan.paths.push(path.clone());
            plan.files.push(super::gvs_registry::CandidateFile {
                identity: candidate_identity(&path, &metadata),
                bytes: metadata.len(),
            });
        }
    }
    // Walk every 2-char shard directory. Store layout is
    // <root>/<shard>/<rest-of-hash>[-exec].
    for shard in root_entries {
        let shard_path = shard.path();
        if !shard.file_type().into_diagnostic()?.is_dir() {
            continue;
        }
        let Some(shard_name) = shard_path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if shard_name.len() != 2 {
            continue;
        }
        for file in read_dir_complete(&shard_path)? {
            let file_path = file.path();
            // The dirent already carries the type; stat nothing until the
            // index has ruled a file out. Referenced files are the bulk of a
            // store — a stat per file on a ~900k-file store costs minutes,
            // and only unreferenced candidates need size and link count.
            if !file.file_type().into_diagnostic()?.is_file() {
                continue;
            }
            let Some(fname) = file_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(base) = fname.strip_suffix("-exec") {
                let content_path = shard_path.join(base);
                markers.push((file_path, content_path));
                continue;
            }
            let hex = format!("{shard_name}{fname}");
            if referenced.contains(&hex) {
                continue;
            }
            let metadata = file.metadata().into_diagnostic()?;
            let identity = candidate_identity(&file_path, &metadata);
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let removed_links = removed_gvs_links.get(&identity).copied().unwrap_or(0);
                if metadata.nlink() > removed_links + 1 {
                    continue;
                }
            }
            content_paths.insert(file_path.clone());
            plan.paths.push(file_path);
            plan.files.push(super::gvs_registry::CandidateFile {
                identity,
                bytes: metadata.len(),
            });
        }
    }
    for (marker, content) in markers {
        if content_paths.contains(&content) {
            plan.paths.push(marker);
        }
    }
    Ok(plan)
}

fn candidate_identity(
    _path: &Path,
    metadata: &std::fs::Metadata,
) -> super::gvs_registry::FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        super::gvs_registry::FileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        super::gvs_registry::FileIdentity::Path(_path.to_path_buf())
    }
}

fn read_dir_complete(path: &Path) -> miette::Result<Vec<std::fs::DirEntry>> {
    std::fs::read_dir(path)
        .into_diagnostic()?
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()
}

fn build_prune_report(
    store: &aube_store::Store,
    gvs_plan: &super::gvs_registry::GvsPrunePlan,
    legacy_plan: &LegacyGvsPrunePlan,
    trees_plan: &TreesPrunePlan,
    cas_plan: &CasPrunePlan,
) -> PruneReport {
    let mut mutation_roots = vec![
        mutation_root("store", store.store_v1_dir()),
        mutation_root("contentStore", store.root().to_path_buf()),
        mutation_root("packageIndex", store.index_dir()),
        mutation_root("globalVirtualStore", store.virtual_store_dir()),
        mutation_root("extractedTrees", store.trees_dir()),
        mutation_root(
            "projectRegistry",
            store
                .virtual_store_dir()
                .join(super::gvs_registry::PROJECTS_DIR),
        ),
        mutation_root("maintenanceLock", store.maintenance_lock_path()),
        mutation_root(
            "globalVirtualStoreLock",
            store
                .virtual_store_dir()
                .join(super::gvs_registry::LOCK_FILE),
        ),
    ];
    if let Some(legacy_root) = &legacy_plan.root {
        mutation_roots.push(mutation_root(
            "legacyGlobalVirtualStore",
            legacy_root.clone(),
        ));
    }
    let mut actions = Vec::new();
    if store.legacy_index_migration_needed() {
        mutation_roots.push(mutation_root(
            "legacyPackageIndex",
            store.legacy_index_dir(),
        ));
        actions.push(PlannedAction {
            kind: "migrateLegacyPackageIndex",
            from: Some(json_path(store.legacy_index_dir())),
            to: Some(json_path(store.index_dir())),
            count: 1,
        });
    }
    actions.extend([
        PlannedAction {
            kind: "pruneGlobalVirtualStoreEntries",
            from: None,
            to: None,
            count: gvs_plan.entries.len(),
        },
        PlannedAction {
            kind: "removeStaleProjectRecords",
            from: None,
            to: None,
            count: gvs_plan.stale_records.len(),
        },
        PlannedAction {
            kind: "pruneLegacyGlobalVirtualStoreEntries",
            from: None,
            to: None,
            count: legacy_plan.entries.len(),
        },
        PlannedAction {
            kind: "removeLegacyGlobalVirtualStoreRecords",
            from: None,
            to: None,
            count: legacy_plan.bookkeeping.len(),
        },
        PlannedAction {
            kind: "pruneExtractedTreeEntries",
            from: None,
            to: None,
            count: trees_plan.entries.len(),
        },
        PlannedAction {
            kind: "pruneContentStoreFiles",
            from: None,
            to: None,
            count: cas_plan.files.len(),
        },
    ]);
    let mut unique = HashMap::new();
    for file in gvs_plan
        .files
        .iter()
        .chain(&legacy_plan.files)
        .chain(&trees_plan.files)
        .chain(&cas_plan.files)
    {
        unique.entry(file.identity.clone()).or_insert(file.bytes);
    }
    PruneReport {
        schema_version: 1,
        dry_run: true,
        mutation_roots,
        actions,
        global_virtual_store: GvsStats {
            entries: gvs_plan.entries.len(),
            bytes_upper_bound: gvs_plan.bytes(),
            stale_project_records: gvs_plan.stale_records.len(),
        },
        legacy_global_virtual_store: LegacyGvsStats {
            entries: legacy_plan.entries.len(),
            bytes_upper_bound: candidate_bytes(&legacy_plan.files),
            deferred_entries: legacy_plan.deferred.len(),
            bookkeeping_paths: legacy_plan.bookkeeping.len(),
            skipped_no_records: legacy_plan.skipped_no_records,
        },
        extracted_trees: TreesStats {
            entries: trees_plan.entries.len(),
            bytes_upper_bound: candidate_bytes(&trees_plan.files),
            deferred_entries: trees_plan.deferred.len(),
            skipped_no_projects: trees_plan.skipped_no_projects,
        },
        content_store: CasStats {
            files: cas_plan.files.len(),
            bytes_upper_bound: candidate_bytes(&cas_plan.files),
        },
        reclaimable_bytes_upper_bound: unique.into_values().sum(),
        warnings: gvs_plan
            .vanished_files
            .iter()
            .map(|path| ("global virtual-store file", path))
            .chain(
                legacy_plan
                    .vanished_files
                    .iter()
                    .map(|path| ("previous-layout global virtual-store file", path)),
            )
            .chain(
                trees_plan
                    .vanished_files
                    .iter()
                    .map(|path| ("extracted-tree file", path)),
            )
            .map(|(what, path)| StructuredWarning {
                code: aube_codes::warnings::WARN_AUBE_STORE_PRUNE_ENTRY_DISAPPEARED,
                message: format!(
                    "{what} {} disappeared while building the prune plan",
                    path.display()
                ),
            })
            .collect(),
    }
}

fn candidate_bytes(files: &[super::gvs_registry::CandidateFile]) -> u64 {
    let mut identities = HashSet::new();
    files
        .iter()
        .filter(|file| identities.insert(file.identity.clone()))
        .map(|file| file.bytes)
        .sum()
}

/// How long an extracted-tree entry must stay unreferenced before a sweep
/// removes it.
///
/// The registry cannot distinguish "nothing needs this" from "the project
/// that needs it has not installed since the registry existed", and those
/// look identical on the first prune after an upgrade — every project that
/// has not been reinstalled yet presents as garbage. Measured on the tier's
/// predecessor, one reinstall plus one prune removed 9 of 11 entries.
///
/// So removal is two-phase. The first prune that finds an entry unreferenced
/// records the time and keeps it; only a prune that still finds it
/// unreferenced this long afterwards removes it. Any install in the meantime
/// makes it reachable again and clears the record, which is what makes the
/// documented remedy — reinstall in the projects you want kept — work.
///
/// The global virtual store deliberately has no such window: its entries are
/// keyed by graph hash and reachability there is exact, so upstream's plan
/// removes an unreferenced entry immediately.
const GRACE: std::time::Duration = std::time::Duration::from_secs(30 * 86_400);

/// Plan the extracted-tree tier sweep.
///
/// The tier is a clone-source cache, not content-addressed, and neither the
/// CAS sweep nor the global-virtual-store plan can see it — a tree is keyed
/// by the linker's virtual-store subdir name and nothing else names it. Two
/// name spaces reach it, because that key carries the graph-hash fold only
/// when the install used the shared store: a global-virtual-store install's
/// trees are named after the entries its project links reach, while a
/// project-local install's trees carry the un-hashed `.aube/` entry names,
/// which appear nowhere in the link walk. Both are marked, per project.
///
/// Every heuristic here fails toward over-marking, and the cost asymmetry is
/// what justifies it: retaining a tree wastes disk on a cache, while dropping
/// one a live project still clones from costs that project a re-extract. So
/// an unreadable directory marks nothing for deletion, a record that stopped
/// resolving simply stops counting as a store user, and a registry with no
/// live record at all sweeps nothing — an empty registry is indistinguishable
/// from an unmigrated one.
///
/// Planning is pure: nothing is removed and no clock is started until
/// [`apply_trees_prune`] runs, so `--dry-run` leaves the tier untouched.
fn plan_trees_prune(
    trees: &Path,
    global_virtual_store: &Path,
    live: &[super::gvs_registry::LiveProject],
) -> TreesPrunePlan {
    let mut plan = TreesPrunePlan::default();
    let Ok(tier) = std::fs::read_dir(trees) else {
        return plan;
    };
    let tier: Vec<_> = tier
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str().map(str::to_owned)?;
            // Dot-prefixed names are aube's own bookkeeping (this tier's
            // state file, a crashed build's `.tmp-tree-…`), never a package
            // entry: a tree key comes from `dep_path_to_filename` and an npm
            // name cannot start with a dot.
            (!name.starts_with('.')).then_some((name, entry.path()))
        })
        .collect();
    if live.is_empty() {
        plan.skipped_no_projects = !tier.is_empty();
        return plan;
    }
    plan.swept = true;

    let mut reachable = HashSet::new();
    let mut visited = HashSet::new();
    for project in live {
        for modules_dir in find_node_modules_dirs(&project.project_dir) {
            mark_from(
                &modules_dir,
                global_virtual_store,
                &mut reachable,
                &mut visited,
            );
        }
        if let Ok(entries) = std::fs::read_dir(&project.aube_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    reachable.insert(name.to_string());
                }
            }
        }
    }

    let now = unix_now();
    let previous = read_grace_state(trees);
    for (name, path) in tier {
        if reachable.contains(&name) {
            // Reachable again — drop any record, so a project that comes back
            // restarts the clock rather than inheriting an old one.
            continue;
        }
        let first_seen = previous.get(&name).copied().unwrap_or(now);
        if now.saturating_sub(first_seen) >= GRACE.as_secs() {
            // A walk failure costs this entry its byte estimate and nothing
            // else — the figure is already reported as an upper bound, and
            // the entry is expired either way.
            let _ = super::gvs_registry::collect_candidate_files(
                &path,
                &mut plan.files,
                &mut plan.vanished_files,
            );
            plan.entries.push(ExpiredEntry {
                path,
                name,
                first_seen,
            });
        } else {
            plan.deferred.insert(name, first_seen);
        }
    }
    plan
}

/// Remove the planned entries and rewrite the tier's grace state.
///
/// Infallible by design, unlike the global-virtual-store apply: a tree is a
/// clone source that the linker rebuilds on demand, so an entry we cannot
/// delete is re-recorded under its ORIGINAL sighting and retried next prune
/// rather than failing the command.
fn apply_trees_prune(trees: &Path, plan: &TreesPrunePlan) {
    if !plan.swept {
        return;
    }
    let mut state = plan.deferred.clone();
    for tree in &plan.entries {
        if aube_linker::remove_dir_all_with_retry(&tree.path).is_err() {
            state.insert(tree.name.clone(), tree.first_seen);
        }
    }
    // `state` holds only entries still on disk and still unreferenced, so the
    // file cannot grow without bound.
    write_grace_state(trees, &state);
}

/// The global virtual store's pre-`v1` root: the parent of today's versioned
/// child, and only when the child really is that version.
///
/// Derived rather than configured, the same way `side_effects_cache_root`
/// derives its sibling, so one constant governs both directions of the move.
/// The `None` arm is the safety property: a store that is not in a versioned
/// child has no earlier layout, and its parent is some unrelated directory
/// that must never be swept.
fn legacy_gvs_root(global_virtual_store: &Path) -> Option<std::path::PathBuf> {
    let versioned = global_virtual_store.file_name()
        == Some(std::ffi::OsStr::new(
            crate::commands::settings_context::GVS_REGISTRY_NAMESPACE_VERSION,
        ));
    versioned
        .then(|| global_virtual_store.parent())
        .flatten()
        .map(Path::to_path_buf)
}

/// Project directories named by the PRE-`v1` registry that still resolve.
///
/// That registry is gone from the code — the sync replaced it with the JSON
/// records under `v1/.projects/` — so its format is re-derived here from the
/// releases that wrote it: `<root>/.projects/<16 hex>`, a plain file holding
/// one absolute project path and nothing else. Only the line terminator is
/// stripped, never `trim()`: a project directory may legitimately end in a
/// space, and eating it turns a live project into an unresolvable record.
fn legacy_project_records(root: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(root.join(super::gvs_registry::PROJECTS_DIR)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .map(|text| std::path::PathBuf::from(text.trim_end_matches(['\n', '\r'])))
        .filter(|dir| dir.is_dir())
        .collect()
}

/// Plan the sweep of the global virtual store's pre-`v1` layout.
///
/// Every heuristic fails toward keeping an entry, for the same asymmetry the
/// tiers above reason from: a retained entry wastes disk, while a deleted one
/// leaves a live project's `node_modules` pointing at nothing. Concretely —
///
/// - Reachability is the OLD registry's records alone. A project holding a
///   `v1` record has by definition re-linked into the versioned store, so it
///   says nothing about this layout; counting it would only make the mark set
///   look complete when it is not.
/// - No old record that still resolves, while entries exist, sweeps nothing.
///   A registry that predates the records is indistinguishable from a store
///   nothing references, and this is the arm that keeps 2.8 GB of live entries
///   rather than guess.
/// - Removal keeps the same 30-day [`GRACE`] the tiers above use, against this
///   root's OWN `.gc-state` — the same file, same format, that the releases
///   which wrote this layout swept it with. So sightings they recorded still
///   count, and an entry already past its window goes on the first prune.
///   A project installed before the old registry existed appears in no record
///   at all; the hold is the only thing standing between it and eviction.
/// - The records and the state file are retired only once nothing is left to
///   protect, which is what finally makes the layout disappear.
///
/// Planning is pure: nothing is removed and no clock starts until
/// [`apply_legacy_gvs_prune`] runs, so `--dry-run` leaves the layout untouched.
fn plan_legacy_gvs_prune(global_virtual_store: &Path) -> LegacyGvsPrunePlan {
    let mut plan = LegacyGvsPrunePlan::default();
    let Some(root) = legacy_gvs_root(global_virtual_store) else {
        return plan;
    };
    let Ok(children) = std::fs::read_dir(&root) else {
        plan.root = Some(root);
        return plan;
    };
    plan.root = Some(root.clone());

    let mut entries = Vec::new();
    let mut bookkeeping = Vec::new();
    for child in children.flatten() {
        let Some(name) = child.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == crate::commands::settings_context::GVS_REGISTRY_NAMESPACE_VERSION {
            continue;
        }
        // A dot name is the old layout's own bookkeeping — its project
        // registry, its sweep state, a crashed install's `.tmp-…`. The new
        // layout keeps none of its own here; it is all inside the versioned
        // child. `node_modules` is excluded for the same reason the versioned
        // sweep excludes it: it is not a graph entry under any layout.
        if name.starts_with('.') {
            bookkeeping.push(child.path());
        } else if name != "node_modules" && child.file_type().is_ok_and(|t| t.is_dir()) {
            entries.push((name, child.path()));
        }
    }

    if entries.is_empty() {
        plan.bookkeeping = bookkeeping;
        return plan;
    }
    let projects = legacy_project_records(&root);
    if projects.is_empty() {
        plan.skipped_no_records = true;
        return plan;
    }
    plan.swept = true;

    // The versioned sweep's walk, rooted at the old store instead. It follows
    // every symlink under a project's `node_modules` and recurses into a
    // marked entry's own, so it covers the project-local virtual-store subdir
    // and scope directories without naming either, and marks an entry
    // reachable only as some other entry's dependency. Its own `visited` set,
    // because a walk shared with the versioned root would stop at the first
    // directory the other one had already entered.
    let mut reachable = HashSet::new();
    let mut visited = HashSet::new();
    for project in &projects {
        for modules_dir in find_node_modules_dirs(project) {
            mark_from(&modules_dir, &root, &mut reachable, &mut visited);
        }
    }

    let now = unix_now();
    let previous = read_grace_state(&root);
    let mut layout_emptied = true;
    for (name, path) in entries {
        if reachable.contains(&name) {
            layout_emptied = false;
            continue;
        }
        let first_seen = previous.get(&name).copied().unwrap_or(now);
        if now.saturating_sub(first_seen) >= GRACE.as_secs() {
            let _ = super::gvs_registry::collect_candidate_files(
                &path,
                &mut plan.files,
                &mut plan.vanished_files,
            );
            plan.entries.push(ExpiredEntry {
                path,
                name,
                first_seen,
            });
        } else {
            plan.deferred.insert(name, first_seen);
            layout_emptied = false;
        }
    }
    if layout_emptied {
        plan.bookkeeping = bookkeeping;
    }
    plan
}

/// Remove the planned entries, then either retire the layout's bookkeeping or
/// rewrite its grace state.
///
/// Infallible, like [`apply_trees_prune`] and unlike the versioned apply: this
/// tier is a leftover nothing writes to, so an entry we cannot delete is
/// re-recorded under its ORIGINAL sighting and retried next prune rather than
/// failing a command whose real work is elsewhere. That tolerance is also why
/// the bookkeeping goes only when every removal SUCCEEDED — deleting the
/// records while one entry survives would strand it forever, with no evidence
/// left to judge it by.
fn apply_legacy_gvs_prune(plan: &LegacyGvsPrunePlan) {
    let Some(root) = &plan.root else {
        return;
    };
    let mut state = plan.deferred.clone();
    let mut all_removed = true;
    for entry in &plan.entries {
        if aube_linker::remove_dir_all_with_retry(&entry.path).is_err() {
            state.insert(entry.name.clone(), entry.first_seen);
            all_removed = false;
        }
    }
    if all_removed && !plan.bookkeeping.is_empty() {
        // The grace state is one of these, so it must not be rewritten after.
        for path in &plan.bookkeeping {
            let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
            let _ = if is_dir {
                aube_linker::remove_dir_all_with_retry(path)
            } else {
                std::fs::remove_file(path)
            };
        }
        return;
    }
    if plan.swept {
        write_grace_state(root, &state);
    }
}

/// Every `node_modules` directory in a project, workspace packages
/// included. Records a `node_modules` without descending into it — the
/// store walk enters it separately — and skips dot directories, which is
/// what pnpm's own `findAllNodeModulesDirs` does: `.git`, `.next`,
/// `.turbo`, `.venv` are large and none of them holds a project. The one
/// dot directory that matters, the project's own `.aube/`, sits INSIDE a
/// `node_modules` and so is reached by `mark_from`, which has no such
/// filter.
fn find_node_modules_dirs(project: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![project.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == "node_modules" {
                // Take it whether it is a real directory or a symlink to one.
                // `DirEntry::file_type` does NOT follow links, so testing
                // `is_dir()` here silently skipped every project whose
                // `node_modules` is a symlink — a scratch disk, a container
                // volume, a hand-linked cache. Such a project marked nothing
                // and its entries were swept while it was still using them.
                // `read_dir` in `mark_from` follows the link, and its
                // `visited` set is canonicalized, so this cannot loop.
                if entry.path().is_dir() {
                    found.push(entry.path());
                }
                continue;
            }
            // Descend only into REAL directories: following a symlink here
            // could walk out of the project or around a cycle.
            if !entry.file_type().is_ok_and(|t| t.is_dir()) || name.starts_with('.') {
                continue;
            }
            stack.push(entry.path());
        }
    }
    found
}

/// Walk `dir`, marking every store entry its symlinks reach. Recurses
/// into a marked entry's own `node_modules` so an entry reachable only as
/// another entry's dependency is marked too.
fn mark_from(
    dir: &Path,
    vstore: &Path,
    reachable: &mut HashSet<String>,
    visited: &mut HashSet<std::path::PathBuf>,
) {
    let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !visited.insert(key) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            let Ok(target) = std::fs::read_link(&path) else {
                continue;
            };
            // GVS targets are absolute today (the linker byte-compares
            // them against an absolute path); resolve a relative one
            // anyway rather than silently failing to mark it.
            let target = if target.is_absolute() {
                target
            } else {
                dir.join(target)
            };
            let Ok(rest) = target.strip_prefix(vstore) else {
                continue;
            };
            let Some(name) = rest.components().next() else {
                continue;
            };
            let Some(name) = name.as_os_str().to_str() else {
                continue;
            };
            reachable.insert(name.to_string());
            mark_from(
                &vstore.join(name).join("node_modules"),
                vstore,
                reachable,
                visited,
            );
        } else if file_type.is_dir() {
            // Scope directories (`@scope/`) and the project-local
            // `.aube/` tree both hold links one level down.
            mark_from(&path, vstore, reachable, visited);
        }
    }
}

/// State file recording when each entry was FIRST seen unreferenced. Lives
/// inside the tier it describes so one delete takes both, and is dot-named
/// so the sweep above skips it.
fn grace_state_path(root: &Path) -> std::path::PathBuf {
    root.join(".gc-state")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_grace_state(root: &Path) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string(grace_state_path(root)) else {
        return map;
    };
    for line in text.lines() {
        if let Some((secs, name)) = line.split_once('\t')
            && let Ok(secs) = secs.parse::<u64>()
        {
            map.insert(name.to_string(), secs);
        }
    }
    map
}

fn write_grace_state(root: &Path, state: &HashMap<String, u64>) {
    let mut out = String::new();
    for (name, secs) in state {
        // A name with a newline or tab would corrupt the file on read-back.
        // `dep_path_to_filename` cannot produce either, so skip rather than
        // escape — a dropped record only costs one extra grace period.
        if name.contains('\n') || name.contains('\t') {
            continue;
        }
        out.push_str(&format!("{secs}\t{name}\n"));
    }
    let _ = std::fs::write(grace_state_path(root), out);
}

fn mutation_root(kind: &'static str, path: std::path::PathBuf) -> MutationRoot {
    let resolved = resolve_physical_path(&path);
    let resolved_path = resolved.filter(|resolved| resolved != &path).map(json_path);
    MutationRoot {
        kind,
        path: json_path(path),
        resolved_path,
    }
}

fn resolve_physical_path(path: &Path) -> Option<std::path::PathBuf> {
    let mut existing = path;
    let mut tail = Vec::new();
    while !existing.exists() {
        tail.push(existing.file_name()?.to_os_string());
        existing = existing.parent()?;
    }
    let mut resolved = std::fs::canonicalize(existing).ok()?;
    for component in tail.into_iter().rev() {
        resolved.push(component);
    }
    Some(resolved)
}

fn json_path(path: std::path::PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

fn status() -> miette::Result<()> {
    let store = open_store()?;
    let mut checked = 0usize;
    let mut broken: Vec<String> = Vec::new();
    visit_cached_indices(&store, |path, index| {
        checked += 1;
        let pkg_label = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().replace("__", "/"))
            .unwrap_or_else(|| path.display().to_string());
        let mut pkg_ok = true;
        for (rel, stored) in &index {
            if !verify_stored_file(&stored.store_path, &stored.hex_hash) {
                broken.push(format!("{pkg_label}: {rel}"));
                pkg_ok = false;
            }
        }
        if pkg_ok {
            tracing::debug!("store ok: {pkg_label}");
        }
    })?;

    if checked == 0 {
        eprintln!("Store is consistent (no cached indices found)");
        return Ok(());
    }

    if broken.is_empty() {
        eprintln!(
            "Store is consistent: {} verified",
            pluralizer::pluralize("package", checked as isize, true)
        );
        Ok(())
    } else {
        // Corruption lines go to stdout so operators can pipe them into
        // `wc -l`, `grep`, etc. while the summary/failure goes to stderr
        // via miette. Mirrors how `store add` emits data on stdout.
        for line in &broken {
            println!("corrupt: {line}");
        }
        Err(miette!(
            "store contains {} corrupted {}",
            broken.len(),
            pluralizer::pluralize("file", broken.len() as isize, false)
        ))
    }
}

/// Stream the file at `path` through BLAKE3 and compare to the expected
/// hex digest. Missing files count as a mismatch.
fn verify_stored_file(path: &Path, expected_hex: &str) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = blake3::Hasher::new();
    if std::io::copy(&mut f, &mut hasher).is_err() {
        return false;
    }
    let actual = hasher.finalize().to_hex().to_string();
    actual == expected_hex
}

#[cfg(all(test, unix))]
mod legacy_gvs_prune_tests {
    use super::*;

    /// A pre-`v1` root with its versioned child beside it, the way an upgraded
    /// store really looks.
    fn layout(tmp: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = tmp.join("store");
        let current = root.join(crate::commands::settings_context::GVS_REGISTRY_NAMESPACE_VERSION);
        std::fs::create_dir_all(&current).unwrap();
        (root, current)
    }

    /// One old-layout entry, with a file inside so a wrong removal is visible
    /// rather than a no-op on an empty directory.
    fn entry(root: &Path, name: &str) -> std::path::PathBuf {
        let modules = root.join(name).join("node_modules");
        std::fs::create_dir_all(&modules).unwrap();
        std::fs::write(modules.join("index.js"), name).unwrap();
        root.join(name)
    }

    /// A pre-`v1` registry record, written the way `Store::register_project`
    /// wrote it before the sync: a file named by the first 16 hex of the
    /// project path's BLAKE3, holding that path and nothing else.
    fn record(root: &Path, project: &Path) {
        let dir = root.join(super::super::gvs_registry::PROJECTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let path = project.to_string_lossy();
        let name = blake3::hash(path.as_bytes()).to_hex()[..16].to_string();
        std::fs::write(dir.join(name), path.as_bytes()).unwrap();
    }

    /// Link a project into the old root through its project-local
    /// virtual-store directory, the way an install of that era did.
    fn link_project(project: &Path, target: &Path, alias: &str) {
        let local = project
            .join("node_modules")
            .join(format!(".{}", aube_util::prog()));
        std::fs::create_dir_all(&local).unwrap();
        std::os::unix::fs::symlink(target, local.join(alias)).unwrap();
    }

    /// Backdate an entry's unreferenced record so the next plan treats it as
    /// already expired, without waiting out the real window.
    fn expire(root: &Path, name: &str) {
        let mut state = read_grace_state(root);
        state.insert(name.to_string(), 0);
        write_grace_state(root, &state);
    }

    fn planned(plan: &LegacyGvsPrunePlan) -> Vec<&str> {
        let mut names: Vec<_> = plan.entries.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        names
    }

    /// The load-bearing case. Both entries are past their window, so the one
    /// that survives can only be surviving because a still-registered project
    /// links to it.
    #[test]
    fn an_old_entry_a_registered_project_links_to_survives() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, current) = layout(tmp.path());
        let live = entry(&root, "live@1.0.0-aaaa");
        let orphan = entry(&root, "orphan@1.0.0-bbbb");

        let project = tmp.path().join("proj");
        link_project(&project, &live, "live@1.0.0");
        record(&root, &project);
        expire(&root, "live@1.0.0-aaaa");
        expire(&root, "orphan@1.0.0-bbbb");

        let plan = plan_legacy_gvs_prune(&current);
        assert_eq!(planned(&plan), vec!["orphan@1.0.0-bbbb"]);
        assert!(live.is_dir() && orphan.is_dir(), "planning removes nothing");

        apply_legacy_gvs_prune(&plan);
        assert!(live.is_dir(), "a linked entry survives its own expiry");
        assert!(!orphan.exists(), "the unreferenced entry is removed");
        assert!(current.is_dir(), "the versioned store is never touched");
    }

    /// No record resolves, so the layout carries no evidence at all — which is
    /// indistinguishable from one every project still uses. Same outcome when
    /// the registry directory is missing entirely: both leave no live record.
    #[test]
    fn an_old_layout_with_no_resolvable_record_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, current) = layout(tmp.path());
        let kept = entry(&root, "still-needed@1.0.0-aaaa");
        expire(&root, "still-needed@1.0.0-aaaa");
        record(&root, &tmp.path().join("deleted-project"));

        let plan = plan_legacy_gvs_prune(&current);
        assert!(plan.skipped_no_records, "the skip must be reported");
        assert!(plan.entries.is_empty() && plan.bookkeeping.is_empty());

        apply_legacy_gvs_prune(&plan);
        assert!(kept.is_dir());
    }

    /// The upgrade case, and the reason the hold exists. A project that has
    /// not reinstalled since the layout was retired appears in no record, so
    /// the first prune to find an entry unreferenced must only RECORD it.
    #[test]
    fn an_unreferenced_old_entry_survives_its_first_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, current) = layout(tmp.path());
        let orphan = entry(&root, "orphan@1.0.0-aaaa");
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        record(&root, &project);

        let plan = plan_legacy_gvs_prune(&current);
        assert_eq!((plan.entries.len(), plan.deferred.len()), (0, 1));
        apply_legacy_gvs_prune(&plan);
        assert!(orphan.is_dir(), "the first prune only records the sighting");
        assert!(read_grace_state(&root).contains_key("orphan@1.0.0-aaaa"));

        // Positive control: it IS removable once the window has passed, so the
        // assertions above pin the hold and not something else.
        expire(&root, "orphan@1.0.0-aaaa");
        let plan = plan_legacy_gvs_prune(&current);
        apply_legacy_gvs_prune(&plan);
        assert!(!orphan.exists());
    }

    /// Retiring the last entry retires the registry and the sweep state with
    /// it, so the layout disappears instead of leaving an empty shell that
    /// every later prune has to reason about again.
    #[test]
    fn the_old_records_go_once_no_entry_is_left() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, current) = layout(tmp.path());
        entry(&root, "orphan@1.0.0-aaaa");
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        record(&root, &project);
        expire(&root, "orphan@1.0.0-aaaa");

        let plan = plan_legacy_gvs_prune(&current);
        assert_eq!(plan.bookkeeping.len(), 2, "the registry and the state file");

        apply_legacy_gvs_prune(&plan);
        assert!(!root.join(super::super::gvs_registry::PROJECTS_DIR).exists());
        assert!(!grace_state_path(&root).exists());
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            1,
            "only the versioned store is left"
        );
    }

    /// The store is not in a versioned child, so there is no earlier layout —
    /// and above all no licence to sweep the store's PARENT, which is an
    /// unrelated cache directory holding unrelated things.
    #[test]
    fn an_unversioned_store_has_no_previous_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let gvs = tmp.path().join("virtual-store");
        std::fs::create_dir_all(gvs.join("entry@1.0.0-aaaa")).unwrap();
        std::fs::create_dir_all(tmp.path().join("sibling")).unwrap();

        let plan = plan_legacy_gvs_prune(&gvs);
        assert!(plan.root.is_none());
        assert!(plan.entries.is_empty() && !plan.skipped_no_records);

        apply_legacy_gvs_prune(&plan);
        assert!(tmp.path().join("sibling").is_dir());
    }
}

#[cfg(all(test, unix))]
mod extracted_tree_prune_tests {
    use super::*;

    /// Backdate an entry's unreferenced record so the next plan treats it as
    /// already expired, without waiting out the real window.
    fn expire(trees: &Path, name: &str) {
        let mut state = read_grace_state(trees);
        state.insert(name.to_string(), 0);
        write_grace_state(trees, &state);
    }

    /// A tree entry, with one file inside so an accidental removal is visible
    /// rather than a no-op on an empty directory.
    fn tree(trees: &Path, name: &str) {
        let dir = trees.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.js"), name).unwrap();
    }

    /// A global-virtual-store entry, and the directory a dependent's links
    /// live in.
    fn gvs_entry(gvs: &Path, name: &str) -> std::path::PathBuf {
        let dir = gvs.join(name).join("node_modules");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn link(from: &Path, name: &str, to: &Path) {
        std::fs::create_dir_all(from).unwrap();
        std::os::unix::fs::symlink(to, from.join(name)).unwrap();
    }

    /// Register `project` the way an install does, with the project-local
    /// virtual store (`<project>/node_modules/.<prog>`) as its `aube_dir`.
    fn register(gvs: &Path, project: &Path) -> std::path::PathBuf {
        let aube_dir = project
            .join("node_modules")
            .join(format!(".{}", aube_util::prog()));
        std::fs::create_dir_all(&aube_dir).unwrap();
        super::super::gvs_registry::register_project(gvs, project, &aube_dir).unwrap();
        aube_dir
    }

    fn live(gvs: &Path) -> Vec<super::super::gvs_registry::LiveProject> {
        super::super::gvs_registry::read_registry(gvs).unwrap().live
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| !n.starts_with('.'))
            .collect();
        out.sort();
        out
    }

    /// Plan and apply in one step, for the tests whose subject is the sweep
    /// rather than the plan/apply split.
    fn sweep(trees: &Path, gvs: &Path, projects: &[super::super::gvs_registry::LiveProject]) {
        let plan = plan_trees_prune(trees, gvs, projects);
        apply_trees_prune(trees, &plan);
    }

    #[test]
    fn a_tree_linked_through_the_global_virtual_store_survives() {
        let tmp = tempfile::tempdir().unwrap();
        let (gvs, trees) = (tmp.path().join("gvs"), tmp.path().join("trees"));
        tree(&trees, "live@1.0.0-aaaa");
        tree(&trees, "orphan@1.0.0-bbbb");
        gvs_entry(&gvs, "live@1.0.0-aaaa");

        let project = tmp.path().join("proj");
        let aube_dir = register(&gvs, &project);
        link(&aube_dir, "live@1.0.0", &gvs.join("live@1.0.0-aaaa"));
        expire(&trees, "orphan@1.0.0-bbbb");

        sweep(&trees, &gvs, &live(&gvs));

        assert_eq!(names(&trees), vec!["live@1.0.0-aaaa"]);
    }

    /// A tree reachable only as another entry's dependency, never linked from
    /// a project directly. Missing this evicts a live clone source.
    #[test]
    fn a_tree_reachable_only_transitively_survives() {
        let tmp = tempfile::tempdir().unwrap();
        let (gvs, trees) = (tmp.path().join("gvs"), tmp.path().join("trees"));
        for name in [
            "direct@1.0.0-aaaa",
            "transitive@2.0.0-bbbb",
            "orphan@3.0.0-cccc",
        ] {
            tree(&trees, name);
        }
        let direct = gvs_entry(&gvs, "direct@1.0.0-aaaa");
        gvs_entry(&gvs, "transitive@2.0.0-bbbb");
        link(&direct, "transitive", &gvs.join("transitive@2.0.0-bbbb"));

        let project = tmp.path().join("proj");
        let aube_dir = register(&gvs, &project);
        link(&aube_dir, "direct@1.0.0", &gvs.join("direct@1.0.0-aaaa"));
        expire(&trees, "orphan@3.0.0-cccc");

        sweep(&trees, &gvs, &live(&gvs));

        assert_eq!(
            names(&trees),
            vec!["direct@1.0.0-aaaa", "transitive@2.0.0-bbbb"]
        );
    }

    /// A project-local install links into its own `.aube/`, never into the
    /// global store, so its trees are named by the UN-hashed dep path and
    /// appear nowhere in the link walk. Marking them from the record's
    /// `aube_dir` is the only thing that keeps them.
    #[test]
    fn a_project_local_install_keeps_its_unhashed_trees() {
        let tmp = tempfile::tempdir().unwrap();
        let (gvs, trees) = (tmp.path().join("gvs"), tmp.path().join("trees"));
        tree(&trees, "local@1.0.0");
        tree(&trees, "orphan@1.0.0");

        let project = tmp.path().join("proj");
        let aube_dir = register(&gvs, &project);
        std::fs::create_dir_all(aube_dir.join("local@1.0.0/node_modules/local")).unwrap();
        expire(&trees, "orphan@1.0.0");

        sweep(&trees, &gvs, &live(&gvs));

        assert_eq!(names(&trees), vec!["local@1.0.0"]);
    }

    /// The registry is the only reachability evidence there is, so a registry
    /// naming no live project must sweep nothing: a store predating the
    /// registry looks exactly like a store nothing references.
    #[test]
    fn an_empty_registry_sweeps_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (gvs, trees) = (tmp.path().join("gvs"), tmp.path().join("trees"));
        tree(&trees, "would-be-orphan@1.0.0-aaaa");
        expire(&trees, "would-be-orphan@1.0.0-aaaa");

        let plan = plan_trees_prune(&trees, &gvs, &live(&gvs));
        assert!(plan.skipped_no_projects, "the skip must be reported");
        apply_trees_prune(&trees, &plan);

        assert_eq!(names(&trees), vec!["would-be-orphan@1.0.0-aaaa"]);
    }

    /// The case the `live` filter guards that the empty registry cannot
    /// reach: records EXIST but none resolves. Count them as store users and
    /// the sweep runs against an empty mark set, evicting the whole tier.
    #[test]
    fn a_wholly_unresolvable_registry_sweeps_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (gvs, trees) = (tmp.path().join("gvs"), tmp.path().join("trees"));
        tree(&trees, "still-needed@1.0.0-aaaa");
        expire(&trees, "still-needed@1.0.0-aaaa");

        for name in ["gone-a", "gone-b"] {
            let project = tmp.path().join(name);
            register(&gvs, &project);
            std::fs::remove_dir_all(&project).unwrap();
        }

        let plan = plan_trees_prune(&trees, &gvs, &live(&gvs));
        assert!(plan.skipped_no_projects);
        apply_trees_prune(&trees, &plan);

        assert_eq!(names(&trees), vec!["still-needed@1.0.0-aaaa"]);
    }

    /// One live record and one that no longer resolves. The sweep must still
    /// run: an unresolvable record carries information only about what THAT
    /// project reached, so disqualifying the whole tier on it turns any caller
    /// that registers a directory it later deletes into a dead sweep forever.
    #[test]
    fn an_unresolvable_record_disqualifies_only_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let (gvs, trees) = (tmp.path().join("gvs"), tmp.path().join("trees"));
        for name in [
            "kept@1.0.0-aaaa",
            "collectable@1.0.0-bbbb",
            "fresh@1.0.0-cccc",
        ] {
            tree(&trees, name);
        }
        gvs_entry(&gvs, "kept@1.0.0-aaaa");

        let project = tmp.path().join("live");
        let aube_dir = register(&gvs, &project);
        link(&aube_dir, "kept@1.0.0", &gvs.join("kept@1.0.0-aaaa"));

        let gone = tmp.path().join("gone");
        register(&gvs, &gone);
        std::fs::remove_dir_all(&gone).unwrap();

        // Already past its window, so its removal proves the sweep ran at all.
        expire(&trees, "collectable@1.0.0-bbbb");

        sweep(&trees, &gvs, &live(&gvs));

        assert_eq!(
            names(&trees),
            vec!["fresh@1.0.0-cccc", "kept@1.0.0-aaaa"],
            "the live project's tree survives, the expired orphan goes, and \
             an orphan still inside its window is held"
        );
    }

    /// The upgrade case, and the reason the grace period exists. A project
    /// that has not reinstalled since the tier appeared is indistinguishable
    /// from garbage, so the first prune must only RECORD it.
    #[test]
    fn an_unreferenced_tree_survives_its_first_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let (gvs, trees) = (tmp.path().join("gvs"), tmp.path().join("trees"));
        tree(&trees, "not-yet-reinstalled@1.0.0-aaaa");
        register(&gvs, &tmp.path().join("proj"));
        let projects = live(&gvs);

        for _ in 0..2 {
            let plan = plan_trees_prune(&trees, &gvs, &projects);
            assert_eq!((plan.entries.len(), plan.deferred.len()), (0, 1));
            apply_trees_prune(&trees, &plan);
            assert_eq!(names(&trees), vec!["not-yet-reinstalled@1.0.0-aaaa"]);
        }

        // Positive control: it IS removable once the window has passed, so the
        // assertions above pin the grace period and not something else.
        expire(&trees, "not-yet-reinstalled@1.0.0-aaaa");
        let plan = plan_trees_prune(&trees, &gvs, &projects);
        assert_eq!((plan.entries.len(), plan.deferred.len()), (1, 0));
        apply_trees_prune(&trees, &plan);
        assert!(names(&trees).is_empty());
    }

    /// Becoming reachable again must CLEAR the record, not merely pause it —
    /// otherwise a project that reinstalls inside the window still loses its
    /// trees the moment the original clock runs out.
    #[test]
    fn reinstalling_inside_the_window_resets_the_clock() {
        let tmp = tempfile::tempdir().unwrap();
        let (gvs, trees) = (tmp.path().join("gvs"), tmp.path().join("trees"));
        tree(&trees, "revived@1.0.0");
        let project = tmp.path().join("proj");
        let aube_dir = register(&gvs, &project);

        // Seen unreferenced once...
        sweep(&trees, &gvs, &live(&gvs));
        assert!(read_grace_state(&trees).contains_key("revived@1.0.0"));

        // ...then its project reinstalls and it is reachable again.
        std::fs::create_dir_all(aube_dir.join("revived@1.0.0")).unwrap();
        sweep(&trees, &gvs, &live(&gvs));
        assert!(
            !read_grace_state(&trees).contains_key("revived@1.0.0"),
            "a reachable entry must lose its unreferenced record"
        );

        // With the record cleared, the next sighting starts a FRESH window
        // rather than reaching back to the original one.
        std::fs::remove_dir_all(aube_dir.join("revived@1.0.0")).unwrap();
        let plan = plan_trees_prune(&trees, &gvs, &live(&gvs));
        assert_eq!((plan.entries.len(), plan.deferred.len()), (0, 1));
    }

    /// The state file must not accumulate records for entries that are gone.
    #[test]
    fn the_grace_state_does_not_grow_without_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let (gvs, trees) = (tmp.path().join("gvs"), tmp.path().join("trees"));
        tree(&trees, "transient@1.0.0-aaaa");
        register(&gvs, &tmp.path().join("proj"));

        sweep(&trees, &gvs, &live(&gvs));
        assert_eq!(read_grace_state(&trees).len(), 1);

        std::fs::remove_dir_all(trees.join("transient@1.0.0-aaaa")).unwrap();
        sweep(&trees, &gvs, &live(&gvs));
        assert!(
            read_grace_state(&trees).is_empty(),
            "a record for a vanished entry should be dropped"
        );
    }

    /// `--dry-run` plans without touching the tier, and without starting the
    /// grace clock — so a dry run never makes a later real prune delete
    /// something it would otherwise have held.
    #[test]
    fn planning_alone_removes_nothing_and_starts_no_clock() {
        let tmp = tempfile::tempdir().unwrap();
        let (gvs, trees) = (tmp.path().join("gvs"), tmp.path().join("trees"));
        tree(&trees, "expired@1.0.0-aaaa");
        register(&gvs, &tmp.path().join("proj"));
        expire(&trees, "expired@1.0.0-aaaa");
        let recorded = read_grace_state(&trees);

        let plan = plan_trees_prune(&trees, &gvs, &live(&gvs));
        assert_eq!(plan.entries.len(), 1, "the entry is planned for removal");
        assert_eq!(names(&trees), vec!["expired@1.0.0-aaaa"]);
        assert_eq!(read_grace_state(&trees), recorded, "state is untouched");
    }

    /// A project whose `node_modules` is a SYMLINK — a scratch disk, a
    /// container volume. `DirEntry::file_type` does not follow links, so
    /// testing `is_dir()` skipped these projects entirely: they marked
    /// nothing and their trees were swept.
    #[test]
    fn a_symlinked_node_modules_is_still_found() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let elsewhere = tmp.path().join("scratch").join("node_modules");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::os::unix::fs::symlink(&elsewhere, project.join("node_modules")).unwrap();

        assert_eq!(
            find_node_modules_dirs(&project),
            vec![project.join("node_modules")],
            "a symlinked node_modules must be walked, not skipped"
        );
    }

    #[test]
    fn the_project_scan_finds_workspace_node_modules_but_does_not_descend() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("node_modules/dep/node_modules")).unwrap();
        std::fs::create_dir_all(root.join("packages/a/node_modules")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();

        let mut found = find_node_modules_dirs(root);
        found.sort();
        assert_eq!(
            found,
            vec![
                root.join("node_modules"),
                root.join("packages/a/node_modules")
            ],
            "the nested node_modules under an already-recorded one must not be recorded again"
        );
    }
}
