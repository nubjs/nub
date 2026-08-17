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
//! - `aube store prune` — remove files from the store that have no remaining
//!   hardlink references. This is a best-effort heuristic (the same one
//!   pnpm uses on hardlink filesystems): on APFS/btrfs reflinks produce
//!   independent inodes so the nlink count is always 1 and pruning there
//!   can't safely tell referenced from unreferenced files; in that case we
//!   fall back to removing only files that no cached package index in
//!   `<store>/v1/index/` points at.
//!
//!   It then mark-and-sweeps the two directory tiers the CAS sweep cannot
//!   see — the global virtual store and `<store>/v1/trees/` — against the
//!   projects registered by [`aube_store::Store::register_project`]. Same
//!   shape as pnpm's `pruneGlobalVirtualStore`. With no registered
//!   projects it prunes neither, because an empty registry is
//!   indistinguishable from an unmigrated one.
//! - `aube store status` — verify every file referenced by a cached package
//!   index still exists in the store and its BLAKE3 hash matches. Exits 0
//!   when everything is consistent, 1 when any corruption is found.
//!
//! None of these subcommands touch `node_modules/`, the lockfile, or the
//! project manifest, so they deliberately skip the project lock and the
//! auto-install check.

use crate::commands::{make_client, packument_full_cache_dir, resolve_version, split_name_spec};
use clap::{Args, Subcommand};
use miette::{IntoDiagnostic, miette};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Args)]
pub struct StoreArgs {
    #[command(subcommand)]
    pub command: StoreCommand,
}

#[derive(Debug, Subcommand)]
pub enum StoreCommand {
    /// Add one or more packages to the global store without linking them
    /// into any project.
    ///
    /// Each argument is a package spec: `lodash`, `lodash@4.17.21`,
    /// `react@next`, or `express@^4`.
    Add {
        /// Package specs to fetch into the store.
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Show the store path.
    Path,
    /// Remove unreferenced packages from the global store.
    ///
    /// Operates on the store printed by `aube store path`; it does not touch
    /// project node_modules directories, manifests, or lockfiles.
    ///
    /// It keeps files referenced by cached package indexes and, on hardlink
    /// filesystems, files that still have project hardlink references.
    ///
    /// On reflink filesystems such as APFS or btrfs, link counts cannot prove
    /// project reachability, so pruning relies on cached package indexes.
    Prune,
    /// Verify the store against cached package indexes.
    ///
    /// Confirms every file referenced by a cached package index is
    /// still present in the store and that its BLAKE3 hash matches.
    /// Exits non-zero when any corruption is detected.
    Status,
}

pub async fn run(args: StoreArgs) -> miette::Result<()> {
    match args.command {
        StoreCommand::Add { packages } => add(packages).await,
        StoreCommand::Path => path(),
        StoreCommand::Prune => prune(),
        StoreCommand::Status => status(),
    }
}

fn open_store() -> miette::Result<aube_store::Store> {
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::commands::open_store(&cwd)
}

fn path() -> miette::Result<()> {
    let store = open_store()?;
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

/// Collect the set of hex hashes referenced by any cached package index
/// under `<store>/v1/index/`. Integrity-keyed entries live under
/// `<16 hex>/<name>@<version>.json` subdirs; integrity-less entries
/// live at the root as `<name>@<version>.json`. Walk both. Used as
/// the "known-referenced" set for `store prune` and `store status`.
fn referenced_hashes(store: &aube_store::Store) -> std::collections::HashSet<String> {
    let mut seen = std::collections::HashSet::new();
    let index_dir = store.index_dir();
    collect_hashes_from_dir(&index_dir, &mut seen);
    let Ok(entries) = std::fs::read_dir(&index_dir) else {
        return seen;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_hashes_from_dir(&path, &mut seen);
        }
    }
    seen
}

fn collect_hashes_from_dir(dir: &std::path::Path, seen: &mut std::collections::HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(index): Result<aube_store::PackageIndex, _> = serde_json::from_str(&content) else {
            continue;
        };
        for stored in index.values() {
            seen.insert(stored.hex_hash.clone());
        }
    }
}

