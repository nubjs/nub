use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

use aube_registry::client::RegistryClient;
use aube_registry::config::NpmConfig;
use miette::{Context, IntoDiagnostic, miette};

use super::{CatalogMap, config, install};

/// Process-wide snapshot of the top-level `--frozen-lockfile` /
/// `--no-frozen-lockfile` / `--prefer-frozen-lockfile` flags. Set once
/// by `async_main` before any command runs so downstream helpers
/// (`ensure_installed`, chained `install::run` calls from
/// `add`/`remove`/`update`/…) can pick them up without plumbing a
/// context struct through every command signature.
static GLOBAL_FROZEN: OnceLock<Option<install::FrozenOverride>> = OnceLock::new();
static GLOBAL_VIRTUAL_STORE: OnceLock<install::GlobalVirtualStoreFlags> = OnceLock::new();
static SKIP_AUTO_INSTALL_ON_PM_MISMATCH: AtomicBool = AtomicBool::new(false);

/// Process-wide registry override from the top-level `--registry=<url>`
/// flag. Applied in `make_client` (and any direct `NpmConfig::load`
/// caller that funnels through `load_npm_config`) so a single flag
/// covers every registry touch point in one invocation.
static REGISTRY_OVERRIDE: RwLock<Option<String>> = RwLock::new(None);

/// Process-wide CLI flag bag for `--fetch-timeout` / `--fetch-retries` /
/// `--fetch-retry-factor` / `--fetch-retry-mintimeout` /
/// `--fetch-retry-maxtimeout`. Threaded into `resolve_fetch_policy`'s
/// `ResolveCtx::cli` so any caller of `make_client` (install, add,
/// publish, audit, …) honors the global flags without each touching the
/// fetch wiring directly. Empty when no flags were set.
static FETCH_CLI_OVERRIDES: OnceLock<Vec<(String, String)>> = OnceLock::new();

#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct GlobalOutputFlags {
    pub ndjson: bool,
    pub silent: bool,
}

static GLOBAL_OUTPUT: OnceLock<GlobalOutputFlags> = OnceLock::new();

pub(crate) fn set_registry_override(url: Option<String>) {
    *REGISTRY_OVERRIDE.write().expect("registry lock poisoned") =
        url.map(|u| aube_registry::config::normalize_registry_url_pub(&u));
}

/// Record the `--fetch-*` global flag bag once per process. Idempotent
/// — second calls (e.g. from a unit test that re-runs `async_main`) are
/// silently ignored, matching the other `set_global_*` helpers.
pub(crate) fn set_fetch_cli_overrides(flags: Vec<(String, String)>) {
    let _ = FETCH_CLI_OVERRIDES.set(flags);
}

pub(crate) fn fetch_cli_overrides() -> &'static [(String, String)] {
    FETCH_CLI_OVERRIDES.get().map(Vec::as_slice).unwrap_or(&[])
}

pub(crate) fn set_skip_auto_install_on_package_manager_mismatch(skip: bool) {
    SKIP_AUTO_INSTALL_ON_PM_MISMATCH.store(skip, Ordering::Relaxed);
}

pub(crate) fn skip_auto_install_on_package_manager_mismatch() -> bool {
    SKIP_AUTO_INSTALL_ON_PM_MISMATCH.load(Ordering::Relaxed)
}

pub(crate) fn registry_override() -> Option<String> {
    REGISTRY_OVERRIDE
        .read()
        .expect("registry lock poisoned")
        .clone()
}

/// Load an `NpmConfig` for `dir` and then apply the process-wide
/// `--registry` override, if any. Use this from any command that
/// needs config but wants the CLI flag to win.
pub(crate) fn load_npm_config(dir: &std::path::Path) -> NpmConfig {
    let mut config = NpmConfig::load(dir);
    if let Some(url) = registry_override() {
        config.registry = url;
    }
    config
}

/// Record the global frozen-lockfile override snapshot. Called once per
/// process from `async_main`.
pub(crate) fn set_global_frozen_override(flags: Option<install::FrozenOverride>) {
    let _ = GLOBAL_FROZEN.set(flags);
}

pub(crate) fn set_global_virtual_store_flags(flags: install::GlobalVirtualStoreFlags) {
    let _ = GLOBAL_VIRTUAL_STORE.set(flags);
}

pub(crate) fn set_global_output_flags(flags: GlobalOutputFlags) {
    let _ = GLOBAL_OUTPUT.set(flags);
}

