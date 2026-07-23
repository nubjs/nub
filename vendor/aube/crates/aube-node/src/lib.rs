//! Node-API surface for embedding aube in JavaScript hosts.

use aube::embed::{
    self, AddToProjectOptions, DepSelection, InstallControl, InstallEvent, InstallOutputLevel,
    InstallPhase, InstallReporter, NetworkMode,
};
use napi::bindgen_prelude::{
    AbortSignal, FnArgs, FromNapiValue, Function, JsObjectValue, JsValue, Object, PromiseRaw,
    ToNapiValue,
};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Env, Status};
use napi_derive::napi;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

type NodeError = napi::Error;
type NodeResult<T> = napi::Result<T>;
type EventCallback =
    ThreadsafeFunction<InstallEventPayload, (), FnArgs<(InstallEventPayload,)>, Status, false>;

struct InstallFailure {
    code: String,
    message: String,
    diagnostic: String,
}

static NODE_HOST: embed::Host = embed::Host {
    name: "aube-node",
    display_name: "aube Node-API",
    vendor: None,
    version: env!("CARGO_PKG_VERSION"),
    user_agent: concat!("aube-node/", env!("CARGO_PKG_VERSION")),
    self_names: &[],
    compatible_names: &["npm", "pnpm", "bun", "yarn"],
    lockfile_basename: "aube-lock.yaml",
    workspace_yaml: None,
    manifest_namespace: "",
    env_prefix: None,
    config_env_prefix: None,
    cache_namespace: "aube-node",
    data_namespace: "aube-node",
    canonical_lockfile_always_wins: false,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
    // nub's fork adds embedder-fixed fields upstream's `Host` does not have.
    // Inheriting them from `AUBE` keeps this host on standalone-aube behavior
    // for every field upstream doesn't name, and survives either side adding
    // more.
    ..embed::AUBE
};

/// Registration of the embedder profile and setting defaults. `configure` and
/// the first `install` race for it; `OnceLock` both serializes the race (a
/// concurrent install blocks until registration completes rather than
/// proceeding under the standalone profile) and records who won, so a
/// `configure` that loses must fail loudly rather than silently not apply.
static EMBEDDER_INIT: OnceLock<()> = OnceLock::new();

fn builtin_defaults() -> Vec<(String, String)> {
    vec![
        ("nodeLinker".to_string(), "hoisted".to_string()),
        ("minimumReleaseAge".to_string(), "0".to_string()),
    ]
}

fn merged_defaults(overrides: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut merged = builtin_defaults();
    for (key, value) in overrides {
        match merged.iter_mut().find(|(name, _)| *name == key) {
            Some(existing) => existing.1 = value,
            None => merged.push((key, value)),
        }
    }
    merged
}

fn validate_defaults(defaults: HashMap<String, String>) -> Result<Vec<(String, String)>, String> {
    let mut overrides = defaults.into_iter().collect::<Vec<_>>();
    // HashMap iteration order is random; keep registration deterministic.
    overrides.sort();
    for (key, _) in &overrides {
        if aube_settings::find(key).is_none() {
            return Err(key.clone());
        }
    }
    Ok(overrides)
}

#[napi(object)]
pub struct ConfigureInput {
    /// Embedder setting defaults by canonical setting name (e.g.
    /// `minimumReleaseAge`). They sit at the lowest precedence, below
    /// environment variables, project files, and user configuration.
    pub defaults: Option<HashMap<String, String>>,
}

/// Register embedder setting defaults for this process.
///
/// Call before the first `install`; registration is process-global and
/// first-write-wins, so calling after an install (or a second time) rejects
/// with `ERR_AUBE_EMBED_ALREADY_INITIALIZED`. Unknown setting names reject
/// with `ERR_AUBE_EMBED_INVALID_SETTING`.
#[napi(catch_unwind)]
pub fn configure(env: &Env, input: Option<ConfigureInput>) -> NodeResult<()> {
    let already_initialized = || {
        into_napi_error(
            env,
            InstallFailure {
                code: aube_codes::errors::ERR_AUBE_EMBED_ALREADY_INITIALIZED.to_string(),
                message: "configure must be called before the first install".to_string(),
                diagnostic: "embedder defaults are process-global and first-write-wins; this \
                             process already registered them"
                    .to_string(),
            },
        )
    };
    if EMBEDDER_INIT.get().is_some() {
        return Err(already_initialized());
    }
    let defaults = input.and_then(|input| input.defaults).unwrap_or_default();
    let overrides = validate_defaults(defaults).map_err(|key| {
        into_napi_error(
            env,
            InstallFailure {
                code: aube_codes::errors::ERR_AUBE_EMBED_INVALID_SETTING.to_string(),
                message: format!("unknown setting {key:?}"),
                diagnostic: format!(
                    "{key:?} is not a canonical aube setting name; see https://aube.jdx.dev/settings"
                ),
            },
        )
    })?;
    let mut applied = false;
    EMBEDDER_INIT.get_or_init(|| {
        embed::initialize(&NODE_HOST, merged_defaults(overrides));
        applied = true;
    });
    if !applied {
        return Err(already_initialized());
    }
    Ok(())
}

