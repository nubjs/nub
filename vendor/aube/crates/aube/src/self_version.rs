//! Self-version switching (corepack semantics; pnpm's
//! `managePackageManagerVersions`): when the project pins aube via
//! `devEngines.packageManager` or the `packageManager` field and the
//! running binary doesn't satisfy the pin, locate or install the
//! pinned version and re-exec it with the same arguments.
//!
//! Runs before command dispatch and before the packageManager guard.
//! Re-exec preserves the multicall name (`aube` / `aubr` / `aubx`),
//! and a guard env var makes a misbehaving install degrade to a
//! warning instead of an exec loop.

use miette::{IntoDiagnostic, miette};
use std::path::{Path, PathBuf};

/// Loop guard: set on the re-exec'd child. If the child *still*
/// doesn't satisfy the pin (broken install, version skew), it warns
/// and continues instead of re-execing again.
const REEXEC_GUARD_ENV: &str = "AUBE_SELF_SWITCHED";

/// The version this binary reports for pin checks (debug builds strip
/// the `-DEBUG` suffix the guard also strips).
fn running_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A pin extracted from the manifest.
struct SelfPin {
    spec: aube_runtime::NodeSpec,
    raw: String,
    on_fail: aube_manifest::OnFail,
    source: &'static str,
}

/// Check for an aube version pin and re-exec the pinned version when
/// the running binary doesn't satisfy it. No-ops fast: a project
/// without a pin costs one (cached) manifest parse.
pub(crate) async fn maybe_switch(settings: &crate::startup::StartupSettings) -> miette::Result<()> {
    if !settings.manage_package_manager_versions {
        return Ok(());
    }
    // Product name for user-facing messages (standalone aube → "aube").
    let name = aube_util::embedder().name;
    let cwd = std::env::current_dir().into_diagnostic()?;
    let Some(root) = crate::dirs::find_workspace_root(&cwd)
        .filter(|root| root.join("package.json").is_file())
        .or_else(|| crate::dirs::find_project_root(&cwd))
    else {
        return Ok(());
    };
    let Ok(manifest) = aube_manifest::PackageJson::from_path_cached(&root.join("package.json"))
    else {
        // Unparseable manifests are the guard's diagnostic to give.
        return Ok(());
    };
    let Some(pin) = extract_pin(&manifest) else {
        return Ok(());
    };

    let current = node_semver::Version::parse(running_version().trim_end_matches("-DEBUG")).ok();
    if let Some(current) = &current
        && pin.spec.satisfied_by(current) == Some(true)
    {
        return Ok(());
    }

    // Resolve the pin to an exact version: exact pins directly, ranges
    // against installed versions first, then the published list. A
    // range that can't be resolved (offline, or nothing satisfies)
    // follows the pin's own onFail policy — warn/ignore must not turn
    // an advisory pin into a hard failure on an air-gapped machine.
    let target = match &pin.spec {
        aube_runtime::NodeSpec::Exact(v) => v.clone(),
        spec => {
            let best_installed = aube_runtime::list_installed_aube()
                .into_iter()
                .filter(|i| spec.satisfied_by(&i.version) == Some(true))
                .map(|i| i.version)
                .max();
            match best_installed {
                Some(v) => v,
                None => {
                    let resolved = match aube_runtime::available_aube_versions(2).await {
                        Ok(published) => {
                            let best = published
                                .iter()
                                .filter(|v| spec.satisfied_by(v) == Some(true))
                                .max()
                                .cloned();
                            best.ok_or_else(|| {
                                format!(
                                    "no published {name} satisfies {} (newest release: {})",
                                    pin.raw,
                                    published
                                        .iter()
                                        .max()
                                        .map(|v| v.to_string())
                                        .unwrap_or_default()
                                )
                            })
                        }
                        Err(e) => Err(format!("could not resolve {}: {e}", pin.raw)),
                    };
                    match resolved {
                        Ok(v) => v,
                        Err(detail) => {
                            return match pin.on_fail {
                                aube_manifest::OnFail::Ignore => Ok(()),
                                aube_manifest::OnFail::Warn => {
                                    tracing::warn!(
                                        code =
                                            aube_codes::warnings::WARN_AUBE_RUNTIME_VERSION_MISMATCH,
                                        requested = pin.raw,
                                        running = running_version(),
                                        "{detail}; continuing on this {name}"
                                    );
                                    Ok(())
                                }
                                _ => self_pin_unsatisfied(&pin, detail),
                            };
                        }
                    }
                }
            }
        }
    };

    // Loop guard, scoped to the resolved target: the guard env is
    // inherited by every descendant process, and a nested aube
    // invocation (lifecycle script, different project) may legitimately
    // need to switch to a *different* version. Only "we already
    // switched to exactly this version and it still doesn't satisfy"
    // is a loop — warn and keep running.
    if std::env::var(REEXEC_GUARD_ENV).is_ok_and(|v| v == target.to_string()) {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_RUNTIME_VERSION_MISMATCH,
            requested = pin.raw,
            running = running_version(),
            target = %target,
            "switched {name} binary still does not satisfy the project's pin; continuing"
        );
        return Ok(());
    }

    // onFail gates the *download*; an already-installed target always
    // switches (that's what the pin means).
    let install = match aube_runtime::find_installed_aube(&target) {
        Some(install) => install,
        None => match pin.on_fail {
            aube_manifest::OnFail::Ignore => return Ok(()),
            aube_manifest::OnFail::Warn => {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_RUNTIME_VERSION_MISMATCH,
                    requested = pin.raw,
                    running = running_version(),
                    source = pin.source,
                    "project pins a different {name} version (onFail: warn); continuing on this one"
                );
                return Ok(());
            }
            aube_manifest::OnFail::Error => {
                return self_pin_unsatisfied(
                    &pin,
                    format!("{name}@{target} is not installed and onFail is \"error\""),
                );
            }
            aube_manifest::OnFail::Download => {
                let cfg = aube_runtime::RuntimeConfig {
                    installer: crate::commands::with_settings_ctx(
                        &root,
                        crate::runtime::RuntimeSettings::from_ctx,
                    )
                    .installer,
                    mirror: None,
                    network: aube_runtime::NetworkMode::Online,
                    retries: 2,
                };
                crate::progress::safe_eprintln(&format!(
                    "Switching to {name}@{target} (pinned by {})…",
                    pin.source
                ));
                aube_runtime::install_aube(&cfg, &target, &crate::runtime::CliProgress::aube())
                    .await
                    .map_err(|e| miette!(code = e.code(), "{e}"))?
            }
        },
    };

    reexec(&install)
}

