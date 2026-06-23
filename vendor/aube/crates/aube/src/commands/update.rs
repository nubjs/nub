use super::install;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::IsTerminal;

#[derive(Debug, Clone, Args)]
pub struct UpdateArgs {
    /// Package(s) to update (all if empty)
    pub packages: Vec<String>,
    /// Update only devDependencies.
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,
    /// Pin manifest specifiers to the resolved version with no range
    /// prefix.
    ///
    /// Pair with `--latest`: when the rewritten specifier replaces the
    /// caret/tilde original, drop the prefix so the manifest carries an
    /// exact pin (`"1.2.3"`) instead of `"^1.2.3"`. Mirrors
    /// `pnpm update --save-exact`.
    #[arg(short = 'E', long, visible_alias = "save-exact")]
    pub exact: bool,
    /// Update globally installed packages.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(short = 'g', long)]
    pub global: bool,
    /// Interactive update picker.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(short = 'i', long)]
    pub interactive: bool,
    /// Update past the manifest range.
    ///
    /// Rewrites `package.json` specifiers to match the newly resolved
    /// versions (the registry's `latest` dist-tag, clamped by
    /// `minimumReleaseAge` / `resolution-mode` as usual).
    #[arg(short = 'L', long)]
    pub latest: bool,
    /// Update only production dependencies.
    #[arg(
        short = 'P',
        long,
        conflicts_with = "dev",
        visible_alias = "production"
    )]
    pub prod: bool,
    /// Update dependencies in the current workspace package.
    #[arg(short = 'w', long)]
    pub workspace: bool,
    /// Dependency traversal depth.
    ///
    /// Parsed for pnpm compatibility but ignored: aube only ever
    /// refreshes direct deps (pnpm's `--depth 0` default). Setting
    /// the flag emits a one-line warning pointing at
    /// `rm aube-lock.yaml && aube install` for the
    /// `--depth Infinity` case.
    #[arg(long)]
    pub depth: Option<String>,
    /// Add a global pnpmfile that runs before the local one.
    ///
    /// Mirrors pnpm's `--global-pnpmfile <path>`. The global hook runs
    /// first and the local hook (if any) runs second.
    #[arg(long, value_name = "PATH", conflicts_with = "ignore_pnpmfile")]
    pub global_pnpmfile: Option<std::path::PathBuf>,
    /// Skip running `.pnpmfile.mjs` / `.pnpmfile.cjs` hooks for this update.
    #[arg(long)]
    pub ignore_pnpmfile: bool,
    /// Skip lifecycle scripts.
    ///
    /// Accepted for pnpm parity — dep scripts are already gated by
    /// `allowBuilds`, so the flag is currently a no-op, but scripts
    /// that wrap `pnpm update --ignore-scripts` keep working without
    /// complaint.
    #[arg(long, hide = true)]
    pub ignore_scripts: bool,
    /// Refresh the lockfile without populating `node_modules`.
    ///
    /// Re-resolves the full graph (direct + transitive) and writes
    /// `aube-lock.yaml`, then skips the linker so `node_modules` is
    /// left untouched. Mirrors `npm update --package-lock-only`.
    #[arg(long, conflicts_with = "frozen_lockfile")]
    pub lockfile_only: bool,
    /// Skip optionalDependencies.
    #[arg(long)]
    pub no_optional: bool,
    /// Refresh the lockfile without rewriting `package.json` ranges.
    ///
    /// Pair with `--latest` to pull a newer resolved version into the
    /// lockfile while leaving the manifest's caret/tilde ranges
    /// untouched. Without `--latest` this flag is a no-op (plain
    /// `update` already doesn't touch the manifest). Mirrors
    /// `pnpm update --no-save`.
    #[arg(long)]
    pub no_save: bool,
    /// Override the local pnpmfile location.
    ///
    /// Mirrors pnpm's `--pnpmfile <path>`. Relative paths resolve
    /// against the project root; absolute paths are used as-is. Wins
    /// over `pnpmfilePath` from `pnpm-workspace.yaml`.
    #[arg(long, value_name = "PATH", conflicts_with = "ignore_pnpmfile")]
    pub pnpmfile: Option<std::path::PathBuf>,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(
    args: UpdateArgs,
    mut filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<Option<i32>> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    let _ = args.ignore_scripts; // parity no-op: dep scripts already gated by allowBuilds
    if let Some(depth) = args.depth.as_deref() {
        // pnpm's `--depth Infinity` is the only useful value; the
        // intermediate ones (`--depth 1`, `--depth 2`) have semantics
        // that even pnpm users get tripped up on. aube only refreshes
        // direct deps, so the flag is a no-op — warn once with the
        // workaround for the genuine refresh-transitives case.
        eprintln!(
            "warn: --depth {depth} is ignored; aube only refreshes direct deps. \
             For a full refresh, run `rm aube-lock.yaml && {}`.",
            aube_util::cmd("install")
        );
    }
    if args.global {
        return run_global(args).await;
    }
    if !filter.is_empty() {
        // Discussion #602: `aube update -r` is expected to bump the
        // workspace root's deps too — pnpm leaves that off by default
        // (`include-workspace-root: false`), but for `update`/`outdated`
        // omitting the root makes the recursive run feel broken when
        // the project keeps shared dev tooling at the root.
        filter.include_workspace_root = true;
        return run_filtered(args, &filter).await;
    }
    reject_unsupported_pkg_specs(&args.packages)?;
    // Parse `<pkg>@<spec>` arg syntax. Today only `@latest` is honored —
    // it's the syntactic equivalent of `--latest` scoped to that one
    // entry, which is how pnpm phrases the manifest-rewrite-past-range
    // case (`pnpm update foo@latest`). Non-`latest` specs are rejected
    // by `reject_unsupported_pkg_specs` above so they don't silently
    // get swallowed.
    let mut explicit_latest_keys: BTreeSet<String> = BTreeSet::new();
    let parsed_packages: Vec<String> = args
        .packages
        .iter()
        .map(|raw| {
            let (name, spec) = split_pkg_arg(raw);
            if spec == Some("latest") {
                explicit_latest_keys.insert(name.to_string());
            }
            name.to_string()
        })
        .collect();
    let packages = &parsed_packages[..];
    let latest = args.latest;
    let no_save = args.no_save;
    // `--latest` flag triggers manifest rewrites for every direct dep;
    // `<pkg>@latest` triggers it only for that one entry. Combine them
    // into a per-key predicate so the same code path serves both.
    let effective_latest = latest || !explicit_latest_keys.is_empty();
    let should_rewrite_key = |key: &str| -> bool { latest || explicit_latest_keys.contains(key) };
    let mut cwd = crate::dirs::project_root()?;
    // `-w/--workspace-root`: act on the workspace root manifest
    // regardless of which sub-package the user ran from. Mirrors
    // `pnpm -w update`. Falls back to the original project root when
    // there's no workspace above (single-project install) so the
    // flag stays safe in shell aliases. Must run before the project
    // lock is acquired so we lock the right directory.
    if args.workspace
        && let Some(root) = crate::dirs::find_workspace_root(&cwd)
    {
        cwd = root;
    }
    let _lock = super::take_project_lock(&cwd)?;
    let manifest_path = cwd.join("package.json");

    let mut manifest = aube_manifest::PackageJson::from_path(&manifest_path)
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;
    let UpdateSettings {
        ignored: ignored_updates,
        rewrites_specifier: rewrites_specifier_setting,
    } = resolve_update_settings(&cwd, &manifest)?;
    // Cosmetic floor-bump: outside `--latest` and `--no-save`, with
    // `updateRewritesSpecifier=true` (default), `aube update <pkg>` also
    // tracks the resolved in-range version in `package.json`. Limited to
    // `^X.Y.Z` / `~X.Y.Z` specs at the rewrite site below; other shapes
    // (`>=`, `1.x`, exact, dist-tags, git, workspace:) are preserved.
    let cosmetic_rewrite_eligible = !effective_latest && rewrites_specifier_setting && !no_save;

    // Read the lockfile from the project, or fall back to the shared
    // workspace-root one when the project doesn't have its own (the
    // common shape after a fresh `aube install` from the workspace
    // root). This is what unblocks `aube update -r <indirect>@latest`
    // — the indirect's snapshot lives in the shared lockfile, not in
    // each project's directory, and without the fallback the
    // indirect-arg validation below would reject it.
    let existing = aube_lockfile::parse_lockfile(&cwd, &manifest)
        .ok()
        .or_else(|| {
            super::find_workspace_root(&cwd)
                .ok()
                .filter(|ws| ws.as_path() != cwd.as_path())
                .and_then(|ws| aube_lockfile::parse_lockfile(&ws, &manifest).ok())
        });
    // Importer keys to try when looking up a direct dep in `existing`.
    // Order matters: try the cwd's own importer first, then fall through
    // to `"."`.
    //
    // In shared-workspace mode, `existing` came from the workspace-root
    // lockfile and carries BOTH the root's `"."` importer (the root
    // manifest's own deps) and the sub-package's importer key
    // (`./packages/foo` etc.) — distinct entries that may share dep
    // names. Trying `"."` first would cross-resolve any shared name
    // (e.g. the root and the sub-package both depend on `typescript`)
    // to the ROOT's locked version, which is wrong for the sub-package
    // pass and would feed the wrong "current" into the picker, the
    // post-resolve report, and the `preserve_pin` guard.
    //
    // For the standalone-project and workspace-root cases, `cwd_importer`
    // resolves to `"."` so the second slot is dropped — the lookup is
    // single-importer and unambiguous.
    //
    // For the sub-package-with-its-own-lockfile case (no shared
    // lockfile), `existing` only has `"."` — `cwd_importer` simply
    // misses and the fall-through to `"."` returns the right entry.
    //
    // `lookup_pkg` walks the list in order and returns `None` when no
    // candidate importer carries a DirectDep for the key — there is no
    // name-scan fallback (see the function's doc comment).
    let cwd_importer = match crate::dirs::find_workspace_root(&cwd) {
        Some(ws) if ws.as_path() != cwd.as_path() => {
            super::workspace_importer_path(&ws, &cwd).unwrap_or_else(|_| ".".to_string())
        }
        _ => ".".to_string(),
    };
    let existing_importers: Vec<&str> = if cwd_importer == "." {
        vec!["."]
    } else {
        vec![cwd_importer.as_str(), "."]
    };

    // Snapshot of every direct dep as (manifest key, specifier). Owned
    // strings so we can hold this across mutations of `manifest`.
    let include_prod = !args.dev;
    let include_dev = !args.prod;
    let include_optional = !args.no_optional && !args.dev;
    let all_specifiers: BTreeMap<String, String> = manifest
        .dependencies
        .iter()
        .filter(|_| include_prod)
        .chain(manifest.dev_dependencies.iter().filter(|_| include_dev))
        .chain(
            manifest
                .optional_dependencies
                .iter()
                .filter(|_| include_optional),
        )
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // Set of every direct manifest key, ignoring `--prod`/`--dev`/
    // `--no-optional` filters. Used below to distinguish "flag-excluded
    // direct dep" from "truly indirect dep" — without this, a devDep
    // named under `--prod` would miss `all_specifiers`, fall through to
    // the indirect-dep branch, pass `in_graph` (the lockfile carries
    // every dep regardless of bucket), and get silently re-resolved.
    let all_direct_keys: BTreeSet<&str> = manifest
        .dependencies
        .keys()
        .chain(manifest.dev_dependencies.keys())
        .chain(manifest.optional_dependencies.keys())
        .map(String::as_str)
        .collect();

    let resolve_real_name =
        |manifest_key: &str| real_name_from_spec(manifest_key, all_specifiers.get(manifest_key));

    // Determine which packages to update.
    //
    // Args split into direct (in package.json) vs indirect (only in the
    // lockfile). pnpm allows `pnpm update <indirect-pkg>` to refresh a
    // transitive dep without touching package.json — match that. Direct
    // args land in `manifest_keys_to_update` and drive the rewrite path;
    // indirect args go into `indirect_arg_names` and are filtered from
    // the locked snapshot below so the resolver picks them fresh.
    let update_all = packages.is_empty();
    let mut indirect_arg_names: BTreeSet<String> = BTreeSet::new();
    if !update_all {
        for name in packages {
            if all_specifiers.contains_key(name.as_str()) {
                if ignored_updates.contains(name.as_str()) {
                    return Err(miette!(
                        "package '{name}' is ignored by updateConfig.ignoreDependencies"
                    ));
                }
                continue;
            }
            // Direct dep excluded by `--prod`/`--dev`/`--no-optional`.
            // Match the pre-indirect-support behavior so the flag
            // mismatch is visible — otherwise the dep would slip into
            // the indirect path below and silently re-resolve.
            if all_direct_keys.contains(name.as_str()) {
                return Err(miette!("package '{name}' is not a dependency"));
            }
            // Indirect dep: must exist in the lockfile graph (either as
            // its own name or as the real-name of an aliased entry).
            let in_graph = existing.as_ref().is_some_and(|g| {
                g.packages
                    .values()
                    .any(|p| p.name == *name || p.alias_of.as_deref() == Some(name.as_str()))
            });
            if !in_graph {
                return Err(miette!("package '{name}' is not a dependency"));
            }
            if ignored_updates.contains(name.as_str()) {
                return Err(miette!(
                    "package '{name}' is ignored by updateConfig.ignoreDependencies"
                ));
            }
            indirect_arg_names.insert(name.clone());
        }
    }
    let mut manifest_keys_to_update: Vec<String> = if update_all {
        all_specifiers
            .keys()
            .filter(|name| !ignored_updates.contains(name.as_str()))
            .cloned()
            .collect()
    } else {
        packages
            .iter()
            .filter(|p| all_specifiers.contains_key(p.as_str()))
            .filter(|p| {
                if ignored_updates.contains(p.as_str()) {
                    tracing::info!("skipping {p} (updateConfig.ignoreDependencies)");
                    false
                } else {
                    true
                }
            })
            .cloned()
            .collect()
    };
    // `--latest`: pre-compute the set of direct deps that should NOT be
    // rewritten because the user is already on a version newer than the
    // registry's `latest` dist-tag. Mirrors pnpm's regression guard from
    // #7436 — a user pinned at `"3.0.0-rc.0"` while latest=`2.0.0`
    // shouldn't be silently downgraded by `update --latest`.
    //
    // Two version sources feed this check:
    //   - The manifest spec (`exact_pin_version`) — picks up exact
    //     prerelease pins even when no lockfile exists yet.
    //   - The LOCKED version from `existing` — picks up caret/tilde
    //     prerelease ranges (e.g. `"^3.0.0-rc.1"`) and dist-tag
    //     specifiers (`"next"`) that resolved to a version above the
    //     registry's current `latest`. Discussion #623: caret-prefix
    //     prereleases were getting downgraded because `exact_pin_version`
    //     only recognized bare exact pins.
    //
    // Only applied to bulk updates (no positional args). When the user
    // explicitly names a package — `aube update --latest <pkg>` or
    // `aube update <pkg>@latest` — they're opting in to whatever the
    // registry's `latest` says, even when that downgrades a prerelease
    // (matching pnpm's behavior for `pnpm update <pkg>@latest`).
    //
    // Computed BEFORE the interactive picker so preserve-pin entries are
    // hidden from the picker — otherwise the picker would surface a
    // phantom downgrade row that the post-picker rewrite path then
    // ignores.
    let preserve_pin: BTreeSet<String> = if latest && update_all {
        let client = std::sync::Arc::new(super::make_client(&cwd));
        let mut handles = Vec::new();
        for key in &manifest_keys_to_update {
            let original = all_specifiers.get(key).map(String::as_str).unwrap_or("");
            if aube_util::pkg::is_workspace_spec(original) {
                continue;
            }
            let real_name = resolve_real_name(key);
            // Candidate "current" versions to compare against the registry
            // `latest`: the manifest pin (if exact) and the locked version
            // (if any). Either tipping above latest is enough to preserve.
            let manifest_pin =
                exact_pin_version(original).and_then(|p| node_semver::Version::parse(p).ok());
            let locked_pin = existing
                .as_ref()
                .and_then(|g| lookup_pkg(g, &existing_importers, key, &real_name))
                .and_then(|p| node_semver::Version::parse(&p.version).ok());
            if manifest_pin.is_none() && locked_pin.is_none() {
                continue;
            }
            let key_owned = key.clone();
            let client = client.clone();
            handles.push(tokio::spawn(async move {
                // A fetch failure here would silently fall through to the
                // rewrite path and downgrade the prerelease pin — exactly
                // what this guard is supposed to prevent. Surface the
                // underlying error via tracing so the user can spot a
                // transient registry failure that broke the guard, then
                // continue with the resolver path (which has its own
                // retry/cache semantics and may still succeed).
                let packument = match client.fetch_packument(&real_name).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            code = aube_codes::warnings::WARN_AUBE_PRERELEASE_CHECK_SKIPPED,
                            "skipping prerelease-preservation check for {real_name}: {e}"
                        );
                        return None;
                    }
                };
                let latest_v = packument.dist_tags.get("latest")?;
                let Ok(parsed_latest) = node_semver::Version::parse(latest_v) else {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_PRERELEASE_CHECK_SKIPPED,
                        "skipping prerelease-preservation check for {real_name}: \
                         registry returned non-semver latest dist-tag {latest_v:?}"
                    );
                    return None;
                };
                let above_latest = manifest_pin.as_ref().is_some_and(|v| v > &parsed_latest)
                    || locked_pin.as_ref().is_some_and(|v| v > &parsed_latest);
                above_latest.then_some(key_owned)
            }));
        }
        let mut set = BTreeSet::new();
        for h in handles {
            if let Ok(Some(key)) = h.await {
                set.insert(key);
            }
        }
        set
    } else {
        BTreeSet::new()
    };

    if args.interactive && !manifest_keys_to_update.is_empty() {
        let selected = match pick_update_interactively(
            &manifest_keys_to_update,
            &manifest,
            &all_specifiers,
            existing.as_ref(),
            &existing_importers,
            &preserve_pin,
            &cwd,
            latest,
        )
        .await?
        {
            Some(sel) => sel,
            // Picker cancelled (Ctrl-C / Esc): exit 130 via the return path.
            None => return Ok(Some(130)),
        };
        if selected.is_empty() && indirect_arg_names.is_empty() {
            eprintln!("No packages selected.");
            return Ok(None);
        }
        manifest_keys_to_update.retain(|key| selected.contains(key));
    }

    let real_names_to_update: std::collections::HashSet<String> = manifest_keys_to_update
        .iter()
        .map(|k| resolve_real_name(k))
        .collect();

    if update_all {
        eprintln!("Updating all dependencies...");
    } else {
        eprintln!("Updating: {}", parsed_packages.join(", "));
    }

    // Rewrite each targeted direct-dep specifier on a *clone* of the
    // manifest handed to the resolver. Mutating the real in-memory
    // manifest would corrupt `package.json` if any package fails to
    // resolve — the literal `"latest"` string would stick.
    // `workspace:` specs are skipped: they refer to local workspace
    // packages, not registry versions. `preserve_pin` entries (a
    // manifest pin newer than the dist-tag latest) are also left
    // untouched so the resolver naturally re-resolves the original
    // version into the graph and the manifest rewrite path leaves the
    // pin alone. Both `--latest` (every direct dep) and `<pkg>@latest`
    // (only the named entries — see `should_rewrite_key`) flow
    // through this loop.
    let resolver_manifest = if effective_latest {
        let mut m = manifest.clone();
        for key in &manifest_keys_to_update {
            if !should_rewrite_key(key) {
                continue;
            }
            let real_name = resolve_real_name(key);
            let original = all_specifiers.get(key).map(String::as_str).unwrap_or("");
            if aube_util::pkg::is_workspace_spec(original)
                || aube_util::pkg::is_catalog_spec(original)
                || preserve_pin.contains(key)
            {
                continue;
            }
            if aube_lockfile::parse_git_spec(original).is_some() {
                // Git specs carry their own committish; rewriting to
                // `latest` would route the resolver at the registry
                // instead of the git source. Symmetric with the guard in
                // the package.json rewrite loop below.
                continue;
            }
            let new_spec = if original.starts_with("npm:") {
                format!("npm:{real_name}@latest")
            } else {
                "latest".to_string()
            };
            if m.dependencies.contains_key(key) {
                m.dependencies.insert(key.clone(), new_spec);
            } else if m.dev_dependencies.contains_key(key) {
                m.dev_dependencies.insert(key.clone(), new_spec);
            } else if m.optional_dependencies.contains_key(key) {
                m.optional_dependencies.insert(key.clone(), new_spec);
            }
        }
        m
    } else {
        manifest.clone()
    };

    // Build a filtered lockfile that excludes packages being updated
    // so the resolver picks the latest matching version instead of the
    // locked one. Three sets need to come out:
    //   - `real_names_to_update` — direct deps by their real name.
    //   - `manifest_keys_to_update` — direct alias entries
    //     (`"alias": "npm:real@x"`) live in the graph with
    //     `pkg.name == "alias"`, not the real name; without this check
    //     the resolver would keep the locked alias version under
    //     `--latest`.
    //   - `indirect_arg_names` — transitive deps the user named on the
    //     command line. We filter both `pkg.name` matches AND
    //     `pkg.alias_of` matches so an indirect aliased entry
    //     (`other-alias: npm:dep-of-pkg-with-1-dep@x`) is also dropped.
    let filtered_existing = existing.as_ref().map(|graph| {
        let mut filtered = graph.clone();
        let manifest_keys: std::collections::HashSet<&str> =
            manifest_keys_to_update.iter().map(String::as_str).collect();
        let indirect_set: std::collections::HashSet<&str> =
            indirect_arg_names.iter().map(String::as_str).collect();
        filtered.packages.retain(|_, pkg| {
            !real_names_to_update.contains(&pkg.name)
                && !manifest_keys.contains(pkg.name.as_str())
                && !indirect_set.contains(pkg.name.as_str())
                && !pkg
                    .alias_of
                    .as_deref()
                    .is_some_and(|a| indirect_set.contains(a))
        });
        // Indirect-arg dist-tag forwarding. When the user passes
        // `<indirect>@latest`, dropping the indirect's own snapshot
        // entry isn't enough on its own — the resolver's lockfile-reuse
        // path (aube_resolver::resolve.rs:1164) iterates each parent's
        // locked `dependencies` map and enqueues transitive tasks using
        // the *locked version* as the range. So a parent locked at
        // `pkg-with-1-dep@100.0.0` with `dependencies: { dep-of: 100.0.0 }`
        // still re-resolves dep-of at exactly 100.0.0, even after we
        // dropped dep-of from `packages`. Rewriting the edge to `latest`
        // rebroadcasts it as a dist-tag spec, so the transitive task
        // resolves through the registry packument and picks the new
        // latest. Only applied for entries the user named with
        // `@latest`; bare `update <indirect>` keeps the locked edge so
        // we don't silently bump something the user didn't ask to bump.
        if !explicit_latest_keys.is_empty() {
            for parent_pkg in filtered.packages.values_mut() {
                for indirect_name in &indirect_arg_names {
                    if !explicit_latest_keys.contains(indirect_name) {
                        continue;
                    }
                    if parent_pkg.dependencies.contains_key(indirect_name.as_str()) {
                        parent_pkg
                            .dependencies
                            .insert(indirect_name.clone(), "latest".to_string());
                    }
                }
            }
        }
        filtered
    });

    // Re-resolve the full dependency tree. Wire the pnpmfile in so
    // `readPackage` mutations apply during update (not just first
    // install) and `afterAllResolved` gets a chance to rewrite the
    // graph before we hand it to the lockfile writer; without this,
    // `aube install` runs in frozen-prefer mode below and never
    // re-evaluates the hook.
    let pnpmfile_paths = if args.ignore_pnpmfile {
        Vec::new()
    } else {
        let (ws, _) = aube_manifest::workspace::load_both(&cwd).unwrap_or_default();
        crate::pnpmfile::ordered_paths(
            crate::pnpmfile::detect_global(&cwd, args.global_pnpmfile.as_deref()).as_deref(),
            crate::pnpmfile::detect(&cwd, args.pnpmfile.as_deref(), ws.pnpmfile_path.as_deref())
                .as_deref(),
        )
    };
    super::run_pnpmfile_pre_resolution(&pnpmfile_paths, &cwd, existing.as_ref()).await?;
    let (read_package_host, read_package_forwarders) =
        match crate::pnpmfile::ReadPackageHostChain::spawn(&pnpmfile_paths, &cwd)
            .await
            .wrap_err("failed to start pnpmfile readPackage host")?
        {
            Some((h, f)) => (Some(h), f),
            None => (None, Vec::new()),
        };
    let workspace_catalogs = super::load_workspace_catalogs(&cwd)?;
    let workspace_package_versions = workspace_package_versions(&cwd)?;
    let mut resolver = super::build_resolver(&cwd, &manifest, workspace_catalogs)?;
    if let Some(host) = read_package_host {
        resolver = resolver
            .with_read_package_hook(Box::new(host) as Box<dyn aube_resolver::ReadPackageHook>);
    }
    let resolver_manifests = [(".".to_string(), resolver_manifest)];
    let mut graph = resolver
        .resolve_workspace(
            &resolver_manifests,
            filtered_existing.as_ref(),
            &workspace_package_versions,
        )
        .await
        .map_err(miette::Report::new)
        .wrap_err("failed to resolve dependencies")?;
    drop(resolver);
    // Drain the readPackage stderr forwarders so resolve-time `ctx.log`
    // records flush to stdout before afterAllResolved emits its own.
    crate::pnpmfile::ReadPackageHostChain::drain_forwarders(read_package_forwarders).await;
    crate::pnpmfile::run_after_all_resolved_chain(&pnpmfile_paths, &cwd, &mut graph).await?;

    // Report what changed. Aliased direct deps (`"alias": "npm:real@x"`)
    // land in the lockfile graph with `pkg.name == "alias"` and
    // `pkg.alias_of == Some("real")`, so the version-lookup match has to
    // accept either the manifest key (the alias) or the real name —
    // matching only on `real_name` would miss aliased entries.
    // The freshly resolved `graph` was built from `resolver_manifests = [(".", ...)]`,
    // so its only importer key is "." regardless of the cwd's path under
    // any workspace root.
    let new_importers: Vec<&str> = vec!["."];
    for manifest_key in &manifest_keys_to_update {
        let real_name = resolve_real_name(manifest_key);

        let old_ver = existing
            .as_ref()
            .and_then(|g| lookup_pkg(g, &existing_importers, manifest_key, &real_name))
            .map(|p| p.version.as_str());
        let new_ver = lookup_pkg(&graph, &new_importers, manifest_key, &real_name)
            .map(|p| p.version.as_str());

        match (old_ver, new_ver) {
            (Some(old), Some(new)) if old != new => {
                eprintln!("  {manifest_key}: {old} -> {new}");
            }
            (Some(ver), Some(_)) => {
                eprintln!("  {manifest_key}: {ver} (already latest)");
            }
            (None, Some(new)) => {
                eprintln!("  {manifest_key}: (new) {new}");
            }
            (Some(old), None) => {
                eprintln!("  {manifest_key}: {old} -> (removed from graph)");
            }
            (None, None) => {}
        }
    }

    eprintln!("Resolved {} packages", graph.packages.len());

    // Rewrite each targeted direct dep in the real `package.json` to
    // pin the resolved version, preserving the user's existing prefix
    // (`^`/`~`/exact) and any `npm:` alias. `--latest` covers every
    // direct dep; `<pkg>@latest` covers only the named entry — both
    // flow through `should_rewrite_key`. Skip `workspace:` specs
    // (sibling packages) and skip deps that resolved to the same spec
    // they already had, so an idempotent rewrite doesn't churn the
    // manifest for no reason.
    //
    // `--no-save` short-circuits the manifest rewrite: the resolver
    // already pulled in the new versions for the lockfile above, so we
    // just skip persisting any range bumps to `package.json`.
    if no_save && (effective_latest || rewrites_specifier_setting) {
        eprintln!("Skipping package.json update (--no-save)");
    } else if effective_latest || cosmetic_rewrite_eligible {
        let mut wrote_any = false;
        for key in &manifest_keys_to_update {
            if effective_latest && !should_rewrite_key(key) {
                continue;
            }
            let real_name = resolve_real_name(key);
            let original = all_specifiers.get(key).cloned().unwrap_or_default();
            if aube_util::pkg::is_workspace_spec(&original)
                || aube_util::pkg::is_catalog_spec(&original)
            {
                continue;
            }
            if aube_lockfile::parse_git_spec(&original).is_some() {
                // Git specs (`github:user/repo`, `git+https://…`, bare
                // `user/repo` shorthand, …) carry their own committish
                // and have no semver range to bump. Rewriting one to
                // `^<resolved>` would silently swap the dep for a
                // registry pin and break install.
                continue;
            }
            // Cosmetic floor-bump (no `--latest`) only rewrites specs
            // that literally begin with `^` or `~` — dist-tags
            // (`"latest"`, `"next"`, …), exact pins, raw ranges
            // (`>=1.0`), and anything else are preserved as-is.
            // `range_prefix` defaults to `"^"` for unknown shapes so it
            // can't be the discriminator here. Caret/tilde under an
            // `npm:` alias lives on the post-`@` portion.
            if !effective_latest {
                let range_slice = original
                    .strip_prefix("npm:")
                    .and_then(|rest| rest.rsplit_once('@').map(|(_, r)| r))
                    .unwrap_or(original.as_str());
                if !range_slice.starts_with('^') && !range_slice.starts_with('~') {
                    continue;
                }
            }
            let Some(resolved) =
                lookup_pkg(&graph, &new_importers, key, &real_name).map(|p| p.version.clone())
            else {
                continue;
            };
            let new_spec = rewrite_specifier(&original, &real_name, &resolved, args.exact);
            if new_spec == original {
                continue;
            }
            if manifest.dependencies.contains_key(key) {
                manifest.dependencies.insert(key.clone(), new_spec);
            } else if manifest.dev_dependencies.contains_key(key) {
                manifest.dev_dependencies.insert(key.clone(), new_spec);
            } else if manifest.optional_dependencies.contains_key(key) {
                manifest.optional_dependencies.insert(key.clone(), new_spec);
            } else {
                continue;
            }
            wrote_any = true;
        }
        if wrote_any {
            super::write_manifest_dep_sections(&manifest_path, &manifest)?;
            eprintln!("Updated package.json");
        }
    }

    install::finalize_lockfile_graph(
        &cwd,
        &mut graph,
        &manifest,
        args.ignore_pnpmfile,
        args.pnpmfile.as_deref(),
    )
    .await?;
    write_update_lockfile(
        &cwd,
        &graph,
        &manifest,
        args.ignore_pnpmfile,
        absolute_cli_pnpmfile(&cwd, args.pnpmfile.as_deref()).as_deref(),
    )
    .await?;

    // Propagate `--ignore-pnpmfile` / `--pnpmfile` / `--global-pnpmfile`
    // into the chained install. Frozen-prefer normally short-circuits to
    // a no-op fetch/link, but if the lockfile we just wrote falls out of
    // sync (drift, manual edits, future chained calls) the install would
    // re-resolve and re-attach the pnpmfile hook — silently overriding
    // the flags the user passed to `aube update`.
    let mut chained =
        install::InstallOptions::with_mode(super::chained_frozen_mode(install::FrozenMode::Prefer));
    chained.ignore_pnpmfile = args.ignore_pnpmfile;
    chained.pnpmfile = args.pnpmfile.clone();
    chained.global_pnpmfile = args.global_pnpmfile.clone();
    // `aube update` is one of the canonical fresh-resolution
    // entry points — by design it pulls newer versions than the
    // lockfile pins. Route the post-resolve transitive set
    // through the live OSV API so the freshest `MAL-*` advisories
    // apply at the moment a human is changing what's installed,
    // matching `aube add`'s behavior.
    chained.osv_transitive_check = true;
    // `--lockfile-only`: lockfile is already written above; tell the
    // chained install to skip linking `node_modules` so the on-disk
    // tree stays as-is. Mirrors `aube install --lockfile-only` and
    // closes the gap with `npm update --package-lock-only`.
    chained.lockfile_only = args.lockfile_only;
    install::run(chained).await?;

    Ok(None)
}

