use std::path::PathBuf;

use crate::identity::embedder;

/// Whether a *branded* settings env-var alias (the tool-prefixed form like
/// `AUBE_NODE_LINKER`) should be read, given the active embedder's
/// [`read_branded_settings_env`](crate::identity::Embedder::read_branded_settings_env)
/// posture and [`env_prefix`](crate::identity::Embedder::env_prefix).
///
/// aube's settings table declares each branded env alias as `{PREFIX}_<NAME>`
/// alongside the neutral `npm_config_*` / `NPM_CONFIG_*` forms and a handful of
/// bare external vars (`CI`, `HTTP_PROXY`, `NODE_OPTIONS`, …). Two
/// embedder-fixed levers gate the *branded* surface only, composed in order:
///
/// 1. [`read_branded_settings_env`](crate::identity::Embedder::read_branded_settings_env)
///    — the on/off switch for the whole branded settings-env family. `true`
///    (standalone aube) honors it; `false` skips *every* tool-branded settings
///    alias regardless of prefix, for an embedder that exposes no branded env
///    surface for its settings.
/// 2. [`env_prefix`](crate::identity::Embedder::env_prefix) — *which* prefix is
///    the brand. When the family is honored, a branded alias is read only when
///    it is `{prefix}_…`; `None` likewise reads no branded settings env vars.
///
/// Standalone aube (`read_branded_settings_env = true`, `env_prefix =
/// Some("AUBE")`) thus reads every `AUBE_*` settings var exactly as before, and
/// nothing else changes. The neutral `npm_config_*` / `NPM_CONFIG_*` aliases and
/// the bare external vars are never the tool's brand and are always honored.
/// Standalone aube's settings table only ever emits its own `env_prefix` as the
/// branded prefix, so the brand family is exactly the `{prefix}_*` set.
pub fn branded_env_alias_enabled(alias: &str) -> bool {
    // npm-compat family — never the tool's brand, always honored.
    if alias.starts_with("npm_config_") || alias.starts_with("NPM_CONFIG_") {
        return true;
    }
    // pnpm-compat family (`pnpm_config_*` / `PNPM_CONFIG_*`). pnpm v11
    // reads its general settings from this env family, and an embedder
    // whose active package manager IS pnpm mirrors that — reading the
    // active PM's own env is faithful mirroring, not a brand leak. But
    // the family is pnpm-NAMED, so the pnpm-named-paths hard gate
    // applies: it rides the existing `read_branded_pnpm_config` posture
    // — on-by-default for standalone aube (which IS a pnpm-compatible
    // PM), gated to the pnpm-incumbent check under the nub profile. Under
    // a non-pnpm incumbent these vars are another tool's state and are
    // skipped —
    // and `looks_branded` would otherwise misclassify the lowercase
    // form as a neutral var and always read it, so the gate must live
    // here, ahead of that check.
    if alias.starts_with("pnpm_config_") || alias.starts_with("PNPM_CONFIG_") {
        return crate::engine_context().read_branded_pnpm_config;
    }
    // Bare external/neutral vars — not part of any tool's brand family.
    if !looks_branded(alias) {
        return true;
    }
    let id = embedder();
    // A branded-shaped alias. First the family on/off posture, then the prefix
    // match. An embedder that hides its branded settings-env surface
    // (`read_branded_settings_env = false`) skips every branded alias even when
    // it would match the active prefix.
    if !id.read_branded_settings_env {
        return false;
    }
    match id.env_prefix {
        Some(prefix) => alias
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('_')),
        None => false,
    }
}

/// Does `alias` have the `<UPPER_PREFIX>_<NAME>` shape of a tool-branded env
/// var, as opposed to a bare external var (`CI`) or neutral proxy/Node var
/// (`HTTP_PROXY`, `NODE_OPTIONS`)? aube's settings table only ever emits its
/// own `env_prefix` as the branded prefix, so this just has to separate the
/// branded family from the recognized neutral vars.
fn looks_branded(alias: &str) -> bool {
    const NEUTRAL: &[&str] = &[
        "CI",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "PROXY",
        "NODE_OPTIONS",
    ];
    if NEUTRAL.contains(&alias) {
        return false;
    }
    match alias.split_once('_') {
        Some((head, _)) if !head.is_empty() => head.chars().all(|c| c.is_ascii_uppercase()),
        _ => false,
    }
}

