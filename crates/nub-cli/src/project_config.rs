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
//! The two files share one published schema, which carries every key either
//! accepts. [`GLOBAL_ONLY_KEYS`] names the sections only the global file may
//! carry — see [`ConfigScope`] for why — and that scope is enforced here, not
//! by the schema, so an editor still completes and validates both files.
//!
//! Project files are discovered from the invocation's final working directory.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::config::ImplicitDlx;

/// The filename discovered up-tree. `nub.json` is NEVER accepted (no dual-name
/// ambiguity) — one name, matching the global file.
pub(crate) const FILE_NAME: &str = "nub.jsonc";

pub(crate) const ROOT_KEYS: &[&str] = &[
    "$schema",
    "nodeCompat",
    "preload",
    "nodeOptions",
    "v8Flags",
    "envFile",
    "loader",
    "conditions",
    "tsconfig",
    "jsx",
    "jsxFactory",
    "jsxFragmentFactory",
    "jsxImportSource",
    "decorators",
    "emitDecoratorMetadata",
    "verifyDeps",
    "install",
    "dlx",
];
pub(crate) const INSTALL_KEYS: &[&str] = &[
    "linker",
    "publicHoist",
    "minimumReleaseAge",
    "minimumReleaseAgeExclude",
];
pub(crate) const DLX_KEYS: &[&str] = &["consent"];

/// Every loader a `loader` entry may name. At module scope rather than inside
/// [`validate_loader`] so `published_schema_exposes_every_parser_key` can pin
/// the published schema against it: the vocabulary appears three times per
/// schema file (`additionalProperties`, plus the `.tsx` and `.jsx` overrides
/// that drop `ts`), and a function-local const left all of them uncomparable.
pub(crate) const LOADERS: &[&str] = &["text", "jsonc", "json5", "toml", "yaml", "ts", "tsx", "jsx"];

/// The extensions whose JSX dialect comes from the extension itself, so a `ts`
/// loader on them could not take effect and is refused. Their schema enums are
/// therefore [`LOADERS`] minus `ts`.
pub(crate) const JSX_PINNED_EXTS: &[&str] = &[".tsx", ".jsx"];

/// The `compilerOptions.jsx` modes that produce executable JavaScript in Nub's
/// per-file runtime transform. TypeScript's `preserve` / `react-native` modes
/// intentionally leave JSX syntax in the output, so the native runtime surface
/// refuses them rather than accepting a setting it cannot apply as written.
pub(crate) const JSX_VALUES: &[&str] = &["react", "react-jsx", "react-jsxdev"];

/// Decorator semantics Nub can currently lower. Standard ECMAScript decorators
/// will join this vocabulary once the native transform can emit them.
pub(crate) const DECORATOR_VALUES: &[&str] = &["legacy"];

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
    /// A recognized root section a project file may not carry — distinct from
    /// [`ConfigError::UnknownKey`] so the author is not sent hunting for a
    /// misspelling that isn't there. See [`GLOBAL_ONLY_KEYS`].
    GlobalOnlyKey { key: String },
    /// The value at `path` has the wrong JSON type.
    Type {
        path: String,
        expected: &'static str,
    },
    /// The value at `path` is the right type but semantically invalid.
    Value { path: String, message: String },
    /// A failure attributed to the file it was read from. The readers wrap what
    /// they return in this; the parsers, which take text and know no path, do
    /// not. Kept a wrapper rather than a field on every variant so the ~30
    /// construction sites stay path-free.
    InFile {
        path: PathBuf,
        source: Box<ConfigError>,
    },
}

impl ConfigError {
    /// Attribute a failure to the file it came from. Discovery walks the ancestor
    /// chain unbounded, so the offending file can sit arbitrarily far above the
    /// cwd — a message naming only `nub.jsonc` sends the author hunting.
    pub(crate) fn in_file(self, path: &Path) -> Self {
        ConfigError::InFile {
            path: path.to_path_buf(),
            source: Box::new(self),
        }
    }

    /// The failure beneath the file attribution, so a caller branching on the
    /// kind does not have to know whether a reader wrapped it.
    ///
    /// Test-only, and that is the honest state rather than an oversight: no
    /// production path branches on a `ConfigError` variant. Every one either
    /// renders it (`write_naming`, which unwraps the attribution itself) or
    /// propagates it. Assertions need this because only the READERS wrap —
    /// errors from `parse_project_config` and the `validate_*` helpers arrive
    /// bare, so a test matching a variant must not care which it got.
    #[cfg(test)]
    pub fn kind(&self) -> &ConfigError {
        match self {
            ConfigError::InFile { source, .. } => source.kind(),
            other => other,
        }
    }

    /// Render naming `file` — the reader's absolute path when the failure is
    /// attributed, the bare filename when a parser was called without one.
    fn write_naming(&self, f: &mut fmt::Formatter<'_>, file: &dyn fmt::Display) -> fmt::Result {
        match self {
            ConfigError::InFile { path, source } => source.write_naming(f, &path.display()),
            ConfigError::Io(e) => write!(f, "reading {file}: {e}"),
            ConfigError::Parse(m) => write!(f, "parsing {file}: {m}"),
            ConfigError::UnknownKey { path, key } => {
                write!(f, "unknown key `{key}` in {path} of {file}")
            }
            ConfigError::GlobalOnlyKey { key } => {
                // Name the destination concretely — the whole failure mode is an
                // author who wrote a valid section into the wrong file. The path
                // honors `XDG_CONFIG_HOME` and the Windows home, so resolve it
                // rather than printing a `~/.config` that may not be theirs.
                match crate::config::config_path() {
                    Some(path) => write!(
                        f,
                        "`{key}` in {file} is configured globally: move it to {}. \
                         Settings that configure your machine are not read from a checkout.",
                        path.display()
                    ),
                    None => write!(
                        f,
                        "`{key}` in {file} is configured globally: move it to the global {FILE_NAME}. \
                         Settings that configure your machine are not read from a checkout."
                    ),
                }
            }
            ConfigError::Type { path, expected } => {
                write!(f, "`{path}` in {file} must be {expected}")
            }
            ConfigError::Value { path, message } => {
                write!(f, "`{path}` in {file}: {message}")
            }
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_naming(f, &FILE_NAME)
    }
}

impl std::error::Error for ConfigError {}

type Result<T> = std::result::Result<T, ConfigError>;

// ─────────────────────────────────────────────────────────────────────────────
// The typed config shape. Every field is consumed by its command path.
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
    pub env_file: Option<EnvFileSetting>,
    pub loader: Option<BTreeMap<String, String>>,
    pub conditions: Option<Vec<String>>,
    pub tsconfig: Option<String>,
    pub jsx: Option<String>,
    pub jsx_factory: Option<String>,
    pub jsx_fragment_factory: Option<String>,
    pub jsx_import_source: Option<String>,
    pub decorators: Option<DecoratorMode>,
    pub emit_decorator_metadata: Option<bool>,
    pub verify_deps: Option<VerifyDeps>,

    // ── install phase ──
    pub install: InstallConfig,

    // ── nubx / dlx ──
    pub dlx: DlxConfig,
}

impl ProjectConfig {
    pub fn builtin_defaults() -> Self {
        Self {
            node_compat: Some(false),
            env_file: Some(EnvFileSetting::Default),
            verify_deps: Some(VerifyDeps::Warn),
            dlx: DlxConfig {
                consent: Some(ImplicitDlx::Prompt),
            },
            ..Self::default()
        }
    }
}

/// The `envFile` field's tri-state (the spec's separate `env` + `envFile` knobs
/// collapsed into one): `true` = today's default discovery, `false` = disable all
/// env-file loading, string / string[] = an exclusive source list. The `env` name
/// is reserved for the future per-VARIABLE allowlist grammar, so this file-
/// selection knob owns `envFile` alone.
/// Source strings are stored RAW; `${VAR}`/`$VAR` expansion is applied at the
/// wiring boundary (it references the process env, which the parser must not).
#[derive(Debug, Clone, PartialEq)]
pub enum EnvFileSetting {
    /// `true` — default `.env*` discovery.
    Default,
    /// `false` — disable all env-file loading.
    Disabled,
    /// A string / string[] exclusive source list.
    Sources(Vec<String>),
}

/// `verifyDeps` — nub's own field name, carrying a SUBSET of pnpm's value space.
/// The name is deliberately NOT pnpm's `verifyDepsBeforeRun`: that spelling is
/// reserved for adopting a pnpm field verbatim, and the gate is also not "before
/// run" — it fires on `nub <file>`, `nub exec`, and `nubx` too.
///
/// pnpm's `install` and `prompt` are absent: nub implements neither, and until
/// 2026-07-29 they parsed here and silently behaved as `warn`, which made the
/// field mean something other than what it said. Every nub-owned surface now
/// rejects them. The pnpm-mirroring surfaces still ACCEPT the strings — pnpm's
/// `verifyDepsBeforeRun` in `.npmrc` / `pnpm-workspace.yaml`, read only under a
/// pnpm incumbent — but they resolve straight to `Policy::Warn` in
/// [`crate::verify_deps`] without passing through this enum, so mirroring the
/// incumbent costs no variant here.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyDeps {
    Enabled(bool),
    Warn,
    Error,
}