async fn run_global(args: UpdateArgs) -> miette::Result<Option<i32>> {
    reject_unsupported_pkg_specs(&args.packages)?;

    let layout = super::global::GlobalLayout::resolve()?;
    let packages = super::global::scan_packages(&layout.pkg_dir);
    if packages.is_empty() {
        return Err(miette!("no global packages installed"));
    }

    let selected = select_global_updates(&args.packages, packages)?;
    let original_cwd = crate::dirs::cwd()?;
    let result = async {
        for (info, package_args) in selected {
            let old_bins = super::global::bin_names_for(&info.install_dir, &info.aliases);
            super::retarget_cwd(&info.install_dir)?;

            let mut inner = args.clone();
            inner.global = false;
            inner.packages = package_args;
            inner.depth = None;
            inner.latest = true;
            inner.exact = true;
            inner.no_save = false;
            inner.workspace = false;

            Box::pin(run(
                inner,
                aube_workspace::selector::EffectiveFilter::default(),
            ))
            .await?;

            let shim_opts = crate::commands::with_settings_ctx(&info.install_dir, |ctx| {
                aube_linker::BinShimOptions {
                    extend_node_path: aube_settings::resolved::extend_node_path(ctx),
                    prefer_symlinked_executables:
                        aube_settings::resolved::prefer_symlinked_executables(ctx),
                    hidden_modules_dir: None,
                }
            });
            let linked = super::global::link_bins(
                &info.install_dir,
                &layout.bin_dir,
                &info.aliases,
                shim_opts,
            )?;
            let linked_set: std::collections::BTreeSet<&str> =
                linked.iter().map(String::as_str).collect();
            let stale_bins: Vec<String> = old_bins
                .into_iter()
                .filter(|name| !linked_set.contains(name.as_str()))
                .collect();
            super::global::unlink_bins(&info.install_dir, &layout.bin_dir, &stale_bins);
            if !linked.is_empty() {
                eprintln!(
                    "Linked {} into {}",
                    pluralizer::pluralize("bin", linked.len() as isize, true),
                    layout.bin_dir.display()
                );
            }
        }
        Ok(())
    }
    .await;
    super::finish_filtered_workspace(&original_cwd, result)?;
    Ok(None)
}

