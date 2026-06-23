use std::path::{Path, PathBuf};

use super::env::{bun_env_entries_from, bun_env_entries_from_std, npm_config_env_entries_from};
use super::npmrc::{parse_npmrc, parse_npmrc_untrusted};
use super::types::{NpmConfig, NpmrcSource};
use super::yarnrc;

/// Whether the loader reads pnpm's global `~/.config/pnpm/auth.ini`
/// (`<XDG_CONFIG_HOME>/pnpm/auth.ini`). Sourced from the engine context's
/// `read_pnpm_global_config` posture; defaults to `true` (upstream behavior:
/// the file is read on every load and its auth tokens merged into the
/// user-scope config).
///
/// This is a GLOBAL / user-scope pnpm-named file, so it is gated by
/// `read_pnpm_global_config` — NOT by the project-derived
/// `read_branded_pnpm_config`. Global config has no project incumbent, so an
/// embedder whose global config is its own neutral surface (e.g. nub) clears
/// `read_pnpm_global_config` UNCONDITIONALLY and this file is never read,
/// regardless of the cwd's incumbent PM. The `.npmrc` / `npmrcAuthFile`
/// sources are unaffected; only the pnpm-named `auth.ini` is gated.
fn pnpm_auth_ini_enabled() -> bool {
    aube_util::engine_context().read_pnpm_global_config
}

impl NpmConfig {
    /// Load config by reading npmrc-style sources in priority order:
    /// builtin, global, user, project, auth sidecars, supported incumbent
    /// adapters, environment.
    ///
    /// Higher-precedence sources override lower-precedence values. Shares file
    /// discovery with [`load_npmrc_entries`] so the registry client and
    /// the generic settings resolver (`aube_cli::settings_values`) can
    /// never disagree on precedence.
    pub fn load(project_dir: &Path) -> Self {
        let env: Vec<(String, String)> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        Self::load_with_env(project_dir, &env)
    }

    /// Test-only loader that reads `project_dir/.npmrc` with a
    /// tempdir pinned as the user's `$HOME` and no env-var merge, so
    /// the developer's real `~/.npmrc` and `NPM_CONFIG_*` vars can't
    /// bleed into assertions. Returns a config seeded the same way
    /// [`NpmConfig::load`] does (npmjs default registry, builtin `@jsr`
    /// scope), so assertions that pin `.registry` or scoped lookups
    /// behave the same as they would on a fresh user machine.
    ///
    /// Keep the `TempDir` binding alive inside the function scope:
    /// `load_npmrc_entries_with_home` reads the files synchronously
    /// and returns before the tempdir drops, so callers don't need to
    /// juggle the handle themselves.
    #[cfg(test)]
    pub(crate) fn load_isolated(project_dir: &Path) -> Self {
        let home = tempfile::tempdir().expect("tempdir for isolated config load");
        let mut config = Self {
            registry: "https://registry.npmjs.org/".to_string(),
            ..Default::default()
        };
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            project_dir,
            None,
        ));
        config.apply_builtin_scoped_defaults();
        config
    }

    /// Same as [`NpmConfig::load`] but takes a captured env snapshot
    /// instead of reading `std::env` directly. Tests that assert on
    /// file-only behavior pass an empty slice so `npm_config_*` vars
    /// leaking from the developer's shell can't perturb the result.
    pub(crate) fn load_with_env(project_dir: &Path, env: &[(String, String)]) -> Self {
        let mut config = Self {
            registry: "https://registry.npmjs.org/".to_string(),
            ..Default::default()
        };
        // Feed tagged entries so `apply_tagged` can reject
        // high-privilege settings sourced from untrusted locations.
        let xdg = aube_util::env::xdg_config_home();
        let home = home_dir();
        // `NPM_CONFIG_USERCONFIG` / `npm_config_userconfig` move the
        // user-level `.npmrc` off the default `$HOME/.npmrc`. npm and
        // pnpm both honor this for XDG layouts and CI secret mounts.
        // Resolve once from the captured env slice and pass it to the
        // loader so tests that drive `load_with_env` can exercise the
        // same code path without mutating process-wide env.
        let user_rc_override = userconfig_override_from_env(env, home.as_deref());
        // System/admin-scoped npmrc (builtin < global), resolved from the
        // same captured env slice so this stays hermetic for tests.
        let global_paths = resolve_global_npmrc_paths(|name| {
            env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
        });
        let tagged = load_npmrc_entries_tagged_with_globals(
            home.as_deref(),
            xdg.as_deref(),
            project_dir,
            user_rc_override.as_deref(),
            &global_paths,
        );
        let mut tagged = merge_yarnrc_tagged_entries(tagged, home.as_deref(), project_dir);
        if aube_util::engine_context().read_yarn_config {
            tagged.extend(
                yarnrc::yarn_env_entries_from(env)
                    .into_iter()
                    .map(|(k, v)| (NpmrcSource::Env, k, v)),
            );
        }
        // `npm_config_*` / `NPM_CONFIG_*` env vars beat file config in
        // npm/pnpm. Apply them after `.npmrc` so last-write-wins gives
        // env the higher slot, and tag them as `Env` so
        // subprocess-settings gating still trusts them.
        tagged.extend(
            npm_config_env_entries_from(env)
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Env, k, v)),
        );
        // Bun's `BUN_CONFIG_REGISTRY` / `BUN_CONFIG_TOKEN` outrank the
        // `npm_config_*` registry/token, so append them last (last-write-wins).
        if aube_util::engine_context().read_bun_config {
            tagged.extend(
                bun_env_entries_from(env)
                    .into_iter()
                    .map(|(k, v)| (NpmrcSource::Env, k, v)),
            );
        }
        config.apply_tagged(tagged);
        // Env vars fill in any proxy fields the .npmrc didn't set.
        // npm/pnpm/curl all check both the upper- and lowercase forms.
        config.apply_proxy_env();
        config.apply_builtin_scoped_defaults();
        config
    }
}