/// The decorator transform selected by Nub's native config surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoratorMode {
    /// TypeScript's pre-Stage-3 decorator calling and emit semantics.
    Legacy,
}

/// The `install` block, consumed by the native PM through its aube settings
/// bridge.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct InstallConfig {
    pub linker: Option<LinkerConfig>,
    pub public_hoist: Option<PublicHoist>,
    pub minimum_release_age: Option<Duration>,
    pub minimum_release_age_exclude: Option<Vec<String>>,
}

/// The layout, discriminated on `strategy`, carrying only the knob that layout
/// admits (replaced the flat `nodeLinker`/`hoist`/`symlinkDisablePattern` trio
/// 2026-07-28). Each knob is meaningless — not merely ignored — outside its own
/// strategy: `hoist` fills a hidden tree that a shared store's residents can
/// never reach on their walk-up, and `eject` names packages to pull OUT of a
/// shared store that a project-local layout does not have. Modelling that as a
/// union rejects the combination where it is written instead of at install time.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkerConfig {
    /// Nub's default: one machine-shared store, every package symlinked out of
    /// it. `eject` names packages that must be real project-local bytes instead
    /// (native builds, anything that resolves relative to its own realpath).
    Global { eject: Option<Vec<String>> },
    /// pnpm-parity project-local store, still symlinked. `hoist` fills the
    /// hidden fallback tree that rescues a dependency's undeclared imports.
    Isolated { hoist: Option<Hoist> },
    /// Flat real directories, npm/yarn-shaped. Everything is already at the
    /// root, so neither knob has anything to say.
    Hoisted,
    /// Reserved for PnP-write; rejected at install time.
    Pnp,
}

/// `linker.hoist` — pnpm-literal `boolean | string[]`. `Bool(false)` = strict,
/// `Bool(true)` ≡ pnpm `['*']`, `Patterns` = the pattern list.
#[derive(Debug, Clone, PartialEq)]
pub enum Hoist {
    Bool(bool),
    Patterns(Vec<String>),
}

/// `publicHoist` — what is forced into the project's ROOT `node_modules`
/// without being declared, for tools that resolve from the project root rather
/// than through a dependency's own walk-up (tsc finding `@types/*`, a linter
/// loading plugins). It sits OUTSIDE [`LinkerConfig`] because it means the same
/// thing under every strategy: the root `node_modules` exists wherever the store
/// lives.
///
/// Patterns only, deliberately — no blanket boolean. pnpm's `shamefully-hoist`
/// is itself sugar for `publicHoistPattern: ['*']` (`getConfig.ts`), so `["*"]`
/// already spells "everything" without a second, differently-shaped way to say
/// it. A boolean also reads as if it overrides `linker`, which it does not: it
/// surrenders the isolation guarantee while leaving the layout alone.
pub type PublicHoist = Vec<String>;

/// The `dlx` block — nubx's own security posture. `consent` reuses the global
/// file's [`ImplicitDlx`] enum.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DlxConfig {
    pub consent: Option<ImplicitDlx>,
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
    EnvFile,
    Loader,
    Conditions,
    Tsconfig,
    Jsx,
    JsxFactory,
    JsxFragmentFactory,
    JsxImportSource,
    Decorators,
    EmitDecoratorMetadata,
    VerifyDeps,
    InstallLinker,
    InstallPublicHoist,
    InstallMinimumReleaseAge,
    InstallMinimumReleaseAgeExclude,
    DlxConsent,
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

/// The snapshot handed to a child through `__NUB_RUNTIME_CONFIG`, so this is a
/// cross-VERSION wire format: a nub of one version can hand it to a nub of
/// another (mid-`nub upgrade`, a `packageManager` pin, global vs project
/// install). `#[serde(default)]` makes a field the writer omits fall back to
/// [`Default`] rather than failing the whole deserialize — without it, an older
/// child aborts every run on a field a newer parent added, blaming a config
/// file the user cannot see. Unknown fields are already tolerated by serde,
/// which covers the other direction.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct RuntimeConfig {
    pub node_compat: bool,
    pub preload: Vec<String>,
    /// The directory `preload` entries were resolved against — the project that owns
    /// them. nub synthesizes its preload chainer INSIDE this directory so a BARE
    /// entry (`dotenv/config`) resolves through that project's `node_modules`
    /// walk-up, exactly as Node resolves the same specifier on a `--require` token.
    /// A chainer anywhere else resolves against nub's own install dir and dies with
    /// ERR_MODULE_NOT_FOUND (measured, with an in-project control that passes).
    /// `None` when `preload` is empty.
    pub preload_root: Option<PathBuf>,
    /// The synthesized chainer nub's OWN preload must load, set by the spawn path
    /// (never by config resolution). `Some` only when the chainer rides nub's
    /// preload rather than its own `NODE_OPTIONS` token — see prepare_preload_chain.
    pub preload_chain: Option<PathBuf>,
    pub node_options: Vec<String>,
    pub v8_flags: Vec<String>,
    pub env_file: RuntimeEnvFile,
    pub loader: BTreeMap<String, String>,
    pub conditions: Vec<String>,
    pub tsconfig: Option<String>,
    pub jsx: Option<String>,
    pub jsx_factory: Option<String>,
    pub jsx_fragment_factory: Option<String>,
    pub jsx_import_source: Option<String>,
    pub experimental_decorators: Option<bool>,
    pub emit_decorator_metadata: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "sources", rename_all = "camelCase")]
pub(crate) enum RuntimeEnvFile {
    #[default]
    Default,
    Disabled,
    Sources(Vec<PathBuf>),
}

const NODE_MODULES_DIR: &str = "node_modules";