fn self_pin_unsatisfied(pin: &SelfPin, detail: String) -> miette::Result<()> {
    // Product name in the spec syntax shown to the user (standalone aube →
    // "aube"): the pin reads `packageManager: "<name>@<version>"`.
    let name = aube_util::embedder().name;
    Err(miette!(
        code = aube_codes::errors::ERR_AUBE_RUNTIME_VERSION_UNSATISFIED,
        help = format!(
            "pin an exact released version (e.g. `packageManager: \"{name}@<version>\"`), or set managePackageManagerVersions=false to skip switching"
        ),
        "the project pins {name}@{} via {}, but this is {name}@{} and {detail}",
        pin.raw,
        pin.source,
        running_version(),
    ))
}

/// Pin sources, highest precedence first: `devEngines.packageManager`
/// (name == aube; ranges allowed), then the corepack `packageManager`
/// field (`aube@<exact>`). Non-aube entries are the startup guard's
/// territory.
fn extract_pin(manifest: &aube_manifest::PackageJson) -> Option<SelfPin> {
    if let Some(entry) = manifest
        .dev_engines
        .as_ref()
        .and_then(|d| d.aube_package_manager())
        && let Some(version) = entry.version.as_deref()
        && let Ok(spec) = aube_runtime::NodeSpec::parse(version)
    {
        return Some(SelfPin {
            spec,
            raw: version.to_string(),
            // Unlike the spec's validation default (`error`), a
            // missing onFail here means "switch" — corepack/pnpm
            // semantics, and the entire point of pinning a package
            // manager version. Explicit onFail values are honored.
            on_fail: entry.on_fail.unwrap_or(aube_manifest::OnFail::Download),
            source: "devEngines.packageManager",
        });
    }
    let raw = manifest.extra.get("packageManager")?.as_str()?;
    let (name, version) = raw.rsplit_once('@')?;
    if !aube_util::embedder().self_names.contains(&name) {
        return None;
    }
    // Strip a corepack `+<hash>` suffix.
    let version = version.split_once('+').map_or(version, |(v, _)| v);
    let version = version.trim().trim_end_matches("-DEBUG");
    let exact = node_semver::Version::parse(version).ok()?;
    Some(SelfPin {
        spec: aube_runtime::NodeSpec::Exact(exact),
        raw: version.to_string(),
        on_fail: aube_manifest::OnFail::Download,
        source: "packageManager",
    })
}

