//! pnpm 11+'s env lockfile: the leading YAML document of `pnpm-lock.yaml`.
//!
//! A project that pins pnpm (`devEngines.packageManager`, or a v12+
//! `packageManager` field) gets the pinned pnpm's package closure, with
//! integrities, recorded in a document written ahead of the project
//! lockfile: `---\n<env>\n---\n<main>`. pnpm's `--frozen-lockfile` refuses
//! a lockfile whose env document does not pin the wanted version, so a
//! writer that drops it breaks every pnpm 11/12 consumer of the file.
//!
//! Ownership mirrors pnpm's: the main-document writer re-prepends whatever
//! env document is already on disk, byte for byte
//! ([`preserved_env_document`]), and only the package-manager sync
//! (`aube::package_manager_pin`) composes a new one
//! ([`render_package_manager_env`]). The env document's `packages:` /
//! `snapshots:` share the main document's per-entry shape, so rendering
//! goes through the same projection and formatter.

use super::format::reformat_for_pnpm_parity;
use crate::{Error, LockfileGraph};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const DOCUMENT_START: &str = "---\n";
const DOCUMENT_SEPARATOR: &str = "\n---\n";

/// Split `content` into its env document (between the leading `---\n` and
/// the next `\n---\n`) and the main document that follows. A file that
/// does not open with `---\n` carries no env document. Same rule as
/// pnpm's `extract_env_document` / `extract_main_document`.
pub fn split_env_document(content: &str) -> (Option<&str>, &str) {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let Some(rest) = content.strip_prefix(DOCUMENT_START) else {
        return (None, content);
    };
    match rest.find(DOCUMENT_SEPARATOR) {
        Some(at) => (Some(&rest[..at]), &rest[at + DOCUMENT_SEPARATOR.len()..]),
        None => (Some(rest), ""),
    }
}

/// The env document of a lockfile, read line by line and abandoned at the
/// separator that closes it, so a caller asking only about the pin never
/// reads the project document. Same split as [`split_env_document`].
pub fn read_env_document(reader: impl std::io::Read) -> std::io::Result<Option<String>> {
    use std::io::BufRead;
    let mut lines = std::io::BufReader::new(reader).lines();
    match lines.next().transpose()? {
        Some(first) if first.strip_prefix('\u{feff}').unwrap_or(&first) == "---" => {}
        _ => return Ok(None),
    }
    let mut env = Vec::new();
    for line in lines {
        let line = line?;
        if line == "---" {
            break;
        }
        env.push(line);
    }
    Ok(Some(env.join("\n")))
}

/// `env` followed by `main`, in pnpm's two-document layout.
pub fn compose_with_env_document(env: &str, main: &str) -> String {
    format!("{DOCUMENT_START}{env}{DOCUMENT_SEPARATOR}{main}")
}