/// Walk up from `start` (inclusive) to the filesystem root, returning the first
/// directory that holds a `nub.jsonc` — skipping anything inside `node_modules`.
///
/// The boundary is load-bearing, not hygiene. A dependency's lifecycle script
/// runs with its own package directory as cwd, and nub's shim is on PATH, so a
/// bare `node` in a `postinstall` re-enters nub from inside `node_modules`.
/// Without this, the DEPENDENCY's published `nub.jsonc` — npm packs the repo
/// root by default — becomes the project config: it would steer that build's
/// Node, and a `dlx` block or a key from a newer nub would fail-loud and abort
/// the install, naming a file the user never wrote. Skipping rather than
/// stopping keeps the real project's config reachable from inside a dependency.
pub fn discover_project_config(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if dir.components().any(|c| c.as_os_str() == NODE_MODULES_DIR) {
            continue;
        }
        let candidate = dir.join(FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Read `path` and hand its text to `parse`, attributing every failure — I/O
/// included — to the absolute path, which discovery may have found far above the
/// cwd.
fn read_config_at(
    path: &Path,
    kind: ConfigSourceKind,
    parse: fn(&str) -> Result<ProjectConfig>,
) -> Result<LoadedConfig> {
    // Resolved before the read so an I/O failure is attributable too.
    let source_path = std::path::absolute(path).map_err(ConfigError::Io)?;
    let text =
        crate::jsonc::read_guarded(path).map_err(|e| ConfigError::Io(e).in_file(&source_path))?;
    Ok(LoadedConfig {
        source: ConfigSource::file(kind, &source_path),
        values: parse(&text).map_err(|e| e.in_file(&source_path))?,
    })
}

/// Parse + validate the `nub.jsonc` at `path`.
/// FAIL-LOUD: an unknown key or malformed value is a [`ConfigError`], NOT a
/// silent degrade (unlike the best-effort global reader).
pub fn read_project_config_at(path: &Path) -> Result<LoadedConfig> {
    read_config_at(path, ConfigSourceKind::Project, parse_project_config)
}

pub(crate) fn read_global_config_at(path: &Path) -> Result<LoadedConfig> {
    read_config_at(path, ConfigSourceKind::Global, parse_global_config)
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
    validate_root(obj, ConfigScope::Project)
}

fn parse_global_config(text: &str) -> Result<ProjectConfig> {
    let value = crate::jsonc::parse_to_value(text).map_err(ConfigError::Parse)?;
    let Some(value) = value else {
        return Ok(ProjectConfig::default());
    };
    let mut obj = as_object(&value, "")?.clone();
    let legacy_consent = obj
        .get("exec")
        .and_then(|exec| ImplicitDlx::parse(exec.get("implicitDlx")?.as_str()?));
    // DELIBERATE, and it reads like an oversight — an unknown ROOT key here is
    // dropped rather than rejected, while the same typo one level down
    // (`install.bogus`) is an error. The global file is BEST-EFFORT by design:
    // it holds one person's defaults across every project on the machine, so a
    // stale key from a newer nub, or one typo, must not take their whole config
    // with it. `malformed_shared_schema_is_best_effort_globally` and
    // `typed_global_layer_accepts_legacy_consent_without_schema_drift` pin that,
    // and a project file — where the file is checked in and shared — keeps the
    // strict behaviour. Removing this retain to "fix the inconsistency" fails
    // both of those tests; the asymmetry is the feature.
    obj.retain(|key, _| ROOT_KEYS.contains(&key.as_str()));
    let mut config = validate_root(&obj, ConfigScope::Global)?;
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
    Ok(retain_effective_config(load_effective_config(
        cwd, overlays,
    )?))
}

/// Initialize the snapshot with the project file layer DROPPED; the CLI,
/// environment, global, and default layers still resolve normally. The
/// forced-compat runtime entrypoints call this once the project file has failed
/// to load — see `cli::initialize_runtime_config_snapshot` for why that degrade
/// is sound, and why leaving the snapshot uninitialized instead would not be
/// (`NODE_COMPAT` reaches the runtime only through these overlays).
pub fn initialize_effective_config_without_project(
    cwd: &Path,
    overlays: ConfigOverlays,
) -> &'static EffectiveConfig {
    if let Some(config) = EFFECTIVE_CONFIG.get() {
        return config;
    }
    let global = crate::config::load_global_config();
    retain_effective_config(resolve_effective_config(cwd, global, None, overlays))
}

fn retain_effective_config(config: EffectiveConfig) -> &'static EffectiveConfig {
    let won_the_race = EFFECTIVE_CONFIG.set(config).is_ok();
    let retained = EFFECTIVE_CONFIG
        .get()
        .expect("effective config was initialized");
    if won_the_race {
        record_snapshot_initialization_for_tests(retained);
    }
    retained
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
    match config.sources.get(&ConfigKey::DlxConsent) {
        // A built-in default is not a configured value: the legacy reader still
        // holds the only setting the user actually wrote.
        Some(source) if source.kind != ConfigSourceKind::Defaults => {
            config.values.dlx.consent.unwrap_or(legacy)
        }
        _ => legacy,
    }
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
        // still stronger. Precedence has already resolved CLI > environment > files;
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
            .iter()
            .flatten()
            .map(|value| {
                if is_path_like(value) {
                    self.resolve_path(ConfigKey::Preload, value)
                        .to_string_lossy()
                        .into_owned()
                } else {
                    value.clone()
                }
            })
            .collect::<Vec<String>>();
        // Anchor the chainer to the same root the entries resolved against, so a
        // bare entry resolves through THAT project's node_modules. Set even with no
        // `preload` entries: an INHERITED NODE_OPTIONS can carry preload flags of its
        // own that nub folds into the same chainers, and those need a directory too.
        let preload_root = Some(self.source_root(ConfigKey::Preload).to_path_buf());

        let env_file = match values.env_file.as_ref().unwrap_or(&EnvFileSetting::Default) {
            EnvFileSetting::Default => RuntimeEnvFile::Default,
            EnvFileSetting::Disabled => RuntimeEnvFile::Disabled,
            EnvFileSetting::Sources(sources) => RuntimeEnvFile::Sources(
                sources
                    .iter()
                    .map(|source| {
                        self.resolve_path(ConfigKey::EnvFile, &expand_runtime_path(source))
                    })
                    .collect(),
            ),
        };

        let tsconfig = values.tsconfig.as_deref().map(|path| {
            self.resolve_path(ConfigKey::Tsconfig, path)
                .to_string_lossy()
                .into_owned()
        });

        // Compat runs no transpiler, so the tsconfig is never consumed — and
        // validating it anyway would abort `--node` / `NODE_COMPAT` on a value
        // that cannot affect the run. Every runtime entrypoint resolves this
        // config BEFORE deciding compat, so without the guard a stale
        // `tsconfig` path disarms the zero-augmentation escape hatch exactly
        // when a broken config is what the user is escaping.
        let node_compat = values.node_compat.unwrap_or(false);
        if !node_compat && let Some(path) = tsconfig.as_deref() {
            let invalid = |message: String| ConfigError::Value {
                path: "tsconfig".into(),
                message,
            };
            let text = crate::jsonc::read_guarded(Path::new(path))
                .map_err(|error| invalid(format!("cannot read `{path}`: {error}")))?;
            let parsed = crate::jsonc::parse_to_value(&text)
                .map_err(|error| invalid(format!("cannot parse `{path}`: {error}")))?;
            if !matches!(parsed, Some(Value::Object(_))) {
                return Err(invalid(format!("`{path}` must contain a JSON object")));
            }
        }

        Ok(RuntimeConfig {
            node_compat,
            preload,
            preload_root,
            preload_chain: None,
            node_options: values.node_options.clone().unwrap_or_default(),
            v8_flags: values.v8_flags.clone().unwrap_or_default(),
            env_file,
            loader: values.loader.clone().unwrap_or_default(),
            conditions: values.conditions.clone().unwrap_or_default(),
            tsconfig,
            jsx: values.jsx.clone(),
            jsx_factory: values.jsx_factory.clone(),
            jsx_fragment_factory: values.jsx_fragment_factory.clone(),
            jsx_import_source: values.jsx_import_source.clone(),
            // Keep the established TypeScript spelling in the internal wire
            // format so an older child Nub can still consume a newer parent's
            // resolved project config during a version handoff.
            experimental_decorators: values
                .decorators
                .map(|mode| matches!(mode, DecoratorMode::Legacy)),
            emit_decorator_metadata: values.emit_decorator_metadata,
        })
    }
}

fn is_path_like(value: &str) -> bool {
    // `/` is listed separately because Windows treats a rooted-but-driveless
    // path as relative, so `is_absolute` alone would miss it there.
    value.starts_with(['.', '/', '~']) || Path::new(value).is_absolute()
}

/// Expand `${VAR}` / `$VAR` in one `envFile` source, by routing it through the
/// engine's map-shaped expander under a sentinel key. Reusing that expander is
/// what keeps `envFile` sources and `.env` values on one grammar, including the
/// bounded re-expansion rounds.
fn expand_runtime_path(value: &str) -> String {
    const SENTINEL: &str = "__NUB_CONFIG_ENV_PATH_VALUE__";
    // `vars()`, not `vars_os()`, PANICS on an environment holding any non-UTF-8
    // key or value — and this runs on the runtime path, once per `envFile`
    // source, for every `nub <file>` in a project that sets one. A single odd
    // variable inherited from the shell (a path off a differently-encoded
    // filesystem, an exotic locale string) would abort the run with a Rust
    // panic rather than any nub error. A name that cannot round-trip is simply
    // not a name `$VAR` expansion can reference, so dropping it loses nothing.
    let mut values: std::collections::HashMap<String, String> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    values.insert(SENTINEL.to_string(), value.to_string());
    nub_core::workspace::env::expand_env_map(&mut values);
    values.remove(SENTINEL).unwrap_or_default()
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

/// Overlay one layer, strongest-last: a field the layer SET (including an
/// explicit `false` / `[]`) overwrites the accumulator and claims the key's
/// source; a field it left absent falls through untouched.
fn merge_layer(
    values: &mut ProjectConfig,
    sources: &mut BTreeMap<ConfigKey, ConfigSource>,
    layer: &ProjectConfig,
    source: &ConfigSource,
) {
    // `merge!(install.linker, ConfigKey::InstallLinker)` — the field path is the
    // same on both sides, so naming it once keeps this a readable table of
    // field ↔ key rather than twenty near-identical `if let`s.
    macro_rules! merge {
        ($($field:ident).+, $key:expr) => {
            if let Some(value) = layer.$($field).+.as_ref() {
                values.$($field).+ = Some(value.clone());
                sources.insert($key, source.clone());
            }
        };
    }

    merge!(node_compat, ConfigKey::NodeCompat);
    merge!(preload, ConfigKey::Preload);
    merge!(node_options, ConfigKey::NodeOptions);
    merge!(v8_flags, ConfigKey::V8Flags);
    merge!(env_file, ConfigKey::EnvFile);
    merge!(loader, ConfigKey::Loader);
    merge!(conditions, ConfigKey::Conditions);
    merge!(tsconfig, ConfigKey::Tsconfig);
    merge!(jsx, ConfigKey::Jsx);
    merge!(jsx_factory, ConfigKey::JsxFactory);
    merge!(jsx_fragment_factory, ConfigKey::JsxFragmentFactory);
    merge!(jsx_import_source, ConfigKey::JsxImportSource);
    merge!(decorators, ConfigKey::Decorators);
    merge!(emit_decorator_metadata, ConfigKey::EmitDecoratorMetadata);
    merge!(verify_deps, ConfigKey::VerifyDeps);

    merge!(install.linker, ConfigKey::InstallLinker);
    merge!(install.public_hoist, ConfigKey::InstallPublicHoist);
    merge!(
        install.minimum_release_age,
        ConfigKey::InstallMinimumReleaseAge
    );
    merge!(
        install.minimum_release_age_exclude,
        ConfigKey::InstallMinimumReleaseAgeExclude
    );

    merge!(dlx.consent, ConfigKey::DlxConsent);
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

/// A JSON path as a message names it. The document root has no key of its own,
/// so it gets a placeholder rather than an empty string.
fn named_path(path: &str) -> String {
    if path.is_empty() {
        "<root>".to_string()
    } else {
        path.to_string()
    }
}

fn as_object<'a>(v: &'a Value, path: &str) -> Result<&'a serde_json::Map<String, Value>> {
    v.as_object().ok_or_else(|| ConfigError::Type {
        path: named_path(path),
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

fn as_nonempty_str<'a>(v: &'a Value, path: &str) -> Result<&'a str> {
    let value = as_str(v, path)?;
    if value.is_empty() {
        return Err(ConfigError::Value {
            path: path.into(),
            message: "must not be empty".into(),
        });
    }
    Ok(value)
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

/// A `{ string: string }` map (`loader`) — every value must be a string.
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
                    "unsupported loader `{loader}`; expected {}",
                    LOADERS.join("|")
                ),
            });
        }
        // `.tsx`/`.jsx` carry their JSX dialect in the extension, and the runtime
        // takes the dialect FROM the extension whenever the configured loader is
        // not itself `tsx`/`jsx` (`runtime/transform-core.mjs::langFor`). So `ts`
        // on one of these is not merely redundant — it is discarded, and the file
        // still parses as JSX. Refuse it here rather than let the config say one
        // thing and the runtime do another; a silent no-op is the exact failure
        // this parser is fail-loud to prevent. Every other pairing does take
        // effect: a data loader moves the extension out of the transpile set, and
        // `tsx`/`jsx` are honored verbatim.
        if loader == "ts" && JSX_PINNED_EXTS.contains(&extension.as_str()) {
            return Err(ConfigError::Value {
                path: child(path, extension),
                message: format!(
                    "`{extension}` is always parsed as JSX, so the `ts` loader cannot apply. \
                     Drop this entry, or rename the files to a non-JSX extension."
                ),
            });
        }
    }
    Ok(values)
}