/// Read the recorded global frozen-lockfile override snapshot, or
/// `None` if none was set — e.g. in unit tests that bypass `async_main`.
pub(crate) fn global_frozen_override() -> Option<install::FrozenOverride> {
    GLOBAL_FROZEN.get().copied().unwrap_or_default()
}

pub(crate) fn global_virtual_store_flags() -> install::GlobalVirtualStoreFlags {
    GLOBAL_VIRTUAL_STORE.get().copied().unwrap_or_default()
}

pub(crate) fn global_output_flags() -> GlobalOutputFlags {
    GLOBAL_OUTPUT.get().copied().unwrap_or_default()
}

/// Owned bundle of the file-source inputs that feed a
/// [`aube_settings::ResolveCtx`]: project + user `.npmrc`, project +
/// user `~/.config/aube/config.toml`, and pnpm's global `config.yaml`.
/// Construct once with `FileSources::load`, borrow into a `ResolveCtx`
/// via `FileSources::ctx`.
pub(crate) struct FileSources {
    pub user_npmrc: Vec<(String, String)>,
    pub project_npmrc: Vec<(String, String)>,
    pub user_aube_config: Vec<(String, String)>,
    pub project_aube_config: Vec<(String, String)>,
    /// pnpm's global `config.yaml` (`<configDir>/config.yaml`, pnpm v11),
    /// or an empty map when pnpm isn't the incumbent / no file exists.
    pub global_config_yaml: std::collections::BTreeMap<String, yaml_serde::Value>,
}

impl FileSources {
    pub(crate) fn load(cwd: &Path) -> Self {
        let npmrc = aube_registry::config::load_npmrc_entries_split(cwd);
        Self {
            user_npmrc: npmrc.user,
            project_npmrc: npmrc.project,
            user_aube_config: config::load_user_aube_config_entries(),
            project_aube_config: config::load_project_aube_config_entries(cwd),
            global_config_yaml: load_global_config_yaml(),
        }
    }

    pub(crate) fn ctx<'a>(
        &'a self,
        workspace_yaml: &'a std::collections::BTreeMap<String, yaml_serde::Value>,
        env: &'a [(String, String)],
        cli: &'a [(String, String)],
    ) -> aube_settings::ResolveCtx<'a> {
        aube_settings::ResolveCtx {
            project_aube_config: &self.project_aube_config,
            project_npmrc: &self.project_npmrc,
            user_aube_config: &self.user_aube_config,
            user_npmrc: &self.user_npmrc,
            workspace_yaml,
            global_config_yaml: &self.global_config_yaml,
            env,
            cli,
            embedder_defaults: aube_settings::embedder_defaults(),
        }
    }
}

/// Load pnpm's global `config.yaml` (`<configDir>/config.yaml`, pnpm
/// v11) into the raw `pnpm-workspace.yaml`-shaped map the settings
/// resolver reads through its `*_from_workspace_yaml` helpers.
///
/// `configDir` is pnpm's per-OS config directory
/// ([`aube_util::env::pnpm_config_dir`]) — `$XDG_CONFIG_HOME/pnpm`, else
/// macOS `~/Library/Preferences/pnpm`, Windows
/// `%LOCALAPPDATA%\pnpm\config`, Linux `~/.config/pnpm`.
///
/// `config.yaml` is a pnpm-NAMED GLOBAL file, so it is gated by the
/// GLOBAL-scope posture `engine_context().read_pnpm_global_config` — NOT
/// the project-derived `read_branded_pnpm_config`. Global config has no
/// project incumbent, so an embedder whose global config is its own neutral
/// surface (e.g. nub) clears `read_pnpm_global_config` UNCONDITIONALLY and
/// this file is never read, regardless of the cwd's incumbent PM. A
/// missing/empty/unparseable file is also an empty map — global config is
/// best-effort and must never fail a command.
pub(crate) fn load_global_config_yaml() -> std::collections::BTreeMap<String, yaml_serde::Value> {
    let empty = std::collections::BTreeMap::new;
    if !aube_util::engine_context().read_pnpm_global_config {
        return empty();
    }
    let Some(config_dir) = aube_util::env::pnpm_config_dir() else {
        return empty();
    };
    let path = config_dir.join("config.yaml");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return empty();
    };
    if content.trim().is_empty() {
        return empty();
    }
    aube_manifest::parse_yaml(&path, content).unwrap_or_else(|_| empty())
}

