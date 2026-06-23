use super::{DepSelection, FrozenMode, FrozenOverride, GlobalVirtualStoreFlags};

#[derive(Debug, clap::Args)]
pub struct InstallArgs {
    /// Install only devDependencies
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,
    /// Skip devDependencies; install only production deps
    #[arg(short = 'P', long, visible_alias = "production")]
    pub prod: bool,
    /// Allow every dependency's lifecycle scripts to run.
    ///
    /// Bypasses the `allowBuilds` allowlist. Do not use in CI.
    #[arg(long)]
    pub dangerously_allow_all_builds: bool,
    /// Re-resolve lockfile entries whose spec drifted from package.json.
    ///
    /// Leaves everything else pinned at its locked version. Unchanged
    /// specs keep their existing version and integrity hash; only
    /// drifted entries (and any new transitives they pull in) get
    /// re-resolved.
    #[arg(long, conflicts_with_all = ["frozen_lockfile", "no_frozen_lockfile", "prefer_frozen_lockfile"])]
    pub fix_lockfile: bool,
    /// Force reinstall, ignoring lockfile/state freshness.
    ///
    /// Bypasses the `node_modules/.aube-state` freshness check and
    /// re-resolves the lockfile even when nothing has drifted. Mirrors
    /// pnpm's `install --force`.
    #[arg(long)]
    pub force: bool,
    /// Add a global pnpmfile that runs before the local one.
    ///
    /// Mirrors pnpm's `--global-pnpmfile <path>`. Relative paths
    /// resolve against the project root. The global hook runs first
    /// and the local hook (if any) runs second, so local mutations
    /// win on conflicts — matching pnpm's composition order.
    #[arg(long, value_name = "PATH", conflicts_with = "ignore_pnpmfile")]
    pub global_pnpmfile: Option<std::path::PathBuf>,
    /// Skip running `.pnpmfile.mjs` / `.pnpmfile.cjs` hooks for this install
    #[arg(long)]
    pub ignore_pnpmfile: bool,
    /// Skip lifecycle scripts (no-op; aube already skips by default)
    #[arg(long)]
    pub ignore_scripts: bool,
    /// Read and write the lockfile in the given directory.
    ///
    /// Instead of placing the lockfile alongside `package.json`, the
    /// project becomes an importer keyed by its relative path from the
    /// lockfile directory. Mirrors pnpm's `--lockfile-dir`.
    #[arg(long, value_name = "PATH")]
    pub lockfile_dir: Option<String>,
    /// Resolve dependencies and write the lockfile, but don't link
    /// `node_modules`.
    ///
    /// Useful for CI workflows that only update the lockfile.
    #[arg(long, conflicts_with = "frozen_lockfile")]
    pub lockfile_only: bool,
    /// Merge per-branch lockfiles into the main `aube-lock.yaml`.
    ///
    /// Combines every `aube-lock.<branch>.yaml` file in the project
    /// into `aube-lock.yaml` and deletes the branch files. Companion
    /// to `gitBranchLockfile`. When
    /// `mergeGitBranchLockfilesBranchPattern` is set in
    /// `pnpm-workspace.yaml`, this happens automatically on matching
    /// branches; the flag forces it regardless.
    #[arg(long)]
    pub merge_git_branch_lockfiles: bool,
    /// Cap concurrent tarball downloads.
    ///
    /// Overrides `network-concurrency` from `.npmrc` /
    /// `aube-workspace.yaml` when set. Falls back to an auto-scaled
    /// default of worker count x3, clamped to 16-64.
    #[arg(long, value_name = "N")]
    pub network_concurrency: Option<u64>,
    /// Skip optionalDependencies; don't install optional native modules
    #[arg(long)]
    pub no_optional: bool,
    /// Inverse of `--side-effects-cache`.
    #[arg(long, overrides_with = "side_effects_cache")]
    pub no_side_effects_cache: bool,
    /// Inverse of `--verify-store-integrity`.
    ///
    /// Skips the SHA-512 verify step for every tarball aube pulls
    /// into the store during this install.
    #[arg(long, overrides_with = "verify_store_integrity")]
    pub no_verify_store_integrity: bool,
    /// Which layout to materialize `node_modules/` as.
    ///
    /// `isolated` (default) uses pnpm's `.aube/`-backed symlink tree;
    /// `hoisted` builds an npm-style flat tree with conflict nesting.
    /// Overrides `node-linker` / `nodeLinker` from `.npmrc` /
    /// `aube-workspace.yaml` when set. `pnp` is not supported.
    #[arg(long, value_name = "MODE")]
    pub node_linker: Option<String>,
    /// Fail if any metadata or tarball isn't already in the local cache.
    ///
    /// Never hits the network.
    #[arg(long, conflicts_with = "prefer_offline")]
    pub offline: bool,
    /// How to import package files from the global store into the
    /// virtual store.
    ///
    /// One of `auto` (default: detect the fastest strategy),
    /// `hardlink`, `copy`, `clone` (reflink; falls back to copy
    /// pending strict enforcement), or `clone-or-copy` (reflink with
    /// a copy fallback). Overrides `package-import-method` /
    /// `packageImportMethod` from `.npmrc` / `aube-workspace.yaml`
    /// when set.
    #[arg(long, value_name = "METHOD")]
    pub package_import_method: Option<String>,
    /// Override the local pnpmfile location.
    ///
    /// Mirrors pnpm's `--pnpmfile <path>`. Relative paths resolve
    /// against the project root; absolute paths are used as-is. Wins
    /// over `pnpmfilePath` from `pnpm-workspace.yaml`. A typo (target
    /// missing) is a hard miss with a warning rather than a silent
    /// fallback to the default.
    #[arg(long, value_name = "PATH", conflicts_with = "ignore_pnpmfile")]
    pub pnpmfile: Option<std::path::PathBuf>,
    /// Prefer cached metadata over revalidation; only hit the network on a miss.
    #[arg(long, conflicts_with = "offline")]
    pub prefer_offline: bool,
    /// Selectively hoist matching transitive deps to the root node_modules.
    ///
    /// Repeatable; comma-separated values are also accepted.
    #[arg(long, value_name = "GLOB", value_delimiter = ',')]
    pub public_hoist_pattern: Vec<String>,
    /// How to resolve version ranges.
    ///
    /// `highest` (pnpm's classic behavior) or `time-based` (pick the
    /// lowest satisfying direct dep and constrain transitives by a
    /// publish-date cutoff). Accepts pnpm's aliases `time` and
    /// `lowest-direct`. When omitted, falls back to the
    /// `resolution-mode` key in `.npmrc` / `aube-workspace.yaml`.
    #[arg(long, value_name = "MODE")]
    pub resolution_mode: Option<String>,
    /// Hoist every non-local transitive dep to the top-level
    /// `node_modules/`.
    ///
    /// Overrides `shamefully-hoist` / `shamefullyHoist` from
    /// `.npmrc` / `aube-workspace.yaml` when set.
    #[arg(long)]
    pub shamefully_hoist: bool,
    /// Cache post-build side effects for dependency packages.
    ///
    /// Defaults to on and only applies to packages allowed by
    /// `allowBuilds` / `onlyBuiltDependencies`. Pair with
    /// `--no-side-effects-cache` to opt out.
    #[arg(long, overrides_with = "no_side_effects_cache")]
    pub side_effects_cache: bool,
    /// Verify tarball SHA-512 before importing into the store.
    ///
    /// Checks each tarball against the lockfile integrity. Defaults to
    /// `true` (pnpm parity); pair with `--no-verify-store-integrity`
    /// to skip.
    #[arg(long, overrides_with = "no_verify_store_integrity")]
    pub verify_store_integrity: bool,
    /// Short alias for the global `--workspace-root` flag.
    ///
    /// Runs install from the workspace root regardless of cwd (`pnpm
    /// install -w`).
    #[arg(short = 'w', hide = true)]
    pub workspace_root_short: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

impl InstallArgs {
    /// Build the CLI flag bag that feeds
    /// [`aube_settings::ResolveCtx::cli`]. Each entry is a
    /// `(flag_name, value)` pair where `flag_name` matches a
    /// `sources.cli` alias declared in `settings.toml`. Values are
    /// already normalized to the raw form the
    /// `aube_settings::values::*_from_cli` helpers expect
    /// (`"true"`/`"false"` for bools, passthrough for strings). Only
    /// flags explicitly present on the command line are emitted —
    /// unset flags stay out of the bag so they don't override
    /// lower-precedence sources with their clap-derived default.
    pub fn to_cli_flag_bag(
        &self,
        global: Option<FrozenOverride>,
        global_gvs: GlobalVirtualStoreFlags,
    ) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        if let Some(mode) = self.resolution_mode.as_deref() {
            out.push(("resolution-mode".to_string(), mode.to_string()));
        }
        if let Some(linker) = self.node_linker.as_deref() {
            out.push(("node-linker".to_string(), linker.to_string()));
        }
        if let Some(d) = self.lockfile_dir.as_deref() {
            out.push(("lockfile-dir".to_string(), d.to_string()));
        }
        if let Some(method) = self.package_import_method.as_deref() {
            out.push(("package-import-method".to_string(), method.to_string()));
        }
        for pattern in &self.public_hoist_pattern {
            out.push(("public-hoist-pattern".to_string(), pattern.to_string()));
        }
        if self.shamefully_hoist {
            out.push(("shamefully-hoist".to_string(), "true".to_string()));
        }
        out.extend(global_gvs.to_cli_flag_bag());
        if let Some(ovr) = global {
            let (k, v) = ovr.cli_flag_bag_entry();
            out.push((k.to_string(), v.to_string()));
        }
        if let Some(n) = self.network_concurrency {
            out.push(("network-concurrency".to_string(), n.to_string()));
        }
        if self.verify_store_integrity {
            out.push(("verify-store-integrity".to_string(), "true".to_string()));
        }
        if self.no_verify_store_integrity {
            out.push(("verify-store-integrity".to_string(), "false".to_string()));
        }
        if self.side_effects_cache {
            out.push(("side-effects-cache".to_string(), "true".to_string()));
        }
        if self.no_side_effects_cache {
            out.push(("side-effects-cache".to_string(), "false".to_string()));
        }
        // `--fix-lockfile` is a distinct `FrozenMode::Fix` state, not a
        // `frozen-lockfile=false` shorthand — don't leak it into the
        // settings bag; `into_options` routes it directly.
        out
    }

