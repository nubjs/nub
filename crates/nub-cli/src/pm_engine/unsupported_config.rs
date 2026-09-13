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
//!   4. `dependenciesMeta.*.injected` → the carve-out from the GVS-aware
//!      hoisting default: a non-injected project pushes no `hoist` (it resolves
//!      to the default `true`, which under nub's `gvs_over_default_hoist` profile
//!      lets GVS engage without a hidden hoist tree), but injected copies
//!      materialize only with the hidden hoist tree on, so an injected project
//!      pushes an EXPLICIT `hoist=true` — vetoing GVS (per-project + hidden
//!      tree, always).
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
use std::sync::Arc;

use nub_core::config_cache::MtimeCache;

use super::config_scope::{IgnoredField, Role};

/// Per-process, mtime-validated cache of raw config-file CONTENTS keyed by path.
/// The unsupported-config readers each opened the same `.yarnrc.yml` / `.npmrc`
/// file once PER KEY (immutable, scripts, network, hardened; omit, include,
/// legacy-peer-deps) — several reads of one file per command.
/// This collapses them to a single read per `(path, mtime)`; the per-key parse
/// then runs against the cached string. mtime validation keeps it stale-proof:
/// any rewrite of the file bumps the mtime, the next lookup misses and re-reads.
static CONFIG_TEXT_CACHE: MtimeCache<String> = MtimeCache::new();

/// Read a config file's full contents through [`CONFIG_TEXT_CACHE`]. `None`
/// (missing / unreadable) is never cached — identical to `read_to_string().ok()`
/// on those paths, just deduplicated across repeated readers in one command.
fn read_config_text(path: &Path) -> Option<Arc<String>> {
    CONFIG_TEXT_CACHE.get_or_read(path, || std::fs::read_to_string(path).ok())
}

/// Whether the root (or any workspace member) manifest declares
/// `dependenciesMeta.<pkg>.injected: true`. aube materializes injected copies
/// only with the hidden hoist tree on, so an injected project is the carve-out
/// from the GVS-aware hoisting default: instead of leaving `hoist` at its
/// default (which lets GVS engage), it pushes an EXPLICIT `hoist=true` that
/// vetoes GVS (per-project + hidden tree), rather than silently dropping the
/// directive.
/// `workspace_members` is the caller's one-shot workspace discovery for `root`
/// (see `nub_setting_defaults`), shared with the version gates that scan the
/// same manifests.
pub(crate) fn injected_deps_present(root: &Path, workspace_members: &[PathBuf]) -> bool {
    manifest_has_injected(&root.join("package.json"))
        || workspace_members
            .iter()
            .any(|dir| manifest_has_injected(&dir.join("package.json")))
}

fn manifest_has_injected(manifest_path: &Path) -> bool {
    let Some(manifest) = super::cached_aube_manifest(manifest_path) else {
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
/// The fatal set contains only fields whose silent omission changes resolution
/// and which Nub cannot honor: npm `legacy-peer-deps` (a different peer graph).
/// Branded layout fields are ignored and disclosed in the install header.
/// Yarn `supportedArchitectures` is not here — the engine honors it via the
/// arch-filter resolver. yarn `nodeLinker: pnp` is a plan-time FATAL handled
/// separately in `pnp_fatal_if_requested` (it needs `.yarnrc.yml` reading, not
/// the role-keyed scan here). `checksumBehavior`/`enableHardenedMode` are NOT here: aube verifies
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
                    remedy: "remove `legacy-peer-deps` from .npmrc and fix the peer conflict — \
                             pin the conflicting versions in `overrides`, or correct a \
                             package's peer metadata (e.g. mark a peer optional) in \
                             `packageExtensions`",
                });
            }
            // `install-strategy` controls layout, so it follows the same policy
            // as every other branded layout setting: ignore it and disclose the
            // replacement source in the install header.
            None
        }
        // yarn `supportedArchitectures` is HONORED, not fatal: the engine
        // reads `.yarnrc.yml`'s `supportedArchitectures` and feeds it to
        // the same arch-filter resolver the pnpm path uses (translated to
        // the `supportedArchitectures` object setting in the yarnrc reader).
        Role::Yarn | Role::Pnpm | Role::Bun | Role::Nub => None,
    }
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
    let Some(manifest) = super::cached_aube_manifest(&root.join("package.json")) else {
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
    let content = read_config_text(&root.join(".yarnrc.yml"))?;
    yarnrc_top_level_bool(&content, key)
}

// ───────────────────────── npmrc reading ─────────────────────────

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
            let content = read_config_text(path)?;
            npmrc_scalar(&content, key)
        })
        .next_back()
}

/// Read a scalar `.npmrc` key across the project walk-up and, when
/// `include_global`, the global `~/.npmrc`. The reusable entry for nub-behavior
/// knobs read from the neutral `.npmrc` surface (the verify-deps policy).
pub(crate) fn npmrc_scalar_value(root: &Path, key: &str, include_global: bool) -> Option<String> {
    npmrc_value_in(&npmrc_paths_inner(root, include_global), key)
}

fn npmrc_bool_set_in(paths: &[PathBuf], key: &str) -> bool {
    npmrc_value_in(paths, key).is_some_and(|v| {
        let v = v.trim();
        v.is_empty() || v.eq_ignore_ascii_case("true")
    })
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

// ───────────────────────── bunfig reading ─────────────────────────

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

    /// Serializes the two tests that mutate the process-global `$HOME`. This lib
    /// test binary runs MULTI-THREADED (nothing pins `--test-threads=1`), so the
    /// set→use→restore window must be held under a lock or it races sibling tests
    /// (and each other) — every `scan_unsupported_config` call reads
    /// `dirs_next::home_dir()`. Poison-recovering, matching the crate's other
    /// process-global seams (`RELEASE_ENV_LOCK`, `CWD_LOCK`).
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn injected_deps_detected_in_root_manifest() {
        let d = tmp();
        fs::write(
            d.path().join("package.json"),
            r#"{"name":"x","dependenciesMeta":{"foo":{"injected":true}}}"#,
        )
        .unwrap();
        assert!(injected_deps_present(d.path(), &[]));
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

    /// npm's layout knob is ignored like every other branded layout setting,
    /// rather than aborting for one value and silently accepting the others.
    #[test]
    fn install_strategy_is_ignored_not_fatal() {
        for value in ["nested", "hoisted", "shallow", "linked"] {
            let d = tmp();
            fs::write(
                d.path().join(".npmrc"),
                format!("install-strategy={value}\n"),
            )
            .unwrap();
            assert!(
                matches!(
                    scan_unsupported_config(Role::Npm, None, None, d.path()),
                    ScanResult::Warn(_)
                ),
                "install-strategy={value} is layout and must not abort"
            );
        }
    }

    /// `legacy-peer-deps` changes resolution rather than layout, so it remains
    /// fatal after the layout carve-out.
    #[test]
    fn legacy_peer_deps_still_aborts_after_the_layout_carve_out() {
        let d = tmp();
        fs::write(d.path().join(".npmrc"), "legacy-peer-deps=true\n").unwrap();
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
        // Held for the whole set→use→restore window: the lib binary is
        // multi-threaded, and HOME_LOCK serializes the two HOME-mutating tests.
        let _home_guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tmp();
        let project = tmp();
        fs::write(home.path().join(".npmrc"), "legacy-peer-deps=true\n").unwrap();

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