/// Compute the `FrozenMode` a chained install (`add`, `remove`,
/// `update`, `ensure_installed`, …) should use, taking into account
/// the process-wide global `--frozen-lockfile` flags and falling back
/// to the given default when none was set on the command line.
pub(crate) fn chained_frozen_mode(default: install::FrozenMode) -> install::FrozenMode {
    match global_frozen_override() {
        Some(ovr) => install::FrozenMode::from_override(Some(ovr), None),
        None => default,
    }
}

pub(crate) fn ensure_registry_auth_for_package(
    client: &RegistryClient,
    registry_url: &str,
    package_name: &str,
) -> miette::Result<()> {
    if client.has_resolved_auth_for_package(registry_url, package_name) {
        Ok(())
    } else {
        let login_cmd = aube_util::cmd("login");
        let login_hint = package_name
            .split_once('/')
            .map(|(scope, _)| scope)
            .filter(|scope| scope.starts_with('@'))
            .map(|scope| format!("{login_cmd} --registry {registry_url} --scope {scope}"))
            .unwrap_or_else(|| format!("{login_cmd} --registry {registry_url}"));
        Err(miette!(
            "no auth token for {registry_url} package {package_name}. Run `{login_hint}` first."
        ))
    }
}

/// Open the global content-addressable store, honoring a `storeDir`
/// override from `.npmrc` or `pnpm-workspace.yaml` in `cwd`. Falls
/// back to the aube-owned default under `$XDG_DATA_HOME/aube/store/`
/// (see [`aube_store::dirs::store_dir`] for exact resolution).
///
/// Path interpretation matches pnpm: a leading `~` expands to the
/// user's home directory; relative paths are resolved against `cwd`
/// (so each project sees a consistent store regardless of where the
/// command was invoked from). The CAS schema suffix `v1/files` is
/// appended to the user-supplied path so the on-disk layout is stable
/// across versions of aube and never collides with a pnpm store rooted
/// at the same path.
pub(crate) fn open_store(cwd: &std::path::Path) -> miette::Result<aube_store::Store> {
    if let Some(custom) = resolved_store_dir(cwd) {
        aube_store::Store::with_root(custom.join("v1").join("files"))
            .into_diagnostic()
            .wrap_err("failed to open store")
    } else {
        aube_store::Store::default_location()
            .into_diagnostic()
            .wrap_err("failed to open store")
    }
}

/// Resolve the configured `storeDir` for `cwd`, returning `None` if
/// no override is set or the value can't be parsed. Walks `.npmrc`
/// and `pnpm-workspace.yaml` via `aube_settings::resolved::store_dir`,
/// then expands `~` and makes relative paths absolute against `cwd`.
/// The returned path is the user-facing store root *without* the
/// `v3/files` schema suffix — callers append it where needed (see
/// [`open_store`]).
pub(crate) fn resolved_store_dir(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    with_settings_ctx(cwd, |ctx| {
        let raw = aube_settings::resolved::store_dir(ctx)?;
        expand_setting_path(&raw, cwd)
    })
}

/// Expand a path-typed setting value. `~` -> home dir, relative ->
/// absolute against `cwd`. Returns None if the value begins with `~`
/// but no home env var is set, caller then falls back to a platform
/// default. On Unix reads HOME. On Windows reads HOME first (for
/// POSIX-compat toolchains that set it) then USERPROFILE (native
/// Windows default). Old code only checked HOME, Windows users got
/// silent None back for any `~/...` settings like `storeDir: ~/store`,
/// and the caller fell through to the platform default, so custom
/// store paths never took effect on Windows.
pub(crate) fn expand_setting_path(raw: &str, cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        std::path::PathBuf::from(home_dir_os()?).join(rest)
    } else if raw == "~" {
        std::path::PathBuf::from(home_dir_os()?)
    } else {
        std::path::PathBuf::from(raw)
    };
    Some(if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    })
}

fn home_dir_os() -> Option<std::ffi::OsString> {
    aube_util::env::home_dir().map(|p| p.into_os_string())
}

