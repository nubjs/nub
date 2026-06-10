//! `nub pm use nub` — the full switch into nub identity — and the yaml
//! regeneration half of `nub pm use pnpm` (the exact reverse).
//!
//! Spec: wiki/commands/pm/identity-policy.md (`pm use`, the four axioms) +
//! wiki/commands/pm/workspace-yaml-migration.md (the exhaustive key table and
//! its Bun-names addendum). The two-mode model in one paragraph: compat mode
//! (default) plays the incumbent PM's role completely — its lockfile, its
//! config surface, its grammar. `pm use nub` is the explicit graduation:
//! `packageManager: "nub@<exact>"` + `devEngines.packageManager
//! {name:"nub", version:"^<ver>", onFail:"warn"}`, the lockfile renamed (or
//! converted) to `lock.yaml` (pnpm-v9 bytes, generic name), and
//! `pnpm-workspace.yaml` ALWAYS migrated and deleted:
//!
//! - resolution-bearing keys → `package.json` under ecosystem-standard
//!   top-level names (`workspaces` incl. the object form,
//!   `workspaces.catalog(s)`, `overrides`, `patchedDependencies`, the
//!   three-state `allowBuilds` map, `auditConfig`);
//! - settings → `.npmrc` (the same vocabulary, kebab spellings — one engine
//!   reads both homes, so this is a mechanical move, not a translation);
//! - engine-unsupported keys → warn-drop naming each (injected-deps state
//!   REFUSES the migration outright);
//! - everything else (transient flags persisted in yaml, keys with no
//!   surviving home) → the loud warn tail. Nothing is ever dropped silently.
//!
//! The `package.json#pnpm.*` namespace is migrated through the same table and
//! removed — under nub identity that namespace is unread (config surface
//! follows role). `use pnpm` reverses every move: lockfile renamed back,
//! `pnpm-workspace.yaml` regenerated from the package.json homes, the
//! migrated top-level keys removed, `packageManager: "pnpm@<resolved>"`
//! restored (see [`regenerate_workspace_yaml`]).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use super::use_align::{self, AlignPlan, NUB_LOCKFILE};

/// Yaml/settings keys whose post-migration home is `.npmrc`, per the audit
/// table. Spellings are the camelCase yaml forms; emission converts to the
/// kebab alias via [`to_kebab_case`] (aube's settings codegen registers both
/// at build time, so the kebab line always parses). Objects and lists are
/// emitted as JSON values — the engine's npmrc readers parse both
/// (`object_setting_from_npmrc`, `parse_string_list`).
const NPMRC_KEYS: &[&str] = &[
    // data-shaped keys whose only engine-readable post-yaml home is .npmrc
    // (Bun-names addendum: no manifest-commons squatting)
    "packageExtensions",
    "allowedDeprecatedVersions",
    "peerDependencyRules",
    "supportedArchitectures",
    "ignoredOptionalDependencies",
    // settings, alphabetical-ish, straight from the table
    "autoInstallPeers",
    "blockExoticSubdeps",
    "ca",
    "cacheDir",
    "cafile",
    "catalogMode",
    "cert",
    "childConcurrency",
    "ci",
    "cleanupUnusedCatalogs",
    "color",
    "dangerouslyAllowAllBuilds",
    "dedupeDirectDeps",
    "dedupePeerDependents",
    "dedupePeers",
    "deployAllFiles",
    "dlxCacheMaxAge",
    "enableGlobalVirtualStore",
    "enableModulesDir",
    "enablePrePostScripts",
    "engineStrict",
    "excludeLinksFromLockfile",
    "extendNodePath",
    "fetchMinSpeedKiBps",
    "fetchTimeout",
    "fetchWarnTimeoutMs",
    "gitBranchLockfile",
    "gitShallowHosts",
    "globalBinDir",
    "globalDir",
    "hoist",
    "hoistPattern",
    "hoistWorkspacePackages",
    "httpProxy",
    "httpsProxy",
    "ignoreCompatibilityDb",
    "ignoreScripts",
    "key",
    "linkWorkspacePackages",
    "localAddress",
    "lockfile",
    "lockfileDir",
    "lockfileIncludeTarballUrl",
    "loglevel",
    "maxsockets",
    "mergeGitBranchLockfilesBranchPattern",
    "minimumReleaseAge",
    "minimumReleaseAgeExclude",
    "minimumReleaseAgeStrict",
    "modulesCacheMaxAge",
    "modulesDir",
    "networkConcurrency",
    "nodeLinker",
    "nodeOptions",
    "nodeVersion",
    "noProxy",
    "noproxy",
    "npmPath",
    "npmrcAuthFile",
    "nodeDownloadMirrors",
    "optimisticRepeatInstall",
    "packageImportMethod",
    "peersSuffixMaxLength",
    "preferFrozenLockfile",
    "preferSymlinkedExecutables",
    "publicHoistPattern",
    "recursiveInstall",
    "registry",
    "registrySupportsTimeField",
    "requiredScripts",
    "resolutionMode",
    "resolvePeersFromWorkspaceRoot",
    "savePrefix",
    "saveWorkspaceProtocol",
    "scriptShell",
    "shamefullyHoist",
    "sharedWorkspaceLockfile",
    "shellEmulator",
    "sideEffectsCache",
    "sideEffectsCacheReadonly",
    "stateDir",
    "storeDir",
    "strictDepBuilds",
    "strictPeerDependencies",
    "strictSsl",
    "strictStorePkgContentCheck",
    "symlink",
    "tag",
    "trustPolicy",
    "trustPolicyExclude",
    "trustPolicyIgnoreAfter",
    "unsafePerm",
    "updateNotifier",
    "useBetaCli",
    "useStderr",
    "verifyDepsBeforeRun",
    "verifyStoreIntegrity",
    "virtualStoreDir",
    "virtualStoreDirMaxLength",
    "virtualStoreOnly",
    // npm-standard transient/publish keys: migrating to .npmrc is safe for
    // sibling tools even where the engine ignores them (table note).
    "umask",
    "depth",
    "otp",
    "scope",
    "access",
    "provenance",
    // aube-only keys a yaml the engine wrote may carry; all have npmrc homes.
    "defaultLockfileFormat",
    "defaultTrust",
    "deprecationWarnings",
    "advisoryCheck",
    "advisoryCheckOnInstall",
    "advisoryBloomCheck",
    "advisoryCheckEveryInstall",
    "lowDownloadThreshold",
    "allowedUnpopularPackages",
    "securityScanner",
    "paranoid",
    "jailBuilds",
    "jailBuildExclusions",
    "strictStoreIntegrity",
    "linkConcurrency",
    "hoistingLimits",
    "disableGlobalVirtualStoreForPackages",
];

