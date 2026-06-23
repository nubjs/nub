use super::lockfile_dir::guard_against_foreign_importers;
use miette::{Context, IntoDiagnostic, miette};

pub(super) struct InstallLayoutConfig {
    pub(super) lockfile_dir: std::path::PathBuf,
    pub(super) lockfile_importer_key: String,
    pub(super) modules_dir_name: String,
    pub(super) aube_dir: std::path::PathBuf,
    pub(super) lockfile_enabled: bool,
    pub(super) shared_workspace_lockfile: bool,
    pub(super) lockfile_only_effective: bool,
    pub(super) lockfile_include_tarball_url: bool,
}

pub(super) fn resolve_install_layout(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    settings_ctx: &aube_settings::ResolveCtx<'_>,
    lockfile_only: bool,
    strict_no_lockfile: bool,
) -> miette::Result<InstallLayoutConfig> {
    let (lockfile_dir, lockfile_importer_key) =
        resolve_lockfile_location(cwd, manifest, settings_ctx)?;

    let modules_dir_name = aube_settings::resolved::modules_dir(settings_ctx);
    let aube_dir = super::super::resolve_virtual_store_dir(settings_ctx, cwd);

    let lockfile_enabled = aube_settings::resolved::lockfile(settings_ctx);
    let shared_workspace_lockfile =
        aube_settings::resolved::shared_workspace_lockfile(settings_ctx);
    let modules_dir_enabled = aube_settings::resolved::enable_modules_dir(settings_ctx);
    let lockfile_only_effective = lockfile_only || !modules_dir_enabled;

    validate_lockfile_mode(
        lockfile_enabled,
        modules_dir_enabled,
        lockfile_only,
        strict_no_lockfile,
    )?;

    let lockfile_include_tarball_url =
        aube_settings::resolved::lockfile_include_tarball_url(settings_ctx);
    tracing::debug!(
        "lockfile: enabled={lockfile_enabled}, include-tarball-url={lockfile_include_tarball_url}"
    );

    Ok(InstallLayoutConfig {
        lockfile_dir,
        lockfile_importer_key,
        modules_dir_name,
        aube_dir,
        lockfile_enabled,
        shared_workspace_lockfile,
        lockfile_only_effective,
        lockfile_include_tarball_url,
    })
}

/// Resolve `--lockfile-dir` / `lockfileDir` into the physical lockfile
/// directory plus the importer key this project owns inside that lockfile.
fn resolve_lockfile_location(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    settings_ctx: &aube_settings::ResolveCtx<'_>,
) -> miette::Result<(std::path::PathBuf, String)> {
    let location = match aube_settings::resolved::lockfile_dir(settings_ctx) {
        Some(raw) => {
            let raw_path = std::path::Path::new(&raw);
            let resolved = if raw_path.is_absolute() {
                raw_path.to_path_buf()
            } else {
                cwd.join(raw_path)
            };
            std::fs::create_dir_all(&resolved)
                .into_diagnostic()
                .wrap_err_with(|| format!("--lockfile-dir: {}", resolved.display()))?;
            let canon = std::fs::canonicalize(&resolved)
                .into_diagnostic()
                .wrap_err_with(|| format!("--lockfile-dir: {}", resolved.display()))?;
            let canon_cwd = std::fs::canonicalize(cwd).into_diagnostic()?;
            if canon == canon_cwd {
                (cwd.to_path_buf(), ".".to_string())
            } else {
                let key = pathdiff::diff_paths(&canon_cwd, &canon)
                    .map(|p| {
                        // Lockfile importer keys are portable forward-slash paths.
                        let s = p.to_string_lossy().into_owned();
                        if std::path::MAIN_SEPARATOR == '/' {
                            s
                        } else {
                            s.replace(std::path::MAIN_SEPARATOR, "/")
                        }
                    })
                    .ok_or_else(|| {
                        miette!(
                            "lockfile-dir {} cannot be related to project {}",
                            canon.display(),
                            canon_cwd.display()
                        )
                    })?;
                (canon, key)
            }
        }
        None => (cwd.to_path_buf(), ".".to_string()),
    };

    guard_lockfile_location(manifest, &location)?;
    Ok(location)
}

/// Reject multi-project shared lockfiles before the resolver can rewrite
/// someone else's importer entries.
fn guard_lockfile_location(
    manifest: &aube_manifest::PackageJson,
    (lockfile_dir, lockfile_importer_key): &(std::path::PathBuf, String),
) -> miette::Result<()> {
    if lockfile_importer_key == "." {
        return Ok(());
    }
    match aube_lockfile::parse_lockfile(lockfile_dir, manifest) {
        Ok(graph) => {
            guard_against_foreign_importers(lockfile_dir, lockfile_importer_key, &graph)
                .map_err(miette::Report::new)?;
        }
        Err(aube_lockfile::Error::NotFound(_)) => {}
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    }
    Ok(())
}

/// Collapse persistent `enableModulesDir=false` onto lockfile-only mode and
/// reject combinations that would otherwise become confusing no-op installs.
fn validate_lockfile_mode(
    lockfile_enabled: bool,
    modules_dir_enabled: bool,
    lockfile_only: bool,
    strict_no_lockfile: bool,
) -> miette::Result<()> {
    if !lockfile_enabled && lockfile_only {
        return Err(miette!(
            "--lockfile-only is incompatible with lockfile=false; \
             remove one or the other"
        ));
    }
    if !lockfile_enabled && !modules_dir_enabled {
        return Err(miette!(
            "enableModulesDir=false is incompatible with lockfile=false; \
             remove one or the other"
        ));
    }
    if !lockfile_enabled && strict_no_lockfile {
        return Err(miette!(
            "--frozen-lockfile is incompatible with lockfile=false; \
             remove one or the other"
        ));
    }
    Ok(())
}
