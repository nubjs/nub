use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

use aube_registry::client::RegistryClient;
use aube_registry::config::NpmConfig;
use miette::{Context, miette};

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
/// [`aube_settings::ResolveCtx`]: managed config, project + user `.npmrc`,
/// project + user `~/.config/aube/config.toml`, and pnpm's global
/// `config.yaml`. Construct once with `FileSources::load`, borrow into a
/// `ResolveCtx` via `FileSources::ctx`.
#[derive(Clone)]
pub(crate) struct FileSources {
    pub managed_aube_config: Vec<(String, String)>,
    pub user_npmrc: Vec<(String, String)>,
    pub project_npmrc: Vec<(String, String)>,
    project_config_settings: Vec<(String, String)>,
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
            managed_aube_config: config::load_managed_aube_config_entries(),
            user_npmrc: npmrc.user,
            project_npmrc: npmrc.project,
            project_config_settings: aube_util::engine_context().project_config_settings,
            user_aube_config: config::load_user_aube_config_entries(),
            project_aube_config: config::load_project_aube_config_entries(cwd),
            global_config_yaml: load_global_config_yaml(),
        }
    }

    pub(crate) fn extend_project_sources(&mut self, cwd: &Path) {
        let npmrc = aube_registry::config::load_npmrc_entries_split(cwd);
        self.project_npmrc.extend(npmrc.project);
        self.project_aube_config
            .extend(config::load_project_aube_config_entries(cwd));
    }

    pub(crate) fn ctx<'a>(
        &'a self,
        workspace_yaml: &'a std::collections::BTreeMap<String, yaml_serde::Value>,
        env: &'a [(String, String)],
        cli: &'a [(String, String)],
    ) -> aube_settings::ResolveCtx<'a> {
        aube_settings::ResolveCtx {
            managed_aube_config: &self.managed_aube_config,
            project_aube_config: &self.project_aube_config,
            project_npmrc: &self.project_npmrc,
            project_config: &self.project_config_settings,
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
/// the project-derived `read_branded_pnpm_config`. The embedding host decides
/// that posture from its identity policy; it may enable this tier for a
/// particular pnpm major while keeping it inert for other project identities. A
/// missing/empty/unparseable file is also an empty map — global config is
/// best-effort and must never fail a command.
pub fn load_global_config_yaml() -> std::collections::BTreeMap<String, yaml_serde::Value> {
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
        Some(ovr) => install::FrozenMode::from_override(Some(ovr), None, false),
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
/// The store's cache dir — packument caches, and the global virtual
/// store unless `globalVirtualStoreDir` moves it — comes from
/// [`resolved_cache_dir`], so `cacheDir` and `storeDir` can be pointed
/// at the same volume without touching `XDG_CACHE_HOME`.
///
/// Path interpretation matches pnpm: a leading `~` expands to the
/// user's home directory; relative paths are resolved against `cwd`
/// (so each project sees a consistent store regardless of where the
/// command was invoked from). The CAS schema suffix `v1/files` is
/// appended to the user-supplied path so the on-disk layout is stable
/// across versions of aube and never collides with a pnpm store rooted
/// at the same path.
pub(crate) fn open_store(cwd: &std::path::Path) -> miette::Result<aube_store::Store> {
    let roots = store_roots(cwd);
    // The virtual store is always passed explicitly so this and every read-side
    // caller resolve it through the same `global_virtual_store_dir` ladder.
    let mut store = aube_store::Store::with_dirs(roots.files, resolved_cache_dir(cwd))
        .with_virtual_store_dir(global_virtual_store_dir(cwd));
    if let Some(read_fallback) = roots.read_fallback {
        store = store.with_read_fallback(read_fallback);
    }
    Ok(store)
}

/// Where the CAS lives for `cwd`: `files` is `<store>/v1/files`, and
/// `read_fallback` is a second, read-only CAS root when `files` is a
/// project-local stand-in for an unwritable global store.
#[derive(Clone)]
struct StoreRoots {
    files: std::path::PathBuf,
    read_fallback: Option<std::path::PathBuf>,
}

/// A `storeDir` the USER set is used verbatim; an unset one, or one that
/// merely restates the embedder profile's default (nub registers its data-dir
/// store as a settings default, so under nub `storeDir` always resolves to
/// *something*), is a default and gets [`default_store_roots`]'s
/// unwritable-fallback treatment.
fn store_roots(cwd: &std::path::Path) -> StoreRoots {
    match resolved_store_dir(cwd) {
        Some(custom) if !is_embedder_default_store_dir(&custom, cwd) => StoreRoots {
            files: custom.join("v1").join("files"),
            read_fallback: None,
        },
        profile_default => {
            default_store_roots(cwd, profile_default.map(|d| d.join("v1").join("files")))
        }
    }
}

fn is_embedder_default_store_dir(resolved: &std::path::Path, cwd: &std::path::Path) -> bool {
    aube_settings::embedder_defaults()
        .iter()
        .find(|(key, _)| key == "storeDir")
        .and_then(|(_, raw)| expand_setting_path(raw, cwd))
        .is_some_and(|default| default == resolved)
}

/// The store's `v1` directories for `cwd`: `primary` is the parent of the CAS
/// root [`open_store`] writes, and `read_fallback` is the read-only global
/// `v1` still consulted for everything it holds when the default store is not
/// writable. A tier layered on the store — the no-integrity bindings, nub's
/// phantom sidecars — reads both, primary first, and writes only the
/// primary, exactly as the CAS and index do.
#[derive(Clone, Debug)]
pub struct StoreV1Dirs {
    pub primary: std::path::PathBuf,
    pub read_fallback: Option<std::path::PathBuf>,
}

pub(crate) fn store_v1_dirs(cwd: &std::path::Path) -> StoreV1Dirs {
    let roots = store_roots(cwd);
    StoreV1Dirs {
        primary: v1_of(roots.files),
        read_fallback: roots.read_fallback.map(v1_of),
    }
}

fn v1_of(files: std::path::PathBuf) -> std::path::PathBuf {
    files
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or(files)
}

/// The `v1` directory store-adjacent state is WRITTEN to for `cwd`. Reads
/// that must also see the global store go through [`store_v1_dirs`].
pub(crate) fn store_v1_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    store_v1_dirs(cwd).primary
}

/// [`store_v1_dirs`] anchored the way [`resolved_project_store_dir`] is, for
/// an embedder's store-adjacent tier. The same fallback decision the engine's
/// own store handle made, so the embedder's producer and consumer key one
/// store — and still read through to the global one for warm packages.
pub fn resolved_project_store_v1_dirs() -> Option<StoreV1Dirs> {
    let anchor = crate::dirs::workspace_or_project_root()
        .or_else(|_| crate::dirs::cwd())
        .ok()?;
    Some(store_v1_dirs(&anchor))
}

/// The CAS root when no `storeDir` is configured: `profile_default` (the
/// embedder's) else the aube-owned default under XDG/HOME (a `$TMPDIR`-rooted
/// store when neither is set — the store ctors used to hard-fail with
/// `NoHome` and aborted installs in HOME-less CI), or, when that default is
/// not writable, a project-local store under `node_modules`.
///
/// The unwritable default is a coding agent's command sandbox: Codex and
/// Claude Code confine writes to the workspace and temp dirs, so the data
/// home EPERMs and even a network-free `file:` install died on its first CAS
/// write. A store inside `node_modules` sits on the writable side of that
/// line — the linker skips dot-entries when it sweeps the root, so it
/// survives an install, and `rm -rf node_modules` disposes of it like any
/// cache. The unwritable global store stays attached as a read-only
/// fallback, so everything it already holds is still reused; only NEW
/// content lands in the project-local store.
///
/// Decided once per process so every open — install, the lifecycle runner's
/// read-side handle, dlx — agrees on one store. Elsewhere the probe costs one
/// temp-file create and unlink.
fn default_store_roots(
    cwd: &std::path::Path,
    profile_default: Option<std::path::PathBuf>,
) -> StoreRoots {
    static DECISION: std::sync::OnceLock<StoreRoots> = std::sync::OnceLock::new();
    DECISION
        .get_or_init(|| {
            let default = profile_default
                .or_else(aube_store::dirs::store_dir)
                .unwrap_or_else(|| std::env::temp_dir().join("aube").join("store/v1/files"));
            match probe_writable(&default) {
                Ok(()) => StoreRoots {
                    files: default,
                    read_fallback: None,
                },
                Err(e) if is_unwritable(&e) => {
                    let fallback = cwd
                        .join("node_modules")
                        .join(format!(".{}-store", aube_util::prog()))
                        .join("v1")
                        .join("files");
                    let sandbox = aube_util::agent_sandbox::detect()
                        .map(|s| format!(" (inside the {} sandbox)", s.label()))
                        .unwrap_or_default();
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_STORE_FALLBACK,
                        "store {} is not writable{sandbox}; new packages go to the project-local store {} for this run",
                        default.display(),
                        fallback.display()
                    );
                    StoreRoots {
                        files: fallback,
                        read_fallback: Some(default),
                    }
                }
                // Anything else is reported by the first real write, with the
                // path it failed on.
                Err(_) => StoreRoots {
                    files: default,
                    read_fallback: None,
                },
            }
        })
        .clone()
}

