//! Bundled-local-ref planning and materialization for `aube deploy`.
//!
//! Workspace siblings + `file:` / `link:` / `portal:` targets reachable
//! from the deployed package land at
//! `<target>/.<name>-deploy-injected/<id>/` (derived from the embedder name;
//! standalone aube: `<target>/.aube-deploy-injected/<id>/`). [`plan_injections`] BFS-walks
//! the deployed manifest plus every bundled sibling's manifest, records
//! one [`Injection`] per distinct local-ref target,
//! [`materialize_injections`] copies the bytes, and the rewrite layer
//! turns each affected dep specifier into a relative `file:` pointer at
//! the staged copy. Tarball sources are an opaque carve-out — they ship
//! verbatim and don't recurse.
use super::DeployArgs;
use super::filtering::StripFields;
use aube_manifest::PackageJson;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

/// Where a bundled local ref ends up under
/// `<target>/.<name>-deploy-injected/` (derived from the embedder name;
/// standalone aube: `.aube-deploy-injected/`). Distinct sources with distinct
/// canonical paths each get their own entry — siblings shared between
/// multiple parents bundle once.
#[derive(Debug, Clone)]
pub(super) struct Injection {
    /// Source directory (workspace sibling root or `file:` directory)
    /// or tarball path on disk. Reads come from here.
    pub(super) source_dir: PathBuf,
    /// Set when `source_dir` is actually a tarball file (`*.tgz` /
    /// `*.tar.gz`) rather than a directory. Materialization copies the
    /// tarball verbatim and the rewriter emits `file:` pointers at the
    /// staged tarball.
    pub(super) is_tarball: bool,
    /// Absolute path inside the deploy target where the bundled copy
    /// lives. For directory sources this is the staged package root;
    /// for tarball sources this is the directory holding the tarball.
    pub(super) target_dir: PathBuf,
    /// For tarball sources: filename under `target_dir`. Empty for
    /// directory sources.
    pub(super) tarball_filename: String,
}

/// Map keyed by the canonical absolute source path — that gives us
/// stable identity across multiple rewriters that find the same local
/// ref via different relative specs (e.g. `file:../foo` from two
/// different consumer manifests).
pub(super) type InjectionPlan = BTreeMap<PathBuf, Injection>;