fn select_global_updates(
    requested: &[String],
    packages: Vec<super::global::GlobalPackageInfo>,
) -> miette::Result<Vec<(super::global::GlobalPackageInfo, Vec<String>)>> {
    if requested.is_empty() {
        return Ok(packages
            .into_iter()
            .map(|info| (info, Vec::new()))
            .collect());
    }

    let mut by_hash: BTreeMap<String, (super::global::GlobalPackageInfo, Vec<String>)> =
        BTreeMap::new();
    let mut missing = Vec::new();
    for raw in requested {
        let (name, _) = split_pkg_arg(raw);
        let Some(info) = packages
            .iter()
            .find(|info| info.aliases.iter().any(|alias| alias == name))
            .cloned()
        else {
            missing.push(name.to_string());
            continue;
        };
        by_hash
            .entry(info.hash.clone())
            .or_insert_with(|| (info, Vec::new()))
            .1
            .push(raw.clone());
    }

    if !missing.is_empty() {
        let joined = missing.join(", ");
        return Err(miette!("not globally installed: {joined}"));
    }
    if by_hash.is_empty() {
        return Err(miette!("no matching global packages were updated"));
    }
    Ok(by_hash.into_values().collect())
}

fn workspace_package_versions(cwd: &std::path::Path) -> miette::Result<HashMap<String, String>> {
    let workspace_root = crate::dirs::find_workspace_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let workspace_packages = aube_workspace::find_workspace_packages(&workspace_root)
        .into_diagnostic()
        .wrap_err("failed to discover workspace packages")?;
    let mut versions = HashMap::new();
    // Include the root package itself as a workspace target so
    // sub-packages can use `workspace:*` to depend on it.
    match aube_manifest::PackageJson::from_path(&workspace_root.join("package.json")) {
        Ok(root) => {
            if let Some(name) = root.name {
                let version = root.version.unwrap_or_else(|| "0.0.0".to_string());
                versions.insert(name, version);
            }
        }
        // Yaml-only workspaces may not have a root package.json;
        // Io errors are expected and silently skipped.
        Err(aube_manifest::Error::Io(..)) => {}
        Err(e) => tracing::warn!(
            "root package.json parse error: {e}; workspace name/version registration skipped"
        ),
    }
    for pkg_dir in workspace_packages {
        let pkg_manifest = aube_manifest::PackageJson::from_path(&pkg_dir.join("package.json"))
            .map_err(miette::Report::new)
            .wrap_err_with(|| format!("failed to read {}/package.json", pkg_dir.display()))?;
        if let Some(name) = pkg_manifest.name {
            let version = pkg_manifest.version.unwrap_or_else(|| "0.0.0".to_string());
            versions.insert(name, version);
        } else {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_WORKSPACE_PACKAGE_MISSING_NAME,
                "workspace package at {} has no 'name' field; skipping workspace version registration",
                pkg_dir.display()
            );
        }
    }
    Ok(versions)
}