/// Scope-split view of [`load_npmrc_entries`]. Returns user-scope
/// entries (user `~/.npmrc` + pnpm `auth.ini`) and project-scope entries
/// (project `<cwd>/.npmrc` + `npmrcAuthFile`) as separate slices so the
/// settings resolver can apply the locality principle (project beats
/// user) while interleaving aube's own config sources.
///
/// Concatenating `user` and `project` (in that order) yields the same
/// list as [`load_npmrc_entries`].
pub fn load_npmrc_entries_split(project_dir: &Path) -> SplitNpmrcEntries {
    use std::sync::{Mutex, OnceLock};
    type CacheMap = std::collections::HashMap<NpmrcCacheKey, SplitNpmrcEntries>;
    static CACHE: OnceLock<Mutex<CacheMap>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let key = npmrc_cache_key(project_dir);
    if let Ok(map) = cache.lock()
        && let Some(hit) = map.get(&key)
    {
        return hit.clone();
    }
    let xdg = aube_util::env::xdg_config_home();
    let home = home_dir();
    let user_rc_override =
        userconfig_env_value().and_then(|raw| expand_userconfig_path(&raw, home.as_deref()));
    let global_paths = resolve_global_npmrc_paths_from_std_env();
    let tagged = load_npmrc_entries_tagged_with_globals(
        home.as_deref(),
        xdg.as_deref(),
        project_dir,
        user_rc_override.as_deref(),
        &global_paths,
    );
    let tagged = merge_yarnrc_tagged_entries(tagged, home.as_deref(), project_dir);
    let yarn_env = if aube_util::engine_context().read_yarn_config {
        yarnrc::yarn_env_entries_from_std()
    } else {
        Vec::new()
    };
    let bun_env = if aube_util::engine_context().read_bun_config {
        bun_env_entries_from_std()
    } else {
        Vec::new()
    };
    let mut split = SplitNpmrcEntries::default();
    for (src, k, v) in tagged {
        match src {
            // Builtin + global sit below user in precedence but are
            // still non-project (locality: project wins over all of
            // them). They're emitted ahead of user by the walker, so
            // pushing them into the same bucket preserves
            // builtin < global < user order.
            NpmrcSource::Builtin
            | NpmrcSource::Global
            | NpmrcSource::User
            | NpmrcSource::PnpmAuth
            | NpmrcSource::UserNpmrcAuthFile => {
                split.user.push((k, v));
            }
            NpmrcSource::Project | NpmrcSource::ProjectNpmrcAuthFile => split.project.push((k, v)),
            // Env-derived entries (npm_config_*) aren't loaded by the
            // tagged file walker, so this arm is unreachable here.
            NpmrcSource::Env => continue,
        }
    }
    split.project.extend(yarn_env);
    split.project.extend(bun_env);
    if let Ok(mut map) = cache.lock() {
        map.insert(key, split.clone());
    }
    split
}

#[derive(Default, Clone)]
pub struct SplitNpmrcEntries {
    pub user: Vec<(String, String)>,
    pub project: Vec<(String, String)>,
}

