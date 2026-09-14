//! The phantom scan's two seams under the pnpm engine.
//!
//! nub keeps a package out of the shared virtual store when the package's own
//! published code imports something it never declared: under a store shared
//! between projects there is nowhere project-local for that import to resolve
//! to. The scan that decides this, and the ancestor closure that makes acting
//! on it sound, are nub's; the engine only has to ask.
//!
//! Both halves are the ones [`crate::dynamic_phantom`] and
//! [`super::phantom_closure`] already run against the other engine, so what
//! is here is the adaptation rather than the policy:
//!
//! - the OBSERVER hears about each package extracted into the store and
//!   writes its verdict to a per-content sidecar, which costs nothing extra
//!   because the scan overlaps the network-bound fetch it rides on;
//! - the POLICY answers which packages the install must keep project-local,
//!   reading those sidecars — and scanning anything that has none, since a
//!   package already in the store is never extracted and so is never
//!   observed.
//!
//! That second half is why the policy is handed each package's store row and
//! not merely its name. Deciding from what was observed alone would silently
//! stop ejecting on exactly the second install of anything.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pnpm_store_dir::{
    ExtractObserver, ExtractedPackage, MaterializePolicy, ResolvedPackage, StoreIndex,
};

use crate::dynamic_phantom;

/// The store this run reads and writes, as a handle — the one thing every path
/// below is derived from.
///
/// A handle rather than the raw setting because the store-version suffix is the
/// engine's: `StoreDir::from` appends it and the setting does not carry it, and
/// nothing downstream fails on the unsuffixed path. `StoreIndex::open` would
/// CREATE an empty index there and answer "no such package" for everything, and
/// a sidecar written there lands in a sibling of the real store that no install
/// ever reads. Both are silent, so deriving from the handle is the only spelling
/// that cannot drift from the store.
///
/// Taken from the settings nub published for the project, which resolve once at
/// the walked-up workspace root — `.npmrc` discovery does not walk up, so
/// resolving against the process cwd instead would miss a `store-dir` override
/// for every command run from inside a workspace member and silently answer with
/// the default store (#643). `None` under pnpm's own identity, which resolves
/// its store itself and installs neither of these hooks.
fn host_store() -> Option<pnpm_store_dir::StoreDir> {
    super::pnpm_engine::host_store_dir().map(pnpm_store_dir::StoreDir::from)
}

/// The sidecar tier of that store: `<store>/phantom/`, beside the CAS and the
/// index, so the verdicts move with the packages they are keyed to.
fn sidecar_dir(store: &pnpm_store_dir::StoreDir) -> PathBuf {
    store.root().join("phantom")
}

/// Scan each package as it lands in the store.
#[derive(Debug)]
struct ScanOnExtract {
    dir: Option<PathBuf>,
}

impl ExtractObserver for ScanOnExtract {
    fn package_extracted(&self, extracted: ExtractedPackage<'_>) {
        // Off under the same A/B seam the policy reads, so the disabled arm
        // writes no sidecars rather than writing verdicts nothing consults.
        if !dynamic_phantom::enabled() {
            return;
        }
        let Some(dir) = &self.dir else {
            return;
        };
        let files = file_list(extracted.cas_paths);
        dynamic_phantom::scan_and_cache_files(dir, &fingerprint_of(extracted.files), &files);
    }
}

/// Keep a package out of the shared store when its own code imports what it
/// never declared — and keep everything that imports it out alongside it.
///
/// The closure is the whole reason this is not a per-package question. A
/// project-local package whose importer stayed in the shared store is worse
/// than no ejection at all: the importer goes on resolving the shared copy,
/// so the package is present twice at two real paths, and anything that
/// depends on it being a single instance breaks in a way nothing reports.
#[derive(Debug)]
struct EjectPhantomImporters {
    /// The store this run reads: the packages it did not extract itself, and
    /// the sidecar tier holding their verdicts. ONE handle for both, so the
    /// index the scan reads and the sidecars it writes cannot name two stores.
    store: Option<pnpm_store_dir::StoreDir>,
    /// Packages the project named itself, plus the ones nub always ejects.
    seeds: Vec<String>,
}