#[allow(clippy::too_many_arguments)]
/// Interactively pick which dependencies to update. `Ok(Some(set))` is the
/// selection (possibly empty); `Ok(None)` means the user cancelled the picker
/// (Ctrl-C / Esc), which the caller maps to exit code 130 — returned up to the
/// binary's single `std::process::exit` rather than terminating here, keeping
/// the command embed-safe.
async fn pick_update_interactively(
    keys: &[String],
    manifest: &aube_manifest::PackageJson,
    specifiers: &BTreeMap<String, String>,
    existing: Option<&aube_lockfile::LockfileGraph>,
    existing_importers: &[&str],
    preserve_pin: &BTreeSet<String>,
    cwd: &std::path::Path,
    latest: bool,
) -> miette::Result<Option<BTreeSet<String>>> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(miette!(
            "`{} --interactive` requires stdin and stderr to be TTYs; pass package names explicitly to update non-interactively",
            aube_util::cmd("update")
        ));
    }

    // Discussion #602: filter the picker to deps that have actual
    // drift, drop sibling-workspace and local-link entries (no
    // registry version to bump them to), and show `current → target`
    // so the user can tell at a glance what each toggle would change.
    // Workspace-protocol specs land in the manifest as `workspace:*`
    // / `workspace:^` / `workspace:~`, and `link:` / `file:` deps
    // carry a path; none of them are registry-resolvable. The
    // pre-fetch is the same packument round-trip `aube outdated`
    // makes, just gated on the `-i` path.
    //
    // Discussion #623: also drop `preserve_pin` entries — direct deps
    // whose locked version sits above the registry's `latest` dist-tag.
    // The post-picker rewrite path skips these so showing them as
    // toggleable in the picker would be a lie.
    let registry_keys: Vec<&String> = keys
        .iter()
        .filter(|key| {
            let spec = specifiers.get(*key).map(String::as_str).unwrap_or("");
            !aube_util::pkg::is_workspace_spec(spec)
                && !spec.starts_with("link:")
                && !spec.starts_with("file:")
                && !preserve_pin.contains(key.as_str())
        })
        .collect();
    if registry_keys.is_empty() {
        return Ok(Some(BTreeSet::new()));
    }

    let client = std::sync::Arc::new(super::make_client(cwd));
    let cache_dir = super::packument_cache_dir();
    let mut set = tokio::task::JoinSet::new();
    for key in &registry_keys {
        let real_name = real_name_from_spec(key, specifiers.get(key.as_str()));
        let key_owned = (*key).clone();
        let client = client.clone();
        let cache_dir = cache_dir.clone();
        set.spawn(async move {
            let result = client.fetch_packument_cached(&real_name, &cache_dir).await;
            (key_owned, result)
        });
    }
    let mut packuments: HashMap<String, aube_registry::Packument> =
        HashMap::with_capacity(registry_keys.len());
    while let Some(joined) = set.join_next().await {
        let (key, result) = joined
            .into_diagnostic()
            .wrap_err("packument fetch panicked")?;
        match result {
            Ok(p) => {
                packuments.insert(key, p);
            }
            Err(e) => {
                tracing::warn!("failed to fetch packument for {key}: {e}");
            }
        }
    }

    let mut picker = demand::MultiSelect::new("Choose which dependencies to update")
        .description("Space to toggle, Enter to confirm")
        .filterable(true);
    let mut shown = 0usize;
    for key in &registry_keys {
        let spec = specifiers
            .get(key.as_str())
            .map(String::as_str)
            .unwrap_or("");
        let real_name = real_name_from_spec(key, specifiers.get(key.as_str()));
        let Some(packument) = packuments.get(key.as_str()) else {
            continue;
        };
        let current = existing
            .and_then(|g| lookup_pkg(g, existing_importers, key, &real_name))
            .map(|p| p.version.as_str());
        let registry_latest = packument.dist_tags.get("latest").map(String::as_str);
        let wanted =
            super::max_satisfying_version(packument, spec).or_else(|| current.map(str::to_owned));
        // `--latest` rewrites past the manifest range, so the picker
        // shows the dist-tag latest as the target. Without `--latest`
        // we only refresh inside the range, so target = wanted.
        let target = if latest {
            registry_latest
                .map(str::to_owned)
                .or_else(|| wanted.clone())
        } else {
            wanted.clone()
        };
        let (Some(current), Some(target)) = (current, target.as_deref()) else {
            continue;
        };
        if current == target {
            continue;
        }
        let label = format!("{} {key} {current} → {target}", dep_bucket(manifest, key),);
        picker = picker.option(
            demand::DemandOption::new((*key).clone())
                .label(&label)
                .selected(true),
        );
        shown += 1;
    }
    if shown == 0 {
        return Ok(Some(BTreeSet::new()));
    }

    let picked: Vec<String> = match picker.run() {
        Ok(picked) => picked,
        // Cancelled (Ctrl-C / Esc): signal to the caller, which returns exit
        // code 130 via the return path rather than hard-exiting in place,
        // keeping the command embed-safe.
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(None),
        Err(e) => {
            return Err(e)
                .into_diagnostic()
                .wrap_err("failed to read update selection");
        }
    };
    Ok(Some(picked.into_iter().collect()))
}