/// Keys the engine has zero implementation for: warn-drop, naming each and
/// carrying the remedy/context note in the summary. (`injectWorkspacePackages`
/// is here for the *setting*; actual injected-deps state — any
/// `dependenciesMeta.injected` — REFUSES the whole migration, see
/// [`refuse_on_injected_deps`].)
const ENGINE_UNSUPPORTED: &[(&str, &str)] = &[
    (
        "configDependencies",
        "config-only install dependencies are not implemented",
    ),
    (
        "packageConfigs",
        "per-project setting override sets (pnpm v11) are not implemented",
    ),
    (
        "namedRegistries",
        "named registry aliases are not implemented — inline the URLs as registry= / @scope:registry= in .npmrc",
    ),
    (
        "injectWorkspacePackages",
        "injected workspace dependencies are not implemented",
    ),
    (
        "dedupeInjectedDeps",
        "injected workspace dependencies are not implemented",
    ),
    (
        "syncInjectedDepsAfterScripts",
        "injected workspace dependencies are not implemented",
    ),
    (
        "useNodeVersion",
        "nub owns Node provisioning — pin Node via devEngines.runtime / .node-version instead",
    ),
    (
        "executionEnv",
        "nub owns Node provisioning — pin Node via devEngines.runtime / .node-version instead",
    ),
    (
        "runtime",
        "nub owns Node provisioning — pin Node via devEngines.runtime / .node-version instead",
    ),
    (
        "runtimeOnFail",
        "nub owns Node provisioning — see `runtime`",
    ),
    (
        "allowUnusedPatches",
        "unreferenced patch files always error in the engine",
    ),
    (
        "fetchingConcurrency",
        "use network-concurrency in .npmrc instead",
    ),
    (
        "mergeGitBranchLockfiles",
        "only the pattern form (mergeGitBranchLockfilesBranchPattern) is supported",
    ),
];

/// Keys with no destination: transient CLI flags persisted in yaml, features
/// without a surviving home, and the keys still awaiting a design decision
/// (`includeWorkspaceRoot`, `pnpmfile`, `workspaceConcurrency`, `saveExact`,
/// `jailBuildPermissions`, `pnpmfilePath` — conservative warn-tail until
/// ruled). Listed loudly in the summary, never silently dropped.
const WARN_TAIL: &[&str] = &[
    "agent",
    "aggregateOutput",
    "allowSameVersion",
    "auditLevel",
    "bail",
    "changedFilesIgnorePattern",
    "commitHooks",
    "configDir",
    "dev",
    "dir",
    "disallowWorkspaceCycles",
    "embedReadme",
    "failIfNoMatch",
    "filter",
    "filterProd",
    "frozenLockfile",
    "gitChecks",
    "gitTagVersion",
    "globalPath",
    "globalPnpmfile",
    "globalVirtualStoreDir",
    "ignorePnpmfile",
    "ignoreWorkspace",
    "ignoreWorkspaceCycles",
    "ignoreWorkspaceRootCheck",
    "includeWorkspaceRoot",
    "initPackageManager",
    "initType",
    "jailBuildPermissions",
    "legacyDirFiltering",
    "lockfileOnly",
    "message",
    "minimumReleaseAgeIgnoreMissingTime",
    "offline",
    "packDestination",
    "packGzipLevel",
    "patchesDir",
    "pmOnFail",
    "pnpmfile",
    "pnpmfilePath",
    "preferOffline",
    "preferWorkspacePackages",
    "preserveAbsolutePaths",
    "production",
    "publishBranch",
    "reporter",
    "reporterHidePrefix",
    "save",
    "saveCatalogName",
    "saveDev",
    "saveExact",
    "saveOptional",
    "savePeer",
    "saveProd",
    "scriptsPrependNodePath",
    "signGitTag",
    "sort",
    "stream",
    "tagVersionPrefix",
    "testPattern",
    "trustLockfile",
    "userAgent",
    "workspaceConcurrency",
    "workspacePackages",
    "workspaceRoot",
    "yes",
];

/// Camel → kebab, byte-for-byte the algorithm in aube-settings' codegen
/// (`vendor/aube/crates/aube-settings/build.rs::to_kebab_case`), so every
/// emitted `.npmrc` key is exactly the kebab alias the engine registered.
fn to_kebab_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev_lower = false;
    for c in s.chars() {
        if c == '.' {
            out.push(c);
            prev_lower = false;
        } else if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else {
            out.push(c);
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        }
    }
    out
}

