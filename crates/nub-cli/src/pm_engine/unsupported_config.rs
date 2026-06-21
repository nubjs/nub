//! Unsupported-config detection + the cheap config-driven install wins.
//!
//! Two halves, both grounded in the same per-incumbent config readers:
//!
//! **A) IMPLEMENT-wins** — config that nub's existing machinery can honor once
//! it's read. Rather than warn/error on these, nub mirrors the incumbent:
//!   1. Dep-type selection — npm `.npmrc` `omit`/`include`, bun bunfig
//!      `[install].production` → the engine's `DepSelection`
//!      (`--prod`/`--dev`/`--no-optional` axis).
//!   2. Frozen-from-config — bun bunfig in-file `frozenLockfile`, yarn
//!      `enableImmutableInstalls`/`immutablePatterns` → the engine's frozen
//!      mode (same path `--frozen-lockfile` takes).
//!   3. `enableScripts: false` (yarn) → force a block-all-builds policy that
//!      overrides even nub's curated default-trust floor.
//!   4. `dependenciesMeta.*.injected` → auto-switch to the isolated linker (the
//!      only layout where aube materializes injected copies) instead of
//!      silently dropping the directive under nub's hoisted default.
//!
//! (`minimumReleaseAge` from bunfig is wired in [`super::bun_config`] — it maps
//! to a synthetic `.npmrc` entry the settings registry already reads.)
//!
//! **B) The scan** ([`scan_unsupported_config`]) — for the genuinely-hard set
//! that nub does NOT implement, a curated FATAL/WARN sweep so the launch claim
//! "nub aborts if unsupported config is detected" holds. FATAL fields abort
//! with an `ERR_NUB_*` code + a remedy (no `--force`); WARN fields proceed with
//! a dim line. NOT a blanket unknown-key warn — only a curated load-bearing set.
//!
//! All readers are name-gated by the resolved [`Role`]: a field is only read
//! from a config surface the active PM owns (an `.npmrc` `omit` is npm's; a
//! `bunfig.toml` key is bun's; a `.yarnrc.yml` key is yarn's), matching the
//! symmetric brand-boundary discipline the rest of `pm_engine` enforces.

use std::path::{Path, PathBuf};

use super::config_scope::{IgnoredField, Role};

/// A config-derived override of the dependency selection axis
/// (`--prod`/`--dev`/`--no-optional`). `None` per field means "not pinned by
/// config — leave the CLI/default behavior". Composed onto the install args
/// only when the active PM owns the config that set it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DepSelectionConfig {
    pub(crate) prod: bool,
    pub(crate) dev: bool,
    pub(crate) no_optional: bool,
}

impl DepSelectionConfig {
    fn is_empty(self) -> bool {
        !self.prod && !self.dev && !self.no_optional
    }
}

/// Read the dependency-selection axis the active PM's persistent config pins.
///
/// - **npm** — `.npmrc` `omit` / `include` (comma- or space-separated lists of
///   `dev` / `optional` / `peer`). `omit=dev` ⇒ prod; `omit=optional` ⇒
///   no-optional; `include=` un-sets a prior `omit` of the same type (npm's own
///   precedence: `include` wins). nub honors the `--prod`/`--no-optional`
///   *flags* already; this is the persistent `.npmrc` spelling of the same.
/// - **bun** — bunfig `[install].production = true` ⇒ prod (omit devDeps).
///
/// Returns `None` (no pin) for roles whose config carries no dep-axis signal,
/// or when the config doesn't set one. The CLI flag still composes on top —
/// this only seeds the default when a flag is absent.
pub(crate) fn dep_selection_from_config(role: Role, root: &Path) -> Option<DepSelectionConfig> {
    let cfg = match role {
        Role::Npm => npm_omit_include(root),
        Role::Bun => bunfig_production(root),
        // pnpm/yarn/nub: no persistent dep-axis config nub doesn't already
        // read through its own surfaces.
        Role::Pnpm | Role::Yarn | Role::Nub => DepSelectionConfig::default(),
    };
    (!cfg.is_empty()).then_some(cfg)
}