fn dep_bucket(manifest: &aube_manifest::PackageJson, key: &str) -> &'static str {
    if manifest.dependencies.contains_key(key) {
        "dependencies"
    } else if manifest.dev_dependencies.contains_key(key) {
        "devDependencies"
    } else {
        "optionalDependencies"
    }
}

fn real_name_from_spec(manifest_key: &str, specifier: Option<&String>) -> String {
    if let Some(specifier) = specifier
        && let Some(rest) = specifier.strip_prefix("npm:")
    {
        // "npm:real-pkg@^2.0.0" -> "real-pkg"
        if let Some(at_idx) = rest.rfind('@') {
            return rest[..at_idx].to_string();
        }
        return rest.to_string();
    }
    manifest_key.to_string()
}

/// Look up the LockedPackage for a direct dep of the current importer.
///
/// Walks `importer_paths` in order and returns the first match found via
/// the importer's `DirectDep` edge — using `dep_path` for an exact lookup
/// rather than name-matching across `g.packages`. This matters when the
/// same package name appears multiple times in the lockfile (e.g. a
/// `catalog:` dep at one version plus the same name pulled in transitively
/// at an older version): a name-only `g.packages.values().find(...)` walks
/// the BTreeMap in `dep_path` lex order and returns whichever version
/// happens to come first, mis-reporting the user's "current" version and
/// driving phantom upgrade offers in `--interactive`. Discussion #623.
///
/// `importer_paths` should list the candidate importer keys to try — `"."`
/// first (standalone or workspace-root cwd), then the cwd's path relative
/// to the workspace root (sub-package under a shared lockfile).
///
/// Returns `None` when no candidate importer carries a `DirectDep` for
/// the requested key. Deliberately does NOT fall back to a name scan
/// across `g.packages`: in a `update -r` fanout the lockfile read between
/// per-project iterations contains the previously-processed project's
/// importer entries but not yet the current one, and a name scan would
/// return some other importer's snapshot of the same name and trip the
/// `preserve_pin` guard with the wrong "current" version. All supported
/// lockfile formats (`aube`, `pnpm`, `npm`, `yarn`, `bun`) populate
/// `importers` from their per-importer direct-dep records, so an
/// importer-only lookup is sufficient.
fn lookup_pkg<'a>(
    g: &'a aube_lockfile::LockfileGraph,
    importer_paths: &[&str],
    manifest_key: &str,
    real_name: &str,
) -> Option<&'a aube_lockfile::LockedPackage> {
    for importer in importer_paths {
        let Some(deps) = g.importers.get(*importer) else {
            continue;
        };
        if let Some(dep) = deps
            .iter()
            .find(|d| d.name == manifest_key || d.name == real_name)
            && let Some(pkg) = g.get_package(&dep.dep_path)
        {
            return Some(pkg);
        }
    }
    None
}

struct UpdateSettings {
    ignored: BTreeSet<String>,
    rewrites_specifier: bool,
}

fn resolve_update_settings(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
) -> miette::Result<UpdateSettings> {
    let mut ignored: BTreeSet<String> = manifest.update_ignore_dependencies().into_iter().collect();
    let rewrites_specifier = with_update_settings_ctx(cwd, |ctx| {
        if let Some(from_settings) = aube_settings::resolved::update_config_ignore_dependencies(ctx)
        {
            ignored.extend(from_settings);
        }
        aube_settings::resolved::update_rewrites_specifier(ctx)
    })?;
    Ok(UpdateSettings {
        ignored,
        rewrites_specifier,
    })
}

