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
        .filter(|v| npm_range(v).is_some())
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

/// A pin's range as npm's `semver` reads it, which is how pnpm reads it. A
/// comma, which npm's grammar lacks, makes it no range, as it is to pnpm.
/// nub-core's `PmRange` reads a range through the same crate, so the version
/// a range launches and the version recorded here agree.
fn npm_range(spec: &str) -> Option<aube_semver::Range> {
    aube_semver::Range::parse(spec)
}

fn satisfies(version: &str, range: &str) -> bool {
    aube_semver::satisfies(version, range)
}

/// What the env document needs, decided without the network.
enum Plan {
    /// It already pins the wanted pnpm, or the pin is one pnpm does not
    /// record (none, `onFail: ignore`, below pnpm 11).
    Current,
    /// Record `version` (or the newest release a range pin admits, when
    /// `None`).
    Record {
        wanted: WantedPnpm,
        version: Option<String>,
    },
}

fn read_manifest(dir: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(dir.join("package.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

fn plan(manifest: &Value, existing: Option<&EnvLockfile>, target: Option<&str>) -> Plan {
    let Some(wanted) = wanted_pnpm(manifest) else {
        return Plan::Current;
    };
    // `nub pm update` names the version to record, which replaces a recorded
    // one the range still allows; it is the only caller that moves a range pin.
    let target = target
        .filter(|v| satisfies(v, &wanted.specifier))
        .map(str::to_string);
    // A recorded version the pin still allows is kept, so a range pin does
    // not move whenever a newer pnpm is published.
    let locked = target.or_else(|| {
        existing.and_then(|env| {
            let version = env
                .package_manager_dependencies()?
                .get("pnpm")?
                .version
                .clone();
            let parsed = node_semver::Version::parse(&version).ok()?;
            (satisfies(&version, &wanted.specifier)
                && env.pins(&version, engine_packages(&parsed), None))
            .then_some(version)
        })
    });
    let Some(version) = locked.or_else(|| offline_version(&wanted)) else {
        // A range below pnpm 11 records nothing whichever release it picks.
        let records = aube_semver::parse_range(">=11.0.0-0").ok();
        let below_11 = npm_range(&wanted.specifier)
            .zip(records)
            .is_some_and(|(range, records)| !range.overlaps(&records));
        return if below_11 {
            Plan::Current
        } else {
            Plan::Record {
                wanted,
                version: None,
            }
        };
    };
    match node_semver::Version::parse(&version) {
        Ok(parsed) if parsed.major >= 11 => {}
        _ => return Plan::Current,
    }
    if existing.is_some_and(|env| {
        let (packages, specifier) = recorded_shape(&wanted, &version);
        env.pins(&version, packages, specifier.as_deref())
    }) {
        return Plan::Current;
    }
    Plan::Record {
        wanted,
        version: Some(version),
    }
}

/// The packages recorded for `version`, and the specifier a lockfile must
/// carry to count as current — `None` where pnpm accepts any. pnpm 12
/// records the pin's own specifier; pnpm 11 does only on its
/// version-switching path (`onFail: download`), and the exact version on
/// its sync path, where it does not check the specifier.
fn recorded_shape(wanted: &WantedPnpm, version: &str) -> (&'static [&'static str], Option<String>) {
    let parsed = node_semver::Version::parse(version).expect("planned versions parse");
    let specifier_is_pin = parsed.major >= 12 || wanted.on_fail_download;
    (
        engine_packages(&parsed),
        specifier_is_pin.then(|| wanted.specifier.clone()),
    )
}

/// Whether `<lockfile_dir>/pnpm-lock.yaml`'s env document already records
/// the pnpm `<lockfile_dir>/package.json` pins — what
/// [`sync_pnpm_env_lockfile`] would leave alone. No network, and only the
/// lockfile's leading document is read, so the warm install path can ask.
pub fn env_document_is_current(lockfile_dir: &Path) -> bool {
    let Some(manifest) = read_manifest(lockfile_dir) else {
        return true;
    };
    if wanted_pnpm(&manifest).is_none() {
        return true;
    }
    let path = lockfile_dir.join(LOCKFILE);
    let Ok(file) = std::fs::File::open(&path) else {
        return true;
    };
    let env = aube_lockfile::pnpm::read_env_document(file)
        .ok()
        .flatten()
        .and_then(|text| EnvLockfile::parse(&text));
    matches!(plan(&manifest, env.as_ref(), None), Plan::Current)
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
pub async fn sync_pnpm_env_lockfile(
    lockfile_dir: &Path,
    network_mode: aube_registry::NetworkMode,
) -> miette::Result<Option<String>> {
    record_pnpm_env_lockfile(lockfile_dir, None, network_mode).await
}

/// [`sync_pnpm_env_lockfile`], recording `target` when the pin allows it
/// rather than keeping a recorded version, and creating the lockfile when
/// there is none. A `target` the pin does not allow is ignored.
pub async fn record_pnpm_env_lockfile(
    lockfile_dir: &Path,
    target: Option<&str>,
    network_mode: aube_registry::NetworkMode,
) -> miette::Result<Option<String>> {
    let lockfile_path = lockfile_dir.join(LOCKFILE);
    // With a target and no lockfile, write what pnpm's own first command
    // writes there: the env document alone.
    let Some(content) = read_lockfile(&lockfile_path, target.is_some())? else {
        return Ok(None);
    };
    let Some(manifest) = read_manifest(lockfile_dir) else {
        return Ok(None);
    };
    let existing = EnvLockfile::from_lockfile_content(&content);
    let (wanted, version) = match plan(&manifest, existing.as_ref(), target) {
        Plan::Current => return Ok(None),
        Plan::Record {
            wanted,
            version: Some(version),
        } => (wanted, version),
        Plan::Record {
            wanted,
            version: None,
        } => {
            let Some(version) =
                newest_in_range(lockfile_dir, &wanted.specifier, network_mode).await?
            else {
                return Ok(None);
            };
            if node_semver::Version::parse(&version).map_or(true, |v| v.major < 11) {
                return Ok(None);
            }
            (wanted, version)
        }
    };
    let (packages, specifier) = recorded_shape(&wanted, &version);
    let specifier = specifier.unwrap_or_else(|| version.clone());

    let deps: BTreeMap<String, String> = packages
        .iter()
        .map(|name| (name.to_string(), version.clone()))
        .collect();
    let closure_manifest = aube_manifest::PackageJson {
        dependencies: deps.clone(),
        ..Default::default()
    };
    let closure = resolve(lockfile_dir, &closure_manifest, false, network_mode)
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

/// The lockfile's text; with `create`, an empty one when the file is missing.
/// Only a missing file counts as no lockfile: any other read failure is an
/// error, because writing over a file that could not be read would erase its
/// project document.
fn read_lockfile(path: &Path, create: bool) -> miette::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(create.then(String::new)),
        Err(e) => Err(e)
            .into_diagnostic()
            .wrap_err_with(|| format!("read {}", path.display())),
    }
}

/// The version a pin with nothing recorded resolves to offline: the
/// `packageManager` field's version when the pin allows it, else the pin
/// itself when it is exact.
fn offline_version(wanted: &WantedPnpm) -> Option<String> {
    if let Some(exact) = &wanted.legacy_exact
        && satisfies(exact, &wanted.specifier)
    {
        return Some(exact.clone());
    }
    node_semver::Version::parse(&wanted.specifier)
        .is_ok_and(|v| v.to_string() == wanted.specifier)
        .then(|| wanted.specifier.clone())
}

/// The newest release in `range` the project's `minimumReleaseAge` admits —
/// the version pnpm itself switches to for a range pin.
async fn newest_in_range(
    lockfile_dir: &Path,
    range: &str,
    network_mode: aube_registry::NetworkMode,
) -> miette::Result<Option<String>> {
    let manifest = aube_manifest::PackageJson {
        dependencies: BTreeMap::from([("pnpm".to_string(), range.to_string())]),
        ..Default::default()
    };
    let graph = resolve(lockfile_dir, &manifest, true, network_mode)
        .await
        .wrap_err_with(|| format!("resolve pnpm@{range}"))?;
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
    network_mode: aube_registry::NetworkMode,
) -> miette::Result<aube_lockfile::LockfileGraph> {
    // The install's own network mode: `--offline` answers from the
    // metadata cache or fails, never reaching the registry.
    let client =
        Arc::new(crate::commands::make_client(lockfile_dir).with_network_mode(network_mode));
    let minimum_release_age = honor_release_age
        .then(|| {
            crate::commands::with_settings_ctx(lockfile_dir, |ctx| {
                crate::commands::install::settings::resolve_minimum_release_age(ctx, None)
            })
        })
        .flatten();
    let mut resolver = aube_resolver::Resolver::new(client)
        .with_packument_cache(crate::commands::packument_cache_dir_for_cwd(lockfile_dir))
        // The full-document cache too: an age or trust check needs publish
        // times, and without this cache the resolver fetches them through
        // an uncached path that does not honor `--offline`.
        .with_packument_full_cache(crate::commands::packument_full_cache_dir_for_cwd(
            lockfile_dir,
        ))
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

    /// Only a range no pnpm 11 can satisfy records nothing. The check is on
    /// bounds: npm's `intersects` would call `11.1.0-rc.1` disjoint from
    /// `>=11.0.0-0`, though pnpm 11.1.0-rc.1 records its pin.
    #[test]
    fn a_range_reaching_pnpm_11_plans_a_record() {
        let plans_record = |range: &str| {
            let manifest = json!({ "devEngines": { "packageManager":
                { "name": "pnpm", "version": range, "onFail": "download" } } });
            matches!(plan(&manifest, None, None), Plan::Record { .. })
        };
        assert!(plans_record("^10.0.0 || 11.1.0-rc.1"));
        assert!(plans_record("^11.0.0"));
        assert!(!plans_record("^10.0.0"));
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
    fn a_target_moves_a_range_pin_that_the_lockfile_already_satisfies() {
        let manifest = json!({ "devEngines": { "packageManager":
            { "name": "pnpm", "version": "^11.20.0", "onFail": "warn" } } });
        let entry = |name: &str| {
            format!("      '{name}':\n        specifier: 11.20.0\n        version: 11.20.0\n")
        };
        let record = |name: &str| {
            format!("  '{name}@11.20.0':\n    resolution: {{integrity: sha512-AAAA}}\n\n")
        };
        let snapshot = |name: &str| format!("  '{name}@11.20.0': {{}}\n\n");
        let env = format!(
            "---\nlockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    packageManagerDependencies:\n{}{}\npackages:\n\n{}{}snapshots:\n\n{}{}---\n",
            entry("@pnpm/exe"),
            entry("pnpm"),
            record("@pnpm/exe"),
            record("pnpm"),
            snapshot("@pnpm/exe"),
            snapshot("pnpm"),
        );
        let existing = EnvLockfile::from_lockfile_content(&env).expect("env document");
        let recorded = |target| match plan(&manifest, Some(&existing), target) {
            Plan::Current => None,
            Plan::Record { version, .. } => Some(version),
        };
        assert_eq!(recorded(None), None, "a satisfying record is kept");
        assert_eq!(recorded(Some("11.29.0")), Some(Some("11.29.0".to_string())));
        assert_eq!(recorded(Some("11.20.0")), None, "already recorded");
        assert_eq!(
            recorded(Some("12.0.0")),
            None,
            "a target outside the range is ignored"
        );
    }

    /// A lockfile that exists but can't be read is an error, never an absent
    /// lockfile to create: replacing it would erase its project document.
    #[cfg(unix)]
    #[tokio::test]
    async fn only_a_missing_lockfile_is_created() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "devEngines": { "packageManager": { "name": "pnpm", "version": "^11.20.0", "onFail": "warn" } } }"#,
        )
        .unwrap();
        let lockfile = dir.path().join(LOCKFILE);
        let bytes = b"lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n";
        std::fs::write(&lockfile, bytes).unwrap();
        std::fs::set_permissions(&lockfile, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&lockfile).is_ok() {
            return; // running as root: the file stays readable
        }
        let read = read_lockfile(&lockfile, true);
        let result = record_pnpm_env_lockfile(
            dir.path(),
            Some("11.29.0"),
            aube_registry::NetworkMode::Offline,
        )
        .await;
        std::fs::set_permissions(&lockfile, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read.is_err(), "an unreadable lockfile is not a missing one");
        assert!(result.is_err());
        assert_eq!(std::fs::read(&lockfile).unwrap(), bytes);
        // Only a missing file reads as no lockfile, and only then is one created.
        let missing = dir.path().join("missing.yaml");
        assert_eq!(read_lockfile(&missing, true).unwrap().as_deref(), Some(""));
        assert_eq!(read_lockfile(&missing, false).unwrap(), None);
        assert_eq!(
            read_lockfile(&lockfile, true)
                .unwrap()
                .as_deref()
                .map(str::as_bytes),
            Some(&bytes[..])
        );
    }

    /// Verdicts from npm's `semver` 7.8.6, which pnpm reads ranges with.
    #[test]
    fn pin_ranges_match_npm_semver() {
        assert!(satisfies("11.29.0", ">=9 <=11"));
        assert!(!satisfies("12.0.0", ">=9 <=11"));
        assert!(satisfies("11.2.9", "<=11.2"));
        assert!(!satisfies("11.3.0", "<=11.2.x"));
        assert!(satisfies("11.29.5", "11.20.0 - 11.29"));
        assert!(satisfies("12.23.0", "^11.20.0 || ^12.0.0"));
        // npm rejects a comma, so pnpm records nothing for such a pin.
        assert!(!satisfies("11.29.0", ">=11.25.0, <12.0.0"));
        // npm reads a set whose comparators cannot all hold as empty.
        assert!(satisfies("11.15.0", ">=11.29.0 <=11.20.* || 11.15.0"));
        assert!(!satisfies("11.20.0", ">=11.29.0 <=11.20.* || 11.15.0"));
        assert!(!satisfies("11.29.0", ">=11.29.0 <=11.20.0"));
        // A range npm rejects pins nothing; no part's arithmetic overflows.
        for spec in [
            "<=11.029",
            "1.02.3",
            "<=18446744073709551615",
            "<=9007199254740991",
        ] {
            assert!(!satisfies("11.29.0", spec), "{spec}");
            assert_eq!(
                wanted(json!({ "devEngines": { "packageManager":
                    { "name": "pnpm", "version": spec } } })),
                None,
                "{spec}"
            );
        }
        assert_eq!(
            wanted(json!({ "devEngines": { "packageManager":
                { "name": "pnpm", "version": ">=11.25.0, <12.0.0" } } })),
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
