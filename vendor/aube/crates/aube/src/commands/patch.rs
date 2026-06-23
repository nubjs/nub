//! `aube patch <pkg>@<version>` — extract a package from `node_modules`
//! into a temporary edit directory so the user can modify its files
//! and then run `aube patch-commit <dir>` to capture the diff.
//!
//! Mirrors `pnpm patch`. Two directories are created under a unique
//! temp parent: `source/` (the original, immutable, used as the diff
//! base) and `user/` (the writable copy printed to the user). A
//! `.<name>_patch_state.json` sidecar (derived from the embedder name via
//! `patch_state_filename()`; standalone aube: `.aube_patch_state.json`) carries
//! the package identity so `patch-commit` can locate the source dir given just
//! the user dir.

use crate::patches::copy_dir_all;
use clap::Args;
use miette::{IntoDiagnostic, Result, miette};
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct PatchArgs {
    /// Package spec, `<name>@<version>`.
    ///
    /// The package must already be installed in `node_modules` (we
    /// copy from the linked virtual store, not from the registry, so
    /// the layout matches what install would later patch).
    pub package: String,

    /// Directory to extract the writable copy into.
    ///
    /// When omitted, `aube` picks a fresh temp dir under the system
    /// tmpdir.
    #[arg(long, value_name = "DIR")]
    pub edit_dir: Option<PathBuf>,

    /// Ignore any existing patch entry for this package.
    ///
    /// Extracts a pristine copy from `node_modules` rather than
    /// re-applying the existing patch first. Accepted for pnpm parity;
    /// aube already extracts from the *linked* (post-patch) tree, so
    /// this flag is effectively informational here.
    #[arg(long)]
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
    let (name, version) = parse_spec(&args.package)?;

    // Locate the source files. The package must be installed —
    // `aube` extracts from the linked tree
    // (`<virtualStoreDir>/<dep_path>/...`) so the user edits exactly
    // what the runtime would see. Honors the `virtualStoreDir`
    // override via `resolve_virtual_store_dir_for_cwd`.
    let pnpm_dir = super::resolve_virtual_store_dir_for_cwd(&cwd);
    let vstore_max_len = super::resolve_virtual_store_dir_max_length_for_cwd(&cwd);
    let pkg_dir = find_pnpm_entry(&pnpm_dir, &name, &version, vstore_max_len)?;

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

    copy_dir_all(&pkg_dir, &source_dir)?;
    copy_dir_all(&pkg_dir, &user_dir)?;

    let state = serde_json::json!({
        "name": name,
        "version": version,
        "project": cwd.display().to_string(),
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
    Ok(PatchState {
        name,
        version,
        project,
        source_dir: parent.join("source"),
        user_dir: edit_dir.to_path_buf(),
    })
}

#[derive(Debug)]
pub struct PatchState {
    pub name: String,
    pub version: String,
    pub project: Option<PathBuf>,
    pub source_dir: PathBuf,
    pub user_dir: PathBuf,
}