/// npm `.npmrc` `omit` / `include` → dep-selection axis. Reads the project
/// `.npmrc` (walk-up to the root) then the user `~/.npmrc`, project winning.
fn npm_omit_include(root: &Path) -> DepSelectionConfig {
    // Collect `omit` / `include` from user then project so project wins.
    let mut omit: Vec<String> = Vec::new();
    let mut include: Vec<String> = Vec::new();
    for path in npmrc_paths(root) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(v) = npmrc_scalar(&content, "omit") {
            omit = split_list(&v);
        }
        if let Some(v) = npmrc_scalar(&content, "include") {
            include = split_list(&v);
        }
    }
    // npm: `include` removes a type from the effective omit set.
    let omits = |ty: &str| omit.iter().any(|o| o == ty) && !include.iter().any(|i| i == ty);
    DepSelectionConfig {
        prod: omits("dev"),
        dev: false,
        no_optional: omits("optional"),
    }
}

/// bun bunfig `[install].production = true` → prod (omit devDependencies).
fn bunfig_production(root: &Path) -> DepSelectionConfig {
    let prod = bunfig_install_bool(root, "production").unwrap_or(false);
    DepSelectionConfig {
        prod,
        dev: false,
        no_optional: false,
    }
}

/// Whether the active PM's config requests a frozen / immutable install — the
/// in-file / config spellings of `--frozen-lockfile` that nub's CLI flag path
/// already honors but the config readers do not.
///
/// - **bun** — bunfig `[install].frozenLockfile = true`.
/// - **yarn** — `.yarnrc.yml` `enableImmutableInstalls: true` (Berry's default
///   in CI) or a non-empty `immutablePatterns` list. Either is the Yarn
///   `--immutable` contract: abort if the lockfile would change.
///
/// Maps to `FrozenMode::Frozen` (the strict CI guard), mirroring what the real
/// PM does. The CLI `--no-frozen-lockfile` still overrides (it's applied after).
pub(crate) fn frozen_from_config(role: Role, root: &Path) -> bool {
    match role {
        Role::Bun => bunfig_install_bool(root, "frozenLockfile").unwrap_or(false),
        Role::Yarn => yarn_immutable(root),
        Role::Npm | Role::Pnpm | Role::Nub => false,
    }
}

/// yarn `.yarnrc.yml` `enableImmutableInstalls: true` or a non-empty
/// `immutablePatterns:` block.
fn yarn_immutable(root: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(root.join(".yarnrc.yml")) else {
        return false;
    };
    if yarnrc_top_level_bool(&content, "enableImmutableInstalls") == Some(true) {
        return true;
    }
    // `immutablePatterns:` followed by an indented list ⇒ non-empty.
    yarnrc_block_nonempty(&content, "immutablePatterns")
}

/// Whether yarn's `enableScripts: false` is set — the security opt-out that
/// disables ALL lifecycle scripts. When true the install must force a
/// block-all-builds policy that overrides even nub's curated default-trust floor.
pub(crate) fn yarn_scripts_disabled(role: Role, root: &Path) -> bool {
    if role != Role::Yarn {
        return false;
    }
    std::fs::read_to_string(root.join(".yarnrc.yml"))
        .ok()
        .and_then(|c| yarnrc_top_level_bool(&c, "enableScripts"))
        == Some(false)
}

/// Whether yarn's `enableNetwork: false` is set in `.yarnrc.yml` — Berry's
/// network opt-out, which forces an OFFLINE install (serve only on-disk cache,
/// error on a miss). Berry's `--offline` flag is itself sugar for this config
/// field, so honoring the field covers both. Maps to `NetworkMode::Offline`,
/// the same mode nub's `--offline` CLI flag takes. The CLI flag still composes
/// on top (it's OR'd in `run_install`/`run_ci`).
pub(crate) fn yarn_network_disabled(role: Role, root: &Path) -> bool {
    if role != Role::Yarn {
        return false;
    }
    std::fs::read_to_string(root.join(".yarnrc.yml"))
        .ok()
        .and_then(|c| yarnrc_top_level_bool(&c, "enableNetwork"))
        == Some(false)
}

/// Whether a classic `.yarnrc` (Yarn 1, NOT `.yarnrc.yml`) configures a
/// `yarn-offline-mirror` — a local tarball-mirror directory installs are meant
/// to read from. nub installs from its content-addressable store and the
/// registry; it has no affordance to read a configured mirror dir, so in
/// offline mode (where the mirror is the user's intended package source) this
/// must FAIL LOUD rather than silently hit the public registry. Online, the
/// mirror is moot, so this is consulted only on the offline-mode path.
///
/// The classic `.yarnrc` is a space-separated `key "value"` format (parsed by
/// Yarn 1's own lockfile parser), distinct from Berry's `.yarnrc.yml`.
fn yarn_offline_mirror_configured(root: &Path) -> bool {
    std::fs::read_to_string(root.join(".yarnrc"))
        .is_ok_and(|c| classic_yarnrc_has_key(&c, "yarn-offline-mirror"))
}

