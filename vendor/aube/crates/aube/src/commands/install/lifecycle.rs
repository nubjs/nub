use miette::{Context, IntoDiagnostic, miette};

use super::bin_linking::{dep_modules_dir_for, materialized_pkg_dir};
use super::node_gyp_bootstrap;
use super::side_effects_cache::{
    SideEffectsCacheConfig, SideEffectsCacheEntry, SideEffectsCacheRestore,
};

/// Run a root-package lifecycle hook, announcing it to the user if defined
/// and turning aube_scripts::Error into a miette::Report with context.
/// Silent when the hook isn't defined in package.json.
pub(super) async fn run_root_lifecycle(
    project_dir: &std::path::Path,
    modules_dir_name: &str,
    manifest: &aube_manifest::PackageJson,
    hook: aube_scripts::LifecycleHook,
) -> miette::Result<()> {
    // Only announce when the hook is actually defined, so projects without
    // lifecycle scripts don't get noise in their install output.
    if !manifest.scripts.contains_key(hook.script_name()) {
        return Ok(());
    }
    tracing::debug!("Running {} script...", hook.script_name());
    aube_scripts::run_root_hook(project_dir, modules_dir_name, manifest, hook)
        .await
        .map_err(|e| {
            // Old message was just the bare error string. User got
            // a cryptic "exit status 1" with no hook name, no script
            // path, nothing. Tag with which hook fired so the log
            // line is self-documenting. This is the common case
            // (failed preinstall on `aube install`) so the regression
            // really hurt triage.
            miette!("root {} script failed: {e}", hook.script_name())
        })?;
    Ok(())
}

/// Build the dependency lifecycle-script `BuildPolicy` by merging
/// every supported source on the root manifest + workspace file:
///
/// - `package.json` / `pnpm-workspace.yaml` `pnpm.allowBuilds` map
///   (aube's superset format — patterns with bool values)
/// - `package.json` / `pnpm-workspace.yaml` `pnpm.onlyBuiltDependencies`
///   flat list (pnpm's canonical allowlist, used by nearly every
///   real-world pnpm project)
/// - `package.json` / `pnpm-workspace.yaml` `pnpm.neverBuiltDependencies`
///   flat list (pnpm's canonical denylist)
/// - `package.json` `dependenciesMeta.<pkg>.built: false` (yarn/pnpm's
///   per-package "do not build this dependency" directive — folded into
///   the same denylist; `true`/absent is the default, only `false` denies)
/// - the `--dangerously-allow-all-builds` escape hatch
///
/// Workspace-level entries in the `allowBuilds` map take precedence
/// over the manifest map for the same pattern, matching pnpm. The
/// flat lists are pure append — deny always wins at `decide()` time.
pub(crate) fn build_policy_from_sources(
    manifest: &aube_manifest::PackageJson,
    workspace: &aube_manifest::WorkspaceConfig,
    dangerously_allow_all_builds: bool,
) -> (
    aube_scripts::BuildPolicy,
    Vec<aube_scripts::BuildPolicyError>,
) {
    build_policy_from_manifest_sources(
        std::iter::once(manifest),
        workspace,
        dangerously_allow_all_builds,
    )
}

pub(crate) fn build_policy_from_manifest_sources<'a>(
    manifests: impl IntoIterator<Item = &'a aube_manifest::PackageJson>,
    workspace: &aube_manifest::WorkspaceConfig,
    dangerously_allow_all_builds: bool,
) -> (
    aube_scripts::BuildPolicy,
    Vec<aube_scripts::BuildPolicyError>,
) {
    let mut merged = std::collections::BTreeMap::new();
    let mut only_built = Vec::new();
    let mut never_built = Vec::new();
    for manifest in manifests {
        for (pattern, allow) in manifest.pnpm_allow_builds() {
            merged
                .entry(pattern)
                .and_modify(|existing| merge_allow_build(existing, allow.clone()))
                .or_insert(allow);
        }
        only_built.extend(manifest.pnpm_only_built_dependencies());
        only_built.extend(manifest.trusted_dependencies());
        never_built.extend(manifest.pnpm_never_built_dependencies());
        // `dependenciesMeta.<pkg>.built: false` (yarn/pnpm) is a
        // per-package deny — fold it into the same denylist as
        // `neverBuiltDependencies`. Neutral package.json field, so it
        // applies for every incumbent.
        never_built.extend(manifest.dependencies_meta_built_false());
    }
    for (k, v) in workspace.allow_builds_raw() {
        merged.insert(k, v);
    }
    only_built.extend(workspace.only_built_dependencies.iter().cloned());
    never_built.extend(workspace.never_built_dependencies.iter().cloned());
    aube_scripts::BuildPolicy::from_config(
        &merged,
        &only_built,
        &never_built,
        dangerously_allow_all_builds,
    )
}

fn merge_allow_build(
    existing: &mut aube_manifest::AllowBuildRaw,
    next: aube_manifest::AllowBuildRaw,
) {
    use aube_manifest::AllowBuildRaw;
    match (&*existing, next) {
        (AllowBuildRaw::Bool(false), _) | (_, AllowBuildRaw::Bool(true)) => {}
        (_, AllowBuildRaw::Bool(false)) => *existing = AllowBuildRaw::Bool(false),
        (AllowBuildRaw::Bool(true), other) => *existing = other,
        (AllowBuildRaw::Other(_), AllowBuildRaw::Other(_)) => {}
    }
}

#[derive(Debug, Clone)]
pub(crate) struct JailBuildPolicy {
    enabled: bool,
    denylist: aube_scripts::BuildPolicy,
    grants: Vec<(String, aube_manifest::JailBuildPermission)>,
}