fn prune() -> miette::Result<()> {
    let store = open_store()?;

    // ONE acquisition covering BOTH sweeps, for the whole command.
    //
    // The CAS sweep needs it as much as the tier sweep, and predates this
    // lock entirely. On a reflink filesystem every CAS file has `nlink == 1`,
    // so `prune_cas` falls back to index-reachability — and a fetching
    // install writes content BEFORE it writes the index that would protect
    // it. Measured on an unlocked build: three of three cold trials aborted
    // the concurrent install, one after deleting 771 CAS files; `store add`
    // was worse, exiting 0 while 218 of its own files were deleted.
    //
    // Taking it once matters: two `try_lock` calls on the same file from one
    // process contend with each other, so a per-sweep acquisition would make
    // the second sweep always decline.
    let _sweep_guard = match store.try_lock_for_sweep() {
        aube_store::SweepLock::Held(file) => Some(file),
        aube_store::SweepLock::Busy => {
            eprintln!("An install is using this store; skipping the prune.");
            return Ok(());
        }
        // No advisory locks here (some FUSE and NFS mounts). Refusing would
        // make prune a permanent no-op on those — including the CAS half,
        // which ran unconditionally before this lock existed — so degrade to
        // unsynchronized rather than never collecting anything.
        aube_store::SweepLock::Unsupported => None,
    };

    // The CAS and the directory tiers are independent: a store can hold
    // virtual-store entries with an absent or already-empty CAS root, so
    // an early return here would silently skip the tier sweep below.
    if store.root().exists() {
        prune_cas(&store)?;
    } else {
        eprintln!("Store is empty: nothing to prune");
    }
    prune_virtual_store(&store);
    Ok(())
}

fn prune_cas(store: &aube_store::Store) -> miette::Result<()> {
    let root = store.root().to_path_buf();
    let referenced = referenced_hashes(store);
    let mut removed_files = 0u64;
    let mut removed_bytes = 0u64;

    // Walk every 2-char shard directory. Store layout is
    // <root>/<shard>/<rest-of-hash>[-exec].
    for shard in std::fs::read_dir(&root).into_diagnostic()?.flatten() {
        let shard_path = shard.path();
        if !shard_path.is_dir() {
            continue;
        }
        let shard_name = match shard_path.file_name().and_then(|s| s.to_str()) {
            Some(s) if s.len() == 2 => s.to_string(),
            _ => continue,
        };
        for file in std::fs::read_dir(&shard_path).into_diagnostic()?.flatten() {
            let file_path = file.path();
            let Some(fname) = file_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // Skip the `-exec` marker; it gets removed alongside its target.
            let is_exec_marker = fname.ends_with("-exec");
            let base = fname.strip_suffix("-exec").unwrap_or(fname);
            let hex = format!("{shard_name}{base}");

            if referenced.contains(&hex) {
                continue;
            }

            // On hardlink filesystems, files with nlink > 1 are referenced
            // by at least one virtual-store entry — don't touch them. Exec
            // markers are never hardlinked, so we can't check them directly;
            // instead we delete a marker only when its companion content
            // file is *also* going away, otherwise we'd silently strip the
            // executable bit from a file pnpm still references.
            let content_len = match file.metadata() {
                Ok(meta) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        if is_exec_marker {
                            let content_path = shard_path.join(base);
                            if let Ok(content_meta) = std::fs::metadata(&content_path)
                                && content_meta.nlink() > 1
                            {
                                continue;
                            }
                        } else if meta.nlink() > 1 {
                            continue;
                        }
                    }
                    meta.len()
                }
                Err(_) => 0,
            };

            // Only credit the byte counter after the unlink actually
            // succeeds, otherwise a permission-denied failure would
            // inflate the "freed" number in the summary.
            if std::fs::remove_file(&file_path).is_ok() && !is_exec_marker {
                removed_files += 1;
                removed_bytes += content_len;
            }
        }
    }

    eprintln!(
        "Pruned {} ({:.1} MB) from the store",
        pluralizer::pluralize("file", removed_files as isize, true),
        removed_bytes as f64 / 1_048_576.0
    );
    Ok(())
}

