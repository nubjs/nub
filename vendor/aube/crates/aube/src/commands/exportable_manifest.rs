//! The publish-time view of a `package.json`, shared by `pack` and `publish`.
//!
//! A workspace member's manifest carries specifiers only the workspace can
//! resolve (`workspace:`, `catalog:`) and fields meant for local development
//! that `publishConfig` replaces at publish time. A tarball or packument that
//! ships them verbatim is uninstallable from a registry, so both commands
//! rewrite the manifest first. This mirrors pnpm's `createExportableManifest`
//! (`@pnpm/exportable-manifest`): catalog first, then workspace, then the
//! `publishConfig` field overrides.

use miette::{Context, IntoDiagnostic, miette};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DEP_FIELDS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

/// `publishConfig` keys that replace the top-level field of the same name
/// in the published manifest — pnpm's `PUBLISH_CONFIG_WHITELIST`. Keys not
/// listed (`access`, `registry`, `tag`, …) stay under `publishConfig`.
const PUBLISH_CONFIG_OVERRIDES: &[&str] = &[
    "name",
    "bin",
    "engines",
    "type",
    "imports",
    "main",
    "module",
    "typings",
    "types",
    "exports",
    "browser",
    "esnext",
    "es2015",
    "unpkg",
    "umd:main",
    "os",
    "cpu",
    "libc",
    "typesVersions",
];

/// Rewrite `manifest_json` (the parsed `package.json` of `project_dir`) into
/// the manifest that gets published. Returns whether anything changed, so a
/// caller can keep the original bytes when nothing did.
pub(crate) fn export_manifest(
    project_dir: &Path,
    manifest_json: &mut serde_json::Value,
) -> miette::Result<bool> {
    // Catalog before workspace: a catalog entry may itself be a
    // `workspace:` spec, which the second pass then resolves.
    let catalogs = super::pack::rewrite_catalog_dependencies(project_dir, manifest_json)?;
    let workspace = rewrite_workspace_dependencies(project_dir, manifest_json)?;
    let publish_config = apply_publish_config_overrides(manifest_json);
    Ok(catalogs || workspace || publish_config)
}

fn apply_publish_config_overrides(manifest_json: &mut serde_json::Value) -> bool {
    let Some(root) = manifest_json.as_object_mut() else {
        return false;
    };
    let Some(config) = root
        .get_mut("publishConfig")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return false;
    };
    // In `publishConfig`'s own key order, which is where pnpm appends them;
    // `retain` keeps the remaining keys in order too.
    let is_override = |key: &str| PUBLISH_CONFIG_OVERRIDES.contains(&key);
    let overrides: Vec<(String, serde_json::Value)> = config
        .iter()
        .filter(|(key, _)| is_override(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    config.retain(|key, _| !is_override(key));
    if overrides.is_empty() {
        return false;
    }
    if config.is_empty() {
        root.remove("publishConfig");
    }
    root.extend(overrides);
    true
}

/// Rewrite every `workspace:` specifier into the range a registry consumer
/// would install. Member discovery is lazy, so a manifest without
/// `workspace:` specs never reads the workspace.
pub(crate) fn rewrite_workspace_dependencies(
    project_dir: &Path,
    manifest_json: &mut serde_json::Value,
) -> miette::Result<bool> {
    let manifest_path = project_dir.join("package.json");
    let Some(root) = manifest_json.as_object_mut() else {
        return Ok(false);
    };
    let mut members: Option<WorkspaceMembers> = None;
    let mut changed = false;
    for field in DEP_FIELDS {
        let peer = field == "peerDependencies";
        let Some(deps) = root
            .get_mut(field)
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        for (key, value) in deps.iter_mut() {
            let Some(spec) = value.as_str() else {
                continue;
            };
            // pnpm finds the protocol anywhere in a peer range
            // (`^1.0.0 || workspace:>`) but only as a prefix elsewhere.
            let carries_protocol = if peer {
                spec.contains("workspace:")
            } else {
                aube_util::pkg::is_workspace_spec(spec)
            };
            if !carries_protocol {
                continue;
            }
            if members.is_none() {
                members = Some(WorkspaceMembers::discover(project_dir)?);
            }
            let members = members.as_ref().expect("members discovered above");
            let exported = export_workspace_spec(key, spec, peer, project_dir, members)
                .map_err(|reason| {
                    miette!(
                        code = aube_codes::errors::ERR_AUBE_WORKSPACE_PKG_NOT_FOUND,
                        help = "a published manifest cannot carry `workspace:`; every such dependency must name a workspace member with a `version`",
                        "{} declares `{key}: {spec}` in `{field}`, which cannot be published: {reason}",
                        manifest_path.display(),
                    )
                })?;
            *value = serde_json::Value::String(exported);
            changed = true;
        }
    }
    Ok(changed)
}

/// Name → version of every workspace member, the workspace root included
/// (install lets members depend on the root with `workspace:*` too).
struct WorkspaceMembers {
    root: Option<PathBuf>,
    versions: HashMap<String, Option<String>>,
}

impl WorkspaceMembers {
    fn discover(project_dir: &Path) -> miette::Result<Self> {
        let Some(root) = crate::dirs::find_workspace_root(project_dir) else {
            return Ok(Self {
                root: None,
                versions: HashMap::new(),
            });
        };
        let member_dirs = aube_workspace::find_workspace_packages(&root)
            .into_diagnostic()
            .wrap_err("failed to discover workspace packages")?;
        let mut versions: HashMap<String, Option<String>> = HashMap::new();
        for dir in std::iter::once(root.clone()).chain(member_dirs) {
            let manifest_path = dir.join("package.json");
            if !manifest_path.is_file() {
                continue;
            }
            let manifest = aube_manifest::PackageJson::from_path(&manifest_path)
                .map_err(miette::Report::new)?;
            let Some(name) = manifest.name else {
                continue;
            };
            // Same tie-break as pnpm's workspace lookup: a later duplicate
            // only replaces an earlier one that had no version.
            let slot = versions.entry(name).or_insert(None);
            if slot.is_none() {
                *slot = manifest.version;
            }
        }
        Ok(Self {
            root: Some(root),
            versions,
        })
    }

    fn version(&self, name: &str) -> Result<&str, String> {
        match self.versions.get(name) {
            Some(Some(version)) => Ok(version.as_str()),
            Some(None) => Err(format!("workspace package `{name}` has no `version` field")),
            None => match &self.root {
                Some(root) => Err(format!(
                    "`{name}` is not a package in the workspace at {}",
                    root.display()
                )),
                None => Err(format!("`{name}` cannot be resolved outside a workspace")),
            },
        }
    }
}

/// The `^` / `~` a sentinel range keeps, or `None` when `range` is an
/// explicit range that publishes as written.
fn sentinel_operator(range: &str) -> Option<&'static str> {
    match range {
        "" | "*" => Some(""),
        "^" => Some("^"),
        "~" => Some("~"),
        _ => None,
    }
}