/// Reject any key of `obj` not in `allowed` (fail-loud). The blessed `$schema`
/// key is tolerated at the root only — [`ROOT_KEYS`] carries it, the nested key
/// sets do not.
fn reject_unknown_keys(
    obj: &serde_json::Map<String, Value>,
    path: &str,
    allowed: &[&str],
) -> Result<()> {
    match obj.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(ConfigError::UnknownKey {
            path: named_path(path),
            key: key.clone(),
        }),
        None => Ok(()),
    }
}

/// Which file is being parsed. The two differ in exactly one way: `dlx` governs
/// whether `nubx` may reach the registry on a local miss, which is a decision
/// about the operator's own machine, not about the checkout. Honouring it from a
/// project file would let a cloned repository widen a consent the user set
/// globally, so the section is global-only and a project file carrying one is an
/// error.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfigScope {
    Project,
    Global,
}

/// Sections only the global file may carry; a project file naming one is an
/// error.
///
/// This does NOT mean the key is absent from the published schema. The schema
/// describes the field surface of `nub.jsonc` as a format — both files use that
/// name and `$schema` points at the same document — so it carries `dlx` too,
/// and `published_schema_exposes_every_parser_key` requires it to. Scope is
/// enforced by the parser at read time, not by omission from the schema; an
/// earlier revision of this comment claimed the opposite and contradicted the
/// test directly below it.
pub(crate) const GLOBAL_ONLY_KEYS: &[&str] = &["dlx"];

/// Validate a whole document object exactly as the file readers do.
///
/// The `nub config set` writer runs the one-key document it is about to write
/// through this before touching the file, so the writer can never accept a value
/// the reader refuses, and the rejection carries the reader's own wording. There
/// is deliberately no second validator to drift from this one.
pub(crate) fn validate_document(
    obj: &serde_json::Map<String, Value>,
    global: bool,
) -> Result<ProjectConfig> {
    let scope = if global {
        ConfigScope::Global
    } else {
        ConfigScope::Project
    };
    validate_root(obj, scope)
}

