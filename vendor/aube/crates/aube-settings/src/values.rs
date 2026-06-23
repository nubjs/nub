//! Resolve typed setting values using the [`meta`](super::meta)
//! registry as the single source of truth for *which* keys map to
//! which setting.
//!
//! The registry (generated at build time from `settings.toml`)
//! records, for every setting:
//!
//!   - its canonical pnpm name
//!   - the `sources.npmrc` keys that populate it from `.npmrc`
//!   - the `sources.workspaceYaml` keys that populate it from
//!     `pnpm-workspace.yaml` / `aube-workspace.yaml`
//!   - the `sources.env` variables that populate it from the shell
//!   - the `sources.cli` flags that populate it from clap
//!   - the type of the value
//!
//! This module is the *value* side of the same registry: given a
//! setting name and a bag of raw source inputs, it walks the metadata
//! and returns the resolved value. Adding a new setting is then a
//! one-place change in `settings.toml` — no corresponding edit in the
//! `NpmConfig::apply` parser or anywhere else.
//!
//! Supported scalar types are `bool`, `string` (including `path` and
//! quoted-union enum strings), `int` (as `u64`), and `list<string>`.
//! Supported sources are `.npmrc` entries, aube's user config file, a
//! raw `pnpm-workspace.yaml` map, captured environment variables, and
//! parsed CLI flags.

use std::sync::OnceLock;

use crate::meta;

/// Process-wide CLI overrides registered from generic `--config.<key>`
/// flags. Walked by every `*_from_cli` helper *after* the per-callsite
/// `cli` slice, so command-specific flags keep first-match priority and
/// the generic form acts as a fallback that still wins over env / file
/// sources.
static GLOBAL_CLI_OVERRIDES: OnceLock<Vec<(String, String)>> = OnceLock::new();

/// Register the parsed `--config.<key>[=<value>]` pairs once per
/// process. Idempotent — second calls are silently ignored, matching
/// the other `set_global_*` helpers in the binary crate.
pub fn set_global_cli_overrides(overrides: Vec<(String, String)>) {
    let _ = GLOBAL_CLI_OVERRIDES.set(overrides);
}

fn global_cli_overrides() -> &'static [(String, String)] {
    GLOBAL_CLI_OVERRIDES.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Bundle of source inputs consumed by the per-setting typed
/// accessors in [`resolved`]. Each field is a borrowed view so
/// callers can reuse the same owned values across many lookups
/// without cloning.
///
/// File-source fields are split by scope (user vs project) so the
/// resolver can apply the locality principle — project-scope entries
/// outrank user-scope entries, and within a scope aube's own config
/// outranks `.npmrc`. See the module-level docs for the full chain.
pub struct ResolveCtx<'a> {
    /// Project-scope aube config (`<cwd>/.config/aube/config.toml`).
    /// Highest-precedence file source by default — a project may pin
    /// settings here as an alternative to committing them into the
    /// project `.npmrc` shared with npm/pnpm/yarn.
    pub project_aube_config: &'a [(String, String)],
    /// Project-scope `.npmrc` (`<cwd>/.npmrc`) plus any
    /// `npmrcAuthFile` it points at, in load order.
    pub project_npmrc: &'a [(String, String)],
    /// User-scope aube config (`~/.config/aube/config.toml`). Aube's
    /// authoritative store for user-level settings written via
    /// `aube config set` — outranks `~/.npmrc` so leftover entries in
    /// a shared `.npmrc` don't silently shadow what aube wrote.
    pub user_aube_config: &'a [(String, String)],
    /// User-scope `.npmrc` (`~/.npmrc` or `NPM_CONFIG_USERCONFIG`) plus
    /// pnpm's global `auth.ini`, in load order.
    pub user_npmrc: &'a [(String, String)],
    /// Raw top-level map from `pnpm-workspace.yaml` /
    /// `aube-workspace.yaml`, as returned by
    /// `aube_manifest::workspace::load_raw`.
    pub workspace_yaml: &'a std::collections::BTreeMap<String, yaml_serde::Value>,
    /// Raw top-level map from pnpm's *global* `config.yaml`
    /// (`<configDir>/config.yaml`, pnpm v11), parsed with the same YAML
    /// reader pnpm uses for the workspace manifest and consulted through
    /// the same `*_from_workspace_yaml` helpers (camelCase keys). It is a
    /// GLOBAL / user-scope source: by the default precedence it ranks
    /// below the project-root `workspace_yaml` (a project
    /// `pnpm-workspace.yaml` overrides the user's global `config.yaml`,
    /// matching pnpm v11) and above the user `.npmrc` / aube config.
    /// Populated under the GLOBAL-scope `read_pnpm_global_config` posture
    /// — on-by-default for standalone aube (which IS a pnpm-compatible
    /// PM), cleared UNCONDITIONALLY (cwd-independent) under the nub profile
    /// (nub's global config is its own neutral home; it never reads a
    /// pnpm-named global file); [`empty_yaml_map`] otherwise. This is a
    /// global source, so it is NOT gated on the project-derived
    /// `read_branded_pnpm_config`.
    pub global_config_yaml: &'a std::collections::BTreeMap<String, yaml_serde::Value>,
    /// Captured environment variables relevant to settings. In
    /// production this is populated by [`capture_env`]; tests build a
    /// literal slice. `sources.env` alias order defines priority; within
    /// one alias, lookups iterate from the end so later entries win.
    pub env: &'a [(String, String)],
    /// Parsed CLI flag values for the command being executed. Each
    /// entry is a `(flag_name, value)` pair where `flag_name` matches
    /// a `sources.cli` alias declared in `settings.toml`. Values
    /// should already be normalized to the raw form the type-specific
    /// parser expects (`"true"`/`"false"` for bools, etc).
    pub cli: &'a [(String, String)],
    /// Lowest-priority defaults supplied by an embedder. An embedding host
    /// (a tool that drives aube's command layer as a library) feeds setting
    /// defaults here through the normal settings path; every user- and
    /// project-level source overrides them. Standalone aube leaves this
    /// empty, so the per-setting built-in defaults from `settings.toml`
    /// apply unchanged. Keyed by the same canonical setting names as the
    /// file sources.
    pub embedder_defaults: &'a [(String, String)],
}

impl<'a> ResolveCtx<'a> {
    /// Construct a context that only sees the merged-`.npmrc` and
    /// workspace-yaml file sources. Convenience for tests and call
    /// sites that don't need scope splitting or env/cli plumbing.
    ///
    /// The supplied `.npmrc` slice is treated as project-scope so its
    /// values win over the (empty) user-scope sources — matching
    /// the install-time precedence callers used to rely on before the
    /// split.
    pub fn files_only(
        npmrc: &'a [(String, String)],
        workspace_yaml: &'a std::collections::BTreeMap<String, yaml_serde::Value>,
    ) -> Self {
        Self {
            project_aube_config: &[],
            project_npmrc: npmrc,
            user_aube_config: &[],
            user_npmrc: &[],
            workspace_yaml,
            // No global config.yaml on the reduced files-only path — these
            // callers (lockfile/workspace readers) resolve project-shaped
            // settings, not the user's global pnpm config.
            global_config_yaml: empty_yaml_map(),
            env: &[],
            cli: &[],
            // Process-global embedder defaults still apply on this reduced
            // path so embedder-fed setting defaults reach lockfile/workspace
            // readers that build a `files_only` ctx.
            embedder_defaults: embedder_defaults(),
        }
    }
}