/// A `.npmrc` value for a migrated setting: scalars verbatim, lists/objects
/// as JSON (both parse — `parse_string_list` accepts JSON-ish arrays,
/// `object_setting_from_npmrc` parses JSON objects).
fn npmrc_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// The planned migration of one source (`pnpm-workspace.yaml` ∪
/// `package.json#pnpm.*`) into the nub-mode homes. Pure data — computed
/// before any write so refusals fire first and the summary can be exact.
#[derive(Debug, Default)]
pub(crate) struct YamlMigration {
    /// `workspaces` packages array (yaml `packages`).
    pub packages: Option<Value>,
    /// `workspaces.catalog` (default catalog).
    pub catalog: Option<Value>,
    /// `workspaces.catalogs` (named catalogs).
    pub catalogs: Option<Value>,
    /// Top-level `overrides` entries.
    pub overrides: Option<Map<String, Value>>,
    /// Top-level `patchedDependencies` entries (paths verbatim — the yaml
    /// and package.json live in the same directory, so relative patch paths
    /// stay correct).
    pub patched_dependencies: Option<Map<String, Value>>,
    /// Top-level three-state `allowBuilds` map. Folds the legacy trio:
    /// `onlyBuiltDependencies` → `true`, `neverBuiltDependencies` /
    /// `ignoredBuiltDependencies` → `false` (asked-and-answered, not
    /// security denial). Explicit `allowBuilds` entries win the fold.
    pub allow_builds: Option<Map<String, Value>>,
    /// Top-level `auditConfig` (aube's existing extension home).
    pub audit_config: Option<Value>,
    /// `.npmrc` lines to append, as `(key, value)` (key already kebab).
    pub npmrc: Vec<(String, String)>,
    /// Engine-unsupported keys found, as `key — note` summary lines.
    pub dropped: Vec<String>,
    /// Warn-tail keys found (no destination), with provenance.
    pub tail: Vec<String>,
    /// Extra advisories (pnp linker, non-default modulesDir, …).
    pub notes: Vec<String>,
}

/// Categorize every key of the merged source per the audit table. `source`
/// is the yaml mapping with `package.json#pnpm.*` entries merged UNDER it
/// (yaml wins on duplicate keys — under pnpm v11 the yaml was the live
/// value). Refusals (`onlyBuiltDependenciesFile`) bail here, before any
/// write anywhere.
pub(crate) fn plan_migration(source: &Map<String, Value>) -> Result<YamlMigration> {
    let mut m = YamlMigration::default();
    let mut allow_builds: Map<String, Value> = Map::new();
    let mut supported_arch: Map<String, Value> = source
        .get("supportedArchitectures")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if source.contains_key("onlyBuiltDependenciesFile") {
        bail!(
            "pnpm-workspace.yaml sets `onlyBuiltDependenciesFile` — an external build \
             allowlist file the engine can't consume. Inline its package names into \
             `onlyBuiltDependencies` (or an `allowBuilds` map), then rerun `nub pm use nub`."
        );
    }

    // Legacy build trio folds into the three-state allowBuilds map first;
    // explicit allowBuilds entries overwrite the folds below.
    for (key, value) in [
        ("onlyBuiltDependencies", true),
        ("neverBuiltDependencies", false),
        ("ignoredBuiltDependencies", false),
    ] {
        if let Some(names) = source.get(key).and_then(Value::as_array) {
            for name in names.iter().filter_map(Value::as_str) {
                allow_builds.insert(name.to_string(), Value::Bool(value));
            }
        }
    }
    if let Some(map) = source.get("allowBuilds").and_then(Value::as_object) {
        for (k, v) in map {
            allow_builds.insert(k.clone(), v.clone());
        }
    }

    for (key, value) in source {
        match key.as_str() {
            // structural / resolution-bearing → package.json
            "packages" => m.packages = Some(value.clone()),
            "catalog" => m.catalog = Some(value.clone()),
            "catalogs" => m.catalogs = Some(value.clone()),
            "overrides" => m.overrides = value.as_object().cloned(),
            "patchedDependencies" => m.patched_dependencies = value.as_object().cloned(),
            "auditConfig" => m.audit_config = Some(value.clone()),
            // handled by the folds above
            "allowBuilds"
            | "onlyBuiltDependencies"
            | "neverBuiltDependencies"
            | "ignoredBuiltDependencies" => {}
            // per-axis architecture filters fold into supportedArchitectures
            "cpu" | "os" | "libc" => {
                supported_arch.insert(key.clone(), value.clone());
            }
            "supportedArchitectures" => {} // seeded above
            // registry map explodes into individual npmrc keys; ${VAR} refs
            // survive verbatim (both engines env-substitute at read time).
            "registries" => {
                if let Some(map) = value.as_object() {
                    for (scope, url) in map {
                        let k = if scope == "default" {
                            "registry".to_string()
                        } else if scope.starts_with('@') {
                            format!("{scope}:registry")
                        } else {
                            format!("@{scope}:registry")
                        };
                        m.npmrc.push((k, npmrc_value(url)));
                    }
                }
            }
            // only one subkey exists in both engines; dotted npmrc spelling
            "updateConfig" => {
                if let Some(ignore) = value.get("ignoreDependencies") {
                    m.npmrc.push((
                        "update-config.ignore-dependencies".into(),
                        npmrc_value(ignore),
                    ));
                }
            }
            _ if ENGINE_UNSUPPORTED.iter().any(|(k, _)| k == key) => {
                let note = ENGINE_UNSUPPORTED
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, n)| *n)
                    .unwrap_or_default();
                m.dropped.push(format!("{key} — {note}"));
            }
            _ if NPMRC_KEYS.contains(&key.as_str()) => {
                if key == "nodeLinker" && value.as_str() == Some("pnp") {
                    m.notes.push(
                        "node-linker=pnp migrated, but the pnp linker is not implemented — \
                         installs will use the isolated layout"
                            .into(),
                    );
                }
                if key == "modulesDir" && value.as_str().is_some_and(|v| v != "node_modules") {
                    m.notes.push(
                        "modulesDir is accepted but only its default (node_modules) is honored"
                            .into(),
                    );
                }
                m.npmrc.push((to_kebab_case(key), npmrc_value(value)));
            }
            _ if WARN_TAIL.contains(&key.as_str()) => m.tail.push(key.clone()),
            // pnpm assigns ANY camelCase key onto its config blindly; nub
            // can't map what it can't name — list it loudly.
            other => m.tail.push(format!("{other} (unrecognized key)")),
        }
    }

    if !allow_builds.is_empty() {
        m.allow_builds = Some(allow_builds);
    }
    // The folded supportedArchitectures map (cpu/os/libc absorbed) is the
    // single emission point — the literal match arms above keep the plain
    // NPMRC_KEYS path from double-emitting it.
    if !supported_arch.is_empty() {
        m.npmrc.push((
            "supported-architectures".into(),
            npmrc_value(&Value::Object(supported_arch)),
        ));
    }
    m.npmrc.sort();
    m.npmrc.dedup();
    m.tail.sort();
    m.dropped.sort();
    Ok(m)
}

