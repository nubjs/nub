//! The project-level `nub.jsonc` — nub's per-project settings file, discovered
//! up-tree from the run's working directory. Distinct from the GLOBAL file
//! (`~/.config/nub/nub.jsonc`, [`crate::config`]): the global file is nub's own
//! durable-settings home and is read best-effort (a malformed file degrades to
//! the default); the PROJECT file is authored by the user for one project and is
//! read FAIL-LOUD (an unknown key or malformed value is an error, per the bunfig
//! silent-no-op lesson — [`.fray/nub-config-spec.md`]).
//!
//! Dialect: JSONC (JSON + comments + trailing commas), the tsconfig dialect,
//! parsed through [`crate::jsonc`] — the same depth-guarded reader the global
//! file uses. camelCase keys. `$schema` is the one blessed non-field key
//! (accepted + ignored). Every other unrecognized key fails loud.
//!
//! Project files are discovered from the invocation's final working directory.
//! Sandbox values are parsed and retained for forward compatibility, but no
//! runtime, install, or dlx consumer applies them yet.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::config::ImplicitDlx;

/// The filename discovered up-tree. `nub.json` is NEVER accepted (no dual-name
/// ambiguity) — one name, matching the global file.
const FILE_NAME: &str = "nub.jsonc";

const ROOT_KEYS: &[&str] = &[
    "$schema",
    "nodeCompat",
    "preload",
    "nodeOptions",
    "v8Flags",
    "env",
    "define",
    "loader",
    "conditions",
    "tsconfig",
    "verifyDepsBeforeRun",
    "sandbox",
    "install",
    "dlx",
];
const INSTALL_KEYS: &[&str] = &[
    "nodeLinker",
    "symlinkDisablePattern",
    "hoist",
    "minimumReleaseAge",
    "minimumReleaseAgeExclude",
    "nodeOptions",
    "sandbox",
];
const DLX_KEYS: &[&str] = &["consent", "sandbox", "env"];
const SANDBOX_AXIS_KEYS: &[&str] = &["fs", "net", "env"];

// ─────────────────────────────────────────────────────────────────────────────
// Error type — fail-loud, with a JSON path so a bad file self-describes.
// ─────────────────────────────────────────────────────────────────────────────

/// A project-config load failure. Carries a dotted JSON path (`install.hoist`)
/// so the message points at the offending key without the user re-deriving it.
#[derive(Debug)]
pub enum ConfigError {
    /// The file exists but could not be read (I/O).
    Io(std::io::Error),
    /// The file is not valid JSONC.
    Parse(String),
    /// A key nub does not recognize at `path` (fail-loud, not a silent no-op).
    UnknownKey { path: String, key: String },
    /// The value at `path` has the wrong JSON type.
    Type {
        path: String,
        expected: &'static str,
    },
    /// The value at `path` is the right type but semantically invalid.
    Value { path: String, message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "reading {FILE_NAME}: {e}"),
            ConfigError::Parse(m) => write!(f, "parsing {FILE_NAME}: {m}"),
            ConfigError::UnknownKey { path, key } => {
                write!(f, "unknown key `{key}` in {path} of {FILE_NAME}")
            }
            ConfigError::Type { path, expected } => {
                write!(f, "`{path}` in {FILE_NAME} must be {expected}")
            }
            ConfigError::Value { path, message } => {
                write!(f, "`{path}` in {FILE_NAME}: {message}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

type Result<T> = std::result::Result<T, ConfigError>;

// ─────────────────────────────────────────────────────────────────────────────
// The typed config shape. Runtime, install, and dlx values are consumed by their
// command paths. Sandbox values are shape-validated and retained, but remain
// inert until the sandbox frontend is enabled.
// ─────────────────────────────────────────────────────────────────────────────

/// The parsed, validated `nub.jsonc`. Absent fields are `None`; explicitly empty
/// collections remain `Some(empty)` for precedence. `$schema` is not stored
/// (accepted + ignored — editor metadata with no runtime effect).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ProjectConfig {
    // ── runtime top-levels (bunfig-style flat) ──
    pub node_compat: Option<bool>,
    pub preload: Option<Vec<String>>,
    pub node_options: Option<Vec<String>>,
    pub v8_flags: Option<Vec<String>>,
    pub env: Option<EnvSetting>,
    pub define: Option<BTreeMap<String, String>>,
    pub loader: Option<BTreeMap<String, String>>,
    pub conditions: Option<Vec<String>>,
    pub tsconfig: Option<String>,
    pub verify_deps_before_run: Option<VerifyDepsBeforeRun>,

    // ── the default sandbox (parsed and retained, not consumed) ──
    pub sandbox: Option<SandboxSetting>,

    // ── install phase ──
    pub install: InstallConfig,

    // ── nubx / dlx ──
    pub dlx: DlxConfig,
}

impl ProjectConfig {
    pub fn builtin_defaults() -> Self {
        Self {
            node_compat: Some(false),
            env: Some(EnvSetting::Default),
            verify_deps_before_run: Some(VerifyDepsBeforeRun::Warn),
            dlx: DlxConfig {
                consent: Some(ImplicitDlx::Prompt),
                ..DlxConfig::default()
            },
            ..Self::default()
        }
    }
}

/// The `env` field's tri-state (merged `env` + `envFile`, per the spec):
/// `true` = today's default discovery, `false` = disable all env loading,
/// string / string[] = an exclusive source list (old `envFile` semantics).
/// Source strings are stored RAW; `${VAR}`/`$VAR` expansion is applied at the
/// wiring boundary (it references the process env, which the parser must not).
#[derive(Debug, Clone, PartialEq)]
pub enum EnvSetting {
    /// `true` — default `.env*` discovery.
    Default,
    /// `false` — disable all env-file loading.
    Disabled,
    /// A string / string[] exclusive source list.
    Sources(Vec<String>),
}

/// `verifyDepsBeforeRun` — pnpm's literal field name + value space (zero-
/// translation compat). `Enabled(false)` is pnpm's `false`; the string arms are
/// pnpm's `"install"|"warn"|"error"|"prompt"`.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyDepsBeforeRun {
    Enabled(bool),
    Install,
    Warn,
    Error,
    Prompt,
}

/// The `install` block. `node_linker` is the collapsed flat layout enum. The
/// native PM consumes the non-sandbox fields through its aube settings bridge;
/// `sandbox` remains inert until the sandbox execution slice lands.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct InstallConfig {
    pub node_linker: Option<NodeLinker>,
    pub symlink_disable_pattern: Option<Vec<String>>,
    pub hoist: Option<Hoist>,
    pub minimum_release_age: Option<Duration>,
    pub minimum_release_age_exclude: Option<Vec<String>>,
    pub node_options: Option<Vec<String>>,
    pub sandbox: Option<SandboxSetting>,
}

/// The flat layout enum (collapsed 2026-07-08). `symlink` (nub's default global
/// virtual store) / `isolated` (pnpm-parity project-local) / `hoisted` (flat real
/// dirs). `pnp` is reserved for PnP-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeLinker {
    Symlink,
    Isolated,
    Hoisted,
    Pnp,
}

/// `hoist` — pnpm-literal `boolean | string[]`. `Bool(false)` = strict,
/// `Bool(true)` ≡ pnpm `['*']`, `Patterns` = the pattern list.
#[derive(Debug, Clone, PartialEq)]
pub enum Hoist {
    Bool(bool),
    Patterns(Vec<String>),
}

/// The `dlx` block — nubx's own security posture. `consent` reuses the global
/// file's [`ImplicitDlx`] enum; `env` reuses the top-level tri-state.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DlxConfig {
    pub consent: Option<ImplicitDlx>,
    pub sandbox: Option<SandboxSetting>,
    pub env: Option<EnvSetting>,
}

/// Where a configuration value came from. File-backed sources retain both the
/// exact path and its containing directory so relative values never fall back to
/// the ambient process cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    pub kind: ConfigSourceKind,
    pub path: Option<PathBuf>,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSourceKind {
    Cli,
    Environment,
    Project,
    Global,
    Defaults,
}

impl ConfigSource {
    fn transient(kind: ConfigSourceKind, root: &Path) -> Self {
        Self {
            kind,
            path: None,
            root: root.to_path_buf(),
        }
    }

    fn file(kind: ConfigSourceKind, path: &Path) -> Self {
        Self {
            kind,
            path: Some(path.to_path_buf()),
            root: path.parent().unwrap_or_else(|| Path::new("")).to_path_buf(),
        }
    }
}

/// Parsed data paired with the file that supplied it.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedConfig {
    pub source: ConfigSource,
    pub values: ProjectConfig,
}

/// Typed keys used to retain the winning source after precedence resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigKey {
    NodeCompat,
    Preload,
    NodeOptions,
    V8Flags,
    Env,
    Define,
    Loader,
    Conditions,
    Tsconfig,
    VerifyDepsBeforeRun,
    Sandbox,
    InstallNodeLinker,
    InstallSymlinkDisablePattern,
    InstallHoist,
    InstallMinimumReleaseAge,
    InstallMinimumReleaseAgeExclude,
    InstallNodeOptions,
    InstallSandbox,
    DlxConsent,
    DlxSandbox,
    DlxEnv,
}

