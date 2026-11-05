//! `aube patch <pkg>@<version>` — extract a package from `node_modules`
//! into a temporary edit directory so the user can modify its files
//! and then run `aube patch-commit <dir>` to capture the diff.
//!
//! Mirrors `pnpm patch`. Two directories are created under a unique
//! temp parent: `source/` (the package's original files, immutable,
//! used as the diff base) and `user/` (the writable copy printed to
//! the user, with any already-declared patch applied so the user
//! edits the currently effective tree). A
//! `.<name>_patch_state.json` sidecar (derived from the embedder name via
//! `patch_state_filename()`; standalone aube: `.aube_patch_state.json`) carries
//! the package identity so `patch-commit` can locate the source dir given just
//! the user dir.
//!
//! Like pnpm, both copies come from the store rather than the linked
//! tree, so `patch-commit` diffs original → edited and the result
//! REPLACES the patch file on recommit. A package with no
//! `name@version` store entry (`file:`, git, tarball) falls back to
//! snapshotting the linked tree, and its recommit appends instead.

use crate::patches::{ResolvedPatch, copy_dir_all};
use miette::{Context, IntoDiagnostic, Result, miette};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, usage_rs::Args)]
pub struct PatchArgs {
    /// Package spec, `<name>@<version>`.
    ///
    /// The package must already be installed in `node_modules` (aube
    /// copies the files install linked, not a fresh registry download,
    /// so the layout matches what install would later patch).
    pub package: String,

    /// Directory to extract the writable copy into.
    ///
    /// When omitted, `aube` picks a fresh temp dir under the system
    /// tmpdir.
    #[usage(long, value_name = "DIR")]
    pub edit_dir: Option<PathBuf>,

    /// Ignore any existing patch entry for this package.
    ///
    /// Starts the edit copy from the package's original files instead
    /// of applying the existing patch first, so the committed patch
    /// replaces it rather than building on it.
    #[usage(long)]
    pub ignore_existing: bool,
}

/// The patch-state sidecar filename, derived from the active embedder's
/// name: `.<name>_patch_state.json`. Standalone aube:
/// `.aube_patch_state.json`. Used by both the writer (`run`) and the
/// reader (`read_state`), so they stay paired.
fn patch_state_filename() -> String {
    format!(".{}_patch_state.json", aube_util::embedder().name)
}

