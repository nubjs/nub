use miette::{Context, IntoDiagnostic, miette};

use super::bin_linking::materialized_pkg_dir;
use super::node_gyp_bootstrap;
use super::side_effects_cache::{
    Confinement, CopyMode, SideEffectsCacheConfig, SideEffectsCacheEntry, SideEffectsCacheRestore,
};

/// Run a root-package lifecycle hook, announcing it to the user if defined
/// and turning aube_scripts::Error into a miette::Report with context.
/// Silent when the hook isn't defined in package.json.
pub(super) async fn run_root_lifecycle(
    project_dir: &std::path::Path,
    modules_dir_name: &str,
    manifest: &aube_manifest::PackageJson,
    hook: aube_scripts::LifecycleHook,
    provenance: aube_scripts::RootProvenance<'_>,
) -> miette::Result<()> {
    // Only announce when the hook is actually defined, so projects without
    // lifecycle scripts don't get noise in their install output.
    if !manifest.scripts.contains_key(hook.script_name()) {
        return Ok(());
    }
    tracing::debug!("Running {} script...", hook.script_name());
    aube_scripts::run_root_hook(project_dir, modules_dir_name, manifest, hook, provenance)
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

/// Whether aube's OWN build jail engages. `paranoid`/`jailBuilds` request it, but an
/// embedder that owns lifecycle confinement (`embedder_owns_lifecycle_sandbox`)
/// suppresses it unconditionally — it interposes its own sandbox instead, and a user
/// setting must not be able to swap that back to aube's jail. Pure so the gate is
/// unit-testable without a settings `ResolveCtx` or the process-global embedder.
fn jail_enabled(embedder_owns_lifecycle_sandbox: bool, jail_builds: bool, paranoid: bool) -> bool {
    !embedder_owns_lifecycle_sandbox && (jail_builds || paranoid)
}

/// Whether this package's build will run confined, asked at PLANNING time.
///
/// The side-effects cache needs it before anything spawns, because it decides whether to
/// RESTORE — and a restore is exactly what skips the spawn. Mirrors the two mechanisms
/// `run_dep_hook` chooses between: aube's own `ScriptJail`, and an embedder that owns
/// confinement via the `EngineContext::lifecycle_sandbox` hook. They are mutually
/// exclusive — `jail_enabled` gates aube's off whenever the embedder owns it — so either
/// one confining is the answer.
///
/// `would_confine` rather than `confines`: this runs for packages that may never spawn,
/// and the spawn-time call is the one allowed to announce.
fn dep_confinement(
    jail_policy: &JailBuildPolicy,
    name: &str,
    version: &str,
    source_key: Option<&str>,
    git_repository_key: Option<&str>,
    package_name: Option<&str>,
    project_dir: &std::path::Path,
) -> Confinement {
    // The version reaches the hook only alongside a name: it is the same identity, and an
    // embedder scoping a per-package policy by version must not be handed one for a package
    // whose name was withheld.
    let package_version = package_name.map(|_| version);
    let embedder_confines = aube_util::embedder().embedder_owns_lifecycle_sandbox
        && aube_util::engine_context()
            .lifecycle_sandbox
            .as_ref()
            .is_some_and(|hook| hook.would_confine(package_name, package_version, project_dir));
    if embedder_confines || jail_policy.should_jail(name, version, source_key, git_repository_key) {
        Confinement::Confined
    } else {
        Confinement::Unconfined
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
        // `paranoid=true` forces the jail on regardless of `jailBuilds`. But when the
        // EMBEDDER owns lifecycle-script confinement, aube's own build jail never
        // engages — the embedder interposes its own sandbox via the
        // `EngineContext::lifecycle_sandbox` hook — so a user `jailBuilds`/`paranoid`
        // cannot swap the choice back to aube's jail. Default-preserving: standalone
        // aube's flag is `false`, leaving this decision byte-for-byte unchanged.
        let enabled = jail_enabled(
            aube_util::embedder().embedder_owns_lifecycle_sandbox,
            aube_settings::resolved::jail_builds(ctx),
            aube_settings::resolved::paranoid(ctx),
        );
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

    fn should_jail(
        &self,
        name: &str,
        version: &str,
        source_key: Option<&str>,
        git_repository_key: Option<&str>,
    ) -> bool {
        self.enabled
            && !matches!(
                self.denylist.decide_package_with_git_repository(
                    name,
                    version,
                    source_key,
                    git_repository_key,
                ),
                aube_scripts::AllowDecision::Deny
            )
    }

    fn jail_for(
        &self,
        name: &str,
        version: &str,
        source_key: Option<&str>,
        git_repository_key: Option<&str>,
        package_dir: &std::path::Path,
        project_dir: &std::path::Path,
    ) -> Option<aube_scripts::ScriptJail> {
        if !self.should_jail(name, version, source_key, git_repository_key) {
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

/// On Windows, a confined lifecycle script cannot read a HARD-LINKED file, so
/// the embedder's build jail forces per-file copy.
///
/// Windows attaches the security descriptor to the file OBJECT, not to the
/// directory entry, so a store file hardlinked into the install keeps the
/// descriptor it was created with under the content-addressed store. MEASURED on
/// `windows-latest` (build-jail-corpus run 31126235717): the denied `install.js`
/// carried `SYSTEM`/`Administrators`/`<user>` full control and NOT ONE ace marked
/// `(I)`, while `fsutil hardlink list` named four entries for that single object,
/// two of them inside the CAS. A descriptor that never auto-inherited is one
/// advapi32's inheritance-propagation pass also skips, so the jail's inheritable
/// grant lands on every sibling the linker CREATED and never on the linked one.
/// Node starts, enters its own ESM loader, and dies at `getSourceSync` with
/// `EPERM` — which is why win32 produced no corpus records at all.
///
/// COPY IS THE ONLY FIX THAT DOES NOT WIDEN THE SHARED STORE. The alternative —
/// acing the file object — rewrites the descriptor EVERY other name for it sees,
/// including the machine-global CAS entry every project on the machine hardlinks
/// from; for a private registry that is source disclosure between packages. A
/// copied file is created fresh inside the granted directory, which restores the
/// create-then-inherit invariant the whole Windows grant path is built around
/// (`nub_sandbox`'s `windows_publish_appcontainer_read`: grant the EMPTY
/// directory, then populate it).
///
/// It overrides an explicit `packageImportMethod=hardlink` deliberately: that
/// combination does not install at all here, so honoring the setting would only
/// choose which way to break.
///
/// Pure, so the rule is unit-testable off Windows — same reason [`jail_enabled`]
/// is.
fn jail_forces_copy(
    strategy: aube_linker::LinkStrategy,
    windows: bool,
    embedder_confines: bool,
) -> aube_linker::LinkStrategy {
    if windows && embedder_confines {
        aube_linker::LinkStrategy::Copy
    } else {
        strategy
    }
}

/// Whether the embedder's lifecycle sandbox will confine anything in this
/// install, asked with no package identity because the strategy is resolved once
/// for the whole link phase. Mirrors [`dep_confinement`]'s gate; aube's own
/// `ScriptJail` is not consulted because it spawns into a `bwrap`/Seatbelt
/// namespace rather than an AppContainer, and neither carries per-file ACLs.
fn embedder_confines_any(project_dir: &std::path::Path) -> bool {
    aube_util::embedder().embedder_owns_lifecycle_sandbox
        && aube_util::engine_context()
            .lifecycle_sandbox
            .as_ref()
            .is_some_and(|hook| hook.would_confine(None, None, project_dir))
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
        // pointed `storeDir` and `cacheDir` at different volumes — a
        // real misconfiguration that costs per-file copies. Warn.
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
                    code = aube_codes::warnings::WARN_AUBE_GVS_CROSS_VOLUME,
                    store = %sd.display(),
                    gvs_dir = %probe_dst.display(),
                    "global virtual store dir is on a different volume than `storeDir`; \
                     install will fall back to per-file copy. Move the two onto one \
                     volume with `globalVirtualStoreDir` \
                     (`AUBE_GLOBAL_VIRTUAL_STORE_DIR`, virtual store only), \
                     `cacheDir` (`AUBE_CACHE_DIR`, the whole cache), or `storeDir` \
                     (`AUBE_STORE_DIR`)."
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
    Ok(jail_forces_copy(
        strategy,
        cfg!(windows),
        embedder_confines_any(cwd),
    ))
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
    // Whether `project_dir` is the USER's own project root rather than a checkout aube
    // fetched. `run_git_dep_prepare` points a nested install's `project_dir` at the
    // clone dir, so on that path `project_dir` is attacker-authored — and an embedder
    // that keys per-package confinement policy off the root manifest must not read it.
    // Derived from the same `RootProvenance` the root-script exemption uses.
    root_is_user_authored: bool,
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
        git_repository_key: Option<String>,
        package_dir: std::path::PathBuf,
        manifest: aube_manifest::PackageJson,
        cache_entry: Option<SideEffectsCacheEntry>,
        /// Graph key, kept so the optional-only classification below resolves
        /// per job without re-walking the graph.
        dep_path: String,
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
            let git_repository_key = pkg.git_repository_approval_key();
            match policy.decide_package_with_git_repository(
                pkg.registry_name(),
                &pkg.version,
                source_key.as_deref(),
                git_repository_key.as_deref(),
            ) {
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
        // The SAME identity `run_dep_hook` will hand the confinement hook below, so the
        // key and the spawn cannot disagree about which package this is — including the
        // withholding under a fetched root, which is what makes a checkout's builds key
        // as confined.
        let confinement = dep_confinement(
            &jail_policy,
            pkg.registry_name(),
            &pkg.version,
            pkg.source_approval_key().as_deref(),
            pkg.git_repository_approval_key().as_deref(),
            root_is_user_authored.then_some(pkg.registry_name()),
            project_dir,
        );
        let cache_entry = side_effects_cache
            .location()
            .map(|loc| {
                SideEffectsCacheEntry::new(loc, &pkg.name, &pkg.version, &package_dir, confinement)
            })
            .transpose()?;
        if via_floor {
            floor_trusted.push(pkg.spec_key());
        }
        jobs.push(BuildJob {
            name: pkg.name.clone(),
            registry_name: pkg.registry_name().to_string(),
            version: pkg.version.clone(),
            source_key: pkg.source_approval_key(),
            git_repository_key: pkg.git_repository_approval_key(),
            package_dir,
            manifest: dep_manifest,
            cache_entry,
            dep_path: dep_path.clone(),
        });
    }

    if jobs.is_empty() {
        return Ok(0);
    }

    // A package reachable ONLY through `optionalDependencies` is one the
    // project declared it can live without, so a failed build for it is
    // non-fatal — npm (`_handleOptionalFailure` during reify) and pnpm
    // (`buildDependency`'s catch, which logs `reason: 'build_failure'` and
    // returns) both continue the install. Computed from the graph's edges
    // rather than read off `LockedPackage::optional`, which only the pnpm
    // reader and the fresh-resolve pass populate; reading the field would make
    // this silently inert on a frozen install off an npm/bun/yarn lockfile.
    // A package with even one fully-required path stays required.
    let optional_only = aube_resolver::platform::optional_only_packages(graph);

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

    // Hand the fan-out a *lazy* shim rather than bootstrapping node-gyp
    // up front. We can't cheaply predict which jobs will shell out to
    // node-gyp (explicit, implicit via binding.gyp, or transitive via
    // node-gyp-build) — but bootstrapping eagerly made every approved
    // build pay for the fetch, and turned an unreachable registry into a
    // failed install even with a warm store and nothing in the graph
    // that wants node-gyp. The shim defers the fetch to first actual
    // invocation; `ensure_cached` still takes the tool dir's own project
    // lock and re-checks under it, so a parallel fan-out converges on
    // one bootstrap. `None` means node-gyp already resolves (project
    // `.bin`, system install, nvm, a test shim) — leave that copy alone.
    //
    // A CONFINED job is the exception and must resolve eagerly: confinement clears
    // the environment and substitutes a temporary HOME, so the shim's re-entry
    // would look for the tool dir under that HOME, find nothing, and be unable
    // to refill it because the sandbox denies network. Warming the real cache
    // first does not help — the re-entry never looks there. Resolving out here
    // hands the confined script a directly executable node-gyp instead.
    //
    // Asked through `dep_confinement`, not `jail_policy` alone: a job confined by
    // an embedder that OWNS the sandbox is just as unable to re-enter, and that
    // path never consults the jail policy.
    //
    // Best-effort by design: a bootstrap failure must not sink an install whose
    // builds never touch node-gyp. One that does still fails, with node-gyp's
    // own error rather than this one.
    let project_bin_dir = project_dir.join(modules_dir_name).join(".bin");
    let any_confined = jobs.iter().any(|job| {
        matches!(
            dep_confinement(
                &jail_policy,
                &job.registry_name,
                &job.version,
                job.source_key.as_deref(),
                job.git_repository_key.as_deref(),
                root_is_user_authored.then_some(job.registry_name.as_str()),
                project_dir,
            ),
            Confinement::Confined
        )
    });
    let node_gyp_bin_dir = std::sync::Arc::new(if any_confined {
        match node_gyp_bootstrap::ensure_bin_dir_for_jail(&project_bin_dir, project_dir).await {
            Ok(dir) => dir,
            Err(err) => {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_NODE_GYP_BOOTSTRAP_FAILED,
                    "could not prepare node-gyp for confined builds: {err:#}"
                );
                None
            }
        }
    } else {
        node_gyp_bootstrap::lazy_shim_bin_dir(&project_bin_dir)?
    });

    // Pass 2 (parallel, bounded): fan out across `child_concurrency`
    // concurrent workers. Inside one job the three hooks
    // (preinstall → install → postinstall) still run sequentially —
    // pnpm's execution model is "at most N packages building in
    // parallel," not "at most N scripts running," so hook ordering
    // within a single package is preserved.
    //
    // FAILURE SEMANTICS: a failing script fails the install (pnpm/npm parity),
    // but it does NOT tear its siblings down mid-write. On the first failure
    // `failed` is raised, so jobs still queued behind the semaphore return
    // without starting a build that would be discarded anyway; jobs already
    // RUNNING drain to completion, and the first error is returned once the set
    // is empty.
    //
    // A predecessor returned from the `join_next` loop on the first error, which
    // dropped the `JoinSet` and aborted every outstanding task. Its stated
    // justification — that siblings are "aborted before they can scribble on
    // disk" — was false on POSIX: the abort SIGKILLs each sibling's shell and
    // nothing else, so `node-gyp`/`make`/`cc` reparent to init and keep writing
    // into `node_modules` long after the install returns. It bought no
    // containment and cost determinism: whether a package finished depended on
    // which UNRELATED package in the same batch failed first, and on whether its
    // own orphaned compiler happened to finish before anything looked. Measured
    // on a 50-package corpus shard, 25 of 27 packages that left no build
    // artifact in-batch left one when run as the batch's only package.
    //
    // Draining is also what pnpm does, structurally: a rejected promise cannot
    // cancel an already-spawned child process, so its in-flight builds run to
    // completion and only the install's exit status carries the failure.
    // `JoinSet` is still the right container — dropping a `Vec<JoinHandle>`
    // detaches rather than cancels, so a panic path would leak live tasks.
    //
    // ACCEPTED COST: the abort was also the only bound on a hung sibling. aube
    // has no lifecycle-script timeout, so a dependency whose script never exits
    // now hangs the install after an unrelated failure instead of being killed
    // on the way out. Taken deliberately — the abort's "bound" was killing a
    // shell whose compilers kept running anyway, so it bounded the process, not
    // the machine — and pnpm has the same property. A real fix is a per-script
    // timeout, which is a separate change.
    let concurrency = child_concurrency.max(1);
    let failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let project_dir = project_dir.to_path_buf();
    let modules_dir_name = modules_dir_name.to_string();
    let should_restore_side_effects_cache = side_effects_cache.should_restore();
    // THE SECOND SITE OF THE SAME WINDOWS DEFECT [`jail_forces_copy`] covers for the linker:
    // a restored tree hardlinked out of the cache carries the CACHE entry's security
    // descriptor, which the jail's inheritable grant on the package dir never reaches — so a
    // LATER package's confined script cannot read a dependency that was restored rather than
    // built. Restoring by copy costs the bytes and keeps the tree private to this install.
    let restore_mode = if cfg!(windows) && embedder_confines_any(&project_dir) {
        CopyMode::Copy
    } else {
        CopyMode::HardlinkOrCopy
    };
    let should_save_side_effects_cache = side_effects_cache.should_save();
    let overwrite_side_effects_cache = side_effects_cache.overwrite_existing();
    let jail_policy = std::sync::Arc::new((*jail_policy).clone());
    // `(optional, spec, outcome)` rather than a bare result: the drain loop has
    // to know whether the package that failed was optional-only, and an error
    // surfacing from `join_next` carries no identity of its own.
    let mut set: tokio::task::JoinSet<(bool, String, miette::Result<usize>)> =
        tokio::task::JoinSet::new();
    for job in jobs {
        let sem = semaphore.clone();
        let project_dir = project_dir.clone();
        let modules_dir_name = modules_dir_name.clone();
        let node_gyp_bin_dir = node_gyp_bin_dir.clone();
        let jail_policy = jail_policy.clone();
        let failed = failed.clone();
        let job_optional = optional_only.contains(&job.dep_path);
        let job_spec = format!("{}@{}", job.name, job.version);
        let task = crate::dep_chain::scope_current(async move {
            let _permit = sem.acquire().await.unwrap();
            if should_restore_side_effects_cache && let Some(cache_entry) = job.cache_entry.clone()
            {
                let package_dir = job.package_dir.clone();
                let restore_result = tokio::task::spawn_blocking(move || {
                    cache_entry.restore_if_available(&package_dir, restore_mode)
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
            // Placed BELOW the side-effects-cache restore and above the build:
            // this is the moment a queued job would otherwise start compiling.
            // The install is already failing, and starting a build whose result
            // is about to be discarded only adds half-written package dirs — but
            // a cache RESTORE is a directory copy, not a build, and skipping it
            // would cost the user that work again on the retry.
            if failed.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(0);
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
                job.git_repository_key.as_deref(),
                &job.package_dir,
                &project_dir,
            );
            let _jail_home_cleanup = jail.as_ref().map(aube_scripts::ScriptJailHomeCleanup::new);
            let mut ran_here = 0usize;
            for hook in aube_scripts::DEP_LIFECYCLE_HOOKS {
                let did_run = aube_scripts::run_dep_hook(
                    &job.package_dir,
                    &project_dir,
                    &modules_dir_name,
                    &job.manifest,
                    hook,
                    &tool_dirs,
                    jail.as_ref(),
                    // The same identity `BuildPolicy`/`jail_for` decide on, so an
                    // embedder's per-package confinement policy and aube's own
                    // allow/deny rules cannot disagree about which package this is.
                    // Withheld entirely under a fetched root: there `project_dir` is the
                    // checkout, so handing over a name would invite the embedder to look
                    // this package up in a manifest the dependency wrote.
                    root_is_user_authored.then_some(job.registry_name.as_str()),
                    root_is_user_authored.then_some(job.version.as_str()),
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
            if ran_here > 0 {
                // A dep build writes its output in place — `node-gyp` emits
                // `build/Release/*.node` right here — and those inodes are
                // created by child processes that inherited this one's
                // quarantine flags. A locally built addon is ad-hoc signed at
                // best, so Gatekeeper refuses to load it, and without this the
                // install that *built* the addon ships it unusable while only a
                // later cache restore heals it.
                //
                // This does NOT make the cache entry saved below clean: `save`
                // copies with `CopyMode::Copy`, so `fs::copy` mints inodes the
                // kernel stamps again regardless of the now-clean source (route
                // 2 in `aube-linker`'s module doc). The restore-side strip is
                // what covers that, and remains load-bearing.
                //
                // Off the async worker like every other filesystem step here: a
                // node-gyp `build/` tree can hold thousands of files, and this
                // walks all of them. Best-effort, so a join error is ignored
                // rather than failing an otherwise-complete build.
                let package_dir = job.package_dir.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    aube_linker::strip_quarantine_from_tree(&package_dir)
                })
                .await;
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
        let task = crate::runtime::scope_current(task);
        let task = aube_scripts::scope_current(task);
        set.spawn(async move { (job_optional, job_spec, task.await) });
    }

    let mut ran = 0usize;
    let mut first_error: Option<miette::Report> = None;
    while let Some(res) = set.join_next().await {
        // A `JoinError` here is a task-level panic — an aube bug, not a package
        // whose build failed — so it stays fatal even for an optional package.
        // Errors are recorded rather than returned so the loop keeps draining:
        // an early `return` is what used to abort the siblings.
        let (optional, spec, outcome) = match res.into_diagnostic() {
            Ok(joined) => joined,
            Err(panic) => {
                if first_error.is_none() {
                    failed.store(true, std::sync::atomic::Ordering::Relaxed);
                    first_error = Some(panic);
                }
                continue;
            }
        };
        match outcome {
            Ok(count) => ran += count,
            // An optional-only package's build failure never raises `failed` and
            // never becomes the install's error, so siblings queued behind the
            // semaphore still run. Warned per package rather than tallied at the
            // end: a skip is rare, and a missing native addon that surfaces later
            // as a runtime import error is miserable to trace back without the
            // build error that caused it. (pnpm logs the same event at debug,
            // where its default reporter never shows it.)
            Err(error) if optional => {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_OPTIONAL_BUILD_FAILED,
                    "{spec} is an optional dependency and failed to build; continuing without it: {error}"
                );
            }
            Err(error) => {
                if first_error.is_none() {
                    // Raised before the report is stashed so the not-yet-started
                    // jobs see it as early as possible.
                    failed.store(true, std::sync::atomic::Ordering::Relaxed);
                    first_error = Some(error);
                }
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(ran),
    }
}

/// Persist a freshly imported package index under the store key(s) a
/// later warm install will read it back by. Shared between the buffered
/// and streaming import paths so both key the cache identically.
///
/// `lockfile_integrity` is the SRI the resolver carried from the
/// lockfile (`None` for a v1/legacy entry or an integrity-stripping
/// proxy); `computed_integrity` is the sha512 the streaming path
/// derived from the tarball bytes when the lockfile carried none. The
/// index is saved under the effective key
/// (`lockfile_integrity.or(computed_integrity)`).
///
/// No content-FREE root-key (`None`) write happens here — ever. The
/// index is cached ONLY when there's an integrity to key it by; a
/// no-integrity package lands under its computed-sha512 hex key, and the
/// warm classifier reaches it by content-addressing through the global
/// URL-keyed no-integrity binding (`state::read_no_integrity_index_for`) —
/// not a bare `<name>@<version>` selector that, in a per-user shared
/// store, would let one project's bytes be served to another for the
/// same coordinate. (A predecessor wrote both keys; that root-key write
/// opened exactly that cross-project substitution surface.) The hex
/// write is kept — both the content-addressed warm read and the
/// v3-self-heal upgrade resolve through it.
///
/// When BOTH integrities are `None` (the non-default buffered import of
/// a no-integrity package — `DISABLE_TARBALL_STREAM=1`; the streaming
/// default always computes a sha512), nothing is cached: writing the
/// bare key would reopen the surface, and the binding the warm read
/// needs comes from the streaming path. The buffered warm install
/// re-fetches instead, which is the pre-existing trade-off for that mode.
fn persist_pkg_index(
    store: &aube_store::Store,
    registry_name: &str,
    version: &str,
    lockfile_integrity: Option<&str>,
    computed_integrity: Option<&str>,
    index: &aube_store::PackageIndex,
    display_name: &str,
) {
    let Some(effective) = lockfile_integrity.or(computed_integrity) else {
        return;
    };
    if let Err(e) = store.save_index(registry_name, version, Some(effective), index) {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_CACHE_WRITE_FAILED,
            "Failed to cache index for {display_name}@{version}: {e}"
        );
    }
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
    // fetches, keyed by the effective integrity (`+<hex>` subdir) so
    // same-(name, version) tarballs from different sources stay
    // discriminated. The buffered path derives no computed integrity, so
    // a no-integrity entry isn't cached here (see `persist_pkg_index`);
    // the streaming default computes the sha512 the warm read needs.
    persist_pkg_index(
        store,
        registry_name,
        version,
        integrity,
        None,
        &index,
        display_name,
    );
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
) -> Result<(aube_store::PackageIndex, u64, Option<String>), TarballStreamErr> {
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
    let computed_integrity = integrity
        .is_none()
        .then(|| aube_store::sha512_integrity_from_digest(&sha512));

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

    persist_pkg_index(
        store,
        registry_name,
        version,
        integrity,
        computed_integrity.as_deref(),
        &index,
        display_name,
    );

    Ok((index, total, computed_integrity))
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
        // can never disagree about a package's status; the seam itself
        // consults the source / git-repository approval keys.
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
    fn embedder_owned_lifecycle_sandbox_suppresses_aubes_jail() {
        // Standalone aube (flag false): `jailBuilds`/`paranoid` engage aube's own jail
        // exactly as before — byte-for-byte default behavior.
        assert!(
            jail_enabled(false, true, false),
            "jailBuilds engages the jail"
        );
        assert!(
            jail_enabled(false, false, true),
            "paranoid engages the jail"
        );
        assert!(!jail_enabled(false, false, false), "neither set → no jail");
        // Embedder-owned confinement (flag true, nub): aube's jail NEVER engages, even
        // when a user (or a compat project's .npmrc) sets jailBuilds/paranoid — the
        // embedder interposes its own build-jail instead.
        assert!(!jail_enabled(true, true, true));
        assert!(!jail_enabled(true, true, false));
        assert!(!jail_enabled(true, false, true));
        assert!(!jail_enabled(true, false, false));
    }

    #[test]
    fn the_windows_build_jail_forces_copy_over_every_linking_strategy() {
        use aube_linker::LinkStrategy::{Copy, Hardlink, Reflink, ReflinkAuto};
        // The defect: a hardlink into the jail keeps the CAS file object's descriptor, so
        // the confined script cannot read its own entry point. Reflink is downgraded for
        // the same reason — `ReflinkAuto` falls back to `hard_link` on a filesystem that
        // refuses the clone, which is exactly NTFS.
        for probed in [Hardlink, ReflinkAuto, Reflink, Copy] {
            assert_eq!(
                jail_forces_copy(probed, true, true),
                Copy,
                "windows + a confining embedder must copy, whatever {probed:?} was probed"
            );
        }
        // Neither half alone changes anything: the other platforms' jails carry no per-file
        // ACLs, and an unconfined Windows install keeps the hardlink fast path.
        assert_eq!(jail_forces_copy(Hardlink, false, true), Hardlink);
        assert_eq!(jail_forces_copy(Hardlink, true, false), Hardlink);
        assert_eq!(jail_forces_copy(ReflinkAuto, false, false), ReflinkAuto);
    }

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

    #[test]
    fn no_integrity_import_keys_only_by_computed_hex_never_root() {
        use flate2::write::GzEncoder;
        use std::io::Write;

        // A minimal gzipped tarball, so the imported index round-trips
        // through save/load exactly like a real package's would.
        let build_tgz = || {
            let mut builder = tar::Builder::new(Vec::new());
            let manifest = br#"{"name":"legacy-pkg","version":"1.0.0"}"#;
            let mut header = tar::Header::new_gnu();
            header.set_size(manifest.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "package/package.json", &manifest[..])
                .unwrap();
            let tar_bytes = builder.into_inner().unwrap();
            let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(&tar_bytes).unwrap();
            encoder.finish().unwrap()
        };

        let name = "legacy-pkg";
        let version = "1.0.0";
        // Two distinct, valid SRIs — keys, not verified against bytes;
        // save/load only routes by them.
        let computed = aube_store::sha512_integrity_from_digest(&[0xAB; 64]);
        let lockfile_sri = aube_store::sha512_integrity_from_digest(&[0xCD; 64]);

        // Legacy/v1 entry: no lockfile integrity, only a computed sha512.
        let dir = tempfile::tempdir().unwrap();
        let store = aube_store::Store::at(dir.path().join("files"));
        let index = store.import_tarball(&build_tgz()).unwrap();
        persist_pkg_index(&store, name, version, None, Some(&computed), &index, name);

        // Option B: the no-integrity import is content-addressed by its
        // computed-sha512 hex key. The warm classifier reaches it through
        // the per-project no-integrity index, NOT a content-free root
        // selector — so NO `None`-key entry may exist in the shared store
        // (that selector is the cross-project substitution surface).
        assert!(
            store.load_index(name, version, Some(&computed)).is_some(),
            "no-integrity import must be retrievable by its computed-sha512 hex key"
        );
        assert!(
            store.load_index(name, version, None).is_none(),
            "no content-free root-key selector may be written to the shared store"
        );

        // CONTROL: an integrity-bearing entry is keyed solely by its SRI
        // and must NOT be saved under the root key — otherwise two
        // sources for the same (name, version) would alias on disk. A
        // fresh store isolates it from the legacy entry above.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = aube_store::Store::at(dir2.path().join("files"));
        let index2 = store2.import_tarball(&build_tgz()).unwrap();
        persist_pkg_index(
            &store2,
            name,
            version,
            Some(&lockfile_sri),
            Some(&computed),
            &index2,
            name,
        );
        assert!(
            store2.load_index(name, version, None).is_none(),
            "integrity-bearing entry must not be dual-saved under the root key"
        );
        assert!(
            store2
                .load_index(name, version, Some(&lockfile_sri))
                .is_some(),
            "integrity-bearing entry must resolve by its lockfile SRI"
        );

        // CONTROL: a buffered no-integrity import (both integrities None —
        // the `DISABLE_TARBALL_STREAM=1` path) caches NOTHING. Writing the
        // bare key would reopen the content-free shared selector.
        let dir3 = tempfile::tempdir().unwrap();
        let store3 = aube_store::Store::at(dir3.path().join("files"));
        let index3 = store3.import_tarball(&build_tgz()).unwrap();
        persist_pkg_index(&store3, name, version, None, None, &index3, name);
        assert!(
            store3.load_index(name, version, None).is_none(),
            "buffered no-integrity import must not write a content-free root selector"
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