/// The env document of the lockfile at `path`, to carry across a rewrite
/// of its main document. A document Git left conflicted is not copied
/// forward: the next package-manager sync re-resolves it instead.
pub(super) fn preserved_env_document(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let env = split_env_document(&content).0?;
    let conflicted = env
        .lines()
        .any(|l| l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>"));
    (!conflicted).then(|| env.to_string())
}

/// One `configDependencies` / `packageManagerDependencies` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvDependency {
    pub specifier: String,
    pub version: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct EnvImporter {
    #[serde(default)]
    config_dependencies: BTreeMap<String, EnvDependency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    package_manager_dependencies: Option<BTreeMap<String, EnvDependency>>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct EnvDocument {
    lockfile_version: yaml_serde::Value,
    #[serde(default)]
    importers: BTreeMap<String, EnvImporter>,
    #[serde(default)]
    packages: BTreeMap<String, yaml_serde::Value>,
    #[serde(default)]
    snapshots: BTreeMap<String, yaml_serde::Value>,
}

/// A parsed env document.
#[derive(Debug)]
pub struct EnvLockfile {
    doc: EnvDocument,
}

impl EnvLockfile {
    /// Parse the env document of `content` (a whole `pnpm-lock.yaml`).
    /// `None` when there is none, or it does not parse.
    pub fn from_lockfile_content(content: &str) -> Option<Self> {
        Self::parse(split_env_document(content).0?)
    }

    /// Parse an env document's own text.
    pub fn parse(env: &str) -> Option<Self> {
        yaml_serde::from_str(env).ok().map(|doc| Self { doc })
    }

    fn root(&self) -> Option<&EnvImporter> {
        self.doc.importers.get(".")
    }

    pub fn package_manager_dependencies(&self) -> Option<&BTreeMap<String, EnvDependency>> {
        self.root()?.package_manager_dependencies.as_ref()
    }

    /// Whether this document pins the package manager at `version`: each
    /// of `names` is recorded, every recorded entry is at `version` with
    /// its `packages:` and `snapshots:` records present, and — when
    /// `specifier` is given — every entry records that specifier. pnpm's
    /// `pins_wanted_package_manager` / `is_package_manager_resolved`.
    pub fn pins(&self, version: &str, names: &[&str], specifier: Option<&str>) -> bool {
        let Some(deps) = self.package_manager_dependencies() else {
            return false;
        };
        names.iter().all(|name| deps.contains_key(*name))
            && deps.iter().all(|(name, dep)| {
                let key = format!("{name}@{}", dep.version);
                dep.version == version
                    && specifier.is_none_or(|s| dep.specifier == s)
                    && self.doc.packages.contains_key(&key)
                    && self.doc.snapshots.contains_key(&key)
            })
    }
}

/// Render the env document that records `package_manager_dependencies`,
/// whose package closure is `closure` (a resolved graph rooted at those
/// packages). `existing`'s `configDependencies` and the records they reach
/// are kept; whatever only the previous package-manager pin reached is
/// pruned, as pnpm's `prune_env_lockfile` does.
pub fn render_package_manager_env(
    existing: Option<&EnvLockfile>,
    package_manager_dependencies: BTreeMap<String, EnvDependency>,
    closure: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<String, Error> {
    let path = Path::new("pnpm-lock.yaml");
    let built = super::write::build(path, closure, manifest)?.lockfile;
    let value = yaml_serde::to_value(&built).map_err(|e| Error::parse(path, e.to_string()))?;
    let section = |key: &str| -> BTreeMap<String, yaml_serde::Value> {
        value
            .get(key)
            .and_then(|v| yaml_serde::from_value(v.clone()).ok())
            .unwrap_or_default()
    };

    let (lockfile_version, config_dependencies, mut packages, mut snapshots) = match existing {
        Some(env) => (
            env.doc.lockfile_version.clone(),
            env.root()
                .map(|r| r.config_dependencies.clone())
                .unwrap_or_default(),
            env.doc.packages.clone(),
            env.doc.snapshots.clone(),
        ),
        None => (
            yaml_serde::Value::String("9.0".to_string()),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        ),
    };
    packages.extend(section("packages"));
    snapshots.extend(section("snapshots"));

    let roots = config_dependencies
        .iter()
        .chain(&package_manager_dependencies)
        .map(|(name, dep)| format!("{name}@{}", dep.version));
    let reachable = reachable_snapshots(roots, &snapshots);
    snapshots.retain(|key, _| reachable.contains(key));
    let package_keys: BTreeSet<&str> = reachable.iter().map(|k| without_peers(k)).collect();
    packages.retain(|key, _| package_keys.contains(key.as_str()));

    let doc = EnvDocument {
        lockfile_version,
        importers: BTreeMap::from([(
            ".".to_string(),
            EnvImporter {
                config_dependencies,
                package_manager_dependencies: Some(package_manager_dependencies),
            },
        )]),
        packages,
        snapshots,
    };
    let yaml = yaml_serde::to_string(&doc).map_err(|e| Error::parse(path, e.to_string()))?;
    Ok(reformat_for_pnpm_parity(&yaml))
}

fn without_peers(key: &str) -> &str {
    key.find('(').map_or(key, |at| &key[..at])
}

/// Snapshot keys reachable from `roots` through `dependencies` /
/// `optionalDependencies` edges. An edge value is either a version
/// (`1.2.3`, possibly peer-suffixed) under the edge's own name, or an
/// alias target spelled `name@version`.
fn reachable_snapshots(
    roots: impl Iterator<Item = String>,
    snapshots: &BTreeMap<String, yaml_serde::Value>,
) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut pending: Vec<String> = roots.collect();
    while let Some(key) = pending.pop() {
        if !seen.insert(key.clone()) {
            continue;
        }
        let Some(snapshot) = snapshots.get(&key) else {
            continue;
        };
        for group in ["dependencies", "optionalDependencies"] {
            let Some(edges) = snapshot.get(group).and_then(|g| g.as_mapping()) else {
                continue;
            };
            for (name, reference) in edges {
                let (Some(name), Some(reference)) = (name.as_str(), reference.as_str()) else {
                    continue;
                };
                pending.push(if without_peers(reference).contains('@') {
                    reference.to_string()
                } else {
                    format!("{name}@{reference}")
                });
            }
        }
    }
    seen
}
