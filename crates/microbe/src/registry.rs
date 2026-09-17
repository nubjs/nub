//! The registry's abbreviated packument (`application/vnd.npm.install-v1+json`) and version
//! selection. The abbreviated form carries exactly the fields an installer needs — `dist`,
//! the dependency maps, `bin`, `os`/`cpu`, `hasInstallScript` — at a fraction of the full
//! document's size, and every npm-compatible registry serves it.

use crate::error::Error;
use node_semver::{Range, Version};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
pub struct Packument {
    #[serde(rename = "dist-tags", default)]
    pub dist_tags: BTreeMap<String, String>,
    pub versions: BTreeMap<String, Manifest>,
}

#[derive(Deserialize, Clone)]
pub struct Manifest {
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(rename = "optionalDependencies", default)]
    pub optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub bin: Bin,
    #[serde(default)]
    pub os: Vec<String>,
    #[serde(default)]
    pub cpu: Vec<String>,
    #[serde(rename = "hasInstallScript", default)]
    pub has_install_script: bool,
    pub dist: Dist,
}

#[derive(Deserialize, Clone)]
pub struct Dist {
    pub tarball: String,
    pub integrity: Option<String>,
    pub shasum: Option<String>,
}

/// `bin` is a single path (the command takes the package's unscoped name) or a map.
#[derive(Deserialize, Clone, Default)]
#[serde(untagged)]
pub enum Bin {
    #[default]
    None,
    Single(String),
    Map(BTreeMap<String, String>),
}

impl Bin {
    pub fn entries(&self, package_name: &str) -> BTreeMap<String, String> {
        match self {
            Bin::None => BTreeMap::new(),
            Bin::Single(path) => {
                let cmd = package_name.rsplit('/').next().unwrap_or(package_name);
                BTreeMap::from([(cmd.to_string(), path.clone())])
            }
            Bin::Map(m) => m.clone(),
        }
    }
}

pub fn parse(name: &str, body: &[u8]) -> Result<Packument, Error> {
    serde_json::from_slice(body).map_err(|e| Error::Registry {
        name: name.to_string(),
        detail: e.to_string(),
    })
}

/// Pick the version for `spec`: a dist-tag (`latest` when empty), an exact version, or the
/// highest version satisfying a range. Versions the range parser rejects are skipped, as npm
/// does, rather than failing the whole packument.
pub fn pick<'a>(
    p: &'a Packument,
    name: &str,
    spec: &str,
) -> Result<(&'a str, &'a Manifest), Error> {
    let tag = if spec.is_empty() { "latest" } else { spec };
    if let Some(v) = p.dist_tags.get(tag)
        && let Some(m) = p.versions.get(v)
    {
        return Ok((v, m));
    }
    let no_version = || Error::NoVersion {
        name: name.to_string(),
        spec: spec.to_string(),
    };
    let range: Range = spec.parse().map_err(|_| no_version())?;
    let mut best: Option<(&str, &Manifest, Version)> = None;
    for (vs, m) in &p.versions {
        let Ok(v) = vs.parse::<Version>() else {
            continue;
        };
        if range.satisfies(&v) && best.as_ref().is_none_or(|(_, _, b)| v > *b) {
            best = Some((vs, m, v));
        }
    }
    best.map(|(v, m, _)| (v, m)).ok_or_else(no_version)
}

pub fn satisfies(version: &str, spec: &str) -> bool {
    match (version.parse::<Version>(), spec.parse::<Range>()) {
        (Ok(v), Ok(r)) => r.satisfies(&v),
        _ => false,
    }
}

/// npm's `os` / `cpu` filter: an empty list allows everything, a `!name` entry excludes,
/// otherwise the platform must be listed. npm spells platforms the Node way, so the Rust
/// constants are translated before comparison.
pub fn platform_allowed(m: &Manifest) -> bool {
    allowed(&m.os, node_os()) && allowed(&m.cpu, node_arch())
}

fn allowed(list: &[String], value: &str) -> bool {
    if list.is_empty() {
        return true;
    }
    if list.iter().any(|e| e.strip_prefix('!') == Some(value)) {
        return false;
    }
    let has_positive = list.iter().any(|e| !e.starts_with('!'));
    !has_positive || list.iter().any(|e| e == value)
}

fn node_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "ia32",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packument(versions: &[&str], latest: &str) -> Packument {
        let vs = versions
            .iter()
            .map(|v| format!(r#""{v}": {{"dist": {{"tarball": "https://r/{v}.tgz"}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        parse(
            "p",
            format!(r#"{{"dist-tags": {{"latest": "{latest}"}}, "versions": {{{vs}}}}}"#)
                .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn picks_tag_exact_and_highest_in_range() {
        let p = packument(&["1.0.0", "1.2.0", "2.0.0-beta.1", "2.0.0"], "1.2.0");
        assert_eq!(pick(&p, "p", "").unwrap().0, "1.2.0");
        assert_eq!(pick(&p, "p", "latest").unwrap().0, "1.2.0");
        assert_eq!(pick(&p, "p", "1.0.0").unwrap().0, "1.0.0");
        assert_eq!(pick(&p, "p", "^1").unwrap().0, "1.2.0");
        assert_eq!(pick(&p, "p", ">=1").unwrap().0, "2.0.0");
        assert!(matches!(pick(&p, "p", "^3"), Err(Error::NoVersion { .. })));
        assert!(matches!(
            pick(&p, "p", "not a range"),
            Err(Error::NoVersion { .. })
        ));
    }

    #[test]
    fn bin_single_takes_unscoped_package_name() {
        let b = Bin::Single("cli.js".into());
        assert_eq!(
            b.entries("@scope/tool"),
            BTreeMap::from([("tool".to_string(), "cli.js".to_string())])
        );
        assert_eq!(
            b.entries("tool"),
            BTreeMap::from([("tool".to_string(), "cli.js".to_string())])
        );
    }

    #[test]
    fn os_cpu_filter_matches_npm_semantics() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(allowed(&[], "darwin"));
        assert!(allowed(&s(&["darwin", "linux"]), "darwin"));
        assert!(!allowed(&s(&["linux"]), "darwin"));
        assert!(!allowed(&s(&["!darwin"]), "darwin"));
        assert!(allowed(&s(&["!win32"]), "darwin"));
        assert!(!allowed(&s(&["!darwin", "linux"]), "darwin"));
    }
}