fn export_workspace_spec(
    key: &str,
    spec: &str,
    peer: bool,
    project_dir: &Path,
    members: &WorkspaceMembers,
) -> Result<String, String> {
    use aube_util::pkg::WorkspaceSpec;
    let Some(parsed) = aube_util::pkg::parse_workspace_spec(spec) else {
        // Only a peer range reaches here: the protocol sits mid-range.
        return export_peer_range(key, spec, members);
    };
    match parsed {
        WorkspaceSpec::Path(rel) => {
            let (name, version) = read_path_member(&project_dir.join(rel))?;
            Ok(if name == key {
                version
            } else {
                format!("npm:{name}@{version}")
            })
        }
        // pnpm resolves an aliased sentinel inside `peerDependencies`
        // differently from every other field: 10.x through 11.24 publish
        // `workspace:a@~` as `1.2.3a@~` (not a valid range), later 11.x as
        // `npm:a@*`, which drops the operator the author wrote. Peers take
        // the same path as dependencies here, giving `npm:a@~1.2.3`.
        WorkspaceSpec::Alias { name, range } => {
            let version = members.version(name)?;
            Ok(match sentinel_operator(range) {
                Some(op) if name == key => format!("{op}{version}"),
                Some(op) => format!("npm:{name}@{op}{version}"),
                None => format!("npm:{name}@{range}"),
            })
        }
        WorkspaceSpec::Range(_) if peer => export_peer_range(key, spec, members),
        WorkspaceSpec::Range(range) => {
            let version = members.version(key)?;
            Ok(match sentinel_operator(range) {
                Some(op) => format!("{op}{version}"),
                None => range.to_string(),
            })
        }
    }
}

/// pnpm's peer rewrite (`resolvePeerWorkspaceSemver`): the first
/// `workspace:` in the range, with an optional comparator and no version
/// after it, becomes that comparator plus the member's version
/// (`workspace:>= || ^3` → `>=4.5.6 || ^3`); with a version after it, the
/// protocol is just dropped.
fn export_peer_range(key: &str, spec: &str, members: &WorkspaceMembers) -> Result<String, String> {
    let version = members.version(key)?;
    let Some(at) = spec.find("workspace:") else {
        return Ok(spec.to_string());
    };
    let tail = &spec[at + "workspace:".len()..];
    let op_len = [">=", "<=", "^", "~", "*", ">", "<"]
        .iter()
        .find(|op| tail.starts_with(**op))
        .map_or(0, |op| op.len());
    let (op, rest) = tail.split_at(op_len);
    let has_version = rest
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit() || matches!(c, 'x' | 'X' | '*'));
    if has_version {
        return Ok(format!("{}{tail}", &spec[..at]));
    }
    let op = if op == "*" { "" } else { op };
    Ok(format!("{}{op}{version}{rest}", &spec[..at]))
}