impl JailBuildPolicy {
    pub(crate) fn from_settings(
        ctx: &aube_settings::ResolveCtx<'_>,
        workspace: &aube_manifest::WorkspaceConfig,
    ) -> (Self, Vec<String>) {
        // `paranoid=true` forces the jail on regardless of `jailBuilds`.
        let enabled =
            aube_settings::resolved::jail_builds(ctx) || aube_settings::resolved::paranoid(ctx);
        let jail_exclusions = aube_settings::resolved::jail_build_exclusions(ctx);
        let (denylist, denylist_warnings) = aube_scripts::BuildPolicy::denylist(&jail_exclusions);
        let mut warnings = denylist_warnings
            .into_iter()
            .map(|warning| format!("jailBuildExclusions: {warning}"))
            .collect::<Vec<_>>();
        let grants = workspace
            .jail_build_permissions
            .iter()
            .filter_map(|(pattern, grant)| {
                if let Err(err) = aube_scripts::pattern_matches(pattern, "", "") {
                    warnings.push(format!("jailBuildPermissions: {err}"));
                    return None;
                }
                Some((pattern.clone(), grant.clone()))
            })
            .collect();
        (
            Self {
                enabled,
                denylist,
                grants,
            },
            warnings,
        )
    }

    fn should_jail(&self, name: &str, version: &str, source_key: Option<&str>) -> bool {
        self.enabled
            && !matches!(
                self.denylist.decide_package(name, version, source_key),
                aube_scripts::AllowDecision::Deny
            )
    }

    fn jail_for(
        &self,
        name: &str,
        version: &str,
        source_key: Option<&str>,
        package_dir: &std::path::Path,
        project_dir: &std::path::Path,
    ) -> Option<aube_scripts::ScriptJail> {
        if !self.should_jail(name, version, source_key) {
            return None;
        }
        let mut env = Vec::new();
        let mut read_paths = Vec::new();
        let mut write_paths = Vec::new();
        let mut network = false;
        for (pattern, grant) in &self.grants {
            match aube_scripts::pattern_matches(pattern, name, version) {
                Ok(true) => {
                    env.extend(grant.env.iter().cloned());
                    read_paths.extend(
                        grant
                            .read
                            .iter()
                            .map(|path| resolve_jail_grant_path(project_dir, path)),
                    );
                    write_paths.extend(
                        grant
                            .write
                            .iter()
                            .map(|path| resolve_jail_grant_path(project_dir, path)),
                    );
                    network |= grant.network;
                }
                Ok(false) => {}
                Err(_) => {}
            }
        }
        Some(
            aube_scripts::ScriptJail::new(package_dir)
                .with_env(env)
                .with_read_paths(read_paths)
                .with_write_paths(write_paths)
                .with_network(network),
        )
    }
}

fn resolve_jail_grant_path(project_dir: &std::path::Path, raw: &str) -> std::path::PathBuf {
    let path = raw.trim();
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return std::path::PathBuf::from(home).join(rest);
    }
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_dir.join(path)
    }
}

/// Resolve the link strategy (reflink / hardlink / copy) from CLI
/// override, `.npmrc` / `pnpm-workspace.yaml`, or filesystem detection.
/// Shared by the prewarm-GVS materializer (which needs the strategy
/// before the full linker is built) and the link phase proper.
///
/// `planned_gvs` tells the probe where the linker will actually write
/// files: when GVS is on, materialization targets the GVS dir (always
/// on the cache-store FS), and `node_modules/.aube/<dep_path>` is a
/// cross-FS-tolerant symlink. When GVS is off, materialization writes
/// straight into the project's `.aube/<dep_path>`. Probing the
/// destination the writes will hit avoids the cross-FS Copy verdict
/// that would otherwise mis-fire on an install where the project
/// lives on a different volume than the store but the GVS layer
/// already absorbs the FS boundary as a symlink.
pub(super) fn resolve_link_strategy(
    cwd: &std::path::Path,
    ctx: &aube_settings::ResolveCtx<'_>,
    planned_gvs: bool,
) -> miette::Result<aube_linker::LinkStrategy> {
    let package_import_method_cli =
        aube_settings::values::string_from_cli("packageImportMethod", ctx.cli);
    // Shared probe used by both the CLI and resolved-setting paths
    // below. The destination passed to `detect_strategy_cross` is the
    // dir the linker will materialize files into:
    //   * GVS enabled → `<store>/virtual-store/` (same FS as store →
    //     hardlink works even when the *project* is on another mount;
    //     the cross-FS hop is absorbed by the symlink from
    //     `node_modules/.aube/<dep_path>`).
    //   * GVS disabled → the project's `.aube/<dep_path>` lives on
    //     the project FS, so probe against `cwd` to catch the cross-
    //     FS case before every file `fs::copy` silently falls back.
    let auto_probe = || {
        // Open the store once and derive both paths from the same
        // handle. `open_store` performs lockfile + IO work; a second
        // call to fetch `virtual_store_dir` would repeat that on the
        // hot path of every `auto`-mode install.
        let store = super::super::open_store(cwd).ok();
        let store_dir = store.as_ref().map(|s| s.root().to_path_buf());
        // Probe against the GVS dir when GVS is on. The GVS dir won't
        // exist yet on a cold install, so create it before the probe
        // writes its test file. If creation fails (permission, ENOSPC,
        // …) `gvs_dir` falls back to `None` so the probe targets `cwd`
        // — better to under-probe than to probe a non-existent dir and
        // get a spurious `Copy` verdict.
        let gvs_dir = planned_gvs
            .then(|| store.as_ref().map(|s| s.virtual_store_dir()))
            .flatten()
            .filter(|gvs| std::fs::create_dir_all(gvs).is_ok());
        let probe_dst = gvs_dir.as_deref().unwrap_or(cwd);
        let strategy = match store_dir.as_deref() {
            Some(sd) => aube_linker::Linker::detect_strategy_cross(sd, probe_dst),
            None => aube_linker::Linker::detect_strategy(probe_dst),
        };
        // Two distinct cross-volume regimes, two different messages.
        // With GVS on, the probe targets the GVS dir (always same FS
        // as the store in a sane setup), so Copy here means the user
        // pointed `storeDir` and `XDG_CACHE_HOME` at different volumes
        // — a real misconfiguration that costs per-file copies. Warn.
        // With GVS off, the probe targets `cwd`; a cross-volume verdict
        // there is the documented "project lives on an external mount"
        // regime where aube still outperforms other PMs, so log at
        // debug to keep the warning out of normal install output.
        if matches!(strategy, aube_linker::LinkStrategy::Copy)
            && let Some(sd) = store_dir.as_deref()
            && aube_util::fs::cross_volume(sd, probe_dst)
        {
            if gvs_dir.is_some() {
                tracing::warn!(
                    store = %sd.display(),
                    gvs_dir = %probe_dst.display(),
                    "global virtual store dir is on a different volume than `storeDir`; \
                     install will fall back to per-file copy. Move `XDG_CACHE_HOME` \
                     (or `storeDir`) so both live on the same volume."
                );
            } else {
                tracing::debug!(
                    store = %sd.display(),
                    project = %cwd.display(),
                    "cross-volume install, using per-file copy. set `storeDir` to a path on the project volume for hardlink fast path."
                );
            }
        }
        strategy
    };
    let strategy = if let Some(cli) = package_import_method_cli.as_deref() {
        match cli.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => auto_probe(),
            "hardlink" => aube_linker::LinkStrategy::Hardlink,
            "copy" => aube_linker::LinkStrategy::Copy,
            "clone-or-copy" => aube_linker::LinkStrategy::Reflink,
            "clone" => {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_CLONE_STRATEGY_FALLBACK,
                    "package-import-method=clone: reflink will silently fall back to copy \
                     if the filesystem does not support it (strict enforcement is a known TODO)"
                );
                aube_linker::LinkStrategy::Reflink
            }
            other => {
                return Err(miette!(
                    "unknown --package-import-method value `{other}`; expected `auto`, `hardlink`, `copy`, `clone`, or `clone-or-copy`"
                ));
            }
        }
    } else {
        match aube_settings::resolved::package_import_method(ctx) {
            aube_settings::resolved::PackageImportMethod::Auto => auto_probe(),
            aube_settings::resolved::PackageImportMethod::Hardlink => {
                aube_linker::LinkStrategy::Hardlink
            }
            aube_settings::resolved::PackageImportMethod::Copy => aube_linker::LinkStrategy::Copy,
            aube_settings::resolved::PackageImportMethod::CloneOrCopy => {
                aube_linker::LinkStrategy::Reflink
            }
            aube_settings::resolved::PackageImportMethod::Clone => {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_CLONE_STRATEGY_FALLBACK,
                    "package-import-method=clone: reflink will silently fall back to copy \
                     if the filesystem does not support it (strict enforcement is a known TODO)"
                );
                aube_linker::LinkStrategy::Reflink
            }
        }
    };
    Ok(strategy)
}

