//! The cheap config-driven install wins.
//!
//! IMPLEMENT-wins — config that nub's existing machinery can honor once
//! it's read. Rather than warn/error on these, nub mirrors the incumbent:
//!   1. Dep-type selection — npm `.npmrc` `omit`/`include`, bun bunfig
//!      `[install].production` → the engine's `DepSelection`
//!      (`--prod`/`--dev`/`--no-optional` axis).
//!   2. Frozen-from-config — bun bunfig in-file `frozenLockfile`, yarn
//!      `enableImmutableInstalls`/`immutablePatterns` → the engine's frozen
//!      mode (same path `--frozen-lockfile` takes).
//!   3. `enableScripts: false` (yarn) → force a block-all-builds policy that
//!      overrides even nub's curated default-trust floor.
//!
//! (`minimumReleaseAge` from bunfig is wired in [`super::bun_config`] — it maps
//! to a synthetic `.npmrc` entry the settings registry already reads.)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nub_core::config_cache::MtimeCache;

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

// ───────────────────────── npmrc reading ─────────────────────────

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