impl MaterializePolicy for EjectPhantomImporters {
    fn materialize_locally(&self, resolved: &[ResolvedPackage<'_>]) -> HashSet<String> {
        // The internal A/B seam turns the whole eject off — the configured
        // seed included, exactly as it does for the other engine, where the
        // expansion hook is simply never installed and every package takes
        // the shared layout.
        if !dynamic_phantom::enabled() {
            return HashSet::new();
        }
        let mut keep: HashSet<String> = resolved
            .iter()
            .filter(|package| self.seeded(package.id))
            .map(|package| package.id.to_owned())
            .collect();
        keep.extend(self.flagged(resolved));
        grow_to_importers(resolved, keep)
    }

    /// Everything that changes this policy's answer for an unchanged graph —
    /// the scanner, the curated list, the eject algorithm and the internal A/B
    /// switch — so the engine re-links a warm tree the previous answer shaped
    /// instead of reporting it up to date.
    fn fingerprint(&self) -> Option<String> {
        Some(dynamic_phantom::settings_fingerprint())
    }
}

impl EjectPhantomImporters {
    /// Whether one of the seed names claims this package before any scan.
    ///
    /// `vite` is the one name answered from the concrete VERSION rather than
    /// the name alone, and here is the first point that has one: from 8.1 vite
    /// reads the virtual store's location out of `node_modules/.modules.yaml`
    /// itself, so it is served correctly from the shared store and ejecting it
    /// would drag vite and its whole ancestor closure project-local for
    /// nothing. Below 8.1 the eject is what puts a writable copy where the
    /// backported sniff can be applied, so those still move — every copy in
    /// the graph, embedded ones included, which is the case a name seed alone
    /// cannot distinguish.
    ///
    /// Deliberately blind to where the seed came from, as the other engine
    /// has it: a project naming `vite` in its own eject list gets the same
    /// answer, because a vite that needs no eject is already working.
    fn seeded(&self, package_id: &str) -> bool {
        if let Some(version) = package_id.strip_prefix("vite@") {
            return super::vite_compat::vite_lt_8_1(version);
        }
        self.seeds.iter().any(|seed| names(package_id, seed))
    }