fn with_update_settings_ctx<T>(
    cwd: &std::path::Path,
    f: impl FnOnce(&aube_settings::ResolveCtx<'_>) -> T,
) -> miette::Result<T> {
    let files = crate::commands::FileSources::load(cwd);
    // `pnpm-workspace.yaml` lives at the workspace root, not in
    // sub-packages. Filtered runs (`update -r`) call us with the
    // sub-package as cwd, so an unwalked load returns an empty map and
    // `updateConfig.ignoreDependencies` silently drops every entry.
    // Discussion #602: zod was in the ignore list yet appeared in the
    // recursive picker because of this miss. Fall back to cwd if no
    // workspace root is found (single-project case).
    let yaml_root = crate::dirs::find_workspace_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let (_workspace_config, raw_workspace) = aube_manifest::workspace::load_both(&yaml_root)
        .into_diagnostic()
        .wrap_err("failed to read workspace config")?;
    let env = aube_settings::values::process_env();
    let ctx = files.ctx(&raw_workspace, env, &[]);
    Ok(f(&ctx))
}

async fn run_filtered(
    args: UpdateArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<Option<i32>> {
    reject_unsupported_pkg_specs(&args.packages)?;
    let cwd = crate::dirs::cwd()?;
    let (root, matched) = super::select_workspace_packages(&cwd, filter, "update")?;
    let shared_workspace_lockfile = resolve_shared_workspace_lockfile(&root)?;
    let root_manifest = if shared_workspace_lockfile {
        Some(super::load_manifest_or_default(&root)?)
    } else {
        None
    };
    let result = async {
        for pkg in matched {
            let root_graph = if let Some(root_manifest) = root_manifest.as_ref() {
                Some(read_workspace_lockfile(&root, root_manifest)?)
            } else {
                None
            };
            super::retarget_cwd(&pkg.dir)?;
            // pnpm's recursive update silently skips packages that aren't
            // declared in a given project's manifest — only updates the
            // ones that match. Without this the fanout hard-errors on the
            // first project that's missing one of the named deps. Compute
            // the per-project arg list by filtering against the project's
            // direct deps, then skip the project entirely if nothing
            // matched (no work to do, no noise).
            let mut per_pkg = args.clone();
            // The top-level `run` already emitted any `--depth` warning;
            // clear it on the per-pkg clone so the recursive call doesn't
            // re-warn once per matched workspace package.
            per_pkg.depth = None;
            // `-w` retargets cwd at the workspace root inside `run`. Each
            // per-package iteration here already retargets via
            // `retarget_cwd(&pkg.dir)`; if the flag survived the clone,
            // the inner `run` would re-retarget every iteration to the
            // workspace root and lock/rewrite the root manifest for every
            // package, leaving the per-package merge step with nothing to
            // pick up. `-r` plus `-w` is documented as a no-op precisely
            // because `-r` already includes the root in its sweep.
            per_pkg.workspace = false;
            if !args.packages.is_empty() {
                let manifest_path = pkg.dir.join("package.json");
                let project_manifest = aube_manifest::PackageJson::from_path(&manifest_path)
                    .map_err(miette::Report::new)
                    .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?;
                // Mirror the bucket filter from `run` so the declared set
                // ignores entries the inner update would skip — without
                // this an arg that's only a devDep under `--prod` survives
                // the filter here and then hard-errors inside `run` with
                // 'package X is not a dependency'.
                let include_prod = !args.dev;
                let include_dev = !args.prod;
                let include_optional = !args.no_optional && !args.dev;
                let declared: BTreeSet<String> = project_manifest
                    .dependencies
                    .keys()
                    .filter(|_| include_prod)
                    .chain(
                        project_manifest
                            .dev_dependencies
                            .keys()
                            .filter(|_| include_dev),
                    )
                    .chain(
                        project_manifest
                            .optional_dependencies
                            .keys()
                            .filter(|_| include_optional),
                    )
                    .cloned()
                    .collect();
                // Same set ignoring flag filters. Used to distinguish
                // "flag-excluded direct dep" from "truly indirect dep"
                // before the lockfile fallback below — a devDep named
                // under `--prod` is in the lockfile too, so without
                // this guard it would get rescued by
                // `project_lockfile_names` and pushed into the inner
                // `run` as if it were transitive.
                let all_declared: BTreeSet<String> = project_manifest
                    .dependencies
                    .keys()
                    .chain(project_manifest.dev_dependencies.keys())
                    .chain(project_manifest.optional_dependencies.keys())
                    .cloned()
                    .collect();
                // Pull in indirect-dep names from the project's lockfile
                // too — without this, `aube update -r <indirect>@latest`
                // is silently dropped from every project (the indirect
                // isn't in `declared`, the per-project filter empties
                // `per_pkg.packages`, and the inner `run` is never
                // invoked). Prefer the project's own per-project
                // lockfile; fall back to the shared workspace-root
                // lockfile (the one `aube install` writes) when there
                // isn't one yet.
                let project_lockfile_names: BTreeSet<String> =
                    aube_lockfile::parse_lockfile(&pkg.dir, &project_manifest)
                        .ok()
                        .or_else(|| {
                            super::find_workspace_root(&pkg.dir).ok().and_then(|ws| {
                                aube_lockfile::parse_lockfile(&ws, &project_manifest).ok()
                            })
                        })
                        .map(|g| g.packages.values().map(|p| p.name.clone()).collect())
                        .unwrap_or_default();
                // Compare each arg's bare name (stripping any
                // `@<spec>` suffix) against the project's declared deps
                // and lockfile names, but pass the original raw arg into
                // the inner `run` call so it re-parses `<pkg>@latest`
                // consistently.
                per_pkg.packages = args
                    .packages
                    .iter()
                    .filter(|raw| {
                        let name = split_pkg_arg(raw).0;
                        if declared.contains(name) {
                            return true;
                        }
                        // Flag-excluded direct dep — drop instead of
                        // letting `project_lockfile_names` rescue it as
                        // a fake indirect. Mirrors pnpm's silent skip
                        // for `-r --prod <devdep>`.
                        if all_declared.contains(name) {
                            return false;
                        }
                        project_lockfile_names.contains(name)
                    })
                    .cloned()
                    .collect();
                if per_pkg.packages.is_empty() {
                    continue;
                }
            }
            Box::pin(run(
                per_pkg,
                aube_workspace::selector::EffectiveFilter::default(),
            ))
            .await?;
            if let (Some(root_manifest), Some(root_graph)) = (root_manifest.as_ref(), root_graph) {
                merge_filtered_update_lockfile(
                    &root,
                    &pkg.dir,
                    &pkg.manifest,
                    root_manifest,
                    root_graph,
                    args.ignore_pnpmfile,
                    absolute_cli_pnpmfile(&pkg.dir, args.pnpmfile.as_deref()).as_deref(),
                )
                .await?;
            }
        }
        Ok(())
    }
    .await;
    super::finish_filtered_workspace(&cwd, result)?;
    Ok(None)
}

fn resolve_shared_workspace_lockfile(cwd: &std::path::Path) -> miette::Result<bool> {
    with_update_settings_ctx(cwd, aube_settings::resolved::shared_workspace_lockfile)
}

async fn merge_filtered_update_lockfile(
    workspace_root: &std::path::Path,
    pkg_dir: &std::path::Path,
    pkg_manifest: &aube_manifest::PackageJson,
    root_manifest: &aube_manifest::PackageJson,
    root_graph: aube_lockfile::LockfileGraph,
    ignore_pnpmfile: bool,
    cli_pnpmfile: Option<&std::path::Path>,
) -> miette::Result<()> {
    let importer_path = super::workspace_importer_path(workspace_root, pkg_dir)?;
    let remove_pkg_lockfile = importer_path != ".";
    // The nested per-package update wrote its transient lockfile in
    // the same resolved format its own write path used — the
    // `package.json`-declared package manager's on a fresh package
    // dir, else the fresh-project fallback (`defaultLockfileFormat`;
    // aube-lock.yaml unless overridden).
    let pkg_kind = super::resolve_lockfile_kind_for_write(pkg_dir)?
        .unwrap_or_else(|| super::default_lockfile_kind_for_cwd(pkg_dir));
    let pkg_lockfile = pkg_dir.join(pkg_kind.filename());
    if !pkg_lockfile.exists() {
        return Ok(());
    }

    let pkg_graph = aube_lockfile::parse_lockfile(pkg_dir, pkg_manifest)
        .map_err(miette::Report::new)
        .wrap_err_with(|| format!("failed to parse {}", pkg_lockfile.display()))?;
    merge_update_graph_into_workspace_lockfile(
        workspace_root,
        pkg_dir,
        root_manifest,
        root_graph,
        pkg_graph,
        ignore_pnpmfile,
        cli_pnpmfile,
    )
    .await?;
    if remove_pkg_lockfile {
        std::fs::remove_file(&pkg_lockfile)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to remove {}", pkg_lockfile.display()))?;
    }
    Ok(())
}

// Resolve a `--pnpmfile` override to an absolute path against `cwd`. The
// workspace-merge stamp below detects the pnpmfile against `workspace_root`,
// not the member `cwd`, so a relative override has to be anchored here or it
// would resolve against the wrong base.
fn absolute_cli_pnpmfile(
    cwd: &std::path::Path,
    cli: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    cli.map(|p| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    })
}

async fn write_update_lockfile(
    cwd: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
    ignore_pnpmfile: bool,
    cli_pnpmfile: Option<&std::path::Path>,
) -> miette::Result<()> {
    let Some(workspace_root) = crate::dirs::find_workspace_root(cwd) else {
        super::write_and_log_lockfile(cwd, graph, manifest)?;
        return Ok(());
    };
    if workspace_root == cwd || !resolve_shared_workspace_lockfile(&workspace_root)? {
        super::write_and_log_lockfile(cwd, graph, manifest)?;
        return Ok(());
    }

    let root_manifest = super::load_manifest_or_default(&workspace_root)?;
    let root_graph = read_workspace_lockfile(&workspace_root, &root_manifest)?;
    merge_update_graph_into_workspace_lockfile(
        &workspace_root,
        cwd,
        &root_manifest,
        root_graph,
        graph.clone(),
        ignore_pnpmfile,
        cli_pnpmfile,
    )
    .await
}

async fn merge_update_graph_into_workspace_lockfile(
    workspace_root: &std::path::Path,
    pkg_dir: &std::path::Path,
    root_manifest: &aube_manifest::PackageJson,
    mut root_graph: aube_lockfile::LockfileGraph,
    mut pkg_graph: aube_lockfile::LockfileGraph,
    ignore_pnpmfile: bool,
    cli_pnpmfile: Option<&std::path::Path>,
) -> miette::Result<()> {
    let importer_path = super::workspace_importer_path(workspace_root, pkg_dir)?;
    let pkg_deps = pkg_graph.importers.remove(".").ok_or_else(|| {
        miette!(
            "workspace update for {} resolved without a root importer",
            pkg_dir.display()
        )
    })?;
    let pkg_skipped_optional = pkg_graph.skipped_optional_dependencies.remove(".");

    root_graph.importers.insert(importer_path.clone(), pkg_deps);
    if let Some(skipped) = pkg_skipped_optional {
        root_graph
            .skipped_optional_dependencies
            .insert(importer_path.clone(), skipped);
    } else {
        root_graph
            .skipped_optional_dependencies
            .remove(&importer_path);
    }
    if let Some(extra) = pkg_graph.workspace_extra_fields.remove(".") {
        root_graph
            .workspace_extra_fields
            .insert(importer_path, extra);
    } else {
        root_graph.workspace_extra_fields.remove(&importer_path);
    }
    root_graph.packages.extend(pkg_graph.packages);
    root_graph.times.extend(pkg_graph.times);
    root_graph.catalogs.extend(pkg_graph.catalogs);
    root_graph
        .patched_dependencies
        .extend(pkg_graph.patched_dependencies);
    for trusted in pkg_graph.trusted_dependencies {
        if !root_graph.trusted_dependencies.contains(&trusted) {
            root_graph.trusted_dependencies.push(trusted);
        }
    }
    root_graph.extra_fields.extend(pkg_graph.extra_fields);

    let mut root_graph = root_graph.filter_deps(|_| true);
    retain_package_times(&mut root_graph);
    // Stamp the *root* lockfile against the workspace-root config
    // (its package extensions + pnpmfile), matching what an install
    // from the root would write — otherwise the shared lockfile loses
    // the checksums on every `aube update` in a member package. Honor the
    // same `--ignore-pnpmfile` / `--pnpmfile` flags the member resolve
    // used so the root stamp can't re-add a pnpmfileChecksum the user
    // opted out of (or stamp the wrong hook).
    install::finalize_lockfile_graph(
        workspace_root,
        &mut root_graph,
        root_manifest,
        ignore_pnpmfile,
        cli_pnpmfile,
    )
    .await?;
    super::write_and_log_lockfile(workspace_root, &root_graph, root_manifest)?;
    Ok(())
}

fn retain_package_times(graph: &mut aube_lockfile::LockfileGraph) {
    let live_specs: BTreeSet<String> = graph
        .packages
        .values()
        .flat_map(|pkg| {
            [
                pkg.spec_key(),
                format!("{}@{}", pkg.registry_name(), pkg.version),
            ]
        })
        .collect();
    graph.times.retain(|spec, _| live_specs.contains(spec));
}

fn read_workspace_lockfile(
    workspace_root: &std::path::Path,
    root_manifest: &aube_manifest::PackageJson,
) -> miette::Result<aube_lockfile::LockfileGraph> {
    match aube_lockfile::parse_lockfile(workspace_root, root_manifest) {
        Ok(graph) => Ok(graph),
        Err(aube_lockfile::Error::NotFound(_)) => Ok(aube_lockfile::LockfileGraph::default()),
        Err(e) => Err(miette::Report::new(e)).wrap_err("failed to parse workspace lockfile"),
    }
}

/// Split a `<pkg>@<spec>` arg into a bare name and an optional spec.
/// Scope-aware: `@scope/foo@latest` → (`@scope/foo`, `Some("latest")`).
/// Bare names (`foo`, `@scope/foo`) yield `None`.
fn split_pkg_arg(arg: &str) -> (&str, Option<&str>) {
    let search_start = if arg.starts_with('@') { 1 } else { 0 };
    match arg[search_start..].find('@') {
        Some(rel) => {
            let at = search_start + rel;
            (&arg[..at], Some(&arg[at + 1..]))
        }
        None => (arg, None),
    }
}

/// Reject any `<pkg>@<spec>` arg whose spec isn't `latest`. Other forms
/// (`foo@^2.0.0`, `foo@1.2.3`) are *parsed* by `split_pkg_arg` but the
/// rest of the update path only acts on `@latest` — silently swallowing
/// them would leave the user wondering why their spec didn't take. Hard
/// error early with the supported alternatives so it's discoverable;
/// future work can lift the restriction by threading the spec into the
/// resolver_manifest rewrite + manifest write paths.
fn reject_unsupported_pkg_specs(packages: &[String]) -> miette::Result<()> {
    for raw in packages {
        let (name, spec) = split_pkg_arg(raw);
        if let Some(s) = spec
            && s != "latest"
        {
            return Err(miette!(
                "package spec '{name}@{s}' is not supported by `update` — use `--latest` (or `<pkg>@latest`) to bump past the manifest range, or omit the spec to refresh in-range",
            ));
        }
    }
    Ok(())
}

/// Rewrite a direct-dep specifier to pin `resolved_version`, preserving:
///   - `npm:<alias>@…` aliases round-trip through the `npm:` prefix.
///   - The leading range operator (`^`, `~`, `>=`, `<`, `=`), or `^`
///     when the original was a bare version / dist-tag / missing.
///
/// `exact == true` forces an exact pin regardless of the original
/// prefix (the `--save-exact` / `-E` knob).
fn rewrite_specifier(
    original: &str,
    real_name: &str,
    resolved_version: &str,
    exact: bool,
) -> String {
    let (prefix, is_alias) = if let Some(rest) = original.strip_prefix("npm:") {
        let range = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or("");
        (if exact { "" } else { range_prefix(range) }, true)
    } else {
        (if exact { "" } else { range_prefix(original) }, false)
    };
    let versioned = format!("{prefix}{resolved_version}");
    if is_alias {
        format!("npm:{real_name}@{versioned}")
    } else {
        versioned
    }
}

/// Extract the leading range operator so `rewrite_specifier` can glue
/// it back onto the resolved version. Returns an empty string for an
/// exact pin (`1.2.3`) so `update --latest` doesn't silently flip it
/// into a caret. Dist-tags and unknown shapes default to `^` — there
/// is no operator to preserve and a bare resolved version would
/// accidentally pin what was previously a floating range.
fn range_prefix(spec: &str) -> &'static str {
    let trimmed = spec.trim_start();
    if trimmed.starts_with("^") {
        "^"
    } else if trimmed.starts_with("~") {
        "~"
    } else if trimmed.starts_with(">=") {
        ">="
    } else if trimmed.starts_with("<=") {
        "<="
    } else if trimmed.starts_with('>') {
        ">"
    } else if trimmed.starts_with('<') {
        "<"
    } else if trimmed.starts_with('=') {
        "="
    } else if looks_like_exact_version(trimmed) {
        ""
    } else {
        "^"
    }
}