/// Mark-and-sweep the global virtual store and the extracted-tree tier.
///
/// Neither tier is content-addressed, so the CAS sweep above cannot see
/// them: a virtual-store entry is a directory keyed by the dep path folded
/// with its graph hash, and nothing ever removed one. Because
/// `calc_deps_hash` mixes each child's hash into every ancestor, one
/// dependency bump re-keys the bumped package and every ancestor that
/// reaches it, so ordinary lockfile churn strands whole generations of
/// entries permanently.
///
/// Reachability comes from the project registry ([`Store::register_project`]):
/// walk each registered project's `node_modules`, follow the symlinks that
/// land in the store, and mark what they reach — including transitively,
/// through a marked entry's own sibling links.
///
/// **Every heuristic here fails toward over-marking.** Retaining garbage
/// costs disk; under-marking deletes a directory a live project is pointing
/// at. So an empty registry prunes nothing, an unreadable directory marks
/// nothing for deletion, and a sweep that cannot take the lock declines.
/// The one place it does not over-mark is the project scan, which skips dot
/// directories to match pnpm — safe because the dot directory that holds the
/// store links, `.aube/`, sits inside a `node_modules` and is reached by
/// `mark_from`.
///
/// The caller must already hold the sweep lock — `prune` takes it once for
/// the whole command, because a second `try_lock` from the same process
/// would contend with the first and make this decline every time.
fn prune_virtual_store(store: &aube_store::Store) {
    let vstore = store.virtual_store_dir();
    let trees = store.trees_dir();
    if !vstore.exists() && !trees.exists() {
        return;
    }

    let all = store.registered_projects();

    // A registered project whose path does not resolve is NOT evidence that it
    // is gone. An unmounted disk, a detached container volume, or a parent
    // directory we cannot traverse look identical to a deletion — and acting
    // on that reading destroys the registration, then sweeps the entries the
    // project is still using, with no way back. So an unresolvable project
    // means we do not know the whole picture, and the sweep declines.
    //
    // Registry records age out on the same clock as entries, so a genuinely
    // deleted project stops blocking pruning once the window passes.
    let registry_dir = store.projects_dir();
    let mut missing_state = read_grace_state(&registry_dir);
    let now = unix_now();
    let mut blocked = 0usize;
    for project in all.iter().filter(|p| !p.exists) {
        let since = *missing_state.entry(project.record.clone()).or_insert(now);
        if now.saturating_sub(since) >= GRACE.as_secs() {
            store.forget_project(&project.record);
            missing_state.remove(&project.record);
        } else {
            blocked += 1;
        }
    }
    missing_state.retain(|record, _| all.iter().any(|p| &p.record == record && !p.exists));
    write_grace_state(&registry_dir, &missing_state);

    if blocked > 0 {
        eprintln!(
            "{} not reachable right now (an unmounted disk, or deleted); skipping the virtual store.\n\
             Entries they may still need cannot be told apart from garbage.",
            pluralizer::pluralize("registered project is", blocked as isize, true),
        );
        return;
    }

    let projects: Vec<std::path::PathBuf> = all.into_iter().map(|p| p.dir).collect();
    if projects.is_empty() {
        // Indistinguishable from "no project has installed since this
        // feature shipped", so sweeping here would delete a live store.
        eprintln!(
            "No projects are registered against the virtual store; skipping it.\n\
             Run an install in each project you want kept, then prune again."
        );
        return;
    }

    let mut reachable = HashSet::new();
    let mut visited = HashSet::new();
    for project in &projects {
        for modules_dir in find_node_modules_dirs(project) {
            mark_from(&modules_dir, &vstore, &mut reachable, &mut visited);
        }
        // The extracted-tree tier is keyed the same way whether or not the
        // graph hash was applied, so a project-local `.aube/` install
        // (CI, hoisted, dlx) names its trees by the un-hashed dep path.
        // Those names appear nowhere in the walk above — collect them
        // directly, or every prune would evict a project-local user's
        // whole clone-source cache.
        let local = crate::commands::resolve_virtual_store_dir_for_cwd(project);
        if let Ok(entries) = std::fs::read_dir(&local) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    reachable.insert(name.to_string());
                }
            }
        }
    }

    let vs = sweep_unreachable(&vstore, &reachable, GRACE);
    let tr = sweep_unreachable(&trees, &reachable, GRACE);
    let (entries, tree_entries) = (vs.removed, tr.removed);
    let deferred = vs.deferred + tr.deferred;

    if entries == 0 && tree_entries == 0 && deferred == 0 {
        eprintln!(
            "Virtual store is fully referenced by {} registered project(s)",
            projects.len()
        );
        return;
    }
    if deferred > 0 {
        // The number that matters on the first prune after an upgrade: every
        // project that has not reinstalled yet looks exactly like garbage, and
        // this is what stops us acting on that.
        eprintln!(
            "Holding {} unreferenced for {} days before removal.\n\
             Install in any project that still needs them and they are kept.",
            pluralizer::pluralize("entry", deferred as isize, true),
            GRACE.as_secs() / 86_400,
        );
    }
    if entries == 0 && tree_entries == 0 {
        return;
    }
    eprintln!(
        "Pruned {} from the virtual store and {} from the extracted-tree tier",
        pluralizer::pluralize("entry", entries as isize, true),
        pluralizer::pluralize("entry", tree_entries as isize, true),
    );
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