/// Whether the root (or any workspace member) manifest declares
/// `dependenciesMeta.<pkg>.injected: true`. aube materializes injected copies
/// only under the isolated linker, so when injection is present nub auto-
/// switches the embedder's `nodeLinker` default to `isolated` (instead of the
/// hoisted default for flat-layout incumbents) rather than silently dropping
/// the directive.
pub(crate) fn injected_deps_present(root: &Path) -> bool {
    manifest_has_injected(&root.join("package.json"))
        || aube_workspace::find_workspace_packages(root)
            .into_iter()
            .flatten()
            .any(|dir| manifest_has_injected(&dir.join("package.json")))
}

fn manifest_has_injected(manifest_path: &Path) -> bool {
    let Ok(manifest) = aube_manifest::PackageJson::from_path(manifest_path) else {
        return false;
    };
    let Some(meta) = manifest
        .extra
        .get("dependenciesMeta")
        .and_then(|v| v.as_object())
    else {
        return false;
    };
    meta.values().any(|v| {
        v.as_object()
            .and_then(|o| o.get("injected"))
            .and_then(|b| b.as_bool())
            == Some(true)
    })
}

// ───────────────────────── the scan ─────────────────────────

/// One unsupported field the scan flagged FATAL: an `ERR_NUB_*` code, a
/// one-line explanation of what nub does NOT support, and a remedy.
struct FatalField {
    code: &'static str,
    field: &'static str,
    detail: &'static str,
    remedy: &'static str,
}

/// Result of the curated unsupported-config scan: a FATAL abort (the first
/// load-bearing field nub can't honor) or a list of WARN fields to surface.
pub(crate) enum ScanResult {
    Fatal(anyhow::Error),
    Warn(Vec<IgnoredField>),
}

/// Curated unsupported-config scan for one install. FATAL on the genuinely-hard
/// load-bearing fields nub does not implement (returns the first hit so the
/// abort names a concrete remedy); otherwise returns the WARN set.
///
/// The FATAL set is deliberately SHORT — only fields whose silent omission
/// produces a correctness-divergent install AND which nub cannot honor:
/// npm `legacy-peer-deps` (different peer graph) and npm
/// `install-strategy=nested` (different resolution/layout). yarn
/// `supportedArchitectures` is NOT here — the engine honors it via the
/// arch-filter resolver. PnP stays a WARN+downgrade (handled separately in
/// `warn_if_pnp_requested`), not
/// promoted. `checksumBehavior`/`enableHardenedMode` are NOT here: aube verifies
/// every tarball's SHA-512 by default (`verifyStoreIntegrity=true`), satisfying
/// the `throw` posture.
pub(crate) fn scan_unsupported_config(
    role: Role,
    major: Option<u64>,
    minor: Option<u64>,
    root: &Path,
) -> ScanResult {
    let _ = (major, minor);
    // FATAL — first hit aborts.
    if let Some(fatal) = scan_fatal(role, root) {
        return ScanResult::Fatal(anyhow::anyhow!(
            "nub: {} ({}) is not supported — {}. {} [{}]",
            fatal.field,
            role.display(),
            fatal.detail,
            fatal.remedy,
            fatal.code,
        ));
    }
    // WARN — non-load-bearing but unsupported, surfaced as dim lines.
    ScanResult::Warn(scan_warn(role, root))
}

fn scan_fatal(role: Role, root: &Path) -> Option<FatalField> {
    match role {
        Role::Npm => {
            if npmrc_project_bool_set(root, "legacy-peer-deps") {
                return Some(FatalField {
                    code: "ERR_NUB_UNSUPPORTED_CONFIG",
                    field: "`legacy-peer-deps`",
                    detail: "nub always resolves peer dependencies; npm's legacy escape hatch \
                             would produce a different peer graph",
                    remedy: "remove `legacy-peer-deps` from .npmrc and fix the peer conflict, \
                             or pin the conflicting versions in `overrides`",
                });
            }
            if let Some(strategy) = npmrc_project_value(root, "install-strategy")
                && strategy.eq_ignore_ascii_case("nested")
            {
                return Some(FatalField {
                    code: "ERR_NUB_UNSUPPORTED_CONFIG",
                    field: "`install-strategy=nested`",
                    detail: "nub installs a hoisted/isolated tree; npm's nested layout can change \
                             which version a require() resolves to",
                    remedy: "remove `install-strategy=nested` from .npmrc",
                });
            }
            None
        }
        // yarn `supportedArchitectures` is HONORED, not fatal: the engine
        // reads `.yarnrc.yml`'s `supportedArchitectures` and feeds it to
        // the same arch-filter resolver the pnpm path uses (translated to
        // the `supportedArchitectures` object setting in the yarnrc reader).
        Role::Yarn | Role::Pnpm | Role::Bun | Role::Nub => None,
    }
}