/// Load only non-project npmrc-shaped sources: builtin/global npmrc,
/// synthetic user entries, user `.npmrc`/`NPM_CONFIG_USERCONFIG`,
/// pnpm `auth.ini`, user-scoped `npmrc-auth-file`, and user `.yarnrc.yml`
/// when Yarn mirroring is enabled. This is intentionally narrower than
/// [`load_npmrc_entries_split`]: scoped config commands must not touch
/// project files that can block or be attacker-controlled.
pub fn load_user_npmrc_entries(project_dir: &Path) -> Vec<(String, String)> {
    let xdg = aube_util::env::xdg_config_home();
    let home = home_dir();
    let user_rc_override =
        userconfig_env_value().and_then(|raw| expand_userconfig_path(&raw, home.as_deref()));
    let global_paths = resolve_global_npmrc_paths_from_std_env();
    let mut tagged = load_user_npmrc_entries_tagged(
        home.as_deref(),
        xdg.as_deref(),
        project_dir,
        user_rc_override.as_deref(),
        &global_paths,
    );
    tagged.extend(
        yarnrc::load_user_yarnrc_entries(home.as_deref())
            .into_iter()
            .map(|(k, v)| (NpmrcSource::User, k, v)),
    );
    tagged.into_iter().map(|(_, k, v)| (k, v)).collect()
}

/// Load only project-controlled npmrc-shaped sources: synthetic project
/// entries, project `.npmrc`, a project-scoped `npmrc-auth-file`, project
/// `.yarnrc.yml` files, and Yarn env entries when Yarn mirroring is enabled.
/// It deliberately skips user/global npmrc, user auth sidecars, and pnpm's
/// user/global `auth.ini`.
pub fn load_project_npmrc_entries(project_dir: &Path) -> Vec<(String, String)> {
    let home = home_dir();
    let mut tagged = load_project_npmrc_entries_tagged(home.as_deref(), project_dir);
    tagged.extend(
        yarnrc::load_project_yarnrc_entries(project_dir)
            .into_iter()
            .map(|(k, v)| (NpmrcSource::Project, k, v)),
    );
    if aube_util::engine_context().read_yarn_config {
        tagged.extend(
            yarnrc::yarn_env_entries_from_std()
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Env, k, v)),
        );
    }
    if aube_util::engine_context().read_bun_config {
        tagged.extend(
            bun_env_entries_from_std()
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Env, k, v)),
        );
    }
    tagged.into_iter().map(|(_, k, v)| (k, v)).collect()
}

/// Load raw `.npmrc` key/value pairs from the same file precedence as
/// [`NpmConfig::load`]: user-level (`~/.npmrc`) first, then project-level
/// (`<cwd>/.npmrc`). Returned in encounter order — a later duplicate key
/// overrides an earlier one, matching npm's own precedence rules.
///
/// Callers that want typed, per-setting values should consume this via
/// `aube_cli::settings_values`, which walks `settings_meta::SETTINGS` and
/// looks up each setting's declared `sources.npmrc` keys. That keeps the
/// registry of "which keys map to which setting" in `settings.toml`
/// instead of scattering it through a hand-rolled parser.
pub fn load_npmrc_entries(project_dir: &Path) -> Vec<(String, String)> {
    // Process-wide memoization keyed by project_dir. `.npmrc` files are
    // not expected to change mid-install, and callers on the hot path
    // (main startup, `with_settings_ctx`, install::run) invoke this
    // repeatedly with the same path. Same pattern as
    // `aube_lockfile::aube_lock_filename`.
    use std::sync::{Mutex, OnceLock};
    type CacheMap = std::collections::HashMap<NpmrcCacheKey, Vec<(String, String)>>;
    static CACHE: OnceLock<Mutex<CacheMap>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let key = npmrc_cache_key(project_dir);
    if let Ok(map) = cache.lock()
        && let Some(hit) = map.get(&key)
    {
        return hit.clone();
    }
    // Read `XDG_CONFIG_HOME` only on the public entry point so that
    // `pnpm` and `aube` agree on where `~/.config/pnpm/auth.ini`
    // resolves when the user has a non-default XDG layout. The env
    // read is confined here — the `_with_home` helper keeps taking an
    // explicit override so tests don't inherit the developer's real
    // `XDG_CONFIG_HOME` and pick up whatever auth tokens live there.
    let xdg = aube_util::env::xdg_config_home();
    let home = home_dir();
    // `*_CONFIG_USERCONFIG` relocates the user-level `.npmrc` (XDG
    // layouts, `~/.config/npm/npmrc`, etc.). Read directly rather than
    // collecting `std::env::vars()` — we only need these keys, and
    // confining the env read to the public entry point keeps
    // `_with_home` fully injectable for tests. See
    // [`userconfig_env_value`] for the spelling precedence and the
    // pnpm-incumbent gate on the `PNPM_CONFIG_*` forms.
    let user_rc_override =
        userconfig_env_value().and_then(|raw| expand_userconfig_path(&raw, home.as_deref()));
    let global_paths = resolve_global_npmrc_paths_from_std_env();
    let tagged = load_npmrc_entries_tagged_with_globals(
        home.as_deref(),
        xdg.as_deref(),
        project_dir,
        user_rc_override.as_deref(),
        &global_paths,
    );
    let mut tagged = merge_yarnrc_tagged_entries(tagged, home.as_deref(), project_dir);
    if aube_util::engine_context().read_yarn_config {
        tagged.extend(
            yarnrc::yarn_env_entries_from_std()
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Env, k, v)),
        );
    }
    if aube_util::engine_context().read_bun_config {
        tagged.extend(
            bun_env_entries_from_std()
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Env, k, v)),
        );
    }
    let entries = tagged
        .into_iter()
        .map(|(_, k, v)| (k, v))
        .collect::<Vec<_>>();
    if let Ok(mut map) = cache.lock() {
        map.insert(key, entries.clone());
    }
    entries
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NpmrcCacheKey {
    project_dir: PathBuf,
    read_branded_pnpm_config: bool,
    read_yarn_config: bool,
    synthetic_user_npmrc_entries: Vec<(String, String)>,
    synthetic_project_npmrc_entries: Vec<(String, String)>,
    yarn_env_entries: Vec<(String, String)>,
    bun_env_entries: Vec<(String, String)>,
}