/// Build a file-only `ResolveCtx` for `cwd` and call `f` with it.
/// Handles the temporary ownership of npmrc/workspace/env data so
/// callers don't need to import `yaml_serde`.
pub(crate) fn with_settings_ctx<T>(
    cwd: &std::path::Path,
    f: impl FnOnce(&aube_settings::ResolveCtx<'_>) -> T,
) -> T {
    let files = FileSources::load(cwd);
    let raw_workspace = aube_manifest::workspace::load_raw(cwd).unwrap_or_default();
    // `process_env()` returns a `&'static` borrow of the once-captured
    // env. Avoids cloning ~200-500 String pairs every time a command
    // builds a ResolveCtx (the typical path hits this 5+ times per
    // `aube run`).
    let env = aube_settings::values::process_env();
    let ctx = files.ctx(&raw_workspace, env, &[]);
    f(&ctx)
}

/// Build a registry client configured from .npmrc files in the project directory.
///
/// Also resolves the `fetch*` settings (timeout + retries + backoff)
/// from the full settings precedence chain and threads the resulting
/// [`aube_registry::config::FetchPolicy`] into
/// the client. The CLI bag comes from [`fetch_cli_overrides`], which
/// `async_main` populates from the global `--fetch-timeout`,
/// `--fetch-retries`, and `--fetch-retry-{factor,mintimeout,maxtimeout}`
/// flags before any command runs.
pub(crate) fn make_client(cwd: &std::path::Path) -> aube_registry::client::RegistryClient {
    let config = load_npm_config(cwd);
    tracing::debug!("registry: {}", config.registry);
    for (scope, url) in &config.scoped_registries {
        tracing::debug!("scoped registry: {scope} -> {url}");
    }
    let policy = resolve_fetch_policy(cwd);
    aube_registry::client::RegistryClient::from_config_with_policy(config, policy)
}

/// Run the pnpmfile `preResolution` hook before the resolver walks
/// the graph. Builds a context snapshot (lockfile dir, store dir,
/// existing lockfile, registry map) from the same sources the rest
/// of the install pipeline consumes, so install and update see an
/// identical hook contract. `paths` is the (`global`, `local`) order
/// produced by [`crate::pnpmfile::ordered_paths`]; passing the whole
/// slice keeps the global-first-then-local contract in one place
/// instead of duplicating it at every install/update call site.
pub(crate) async fn run_pnpmfile_pre_resolution(
    paths: &[std::path::PathBuf],
    cwd: &std::path::Path,
    existing: Option<&aube_lockfile::LockfileGraph>,
) -> miette::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let config = load_npm_config(cwd);
    let mut registries = std::collections::BTreeMap::new();
    registries.insert("default".to_string(), config.registry);
    for (scope, url) in config.scoped_registries {
        registries.insert(scope, url);
    }
    // Honor `storeDir` from `.npmrc` / `pnpm-workspace.yaml` so the
    // hook's `storeDir` field matches the path `open_store` operates
    // on. Both branches return the user-facing root (without the
    // `v1/files` CAS schema suffix) so a hook reading `storeDir`
    // doesn't see different depths depending on whether the user set
    // an override; the platform default's CAS path lives at
    // `<root>/v1/files`, so we strip those two segments.
    let store_dir = resolved_store_dir(cwd).or_else(|| {
        aube_store::dirs::store_dir()
            .and_then(|p| p.parent()?.parent().map(std::path::Path::to_path_buf))
    });
    let ctx = crate::pnpmfile::PreResolutionContext::from_existing(
        cwd,
        store_dir.as_deref(),
        existing,
        registries,
    );
    crate::pnpmfile::run_pre_resolution_chain(paths, cwd, &ctx)
        .await
        .wrap_err("pnpmfile preResolution hook failed")
}