/// Walk every linked dependency, check its `package.json` for
/// lifecycle scripts, and run the ones the policy allows. Runs
/// `preinstall` → `install` → `postinstall` per package in that order;
/// `prepare` is skipped for deps (pnpm does the same).
///
/// `package_indices` gives us the stored `package.json` for each dep
/// without a second disk read, and the actual execution cwd is
/// `node_modules/.aube/<dep_path>/node_modules/<name>` — i.e. the
/// linked dir inside the virtual store.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_dep_lifecycle_scripts(
    project_dir: &std::path::Path,
    modules_dir_name: &str,
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    policy: &aube_scripts::BuildPolicy,
    // The `defaultTrust` floor, consulted only when `policy` leaves a
    // package `Unspecified`. Pass `DefaultTrustFloor::disabled()` on
    // paths that must never floor (rebuild).
    floor: &super::default_trust::DefaultTrustFloor,
    virtual_store_dir_max_length: usize,
    child_concurrency: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
    side_effects_cache: SideEffectsCacheConfig<'_>,
    jail_policy: &JailBuildPolicy,
    // `Some` enables install-delta mode: only current graph entries
    // whose dep_path changed since the prior install are eligible.
    // Policy still gates those packages below. This differs from
    // `selected_names`, which is rebuild's explicit user selection
    // and intentionally bypasses policy.
    selected_dep_paths: Option<&std::collections::BTreeSet<String>>,
    // `Some` enables selective mode: only deps whose in-tree `name`
    // (the alias when one is configured) is in the set are eligible,
    // and the policy is bypassed for those deps. `None` is the
    // default install path: every dep is eligible and the policy
    // gates which ones actually run. Match is by `pkg.name`, matching
    // pnpm's `pnpm rebuild <name>`.
    selected_names: Option<&std::collections::HashSet<String>>,
) -> miette::Result<usize> {
    // Pass 1 (serial, cheap): walk the graph, keep only the packages
    // the policy allows AND that actually define at least one dep
    // lifecycle hook in their on-disk `package.json`. Filtering up front
    // means the fan-out below only spawns real work — no tokio task per
    // every 200-package graph for a graph that has 3 allowlisted deps.
    #[derive(Clone)]
    struct BuildJob {
        name: String,
        registry_name: String,
        version: String,
        source_key: Option<String>,
        package_dir: std::path::PathBuf,
        /// Directory containing the dep package and its sibling
        /// symlinks — i.e. `package_dir`'s enclosing `node_modules/`.
        /// `<dep_modules_dir>/.bin` is prepended to PATH so the
        /// postinstall can call binaries declared in the dep's own
        /// `dependencies`. See `link_dep_bins` for the write side.
        dep_modules_dir: std::path::PathBuf,
        manifest: aube_manifest::PackageJson,
        cache_entry: Option<SideEffectsCacheEntry>,
    }

    let mut jobs: Vec<BuildJob> = Vec::new();
    let mut floor_trusted: Vec<String> = Vec::new();
    for (dep_path, pkg) in &graph.packages {
        if let Some(selected) = selected_dep_paths
            && !selected.contains(dep_path)
        {
            continue;
        }
        // True when this package runs only because the `defaultTrust`
        // floor vouched for it (policy said `Unspecified`). Recorded
        // alongside the job so the floor is never silent about what
        // it let through.
        let mut via_floor = false;
        if let Some(selected) = selected_names {
            // Selective mode: user named this dep explicitly, so
            // bypass the policy. Match by `pkg.name` (the in-tree
            // alias when one is configured), matching pnpm's
            // `pnpm rebuild <name>`.
            if !selected.contains(&pkg.name) {
                continue;
            }
        } else {
            // Use registry_name(), not pkg.name. pkg.name is the in-tree
            // alias (`h3-safe`). Real package is `h3`. Allowlist entry for
            // `h3` would miss if we checked against the alias. Attacker
            // writes `"h3-safe": "npm:h3@0.19.0"` to sneak a denied pkg
            // through the allowlist. registry_name() strips alias back to
            // real name. #860 added a per-package source key
            // (`Option<String>`); `decide_package` consults the explicit
            // allowlist/denylist by name+version+source. We keep nub's
            // `defaultTrust` floor as a belt-and-suspenders arm that only
            // fires on `Unspecified`, so explicit entries always win.
            let source_key = pkg.source_approval_key();
            match policy.decide_package(pkg.registry_name(), &pkg.version, source_key.as_deref()) {
                aube_scripts::AllowDecision::Allow => {}
                aube_scripts::AllowDecision::Unspecified if floor.trusts(pkg, &graph.times) => {
                    via_floor = true;
                }
                aube_scripts::AllowDecision::Deny | aube_scripts::AllowDecision::Unspecified => {
                    continue;
                }
            }
        }
        let package_dir = materialized_pkg_dir(
            aube_dir,
            dep_path,
            &pkg.name,
            virtual_store_dir_max_length,
            placements,
        );
        if !package_dir.exists() {
            tracing::debug!(
                "allowBuilds: skipping {} — {} not on disk",
                pkg.name,
                package_dir.display()
            );
            continue;
        }
        // Read the dep's `package.json` directly from its materialized
        // location. Previously we looked it up via `package_indices`,
        // but the fetch phase now skips `load_index` for packages
        // whose virtual-store entry already exists (which is every
        // package on a no-op re-install), so the map is sparse and
        // many dep_paths legitimately won't have an entry. The
        // on-disk file is hardlinked to the same bytes the store
        // would have pointed us at.
        //
        // `NotFound` is the only error we swallow here: some packages
        // legitimately ship without a top-level `package.json` (or
        // the field gets stripped by linkers that treat the virtual
        // store as opaque), and we shouldn't fail the install over
        // that. Every other I/O error — permission denied, disk
        // corruption, short reads — surfaces as a hard failure so
        // the user sees the real problem instead of a silently
        // skipped `node-gyp rebuild` or similar.
        let pkg_json_path = package_dir.join("package.json");
        let pkg_json_content = match std::fs::read_to_string(&pkg_json_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(miette!(
                    "failed to read package.json for {} at {}: {}",
                    pkg.name,
                    pkg_json_path.display(),
                    e
                ));
            }
        };
        let dep_manifest = aube_manifest::PackageJson::parse(&pkg_json_path, pkg_json_content)
            .map_err(miette::Report::new)
            .wrap_err_with(|| {
                format!(
                    "failed to parse package.json for {}{}",
                    pkg.name,
                    crate::dep_chain::format_chain_for(&pkg.name, &pkg.version)
                )
            })?;
        // `has_dep_lifecycle_work` also accounts for the implicit
        // `node-gyp rebuild` fallback: a package with a top-level
        // `binding.gyp` and no `install`/`preinstall` script still has
        // work to run, and pre-filtering on `scripts` alone would drop
        // it before the fan-out even saw it.
        if !aube_scripts::has_dep_lifecycle_work(&package_dir, &dep_manifest) {
            continue;
        }
        let cache_entry = side_effects_cache
            .root()
            .map(|root| SideEffectsCacheEntry::new(root, &pkg.name, &pkg.version, &package_dir))
            .transpose()?;
        let dep_modules_dir = dep_modules_dir_for(&package_dir, &pkg.name);
        if via_floor {
            floor_trusted.push(pkg.spec_key());
        }
        jobs.push(BuildJob {
            name: pkg.name.clone(),
            registry_name: pkg.registry_name().to_string(),
            version: pkg.version.clone(),
            source_key: pkg.source_approval_key(),
            package_dir,
            dep_modules_dir,
            manifest: dep_manifest,
            cache_entry,
        });
    }

    if jobs.is_empty() {
        return Ok(0);
    }

    // Name what the floor let through — the floor must never be a
    // silent allow path. One line, not per-package, so big graphs
    // don't drown the install output. Emitted at `warn` with a stable
    // code because `warn` is the CLI's default-visible level — an
    // `info!` disclosure would only reach users who already opted into
    // verbose logging, which defeats its purpose.
    if !floor_trusted.is_empty() {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_DEFAULT_TRUST_BUILDS,
            count = floor_trusted.len(),
            packages = ?floor_trusted,
            "defaultTrust: running build scripts for {} default-trusted package(s): {}",
            floor_trusted.len(),
            floor_trusted.join(", ")
        );
    }

    // Bootstrap node-gyp once before the fan-out when the ambient
    // `PATH` doesn't already provide one. At least one job is about
    // to run a lifecycle script, and we can't cheaply predict which
    // ones will end up shelling out to `node-gyp` (explicit,
    // implicit via binding.gyp, or transitive via node-gyp-build).
    // If the user already has node-gyp (system install, nvm, a test
    // shim), `ensure` returns `None` and we leave their copy alone.
    let node_gyp_bin_dir = std::sync::Arc::new(node_gyp_bootstrap::ensure(project_dir).await?);

    // Pass 2 (parallel, bounded): fan out across `child_concurrency`
    // concurrent workers. Inside one job the three hooks
    // (preinstall → install → postinstall) still run sequentially —
    // pnpm's execution model is "at most N packages building in
    // parallel," not "at most N scripts running," so hook ordering
    // within a single package is preserved.
    //
    // Cancellation on first failure uses `JoinSet`, which aborts every
    // outstanding task when it's dropped. A plain `Vec<JoinHandle>`
    // would NOT be safe here — dropping a `tokio::spawn` handle lets
    // the task keep running detached, so a failing script would
    // silently leave N siblings still executing `postinstall` against
    // the user's machine after the install returned an error.
    // `join_next` also surfaces whichever task fails first rather than
    // waiting for the longest-running one to finish.
    let concurrency = child_concurrency.max(1);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let project_dir = project_dir.to_path_buf();
    let modules_dir_name = modules_dir_name.to_string();
    let should_restore_side_effects_cache = side_effects_cache.should_restore();
    let should_save_side_effects_cache = side_effects_cache.should_save();
    let overwrite_side_effects_cache = side_effects_cache.overwrite_existing();
    let jail_policy = std::sync::Arc::new((*jail_policy).clone());
    let mut set: tokio::task::JoinSet<miette::Result<usize>> = tokio::task::JoinSet::new();
    for job in jobs {
        let sem = semaphore.clone();
        let project_dir = project_dir.clone();
        let modules_dir_name = modules_dir_name.clone();
        let node_gyp_bin_dir = node_gyp_bin_dir.clone();
        let jail_policy = jail_policy.clone();
        set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            if should_restore_side_effects_cache && let Some(cache_entry) = job.cache_entry.clone()
            {
                let package_dir = job.package_dir.clone();
                let restore_result = tokio::task::spawn_blocking(move || {
                    cache_entry.restore_if_available(&package_dir)
                })
                .await
                .map_err(|e| {
                    miette!(
                        "side-effects-cache restore task panicked for {}@{}: {e}",
                        job.name,
                        job.version
                    )
                })?;
                match restore_result? {
                    SideEffectsCacheRestore::Restored | SideEffectsCacheRestore::AlreadyApplied => {
                        return Ok(0);
                    }
                    SideEffectsCacheRestore::Miss => {}
                }
            }
            // Before the lifecycle script runs in-place inside the
            // materialized package directory, break any hardlinks that
            // still share an inode with the content-addressed store. On a
            // hardlink filesystem (ext4, most Linux/CI) the linker
            // hard-links store blobs into the package dir, so an in-place
            // build write (node-gyp emitting `build/Release/*.node`, a
            // postinstall rewriting its own files) would otherwise write
            // *through* the shared inode and corrupt the machine-wide
            // store — poisoning every project that shares that content
            // hash. On reflink/copy filesystems (APFS, btrfs/xfs) the
            // materialized files already have private inodes (nlink == 1),
            // so this is a no-op and the default path is unchanged.
            // (The side-effects-cache restore branch above returns early;
            // its `copy_dir` removes and recreates the package dir, so a
            // restored package never reaches a live store link here.)
            #[cfg(unix)]
            {
                let package_dir = job.package_dir.clone();
                let name = job.name.clone();
                let version = job.version.clone();
                tokio::task::spawn_blocking(move || {
                    aube_scripts::break_cas_hardlinks(&package_dir)
                })
                .await
                .map_err(|e| miette!("store-unshare task panicked for {name}@{version}: {e}"))?
                .map_err(|e| {
                    miette!(
                        "failed to break store hardlinks for {name}@{version} before build: {e}"
                    )
                })?;
            }
            let tool_dirs: Vec<&std::path::Path> = node_gyp_bin_dir
                .as_ref()
                .as_deref()
                .map(|p| vec![p])
                .unwrap_or_default();
            let jail = jail_policy.jail_for(
                &job.registry_name,
                &job.version,
                job.source_key.as_deref(),
                &job.package_dir,
                &project_dir,
            );
            let _jail_home_cleanup = jail.as_ref().map(aube_scripts::ScriptJailHomeCleanup::new);
            let mut ran_here = 0usize;
            for hook in aube_scripts::DEP_LIFECYCLE_HOOKS {
                let did_run = aube_scripts::run_dep_hook(
                    &job.package_dir,
                    &job.dep_modules_dir,
                    &project_dir,
                    &modules_dir_name,
                    &job.manifest,
                    hook,
                    &tool_dirs,
                    jail.as_ref(),
                )
                .await
                .map_err(|e| {
                    miette!(
                        "lifecycle script {} failed for {}@{}: {}",
                        hook.script_name(),
                        job.name,
                        job.version,
                        e
                    )
                })?;
                if did_run {
                    tracing::debug!(
                        "ran {} for {}@{}",
                        hook.script_name(),
                        job.name,
                        job.version
                    );
                    ran_here += 1;
                }
            }
            if should_save_side_effects_cache
                && ran_here > 0
                && let Some(cache_entry) = job.cache_entry.clone()
            {
                let package_dir = job.package_dir.clone();
                let save_result = tokio::task::spawn_blocking(move || {
                    cache_entry.save(&package_dir, overwrite_side_effects_cache)
                })
                .await
                .map_err(|e| {
                    miette!(
                        "side-effects-cache save task panicked for {}@{}: {e}",
                        job.name,
                        job.version
                    )
                })
                .and_then(|r| r);
                if let Err(e) = save_result {
                    tracing::debug!(
                        "side-effects-cache: ignoring cache save error for {}@{}: {e}",
                        job.name,
                        job.version
                    );
                }
            }
            Ok(ran_here)
        });
    }

    let mut ran = 0usize;
    while let Some(res) = set.join_next().await {
        // `?` on the outer `Result` propagates a real task-level panic
        // (tokio's `JoinError`); `?` on the inner `miette::Result`
        // propagates a script failure. Either way, the function
        // returns, `set` is dropped, and the remaining in-flight
        // scripts are aborted before they can scribble on disk.
        ran += res.into_diagnostic()??;
    }
    Ok(ran)
}