#[napi(object)]
pub struct PackageToAdd {
    pub name: String,
    pub version: Option<String>,
    pub dev: Option<bool>,
}

#[napi(object, object_to_js = false)]
pub struct InstallInput {
    pub add: Option<Vec<PackageToAdd>>,
    pub force: Option<bool>,
    pub offline: Option<bool>,
    pub osv_transitive_check: Option<bool>,
    pub on_event: Option<Function<'static, FnArgs<(InstallEventPayload,)>, ()>>,
    pub signal: Option<Object<'static>>,
}

#[napi(object)]
pub struct InstallResult {
    pub project_dir: String,
    pub added: Vec<String>,
    pub resolved: f64,
    pub reused: f64,
    pub downloaded: f64,
    pub duration_ms: f64,
}

#[napi(object)]
pub struct InstallEventPayload {
    pub kind: String,
    pub phase: Option<String>,
    pub level: Option<String>,
    pub code: Option<String>,
    pub message: Option<String>,
    pub resolved: Option<f64>,
    pub total: Option<f64>,
    pub reused: Option<f64>,
    pub downloaded: Option<f64>,
    pub downloaded_bytes: Option<f64>,
    pub estimated_bytes: Option<f64>,
}

struct NodeReporter {
    callback: Option<EventCallback>,
    stats: Arc<Mutex<InstallStats>>,
}

#[derive(Clone, Default)]
struct InstallStats {
    resolved: u64,
    reused: u64,
    downloaded: u64,
}

impl InstallReporter for NodeReporter {
    fn report(&self, event: InstallEvent) {
        if let InstallEvent::Progress(progress) = &event {
            let mut stats = self
                .stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            stats.resolved = progress.resolved as u64;
            stats.reused = progress.reused as u64;
            stats.downloaded = progress.downloaded as u64;
        }
        if let Some(callback) = &self.callback {
            let _ = callback.call(
                InstallEventPayload::from(event),
                ThreadsafeFunctionCallMode::NonBlocking,
            );
        }
    }
}

/// Install a project's declared dependencies and optionally add packages.
///
/// Added packages are saved at exact versions. Set an entry's `dev` field to
/// save it to `devDependencies`. Lifecycle scripts are always disabled.
#[napi(catch_unwind)]
pub fn install<'env>(
    env: &'env Env,
    project_dir: String,
    input: Option<InstallInput>,
) -> NodeResult<PromiseRaw<'env, InstallResult>> {
    initialize_embedder();

    let InstallInput {
        add,
        force,
        offline,
        osv_transitive_check,
        on_event,
        signal,
    } = input.unwrap_or(InstallInput {
        add: None,
        force: None,
        offline: None,
        osv_transitive_check: None,
        on_event: None,
        signal: None,
    });
    let callback = match on_event {
        Some(callback) => {
            let callback = callback
                .build_threadsafe_function()
                .build_callback(|ctx| Ok(FnArgs::from((ctx.value,))))
                .map_err(|error| {
                    into_napi_error(
                        env,
                        InstallFailure {
                            code: aube_codes::errors::ERR_AUBE_EMBED_INSTALL_FAILED.to_string(),
                            message: format!("failed to create install event callback: {error}"),
                            diagnostic: format!("{error:?}"),
                        },
                    )
                })?;
            Some(callback)
        }
        None => None,
    };
    let stats = Arc::new(Mutex::new(InstallStats::default()));
    let control = InstallControl::events(Arc::new(NodeReporter {
        callback,
        stats: Arc::clone(&stats),
    }));
    if let Some(signal) = signal {
        let aborted = signal.get_named_property::<bool>("aborted")?;
        // SAFETY: `signal` is the same live JS object validated by napi as an
        // Object above, and both conversions happen synchronously on the JS
        // thread before the native call returns.
        let signal = unsafe { AbortSignal::from_napi_value(env.raw(), signal.raw()) }?;
        let abort_control = control.clone();
        signal.on_abort(move || abort_control.cancel());
        if aborted {
            control.cancel();
        }
    }

    let packages = add
        .unwrap_or_default()
        .into_iter()
        .map(|package| {
            let spec = match package.version {
                Some(version) if !version.is_empty() => format!("{}@{version}", package.name),
                _ => package.name,
            };
            (spec, package.dev.unwrap_or(false))
        })
        .collect::<Vec<_>>();
    let force = force.unwrap_or(false);
    let offline = offline.unwrap_or(false);
    let osv_transitive_check = osv_transitive_check.unwrap_or(false);

    env.spawn_future_with_callback(
        async move {
            Ok::<_, napi::Error>(
                run_install(
                    project_dir,
                    packages,
                    force,
                    offline,
                    osv_transitive_check,
                    control,
                    stats,
                )
                .await,
            )
        },
        |env, result| match result {
            Ok(result) => Ok(result),
            Err(error) => Err(into_napi_error(env, error)),
        },
    )
}