fn read_path_member(dir: &Path) -> Result<(String, String), String> {
    let manifest_path = dir.join("package.json");
    if !manifest_path.is_file() {
        return Err(format!("no package.json at {}", dir.display()));
    }
    let manifest =
        aube_manifest::PackageJson::from_path(&manifest_path).map_err(|e| e.to_string())?;
    let name = manifest
        .name
        .ok_or_else(|| format!("{} has no `name` field", manifest_path.display()))?;
    let version = manifest
        .version
        .ok_or_else(|| format!("{} has no `version` field", manifest_path.display()))?;
    Ok((name, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A workspace whose members cover the plain, scoped, and aliased
    /// targets, plus a catalog entry that is itself a `workspace:` spec.
    fn workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\ncatalog:\n  b: workspace:~\n  is-number: ^7.0.0\n",
        );
        write(
            &root.join("package.json"),
            r#"{"name":"root","private":true}"#,
        );
        write(
            &root.join("packages/a/package.json"),
            r#"{"name":"a","version":"1.2.3"}"#,
        );
        write(
            &root.join("packages/b/package.json"),
            r#"{"name":"b","version":"0.4.0"}"#,
        );
        write(
            &root.join("packages/ui/package.json"),
            r#"{"name":"@scope/ui","version":"3.4.5"}"#,
        );
        tmp
    }

    /// Write `manifest` as `dir/package.json`, then export it.
    fn export(dir: &Path, manifest: serde_json::Value) -> miette::Result<serde_json::Value> {
        write(&dir.join("package.json"), &manifest.to_string());
        let mut manifest = manifest;
        export_manifest(dir, &mut manifest)?;
        Ok(manifest)
    }

    #[test]
    fn workspace_and_catalog_specs_become_registry_ranges_in_every_field() {
        let tmp = workspace();
        let consumer = tmp.path().join("packages/consumer");
        let out = export(
            &consumer,
            serde_json::json!({
                "name": "consumer",
                "version": "1.0.0",
                "dependencies": {
                    "a": "workspace:*",
                    "@scope/ui": "workspace:^",
                    "card": "workspace:a@*",
                    "scoped": "workspace:@scope/ui@~",
                    "a-range": "workspace:a@^1.0.0",
                    "by-path": "workspace:../ui",
                    "is-number": "catalog:",
                    "b": "catalog:",
                },
                "devDependencies": {"a": "workspace:~", "@scope/ui": "workspace:^3.0.0"},
                "optionalDependencies": {"a": "workspace:a@^"},
                "peerDependencies": {
                    "a": "workspace:^",
                    "a-tilde": "workspace:a@~",
                    "@scope/ui": "workspace:>= || ^1.0.0",
                    "b": "^0.1.0 || workspace:>",
                },
            }),
        )
        .unwrap();
        assert_eq!(
            out["dependencies"],
            serde_json::json!({
                "a": "1.2.3",
                "@scope/ui": "^3.4.5",
                "card": "npm:a@1.2.3",
                "scoped": "npm:@scope/ui@~3.4.5",
                "a-range": "npm:a@^1.0.0",
                "by-path": "npm:@scope/ui@3.4.5",
                "is-number": "^7.0.0",
                "b": "~0.4.0",
            })
        );
        assert_eq!(
            out["devDependencies"],
            serde_json::json!({"a": "~1.2.3", "@scope/ui": "^3.0.0"})
        );
        assert_eq!(
            out["optionalDependencies"],
            serde_json::json!({"a": "^1.2.3"})
        );
        assert_eq!(
            out["peerDependencies"],
            serde_json::json!({
                "a": "^1.2.3",
                "a-tilde": "npm:a@~1.2.3",
                "@scope/ui": ">=3.4.5 || ^1.0.0",
                "b": "^0.1.0 || >0.4.0",
            })
        );
    }

    #[test]
    fn workspace_spec_naming_a_non_member_is_an_error() {
        let tmp = workspace();
        let consumer = tmp.path().join("packages/consumer");
        let err = export(
            &consumer,
            serde_json::json!({"name": "consumer", "dependencies": {"missing": "workspace:*"}}),
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("`missing: workspace:*`")
                && message.contains("not a package in the workspace"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn publish_config_fields_replace_top_level_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let out = export(
            tmp.path(),
            serde_json::json!({
                "name": "local",
                "main": "src/index.ts",
                "publishConfig": {"name": "published", "main": "dist/index.js", "access": "public"},
            }),
        )
        .unwrap();
        assert_eq!(
            out,
            serde_json::json!({
                "name": "published",
                "main": "dist/index.js",
                "publishConfig": {"access": "public"},
            })
        );
    }
}