/// Verify + import + validate + save-index for a freshly fetched
/// tarball. Shared between the lockfile-driven fetch path and the
/// no-lockfile streaming fetch path so both honor the same integrity
/// and content-check settings. Runs inside `spawn_blocking` — no
/// async in this function.
#[allow(clippy::too_many_arguments)]
pub(super) fn import_verified_tarball(
    store: &aube_store::Store,
    bytes: &[u8],
    display_name: &str,
    registry_name: &str,
    version: &str,
    integrity: Option<&str>,
    verify_integrity: bool,
    strict_integrity: bool,
    strict_pkg_content_check: bool,
) -> miette::Result<aube_store::PackageIndex> {
    if verify_integrity {
        if let Some(expected) = integrity {
            aube_store::verify_integrity(bytes, expected).map_err(|e| {
                miette!(
                    "{display_name}@{version}: {e}{}",
                    crate::dep_chain::format_chain_for(registry_name, version)
                )
            })?;
        } else if strict_integrity {
            // strict-store-integrity=true opts the user into
            // fail-closed. Default is off so ecosystem parity with
            // pnpm stays intact. A registry proxy that strips
            // dist.integrity will no longer slip past silently when
            // strict is on.
            return Err(miette!(
                "{display_name}@{version}: registry response has no `dist.integrity` and `strict-store-integrity` is on. Refusing to import unverified bytes.{}",
                crate::dep_chain::format_chain_for(registry_name, version)
            ));
        } else {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_MISSING_INTEGRITY,
                "{display_name}@{version}: registry response has no `dist.integrity`, importing without content verification. Set `strict-store-integrity=true` to refuse instead."
            );
        }
    }
    let index = store.import_tarball(bytes).map_err(|e| {
        miette!(
            "failed to import {display_name}@{version}: {e}{}",
            crate::dep_chain::format_chain_for(registry_name, version)
        )
    })?;
    // strictStorePkgContentCheck: cross-check the freshly stored
    // package.json against the resolver-asserted (name, version)
    // before the index is cached or returned to the linker. Validate
    // against `registry_name` — the real package name that appears
    // in the tarball's own `package.json` — not the alias, or this
    // would fail every npm-aliased entry.
    if strict_pkg_content_check {
        aube_store::validate_pkg_content(&index, registry_name, version).map_err(|e| {
            miette!(
                "{display_name}@{version}: {e}{}",
                crate::dep_chain::format_chain_for(registry_name, version)
            )
        })?;
    }
    // Cache under `registry_name` so two aliases of the same real
    // package hit the same on-disk index file and avoid redundant
    // fetches. When `integrity` is `Some` the filename carries a
    // `+<hex>` suffix that discriminates same-(name, version)
    // tarballs from different sources; when `None` falls back to the
    // plain name@version key so warm installs still find the cache
    // on integrity-stripping proxies.
    if let Err(e) = store.save_index(registry_name, version, integrity, &index) {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_CACHE_WRITE_FAILED,
            "Failed to cache index for {display_name}@{version}: {e}"
        );
    }
    Ok(index)
}

