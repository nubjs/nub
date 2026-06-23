//! `aube config` — read/write settings in aube config and `.npmrc`.
//!
//! The command's known setting surface is derived from
//! [`aube_settings::meta::SETTINGS`], generated at build time from
//! `settings.toml`. Known aube-owned user/global settings are written
//! to `~/.config/aube/config.toml`; unknown and registry/auth keys are
//! still accepted verbatim because `.npmrc` is free-form and includes
//! auth-token entries such as `//registry.npmjs.org/:_authToken`.

mod aube_config;
mod delete;
mod explain;
mod find;
#[path = "get.rs"]
mod get_cmd;
mod list;
#[path = "set.rs"]
mod set_cmd;
#[cfg(feature = "config-tui")]
mod tui;

use crate::commands::npmrc::{NpmrcEdit, user_npmrc_path};
use aube_settings::meta as settings_meta;
use clap::{Args, Subcommand, ValueEnum};
use miette::miette;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(flatten)]
    pub list: list::ListArgs,

    #[command(subcommand)]
    pub command: Option<ConfigCommand>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Delete a key from aube config or the selected `.npmrc` file
    #[command(visible_aliases = ["rm", "remove", "unset"])]
    Delete(delete::DeleteArgs),
    /// Explain a known setting, including defaults and supported config sources
    Explain(explain::ExplainArgs),
    /// Search known settings by name, source key, or description
    #[command(visible_alias = "search")]
    Find(find::FindArgs),
    /// Print the effective value of a key
    Get(GetArgs),
    /// Print every key/value from aube config and selected `.npmrc` file(s)
    #[command(visible_alias = "ls")]
    List(list::ListArgs),
    /// Write a key=value pair to aube config or the selected `.npmrc` file
    Set(SetArgs),
    /// Browse known settings in an interactive terminal UI
    Tui,
}

#[derive(Debug, Args)]
pub struct KeyArgs {
    /// The setting key.
    ///
    /// Accepts either a pnpm canonical name (e.g. `autoInstallPeers`)
    /// or an `.npmrc` alias (e.g. `auto-install-peers`).
    pub key: String,

    /// Shortcut for `--location project`.
    #[arg(long, conflicts_with = "location")]
    pub local: bool,

    /// Which config location to act on.
    ///
    /// Defaults to `user`. Delete sweeps both aube's own config
    /// (`~/.config/aube/config.toml` at user-scope,
    /// `<cwd>/.config/aube/config.toml` at project-scope) and the
    /// matching `.npmrc`, so the call works regardless of which file
    /// the value was originally written to.
    #[arg(long, value_enum, default_value_t = Location::User)]
    pub location: Location,
}