    /// Resolve this CLI arg set into a full `InstallOptions`,
    /// consulting the workspace config for `preferFrozenLockfile`
    /// when no CLI flag forces it. Takes a pre-built `cli_flags` bag
    /// so the caller can reuse a single `to_cli_flag_bag` call for
    /// both the early `ResolveCtx` (used to read
    /// `preferFrozenLockfile`) and the `InstallOptions.cli_flags`
    /// field that threads the same values into `install::run`.
    pub fn into_options(
        self,
        global: Option<FrozenOverride>,
        yaml_prefer_frozen: Option<bool>,
        cli_flags: Vec<(String, String)>,
        env_snapshot: Vec<(String, String)>,
    ) -> InstallOptions {
        let force = self.force;
        let mode = if self.fix_lockfile {
            FrozenMode::Fix
        } else if force && global.is_none() {
            // `--force` without an explicit frozen mode re-resolves.
            FrozenMode::No
        } else {
            FrozenMode::from_override(global, yaml_prefer_frozen)
        };
        let network_mode = if self.offline {
            aube_registry::NetworkMode::Offline
        } else if self.prefer_offline {
            aube_registry::NetworkMode::PreferOffline
        } else {
            aube_registry::NetworkMode::Online
        };
        // pnpm parity: explicit `--frozen-lockfile` errors on a missing
        // lockfile (ERR_PNPM_NO_LOCKFILE), but the auto-CI default does
        // not — CI without a lockfile just does a regular resolve + write.
        let strict_no_lockfile = matches!(global, Some(FrozenOverride::Frozen));
        InstallOptions {
            project_dir: None,
            mode,
            dep_selection: DepSelection::from_flags(self.prod, self.dev, self.no_optional),
            ignore_pnpmfile: self.ignore_pnpmfile,
            pnpmfile: self.pnpmfile,
            global_pnpmfile: self.global_pnpmfile,
            ignore_scripts: self.ignore_scripts,
            lockfile_only: self.lockfile_only,
            merge_git_branch_lockfiles: self.merge_git_branch_lockfiles,
            dangerously_allow_all_builds: self.dangerously_allow_all_builds,
            network_mode,
            minimum_release_age_override: None,
            strict_no_lockfile,
            force,
            cli_flags,
            env_snapshot,
            git_prepare_depth: 0,
            inherited_build_policy: None,
            build_policy_override: None,
            workspace_filter: aube_workspace::selector::EffectiveFilter::default(),
            // Argumentless `aube install` runs root lifecycle hooks; the
            // chained-call constructor (`with_mode`) is where commands
            // with package args opt into skipping them.
            skip_root_lifecycle: false,
            // Argumentless `aube install` doesn't force the live-API
            // transitive gate by itself. `install::run` still runs
            // the gate when it detects fresh resolution (no
            // pre-existing lockfile, or the resolver picked a
            // version the lockfile didn't pin), and the
            // `advisoryCheckEveryInstall` setting flips it on for
            // every install — neither needs the caller to opt in.
            osv_transitive_check: false,
        }
    }
}