fn npmrc_cache_key(project_dir: &Path) -> NpmrcCacheKey {
    let ctx = aube_util::engine_context();
    let yarn_env_entries = if ctx.read_yarn_config {
        yarnrc::yarn_env_entries_from_std()
    } else {
        Vec::new()
    };
    let bun_env_entries = if ctx.read_bun_config {
        bun_env_entries_from_std()
    } else {
        Vec::new()
    };
    NpmrcCacheKey {
        project_dir: project_dir.to_path_buf(),
        read_branded_pnpm_config: ctx.read_branded_pnpm_config,
        read_yarn_config: ctx.read_yarn_config,
        synthetic_user_npmrc_entries: ctx.synthetic_user_npmrc_entries,
        synthetic_project_npmrc_entries: ctx.synthetic_project_npmrc_entries,
        yarn_env_entries,
        bun_env_entries,
    }
}

/// The two system/admin-scoped npmrc files that sit *below* the user
/// `.npmrc` in npm's config cascade: the builtin `npmrc` shipped next to
/// the npm CLI, and the global `npmrc` (`$PREFIX/etc/npmrc`). Both are
/// resolved from the environment ([`resolve_global_npmrc_paths`]) at the
/// public entry points and injected here so the walker stays testable.
/// Either may be `None` when the path can't be located — aube does not
/// fabricate a path, the scope is simply absent.
#[derive(Default, Clone)]
pub(super) struct GlobalNpmrcPaths {
    /// `resolve(npmPath, 'npmrc')` — the builtin layer.
    pub builtin: Option<PathBuf>,
    /// `$PREFIX/etc/npmrc` or `NPM_CONFIG_GLOBALCONFIG`.
    pub global: Option<PathBuf>,
}

/// Same as [`load_npmrc_entries_with_home`] but each entry is tagged
/// with the file it came from. `apply_tagged` uses the tag to refuse
/// high-privilege settings (currently `tokenHelper`) that originated
/// from a project-scope `.npmrc` a hostile repo can commit.
///
/// This entry point reads no global/builtin npmrc (passes
/// [`GlobalNpmrcPaths::default`]); the production loaders go through
/// [`load_npmrc_entries_tagged_with_globals`], which resolves those
/// system-scoped files from the environment. Test-only: every production
/// path resolves the global scopes, so this shim exists purely to keep the
/// many fixtures that don't exercise builtin/global terse.
#[cfg(test)]
pub(super) fn load_npmrc_entries_tagged_with_home(
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
    project_dir: &Path,
    user_rc_override: Option<&Path>,
) -> Vec<(NpmrcSource, String, String)> {
    load_npmrc_entries_tagged_with_globals(
        home,
        xdg_config_home,
        project_dir,
        user_rc_override,
        &GlobalNpmrcPaths::default(),
    )
}