/// Process-local overlays. Every field remains optional so an explicit false or
/// empty collection wins while an absent value falls through.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ConfigOverlays {
    pub cli: ProjectConfig,
    pub environment: ProjectConfig,
    pub defaults: ProjectConfig,
}

/// The one resolved configuration view for an invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveConfig {
    pub cwd: PathBuf,
    pub values: ProjectConfig,
    pub sources: BTreeMap<ConfigKey, ConfigSource>,
    pub project: Option<LoadedConfig>,
    pub global: Option<LoadedConfig>,
}

static EFFECTIVE_CONFIG: OnceLock<EffectiveConfig> = OnceLock::new();

pub(crate) const RUNTIME_CONFIG_ENV: &str = nub_core::node::spawn::RUNTIME_CONFIG_ENV;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeConfig {
    pub node_compat: bool,
    pub preload: Vec<String>,
    pub node_options: Vec<String>,
    pub v8_flags: Vec<String>,
    pub env: RuntimeEnv,
    pub define: BTreeMap<String, String>,
    pub loader: BTreeMap<String, String>,
    pub conditions: Vec<String>,
    pub tsconfig: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "sources", rename_all = "camelCase")]
pub(crate) enum RuntimeEnv {
    Default,
    Disabled,
    Sources(Vec<PathBuf>),
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            node_compat: false,
            preload: Vec::new(),
            node_options: Vec::new(),
            v8_flags: Vec::new(),
            env: RuntimeEnv::Default,
            define: BTreeMap::new(),
            loader: BTreeMap::new(),
            conditions: Vec::new(),
            tsconfig: None,
        }
    }
}

// ── sandbox skeleton ─────────────────────────────────────────────────────────

/// The `sandbox` wrapper trichotomy (skeleton). Shape-validated here; the Phase-1
/// compiler owns resolution (presets → policy, spread, layering, env grammar).
/// Preset vs file-ref disambiguation: a path-like string (leading `./`/`../`/`/`/
/// `~`, or carrying an extension) is a file-ref; a bare identifier is a preset.
#[derive(Debug, Clone, PartialEq)]
pub enum SandboxSetting {
    /// `false` — fully unjail.
    Disabled,
    /// `true` — secure defaults / inherit.
    Enabled,
    /// A bare-identifier preset name (e.g. `"build-jail"`).
    Preset(String),
    /// A `"./file.json"` external sandbox config reference.
    FileRef(String),
    /// The granular `{ fs, net, env }` object form.
    Granular(SandboxAxes),
}

/// The three sandbox axes. Each is independently optional; the Phase-1 compiler
/// assigns the decided floor/inheritance semantics.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SandboxAxes {
    pub fs: Option<SandboxAxis>,
    pub net: Option<SandboxAxis>,
    pub env: Option<SandboxAxis>,
}

/// One axis value. Both surface forms are accepted (spec: every axis takes
/// `false | true | array | pattern-keyed object`). Entry semantics (polarity,
/// per-axis value ladder, env grammar) are the Phase-1 compiler's job — the
/// object form keeps its raw values so nothing is lost before then.
#[derive(Debug, Clone, PartialEq)]
pub enum SandboxAxis {
    Bool(bool),
    /// Array form: ordered glob entries (validated all-strings).
    Array(Vec<String>),
    /// Pattern-keyed object form: `{ "<pattern>": <value> }`, values kept raw.
    Object(BTreeMap<String, Value>),
}

/// Walk up from `start` (inclusive) to the filesystem root, returning the first
/// directory that holds a `nub.jsonc`.
pub fn discover_project_config(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir.join(FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Parse + validate the `nub.jsonc` at `path`.
/// FAIL-LOUD: an unknown key or malformed value is a [`ConfigError`], NOT a
/// silent degrade (unlike the best-effort global reader).
pub fn read_project_config_at(path: &Path) -> Result<LoadedConfig> {
    let text = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
    let source_path = std::path::absolute(path).map_err(ConfigError::Io)?;
    Ok(LoadedConfig {
        source: ConfigSource::file(ConfigSourceKind::Project, &source_path),
        values: parse_project_config(&text)?,
    })
}

pub(crate) fn read_global_config_at(path: &Path) -> Result<LoadedConfig> {
    let text = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
    let source_path = std::path::absolute(path).map_err(ConfigError::Io)?;
    Ok(LoadedConfig {
        source: ConfigSource::file(ConfigSourceKind::Global, &source_path),
        values: parse_global_config(&text)?,
    })
}

/// Parse + validate from raw JSONC text. Split out so tests can hit the validator
/// without touching the filesystem.
pub fn parse_project_config(text: &str) -> Result<ProjectConfig> {
    let value = crate::jsonc::parse_to_value(text).map_err(ConfigError::Parse)?;
    let Some(value) = value else {
        // An empty / comment-only file is a valid empty config.
        return Ok(ProjectConfig::default());
    };
    let obj = as_object(&value, "")?;
    validate_root(obj)
}

fn parse_global_config(text: &str) -> Result<ProjectConfig> {
    let value = crate::jsonc::parse_to_value(text).map_err(ConfigError::Parse)?;
    let Some(value) = value else {
        return Ok(ProjectConfig::default());
    };
    let mut obj = as_object(&value, "")?.clone();
    let legacy_consent = obj
        .remove("exec")
        .and_then(|exec| {
            exec.get("implicitDlx")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .and_then(|value| ImplicitDlx::parse(&value));
    obj.retain(|key, _| ROOT_KEYS.contains(&key.as_str()));
    let mut config = validate_root(&obj)?;
    if config.dlx.consent.is_none() {
        config.dlx.consent = legacy_consent;
    }
    Ok(config)
}

/// Discover up-tree from `start` and read the nearest project config. A malformed
/// project file propagates as an error; a genuinely absent file is `Ok(None)`.
pub fn load_project_config(start: &Path) -> Result<Option<LoadedConfig>> {
    match discover_project_config(start) {
        Some(path) => read_project_config_at(&path).map(Some),
        None => Ok(None),
    }
}

/// Resolve one invocation's typed snapshot after cwd handling. Project failures
/// are fail-loud; the global layer is supplied best-effort by `crate::config`.
pub fn load_effective_config(cwd: &Path, overlays: ConfigOverlays) -> Result<EffectiveConfig> {
    let project = load_project_config(cwd)?;
    let global = crate::config::load_global_config();
    Ok(resolve_effective_config(cwd, global, project, overlays))
}

/// Initialize and retain the process snapshot. Callers must do this only after
/// the invocation's final cwd has been applied and before command dispatch.
pub fn initialize_effective_config(
    cwd: &Path,
    overlays: ConfigOverlays,
) -> Result<&'static EffectiveConfig> {
    if let Some(config) = EFFECTIVE_CONFIG.get() {
        return Ok(config);
    }
    let config = load_effective_config(cwd, overlays)?;
    if EFFECTIVE_CONFIG.set(config).is_ok() {
        record_snapshot_initialization_for_tests(
            EFFECTIVE_CONFIG
                .get()
                .expect("effective config was initialized"),
        );
    }
    Ok(EFFECTIVE_CONFIG
        .get()
        .expect("effective config was initialized"))
}

fn record_snapshot_initialization_for_tests(config: &EffectiveConfig) {
    use std::io::Write;

    // Internal process-test seam: one line is appended only when this process
    // wins the OnceLock initialization. Double-underscore test controls are not
    // part of nub's public configuration surface.
    let Some(path) = std::env::var_os("__NUB_TEST_CONFIG_SNAPSHOT_LOG") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            file,
            "cwd={} project={}",
            config.cwd.display(),
            if config.project.is_some() {
                "loaded"
            } else {
                "none"
            }
        );
    }
}

pub fn effective_config() -> Option<&'static EffectiveConfig> {
    EFFECTIVE_CONFIG.get()
}

/// The resolved implicit-registry policy. A non-default snapshot value wins;
/// falling back to the legacy reader preserves `exec.implicitDlx` even when an
/// otherwise malformed global file could not become a typed layer.
pub fn effective_dlx_consent() -> ImplicitDlx {
    let Some(config) = effective_config() else {
        return crate::config::implicit_dlx();
    };
    dlx_consent_for(config, crate::config::implicit_dlx())
}

fn dlx_consent_for(config: &EffectiveConfig, legacy: ImplicitDlx) -> ImplicitDlx {
    let source = config.sources.get(&ConfigKey::DlxConsent);
    if !source.is_some_and(|source| source.kind != ConfigSourceKind::Defaults) {
        return legacy;
    }
    config.values.dlx.consent.unwrap_or(legacy)
}

/// Load the resolved dlx env-file layer. `None` means no dlx env setting won,
/// preserving the pre-config behavior; `Some(empty)` is an explicit disable or
/// empty exclusive source list.
pub fn effective_dlx_env() -> Option<BTreeMap<String, String>> {
    let config = effective_config()?;
    dlx_env_for(config)
}