/// Run `import_verified_tarball_streamed` on the blocking pool.
/// Centralizes the spawn_blocking + clone + into_diagnostic dance
/// that both fetch branches duplicate. Returns (index, elapsed)
/// so callers can record import phase time without rebuilding the
/// stopwatch outside.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_import_on_blocking(
    store: std::sync::Arc<aube_store::Store>,
    bytes: bytes::Bytes,
    streamed_digest: Option<[u8; 64]>,
    display_name: String,
    registry_name: String,
    version: String,
    integrity: Option<String>,
    verify_integrity: bool,
    strict_integrity: bool,
    strict_pkg_content_check: bool,
) -> miette::Result<(aube_store::PackageIndex, std::time::Duration)> {
    use miette::IntoDiagnostic;
    tokio::task::spawn_blocking(move || -> miette::Result<_> {
        let import_start = std::time::Instant::now();
        let index = import_verified_tarball_streamed(
            &store,
            &bytes,
            streamed_digest.as_ref(),
            &display_name,
            &registry_name,
            &version,
            integrity.as_deref(),
            verify_integrity,
            strict_integrity,
            strict_pkg_content_check,
        )?;
        Ok((index, import_start.elapsed()))
    })
    .await
    .into_diagnostic()?
}

/// Streaming-aware variant of [`import_verified_tarball`]. When
/// `streamed_sha512` is `Some`, the SRI is verified against the
/// precomputed digest and the buffered hash pass is skipped. When
/// the SRI uses a non-SHA-512 algo (legacy), the buffered fallback
/// re-hashes with the right algo. `None` is identical to calling
/// `import_verified_tarball` directly.
#[allow(clippy::too_many_arguments)]
pub(super) fn import_verified_tarball_streamed(
    store: &aube_store::Store,
    bytes: &[u8],
    streamed_sha512: Option<&[u8; 64]>,
    display_name: &str,
    registry_name: &str,
    version: &str,
    integrity: Option<&str>,
    verify_integrity: bool,
    strict_integrity: bool,
    strict_pkg_content_check: bool,
) -> miette::Result<aube_store::PackageIndex> {
    let already_verified = match (verify_integrity, streamed_sha512, integrity) {
        (true, Some(digest), Some(expected)) => {
            aube_store::verify_precomputed_sha512(digest, expected).map_err(|e| {
                miette!(
                    "{display_name}@{version}: {e}{}",
                    crate::dep_chain::format_chain_for(registry_name, version)
                )
            })?
        }
        _ => false,
    };
    import_verified_tarball(
        store,
        bytes,
        display_name,
        registry_name,
        version,
        integrity,
        verify_integrity && !already_verified,
        strict_integrity,
        strict_pkg_content_check,
    )
}

