//! Record the pnpm a project pins in `pnpm-lock.yaml`'s env document.
//!
//! pnpm 11 and 12 write the pinned pnpm's package closure, with
//! integrities, into a leading YAML document of the lockfile
//! (`importers['.'].packageManagerDependencies`), and their
//! `--frozen-lockfile` refuses a lockfile that does not record it
//! (`ERR_PNPM_FROZEN_LOCKFILE_WITH_OUTDATED_LOCKFILE`). This module is the
//! port of pnpm's `syncEnvLockfile`: same opt-in rule, same version kept or
//! chosen, same specifier recorded per major, so a lockfile written here is
//! one pnpm accepts frozen and leaves byte-identical. pnpm 10 has no env
//! document, and a pin that resolves to a pnpm below 11 records nothing.
//!
//! References: pnpm 12 `crates/cli/src/cli_args/package_manager.rs`
//! (`wanted_package_manager`, `should_persist_package_manager_lockfile`)
//! and `crates/env-installer/src/resolve_package_manager_integrities.rs`;
//! pnpm 11 `pnpm/src/{syncEnvLockfile,switchCliVersion}.ts`.

use aube_lockfile::pnpm::{EnvDependency, EnvLockfile};
use miette::{IntoDiagnostic, WrapErr};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const LOCKFILE: &str = "pnpm-lock.yaml";

/// A project's pnpm pin, when it is one pnpm records in the lockfile.
#[derive(Debug, PartialEq, Eq)]
struct WantedPnpm {
    /// `devEngines.packageManager.version` verbatim, or the exact version
    /// of a `packageManager: pnpm@<v>` field without its `+<hash>` build.
    specifier: String,
    /// The effective `onFail` policy. pnpm 11 records the range as the
    /// specifier only when it switches versions (`download`).
    on_fail_download: bool,
    /// The exact version a `packageManager: pnpm@<v>` field names, also
    /// when `devEngines` is the pin — the version this project actually
    /// runs, which a range pin then records.
    legacy_exact: Option<String>,
}

/// pnpm's `wanted_package_manager` + `should_persist_package_manager_lockfile`:
/// `devEngines.packageManager` wins over `packageManager`; a `devEngines`
/// pin persists unless `onFail` is `ignore`; a bare `packageManager` pin
/// persists only from pnpm 12.
fn wanted_pnpm(manifest: &Value) -> Option<WantedPnpm> {
    let legacy = manifest
        .get("packageManager")
        .and_then(Value::as_str)
        .and_then(parse_package_manager_field);
    let legacy_exact = legacy
        .as_ref()
        .filter(|(name, _)| name == "pnpm")
        .and_then(|(_, version)| version.clone());

    let engines = manifest
        .get("devEngines")
        .and_then(|d| d.get("packageManager"));
    let from_dev_engines = match engines {
        Some(Value::Array(entries)) if !entries.is_empty() => {
            let index = entries
                .iter()
                .position(|e| e.get("name").and_then(Value::as_str) == Some("pnpm"))
                .unwrap_or(0);
            let default = if index == entries.len() - 1 {
                "error"
            } else {
                "ignore"
            };
            engine_pin(&entries[index], default)
        }
        Some(entry @ Value::Object(_)) => engine_pin(entry, "download"),
        _ => None,
    };
    // A `devEngines` entry without a name pins nothing, and the legacy
    // field governs instead.
    let (name, version, on_fail, from_dev_engines) = match from_dev_engines {
        Some(pin) => pin,
        None => {
            let (name, version) = legacy?;
            (name, version, "download".to_string(), false)
        }
    };
    let version = version?;
    if name != "pnpm" || on_fail == "ignore" {
        return None;
    }
    if !from_dev_engines && node_semver::Version::parse(&version).ok()?.major < 12 {
        return None;
    }
    Some(WantedPnpm {
        specifier: version,
        on_fail_download: on_fail == "download",
        legacy_exact,
    })
}

/// `(name, version, onFail, from_dev_engines)` of one `devEngines.packageManager`
/// entry. A version that is not a valid range pins nothing.
fn engine_pin(
    entry: &Value,
    default_on_fail: &str,
) -> Option<(String, Option<String>, String, bool)> {
    let name = entry.get("name")?.as_str()?.to_string();
    let version = entry
        .get("version")
        .and_then(Value::as_str)
        .filter(|v| node_semver::Range::parse(v).is_ok())
        .map(str::to_string);
    let on_fail = entry
        .get("onFail")
        .and_then(Value::as_str)
        .unwrap_or(default_on_fail)
        .to_string();
    Some((name, version, on_fail, true))
}