/// A shared, empty `pnpm-workspace.yaml`-shaped map. Returned as the
/// default for `ResolveCtx::global_config_yaml` (and any other
/// yaml-map source) when the source is absent — lets construction sites
/// and tests fill the field without allocating a throwaway `BTreeMap`.
pub fn empty_yaml_map() -> &'static std::collections::BTreeMap<String, yaml_serde::Value> {
    static EMPTY: OnceLock<std::collections::BTreeMap<String, yaml_serde::Value>> = OnceLock::new();
    EMPTY.get_or_init(std::collections::BTreeMap::new)
}

/// Embedder-supplied setting defaults, registered once at startup by an
/// embedding host. Standalone aube never registers any, so this stays empty
/// and the per-setting built-in defaults from `settings.toml` apply. Each
/// entry is a `(canonical_setting_name, raw_value)` pair, parsed by the same
/// type-specific readers the file sources use.
static EMBEDDER_DEFAULTS: OnceLock<Vec<(String, String)>> = OnceLock::new();

/// Register embedder default settings. Idempotent — the first registration
/// wins. Not part of standalone aube's path; an embedder calls this (via the
/// library entry point) before any command resolves settings.
pub fn set_embedder_defaults(defaults: Vec<(String, String)>) {
    let _ = EMBEDDER_DEFAULTS.set(defaults);
}

/// The registered embedder defaults, or an empty slice when none were set.
pub fn embedder_defaults() -> &'static [(String, String)] {
    EMBEDDER_DEFAULTS.get().map_or(&[], Vec::as_slice)
}

/// Process-wide env snapshot. Captured once on first read so every
/// `ResolveCtx` walks the same list without repeating the
/// `std::env::vars()` syscall storm. Subprocesses can't mutate the
/// parent env, so a single capture is correct for the lifetime of the
/// CLI process.
static PROCESS_ENV: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| std::env::vars().collect());

/// Snapshot the process environment into a `(name, value)` list the
/// resolver can walk. Filtering happens at lookup time against the
/// setting's declared `env_vars` aliases, so this captures everything
/// upfront and lets the metadata decide what's relevant.
///
/// First caller in the process triggers the underlying `std::env::vars()`
/// walk; subsequent callers get a cheap `Vec` clone of the cached
/// snapshot. The clone keeps the existing `Vec<(String, String)>` API
/// surface; callers that want zero-alloc access can read [`process_env`]
/// directly.
pub fn capture_env() -> Vec<(String, String)> {
    PROCESS_ENV.clone()
}

/// Borrowed view of the process-wide env snapshot. Callers that only
/// need to read should prefer this over [`capture_env`] — no Vec
/// clone, no per-entry String clone.
pub fn process_env() -> &'static [(String, String)] {
    PROCESS_ENV.as_slice()
}

/// Typed per-setting accessors generated at build time from
/// `settings.toml`. One function per scalar setting (`bool`,
/// `string`/`path`/`url`, quoted-union enum, `int`, `list<string>`). The
/// function signature *is* the type check — `auto_install_peers`
/// returns `bool`, `store_dir` returns `Option<String>`, and
/// calling either on the wrong type is a compile error.
///
/// Default precedence, high-to-low:
///
/// ```text
/// cli > env
///     > workspace_yaml      (pnpm-workspace.yaml / aube-workspace.yaml)
///     > global_config_yaml  (<configDir>/config.yaml, pnpm v11, pnpm-incumbent only)
///     > project_aube_config (<cwd>/.config/aube/config.toml)
///     > project_npmrc       (<cwd>/.npmrc + npmrcAuthFile)
///     > user_aube_config    (~/.config/aube/config.toml)
///     > user_npmrc          (~/.npmrc + pnpm auth.ini)
/// ```
///
/// The file-source ordering matches pnpm's config precedence (v10.5+/v11):
/// the YAML settings sources outrank the project `.npmrc`. pnpm reads
/// `.npmrc` first and then merges the global `config.yaml` and project
/// `pnpm-workspace.yaml` over it (last-write-wins), so both YAML sources
/// beat `.npmrc`. Two principles fill in the rest:
///
/// - **YAML-over-`.npmrc` (pnpm parity)**: `workspace_yaml` and
///   `global_config_yaml` outrank `.npmrc`. A project
///   `pnpm-workspace.yaml` setting is not silently shadowed by a stale
///   `.npmrc` entry. (Both YAML maps are empty unless pnpm is the
///   provable incumbent, so under a non-pnpm project this is inert.)
/// - **Scope locality + aube authority** (below the YAMLs): project-scope
///   beats user-scope, and within a scope aube's own `config.toml`
///   beats `.npmrc` so values written via `aube config set` aren't
///   shadowed by leftover entries other tools (npm, pnpm, yarn) read.
///
/// The per-setting `precedence` override in `settings.toml` reorders
/// the file-based sources but cannot demote `cli` or `env` off the
/// top — CLI flags and environment variables always win. Bare names
/// `npmrc` and `aubeConfig` in a `precedence` list expand to their
/// project+user pair (project first); use the scope-qualified names
/// `projectNpmrc`/`userNpmrc`/`projectAubeConfig`/`userAubeConfig` for
/// fine-grained control.
///
/// Settings with concrete parseable defaults return the defaulted
/// value directly; settings whose default is undefined or contextual
/// still return `Option<T>`.
pub mod resolved {
    use super::ResolveCtx;
    include!(concat!(env!("OUT_DIR"), "/settings_resolved.rs"));
}

/// Resolve a `bool` setting by walking its declared `.npmrc` source
/// keys in reverse order (so a later `.npmrc` entry overrides an
/// earlier one). Returns `None` if the metadata entry doesn't exist,
/// the setting isn't a bool, or no source key was found in `entries`.
///
/// `entries` is one of the per-scope slices from
/// [`crate::ResolveCtx`] (e.g. `project_npmrc` or `user_npmrc`).
/// Within a single scope, iterating from the end gives last-write-wins
/// over duplicate keys.
pub(crate) fn bool_from_npmrc(setting: &str, entries: &[(String, String)]) -> Option<bool> {
    let meta = meta::find(setting)?;
    if meta.type_ != "bool" {
        return None;
    }
    for (key, raw) in entries.iter().rev() {
        if meta.npmrc_keys.contains(&key.as_str())
            && let Some(v) = parse_bool(raw)
        {
            return Some(v);
        }
    }
    None
}

/// Resolve a `string` setting by walking its declared `.npmrc` source
/// keys in reverse order. Mirrors [`bool_from_npmrc`] but returns the
/// raw value verbatim — trimming and further interpretation are left
/// to the caller, since "string" settings (e.g. `nodeVersion`,
/// registry URLs) have per-setting normalization rules.
pub fn string_from_npmrc(setting: &str, entries: &[(String, String)]) -> Option<String> {
    let meta = meta::find(setting)?;
    if !is_stringish(meta.type_) {
        return None;
    }
    for (key, raw) in entries.iter().rev() {
        if meta.npmrc_keys.contains(&key.as_str()) {
            return Some(raw.clone());
        }
    }
    None
}