/// FATAL when offline mode is active AND a classic `.yarnrc` `yarn-offline-mirror`
/// is configured: nub installs from its content-addressable store and the
/// registry and cannot read a user-configured offline-mirror directory, so in
/// offline mode (where that mirror is the user's intended package source)
/// silently falling back would diverge. Returns the abort error, or `None` when
/// there's no mirror configured (or offline mode is off — the caller gates on
/// that, since a mirror is moot when online).
///
/// Gated by the active [`Role`] (only consulted for yarn) and called from the
/// install path once the effective offline state is known — distinct from
/// [`scan_unsupported_config`]'s unconditional fatals.
pub(crate) fn offline_mirror_fatal(role: Role, root: &Path) -> Option<anyhow::Error> {
    if role != Role::Yarn || !yarn_offline_mirror_configured(root) {
        return None;
    }
    Some(anyhow::anyhow!(
        "nub: `yarn-offline-mirror` (yarn) cannot be honored in offline mode — \
         nub installs from its content-addressable store and the registry, not a \
         configured offline-mirror directory. Run `nub install` once while online \
         to populate nub's store, then remove `yarn-offline-mirror` from .yarnrc \
         (or drop offline mode). [ERR_NUB_UNSUPPORTED_CONFIG]"
    ))
}

fn scan_warn(role: Role, root: &Path) -> Vec<IgnoredField> {
    let mut out = Vec::new();
    // enableHardenedMode (yarn): aube verifies tarball SHA-512 by default, so
    // the integrity core is covered, but Berry's extra registry-range
    // re-verification is not — surface it as ignored.
    if role == Role::Yarn && yarnrc_top_level_bool_str(root, "enableHardenedMode") == Some(true) {
        out.push(IgnoredField {
            field: "enableHardenedMode",
            fix: "nub verifies every tarball's checksum by default; the extra \
                  registry-range re-verification is not applied"
                .to_string(),
        });
    }
    // Brand-symmetry consistency warn: a `pnpm.overrides` block present under a
    // role that isn't pnpm is dropped silently by the scope filter. Surface it.
    if role != Role::Pnpm && manifest_has_pnpm_overrides(root) {
        out.push(IgnoredField {
            field: "pnpm.overrides",
            fix: "nub mirrors this project's package manager and does not apply another PM's \
                  branded config; move the pins to `overrides` or `resolutions`"
                .to_string(),
        });
    }
    out
}

fn manifest_has_pnpm_overrides(root: &Path) -> bool {
    let Ok(manifest) = aube_manifest::PackageJson::from_path(&root.join("package.json")) else {
        return false;
    };
    manifest
        .extra
        .get("pnpm")
        .and_then(|v| v.as_object())
        .and_then(|p| p.get("overrides"))
        .and_then(|v| v.as_object())
        .is_some_and(|o| !o.is_empty())
}

fn yarnrc_top_level_bool_str(root: &Path, key: &str) -> Option<bool> {
    let content = std::fs::read_to_string(root.join(".yarnrc.yml")).ok()?;
    yarnrc_top_level_bool(&content, key)
}

// ───────────────────────── npmrc reading ─────────────────────────

/// `.npmrc` files in precedence-low-to-high order: user `~/.npmrc` first, then
/// project `.npmrc` (walk from root up to filesystem root). Later entries win.
/// Used for the dep-selection IMPLEMENT-win, where global `omit`/`include`
/// genuinely participate (it only seeds a default a CLI flag overrides, matching
/// npm's own precedence).
fn npmrc_paths(root: &Path) -> Vec<PathBuf> {
    npmrc_paths_inner(root, true)
}

/// PROJECT-SCOPED `.npmrc` files only: the walk from `root` up to the filesystem
/// root, EXCLUDING the user/global `~/.npmrc`. This is what the FATAL scan must
/// read from — a personal global setting (`legacy-peer-deps=true` in `~/.npmrc`)
/// must never abort an unrelated project's install.
fn npmrc_project_paths(root: &Path) -> Vec<PathBuf> {
    npmrc_paths_inner(root, false)
}