/// Tagged walker over the full npm config-file cascade, lowest → highest
/// precedence: **builtin < global < user < project** (env and CLI sit
/// above and are layered on by the caller). The builtin and global
/// `npmrc` are admin/system-controlled, so they parse as *trusted*
/// (`${VAR}` expansion on, `tokenHelper`/proxy/`strict-ssl` allowed) —
/// an admin who baked a setting into the installed toolchain or the
/// global config is at least as trusted as the user's own `.npmrc`. Only
/// the project `.npmrc` (and a project-pointed auth file) is untrusted.
pub(super) fn load_npmrc_entries_tagged_with_globals(
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
    project_dir: &Path,
    user_rc_override: Option<&Path>,
    global_paths: &GlobalNpmrcPaths,
) -> Vec<(NpmrcSource, String, String)> {
    let mut out: Vec<(NpmrcSource, String, String)> = Vec::new();
    // Builtin npmrc (lowest precedence): the `npmrc` shipped next to the
    // npm CLI. System-controlled, so trusted (env expansion on).
    if let Some(builtin_rc) = global_paths.builtin.as_deref()
        && builtin_rc.exists()
        && let Ok(entries) = parse_npmrc(builtin_rc)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Builtin, k, v)),
        );
    }
    // Global npmrc (`$PREFIX/etc/npmrc` / `NPM_CONFIG_GLOBALCONFIG`):
    // admin-controlled corporate/CI config. Sits below user, above
    // builtin. Trusted.
    if let Some(global_rc) = global_paths.global.as_deref()
        && global_rc.exists()
        && let Ok(entries) = parse_npmrc(global_rc)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Global, k, v)),
        );
    }
    let engine_context = aube_util::engine_context();
    out.extend(
        engine_context
            .synthetic_user_npmrc_entries
            .iter()
            .cloned()
            .map(|(k, v)| (NpmrcSource::User, k, v)),
    );
    out.extend(
        engine_context
            .synthetic_project_npmrc_entries
            .iter()
            .cloned()
            .map(|(k, v)| (NpmrcSource::Project, k, v)),
    );
    // User-level rc: explicit override (from `NPM_CONFIG_USERCONFIG`)
    // wins over `$HOME/.npmrc`. Keeps the `User` source tag either
    // way — the user chose the file location, so `apply_tagged`'s
    // trust level is unchanged. The pnpm `auth.ini` is a separate
    // file under `$HOME`/`XDG_CONFIG_HOME` and is not affected by
    // the userconfig override.
    let user_rc = user_rc_override
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".npmrc")));
    if let Some(user_rc) = user_rc.as_deref()
        && user_rc.exists()
        && let Ok(entries) = parse_npmrc(user_rc)
    {
        out.extend(entries.into_iter().map(|(k, v)| (NpmrcSource::User, k, v)));
    }
    if let Some(home) = home
        && pnpm_auth_ini_enabled()
    {
        let auth_ini = pnpm_global_auth_ini_path(home, xdg_config_home);
        if auth_ini.exists()
            && let Ok(entries) = parse_npmrc(&auth_ini)
        {
            out.extend(
                entries
                    .into_iter()
                    .map(|(k, v)| (NpmrcSource::PnpmAuth, k, v)),
            );
        }
    }
    if let Some((auth_path, auth_source)) = resolve_npmrc_auth_file_tagged(home, project_dir, &out)
        && !auth_source.is_project_controlled()
        && auth_path.exists()
        && let Ok(entries) = parse_npmrc(&auth_path)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::UserNpmrcAuthFile, k, v)),
        );
    }
    let project_rc = project_dir.join(".npmrc");
    if project_rc.exists()
        && !is_same_npmrc_file(&project_rc, user_rc.as_deref())
        && let Ok(entries) = parse_npmrc_untrusted(&project_rc)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Project, k, v)),
        );
    }
    if let Some((auth_path, auth_source)) = resolve_npmrc_auth_file_tagged(home, project_dir, &out)
        && auth_source.is_project_controlled()
        && !is_same_npmrc_file(&auth_path, user_rc.as_deref())
        && auth_path.exists()
        && let Ok(entries) = parse_npmrc_untrusted(&auth_path)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::ProjectNpmrcAuthFile, k, v)),
        );
    }
    out
}

fn load_user_npmrc_entries_tagged(
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
    project_dir: &Path,
    user_rc_override: Option<&Path>,
    global_paths: &GlobalNpmrcPaths,
) -> Vec<(NpmrcSource, String, String)> {
    let mut out: Vec<(NpmrcSource, String, String)> = Vec::new();
    if let Some(builtin_rc) = global_paths.builtin.as_deref()
        && builtin_rc.exists()
        && let Ok(entries) = parse_npmrc(builtin_rc)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Builtin, k, v)),
        );
    }
    if let Some(global_rc) = global_paths.global.as_deref()
        && global_rc.exists()
        && let Ok(entries) = parse_npmrc(global_rc)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Global, k, v)),
        );
    }
    out.extend(
        aube_util::engine_context()
            .synthetic_user_npmrc_entries
            .iter()
            .cloned()
            .map(|(k, v)| (NpmrcSource::User, k, v)),
    );
    let user_rc = user_rc_override
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".npmrc")));
    if let Some(user_rc) = user_rc
        && user_rc.exists()
        && let Ok(entries) = parse_npmrc(&user_rc)
    {
        out.extend(entries.into_iter().map(|(k, v)| (NpmrcSource::User, k, v)));
    }
    if let Some(home) = home
        && pnpm_auth_ini_enabled()
    {
        let auth_ini = pnpm_global_auth_ini_path(home, xdg_config_home);
        if auth_ini.exists()
            && let Ok(entries) = parse_npmrc(&auth_ini)
        {
            out.extend(
                entries
                    .into_iter()
                    .map(|(k, v)| (NpmrcSource::PnpmAuth, k, v)),
            );
        }
    }
    if let Some((auth_path, _auth_source)) = resolve_npmrc_auth_file_tagged(home, project_dir, &out)
        && auth_path.exists()
        && let Ok(entries) = parse_npmrc(&auth_path)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::UserNpmrcAuthFile, k, v)),
        );
    }
    out
}