/// Resolve a `bool` setting from a raw `pnpm-workspace.yaml` map,
/// walking the declared `sources.workspaceYaml` aliases. Returns
/// `None` if no alias is present in the map, the setting isn't a
/// bool, or the value isn't a boolean (or boolean-like string).
///
/// Aliases are walked in the order they appear in
/// `workspace_yaml_keys`; pnpm files don't permit duplicate top-level
/// keys, so precedence among aliases within one file is moot —
/// whichever one is present wins.
pub(crate) fn bool_from_workspace_yaml(
    setting: &str,
    raw: &std::collections::BTreeMap<String, yaml_serde::Value>,
) -> Option<bool> {
    let meta = meta::find(setting)?;
    if meta.type_ != "bool" {
        return None;
    }
    for key in meta.workspace_yaml_keys {
        let Some(val) = workspace_yaml_value(raw, key) else {
            continue;
        };
        match val {
            yaml_serde::Value::Bool(b) => return Some(*b),
            yaml_serde::Value::String(s) => {
                if let Some(b) = parse_bool(s) {
                    return Some(b);
                }
            }
            _ => {}
        }
    }
    None
}

/// Resolve a `string` setting from a raw `pnpm-workspace.yaml` map,
/// walking the declared `sources.workspaceYaml` aliases. Returns
/// `None` if no alias is present in the map, the setting isn't a
/// string, or the value is not a YAML string/number/bool scalar.
///
/// Non-string scalars (numbers, booleans) are coerced to their
/// lexical form. Complex values (sequences, mappings) return `None`
/// rather than a bogus rendering.
pub fn string_from_workspace_yaml(
    setting: &str,
    raw: &std::collections::BTreeMap<String, yaml_serde::Value>,
) -> Option<String> {
    let meta = meta::find(setting)?;
    if !is_stringish(meta.type_) {
        return None;
    }
    for key in meta.workspace_yaml_keys {
        let Some(val) = workspace_yaml_value(raw, key) else {
            continue;
        };
        match val {
            yaml_serde::Value::String(s) => return Some(s.clone()),
            yaml_serde::Value::Number(n) => return Some(n.to_string()),
            yaml_serde::Value::Bool(b) => return Some(b.to_string()),
            _ => {}
        }
    }
    None
}

/// True if this setting's declared type is one the generic string
/// helpers should accept: `string`, `path`, or an enum-style union
/// literal like `"highest" | "time-based"`. Mirrors the type set the
/// build-time generator emits as `Option<String>` accessors.
fn is_stringish(ty: &str) -> bool {
    matches!(ty, "string" | "path" | "url") || ty.starts_with('"')
}

/// Resolve an `int` setting from `.npmrc` entries, parsed as `u64`.
/// Mirrors [`bool_from_npmrc`].
pub(crate) fn u64_from_npmrc(setting: &str, entries: &[(String, String)]) -> Option<u64> {
    let meta = meta::find(setting)?;
    if meta.type_ != "int" {
        return None;
    }
    for (key, raw) in entries.iter().rev() {
        if meta.npmrc_keys.contains(&key.as_str())
            && let Ok(v) = raw.trim().parse::<u64>()
        {
            return Some(v);
        }
    }
    None
}

/// Resolve an `int` setting from a raw `pnpm-workspace.yaml` map.
/// Accepts YAML integers and stringified numbers.
pub(crate) fn u64_from_workspace_yaml(
    setting: &str,
    raw: &std::collections::BTreeMap<String, yaml_serde::Value>,
) -> Option<u64> {
    let meta = meta::find(setting)?;
    if meta.type_ != "int" {
        return None;
    }
    for key in meta.workspace_yaml_keys {
        let Some(val) = workspace_yaml_value(raw, key) else {
            continue;
        };
        match val {
            yaml_serde::Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    return Some(u);
                }
            }
            yaml_serde::Value::String(s) => {
                if let Ok(u) = s.trim().parse::<u64>() {
                    return Some(u);
                }
            }
            _ => {}
        }
    }
    None
}

/// Resolve a `list<string>` setting from `.npmrc` entries. pnpm and
/// npm accept either a JSON-ish array (`["a","b"]`) or a
/// comma-separated bare string (`a,b`).
pub(crate) fn string_list_from_npmrc(
    setting: &str,
    entries: &[(String, String)],
) -> Option<Vec<String>> {
    let meta = meta::find(setting)?;
    if meta.type_ != "list<string>" {
        return None;
    }
    for (key, raw) in entries.iter().rev() {
        if meta.npmrc_keys.contains(&key.as_str()) {
            return Some(parse_string_list(raw));
        }
    }
    None
}

/// Resolve a `list<string>` setting from a raw workspace yaml map.
/// Accepts YAML sequences of strings, or a single string that gets
/// parsed with [`parse_string_list`] (for pnpm-compat YAML files
/// that stringify the list).
pub(crate) fn string_list_from_workspace_yaml(
    setting: &str,
    raw: &std::collections::BTreeMap<String, yaml_serde::Value>,
) -> Option<Vec<String>> {
    let meta = meta::find(setting)?;
    if meta.type_ != "list<string>" {
        return None;
    }
    for key in meta.workspace_yaml_keys {
        let Some(val) = workspace_yaml_value(raw, key) else {
            continue;
        };
        match val {
            yaml_serde::Value::Sequence(seq) => {
                let items: Vec<String> = seq
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                return Some(items);
            }
            yaml_serde::Value::String(s) => return Some(parse_string_list(s)),
            _ => {}
        }
    }
    None
}

pub fn workspace_yaml_value<'a>(
    raw: &'a std::collections::BTreeMap<String, yaml_serde::Value>,
    key: &str,
) -> Option<&'a yaml_serde::Value> {
    let mut parts = key.split('.');
    let first = parts.next()?;
    let mut value = raw.get(first)?;
    for part in parts {
        let yaml_serde::Value::Mapping(map) = value else {
            return None;
        };
        value = map.get(yaml_serde::Value::String(part.to_string()))?;
    }
    Some(value)
}

fn raw_from_env<'a>(meta: &meta::SettingMeta, env: &'a [(String, String)]) -> Option<&'a str> {
    for alias in meta.env_vars.iter().rev() {
        // Gate the tool-branded alias (`AUBE_<NAME>`) on the active embedder's
        // `env_prefix`: standalone aube (`Some("AUBE")`) reads it as before, an
        // embedder with `None` reads no branded settings env vars. The neutral
        // `npm_config_*` / `NPM_CONFIG_*` aliases and bare external vars are
        // never gated. `env_prefix` is the single binary switch for the whole
        // branded-env settings surface.
        if !aube_util::env::branded_env_alias_enabled(alias) {
            continue;
        }
        for (key, raw) in env.iter().rev() {
            if key == alias {
                return Some(raw);
            }
        }
    }
    None
}

/// Resolve a `bool` setting from a captured environment snapshot,
/// walking the declared `sources.env` aliases in reverse priority order.
/// Returns `None` on unknown setting, wrong type, or unparseable value.
pub(crate) fn bool_from_env(setting: &str, env: &[(String, String)]) -> Option<bool> {
    let meta = meta::find(setting)?;
    if meta.type_ != "bool" {
        return None;
    }
    raw_from_env(meta, env).and_then(parse_bool)
}