/// `pnpm@9.5.0+sha512.…` → `("pnpm", Some("9.5.0"))`. The version is kept
/// only when it is an exact version already in canonical form, as pnpm's
/// legacy-field reader requires; a URL reference names no version.
fn parse_package_manager_field(field: &str) -> Option<(String, Option<String>)> {
    let at = if let Some(scoped) = field.strip_prefix('@') {
        scoped.find('@')? + 1
    } else {
        field.find('@')?
    };
    let (name, reference) = (&field[..at], &field[at + 1..]);
    if reference.contains(':') {
        return Some((name.to_string(), None));
    }
    let version = reference.split_once('+').map_or(reference, |(v, _)| v);
    let exact = node_semver::Version::parse(version)
        .ok()
        .filter(|parsed| parsed.to_string() == version)
        .map(|_| version.to_string());
    Some((name.to_string(), exact))
}

/// The packages pnpm installs itself from: `pnpm` alone from 12 (the
/// native build) and below 6.17.1, `pnpm` plus `@pnpm/exe` between.
fn engine_packages(version: &node_semver::Version) -> &'static [&'static str] {
    if version.major >= 12 || (version.major, version.minor, version.patch) < (6, 17, 1) {
        &["pnpm"]
    } else {
        &["pnpm", "@pnpm/exe"]
    }
}

fn satisfies(version: &str, range: &str) -> bool {
    match (
        node_semver::Version::parse(version),
        node_semver::Range::parse(range),
    ) {
        (Ok(v), Ok(r)) => r.satisfies(&v),
        _ => false,
    }
}