impl KeyArgs {
    pub(super) fn effective_location(&self) -> Location {
        if self.local {
            Location::Project
        } else {
            self.location
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Location {
    /// User config (`~/.config/aube/config.toml` for known aube
    /// settings, `~/.npmrc` for registry/auth and unknown keys)
    User,
    /// `<cwd>/.npmrc`
    Project,
    /// Alias for `user` — aube has no separate global config file.
    Global,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ListLocation {
    /// Merge every runtime settings source, last-write-wins (same
    /// precedence install uses).
    Merged,
    /// User/global config sources.
    User,
    /// Project config sources.
    Project,
    /// Alias for `user`.
    Global,
}

pub(crate) use aube_config::{
    load_project_entries as load_project_aube_config_entries,
    load_user_entries as load_user_aube_config_entries,
};
pub(crate) use get_cmd::GetArgs;
pub use set_cmd::set_project_scalar_to_workspace_yaml;
pub(crate) use set_cmd::SetArgs;

impl Location {
    pub(super) fn path(self) -> miette::Result<PathBuf> {
        match self {
            Location::User | Location::Global => user_npmrc_path(),
            Location::Project => Ok(crate::dirs::project_root_or_cwd()?.join(".npmrc")),
        }
    }
}

pub async fn run(args: ConfigArgs) -> miette::Result<()> {
    match args.command {
        Some(ConfigCommand::Get(a)) => {
            reject_parent_list_args(&args.list, "get")?;
            get(a)
        }
        Some(ConfigCommand::Set(a)) => {
            reject_parent_list_args(&args.list, "set")?;
            set(a)
        }
        Some(ConfigCommand::Delete(a)) => {
            reject_parent_list_args(&args.list, "delete")?;
            delete::run(a)
        }
        Some(ConfigCommand::Explain(a)) => {
            reject_parent_list_args(&args.list, "explain")?;
            explain::run(a)
        }
        Some(ConfigCommand::Find(a)) => {
            reject_parent_list_args(&args.list, "find")?;
            find::run(a)
        }
        Some(ConfigCommand::List(mut a)) => {
            a.apply_parent(args.list);
            list::run(a)
        }
        Some(ConfigCommand::Tui) => {
            reject_parent_list_args(&args.list, "tui")?;
            tui::run()
        }
        None => list::run(args.list),
    }
}

fn reject_parent_list_args(args: &list::ListArgs, subcommand: &str) -> miette::Result<()> {
    if args.has_parent_overrides() {
        Err(miette!(
            "`{}` list flags must be used with `{}` or `{}`, not `{} {subcommand}`",
            aube_util::cmd("config"),
            aube_util::cmd("config"),
            aube_util::cmd("config list"),
            aube_util::cmd("config")
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(feature = "config-tui"))]
mod tui {
    use miette::miette;

    pub fn run() -> miette::Result<()> {
        Err(miette!(
            "`{}` was not enabled in this build; rebuild with the `config-tui` feature",
            aube_util::cmd("config tui")
        ))
    }
}

pub(crate) fn get(args: GetArgs) -> miette::Result<()> {
    get_cmd::run(args)
}

pub(crate) fn set(args: SetArgs) -> miette::Result<()> {
    set_cmd::run(args)
}

/// True for entries in `SettingMeta::npmrc_keys` that are real, literal
/// `.npmrc` keys — not pattern templates like `@scope:registry` or
/// `//host/:_authToken`.
fn is_literal_alias(key: &str) -> bool {
    !key.starts_with("//") && !key.contains(':')
}

/// Expand a user-supplied key into the full set of `.npmrc` aliases it
/// covers. Pattern-template entries in `npmrc_keys` (e.g.
/// `@scope:registry`) are filtered out — see [`is_literal_alias`].
pub(super) fn resolve_aliases(key: &str) -> Vec<String> {
    if let Some(meta) = settings_meta::find(key) {
        let literals = literal_aliases(meta.npmrc_keys);
        if !literals.is_empty() {
            return literals;
        }
    }
    for meta in settings_meta::all() {
        let literals = literal_aliases(meta.npmrc_keys);
        if literals.iter().any(|a| a == key) {
            return literals;
        }
    }
    vec![key.to_string()]
}

pub(super) fn literal_aliases(keys: &[&'static str]) -> Vec<String> {
    keys.iter()
        .filter(|k| is_literal_alias(k))
        .map(|s| s.to_string())
        .collect()
}

/// True when `key` belongs to the npm-shared `.npmrc` surface: npm,
/// pnpm, and yarn read it from `.npmrc` so `aube config set` keeps
/// the value there for cross-tool visibility. The two pattern checks
/// cover per-host auth/cert templates (`//host/:_authToken`, etc.)
/// and scoped registries (`@scope:registry`); everything else is
/// driven by the `npmShared` flag on each entry in `settings.toml`,
/// so the answer for any specific key lives next to that setting's
/// other metadata rather than in a hardcoded list here.
pub(super) fn is_npm_shared_key(key: &str) -> bool {
    if key.starts_with("//") {
        return true;
    }
    if let Some(rest) = key.strip_prefix('@')
        && rest.ends_with(":registry")
    {
        return true;
    }
    setting_for_key(key).is_some_and(|meta| meta.npm_shared)
}

/// The auth-bearing config names npm refuses to print, mirroring the
/// `protected` array in npm's `lib/commands/config.js`. A `config get`
/// of any protected key errors instead of leaking the value, and
/// `config list` renders it as `(protected)`.
const PROTECTED_NAMES: &[&str] = &[
    "auth",
    "authToken",
    "certfile",
    "email",
    "keyfile",
    "password",
    "username",
];

/// True when `key` names a secret npm declines to reveal. Ported from
/// npm's `isProtected` (npm `lib/commands/config.js`):
///   * any `_`-prefixed key (`_auth`, `_authToken`, `_password`, …),
///   * a bare protected name (`username`, `email`, `certfile`, …),
///   * a nerf-darted per-host form (`//host/:_authToken`,
///     `//host/:username`, `//host/:_auth`, …) — matched when the
///     `//`-prefixed key contains `:_` or ends with `:<name>` /
///     `:_<name>` for a protected name.
///
/// This is the security floor that keeps `config get`/`config list`
/// from echoing registry tokens, in parity with `npm config get`.
pub(super) fn is_protected_key(key: &str) -> bool {
    if let Some(stripped) = key.strip_prefix("//") {
        if stripped.contains(":_") {
            return true;
        }
        return PROTECTED_NAMES.iter().any(|name| {
            stripped.ends_with(&format!(":{name}")) || stripped.ends_with(&format!(":_{name}"))
        });
    }
    if key.starts_with('_') {
        return true;
    }
    PROTECTED_NAMES.contains(&key)
}

pub(super) fn setting_for_key(key: &str) -> Option<&'static settings_meta::SettingMeta> {
    settings_meta::find(key).or_else(|| {
        settings_meta::all().iter().find(|meta| {
            meta.npmrc_keys.iter().any(|candidate| candidate == &key)
                || meta
                    .workspace_yaml_keys
                    .iter()
                    .any(|candidate| candidate == &key)
                || meta.env_vars.iter().any(|candidate| candidate == &key)
                || meta.cli_flags.iter().any(|candidate| candidate == &key)
        })
    })
}

pub(super) fn setting_search_score(meta: &settings_meta::SettingMeta, terms: &[String]) -> usize {
    let names = setting_search_text(&[
        &[meta.name],
        meta.cli_flags,
        meta.env_vars,
        meta.npmrc_keys,
        meta.workspace_yaml_keys,
    ]);
    let summary = setting_search_text(&[&[meta.description]]);
    let body = setting_search_text(&[&[meta.docs], meta.examples]);

    terms
        .iter()
        .map(|term| {
            usize::from(search_text_matches(&names, term)) * 4
                + usize::from(search_text_matches(&summary, term)) * 2
                + usize::from(search_text_matches(&body, term))
        })
        .sum()
}

fn setting_search_text(groups: &[&[&str]]) -> String {
    let mut out = String::new();
    for value in groups.iter().copied().flatten().copied() {
        out.push(' ');
        out.push_str(value);
    }
    out.to_ascii_lowercase()
}

fn search_text_matches(haystack: &str, term: &str) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word.starts_with(term))
}

/// Walk every config source in low-to-high precedence order so a later
/// duplicate wins. Mirrors the default file-source chain generated for
/// install/runtime settings in [`aube_settings::resolved`]:
/// `embedderDefaults < userNpmrc < userAubeConfig < projectNpmrc <
/// projectAubeConfig < globalConfigYaml < workspaceYaml`.
pub(super) fn read_merged(cwd: &Path) -> miette::Result<Vec<(String, String)>> {
    let files = crate::commands::FileSources::load(cwd);
    let workspace_yaml = read_workspace_yaml_raw(cwd);
    let mut out = Vec::new();
    out.extend(aube_settings::embedder_defaults().iter().cloned());
    out.extend(files.user_npmrc);
    out.extend(files.user_aube_config);
    out.extend(files.project_npmrc);
    out.extend(files.project_aube_config);
    out.extend(read_yaml_flat(&files.global_config_yaml));
    out.extend(read_yaml_flat(&workspace_yaml));
    Ok(out)
}

pub(super) fn read_user_entries(cwd: &Path) -> miette::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    out.extend(aube_registry::config::load_user_npmrc_entries(cwd));
    out.extend(aube_config::load_user_entries());
    out.extend(read_yaml_flat(&crate::commands::load_global_config_yaml()));
    Ok(out)
}

pub(super) fn read_project_entries(cwd: &Path) -> miette::Result<Vec<(String, String)>> {
    let workspace_yaml = read_workspace_yaml_raw(cwd);
    let mut out = Vec::new();
    out.extend(aube_registry::config::load_project_npmrc_entries(cwd));
    out.extend(aube_config::load_project_entries(cwd));
    out.extend(read_yaml_flat(&workspace_yaml));
    Ok(out)
}

fn read_workspace_yaml_raw(cwd: &Path) -> BTreeMap<String, yaml_serde::Value> {
    let Ok(map) = aube_manifest::workspace::load_raw(cwd) else {
        return BTreeMap::new();
    };
    map
}

fn read_yaml_flat(
    map: &std::collections::BTreeMap<String, yaml_serde::Value>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for meta in settings_meta::all() {
        for key in meta.workspace_yaml_keys {
            let Some(raw) = yaml_setting_string(meta, map, key) else {
                continue;
            };
            if !out.iter().any(|(existing, _)| existing == key) {
                out.push((key.to_string(), raw));
            }
        }
    }
    let scalar_entries: Vec<_> = map
        .iter()
        .filter_map(|(k, v)| yaml_scalar_string(v).map(|raw| (k.clone(), raw)))
        .collect();
    for (key, raw) in scalar_entries {
        if !out.iter().any(|(existing, _)| existing == &key) {
            out.push((key, raw));
        }
    }
    out
}

fn yaml_setting_string(
    meta: &settings_meta::SettingMeta,
    map: &std::collections::BTreeMap<String, yaml_serde::Value>,
    key: &str,
) -> Option<String> {
    let value = aube_settings::workspace_yaml_value(map, key)?;
    match meta.type_ {
        "bool" => match value {
            yaml_serde::Value::Bool(b) => Some(b.to_string()),
            yaml_serde::Value::String(s) => aube_settings::parse_bool(s).map(|b| b.to_string()),
            _ => None,
        },
        "int" => match value {
            yaml_serde::Value::Number(n) => n.as_u64().map(|u| u.to_string()),
            yaml_serde::Value::String(s) => s.trim().parse::<u64>().ok().map(|u| u.to_string()),
            _ => None,
        },
        "list<string>" => match value {
            yaml_serde::Value::Sequence(items) => {
                let strings: Vec<String> = items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect();
                if strings.is_empty() {
                    Some("[]".to_string())
                } else {
                    Some(strings.join(","))
                }
            }
            yaml_serde::Value::String(s) => Some(s.clone()),
            _ => None,
        },
        "object" => {
            let json = serde_json::to_value(value).ok()?;
            json.as_object()?;
            serde_json::to_string(&json).ok()
        }
        ty if is_stringish_type(ty) => yaml_scalar_string(value),
        _ => None,
    }
}

fn is_stringish_type(ty: &str) -> bool {
    matches!(ty, "string" | "path" | "url") || ty.starts_with('"')
}

fn yaml_scalar_string(value: &yaml_serde::Value) -> Option<String> {
    match value {
        yaml_serde::Value::String(s) => Some(s.clone()),
        yaml_serde::Value::Number(n) => Some(n.to_string()),
        yaml_serde::Value::Bool(b) => Some(b.to_string()),
        yaml_serde::Value::Sequence(items) => {
            let parts: Vec<String> = items.iter().filter_map(yaml_scalar_string).collect();
            (!parts.is_empty()).then(|| parts.join(","))
        }
        _ => None,
    }
}

#[cfg(feature = "config-tui")]
pub(super) fn read_single(path: &std::path::Path) -> miette::Result<Vec<(String, String)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let edit = NpmrcEdit::load(path)?;
    Ok(edit.entries())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn config_test_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        match LOCK.get_or_init(|| Mutex::new(())).lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let old = std::env::var_os(key);
            // SAFETY: config tests that mutate process env hold
            // `config_test_lock`, and they restore the prior value on drop.
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var_os(key);
            // SAFETY: config tests that mutate process env hold
            // `config_test_lock`, and they restore the prior value on drop.
            unsafe { std::env::remove_var(key) };
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: see `EnvGuard::set`.
            unsafe {
                match &self.old {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    struct PnpmReadGateGuard {
        old: bool,
    }

    impl PnpmReadGateGuard {
        fn set(enabled: bool) -> Self {
            let old = aube_util::engine_context().read_branded_pnpm_config;
            aube_util::update_engine_context(|ctx| ctx.read_branded_pnpm_config = enabled);
            Self { old }
        }
    }

    impl Drop for PnpmReadGateGuard {
        fn drop(&mut self) {
            let old = self.old;
            aube_util::update_engine_context(|ctx| ctx.read_branded_pnpm_config = old);
        }
    }

    /// Toggles the GLOBAL pnpm-named-files gate (`read_pnpm_global_config`),
    /// which is independent of the project gate above.
    struct PnpmGlobalGateGuard {
        old: bool,
    }

    impl PnpmGlobalGateGuard {
        fn set(enabled: bool) -> Self {
            let old = aube_util::engine_context().read_pnpm_global_config;
            aube_util::update_engine_context(|ctx| ctx.read_pnpm_global_config = enabled);
            Self { old }
        }
    }

    impl Drop for PnpmGlobalGateGuard {
        fn drop(&mut self) {
            let old = self.old;
            aube_util::update_engine_context(|ctx| ctx.read_pnpm_global_config = old);
        }
    }

    #[cfg(unix)]
    fn mkfifo(path: &Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo failed for {}", path.display());
    }

    #[cfg(unix)]
    fn assert_returns_quickly<F>(label: &'static str, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            let _ = tx.send(result);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Ok(())) => {}
            Ok(Err(payload)) => std::panic::resume_unwind(payload),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("{label} blocked while reading an unrelated scoped config source")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("{label} worker disconnected before reporting")
            }
        }
    }

    #[test]
    fn protected_key_matches_npm_auth_surface() {
        // Mirrors npm's `isProtected` (lib/commands/config.js): bare
        // auth-bearing names, any `_`-prefixed key, and the nerf-darted
        // per-host forms `//host/:_authToken`, `//host/:username`, etc.
        for key in [
            "_auth",
            "_authToken",
            "_password",
            "username",
            "email",
            "authToken",
            "certfile",
            "keyfile",
            "//registry.npmjs.org/:_authToken",
            "//registry.npmjs.org/:_password",
            "//registry.npmjs.org/:_auth",
            "//registry.npmjs.org/:username",
            "//registry.npmjs.org/:email",
            "//registry.npmjs.org/:authToken",
        ] {
            assert!(is_protected_key(key), "{key} should be protected");
        }
    }

    #[test]
    fn protected_key_leaves_ordinary_settings_readable() {
        for key in [
            "registry",
            "save-exact",
            "auto-install-peers",
            "//registry.npmjs.org/:always-auth",
            "@scope:registry",
        ] {
            assert!(!is_protected_key(key), "{key} should not be protected");
        }
    }

    #[test]
    fn canonical_list_key_collapses_alias_to_primary() {
        assert_eq!(
            list::canonical_list_key("autoInstallPeers"),
            "auto-install-peers"
        );
        assert_eq!(
            list::canonical_list_key("auto-install-peers"),
            "auto-install-peers"
        );
    }

    #[test]
    fn canonical_list_key_passthrough_for_unknown_key() {
        assert_eq!(
            list::canonical_list_key("//registry.example.com/:_authToken"),
            "//registry.example.com/:_authToken"
        );
    }

    #[test]
    fn resolve_aliases_canonical_name() {
        let aliases = resolve_aliases("autoInstallPeers");
        assert!(aliases.iter().any(|a| a == "auto-install-peers"));
        assert!(aliases.iter().any(|a| a == "autoInstallPeers"));
    }

    #[test]
    fn resolve_aliases_from_alias() {
        let aliases = resolve_aliases("auto-install-peers");
        assert!(aliases.iter().any(|a| a == "auto-install-peers"));
        assert!(aliases.iter().any(|a| a == "autoInstallPeers"));
    }

    #[test]
    fn resolve_aliases_registry_excludes_template_keys() {
        let aliases = resolve_aliases("registry");
        assert_eq!(aliases, vec!["registry".to_string()]);
        for a in &aliases {
            assert!(is_literal_alias(a), "leaked template alias: {a}");
        }
    }

    #[test]
    fn resolve_aliases_template_input_is_identity() {
        for template in [
            "@scope:registry",
            "//registry.example.com/:_authToken",
            "//registry.example.com/:_auth",
        ] {
            assert_eq!(
                resolve_aliases(template),
                vec![template.to_string()],
                "{template} should be identity, not registries-grouped"
            );
        }
    }

    #[test]
    fn is_literal_alias_recognizes_templates() {
        assert!(is_literal_alias("registry"));
        assert!(is_literal_alias("auto-install-peers"));
        assert!(!is_literal_alias("@scope:registry"));
        assert!(!is_literal_alias("//host/:_authToken"));
        assert!(!is_literal_alias("//host/:_auth"));
    }

    #[test]
    fn resolve_aliases_unknown_key_is_identity() {
        let aliases = resolve_aliases("//registry.example.com/:_authToken");
        assert_eq!(
            aliases,
            vec!["//registry.example.com/:_authToken".to_string()]
        );
    }

    #[test]
    fn preferred_write_key_keeps_user_typed_alias() {
        let aliases = vec![
            "auto-install-peers".to_string(),
            "autoInstallPeers".to_string(),
        ];
        assert_eq!(
            set_cmd::preferred_write_key("autoInstallPeers", &aliases),
            "autoInstallPeers"
        );
        assert_eq!(
            set_cmd::preferred_write_key("auto-install-peers", &aliases),
            "auto-install-peers"
        );
    }

    #[test]
    fn preferred_write_key_falls_back_to_first_alias() {
        let aliases = vec![
            "auto-install-peers".to_string(),
            "autoInstallPeers".to_string(),
        ];
        assert_eq!(
            set_cmd::preferred_write_key("something-else", &aliases),
            "auto-install-peers"
        );
    }

    #[test]
    fn config_get_and_list_prefer_workspace_yaml_over_project_npmrc() {
        let _lock = config_test_lock();
        let _gate = PnpmReadGateGuard::set(true);
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join(".npmrc"), "auto-install-peers=true\n").unwrap();
        fs::write(
            project.join("pnpm-workspace.yaml"),
            "autoInstallPeers: false\n",
        )
        .unwrap();

        let entries = read_merged(project).unwrap();
        let aliases = resolve_aliases("autoInstallPeers");

        assert_eq!(
            get_cmd::find_value(&entries, &aliases).as_deref(),
            Some("false"),
            "config get must report the same workspace-yaml value runtime settings resolve"
        );

        let seen = list::collect_seen(entries);
        assert_eq!(
            seen.get("auto-install-peers").map(String::as_str),
            Some("false"),
            "config list must dedupe to the workspace-yaml value"
        );
    }

    #[test]
    fn config_get_and_list_prefer_global_config_yaml_over_project_npmrc() {
        let _lock = config_test_lock();
        let _gate = PnpmReadGateGuard::set(true);
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        let xdg = dir.path().join("xdg");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(xdg.join("pnpm")).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(project.join(".npmrc"), "auto-install-peers=true\n").unwrap();
        fs::write(
            xdg.join("pnpm").join("config.yaml"),
            "autoInstallPeers: false\n",
        )
        .unwrap();

        let _home = EnvGuard::set("HOME", &home);
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", &xdg);

        let entries = read_merged(&project).unwrap();
        let aliases = resolve_aliases("auto-install-peers");

        assert_eq!(
            get_cmd::find_value(&entries, &aliases).as_deref(),
            Some("false"),
            "config get must report the global config.yaml value runtime settings resolve"
        );

        let seen = list::collect_seen(entries);
        assert_eq!(
            seen.get("auto-install-peers").map(String::as_str),
            Some("false"),
            "config list must dedupe to the global config.yaml value"
        );
    }

    #[test]
    fn config_get_keeps_the_project_pnpm_yaml_gated_off() {
        // PROJECT pnpm-workspace.yaml is gated by the project-scope
        // `read_branded_pnpm_config`. With it off (non-pnpm incumbent), the
        // project pnpm-workspace.yaml stays inert; the project `.npmrc` wins.
        // The GLOBAL config.yaml is gated separately — turn it off here too so
        // this test isolates the PROJECT gate.
        let _lock = config_test_lock();
        let _gate = PnpmReadGateGuard::set(false);
        let _global_gate = PnpmGlobalGateGuard::set(false);
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        let xdg = dir.path().join("xdg");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(xdg.join("pnpm")).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(project.join(".npmrc"), "auto-install-peers=true\n").unwrap();
        fs::write(
            project.join("pnpm-workspace.yaml"),
            "autoInstallPeers: false\n",
        )
        .unwrap();

        let _home = EnvGuard::set("HOME", &home);
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", &xdg);

        let entries = read_merged(&project).unwrap();
        let aliases = resolve_aliases("autoInstallPeers");

        assert_eq!(
            get_cmd::find_value(&entries, &aliases).as_deref(),
            Some("true"),
            "project pnpm-workspace.yaml must stay inert when the project incumbent gate is off"
        );

        let seen = list::collect_seen(entries);
        assert_eq!(
            seen.get("auto-install-peers").map(String::as_str),
            Some("true"),
            "config list must preserve the same project pnpm-source gate"
        );
    }

    #[test]
    fn config_get_reads_global_config_yaml_independent_of_the_project_gate() {
        // GLOBAL config.yaml is gated by `read_pnpm_global_config`, NOT the
        // project-scope `read_branded_pnpm_config`. So even with the PROJECT
        // gate OFF (non-pnpm incumbent), the user's GLOBAL pnpm config.yaml is
        // still read when the global gate is on (the asymmetric-read model:
        // honor whatever global config the user has, ungated by cwd). With the
        // global gate OFF, it goes inert.
        let _lock = config_test_lock();
        let _gate = PnpmReadGateGuard::set(false);
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        let xdg = dir.path().join("xdg");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(xdg.join("pnpm")).unwrap();
        fs::create_dir_all(&home).unwrap();
        // No project sources — only the user's GLOBAL config.yaml sets it.
        fs::write(
            xdg.join("pnpm").join("config.yaml"),
            "networkConcurrency: 3\n",
        )
        .unwrap();

        let _home = EnvGuard::set("HOME", &home);
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", &xdg);
        let aliases = resolve_aliases("networkConcurrency");

        {
            let _global_gate = PnpmGlobalGateGuard::set(true);
            let entries = read_merged(&project).unwrap();
            assert_eq!(
                get_cmd::find_value(&entries, &aliases).as_deref(),
                Some("3"),
                "global config.yaml must be read when the global gate is on, even with the project gate off"
            );
        }
        {
            let _global_gate = PnpmGlobalGateGuard::set(false);
            let entries = read_merged(&project).unwrap();
            assert_eq!(
                get_cmd::find_value(&entries, &aliases).as_deref(),
                None,
                "global config.yaml must go inert when the global gate is off"
            );
        }
    }

    #[test]
    fn config_get_and_list_honor_npm_config_userconfig() {
        let _lock = config_test_lock();
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        let custom_userconfig = dir.path().join("custom-user.npmrc");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(&custom_userconfig, "auto-install-peers=false\n").unwrap();

        let _home = EnvGuard::set("HOME", &home);
        let _userconfig = EnvGuard::set("NPM_CONFIG_USERCONFIG", &custom_userconfig);

        let entries = read_merged(&project).unwrap();
        let aliases = resolve_aliases("auto-install-peers");

        assert_eq!(
            get_cmd::find_value(&entries, &aliases).as_deref(),
            Some("false"),
            "config get must use the same userconfig relocation runtime settings use"
        );

        let seen = list::collect_seen(entries);
        assert_eq!(
            seen.get("auto-install-peers").map(String::as_str),
            Some("false"),
            "config list must include values from NPM_CONFIG_USERCONFIG"
        );
    }

    #[test]
    fn config_get_and_list_include_nested_workspace_yaml_lists() {
        let _lock = config_test_lock();
        let _gate = PnpmReadGateGuard::set(true);
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(
            project.join("pnpm-workspace.yaml"),
            "updateConfig:\n  ignoreDependencies:\n    - left-pad\n",
        )
        .unwrap();

        let entries = read_merged(project).unwrap();
        let aliases = resolve_aliases("updateConfig.ignoreDependencies");

        assert_eq!(
            get_cmd::find_value(&entries, &aliases).as_deref(),
            Some("left-pad"),
            "config get must flatten metadata-declared dotted workspace YAML paths"
        );

        let seen = list::collect_seen(entries);
        assert_eq!(
            seen.get("updateConfig.ignoreDependencies")
                .map(String::as_str),
            Some("left-pad"),
            "config list must include metadata-declared dotted workspace YAML paths"
        );
    }

    #[test]
    fn config_get_and_list_include_embedder_defaults_at_lowest_precedence() {
        let _lock = config_test_lock();
        aube_settings::set_embedder_defaults(vec![(
            "virtualStoreDir".to_string(),
            "node_modules/.nub".to_string(),
        )]);
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();

        let entries = read_merged(project).unwrap();
        let aliases = resolve_aliases("virtualStoreDir");

        assert_eq!(
            get_cmd::find_value(&entries, &aliases).as_deref(),
            Some("node_modules/.nub"),
            "config get must report embedder defaults when no higher source overrides them"
        );

        let seen = list::collect_seen(entries);
        assert_eq!(
            seen.get("virtualStoreDir")
                .or_else(|| seen.get("virtual-store-dir"))
                .map(String::as_str),
            Some("node_modules/.nub"),
            "config list must include embedder defaults"
        );
    }

    #[test]
    fn config_get_and_list_render_empty_yaml_string_lists_as_present() {
        let _lock = config_test_lock();
        let _gate = PnpmReadGateGuard::set(true);
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(
            project.join("pnpm-workspace.yaml"),
            "updateConfig:\n  ignoreDependencies: []\n",
        )
        .unwrap();

        let entries = read_merged(project).unwrap();
        let aliases = resolve_aliases("updateConfig.ignoreDependencies");

        assert_eq!(
            get_cmd::find_value(&entries, &aliases).as_deref(),
            Some("[]"),
            "empty YAML lists are present runtime values, not absent config"
        );

        let seen = list::collect_seen(entries);
        assert_eq!(
            seen.get("updateConfig.ignoreDependencies")
                .map(String::as_str),
            Some("[]"),
            "config list must preserve empty YAML list presence"
        );
    }

    #[test]
    fn config_get_and_list_render_object_workspace_yaml_as_json() {
        let _lock = config_test_lock();
        let _gate = PnpmReadGateGuard::set(true);
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(
            project.join("pnpm-workspace.yaml"),
            "allowBuilds:\n  esbuild: true\n",
        )
        .unwrap();

        let entries = read_merged(project).unwrap();
        let aliases = resolve_aliases("allowBuilds");

        assert_eq!(
            get_cmd::find_value(&entries, &aliases).as_deref(),
            Some("{\"esbuild\":true}"),
            "object-shaped workspace YAML settings should be visible as JSON"
        );

        let seen = list::collect_seen(entries);
        assert_eq!(
            seen.get("allowBuilds").map(String::as_str),
            Some("{\"esbuild\":true}"),
            "config list must include object-shaped workspace YAML settings"
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_get_and_list_user_location_does_not_touch_project_auth_file() {
        let _lock = config_test_lock();
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        let userconfig = dir.path().join("user.npmrc");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(&userconfig, "registry=https://user.example/\n").unwrap();
        let project_auth = project.join("project-auth.fifo");
        mkfifo(&project_auth);
        fs::write(
            project.join(".npmrc"),
            format!("npmrc-auth-file={}\n", project_auth.display()),
        )
        .unwrap();

        let _home = EnvGuard::set("HOME", &home);
        let _userconfig = EnvGuard::set("NPM_CONFIG_USERCONFIG", &userconfig);
        let _lower_userconfig = EnvGuard::remove("npm_config_userconfig");
        let _pnpm_userconfig = EnvGuard::remove("PNPM_CONFIG_USERCONFIG");
        let _lower_pnpm_userconfig = EnvGuard::remove("pnpm_config_userconfig");

        assert_returns_quickly("read_user_entries", move || {
            let entries = read_user_entries(&project).unwrap();
            let aliases = resolve_aliases("registry");
            assert_eq!(
                get_cmd::find_value(&entries, &aliases).as_deref(),
                Some("https://user.example/"),
                "user-scoped config must still include the selected user source"
            );
            let seen = list::collect_seen(entries);
            assert_eq!(
                seen.get("registry").map(String::as_str),
                Some("https://user.example/"),
                "user-scoped config list must report the selected user source"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn config_get_and_list_project_location_does_not_touch_user_auth_sources() {
        let _lock = config_test_lock();
        let _gate = PnpmReadGateGuard::set(true);
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        let xdg = dir.path().join("xdg");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(xdg.join("pnpm")).unwrap();
        fs::write(
            project.join(".npmrc"),
            "registry=https://project.example/\n",
        )
        .unwrap();

        let user_auth = home.join("user-auth.fifo");
        let auth_ini = xdg.join("pnpm").join("auth.ini");
        mkfifo(&user_auth);
        mkfifo(&auth_ini);
        fs::write(
            home.join(".npmrc"),
            format!("npmrc-auth-file={}\n", user_auth.display()),
        )
        .unwrap();

        let _home = EnvGuard::set("HOME", &home);
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", &xdg);
        let _userconfig = EnvGuard::remove("NPM_CONFIG_USERCONFIG");
        let _lower_userconfig = EnvGuard::remove("npm_config_userconfig");
        let _pnpm_userconfig = EnvGuard::remove("PNPM_CONFIG_USERCONFIG");
        let _lower_pnpm_userconfig = EnvGuard::remove("pnpm_config_userconfig");

        assert_returns_quickly("read_project_entries", move || {
            let entries = read_project_entries(&project).unwrap();
            let aliases = resolve_aliases("registry");
            assert_eq!(
                get_cmd::find_value(&entries, &aliases).as_deref(),
                Some("https://project.example/"),
                "project-scoped config must still include the project source"
            );
            let seen = list::collect_seen(entries);
            assert_eq!(
                seen.get("registry").map(String::as_str),
                Some("https://project.example/"),
                "project-scoped config list must report the project source"
            );
        });
    }
}