/// Refuse the migration when injected-deps state exists anywhere: any
/// `dependenciesMeta.<dep>.injected: true` in the root or a workspace member
/// manifest. The engine has no injected-deps implementation, so migrating
/// would silently change install semantics — exactly what `use` must never
/// do. Members are enumerated through the engine's own workspace discovery
/// (the yaml still exists at this point).
pub(crate) fn refuse_on_injected_deps(root: &Path) -> Result<()> {
    let mut offenders: Vec<String> = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    if let Ok(members) = aube_workspace::find_workspace_packages(root) {
        dirs.extend(members);
    }
    for dir in dirs {
        let manifest_path = dir.join("package.json");
        let Ok(content) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        let injected = manifest
            .get("dependenciesMeta")
            .and_then(Value::as_object)
            .is_some_and(|meta| {
                meta.values()
                    .any(|m| m.get("injected").and_then(Value::as_bool) == Some(true))
            });
        if injected {
            offenders.push(manifest_path.display().to_string());
        }
    }
    if !offenders.is_empty() {
        bail!(
            "this workspace uses injected dependencies (dependenciesMeta.injected in {}) — \
             not implemented by nub's engine, so `nub pm use nub` would change install \
             semantics. Staying on pnpm is the right call here.",
            offenders.join(", ")
        );
    }
    Ok(())
}

/// Apply the migration's package.json half onto the (already parsed) root
/// manifest object, plus the identity fields. Merge rules preserve the
/// pre-switch EFFECTIVE config:
///
/// - `workspaces` membership + catalogs: the yaml was authoritative under
///   pnpm (it shadows `package.json#workspaces`), so yaml values overwrite —
///   any differing pre-existing value is named in the returned notes.
/// - `overrides` / `patchedDependencies` / `allowBuilds` / `auditConfig`:
///   per-key insert; an existing top-level entry wins (the engine's merge
///   already ranked top-level above the yaml), conflicts named.
/// - `pnpm` namespace: removed (migrated through the same table by the
///   caller; unread under nub identity).
///
/// `workspaces` lands as a plain array when only membership exists, and as
/// the object form (`{packages?, catalog?, catalogs?}`) when catalogs are
/// present — packages-less object for single-package catalog repos.
pub(crate) fn apply_manifest_edits(
    obj: &mut Map<String, Value>,
    m: &YamlMigration,
    nub_version: &str,
) -> Vec<String> {
    let mut notes = Vec::new();

    obj.insert(
        "packageManager".into(),
        Value::String(format!("nub@{nub_version}")),
    );
    // devEngines.packageManager: created (this verb is the one sanctioned
    // creator), replacing any prior entry wholesale; sibling devEngines
    // entries (runtime/os/…) survive.
    let dev = obj
        .entry("devEngines")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(dev) = dev.as_object_mut() {
        dev.insert(
            "packageManager".into(),
            json!({ "name": "nub", "version": format!("^{nub_version}"), "onFail": "warn" }),
        );
    }

    // workspaces: array vs object form.
    let has_catalogs = m.catalog.is_some() || m.catalogs.is_some();
    if m.packages.is_some() || has_catalogs {
        let existing = obj.get("workspaces").cloned();
        if let Some(prev) = &existing {
            let prev_packages = match prev {
                Value::Array(_) => Some(prev.clone()),
                Value::Object(o) => o.get("packages").cloned(),
                _ => None,
            };
            if let (Some(prev_p), Some(new_p)) = (&prev_packages, &m.packages)
                && prev_p != new_p
            {
                notes.push(
                    "workspaces: pnpm-workspace.yaml packages replaced a differing \
                     package.json#workspaces value (the yaml was authoritative under pnpm)"
                        .into(),
                );
            }
        }
        let packages = m.packages.clone().or_else(|| {
            existing.as_ref().and_then(|prev| match prev {
                Value::Array(_) => Some(prev.clone()),
                Value::Object(o) => o.get("packages").cloned(),
                _ => None,
            })
        });
        let value = if has_catalogs {
            let mut ws = Map::new();
            if let Some(p) = packages {
                ws.insert("packages".into(), p);
            }
            if let Some(c) = &m.catalog {
                ws.insert("catalog".into(), c.clone());
            }
            if let Some(c) = &m.catalogs {
                ws.insert("catalogs".into(), c.clone());
            }
            Value::Object(ws)
        } else {
            packages.unwrap_or_else(|| Value::Array(vec![]))
        };
        obj.insert("workspaces".into(), value);
    }

    for (key, entries) in [
        ("overrides", &m.overrides),
        ("patchedDependencies", &m.patched_dependencies),
        ("allowBuilds", &m.allow_builds),
    ] {
        let Some(entries) = entries else { continue };
        let target = obj.entry(key).or_insert_with(|| Value::Object(Map::new()));
        let Some(target) = target.as_object_mut() else {
            notes.push(format!(
                "{key}: existing non-object package.json value left untouched; yaml \
                 entries NOT migrated — resolve by hand"
            ));
            continue;
        };
        for (k, v) in entries {
            match target.get(k) {
                Some(existing) if existing != v => notes.push(format!(
                    "{key}.{k}: package.json already sets {existing} — kept (top-level wins); \
                     yaml value {v} dropped"
                )),
                Some(_) => {}
                None => {
                    target.insert(k.clone(), v.clone());
                }
            }
        }
    }
    if let Some(audit) = &m.audit_config
        && obj.get("auditConfig").is_none_or(|prev| prev == audit)
    {
        obj.insert("auditConfig".into(), audit.clone());
    } else if m.audit_config.is_some() {
        notes.push("auditConfig: package.json already sets a differing value — kept".into());
    }

    obj.remove("pnpm");
    notes
}