fn dlx_env_for(config: &EffectiveConfig) -> Option<BTreeMap<String, String>> {
    let setting = config.values.dlx.env.as_ref()?;
    let source = config.sources.get(&ConfigKey::DlxEnv)?;
    let mut values = match setting {
        EnvSetting::Disabled => BTreeMap::new(),
        EnvSetting::Default => {
            let root = nub_core::workspace::detect::detect_project(&config.cwd)
                .map(|project| project.root)
                .unwrap_or_else(|| config.cwd.clone());
            nub_core::workspace::env::load_env_files(&root)
                .into_iter()
                .collect()
        }
        EnvSetting::Sources(paths) => {
            let mut values = BTreeMap::new();
            for raw in paths {
                let mut expansion = std::collections::HashMap::from([(
                    "__NUB_CONFIG_ENV_PATH".to_string(),
                    raw.clone(),
                )]);
                nub_core::workspace::env::expand_env_map(&mut expansion);
                let expanded = expansion
                    .remove("__NUB_CONFIG_ENV_PATH")
                    .expect("the path expansion entry remains present");
                let path = Path::new(&expanded);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    source.root.join(path)
                };
                let Some(contents) = nub_core::workspace::env::read_env_file(&path) else {
                    continue;
                };
                for (key, value) in nub_core::workspace::env::parse_env(&contents) {
                    values.insert(key, value);
                }
            }
            let mut values: std::collections::HashMap<_, _> = values.into_iter().collect();
            let denied = nub_core::workspace::env::strip_denied_env_file_keys(&mut values);
            nub_core::workspace::env::warn_denied_env_file_keys(&denied);
            nub_core::workspace::env::expand_env_map(&mut values);
            values.into_iter().collect()
        }
    };
    values.retain(|key, _| std::env::var_os(key).is_none());
    Some(values)
}

pub(crate) fn runtime_config() -> Result<RuntimeConfig> {
    if let Some(serialized) = std::env::var_os(RUNTIME_CONFIG_ENV) {
        let mut runtime: RuntimeConfig = serde_json::from_slice(serialized.as_encoded_bytes())
            .map_err(|error| ConfigError::Value {
                path: "runtime".to_string(),
                message: format!("invalid inherited runtime snapshot: {error}"),
            })?;
        // The inherited snapshot is the source-anchored base for nested shim
        // launches, but the new invocation's explicit compatibility overlay is
        // still stronger. P1 has already resolved CLI > environment > files;
        // preserve every inherited runtime field and replace only nodeCompat
        // when this invocation supplied one of those two transient layers.
        if let Some(effective) = effective_config()
            && effective
                .sources
                .get(&ConfigKey::NodeCompat)
                .is_some_and(|source| {
                    matches!(
                        source.kind,
                        ConfigSourceKind::Cli | ConfigSourceKind::Environment
                    )
                })
        {
            runtime.node_compat = effective.values.node_compat.unwrap_or(false);
        }
        return Ok(runtime);
    }

    let Some(effective) = effective_config() else {
        return Ok(RuntimeConfig::default());
    };
    effective.runtime_config()
}

impl EffectiveConfig {
    fn source_root(&self, key: ConfigKey) -> &Path {
        self.sources
            .get(&key)
            .map(|source| source.root.as_path())
            .unwrap_or(self.cwd.as_path())
    }

    fn resolve_path(&self, key: ConfigKey, raw: &str) -> PathBuf {
        if let Some(rest) = raw.strip_prefix("~/")
            && let Some(home) = dirs_next::home_dir()
        {
            return home.join(rest);
        }
        let path = Path::new(raw);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.source_root(key).join(path)
        }
    }

    fn runtime_config(&self) -> Result<RuntimeConfig> {
        let values = &self.values;
        let preload = values
            .preload
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|value| {
                if is_path_like(value) {
                    self.resolve_path(ConfigKey::Preload, value)
                        .to_string_lossy()
                        .into_owned()
                } else {
                    value.clone()
                }
            })
            .collect();

        let env = match values.env.as_ref().unwrap_or(&EnvSetting::Default) {
            EnvSetting::Default => RuntimeEnv::Default,
            EnvSetting::Disabled => RuntimeEnv::Disabled,
            EnvSetting::Sources(sources) => RuntimeEnv::Sources(
                sources
                    .iter()
                    .map(|source| {
                        let expanded = expand_runtime_path(source);
                        self.resolve_path(ConfigKey::Env, &expanded)
                    })
                    .collect(),
            ),
        };

        let tsconfig = values
            .tsconfig
            .as_deref()
            .map(|path| self.resolve_path(ConfigKey::Tsconfig, path))
            .map(|path| path.to_string_lossy().into_owned());

        if let Some(path) = tsconfig.as_deref() {
            let text = std::fs::read_to_string(path).map_err(|error| ConfigError::Value {
                path: "tsconfig".into(),
                message: format!("cannot read `{path}`: {error}"),
            })?;
            let parsed =
                crate::jsonc::parse_to_value(&text).map_err(|error| ConfigError::Value {
                    path: "tsconfig".into(),
                    message: format!("cannot parse `{path}`: {error}"),
                })?;
            if !matches!(parsed, Some(Value::Object(_))) {
                return Err(ConfigError::Value {
                    path: "tsconfig".into(),
                    message: format!("`{path}` must contain a JSON object"),
                });
            }
        }

        Ok(RuntimeConfig {
            node_compat: values.node_compat.unwrap_or(false),
            preload,
            node_options: values.node_options.clone().unwrap_or_default(),
            v8_flags: values.v8_flags.clone().unwrap_or_default(),
            env,
            define: values.define.clone().unwrap_or_default(),
            loader: values.loader.clone().unwrap_or_default(),
            conditions: values.conditions.clone().unwrap_or_default(),
            tsconfig,
        })
    }
}

fn is_path_like(value: &str) -> bool {
    value.starts_with('.')
        || value.starts_with('/')
        || value.starts_with('~')
        || Path::new(value).is_absolute()
}

fn expand_runtime_path(value: &str) -> String {
    let key = "__NUB_CONFIG_ENV_PATH_VALUE__";
    let mut values: std::collections::HashMap<String, String> = std::env::vars().collect();
    values.insert(key.to_string(), value.to_string());
    nub_core::workspace::env::expand_env_map(&mut values);
    values.remove(key).unwrap_or_default()
}

fn resolve_effective_config(
    cwd: &Path,
    global: Option<LoadedConfig>,
    project: Option<LoadedConfig>,
    overlays: ConfigOverlays,
) -> EffectiveConfig {
    let mut values = ProjectConfig::default();
    let mut sources = BTreeMap::new();

    let defaults_source = ConfigSource::transient(ConfigSourceKind::Defaults, cwd);
    merge_layer(
        &mut values,
        &mut sources,
        &overlays.defaults,
        &defaults_source,
    );
    if let Some(layer) = global.as_ref() {
        merge_layer(&mut values, &mut sources, &layer.values, &layer.source);
    }
    if let Some(layer) = project.as_ref() {
        merge_layer(&mut values, &mut sources, &layer.values, &layer.source);
    }
    let environment_source = ConfigSource::transient(ConfigSourceKind::Environment, cwd);
    merge_layer(
        &mut values,
        &mut sources,
        &overlays.environment,
        &environment_source,
    );
    let cli_source = ConfigSource::transient(ConfigSourceKind::Cli, cwd);
    merge_layer(&mut values, &mut sources, &overlays.cli, &cli_source);

    EffectiveConfig {
        cwd: cwd.to_path_buf(),
        values,
        sources,
        project,
        global,
    }
}

macro_rules! merge_field {
    ($dst:expr, $sources:expr, $src:expr, $source:expr, $field:ident, $key:expr) => {
        if let Some(value) = $src.$field.as_ref() {
            $dst.$field = Some(value.clone());
            $sources.insert($key, $source.clone());
        }
    };
}