/// Can this process create files under `root`? Creates the directory chain
/// if missing (an empty store dir is the normal first-run state anyway) and
/// a temp file inside it that is unlinked on drop.
fn probe_writable(root: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    tempfile::Builder::new()
        .prefix(".write-probe-")
        .tempfile_in(root)?;
    Ok(())
}

fn is_unwritable(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
    )
}

/// Resolve the configured `storeDir` for `cwd`, returning `None` if
/// no override is set or the value can't be parsed. Walks `.npmrc`
/// and `pnpm-workspace.yaml` via `aube_settings::resolved::store_dir`,
/// then expands `~` and makes relative paths absolute against `cwd`.
/// The returned path is the user-facing store root *without* the
/// `v1/files` schema suffix — callers append it where needed (see
/// [`open_store`]). Stays crate-internal, and deliberately: the answer
/// is only as good as `cwd`, so the export an embedder gets is
/// [`resolved_project_store_dir`], which picks the anchor itself.
pub(crate) fn resolved_store_dir(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    with_settings_ctx(cwd, |ctx| {
        let raw = aube_settings::resolved::store_dir(ctx)?;
        expand_setting_path(&raw, cwd)
    })
}

/// The store root for the PROJECT, anchored exactly where the install
/// pipeline anchors it — [`crate::dirs::workspace_or_project_root`],
/// which walks UP to the workspace root — not the process cwd. An
/// embedder resolving a store-adjacent tier must use this: a command run
/// from inside a workspace member sees neither the root `.npmrc` nor the
/// root `pnpm-workspace.yaml`, so a cwd-anchored resolution silently
/// returns the default while the engine's own store handle honors the
/// override (#643).
pub fn resolved_project_store_dir() -> Option<std::path::PathBuf> {
    let anchor = crate::dirs::workspace_or_project_root()
        .or_else(|_| crate::dirs::cwd())
        .ok()?;
    resolved_store_dir(&anchor)
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
    // `add`/`update`/`dedupe`/`audit` route `<alias>:<spec>` deps the same as
    // install; empty under any non-pnpm posture.
    let named_registries = super::discover_named_registries(cwd);
    Ok(install::configure_resolver(
        aube_resolver::Resolver::new(std::sync::Arc::new(make_client(cwd))),
        cwd,
        manifest,
        install::ResolverConfigInputs {
            settings_ctx: &ctx,
            workspace_config: &ws_config,
            workspace_catalogs: &catalogs,
            named_registries: &named_registries,
            minimum_release_age_override: None,
            target_lockfile_kind,
            dependency_policy: None,
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

/// Resolve the `cacheDir` setting for `cwd`. When set (via the host's own
/// `<BRAND>_CACHE_DIR` / `npm_config_cache_dir`, or `cache-dir` in
/// `.npmrc` / `aube-config.toml`), expands it and returns that path.
/// Otherwise falls back to the platform cache dir:
/// `$XDG_CACHE_HOME/aube` when `XDG_CACHE_HOME` is set,
/// `%LOCALAPPDATA%\aube` on Windows, else `~/.cache/aube`.
///
/// `cacheDir` has no default baked into the generated accessor (see the
/// exclusion list in `aube-settings/build.rs`) precisely so this
/// function can tell "not configured" from "configured to the literal
/// default path": every platform fallback above is a *base* directory
/// that aube appends its own name to, so routing them through the
/// settings accessor would lose the subdirectory.
///
/// The packument caches and — unless [`global_virtual_store_dir`]
/// overrides it — the global virtual store hang off this path, so a
/// user who moves the cache to another volume moves them together.
pub(crate) fn resolved_cache_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    let platform_default =
        || aube_store::dirs::cache_dir().unwrap_or_else(|| std::env::temp_dir().join("aube"));
    // The host's first-class cache knob is read AHEAD of the settings chain.
    // Byte-identical for standalone aube: `config_env` composes the same
    // `AUBE_CACHE_DIR` the settings table declares, and the table already
    // walks its aliases in reverse, so the branded form already outranked the
    // `npm_config_*` / `pnpm_config_*` forms — this preserves that order
    // rather than introducing one. Under an embedder with its own
    // `config_env_prefix` it is the ONLY spelling that reaches the cache:
    // aube's branded alias is gated off by `env_prefix = None`, and the host's
    // form (`NUB_CACHE_DIR`) is not in the table, so without this the resolver
    // primer — which reads `config_env` directly — was the knob's sole
    // consumer and the cache it names silently ignored it (#654).
    // `cacheDir` carries no `managedPolicy`, so returning early here skips no
    // hardening pass. A setting that grows one must not be short-circuited
    // this way, and the assert is what turns that from a note into a tripwire:
    // the enforcement lives in `apply_managed_string` at the END of the
    // generated accessor, which the early return never reaches.
    debug_assert!(
        aube_settings::meta::find("cacheDir").is_none_or(|m| m.managed_policy.is_empty()),
        "cacheDir grew a managedPolicy; the early return below skips it"
    );
    if let Some(raw) = aube_util::env::config_env("CACHE_DIR")
        && let Some(raw) = raw.to_str()
        && !raw.is_empty()
    {
        return expand_setting_path(raw, cwd).unwrap_or_else(platform_default);
    }
    with_settings_ctx(cwd, |ctx| match aube_settings::resolved::cache_dir(ctx) {
        Some(raw) => expand_setting_path(&raw, cwd).unwrap_or_else(platform_default),
        None => platform_default(),
    })
}

/// Absolute path of the global virtual store — the shared tree of
/// materialized packages that project `node_modules/.aube/<dep_path>`
/// entries symlink into.
///
/// `globalVirtualStoreDir` wins when set and is used verbatim (no
/// subdir suffix); otherwise the store lands under the resolved
/// [`resolved_cache_dir`], in the active embedder's virtual-store
/// subdir (`virtual-store` standalone, the host's own name when
/// embedded) so it matches [`aube_store::Store::virtual_store_dir`].
/// The dedicated setting exists
/// because this tree — unlike the rest of the cache — has to sit on
/// the same volume as `storeDir` to be hardlinkable, which is not
/// necessarily where the packument caches belong.
///
/// Every read-side caller (layout detection, mode-change reset) must
/// resolve it the same way the install write path does, otherwise a
/// relocated store makes them look at an empty directory and silently
/// re-materialize.
pub(crate) fn global_virtual_store_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    let from_setting = with_settings_ctx(cwd, |ctx| {
        let raw = aube_settings::resolved::global_virtual_store_dir(ctx)?;
        expand_setting_path(&raw, cwd)
    });
    from_setting
        .unwrap_or_else(|| resolved_cache_dir(cwd).join(aube_util::embedder().virtual_store_subdir))
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
        // One format, two spellings. An embedder that renames the engine's own
        // lockfile needs a name for it that does not carry the engine's brand;
        // the default and every existing config keep working because `aube` is
        // still accepted and still the default.
        Format::Aube | Format::Nub => aube_lockfile::LockfileKind::Aube,
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
    // Guarded by the same posture the resolver's YAML source obeys: when layout
    // is the embedder's own axis, a workspace-YAML `virtualStoreDir` supplies no
    // value, so counting it as explicit here would send a project down the
    // resolver branch with nothing to resolve.
    let has_explicit_yaml = aube_util::engine_context().read_layout_from_workspace_yaml
        && ctx.workspace_yaml.contains_key("virtualStoreDir");
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
    // `set_embedder_defaults` (e.g. a host that wants `node_modules/.store`)
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
    packument_cache_dir_for_cwd(&cwd)
}

/// Disk cache directory for packument metadata resolved against an explicit
/// project directory. Embedders must use this form instead of logical cwd.
pub(crate) fn packument_cache_dir_for_cwd(cwd: &std::path::Path) -> std::path::PathBuf {
    resolved_cache_dir(cwd).join("packuments-v1")
}

/// Disk cache directory for *full* (non-corgi) packument JSON used by
/// human-facing commands like `aube view`. Separate from the corgi cache
/// because the shapes differ.
pub(crate) fn packument_full_cache_dir() -> std::path::PathBuf {
    let cwd = crate::dirs::cwd().unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    resolved_cache_dir(&cwd).join("packuments-full-v1")
}

/// [`packument_full_cache_dir`] against an explicit project dir instead of
/// the process-global logical cwd — for embed-safe paths like
/// `add_to_project`, which must not consult the host process's cwd.
pub(crate) fn packument_full_cache_dir_for_cwd(cwd: &std::path::Path) -> std::path::PathBuf {
    resolved_cache_dir(cwd).join("packuments-full-v1")
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
            managed_aube_config: &[],
            project_aube_config: &[],
            project_npmrc: &[],
            project_config: &[],
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
            managed_aube_config: &[],
            project_aube_config: &[],
            project_npmrc: npmrc,
            project_config: &[],
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