/// Read a tool-prefixed *non-settings, non-user-facing* env toggle through the
/// active embedder's [`env_prefix`](crate::identity::Embedder::env_prefix). For
/// standalone aube (`Some("AUBE")`) `embedder_env("DISABLE_CLONEDIR")` reads
/// `AUBE_DISABLE_CLONEDIR`; for an embedder with `env_prefix = None` (a host
/// that exposes no branded debug surface) it reads nothing and returns `None`,
/// so no branded debug/perf/diag toggle leaks under the embedding host's brand.
///
/// This is for the dev/debug/perf-bisect/diagnostic toggles that are NOT
/// user-facing config — `AUBE_DISABLE_*`, `AUBE_DIAG_*`, `AUBE_CAS_*`,
/// `AUBE_INTERNAL_*`, `AUBE_BENCH_*`, the self-update endpoints, … User-facing
/// config knobs go through [`config_env`] instead, and settings-table branded
/// aliases through [`branded_env_alias_enabled`]. Additive and no-op for
/// standalone aube: an embedder that registers nothing reads exactly the
/// `AUBE_*` forms it read before.
pub fn embedder_env(suffix: &str) -> Option<std::ffi::OsString> {
    let prefix = embedder().env_prefix?;
    std::env::var_os(format!("{prefix}_{suffix}"))
}

/// Read one of the tool's *first-class config* env knobs through the active
/// embedder's [`config_env_prefix`](crate::identity::Embedder::config_env_prefix).
/// For standalone aube (`Some("AUBE")`) `config_env("CACHE_DIR")` reads
/// `AUBE_CACHE_DIR`; for an embedder with `config_env_prefix = Some("NUB")` it
/// reads `NUB_CACHE_DIR`. `None` reads nothing.
///
/// This is the deliberate, minimal exception to the debug-toggle gate: the
/// handful of knobs a host legitimately wants under its OWN brand — the cache
/// dir, the fetch concurrency, the primer TTL — rather than hidden. Distinct
/// from [`embedder_env`]: that family vanishes under an embedder with no
/// `env_prefix`; this family follows the host's `config_env_prefix`, so a host
/// reads its own brand for exactly these knobs and the branded `AUBE_*` form is
/// never read under it.
pub fn config_env(suffix: &str) -> Option<std::ffi::OsString> {
    let prefix = embedder().config_env_prefix?;
    std::env::var_os(format!("{prefix}_{suffix}"))
}

/// Parse a primer-TTL env value into an *override* of the embedder's default.
///
/// Returns:
/// - `None` — the value is unset/empty/unrecognized; the caller keeps the
///   embedder's `primer_ttl` default.
/// - `Some(None)` — an explicit *unlimited* TTL (`0`, `unlimited`, `inf`,
///   `infinite`, `never`); the primer never expires.
/// - `Some(Some(d))` — a finite duration, e.g. `30d`, `720h`, `45m`, `90s`.
///
/// A bare integer with no unit is read as *seconds* (so `0` → unlimited, any
/// other bare number → that many seconds). Units: `s` seconds, `m` minutes,
/// `h` hours, `d` days, `w` weeks. Case-insensitive, surrounding whitespace
/// trimmed. An unparseable value falls through to `None` (embedder default) —
/// a typo never silently disables or un-disables the primer.
pub fn parse_primer_ttl(value: Option<&str>) -> Option<Option<std::time::Duration>> {
    use std::time::Duration;
    let raw = value?.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "0" | "unlimited" | "inf" | "infinite" | "never"
    ) {
        return Some(None);
    }
    // Split a trailing alphabetic unit off the leading numeric magnitude.
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (num, unit) = raw.split_at(split);
    let n: u64 = num.parse().ok()?;
    let secs = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => n,
        "m" | "min" | "mins" | "minute" | "minutes" => n.checked_mul(60)?,
        "h" | "hr" | "hrs" | "hour" | "hours" => n.checked_mul(3600)?,
        "d" | "day" | "days" => n.checked_mul(86_400)?,
        "w" | "wk" | "wks" | "week" | "weeks" => n.checked_mul(604_800)?,
        _ => return None,
    };
    Some(Some(Duration::from_secs(secs)))
}

pub fn is_ci() -> bool {
    std::env::var_os("CI").is_some()
}

pub fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Some(h.into());
    }
    #[cfg(windows)]
    if let Some(h) = std::env::var_os("USERPROFILE") {
        return Some(h.into());
    }
    None
}