fn merge_layer(
    values: &mut ProjectConfig,
    sources: &mut BTreeMap<ConfigKey, ConfigSource>,
    layer: &ProjectConfig,
    source: &ConfigSource,
) {
    merge_field!(
        values,
        sources,
        layer,
        source,
        node_compat,
        ConfigKey::NodeCompat
    );
    merge_field!(values, sources, layer, source, preload, ConfigKey::Preload);
    merge_field!(
        values,
        sources,
        layer,
        source,
        node_options,
        ConfigKey::NodeOptions
    );
    merge_field!(values, sources, layer, source, v8_flags, ConfigKey::V8Flags);
    merge_field!(values, sources, layer, source, env, ConfigKey::Env);
    merge_field!(values, sources, layer, source, define, ConfigKey::Define);
    merge_field!(values, sources, layer, source, loader, ConfigKey::Loader);
    merge_field!(
        values,
        sources,
        layer,
        source,
        conditions,
        ConfigKey::Conditions
    );
    merge_field!(
        values,
        sources,
        layer,
        source,
        tsconfig,
        ConfigKey::Tsconfig
    );
    merge_field!(
        values,
        sources,
        layer,
        source,
        verify_deps_before_run,
        ConfigKey::VerifyDepsBeforeRun
    );
    merge_field!(values, sources, layer, source, sandbox, ConfigKey::Sandbox);

    merge_field!(
        values.install,
        sources,
        layer.install,
        source,
        node_linker,
        ConfigKey::InstallNodeLinker
    );
    merge_field!(
        values.install,
        sources,
        layer.install,
        source,
        symlink_disable_pattern,
        ConfigKey::InstallSymlinkDisablePattern
    );
    merge_field!(
        values.install,
        sources,
        layer.install,
        source,
        hoist,
        ConfigKey::InstallHoist
    );
    merge_field!(
        values.install,
        sources,
        layer.install,
        source,
        minimum_release_age,
        ConfigKey::InstallMinimumReleaseAge
    );
    merge_field!(
        values.install,
        sources,
        layer.install,
        source,
        minimum_release_age_exclude,
        ConfigKey::InstallMinimumReleaseAgeExclude
    );
    merge_field!(
        values.install,
        sources,
        layer.install,
        source,
        node_options,
        ConfigKey::InstallNodeOptions
    );
    merge_field!(
        values.install,
        sources,
        layer.install,
        source,
        sandbox,
        ConfigKey::InstallSandbox
    );
    merge_field!(
        values.dlx,
        sources,
        layer.dlx,
        source,
        consent,
        ConfigKey::DlxConsent
    );
    merge_field!(
        values.dlx,
        sources,
        layer.dlx,
        source,
        sandbox,
        ConfigKey::DlxSandbox
    );
    merge_field!(
        values.dlx,
        sources,
        layer.dlx,
        source,
        env,
        ConfigKey::DlxEnv
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Validation. Hand-walk the serde value so every object level can reject unknown
// keys (serde's `deny_unknown_fields` can't express the per-axis raw-value forms
// or the trichotomy, and loses the JSON-path for messages).
// ─────────────────────────────────────────────────────────────────────────────

/// Join a parent JSON path with a child key (`""` at the root ⇒ bare key).
fn child(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn as_object<'a>(v: &'a Value, path: &str) -> Result<&'a serde_json::Map<String, Value>> {
    v.as_object().ok_or_else(|| ConfigError::Type {
        path: if path.is_empty() {
            "<root>".into()
        } else {
            path.into()
        },
        expected: "an object",
    })
}

fn as_bool(v: &Value, path: &str) -> Result<bool> {
    v.as_bool().ok_or_else(|| ConfigError::Type {
        path: path.into(),
        expected: "a boolean",
    })
}

fn as_str<'a>(v: &'a Value, path: &str) -> Result<&'a str> {
    v.as_str().ok_or_else(|| ConfigError::Type {
        path: path.into(),
        expected: "a string",
    })
}

/// A `string[]` field — every element must be a string.
fn as_string_array(v: &Value, path: &str) -> Result<Vec<String>> {
    let arr = v.as_array().ok_or_else(|| ConfigError::Type {
        path: path.into(),
        expected: "an array of strings",
    })?;
    arr.iter()
        .map(|e| as_str(e, path).map(str::to_string))
        .collect()
}

/// A `{ string: string }` map (define, loader) — every value must be a string.
fn as_string_map(v: &Value, path: &str) -> Result<BTreeMap<String, String>> {
    let obj = as_object(v, path)?;
    obj.iter()
        .map(|(k, val)| Ok((k.clone(), as_str(val, &child(path, k))?.to_string())))
        .collect()
}

/// Per-extension overrides are intentionally limited to transform capabilities
/// the runtime already implements. New names are capability decisions, not a
/// permissive config-parser fallback.
fn validate_loader(v: &Value, path: &str) -> Result<BTreeMap<String, String>> {
    const LOADERS: &[&str] = &["text", "jsonc", "json5", "toml", "yaml", "ts", "tsx", "jsx"];
    let values = as_string_map(v, path)?;
    for (extension, loader) in &values {
        if !extension.starts_with('.') || extension.len() == 1 {
            return Err(ConfigError::Value {
                path: child(path, extension),
                message: "extension keys must start with `.` and include a suffix".into(),
            });
        }
        if !LOADERS.contains(&loader.as_str()) {
            return Err(ConfigError::Value {
                path: child(path, extension),
                message: format!(
                    "unsupported loader `{loader}`; expected text|jsonc|json5|toml|yaml|ts|tsx|jsx"
                ),
            });
        }
    }
    Ok(values)
}

/// Reject any key of `obj` not in `allowed` (fail-loud). The blessed `$schema`
/// key is tolerated at the root only (the caller adds it to `allowed` there).
fn reject_unknown_keys(
    obj: &serde_json::Map<String, Value>,
    path: &str,
    allowed: &[&str],
) -> Result<()> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ConfigError::UnknownKey {
                path: if path.is_empty() {
                    "<root>".into()
                } else {
                    path.into()
                },
                key: key.clone(),
            });
        }
    }
    Ok(())
}

fn validate_root(obj: &serde_json::Map<String, Value>) -> Result<ProjectConfig> {
    reject_unknown_keys(obj, "", ROOT_KEYS)?;

    // `$schema` is accepted + ignored, but still typed: the published schema
    // declares it a string, and fail-loud validation applies to it too.
    if let Some(v) = obj.get("$schema") {
        as_str(v, "$schema")?;
    }

    let mut cfg = ProjectConfig::default();
    if let Some(v) = obj.get("nodeCompat") {
        cfg.node_compat = Some(as_bool(v, "nodeCompat")?);
    }
    if let Some(v) = obj.get("preload") {
        cfg.preload = Some(as_string_array(v, "preload")?);
    }
    if let Some(v) = obj.get("nodeOptions") {
        cfg.node_options = Some(as_string_array(v, "nodeOptions")?);
    }
    if let Some(v) = obj.get("v8Flags") {
        cfg.v8_flags = Some(as_string_array(v, "v8Flags")?);
    }
    if let Some(v) = obj.get("env") {
        cfg.env = Some(validate_env_setting(v, "env")?);
    }
    if let Some(v) = obj.get("define") {
        cfg.define = Some(as_string_map(v, "define")?);
    }
    if let Some(v) = obj.get("loader") {
        cfg.loader = Some(validate_loader(v, "loader")?);
    }
    if let Some(v) = obj.get("conditions") {
        cfg.conditions = Some(as_string_array(v, "conditions")?);
    }
    if let Some(v) = obj.get("tsconfig") {
        cfg.tsconfig = Some(as_str(v, "tsconfig")?.to_string());
    }
    if let Some(v) = obj.get("verifyDepsBeforeRun") {
        cfg.verify_deps_before_run = Some(validate_verify_deps(v, "verifyDepsBeforeRun")?);
    }
    if let Some(v) = obj.get("sandbox") {
        cfg.sandbox = Some(validate_sandbox(v, "sandbox")?);
    }
    if let Some(v) = obj.get("install") {
        cfg.install = validate_install(v, "install")?;
    }
    if let Some(v) = obj.get("dlx") {
        cfg.dlx = validate_dlx(v, "dlx")?;
    }
    Ok(cfg)
}

/// `env` / `dlx.env`: `true` | `false` | string | string[].
fn validate_env_setting(v: &Value, path: &str) -> Result<EnvSetting> {
    match v {
        Value::Bool(true) => Ok(EnvSetting::Default),
        Value::Bool(false) => Ok(EnvSetting::Disabled),
        Value::String(s) => Ok(EnvSetting::Sources(vec![s.clone()])),
        Value::Array(_) => Ok(EnvSetting::Sources(as_string_array(v, path)?)),
        _ => Err(ConfigError::Type {
            path: path.into(),
            expected: "a boolean, string, or array of strings",
        }),
    }
}

fn validate_verify_deps(v: &Value, path: &str) -> Result<VerifyDepsBeforeRun> {
    match v {
        Value::Bool(b) => Ok(VerifyDepsBeforeRun::Enabled(*b)),
        Value::String(s) => match s.as_str() {
            "install" => Ok(VerifyDepsBeforeRun::Install),
            "warn" => Ok(VerifyDepsBeforeRun::Warn),
            "error" => Ok(VerifyDepsBeforeRun::Error),
            "prompt" => Ok(VerifyDepsBeforeRun::Prompt),
            other => Err(ConfigError::Value {
                path: path.into(),
                message: format!(
                    "unknown value `{other}` (expected \"install\", \"warn\", \"error\", or \"prompt\")"
                ),
            }),
        },
        _ => Err(ConfigError::Type {
            path: path.into(),
            expected: "a boolean or one of \"install\"/\"warn\"/\"error\"/\"prompt\"",
        }),
    }
}