pub async fn run(args: PatchArgs) -> Result<()> {
    // Mirror install's resolution: workspace-first so `aube patch`
    // from a workspace member finds the shared `.aube/` store at the
    // workspace root (where install actually wrote it). Falls back to
    // the project root for non-workspace trees.
    //
    // Yaml-only workspace roots are handled downstream:
    // `upsert_patched_dependency` routes through `config_write_target`
    // and lands the entry in the workspace yaml when no
    // `package.json` exists at the resolved root.
    let cwd = crate::dirs::workspace_or_project_root()?;
    let (spec_name, version) = parse_spec(&args.package)?;

    // Resolve the spec against the lockfile, as pnpm does. An npm alias
    // is found by its registry name, the key install applies a patch
    // under, and that is the name the patch is recorded with.
    let locked = locked_entries(&cwd, &spec_name, &version)?;
    let name = locked
        .first()
        .map_or(spec_name, |pkg| pkg.registry_name().to_string());
    // Only a package with no store copy is snapshotted from the linked
    // tree, so only then must one be found.
    let original = match original_files_index(&cwd, &locked)? {
        Some(index) => Original::Store(index),
        None => Original::Linked(find_linked_dir(&cwd, &locked, &name, &version)?),
    };

    // The edit snapshot must represent the currently declared patch.
    // In particular, a prior patch-commit can have updated the patch
    // and lockfile before its convenience relink failed. Refuse to
    // snapshot that stale state: the store lookup below reads the
    // lockfile, and the linked-tree fallback would append a diff taken
    // against the wrong baseline.
    if let Some(reason) = crate::state::check_needs_install(&cwd) {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_PATCH_FAILED,
            "installed package tree is out of date ({reason}); run `{}` before `{}`",
            aube_util::cmd("install"),
            aube_util::cmd("patch")
        ));
    }

    let declared =
        find_applied_patch(&cwd, &name, &version)?.filter(|patch| !patch.content.trim().is_empty());
    if matches!(original, Original::Linked(_)) && declared.is_some() && args.ignore_existing {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_PATCH_FAILED,
            "`--ignore-existing` needs the original files of {name}@{version}, and the store \
             has no copy of them (as with a `file:`, git or tarball dependency); remove the \
             patch with `{}` and run `{}` instead",
            aube_util::cmd("patch-remove"),
            aube_util::cmd("patch")
        ));
    }

    // Build the edit + source dirs. Defaults live under
    // `<tmp>/aube-patch-<name>-<version>-<pid>/` so concurrent
    // `aube patch` runs in different terminals don't collide.
    let parent = match args.edit_dir {
        Some(p) => p,
        None => default_edit_parent(&name, &version)?,
    };
    let source_dir = parent.join("source");
    let user_dir = parent.join("user");

    if source_dir.exists() {
        std::fs::remove_dir_all(&source_dir)
            .into_diagnostic()
            .map_err(|e| miette!("failed to clear {}: {e}", source_dir.display()))?;
    }
    if user_dir.exists() {
        std::fs::remove_dir_all(&user_dir)
            .into_diagnostic()
            .map_err(|e| miette!("failed to clear {}: {e}", user_dir.display()))?;
    }

    // Both copies start from the package's original files and the
    // declared patch is applied to the edit copy alone, the way pnpm
    // builds them. `source/` is then free of every patch, so the diff
    // `patch-commit` takes is the whole patch and replaces the file. The
    // store copy also leaves out whatever lifecycle scripts wrote into
    // the linked package, which would otherwise land in the diff.
    let replaces_patch = match &original {
        Original::Store(index) => {
            for dir in [&source_dir, &user_dir] {
                aube_linker::copy_package_from_store(index, dir).map_err(|e| {
                    miette!(
                        code = aube_codes::errors::ERR_AUBE_PATCH_FAILED,
                        "failed to copy {name}@{version} out of the store: {e}"
                    )
                })?;
            }
            if let Some(patch) = &declared
                && !args.ignore_existing
            {
                aube_linker::apply_multi_file_patch(&user_dir, &patch.content).map_err(|e| {
                    miette!(
                        code = aube_codes::errors::ERR_AUBE_PATCH_FAILED,
                        "failed to apply {} to the edit copy of {name}@{version}: {e}",
                        patch.rel
                    )
                })?;
            }
            true
        }
        // No store entry to diff against: snapshot the linked tree. When
        // it already carries a patch, the diff holds only the new edits
        // and `patch-commit` appends them to the patch file.
        Original::Linked(pkg_dir) => {
            copy_dir_all(pkg_dir, &source_dir)?;
            copy_dir_all(pkg_dir, &user_dir)?;
            declared.is_none()
        }
    };

    let state = serde_json::json!({
        "name": name,
        "version": version,
        "project": cwd.display().to_string(),
        "replaces_patch": replaces_patch,
    });
    std::fs::write(
        parent.join(patch_state_filename()),
        serde_json::to_string_pretty(&state).unwrap(),
    )
    .into_diagnostic()
    .map_err(|e| miette!("failed to write patch state: {e}"))?;

    println!(
        "You can now edit the following folder: {}",
        user_dir.display()
    );
    println!(
        "Once you're done with your changes, run \"{} '{}'\"",
        aube_util::cmd("patch-commit"),
        user_dir.display()
    );
    Ok(())
}