/// A rough "is this a concrete semver?" check: first char must be a
/// digit and every remaining char must be a member of the semver
/// grammar (digits, `.`, `-`, `+`, ASCII letters for prerelease/build
/// ids). Deliberately permissive — the goal is to tell `1.2.3` apart
/// from `latest`, not to fully validate semver.
fn looks_like_exact_version(spec: &str) -> bool {
    let mut chars = spec.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

/// If `spec` is an exact-version pin (no caret, tilde, range
/// operator, or non-semver tag), return the version substring.
/// Handles bare `1.2.3` and `=1.2.3` plus the `npm:<real>@<version>`
/// alias form. Returns `None` for ranges, tags, or anything else
/// `looks_like_exact_version` rejects.
///
/// The returned slice is the bare version (already stripped of
/// `npm:`, the `<name>@` alias prefix, the optional `=` operator, and
/// any surrounding whitespace) — suitable for `Version::parse`. It is
/// NOT a valid round-trip back to the original specifier.
fn exact_pin_version(spec: &str) -> Option<&str> {
    let stripped = spec.strip_prefix("npm:").unwrap_or(spec);
    // Drop the optional `<name>@` prefix on alias forms.
    let after_name = stripped
        .rsplit_once('@')
        .map(|(_, v)| v)
        .unwrap_or(stripped);
    let trimmed = after_name.trim_start_matches('=').trim();
    looks_like_exact_version(trimmed).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locked(name: &str, version: &str) -> aube_lockfile::LockedPackage {
        aube_lockfile::LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            dep_path: format!("{name}@{version}"),
            ..Default::default()
        }
    }

    #[test]
    fn retain_package_times_drops_versions_no_longer_in_graph() {
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph
            .packages
            .insert("foo@100.1.0".to_string(), locked("foo", "100.1.0"));
        graph.times.insert(
            "foo@100.0.0".to_string(),
            "2026-01-01T00:00:00.000Z".to_string(),
        );
        graph.times.insert(
            "foo@100.1.0".to_string(),
            "2026-01-02T00:00:00.000Z".to_string(),
        );

        retain_package_times(&mut graph);

        assert_eq!(
            graph.times.keys().cloned().collect::<Vec<_>>(),
            vec!["foo@100.1.0"]
        );
    }

    #[test]
    fn retain_package_times_accepts_alias_registry_key() {
        let mut pkg = locked("foo-alias", "1.0.0");
        pkg.alias_of = Some("foo".to_string());

        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.packages.insert(pkg.dep_path.clone(), pkg);
        graph.times.insert(
            "foo@1.0.0".to_string(),
            "2026-01-01T00:00:00.000Z".to_string(),
        );

        retain_package_times(&mut graph);

        assert!(graph.times.contains_key("foo@1.0.0"));
    }

    #[test]
    fn lookup_pkg_uses_importer_edge_to_disambiguate_duplicate_names() {
        // Discussion #623: when the same package name lives at multiple
        // versions in `packages` (a `catalog:` direct dep at 6.2.3 plus a
        // transitive at 5.10.0 from another snapshot), `lookup_pkg` must
        // return the importer's actual resolved version — not whichever
        // dep_path comes first in BTreeMap iteration order ("jose@5.10.0"
        // < "jose@6.2.3" lex-wise).
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph
            .packages
            .insert("jose@5.10.0".to_string(), locked("jose", "5.10.0"));
        graph
            .packages
            .insert("jose@6.2.3".to_string(), locked("jose", "6.2.3"));
        graph.importers.insert(
            ".".to_string(),
            vec![aube_lockfile::DirectDep {
                name: "jose".to_string(),
                dep_path: "jose@6.2.3".to_string(),
                dep_type: aube_lockfile::DepType::Production,
                specifier: Some("catalog:".to_string()),
            }],
        );

        let pkg = lookup_pkg(&graph, &["."], "jose", "jose")
            .expect("jose direct dep should resolve via importer edge");
        assert_eq!(pkg.version, "6.2.3");
    }

    #[test]
    fn lookup_pkg_walks_workspace_importer_after_root() {
        // Sub-package under a shared workspace lockfile: the loaded graph
        // has importers ".", "./pkgs/a", "./pkgs/b". The cwd's importer
        // is "./pkgs/a"; "." carries an unrelated dep so the helper must
        // skip past it without false-matching by name.
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph
            .packages
            .insert("foo@1.0.0".to_string(), locked("foo", "1.0.0"));
        graph
            .packages
            .insert("foo@2.0.0".to_string(), locked("foo", "2.0.0"));
        graph.importers.insert(
            ".".to_string(),
            vec![aube_lockfile::DirectDep {
                name: "bar".to_string(),
                dep_path: "bar@1.0.0".to_string(),
                dep_type: aube_lockfile::DepType::Production,
                specifier: Some("^1.0.0".to_string()),
            }],
        );
        graph.importers.insert(
            "./pkgs/a".to_string(),
            vec![aube_lockfile::DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@2.0.0".to_string(),
                dep_type: aube_lockfile::DepType::Production,
                specifier: Some("^2.0.0".to_string()),
            }],
        );

        let pkg = lookup_pkg(&graph, &[".", "./pkgs/a"], "foo", "foo")
            .expect("foo should resolve via the sub-package importer");
        assert_eq!(pkg.version, "2.0.0");
    }

    #[test]
    fn lookup_pkg_prefers_first_importer_when_both_carry_the_same_name() {
        // Shared-workspace lockfile case: the root manifest and the
        // sub-package both depend on `typescript` at different versions.
        // The lockfile records each under its own importer key. Looking
        // up `typescript` for the sub-package must return the
        // sub-package's locked version — `existing_importers` is built
        // as `[cwd_importer, "."]` so the sub-package's entry is tried
        // first; this test pins that contract on the helper.
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.packages.insert(
            "typescript@5.0.0".to_string(),
            locked("typescript", "5.0.0"),
        );
        graph.packages.insert(
            "typescript@5.5.0".to_string(),
            locked("typescript", "5.5.0"),
        );
        graph.importers.insert(
            ".".to_string(),
            vec![aube_lockfile::DirectDep {
                name: "typescript".to_string(),
                dep_path: "typescript@5.0.0".to_string(),
                dep_type: aube_lockfile::DepType::Dev,
                specifier: Some("^5.0.0".to_string()),
            }],
        );
        graph.importers.insert(
            "./packages/foo".to_string(),
            vec![aube_lockfile::DirectDep {
                name: "typescript".to_string(),
                dep_path: "typescript@5.5.0".to_string(),
                dep_type: aube_lockfile::DepType::Dev,
                specifier: Some("^5.5.0".to_string()),
            }],
        );

        let pkg = lookup_pkg(&graph, &["./packages/foo", "."], "typescript", "typescript")
            .expect("typescript should resolve via the sub-package importer first");
        assert_eq!(pkg.version, "5.5.0");
    }

    #[test]
    fn lookup_pkg_returns_none_when_no_importer_carries_the_dep() {
        // Mid-`update -r` lockfile read scenario: the workspace lockfile
        // already lists project-1's importer (with `foo@3.0.0-rc.0`) but
        // not yet project-2's. Looking up `foo` for project-2's importer
        // must return None — falling back to a name scan across `packages`
        // would return project-1's snapshot and trip the prerelease-pin
        // guard against the wrong "current" version.
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph
            .packages
            .insert("foo@3.0.0-rc.0".to_string(), locked("foo", "3.0.0-rc.0"));
        graph.importers.insert(
            "project-1".to_string(),
            vec![aube_lockfile::DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@3.0.0-rc.0".to_string(),
                dep_type: aube_lockfile::DepType::Production,
                specifier: Some("3.0.0-rc.0".to_string()),
            }],
        );

        // project-2 isn't in importers yet — the helper must say so.
        assert!(lookup_pkg(&graph, &[".", "project-2"], "foo", "foo").is_none());
    }

    #[test]
    fn workspace_package_versions_includes_root_and_members() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("package.json"),
            br#"{"name": "@my/root", "version": "2.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            b"packages:\n  - packages/*\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("packages/lib")).unwrap();
        std::fs::write(
            root.join("packages/lib/package.json"),
            br#"{"name": "@my/lib", "version": "1.0.0"}"#,
        )
        .unwrap();

        let versions = workspace_package_versions(root).unwrap();
        assert_eq!(versions.get("@my/root").unwrap(), "2.0.0");
        assert_eq!(versions.get("@my/lib").unwrap(), "1.0.0");
        assert_eq!(versions.len(), 2);
    }

    #[test]
    fn workspace_package_versions_defaults_version_for_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("package.json"), br#"{"name": "@my/root"}"#).unwrap();
        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            b"packages:\n  - packages/*\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("packages/lib")).unwrap();
        std::fs::write(
            root.join("packages/lib/package.json"),
            br#"{"name": "@my/lib", "version": "1.0.0"}"#,
        )
        .unwrap();

        let versions = workspace_package_versions(root).unwrap();
        assert_eq!(versions.get("@my/root").unwrap(), "0.0.0");
        assert_eq!(versions.get("@my/lib").unwrap(), "1.0.0");
    }

    #[test]
    fn workspace_package_versions_skips_root_when_yaml_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // No root package.json — yaml-only workspace.
        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            b"packages:\n  - packages/*\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("packages/lib")).unwrap();
        std::fs::write(
            root.join("packages/lib/package.json"),
            br#"{"name": "@my/lib", "version": "1.0.0"}"#,
        )
        .unwrap();

        let versions = workspace_package_versions(root).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions.get("@my/lib").unwrap(), "1.0.0");
        assert!(!versions.contains_key("@my/root"));
    }

    #[test]
    fn workspace_package_versions_includes_root_when_subpackage_depends_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("package.json"),
            br#"{"name": "@my/root", "version": "3.0.0"}"#,
        )
        .unwrap();
        std::fs::write(root.join("pnpm-workspace.yaml"), b"packages:\n  - docs\n").unwrap();
        // Sub-package docs/ depends on root via workspace:*
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(
            root.join("docs/package.json"),
            br#"{"name": "@my/docs", "version": "1.0.0", "dependencies": {"@my/root": "workspace:*"}}"#,
        )
        .unwrap();

        let versions = workspace_package_versions(root).unwrap();
        assert_eq!(versions.get("@my/root").unwrap(), "3.0.0");
        assert_eq!(versions.get("@my/docs").unwrap(), "1.0.0");
    }
}
