//! Node version request parsing: exact versions, semver ranges, and
//! the alias vocabulary `pnpm runtime` / nvm users expect (`lts`,
//! `latest`, LTS codenames like `jod` / `lts/jod`).

use crate::error::Error;

/// A parsed Node version request.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeSpec {
    Exact(node_semver::Version),
    Range(node_semver::Range),
    /// Newest LTS release (`lts`, `lts/*`).
    Lts,
    /// Newest release of any kind (`latest`, `current`, `node`).
    Latest,
    /// A named LTS line (`jod`, `lts/jod`, `lts/iron`). Stored
    /// lowercased; validated against the dist index at resolve time.
    LtsCodename(String),
}

impl NodeSpec {
    /// Parse a user-written request. Accepts a leading `v` on
    /// versions and either `lts/<name>` or a bare codename.
    pub fn parse(raw: &str) -> Result<NodeSpec, Error> {
        let s = raw.trim();
        let lowered = s.to_ascii_lowercase();
        match lowered.as_str() {
            "lts" | "lts/*" => return Ok(NodeSpec::Lts),
            "latest" | "current" | "node" | "*" => return Ok(NodeSpec::Latest),
            _ => {}
        }
        if let Some(name) = lowered.strip_prefix("lts/") {
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphabetic()) {
                return Ok(NodeSpec::LtsCodename(name.to_string()));
            }
            return Err(Error::NoMatchingVersion {
                requested: raw.to_string(),
                platform_note: String::new(),
            });
        }
        let unprefixed = s.strip_prefix('v').unwrap_or(s);
        if let Ok(v) = node_semver::Version::parse(unprefixed) {
            return Ok(NodeSpec::Exact(v));
        }
        if let Ok(r) = node_semver::Range::parse(unprefixed) {
            return Ok(NodeSpec::Range(r));
        }
        // Bare alphabetic word → treat as an LTS codename (`jod`,
        // `iron`); resolution fails cleanly if the index has no such
        // LTS line.
        if !lowered.is_empty() && lowered.chars().all(|c| c.is_ascii_alphabetic()) {
            return Ok(NodeSpec::LtsCodename(lowered));
        }
        Err(Error::NoMatchingVersion {
            requested: raw.to_string(),
            platform_note: String::new(),
        })
    }

    /// Whether `version` satisfies this request, for the parts of the
    /// vocabulary that are decidable without the dist index. `Lts` /
    /// `Latest` / codenames return `None` — satisfaction depends on
    /// index data the caller may not have.
    pub fn satisfied_by(&self, version: &node_semver::Version) -> Option<bool> {
        match self {
            NodeSpec::Exact(v) => Some(v == version),
            NodeSpec::Range(r) => Some(version.satisfies(r)),
            NodeSpec::Lts | NodeSpec::Latest | NodeSpec::LtsCodename(_) => None,
        }
    }

    /// The request as the user would write it (used for messages and
    /// for the lockfile `specifier`).
    pub fn display(&self) -> String {
        match self {
            NodeSpec::Exact(v) => v.to_string(),
            NodeSpec::Range(r) => r.to_string(),
            NodeSpec::Lts => "lts".to_string(),
            NodeSpec::Latest => "latest".to_string(),
            NodeSpec::LtsCodename(name) => format!("lts/{name}"),
        }
    }
}

impl std::str::FromStr for NodeSpec {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        NodeSpec::parse(s)
    }
}

impl std::fmt::Display for NodeSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

/// Where a node version request came from, in precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestSource {
    /// `package.json` `devEngines.runtime` (name == node).
    DevEngines,
    /// A `.node-version` file.
    NodeVersionFile,
    /// A `.nvmrc` file.
    Nvmrc,
}

impl RequestSource {
    pub fn label(self) -> &'static str {
        match self {
            RequestSource::DevEngines => "devEngines.runtime",
            RequestSource::NodeVersionFile => ".node-version",
            RequestSource::Nvmrc => ".nvmrc",
        }
    }
}

/// A fully-formed request: what version, what to do when it can't be
/// satisfied locally, and where the requirement came from.
#[derive(Debug, Clone)]
pub struct NodeRequest {
    pub spec: NodeSpec,
    /// The request exactly as the user wrote it (`"22"`, `"^24.4.0"`,
    /// `"lts/jod"`). Lockfile specifiers and display both use this —
    /// `NodeSpec::display()` normalizes ranges, and a normalized form
    /// would never string-match the verbatim range pnpm records.
    pub raw: String,
    pub on_fail: aube_manifest::OnFail,
    pub source: RequestSource,
    /// Path of the file the request was read from (version file or
    /// package.json), for diagnostics.
    pub origin: std::path::PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> NodeSpec {
        NodeSpec::parse(s).unwrap()
    }

    #[test]
    fn parses_exact_versions() {
        assert!(matches!(parse("22.1.0"), NodeSpec::Exact(_)));
        assert!(matches!(parse("v22.1.0"), NodeSpec::Exact(_)));
        assert!(matches!(parse("18.0.0-rc.1"), NodeSpec::Exact(_)));
    }

    #[test]
    fn parses_ranges() {
        assert!(matches!(parse("^22"), NodeSpec::Range(_)));
        assert!(matches!(parse(">=18 <21"), NodeSpec::Range(_)));
        assert!(matches!(parse("22"), NodeSpec::Range(_)));
        assert!(matches!(parse("22.x"), NodeSpec::Range(_)));
        assert!(matches!(parse("~18.12"), NodeSpec::Range(_)));
    }

    #[test]
    fn parses_aliases() {
        assert_eq!(parse("lts"), NodeSpec::Lts);
        assert_eq!(parse("LTS"), NodeSpec::Lts);
        assert_eq!(parse("lts/*"), NodeSpec::Lts);
        assert_eq!(parse("latest"), NodeSpec::Latest);
        assert_eq!(parse("current"), NodeSpec::Latest);
        assert_eq!(parse("node"), NodeSpec::Latest);
        assert_eq!(parse("*"), NodeSpec::Latest);
        assert_eq!(parse("lts/jod"), NodeSpec::LtsCodename("jod".into()));
        assert_eq!(parse("lts/Jod"), NodeSpec::LtsCodename("jod".into()));
        assert_eq!(parse("jod"), NodeSpec::LtsCodename("jod".into()));
    }

    #[test]
    fn rejects_garbage() {
        assert!(NodeSpec::parse("lts/").is_err());
        assert!(NodeSpec::parse("not a spec !!").is_err());
        assert!(NodeSpec::parse("").is_err());
    }

    #[test]
    fn local_satisfaction() {
        let v: node_semver::Version = "22.3.0".parse().unwrap();
        assert_eq!(parse("^22").satisfied_by(&v), Some(true));
        assert_eq!(parse("^20").satisfied_by(&v), Some(false));
        assert_eq!(parse("22.3.0").satisfied_by(&v), Some(true));
        assert_eq!(parse("lts").satisfied_by(&v), None);
        assert_eq!(parse("jod").satisfied_by(&v), None);
    }

    #[test]
    fn bare_number_is_a_range() {
        // "22" must match every 22.x.y, not just 22.0.0.
        let v: node_semver::Version = "22.9.1".parse().unwrap();
        assert_eq!(parse("22").satisfied_by(&v), Some(true));
    }
}
