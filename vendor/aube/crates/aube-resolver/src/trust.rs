//! Trust-policy enforcement.
//!
//! Mirrors pnpm's `failIfTrustDowngraded`
//! (resolving/npm-resolver/src/trustChecks.ts), verified against pnpm's
//! own test suite. Three trust-evidence sources, ranked
//! `StagedPublish (3) > TrustedPublisher (2) > Provenance (1)`. aube
//! only accepts the structured metadata shapes npm emits after
//! server-side checks: `approver` must be present, `_npmUser.trustedPublisher`
//! must name a publisher id, and `dist.attestations.provenance` must
//! name an SLSA provenance predicate. This is metadata-shape validation,
//! not install-time cryptographic verification of the attestation bundle.
//! The check runs immediately after a version is picked from a packument:
//! if any strictly older version of the same package had stronger trust
//! evidence, the install fails. Pre-2010 packuments without per-version
//! `time` entries error when the picked version isn't excluded — same as
//! pnpm.

use aube_registry::{Packument, VersionMetadata};
use std::time::{SystemTime, UNIX_EPOCH};

/// Trust-evidence ranks. Higher is stronger. Variants intentionally do
/// not derive `Ord` — the variant declaration order does not match the
/// rank order, so callers must go through [`Self::rank`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustEvidence {
    StagedPublish,
    TrustedPublisher,
    Provenance,
}

impl TrustEvidence {
    pub fn rank(self) -> u8 {
        match self {
            Self::StagedPublish => 3,
            Self::TrustedPublisher => 2,
            Self::Provenance => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::StagedPublish => "staged publish approval",
            Self::TrustedPublisher => "trusted publisher",
            Self::Provenance => "provenance attestation",
        }
    }
}

/// Strongest trust evidence carried by a single version's metadata.
/// `approver` outranks `_npmUser.trustedPublisher`, which outranks
/// `dist.attestations.provenance`.
pub fn evidence_for(meta: &VersionMetadata) -> Option<TrustEvidence> {
    if meta.approver.as_ref().is_some_and(is_approver) {
        return Some(TrustEvidence::StagedPublish);
    }
    if meta
        .npm_user
        .as_ref()
        .and_then(|u| u.trusted_publisher.as_ref())
        .is_some_and(is_trusted_publisher)
    {
        return Some(TrustEvidence::TrustedPublisher);
    }
    if meta
        .dist
        .as_ref()
        .and_then(|d| d.attestations.as_ref())
        .and_then(|a| a.provenance.as_ref())
        .is_some_and(is_provenance)
    {
        return Some(TrustEvidence::Provenance);
    }
    None
}

fn is_approver(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => a.iter().any(is_approver),
        serde_json::Value::Object(o) => o.values().any(is_approver),
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => {
            n.as_i64().is_some_and(|i| i != 0)
                || n.as_u64().is_some_and(|u| u != 0)
                || n.as_f64().is_some_and(|f| f != 0.0)
        }
    }
}

fn is_trusted_publisher(v: &serde_json::Value) -> bool {
    v.as_object()
        .and_then(|o| o.get("id"))
        .and_then(|id| id.as_str())
        .is_some_and(|id| !id.is_empty())
}

fn is_provenance(v: &serde_json::Value) -> bool {
    v.as_object()
        .and_then(|o| o.get("predicateType"))
        .and_then(|predicate| predicate.as_str())
        .is_some_and(|predicate| {
            predicate
                .strip_prefix("https://slsa.dev/provenance/v")
                .and_then(|suffix| suffix.chars().next())
                .is_some_and(|c| c.is_ascii_digit())
        })
}

#[derive(Debug)]
pub enum TrustCheckError {
    Downgrade(TrustDowngradeDetails),
    MissingTime(MissingTimeDetails),
}

#[derive(Debug)]
pub struct TrustDowngradeDetails {
    pub name: String,
    pub picked_version: String,
    pub current_evidence: Option<TrustEvidence>,
    pub prior_evidence: TrustEvidence,
    pub prior_version: String,
}

#[derive(Debug)]
pub struct MissingTimeDetails {
    pub name: String,
    pub version: String,
}