/// BFS the deployed package's manifest plus every bundled sibling's
/// manifest, recording one [`Injection`] per distinct local-ref target.
/// The returned map preserves insertion order via canonical path keys —
/// callers iterate it to materialize copies and rewrite manifests in any
/// order they like.
pub(super) fn plan_injections(
    deployed_pkg_dir: &Path,
    target_root: &Path,
    ws_index: &BTreeMap<String, (PathBuf, Option<String>)>,
    args: &DeployArgs,
) -> miette::Result<InjectionPlan> {
    // Injected-deps staging leaf from the active embedder's name:
    // `.<name>-deploy-injected`. Standalone aube → `.aube-deploy-injected`.
    let injected_root =
        target_root.join(format!(".{}-deploy-injected", aube_util::embedder().name));
    let mut plan: InjectionPlan = BTreeMap::new();
    // Track id collisions so a second sibling with the same encoded
    // name gets a `_2`, `_3`, ... suffix. Keyed by the encoded id.
    let mut used_ids: BTreeMap<String, u32> = BTreeMap::new();
    // Don't bundle the deployed package itself: a sibling B with a
    // back-dep `"@deployed-pkg": "workspace:*"` would otherwise duplicate
    // the deploy root under the injected-deps dir and break runtime
    // singleton assumptions (two distinct module instances). The
    // rewriter handles back-refs separately.
    let deployed_canonical = super::canonicalize(deployed_pkg_dir);

    // BFS frontier: each entry is `(source_dir, strip)`. The first
    // entry is the deployed package; everything queued after is a
    // bundled sibling, which uses the bundled-sibling strip policy.
    let mut queue: VecDeque<(PathBuf, StripFields)> = VecDeque::new();
    queue.push_back((deployed_pkg_dir.to_path_buf(), StripFields::for_args(args)));

    while let Some((pkg_dir, strip)) = queue.pop_front() {
        let manifest_path = pkg_dir.join("package.json");
        let manifest = crate::commands::load_manifest(&manifest_path)?;

        for (dep_name, dep_spec) in iter_strippable_deps(&manifest, strip) {
            // Workspace sibling refs win over file:/link: parsing —
            // a workspace sibling can also be referenced via `link:`
            // pointing at its dir, but the workspace index is the
            // authoritative match.
            if aube_util::pkg::is_workspace_spec(&dep_spec) {
                let Some((sibling_dir, _)) = ws_index.get(&dep_name) else {
                    return Err(miette!(
                        "{}: {} declares `{dep_name}: {dep_spec}` but no workspace package named {dep_name:?} was found",
                        aube_util::cmd("deploy"),
                        manifest_path.display()
                    ));
                };
                let canonical = super::canonicalize(sibling_dir);
                if canonical == deployed_canonical {
                    continue;
                }
                if !plan.contains_key(&canonical) {
                    let id = unique_id(&dep_name, &mut used_ids);
                    plan.insert(
                        canonical.clone(),
                        Injection {
                            source_dir: canonical.clone(),
                            is_tarball: false,
                            target_dir: injected_root.join(&id),
                            tarball_filename: String::new(),
                        },
                    );
                    queue.push_back((canonical, StripFields::for_bundled_sibling(args)));
                }
            } else if let Some(local) = aube_lockfile::LocalSource::parse(&dep_spec, &pkg_dir) {
                match local {
                    aube_lockfile::LocalSource::Directory(rel)
                    | aube_lockfile::LocalSource::Link(rel)
                    | aube_lockfile::LocalSource::Portal(rel) => {
                        let abs = pkg_dir.join(&rel);
                        let canonical = super::canonicalize(&abs);
                        // Same back-ref guard as the `workspace:` branch:
                        // a bundled sibling reaching the deployed package
                        // via `file:../deployed-pkg` must not duplicate it.
                        if canonical == deployed_canonical {
                            continue;
                        }
                        if !plan.contains_key(&canonical) {
                            let id_seed = canonical
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or(&dep_name);
                            let id = unique_id(id_seed, &mut used_ids);
                            plan.insert(
                                canonical.clone(),
                                Injection {
                                    source_dir: canonical.clone(),
                                    is_tarball: false,
                                    target_dir: injected_root.join(&id),
                                    tarball_filename: String::new(),
                                },
                            );
                            // Recurse: a bundled `file:` directory may
                            // itself reach further siblings or `file:`
                            // targets. Tarballs don't recurse — they
                            // ship as opaque archives.
                            queue.push_back((
                                canonical.clone(),
                                StripFields::for_bundled_sibling(args),
                            ));
                        }
                    }
                    aube_lockfile::LocalSource::Tarball(rel) => {
                        let abs = pkg_dir.join(&rel);
                        let canonical = super::canonicalize(&abs);
                        if !plan.contains_key(&canonical) {
                            let stem = canonical
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or(&dep_name);
                            let id = unique_id(stem, &mut used_ids);
                            let filename = canonical
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| format!("{stem}.tgz"));
                            plan.insert(
                                canonical.clone(),
                                Injection {
                                    source_dir: canonical.clone(),
                                    is_tarball: true,
                                    target_dir: injected_root.join(&id),
                                    tarball_filename: filename,
                                },
                            );
                        }
                    }
                    // Exec / Git / RemoteTarball: install fetches these
                    // standalone from their source — the deploy target
                    // doesn't need a bundled copy.
                    aube_lockfile::LocalSource::Exec(_)
                    | aube_lockfile::LocalSource::Git(_)
                    | aube_lockfile::LocalSource::RemoteTarball(_) => {}
                }
            }
        }
    }

    Ok(plan)
}