/// Fetch + import in one streaming pass. HTTP body chunks pipe through
/// SHA-512 hasher + a bounded channel into a blocking task that runs
/// gz+tar+CAS as bytes arrive. RSS bound is current tar entry size,
/// not full tarball. SHA-512 verifies AFTER import: CAS files use
/// content-addressed BLAKE3 paths so a verify mismatch leaves orphan
/// shards but no package_index referencing them.
///
/// On by default. AUBE_DISABLE_TARBALL_STREAM=1 forces the buffered
/// path. Non-SHA-512 SRI auto-falls-back since streaming verify can't
/// re-hash with another algo.
#[allow(clippy::too_many_arguments)]
/// Error from [`fetch_and_import_tarball_streaming`] that
/// preserves whether the underlying registry call hit upstream
/// backpressure (HTTP 429/502/503/504/timeout). Callers feed
/// `is_throttle` into
/// [`aube_util::adaptive::AdaptivePermit::record_throttle`] so the
/// AIMD halving path actually fires when registries push back.
/// `From<TarballStreamErr> for miette::Report` lets `?` keep
/// working at sites that don't care about the distinction.
pub(super) struct TarballStreamErr {
    pub report: miette::Report,
    pub is_throttle: bool,
}

impl From<TarballStreamErr> for miette::Report {
    fn from(e: TarballStreamErr) -> Self {
        e.report
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn fetch_and_import_tarball_streaming(
    client: &aube_registry::client::RegistryClient,
    store: &std::sync::Arc<aube_store::Store>,
    url: &str,
    display_name: &str,
    registry_name: &str,
    version: &str,
    integrity: Option<&str>,
    verify_integrity: bool,
    strict_integrity: bool,
    strict_pkg_content_check: bool,
) -> Result<(aube_store::PackageIndex, u64), TarballStreamErr> {
    use sha2::Digest;

    // Local-error helper. Anything we observe past the response
    // headers (chunk read errors are an exception, see below) is
    // either local IO, hash mismatch, or content validation —
    // none of which respond to backing off the registry, so they
    // should not trip the AIMD throttle path.
    let local = |report: miette::Report| TarballStreamErr {
        report,
        is_throttle: false,
    };
    // Network-error helper, used for chunk read errors during
    // body streaming. Connection resets and read timeouts mid-
    // body are the same kind of upstream signal as a 503 reply.
    let net = |e: aube_registry::Error, ctx: miette::Report| TarballStreamErr {
        is_throttle: e.is_throttle(),
        report: ctx,
    };

    let mut resp = client.start_tarball_stream(url).await.map_err(|e| {
        let is_throttle = e.is_throttle();
        TarballStreamErr {
            report: miette!(
                "failed to fetch {display_name}@{version}: {e}{}",
                crate::dep_chain::format_chain_for(registry_name, version)
            ),
            is_throttle,
        }
    })?;

    let cap = client.tarball_max_bytes();
    let (chunk_tx, chunk_rx) =
        tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);

    let store_for_import = store.clone();
    let display_for_import = display_name.to_string();
    let version_for_import = version.to_string();
    let registry_for_import = registry_name.to_string();
    let import_handle: tokio::task::JoinHandle<miette::Result<aube_store::PackageIndex>> =
        tokio::task::spawn_blocking(move || {
            let reader = aube_util::io::ChunkReader::new(chunk_rx);
            store_for_import.import_tarball_reader(reader).map_err(|e| {
                miette!(
                    "failed to import {display_for_import}@{version_for_import}: {e}{}",
                    crate::dep_chain::format_chain_for(&registry_for_import, &version_for_import)
                )
            })
        });

    // Hash every byte the server sent, regardless of whether the
    // import task consumed them. tar end-of-archive can fire before
    // gzip padding finishes streaming. importer drops rx, send fails,
    // but SHA-512 still has to cover the full body or partial-stream
    // SRI passes when verify is on.
    let mut hasher = sha2::Sha512::new();
    let mut total: u64 = 0;
    let mut chunk_tx = Some(chunk_tx);
    let stream_err: Option<aube_registry::Error> = loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if cap > 0 && total.saturating_add(chunk.len() as u64) > cap {
                    if let Some(tx) = chunk_tx.as_ref() {
                        let _ = tx
                            .send(Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("tarball body exceeds cap {cap}"),
                            )))
                            .await;
                    }
                    break Some(aube_registry::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("tarball body exceeds cap {cap}"),
                    )));
                }
                total += chunk.len() as u64;
                hasher.update(&chunk);
                if let Some(tx) = chunk_tx.as_ref()
                    && tx.send(Ok(chunk)).await.is_err()
                {
                    // Import task closed the channel (tar EOF hit).
                    // Drop the sender and keep draining the response
                    // so SHA-512 covers the full tarball body.
                    chunk_tx = None;
                }
            }
            Ok(None) => break None,
            Err(e) => {
                if let Some(tx) = chunk_tx.as_ref() {
                    let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                }
                break Some(aube_registry::Error::from(e));
            }
        }
    };
    drop(chunk_tx);

    let import_result = import_handle.await.into_diagnostic().map_err(local)?;
    if let Some(e) = stream_err {
        // Stash the Display rendering before `net` consumes `e`
        // for `is_throttle()` — the user-facing diagnostic must
        // still name the underlying cause (timeout, status 503,
        // connection reset). Dropping it would leave triage with
        // a bare "stream error for foo@1.2.3".
        let cause = e.to_string();
        return Err(net(
            e,
            miette!(
                "stream error for {display_name}@{version}: {cause}{}",
                crate::dep_chain::format_chain_for(registry_name, version)
            ),
        ));
    }
    let index = import_result.map_err(local)?;

    let mut sha512 = [0u8; 64];
    sha512.copy_from_slice(&hasher.finalize()[..]);

    if verify_integrity {
        if let Some(expected) = integrity {
            // Returns true on SHA-512 match, false on non-SHA-512 algo.
            // Streaming path can't fall back to re-hash with another
            // algo (no buffered bytes), so non-SHA-512 SRI bails and
            // the caller falls back to the buffered path.
            let matched =
                aube_store::verify_precomputed_sha512(&sha512, expected).map_err(|e| {
                    local(miette!(
                        "{display_name}@{version}: {e}{}",
                        crate::dep_chain::format_chain_for(registry_name, version)
                    ))
                })?;
            if !matched {
                return Err(local(miette!(
                    "{display_name}@{version}: SRI uses non-SHA-512 algo, streaming path cannot re-hash. Set AUBE_DISABLE_TARBALL_STREAM=1 to force buffered fetch{}",
                    crate::dep_chain::format_chain_for(registry_name, version)
                )));
            }
        } else if strict_integrity {
            return Err(local(miette!(
                "{display_name}@{version}: registry response has no `dist.integrity` and `strict-store-integrity` is on. Refusing to import unverified bytes.{}",
                crate::dep_chain::format_chain_for(registry_name, version)
            )));
        } else {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_MISSING_INTEGRITY,
                "{display_name}@{version}: registry response has no `dist.integrity`, importing without content verification. Set `strict-store-integrity=true` to refuse instead."
            );
        }
    }

    if strict_pkg_content_check {
        aube_store::validate_pkg_content(&index, registry_name, version).map_err(|e| {
            local(miette!(
                "{display_name}@{version}: {e}{}",
                crate::dep_chain::format_chain_for(registry_name, version)
            ))
        })?;
    }

    if let Err(e) = store.save_index(registry_name, version, integrity, &index) {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_CACHE_WRITE_FAILED,
            "Failed to cache index for {display_name}@{version}: {e}"
        );
    }

    Ok((index, total))
}

