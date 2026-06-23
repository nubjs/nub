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
    let root = store.root().to_path_buf();
    if !root.exists() {
        eprintln!("Store is empty: nothing to prune");
        return Ok(());
    }

    let referenced = referenced_hashes(&store);
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