fn load_project_npmrc_entries_tagged(
    home: Option<&Path>,
    project_dir: &Path,
) -> Vec<(NpmrcSource, String, String)> {
    let mut out: Vec<(NpmrcSource, String, String)> = Vec::new();
    out.extend(
        aube_util::engine_context()
            .synthetic_project_npmrc_entries
            .iter()
            .cloned()
            .map(|(k, v)| (NpmrcSource::Project, k, v)),
    );
    let user_rc = resolve_user_rc_path(home);
    let project_rc = project_dir.join(".npmrc");
    if project_rc.exists()
        && !is_same_npmrc_file(&project_rc, user_rc.as_deref())
        && let Ok(entries) = parse_npmrc_untrusted(&project_rc)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Project, k, v)),
        );
    }
    if let Some((auth_path, _auth_source)) = resolve_npmrc_auth_file_tagged(home, project_dir, &out)
        && auth_path.exists()
        && !is_same_npmrc_file(&auth_path, user_rc.as_deref())
        && let Ok(entries) = parse_npmrc_untrusted(&auth_path)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::ProjectNpmrcAuthFile, k, v)),
        );
    }
    out
}

pub(super) fn merge_yarnrc_tagged_entries(
    tagged: Vec<(NpmrcSource, String, String)>,
    home: Option<&Path>,
    project_dir: &Path,
) -> Vec<(NpmrcSource, String, String)> {
    let yarnrc = yarnrc::load_yarnrc_entries_split(home, project_dir);
    if yarnrc.user.is_empty() && yarnrc.project.is_empty() {
        return tagged;
    }
    let mut out = Vec::with_capacity(tagged.len() + yarnrc.user.len() + yarnrc.project.len());
    let mut inserted_user_yarn = false;
    for (source, key, value) in tagged {
        if !inserted_user_yarn && source.is_project_controlled() {
            out.extend(
                yarnrc
                    .user
                    .iter()
                    .cloned()
                    .map(|(k, v)| (NpmrcSource::User, k, v)),
            );
            inserted_user_yarn = true;
        }
        out.push((source, key, value));
    }
    if !inserted_user_yarn {
        out.extend(
            yarnrc
                .user
                .into_iter()
                .map(|(k, v)| (NpmrcSource::User, k, v)),
        );
    }
    out.extend(
        yarnrc
            .project
            .into_iter()
            .map(|(k, v)| (NpmrcSource::Project, k, v)),
    );
    out
}
/// Same as [`load_npmrc_entries`] but with an injectable user-home
/// directory and XDG config-home override. Used by tests that need to
/// isolate from the developer's real `~/.npmrc` and pnpm config dir
/// without mutating process-wide environment variables. Test-only — reads
/// no global/builtin scope.
#[cfg(test)]
pub(super) fn load_npmrc_entries_with_home(
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
    project_dir: &Path,
    user_rc_override: Option<&Path>,
) -> Vec<(String, String)> {
    merge_yarnrc_tagged_entries(
        load_npmrc_entries_tagged_with_home(home, xdg_config_home, project_dir, user_rc_override),
        home,
        project_dir,
    )
    .into_iter()
    .map(|(_, k, v)| (k, v))
    .collect()
}

/// Resolve an `npmrcAuthFile` / `npmrc-auth-file` value to an absolute
/// path. `~` expands against `home`; relative paths resolve against the
/// project root, matching the storeDir convention.
fn resolve_npmrc_auth_file_path(
    home: Option<&Path>,
    project_dir: &Path,
    raw: &str,
) -> Option<PathBuf> {
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        home.map(|h| h.join(rest))?
    } else if raw == "~" {
        home.map(PathBuf::from)?
    } else {
        PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        Some(expanded)
    } else {
        Some(project_dir.join(expanded))
    }
}

fn resolve_npmrc_auth_file_tagged(
    home: Option<&Path>,
    project_dir: &Path,
    entries: &[(NpmrcSource, String, String)],
) -> Option<(PathBuf, NpmrcSource)> {
    let (source, _, raw) = entries
        .iter()
        .rev()
        .find(|(_, k, _)| matches!(k.as_str(), "npmrcAuthFile" | "npmrc-auth-file"))?;
    let path = resolve_npmrc_auth_file_path(home, project_dir, raw)?;
    Some((path, *source))
}