/// Normalized key set of an existing `.npmrc` body — used to skip (and name)
/// migrated keys the file already sets. In the engine's precedence `.npmrc`
/// already outranked the yaml, so skipping preserves the effective value.
fn npmrc_existing_keys(content: &str) -> BTreeSet<String> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                return None;
            }
            line.split_once('=').map(|(k, _)| to_kebab_case(k.trim()))
        })
        .collect()
}

/// Append migrated settings to `<root>/.npmrc` (created if absent), skipping
/// keys the file already sets. Returns `(appended, skipped)` lines for the
/// summary.
pub(crate) fn append_npmrc(
    root: &Path,
    lines: &[(String, String)],
) -> Result<(Vec<String>, Vec<String>)> {
    let path = root.join(".npmrc");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let existing_keys = npmrc_existing_keys(&existing);
    let mut appended = Vec::new();
    let mut skipped = Vec::new();
    let mut body = String::new();
    for (key, value) in lines {
        if existing_keys.contains(&to_kebab_case(key)) {
            skipped.push(key.clone());
            continue;
        }
        body.push_str(&format!("{key}={value}\n"));
        appended.push(format!("{key}={value}"));
    }
    if !body.is_empty() {
        let mut content = existing.clone();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&body);
        std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok((appended, skipped))
}

/// Parse `<root>/pnpm-workspace.yaml` into a JSON object, when present.
/// Non-mapping yaml (empty file, a bare scalar) reads as an empty mapping.
pub(crate) fn read_workspace_yaml(root: &Path) -> Result<Option<Map<String, Value>>> {
    let path = root.join("pnpm-workspace.yaml");
    if !path.is_file() {
        return Ok(None);
    }
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
    let json = serde_json::to_value(yaml)
        .map_err(|e| anyhow::anyhow!("converting {}: {e}", path.display()))?;
    Ok(Some(json.as_object().cloned().unwrap_or_default()))
}