/// Build the standard resolver used by add/remove/update/dedupe/audit.
/// Internally routes through install's `configure_resolver` so every
/// setting `aube install` plumbs — `supportedArchitectures`,
/// `resolutionMode`, `minimumReleaseAge`, `autoInstallPeers`,
/// `dedupePeerDependents`, overrides, `ignoredOptionalDependencies`,
/// peer suffix length, git shallow hosts, network concurrency — lands
/// here too. Skipping this caused `aube update` to rewrite the
/// lockfile against host-only `supportedArchitectures` (collapsing
/// platform-variant optional deps like `@biomejs/biome-*` /
/// `@rollup/rollup-linux-*`) and to drop `time:` entries for direct
/// deps reused from the lockfile (the resolver only records times
/// when `resolutionMode=time-based` / `minimumReleaseAge` is on /
/// `trustPolicy=no-downgrade`, and none of those were threaded
/// through here).
///
/// Reads `.npmrc` + workspace yaml once via `with_settings_ctx`,
/// detects the existing lockfile kind so the platform-widening
/// behaves identically to the install that wrote that lockfile, and
/// passes `minimum_release_age_override = None` since these commands
/// don't expose `--minimum-release-age` today.
pub(crate) fn build_resolver(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    catalogs: CatalogMap,
) -> miette::Result<aube_resolver::Resolver> {
    let (ws_config, raw_workspace) = aube_manifest::workspace::load_both(cwd).unwrap_or_default();
    let files = FileSources::load(cwd);
    let env = aube_settings::values::process_env();
    let ctx = files.ctx(&raw_workspace, env, &[]);
    // `aube update` and friends always rewrite a lockfile, so pick a
    // target kind. Resolve the project's format (existing lockfile,
    // or the `package.json`-declared package manager's format on a
    // fresh project) to match install's cross-platform widening rules
    // — a project on `pnpm-lock.yaml` keeps pnpm's host-only optional
    // set, `aube-lock.yaml` gets the wide aube default. Errors when
    // the declaration contradicts the on-disk lockfiles or several
    // tools' lockfiles coexist undeclared.
    let target_lockfile_kind =
        Some(resolve_lockfile_kind_for_write(cwd)?.unwrap_or_else(|| default_lockfile_kind(&ctx)));
    Ok(install::configure_resolver(
        aube_resolver::Resolver::new(std::sync::Arc::new(make_client(cwd))),
        cwd,
        manifest,
        install::ResolverConfigInputs {
            settings_ctx: &ctx,
            workspace_config: &ws_config,
            workspace_catalogs: &catalogs,
            minimum_release_age_override: None,
            target_lockfile_kind,
            // Update / add / dedupe / audit deliberately skip the
            // full-packument disk cache install populates: the cache's
            // freshness window can outlive a registry dist-tag bump,
            // and these commands need to observe `latest` exactly as
            // it stands right now (pnpm_update.bats simulates this by
            // mutating `dist-tags` between commands). The abbreviated
            // cache stays on either way.
            cache_full_packuments: false,
            ignore_scripts: false,
        },
        None,
    ))
}

/// Declaration-aware lockfile-kind resolution for `cwd`, collapsed to
/// the `Option` shape the resolve/write sites consume: `Some(kind)`
/// when a lockfile exists or `package.json` declares a package
/// manager (pin-over-inference — the declaration outranks both file
/// precedence and `defaultLockfileFormat`), `None` when the project
/// is genuinely fresh and undeclared so the caller falls back to
/// [`default_lockfile_kind`]. Propagates the structured
/// declaration-mismatch / ambiguous-lockfiles errors.
pub(crate) fn resolve_lockfile_kind_for_write(
    cwd: &std::path::Path,
) -> miette::Result<Option<aube_lockfile::LockfileKind>> {
    aube_lockfile::resolve_project_lockfile_kind(cwd)
        .map(aube_lockfile::ResolvedLockfileKind::kind)
        .map_err(miette::Report::new)
}

/// Resolve [`aube_registry::config::FetchPolicy`] from the same
/// sources the rest of the CLI consumes settings from. Kept separate
/// from [`make_client`] so tests and ad-hoc callers (publish,
/// deprecate, etc) can opt in without duplicating the ctx-building
/// boilerplate.
pub(crate) fn resolve_fetch_policy(cwd: &std::path::Path) -> aube_registry::config::FetchPolicy {
    let files = FileSources::load(cwd);
    let workspace_yaml = aube_manifest::workspace::load_both(cwd)
        .map(|(_, raw)| raw)
        .unwrap_or_default();
    let env = aube_settings::values::process_env();
    let ctx = files.ctx(&workspace_yaml, env, fetch_cli_overrides());
    aube_registry::config::FetchPolicy::from_ctx(&ctx)
}

/// Resolve the `cacheDir` setting for `cwd`. If an explicit override
/// is set in `.npmrc`, expands it and returns that path. Otherwise
/// falls back to the XDG-aware platform default (`~/.cache/aube`).
///
/// Note: `XDG_CACHE_HOME` is intentionally *not* a source for this
/// setting — it's a base directory, and `aube_store::dirs::cache_dir()`
/// already appends `/aube`. Routing it through the settings accessor
/// would lose the subdirectory.
pub(crate) fn resolved_cache_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    let platform_default =
        || aube_store::dirs::cache_dir().unwrap_or_else(|| std::env::temp_dir().join("aube"));
    // Check whether .npmrc explicitly sets cacheDir, rather than comparing
    // the resolved value against the default string — a user who writes
    // `cacheDir=~/.cache/aube` explicitly should get that literal path,
    // not the XDG_CACHE_HOME-aware platform default.
    let npmrc = aube_registry::config::load_npmrc_entries(cwd);
    let has_explicit = npmrc
        .iter()
        .any(|(k, _)| k == "cacheDir" || k == "cache-dir");
    if !has_explicit {
        return platform_default();
    }
    with_settings_ctx(cwd, |ctx| {
        let raw = aube_settings::resolved::cache_dir(ctx);
        expand_setting_path(&raw, cwd).unwrap_or_else(platform_default)
    })
}