    /// The packages whose own code imports something undeclared.
    ///
    /// A package with no store row to read is skipped rather than ejected: a
    /// scan that could not run is not evidence of a phantom, and ejecting on
    /// a miss would move packages for no reason on every install.
    fn flagged(&self, resolved: &[ResolvedPackage<'_>]) -> Vec<String> {
        let Some(store) = &self.store else {
            return Vec::new();
        };
        let cache_dir = sidecar_dir(store);
        // `open_in`, never `open`: the raw-path form is the silent-miss trap
        // [`host_store`] describes.
        let Ok(index) = StoreIndex::open_in(store) else {
            return Vec::new();
        };
        // The names a project depends on directly. Under the shared store a
        // package's own resolution walk reaches only its siblings, so the
        // project's top level is the one place an eject changes what an
        // undeclared import can see.
        let top_level: HashSet<&str> = resolved
            .iter()
            .filter(|package| package.root_direct)
            .filter_map(|package| package_name(package.id))
            .collect();
        resolved
            .iter()
            .filter(|package| {
                let Some(scan) = package
                    .index_key
                    .and_then(|key| verdict(&index, store, &cache_dir, key))
                else {
                    return false;
                };
                if !scan.has_unguarded_phantom {
                    return false;
                }
                let siblings: HashSet<&str> = package
                    .dependencies
                    .iter()
                    .filter_map(|id| package_name(id))
                    .collect();
                should_seed(&scan.targets, &siblings, &top_level)
            })
            .map(|package| package.id.to_owned())
            .collect()
    }
}

/// Whether a package the scan flagged must actually be kept out of the
/// shared store.
///
/// The default is YES, and a flag is downgraded only when every undeclared
/// target can be PROVEN to resolve identically either way: a target that is
/// both a direct sibling of the flagged package and absent from the
/// project's top level is reachable from the shared copy and unreachable
/// from a project-local one, so moving the package changes nothing for it.
///
/// The asymmetry is deliberate and it is the whole safety story: a wrong
/// SKIP is a real import failure at runtime, while a redundant eject costs
/// only disk. So every uncertainty — no targets recorded, a target that is
/// not a direct sibling, a target the project itself depends on — keeps the
/// eject.
fn should_seed(
    targets: &[nub_phantom_scan::PhantomTarget],
    siblings: &HashSet<&str>,
    top_level: &HashSet<&str>,
) -> bool {
    if targets.is_empty() {
        return true;
    }
    !targets.iter().all(|target| {
        siblings.contains(target.name.as_str()) && !top_level.contains(target.name.as_str())
    })
}

/// The package name inside an install identifier, which spells a registry
/// package `name@version` — with the name's own leading `@` for a scoped
/// one, so the separator is the LAST `@` rather than the first. The peer
/// suffix is already stripped by the time an identifier reaches here.
///
/// A non-registry resolution is identified by its bare resolution id
/// instead, carrying no name at all, so this answers nothing usable for
/// one. That is the safe direction: a name that matches no target leaves
/// the eject in place.
fn package_name(package_id: &str) -> Option<&str> {
    let at = package_id.rfind('@').filter(|at| *at > 0)?;
    Some(&package_id[..at])
}

/// The verdict for one stored package: its cached sidecar, or a scan run now
/// and cached for the next install.
fn verdict(
    index: &StoreIndex,
    store: &pnpm_store_dir::StoreDir,
    cache_dir: &Path,
    index_key: &str,
) -> Option<nub_phantom_scan::ScanResult> {
    let row = index.get(index_key).ok()??;
    let files = file_list(&cas_paths_of(store, &row));
    dynamic_phantom::cached_or_scan_verdict_files(cache_dir, &fingerprint_of(&row), &files)
}

/// Grow `keep` until it holds every package that transitively imports one of
/// its members. Bounded by construction: each pass either adds a package or
/// is the last, and no package is added twice.
fn grow_to_importers(
    resolved: &[ResolvedPackage<'_>],
    mut keep: HashSet<String>,
) -> HashSet<String> {
    loop {
        let added: Vec<String> = resolved
            .iter()
            .filter(|package| !keep.contains(package.id))
            .filter(|package| package.dependencies.iter().any(|dep| keep.contains(dep)))
            .map(|package| package.id.to_owned())
            .collect();
        if added.is_empty() {
            return keep;
        }
        keep.extend(added);
    }
}

/// Whether `package_id` — `"{name}@{version}"` — is the package called `name`.
fn names(package_id: &str, name: &str) -> bool {
    package_id
        .strip_prefix(name)
        .is_some_and(|rest| rest.starts_with('@'))
}

/// The scan's input: each file's path inside the package, and the
/// content-addressed file holding it.
fn file_list(cas_paths: &HashMap<String, PathBuf>) -> Vec<(String, PathBuf)> {
    cas_paths
        .iter()
        .map(|(rel, path)| (rel.clone(), path.clone()))
        .collect()
}

/// A content fingerprint over the store's own record of the package, which is
/// what keys its sidecar.
fn fingerprint_of(files: &pnpm_store_dir::PackageFilesIndex) -> String {
    dynamic_phantom::content_fingerprint(
        files
            .files
            .iter()
            .map(|(path, info)| (path.as_str(), info.digest.as_str(), info.mode & 0o111 != 0)),
    )
}

/// Where a stored package's files actually are. The row records each file's
/// digest, and the store derives the blob's location from that and the mode,
/// by the same rule the write side used. A file whose digest the store will
/// not accept is dropped, which costs the scan that file rather than the
/// whole package.
fn cas_paths_of(
    store: &pnpm_store_dir::StoreDir,
    row: &pnpm_store_dir::PackageFilesIndex,
) -> HashMap<String, PathBuf> {
    row.files
        .iter()
        .filter_map(|(rel, info)| {
            Some((
                rel.clone(),
                store.cas_file_path_by_mode(&info.digest, info.mode)?,
            ))
        })
        .collect()
}

/// The observer this host registers.
pub(crate) fn extract_observer() -> Arc<dyn ExtractObserver> {
    Arc::new(ScanOnExtract {
        dir: host_store().as_ref().map(sidecar_dir),
    })
}

/// The policy this host registers.
pub(crate) fn materialize_policy() -> Arc<dyn MaterializePolicy> {
    Arc::new(EjectPhantomImporters {
        store: host_store(),
        seeds: super::phantom_closure::configured_eject_names(),
    })
}

#[cfg(test)]
mod tests;
