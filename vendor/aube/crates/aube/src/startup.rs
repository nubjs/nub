use super::{Cli, Commands, LogLevel, ReporterType};
use miette::{Context, IntoDiagnostic, miette};
use std::path::PathBuf;
use tracing_subscriber::prelude::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug)]
pub(crate) struct StartupSettings {
    pub loglevel: Option<String>,
    package_manager_strict: PackageManagerStrictMode,
    package_manager_strict_version: bool,
    /// `managePackageManagerVersions` — when on, a `packageManager` /
    /// `devEngines.packageManager` aube pin is *switched to* (see
    /// `self_version`), so the guard's version mismatch error is
    /// superseded.
    pub manage_package_manager_versions: bool,
}

/// Tri-state for the `packageManagerStrict` setting.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub(crate) enum PackageManagerStrictMode {
    Off,
    #[default]
    Warn,
    Error,
}

impl PackageManagerStrictMode {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" => Some(Self::Off),
            "warn" => Some(Self::Warn),
            "error" | "true" | "1" => Some(Self::Error),
            _ => None,
        }
    }
}

fn resolve_package_manager_strict(ctx: &aube_settings::ResolveCtx<'_>) -> PackageManagerStrictMode {
    let raw = aube_settings::resolved::package_manager_strict(ctx);
    if let Some(mode) = PackageManagerStrictMode::parse(&raw) {
        return mode;
    }
    eprintln!(
        "warning: packageManagerStrict={raw:?} is not a recognized value (expected `off`, `warn`, `error`, or back-compat bool `true`/`false`); falling back to `warn`."
    );
    PackageManagerStrictMode::default()
}

pub(crate) fn resolve_color_mode(cli: &Cli) -> ColorMode {
    if cli.no_color {
        return ColorMode::Never;
    }
    if cli.color {
        return ColorMode::Always;
    }
    let env = aube_settings::values::capture_env();
    if let Some(mode) =
        aube_settings::values::string_from_env("color", &env).and_then(|raw| parse_color_mode(&raw))
    {
        return mode;
    }
    let Ok(cwd) = startup_cwd(cli) else {
        return ColorMode::Auto;
    };
    let npmrc = aube_registry::config::load_npmrc_entries(&cwd);
    aube_settings::values::string_from_npmrc("color", &npmrc)
        .and_then(|raw| parse_color_mode(&raw))
        .unwrap_or(ColorMode::Auto)
}

pub(crate) fn ci_renders_ansi() -> bool {
    use ci_info::types::Vendor;
    matches!(
        ci_info::get().vendor,
        Some(
            Vendor::GitHubActions
                | Vendor::GitLabCI
                | Vendor::Buildkite
                | Vendor::CircleCI
                | Vendor::TravisCI
                | Vendor::Drone
                | Vendor::AppVeyor
                | Vendor::AzurePipelines
                | Vendor::BitbucketPipelines
                | Vendor::TeamCity
                | Vendor::WoodpeckerCI
        )
    )
}

pub(crate) fn env_disables_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
        || std::env::var_os("CLICOLOR").is_some_and(|v| v == "0")
}

pub(crate) fn startup_cwd(cli: &Cli) -> miette::Result<PathBuf> {
    let cwd = match &cli.dir {
        Some(dir) if dir.is_absolute() => Ok(dir.clone()),
        Some(dir) => std::env::current_dir()
            .into_diagnostic()
            .map(|cwd| cwd.join(dir)),
        None => std::env::current_dir().into_diagnostic(),
    }?;
    if cli.workspace_root {
        crate::commands::find_workspace_root(&cwd)
    } else {
        Ok(cwd)
    }
}

