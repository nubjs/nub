//! The read side of `nub config`: `get`, `list`, `delete`, and the sources
//! they answer from.
//!
//! One key space spans two homes — `.npmrc`, which every Node package manager
//! shares, and `nub.jsonc`, which is nub's own — so a reader that consulted
//! only one of them would report a value the install does not use. The
//! `nub.jsonc` tier is lowered by the install's OWN code
//! ([`super::host_settings::supplied_settings`]), so a curated key cannot mean
//! one thing to the install and another to `config get`.
//!
//! The merged view reports what an install would ACT ON, so it opens with the
//! defaults nub itself applies ([`super::host_settings::defaults`]) and lets each
//! file override them: `minimumReleaseAge` reads `1440` in a project that has
//! never configured it, because that is the quarantine the next install
//! applies. A key nothing defaults and nobody set reports `undefined`, which is
//! what `pnpm config get` and `npm config get` both print.
//!
//! In a pnpm project `config` is pnpm's own command
//! ([`super::verb_routing::engine_takes`]), so these verbs answer a nub
//! project, where no pnpm-named file is read. The branded files join the chain
//! under a pnpm incumbent ([`branded_yaml`]) for one caller, [`registry_at`],
//! which `nub run` asks in either kind of project. The previous engine's
//! `config.toml` went with that engine and has no successor.
//!
//! The scope flags select which sources answer: `--global` the user's `.npmrc`,
//! `--local` the project's files, neither the merged
//! view. Neither scope reports defaults or `npm_config_*` — each names a FILE,
//! so answering one from elsewhere would answer a question nobody asked.

use super::host_settings::{self, Raw};
use super::present;
use anyhow::{Result, anyhow, bail};
use nub_settings::meta as settings_meta;
use nub_settings::meta::SettingMeta;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// A write destination. `config set`/`delete` name one file; `get`/`list` can
/// also ask for the merged view, which is [`ListLocation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Location {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListLocation {
    Merged,
    User,
    Project,
}

// ─────────────────────────────── arg surface ───────────────────────────────
//
// nub's `config` grammar is its own documented surface — project scope by
// default, `--global` for user scope, and no `--location`. These carry it.

#[derive(Debug, usage_rs::Args)]
pub(crate) struct GetArgs {
    /// The setting key.
    ///
    /// Accepts either a canonical name (e.g. `autoInstallPeers`)
    /// or an `.npmrc` alias (e.g. `auto-install-peers`).
    pub key: String,

    /// Read only the user configuration.
    #[usage(short = 'g', long, conflicts = "--local")]
    pub global: bool,

    /// Emit the value as JSON.
    ///
    /// A missing key renders as `undefined`, a found value is JSON-encoded.
    #[usage(long)]
    pub json: bool,

    /// Read only the project configuration.
    #[usage(long, conflicts = "--global")]
    pub local: bool,
}

#[derive(Debug, usage_rs::Args)]
pub(crate) struct SetArgs {
    /// Setting key (canonical name or `.npmrc` alias).
    pub key: String,

    /// Value to write. Stored verbatim after `key=`.
    pub value: String,

    /// Use the user configuration instead of the project configuration.
    #[usage(short = 'g', long, conflicts = "--local")]
    pub global: bool,

    /// Use the project configuration (the default).
    #[usage(long, conflicts = "--global")]
    pub local: bool,
}

/// `config delete`: a key plus a scope, with no value.
#[derive(Debug, usage_rs::Args)]
pub(crate) struct KeyArgs {
    /// Setting key (canonical name or `.npmrc` alias).
    pub key: String,

    /// Use the user configuration instead of the project configuration.
    #[usage(short = 'g', long, conflicts = "--local")]
    pub global: bool,

    /// Use the project configuration (the default).
    #[usage(long, conflicts = "--global")]
    pub local: bool,
}

#[derive(Debug, Default, usage_rs::Args)]
pub(crate) struct ListArgs {
    /// Also list settings that have no value set.
    ///
    /// Renders one row per known setting, with its default shown.
    ///
    /// Not valid with `--local` or `--global`, since a single-file view cannot
    /// distinguish "not set anywhere" from "set in the other file".
    #[usage(long)]
    pub all: bool,