fn validate_install(v: &Value, path: &str) -> Result<InstallConfig> {
    let obj = as_object(v, path)?;
    reject_unknown_keys(obj, path, INSTALL_KEYS)?;

    let mut install = InstallConfig::default();
    if let Some(v) = obj.get("nodeLinker") {
        let p = child(path, "nodeLinker");
        install.node_linker = Some(match as_str(v, &p)? {
            "symlink" => NodeLinker::Symlink,
            "isolated" => NodeLinker::Isolated,
            "hoisted" => NodeLinker::Hoisted,
            "pnp" => NodeLinker::Pnp,
            other => {
                return Err(ConfigError::Value {
                    path: p,
                    message: format!(
                        "unknown linker `{other}` (expected \"symlink\", \"isolated\", \"hoisted\", or \"pnp\")"
                    ),
                });
            }
        });
    }
    if let Some(v) = obj.get("symlinkDisablePattern") {
        install.symlink_disable_pattern =
            Some(as_string_array(v, &child(path, "symlinkDisablePattern"))?);
    }
    if let Some(v) = obj.get("hoist") {
        let p = child(path, "hoist");
        install.hoist = Some(match v {
            Value::Bool(b) => Hoist::Bool(*b),
            Value::Array(_) => Hoist::Patterns(as_string_array(v, &p)?),
            _ => {
                return Err(ConfigError::Type {
                    path: p,
                    expected: "a boolean or array of strings",
                });
            }
        });
    }
    if let Some(v) = obj.get("minimumReleaseAge") {
        install.minimum_release_age = Some(parse_duration(
            as_str(v, &child(path, "minimumReleaseAge"))?,
            &child(path, "minimumReleaseAge"),
        )?);
    }
    if let Some(v) = obj.get("minimumReleaseAgeExclude") {
        install.minimum_release_age_exclude = Some(as_string_array(
            v,
            &child(path, "minimumReleaseAgeExclude"),
        )?);
    }
    if let Some(v) = obj.get("nodeOptions") {
        install.node_options = Some(as_string_array(v, &child(path, "nodeOptions"))?);
    }
    if let Some(v) = obj.get("sandbox") {
        install.sandbox = Some(validate_sandbox(v, &child(path, "sandbox"))?);
    }
    Ok(install)
}

fn validate_dlx(v: &Value, path: &str) -> Result<DlxConfig> {
    let obj = as_object(v, path)?;
    reject_unknown_keys(obj, path, DLX_KEYS)?;

    let mut dlx = DlxConfig::default();
    if let Some(v) = obj.get("consent") {
        let p = child(path, "consent");
        let s = as_str(v, &p)?;
        dlx.consent = Some(ImplicitDlx::parse(s).ok_or_else(|| ConfigError::Value {
            path: p,
            message: format!("unknown value `{s}` (expected \"prompt\" or \"never\")"),
        })?);
    }
    if let Some(v) = obj.get("sandbox") {
        dlx.sandbox = Some(validate_sandbox(v, &child(path, "sandbox"))?);
    }
    if let Some(v) = obj.get("env") {
        dlx.env = Some(validate_env_setting(v, &child(path, "env"))?);
    }
    Ok(dlx)
}

/// The `sandbox` trichotomy skeleton. Shape only — the Phase-1 compiler resolves
/// presets/spread/grammar; here we just classify the wrapper and validate axis
/// forms so the schema is right.
fn validate_sandbox(v: &Value, path: &str) -> Result<SandboxSetting> {
    match v {
        Value::Bool(true) => Ok(SandboxSetting::Enabled),
        Value::Bool(false) => Ok(SandboxSetting::Disabled),
        Value::String(s) => Ok(classify_sandbox_string(s)),
        Value::Object(_) => Ok(SandboxSetting::Granular(validate_sandbox_axes(v, path)?)),
        _ => Err(ConfigError::Type {
            path: path.into(),
            expected: "a boolean, string (preset or \"./file.json\"), or object",
        }),
    }
}

/// Preset (bare identifier) vs file-ref (path-like) — the disambiguation rule
/// from the config spec. Unknown-preset validation is the compiler's job (it owns
/// the closed preset vocabulary), so the skeleton only classifies the string.
fn classify_sandbox_string(s: &str) -> SandboxSetting {
    let path_like = s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('/')
        || s.starts_with('~')
        || Path::new(s).extension().is_some();
    if path_like {
        SandboxSetting::FileRef(s.to_string())
    } else {
        SandboxSetting::Preset(s.to_string())
    }
}

fn validate_sandbox_axes(v: &Value, path: &str) -> Result<SandboxAxes> {
    let obj = as_object(v, path)?;
    reject_unknown_keys(obj, path, SANDBOX_AXIS_KEYS)?;

    let mut axes = SandboxAxes::default();
    if let Some(v) = obj.get("fs") {
        axes.fs = Some(validate_sandbox_axis(v, &child(path, "fs"))?);
    }
    if let Some(v) = obj.get("net") {
        axes.net = Some(validate_sandbox_axis(v, &child(path, "net"))?);
    }
    if let Some(v) = obj.get("env") {
        axes.env = Some(validate_sandbox_axis(v, &child(path, "env"))?);
    }
    Ok(axes)
}

fn validate_sandbox_axis(v: &Value, path: &str) -> Result<SandboxAxis> {
    match v {
        Value::Bool(b) => Ok(SandboxAxis::Bool(*b)),
        Value::Array(_) => Ok(SandboxAxis::Array(as_string_array(v, path)?)),
        Value::Object(obj) => Ok(SandboxAxis::Object(
            obj.iter()
                .map(|(k, val)| (k.clone(), val.clone()))
                .collect(),
        )),
        _ => Err(ConfigError::Type {
            path: path.into(),
            expected: "a boolean, array, or object",
        }),
    }
}

/// Parse the strict `minimumReleaseAge` duration grammar: `<integer><unit>`,
/// units `s|m|h|d|w` ONLY (no months/years — calendar ambiguity; `m` is
/// unambiguously minutes). A bare unit-less number is REJECTED (the npm-days vs
/// pnpm-minutes trap, made unrepresentable).
fn parse_duration(s: &str, path: &str) -> Result<Duration> {
    let invalid = |msg: &str| ConfigError::Value {
        path: path.into(),
        message: format!("invalid duration `{s}` — {msg}"),
    };
    let Some(unit) = s.chars().last() else {
        return Err(invalid("empty"));
    };
    let per_unit = match unit {
        's' => 1u64,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        'w' => 604_800,
        _ => {
            return Err(invalid(
                "expected an integer followed by a unit s|m|h|d|w (e.g. \"3d\")",
            ));
        }
    };
    let digits = &s[..s.len() - unit.len_utf8()];
    if digits.is_empty() {
        return Err(invalid("missing the integer amount"));
    }
    let amount: u64 = digits
        .parse()
        .map_err(|_| invalid("the amount must be a non-negative integer"))?;
    per_unit
        .checked_mul(amount)
        .map(Duration::from_secs)
        .ok_or_else(|| invalid("overflows"))
}

// P5/P6 test packages (separate files; children of this module so they reach
// the private resolver). `sandbox_shapes` also exports the shared five-shape
// table pm_engine's install-lowering test reuses.
#[cfg(test)]
#[path = "project_config_schema_matrix.rs"]
mod schema_matrix;

#[cfg(test)]
#[path = "project_config_precedence_matrix.rs"]
mod precedence_matrix;