/// Iterate the three bundleable dep fields, skipping any field the
/// strip policy will drop. Yields `(name, spec)` pairs the rewriter
/// will keep — the bundling planner only needs to see deps that
/// survive the strip, otherwise it would copy a sibling that the
/// deployed manifest is about to discard. `peerDependencies` is
/// intentionally omitted: peers are satisfied by the consumer's
/// installed tree, not bundled into the deploy.
fn iter_strippable_deps(manifest: &PackageJson, strip: StripFields) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if !strip.dependencies {
        for (k, v) in &manifest.dependencies {
            out.push((k.clone(), v.clone()));
        }
    }
    if !strip.dev_dependencies {
        for (k, v) in &manifest.dev_dependencies {
            out.push((k.clone(), v.clone()));
        }
    }
    if !strip.optional_dependencies {
        for (k, v) in &manifest.optional_dependencies {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

/// Pick a filesystem-safe id under the `.<name>-deploy-injected/` dir
/// (standalone aube: `.aube-deploy-injected/`). Starts from `seed` (with `/`
/// and any other unsafe characters sanitized) and disambiguates collisions with
/// `_2`, `_3`, ... — collisions are rare and the suffix keeps the staged path
/// readable when debugging.
fn unique_id(seed: &str, used: &mut BTreeMap<String, u32>) -> String {
    let cleaned: String = seed
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | ' ' | '\t') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let base = if cleaned.is_empty() {
        "pkg".to_string()
    } else {
        cleaned
    };
    let count = used.entry(base.clone()).or_insert(0);
    *count += 1;
    if *count == 1 {
        base
    } else {
        format!("{base}_{count}")
    }
}

/// Copy each planned source into its `target_dir`. Directory sources
/// honor pack's selection (or the `deployAllFiles` carve-out when the
/// caller opted in); tarball sources copy the archive bytes verbatim.
pub(super) fn materialize_injections(
    plan: &InjectionPlan,
    ws_index: &BTreeMap<String, (PathBuf, Option<String>)>,
    deploy_all_files: bool,
) -> miette::Result<()> {
    for inj in plan.values() {
        std::fs::create_dir_all(&inj.target_dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to create {}", inj.target_dir.display()))?;

        if inj.is_tarball {
            let dst = inj.target_dir.join(&inj.tarball_filename);
            std::fs::copy(&inj.source_dir, &dst)
                .into_diagnostic()
                .wrap_err_with(|| {
                    format!(
                        "failed to copy {} -> {}",
                        inj.source_dir.display(),
                        dst.display()
                    )
                })?;
            continue;
        }

        // Directory source. Reuse pack's file selection by default so a
        // sibling with `files: [...]` ships the same payload it would
        // publish; honor `deployAllFiles=true` for parity with the
        // top-level deployed-package selection.
        let source_is_workspace_sibling = ws_index
            .values()
            .any(|(p, _)| super::canonicalize(p) == inj.source_dir);
        let files: Vec<(PathBuf, String)> = if deploy_all_files && source_is_workspace_sibling {
            super::staging::collect_all_files(&inj.source_dir, &inj.target_dir)?
        } else {
            let manifest = crate::commands::load_manifest(&inj.source_dir.join("package.json"))?;
            crate::commands::pack::collect_package_files(&inj.source_dir, &manifest)?
        };
        for (src, rel) in &files {
            let dst = inj.target_dir.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
            }
            std::fs::copy(src, &dst)
                .into_diagnostic()
                .wrap_err_with(|| {
                    format!("failed to copy {} -> {}", src.display(), dst.display())
                })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_id_disambiguates_collisions() {
        let mut used = BTreeMap::new();
        assert_eq!(unique_id("lib", &mut used), "lib");
        assert_eq!(unique_id("lib", &mut used), "lib_2");
    }

    #[test]
    fn unique_id_sanitizes_unsafe_chars() {
        let mut used = BTreeMap::new();
        assert_eq!(unique_id("@scope/name", &mut used), "@scope_name");
    }
}