fn validate_root(
    obj: &serde_json::Map<String, Value>,
    scope: ConfigScope,
) -> Result<ProjectConfig> {
    if scope == ConfigScope::Project
        && let Some(key) = obj
            .keys()
            .find(|key| GLOBAL_ONLY_KEYS.contains(&key.as_str()))
    {
        return Err(ConfigError::GlobalOnlyKey { key: key.clone() });
    }
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
    if let Some(v) = obj.get("envFile") {
        cfg.env_file = Some(validate_env_file_setting(v, "envFile")?);
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
    if let Some(v) = obj.get("jsx") {
        let value = as_str(v, "jsx")?;
        if !JSX_VALUES.contains(&value) {
            return Err(ConfigError::Value {
                path: "jsx".into(),
                message: format!(
                    "must be one of {}",
                    JSX_VALUES
                        .iter()
                        .map(|value| format!("`{value}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        cfg.jsx = Some(value.to_string());
    }
    if let Some(v) = obj.get("jsxFactory") {
        cfg.jsx_factory = Some(as_nonempty_str(v, "jsxFactory")?.to_string());
    }
    if let Some(v) = obj.get("jsxFragmentFactory") {
        cfg.jsx_fragment_factory = Some(as_nonempty_str(v, "jsxFragmentFactory")?.to_string());
    }
    if let Some(v) = obj.get("jsxImportSource") {
        cfg.jsx_import_source = Some(as_nonempty_str(v, "jsxImportSource")?.to_string());
    }
    if let Some(v) = obj.get("decorators") {
        let value = as_nonempty_str(v, "decorators")?;
        cfg.decorators = Some(match value {
            "legacy" => DecoratorMode::Legacy,
            _ => {
                return Err(ConfigError::Value {
                    path: "decorators".into(),
                    message: format!(
                        "must be one of {}",
                        DECORATOR_VALUES
                            .iter()
                            .map(|value| format!("`{value}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
        });
    }
    if let Some(v) = obj.get("emitDecoratorMetadata") {
        cfg.emit_decorator_metadata = Some(as_bool(v, "emitDecoratorMetadata")?);
    }
    if let Some(v) = obj.get("verifyDeps") {
        cfg.verify_deps = Some(validate_verify_deps(v, "verifyDeps")?);
    }
    if let Some(v) = obj.get("install") {
        cfg.install = validate_install(v, "install")?;
    }
    // Global scope only: the project arm returned above.
    if let Some(v) = obj.get("dlx") {
        cfg.dlx = validate_dlx(v, "dlx")?;
    }
    Ok(cfg)
}

/// `envFile`: `true` | `false` | string | string[].
fn validate_env_file_setting(v: &Value, path: &str) -> Result<EnvFileSetting> {
    match v {
        Value::Bool(true) => Ok(EnvFileSetting::Default),
        Value::Bool(false) => Ok(EnvFileSetting::Disabled),
        Value::String(s) => Ok(EnvFileSetting::Sources(vec![s.clone()])),
        Value::Array(_) => Ok(EnvFileSetting::Sources(as_string_array(v, path)?)),
        _ => Err(ConfigError::Type {
            path: path.into(),
            expected: "a boolean, string, or array of strings",
        }),
    }
}

fn validate_verify_deps(v: &Value, path: &str) -> Result<VerifyDeps> {
    let unknown = |value: &str, note: &str| ConfigError::Value {
        path: path.into(),
        message: format!("{note}`{value}` (expected \"warn\", \"error\", or a boolean)"),
    };
    match v {
        Value::Bool(b) => Ok(VerifyDeps::Enabled(*b)),
        Value::String(s) => match s.as_str() {
            "warn" => Ok(VerifyDeps::Warn),
            "error" => Ok(VerifyDeps::Error),
            // pnpm's two remaining values are refused rather than resolved to
            // `warn`: nub has neither an auto-install nor an interactive confirm
            // at this gate, so accepting them would promise a behavior nothing
            // implements. See [`VerifyDeps`].
            "install" | "prompt" => Err(unknown(s, "nub does not implement ")),
            other => Err(unknown(other, "unknown value ")),
        },
        _ => Err(ConfigError::Type {
            path: path.into(),
            expected: "a boolean, \"warn\", or \"error\"",
        }),
    }
}

/// Per-strategy key sets. A key absent from its strategy's set but present in
/// another's is reported against THAT strategy rather than as a bare unknown
/// key — the whole reason the union exists is that "hoist under
/// global-virtual-store" is a recognizable mistake with a specific answer, not
/// a typo.
const LINKER_STRATEGY_KEYS: &[(&str, &[&str])] = &[
    ("global-virtual-store", &["strategy", "eject"]),
    ("isolated", &["strategy", "hoist"]),
    ("hoisted", &["strategy"]),
    ("pnp", &["strategy"]),
];

fn validate_linker(v: &Value, path: &str) -> Result<LinkerConfig> {
    // String shorthand: the strategies that admit no knob are the common case,
    // and `"linker": "hoisted"` should not have to be spelled as an object.
    if let Value::String(strategy) = v {
        return linker_without_options(strategy, path, path);
    }
    let obj = as_object(v, path)?;

    let strategy_path = child(path, "strategy");
    let Some(strategy) = obj.get("strategy") else {
        return Err(ConfigError::Value {
            path: path.into(),
            message: "missing `strategy` (one of \"global-virtual-store\", \"isolated\", \
                      \"hoisted\", or \"pnp\")"
                .into(),
        });
    };
    let strategy = as_str(strategy, &strategy_path)?;

    let Some((_, allowed)) = LINKER_STRATEGY_KEYS
        .iter()
        .find(|(name, _)| *name == strategy)
    else {
        return Err(unknown_strategy(strategy, &strategy_path, path));
    };
    if let Some(key) = obj.keys().find(|key| !allowed.contains(&key.as_str())) {
        let owner = LINKER_STRATEGY_KEYS
            .iter()
            .find(|(_, keys)| keys.contains(&key.as_str()));
        return Err(ConfigError::Value {
            path: child(path, key),
            message: match owner {
                Some((owner, _)) => format!(
                    "not valid with `strategy: \"{strategy}\"` — it configures the \
                     \"{owner}\" layout. Either switch to `strategy: \"{owner}\"` or drop this key."
                ),
                None => {
                    let mut names: Vec<_> = allowed.iter().map(|k| format!("`{k}`")).collect();
                    names.sort();
                    format!(
                        "unknown key (`strategy: \"{strategy}\"` accepts {})",
                        names.join(", ")
                    )
                }
            },
        });
    }

    match strategy {
        "global-virtual-store" => Ok(LinkerConfig::Global {
            eject: obj
                .get("eject")
                .map(|eject| as_string_array(eject, &child(path, "eject")))
                .transpose()?,
        }),
        "isolated" => Ok(LinkerConfig::Isolated {
            hoist: obj
                .get("hoist")
                .map(|hoist| validate_hoist(hoist, &child(path, "hoist")))
                .transpose()?,
        }),
        // `hoisted` and `pnp`, already checked against LINKER_STRATEGY_KEYS —
        // an object carrying nothing but `strategy` means what the shorthand means.
        knobless => linker_without_options(knobless, &strategy_path, path),
    }
}

/// `linker.hoist` — pnpm-literal `boolean | string[]`.
fn validate_hoist(v: &Value, path: &str) -> Result<Hoist> {
    match v {
        Value::Bool(b) => Ok(Hoist::Bool(*b)),
        Value::Array(_) => Ok(Hoist::Patterns(as_string_array(v, path)?)),
        _ => Err(ConfigError::Type {
            path: path.into(),
            expected: "a boolean or array of strings",
        }),
    }
}

/// Every strategy with its knob left unset — the string shorthand, and the two
/// strategies for which that is the only reachable shape.
fn linker_without_options(strategy: &str, value_path: &str, at: &str) -> Result<LinkerConfig> {
    match strategy {
        "global-virtual-store" => Ok(LinkerConfig::Global { eject: None }),
        "isolated" => Ok(LinkerConfig::Isolated { hoist: None }),
        "hoisted" => Ok(LinkerConfig::Hoisted),
        "pnp" => Ok(LinkerConfig::Pnp),
        other => Err(unknown_strategy(other, value_path, at)),
    }
}

/// `value_path` carries the bad value; `at` is the `linker` node the
/// object-form suggestion hangs off (they coincide under the string shorthand).
fn unknown_strategy(strategy: &str, value_path: &str, at: &str) -> ConfigError {
    ConfigError::Value {
        path: value_path.into(),
        message: format!(
            "unknown strategy `{strategy}` (expected \"global-virtual-store\", \"isolated\", \
             \"hoisted\", or \"pnp\"); `{at}` accepts either that string or an object with a \
             `strategy` key"
        ),
    }
}

fn validate_install(v: &Value, path: &str) -> Result<InstallConfig> {
    let obj = as_object(v, path)?;
    reject_unknown_keys(obj, path, INSTALL_KEYS)?;

    let mut install = InstallConfig::default();
    if let Some(v) = obj.get("linker") {
        install.linker = Some(validate_linker(v, &child(path, "linker"))?);
    }
    if let Some(v) = obj.get("publicHoist") {
        install.public_hoist = Some(as_string_array(v, &child(path, "publicHoist"))?);
    }
    if let Some(v) = obj.get("minimumReleaseAge") {
        let p = child(path, "minimumReleaseAge");
        install.minimum_release_age = Some(parse_duration(as_str(v, &p)?, &p)?);
    }
    if let Some(v) = obj.get("minimumReleaseAgeExclude") {
        let p = child(path, "minimumReleaseAgeExclude");
        install.minimum_release_age_exclude = Some(as_string_array(v, &p)?);
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
    Ok(dlx)
}

/// Parse the strict `minimumReleaseAge` duration grammar: `<integer><unit>`,
/// units `s|m|h|d|w` ONLY (no months/years — calendar ambiguity; `m` is
/// unambiguously minutes). A bare unit-less number is REJECTED (the npm-days vs
/// pnpm-minutes trap, made unrepresentable).
///
/// Shared with the `--minimum-release-age` CLI flag
/// ([`crate::pm_engine::min_release_age`]) so the file and the flag cannot drift
/// to two different grammars. The flag layers ONE relaxation on top: a bare
/// number there means MINUTES, because that is pnpm's CLI contract and the CLI
/// mirrors pnpm. The file keeps the rejection — it is nub's own surface, with no
/// incumbent grammar to match and nothing to disambiguate it.
pub(crate) fn parse_duration(s: &str, path: &str) -> Result<Duration> {
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
    // Digits only. `u64::from_str` also accepts a leading `+`, which the
    // published schema's `^[0-9]+[smhdw]$` rejects — so `"+3d"` parsed here
    // while failing validation in any editor reading the schema. Parser and
    // schema have to agree on what is accepted; the schema is the stricter and
    // the documented one, so this matches it rather than the other way round.
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("the amount must be a non-negative integer"));
    }
    // Every non-digit form is already rejected above, so the only way either of
    // these two steps can still fail is a value too large for the seconds count.
    let amount: u64 = digits.parse().map_err(|_| invalid("overflows"))?;
    per_unit
        .checked_mul(amount)
        .map(Duration::from_secs)
        .ok_or_else(|| invalid("overflows"))
}

// Matrix test packages (separate files; children of this module so they reach
// the private resolver).
#[cfg(test)]
#[path = "project_config_schema_matrix.rs"]
mod schema_matrix;

#[cfg(test)]
#[path = "project_config_precedence_matrix.rs"]
mod precedence_matrix;

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> ProjectConfig {
        parse_project_config(text).expect("valid config")
    }

    /// The global file's parser. `dlx` bodies must go through this one — the
    /// project parser rejects them by design.
    fn parse_global(text: &str) -> ProjectConfig {
        parse_global_config(text).expect("valid global config")
    }

    #[test]
    fn empty_and_comment_only_files_are_valid_empty_configs() {
        assert_eq!(parse(""), ProjectConfig::default());
        assert_eq!(parse("// just a comment\n"), ProjectConfig::default());
        assert_eq!(parse("{}"), ProjectConfig::default());
    }

    #[test]
    fn schema_key_is_accepted_and_ignored() {
        let cfg = parse("{ \"$schema\": \"https://nubjs.com/schema/latest.json\" }");
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
              "loader": { ".svg": "text" },
              "conditions": ["worker"],
              "tsconfig": "./tsconfig.runtime.json",
              "jsx": "react",
              "jsxFactory": "createElement",
              "jsxFragmentFactory": "Fragment",
              "jsxImportSource": "preact",
              "decorators": "legacy",
              "emitDecoratorMetadata": false,
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
            cfg.loader
                .as_ref()
                .and_then(|values| values.get(".svg"))
                .map(String::as_str),
            Some("text")
        );
        assert_eq!(cfg.conditions, Some(vec!["worker".into()]));
        assert_eq!(cfg.tsconfig.as_deref(), Some("./tsconfig.runtime.json"));
        assert_eq!(cfg.jsx.as_deref(), Some("react"));
        assert_eq!(cfg.jsx_factory.as_deref(), Some("createElement"));
        assert_eq!(cfg.jsx_fragment_factory.as_deref(), Some("Fragment"));
        assert_eq!(cfg.jsx_import_source.as_deref(), Some("preact"));
        assert_eq!(cfg.decorators, Some(DecoratorMode::Legacy));
        assert_eq!(cfg.emit_decorator_metadata, Some(false));
    }

    #[test]
    fn jsx_accepts_only_executable_runtime_modes() {
        for value in JSX_VALUES {
            parse_project_config(&format!(r#"{{ "jsx": "{value}" }}"#)).unwrap();
        }
        let err = parse_project_config(r#"{ "jsx": "solid" }"#).unwrap_err();
        assert!(
            matches!(err, ConfigError::Value { ref path, .. } if path == "jsx"),
            "{err}"
        );

        for key in ["jsxFactory", "jsxFragmentFactory", "jsxImportSource"] {
            let err = parse_project_config(&format!(r#"{{ "{key}": "" }}"#)).unwrap_err();
            assert!(
                matches!(err, ConfigError::Value { ref path, .. } if path == key),
                "{key}: {err}"
            );
        }
    }

    #[test]
    fn decorators_accepts_only_implemented_semantics() {
        let cfg = parse_project_config(r#"{ "decorators": "legacy" }"#).unwrap();
        assert_eq!(cfg.decorators, Some(DecoratorMode::Legacy));

        for document in [
            r#"{ "decorators": "standard" }"#,
            r#"{ "decorators": "experimental" }"#,
            r#"{ "decorators": true }"#,
        ] {
            let err = parse_project_config(document).unwrap_err();
            assert!(
                matches!(err, ConfigError::Type { ref path, .. } | ConfigError::Value { ref path, .. } if path == "decorators"),
                "{err}"
            );
        }
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

    /// `langFor` takes the dialect from the extension unless the configured
    /// loader is itself `tsx`/`jsx`, so `ts` on a JSX extension was accepted and
    /// then discarded — the file kept parsing as JSX. Refusing it is the whole
    /// point of a fail-loud config: a setting that cannot take effect must not
    /// look like it did. Everything adjacent still resolves, so the refusal
    /// cannot widen into "you may not configure these extensions at all".
    #[test]
    fn a_jsx_extension_rejects_the_non_jsx_ts_loader_but_nothing_else() {
        for extension in [".tsx", ".jsx"] {
            let error =
                parse_project_config(&format!(r#"{{ "loader": {{ "{extension}": "ts" }} }}"#))
                    .unwrap_err();
            assert!(
                matches!(&error, ConfigError::Value { message, .. }
                    if message.contains("always parsed as JSX")),
                "{extension}: {error}"
            );
        }

        // Still accepted, because each of these genuinely reaches the runtime:
        // `tsx`/`jsx` are honored verbatim, a data loader moves the extension out
        // of the transpile set, and `ts` on a non-JSX extension is what the
        // extension already resolves to.
        for (extension, loader) in [
            (".tsx", "jsx"),
            (".jsx", "tsx"),
            (".tsx", "text"),
            (".jsx", "yaml"),
            (".ts", "ts"),
            (".mts", "ts"),
            (".cts", "ts"),
            (".ts", "tsx"),
        ] {
            parse_project_config(&format!(
                r#"{{ "loader": {{ "{extension}": "{loader}" }} }}"#
            ))
            .unwrap_or_else(|e| panic!("{extension} -> {loader} must stay valid: {e}"));
        }
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
                env_file: Some(EnvFileSetting::Sources(vec!["./custom.env".into()])),
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
            snapshot.env_file,
            RuntimeEnvFile::Sources(vec![project_root.join("./custom.env")])
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

    /// `/dev/zero` is the case that matters: an unbounded read of it never
    /// returns, so reaching an assertion at all is half the contract. The other
    /// half is that it still fails as a `tsconfig` problem — the author gets
    /// pointed at the key they wrote, not at a bare I/O error.
    #[cfg(unix)]
    #[test]
    fn a_tsconfig_naming_a_device_fails_as_a_tsconfig_error() {
        let dir = tempfile::tempdir().unwrap();
        let project = LoadedConfig {
            source: ConfigSource::file(ConfigSourceKind::Project, &dir.path().join(FILE_NAME)),
            values: ProjectConfig {
                tsconfig: Some("/dev/zero".into()),
                ..ProjectConfig::default()
            },
        };
        let err = resolve_effective_config(
            dir.path(),
            None,
            Some(project),
            ConfigOverlays {
                defaults: ProjectConfig::builtin_defaults(),
                ..ConfigOverlays::default()
            },
        )
        .runtime_config()
        .unwrap_err();
        assert!(
            matches!(&err, ConfigError::Value { path, .. } if path == "tsconfig"),
            "{err:?}"
        );
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn env_file_tristate_covers_all_arms() {
        assert_eq!(
            parse(r#"{ "envFile": true }"#).env_file,
            Some(EnvFileSetting::Default)
        );
        assert_eq!(
            parse(r#"{ "envFile": false }"#).env_file,
            Some(EnvFileSetting::Disabled)
        );
        assert_eq!(
            parse(r#"{ "envFile": ".env.local" }"#).env_file,
            Some(EnvFileSetting::Sources(vec![".env.local".into()]))
        );
        assert_eq!(
            parse(r#"{ "envFile": [".env", ".env.local"] }"#).env_file,
            Some(EnvFileSetting::Sources(vec![
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
    fn verify_deps_covers_bool_and_string_arms() {
        for (text, expected) in [
            (r#"{ "verifyDeps": true }"#, VerifyDeps::Enabled(true)),
            (r#"{ "verifyDeps": false }"#, VerifyDeps::Enabled(false)),
            (r#"{ "verifyDeps": "warn" }"#, VerifyDeps::Warn),
            (r#"{ "verifyDeps": "error" }"#, VerifyDeps::Error),
        ] {
            assert_eq!(parse(text).verify_deps, Some(expected), "{text}");
        }
        assert!(matches!(
            parse_project_config(r#"{ "verifyDeps": "yes" }"#),
            Err(ConfigError::Value { .. })
        ));
    }

    /// pnpm's `install`/`prompt` parsed and silently behaved as `warn` until
    /// 2026-07-29. Rejecting them is the whole point, so the message has to name
    /// the field and say what it will take instead.
    #[test]
    fn verify_deps_rejects_the_values_nub_never_implemented() {
        for value in ["install", "prompt"] {
            let err = parse_project_config(&format!(r#"{{ "verifyDeps": "{value}" }}"#))
                .expect_err(&format!("`{value}` must fail loud"));
            match err.kind() {
                ConfigError::Value { path, message } => {
                    assert_eq!(path, "verifyDeps");
                    assert!(
                        message.contains(value)
                            && message.contains("\"warn\"")
                            && message.contains("\"error\""),
                        "{value}: {message}"
                    );
                }
                other => panic!("{value}: expected Value error, got {other:?}"),
            }
            assert!(
                err.to_string().contains("verifyDeps"),
                "the rendered error must name the field: {err}"
            );
        }
    }

    #[test]
    fn install_block_parses_and_validates() {
        let cfg = parse(
            r#"{
              "install": {
                "linker": { "strategy": "isolated", "hoist": ["*types*"] },
                "publicHoist": ["@types/*"],
                "minimumReleaseAge": "3d",
                "minimumReleaseAgeExclude": ["@myorg/*"]
              }
            }"#,
        );
        assert_eq!(
            cfg.install.linker,
            Some(LinkerConfig::Isolated {
                hoist: Some(Hoist::Patterns(vec!["*types*".into()]))
            })
        );
        assert_eq!(cfg.install.public_hoist, Some(vec!["@types/*".to_string()]));
        assert_eq!(
            cfg.install.minimum_release_age,
            Some(Duration::from_secs(3 * 86_400))
        );
        assert_eq!(
            cfg.install.minimum_release_age_exclude,
            Some(vec!["@myorg/*".into()])
        );
    }

    #[test]
    fn linker_string_shorthand_equals_the_knobless_object() {
        assert_eq!(
            parse(r#"{ "install": { "linker": "hoisted" } }"#)
                .install
                .linker,
            Some(LinkerConfig::Hoisted)
        );
        assert_eq!(
            parse(r#"{ "install": { "linker": "global-virtual-store" } }"#)
                .install
                .linker,
            parse(r#"{ "install": { "linker": { "strategy": "global-virtual-store" } } }"#)
                .install
                .linker,
        );
    }

    #[test]
    fn linker_rejects_an_unknown_strategy_in_either_form() {
        for (src, at) in [
            (
                r#"{ "install": { "linker": "hardlink" } }"#,
                "install.linker",
            ),
            (
                r#"{ "install": { "linker": { "strategy": "hardlink" } } }"#,
                "install.linker.strategy",
            ),
        ] {
            match parse_project_config(src).unwrap_err() {
                ConfigError::Value { path, .. } => assert_eq!(path, at, "{src}"),
                other => panic!("expected Value error for {src}, got {other:?}"),
            }
        }
    }

    /// The union's whole reason for existing: a knob belonging to a DIFFERENT
    /// strategy is a recognizable mistake with exactly one answer, so the error
    /// names that strategy instead of reporting an anonymous unknown key.
    #[test]
    fn a_knob_from_another_strategy_names_that_strategy() {
        for (src, key, expected) in [
            (
                r#"{ "install": { "linker": { "strategy": "global-virtual-store", "hoist": true } } }"#,
                "install.linker.hoist",
                "isolated",
            ),
            (
                r#"{ "install": { "linker": { "strategy": "isolated", "eject": ["x"] } } }"#,
                "install.linker.eject",
                "global-virtual-store",
            ),
        ] {
            match parse_project_config(src).unwrap_err() {
                ConfigError::Value { path, message } => {
                    assert_eq!(path, key, "{src}");
                    assert!(
                        message.contains(expected),
                        "message must point at `{expected}`: {message}"
                    );
                }
                other => panic!("expected Value error for {src}, got {other:?}"),
            }
        }
    }

    #[test]
    fn hoist_bool_and_array_forms() {
        let hoist = |src: &str| match parse(src).install.linker {
            Some(LinkerConfig::Isolated { hoist }) => hoist,
            other => panic!("expected an isolated linker, got {other:?}"),
        };
        assert_eq!(
            hoist(r#"{ "install": { "linker": { "strategy": "isolated", "hoist": false } } }"#),
            Some(Hoist::Bool(false))
        );
        assert_eq!(
            hoist(r#"{ "install": { "linker": { "strategy": "isolated", "hoist": ["a","b"] } } }"#),
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
        // `+3d` is here because `u64::from_str` accepts a leading `+` while the
        // published schema's `^[0-9]+[smhdw]$` does not: it parsed fine but any
        // editor reading the schema flagged it, which is the parser and the
        // documented grammar disagreeing about what is valid.
        for bad in ["3", "3y", "d", "-1d", "3 d", "+3d", "3_0d"] {
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
    fn dlx_consent_rejects_unknown_value() {
        assert!(matches!(
            parse_global_config(r#"{ "dlx": { "consent": "always" } }"#),
            Err(ConfigError::Value { .. })
        ));
    }

    #[test]
    fn dlx_is_rejected_in_a_project_file_and_accepted_in_the_global_one() {
        let body = r#"{ "dlx": { "consent": "prompt" } }"#;
        let err = parse_project_config(body).expect_err("a project dlx block must fail loud");
        assert!(
            matches!(&err, ConfigError::GlobalOnlyKey { key } if key.as_str() == "dlx"),
            "a correctly-spelled global-only section must not report as a typo: {err:?}"
        );
        let message = err.to_string();
        assert!(
            message.contains("`dlx`"),
            "message must name the key: {message}"
        );
        assert!(
            message.contains("configured globally"),
            "message must say the section belongs to the global file: {message}"
        );
        assert!(
            !message.contains("unknown key"),
            "the typo wording must not leak into the scope error: {message}"
        );

        // The same body is the point of the global file — a project `prompt`
        // must not be able to widen a global `never`.
        assert_eq!(
            parse_global(body).dlx.consent,
            Some(ImplicitDlx::Prompt),
            "the global file still accepts the section"
        );
    }

    #[test]
    fn global_only_key_error_names_the_source_and_destination_files() {
        let dir = tempfile::tempdir().unwrap();
        let project_path = dir.path().join(FILE_NAME);
        std::fs::write(&project_path, r#"{ "dlx": { "consent": "prompt" } }"#).unwrap();

        let err = read_project_config_at(&project_path).unwrap_err();
        let message = err.to_string();
        let source_path = std::path::absolute(&project_path).unwrap();
        assert!(
            message.contains(&source_path.display().to_string()),
            "scope error must name the project file containing the misplaced key: {message}"
        );
        match crate::config::config_path() {
            Some(destination_path) => assert!(
                message.contains(&destination_path.display().to_string()),
                "scope error must name the global destination file: {message}"
            ),
            None => assert!(
                message.contains(&format!("global {FILE_NAME}")),
                "scope error must name its global-file destination: {message}"
            ),
        }
    }

    #[test]
    fn a_project_files_own_sections_still_report_as_typos() {
        // The scope check runs before unknown-key rejection, so guard that it
        // did not swallow the ordinary misspelling path.
        let err = parse_project_config(r#"{ "instal": {} }"#).unwrap_err();
        assert!(
            matches!(&err, ConfigError::UnknownKey { key, .. } if key.as_str() == "instal"),
            "{err:?}"
        );
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
    fn published_schema_exposes_every_parser_key() {
        fn keys(value: &Value, pointer: &str) -> std::collections::BTreeSet<String> {
            value
                .pointer(pointer)
                .and_then(Value::as_object)
                .unwrap_or_else(|| panic!("schema object missing at {pointer}"))
                .keys()
                .cloned()
                .collect()
        }
        // ONE schema covers both files, so it carries every key either accepts,
        // global-only sections included. Scope is the parser's job and it fails
        // loud with a message naming the file to move the block to; the schema's
        // job is completion and value validation, and splitting it per file
        // would leave the global file with none at all. The two key sets are
        // therefore compared with no exemptions: a key the parser accepts and
        // the schema does not is an editor reporting a valid file as invalid.
        fn expected(values: &[&str]) -> std::collections::BTreeSet<String> {
            values.iter().map(|value| (*value).to_string()).collect()
        }

        // Read at RUNTIME, not via include_str!. The schema is a PUBLISHED site
        // artifact, and the site withdraws it whenever the config reference is
        // hidden pending ship (3539b65db1) — a compile-time include turns that
        // ordinary content decision into a build failure for the whole crate's test
        // target. Absent schema means there is nothing published to be out of step
        // with, so skip; the assertions return with the file.
        let published = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../site/public/schema/latest.json"
        );
        let Ok(source) = std::fs::read_to_string(published) else {
            eprintln!("skipping: {published} is not published right now");
            return;
        };
        let schema: Value = serde_json::from_str(&source)
            .expect("the published nub.json schema must be valid JSON");
        assert_eq!(keys(&schema, "/properties"), expected(ROOT_KEYS));
        assert_eq!(
            keys(&schema, "/properties/install/properties"),
            expected(INSTALL_KEYS)
        );
        assert_eq!(
            keys(&schema, "/properties/dlx/properties"),
            expected(DLX_KEYS)
        );
        assert_eq!(
            schema.get("$id").and_then(Value::as_str),
            Some("https://nubjs.com/schema/latest.json")
        );

        // Key sets alone leave a hole: a field whose VALUE SET drifts keeps its
        // key, so removing a value from the parser and forgetting the schema
        // passes everything above while the editor still offers a value the
        // parser now rejects. Pin the enumerated fields by value too.
        fn enum_values(schema: &Value, pointer: &str) -> std::collections::BTreeSet<String> {
            schema
                .pointer(pointer)
                .and_then(Value::as_array)
                .unwrap_or_else(|| panic!("schema enum missing at {pointer}"))
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }
        assert_eq!(
            enum_values(&schema, "/properties/verifyDeps/oneOf/1/enum"),
            expected(&["warn", "error"]),
            "verifyDeps: nub rejects the values it never implemented, so the schema must not offer them"
        );
        assert_eq!(
            enum_values(&schema, "/properties/jsx/enum"),
            expected(JSX_VALUES),
            "jsx must offer exactly the TypeScript modes the parser accepts"
        );
        for key in ["jsxFactory", "jsxFragmentFactory", "jsxImportSource"] {
            assert_eq!(
                schema
                    .pointer(&format!("/properties/{key}/minLength"))
                    .and_then(Value::as_u64),
                Some(1),
                "{key}: the schema must reject the empty string like the parser"
            );
        }
        assert_eq!(
            enum_values(&schema, "/properties/dlx/properties/consent/enum"),
            expected(&["prompt", "never"])
        );

        // `loader` spells its vocabulary three times: the open map, plus a
        // narrowed override per JSX-pinned extension. All three drift
        // independently of the parser, and an extra value here is an editor
        // offering a loader `validate_loader` rejects.
        assert_eq!(
            enum_values(&schema, "/properties/loader/additionalProperties/enum"),
            expected(LOADERS),
            "loader map must offer exactly the loaders the parser accepts"
        );
        let without_ts: std::collections::BTreeSet<String> = LOADERS
            .iter()
            .filter(|loader| **loader != "ts")
            .map(|loader| (*loader).to_string())
            .collect();
        for extension in JSX_PINNED_EXTS {
            assert_eq!(
                enum_values(
                    &schema,
                    &format!("/properties/loader/properties/{extension}/enum")
                ),
                without_ts,
                "{extension} is always parsed as JSX, so its schema enum is LOADERS minus `ts`"
            );
        }
        // Values alone are not enough: the loop above only visits extensions the
        // parser pins, so a narrowed override added to the schema for one it does
        // not — `.cts`, say — would stay invisible here while an editor rejected a
        // `ts` loader the parser accepts on it. Compare the KEY set too, exactly
        // as this test already does for the root/install/dlx sections.
        assert_eq!(
            keys(&schema, "/properties/loader/properties"),
            expected(JSX_PINNED_EXTS),
            "only a JSX-pinned extension may carry a narrowed loader enum"
        );

        // `linker` admits both a string shorthand and a discriminated object
        // union. All three schema representations of its strategies must stay
        // aligned with the parser's one source of truth: the object-level enum
        // determines the accepted discriminator, while each union arm's const
        // determines the options that discriminator unlocks.
        let linker_strategies = LINKER_STRATEGY_KEYS
            .iter()
            .map(|(strategy, _)| (*strategy).to_string())
            .collect();
        assert_eq!(
            &enum_values(
                &schema,
                "/properties/install/properties/linker/oneOf/0/enum"
            ),
            &linker_strategies,
            "install.linker string shorthand must accept exactly the parser strategies"
        );
        assert_eq!(
            &enum_values(
                &schema,
                "/properties/install/properties/linker/oneOf/1/properties/strategy/enum"
            ),
            &linker_strategies,
            "install.linker object strategy must accept exactly the parser strategies"
        );
        let linker_strategy_consts: std::collections::BTreeSet<String> = schema
            .pointer("/properties/install/properties/linker/oneOf/1/allOf")
            .and_then(Value::as_array)
            .expect("linker object union arms missing from schema")
            .iter()
            .map(|arm| {
                arm.pointer("/then/properties/strategy/const")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| {
                        panic!("linker object union arm missing its strategy const: {arm}")
                    })
                    .to_string()
            })
            .collect();
        assert_eq!(
            &linker_strategy_consts, &linker_strategies,
            "install.linker object union arms must cover exactly the parser strategies"
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

    /// The other half of `__NUB_RUNTIME_CONFIG` version skew. A newer parent
    /// handing a child a field it does not know is already tolerated by serde;
    /// this pins the reverse — an OLDER parent omitting a field a newer child
    /// has must fall back to the default, not abort the child's run.
    #[test]
    fn a_runtime_snapshot_missing_a_field_falls_back_to_its_default() {
        let sparse = serde_json::from_str::<RuntimeConfig>(r#"{ "nodeCompat": true }"#)
            .expect("a snapshot from an older nub must still deserialize");
        assert!(
            sparse.node_compat,
            "the field that WAS present must survive"
        );
        assert_eq!(
            sparse,
            RuntimeConfig {
                node_compat: true,
                ..RuntimeConfig::default()
            }
        );
    }

    /// A dependency's lifecycle script runs with its own package directory as
    /// cwd and nub's shim on PATH, so a bare `node` in a `postinstall` starts
    /// discovery from inside `node_modules`. The dependency ships whatever npm
    /// packed; it must never become this project's config, and the project's own
    /// must still be reachable from there.
    #[test]
    fn discovery_never_takes_a_config_from_inside_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(FILE_NAME), r#"{ "nodeCompat": true }"#).unwrap();
        let dep = root.join("node_modules").join("some-dep");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(dep.join(FILE_NAME), r#"{ "nodeCompat": false }"#).unwrap();

        assert_eq!(
            discover_project_config(&dep),
            Some(root.join(FILE_NAME)),
            "a dependency's own nub.jsonc must be skipped in favor of the project's"
        );

        let nested = dep.join("build");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            discover_project_config(&nested),
            Some(root.join(FILE_NAME)),
            "the boundary must hold from any depth inside the dependency"
        );
    }

    #[test]
    fn load_reads_a_present_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), r#"{ "nodeCompat": true }"#).unwrap();
        let loaded = load_project_config(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.values.node_compat, Some(true));
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

        let global = LoadedConfig {
            source: ConfigSource::file(ConfigSourceKind::Global, Path::new("/global/nub.jsonc")),
            values: ProjectConfig {
                dlx: DlxConfig {
                    consent: Some(ImplicitDlx::Prompt),
                },
                ..ProjectConfig::default()
            },
        };
        let snapshot = resolve_effective_config(
            Path::new("/cwd"),
            Some(global),
            None,
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
    fn shared_schema_keysets_match_the_parser() {
        assert!(
            GLOBAL_ONLY_KEYS.iter().all(|key| ROOT_KEYS.contains(key)),
            "a global-only key must be a real root key, or the scope filter is a no-op"
        );

        // Everything outside `GLOBAL_ONLY_KEYS` must read identically through
        // both parsers — the scope split is one carve-out, not two schemas.
        let shared = r#"{
          "$schema": "https://nubjs.com/schema/latest.json",
          "nodeCompat": false,
          "preload": [],
          "nodeOptions": [],
          "v8Flags": [],
          "envFile": false,
          "loader": {},
          "conditions": [],
          "tsconfig": "./tsconfig.json",
          "jsx": "react-jsx",
          "jsxFactory": "createElement",
          "jsxFragmentFactory": "Fragment",
          "jsxImportSource": "preact",
          "decorators": "legacy",
          "emitDecoratorMetadata": true,
          "verifyDeps": false,
          "install": {
            "linker": { "strategy": "isolated", "hoist": false },
            "publicHoist": [],
            "minimumReleaseAge": "1d",
            "minimumReleaseAgeExclude": []
          }
        }"#;
        let dir = tempfile::tempdir().unwrap();
        let project_path = dir.path().join("project.jsonc");
        let global_path = dir.path().join("global.jsonc");
        std::fs::write(&project_path, shared).unwrap();
        std::fs::write(&global_path, shared).unwrap();
        let project = read_project_config_at(&project_path).unwrap();
        let global = read_global_config_at(&global_path).unwrap();
        assert_eq!(project.values, global.values);
        assert_eq!(project.source.kind, ConfigSourceKind::Project);
        assert_eq!(global.source.kind, ConfigSourceKind::Global);

        let with_dlx = r#"{
          "nodeCompat": false,
          "dlx": { "consent": "prompt" }
        }"#;
        std::fs::write(&project_path, with_dlx).unwrap();
        std::fs::write(&global_path, with_dlx).unwrap();
        let global = read_global_config_at(&global_path).unwrap();
        assert_eq!(global.values.dlx.consent, Some(ImplicitDlx::Prompt));
        let err = read_project_config_at(&project_path).unwrap_err();
        assert!(matches!(err.kind(), ConfigError::GlobalOnlyKey { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_project_file_fails_loud() {
        use std::os::unix::fs::PermissionsExt;

        // Root ignores the mode bits, so the file stays readable and the
        // `unwrap_err` below would panic on a successful load. Not theoretical:
        // AGENTS.md routes config and global-cache behaviour through
        // `docker run --rm`, whose default user is root.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, r#"{ "nodeCompat": true }"#).unwrap();
        let original = std::fs::metadata(&path).unwrap().permissions();
        let mut unreadable = original.clone();
        unreadable.set_mode(0o000);
        std::fs::set_permissions(&path, unreadable).unwrap();

        let err = load_project_config(dir.path()).unwrap_err();
        assert!(matches!(err.kind(), ConfigError::Io(_)));

        std::fs::set_permissions(&path, original).unwrap();
    }

    #[test]
    fn project_reader_rejects_jsonc_extensions_outside_the_published_dialect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        for body in [
            r#"{ "nodeCompat": 0x1 }"#,
            r#"{ "nodeCompat": +1 }"#,
            r#"{ "nodeCompat": true "preload": [] }"#,
        ] {
            std::fs::write(&path, body).unwrap();
            let err = read_project_config_at(&path).expect_err(
                "the project reader must reject JSONC extensions absent from the schema",
            );
            assert!(matches!(err.kind(), ConfigError::Parse(_)), "{body}: {err}");
        }
    }

    #[test]
    fn malformed_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{ broken").unwrap();
        let err = load_project_config(dir.path()).unwrap_err();
        assert!(matches!(err.kind(), ConfigError::Parse(_)));
        // The reader attributes the failure to the file it read: discovery walks
        // up unbounded, so a bare filename can name a file far above the cwd.
        assert!(
            err.to_string()
                .contains(&std::path::absolute(&path).unwrap().display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn no_file_is_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_project_config(dir.path()).unwrap();
        assert_eq!(cfg, None);
    }
}