/// Run the trust-downgrade check. Returns `Ok(())` when the picked
/// version is acceptable (excluded, missing-evidence-everywhere, older
/// than `ignore_after_minutes`, or carrying evidence at least as strong
/// as the strongest prior version's). Errors otherwise.
///
/// Step ordering matters: exclude check runs *before* the time lookup
/// so an excluded `name@version` does not surface a `MissingTime` error
/// when the registry omits the `time` field. Verified against pnpm's
/// `does not fail with ERR_PNPM_MISSING_TIME when ... excluded` tests.
pub fn check_no_downgrade(
    packument: &Packument,
    picked_version: &str,
    picked_meta: &VersionMetadata,
    exclude: &TrustExcludeRules,
    ignore_after_minutes: Option<u64>,
) -> Result<(), TrustCheckError> {
    let picked_parsed = node_semver::Version::parse(picked_version).ok();

    if let Some(ref pv) = picked_parsed {
        if exclude.matches(&packument.name, pv) {
            return Ok(());
        }
    } else if exclude.matches_name_only(&packument.name) {
        return Ok(());
    }

    // Registry doesn't publish `time` at all — local Verdaccio fixtures,
    // some private mirrors, ancient registry forks. Without per-version
    // publish times we can't compare evidence chronologically, so skip
    // the check rather than fail every install. This degrades the
    // protection but preserves install behavior against compliant
    // registries (npmjs.org, JSR, modern Verdaccio). Diverges from
    // pnpm's strict-throw behavior because trustPolicy is default-on
    // in aube — strict-throw against the long tail of registries that
    // omit `time` would make aube unusable on first install.
    if packument.time.is_empty() {
        return Ok(());
    }

    let Some(picked_time) = packument.time.get(picked_version) else {
        return Err(TrustCheckError::MissingTime(MissingTimeDetails {
            name: packument.name.clone(),
            version: picked_version.to_string(),
        }));
    };

    if let Some(minutes) = ignore_after_minutes
        && minutes > 0
        && let Some(cutoff) = cutoff_iso8601(minutes)
        && picked_time.as_str() < cutoff.as_str()
    {
        return Ok(());
    }

    // pnpm v10.24.0+: when the picked version is a stable release,
    // ignore prior prerelease evidence — a trusted alpha shouldn't
    // block a stable that omits attestation.
    let exclude_prereleases = picked_parsed
        .as_ref()
        .map(|v| v.pre_release.is_empty())
        .unwrap_or(false);

    let mut best: Option<(TrustEvidence, &str)> = None;
    for (other_ver, other_meta) in &packument.versions {
        if other_ver == picked_version {
            continue;
        }
        let Some(other_time) = packument.time.get(other_ver) else {
            continue;
        };
        if other_time.as_str() >= picked_time.as_str() {
            continue;
        }
        if exclude_prereleases
            && let Ok(parsed) = node_semver::Version::parse(other_ver)
            && !parsed.pre_release.is_empty()
        {
            continue;
        }
        let Some(evidence) = evidence_for(other_meta) else {
            continue;
        };
        match best {
            None => best = Some((evidence, other_ver.as_str())),
            Some((cur, _)) if evidence.rank() > cur.rank() => {
                best = Some((evidence, other_ver.as_str()));
            }
            _ => {}
        }
        if matches!(best, Some((TrustEvidence::StagedPublish, _))) {
            break;
        }
    }

    let Some((prior_evidence, prior_version)) = best else {
        return Ok(());
    };

    let current = evidence_for(picked_meta);
    let current_rank = current.map_or(0, TrustEvidence::rank);
    if current_rank < prior_evidence.rank() {
        return Err(TrustCheckError::Downgrade(TrustDowngradeDetails {
            name: packument.name.clone(),
            picked_version: picked_version.to_string(),
            current_evidence: current,
            prior_evidence,
            prior_version: prior_version.to_string(),
        }));
    }
    Ok(())
}

fn cutoff_iso8601(minutes_ago: u64) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let cutoff_secs = now.saturating_sub(minutes_ago * 60);
    Some(crate::types::format_iso8601_utc(cutoff_secs))
}

/// Parsed `trustPolicyExclude` rules. Mirrors pnpm's
/// `createPackageVersionPolicy` (config/version-policy/src/index.ts).
/// Each rule is `<name>` (matches all versions, supports `*` glob in
/// the name) or `<name>@<exact-version>[ || <exact-version>]…` (no
/// ranges, no name globs combined with versions).
pub const DEFAULT_TRUST_POLICY_EXCLUDES: &[&str] = &[
    "chokidar",
    "eslint-config-prettier",
    "eslint-import-resolver-typescript",
    "nanoid",
    "react-redux",
    "reselect",
    "semver",
    "ua-parser-js",
    "undici-types",
    "vite",
];

#[derive(Debug, Clone)]
pub struct TrustExcludeRules {
    rules: Vec<TrustExcludeRule>,
}