#[cfg(test)]
#[path = "project_config_sandbox_shapes.rs"]
pub(crate) mod sandbox_shapes;

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> ProjectConfig {
        parse_project_config(text).expect("valid config")
    }

    #[test]
    fn empty_and_comment_only_files_are_valid_empty_configs() {
        assert_eq!(parse(""), ProjectConfig::default());
        assert_eq!(parse("// just a comment\n"), ProjectConfig::default());
        assert_eq!(parse("{}"), ProjectConfig::default());
    }

    #[test]
    fn schema_key_is_accepted_and_ignored() {
        let cfg = parse("{ \"$schema\": \"https://nubjs.com/schema/nub.json\" }");
        assert_eq!(cfg, ProjectConfig::default());
    }

    #[test]
    fn unknown_top_level_key_fails_loud() {
        let err = parse_project_config("{ \"nodeComapt\": true }").unwrap_err();
        match err {
            ConfigError::UnknownKey { key, .. } => assert_eq!(key, "nodeComapt"),
            other => panic!("expected UnknownKey, got {other:?}"),
        }
    }

    #[test]
    fn unknown_nested_key_reports_its_path() {
        let err =
            parse_project_config("{ \"install\": { \"nodeLinkr\": \"symlink\" } }").unwrap_err();
        match err {
            ConfigError::UnknownKey { path, key } => {
                assert_eq!(path, "install");
                assert_eq!(key, "nodeLinkr");
            }
            other => panic!("expected UnknownKey, got {other:?}"),
        }
    }

    #[test]
    fn malformed_jsonc_is_a_parse_error_not_a_degrade() {
        assert!(matches!(
            parse_project_config("{ this is not json"),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn runtime_top_levels_parse() {
        let cfg = parse(
            r#"{
              // jsonc: comments + trailing commas
              "nodeCompat": true,
              "preload": ["./telemetry.ts"],
              "nodeOptions": ["--max-old-space-size=4096"],
              "v8Flags": ["--expose-gc"],
              "define": { "__DEV__": "false" },
              "loader": { ".svg": "text" },
              "conditions": ["worker"],
              "tsconfig": "./tsconfig.runtime.json",
            }"#,
        );
        assert_eq!(cfg.node_compat, Some(true));
        assert_eq!(cfg.preload, Some(vec!["./telemetry.ts".into()]));
        assert_eq!(
            cfg.node_options,
            Some(vec!["--max-old-space-size=4096".into()])
        );
        assert_eq!(cfg.v8_flags, Some(vec!["--expose-gc".into()]));
        assert_eq!(
            cfg.define
                .as_ref()
                .and_then(|values| values.get("__DEV__"))
                .map(String::as_str),
            Some("false")
        );
        assert_eq!(
            cfg.loader
                .as_ref()
                .and_then(|values| values.get(".svg"))
                .map(String::as_str),
            Some("text")
        );
        assert_eq!(cfg.conditions, Some(vec!["worker".into()]));
        assert_eq!(cfg.tsconfig.as_deref(), Some("./tsconfig.runtime.json"));
    }

    #[test]
    fn loader_accepts_only_settled_runtime_capabilities() {
        for loader in ["text", "jsonc", "json5", "toml", "yaml", "ts", "tsx", "jsx"] {
            parse_project_config(&format!(r#"{{ "loader": {{ ".asset": "{loader}" }} }}"#))
                .unwrap();
        }
        for loader in ["file", "css", "wasm", "html", "json", "js"] {
            let error =
                parse_project_config(&format!(r#"{{ "loader": {{ ".asset": "{loader}" }} }}"#))
                    .unwrap_err();
            assert!(
                matches!(error, ConfigError::Value { ref path, .. } if path == "loader..asset"),
                "{loader}: {error}"
            );
        }
        assert!(matches!(
            parse_project_config(r#"{ "loader": { "asset": "text" } }"#),
            Err(ConfigError::Value { .. })
        ));
    }

    #[test]
    fn runtime_snapshot_anchors_paths_to_the_winning_source() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        let cwd = project_root.join("packages/app");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(project_root.join("runtime.jsonc"), "{}").unwrap();
        let project = LoadedConfig {
            source: ConfigSource::file(ConfigSourceKind::Project, &project_root.join(FILE_NAME)),
            values: ProjectConfig {
                preload: Some(vec!["./preload.mjs".into(), "bare-package".into()]),
                env: Some(EnvSetting::Sources(vec!["./custom.env".into()])),
                tsconfig: Some("./runtime.jsonc".into()),
                ..ProjectConfig::default()
            },
        };
        let snapshot = resolve_effective_config(
            &cwd,
            None,
            Some(project),
            ConfigOverlays {
                defaults: ProjectConfig::builtin_defaults(),
                ..ConfigOverlays::default()
            },
        )
        .runtime_config()
        .unwrap();
        assert_eq!(
            snapshot.preload,
            vec![
                project_root
                    .join("./preload.mjs")
                    .to_string_lossy()
                    .into_owned(),
                "bare-package".to_string()
            ]
        );
        assert_eq!(
            snapshot.env,
            RuntimeEnv::Sources(vec![project_root.join("./custom.env")])
        );
        assert_eq!(
            snapshot.tsconfig.as_deref(),
            Some(
                project_root
                    .join("./runtime.jsonc")
                    .to_string_lossy()
                    .as_ref()
            )
        );
    }

    #[test]
    fn env_tristate_covers_all_arms() {
        assert_eq!(parse(r#"{ "env": true }"#).env, Some(EnvSetting::Default));
        assert_eq!(parse(r#"{ "env": false }"#).env, Some(EnvSetting::Disabled));
        assert_eq!(
            parse(r#"{ "env": ".env.local" }"#).env,
            Some(EnvSetting::Sources(vec![".env.local".into()]))
        );
        assert_eq!(
            parse(r#"{ "env": [".env", ".env.local"] }"#).env,
            Some(EnvSetting::Sources(vec![
                ".env".into(),
                ".env.local".into()
            ]))
        );
    }

    #[test]
    fn wrong_type_reports_the_field_and_expectation() {
        let err = parse_project_config(r#"{ "preload": "single" }"#).unwrap_err();
        match err {
            ConfigError::Type { path, expected } => {
                assert_eq!(path, "preload");
                assert_eq!(expected, "an array of strings");
            }
            other => panic!("expected Type error, got {other:?}"),
        }
    }

    #[test]
    fn define_rejects_non_string_values() {
        let err = parse_project_config(r#"{ "define": { "__DEV__": false } }"#).unwrap_err();
        match err {
            ConfigError::Type { path, expected } => {
                assert_eq!(path, "define.__DEV__");
                assert_eq!(expected, "a string");
            }
            other => panic!("expected Type error, got {other:?}"),
        }
    }

    #[test]
    fn verify_deps_covers_bool_and_string_arms() {
        assert_eq!(
            parse(r#"{ "verifyDepsBeforeRun": true }"#).verify_deps_before_run,
            Some(VerifyDepsBeforeRun::Enabled(true))
        );
        assert_eq!(
            parse(r#"{ "verifyDepsBeforeRun": "warn" }"#).verify_deps_before_run,
            Some(VerifyDepsBeforeRun::Warn)
        );
        assert!(matches!(
            parse_project_config(r#"{ "verifyDepsBeforeRun": "yes" }"#),
            Err(ConfigError::Value { .. })
        ));
    }

    #[test]
    fn install_block_parses_and_validates() {
        let cfg = parse(
            r#"{
              "install": {
                "nodeLinker": "isolated",
                "symlinkDisablePattern": ["@corp/tool-*"],
                "hoist": ["*types*"],
                "minimumReleaseAge": "3d",
                "minimumReleaseAgeExclude": ["@myorg/*"],
                "nodeOptions": ["--max-old-space-size=2048"]
              }
            }"#,
        );
        assert_eq!(cfg.install.node_linker, Some(NodeLinker::Isolated));
        assert_eq!(
            cfg.install.symlink_disable_pattern,
            Some(vec!["@corp/tool-*".into()])
        );
        assert_eq!(
            cfg.install.hoist,
            Some(Hoist::Patterns(vec!["*types*".into()]))
        );
        assert_eq!(
            cfg.install.minimum_release_age,
            Some(Duration::from_secs(3 * 86_400))
        );
        assert_eq!(
            cfg.install.minimum_release_age_exclude,
            Some(vec!["@myorg/*".into()])
        );
        assert_eq!(
            cfg.install.node_options,
            Some(vec!["--max-old-space-size=2048".into()])
        );
    }

    #[test]
    fn node_linker_rejects_unknown_value() {
        let err =
            parse_project_config(r#"{ "install": { "nodeLinker": "hardlink" } }"#).unwrap_err();
        match err {
            ConfigError::Value { path, .. } => assert_eq!(path, "install.nodeLinker"),
            other => panic!("expected Value error, got {other:?}"),
        }
    }

    #[test]
    fn hoist_bool_and_array_forms() {
        assert_eq!(
            parse(r#"{ "install": { "hoist": false } }"#).install.hoist,
            Some(Hoist::Bool(false))
        );
        assert_eq!(
            parse(r#"{ "install": { "hoist": ["a", "b"] } }"#)
                .install
                .hoist,
            Some(Hoist::Patterns(vec!["a".into(), "b".into()]))
        );
    }

    #[test]
    fn minimum_release_age_grammar() {
        let units = [
            ("30s", 30u64),
            ("5m", 300),
            ("2h", 7200),
            ("3d", 259_200),
            ("1w", 604_800),
        ];
        for (input, secs) in units {
            let cfg = parse(&format!(
                r#"{{ "install": {{ "minimumReleaseAge": "{input}" }} }}"#
            ));
            assert_eq!(
                cfg.install.minimum_release_age,
                Some(Duration::from_secs(secs)),
                "{input}"
            );
        }
        // Bare unit-less numbers are the days-vs-minutes trap — rejected.
        for bad in ["3", "3y", "d", "-1d", "3 d"] {
            assert!(
                matches!(
                    parse_project_config(&format!(
                        r#"{{ "install": {{ "minimumReleaseAge": "{bad}" }} }}"#
                    )),
                    Err(ConfigError::Value { .. })
                ),
                "expected `{bad}` to be rejected"
            );
        }
    }

    #[test]
    fn dlx_block_parses() {
        let cfg = parse(
            r#"{
              "dlx": {
                "consent": "never",
                "sandbox": { "net": ["registry.npmjs.org"] },
                "env": false
              }
            }"#,
        );
        assert_eq!(cfg.dlx.consent, Some(ImplicitDlx::Never));
        assert_eq!(cfg.dlx.env, Some(EnvSetting::Disabled));
        assert!(matches!(cfg.dlx.sandbox, Some(SandboxSetting::Granular(_))));
    }

    #[test]
    fn dlx_consent_rejects_unknown_value() {
        assert!(matches!(
            parse_project_config(r#"{ "dlx": { "consent": "always" } }"#),
            Err(ConfigError::Value { .. })
        ));
    }

    #[test]
    fn sandbox_trichotomy_classifies_every_form() {
        assert_eq!(
            parse(r#"{ "sandbox": false }"#).sandbox,
            Some(SandboxSetting::Disabled)
        );
        assert_eq!(
            parse(r#"{ "sandbox": true }"#).sandbox,
            Some(SandboxSetting::Enabled)
        );
        assert_eq!(
            parse(r#"{ "sandbox": "build-jail" }"#).sandbox,
            Some(SandboxSetting::Preset("build-jail".into()))
        );
        assert_eq!(
            parse(r#"{ "sandbox": "./policy.json" }"#).sandbox,
            Some(SandboxSetting::FileRef("./policy.json".into()))
        );
        assert!(matches!(
            parse(r#"{ "sandbox": { "fs": {} } }"#).sandbox,
            Some(SandboxSetting::Granular(_))
        ));
    }

    #[test]
    fn sandbox_five_shapes_are_lossless_and_inert_in_the_snapshot() {
        let granular = serde_json::json!({
            "fs": { "./data": { "read": true, "write": false } },
            "net": [],
            "env": false
        });
        let cases = vec![
            ("false".to_string(), SandboxSetting::Disabled),
            ("true".to_string(), SandboxSetting::Enabled),
            (
                r#""build-jail""#.to_string(),
                SandboxSetting::Preset("build-jail".into()),
            ),
            (
                r#""./team.json""#.to_string(),
                SandboxSetting::FileRef("./team.json".into()),
            ),
            (
                granular.to_string(),
                SandboxSetting::Granular(SandboxAxes {
                    fs: Some(SandboxAxis::Object(BTreeMap::from([(
                        "./data".into(),
                        serde_json::json!({ "read": true, "write": false }),
                    )]))),
                    net: Some(SandboxAxis::Array(Vec::new())),
                    env: Some(SandboxAxis::Bool(false)),
                }),
            ),
        ];

        for (raw, expected) in cases {
            let parsed = parse(&format!(r#"{{ "sandbox": {raw} }}"#));
            assert_eq!(parsed.sandbox, Some(expected.clone()));
            let snapshot = resolve_effective_config(
                Path::new("/effective"),
                None,
                Some(LoadedConfig {
                    source: ConfigSource::file(
                        ConfigSourceKind::Project,
                        Path::new("/project/nub.jsonc"),
                    ),
                    values: parsed,
                }),
                ConfigOverlays::default(),
            );
            assert_eq!(snapshot.values.sandbox, Some(expected));
        }
    }

    #[test]
    fn sandbox_axes_accept_array_and_object_forms() {
        let cfg = parse(
            r#"{
              "sandbox": {
                "env": ["NODE_ENV", "VITE_*", "!*_TOKEN"],
                "fs": { "./data": "rw", "~/.ssh": false },
                "net": { "*.sentry.io": true, "*": false }
              }
            }"#,
        );
        let Some(SandboxSetting::Granular(axes)) = cfg.sandbox else {
            panic!("expected granular sandbox");
        };
        assert_eq!(
            axes.env,
            Some(SandboxAxis::Array(vec![
                "NODE_ENV".into(),
                "VITE_*".into(),
                "!*_TOKEN".into()
            ]))
        );
        assert!(matches!(axes.fs, Some(SandboxAxis::Object(_))));
        assert!(matches!(axes.net, Some(SandboxAxis::Object(_))));
    }

    #[test]
    fn sandbox_axis_array_rejects_non_strings() {
        let err = parse_project_config(r#"{ "sandbox": { "env": ["OK", 5] } }"#).unwrap_err();
        assert!(matches!(err, ConfigError::Type { .. }));
    }

    #[test]
    fn unknown_sandbox_axis_fails_loud() {
        let err = parse_project_config(r#"{ "sandbox": { "disk": true } }"#).unwrap_err();
        match err {
            ConfigError::UnknownKey { path, key } => {
                assert_eq!(path, "sandbox");
                assert_eq!(key, "disk");
            }
            other => panic!("expected UnknownKey, got {other:?}"),
        }
    }

    #[test]
    fn hostile_input_never_panics() {
        // A malicious/broken project file must degrade to Ok/Err, never panic or
        // stack-overflow. Deep nesting, huge arrays, duplicate keys, embedded NUL.
        let deep = format!("{}1{}", "[".repeat(2000), "]".repeat(2000));
        let _ = parse_project_config(&deep); // deep-nesting: no overflow

        let dup = parse_project_config(r#"{ "nodeCompat": true, "nodeCompat": false }"#);
        assert_eq!(dup.unwrap().node_compat, Some(false), "last duplicate wins");

        let huge = format!(r#"{{ "preload": [{}] }}"#, "\"x\",".repeat(5000));
        assert!(parse_project_config(&huge).is_ok(), "huge array parses");

        // Embedded NUL / control chars in a string value → parses (Type-valid) or
        // Parse-errors, never panics.
        let _ = parse_project_config("{ \"tsconfig\": \"a\\u0000b\" }");
    }

    #[test]
    fn published_schema_exposes_only_active_parser_keys() {
        fn keys(value: &Value, pointer: &str) -> std::collections::BTreeSet<String> {
            value
                .pointer(pointer)
                .and_then(Value::as_object)
                .unwrap_or_else(|| panic!("schema object missing at {pointer}"))
                .keys()
                .cloned()
                .collect()
        }
        fn active(values: &[&str]) -> std::collections::BTreeSet<String> {
            values
                .iter()
                .filter(|value| **value != "sandbox")
                .map(|value| (*value).to_string())
                .collect()
        }

        let schema: Value =
            serde_json::from_str(include_str!("../../../site/public/schema/nub.json"))
                .expect("the published nub.json schema must be valid JSON");
        assert_eq!(keys(&schema, "/properties"), active(ROOT_KEYS));
        assert_eq!(
            keys(&schema, "/properties/install/properties"),
            active(INSTALL_KEYS)
        );
        assert_eq!(
            keys(&schema, "/properties/dlx/properties"),
            active(DLX_KEYS)
        );
        assert!(schema.pointer("/$defs/sandboxSetting").is_none());
        assert_eq!(
            schema.get("$id").and_then(Value::as_str),
            Some("https://nubjs.com/schema/nub.json")
        );
    }

    // ── discovery ──

    #[test]
    fn discover_walks_up_tree_to_first_nub_jsonc() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(FILE_NAME), "{}").unwrap();
        let app = root.join("packages").join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join(FILE_NAME), r#"{ "nodeCompat": false }"#).unwrap();
        let deep = app.join("src");
        std::fs::create_dir_all(&deep).unwrap();

        let found = discover_project_config(&deep).expect("walks up to the nearest file");
        assert_eq!(found, app.join(FILE_NAME));
    }

    #[test]
    fn discover_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(discover_project_config(dir.path()), None);
    }

    #[test]
    fn load_reads_a_present_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), r#"{ "nodeCompat": true }"#).unwrap();
        let loaded = load_project_config(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.values.node_compat, Some(true));
    }

    #[test]
    fn load_surfaces_a_malformed_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "{ broken").unwrap();
        assert!(matches!(
            load_project_config(dir.path()),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn dlx_consent_uses_typed_winner_then_legacy_default_fallback() {
        let defaults = resolve_effective_config(
            Path::new("/cwd"),
            None,
            None,
            ConfigOverlays {
                defaults: ProjectConfig::builtin_defaults(),
                ..ConfigOverlays::default()
            },
        );
        assert_eq!(
            dlx_consent_for(&defaults, ImplicitDlx::Never),
            ImplicitDlx::Never
        );

        let project = LoadedConfig {
            source: ConfigSource::file(ConfigSourceKind::Project, Path::new("/project/nub.jsonc")),
            values: ProjectConfig {
                dlx: DlxConfig {
                    consent: Some(ImplicitDlx::Prompt),
                    ..DlxConfig::default()
                },
                ..ProjectConfig::default()
            },
        };
        let snapshot = resolve_effective_config(
            Path::new("/cwd"),
            None,
            Some(project),
            ConfigOverlays {
                defaults: ProjectConfig::builtin_defaults(),
                ..ConfigOverlays::default()
            },
        );
        assert_eq!(
            dlx_consent_for(&snapshot, ImplicitDlx::Never),
            ImplicitDlx::Prompt
        );
    }

    #[test]
    fn project_dlx_env_sources_anchor_to_the_winning_file() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        let cwd = project_root.join("packages/app");
        std::fs::create_dir_all(project_root.join("config")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            project_root.join(FILE_NAME),
            r#"{ "dlx": { "env": ["./config/first.env", "./config/second.env"] } }"#,
        )
        .unwrap();
        std::fs::write(
            project_root.join("config/first.env"),
            "DLX_VALUE=first\nCHAIN=$BASE\n",
        )
        .unwrap();
        std::fs::write(
            project_root.join("config/second.env"),
            "DLX_VALUE=second\nBASE=expanded\n",
        )
        .unwrap();

        let project = load_project_config(&cwd).unwrap();
        let snapshot = resolve_effective_config(&cwd, None, project, ConfigOverlays::default());
        let values = dlx_env_for(&snapshot).expect("project dlx env is active");
        assert_eq!(values.get("DLX_VALUE").map(String::as_str), Some("second"));
        assert_eq!(values.get("CHAIN").map(String::as_str), Some("expanded"));
        assert_eq!(
            snapshot.sources.get(&ConfigKey::DlxEnv).unwrap().root,
            project_root
        );
    }

    #[test]
    fn dlx_env_disabled_empty_and_sandbox_only_are_inert() {
        for (env, expected) in [
            (Some(EnvSetting::Disabled), Some(BTreeMap::new())),
            (Some(EnvSetting::Sources(Vec::new())), Some(BTreeMap::new())),
            (None, None),
        ] {
            let snapshot = resolve_effective_config(
                Path::new("/cwd"),
                None,
                Some(LoadedConfig {
                    source: ConfigSource::file(
                        ConfigSourceKind::Project,
                        Path::new("/project/nub.jsonc"),
                    ),
                    values: ProjectConfig {
                        dlx: DlxConfig {
                            env,
                            sandbox: Some(SandboxSetting::Enabled),
                            ..DlxConfig::default()
                        },
                        ..ProjectConfig::default()
                    },
                }),
                ConfigOverlays::default(),
            );
            assert_eq!(dlx_env_for(&snapshot), expected);
        }
    }

    #[test]
    fn reads_the_discovered_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), r#"{ "nodeCompat": true }"#).unwrap();
        let cfg = load_project_config(dir.path()).unwrap();
        assert_eq!(cfg.unwrap().values.node_compat, Some(true));
    }

    #[test]
    fn loaded_config_retains_exact_source_path_and_containing_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let deep = root.join("packages/app");
        std::fs::create_dir_all(&deep).unwrap();
        let path = root.join(FILE_NAME);
        std::fs::write(&path, r#"{ "tsconfig": "./tsconfig.runtime.json" }"#).unwrap();

        let loaded = load_project_config(&deep).unwrap().unwrap();
        assert_eq!(loaded.source.kind, ConfigSourceKind::Project);
        assert_eq!(loaded.source.path.as_deref(), Some(path.as_path()));
        assert_eq!(loaded.source.root, root);
        assert_eq!(
            loaded.values.tsconfig.as_deref(),
            Some("./tsconfig.runtime.json")
        );
    }

    #[test]
    fn effective_cwd_drives_discovery_not_the_ambient_parent() {
        let dir = tempfile::tempdir().unwrap();
        let ambient = dir.path().join("ambient");
        let effective = dir.path().join("requested").join("child");
        std::fs::create_dir_all(&ambient).unwrap();
        std::fs::create_dir_all(&effective).unwrap();
        std::fs::write(
            dir.path().join("requested").join(FILE_NAME),
            r#"{ "preload": ["./from-requested.ts"] }"#,
        )
        .unwrap();
        std::fs::write(
            ambient.join(FILE_NAME),
            r#"{ "preload": ["./from-ambient.ts"] }"#,
        )
        .unwrap();

        let project = load_project_config(&effective).unwrap();
        let snapshot =
            resolve_effective_config(&effective, None, project, ConfigOverlays::default());
        assert_eq!(
            snapshot.values.preload,
            Some(vec!["./from-requested.ts".into()])
        );
        assert_eq!(snapshot.cwd, effective);
        assert_eq!(
            snapshot.sources.get(&ConfigKey::Preload).unwrap().root,
            dir.path().join("requested")
        );
    }

    #[test]
    fn precedence_is_cli_then_environment_then_project_then_global_then_defaults() {
        fn layer(kind: ConfigSourceKind, name: &str) -> LoadedConfig {
            LoadedConfig {
                source: ConfigSource::file(kind, Path::new(&format!("/{name}/nub.jsonc"))),
                values: ProjectConfig {
                    tsconfig: Some(name.to_string()),
                    ..ProjectConfig::default()
                },
            }
        }

        let defaults = ProjectConfig {
            tsconfig: Some("defaults".into()),
            ..ProjectConfig::default()
        };
        let global = Some(layer(ConfigSourceKind::Global, "global"));
        let project = Some(layer(ConfigSourceKind::Project, "project"));
        let environment = ProjectConfig {
            tsconfig: Some("environment".into()),
            ..ProjectConfig::default()
        };
        let cli = ProjectConfig {
            tsconfig: Some("cli".into()),
            ..ProjectConfig::default()
        };

        let snapshots = [
            (
                ConfigOverlays {
                    cli: cli.clone(),
                    environment: environment.clone(),
                    defaults: defaults.clone(),
                },
                global.clone(),
                project.clone(),
                "cli",
                ConfigSourceKind::Cli,
            ),
            (
                ConfigOverlays {
                    environment: environment.clone(),
                    defaults: defaults.clone(),
                    ..ConfigOverlays::default()
                },
                global.clone(),
                project.clone(),
                "environment",
                ConfigSourceKind::Environment,
            ),
            (
                ConfigOverlays {
                    defaults: defaults.clone(),
                    ..ConfigOverlays::default()
                },
                global.clone(),
                project.clone(),
                "project",
                ConfigSourceKind::Project,
            ),
            (
                ConfigOverlays {
                    defaults: defaults.clone(),
                    ..ConfigOverlays::default()
                },
                global.clone(),
                None,
                "global",
                ConfigSourceKind::Global,
            ),
            (
                ConfigOverlays {
                    defaults,
                    ..ConfigOverlays::default()
                },
                None,
                None,
                "defaults",
                ConfigSourceKind::Defaults,
            ),
        ];

        for (overlays, global, project, expected, source) in snapshots {
            let snapshot = resolve_effective_config(Path::new("/cwd"), global, project, overlays);
            assert_eq!(snapshot.values.tsconfig.as_deref(), Some(expected));
            assert_eq!(
                snapshot.sources.get(&ConfigKey::Tsconfig).unwrap().kind,
                source
            );
        }
    }

    #[test]
    fn explicit_false_and_empty_values_override_lower_layers() {
        let project = LoadedConfig {
            source: ConfigSource::file(ConfigSourceKind::Project, Path::new("/project/nub.jsonc")),
            values: ProjectConfig {
                node_compat: Some(true),
                preload: Some(vec!["./project.ts".into()]),
                define: Some(BTreeMap::from([("PROJECT".into(), "true".into())])),
                ..ProjectConfig::default()
            },
        };
        let snapshot = resolve_effective_config(
            Path::new("/cwd"),
            None,
            Some(project),
            ConfigOverlays {
                cli: ProjectConfig {
                    node_compat: Some(false),
                    preload: Some(Vec::new()),
                    define: Some(BTreeMap::new()),
                    ..ProjectConfig::default()
                },
                ..ConfigOverlays::default()
            },
        );

        assert_eq!(snapshot.values.node_compat, Some(false));
        assert_eq!(snapshot.values.preload, Some(Vec::new()));
        assert_eq!(snapshot.values.define, Some(BTreeMap::new()));
        for key in [ConfigKey::NodeCompat, ConfigKey::Preload, ConfigKey::Define] {
            assert_eq!(
                snapshot.sources.get(&key).unwrap().kind,
                ConfigSourceKind::Cli
            );
        }
    }

    #[test]
    fn shared_schema_keysets_match_the_parser() {
        assert_eq!(
            ROOT_KEYS,
            [
                "$schema",
                "nodeCompat",
                "preload",
                "nodeOptions",
                "v8Flags",
                "env",
                "define",
                "loader",
                "conditions",
                "tsconfig",
                "verifyDepsBeforeRun",
                "sandbox",
                "install",
                "dlx",
            ]
        );
        let text = r#"{
          "$schema": "https://nubjs.com/schema/nub.json",
          "nodeCompat": false,
          "preload": [],
          "nodeOptions": [],
          "v8Flags": [],
          "env": false,
          "define": {},
          "loader": {},
          "conditions": [],
          "tsconfig": "./tsconfig.json",
          "verifyDepsBeforeRun": false,
          "sandbox": false,
          "install": {
            "nodeLinker": "symlink",
            "symlinkDisablePattern": [],
            "hoist": false,
            "minimumReleaseAge": "1d",
            "minimumReleaseAgeExclude": [],
            "nodeOptions": [],
            "sandbox": false
          },
          "dlx": { "consent": "prompt", "sandbox": false, "env": false }
        }"#;
        let dir = tempfile::tempdir().unwrap();
        let project_path = dir.path().join("project.jsonc");
        let global_path = dir.path().join("global.jsonc");
        std::fs::write(&project_path, text).unwrap();
        std::fs::write(&global_path, text).unwrap();
        let project = read_project_config_at(&project_path).unwrap();
        let global = read_global_config_at(&global_path).unwrap();
        assert_eq!(project.values, global.values);
        assert_eq!(project.source.kind, ConfigSourceKind::Project);
        assert_eq!(global.source.kind, ConfigSourceKind::Global);
    }

    #[test]
    fn repeated_loads_keep_discovery_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, r#"{ "nodeCompat": true }"#).unwrap();

        let first = load_project_config(dir.path()).unwrap();
        assert_eq!(first.unwrap().values.node_compat, Some(true));
        let second = load_project_config(dir.path()).unwrap();
        assert_eq!(second.unwrap().values.node_compat, Some(true));

        std::fs::write(&path, "{ malformed").unwrap();
        assert!(matches!(
            load_project_config(dir.path()),
            Err(ConfigError::Parse(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_project_file_fails_loud() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, r#"{ "nodeCompat": true }"#).unwrap();
        let original = std::fs::metadata(&path).unwrap().permissions();
        let mut unreadable = original.clone();
        unreadable.set_mode(0o000);
        std::fs::set_permissions(&path, unreadable).unwrap();

        assert!(matches!(
            load_project_config(dir.path()),
            Err(ConfigError::Io(_))
        ));

        std::fs::set_permissions(&path, original).unwrap();
    }

    #[test]
    fn malformed_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "{ broken").unwrap();
        let result = load_project_config(dir.path());
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn no_file_is_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_project_config(dir.path()).unwrap();
        assert_eq!(cfg, None);
    }
}