async fn run_install(
    project_dir: String,
    packages: Vec<(String, bool)>,
    force: bool,
    offline: bool,
    osv_transitive_check: bool,
    control: InstallControl,
    stats: Arc<Mutex<InstallStats>>,
) -> Result<InstallResult, InstallFailure> {
    let started = Instant::now();
    let project_dir = prepare_project_dir(Path::new(&project_dir)).await?;
    let dep_selection = npmrc_dep_selection(&project_dir);

    if !packages.is_empty() {
        for save_dev in [false, true] {
            let selected = packages
                .iter()
                .filter(|(_, dev)| *dev == save_dev)
                .map(|(spec, _)| spec.clone())
                .collect::<Vec<_>>();
            if selected.is_empty() {
                continue;
            }
            embed::add(
                &project_dir,
                &selected,
                AddToProjectOptions {
                    save_dev,
                    save_exact: true,
                    save_optional: false,
                    save_peer: false,
                    ignore_scripts: true,
                    force,
                    dangerously_allow_all_builds: false,
                    offline,
                    dep_selection,
                    osv_transitive_check,
                    control: control.clone(),
                    // JS hosts rely on ambient node; no host-supplied runtime.
                    node_bin_dir: None,
                },
            )
            .await
            .map_err(to_install_failure)?;
        }
    } else {
        let mut options = embed::InstallOptions::new(project_dir.clone());
        options.ignore_scripts = true;
        options.force = force;
        options.dep_selection = dep_selection;
        options.osv_transitive_check = osv_transitive_check;
        options.control = control;
        if offline {
            options.network_mode = NetworkMode::Offline;
        }

        embed::install(options).await.map_err(to_install_failure)?;
    }

    let stats = stats
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();

    Ok(InstallResult {
        project_dir: project_dir.to_string_lossy().into_owned(),
        added: packages.into_iter().map(|(spec, _)| spec).collect(),
        resolved: stats.resolved as f64,
        reused: stats.reused as f64,
        downloaded: stats.downloaded as f64,
        duration_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

fn npmrc_dep_selection(project_dir: &Path) -> DepSelection {
    let omit = aube_registry::config::load_npmrc_entries(project_dir)
        .into_iter()
        .rev()
        .find(|(key, _)| key == "omit" || key == "omit[]")
        .map(|(_, value)| value)
        .unwrap_or_default();
    let mut omit_dev = false;
    let mut omit_optional = false;
    for value in omit.split([',', ' ', '\t', '\n']) {
        omit_dev |= value == "dev";
        omit_optional |= value == "optional";
    }
    DepSelection::from_flags(omit_dev, false, omit_optional)
}

fn initialize_embedder() {
    EMBEDDER_INIT.get_or_init(|| embed::initialize(&NODE_HOST, builtin_defaults()));
}

async fn prepare_project_dir(project_dir: &Path) -> Result<PathBuf, InstallFailure> {
    tokio::fs::create_dir_all(project_dir)
        .await
        .map_err(|error| {
            invalid_project_error(project_dir, format!("failed to create directory: {error}"))
        })?;
    let project_dir = tokio::fs::canonicalize(project_dir)
        .await
        .map_err(|error| {
            invalid_project_error(project_dir, format!("failed to resolve directory: {error}"))
        })?;
    let manifest = project_dir.join("package.json");
    if !tokio::fs::try_exists(&manifest).await.map_err(|error| {
        invalid_project_error(
            &project_dir,
            format!("failed to inspect package.json: {error}"),
        )
    })? {
        tokio::fs::write(&manifest, b"{}\n")
            .await
            .map_err(|error| {
                invalid_project_error(
                    &project_dir,
                    format!("failed to create package.json: {error}"),
                )
            })?;
    }
    Ok(project_dir)
}

fn invalid_project_error(project_dir: &Path, detail: String) -> InstallFailure {
    let message = format!(
        "invalid project directory {}: {detail}",
        project_dir.display()
    );
    InstallFailure {
        code: aube_codes::errors::ERR_AUBE_EMBED_INVALID_PROJECT.to_string(),
        diagnostic: message.clone(),
        message,
    }
}

fn to_install_failure(error: miette::Report) -> InstallFailure {
    let code = embed::error_code(&error)
        .unwrap_or_else(|| aube_codes::errors::ERR_AUBE_EMBED_INSTALL_FAILED.to_string());
    InstallFailure {
        code,
        message: error.to_string(),
        diagnostic: format!("{error:?}"),
    }
}

fn into_napi_error(env: &Env, failure: InstallFailure) -> NodeError {
    let fallback_reason = format!("[{}] {}", failure.code, failure.message);
    let fallback = || NodeError::new(Status::GenericFailure, fallback_reason.clone());
    let Ok(mut error) = env.create_error(NodeError::new(Status::GenericFailure, &failure.message))
    else {
        return fallback();
    };
    if error.set("code", failure.code).is_err()
        || error.set("diagnostic", failure.diagnostic).is_err()
    {
        return fallback();
    }
    match error.into_unknown(env) {
        Ok(error) => NodeError::from(error),
        Err(_) => fallback(),
    }
}

impl From<InstallEvent> for InstallEventPayload {
    fn from(event: InstallEvent) -> Self {
        match event {
            InstallEvent::Phase(phase) => Self {
                kind: "phase".to_string(),
                phase: Some(phase_name(phase).to_string()),
                level: None,
                code: None,
                message: None,
                resolved: None,
                total: None,
                reused: None,
                downloaded: None,
                downloaded_bytes: None,
                estimated_bytes: None,
            },
            InstallEvent::Progress(progress) => Self {
                kind: "progress".to_string(),
                phase: None,
                level: None,
                code: None,
                message: None,
                resolved: Some(progress.resolved as f64),
                total: Some(progress.total as f64),
                reused: Some(progress.reused as f64),
                downloaded: Some(progress.downloaded as f64),
                downloaded_bytes: Some(progress.downloaded_bytes as f64),
                estimated_bytes: (progress.estimated_bytes > 0)
                    .then_some(progress.estimated_bytes as f64),
            },
            InstallEvent::Output {
                level,
                code,
                message,
            } => Self {
                kind: "output".to_string(),
                phase: None,
                level: Some(level_name(level).to_string()),
                code,
                message: Some(message),
                resolved: None,
                total: None,
                reused: None,
                downloaded: None,
                downloaded_bytes: None,
                estimated_bytes: None,
            },
        }
    }
}

fn phase_name(phase: InstallPhase) -> &'static str {
    match phase {
        InstallPhase::Resolving => "resolving",
        InstallPhase::Fetching => "fetching",
        InstallPhase::Linking => "linking",
        InstallPhase::Complete => "complete",
    }
}

fn level_name(level: InstallOutputLevel) -> &'static str {
    match level {
        InstallOutputLevel::Info => "info",
        InstallOutputLevel::Warning => "warning",
        InstallOutputLevel::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_defaults_lets_overrides_replace_builtins() {
        let merged = merged_defaults(vec![(
            "minimumReleaseAge".to_string(),
            "259200".to_string(),
        )]);
        assert!(merged.contains(&("minimumReleaseAge".to_string(), "259200".to_string())));
        assert!(merged.contains(&("nodeLinker".to_string(), "hoisted".to_string())));
        assert_eq!(
            merged
                .iter()
                .filter(|(key, _)| key == "minimumReleaseAge")
                .count(),
            1
        );
    }

    #[test]
    fn merged_defaults_appends_settings_without_builtin_values() {
        let merged = merged_defaults(vec![("paranoid".to_string(), "true".to_string())]);
        assert!(merged.contains(&("paranoid".to_string(), "true".to_string())));
        assert_eq!(merged.len(), builtin_defaults().len() + 1);
    }

    #[test]
    fn validate_defaults_rejects_unknown_setting_names() {
        let defaults = HashMap::from([("minimumRelaseAge".to_string(), "259200".to_string())]);
        assert_eq!(
            validate_defaults(defaults),
            Err("minimumRelaseAge".to_string())
        );
    }

    #[test]
    fn validate_defaults_sorts_canonical_names() {
        let defaults = HashMap::from([
            ("nodeLinker".to_string(), "hoisted".to_string()),
            ("minimumReleaseAge".to_string(), "259200".to_string()),
        ]);
        assert_eq!(
            validate_defaults(defaults),
            Ok(vec![
                ("minimumReleaseAge".to_string(), "259200".to_string()),
                ("nodeLinker".to_string(), "hoisted".to_string()),
            ])
        );
    }
}