fn npmrc_paths_inner(root: &Path, include_global: bool) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if include_global && let Some(home) = dirs_next::home_dir() {
        paths.push(home.join(".npmrc"));
    }
    // Walk-up: ancestors first (less specific) so the project's own .npmrc wins.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut current = root.to_path_buf();
    loop {
        dirs.push(current.clone());
        if !current.pop() {
            break;
        }
    }
    dirs.reverse();
    paths.extend(dirs.into_iter().map(|d| d.join(".npmrc")));
    // Defensive: if the project walk-up reaches the home dir, that surfaces the
    // user `~/.npmrc` even in project-scoped mode (e.g. a package.json directly
    // in $HOME). The FATAL scan must not see the global file, so drop it.
    if !include_global && let Some(home) = dirs_next::home_dir() {
        let global = home.join(".npmrc");
        paths.retain(|p| p != &global);
    }
    paths
}

/// Read a scalar `.npmrc` key across the given paths (later wins).
fn npmrc_value_in(paths: &[PathBuf], key: &str) -> Option<String> {
    // Last-wins across the precedence-ordered paths.
    paths
        .iter()
        .filter_map(|path| {
            let content = std::fs::read_to_string(path).ok()?;
            npmrc_scalar(&content, key)
        })
        .next_back()
}

fn npmrc_bool_set_in(paths: &[PathBuf], key: &str) -> bool {
    npmrc_value_in(paths, key).is_some_and(|v| {
        let v = v.trim();
        v.is_empty() || v.eq_ignore_ascii_case("true")
    })
}

/// Read a scalar `.npmrc` key from PROJECT-SCOPED `.npmrc` files only (the
/// project tree walk-up, excluding `~/.npmrc`). Used by the FATAL scan.
fn npmrc_project_value(root: &Path, key: &str) -> Option<String> {
    npmrc_value_in(&npmrc_project_paths(root), key)
}

/// Whether a boolean `.npmrc` key is set truthy (`key=true`, or bare `key`) in
/// PROJECT-SCOPED `.npmrc` files only. Used by the FATAL scan — a global
/// `~/.npmrc` setting must never trip a project's install.
fn npmrc_project_bool_set(root: &Path, key: &str) -> bool {
    npmrc_bool_set_in(&npmrc_project_paths(root), key)
}

/// Parse a single scalar key from `.npmrc` content (ini-style `key=value`,
/// `#`/`;` comments). Returns the LAST occurrence's value. Key match is
/// kebab/camel insensitive on the exact spelling passed.
fn npmrc_scalar(content: &str, key: &str) -> Option<String> {
    let mut found = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            // Bare `key` (no `=`) — npm treats it as `key=true`.
            if line.eq_ignore_ascii_case(key) {
                found = Some(String::new());
            }
            continue;
        };
        if k.trim().eq_ignore_ascii_case(key) {
            found = Some(strip_inline_value(v));
        }
    }
    found
}

/// Strip surrounding quotes from an npmrc value. (npmrc does not support inline
/// `#` comments on a value line, so only quote-stripping applies.)
fn strip_inline_value(raw: &str) -> String {
    let v = raw.trim();
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q)
            && let Some(end) = inner.find(q)
        {
            return inner[..end].to_string();
        }
    }
    v.to_string()
}

/// Split a comma- or whitespace-separated list value into lowercased tokens.
fn split_list(v: &str) -> Vec<String> {
    v.split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

// ───────────────────────── bunfig reading ─────────────────────────

/// Read a boolean `[install].<key>` from the project + global bunfig.
fn bunfig_install_bool(root: &Path, key: &str) -> Option<bool> {
    let mut value = None;
    for path in bunfig_paths(root) {
        if let Ok(raw) = std::fs::read_to_string(&path)
            && let Ok(parsed) = raw.parse::<toml::Value>()
            && let Some(b) = parsed
                .get("install")
                .and_then(toml::Value::as_table)
                .and_then(|t| t.get(key))
                .and_then(toml::Value::as_bool)
        {
            value = Some(b);
        }
    }
    value
}

/// bunfig files in low-to-high precedence: global `~/.bunfig.toml` then the
/// project `bunfig.toml` (project wins). Mirrors [`super::bun_config`]'s path
/// resolution.
fn bunfig_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let global = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .map(|dir| dir.join(".bunfig.toml"));
    if let Some(g) = global {
        paths.push(g);
    }
    paths.push(root.join("bunfig.toml"));
    paths
}

// ───────────────────────── yarnrc reading ─────────────────────────