fn non_empty_path_var(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

pub fn xdg_config_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_CONFIG_HOME")
}

pub fn xdg_data_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_DATA_HOME")
}

pub fn xdg_cache_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_CACHE_HOME")
}

/// Read `%LOCALAPPDATA%` (the Windows per-user, machine-local app-data
/// root, e.g. `C:\Users\me\AppData\Local`). Empty/whitespace values are
/// treated as unset. pnpm's directory layout uses this as the base for
/// its config / cache / data / state dirs on Windows when no XDG
/// override is present; mirror that here so [`pnpm_config_dir`] resolves
/// to the same place pnpm itself does.
pub fn local_app_data() -> Option<PathBuf> {
    non_empty_path_var("LOCALAPPDATA")
}

/// Resolve pnpm's per-user *config* directory the same way pnpm's
/// `getConfigDir` does (`@pnpm/config.reader`'s `dirs.ts`):
///
/// 1. `$XDG_CONFIG_HOME/pnpm` when `XDG_CONFIG_HOME` is set (every OS);
/// 2. macOS → `~/Library/Preferences/pnpm`;
/// 3. non-Windows (Linux/other) → `~/.config/pnpm`;
/// 4. Windows → `%LOCALAPPDATA%\pnpm\config` when `LOCALAPPDATA` is set,
///    else `~/.config/pnpm`.
///
/// This is the directory that holds pnpm's global `config.yaml` (pnpm
/// v11) and its global `auth.ini`. The platform branches matter: a flat
/// `~/.config/pnpm` is correct only on Linux — on a stock macOS or
/// Windows box pnpm's config lives elsewhere, so reading the flat path
/// there silently misses the user's real global config.
///
/// Returns `None` only when neither an XDG override nor a home directory
/// can be determined — callers then have no global config dir to read.
///
/// `home` and `xdg_config_home` are injected (not read from the
/// environment here) so tests can pin a tempdir without mutating
/// process-wide env; production callers pass [`home_dir`] /
/// [`xdg_config_home`]. Only `LOCALAPPDATA` is read env-direct — it has
/// no per-call override site today and a defined non-env fallback
/// (`~/.config/pnpm`).
pub fn pnpm_config_dir_with(
    home: Option<&std::path::Path>,
    xdg_config_home: Option<&std::path::Path>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        return Some(xdg.join("pnpm"));
    }
    let home = home?;
    if cfg!(target_os = "macos") {
        return Some(home.join("Library").join("Preferences").join("pnpm"));
    }
    if cfg!(windows) {
        if let Some(local) = local_app_data() {
            return Some(local.join("pnpm").join("config"));
        }
        return Some(home.join(".config").join("pnpm"));
    }
    Some(home.join(".config").join("pnpm"))
}

/// [`pnpm_config_dir_with`] using the process `$HOME` /
/// `$XDG_CONFIG_HOME` (via [`home_dir`] / [`xdg_config_home`]).
/// Production entry point; tests prefer the `_with` form to stay
/// hermetic.
pub fn pnpm_config_dir() -> Option<PathBuf> {
    pnpm_config_dir_with(home_dir().as_deref(), xdg_config_home().as_deref())
}

#[cfg(test)]
mod pnpm_config_dir_tests {
    use super::*;
    use std::path::Path;

    // These tests assert the *non-XDG* platform branch, so they must run
    // with `XDG_CONFIG_HOME` unset. The suite runs serially
    // (RUST_TEST_THREADS=1), and no other test in this crate sets
    // `XDG_CONFIG_HOME`, so reading it env-direct is safe here. Guard
    // anyway: if a developer's shell exports it, skip the platform-branch
    // assertion rather than fail spuriously.
    #[test]
    fn xdg_override_wins_on_every_platform() {
        let xdg = Path::new("/custom/xdg");
        assert_eq!(
            pnpm_config_dir_with(Some(Path::new("/home/tester")), Some(xdg)),
            Some(xdg.join("pnpm")),
            "an explicit XDG_CONFIG_HOME points the config dir at <xdg>/pnpm regardless of OS"
        );
    }