/// The merged migration source: yaml keys ∪ `package.json#pnpm.*` keys, yaml
/// winning duplicates (it was the live value under pnpm v11). Returns the
/// source plus conflict notes for the summary.
pub(crate) fn merged_source(
    yaml: Option<&Map<String, Value>>,
    pnpm_ns: Option<&Map<String, Value>>,
) -> (Map<String, Value>, Vec<String>) {
    let mut merged = yaml.cloned().unwrap_or_default();
    let mut notes = Vec::new();
    if let Some(ns) = pnpm_ns {
        for (k, v) in ns {
            match merged.get(k) {
                Some(existing) if existing != v => notes.push(format!(
                    "package.json#pnpm.{k} differs from the pnpm-workspace.yaml value — \
                     the yaml value migrated (it was authoritative under pnpm v11)"
                )),
                Some(_) => {}
                None => {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
    }
    (merged, notes)
}

/// `nub pm use nub` — the full switch. `root` is the workspace root (where
/// the declaration, lockfiles, yaml, and `.npmrc` live). Prints the
/// file-by-file summary; never silent.
pub(crate) fn run_use_nub(root: &Path) -> Result<i32> {
    // The brand preflight registers the yaml names + lock.yaml filename the
    // discovery below and the engine writers read.
    super::engine_brand_preflight();

    // ── plan everything before writing anything (refuse-early) ──────────
    let plan = use_align::plan_alignment(root, "nub")?;
    refuse_on_injected_deps(root)?;

    let manifest_content = std::fs::read_to_string(root.join("package.json"))
        .with_context(|| format!("reading {}", root.join("package.json").display()))?;
    let manifest: Value = serde_json::from_str(&manifest_content)
        .with_context(|| format!("parsing {}", root.join("package.json").display()))?;
    let pnpm_ns = manifest.get("pnpm").and_then(Value::as_object).cloned();

    let yaml = read_workspace_yaml(root)?;
    let had_yaml = yaml.is_some();
    let (source, mut merge_notes) = merged_source(yaml.as_ref(), pnpm_ns.as_ref());
    let mut migration = plan_migration(&source)?;
    migration.notes.append(&mut merge_notes);

    // Layout preservation (Colin ruling, 2026-06-10): a hoisted-origin repo
    // (npm/yarn/bun lockfile converted) keeps its flat layout — going
    // isolated is the team's deliberate follow-up, not a side effect of
    // switching tools. Skipped when the migration itself carries a
    // node-linker (the explicit setting wins).
    let hoisted_origin = matches!(
        &plan,
        AlignPlan::Convert {
            from_kind: aube_lockfile::LockfileKind::Npm
                | aube_lockfile::LockfileKind::NpmShrinkwrap
                | aube_lockfile::LockfileKind::Yarn
                | aube_lockfile::LockfileKind::YarnBerry
                | aube_lockfile::LockfileKind::Bun,
            ..
        }
    );
    if hoisted_origin && !migration.npmrc.iter().any(|(k, _)| k == "node-linker") {
        migration
            .npmrc
            .push(("node-linker".into(), "hoisted".into()));
        migration.notes.push(
            "layout preserved: the project was on a hoisted (flat) node_modules layout, so \
             node-linker=hoisted is written to .npmrc — remove it to adopt nub's isolated layout"
                .into(),
        );
    }

    // ── writes ──────────────────────────────────────────────────────────
    let nub_version = env!("CARGO_PKG_VERSION");
    let mut manifest_notes = Vec::new();
    nub_core::pm::resolve::edit_root_manifest(root, |obj| {
        manifest_notes = apply_manifest_edits(obj, &migration, nub_version);
    })?;

    let (appended, skipped) = append_npmrc(root, &migration.npmrc)?;

    println!("using nub@{nub_version}");
    println!("  package.json: packageManager = nub@{nub_version}");
    println!(
        "  package.json: devEngines.packageManager = {{ name: \"nub\", version: \"^{nub_version}\", onFail: \"warn\" }}"
    );
    if migration.packages.is_some() || migration.catalog.is_some() || migration.catalogs.is_some() {
        println!(
            "  package.json: workspaces (packages + catalogs) migrated from pnpm-workspace.yaml"
        );
    }
    for (key, present) in [
        ("overrides", migration.overrides.is_some()),
        (
            "patchedDependencies",
            migration.patched_dependencies.is_some(),
        ),
        ("allowBuilds", migration.allow_builds.is_some()),
        ("auditConfig", migration.audit_config.is_some()),
    ] {
        if present {
            println!("  package.json: top-level {key} migrated");
        }
    }
    if pnpm_ns.is_some() {
        println!("  package.json: pnpm namespace removed (unread under nub identity)");
    }

    // Lockfile alignment.
    match plan {
        AlignPlan::Fresh => {
            println!("  no lockfile — the next install writes {NUB_LOCKFILE}");
        }
        AlignPlan::Keep { kept, remove } => {
            println!(
                "  {}: kept (already nub's lockfile)",
                kept.file_name().unwrap_or_default().to_string_lossy()
            );
            remove_strays(&remove, "lock.yaml is authoritative")?;
        }
        AlignPlan::Rename { from, remove } => {
            let to = root.join(NUB_LOCKFILE);
            std::fs::rename(&from, &to)
                .with_context(|| format!("renaming {} to {NUB_LOCKFILE}", from.display()))?;
            println!(
                "  {NUB_LOCKFILE}: renamed from {} (bytes unchanged — still pnpm-v9 format)",
                from.file_name().unwrap_or_default().to_string_lossy()
            );
            remove_strays(&remove, "migrated")?;
        }
        AlignPlan::Convert {
            from,
            from_kind,
            remove,
        } => {
            let written = use_align::convert_lockfile(root, &from, from_kind, "nub")?;
            println!(
                "  {}: written (converted from {})",
                written.file_name().unwrap_or_default().to_string_lossy(),
                from.file_name().unwrap_or_default().to_string_lossy()
            );
            remove_strays(&remove, "migrated")?;
        }
    }

    // The yaml is ALWAYS deleted (Colin, 2026-06-10): every key has been
    // migrated, warn-dropped, or warn-tailed above — leaving the file would
    // recreate the dual-home shadowing `use nub` exists to end.
    if had_yaml {
        std::fs::remove_file(root.join("pnpm-workspace.yaml"))
            .context("removing pnpm-workspace.yaml")?;
        println!("  pnpm-workspace.yaml: deleted (all keys migrated or named below)");
    }

    for line in &appended {
        println!("  .npmrc: + {line}");
    }
    for key in &skipped {
        println!(
            "  .npmrc: {key} already set — existing value kept (it already outranked the yaml)"
        );
    }
    for note in migration.notes.iter().chain(manifest_notes.iter()) {
        println!("  note: {note}");
    }
    if !migration.dropped.is_empty() {
        println!("  dropped (engine-unsupported):");
        for line in &migration.dropped {
            println!("    - {line}");
        }
    }
    if !migration.tail.is_empty() {
        println!("  no destination (named, not migrated):");
        for key in &migration.tail {
            println!("    - {key}");
        }
    }

    // The consequences block: switching identity is a forcing function and
    // teammates on other tooling hit it immediately — say so here, not in a
    // support thread later.
    println!();
    println!("heads-up for teammates and tooling not on nub:");
    println!("  - corepack-enabled shells hard-error on packageManager \"nub@…\" —");
    println!("    fix: install nub (npm i -g @nubjs/nub) or `corepack disable`");
    println!("  - real pnpm refuses to run here (ERR_PNPM_OTHER_PM_EXPECTED) — by design;");
    println!("    `nub pm use pnpm` reverses this switch completely");
    println!("  - turbo requires a recognized packageManager + lockfile name and will error");
    println!("  - hosted update bots (Renovate/Dependabot) can't regenerate {NUB_LOCKFILE} yet");
    println!("  - lockfile-sniffing deploy platforms won't auto-detect a PM — run installs");
    println!("    with nub in CI");
    Ok(0)
}

fn remove_strays(paths: &[std::path::PathBuf], why: &str) -> Result<()> {
    for path in paths {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        println!(
            "  {}: removed ({why})",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    Ok(())
}

/// The yaml-regeneration half of `nub pm use pnpm` — the exact reverse of
/// [`run_use_nub`]'s migration: collect the nub-mode homes out of
/// `package.json` (`workspaces` membership + catalogs, top-level `overrides`
/// / `patchedDependencies` / `allowBuilds` / `auditConfig`), write them into
/// `pnpm-workspace.yaml` (merging into an existing yaml, package.json values
/// winning — they were the live config under nub), and remove the migrated
/// keys from `package.json`. Settings already in `.npmrc` stay there — pnpm
/// reads them too. Returns the summary lines; empty when there was nothing
/// to move (the idempotent rerun).
pub(crate) fn regenerate_workspace_yaml(root: &Path) -> Result<Vec<String>> {
    let manifest_path = root.join("package.json");
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?,
    )
    .with_context(|| format!("parsing {}", manifest_path.display()))?;

    let workspaces = manifest.get("workspaces");
    let (packages, catalog, catalogs) = match workspaces {
        Some(Value::Array(_)) => (workspaces.cloned(), None, None),
        Some(Value::Object(o)) => (
            o.get("packages").cloned(),
            o.get("catalog").cloned(),
            o.get("catalogs").cloned(),
        ),
        _ => (None, None, None),
    };
    let overrides = manifest.get("overrides").cloned();
    let patched = manifest.get("patchedDependencies").cloned();
    let allow_builds = manifest.get("allowBuilds").cloned();
    let audit_config = manifest.get("auditConfig").cloned();

    if packages.is_none()
        && catalog.is_none()
        && catalogs.is_none()
        && overrides.is_none()
        && patched.is_none()
        && allow_builds.is_none()
        && audit_config.is_none()
    {
        return Ok(Vec::new());
    }

    let mut yaml = read_workspace_yaml(root)?.unwrap_or_default();
    let mut moved: Vec<&str> = Vec::new();
    for (key, value) in [
        ("packages", packages),
        ("catalog", catalog),
        ("catalogs", catalogs),
        ("overrides", overrides),
        ("patchedDependencies", patched),
        ("allowBuilds", allow_builds),
        ("auditConfig", audit_config),
    ] {
        if let Some(value) = value {
            yaml.insert(key.into(), value);
            moved.push(key);
        }
    }

    let yaml_path = root.join("pnpm-workspace.yaml");
    let body = serde_yaml::to_string(&serde_json::Value::Object(yaml.clone()))
        .context("serializing pnpm-workspace.yaml")?;
    std::fs::write(&yaml_path, body).with_context(|| format!("writing {}", yaml_path.display()))?;

    nub_core::pm::resolve::edit_root_manifest(root, |obj| {
        for key in [
            "workspaces",
            "overrides",
            "patchedDependencies",
            "allowBuilds",
            "auditConfig",
        ] {
            obj.remove(key);
        }
    })?;

    let mut lines = vec![format!(
        "pnpm-workspace.yaml: written ({})",
        moved.join(", ")
    )];
    lines.push("package.json: migrated keys moved back into pnpm-workspace.yaml".into());
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(json: serde_json::Value) -> Map<String, Value> {
        json.as_object().cloned().unwrap()
    }

    #[test]
    fn migration_routes_each_table_family_to_its_home() {
        let m = plan_migration(&src(json!({
            // structural → package.json
            "packages": ["packages/*"],
            "catalog": { "react": "^18.0.0" },
            "overrides": { "lodash": "4.17.21" },
            "patchedDependencies": { "left-pad@1.3.0": "patches/left-pad.patch" },
            // build folds → three-state allowBuilds
            "onlyBuiltDependencies": ["esbuild"],
            "ignoredBuiltDependencies": ["fsevents"],
            "allowBuilds": { "sharp": true },
            // settings → .npmrc (kebab)
            "nodeLinker": "hoisted",
            "minimumReleaseAge": 1440,
            "hoistPattern": ["*"],
            // per-axis arch fold
            "supportedArchitectures": { "os": ["linux"] },
            "cpu": ["x64"],
            // engine-unsupported → warn-drop
            "configDependencies": { "x": "1.0.0" },
            // transient → warn tail
            "production": true,
            // unknown camelCase → warn tail, named
            "totallyMadeUpKey": 7
        })))
        .unwrap();

        assert_eq!(m.packages, Some(json!(["packages/*"])));
        assert_eq!(m.catalog, Some(json!({ "react": "^18.0.0" })));
        assert_eq!(
            m.overrides.as_ref().and_then(|o| o.get("lodash")),
            Some(&json!("4.17.21"))
        );
        assert!(m.patched_dependencies.is_some());

        let builds = m.allow_builds.as_ref().unwrap();
        assert_eq!(
            builds.get("esbuild"),
            Some(&json!(true)),
            "onlyBuilt → true"
        );
        assert_eq!(
            builds.get("fsevents"),
            Some(&json!(false)),
            "ignored → false"
        );
        assert_eq!(
            builds.get("sharp"),
            Some(&json!(true)),
            "explicit entry kept"
        );

        let npmrc: std::collections::BTreeMap<_, _> = m.npmrc.iter().cloned().collect();
        assert_eq!(
            npmrc.get("node-linker").map(String::as_str),
            Some("hoisted")
        );
        assert_eq!(
            npmrc.get("minimum-release-age").map(String::as_str),
            Some("1440")
        );
        assert_eq!(
            npmrc.get("hoist-pattern").map(String::as_str),
            Some(r#"["*"]"#)
        );
        // the arch fold lands cpu inside the supportedArchitectures JSON
        let arch = npmrc.get("supported-architectures").unwrap();
        assert!(arch.contains("linux") && arch.contains("x64"), "{arch}");

        assert!(
            m.dropped
                .iter()
                .any(|d| d.starts_with("configDependencies"))
        );
        assert!(m.tail.iter().any(|t| t == "production"));
        assert!(m.tail.iter().any(|t| t.contains("totallyMadeUpKey")));
    }

    #[test]
    fn registries_explode_and_only_built_file_refuses() {
        let m = plan_migration(&src(json!({
            "registries": {
                "default": "https://registry.example.com/",
                "@acme": "https://npm.acme.dev/"
            }
        })))
        .unwrap();
        let npmrc: std::collections::BTreeMap<_, _> = m.npmrc.iter().cloned().collect();
        assert_eq!(
            npmrc.get("registry").map(String::as_str),
            Some("https://registry.example.com/")
        );
        assert_eq!(
            npmrc.get("@acme:registry").map(String::as_str),
            Some("https://npm.acme.dev/")
        );

        let err = plan_migration(&src(json!({ "onlyBuiltDependenciesFile": "builds.json" })))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("onlyBuiltDependenciesFile") && err.contains("nub pm use nub"),
            "refusal must name the key and the rerun remedy: {err}"
        );
    }

    #[test]
    fn manifest_edits_write_identity_workspaces_object_form_and_strip_pnpm_ns() {
        // Single-package catalog repo: packages-less workspaces object.
        let m = plan_migration(&src(json!({
            "catalog": { "react": "^18.0.0" }
        })))
        .unwrap();
        let mut obj = src(json!({ "name": "app", "pnpm": { "overrides": {} } }));
        apply_manifest_edits(&mut obj, &m, "0.1.0");

        assert_eq!(obj.get("packageManager"), Some(&json!("nub@0.1.0")));
        assert_eq!(
            obj.get("devEngines").unwrap().get("packageManager"),
            Some(&json!({ "name": "nub", "version": "^0.1.0", "onFail": "warn" }))
        );
        assert_eq!(
            obj.get("workspaces"),
            Some(&json!({ "catalog": { "react": "^18.0.0" } })),
            "single-package catalogs land as a packages-less workspaces object"
        );
        assert!(obj.get("pnpm").is_none(), "pnpm namespace must be removed");

        // Conflicting top-level override: package.json wins, named in notes.
        let m = plan_migration(&src(json!({
            "packages": ["packages/*"],
            "overrides": { "lodash": "4.17.21" }
        })))
        .unwrap();
        let mut obj = src(json!({ "name": "app", "overrides": { "lodash": "4.17.20" } }));
        let notes = apply_manifest_edits(&mut obj, &m, "0.1.0");
        assert_eq!(
            obj.get("overrides").unwrap().get("lodash"),
            Some(&json!("4.17.20")),
            "existing top-level override wins"
        );
        assert!(
            notes.iter().any(|n| n.contains("overrides.lodash")),
            "{notes:?}"
        );
        assert_eq!(
            obj.get("workspaces"),
            Some(&json!(["packages/*"])),
            "membership-only migration uses the plain array form"
        );
    }

    #[test]
    fn npmrc_append_skips_existing_keys_in_either_spelling() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".npmrc"), "nodeLinker=isolated\n").unwrap();
        let (appended, skipped) = append_npmrc(
            dir.path(),
            &[
                ("node-linker".into(), "hoisted".into()),
                ("store-dir".into(), "/tmp/store".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            skipped,
            vec!["node-linker"],
            "camel spelling must match kebab key"
        );
        assert_eq!(appended, vec!["store-dir=/tmp/store"]);
        let body = std::fs::read_to_string(dir.path().join(".npmrc")).unwrap();
        assert!(
            body.contains("nodeLinker=isolated\nstore-dir=/tmp/store\n"),
            "{body}"
        );
    }

    #[test]
    fn regenerate_reverses_the_migration_into_a_fresh_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            serde_json::to_string_pretty(&json!({
                "name": "app",
                "packageManager": "nub@0.1.0",
                "workspaces": { "packages": ["packages/*"], "catalog": { "react": "^18.0.0" } },
                "overrides": { "lodash": "4.17.21" },
                "allowBuilds": { "esbuild": true, "fsevents": false }
            }))
            .unwrap(),
        )
        .unwrap();

        let lines = regenerate_workspace_yaml(dir.path()).unwrap();
        assert!(!lines.is_empty());

        let yaml: serde_yaml::Value = serde_yaml::from_str(
            &std::fs::read_to_string(dir.path().join("pnpm-workspace.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(yaml["packages"][0], "packages/*");
        assert_eq!(yaml["catalog"]["react"], "^18.0.0");
        assert_eq!(yaml["overrides"]["lodash"], "4.17.21");
        assert_eq!(yaml["allowBuilds"]["esbuild"], true);

        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("package.json")).unwrap(),
        )
        .unwrap();
        for key in ["workspaces", "overrides", "allowBuilds"] {
            assert!(
                manifest.get(key).is_none(),
                "{key} must move back into the yaml"
            );
        }

        // Idempotent rerun: nothing left to move.
        assert!(regenerate_workspace_yaml(dir.path()).unwrap().is_empty());
    }
}