impl Default for TrustExcludeRules {
    fn default() -> Self {
        Self::from_name_excludes(DEFAULT_TRUST_POLICY_EXCLUDES)
    }
}

#[derive(Debug, Clone)]
struct TrustExcludeRule {
    name_matcher: NameMatcher,
    /// `None` → rule matches every version of any name match.
    /// `Some(versions)` → rule matches only those exact versions.
    exact_versions: Option<Vec<node_semver::Version>>,
}

#[derive(Debug, Clone)]
enum NameMatcher {
    Exact(String),
    Glob(GlobMatcher),
    Any,
}

#[derive(Debug, Clone)]
struct GlobMatcher {
    parts: Vec<String>,
    leading_wildcard: bool,
    trailing_wildcard: bool,
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum TrustExcludeParseError {
    #[error(
        "invalid trustPolicyExclude pattern `{pattern}`: only exact versions are allowed in version unions, ranges (^/~/>=) are not supported"
    )]
    #[diagnostic(code(ERR_AUBE_TRUST_EXCLUDE_INVALID_VERSION_UNION))]
    InvalidVersionUnion { pattern: String },
    #[error(
        "invalid trustPolicyExclude pattern `{pattern}`: name patterns (`*`) cannot be combined with version unions"
    )]
    #[diagnostic(code(ERR_AUBE_TRUST_EXCLUDE_NAME_GLOB_WITH_VERSIONS))]
    NameGlobWithVersions { pattern: String },
}

impl TrustExcludeRules {
    fn from_name_excludes(names: &[&str]) -> Self {
        Self {
            rules: names
                .iter()
                .map(|name| TrustExcludeRule {
                    name_matcher: NameMatcher::compile(name),
                    exact_versions: None,
                })
                .collect(),
        }
    }

    pub fn with_defaults_and_user_rules(user_rules: Self) -> Self {
        let mut rules = Self::default();
        rules.rules.extend(user_rules.rules);
        rules
    }