/// Aggregated options for `install::run`. Grouped into a struct so we can add
/// more flags (`--no-optional`, `--offline`, etc.) without changing every caller.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Explicit project directory for in-process nested installs. When
    /// unset, install discovers the project from the logical command cwd.
    pub project_dir: Option<std::path::PathBuf>,
    pub mode: FrozenMode,
    /// Which dep sections to keep in the materialized graph
    /// (`--prod` / `--dev` / `--no-optional`, in any valid combo).
    pub dep_selection: DepSelection,
    /// `--ignore-pnpmfile`: don't load or execute `.pnpmfile.mjs` / `.pnpmfile.cjs`
    /// hooks for this install, even if one exists in the project root.
    pub ignore_pnpmfile: bool,
    /// `--pnpmfile <path>`: override the local pnpmfile location for
    /// this run. Wins over `pnpmfilePath` in `pnpm-workspace.yaml` and
    /// the `.pnpmfile.mjs` / `.pnpmfile.cjs` defaults. `None` falls
    /// back to the workspace yaml + default search.
    pub pnpmfile: Option<std::path::PathBuf>,
    /// `--global-pnpmfile <path>`: add a second pnpmfile that runs
    /// *before* the local one, so org-wide rules can be layered under
    /// per-project hooks.
    pub global_pnpmfile: Option<std::path::PathBuf>,
    /// `--ignore-scripts`: skip root lifecycle scripts (`preinstall`,
    /// `install`, `postinstall`, `prepare`) *and* every dependency's
    /// lifecycle scripts, regardless of `allowBuilds`.
    pub ignore_scripts: bool,
    /// `--lockfile-only`: resolve and write the lockfile, but skip
    /// linking `node_modules` and running lifecycle scripts. Useful
    /// for CI workflows that only need to refresh the lockfile.
    pub lockfile_only: bool,
    /// `--merge-git-branch-lockfiles`: force a one-shot branch
    /// lockfile merge before the main install runs. See
    /// [`aube_lockfile::merge_branch_lockfiles`]. Equivalent to the
    /// `mergeGitBranchLockfilesBranchPattern` setting matching the
    /// current branch.
    pub merge_git_branch_lockfiles: bool,
    /// `--dangerously-allow-all-builds`: run every dependency's
    /// lifecycle scripts, bypassing the `allowBuilds` allowlist.
    /// Equivalent to pnpm's `--dangerously-allow-all-builds`.
    pub dangerously_allow_all_builds: bool,
    /// `--offline` / `--prefer-offline`: controls whether the registry client
    /// is allowed to hit the network during resolve and fetch.
    pub network_mode: aube_registry::NetworkMode,
    /// CLI override for `minimumReleaseAge` in minutes. `None` means
    /// "consult .npmrc / workspace config" — the run path resolves it
    /// to a concrete value (defaulting to 1440) before creating the
    /// resolver. There is no CLI flag yet, so this is always `None`
    /// today; reserved so future flags don't change the call site.
    pub minimum_release_age_override: Option<u64>,
    /// Error out if no lockfile is present. Matches pnpm's
    /// `ERR_PNPM_NO_LOCKFILE`: set by an explicit `--frozen-lockfile`
    /// flag and by `aube ci` / `aube clean-install`. The auto-CI
    /// default (`CI=1`, no explicit flag) leaves this `false` so a
    /// fresh checkout still resolves and writes a lockfile.
    pub strict_no_lockfile: bool,
    /// `--force`: re-resolve and relink even when `node_modules/.aube-state` says the
    /// tree is up to date. Mirrors pnpm's `install --force`.
    pub force: bool,
    /// Parsed CLI flag bag forwarded into
    /// [`aube_settings::ResolveCtx::cli`] so the build-time-generated
    /// `aube_settings::resolved::*` accessors can see CLI values with
    /// the highest precedence. Entries are `(long_flag, value)` pairs
    /// where `value` is already normalized to the raw form the
    /// type-specific resolver expects (`"true"`/`"false"` for bools,
    /// passthrough for strings). Populated at the clap-aware entry
    /// point via [`InstallArgs::to_cli_flag_bag`] and then threaded
    /// through every downstream caller that builds a `ResolveCtx`.
    pub cli_flags: Vec<(String, String)>,
    /// Process environment snapshot forwarded into
    /// [`aube_settings::ResolveCtx::env`]. Captured once at the
    /// clap-aware entry point via
    /// [`aube_settings::values::capture_env`] and threaded through so
    /// every `ResolveCtx` within a single `aube install` invocation
    /// sees the same env, keeping `preferFrozenLockfile` and the
    /// settings resolved inside [`run`] consistent. Commands that
    /// construct `InstallOptions` directly (`ci`, `deploy`) populate
    /// this with [`capture_env`] at their own entry point.
    pub env_snapshot: Vec<(String, String)>,
    /// Current git dependency prepare nesting depth. Kept in options so
    /// in-process prepare installs do not need cascading environment vars.
    pub git_prepare_depth: u32,
    /// Dependency build policy inherited by an in-process nested install.
    /// Used for git dependency `prepare`: the nested install runs in a
    /// scratch clone, but dependency build approval belongs to the outer
    /// project that requested the git package.
    pub inherited_build_policy: Option<std::sync::Arc<aube_scripts::BuildPolicy>>,
    /// Fully replace the build policy discovered from manifests and
    /// workspace config. Used by throwaway installs whose trust boundary
    /// must not inherit ambient project/user approvals.
    pub build_policy_override: Option<std::sync::Arc<aube_scripts::BuildPolicy>>,
    /// Global `--filter` / `--filter-prod` selectors. Resolution and
    /// lockfile writing still happen at the workspace root; these
    /// selectors narrow only the graph passed to the linker. Prod-only
    /// selectors additionally skip `devDependencies` edges during
    /// graph traversal — see `aube_workspace::selector::EffectiveFilter`.
    pub workspace_filter: aube_workspace::selector::EffectiveFilter,
    /// Skip the root package's `preinstall` / `install` / `postinstall` /
    /// `prepare` lifecycle hooks. pnpm parity: those hooks fire only on
    /// argumentless `pnpm install`. Every other user-facing entry point —
    /// `add`, `remove`, `update`, `dedupe`, `dlx`, patch tooling, the
    /// `ensure_installed` auto-install before `run`/`test` — must skip
    /// them so a chained `aube add foo` doesn't re-run an expensive root
    /// postinstall on every invocation. Independent of `ignore_scripts`,
    /// which also skips dep scripts. `with_mode()` defaults to `true`
    /// (chained-call constructor). The exceptions are argumentless
    /// `aube install` (`InstallArgs::into_options`), `aube ci` /
    /// `aube deploy` (literal struct constructions), and the nested
    /// git-prepare install — that one's "root" IS the git dep itself and
    /// running its `prepare` is the whole point.
    pub skip_root_lifecycle: bool,
    /// Run the post-resolve transitive OSV `MAL-*` gate against
    /// the live OSV API (not the mirror). Flipped on by commands
    /// whose whole point is fresh resolution — `aube add` and
    /// `aube update` — so the freshest signal lands at the moment
    /// the user is changing what's installed. Default `false` for
    /// every other entry point; `install::run` also flips it on
    /// internally when `advisoryCheckEveryInstall` is set, when
    /// no lockfile existed before resolve, or when the resolver
    /// picked a `(name, version)` pair the lockfile didn't
    /// already pin.
    pub osv_transitive_check: bool,
}