/// Read a top-level (unindented) boolean `key:` from `.yarnrc.yml` content.
fn yarnrc_top_level_bool(content: &str, key: &str) -> Option<bool> {
    for line in content.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(key)
            && let Some(rest) = rest.strip_prefix(':')
        {
            let v = strip_yarnrc_scalar(rest);
            return match v.to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        }
    }
    None
}

/// Whether a top-level `key:` introduces a non-empty indented block (a YAML
/// list/map) in `.yarnrc.yml`.
fn yarnrc_block_nonempty(content: &str, key: &str) -> bool {
    let mut lines = content.lines();
    while let Some(line) = lines.next() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(key)
            && let Some(rest) = rest.strip_prefix(':')
        {
            let inline = strip_yarnrc_scalar(rest);
            if !inline.is_empty() {
                // `key: [a, b]` inline non-empty.
                return inline != "[]";
            }
            // Block form: the next non-blank line must be indented.
            for next in lines.by_ref() {
                if next.trim().is_empty() {
                    continue;
                }
                return next.starts_with(char::is_whitespace);
            }
            return false;
        }
    }
    false
}

/// Whether a CLASSIC `.yarnrc` (Yarn 1) sets a non-empty value for `key`.
/// Classic `.yarnrc` is a `key "value"` / `key value` line format (Yarn 1's
/// own lockfile dialect), unrelated to Berry's `.yarnrc.yml`. A bare key with
/// no value, or an explicitly empty `""`, does not count as configured.
fn classic_yarnrc_has_key(content: &str, key: &str) -> bool {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `key value` / `key "value"`: split on the first whitespace.
        let Some((k, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        return !strip_yarnrc_scalar(rest).is_empty();
    }
    false
}

/// Strip surrounding quotes / trailing `# comment` from a yarnrc scalar.
fn strip_yarnrc_scalar(rest: &str) -> String {
    let rest = rest.trim();
    for q in ['"', '\''] {
        if let Some(inner) = rest.strip_prefix(q)
            && let Some(end) = inner.find(q)
        {
            return inner[..end].to_string();
        }
    }
    rest.split('#')
        .next()
        .map(str::trim)
        .unwrap_or(rest)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn npm_omit_dev_selects_prod() {
        let d = tmp();
        fs::write(d.path().join(".npmrc"), "omit=dev\n").unwrap();
        let cfg = npm_omit_include(d.path());
        assert!(cfg.prod, "omit=dev must select prod-only");
        assert!(!cfg.no_optional);
    }

    #[test]
    fn npm_omit_optional_skips_optional() {
        let d = tmp();
        fs::write(d.path().join(".npmrc"), "omit=optional\n").unwrap();
        let cfg = npm_omit_include(d.path());
        assert!(cfg.no_optional);
        assert!(!cfg.prod);
    }

    #[test]
    fn npm_include_overrides_omit_of_same_type() {
        let d = tmp();
        fs::write(d.path().join(".npmrc"), "omit=dev\ninclude=dev\n").unwrap();
        let cfg = npm_omit_include(d.path());
        assert!(!cfg.prod, "include=dev must cancel omit=dev");
    }

    #[test]
    fn bunfig_production_selects_prod() {
        let d = tmp();
        fs::write(
            d.path().join("bunfig.toml"),
            "[install]\nproduction = true\n",
        )
        .unwrap();
        let cfg = bunfig_production(d.path());
        assert!(cfg.prod);
    }

    #[test]
    fn bunfig_frozen_lockfile_is_frozen() {
        let d = tmp();
        fs::write(
            d.path().join("bunfig.toml"),
            "[install]\nfrozenLockfile = true\n",
        )
        .unwrap();
        assert!(frozen_from_config(Role::Bun, d.path()));
    }

    #[test]
    fn yarn_immutable_installs_is_frozen() {
        let d = tmp();
        fs::write(
            d.path().join(".yarnrc.yml"),
            "enableImmutableInstalls: true\n",
        )
        .unwrap();
        assert!(frozen_from_config(Role::Yarn, d.path()));
    }

    #[test]
    fn yarn_immutable_patterns_block_is_frozen() {
        let d = tmp();
        fs::write(
            d.path().join(".yarnrc.yml"),
            "immutablePatterns:\n  - \"**/*.lock\"\n",
        )
        .unwrap();
        assert!(frozen_from_config(Role::Yarn, d.path()));
    }

    #[test]
    fn yarn_enable_scripts_false_disables_scripts() {
        let d = tmp();
        fs::write(d.path().join(".yarnrc.yml"), "enableScripts: false\n").unwrap();
        assert!(yarn_scripts_disabled(Role::Yarn, d.path()));
        // Not yarn role ⇒ ignored.
        assert!(!yarn_scripts_disabled(Role::Npm, d.path()));
    }

    #[test]
    fn yarn_enable_network_false_maps_to_offline() {
        let d = tmp();
        fs::write(d.path().join(".yarnrc.yml"), "enableNetwork: false\n").unwrap();
        assert!(yarn_network_disabled(Role::Yarn, d.path()));
        // enableNetwork: true (the default) is not offline.
        fs::write(d.path().join(".yarnrc.yml"), "enableNetwork: true\n").unwrap();
        assert!(!yarn_network_disabled(Role::Yarn, d.path()));
        // Non-yarn role ⇒ never consulted.
        fs::write(d.path().join(".yarnrc.yml"), "enableNetwork: false\n").unwrap();
        assert!(!yarn_network_disabled(Role::Npm, d.path()));
    }

    #[test]
    fn offline_mirror_in_classic_yarnrc_is_fatal_for_yarn() {
        let d = tmp();
        // Classic .yarnrc (NOT .yarnrc.yml): space-separated `key "value"`.
        fs::write(
            d.path().join(".yarnrc"),
            "yarn-offline-mirror \"./npm-packages-offline-cache\"\n",
        )
        .unwrap();
        let err = offline_mirror_fatal(Role::Yarn, d.path()).expect("mirror must be fatal");
        let msg = err.to_string();
        assert!(
            msg.contains("yarn-offline-mirror"),
            "names the field: {msg}"
        );
        assert!(
            msg.contains("ERR_NUB_UNSUPPORTED_CONFIG"),
            "carries the code: {msg}"
        );
        assert!(
            msg.contains("online"),
            "states the remedy (populate while online): {msg}"
        );
    }

    #[test]
    fn offline_mirror_not_configured_is_not_fatal() {
        let d = tmp();
        // No .yarnrc at all.
        assert!(offline_mirror_fatal(Role::Yarn, d.path()).is_none());
        // A .yarnrc without the mirror key.
        fs::write(
            d.path().join(".yarnrc"),
            "registry \"https://registry.npmjs.org\"\n",
        )
        .unwrap();
        assert!(offline_mirror_fatal(Role::Yarn, d.path()).is_none());
        // An empty mirror value does not count as configured.
        fs::write(d.path().join(".yarnrc"), "yarn-offline-mirror \"\"\n").unwrap();
        assert!(offline_mirror_fatal(Role::Yarn, d.path()).is_none());
    }

    #[test]
    fn offline_mirror_only_consulted_for_yarn_role() {
        let d = tmp();
        fs::write(
            d.path().join(".yarnrc"),
            "yarn-offline-mirror \"./cache\"\n",
        )
        .unwrap();
        // The .yarnrc belongs to yarn; under another role it isn't read.
        assert!(offline_mirror_fatal(Role::Npm, d.path()).is_none());
    }

    #[test]
    fn injected_deps_detected_in_root_manifest() {
        let d = tmp();
        fs::write(
            d.path().join("package.json"),
            r#"{"name":"x","dependenciesMeta":{"foo":{"injected":true}}}"#,
        )
        .unwrap();
        assert!(injected_deps_present(d.path()));
    }

    #[test]
    fn scan_fatal_on_legacy_peer_deps() {
        let d = tmp();
        fs::write(d.path().join(".npmrc"), "legacy-peer-deps=true\n").unwrap();
        match scan_unsupported_config(Role::Npm, Some(10), None, d.path()) {
            ScanResult::Fatal(e) => {
                let msg = e.to_string();
                assert!(msg.contains("legacy-peer-deps"), "msg: {msg}");
                assert!(msg.contains("ERR_NUB_UNSUPPORTED_CONFIG"));
            }
            ScanResult::Warn(_) => panic!("legacy-peer-deps must be FATAL"),
        }
    }

    #[test]
    fn scan_fatal_on_install_strategy_nested() {
        let d = tmp();
        fs::write(d.path().join(".npmrc"), "install-strategy=nested\n").unwrap();
        assert!(matches!(
            scan_unsupported_config(Role::Npm, None, None, d.path()),
            ScanResult::Fatal(_)
        ));
    }

    #[test]
    fn supported_architectures_is_honored_not_fatal() {
        // The arch-filter resolver honors yarn `supportedArchitectures`
        // (the yarnrc reader translates it to the `supportedArchitectures`
        // object setting), so it must no longer abort the install.
        let d = tmp();
        fs::write(
            d.path().join(".yarnrc.yml"),
            "supportedArchitectures:\n  os:\n    - linux\n",
        )
        .unwrap();
        match scan_unsupported_config(Role::Yarn, None, None, d.path()) {
            ScanResult::Warn(_) => {}
            ScanResult::Fatal(e) => {
                panic!("supportedArchitectures is honored and must not be fatal: {e}")
            }
        }
    }

    #[test]
    fn scan_warn_on_hardened_mode_not_fatal() {
        let d = tmp();
        fs::write(d.path().join(".yarnrc.yml"), "enableHardenedMode: true\n").unwrap();
        match scan_unsupported_config(Role::Yarn, None, None, d.path()) {
            ScanResult::Warn(w) => {
                assert!(w.iter().any(|f| f.field == "enableHardenedMode"));
            }
            ScanResult::Fatal(_) => panic!("hardened mode is WARN (checksum core covered by CAS)"),
        }
    }

    #[test]
    fn supported_config_does_not_trip_scan() {
        let d = tmp();
        // A benign, fully-supported .npmrc — registry + save-exact.
        fs::write(
            d.path().join(".npmrc"),
            "registry=https://registry.npmjs.org/\nsave-exact=true\n",
        )
        .unwrap();
        match scan_unsupported_config(Role::Npm, Some(10), None, d.path()) {
            ScanResult::Warn(w) => assert!(w.is_empty(), "supported config must not warn: {w:?}"),
            ScanResult::Fatal(e) => panic!("supported config tripped FATAL: {e}"),
        }
    }

    /// PRIORITY-1 regression: a `legacy-peer-deps=true` in the user/global
    /// `~/.npmrc` must NOT trip the FATAL scan for an unrelated project — a
    /// personal global setting may not abort every install. The project's own
    /// `.npmrc` setting MUST still be fatal.
    ///
    /// `dirs_next::home_dir()` reads `$HOME`; point it at a temp dir holding a
    /// global `.npmrc`, and put the project under a SEPARATE temp dir so the
    /// project walk-up never reaches the fake home.
    #[test]
    fn global_npmrc_legacy_peer_deps_does_not_trip_fatal() {
        let home = tmp();
        let project = tmp();
        fs::write(home.path().join(".npmrc"), "legacy-peer-deps=true\n").unwrap();

        // SAFETY: single-threaded test; restored before returning.
        let prev_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
        }

        let global_only = scan_unsupported_config(Role::Npm, Some(10), None, project.path());
        let global_is_fatal = matches!(global_only, ScanResult::Fatal(_));

        // Now the SAME key in the PROJECT .npmrc — must be fatal.
        fs::write(project.path().join(".npmrc"), "legacy-peer-deps=true\n").unwrap();
        let project_set = scan_unsupported_config(Role::Npm, Some(10), None, project.path());
        let project_is_fatal = matches!(project_set, ScanResult::Fatal(_));

        // Restore $HOME before asserting so a panic can't leak it.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        assert!(
            !global_is_fatal,
            "a global ~/.npmrc legacy-peer-deps must NOT abort an unrelated project"
        );
        assert!(
            project_is_fatal,
            "a project ./.npmrc legacy-peer-deps MUST be fatal"
        );
    }

    /// Companion: `install-strategy=nested` in the global `~/.npmrc` is likewise
    /// not project-fatal, while the project spelling is.
    #[test]
    fn global_npmrc_install_strategy_does_not_trip_fatal() {
        let home = tmp();
        let project = tmp();
        fs::write(home.path().join(".npmrc"), "install-strategy=nested\n").unwrap();

        let prev_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
        }
        let global_only = scan_unsupported_config(Role::Npm, None, None, project.path());
        let global_is_fatal = matches!(global_only, ScanResult::Fatal(_));
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        assert!(
            !global_is_fatal,
            "a global ~/.npmrc install-strategy=nested must NOT abort an unrelated project"
        );
    }

    #[test]
    fn pnpm_overrides_under_npm_warns() {
        let d = tmp();
        fs::write(
            d.path().join("package.json"),
            r#"{"name":"x","pnpm":{"overrides":{"lodash":"4.17.21"}}}"#,
        )
        .unwrap();
        match scan_unsupported_config(Role::Npm, Some(10), None, d.path()) {
            ScanResult::Warn(w) => assert!(w.iter().any(|f| f.field == "pnpm.overrides")),
            ScanResult::Fatal(_) => panic!("pnpm.overrides is a WARN, not FATAL"),
        }
    }
}