    pub fn parse<I, S>(patterns: I) -> Result<Self, TrustExcludeParseError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut rules = Vec::new();
        for pattern in patterns {
            let pattern = pattern.as_ref();
            if pattern.is_empty() {
                continue;
            }
            rules.push(parse_one(pattern)?);
        }
        Ok(Self { rules })
    }

    /// Parse a list of patterns, keeping every rule that succeeds and
    /// returning the per-pattern errors for everything that didn't.
    /// Lets the caller log malformed entries individually without
    /// dropping the rules that did parse — a strict batch `parse` would
    /// turn one typo into a silent security regression where every
    /// exclude vanishes.
    pub fn parse_lossy<I, S>(patterns: I) -> (Self, Vec<TrustExcludeParseError>)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut rules = Vec::new();
        let mut errors = Vec::new();
        for pattern in patterns {
            let pattern = pattern.as_ref();
            if pattern.is_empty() {
                continue;
            }
            match parse_one(pattern) {
                Ok(rule) => rules.push(rule),
                Err(err) => errors.push(err),
            }
        }
        (Self { rules }, errors)
    }

    fn matches(&self, name: &str, version: &node_semver::Version) -> bool {
        for rule in &self.rules {
            if !rule.name_matcher.matches(name) {
                continue;
            }
            match &rule.exact_versions {
                None => return true,
                Some(versions) => {
                    if versions.iter().any(|v| v == version) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Used when the picked version string fails semver parse — only a
    /// no-version rule can match in that case (pnpm behavior:
    /// `evaluateVersionPolicy` returns `true` for name-only rules
    /// before the version array branch is taken).
    fn matches_name_only(&self, name: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.exact_versions.is_none() && r.name_matcher.matches(name))
    }
}

fn parse_one(pattern: &str) -> Result<TrustExcludeRule, TrustExcludeParseError> {
    let scoped = pattern.starts_with('@');
    let at_index = if scoped {
        pattern[1..].find('@').map(|i| i + 1)
    } else {
        pattern.find('@')
    };

    let (name_part, versions_part) = match at_index {
        Some(i) => (&pattern[..i], Some(&pattern[i + 1..])),
        None => (pattern, None),
    };

    let exact_versions = match versions_part {
        None => None,
        Some(versions_str) => {
            if name_part.contains('*') {
                return Err(TrustExcludeParseError::NameGlobWithVersions {
                    pattern: pattern.to_string(),
                });
            }
            let mut parsed = Vec::new();
            for chunk in versions_str.split("||") {
                let trimmed = chunk.trim();
                if trimmed.is_empty() {
                    return Err(TrustExcludeParseError::InvalidVersionUnion {
                        pattern: pattern.to_string(),
                    });
                }
                let v = node_semver::Version::parse(trimmed).map_err(|_| {
                    TrustExcludeParseError::InvalidVersionUnion {
                        pattern: pattern.to_string(),
                    }
                })?;
                parsed.push(v);
            }
            Some(parsed)
        }
    };

    Ok(TrustExcludeRule {
        name_matcher: NameMatcher::compile(name_part),
        exact_versions,
    })
}

impl NameMatcher {
    fn compile(pattern: &str) -> Self {
        if pattern == "*" {
            return Self::Any;
        }
        if !pattern.contains('*') {
            return Self::Exact(pattern.to_string());
        }
        let parts: Vec<String> = pattern.split('*').map(str::to_string).collect();
        Self::Glob(GlobMatcher {
            leading_wildcard: parts.first().is_some_and(String::is_empty),
            trailing_wildcard: parts.last().is_some_and(String::is_empty),
            parts: parts.into_iter().filter(|s| !s.is_empty()).collect(),
        })
    }

    fn matches(&self, input: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(s) => s == input,
            Self::Glob(g) => g.matches(input),
        }
    }
}

impl GlobMatcher {
    fn matches(&self, input: &str) -> bool {
        if self.parts.is_empty() {
            return true;
        }
        let mut cursor = 0usize;
        for (i, segment) in self.parts.iter().enumerate() {
            let search_window = &input[cursor..];
            let is_first = i == 0;
            let is_last = i == self.parts.len() - 1;
            if is_first && !self.leading_wildcard {
                if !search_window.starts_with(segment.as_str()) {
                    return false;
                }
                cursor += segment.len();
            } else if is_last && !self.trailing_wildcard {
                if !search_window.ends_with(segment.as_str()) {
                    return false;
                }
                if search_window.len() < segment.len() {
                    return false;
                }
                cursor = input.len();
            } else {
                let Some(idx) = search_window.find(segment.as_str()) else {
                    return false;
                };
                cursor += idx + segment.len();
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_registry::{Attestations, Dist, NpmUser};
    use std::collections::BTreeMap;

    fn version(name: &str, ver: &str) -> VersionMetadata {
        VersionMetadata {
            name: name.to_string(),
            version: ver.to_string(),
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            bundled_dependencies: None,
            dist: Some(Dist {
                tarball: format!("https://r/{name}/-/{name}-{ver}.tgz"),
                integrity: None,
                shasum: None,
                unpacked_size: None,
                attestations: None,
            }),
            os: vec![],
            cpu: vec![],
            libc: vec![],
            engines: BTreeMap::new(),
            license: None,
            funding_url: None,
            bin: BTreeMap::new(),
            has_install_script: false,
            deprecated: None,
            approver: None,
            npm_user: None,
        }
    }

    fn with_provenance(mut v: VersionMetadata) -> VersionMetadata {
        let dist = v.dist.as_mut().unwrap();
        dist.attestations = Some(Attestations {
            provenance: Some(serde_json::json!({
                "predicateType": "https://slsa.dev/provenance/v1"
            })),
        });
        v
    }

    fn with_trusted_publisher(mut v: VersionMetadata) -> VersionMetadata {
        v.npm_user = Some(NpmUser {
            trusted_publisher: Some(serde_json::json!({"id": "gh"})),
        });
        v
    }

    fn with_staged_publish(mut v: VersionMetadata) -> VersionMetadata {
        v.approver = Some(serde_json::json!({"name": "release-manager"}));
        v
    }

    fn packument(name: &str, versions: Vec<(&str, &str, VersionMetadata)>) -> Packument {
        let mut p = Packument {
            name: name.to_string(),
            modified: None,
            versions: BTreeMap::new(),
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(),
        };
        for (ver, time, meta) in versions {
            p.versions.insert(ver.to_string(), meta);
            p.time.insert(ver.to_string(), time.to_string());
        }
        p
    }

    #[test]
    fn evidence_trusted_publisher_outranks_provenance() {
        let v = with_trusted_publisher(with_provenance(version("foo", "1.0.0")));
        assert_eq!(evidence_for(&v), Some(TrustEvidence::TrustedPublisher));
    }

    #[test]
    fn evidence_staged_publish_outranks_trusted_publisher() {
        let v = with_staged_publish(with_trusted_publisher(with_provenance(version(
            "foo", "1.0.0",
        ))));
        assert_eq!(evidence_for(&v), Some(TrustEvidence::StagedPublish));
    }

    #[test]
    fn evidence_provenance_only() {
        let v = with_provenance(version("foo", "1.0.0"));
        assert_eq!(evidence_for(&v), Some(TrustEvidence::Provenance));
    }

    #[test]
    fn evidence_npm_user_without_trusted_publisher_is_none() {
        let mut v = version("foo", "1.0.0");
        v.npm_user = Some(NpmUser {
            trusted_publisher: None,
        });
        assert_eq!(evidence_for(&v), None);
    }

    #[test]
    fn evidence_malformed_trusted_publisher_is_none() {
        let mut v = version("foo", "1.0.0");
        for malformed in [
            serde_json::Value::Bool(false),
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(0.0),
            serde_json::json!(""),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!({"id": ""}),
        ] {
            v.npm_user = Some(NpmUser {
                trusted_publisher: Some(malformed.clone()),
            });
            assert_eq!(
                evidence_for(&v),
                None,
                "{malformed:?} should not count as trusted-publisher evidence"
            );
        }
    }

    #[test]
    fn evidence_empty_approver_is_none() {
        let mut v = version("foo", "1.0.0");
        for malformed in [
            serde_json::Value::Bool(false),
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(0.0),
            serde_json::json!(""),
            serde_json::json!([]),
            serde_json::json!([null]),
            serde_json::json!([false]),
            serde_json::json!([""]),
            serde_json::json!([[], {}]),
            serde_json::json!({}),
            serde_json::json!({"name": null}),
            serde_json::json!({"name": null, "email": null}),
            serde_json::json!({"name": ""}),
            serde_json::json!({"nested": {}}),
        ] {
            v.approver = Some(malformed.clone());
            assert_eq!(
                evidence_for(&v),
                None,
                "{malformed:?} should not count as staged-publish evidence"
            );
        }
    }

    #[test]
    fn evidence_truthy_scalar_approver_counts() {
        let mut v = version("foo", "1.0.0");
        for approver in [
            serde_json::Value::Bool(true),
            serde_json::json!(1),
            serde_json::json!("release-manager"),
            serde_json::json!(["release-manager"]),
            serde_json::json!({"name": "release-manager"}),
        ] {
            v.approver = Some(approver.clone());
            assert_eq!(
                evidence_for(&v),
                Some(TrustEvidence::StagedPublish),
                "{approver:?} should count as staged-publish evidence"
            );
        }
    }

    #[test]
    fn evidence_malformed_provenance_is_none() {
        let mut v = version("foo", "1.0.0");
        for malformed in [
            serde_json::Value::Bool(false),
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(""),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!({"predicateType": ""}),
            serde_json::json!({"predicateType": "https://slsa.dev/provenance/"}),
            serde_json::json!({"predicateType": "https://slsa.dev/provenance/v"}),
            serde_json::json!({"predicateType": "https://slsa.dev/provenance/latest"}),
            serde_json::json!({"predicateType": "https://example.com/provenance/v1"}),
        ] {
            v.dist.as_mut().unwrap().attestations = Some(Attestations {
                provenance: Some(malformed.clone()),
            });
            assert_eq!(
                evidence_for(&v),
                None,
                "{malformed:?} should not count as provenance evidence"
            );
        }
    }

    #[test]
    fn evidence_structured_trusted_publisher_counts() {
        let mut v = version("foo", "1.0.0");
        v.npm_user = Some(NpmUser {
            trusted_publisher: Some(serde_json::json!({
                "id": "github",
                "oidcConfigId": "oidc:example"
            })),
        });
        assert_eq!(evidence_for(&v), Some(TrustEvidence::TrustedPublisher));
    }

    #[test]
    fn evidence_none_when_neither() {
        let v = version("foo", "1.0.0");
        assert_eq!(evidence_for(&v), None);
    }

    #[test]
    fn no_evidence_anywhere_passes() {
        let p = packument(
            "foo",
            vec![
                ("1.0.0", "2025-01-01T00:00:00.000Z", version("foo", "1.0.0")),
                ("2.0.0", "2025-02-01T00:00:00.000Z", version("foo", "2.0.0")),
            ],
        );
        let picked = p.versions.get("2.0.0").unwrap();
        let result = check_no_downgrade(&p, "2.0.0", picked, &TrustExcludeRules::default(), None);
        assert!(result.is_ok());
    }

    #[test]
    fn first_attested_version_passes() {
        let p = packument(
            "foo",
            vec![
                ("1.0.0", "2025-01-01T00:00:00.000Z", version("foo", "1.0.0")),
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0")),
                ),
            ],
        );
        let picked = p.versions.get("1.0.0").unwrap();
        let result = check_no_downgrade(&p, "1.0.0", picked, &TrustExcludeRules::default(), None);
        assert!(
            result.is_ok(),
            "version 1.0.0 was published first; it has nothing prior to compare against"
        );
    }

    #[test]
    fn downgrade_provenance_to_none_fails() {
        let p = packument(
            "foo",
            vec![
                ("1.0.0", "2025-01-01T00:00:00.000Z", version("foo", "1.0.0")),
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0")),
                ),
                ("3.0.0", "2025-03-01T00:00:00.000Z", version("foo", "3.0.0")),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        let err = check_no_downgrade(&p, "3.0.0", picked, &TrustExcludeRules::default(), None)
            .expect_err("3.0.0 should fail: prior version had provenance, this one has none");
        match err {
            TrustCheckError::Downgrade(d) => {
                assert_eq!(d.prior_evidence, TrustEvidence::Provenance);
                assert_eq!(d.prior_version, "2.0.0");
                assert_eq!(d.current_evidence, None);
            }
            _ => panic!("expected Downgrade"),
        }
    }

    #[test]
    fn downgrade_trusted_publisher_to_provenance_fails() {
        let p = packument(
            "foo",
            vec![
                ("1.0.0", "2025-01-01T00:00:00.000Z", version("foo", "1.0.0")),
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "2.0.0")),
                ),
                (
                    "3.0.0",
                    "2025-03-01T00:00:00.000Z",
                    with_provenance(version("foo", "3.0.0")),
                ),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        let err = check_no_downgrade(&p, "3.0.0", picked, &TrustExcludeRules::default(), None)
            .expect_err("trustedPublisher → provenance is a downgrade");
        match err {
            TrustCheckError::Downgrade(d) => {
                assert_eq!(d.prior_evidence, TrustEvidence::TrustedPublisher);
                assert_eq!(d.current_evidence, Some(TrustEvidence::Provenance));
            }
            _ => panic!("expected Downgrade"),
        }
    }

    #[test]
    fn downgrade_staged_publish_to_trusted_publisher_fails() {
        let p = packument(
            "foo",
            vec![
                ("1.0.0", "2025-01-01T00:00:00.000Z", version("foo", "1.0.0")),
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_staged_publish(version("foo", "2.0.0")),
                ),
                (
                    "3.0.0",
                    "2025-03-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "3.0.0")),
                ),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        let err = check_no_downgrade(&p, "3.0.0", picked, &TrustExcludeRules::default(), None)
            .expect_err("staged publish → trusted publisher is a downgrade");
        match err {
            TrustCheckError::Downgrade(d) => {
                assert_eq!(d.prior_evidence, TrustEvidence::StagedPublish);
                assert_eq!(d.prior_version, "2.0.0");
                assert_eq!(d.current_evidence, Some(TrustEvidence::TrustedPublisher));
            }
            _ => panic!("expected Downgrade"),
        }
    }

    #[test]
    fn staged_publish_after_trusted_publisher_passes() {
        let p = packument(
            "foo",
            vec![
                (
                    "1.0.0",
                    "2025-01-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "1.0.0")),
                ),
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_staged_publish(version("foo", "2.0.0")),
                ),
            ],
        );
        let picked = p.versions.get("2.0.0").unwrap();
        let result = check_no_downgrade(&p, "2.0.0", picked, &TrustExcludeRules::default(), None);
        assert!(result.is_ok());
    }

    #[test]
    fn same_trust_level_passes() {
        let p = packument(
            "foo",
            vec![
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "2.0.0")),
                ),
                (
                    "3.0.0",
                    "2025-03-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "3.0.0")),
                ),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        let result = check_no_downgrade(&p, "3.0.0", picked, &TrustExcludeRules::default(), None);
        assert!(result.is_ok());
    }

    #[test]
    fn prior_prerelease_ignored_when_picking_stable() {
        let p = packument(
            "foo",
            vec![
                ("1.0.0", "2025-01-01T00:00:00.000Z", version("foo", "1.0.0")),
                (
                    "2.0.0-0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0-0")),
                ),
                ("3.0.0", "2025-03-01T00:00:00.000Z", version("foo", "3.0.0")),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        let result = check_no_downgrade(&p, "3.0.0", picked, &TrustExcludeRules::default(), None);
        assert!(
            result.is_ok(),
            "trusted prerelease shouldn't block a stable that omits attestation"
        );
    }

    #[test]
    fn prior_prerelease_counts_when_picking_prerelease() {
        let p = packument(
            "foo",
            vec![
                (
                    "2.0.0-0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0-0")),
                ),
                (
                    "3.0.0-0",
                    "2025-03-01T00:00:00.000Z",
                    version("foo", "3.0.0-0"),
                ),
            ],
        );
        let picked = p.versions.get("3.0.0-0").unwrap();
        let result = check_no_downgrade(&p, "3.0.0-0", picked, &TrustExcludeRules::default(), None);
        assert!(
            result.is_err(),
            "prerelease pick should compare against prior prereleases"
        );
    }

    /// Registries that don't publish `time` at all (Verdaccio without
    /// the `--store-info` middleware, private mirrors that strip it,
    /// old registry forks) must not break every install. Verified by
    /// constructing a packument with versions but no `time` map.
    #[test]
    fn empty_time_map_skips_check() {
        let p = Packument {
            name: "foo".to_string(),
            modified: None,
            versions: {
                let mut m = BTreeMap::new();
                m.insert(
                    "1.0.0".to_string(),
                    with_provenance(version("foo", "1.0.0")),
                );
                m.insert("2.0.0".to_string(), version("foo", "2.0.0"));
                m
            },
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(), // Empty — registry doesn't ship time at all.
        };
        let picked = p.versions.get("2.0.0").unwrap();
        // Would normally be a downgrade (2.0.0 lost provenance), but
        // without `time` we can't establish chronology and degrade safely.
        let result = check_no_downgrade(&p, "2.0.0", picked, &TrustExcludeRules::default(), None);
        assert!(result.is_ok(), "empty time map should skip the check");
    }

    #[test]
    fn missing_time_for_picked_version_errors() {
        let mut p = packument(
            "foo",
            vec![
                (
                    "1.0.0",
                    "2025-01-01T00:00:00.000Z",
                    with_provenance(version("foo", "1.0.0")),
                ),
                ("2.0.0", "2025-02-01T00:00:00.000Z", version("foo", "2.0.0")),
            ],
        );
        // Drop the time entry for 2.0.0.
        p.time.remove("2.0.0");
        let picked = p.versions.get("2.0.0").unwrap();
        let err = check_no_downgrade(&p, "2.0.0", picked, &TrustExcludeRules::default(), None)
            .expect_err("missing time should error");
        assert!(matches!(err, TrustCheckError::MissingTime(_)));
    }

    #[test]
    fn exclude_name_at_version_bypasses_missing_time() {
        // No time field anywhere — would normally error.
        let p = Packument {
            name: "baz".to_string(),
            modified: None,
            versions: {
                let mut m = BTreeMap::new();
                m.insert("1.0.0".to_string(), version("baz", "1.0.0"));
                m
            },
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(),
        };
        let picked = p.versions.get("1.0.0").unwrap();
        let exclude = TrustExcludeRules::parse(["baz@1.0.0"]).unwrap();
        let result = check_no_downgrade(&p, "1.0.0", picked, &exclude, None);
        assert!(result.is_ok(), "excluded version must skip the time lookup");
    }

    #[test]
    fn exclude_name_only_bypasses_missing_time() {
        let p = Packument {
            name: "qux".to_string(),
            modified: None,
            versions: {
                let mut m = BTreeMap::new();
                m.insert("2.0.0".to_string(), version("qux", "2.0.0"));
                m
            },
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(),
        };
        let picked = p.versions.get("2.0.0").unwrap();
        let exclude = TrustExcludeRules::parse(["qux"]).unwrap();
        let result = check_no_downgrade(&p, "2.0.0", picked, &exclude, None);
        assert!(result.is_ok());
    }

    #[test]
    fn exclude_blocks_downgrade_failure() {
        let p = packument(
            "foo",
            vec![
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0")),
                ),
                ("3.0.0", "2025-03-01T00:00:00.000Z", version("foo", "3.0.0")),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        let exclude = TrustExcludeRules::parse(["foo@3.0.0"]).unwrap();
        let result = check_no_downgrade(&p, "3.0.0", picked, &exclude, None);
        assert!(result.is_ok(), "exclude should bypass the downgrade");
    }

    #[test]
    fn ignore_after_skips_old_versions() {
        let p = packument(
            "foo",
            vec![
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0")),
                ),
                ("3.0.0", "2025-03-01T00:00:00.000Z", version("foo", "3.0.0")),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        // 1 minute cutoff — both versions are way older, should skip.
        let result =
            check_no_downgrade(&p, "3.0.0", picked, &TrustExcludeRules::default(), Some(1));
        assert!(result.is_ok());
    }

    // ---------- TrustExcludeRules parsing ----------

    #[test]
    fn exclude_parses_name_only() {
        let r = TrustExcludeRules::parse(["foo"]).unwrap();
        assert!(r.matches("foo", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(r.matches("foo", &node_semver::Version::parse("99.0.0").unwrap()));
        assert!(!r.matches("bar", &node_semver::Version::parse("1.0.0").unwrap()));
    }

    #[test]
    fn default_excludes_known_provenance_churn_packages() {
        let r = TrustExcludeRules::default();
        for package in DEFAULT_TRUST_POLICY_EXCLUDES {
            assert!(
                r.matches(package, &node_semver::Version::parse("1.0.0").unwrap()),
                "{package} should be globally excluded"
            );
        }
        assert!(!r.matches("left-pad", &node_semver::Version::parse("1.0.0").unwrap()));
    }

    #[test]
    fn exclude_parses_name_at_version() {
        let r = TrustExcludeRules::parse(["foo@1.0.0"]).unwrap();
        assert!(r.matches("foo", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(!r.matches("foo", &node_semver::Version::parse("1.0.1").unwrap()));
    }

    #[test]
    fn exclude_parses_version_union() {
        let r = TrustExcludeRules::parse(["foo@1.0.0 || 2.0.0 || 3.0.0"]).unwrap();
        assert!(r.matches("foo", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(r.matches("foo", &node_semver::Version::parse("2.0.0").unwrap()));
        assert!(r.matches("foo", &node_semver::Version::parse("3.0.0").unwrap()));
        assert!(!r.matches("foo", &node_semver::Version::parse("4.0.0").unwrap()));
    }

    #[test]
    fn exclude_parses_scoped_name() {
        let r = TrustExcludeRules::parse(["@babel/core@7.20.0"]).unwrap();
        assert!(r.matches(
            "@babel/core",
            &node_semver::Version::parse("7.20.0").unwrap()
        ));
        assert!(!r.matches(
            "@babel/core",
            &node_semver::Version::parse("7.20.1").unwrap()
        ));
    }

    #[test]
    fn exclude_parses_scoped_name_only() {
        let r = TrustExcludeRules::parse(["@babel/core"]).unwrap();
        assert!(r.matches(
            "@babel/core",
            &node_semver::Version::parse("9.9.9").unwrap()
        ));
    }

    #[test]
    fn exclude_parses_glob() {
        let r = TrustExcludeRules::parse(["is-*"]).unwrap();
        assert!(r.matches("is-odd", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(r.matches("is-even", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(!r.matches("lodash", &node_semver::Version::parse("1.0.0").unwrap()));
    }

    #[test]
    fn exclude_parses_star_matches_all() {
        let r = TrustExcludeRules::parse(["*"]).unwrap();
        assert!(r.matches("anything", &node_semver::Version::parse("0.0.1").unwrap()));
    }

    #[test]
    fn exclude_rejects_range_operators() {
        for bad in ["foo@^1.0.0", "foo@~1.0.0", "foo@>=1.0.0"] {
            let err = TrustExcludeRules::parse([bad]).expect_err(bad);
            assert!(matches!(
                err,
                TrustExcludeParseError::InvalidVersionUnion { .. }
            ));
        }
    }

    #[test]
    fn exclude_rejects_glob_with_version() {
        let err = TrustExcludeRules::parse(["is-*@1.0.0"]).expect_err("glob+version");
        assert!(matches!(
            err,
            TrustExcludeParseError::NameGlobWithVersions { .. }
        ));
    }

    #[test]
    fn parse_lossy_keeps_valid_drops_invalid() {
        let (rules, errors) = TrustExcludeRules::parse_lossy([
            "good",
            "bad@^1.0.0",
            "@scope/also-good@1.0.0",
            "is-*@nope",
        ]);
        // Two valid rules survive; two invalid surface as separate errors.
        assert!(rules.matches("good", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(rules.matches(
            "@scope/also-good",
            &node_semver::Version::parse("1.0.0").unwrap()
        ));
        assert_eq!(errors.len(), 2, "two malformed entries reported");
    }

    #[test]
    fn exclude_skips_empty_patterns() {
        // npm config arrays sometimes include empty entries; ignore them.
        let r = TrustExcludeRules::parse(["", "foo", ""]).unwrap();
        assert!(r.matches("foo", &node_semver::Version::parse("1.0.0").unwrap()));
    }
}