/// Expand a raw `userconfig` / `NPM_CONFIG_USERCONFIG` value into a
/// concrete path, applying the same tilde-expansion rules
/// `npmrc-auth-file` uses so both env-var and `.npmrc`-derived path
/// overrides behave the same way. Empty (after trim) returns
/// `None` so callers can skip a pointless file probe. Relative paths
/// are returned verbatim and resolve against the process cwd when
/// later fed to `exists()` / `parse_npmrc` — matching npm's behavior.
pub(super) fn expand_userconfig_path(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    if trimmed == "~" {
        return home.map(PathBuf::from);
    }
    Some(PathBuf::from(trimmed))
}

/// Resolve the user-scope `.npmrc` path the same way the tagged loader
/// does: the `*_CONFIG_USERCONFIG` env override (highest precedence,
/// honoring the pnpm-incumbent gate) wins, else `$HOME/.npmrc`. Used to
/// recognize when a project-scope read would re-open the *same physical
/// file* as the user rc (e.g. `cwd == $HOME`).
fn resolve_user_rc_path(home: Option<&Path>) -> Option<PathBuf> {
    userconfig_env_value()
        .and_then(|raw| expand_userconfig_path(&raw, home))
        .or_else(|| home.map(|h| h.join(".npmrc")))
}

/// True when `candidate` is the SAME physical file as the user rc — so a
/// project-scope (untrusted) read of it would merely duplicate the
/// user-scope (trusted) read under a wrong trust tag. Compares canonical
/// paths so a symlinked home, a `~/`-spelled override, and the absolute
/// spelling all collapse to one identity; falls back to a lexical compare
/// when either path can't be canonicalized (e.g. it doesn't exist).
///
/// This is purely a same-file dedup: a genuine committed repo `.npmrc`
/// (a different inode from `~/.npmrc`) never matches, so the
/// untrusted-committed-`.npmrc` trust gate is fully preserved. The only
/// case it suppresses is `$HOME/.npmrc` being read twice when the project
/// dir IS the home dir — `$HOME/.npmrc` is npm's canonical user-config
/// path and is always user-trusted, never a project-committed source.
fn is_same_npmrc_file(candidate: &Path, user_rc: Option<&Path>) -> bool {
    let Some(user_rc) = user_rc else {
        return false;
    };
    match (candidate.canonicalize(), user_rc.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => candidate == user_rc,
    }
}

/// Spelling precedence for the `userconfig` relocation env var, highest
/// first. pnpm's branded `PNPM_CONFIG_USERCONFIG` / `pnpm_config_userconfig`
/// outrank the npm-compat `NPM_CONFIG_USERCONFIG` / `npm_config_userconfig`
/// (matching pnpm v11's `readEnvVar` > `readNpmEnvVar` order), and within
/// a family the SCREAMING form is canonical so it wins over the lowercase.
///
/// The pnpm-named entries carry an `incumbent_gated: true` flag: they
/// ride the existing `read_branded_pnpm_config` posture — on-by-default
/// for standalone aube (which IS a pnpm-compatible PM), gated to the
/// pnpm-incumbent check under the nub profile (the pnpm-named-paths hard
/// gate). The npm-compat entries are never gated.
const USERCONFIG_ENV_SPELLINGS: &[(&str, bool)] = &[
    ("PNPM_CONFIG_USERCONFIG", true),
    ("pnpm_config_userconfig", true),
    ("NPM_CONFIG_USERCONFIG", false),
    ("npm_config_userconfig", false),
];

/// Whether a pnpm-named, incumbent-gated env spelling may be read. A
/// pnpm-branded `userconfig` relocation is faithful mirroring of the
/// active PM only when pnpm IS the incumbent; under any other incumbent
/// it's another tool's state and is skipped.
fn pnpm_branded_env_enabled() -> bool {
    aube_util::engine_context().read_branded_pnpm_config
}

/// Read the highest-precedence `*_CONFIG_USERCONFIG` value from the
/// process environment, honoring [`USERCONFIG_ENV_SPELLINGS`] and the
/// pnpm-incumbent gate. Returns the raw (unexpanded) value.
fn userconfig_env_value() -> Option<String> {
    for (name, incumbent_gated) in USERCONFIG_ENV_SPELLINGS {
        if *incumbent_gated && !pnpm_branded_env_enabled() {
            continue;
        }
        if let Ok(v) = std::env::var(name) {
            return Some(v);
        }
    }
    None
}

/// Find the highest-precedence `*_CONFIG_USERCONFIG` value in a captured
/// env slice and expand it, honoring [`USERCONFIG_ENV_SPELLINGS`] and
/// the pnpm-incumbent gate. Positional ordering in the slice can't be
/// the tiebreaker — the typical caller builds it from
/// `std::env::vars()`, which iterates in HashMap order — so we pick by
/// the declared spelling precedence instead. This keeps
/// [`NpmConfig::load_with_env`] agreeing with the direct `std::env::var`
/// chain in [`load_npmrc_entries`], so generic settings and auth config
/// can't resolve to different files on the same host.
pub(super) fn userconfig_override_from_env(
    env: &[(String, String)],
    home: Option<&Path>,
) -> Option<PathBuf> {
    for (spelling, incumbent_gated) in USERCONFIG_ENV_SPELLINGS {
        if *incumbent_gated && !pnpm_branded_env_enabled() {
            continue;
        }
        if let Some((_, raw)) = env.iter().find(|(name, _)| name == spelling) {
            return expand_userconfig_path(raw, home);
        }
    }
    None
}
pub(super) fn home_dir() -> Option<PathBuf> {
    aube_util::env::home_dir()
}