/// Find the linked package directory for `<name>@<version>` under
/// `.aube/`. Plain installs land at
/// `.aube/<encoded dep_path>/node_modules/<name>`, where the encoded
/// dep_path is produced by `dep_path_to_filename` — so a bare
/// `<name>@<version>` hits on an exact match, and peer-dep variants
/// show up with a flattened `_peer@ver` suffix. We scan the `.aube/`
/// entries (now flat, even for scoped packages) and match on either
/// the bare name or the peer-decorated prefix. When the same
/// `(name, version)` resolves under multiple peer contexts the choice
/// is arbitrary — pnpm has the same limitation; the user can
/// disambiguate via `--edit-dir`.
fn find_pnpm_entry(
    pnpm_dir: &Path,
    name: &str,
    version: &str,
    vstore_max_len: usize,
) -> Result<PathBuf> {
    use aube_lockfile::dep_path_filename::dep_path_to_filename;
    let exact_encoded = dep_path_to_filename(&format!("{name}@{version}"), vstore_max_len);
    let exact_dir = pnpm_dir
        .join(&exact_encoded)
        .join("node_modules")
        .join(name);
    if exact_dir.exists() {
        return Ok(exact_dir);
    }

    // Peer-dep variants: `<encoded exact>_peer@ver...`. `exact_encoded`
    // is already filesystem-safe (slashes turned into `+`) so we can
    // match on it directly against the flat `.aube/` entries.
    //
    // The prefix match only works as long as `exact_encoded` was NOT
    // itself run through the hash branch of `dep_path_to_filename` —
    // i.e. the bare `name@version` must be ≤ `max_length` and all
    // lowercase. If either condition fails, `exact_encoded` becomes a
    // `truncated_<32 hex>` shape and the peer-dep variants hash a
    // different input, so they no longer share the first
    // `exact_encoded` bytes. In practice npm's registry prohibits
    // uppercase names and bare `name@version` stays well under 120
    // bytes, so this doesn't bite real packages — but keep the
    // invariant in mind if `max_length` ever drops toward the bare
    // `name@version` length.
    let peer_prefix = format!("{exact_encoded}_");
    if let Ok(entries) = std::fs::read_dir(pnpm_dir) {
        for entry in entries.flatten() {
            let leaf = entry.file_name();
            let leaf_str = leaf.to_string_lossy();
            if leaf_str.starts_with(&peer_prefix) {
                let candidate = entry.path().join("node_modules").join(name);
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }
    Err(miette!(
        "package {name}@{version} is not installed (looked under {}). Run `{}` first.",
        pnpm_dir.display(),
        aube_util::cmd("install")
    ))
}

/// Where `patch` takes a package's original files from.
enum Original {
    /// The store's copy, the files install linked.
    Store(aube_store::PackageIndex),
    /// The linked package, when the store holds no copy of it.
    Linked(PathBuf),
}

/// The lockfile entries for `name@version`, matched on the registry
/// name. With no such package, the name an npm alias is installed
/// under matches instead, so `<alias>@<version>` finds it too. Empty
/// when the project has no lockfile.
fn locked_entries(
    cwd: &Path,
    name: &str,
    version: &str,
) -> Result<Vec<aube_lockfile::LockedPackage>> {
    let manifest = super::load_manifest_or_default(cwd)?;
    let graph = match aube_lockfile::parse_lockfile(cwd, &manifest) {
        Ok(graph) => graph,
        Err(aube_lockfile::Error::NotFound(_)) => return Ok(Vec::new()),
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };
    let matching = |by_registry_name: bool| -> Vec<aube_lockfile::LockedPackage> {
        graph
            .packages
            .values()
            .filter(|pkg| {
                let candidate = if by_registry_name {
                    pkg.registry_name()
                } else {
                    &pkg.name
                };
                pkg.version == version && candidate == name
            })
            .cloned()
            .collect()
    };
    let found = matching(true);
    Ok(if found.is_empty() {
        matching(false)
    } else {
        found
    })
}

/// The linked directory of a package the store holds no copy of. The
/// virtual store keeps each package under its lockfile `dep_path`, the
/// name the alias is installed under plus any peer suffix. The hoisted
/// linker has no virtual store and places the package at
/// `node_modules/<name>`.
fn find_linked_dir(
    cwd: &Path,
    locked: &[aube_lockfile::LockedPackage],
    name: &str,
    version: &str,
) -> Result<PathBuf> {
    use aube_lockfile::dep_path_filename::dep_path_to_filename;
    let pnpm_dir = super::resolve_virtual_store_dir_for_cwd(cwd);
    let vstore_max_len = super::resolve_virtual_store_dir_max_length_for_cwd(cwd);
    let linked = locked.iter().map(|pkg| {
        pnpm_dir
            .join(dep_path_to_filename(&pkg.dep_path, vstore_max_len))
            .join("node_modules")
            .join(&pkg.name)
    });
    let hoisted = locked
        .iter()
        .map(|pkg| cwd.join("node_modules").join(&pkg.name))
        .filter(|dir| installed_version(dir).as_deref() == Some(version));
    if let Some(dir) = linked.filter(|dir| dir.exists()).chain(hoisted).next() {
        return Ok(dir);
    }
    find_pnpm_entry(&pnpm_dir, name, version, vstore_max_len)
}

fn installed_version(pkg_dir: &Path) -> Option<String> {
    let manifest = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    manifest.get("version")?.as_str().map(str::to_string)
}

/// The store index of the package's original files, read with the
/// same key install uses: the lockfile integrity, or for a package
/// without one, the sha512 this project bound to it. `None` for a
/// `file:`, git or tarball dependency, which has no `name@version`
/// store entry, and when the store no longer holds every file.
fn original_files_index(
    cwd: &Path,
    locked: &[aube_lockfile::LockedPackage],
) -> Result<Option<aube_store::PackageIndex>> {
    let candidates: Vec<&aube_lockfile::LockedPackage> = locked
        .iter()
        .filter(|pkg| pkg.local_source.is_none())
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    let store = super::open_store(cwd)?;
    let no_integrity_index =
        crate::state::read_no_integrity_index_for(cwd, candidates.iter().copied());
    Ok(candidates.into_iter().find_map(|pkg| {
        let key = pkg.integrity.as_deref().or_else(|| {
            no_integrity_index
                .get(&format!("{}@{}", pkg.registry_name(), pkg.version))
                .map(String::as_str)
        })?;
        store.load_index_verified(pkg.registry_name(), &pkg.version, Some(key))
    }))
}

/// The declared patch that install applies to `name@version`, resolved
/// with the same selector priority the linker uses (pnpm's
/// `getPatchInfo`: exact > range > all). Returns `None` when no
/// `patchedDependencies` entry covers this package.
fn find_applied_patch(cwd: &Path, name: &str, version: &str) -> Result<Option<ResolvedPatch>> {
    let entries = crate::patches::load_declared_patch_paths(cwd)?;
    if entries.is_empty() {
        return Ok(None);
    }
    let resolved = crate::patches::resolve_declared_patches(cwd, entries)?;
    let spec = format!("{name}@{version}");
    // A synthetic single-package graph reuses the linker's exact
    // resolution rules (including range/wildcard keys and conflict
    // detection) instead of reimplementing key matching here.
    let packages = BTreeMap::from([(
        spec.clone(),
        aube_lockfile::LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            ..Default::default()
        },
    )]);
    let by_version = aube_lockfile::patch_groups::resolve_patched_by_version(&resolved, &packages)
        .map_err(crate::patches::map_patch_resolve_err)?;
    Ok(by_version
        .into_iter()
        .find_map(|(k, v)| (k == spec).then_some(v)))
}

fn parse_spec(input: &str) -> Result<(String, String)> {
    let (name, ver) = crate::commands::split_name_spec(input);
    let ver = ver.ok_or_else(|| {
        miette!("`{}` requires `<name>@<version>` (got {input:?}); a bare name is ambiguous because the same package can be installed at multiple versions", aube_util::cmd("patch"))
    })?;
    Ok((name.to_string(), ver.to_string()))
}

fn default_edit_parent(name: &str, version: &str) -> Result<PathBuf> {
    let safe_name = name.replace('/', "+");
    let dir = std::env::temp_dir().join(format!(
        "aube-patch-{safe_name}-{version}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir)
        .into_diagnostic()
        .map_err(|e| miette!("failed to create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Read the `.<name>_patch_state.json` sidecar (standalone aube:
/// `.aube_patch_state.json`) that `patch` writes next to a user-edit dir.
/// `patch-commit` calls this to recover the package identity (name, version)
/// and the matching source dir.
pub fn read_state(edit_dir: &Path) -> Result<PatchState> {
    let parent = edit_dir
        .parent()
        .ok_or_else(|| miette!("edit dir {} has no parent", edit_dir.display()))?;
    let state_path = parent.join(patch_state_filename());
    let raw = std::fs::read_to_string(&state_path)
        .into_diagnostic()
        .map_err(|e| {
            miette!(
                "{} is not a directory created by `{}` (no state sidecar: {e})",
                edit_dir.display(),
                aube_util::cmd("patch")
            )
        })?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .into_diagnostic()
        .map_err(|e| miette!("corrupt patch state at {}: {e}", state_path.display()))?;
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .ok_or_else(|| miette!("patch state missing `name`"))?
        .to_string();
    let version = v
        .get("version")
        .and_then(|x| x.as_str())
        .ok_or_else(|| miette!("patch state missing `version`"))?
        .to_string();
    let project = v.get("project").and_then(|x| x.as_str()).map(PathBuf::from);
    let replaces_patch = v
        .get("replaces_patch")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    Ok(PatchState {
        name,
        version,
        project,
        source_dir: parent.join("source"),
        user_dir: edit_dir.to_path_buf(),
        replaces_patch,
    })
}

#[derive(Debug)]
pub struct PatchState {
    pub name: String,
    pub version: String,
    pub project: Option<PathBuf>,
    pub source_dir: PathBuf,
    pub user_dir: PathBuf,
    /// `source/` holds the package without any declared patch, so the
    /// diff is the complete patch and replaces the patch file. False
    /// when it is a snapshot of an already-patched linked tree, and for
    /// an edit dir from a version that predates the field: those diffs
    /// hold only the new edits, which are appended.
    pub replaces_patch: bool,
}