pub(crate) fn load_startup_settings() -> miette::Result<StartupSettings> {
    let cwd = std::env::current_dir().into_diagnostic()?;
    let files = crate::commands::FileSources::load(&cwd);
    let empty_ws = std::collections::BTreeMap::new();
    let env = aube_settings::values::capture_env();
    let ctx = files.ctx(&empty_ws, &env, &[]);
    Ok(StartupSettings {
        loglevel: aube_settings::values::string_from_env("loglevel", &env)
            .or_else(|| {
                aube_settings::values::string_from_npmrc("loglevel", &files.project_aube_config)
            })
            .or_else(|| aube_settings::values::string_from_npmrc("loglevel", &files.project_npmrc))
            .or_else(|| {
                aube_settings::values::string_from_npmrc("loglevel", &files.user_aube_config)
            })
            .or_else(|| aube_settings::values::string_from_npmrc("loglevel", &files.user_npmrc)),
        package_manager_strict: resolve_package_manager_strict(&ctx),
        manage_package_manager_versions: aube_settings::resolved::manage_package_manager_versions(
            &ctx,
        ),
        package_manager_strict_version: aube_settings::resolved::package_manager_strict_version(
            &ctx,
        ),
    })
}

pub(crate) fn resolve_loglevel(cli: &Cli, configured: Option<&str>) -> LogLevel {
    let reporter_silent = matches!(cli.reporter, Some(ReporterType::Silent));
    if cli.silent || reporter_silent {
        return LogLevel::Silent;
    }
    if let Some(level) = cli.loglevel {
        return level;
    }
    if env_is_truthy("AUBE_TRACE") {
        return LogLevel::Trace;
    }
    if cli.verbose || env_is_truthy("AUBE_DEBUG") {
        return LogLevel::Debug;
    }
    configured
        .and_then(parse_loglevel)
        .unwrap_or(LogLevel::Warn)
}

fn env_is_truthy(name: &str) -> bool {
    let Ok(raw) = std::env::var(name) else {
        return false;
    };
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y"
    )
}

fn parse_loglevel(raw: &str) -> Option<LogLevel> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "trace" => Some(LogLevel::Trace),
        "debug" => Some(LogLevel::Debug),
        "info" => Some(LogLevel::Info),
        "warn" | "warning" => Some(LogLevel::Warn),
        "error" => Some(LogLevel::Error),
        "silent" => Some(LogLevel::Silent),
        _ => None,
    }
}

fn parse_color_mode(raw: &str) -> Option<ColorMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "always" | "true" | "1" => Some(ColorMode::Always),
        "never" | "false" | "0" => Some(ColorMode::Never),
        "auto" => Some(ColorMode::Auto),
        _ => None,
    }
}