impl InstallOptions {
    /// Construct with the given frozen mode and all other flags at their
    /// defaults. Used by commands that chain into install (`add`, `remove`,
    /// `update`, `ensure_installed`) where none of the install-specific flags
    /// apply.
    pub fn with_mode(mode: FrozenMode) -> Self {
        Self {
            project_dir: None,
            mode,
            dep_selection: DepSelection::All,
            ignore_pnpmfile: false,
            pnpmfile: None,
            global_pnpmfile: None,
            ignore_scripts: false,
            lockfile_only: false,
            merge_git_branch_lockfiles: false,
            dangerously_allow_all_builds: false,
            network_mode: aube_registry::NetworkMode::Online,
            minimum_release_age_override: None,
            strict_no_lockfile: false,
            force: false,
            cli_flags: Vec::new(),
            env_snapshot: aube_settings::values::capture_env(),
            git_prepare_depth: 0,
            inherited_build_policy: None,
            build_policy_override: None,
            workspace_filter: aube_workspace::selector::EffectiveFilter::default(),
            // pnpm parity: every chained-call site (add / remove / update
            // / dedupe / dlx / patch / ensure_installed / git prepare)
            // skips root lifecycle hooks. Argumentless `aube install` is
            // the only construction path that runs them and it goes
            // through `InstallArgs::into_options`, not here.
            skip_root_lifecycle: true,
            // Default `false`. `aube add` and `aube update` flip
            // this on at construction. Other chained callers
            // (remove, dedupe, patch_commit, ...) leave it off so
            // their chained install relies on the install-time
            // routing (fresh-resolution detection / mirror
            // fallback) instead of an unconditional API hit.
            osv_transitive_check: false,
        }
    }
}

impl From<FrozenMode> for InstallOptions {
    fn from(mode: FrozenMode) -> Self {
        Self::with_mode(mode)
    }
}