/// Replace this process with the pinned binary, preserving the
/// multicall name (`aubr`/`aubx` dispatch on argv[0]) and arguments.
fn reexec(install: &aube_runtime::InstalledAube) -> miette::Result<()> {
    let exe = multicall_target(install);
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    tracing::debug!(
        target_exe = %exe.display(),
        version = %install.version,
        origin = install.origin.label(),
        "re-exec into pinned aube version"
    );

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args)
        .env(REEXEC_GUARD_ENV, install.version.to_string());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec() only returns on failure.
        let err = cmd.exec();
        Err(miette!(
            "failed to exec pinned aube at {}: {err}",
            exe.display()
        ))
    }
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .into_diagnostic()
            .map_err(|e| miette!("failed to spawn pinned aube at {}: {e}", exe.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// The binary in the install matching how this process was invoked.
/// Falls back to `aube` if the sibling is missing (it never is in
/// real archives — all three ship together).
fn multicall_target(install: &aube_runtime::InstalledAube) -> PathBuf {
    let invoked = std::env::args_os()
        .next()
        .map(PathBuf::from)
        .and_then(|p| {
            p.file_stem()
                .map(|s| s.to_string_lossy().to_ascii_lowercase())
        })
        .unwrap_or_default();
    let name = match invoked.as_str() {
        "aubr" => "aubr",
        "aubx" => "aubx",
        _ => "aube",
    };
    let sibling = sibling_bin(&install.exe, name);
    if sibling.is_file() {
        sibling
    } else {
        install.exe.clone()
    }
}

fn sibling_bin(aube_exe: &Path, name: &str) -> PathBuf {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    aube_exe
        .parent()
        .map(|d| d.join(&file))
        .unwrap_or_else(|| PathBuf::from(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(json: &str) -> aube_manifest::PackageJson {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn extracts_corepack_field() {
        let m = manifest(r#"{"name": "t", "packageManager": "aube@1.17.2"}"#);
        let pin = extract_pin(&m).unwrap();
        assert_eq!(pin.raw, "1.17.2");
        assert_eq!(pin.source, "packageManager");
        assert!(matches!(pin.spec, aube_runtime::NodeSpec::Exact(_)));
        assert_eq!(pin.on_fail, aube_manifest::OnFail::Download);
    }

    #[test]
    fn corepack_hash_suffix_is_stripped() {
        let m = manifest(r#"{"packageManager": "aube@1.17.2+sha256.deadbeef"}"#);
        assert_eq!(extract_pin(&m).unwrap().raw, "1.17.2");
    }

    #[test]
    fn non_aube_package_manager_is_ignored() {
        let m = manifest(r#"{"packageManager": "pnpm@10.4.1"}"#);
        assert!(extract_pin(&m).is_none());
    }

    #[test]
    fn dev_engines_beats_corepack_field() {
        let m = manifest(
            r#"{
                "packageManager": "aube@1.0.0",
                "devEngines": {"packageManager": {"name": "aube", "version": "^1.17"}}
            }"#,
        );
        let pin = extract_pin(&m).unwrap();
        assert_eq!(pin.source, "devEngines.packageManager");
        assert_eq!(pin.raw, "^1.17");
        assert!(matches!(pin.spec, aube_runtime::NodeSpec::Range(_)));
    }

    #[test]
    fn dev_engines_on_fail_is_honored() {
        let m = manifest(
            r#"{"devEngines": {"packageManager": {"name": "aube", "version": "^1.17", "onFail": "warn"}}}"#,
        );
        assert_eq!(
            extract_pin(&m).unwrap().on_fail,
            aube_manifest::OnFail::Warn
        );
    }

    #[test]
    fn no_pin_returns_none() {
        assert!(extract_pin(&manifest(r#"{"name": "t"}"#)).is_none());
        let m =
            manifest(r#"{"devEngines": {"packageManager": {"name": "pnpm", "version": "^10"}}}"#);
        assert!(extract_pin(&m).is_none());
    }
}
