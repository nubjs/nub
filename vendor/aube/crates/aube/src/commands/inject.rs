//! Injected workspace dependencies (`dependenciesMeta.<name>.injected`).
//!
//! Runs after the linker has built the normal isolated tree. For each
//! importer that declares `dependenciesMeta.<dep>.injected = true` on a
//! workspace sibling, this step replaces the top-level symlink with a
//! fresh hard copy of the dep's packed form and gives that copy its
//! own `node_modules/` populated with sibling symlinks for the dep's
//! declared deps.
//!
//! The injected copy lives at
//!   `<root>/node_modules/.aube/<name>@<version>+inject_<hash>/node_modules/<name>/`
//! where `<hash>` is derived from the consumer importer path, so two
//! importers that inject the same sibling get independent copies. That
//! matches pnpm's semantics: the injected form is a snapshot of the
//! source's published file set, and peer deps resolve against the
//! consumer's tree rather than the source's own `node_modules/`.
//!
//! Isolated-linker only: the installer gates the call on
//! `node_linker == Isolated`. Hoisted mode has no `.aube/<dep_path>/`
//! virtual store for the sibling symlinks below to target, and hoisted
//! resolution already walks the consumer's root-level `node_modules/`
//! — the peer-context guarantee injection exists to provide is
//! satisfied there without a copy step.
//!
//! Limitations vs pnpm, intentional for the first cut:
//!
//! - We don't re-run the source package's `prepare` / `build` script
//!   before snapshotting. Users must run their build before `aube
//!   install` (same caveat pnpm has when `package-manager-strict` is
//!   off). Lifecycle automation is tracked separately.
//! - The injected copy is rebuilt from scratch on every install — no
//!   content-hash fast path yet. This keeps the implementation simple
//!   and mirrors pnpm's own "always re-inject" default.