/// Bring the env document of `<lockfile_dir>/pnpm-lock.yaml` in line with
/// the pnpm pin in `<lockfile_dir>/package.json`. Rewrites only that
/// document, leaving the project document's bytes alone, and only when it
/// does not already pin the wanted pnpm. Returns the pnpm version newly
/// recorded, `None` when nothing was written.
///
/// A missing lockfile or manifest, or a project that pins no pnpm, is a
/// no-op: the env document is never removed, as pnpm keeps it after a pin
/// is dropped.
pub async fn sync_pnpm_env_lockfile(lockfile_dir: &Path) -> miette::Result<Option<String>> {
    let lockfile_path = lockfile_dir.join(LOCKFILE);
    let Ok(content) = std::fs::read_to_string(&lockfile_path) else {
        return Ok(None);
    };
    let Some(manifest) = std::fs::read_to_string(lockfile_dir.join("package.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    else {
        return Ok(None);
    };
    let Some(wanted) = wanted_pnpm(&manifest) else {
        return Ok(None);
    };
    let existing = EnvLockfile::from_lockfile_content(&content);

    // A recorded version the pin still allows is kept, so a range pin does
    // not move whenever a newer pnpm is published.
    let locked = existing.as_ref().and_then(|env| {
        let version = env
            .package_manager_dependencies()?
            .get("pnpm")?
            .version
            .clone();
        let parsed = node_semver::Version::parse(&version).ok()?;
        (satisfies(&version, &wanted.specifier)
            && env.pins(&version, engine_packages(&parsed), None))
        .then_some(version)
    });
    let version = match locked {
        Some(version) => version,
        None => match choose_version(lockfile_dir, &wanted).await? {
            Some(version) => version,
            None => return Ok(None),
        },
    };
    let parsed = node_semver::Version::parse(&version)
        .into_diagnostic()
        .wrap_err_with(|| format!("pnpm version {version}"))?;
    if parsed.major < 11 {
        return Ok(None);
    }
    let packages = engine_packages(&parsed);
    // pnpm 12 records the pin's own specifier; pnpm 11 does only on its
    // version-switching path, and records the exact version otherwise.
    let specifier_is_pin = parsed.major >= 12 || wanted.on_fail_download;
    let specifier = if specifier_is_pin {
        wanted.specifier.clone()
    } else {
        version.clone()
    };
    if existing
        .as_ref()
        .is_some_and(|env| env.pins(&version, packages, specifier_is_pin.then_some(&*specifier)))
    {
        return Ok(None);
    }

    let deps: BTreeMap<String, String> = packages
        .iter()
        .map(|name| (name.to_string(), version.clone()))
        .collect();
    let closure_manifest = aube_manifest::PackageJson {
        dependencies: deps.clone(),
        ..Default::default()
    };
    let closure = resolve(lockfile_dir, &closure_manifest, false)
        .await
        .wrap_err_with(|| format!("resolve pnpm@{version} for {LOCKFILE}"))?;
    let recorded: BTreeMap<String, EnvDependency> = deps
        .into_keys()
        .map(|name| {
            (
                name,
                EnvDependency {
                    specifier: specifier.clone(),
                    version: version.clone(),
                },
            )
        })
        .collect();
    let env = aube_lockfile::pnpm::render_package_manager_env(
        existing.as_ref(),
        recorded,
        &closure,
        &closure_manifest,
    )
    .into_diagnostic()?;
    let main = aube_lockfile::pnpm::split_env_document(&content).1;
    let combined = aube_lockfile::pnpm::compose_with_env_document(&env, main);
    aube_util::fs_atomic::atomic_write(&lockfile_path, combined.as_bytes())
        .into_diagnostic()
        .wrap_err_with(|| format!("write {}", lockfile_path.display()))?;
    Ok(Some(version))
}

/// The pnpm version a pin with nothing recorded resolves to: the
/// `packageManager` field's version when the pin allows it, the pin itself
/// when it is exact, else the newest release in the range the project's
/// `minimumReleaseAge` admits — the version pnpm itself switches to.
async fn choose_version(
    lockfile_dir: &Path,
    wanted: &WantedPnpm,
) -> miette::Result<Option<String>> {
    if let Some(exact) = &wanted.legacy_exact
        && satisfies(exact, &wanted.specifier)
    {
        return Ok(Some(exact.clone()));
    }
    if node_semver::Version::parse(&wanted.specifier)
        .is_ok_and(|v| v.to_string() == wanted.specifier)
    {
        return Ok(Some(wanted.specifier.clone()));
    }
    // A range below pnpm 11 records nothing whichever release it picks, so
    // spare the registry round trip on every install of a pnpm 10 project.
    let records = node_semver::Range::parse(">=11.0.0-0").ok();
    if node_semver::Range::parse(&wanted.specifier)
        .ok()
        .zip(records)
        .is_some_and(|(range, records)| !range.allows_any(&records))
    {
        return Ok(None);
    }
    let manifest = aube_manifest::PackageJson {
        dependencies: BTreeMap::from([("pnpm".to_string(), wanted.specifier.clone())]),
        ..Default::default()
    };
    let graph = resolve(lockfile_dir, &manifest, true)
        .await
        .wrap_err_with(|| format!("resolve pnpm@{}", wanted.specifier))?;
    Ok(graph
        .importers
        .get(".")
        .and_then(|deps| deps.iter().find(|d| d.name == "pnpm"))
        .and_then(|d| graph.packages.get(&d.dep_path))
        .map(|pkg| pkg.version.clone()))
}

/// Resolve `manifest`'s dependencies through the project's registry
/// configuration, keeping every platform's optional natives — the shape a
/// lockfile records. The project's own overrides and catalogs do not apply:
/// the closure is pnpm's, not the project's.
async fn resolve(
    lockfile_dir: &Path,
    manifest: &aube_manifest::PackageJson,
    honor_release_age: bool,
) -> miette::Result<aube_lockfile::LockfileGraph> {
    let client = Arc::new(crate::commands::make_client(lockfile_dir));
    let minimum_release_age = honor_release_age
        .then(|| {
            crate::commands::with_settings_ctx(lockfile_dir, |ctx| {
                crate::commands::install::settings::resolve_minimum_release_age(ctx, None)
            })
        })
        .flatten();
    let mut resolver = aube_resolver::Resolver::new(client)
        .with_packument_cache(crate::commands::packument_cache_dir_for_cwd(lockfile_dir))
        .with_project_root(lockfile_dir.to_path_buf())
        .with_minimum_release_age(minimum_release_age)
        .with_supported_architectures(aube_resolver::SupportedArchitectures {
            accept_all: true,
            ..Default::default()
        });
    let mut graph = resolver.resolve(manifest, None).await.into_diagnostic()?;
    crate::commands::prepare_resolved_graph_for_lockfile_write(&mut graph);
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wanted(manifest: Value) -> Option<(String, bool, Option<String>)> {
        wanted_pnpm(&manifest).map(|w| (w.specifier, w.on_fail_download, w.legacy_exact))
    }

    #[test]
    fn dev_engines_pin_persists_unless_on_fail_is_ignore() {
        let pin = |on_fail: Value| {
            let mut engine = json!({ "name": "pnpm", "version": "^12.16.1" });
            if !on_fail.is_null() {
                engine["onFail"] = on_fail;
            }
            wanted(
                json!({ "packageManager": "pnpm@12.16.1+sha512.abc", "devEngines": { "packageManager": engine } }),
            )
        };
        let legacy = Some("12.16.1".to_string());
        assert_eq!(
            pin(Value::Null),
            Some(("^12.16.1".into(), true, legacy.clone()))
        );
        assert_eq!(pin(json!("warn")), Some(("^12.16.1".into(), false, legacy)));
        assert_eq!(pin(json!("ignore")), None);
        // An array's last entry defaults to `error`, earlier ones to `ignore`.
        let array = |entries: Value| wanted(json!({ "devEngines": { "packageManager": entries } }));
        assert_eq!(
            array(json!([{ "name": "pnpm", "version": "11.28.9" }])),
            Some(("11.28.9".into(), false, None))
        );
        assert_eq!(
            array(json!([{ "name": "pnpm", "version": "11.28.9" }, { "name": "npm" }])),
            None
        );
    }

    #[test]
    fn bare_package_manager_field_persists_only_from_pnpm_12() {
        let field = |v: &str| wanted(json!({ "packageManager": v }));
        assert_eq!(
            field("pnpm@12.16.1+sha512.abc"),
            Some(("12.16.1".into(), true, Some("12.16.1".into())))
        );
        assert_eq!(field("pnpm@11.28.9"), None);
        assert_eq!(field("yarn@4.5.0"), None);
    }
}