/// Resolve a `string` setting from a captured environment snapshot.
pub fn string_from_env(setting: &str, env: &[(String, String)]) -> Option<String> {
    let meta = meta::find(setting)?;
    if !is_stringish(meta.type_) {
        return None;
    }
    raw_from_env(meta, env).map(ToOwned::to_owned)
}

/// Resolve an `int` setting from a captured environment snapshot.
pub(crate) fn u64_from_env(setting: &str, env: &[(String, String)]) -> Option<u64> {
    let meta = meta::find(setting)?;
    if meta.type_ != "int" {
        return None;
    }
    raw_from_env(meta, env).and_then(|raw| raw.trim().parse::<u64>().ok())
}

/// Resolve a `list<string>` setting from a captured environment
/// snapshot. Accepts the same stringified forms as `.npmrc`.
pub(crate) fn string_list_from_env(setting: &str, env: &[(String, String)]) -> Option<Vec<String>> {
    let meta = meta::find(setting)?;
    if meta.type_ != "list<string>" {
        return None;
    }
    raw_from_env(meta, env).map(parse_string_list)
}

/// True if the user-supplied CLI key targets `meta`. Matches against
/// the declared `sources.cli` aliases first (preserving exact behavior
/// for command-specific flags) and then falls back to the canonical
/// pnpm name in either kebab- or camelCase form so generic
/// `--config.<key>` overrides resolve regardless of which spelling the
/// user typed.
fn cli_key_matches(key: &str, meta: &meta::SettingMeta) -> bool {
    if meta.cli_flags.contains(&key) {
        return true;
    }
    if key == meta.name {
        return true;
    }
    let key_kebab = to_kebab_case(key);
    if key_kebab == to_kebab_case(meta.name) {
        return true;
    }
    false
}

/// Lower-case kebab form of a setting / flag identifier. Splits on
/// `-`, `_`, dotted-path segments, and lowercase→UPPER transitions so
/// callers can compare `strict-dep-builds`, `strictDepBuilds`,
/// `STRICT_DEP_BUILDS`, and `strict_dep_builds` interchangeably. Dots
/// are preserved so nested settings like
/// `peerDependencyRules.ignoreMissing` keep their path structure.
///
/// Consecutive uppercase runs (e.g. `XMLConfig`) are collapsed to a
/// single lowercase token (`xmlconfig`), matching the auto-alias
/// generator in `aube-settings/build.rs`. No pnpm setting today contains
/// an internal acronym, so the imperfection is invisible in practice; if
/// one is ever added, the synthesized npmrc alias and this matcher have
/// to evolve together.
fn to_kebab_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev_lower = false;
    for c in s.chars() {
        if c == '_' || c == '-' {
            if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
            prev_lower = false;
        } else if c == '.' {
            out.push('.');
            prev_lower = false;
        } else if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else {
            out.push(c);
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        }
    }
    out
}

/// Walk the per-callsite `cli` slice (newest entry first), then the
/// process-global `--config.<key>` overrides. The `accept` predicate
/// lets typed callers (`bool`, `int`) keep scanning past an unparseable
/// value so a later valid duplicate still wins — matching the original
/// per-source loops. String / string-list callers pass `|_| true`; the
/// global overrides have `'static` storage so the merged lifetime is
/// whichever `cli` borrow the caller passed in.
fn cli_raw_for<'a>(
    meta: &meta::SettingMeta,
    cli: &'a [(String, String)],
    accept: impl Fn(&str) -> bool,
) -> Option<&'a str> {
    for (key, raw) in cli.iter().rev() {
        if cli_key_matches(key, meta) && accept(raw.as_str()) {
            return Some(raw.as_str());
        }
    }
    for (key, raw) in global_cli_overrides().iter().rev() {
        if cli_key_matches(key, meta) && accept(raw.as_str()) {
            return Some(raw.as_str());
        }
    }
    None
}

/// Resolve a `bool` setting from a parsed CLI flag bag. The bag
/// entries are whatever each command extracts from its clap struct
/// before building the `ResolveCtx`. Keys may be either an alias
/// declared in `sources.cli` or the canonical setting name (in any
/// reasonable case form), so generic `--config.<key>` overrides reach
/// every setting without per-flag wiring. An unparseable value (e.g.
/// `--config.strictDepBuilds=notabool`) is skipped rather than masking
/// an earlier valid entry — caller still gets `None` if every match is
/// invalid, matching how `bool_from_npmrc` handles the same case.
pub(crate) fn bool_from_cli(setting: &str, cli: &[(String, String)]) -> Option<bool> {
    let meta = meta::find(setting)?;
    if meta.type_ != "bool" {
        return None;
    }
    cli_raw_for(meta, cli, |raw| parse_bool(raw).is_some()).and_then(parse_bool)
}

/// Resolve a `string` setting from a parsed CLI flag bag.
pub fn string_from_cli(setting: &str, cli: &[(String, String)]) -> Option<String> {
    let meta = meta::find(setting)?;
    if !is_stringish(meta.type_) {
        return None;
    }
    cli_raw_for(meta, cli, |_| true).map(ToOwned::to_owned)
}

/// Resolve an `int` setting from a parsed CLI flag bag.
pub(crate) fn u64_from_cli(setting: &str, cli: &[(String, String)]) -> Option<u64> {
    let meta = meta::find(setting)?;
    if meta.type_ != "int" {
        return None;
    }
    cli_raw_for(meta, cli, |raw| raw.trim().parse::<u64>().is_ok())
        .and_then(|raw| raw.trim().parse::<u64>().ok())
}

/// Resolve a `list<string>` setting from a parsed CLI flag bag.
pub(crate) fn string_list_from_cli(setting: &str, cli: &[(String, String)]) -> Option<Vec<String>> {
    let meta = meta::find(setting)?;
    if meta.type_ != "list<string>" {
        return None;
    }
    cli_raw_for(meta, cli, |_| true).map(parse_string_list)
}