/// Resolve the `virtualStoreDirMaxLength` setting, falling back to the
/// platform default (`DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH`, which is
/// 120 on Linux/macOS and will become 60 on Windows once Windows
/// support lands). Every call site that encodes `dep_path`s into
/// `.aube/<name>` filenames — install, list, why, patch, rebuild,
/// engines check — must resolve the same cap, otherwise the long-path
/// truncate-and-hash branch of `dep_path_to_filename` produces
/// different filenames for read-side and write-side callers and
/// silently misses packages.
pub(crate) fn resolve_virtual_store_dir_max_length(ctx: &aube_settings::ResolveCtx<'_>) -> usize {
    aube_settings::resolved::virtual_store_dir_max_length(ctx)
        .map(|v| v as usize)
        .unwrap_or(aube_lockfile::dep_path_filename::DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH)
}

/// Load `.npmrc` + `pnpm-workspace.yaml` for `cwd` and resolve the
/// effective `virtualStoreDirMaxLength` in one call. Convenience for
/// post-install commands (list, why, patch) that don't build a
/// `ResolveCtx` for any other reason.
pub(crate) fn resolve_virtual_store_dir_max_length_for_cwd(cwd: &std::path::Path) -> usize {
    with_settings_ctx(cwd, resolve_virtual_store_dir_max_length)
}

/// Lockfile format to write when the project has no lockfile yet —
/// the `defaultLockfileFormat` setting mapped onto
/// [`aube_lockfile::LockfileKind`]. Every fresh-project fallback that
/// used to hard-code `LockfileKind::Aube` resolves through here, so
/// the setting reaches the resolver's platform-widening target and the
/// install/add/update write paths alike. Projects with an existing
/// lockfile are unaffected: format detection still wins, this is only
/// the fallback.
pub(crate) fn default_lockfile_kind(
    ctx: &aube_settings::ResolveCtx<'_>,
) -> aube_lockfile::LockfileKind {
    use aube_settings::resolved::DefaultLockfileFormat as Format;
    match aube_settings::resolved::default_lockfile_format(ctx) {
        Format::Aube => aube_lockfile::LockfileKind::Aube,
        Format::Pnpm => aube_lockfile::LockfileKind::Pnpm,
        Format::Npm => aube_lockfile::LockfileKind::Npm,
        Format::Yarn => aube_lockfile::LockfileKind::Yarn,
        Format::Bun => aube_lockfile::LockfileKind::Bun,
    }
}

/// Load `.npmrc` + `pnpm-workspace.yaml` for `cwd` and resolve the
/// effective fresh-project lockfile format in one call. Convenience
/// for call sites that don't already hold a `ResolveCtx`.
pub(crate) fn default_lockfile_kind_for_cwd(cwd: &std::path::Path) -> aube_lockfile::LockfileKind {
    with_settings_ctx(cwd, default_lockfile_kind)
}

/// Project-level `node_modules` directory name (pnpm's `modulesDir`
/// setting). Defaults to `"node_modules"` — users who change it are
/// responsible for setting `NODE_PATH` themselves since Node's own
/// resolver still looks for a literal `node_modules/`.
///
/// Every command that touches the top-level project directory (bin,
/// root, prune, clean, link, unlink, run, exec, etc.) reads this so
/// it lands on the same path the install wrote to. Commands that
/// already build a `ResolveCtx` for other settings should call
/// `aube_settings::resolved::modules_dir(&ctx)` directly instead of
/// this shortcut.
pub(crate) fn resolve_modules_dir_name_for_cwd(cwd: &std::path::Path) -> String {
    with_settings_ctx(cwd, aube_settings::resolved::modules_dir)
}

/// Convenience: `<cwd>/<modulesDir>` as a `PathBuf`. Matches the
/// `project_dir.join("node_modules")` pattern that every command used
/// before `modulesDir` was wired; prefer this over the raw literal
/// so a workspace-level override flows through automatically.
pub(crate) fn project_modules_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    cwd.join(resolve_modules_dir_name_for_cwd(cwd))
}