pub(super) fn validate_required_scripts(
    project_dir: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    required: &[String],
) -> miette::Result<()> {
    if required.is_empty() {
        return Ok(());
    }
    let mut missing = Vec::new();
    collect_missing_required_scripts(".", manifest, required, &mut missing);
    for pkg_dir in aube_workspace::find_workspace_packages(project_dir)
        .map_err(|e| miette!("failed to discover workspace packages: {e}"))?
    {
        let manifest_path = pkg_dir.join("package.json");
        let pkg_manifest = aube_manifest::PackageJson::from_path(&manifest_path)
            .map_err(miette::Report::new)
            .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?;
        let label = pkg_manifest
            .name
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| {
                pkg_dir
                    .strip_prefix(project_dir)
                    .unwrap_or(&pkg_dir)
                    .display()
                    .to_string()
            });
        collect_missing_required_scripts(&label, &pkg_manifest, required, &mut missing);
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(miette!(
            "requiredScripts check failed:\n{}",
            missing
                .into_iter()
                .map(|(pkg, script)| format!("  - {pkg} is missing `{script}`"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

fn collect_missing_required_scripts(
    label: &str,
    manifest: &aube_manifest::PackageJson,
    required: &[String],
    missing: &mut Vec<(String, String)>,
) {
    for script in required {
        if !manifest.scripts.contains_key(script) {
            missing.push((label.to_string(), script.clone()));
        }
    }
}

/// One dependency whose build scripts were skipped because it's not
/// on the `allowBuilds` allowlist. `suspicions` is the result of
/// running the content sniff against the dep's lifecycle script
/// bodies; empty when the scripts looked clean (the common case).
/// The sniff is derived from the live materialized tree and not
/// persisted to install state.
#[derive(Debug, Clone)]
pub(in crate::commands::install) struct UnreviewedBuild {
    pub spec_key: String,
    pub suspicions: Vec<aube_scripts::Suspicion>,
}

pub(super) fn unreviewed_dep_builds(
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    policy: &aube_scripts::BuildPolicy,
    floor: &super::default_trust::DefaultTrustFloor,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> miette::Result<Vec<UnreviewedBuild>> {
    let mut unreviewed = Vec::new();
    for (dep_path, pkg) in &graph.packages {
        // A package the `defaultTrust` floor vouches for is not
        // unreviewed — its scripts ran. Same decision seam as
        // `run_dep_lifecycle_scripts` so the warning and the runner
        // can never disagree about a package's status.
        if !matches!(
            super::default_trust::decide_with_floor(policy, floor, pkg, &graph.times),
            aube_scripts::AllowDecision::Unspecified
        ) {
            continue;
        }
        let package_dir = materialized_pkg_dir(
            aube_dir,
            dep_path,
            &pkg.name,
            virtual_store_dir_max_length,
            placements,
        );
        if !package_dir.exists() {
            continue;
        }
        let pkg_json_path = package_dir.join("package.json");
        let pkg_json_content = match std::fs::read_to_string(&pkg_json_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(miette!(
                    "failed to read package.json for {} at {}: {}",
                    pkg.name,
                    pkg_json_path.display(),
                    e
                ));
            }
        };
        let dep_manifest = aube_manifest::PackageJson::parse(&pkg_json_path, pkg_json_content)
            .map_err(miette::Report::new)
            .wrap_err_with(|| {
                format!(
                    "failed to parse package.json for {}{}",
                    pkg.name,
                    crate::dep_chain::format_chain_for(&pkg.name, &pkg.version)
                )
            })?;
        if aube_scripts::has_dep_lifecycle_work(&package_dir, &dep_manifest) {
            unreviewed.push(UnreviewedBuild {
                spec_key: pkg.source_approval_key().unwrap_or_else(|| pkg.spec_key()),
                suspicions: aube_scripts::sniff_lifecycle(&dep_manifest),
            });
        }
    }
    unreviewed.sort_by(|a, b| a.spec_key.cmp(&b.spec_key));
    unreviewed.dedup_by(|a, b| a.spec_key == b.spec_key);
    Ok(unreviewed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_allow_build_conflict_denies() {
        let allow_manifest = manifest_with_allow_build("native-dep", true);
        let deny_manifest = manifest_with_allow_build("native-dep", false);
        let workspace = aube_manifest::WorkspaceConfig::default();
        let (policy, warnings) = build_policy_from_manifest_sources(
            [&allow_manifest, &deny_manifest],
            &workspace,
            false,
        );

        assert!(warnings.is_empty());
        assert_eq!(
            policy.decide("native-dep", "1.0.0"),
            aube_scripts::AllowDecision::Deny
        );
    }

    #[test]
    fn dependencies_meta_built_false_denies_even_when_allow_listed() {
        // `dependenciesMeta.<pkg>.built: false` is a deny that must beat
        // an explicit allow-build, identical to a `neverBuiltDependencies`
        // entry — proving it routes into the denylist, not just leaving
        // the package Unspecified.
        let mut manifest = manifest_with_allow_build("native-dep", true);
        let mut meta = serde_json::Map::new();
        let mut entry = serde_json::Map::new();
        entry.insert("built".to_string(), serde_json::Value::Bool(false));
        meta.insert("native-dep".to_string(), serde_json::Value::Object(entry));
        manifest.extra.insert(
            "dependenciesMeta".to_string(),
            serde_json::Value::Object(meta),
        );

        let workspace = aube_manifest::WorkspaceConfig::default();
        let (policy, warnings) = build_policy_from_sources(&manifest, &workspace, false);

        assert!(warnings.is_empty());
        assert_eq!(
            policy.decide("native-dep", "1.0.0"),
            aube_scripts::AllowDecision::Deny
        );
    }

    fn manifest_with_allow_build(name: &str, allow: bool) -> aube_manifest::PackageJson {
        let mut pnpm = serde_json::Map::new();
        let mut allow_builds = serde_json::Map::new();
        allow_builds.insert(name.to_string(), serde_json::Value::Bool(allow));
        pnpm.insert(
            "allowBuilds".to_string(),
            serde_json::Value::Object(allow_builds),
        );

        let mut manifest = aube_manifest::PackageJson::default();
        manifest
            .extra
            .insert("pnpm".to_string(), serde_json::Value::Object(pnpm));
        manifest
    }
}