/// Parse a pnpm/npm-style stringified list. Accepts a JSON-ish array
/// `["a","b"]` or a plain comma-separated list `a,b,c`. Empty entries
/// and surrounding whitespace/quotes are trimmed.
fn parse_string_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return inner
            .split(',')
            .map(|s| {
                s.trim()
                    .trim_matches(|c: char| c == '"' || c == '\'')
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .collect();
    }
    trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse a `.npmrc`-style boolean. npm/pnpm accept `true`/`false` and
/// the shell-style `"1"` / `"0"`. Anything else returns `None` so the
/// caller's default takes over.
///
/// Public so `aube-registry` and any other crate that hand-parses
/// `.npmrc` scalar values can share the same accept-set — a future
/// tweak (e.g. accepting `yes`/`no`) lands in one place.
pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn workspace_yaml_value_resolves_dotted_paths() {
        let raw: BTreeMap<String, yaml_serde::Value> =
            yaml_serde::from_str("outer:\n  inner:\n    key: value\n").unwrap();

        assert_eq!(
            workspace_yaml_value(&raw, "outer.inner.key").and_then(|v| v.as_str()),
            Some("value")
        );
        assert!(workspace_yaml_value(&raw, "outer.missing.key").is_none());
    }

    #[test]
    fn resolves_auto_install_peers_kebab_case() {
        let e = entries(&[("auto-install-peers", "false")]);
        assert_eq!(bool_from_npmrc("autoInstallPeers", &e), Some(false));
    }

    #[test]
    fn resolves_auto_install_peers_camel_case() {
        // settings.toml lists both spellings in sources.npmrc.
        let e = entries(&[("autoInstallPeers", "true")]);
        assert_eq!(bool_from_npmrc("autoInstallPeers", &e), Some(true));
    }

    #[test]
    fn resolves_package_manager_strict_kebab_case() {
        // pnpm's `.npmrc` convention is kebab-case. Real-world yarn/npm
        // projects that want to bypass the guardrail need the kebab
        // spelling to work. `packageManagerStrict` is a tri-state
        // (`off` | `warn` | `error`) with bool spellings accepted for
        // back-compat, so the accessor returns a raw string.
        let e = entries(&[("package-manager-strict", "false")]);
        assert_eq!(
            string_from_npmrc("packageManagerStrict", &e),
            Some("false".to_string())
        );
    }

    #[test]
    fn resolves_package_manager_strict_camel_case() {
        let e = entries(&[("packageManagerStrict", "warn")]);
        assert_eq!(
            string_from_npmrc("packageManagerStrict", &e),
            Some("warn".to_string())
        );
    }

    #[test]
    fn resolves_package_manager_strict_version_kebab_case() {
        let e = entries(&[("package-manager-strict-version", "true")]);
        assert_eq!(
            bool_from_npmrc("packageManagerStrictVersion", &e),
            Some(true)
        );
    }

    #[test]
    fn resolves_git_shallow_hosts_kebab_case() {
        // pnpm's `.npmrc` convention is kebab-case; settings.toml
        // must list both spellings so projects copied from a pnpm
        // setup keep working without a rename.
        let e = entries(&[("git-shallow-hosts", "[example.invalid, other.test]")]);
        assert_eq!(
            string_list_from_npmrc("gitShallowHosts", &e),
            Some(vec![
                "example.invalid".to_string(),
                "other.test".to_string(),
            ])
        );
    }

    #[test]
    fn resolves_git_shallow_hosts_camel_case() {
        let e = entries(&[("gitShallowHosts", "example.invalid")]);
        assert_eq!(
            string_list_from_npmrc("gitShallowHosts", &e),
            Some(vec!["example.invalid".to_string()])
        );
    }

    #[test]
    fn returns_none_when_no_key_matches() {
        let e = entries(&[("registry", "https://x.test/")]);
        assert_eq!(bool_from_npmrc("autoInstallPeers", &e), None);
    }

    #[test]
    fn returns_none_for_unknown_setting() {
        let e = entries(&[("auto-install-peers", "false")]);
        assert_eq!(
            bool_from_npmrc("totally-fake-setting", &e),
            None,
            "unknown setting must return None without crashing"
        );
    }

    #[test]
    fn parses_numeric_shell_booleans() {
        assert_eq!(
            bool_from_npmrc("autoInstallPeers", &entries(&[("auto-install-peers", "1")])),
            Some(true)
        );
        assert_eq!(
            bool_from_npmrc("autoInstallPeers", &entries(&[("auto-install-peers", "0")])),
            Some(false)
        );
    }

    #[test]
    fn later_entries_win_over_earlier_ones() {
        // user .npmrc sets false, project .npmrc overrides to true.
        // load_npmrc_entries returns user-first then project-later, so
        // iterating from the end gives the project value.
        let e = entries(&[
            ("auto-install-peers", "false"),
            ("auto-install-peers", "true"),
        ]);
        assert_eq!(bool_from_npmrc("autoInstallPeers", &e), Some(true));
    }

    #[test]
    fn ignores_unparseable_value_and_falls_back() {
        // A garbage value should not poison the lookup — we should
        // fall through to an earlier valid entry.
        let e = entries(&[
            ("auto-install-peers", "false"),
            ("auto-install-peers", "maybe"),
        ]);
        assert_eq!(bool_from_npmrc("autoInstallPeers", &e), Some(false));
    }

    fn raw_yaml(src: &str) -> std::collections::BTreeMap<String, yaml_serde::Value> {
        yaml_serde::from_str(src).expect("test fixture is valid yaml")
    }

    #[test]
    fn workspace_yaml_resolves_bool_field() {
        let m = raw_yaml("autoInstallPeers: false\n");
        assert_eq!(
            bool_from_workspace_yaml("autoInstallPeers", &m),
            Some(false)
        );
    }

    #[test]
    fn workspace_yaml_returns_none_when_absent() {
        let m = raw_yaml("packages:\n  - 'pkgs/*'\n");
        assert_eq!(bool_from_workspace_yaml("autoInstallPeers", &m), None);
    }

    #[test]
    fn workspace_yaml_accepts_stringified_bool() {
        // YAML normally parses bare `true`/`false` as booleans, but a
        // quoted string should still resolve via `parse_bool`.
        let m = raw_yaml("autoInstallPeers: \"false\"\n");
        assert_eq!(
            bool_from_workspace_yaml("autoInstallPeers", &m),
            Some(false)
        );
    }

    #[test]
    fn workspace_yaml_ignores_non_bool_setting() {
        // storeDir is a string setting — the bool helper refuses it.
        let m = raw_yaml("storeDir: /tmp/x\n");
        assert_eq!(bool_from_workspace_yaml("storeDir", &m), None);
    }

    #[test]
    fn workspace_yaml_resolves_string_field() {
        let m = raw_yaml("storeDir: /tmp/my-store\n");
        assert_eq!(
            string_from_workspace_yaml("storeDir", &m),
            Some("/tmp/my-store".to_string())
        );
    }

    #[test]
    fn workspace_yaml_string_ignores_bool_setting() {
        let m = raw_yaml("autoInstallPeers: false\n");
        assert_eq!(string_from_workspace_yaml("autoInstallPeers", &m), None);
    }

    #[test]
    fn workspace_yaml_resolves_save_prefix() {
        // pnpm reads a top-level `savePrefix` from pnpm-workspace.yaml and
        // applies it when writing added deps (`add is-odd` → `~3.0.1`). The
        // `savePrefix` setting declares `sources.workspaceYaml = ["savePrefix"]`
        // so the resolver honors it as a YAML source (above `.npmrc`).
        let m = raw_yaml("savePrefix: \"~\"\npackages: []\n");
        assert_eq!(
            string_from_workspace_yaml("savePrefix", &m),
            Some("~".to_string())
        );
    }

    #[test]
    fn workspace_yaml_resolves_nested_string_list_field() {
        let m = raw_yaml("updateConfig:\n  ignoreDependencies:\n    - is-odd\n    - is-even\n");
        assert_eq!(
            string_list_from_workspace_yaml("updateConfig.ignoreDependencies", &m),
            Some(vec!["is-odd".to_string(), "is-even".to_string()])
        );
    }

    #[test]
    fn generated_accessor_prefers_workspace_yaml_over_npmrc() {
        // pnpm parity (v10.5+/v11): pnpm-workspace.yaml wins over the
        // project `.npmrc` (`files_only` treats the npmrc slice as
        // project-scope).
        let npmrc = entries(&[("auto-install-peers", "false")]);
        let ws = raw_yaml("autoInstallPeers: true\n");
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert!(resolved::auto_install_peers(&ctx));
    }

    #[test]
    fn generated_accessor_falls_through_to_workspace_yaml() {
        let npmrc: Vec<(String, String)> = Vec::new();
        let ws = raw_yaml("autoInstallPeers: false\n");
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert!(!resolved::auto_install_peers(&ctx));
    }

    #[test]
    fn generated_accessor_returns_declared_default_when_no_source_matches() {
        let npmrc: Vec<(String, String)> = Vec::new();
        let ws: std::collections::BTreeMap<String, yaml_serde::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert!(resolved::auto_install_peers(&ctx));
    }

    #[test]
    fn env_resolves_auto_install_peers_via_declared_aliases() {
        // `settings.toml` declares both npm-compatible env spellings.
        // This test guards that the metadata-driven env resolver honors
        // them without any generated alias synthesis.
        let env_lower = vec![(
            "npm_config_auto_install_peers".to_string(),
            "false".to_string(),
        )];
        assert_eq!(bool_from_env("autoInstallPeers", &env_lower), Some(false));
        let env_upper = vec![(
            "NPM_CONFIG_AUTO_INSTALL_PEERS".to_string(),
            "true".to_string(),
        )];
        assert_eq!(bool_from_env("autoInstallPeers", &env_upper), Some(true));
    }

    #[test]
    fn env_resolves_synthesized_pnpm_config_family() {
        // build.rs auto-synthesizes `pnpm_config_<x>` / `PNPM_CONFIG_<X>`
        // from each declared `npm_config_<x>` / `NPM_CONFIG_<X>` alias, so
        // pnpm v11's general-settings env family resolves with no
        // per-setting edit in settings.toml. Default engine context has
        // `read_branded_pnpm_config = true`, so the pnpm-named family is
        // honored (the pnpm-incumbent gate; off-incumbent coverage lives
        // in the gate's own unit test in aube-util).
        let lower = vec![(
            "pnpm_config_auto_install_peers".to_string(),
            "false".to_string(),
        )];
        assert_eq!(bool_from_env("autoInstallPeers", &lower), Some(false));
        let upper = vec![(
            "PNPM_CONFIG_AUTO_INSTALL_PEERS".to_string(),
            "true".to_string(),
        )];
        assert_eq!(bool_from_env("autoInstallPeers", &upper), Some(true));
    }

    #[test]
    fn pnpm_config_env_outranks_npm_config_env() {
        // pnpm's own resolution prefers `pnpm_config_*` over the
        // npm-compat `npm_config_*` fallback. build.rs orders the
        // synthesized pnpm alias after the npm alias it derives from, and
        // `raw_from_env` walks aliases in reverse, so the pnpm spelling
        // wins when both are present.
        let env = entries(&[
            ("npm_config_auto_install_peers", "false"),
            ("pnpm_config_auto_install_peers", "true"),
        ]);
        assert_eq!(bool_from_env("autoInstallPeers", &env), Some(true));
    }

    #[test]
    fn cli_bag_resolves_resolution_mode_string() {
        // `resolutionMode` is a quoted-union (string) setting with a
        // `sources.cli = ["resolution-mode"]` declaration.
        let cli = vec![("resolution-mode".to_string(), "time-based".to_string())];
        assert_eq!(
            string_from_cli("resolutionMode", &cli),
            Some("time-based".to_string())
        );
    }

    #[test]
    fn cli_bag_matches_canonical_name_for_settings_without_declared_cli_alias() {
        // `strictDepBuilds` declares `sources.cli = []`, but generic
        // `--config.<key>` overrides should still reach it via the
        // canonical name in any reasonable case form.
        let kebab = vec![("strict-dep-builds".to_string(), "true".to_string())];
        assert_eq!(bool_from_cli("strictDepBuilds", &kebab), Some(true));

        let camel = vec![("strictDepBuilds".to_string(), "true".to_string())];
        assert_eq!(bool_from_cli("strictDepBuilds", &camel), Some(true));

        let screaming = vec![("STRICT_DEP_BUILDS".to_string(), "false".to_string())];
        assert_eq!(bool_from_cli("strictDepBuilds", &screaming), Some(false));
    }

    #[test]
    fn cli_bag_keeps_existing_alias_match_for_declared_settings() {
        // `verifyStoreIntegrity` declares `sources.cli = ["verify-store-integrity"]`.
        // The exact alias must keep working unchanged.
        let cli = vec![("verify-store-integrity".to_string(), "true".to_string())];
        assert_eq!(bool_from_cli("verifyStoreIntegrity", &cli), Some(true));
    }

    #[test]
    fn cli_bag_falls_through_unparseable_values_to_earlier_valid_entry() {
        // Regression: an unparseable `--config.<key>=garbage` must not
        // mask an earlier valid entry for the same setting. Iteration
        // is reverse, so the later (garbage) entry is visited first;
        // the helper has to keep scanning rather than commit to it.
        let cli = vec![
            ("strictDepBuilds".to_string(), "true".to_string()),
            ("strictDepBuilds".to_string(), "notabool".to_string()),
        ];
        assert_eq!(bool_from_cli("strictDepBuilds", &cli), Some(true));

        let cli = vec![
            ("network-concurrency".to_string(), "8".to_string()),
            ("network-concurrency".to_string(), "garbage".to_string()),
        ];
        assert_eq!(u64_from_cli("networkConcurrency", &cli), Some(8));
    }

    #[test]
    fn cli_beats_env_beats_every_file_source() {
        // CLI and env always win over file sources. This test hits
        // every layer (cli, env, project npmrc, workspace yaml) by
        // setting a unique value at each and asserting the generated
        // accessor returns the CLI value.
        let npmrc = entries(&[("auto-install-peers", "false")]);
        let ws = raw_yaml("autoInstallPeers: false\n");
        let env = vec![(
            "npm_config_auto_install_peers".to_string(),
            "false".to_string(),
        )];
        let cli = vec![("auto-install-peers".to_string(), "true".to_string())];
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &npmrc,
            user_aube_config: &[],
            user_npmrc: &[],
            workspace_yaml: &ws,
            global_config_yaml: empty_yaml_map(),
            env: &env,
            cli: &cli,
            embedder_defaults: &[],
        };
        assert!(resolved::auto_install_peers(&ctx));
    }

    #[test]
    fn env_wins_over_file_sources_when_cli_empty() {
        let npmrc = entries(&[("auto-install-peers", "false")]);
        let aube_config = entries(&[("autoInstallPeers", "false")]);
        let ws = raw_yaml("autoInstallPeers: false\n");
        let env = vec![(
            "npm_config_auto_install_peers".to_string(),
            "true".to_string(),
        )];
        let ctx = ResolveCtx {
            project_aube_config: &aube_config,
            project_npmrc: &npmrc,
            user_aube_config: &aube_config,
            user_npmrc: &npmrc,
            workspace_yaml: &ws,
            global_config_yaml: empty_yaml_map(),
            env: &env,
            cli: &[],
            embedder_defaults: &[],
        };
        assert!(resolved::auto_install_peers(&ctx));
    }

    #[test]
    fn minimum_release_age_honors_per_setting_precedence_override() {
        // `minimumReleaseAge` overrides the default file precedence to
        // `["workspaceYaml", "npmrc"]`. With `aubeConfig` appended at
        // the tail, the effective order is workspaceYaml > npmrc >
        // aubeConfig — workspace YAML wins when present, and
        // `config.toml` is consulted only as a last resort.
        let aube_config = entries(&[("minimumReleaseAge", "2880")]);
        let ws = raw_yaml("minimumReleaseAge: 1440\n");
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &[],
            user_aube_config: &aube_config,
            user_npmrc: &[],
            workspace_yaml: &ws,
            global_config_yaml: empty_yaml_map(),
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert_eq!(resolved::minimum_release_age(&ctx), 1440);

        let ws = BTreeMap::new();
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &[],
            user_aube_config: &aube_config,
            user_npmrc: &[],
            workspace_yaml: &ws,
            global_config_yaml: empty_yaml_map(),
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert_eq!(resolved::minimum_release_age(&ctx), 2880);
    }

    #[test]
    fn user_aube_config_wins_over_user_npmrc_by_default() {
        // Within user-scope, `~/.config/aube/config.toml` outranks
        // `~/.npmrc` so values aube wrote via `aube config set` are
        // authoritative — a leftover entry in `~/.npmrc` (which other
        // tools like npm/pnpm/yarn also read) does not silently shadow
        // them. `autoInstallPeers` has no per-setting precedence
        // override, so it follows the default.
        let user_npmrc = entries(&[("auto-install-peers", "false")]);
        let user_aube_config = entries(&[("autoInstallPeers", "true")]);
        let ws = BTreeMap::new();
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &[],
            user_aube_config: &user_aube_config,
            user_npmrc: &user_npmrc,
            workspace_yaml: &ws,
            global_config_yaml: empty_yaml_map(),
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert!(
            resolved::auto_install_peers(&ctx),
            "user aube_config=true should win over user npmrc=false"
        );
    }

    #[test]
    fn project_npmrc_wins_over_user_aube_config_by_default() {
        // Locality principle: a project `.npmrc` outranks user-scope
        // `~/.config/aube/config.toml`. A repo-specific override should
        // not be silently shadowed by a user-level aube preference.
        let project_npmrc = entries(&[("auto-install-peers", "false")]);
        let user_aube_config = entries(&[("autoInstallPeers", "true")]);
        let ws = BTreeMap::new();
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &project_npmrc,
            user_aube_config: &user_aube_config,
            user_npmrc: &[],
            workspace_yaml: &ws,
            global_config_yaml: empty_yaml_map(),
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert!(
            !resolved::auto_install_peers(&ctx),
            "project npmrc=false should win over user aube_config=true"
        );
    }

    #[test]
    fn project_aube_config_wins_over_project_npmrc_by_default() {
        // Within project-scope, `<cwd>/.config/aube/config.toml`
        // outranks `<cwd>/.npmrc` — same authority principle as the
        // user-scope pair.
        let project_npmrc = entries(&[("auto-install-peers", "false")]);
        let project_aube_config = entries(&[("autoInstallPeers", "true")]);
        let ws = BTreeMap::new();
        let ctx = ResolveCtx {
            project_aube_config: &project_aube_config,
            project_npmrc: &project_npmrc,
            user_aube_config: &[],
            user_npmrc: &[],
            workspace_yaml: &ws,
            global_config_yaml: empty_yaml_map(),
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert!(
            resolved::auto_install_peers(&ctx),
            "project aube_config=true should win over project npmrc=false"
        );
    }

    #[test]
    fn workspace_yaml_wins_over_user_sources_by_default() {
        // `pnpm-workspace.yaml` / `aube-workspace.yaml` live at the
        // project root, so by the scope-locality principle they must
        // outrank both user `.npmrc` and user `config.toml`. Without
        // this, project-scope writes routed to the workspace yaml
        // would be silently shadowed by anything the user has at
        // `~/.config/aube/config.toml` or `~/.npmrc`.
        let user_npmrc = entries(&[("auto-install-peers", "true")]);
        let user_aube_config = entries(&[("autoInstallPeers", "true")]);
        let ws = raw_yaml("autoInstallPeers: false\n");
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &[],
            user_aube_config: &user_aube_config,
            user_npmrc: &user_npmrc,
            workspace_yaml: &ws,
            global_config_yaml: empty_yaml_map(),
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert!(
            !resolved::auto_install_peers(&ctx),
            "workspace yaml should win over user-scope sources"
        );
    }

    #[test]
    fn workspace_yaml_wins_over_project_npmrc_by_default() {
        // pnpm parity (v10.5+/v11): a project `pnpm-workspace.yaml`
        // setting outranks the same key in the project `.npmrc`. pnpm
        // merges the workspace YAML over the `.npmrc`-derived config
        // (last-write-wins), so a stale `.npmrc` entry must not shadow
        // the workspace YAML. `autoInstallPeers` has no per-setting
        // precedence override, so it exercises the default order.
        let project_npmrc = entries(&[("auto-install-peers", "true")]);
        let ws = raw_yaml("autoInstallPeers: false\n");
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &project_npmrc,
            user_aube_config: &[],
            user_npmrc: &[],
            workspace_yaml: &ws,
            global_config_yaml: empty_yaml_map(),
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert!(
            !resolved::auto_install_peers(&ctx),
            "workspace yaml=false should win over project npmrc=true"
        );
    }

    #[test]
    fn global_config_yaml_wins_over_project_npmrc_by_default() {
        // pnpm v11 ranks the global `<configDir>/config.yaml` above the
        // project `.npmrc` (it's merged over the `.npmrc`-derived
        // config before the project workspace YAML). With no project
        // workspace YAML present, global config.yaml still beats
        // `.npmrc`.
        let project_npmrc = entries(&[("auto-install-peers", "true")]);
        let global_yaml = raw_yaml("autoInstallPeers: false\n");
        let ws = BTreeMap::new();
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &project_npmrc,
            user_aube_config: &[],
            user_npmrc: &[],
            workspace_yaml: &ws,
            global_config_yaml: &global_yaml,
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert!(
            !resolved::auto_install_peers(&ctx),
            "global config.yaml=false should win over project npmrc=true"
        );
    }

    #[test]
    fn workspace_yaml_wins_over_global_config_yaml_by_default() {
        // pnpm v11: a project `pnpm-workspace.yaml` overrides the user's
        // global `config.yaml` (the workspace YAML is merged last).
        let global_yaml = raw_yaml("autoInstallPeers: true\n");
        let ws = raw_yaml("autoInstallPeers: false\n");
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &[],
            user_aube_config: &[],
            user_npmrc: &[],
            workspace_yaml: &ws,
            global_config_yaml: &global_yaml,
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert!(
            !resolved::auto_install_peers(&ctx),
            "project workspace yaml=false should win over global config.yaml=true"
        );
    }

    #[test]
    fn env_alias_order_defines_priority() {
        let env = entries(&[
            ("CI", "true"),
            ("NPM_CONFIG_CI", "false"),
            ("npm_config_no_proxy", ".internal"),
        ]);
        assert_eq!(bool_from_env("ci", &env), Some(true));
        assert_eq!(
            string_from_env("noProxy", &env),
            Some(".internal".to_string())
        );
    }

    #[test]
    fn generated_enum_accessor_returns_typed_variant() {
        // `resolutionMode` is an enum-style union with a concrete
        // default. The generator should emit `resolved::ResolutionMode`
        // and a non-optional accessor instead of the old `Option<String>`.
        // Callers match on the variant rather than hand-parsing the raw
        // string.
        let npmrc = entries(&[("resolutionMode", "time-based")]);
        let ws: std::collections::BTreeMap<String, yaml_serde::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(
            resolved::resolution_mode(&ctx),
            resolved::ResolutionMode::TimeBased
        );
    }

    #[test]
    fn generated_enum_accessor_uses_default_for_unknown_variant() {
        // An unrecognized value should not pollute the result — the
        // accessor falls back to the declared default when it has one.
        let npmrc = entries(&[("nodeLinker", "totally-fake")]);
        let ws: std::collections::BTreeMap<String, yaml_serde::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::node_linker(&ctx), resolved::NodeLinker::Isolated);
    }

    #[test]
    fn generated_enum_accessor_preserves_strict_precedence_on_unknown_value() {
        // Regression: an unrecognized value in a higher-precedence
        // source must NOT fall through to a lower-precedence source.
        // The generator used to apply `from_str_normalized` per-source
        // via `.and_then`, which silently skipped the typo and let the
        // lower source win — a strict precedence violation.
        //
        // pnpm-workspace.yaml outranks the project `.npmrc` (pnpm
        // v10.5+/v11), so an unparseable value in the workspace YAML
        // must block the `.npmrc` value and fall back to the default.
        let npmrc = entries(&[("nodeLinker", "hoisted")]);
        let ws = raw_yaml("nodeLinker: totally-fake\n");
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(
            resolved::node_linker(&ctx),
            resolved::NodeLinker::Isolated,
            "pnpm-workspace.yaml had a raw value, even if unparseable — \
             it must win over the project .npmrc and fall back to the \
             generated default"
        );
    }

    #[test]
    fn generated_enum_accessor_is_case_insensitive() {
        // pnpm normalizes enum values before matching; the generated
        // `from_str_normalized` mirrors that.
        let npmrc = entries(&[("nodeLinker", "Hoisted")]);
        let ws: std::collections::BTreeMap<String, yaml_serde::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::node_linker(&ctx), resolved::NodeLinker::Hoisted);
    }

    #[test]
    fn generated_enum_accessor_reads_kebab_case_npmrc_alias() {
        // pnpm's `.npmrc` docs use `node-linker=hoisted` (kebab-case).
        // aube must accept it alongside the camelCase `nodeLinker` form —
        // otherwise the setting is silently ignored for anyone copying
        // from pnpm docs.
        let npmrc = entries(&[("node-linker", "hoisted")]);
        let ws: std::collections::BTreeMap<String, yaml_serde::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::node_linker(&ctx), resolved::NodeLinker::Hoisted);
    }

    #[test]
    fn link_workspace_packages_accepts_deep_from_workspace_yaml() {
        let npmrc: Vec<(String, String)> = Vec::new();
        let ws = raw_yaml("linkWorkspacePackages: deep\n");
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(
            resolved::link_workspace_packages(&ctx),
            resolved::LinkWorkspacePackages::Deep
        );
    }

    #[test]
    fn link_workspace_packages_accepts_yaml_bool_values() {
        let npmrc: Vec<(String, String)> = Vec::new();
        let ws = raw_yaml("linkWorkspacePackages: true\n");
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(
            resolved::link_workspace_packages(&ctx),
            resolved::LinkWorkspacePackages::True
        );
    }

    #[test]
    fn link_workspace_packages_accepts_deep_from_npmrc() {
        let npmrc = entries(&[("link-workspace-packages", "deep")]);
        let ws: std::collections::BTreeMap<String, yaml_serde::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(
            resolved::link_workspace_packages(&ctx),
            resolved::LinkWorkspacePackages::Deep
        );
    }

    #[test]
    fn npmrc_accepts_kebab_alias_for_camel_only_setting() {
        // `virtualStoreDirMaxLength` is declared in settings.toml
        // with the single npmrc key `virtualStoreDirMaxLength`. The
        // generator must auto-synthesize the kebab alias
        // `virtual-store-dir-max-length` so users copying from pnpm's
        // `.npmrc` docs get the expected behaviour.
        let npmrc = entries(&[("virtual-store-dir-max-length", "40")]);
        let ws: std::collections::BTreeMap<String, yaml_serde::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::virtual_store_dir_max_length(&ctx), Some(40));
    }

    #[test]
    fn npmrc_accepts_camel_alias_for_kebab_only_setting() {
        // Mirror case: `prefer-frozen-lockfile` was declared only in
        // kebab form, so authors writing `preferFrozenLockfile` in
        // `.npmrc` (the pnpm-workspace.yaml spelling) were silently
        // ignored. Auto-synth fills in the camelCase alias too.
        let npmrc = entries(&[("preferFrozenLockfile", "false")]);
        let ws: std::collections::BTreeMap<String, yaml_serde::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::prefer_frozen_lockfile(&ctx), Some(false));
    }

    #[test]
    fn global_config_yaml_reads_string_setting() {
        // pnpm v11's global `config.yaml` is consulted through the same
        // `*_from_workspace_yaml` helpers (camelCase keys). A `storeDir`
        // set only in config.yaml must resolve when no higher source
        // speaks.
        let ws = BTreeMap::new();
        let cfg = raw_yaml("storeDir: /tmp/from-global-config\n");
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &[],
            user_aube_config: &[],
            user_npmrc: &[],
            workspace_yaml: &ws,
            global_config_yaml: &cfg,
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert_eq!(
            resolved::store_dir(&ctx),
            Some("/tmp/from-global-config".to_string())
        );
    }

    #[test]
    fn project_workspace_yaml_outranks_global_config_yaml() {
        // Precedence: the project-root `pnpm-workspace.yaml` overrides the
        // user's global `config.yaml` (matching pnpm v11, where the
        // project workspace manifest is applied after the global config).
        let ws = raw_yaml("storeDir: /tmp/from-project-ws\n");
        let cfg = raw_yaml("storeDir: /tmp/from-global-config\n");
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &[],
            user_aube_config: &[],
            user_npmrc: &[],
            workspace_yaml: &ws,
            global_config_yaml: &cfg,
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert_eq!(
            resolved::store_dir(&ctx),
            Some("/tmp/from-project-ws".to_string()),
            "project pnpm-workspace.yaml must win over global config.yaml"
        );
    }

    #[test]
    fn global_config_yaml_outranks_user_npmrc() {
        // The global config.yaml ranks above user-scope sources: a value
        // in config.yaml beats the same key left in `~/.npmrc`.
        let user_npmrc = entries(&[("store-dir", "/tmp/from-user-npmrc")]);
        let cfg = raw_yaml("storeDir: /tmp/from-global-config\n");
        let ws = BTreeMap::new();
        let ctx = ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &[],
            user_aube_config: &[],
            user_npmrc: &user_npmrc,
            workspace_yaml: &ws,
            global_config_yaml: &cfg,
            env: &[],
            cli: &[],
            embedder_defaults: &[],
        };
        assert_eq!(
            resolved::store_dir(&ctx),
            Some("/tmp/from-global-config".to_string()),
            "global config.yaml must win over user .npmrc"
        );
    }

    #[test]
    fn generated_string_accessor_reads_workspace_yaml() {
        // `storeDir` is a string setting with a workspaceYaml source.
        // Before the generator learned about `string_from_workspace_yaml`,
        // this returned `None` — the test guards the fix.
        let npmrc: Vec<(String, String)> = Vec::new();
        let ws = raw_yaml("storeDir: /tmp/from-ws\n");
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::store_dir(&ctx), Some("/tmp/from-ws".to_string()));
    }
}