/// How long an entry must stay unreferenced before a sweep removes it.
///
/// The registry cannot distinguish "nothing needs this" from "the project
/// that needs it has not installed since the registry existed", and those
/// look identical on the first prune after an upgrade — every project that
/// has not been reinstalled yet presents as garbage. Deleting on that
/// evidence breaks working trees: measured, one reinstall plus one prune
/// removed 9 of 11 entries and broke the two projects that had not been
/// touched.
///
/// So removal is two-phase. The first sweep that finds an entry
/// unreferenced records the time and keeps it; only a sweep that still
/// finds it unreferenced this long afterwards removes it. Any install in
/// the meantime makes it reachable again and clears the record, which is
/// what makes the documented remedy — reinstall in the projects you want
/// kept — actually work.
const GRACE: std::time::Duration = std::time::Duration::from_secs(30 * 86_400);

#[derive(Default)]
struct SweepOutcome {
    removed: u64,
    deferred: u64,
}

/// State file recording when each entry was FIRST seen unreferenced. Lives
/// inside the tier it describes so one delete takes both, and is dot-named
/// so the sweep below skips it.
fn grace_state_path(root: &Path) -> std::path::PathBuf {
    root.join(".gc-state")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_grace_state(root: &Path) -> std::collections::HashMap<String, u64> {
    let mut map = std::collections::HashMap::new();
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

fn write_grace_state(root: &Path, state: &std::collections::HashMap<String, u64>) {
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

/// Remove every immediate child of `root` that has been absent from `keep`
/// for at least `grace`, and record the rest as newly-unreferenced.
///
/// Dot-prefixed names are skipped: they are aube's own bookkeeping (the
/// project registry, this tier's state file), never a package entry, since
/// a store entry name comes from `dep_path_to_filename` and an npm name
/// cannot start with a dot.
fn sweep_unreachable(
    root: &Path,
    keep: &HashSet<String>,
    grace: std::time::Duration,
) -> SweepOutcome {
    let Ok(entries) = std::fs::read_dir(root) else {
        return SweepOutcome::default();
    };
    let now = unix_now();
    let previous = read_grace_state(root);
    let mut next = std::collections::HashMap::new();
    let mut outcome = SweepOutcome::default();

    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') || keep.contains(&name) {
            // Reachable again — drop any record, so a project that comes
            // back restarts the clock rather than inheriting an old one.
            continue;
        }
        let first_seen = previous.get(&name).copied().unwrap_or(now);
        if now.saturating_sub(first_seen) >= grace.as_secs() {
            if std::fs::remove_dir_all(entry.path()).is_ok() {
                outcome.removed += 1;
            } else {
                next.insert(name, first_seen);
            }
        } else {
            outcome.deferred += 1;
            next.insert(name, first_seen);
        }
    }

    // `next` holds only entries still on disk and still unreferenced, so the
    // file cannot grow without bound.
    write_grace_state(root, &next);
    outcome
}

fn status() -> miette::Result<()> {
    let store = open_store()?;
    let index_dir = store.index_dir();
    if !index_dir.exists() {
        eprintln!("Store is consistent (no cached indices found)");
        return Ok(());
    }

    let mut checked = 0usize;
    let mut broken: Vec<String> = Vec::new();

    // Walk the index root (integrity-less entries) and every
    // `<16 hex>/` subdir (integrity-keyed entries). Flat filenames at
    // the root are the integrity-less variants; files one level
    // deep are the integrity-keyed variants — both need verifying.
    verify_indices_in_dir(&index_dir, &mut checked, &mut broken).into_diagnostic()?;
    for entry in std::fs::read_dir(&index_dir).into_diagnostic()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            verify_indices_in_dir(&path, &mut checked, &mut broken).into_diagnostic()?;
        }
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

/// Verify every `*.json` cached index directly inside `dir` (no
/// recursion). Callers walk the layout hierarchy and call this on
/// each directory that can hold index files. Keeps the BLAKE3 hot
/// loop in one place.
fn verify_indices_in_dir(
    dir: &Path,
    checked: &mut usize,
    broken: &mut Vec<String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // `@scope/name@version.json` gets stored as `@scope__name@version.json`.
        let pkg_label = stem.replace("__", "/");

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(index): Result<aube_store::PackageIndex, _> = serde_json::from_str(&content) else {
            continue;
        };

        *checked += 1;
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
    }
    Ok(())
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

#[cfg(test)]
mod virtual_store_prune_tests {
    use super::*;

    /// Zero grace — "remove anything unreferenced right now". The two-phase
    /// behavior gets its own tests below; the rest assert on the sweep.
    const NOW: std::time::Duration = std::time::Duration::ZERO;

    /// A store entry: `<vstore>/<name>/node_modules/` plus one file, so an
    /// accidental removal is visible rather than a no-op on an empty dir.
    fn entry(vstore: &Path, name: &str) -> std::path::PathBuf {
        let dir = vstore.join(name).join("node_modules");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), name).unwrap();
        dir
    }

    fn link(from: &Path, name: &str, to: &Path) {
        std::fs::create_dir_all(from).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(to, from.join(name)).unwrap();
    }

    /// Backdate an entry's unreferenced record so a full `prune_virtual_store`
    /// (which uses the real `GRACE`) treats it as already expired. Lets a test
    /// exercise the sweep without waiting out the window.
    fn expire(vstore: &Path, name: &str) {
        let mut state = read_grace_state(vstore);
        state.insert(name.to_string(), 0);
        write_grace_state(vstore, &state);
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

    #[test]
    #[cfg(unix)]
    fn sweep_keeps_reachable_entries_and_removes_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let vstore = tmp.path().join("vstore");
        entry(&vstore, "live@1.0.0-aaaa");
        entry(&vstore, "orphan@1.0.0-bbbb");

        let project = tmp.path().join("proj");
        let modules = project.join("node_modules");
        link(&modules, "live", &vstore.join("live@1.0.0-aaaa"));

        let mut reachable = HashSet::new();
        let mut visited = HashSet::new();
        mark_from(&modules, &vstore, &mut reachable, &mut visited);
        assert!(
            reachable.contains("live@1.0.0-aaaa"),
            "the linked entry should be marked, got {reachable:?}"
        );

        assert_eq!(
            sweep_unreachable(&vstore, &reachable, NOW).removed,
            1,
            "one orphan should be removed"
        );
        assert_eq!(names(&vstore), vec!["live@1.0.0-aaaa"]);
    }

    /// An entry no project links directly, reached only as another entry's
    /// dependency. Missing this is the failure that deletes a live tree.
    #[test]
    #[cfg(unix)]
    fn sweep_keeps_an_entry_reachable_only_transitively() {
        let tmp = tempfile::tempdir().unwrap();
        let vstore = tmp.path().join("vstore");
        let direct = entry(&vstore, "direct@1.0.0-aaaa");
        entry(&vstore, "transitive@2.0.0-bbbb");
        entry(&vstore, "orphan@3.0.0-cccc");
        link(&direct, "transitive", &vstore.join("transitive@2.0.0-bbbb"));

        let modules = tmp.path().join("proj").join("node_modules");
        link(&modules, "direct", &vstore.join("direct@1.0.0-aaaa"));

        let mut reachable = HashSet::new();
        let mut visited = HashSet::new();
        mark_from(&modules, &vstore, &mut reachable, &mut visited);

        assert_eq!(sweep_unreachable(&vstore, &reachable, NOW).removed, 1);
        assert_eq!(
            names(&vstore),
            vec!["direct@1.0.0-aaaa", "transitive@2.0.0-bbbb"]
        );
    }

    /// The registry is the only reachability evidence there is, so an empty
    /// one must prune nothing. A store predating the registry looks exactly
    /// like a store nothing references.
    #[test]
    fn an_empty_registry_prunes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = aube_store::Store::with_dirs(tmp.path().join("cas"), tmp.path().join("cache"));
        let vstore = store.virtual_store_dir();
        entry(&vstore, "would-be-orphan@1.0.0-aaaa");
        assert!(store.registered_projects().is_empty());

        prune_virtual_store(&store);

        assert_eq!(
            names(&vstore),
            vec!["would-be-orphan@1.0.0-aaaa"],
            "an unregistered store must survive prune untouched"
        );
    }

    /// The tier sweep must not depend on the CAS existing. `prune` used to
    /// return early when `store.root()` was absent, which skipped the tier
    /// sweep entirely — and the e2e case that was meant to cover the
    /// empty-registry guard passed anyway, because prune never reached it.
    #[test]
    #[cfg(unix)]
    fn the_tier_sweep_runs_with_no_cas_root() {
        let tmp = tempfile::tempdir().unwrap();
        let store = aube_store::Store::with_dirs(tmp.path().join("cas"), tmp.path().join("cache"));
        assert!(!store.root().exists(), "this test is about an absent CAS");

        let project = tmp.path().join("proj");
        let modules = project.join("node_modules");
        let vstore = store.virtual_store_dir();
        entry(&vstore, "live@1.0.0-aaaa");
        entry(&vstore, "orphan@1.0.0-bbbb");
        link(&modules, "live", &vstore.join("live@1.0.0-aaaa"));
        store.register_project(&project).unwrap();
        expire(&vstore, "orphan@1.0.0-bbbb");

        prune_virtual_store(&store);

        assert_eq!(names(&vstore), vec!["live@1.0.0-aaaa"]);
    }

    /// The upgrade case, and the reason the grace period exists. A project
    /// that has not reinstalled since the registry appeared is indistinguishable
    /// from garbage, so the first sweep must only RECORD it.
    #[test]
    #[cfg(unix)]
    fn an_unreferenced_entry_survives_its_first_sweep() {
        let tmp = tempfile::tempdir().unwrap();
        let vstore = tmp.path().join("vstore");
        entry(&vstore, "not-yet-reinstalled@1.0.0-aaaa");
        let keep = HashSet::new();

        let first = sweep_unreachable(&vstore, &keep, GRACE);
        assert_eq!((first.removed, first.deferred), (0, 1));
        assert_eq!(names(&vstore), vec!["not-yet-reinstalled@1.0.0-aaaa"]);

        // Still inside the window on a later prune — still kept.
        let second = sweep_unreachable(&vstore, &keep, GRACE);
        assert_eq!((second.removed, second.deferred), (0, 1));
        assert_eq!(names(&vstore), vec!["not-yet-reinstalled@1.0.0-aaaa"]);

        // Positive control: it IS removable once the window has passed, so
        // the assertions above pin the grace period and not something else.
        let expired = sweep_unreachable(&vstore, &keep, NOW);
        assert_eq!((expired.removed, expired.deferred), (1, 0));
        assert!(names(&vstore).is_empty());
    }

    /// Becoming reachable again must CLEAR the record, not merely pause it —
    /// otherwise a project that reinstalls inside the window still loses its
    /// entries the moment the original clock runs out.
    #[test]
    #[cfg(unix)]
    fn reinstalling_inside_the_window_resets_the_clock() {
        let tmp = tempfile::tempdir().unwrap();
        let vstore = tmp.path().join("vstore");
        entry(&vstore, "revived@1.0.0-aaaa");

        // Seen unreferenced once...
        sweep_unreachable(&vstore, &HashSet::new(), GRACE);
        assert!(read_grace_state(&vstore).contains_key("revived@1.0.0-aaaa"));

        // ...then its project reinstalls and it is reachable again.
        let keep = HashSet::from(["revived@1.0.0-aaaa".to_string()]);
        sweep_unreachable(&vstore, &keep, GRACE);
        assert!(
            !read_grace_state(&vstore).contains_key("revived@1.0.0-aaaa"),
            "a reachable entry must lose its unreferenced record"
        );

        // With the record cleared, an immediate expiry cannot reach back to
        // the original sighting: it starts a fresh window instead.
        let after = sweep_unreachable(&vstore, &HashSet::new(), GRACE);
        assert_eq!((after.removed, after.deferred), (0, 1));
        assert_eq!(names(&vstore), vec!["revived@1.0.0-aaaa"]);
    }

    /// The state file must not accumulate records for entries that are gone.
    #[test]
    #[cfg(unix)]
    fn the_grace_state_does_not_grow_without_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let vstore = tmp.path().join("vstore");
        entry(&vstore, "transient@1.0.0-aaaa");
        sweep_unreachable(&vstore, &HashSet::new(), GRACE);
        assert_eq!(read_grace_state(&vstore).len(), 1);

        std::fs::remove_dir_all(vstore.join("transient@1.0.0-aaaa")).unwrap();
        sweep_unreachable(&vstore, &HashSet::new(), GRACE);
        assert!(
            read_grace_state(&vstore).is_empty(),
            "a record for a vanished entry should be dropped"
        );
    }

    /// A sweep must decline rather than race an install. The linker publishes
    /// an entry under its final name before anything links at it, so a sweep
    /// in that window deletes a live install's directory.
    #[test]
    #[cfg(unix)]
    fn a_held_link_lock_stops_the_sweep() {
        let tmp = tempfile::tempdir().unwrap();
        let store = aube_store::Store::with_dirs(tmp.path().join("cas"), tmp.path().join("cache"));
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        store.register_project(&project).unwrap();
        let vstore = store.virtual_store_dir();
        // Reachable from nothing — exactly what an install has just published
        // and not yet linked, and what the sweep would otherwise remove.
        entry(&vstore, "mid-install@1.0.0-aaaa");
        expire(&vstore, "mid-install@1.0.0-aaaa");

        let guard = store
            .lock_for_link()
            .expect("shared lock should be takeable");
        // `prune` takes the lock once for the whole command and declines when
        // it cannot, so what a live install blocks is the ACQUISITION.
        assert!(
            matches!(store.try_lock_for_sweep(), aube_store::SweepLock::Busy),
            "a sweep must not acquire while an install holds the lock"
        );

        // Positive control: it IS acquirable once the install ends, and the
        // sweep then removes the entry — so the assertion above pins the lock
        // rather than an entry that was never collectable.
        drop(guard);
        assert!(
            matches!(store.try_lock_for_sweep(), aube_store::SweepLock::Held(_)),
            "a released lock must be re-acquirable"
        );
        prune_virtual_store(&store);
        assert!(
            names(&vstore).is_empty(),
            "released lock should let the sweep run"
        );
    }

    #[test]
    fn the_registry_round_trips_and_forgets_deleted_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let store = aube_store::Store::with_dirs(tmp.path().join("cas"), tmp.path().join("cache"));
        let live = tmp.path().join("live-project");
        let gone = tmp.path().join("deleted-project");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&gone).unwrap();
        store.register_project(&live).unwrap();
        store.register_project(&gone).unwrap();
        // Re-registering the same project must not create a second entry.
        store.register_project(&live).unwrap();
        assert_eq!(store.registered_projects().len(), 2);

        std::fs::remove_dir_all(&gone).unwrap();
        let after = store.registered_projects();
        assert_eq!(after.len(), 2, "reading must not delete a record");
        let missing: Vec<_> = after.iter().filter(|p| !p.exists).collect();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].dir, gone);
        // Reading the registry must NOT destroy a record whose path does not
        // resolve: an unmounted disk looks exactly like a deletion, and losing
        // the registration loses the project's protection for good. Removal is
        // the caller's decision, on a grace period.
        assert_eq!(
            std::fs::read_dir(store.projects_dir()).unwrap().count(),
            2,
            "an unresolvable path must not silently drop its registration"
        );

        store.forget_project(&missing[0].record);
        assert_eq!(
            std::fs::read_dir(store.projects_dir()).unwrap().count(),
            1,
            "an explicit forget should remove exactly that record"
        );
        assert_eq!(store.registered_projects()[0].dir, live);
    }

    /// A project directory whose name ends in a space. `trim()` on the record
    /// made the path not resolve, so the registration was destroyed and the
    /// project's entries swept while it was still using them.
    #[test]
    #[cfg(unix)]
    fn a_project_path_ending_in_a_space_still_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let store = aube_store::Store::with_dirs(tmp.path().join("cas"), tmp.path().join("cache"));
        let spaced = tmp.path().join("trailing space ");
        std::fs::create_dir_all(&spaced).unwrap();
        store.register_project(&spaced).unwrap();

        let got = store.registered_projects();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].dir, spaced, "the path must round-trip byte-for-byte");
        assert!(got[0].exists, "and must still resolve on disk");
    }

    /// A project whose `node_modules` is a SYMLINK — a scratch disk, a
    /// container volume. `DirEntry::file_type` does not follow links, so
    /// testing `is_dir()` skipped these projects entirely: they marked
    /// nothing and their entries were swept.
    #[test]
    #[cfg(unix)]
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