/// Resolve the absolute path of the per-project virtual store
/// (pnpm's `virtualStoreDir`). When the user explicitly sets the value
/// in `.npmrc`, `pnpm-workspace.yaml`, or the environment, expand it
/// (relative paths resolve against `project_dir`, `~` expands to
/// `$HOME`) and return it. Otherwise derive from `modulesDir`:
/// `<project_dir>/<modulesDir>/.aube`. This matches pnpm, where the
/// documented default is `<modulesDir>/.pnpm` — a user who overrides
/// `modulesDir` alone keeps a coherent layout without having to set
/// both.
///
/// Every site that touches `.aube/<dep_path>/` — linker, install state
/// sidecar, `patch`, `rebuild`, `list --long`, `why`, `prune`, `clean`,
/// etc. — must resolve through this helper so a workspace-level
/// override lands at the same path the install wrote to.
pub(crate) fn resolve_virtual_store_dir(
    ctx: &aube_settings::ResolveCtx<'_>,
    project_dir: &std::path::Path,
) -> std::path::PathBuf {
    let default_from_modules_dir = || {
        let modules_dir = aube_settings::resolved::modules_dir(ctx);
        // Virtual-store leaf from the active embedder's name: `.<name>`.
        // Standalone aube → `.aube`.
        let leaf = format!(".{}", aube_util::embedder().name);
        project_dir.join(modules_dir).join(leaf)
    };
    let has_explicit_npmrc = [
        ctx.project_aube_config,
        ctx.project_npmrc,
        ctx.user_aube_config,
        ctx.user_npmrc,
    ]
    .iter()
    .any(|entries| {
        entries
            .iter()
            .any(|(k, _)| k == "virtualStoreDir" || k == "virtual-store-dir")
    });
    let has_explicit_yaml = ctx.workspace_yaml.contains_key("virtualStoreDir");
    // Mirrors the `sources.env` list in settings.toml (`virtualStoreDir`).
    // Keep all three aliases here — dropping `AUBE_VIRTUAL_STORE_DIR`
    // silently routes through the default branch even though
    // `aube_settings::resolved::virtual_store_dir` honors the env value.
    let has_explicit_env = ctx.env.iter().any(|(k, _)| {
        k == "npm_config_virtual_store_dir"
            || k == "NPM_CONFIG_VIRTUAL_STORE_DIR"
            || k == "AUBE_VIRTUAL_STORE_DIR"
    });
    // An embedder-supplied default (keyed by canonical setting name) counts
    // as explicit too — without this check a `virtualStoreDir` registered via
    // `set_embedder_defaults` (e.g. a host that wants `node_modules/.nub`)
    // would be silently discarded in favor of the `<modulesDir>/.aube`
    // derivation, since `resolved::virtual_store_dir` *does* honor the
    // embedder-defaults source via the ctx. Standalone aube registers no
    // embedder defaults, so this is empty and the default branch is taken
    // exactly as before.
    let has_explicit_embedder_default = ctx
        .embedder_defaults
        .iter()
        .any(|(k, _)| k == "virtualStoreDir");
    if !(has_explicit_npmrc
        || has_explicit_yaml
        || has_explicit_env
        || has_explicit_embedder_default)
    {
        return default_from_modules_dir();
    }
    let raw = aube_settings::resolved::virtual_store_dir(ctx);
    expand_setting_path(&raw, project_dir).unwrap_or_else(default_from_modules_dir)
}

/// Load `.npmrc` + `pnpm-workspace.yaml` for `cwd` and resolve the
/// effective virtual-store path in one call. Convenience for
/// post-install commands (`patch`, `list --long`, `why`, `clean`,
/// `unlink`) that don't build a `ResolveCtx` for any other reason.
pub(crate) fn resolve_virtual_store_dir_for_cwd(cwd: &std::path::Path) -> std::path::PathBuf {
    with_settings_ctx(cwd, |ctx| resolve_virtual_store_dir(ctx, cwd))
}

/// Disk cache directory for packument metadata. Falls back to a tmp dir if
/// the user cache dir can't be resolved (rare).
pub(crate) fn packument_cache_dir() -> std::path::PathBuf {
    let cwd = crate::dirs::cwd().unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    resolved_cache_dir(&cwd).join("packuments-v1")
}