    /// List only the user configuration.
    #[usage(short = 'g', long, conflicts("--local", "--all"))]
    pub global: bool,

    /// Emit all entries as a JSON object keyed by setting name.
    #[usage(long)]
    pub json: bool,

    /// List only the project configuration.
    #[usage(long, conflicts("--global", "--all"))]
    pub local: bool,
}

#[derive(Debug, usage_rs::Args)]
pub(crate) struct ConfigArgs {
    #[usage(flatten)]
    pub list: ListArgs,
    #[usage(subcommand)]
    pub command: Option<ConfigCommand>,
}

#[derive(Debug, usage_rs::Subcommands)]
pub(crate) enum ConfigCommand {
    Get(GetArgs),
    Set(SetArgs),
    Delete(KeyArgs),
    List(ListArgs),
}

impl GetArgs {
    fn effective_location(&self) -> ListLocation {
        scope(self.global, self.local)
    }
}

impl KeyArgs {
    pub(crate) fn effective_location(&self) -> Location {
        if self.global {
            Location::User
        } else {
            Location::Project
        }
    }
}

impl ListArgs {
    fn effective_location(&self) -> ListLocation {
        scope(self.global, self.local)
    }
}

fn scope(global: bool, local: bool) -> ListLocation {
    if global {
        ListLocation::User
    } else if local {
        ListLocation::Project
    } else {
        ListLocation::Merged
    }
}

// ───────────────────────────── protected keys ─────────────────────────────

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