/// Resolve the builtin + global `npmrc` paths the way npm does, reading
/// only the npm-compat env vars (`NPM_CONFIG_GLOBALCONFIG`,
/// `NPM_CONFIG_PREFIX`/`PREFIX`) via the supplied lookup. npm's rules:
///
/// - **global** = `NPM_CONFIG_GLOBALCONFIG` if set, else
///   `$PREFIX/etc/npmrc`, where `$PREFIX` = `NPM_CONFIG_PREFIX` ?? `PREFIX`.
/// - **builtin** = `npmrc` shipped inside the npm install, at
///   `$PREFIX/lib/node_modules/npm/npmrc` (POSIX) or
///   `$PREFIX/node_modules/npm/npmrc` (Windows), overridable via
///   `NPM_CONFIG_BUILTIN_CONFIG`.
///
/// When the prefix can't be determined (no `NPM_CONFIG_PREFIX`/`PREFIX`
/// and no explicit override), the corresponding scope is `None` — aube is
/// embedded and has no reliable way to locate a foreign npm install, so it
/// declines to guess rather than fabricate a path. `existence` is *not*
/// probed here; the walker checks `.exists()` itself.
pub(super) fn resolve_global_npmrc_paths(
    lookup: impl Fn(&str) -> Option<String>,
) -> GlobalNpmrcPaths {
    let non_empty = |s: String| {
        let t = s.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    };
    let read = |name: &str| lookup(name).and_then(non_empty);

    let prefix = read("NPM_CONFIG_PREFIX")
        .or_else(|| read("npm_config_prefix"))
        .or_else(|| read("PREFIX"))
        .map(PathBuf::from);

    let global = read("NPM_CONFIG_GLOBALCONFIG")
        .or_else(|| read("npm_config_globalconfig"))
        .map(PathBuf::from)
        .or_else(|| prefix.as_ref().map(|p| p.join("etc").join("npmrc")));

    let builtin = read("NPM_CONFIG_BUILTIN_CONFIG")
        .or_else(|| read("npm_config_builtin_config"))
        .map(PathBuf::from)
        .or_else(|| prefix.as_ref().map(|p| p.join(builtin_npm_subpath())));

    GlobalNpmrcPaths { builtin, global }
}

/// Path from the npm install prefix to the bundled npm package's `npmrc`.
/// POSIX installs nest packages under `lib/node_modules`; Windows omits
/// the `lib` segment.
fn builtin_npm_subpath() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("node_modules").join("npm").join("npmrc")
    } else {
        PathBuf::from("lib")
            .join("node_modules")
            .join("npm")
            .join("npmrc")
    }
}

/// [`resolve_global_npmrc_paths`] reading the process environment.
fn resolve_global_npmrc_paths_from_std_env() -> GlobalNpmrcPaths {
    resolve_global_npmrc_paths(|name| std::env::var(name).ok())
}

/// Resolve the path to pnpm's global auth file: `<configDir>/auth.ini`,
/// where `configDir` is pnpm's per-OS config directory
/// ([`aube_util::env::pnpm_config_dir_with`]). When `xdg_config_home` is
/// supplied (production reads it from `$XDG_CONFIG_HOME` in
/// [`load_npmrc_entries`]; tests inject an override or `None`) the file
/// lives at `<xdg>/pnpm/auth.ini` on every OS; otherwise it follows the
/// platform default — macOS `~/Library/Preferences/pnpm`, Windows
/// `%LOCALAPPDATA%\pnpm\config`, Linux `~/.config/pnpm`. A flat
/// `~/.config/pnpm` is correct only on Linux, so the previous
/// home-joined fallback read the wrong location on a stock macOS or
/// Windows box and silently missed the user's `auth.ini` there.
fn pnpm_global_auth_ini_path(home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
    let config_dir = aube_util::env::pnpm_config_dir_with(Some(home), xdg_config_home)
        // `home` is always `Some` at every call site (guarded by
        // `if let Some(home) = home`), so the helper only returns `None`
        // when both home and XDG are absent — impossible here. Keep a
        // defined fallback rather than unwrap so a future caller change
        // can't panic.
        .unwrap_or_else(|| home.join(".config").join("pnpm"));
    config_dir.join("auth.ini")
}