use aube_lockfile::LockfileGraph;
use aube_lockfile::dep_path_filename::dep_path_to_filename;
use aube_manifest::PackageJson;
use miette::{Context, miette};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Apply `dependenciesMeta.injected` to the already-linked workspace.
///
/// `manifests` is the same `(importer_path, PackageJson)` list used by
/// the resolver: root importer is `"."`, workspace packages use their
/// rel path. `ws_dirs` maps workspace package name → absolute dir.
///
/// `aube_dir` is the resolved `virtualStoreDir` (from
/// `commands::resolve_virtual_store_dir`) — the injected entries are
/// written there, and the sibling-symlink pass reads the registry-dep
/// entries from the same path. `virtual_store_dir_max_length` must
/// match the value the linker was built with so both this step's
/// `<name>@<ver>+inject_<hash>` filename and the sibling lookups
/// agree with what the linker wrote.
///
/// Returns the number of injections performed, for the install summary.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_injected(
    root_dir: &Path,
    modules_dir_name: &str,
    aube_dir: &Path,
    virtual_store_dir_max_length: usize,
    graph: &LockfileGraph,
    manifests: &[(String, PackageJson)],
    ws_dirs: &BTreeMap<String, PathBuf>,
) -> miette::Result<usize> {
    // Invert ws_dirs so we can look up a workspace dep's own importer
    // path (and therefore its resolved declared deps in the graph) by
    // name. Two workspace packages can't share a name so the inversion
    // is total.
    let name_to_importer: BTreeMap<String, String> = ws_dirs
        .iter()
        .filter_map(|(name, dir)| {
            let rel = dir.strip_prefix(root_dir).ok()?;
            Some((name.clone(), rel.to_string_lossy().into_owned()))
        })
        .collect();

    let mut count = 0usize;

    for (importer_path, manifest) in manifests {
        let injected = manifest.dependencies_meta_injected();
        if injected.is_empty() {
            continue;
        }

        for dep_name in &injected {
            // Only workspace siblings are injectable in this pass.
            // Registry deps flagged `injected` are ignored — pnpm
            // supports that too but it's a rarer use case and doesn't
            // change the link shape we already produce.
            let Some(src_dir) = ws_dirs.get(dep_name) else {
                continue;
            };

            let src_manifest = PackageJson::from_path(&src_dir.join("package.json"))
                .map_err(miette::Report::new)
                .wrap_err_with(|| {
                    format!("inject: failed to read {}/package.json", src_dir.display())
                })?;
            let version = src_manifest.version.as_deref().unwrap_or("0.0.0");

            // Build a per-consumer dep_path: the same workspace sibling
            // injected into two different consumers must resolve to two
            // distinct `.aube/` entries because their peer-dep closures
            // can diverge.
            let inject_key = format!("{importer_path}\0{dep_name}@{version}");
            let consumer_hash = short_hash(&inject_key);
            let inject_dep_path = format!("{dep_name}@{version}+inject_{consumer_hash}");
            let entry_name = dep_path_to_filename(&inject_dep_path, virtual_store_dir_max_length);
            let entry_root = aube_dir.join(&entry_name);
            let entry_nm = entry_root.join("node_modules");
            let entry_pkg = entry_nm.join(dep_name);

            // Start from a clean slate — repeated installs shouldn't
            // leak stale files from a previous snapshot.
            if entry_root.exists() {
                std::fs::remove_dir_all(&entry_root)
                    .map_err(|e| miette!("inject: remove {}: {e}", entry_root.display()))?;
            }
            std::fs::create_dir_all(&entry_pkg)
                .map_err(|e| miette!("inject: mkdir {}: {e}", entry_pkg.display()))?;

            // Copy the source's packed file set into the injected
            // directory. We reuse `pack::collect_package_files` so the
            // included set is exactly what `aube pack` would ship —
            // honoring `files`, `.npmignore`, and the always-on list.
            let files =
                super::pack::collect_package_files(src_dir, &src_manifest).wrap_err_with(|| {
                    format!(
                        "inject: failed to enumerate {}'s packed files",
                        src_dir.display()
                    )
                })?;
            for (abs, rel) in &files {
                let dst = entry_pkg.join(rel);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| miette!("inject: mkdir {}: {e}", parent.display()))?;
                }
                std::fs::copy(abs, &dst).map_err(|e| {
                    miette!("inject: copy {} -> {}: {e}", abs.display(), dst.display())
                })?;
            }

            // Give the injected copy its own transitive-dep symlinks so
            // Node's resolver can walk out of it. The siblings point at
            // whatever the *source* importer resolved each declared dep
            // to — that's already materialized under
            // `.aube/<dep_dep_path>/node_modules/<dep_name>` by the
            // main linker.
            if let Some(src_importer_path) = name_to_importer.get(dep_name)
                && let Some(direct_deps) = graph.importers.get(src_importer_path)
            {
                for direct in direct_deps {
                    if direct.name == *dep_name {
                        continue;
                    }
                    // Another workspace sibling: point straight at its
                    // source dir. We don't cascade injection — if the
                    // sibling is also flagged `injected`, the consumer
                    // should list it directly in its own
                    // `dependenciesMeta`.
                    if let Some(sibling_ws_dir) = ws_dirs.get(&direct.name) {
                        create_symlink(&entry_nm, &direct.name, sibling_ws_dir)?;
                        continue;
                    }

                    // Registry (or file:/git:) dep: link into the
                    // already-materialized `.aube/<dep_path>` entry.
                    let sibling_entry = aube_dir
                        .join(dep_path_to_filename(
                            &direct.dep_path,
                            virtual_store_dir_max_length,
                        ))
                        .join("node_modules")
                        .join(&direct.name);
                    if !sibling_entry.exists() {
                        continue;
                    }
                    create_symlink(&entry_nm, &direct.name, &sibling_entry)?;
                }
            }

            // Replace the top-level entry (which the linker left as a
            // symlink into the workspace source dir) with a symlink
            // into our freshly-populated injected copy. The importer's
            // node_modules was created by `link_workspace` already.
            let consumer_dir = if importer_path == "." {
                root_dir.to_path_buf()
            } else {
                root_dir.join(importer_path)
            };
            let top_link = consumer_dir.join(modules_dir_name).join(dep_name);
            // `remove_file` works for symlinks; fall back to
            // `remove_dir_all` if the linker somehow left a real dir.
            if top_link.symlink_metadata().is_ok() && std::fs::remove_file(&top_link).is_err() {
                let _ = std::fs::remove_dir_all(&top_link);
            }
            if let Some(parent) = top_link.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| miette!("inject: mkdir {}: {e}", parent.display()))?;
            }
            create_symlink_exact(&top_link, &entry_pkg)?;

            count += 1;
            tracing::debug!("injected {dep_name}@{version} into {importer_path} as {entry_name}");
        }
    }

    Ok(count)
}

/// Short, stable hash of `s` for use as a per-consumer `.aube/` suffix.
/// Ten hex chars is enough to avoid collisions across the few dozen
/// injection sites a realistic monorepo has without blowing out the
/// filename budget.
fn short_hash(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(10);
    for byte in digest.iter().take(5) {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// Create a relative symlink at `parent/<name>` pointing at `target`.
/// Handles scoped names (`@scope/foo`) by ensuring the scope dir exists
/// first. Existing entries at the path are removed so the call is
/// idempotent across re-runs.
fn create_symlink(parent: &Path, name: &str, target: &Path) -> miette::Result<()> {
    let link_path = parent.join(name);
    if let Some(p) = link_path.parent() {
        std::fs::create_dir_all(p).map_err(|e| miette!("inject: mkdir {}: {e}", p.display()))?;
    }
    if link_path.symlink_metadata().is_ok() {
        std::fs::remove_file(&link_path)
            .map_err(|e| miette!("inject: remove {}: {e}", link_path.display()))?;
    }
    create_symlink_exact(&link_path, target)
}

fn create_symlink_exact(link_path: &Path, target: &Path) -> miette::Result<()> {
    let link_parent = link_path.parent().unwrap_or(Path::new(""));
    let rel = pathdiff::diff_paths(target, link_parent).unwrap_or_else(|| target.to_path_buf());
    aube_linker::create_dir_link(&rel, link_path)
        .map_err(|e| miette!("inject: symlink {}: {e}", link_path.display()))
}