/// Disk cache directory for *full* (non-corgi) packument JSON used by
/// human-facing commands like `aube view`. Separate from the corgi cache
/// because the shapes differ.
pub(crate) fn packument_full_cache_dir() -> std::path::PathBuf {
    let cwd = crate::dirs::cwd().unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    resolved_cache_dir(&cwd).join("packuments-full-v1")
}

#[cfg(test)]
mod resolve_virtual_store_dir_tests {
    use super::resolve_virtual_store_dir;
    use aube_settings::ResolveCtx;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn ctx_with_env<'a>(
        env: &'a [(String, String)],
        ws: &'a BTreeMap<String, yaml_serde::Value>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &[],
            user_aube_config: &[],
            user_npmrc: &[],
            workspace_yaml: ws,
            global_config_yaml: aube_settings::values::empty_yaml_map(),
            env,
            cli: &[],
            embedder_defaults: &[],
        }
    }

    #[test]
    fn default_when_no_explicit_override() {
        let env = vec![];
        let ws = BTreeMap::new();
        let ctx = ctx_with_env(&env, &ws);
        let project = PathBuf::from("/proj");
        assert_eq!(
            resolve_virtual_store_dir(&ctx, &project),
            PathBuf::from("/proj/node_modules/.aube"),
        );
    }

    #[test]
    fn aube_env_var_relocates_virtual_store() {
        // Regression guard: AUBE_VIRTUAL_STORE_DIR is declared in
        // settings.toml's `sources.env` for `virtualStoreDir`. Without
        // it in the explicit-detection list, a user setting only
        // AUBE_VIRTUAL_STORE_DIR (and not the npm_config_* aliases)
        // would silently fall through to the default path.
        let env = vec![("AUBE_VIRTUAL_STORE_DIR".into(), ".aube".into())];
        let ws = BTreeMap::new();
        let ctx = ctx_with_env(&env, &ws);
        let project = PathBuf::from("/proj");
        assert_eq!(
            resolve_virtual_store_dir(&ctx, &project),
            PathBuf::from("/proj/.aube"),
        );
    }

    #[test]
    fn npm_config_env_var_relocates_virtual_store() {
        let env = vec![("npm_config_virtual_store_dir".into(), ".vstore".into())];
        let ws = BTreeMap::new();
        let ctx = ctx_with_env(&env, &ws);
        let project = PathBuf::from("/proj");
        assert_eq!(
            resolve_virtual_store_dir(&ctx, &project),
            PathBuf::from("/proj/.vstore"),
        );
    }
}

#[cfg(test)]
mod package_manager_mismatch_tests {
    use super::skip_auto_install_on_package_manager_mismatch;

    #[test]
    fn skip_auto_install_defaults_off() {
        assert!(!skip_auto_install_on_package_manager_mismatch());
    }
}

#[cfg(test)]
mod default_lockfile_kind_tests {
    use super::default_lockfile_kind;
    use aube_settings::ResolveCtx;
    use std::collections::BTreeMap;

    fn ctx<'a>(
        npmrc: &'a [(String, String)],
        ws: &'a BTreeMap<String, yaml_serde::Value>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            project_aube_config: &[],
            project_npmrc: npmrc,
            user_aube_config: &[],
            user_npmrc: &[],
            workspace_yaml: ws,
            global_config_yaml: aube_settings::values::empty_yaml_map(),
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        }
    }

    #[test]
    fn defaults_to_aube_lock_when_unset() {
        let ws = BTreeMap::new();
        assert_eq!(
            default_lockfile_kind(&ctx(&[], &ws)),
            aube_lockfile::LockfileKind::Aube
        );
    }

    #[test]
    fn npmrc_value_selects_foreign_format() {
        let npmrc = vec![("defaultLockfileFormat".to_string(), "pnpm".to_string())];
        let ws = BTreeMap::new();
        assert_eq!(
            default_lockfile_kind(&ctx(&npmrc, &ws)),
            aube_lockfile::LockfileKind::Pnpm,
            "defaultLockfileFormat=pnpm must map the fresh-project fallback to pnpm-lock.yaml"
        );
    }

    #[test]
    fn unknown_value_falls_back_to_aube() {
        // The generated enum accessor turns an unrecognized value into
        // the declared default rather than poisoning the install.
        let npmrc = vec![(
            "default-lockfile-format".to_string(),
            "totally-fake".to_string(),
        )];
        let ws = BTreeMap::new();
        assert_eq!(
            default_lockfile_kind(&ctx(&npmrc, &ws)),
            aube_lockfile::LockfileKind::Aube
        );
    }
}