#[cfg(unix)]
pub(crate) fn raise_nofile_limit() {
    // SAFETY: get/setrlimit are sync syscalls that read/write our own
    // process's resource table. No aliasing. Failure is reported as a
    // non-zero return and handled by the caller.
    unsafe {
        let mut rlim = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            tracing::trace!("getrlimit(RLIMIT_NOFILE) failed; keeping default FD limit");
            return;
        }
        let before = rlim.rlim_cur;
        if before >= rlim.rlim_max {
            tracing::trace!("RLIMIT_NOFILE soft={before} already at hard limit");
            return;
        }
        let hard = rlim.rlim_max;
        rlim.rlim_cur = hard;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
            tracing::trace!("raised RLIMIT_NOFILE soft {before} -> {hard}");
            return;
        }
        rlim.rlim_cur = before.max(10240).min(hard);
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
            tracing::trace!(
                "raised RLIMIT_NOFILE soft {before} -> {} (hard={hard}, fallback cap)",
                rlim.rlim_cur
            );
        } else {
            tracing::trace!("setrlimit(RLIMIT_NOFILE) failed; keeping soft={before}");
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn raise_nofile_limit() {}

pub(crate) fn diag_config_from_flag(cli: &Cli) -> Option<Option<aube_util::diag::DiagConfig>> {
    let mode = cli.diag.as_deref()?;
    let mode = mode.trim().to_ascii_lowercase();
    let valid = ["summary", "trace", "live", "full"];
    if !valid.contains(&mode.as_str()) {
        eprintln!(
            "[diag] unknown --diag mode {:?}. Valid: summary | trace | live | full",
            mode
        );
        return Some(None);
    }
    let track_events = mode != "summary";
    let print_stderr = mode == "live";
    let threshold_ms = if print_stderr {
        cli.diag_threshold_ms.unwrap_or(100)
    } else {
        0
    };
    let file = if mode == "full" {
        Some(
            cli.diag_file
                .clone()
                .unwrap_or_else(|| PathBuf::from("aube-diag.jsonl")),
        )
    } else {
        cli.diag_file.clone()
    };
    eprintln!(
        "[diag] mode={} (summary{}{}{})",
        mode,
        if track_events { " + critpath" } else { "" },
        if print_stderr { " + live" } else { "" },
        if file.is_some() { " + jsonl" } else { "" }
    );
    Some(Some(aube_util::diag::DiagConfig {
        file,
        print_stderr,
        summary: true,
        track_events,
        threshold_ms,
    }))
}

pub(crate) fn init_logging(cli: &Cli, effective_level: LogLevel) {
    let log_level = effective_level.filter();
    let env_filter = tracing_subscriber::EnvFilter::try_from_env("AUBE_LOG").unwrap_or_else(|_| {
        format!(
            "aube={log_level},aube_cli={log_level},aube_registry={log_level},\
             aube_resolver={log_level},aube_lockfile={log_level},aube_store={log_level},\
             aube_linker={log_level},aube_manifest={log_level},aube_scripts={log_level},\
             aube_workspace={log_level},aube_settings={log_level},aube_util={log_level}"
        )
        .into()
    });

    let drop_timestamp = !matches!(effective_level, LogLevel::Debug | LogLevel::Trace);
    let registry = tracing_subscriber::registry().with(env_filter);
    if matches!(cli.reporter, Some(ReporterType::Ndjson)) {
        crate::pnpmfile::set_ndjson_reporter(true);
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_writer(crate::progress::PausingWriter),
            )
            .init();
    } else if drop_timestamp {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .without_time()
                    .with_writer(crate::progress::PausingWriter),
            )
            .init();
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(crate::progress::PausingWriter))
            .init();
    }

    let force_text = matches!(
        effective_level,
        LogLevel::Trace | LogLevel::Debug | LogLevel::Silent
    ) || matches!(
        cli.reporter,
        Some(ReporterType::AppendOnly) | Some(ReporterType::Ndjson)
    );
    if force_text {
        clx::progress::set_output(clx::progress::ProgressOutput::Text);
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum PackageManagerGuard {
    Ok,
    WarnRunOnly,
}

pub(crate) fn enforce_package_manager_guardrails(
    settings: &StartupSettings,
    command: Option<&Commands>,
) -> miette::Result<PackageManagerGuard> {
    if settings.package_manager_strict == PackageManagerStrictMode::Off {
        return Ok(PackageManagerGuard::Ok);
    }

    let cwd = std::env::current_dir().into_diagnostic()?;
    let Some(root) = crate::dirs::find_workspace_root(&cwd)
        .filter(|root| root.join("package.json").is_file())
        .or_else(|| crate::dirs::find_project_root(&cwd))
    else {
        return Ok(PackageManagerGuard::Ok);
    };
    let path = root.join("package.json");
    let raw = std::fs::read_to_string(&path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&raw)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
    let Some(package_manager) = json.get("packageManager").and_then(|v| v.as_str()) else {
        return Ok(PackageManagerGuard::Ok);
    };
    let Some((name, version)) = parse_package_manager(package_manager) else {
        return Err(miette!(
            "invalid packageManager field `{package_manager}` in {}",
            path.display()
        ));
    };

    let normalized = version.strip_suffix("-DEBUG").unwrap_or(version);
    let id = aube_util::embedder();
    let self_name = id.name;
    let self_version = id.version;
    let compatible = id.compatible_names.first().copied().unwrap_or("pnpm");
    if id.self_names.contains(&name) {
        // With managePackageManagerVersions on, version mismatch
        // is handled *before* this guard by the self-switch
        // (crate::self_version) — reaching here mismatched means
        // switching failed soft (onFail warn/ignore) or the
        // switched binary still reports another version; either
        // way the switcher already surfaced it.
        if !settings.manage_package_manager_versions
            && settings.package_manager_strict_version
            && normalized != self_version
        {
            return Err(miette!(
                "packageManager requires {self_name}@{version}, but this is {self_name}@{self_version} \
                 (managePackageManagerVersions=false; re-enable it to switch automatically)"
            ));
        }
        Ok(PackageManagerGuard::Ok)
    } else if id.compatible_names.contains(&name) {
        if settings.package_manager_strict_version {
            return Err(miette!(
                "packageManager requires exact {name}@{version}, but {self_name} cannot download or re-exec a specific {name} version. Use {name} directly, set packageManagerStrictVersion=false, or pin packageManager to {self_name}@{self_version}."
            ));
        }
        Ok(PackageManagerGuard::Ok)
    } else {
        let other = name;
        {
            let mode = match settings.package_manager_strict {
                PackageManagerStrictMode::Error => package_manager_guard_mode(command),
                _ => PackageManagerGuardMode::WarnAndSkipAutoInstall,
            };
            match mode {
                PackageManagerGuardMode::Error => Err(miette!(
                    "packageManager in {} uses unsupported package manager `{other}`. {self_name}'s packageManagerStrict=error guard only accepts `{self_name}` and `{compatible}`; remove or change the `packageManager` field, or set `package-manager-strict=warn` (the default) or `=off` in .npmrc to soften this guard.",
                    path.display()
                )),
                PackageManagerGuardMode::WarnAndSkipAutoInstall => {
                    eprintln!(
                        "warning: packageManager in {} uses unsupported package manager `{other}`; continuing but auto-install is disabled. Switch packageManager to `{self_name}`/`{compatible}`, set packageManagerStrict=off, or pass `--no-install` to skip the install probe explicitly.",
                        path.display()
                    );
                    Ok(PackageManagerGuard::WarnRunOnly)
                }
            }
        }
    }
}

fn parse_package_manager(raw: &str) -> Option<(&str, &str)> {
    let (name, rest) = raw.rsplit_once('@')?;
    if name.is_empty() || rest.is_empty() {
        return None;
    }
    let version = rest.split_once('+').map_or(rest, |(version, _)| version);
    if version.is_empty() {
        return None;
    }
    Some((name, version))
}

pub(crate) fn command_needs_package_manager_guard(command: Option<&Commands>) -> bool {
    !matches!(
        command,
        None | Some(Commands::Config(_))
            | Some(Commands::Get(_))
            | Some(Commands::Set(_))
            | Some(Commands::Prefix(_))
            | Some(Commands::Completion(_))
    )
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum PackageManagerGuardMode {
    Error,
    WarnAndSkipAutoInstall,
}

pub(crate) fn package_manager_guard_mode(command: Option<&Commands>) -> PackageManagerGuardMode {
    if matches!(
        command,
        Some(Commands::Run(_))
            | Some(Commands::Test(_))
            | Some(Commands::Start(_))
            | Some(Commands::Stop(_))
            | Some(Commands::Restart(_))
            | Some(Commands::External(_))
    ) {
        PackageManagerGuardMode::WarnAndSkipAutoInstall
    } else {
        PackageManagerGuardMode::Error
    }
}

pub(crate) fn compute_effective_filter(cli: &Cli) -> aube_workspace::selector::EffectiveFilter {
    let mut filters = cli.filter.clone();
    if cli.recursive && filters.is_empty() && cli.filter_prod.is_empty() {
        filters.push("*".to_string());
    }
    aube_workspace::selector::EffectiveFilter {
        filters,
        filter_prods: cli.filter_prod.clone(),
        fail_if_no_match: cli.fail_if_no_match,
        include_workspace_root: cli.include_workspace_root || cli.workspace_root,
    }
}
