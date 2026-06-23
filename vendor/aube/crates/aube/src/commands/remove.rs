use super::install;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};

#[derive(Debug, Clone, Args)]
pub struct RemoveArgs {
    /// Package(s) to remove
    pub packages: Vec<String>,
    /// Remove only from devDependencies
    #[arg(short = 'D', long)]
    pub save_dev: bool,
    /// Remove from the global install directory instead of the project
    #[arg(short = 'g', long)]
    pub global: bool,
    /// Skip root lifecycle scripts during the chained reinstall
    #[arg(long)]
    pub ignore_scripts: bool,
    /// Remove the dependency from the workspace root's `package.json`.
    ///
    /// Applies regardless of the current working directory: walks up
    /// from cwd looking for `aube-workspace.yaml`, `pnpm-workspace.yaml`,
    /// or a `package.json` with a `workspaces` field and runs the
    /// remove against that directory. Takes precedence over `--filter`
    /// when both are supplied (same as `add --workspace`).
    #[arg(short = 'w', long, conflicts_with = "global")]
    pub workspace: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(
    args: RemoveArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    let packages = &args.packages[..];
    if packages.is_empty() {
        return Err(miette!("no packages specified"));
    }

    if !filter.is_empty() && !args.global && !args.workspace {
        return run_filtered(args, &filter).await;
    }

    if args.global {
        return run_global(packages);
    }

    // `--workspace` / `-w`: redirect the remove at the workspace root
    // before anything reads `dirs::cwd()`.
    if args.workspace {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = super::find_workspace_root(&start).wrap_err("--workspace")?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to chdir into {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }

    let cwd = crate::dirs::project_root()?;
    let _lock = super::take_project_lock(&cwd)?;
    let manifest_path = cwd.join("package.json");

    let mut manifest = super::load_manifest(&manifest_path)?;

    for name in packages {
        let removed = if args.save_dev {
            manifest.dev_dependencies.remove(name).is_some()
        } else {
            // Strip from every section. `--save-peer` previously
            // wrote to both peerDependencies and devDependencies so
            // both need clearing on a full uninstall.
            let from_deps = manifest.dependencies.remove(name).is_some();
            let from_dev = manifest.dev_dependencies.remove(name).is_some();
            let from_optional = manifest.optional_dependencies.remove(name).is_some();
            let from_peer = manifest.peer_dependencies.remove(name).is_some();
            from_deps || from_dev || from_optional || from_peer
        };

        // Also prune sidecar metadata so a later `aube add <name>`
        // does not silently inherit the old entries. Main concern is
        // pnpm.allowBuilds. If user removes a build-script package
        // then later adds a malicious package with the same name
        // (typo-squat, name reclaim), the old allowBuilds entry
        // would auto-approve its postinstall. Same hazard, lower
        // risk, for overrides and resolutions which just leave dead
        // rewrite rules around. Matches pnpm remove behavior.
        prune_sidecar_entries(&mut manifest, name);

        if !removed {
            let section = if args.save_dev {
                "a devDependency"
            } else {
                "a dependency"
            };
            return Err(miette!("package '{name}' is not {section}"));
        }

        eprintln!("  - {name}");
    }

    // Write updated package.json atomically. Crash mid-write would
    // otherwise truncate the user manifest, worst-case aube failure
    // mode. Tempfile + persist keeps the swap atomic.
    //
    // We mutate the parsed JSON object in place rather than going
    // through `sync_manifest_dep_sections`. The latter rebuilds each
    // dep section from `BTreeMap`, which would alphabetize the keys
    // and reshuffle the user's manifest as a side-effect of removing
    // an unrelated entry. `aube remove` must only touch the names the
    // user named — surrounding entries stay in their original on-disk
    // order. (`aube add` keeps using the BTreeMap path because it
    // both inserts and is expected to land new entries in a stable
    // sorted spot.)
    let dep_sections: &[&str] = if args.save_dev {
        &["devDependencies"]
    } else {
        &[
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ]
    };
    super::update_manifest_json_object(&manifest_path, |obj| {
        for section_key in dep_sections {
            let Some(section) = obj.get_mut(*section_key).and_then(|v| v.as_object_mut()) else {
                continue;
            };
            for name in packages {
                // shift_remove rather than remove: serde_json's `Map`
                // is an `IndexMap` under the `preserve_order` feature
                // and the default `remove` is `swap_remove`, which
                // would scramble the surviving keys. shift_remove
                // keeps every other entry in its on-disk position.
                section.shift_remove(name);
            }
            if section.is_empty() {
                obj.shift_remove(*section_key);
            }
        }
        for name in packages {
            prune_sidecar_entries_json(obj, name);
        }
        Ok(())
    })?;
    eprintln!("Updated package.json");

    // Re-resolve + relink through the install pipeline, the same way
    // `add` chains into `install::run` after mutating the manifest.
    // The pipeline is the only path that seeds the resolver with the
    // local workspace packages (`discover_workspace_plan` →
    // `resolve_workspace` with `ws_package_versions`), so a sibling
    // dep declared `workspace:*` resolves to its local copy instead of
    // being looked up on the registry. The previous standalone
    // `resolver.resolve(&manifest, …)` here passed a single `.`
    // importer and an empty workspace map, which made any surviving
    // `workspace:*` dependency fail with ERR_AUBE_NO_MATCHING_VERSION
    // — and left the manifest edited but the lockfile stale.
    //
    // `Fix` (vs `Prefer`) re-resolves while seeding from the existing
    // lockfile, so unchanged specs keep their pinned versions and only
    // the removed entry (and anything that depended on it) drops out —
    // matching `add`'s post-mutation contract. `with_mode()` already
    // skips the root lifecycle hooks (chained-call contract).
    let mut opts =
        install::InstallOptions::with_mode(super::chained_frozen_mode(install::FrozenMode::Fix));
    opts.ignore_scripts = args.ignore_scripts;
    install::run(opts).await?;

    Ok(())
}

async fn run_filtered(
    args: RemoveArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let cwd = crate::dirs::cwd()?;
    let (_root, matched) = super::select_workspace_packages(&cwd, filter, "remove")?;
    let result = async {
        for pkg in matched {
            // Match pnpm's recursive-remove semantics: silently skip
            // projects that don't declare any of the named packages,
            // and per-project narrow the package list to just the
            // ones present so a partial overlap (e.g. `aube -r remove
            // pkg1 pkg2` against a project that only declares `pkg1`)
            // doesn't trip the strict "package is not a dependency"
            // error in `run` after the first mutation has already
            // landed. The single-project (`aube remove`) path keeps
            // the strict per-package error so an isolated typo in
            // one shell still fails fast.
            let present = manifest_present_deps(&pkg.manifest, &args.packages, args.save_dev);
            if present.is_empty() {
                continue;
            }
            super::retarget_cwd(&pkg.dir)?;
            let mut narrowed = args.clone();
            narrowed.packages = present;
            Box::pin(run(
                narrowed,
                aube_workspace::selector::EffectiveFilter::default(),
            ))
            .await?;
        }
        Ok(())
    }
    .await;
    super::finish_filtered_workspace(&cwd, result)
}

fn manifest_present_deps(
    manifest: &aube_manifest::PackageJson,
    packages: &[String],
    save_dev: bool,
) -> Vec<String> {
    packages
        .iter()
        .filter(|name| {
            if save_dev {
                manifest.dev_dependencies.contains_key(*name)
            } else {
                manifest.dependencies.contains_key(*name)
                    || manifest.dev_dependencies.contains_key(*name)
                    || manifest.optional_dependencies.contains_key(*name)
                    || manifest.peer_dependencies.contains_key(*name)
            }
        })
        .cloned()
        .collect()
}

/// `aube remove -g <pkg>...` — delete globally-installed packages and
/// unlink their bins. Each named package is looked up in the global pkg
/// dir; if found, the whole install (hash symlink + physical dir + bins)
/// is removed atomically.
fn run_global(packages: &[String]) -> miette::Result<()> {
    let layout = super::global::GlobalLayout::resolve()?;

    let mut any_removed = false;
    for name in packages {
        match super::global::find_package(&layout.pkg_dir, name) {
            Some(info) => {
                super::global::remove_package(&info, &layout)?;
                eprintln!("Removed global {name}");
                any_removed = true;
            }
            None => {
                eprintln!("Not globally installed: {name}");
            }
        }
    }
    if !any_removed {
        return Err(miette!("no matching global packages were removed"));
    }
    Ok(())
}

fn prune_sidecar_entries_json(obj: &mut serde_json::Map<String, serde_json::Value>, name: &str) {
    // shift_remove (not remove → swap_remove) keeps the surrounding
    // keys in their original on-disk position. Same rationale as the
    // dep-section pruning above: `remove` must not reshuffle the user's
    // manifest as a side effect.
    // Compat namespaces plus the embedder's own (standalone aube →
    // ["pnpm", "aube"]); an embedder with no namespace ("") skips it.
    let id = aube_util::embedder();
    let ns_keys = id
        .compatible_names
        .iter()
        .copied()
        .chain(std::iter::once(id.manifest_namespace).filter(|ns| !ns.is_empty()));
    for ns_key in ns_keys {
        let remove_ns = if let Some(ns) = obj.get_mut(ns_key).and_then(|v| v.as_object_mut()) {
            for map_key in ["allowBuilds", "overrides", "peerDependencyRules"] {
                if let Some(inner) = ns.get_mut(map_key).and_then(|v| v.as_object_mut()) {
                    inner.shift_remove(name);
                    if inner.is_empty() {
                        ns.shift_remove(map_key);
                    }
                }
            }
            for arr_key in [
                "onlyBuiltDependencies",
                "neverBuiltDependencies",
                "trustedDependencies",
            ] {
                if let Some(arr) = ns.get_mut(arr_key).and_then(|v| v.as_array_mut()) {
                    arr.retain(|entry| match entry.as_str() {
                        Some(s) => s.rsplit_once('@').map(|(base, _)| base).unwrap_or(s) != name,
                        None => true,
                    });
                    if arr.is_empty() {
                        ns.shift_remove(arr_key);
                    }
                }
            }
            ns.is_empty()
        } else {
            false
        };
        if remove_ns {
            obj.shift_remove(ns_key);
        }
    }

    for top_key in ["overrides", "resolutions"] {
        let remove_top = if let Some(top) = obj.get_mut(top_key).and_then(|v| v.as_object_mut()) {
            top.shift_remove(name);
            top.is_empty()
        } else {
            false
        };
        if remove_top {
            obj.shift_remove(top_key);
        }
    }
}

/// Prune aube/pnpm sidecar metadata entries that reference `name`.
/// Covers pnpm.allowBuilds, pnpm.onlyBuiltDependencies,
/// pnpm.neverBuiltDependencies, pnpm.overrides, aube.* mirrors,
/// top-level overrides, yarn resolutions. Also removes the whole
/// namespace block if its last entry was the one we just dropped.
/// Safe no-op if the manifest has none of these fields.
fn prune_sidecar_entries(manifest: &mut aube_manifest::PackageJson, name: &str) {
    // Namespaced allowlists, overrides, denylists: the compat namespaces
    // plus the embedder's own (standalone aube → pnpm.* / aube.*); an
    // embedder with no namespace ("") skips it.
    let id = aube_util::embedder();
    let ns_keys = id
        .compatible_names
        .iter()
        .copied()
        .chain(std::iter::once(id.manifest_namespace).filter(|ns| !ns.is_empty()));
    for ns_key in ns_keys {
        let Some(ns) = manifest.extra.get_mut(ns_key) else {
            continue;
        };
        let Some(obj) = ns.as_object_mut() else {
            continue;
        };
        // Map-shape fields: key is package name.
        for map_key in ["allowBuilds", "overrides", "peerDependencyRules"] {
            if let Some(inner) = obj.get_mut(map_key).and_then(|v| v.as_object_mut()) {
                inner.remove(name);
                // peerDependencyRules has nested allowedVersions,
                // ignoreMissing. Only clean the outer pkg-keyed
                // entries, deeper structures are author-controlled.
                if inner.is_empty() {
                    obj.remove(map_key);
                }
            }
        }
        // Array-shape fields: whole entries match name or name@ver.
        for arr_key in [
            "onlyBuiltDependencies",
            "neverBuiltDependencies",
            "trustedDependencies",
        ] {
            if let Some(arr) = obj.get_mut(arr_key).and_then(|v| v.as_array_mut()) {
                arr.retain(|entry| match entry.as_str() {
                    Some(s) => {
                        // "pkg" stays only if it is not our name.
                        // "pkg@range" stays only if pkg is not ours.
                        let base = s.rsplit_once('@').map(|(a, _)| a).unwrap_or(s);
                        base != name
                    }
                    None => true,
                });
                if arr.is_empty() {
                    obj.remove(arr_key);
                }
            }
        }
        // Drop the whole pnpm/aube block if we emptied it completely.
        if obj.is_empty() {
            manifest.extra.remove(ns_key);
        }
    }
    // Top-level `overrides` (npm + pnpm both accept it here).
    if let Some(top) = manifest
        .extra
        .get_mut("overrides")
        .and_then(|v| v.as_object_mut())
    {
        top.remove(name);
        if top.is_empty() {
            manifest.extra.remove("overrides");
        }
    }
    // yarn `resolutions` at top level.
    if let Some(top) = manifest
        .extra
        .get_mut("resolutions")
        .and_then(|v| v.as_object_mut())
    {
        top.remove(name);
        if top.is_empty() {
            manifest.extra.remove("resolutions");
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    fn collect_section_order(raw: &str, section: &str) -> Vec<String> {
        let v: Value = serde_json::from_str(raw).unwrap();
        let obj = v.as_object().unwrap().get(section).unwrap();
        obj.as_object().unwrap().keys().cloned().collect()
    }

    /// Regression: `aube remove` previously rebuilt every dep section
    /// from `BTreeMap`, alphabetizing the surviving entries even
    /// though the user only asked to drop one name. This test exercises
    /// the in-place pruning path used by `update_manifest_json_object`
    /// to confirm the surrounding keys stay in their original order.
    #[test]
    fn remove_preserves_dep_order_in_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(
            &path,
            r#"{
  "name": "example",
  "dependencies": {
    "zod": "^3.22.0",
    "axios": "^1.6.0",
    "lodash": "^4.17.21",
    "react": "^18.2.0"
  }
}
"#,
        )
        .unwrap();

        crate::commands::update_manifest_json_object(&path, |obj| {
            let dep_sections: &[&str] = &[
                "dependencies",
                "devDependencies",
                "optionalDependencies",
                "peerDependencies",
            ];
            for section_key in dep_sections {
                let Some(section) = obj.get_mut(*section_key).and_then(|v| v.as_object_mut())
                else {
                    continue;
                };
                section.shift_remove("axios");
                if section.is_empty() {
                    obj.shift_remove(*section_key);
                }
            }
            Ok(())
        })
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            collect_section_order(&written, "dependencies"),
            ["zod", "lodash", "react"],
            "remove must keep on-disk order — got:\n{written}"
        );
    }
}