    #[test]
    fn resolves_per_os_config_dir_without_xdg() {
        let home = Path::new("/home/tester");
        let got = pnpm_config_dir_with(Some(home), None).expect("home given");
        let expected = if cfg!(target_os = "macos") {
            home.join("Library").join("Preferences").join("pnpm")
        } else if cfg!(windows) {
            // On a Windows test host LOCALAPPDATA is normally set; accept
            // either the LOCALAPPDATA-rooted path or the `~/.config`
            // fallback so the assertion holds regardless.
            let local = local_app_data().map(|l| l.join("pnpm").join("config"));
            local.unwrap_or_else(|| home.join(".config").join("pnpm"))
        } else {
            home.join(".config").join("pnpm")
        };
        assert_eq!(got, expected);
    }

    #[test]
    fn none_when_no_home_and_no_xdg() {
        assert_eq!(pnpm_config_dir_with(None, None), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under the default (AUBE) profile — `env_prefix = Some("AUBE")` — every
    /// settings env alias aube's table declares is honored: the branded
    /// `AUBE_*` form, the neutral `npm_config_*` / `NPM_CONFIG_*` forms, and
    /// the bare external vars. This is the standalone-neutrality contract for
    /// the env-prefix gate: a binary that registers no profile reads exactly
    /// what aube read before the gate existed.
    #[test]
    fn aube_profile_honors_every_settings_env_family() {
        // Branded family (the tool's own prefix).
        assert!(branded_env_alias_enabled("AUBE_NODE_LINKER"));
        assert!(branded_env_alias_enabled("AUBE_NO_LOCK"));
        assert!(branded_env_alias_enabled("AUBE_LINK_CONCURRENCY"));
        // npm-compat family — never gated.
        assert!(branded_env_alias_enabled("npm_config_node_linker"));
        assert!(branded_env_alias_enabled("NPM_CONFIG_NODE_LINKER"));
        // Bare external / neutral vars — never gated.
        assert!(branded_env_alias_enabled("CI"));
        assert!(branded_env_alias_enabled("HTTP_PROXY"));
        assert!(branded_env_alias_enabled("NODE_OPTIONS"));
    }

    /// Under the default (AUBE) profile — `env_prefix = Some("AUBE")`,
    /// `config_env_prefix = Some("AUBE")` — both helpers compose the prefix onto
    /// the suffix and read the resulting `AUBE_*` var. This is the
    /// standalone-neutrality contract: a binary that registers no profile reads
    /// exactly the `AUBE_*` forms it read before the helpers existed. Tests run
    /// serially (`RUST_TEST_THREADS=1`) and restore the prior value so they
    /// don't bleed into the next test.
    ///
    /// The `None`-prefix branch (an embedder that hides a family → the helper
    /// returns `None`) can't be exercised here without `set_embedder`, which
    /// would flip the process-global fallback the default-profile tests rely on;
    /// it's covered by the `embedder_env_brand_gate` integration test, which
    /// registers a real non-aube profile in its own process.
    #[test]
    fn embedder_and_config_env_read_aube_prefixed_under_default_profile() {
        // RAII guard so a panic in `f()` still restores the prior value —
        // a bare restore-after-`f()` would leak the var on panic and flake
        // the next serial test.
        struct EnvGuard {
            key: String,
            prev: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: tests run serially via RUST_TEST_THREADS=1.
                unsafe {
                    match &self.prev {
                        Some(v) => std::env::set_var(&self.key, v),
                        None => std::env::remove_var(&self.key),
                    }
                }
            }
        }
        fn with_var<F: FnOnce()>(key: &str, value: &str, f: F) {
            let _guard = EnvGuard {
                key: key.to_string(),
                prev: std::env::var_os(key),
            };
            // SAFETY: tests run serially via RUST_TEST_THREADS=1.
            unsafe { std::env::set_var(key, value) };
            f();
        }

        with_var("AUBE_DISABLE_CLONEDIR", "1", || {
            assert_eq!(
                embedder_env("DISABLE_CLONEDIR").as_deref(),
                Some(std::ffi::OsStr::new("1")),
            );
        });
        with_var("AUBE_CACHE_DIR", "/tmp/x", || {
            assert_eq!(
                config_env("CACHE_DIR").as_deref(),
                Some(std::ffi::OsStr::new("/tmp/x")),
            );
        });
    }

    /// `parse_primer_ttl` distinguishes the three outcomes the gate needs:
    /// unset/typo → keep the embedder default (`None`); explicit-unlimited →
    /// `Some(None)`; a unit'd duration → `Some(Some(d))`. A bare integer is
    /// seconds, and `0` is the unlimited sentinel, not a zero-second TTL.
    #[test]
    fn parse_primer_ttl_classifies_unlimited_finite_and_default() {
        use std::time::Duration;
        // Unset / empty / unrecognized → embedder default.
        assert_eq!(parse_primer_ttl(None), None);
        assert_eq!(parse_primer_ttl(Some("")), None);
        assert_eq!(parse_primer_ttl(Some("   ")), None);
        assert_eq!(parse_primer_ttl(Some("garbage")), None);
        assert_eq!(parse_primer_ttl(Some("30x")), None); // unknown unit
        // Explicit unlimited.
        assert_eq!(parse_primer_ttl(Some("0")), Some(None));
        assert_eq!(parse_primer_ttl(Some("unlimited")), Some(None));
        assert_eq!(parse_primer_ttl(Some("INF")), Some(None));
        assert_eq!(parse_primer_ttl(Some("never")), Some(None));
        // Finite durations.
        assert_eq!(
            parse_primer_ttl(Some("90")),
            Some(Some(Duration::from_secs(90)))
        );
        assert_eq!(
            parse_primer_ttl(Some("45m")),
            Some(Some(Duration::from_secs(45 * 60)))
        );
        assert_eq!(
            parse_primer_ttl(Some("720h")),
            Some(Some(Duration::from_secs(720 * 3600)))
        );
        assert_eq!(
            parse_primer_ttl(Some("30d")),
            Some(Some(Duration::from_secs(30 * 86_400)))
        );
        assert_eq!(
            parse_primer_ttl(Some(" 2w ")),
            Some(Some(Duration::from_secs(2 * 604_800)))
        );
        // 30d and 720h are the same window.
        assert_eq!(
            parse_primer_ttl(Some("30d")),
            parse_primer_ttl(Some("720h"))
        );
    }

    /// The pnpm-compat env family (`pnpm_config_*` / `PNPM_CONFIG_*`) is
    /// pnpm-NAMED, so the pnpm-named-paths hard gate applies: it rides the
    /// existing `read_branded_pnpm_config` posture — on-by-default for
    /// standalone aube, gated to the pnpm-incumbent check under the nub
    /// profile (`engine_context().read_branded_pnpm_config`). Flips the
    /// process-global engine context and restores it; the suite runs
    /// serially via `RUST_TEST_THREADS=1`, so the temporary flip can't
    /// bleed into a sibling test.
    ///
    /// Note the lowercase form would otherwise read as a *neutral* var
    /// (its head `pnpm` isn't all-uppercase), so without the explicit
    /// gate it would be honored unconditionally — this guards that the
    /// gate, not `looks_branded`, decides the pnpm family.
    #[test]
    fn pnpm_config_env_family_gated_on_pnpm_incumbent() {
        let restore = crate::engine_context().read_branded_pnpm_config;

        crate::update_engine_context(|c| c.read_branded_pnpm_config = true);
        assert!(branded_env_alias_enabled("pnpm_config_node_linker"));
        assert!(branded_env_alias_enabled("PNPM_CONFIG_NODE_LINKER"));

        crate::update_engine_context(|c| c.read_branded_pnpm_config = false);
        assert!(
            !branded_env_alias_enabled("pnpm_config_node_linker"),
            "lowercase pnpm_config_* must be skipped under a non-pnpm incumbent"
        );
        assert!(
            !branded_env_alias_enabled("PNPM_CONFIG_NODE_LINKER"),
            "uppercase PNPM_CONFIG_* must be skipped under a non-pnpm incumbent"
        );
        // The npm-compat family is never gated on the incumbent.
        assert!(branded_env_alias_enabled("npm_config_node_linker"));
        assert!(branded_env_alias_enabled("NPM_CONFIG_NODE_LINKER"));

        crate::update_engine_context(|c| c.read_branded_pnpm_config = restore);
    }

    /// `looks_branded` separates the tool-branded `<UPPER>_<NAME>` shape from
    /// the recognized neutral/external vars, so the `None`-prefix embedder
    /// skips exactly the branded family and nothing else.
    #[test]
    fn looks_branded_distinguishes_brand_from_neutral() {
        assert!(looks_branded("AUBE_NODE_LINKER"));
        assert!(looks_branded("FOO_BAR")); // any UPPER-prefixed var reads as branded
        assert!(!looks_branded("CI"));
        assert!(!looks_branded("HTTP_PROXY"));
        assert!(!looks_branded("NODE_OPTIONS"));
        assert!(!looks_branded("npm_config_node_linker")); // lowercase head
    }
}