/// True when `key` names a secret npm declines to reveal. Ported from npm's
/// `isProtected`: any `_`-prefixed key, a bare protected name, or a
/// nerf-darted per-host form (`//host/:_authToken`, `//host/:username`).
///
/// This is the security floor that keeps `config get` and `config list` from
/// echoing registry tokens, in parity with `npm config get`.
pub(crate) fn is_protected_key(key: &str) -> bool {
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

// ──────────────────────────── settings metadata ────────────────────────────

/// A `.npmrc` key a user can literally type, as opposed to a pattern template
/// (`@scope:registry`, `//host/:_authToken`) that stands for a family.
fn is_literal_alias(key: &str) -> bool {
    !key.starts_with("//") && !key.contains(':')
}

pub(crate) fn literal_aliases(keys: &[&'static str]) -> Vec<String> {
    keys.iter()
        .filter(|k| is_literal_alias(k))
        .map(|s| (*s).to_string())
        .collect()
}

/// Expand a user-supplied key into the full set of `.npmrc` aliases it covers,
/// so a value written under one spelling is found under any other. A key no
/// setting claims stands for itself, which is what lets free-form config
/// round-trip.
pub(crate) fn resolve_aliases(key: &str) -> Vec<String> {
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

/// The setting `key` names under any spelling it is reachable by — canonical
/// name, `.npmrc` alias, workspace-yaml key, env var, or CLI flag.
pub(crate) fn setting_for_key(key: &str) -> Option<&'static SettingMeta> {
    settings_meta::find(key).or_else(|| {
        settings_meta::all().find(|meta| {
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

/// The one spelling a setting is reported under, so a value set as
/// `auto-install-peers` and one set as `autoInstallPeers` collapse to a single
/// row rather than appearing twice with different names.
pub(crate) fn primary_entry_key(meta: &SettingMeta) -> String {
    literal_aliases(meta.npmrc_keys)
        .into_iter()
        .next()
        .unwrap_or_else(|| meta.name.to_string())
}

fn canonical_list_key(key: &str) -> String {
    setting_for_key(key).map_or_else(|| key.to_string(), primary_entry_key)
}

// ────────────────────────────────── sources ──────────────────────────────────

/// The project root a config read resolves against: the nearest ancestor that
/// looks like a project. Matches the root `config set` writes into, so a value
/// written and a value read come from the same directory.
pub(crate) fn project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = cwd.clone();
    for _ in 0..16 {
        if dir.join("package.json").is_file() || dir.join("pnpm-workspace.yaml").is_file() {
            return dir;
        }
        if !dir.pop() {
            break;
        }
    }
    cwd
}

/// The user's `.npmrc` path, honoring `npm_config_userconfig`.
pub(crate) fn user_npmrc_path() -> Option<PathBuf> {
    ["npm_config_userconfig", "NPM_CONFIG_USERCONFIG"]
        .into_iter()
        .find_map(|name| std::env::var_os(name).filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .or_else(|| dirs_next::home_dir().map(|home| home.join(".npmrc")))
}

/// The `.npmrc` file a write at `location` lands in.
pub(crate) fn npmrc_path(location: Location) -> Result<PathBuf> {
    match location {
        Location::User => user_npmrc_path()
            .ok_or_else(|| anyhow!("nub could not determine a home directory for `.npmrc`")),
        Location::Project => Ok(project_root().join(".npmrc")),
    }
}

/// Flatten one `.npmrc` file's entries. The repeated `key[]=value` list form
/// collapses to a comma-joined scalar, which is how `config get` has always
/// rendered a list and what `config set` accepts back.
///
/// Parsed by the same reader the install uses, so a line one of them accepts
/// and the other rejects cannot exist.
pub(super) fn entries_of(text: &str) -> Vec<(String, String)> {
    host_settings::npmrc_entries(text)
        .into_iter()
        .map(|(key, raw)| {
            let value = match raw {
                Raw::Scalar(v) => v,
                Raw::List(items) => items.join(","),
            };
            (key, value)
        })
        .collect()
}

/// The same flattening, without the engine's reader. Reachable only in the
/// transitional build that has no engine at all.
fn read_npmrc(path: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(path)
        .map(|t| entries_of(&t))
        .unwrap_or_default()
}

pub(crate) fn read_user_entries() -> Vec<(String, String)> {
    let mut out = user_npmrc_path()
        .map(|p| read_npmrc(&p))
        .unwrap_or_default();
    out.extend(branded_yaml(&project_root(), BrandedSource::GlobalConfig));
    out
}

pub(crate) fn read_project_entries() -> Result<Vec<(String, String)>> {
    let root = project_root();
    let project = root.join(".npmrc");
    // A project whose root IS the home directory would otherwise report the
    // user file as its own.
    let mut out = if user_npmrc_path().as_deref() == Some(project.as_path()) {
        Vec::new()
    } else {
        read_npmrc(&project)
    };
    out.extend(branded_yaml(&root, BrandedSource::WorkspaceYaml));
    out.extend(nub_jsonc_entries(&root)?);
    Ok(out)
}

/// The environment's overlay, which belongs to the merged view and to no
/// file. `--local` and `--global` each name a FILE, so an environment value
/// under either would answer a question that was not asked.
///
/// Which variables count follows the identity, because the two installs read
/// different ones: pnpm 12 reads `pnpm_config_*` and no `npm_config_*`, and a
/// nub install the reverse (measured against pnpm 12.4.1). pnpm's are read
/// through the engine's own reader, so the answer is the one its install uses.
fn env_entries(root: &Path) -> Vec<(String, String)> {
    let settings = if super::project_identity::detect(root)
        == super::project_identity::ProjectIdentity::Pnpm
    {
        pnpm_env_settings()
    } else {
        super::host_settings::env_settings()
    };
    settings
        .into_iter()
        .filter_map(|(key, value)| Some((canonical_list_key(&key), render(value)?)))
        .collect()
}

/// The settings `pnpm_config_*` variables set, as the engine reads them. A
/// field still at its default was set by nothing.
fn pnpm_env_settings() -> serde_json::Map<String, Value> {
    let from_env = pnpm_config::WorkspaceSettings::from_pnpm_config_env::<pnpm_config::Host>();
    let (Ok(Value::Object(set)), Ok(Value::Object(unset))) = (
        serde_json::to_value(from_env),
        serde_json::to_value(pnpm_config::WorkspaceSettings::default()),
    ) else {
        return serde_json::Map::new();
    };
    set.into_iter()
        .filter(|(key, value)| !value.is_null() && unset.get(key) != Some(value))
        .collect()
}

/// Every source, lowest precedence first, so a later duplicate wins.
///
/// Built explicitly rather than by concatenating the two scope readers: pnpm's
/// global `config.yaml` belongs to the USER scope but outranks the project
/// `.npmrc` in the merged view, which is the order the resolver uses and the
/// only order in which `config get` answers what an install would act on.
///
/// The defaults tier is here and not in either scope reader for the same
/// reason it is in neither of npm's: a single-file view answers "what does
/// THIS file say", and a default says nothing about any file.
///
/// Anchored at the WORKSPACE root, not at the member the command runs in. An
/// install from a member reads the root's `.npmrc` and `pnpm-workspace.yaml`
/// and never the member's own `.npmrc`, and pnpm's `config get` answers the
/// same way. `config set` still writes the member's file, as pnpm's does, so
/// only this merged view moves.
pub(crate) fn read_merged() -> Result<Vec<(String, String)>> {
    let root = merged_root();
    let mut out = default_entries(&root);
    out.extend(configured_entries(&root)?);
    Ok(out)
}

/// The merged view without nub's defaults: every source something actually
/// set, lowest precedence first.
fn configured_entries(root: &Path) -> Result<Vec<(String, String)>> {
    let mut out = file_entries(root);
    out.extend(nub_jsonc_entries(root)?);
    // Last because it is highest.
    out.extend(env_entries(root));
    Ok(out)
}

/// The `.npmrc` chain and the branded YAML files, lowest precedence first:
/// every file source but `nub.jsonc`, which is the one that can refuse.
fn file_entries(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(user) = user_npmrc_path() {
        out.extend(read_npmrc(&user));
        let project = root.join(".npmrc");
        if project != user {
            out.extend(read_npmrc(&project));
        }
    } else {
        out.extend(read_npmrc(&root.join(".npmrc")));
    }
    out.extend(branded_yaml(root, BrandedSource::GlobalConfig));
    out.extend(branded_yaml(root, BrandedSource::WorkspaceYaml));
    out
}

/// The workspace root of the project a config read runs in, or the project
/// root itself outside a workspace.
fn merged_root() -> PathBuf {
    let root = project_root();
    nub_core::workspace::detect::detect_project(&root)
        .map(|project| project.workspace_root.unwrap_or(project.root))
        .unwrap_or(root)
}

/// The registry an install from the workspace at `root` fetches unscoped
/// packages from: the merged view's answer, else the engine's own default.
///
/// For `nub run`'s `npm_config_registry` export, which has to say what `nub
/// config get registry` says. The defaults tier is skipped because it carries
/// no registry, and computing it walks the project for lockfiles on a path
/// that runs before every script.
pub(crate) fn registry_at(root: &Path) -> String {
    let aliases = resolve_aliases("registry");
    // A script run never installs, so a `nub.jsonc` value the install refuses
    // must not stop it: that tier is read as far as it goes here, and the
    // files and the environment still answer.
    let mut entries = file_entries(root);
    entries.extend(nub_jsonc_entries(root).unwrap_or_default());
    entries.extend(env_entries(root));
    entries
        .into_iter()
        .rev()
        .find_map(|(key, value)| (key == "registry" || aliases.contains(&key)).then_some(value))
        .unwrap_or_else(pnpm_config::default_registry)
}

/// The defaults nub itself applies, lowest precedence of all.
///
/// Sourced from the install's own defaults ([`super::host_settings::defaults`])
/// rather than from the settings TABLE. The distinction is what the layout axis rests
/// on: the table declares a default for every setting it describes, including
/// layout ones nub deliberately does not default here, so a table-wide tier
/// would answer `shamefullyHoist` with `false` where the correct answer is that
/// nothing set it. The list also carries values the table cannot know — the
/// store and cache directories, and whether the shared store is on.
fn default_entries(root: &Path) -> Vec<(String, String)> {
    super::host_settings::defaults(root)
        .into_iter()
        .map(|(key, value)| (canonical_list_key(&key), value))
        .collect()
}

/// The two pnpm-named files a config read may consult.
#[derive(Clone, Copy)]
enum BrandedSource {
    /// `$XDG_CONFIG_HOME/pnpm/config.yaml`.
    GlobalConfig,
    /// The project's `pnpm-workspace.yaml`.
    WorkspaceYaml,
}

/// What a pnpm-named file supplies, or nothing at all when this project has no
/// pnpm incumbent.
///
/// The brand boundary is about a NUB project: under pnpm incumbency these are
/// the incumbent's own files and reading them is what compatibility means. The
/// two gates differ because the files do — `pnpm-workspace.yaml` is read for
/// any pnpm major, while the global `config.yaml` only became a settings home
/// in pnpm 11, so reading it under a v10 incumbent would report a value that
/// project's own pnpm ignores.
///
/// LAYOUT settings are dropped from both. Under a pnpm incumbent the one caller
/// left is [`registry_at`], so the drop changes no answer this module gives.
fn branded_yaml(root: &Path, source: BrandedSource) -> Vec<(String, String)> {
    let Some(map) = branded_yaml_map(root, source) else {
        return Vec::new();
    };

    // Suppressed by NAME as well as by setting: the trailing pass admits an
    // unrecognized key verbatim so free-form config round-trips, and without
    // the name list a layout key would come back in through that door.
    let mut suppressed: Vec<&str> = Vec::new();
    let mut out: Vec<(String, String)> = Vec::new();
    for meta in settings_meta::all() {
        if meta.layout {
            suppressed.extend(meta.workspace_yaml_keys.iter().copied());
            continue;
        }
        for key in meta.workspace_yaml_keys {
            if let Some(raw) = map.get(*key).and_then(yaml_scalar)
                && !out.iter().any(|(existing, _)| existing == key)
            {
                out.push(((*key).to_string(), raw));
            }
        }
    }
    for (key, value) in &map {
        if suppressed.contains(&key.as_str()) || out.iter().any(|(e, _)| e == key) {
            continue;
        }
        if let Some(raw) = yaml_scalar(value) {
            out.push((key.clone(), raw));
        }
    }
    out
}

/// The raw map one pnpm-named file carries, after the same incumbency gates
/// [`branded_yaml`] applies. `None` covers every reason there is nothing to
/// read: no pnpm incumbent, no file, unparseable YAML.
fn branded_yaml_map(
    root: &Path,
    source: BrandedSource,
) -> Option<BTreeMap<String, serde_yaml::Value>> {
    let path = branded_yaml_path(root, source)?;
    let text = std::fs::read_to_string(&path).ok()?;
    serde_yaml::from_str(&text).ok()
}

fn branded_yaml_path(root: &Path, source: BrandedSource) -> Option<PathBuf> {
    if !pnpm_incumbent(root) {
        return None;
    }
    match source {
        BrandedSource::WorkspaceYaml => {
            let path = root.join("pnpm-workspace.yaml");
            path.exists().then_some(path)
        }
        BrandedSource::GlobalConfig => {
            // pnpm's global `config.yaml` only became a settings home in
            // pnpm 11; the same major gate `config set` routes on.
            if !super::store_config_family::pnpm_v11_scalar_home() {
                return None;
            }
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| dirs_next::home_dir().map(|h| h.join(".config")))?;
            let path = base.join("pnpm").join("config.yaml");
            path.exists().then_some(path)
        }
    }
}

/// Whether the project at `root` declares pnpm as its package manager.
///
/// Anchored at a root the caller names rather than at the process directory:
/// the install report resolves the project the command line named (`--dir`),
/// which is not always the one the process sits in, and answering from the
/// wrong project reads as a working install of a different tree.
fn pnpm_incumbent(root: &Path) -> bool {
    use super::project_identity::{ProjectIdentity, detect};
    detect(root) == ProjectIdentity::Pnpm
}

/// A YAML scalar as `config get` prints it. A mapping or a nested sequence has
/// no one-line spelling and is left to the file.
fn yaml_scalar(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Sequence(items) => {
            let parts: Vec<String> = items.iter().filter_map(yaml_scalar).collect();
            (parts.len() == items.len()).then(|| parts.join(","))
        }
        _ => None,
    }
}

/// What `nub.jsonc` and the environment supply, spelled the way `.npmrc`
/// spells it.
///
/// Lowered by the install's OWN code ([`host_settings::supplied_settings`])
/// rather than re-derived here, so the two cannot disagree about what a
/// curated key means — `install.linker: "global"` is `nodeLinker` plus
/// `enableGlobalVirtualStore` in exactly one place.
fn nub_jsonc_entries(_root: &Path) -> Result<Vec<(String, String)>> {
    // The config verbs dispatch through `lookup_verb` and RETURN before the
    // parser match that initializes the snapshot for every other route, so on
    // this path `effective_config` is unset unless it is asked for here.
    // Without it the whole tier reports "nub.jsonc supplies nothing" for every
    // project — silently, because an absent tier just reads as an unset key.
    //
    // A parse failure reports the tier as empty rather than propagating: a
    // malformed `nub.jsonc` means we cannot know what it supplies, and refusing
    // every `config get` on the strength of an unparseable file is a worse
    // answer than the file view the caller already has. A value the file
    // parses but the install refuses is the opposite case — exactly known, and
    // the install's own error is the answer — so that one propagates.
    if crate::cli::initialize_config_snapshot(false, false).is_err() {
        return Ok(Vec::new());
    }
    let Some(config) = crate::project_config::effective_config() else {
        return Ok(Vec::new());
    };
    Ok(host_settings::supplied_settings(&config.values.install)?
        .into_iter()
        .filter_map(|(key, value)| Some((canonical_list_key(&key), render(value)?)))
        .collect())
}

/// A supplied value as `config get` prints it. A map or a nested structure has
/// no one-line spelling, so it is reported through `--json` only.
pub(super) fn render(value: Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(items) => Some(
            items
                .into_iter()
                .filter_map(|item| match item {
                    Value::String(s) => Some(s),
                    Value::Bool(b) => Some(b.to_string()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(","),
        ),
        Value::Null | Value::Object(_) => None,
    }
}

fn entries_for(location: ListLocation) -> Result<Vec<(String, String)>> {
    match location {
        ListLocation::Merged => read_merged(),
        ListLocation::User => Ok(read_user_entries()),
        ListLocation::Project => read_project_entries(),
    }
}

// ────────────────────────────────── commands ──────────────────────────────────

/// `config` with no subcommand lists; otherwise dispatch. The parent-position
/// list flags are rejected on a key subcommand rather than silently ignored,
/// so `nub config --all get registry` says what it means.
pub(crate) fn run(args: ConfigArgs) -> Result<i32> {
    let ConfigArgs { list, command } = args;
    let code = match command {
        None => run_list(list),
        Some(ConfigCommand::List(mut sub)) => {
            sub.all |= list.all;
            sub.json |= list.json;
            run_list(sub)
        }
        Some(ConfigCommand::Get(sub)) => {
            reject_parent_list_args(&list, "get")?;
            run_get(sub)
        }
        Some(ConfigCommand::Delete(sub)) => {
            reject_parent_list_args(&list, "delete")?;
            run_delete(sub)
        }
        // Unreachable: every `set` route returns from `dispatch_config`. Kept
        // as a refusal rather than an `unreachable!` so a future route that
        // forgets to return reports itself instead of aborting the process.
        Some(ConfigCommand::Set(set)) => {
            bail!("nub config set {} was not routed to a writer", set.key)
        }
    };
    match code {
        Ok(()) => Ok(0),
        Err(err) => Err(err),
    }
}

/// Run `args` with stdout handed to `then` instead of the terminal.
///
/// One caller: `config get registry`, which substitutes the registry an
/// install would actually reach for the bare `undefined` an unset key prints.
/// Capturing rather than branching before the lookup keeps every other
/// outcome — a configured value, a scope flag, `--json` — on exactly the path
/// it was already on.
pub(crate) fn run_captured(args: ConfigArgs, then: impl FnOnce(&str)) -> Result<i32> {
    let (result, captured) = super::with_fd_captured(1, || run(args));
    let code = result?;
    then(&captured);
    Ok(code)
}

fn reject_parent_list_args(list: &ListArgs, sub: &str) -> Result<()> {
    if list.all {
        bail!("--all applies to `nub config list`, not `nub config {sub}`");
    }
    Ok(())
}

fn run_get(args: GetArgs) -> Result<()> {
    // Refuse to echo auth-bearing keys, matching `npm config get`'s protected-key
    // guard. Without this, `config get //registry.npmjs.org/:_authToken` would
    // print the registry token.
    if is_protected_key(&args.key) {
        bail!(
            "The {} option is protected, and cannot be retrieved in this way",
            args.key
        );
    }
    let aliases = resolve_aliases(&args.key);
    let entries = entries_for(args.effective_location())?;
    let found = entries
        .iter()
        .rev()
        .find_map(|(k, v)| (aliases.iter().any(|a| a == k) || k == &args.key).then(|| v.clone()));
    match found {
        Some(v) if args.json => println!("{}", Value::String(v)),
        Some(v) => println!("{v}"),
        // `undefined` under both renderings, which is what `pnpm config get`
        // and `npm config get` both print — and what `--json` prints too, since
        // JSON has no way to spell "no value" that a shell consumer expects.
        None => println!("undefined"),
    }
    Ok(())
}

fn run_list(args: ListArgs) -> Result<()> {
    let location = args.effective_location();
    if args.all && !matches!(location, ListLocation::Merged) {
        bail!("--all cannot be combined with --local or --global");
    }
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in entries_for(location)? {
        seen.insert(canonical_list_key(&key), value);
    }

    let mut defaults: HashSet<String> = HashSet::new();
    if args.all {
        for meta in settings_meta::all() {
            let literals = literal_aliases(meta.npmrc_keys);
            let Some(primary) = literals.first().cloned() else {
                continue;
            };
            if !literals.iter().any(|k| seen.contains_key(k)) {
                seen.insert(primary.clone(), meta.rendered_default().into_owned());
                defaults.insert(primary);
            }
        }
    }

    if args.json {
        let obj: serde_json::Map<String, Value> = seen
            .into_iter()
            // npm's `config list --json` omits protected keys entirely; mirror
            // that so tokens never reach a JSON consumer.
            .filter(|(k, _)| !is_protected_key(k))
            .map(|(k, v)| {
                let value = if args.all {
                    serde_json::json!({ "value": v, "default": defaults.contains(&k) })
                } else {
                    Value::String(v)
                };
                (k, value)
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Object(obj))
                .map_err(|e| anyhow!("failed to serialize config: {e}"))?
        );
    } else {
        for (k, v) in &seen {
            if is_protected_key(k) {
                // Render auth-bearing keys as `(protected)` rather than echoing
                // the secret, matching `npm config list`.
                println!("{k}=(protected)");
            } else if defaults.contains(k) {
                println!("{k}={v} (default)");
            } else {
                println!("{k}={v}");
            }
        }
    }
    Ok(())
}

/// `config delete` sweeps exactly the files `config set` writes, so a value
/// this command cannot reach is one that command could not have created.
fn run_delete(args: KeyArgs) -> Result<()> {
    let location = args.effective_location();
    let aliases = resolve_aliases(&args.key);
    let path = npmrc_path(location)?;
    let mut removed: Vec<PathBuf> = Vec::new();

    if path.exists() && remove_npmrc_keys(&path, &aliases, &args.key)? {
        removed.push(path.clone());
    }

    if removed.is_empty() {
        // Name the file that was searched: "not set" is only useful alongside
        // where it was looked for, and the scope flags change that answer.
        bail!("{} not set in {}", args.key, path.display());
    }
    let joined = removed
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    present::info(&format!("deleted {} ({joined})", args.key));
    Ok(())
}

/// Drop every line naming `key` or any of its aliases, preserving the rest of
/// the file byte for byte. A line-level edit rather than a parse-and-rewrite,
/// for the same reason the `.npmrc` writer is one: the file is a user's, and
/// round-tripping it through a parser loses their comments and ordering.
fn remove_npmrc_keys(path: &Path, aliases: &[String], raw_key: &str) -> Result<bool> {
    let original = std::fs::read_to_string(path)?;
    let matches_key = |line: &str| {
        let name = line.split('=').next().unwrap_or("").trim();
        let name = name.strip_suffix("[]").unwrap_or(name);
        aliases.iter().any(|a| a == name) || name == raw_key
    };
    let kept: Vec<&str> = original
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with(['#', ';', '[']) {
                return true;
            }
            !matches_key(trimmed)
        })
        .collect();
    if kept.len() == original.lines().count() {
        return Ok(false);
    }
    let mut text = kept.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    std::fs::write(path, text)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_keys_cover_npms_own_list_and_the_nerf_darted_forms() {
        assert!(is_protected_key("_auth"));
        assert!(is_protected_key("authToken"));
        assert!(is_protected_key("//registry.npmjs.org/:_authToken"));
        assert!(is_protected_key("//registry.npmjs.org/:username"));
        assert!(!is_protected_key("registry"));
        assert!(!is_protected_key("//registry.npmjs.org/:always-auth"));
    }

    /// A pattern template stands for a family of keys rather than one a user
    /// types, so it must never become an alias a lookup expands into.
    #[test]
    fn only_literal_aliases_take_part_in_lookup() {
        assert!(is_literal_alias("registry"));
        assert!(is_literal_alias("auto-install-peers"));
        assert!(!is_literal_alias("@scope:registry"));
        assert!(!is_literal_alias("//host/:_authToken"));
    }

    /// The two spellings of one setting collapse to a single row, or the same
    /// value would be listed twice under different names.
    #[test]
    fn both_spellings_of_a_setting_resolve_to_one_reported_key() {
        let camel = canonical_list_key("autoInstallPeers");
        let kebab = canonical_list_key("auto-install-peers");
        assert_eq!(camel, kebab);
        assert!(resolve_aliases("autoInstallPeers").contains(&kebab));
    }

    /// A key no setting claims is reported under the name the user wrote, so
    /// free-form `.npmrc` config still round-trips through `get` and `list`.
    #[test]
    fn an_unknown_key_stands_for_itself() {
        assert_eq!(canonical_list_key("some-custom-key"), "some-custom-key");
        assert_eq!(resolve_aliases("some-custom-key"), vec!["some-custom-key"]);
    }

    #[test]
    fn the_repeated_list_form_reads_back_as_one_comma_joined_value() {
        let entries =
            entries_of("public-hoist-pattern[]=*eslint*\npublic-hoist-pattern[]=*prettier*\n");
        assert_eq!(
            entries,
            vec![(
                "public-hoist-pattern".to_string(),
                "*eslint*,*prettier*".to_string()
            )]
        );
    }

    /// The delete sweep is line-level so a user's comments and unrelated
    /// entries survive it; the aliases make a value written under one spelling
    /// removable by the other.
    #[test]
    fn deleting_a_key_leaves_the_rest_of_the_file_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".npmrc");
        std::fs::write(
            &path,
            "# keep me\nregistry=https://example.test/\nauto-install-peers=false\n",
        )
        .expect("write");
        let removed = remove_npmrc_keys(
            &path,
            &resolve_aliases("autoInstallPeers"),
            "autoInstallPeers",
        )
        .expect("remove");
        assert!(removed);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "# keep me\nregistry=https://example.test/\n"
        );
    }

    #[test]
    fn deleting_a_key_that_is_not_there_reports_no_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".npmrc");
        std::fs::write(&path, "registry=https://example.test/\n").expect("write");
        assert!(
            !remove_npmrc_keys(
                &path,
                &resolve_aliases("autoInstallPeers"),
                "autoInstallPeers"
            )
            .expect("remove")
        );
    }
}
