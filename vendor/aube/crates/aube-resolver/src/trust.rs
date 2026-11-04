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

fn compact_evidence_for(meta: &aube_registry::VersionTrustMetadata) -> Option<TrustEvidence> {
    if meta.approver.as_ref().is_some_and(is_approver) {
        return Some(TrustEvidence::StagedPublish);
    }
    if meta
        .npm_user
        .as_ref()
        .and_then(|user| user.trusted_publisher.as_ref())
        .is_some_and(is_trusted_publisher)
    {
        return Some(TrustEvidence::TrustedPublisher);
    }
    if meta
        .dist
        .as_ref()
        .and_then(|dist| dist.attestations.as_ref())
        .and_then(|attestations| attestations.provenance.as_ref())
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

/// The strongest trust evidence carried by a version published before the
/// selected release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorTrustEvidence {
    pub version: String,
    pub evidence: TrustEvidence,
}

/// Find the strongest trust evidence on a release published before
/// `picked_version`.
///
/// Stable releases ignore evidence from prereleases, matching
/// [`check_no_downgrade`]. Returns `None` when the selected version has no
/// publish time or no earlier release carries trust evidence.
pub fn strongest_prior_evidence(
    packument: &Packument,
    picked_version: &str,
) -> Option<PriorTrustEvidence> {
    let picked_time = packument.time.get(picked_version)?;
    let exclude_prereleases = node_semver::Version::parse(picked_version)
        .map(|v| v.pre_release.is_empty())
        .unwrap_or(false);

    let mut best: Option<PriorTrustEvidence> = None;
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
            None => {
                best = Some(PriorTrustEvidence {
                    version: other_ver.clone(),
                    evidence,
                });
            }
            Some(ref current) if evidence.rank() > current.evidence.rank() => {
                best = Some(PriorTrustEvidence {
                    version: other_ver.clone(),
                    evidence,
                });
            }
            _ => {}
        }
        if matches!(
            best,
            Some(PriorTrustEvidence {
                evidence: TrustEvidence::StagedPublish,
                ..
            })
        ) {
            break;
        }
    }
    best
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

    let Some(prior) = strongest_prior_evidence(packument, picked_version) else {
        return Ok(());
    };

    let current = evidence_for(picked_meta);
    let current_rank = current.map_or(0, TrustEvidence::rank);
    if current_rank < prior.evidence.rank() {
        return Err(TrustCheckError::Downgrade(TrustDowngradeDetails {
            name: packument.name.clone(),
            picked_version: picked_version.to_string(),
            current_evidence: current,
            prior_evidence: prior.evidence,
            prior_version: prior.version,
        }));
    }
    Ok(())
}

/// Find the best version satisfying `range_str` that clears both the age
/// cutoff and [`check_no_downgrade`], searching away from a version the
/// trust gate just rejected.
///
/// The age gate already backtracks: `pick_version` scans every satisfying
/// version and keeps the newest one clearing the cutoff, so a too-new release
/// costs the user an older pick rather than the whole install. The trust gate
/// used to abort instead, which deadlocks whenever a package publishes one
/// version manually between two attested ones — the age gate walks down to the
/// manual publish, the trust gate refuses it, and a fully-signed release
/// sitting one version lower is never considered. This makes the two gates
/// consistent. It is a deliberate divergence from pnpm's
/// `failIfTrustDowngraded`, which throws: pnpm implements its own age gate as a
/// packument FILTER (`filterPkgMetadataByPublishDate`) and its trust gate as a
/// post-pick throw, so the same asymmetry is baked into upstream.
///
/// Bounded at `rejected_version` in the pick's own direction — strictly lower
/// for a highest-wins pick, strictly higher for `pick_lowest`. Unbounded, a
/// re-pick could climb ABOVE `dist-tags.latest` and hand back a release the
/// publisher has already moved the tag off, which is the same trap
/// `pick_version`'s `<=` fallback avoids for a gated `latest`.
///
/// Two tiers, mirroring [`crate::resolve::vulnerable::prefer_non_vulnerable_pick`]:
/// a known-vulnerable version is a worse answer than a clean one, but still a
/// better answer than failing the install, so it ranks second rather than
/// being excluded. Trust and age are hard filters in both tiers.
///
/// Cost is `O(versions²)` in the worst case — `check_no_downgrade` walks the
/// packument once per candidate — and is paid only on the failure path, after
/// the ordinary pick has already been refused.
#[allow(clippy::too_many_arguments)]
pub(crate) fn repick_past_downgrade<'a>(
    packument: &'a Packument,
    registry_name: &str,
    range_str: &str,
    rejected_version: &str,
    pick_lowest: bool,
    cutoff: Option<&str>,
    exempt_cutoff: Option<&str>,
    strict: bool,
    exclude: &TrustExcludeRules,
    ignore_after_minutes: Option<u64>,
    vulnerable_ranges: &std::collections::BTreeMap<String, Vec<String>>,
    is_age_exempt: impl Fn(&str, Option<&node_semver::Version>) -> bool,
) -> Option<&'a VersionMetadata> {
    // Mirror `pick_version`'s dist-tag handling. A bare tag is not a semver
    // range, and a `latest` the gate refused widens to everything at or below
    // the tag so the scan can reach the release under it — the same widening
    // `pick_version` does for an age-gated `latest` (#681), which is the shape
    // `dlx` and an unversioned `add` always take. Restricted to `latest`
    // pointing at a STABLE release: every other tag resolves to exactly one
    // version, which has no lower alternative by definition, and a channel
    // pointer like `next` must not leak a stable release into a prerelease
    // line.
    let range = match node_semver::Range::parse(crate::semver_util::normalize_range(range_str)) {
        Ok(range) => range,
        Err(_) if range_str == "latest" => {
            let latest = packument.dist_tags.get("latest")?;
            if !node_semver::Version::parse(latest)
                .ok()?
                .pre_release
                .is_empty()
            {
                return None;
            }
            node_semver::Range::parse(format!("<={latest}")).ok()?
        }
        Err(_) => return None,
    };
    let rejected = node_semver::Version::parse(rejected_version).ok()?;

    let mut best: Option<(node_semver::Version, &'a VersionMetadata)> = None;
    let mut best_vulnerable: Option<(node_semver::Version, &'a VersionMetadata)> = None;
    for (ver_str, meta) in &packument.versions {
        let Ok(version) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        // Away from the rejected pick, never past it.
        if pick_lowest {
            if version <= rejected {
                continue;
            }
        } else if version >= rejected {
            continue;
        }
        if !version.satisfies(&range) {
            continue;
        }
        let effective = if is_age_exempt(ver_str, Some(&version)) {
            exempt_cutoff
        } else {
            cutoff
        };
        if !crate::semver_util::version_clears_cutoff(packument, ver_str, effective, strict) {
            continue;
        }
        if check_no_downgrade(packument, ver_str, meta, exclude, ignore_after_minutes).is_err() {
            continue;
        }
        let tier =
            if crate::resolve::vulnerable::is_vulnerable(registry_name, ver_str, vulnerable_ranges)
            {
                &mut best_vulnerable
            } else {
                &mut best
            };
        if crate::semver_util::outranks(&version, meta, tier.as_ref(), pick_lowest) {
            *tier = Some((version, meta));
        }
    }
    best.or(best_vulnerable).map(|(_, meta)| meta)
}

/// Trust-policy check using the compact history fetched for an exact
/// dependency. The selected release remains full [`VersionMetadata`]; only
/// historical releases use the evidence-only representation.
pub fn check_no_downgrade_compact(
    packument: &Packument,
    picked_version: &str,
    picked_meta: &VersionMetadata,
    history: &std::collections::BTreeMap<String, aube_registry::VersionTrustMetadata>,
    exclude: &TrustExcludeRules,
    ignore_after_minutes: Option<u64>,
) -> Result<(), TrustCheckError> {
    check_no_downgrade_over_history(
        &packument.name,
        &packument.time,
        picked_version,
        evidence_for(picked_meta),
        history,
        exclude,
        ignore_after_minutes,
    )
}

/// Trust-policy check over a standalone compact trust history — no full
/// [`Packument`] or [`VersionMetadata`] in hand. Used by the lockfile
/// validator, which fetches [`aube_registry::PackumentTrustHistory`]
/// per name; `history.versions` here *includes* the picked version
/// (the shared loop skips it when ranking prior evidence).
pub fn check_no_downgrade_history(
    name: &str,
    history: &aube_registry::PackumentTrustHistory,
    picked_version: &str,
    picked_meta: &aube_registry::VersionTrustMetadata,
    exclude: &TrustExcludeRules,
    ignore_after_minutes: Option<u64>,
) -> Result<(), TrustCheckError> {
    check_no_downgrade_over_history(
        name,
        &history.time,
        picked_version,
        compact_evidence_for(picked_meta),
        &history.versions,
        exclude,
        ignore_after_minutes,
    )
}

/// Shared core for the compact-history trust checks: rank the strongest
/// prior evidence in `history` and reject a picked version that weakens
/// it. `history` may or may not contain `picked_version` itself — the
/// ranking loop always skips it.
fn check_no_downgrade_over_history(
    name: &str,
    time: &std::collections::BTreeMap<String, String>,
    picked_version: &str,
    picked_evidence: Option<TrustEvidence>,
    history: &std::collections::BTreeMap<String, aube_registry::VersionTrustMetadata>,
    exclude: &TrustExcludeRules,
    ignore_after_minutes: Option<u64>,
) -> Result<(), TrustCheckError> {
    let picked_parsed = node_semver::Version::parse(picked_version).ok();
    if let Some(ref version) = picked_parsed {
        if exclude.matches(name, version) {
            return Ok(());
        }
    } else if exclude.matches_name_only(name) {
        return Ok(());
    }
    if time.is_empty() {
        return Ok(());
    }
    let Some(picked_time) = time.get(picked_version) else {
        return Err(TrustCheckError::MissingTime(MissingTimeDetails {
            name: name.to_string(),
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

    let exclude_prereleases = picked_parsed
        .as_ref()
        .map(|version| version.pre_release.is_empty())
        .unwrap_or(false);
    let mut strongest: Option<PriorTrustEvidence> = None;
    for (version, meta) in history {
        if version == picked_version {
            continue;
        }
        let Some(published_at) = time.get(version) else {
            continue;
        };
        if published_at >= picked_time {
            continue;
        }
        if exclude_prereleases
            && let Ok(parsed) = node_semver::Version::parse(version)
            && !parsed.pre_release.is_empty()
        {
            continue;
        }
        let Some(evidence) = compact_evidence_for(meta) else {
            continue;
        };
        if strongest
            .as_ref()
            .is_none_or(|current| evidence.rank() > current.evidence.rank())
        {
            strongest = Some(PriorTrustEvidence {
                version: version.clone(),
                evidence,
            });
        }
        if strongest
            .as_ref()
            .is_some_and(|prior| prior.evidence == TrustEvidence::StagedPublish)
        {
            break;
        }
    }
    let Some(prior) = strongest else {
        return Ok(());
    };
    if picked_evidence.map_or(0, TrustEvidence::rank) < prior.evidence.rank() {
        return Err(TrustCheckError::Downgrade(TrustDowngradeDetails {
            name: name.to_string(),
            picked_version: picked_version.to_string(),
            current_evidence: picked_evidence,
            prior_evidence: prior.evidence,
            prior_version: prior.version,
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
/// the name) or `<name>@<semver-range>[ || <semver-range>]…` (no name
/// globs combined with versions).
pub const DEFAULT_TRUST_POLICY_EXCLUDES: &[&str] = &[
    // @octokit maintains several major-version lines in parallel and backports
    // fixes to older lines without provenance attestation. A backport (e.g.
    // @octokit/endpoint@9.0.6) is published after an attested newer major
    // (10.1.0), so the no-downgrade check flags the legitimate older release.
    "@octokit/endpoint",
    // @hono/node-server@1.19.15 (2026-07-24) was hand-published without
    // attestation after the trusted 2.0.10 release (2026-07-15). Trusted
    // publishing resumed with 1.19.17, so keep the exception version-scoped.
    "@hono/node-server@1.19.15",
    "chokidar",
    "eslint-config-prettier",
    "eslint-import-resolver-typescript",
    "nanoid",
    "react-redux",
    "reselect",
    "semver",
    "ua-parser-js",
    "undici",
    "undici-types",
    "vite",
];

/// A parsed package-version policy: a set of `<name>[@<semver-range>…]`
/// rules with `*` name globs. pnpm backs both `trustPolicyExclude` and
/// `minimumReleaseAgeExclude` with the same `createPackageVersionPolicy`
/// engine, so we do too — `TrustExcludeRules` is the neutral
/// [`PackageVersionPolicy`] type seeded with the trust defaults, while
/// `minimumReleaseAgeExclude` builds an empty one from user rules only.
///
/// # Warning
///
/// Because this is an alias for [`TrustExcludeRules`],
/// `PackageVersionPolicy::default()` runs that type's [`Default`] impl,
/// which seeds [`DEFAULT_TRUST_POLICY_EXCLUDES`] (40+ well-known
/// packages). For the age gate that is wrong — it would silently exempt
/// those packages from `minimumReleaseAge`. Age-gate call sites must use
/// [`TrustExcludeRules::empty`]; never `default()`, `#[derive(Default)]`
/// on a containing struct, or `unwrap_or_default()`.
pub type PackageVersionPolicy = TrustExcludeRules;

#[derive(Debug, Clone)]
pub struct TrustExcludeRules {
    rules: Vec<TrustExcludeRule>,
}

impl Default for TrustExcludeRules {
    fn default() -> Self {
        // Parse the entries rather than assuming each is a bare name, so a
        // future `name@range` default actually works. The previous
        // `from_name_excludes` hardcoded `version_ranges: None`, which would
        // have compiled such an entry into a name matcher for the literal
        // string `"name@range"` — silently matching nothing and dropping the
        // exemption rather than erroring. Every current entry is a bare name,
        // for which this is behavior-identical. The list is a compile-time
        // constant, so a malformed entry is an authoring bug;
        // `default_excludes_known_provenance_churn_packages` asserts each one
        // parses.
        Self::parse(DEFAULT_TRUST_POLICY_EXCLUDES)
            .expect("DEFAULT_TRUST_POLICY_EXCLUDES must be valid exclude patterns")
    }
}

#[derive(Debug, Clone)]
struct TrustExcludeRule {
    name_matcher: NameMatcher,
    /// `None` → rule matches every version of any name match.
    /// `Some(ranges)` → rule matches any version satisfying one range.
    version_ranges: Option<Vec<node_semver::Range>>,
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

// Shared by both `trustPolicyExclude` and `minimumReleaseAgeExclude`, so
// the message text stays setting-neutral — the caller's log line names
// the specific setting the bad entry came from.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum TrustExcludeParseError {
    #[error("invalid exclude pattern `{pattern}`: version selectors must be valid semver ranges")]
    #[diagnostic(code(ERR_AUBE_TRUST_EXCLUDE_INVALID_VERSION_UNION))]
    InvalidVersionUnion { pattern: String },
    #[error(
        "invalid exclude pattern `{pattern}`: name patterns (`*`) cannot be combined with version unions"
    )]
    #[diagnostic(code(ERR_AUBE_TRUST_EXCLUDE_NAME_GLOB_WITH_VERSIONS))]
    NameGlobWithVersions { pattern: String },
}

impl TrustExcludeRules {
    /// A policy that matches nothing. Used as the
    /// `minimumReleaseAgeExclude` default — unlike [`Default`], which
    /// seeds the trust-specific exclude list, the age gate must start
    /// with no exemptions.
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Return whether this policy excludes `name@version`.
    pub fn excludes(&self, name: &str, version: &str) -> bool {
        match node_semver::Version::parse(version) {
            Ok(version) => self.matches(name, &version),
            Err(_) => self.matches_name_only(name),
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

    pub(crate) fn matches(&self, name: &str, version: &node_semver::Version) -> bool {
        for rule in &self.rules {
            if !rule.name_matcher.matches(name) {
                continue;
            }
            match &rule.version_ranges {
                None => return true,
                Some(ranges) => {
                    if ranges.iter().any(|r| version.satisfies(r)) {
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
    pub(crate) fn matches_name_only(&self, name: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.version_ranges.is_none() && r.name_matcher.matches(name))
    }

    /// True when `name` is matched by at least one *version-pinned* rule
    /// (a rule carrying a version-range union — jdx/aube#989 generalized the
    /// former exact-version list to ranges; the predicate is unchanged).
    /// Callers use this to know the cutoff must be evaluated per-candidate
    /// version rather than skipped wholesale — a name-only match is handled by
    /// [`Self::matches_name_only`] instead.
    pub(crate) fn has_versioned_match(&self, name: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.version_ranges.is_some() && r.name_matcher.matches(name))
    }
}

/// Split `<name>[@<versions>]` on the separator that isn't a scope marker,
/// so a scoped name's leading `@` isn't mistaken for a version selector.
fn split_name_and_versions(pattern: &str) -> (&str, Option<&str>) {
    let at_index = match pattern.strip_prefix('@') {
        // Scoped name: the leading `@` is the scope marker, so the version
        // separator is the next `@`, offset back past the one we stripped.
        Some(rest) => rest.find('@').map(|i| i + 1),
        None => pattern.find('@'),
    };
    match at_index {
        Some(i) => (&pattern[..i], Some(&pattern[i + 1..])),
        None => (pattern, None),
    }
}

fn parse_one(pattern: &str) -> Result<TrustExcludeRule, TrustExcludeParseError> {
    let (name_part, versions_part) = split_name_and_versions(pattern);

    let version_ranges = match versions_part {
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
                let r = node_semver::Range::parse(trimmed).map_err(|_| {
                    TrustExcludeParseError::InvalidVersionUnion {
                        pattern: pattern.to_string(),
                    }
                })?;
                parsed.push(r);
            }
            Some(parsed)
        }
    };

    Ok(TrustExcludeRule {
        name_matcher: NameMatcher::compile(name_part),
        version_ranges,
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
    fn compact_history_preserves_downgrade_detection() {
        let p = packument(
            "foo",
            vec![("3.0.0", "2025-03-01T00:00:00.000Z", version("foo", "3.0.0"))],
        );
        let history = BTreeMap::from([
            (
                "2.0.0".to_string(),
                aube_registry::VersionTrustMetadata {
                    approver: None,
                    npm_user: None,
                    dist: Some(aube_registry::VersionTrustDist {
                        attestations: Some(Attestations {
                            provenance: Some(serde_json::json!({
                                "predicateType": "https://slsa.dev/provenance/v1"
                            })),
                        }),
                    }),
                },
            ),
            (
                "3.0.0".to_string(),
                aube_registry::VersionTrustMetadata {
                    approver: None,
                    npm_user: None,
                    dist: None,
                },
            ),
        ]);
        let mut p = p;
        p.time
            .insert("2.0.0".to_string(), "2025-02-01T00:00:00.000Z".to_string());
        let picked = &p.versions["3.0.0"];

        let err = check_no_downgrade_compact(
            &p,
            "3.0.0",
            picked,
            &history,
            &TrustExcludeRules::default(),
            None,
        )
        .expect_err("compact prior provenance must still block a downgrade");
        let TrustCheckError::Downgrade(details) = err else {
            panic!("expected Downgrade");
        };
        assert_eq!(details.prior_version, "2.0.0");
        assert_eq!(details.prior_evidence, TrustEvidence::Provenance);
    }

    #[test]
    fn standalone_history_detects_downgrade_and_skips_picked_version() {
        let trust_meta =
            |dist: Option<aube_registry::VersionTrustDist>| aube_registry::VersionTrustMetadata {
                approver: None,
                npm_user: None,
                dist,
            };
        let provenance_dist = aube_registry::VersionTrustDist {
            attestations: Some(Attestations {
                provenance: Some(serde_json::json!({
                    "predicateType": "https://slsa.dev/provenance/v1"
                })),
            }),
        };
        // Unlike the compact map, the standalone history includes the
        // picked version itself — the ranking loop must skip it even
        // when it carries evidence.
        let history = aube_registry::PackumentTrustHistory {
            time: BTreeMap::from([
                ("2.0.0".to_string(), "2025-02-01T00:00:00.000Z".to_string()),
                ("3.0.0".to_string(), "2025-03-01T00:00:00.000Z".to_string()),
            ]),
            versions: BTreeMap::from([
                ("2.0.0".to_string(), trust_meta(Some(provenance_dist))),
                ("3.0.0".to_string(), trust_meta(None)),
            ]),
        };

        let picked = &history.versions["3.0.0"];
        let err = check_no_downgrade_history(
            "foo",
            &history,
            "3.0.0",
            picked,
            &TrustExcludeRules::default(),
            None,
        )
        .expect_err("prior provenance must block a downgrade");
        let TrustCheckError::Downgrade(details) = err else {
            panic!("expected Downgrade");
        };
        assert_eq!(details.name, "foo");
        assert_eq!(details.prior_version, "2.0.0");
        assert_eq!(details.prior_evidence, TrustEvidence::Provenance);
        assert_eq!(details.current_evidence, None);

        // Same history, but picking the version that carries the
        // evidence: no prior outranks it, so the check passes.
        let picked = &history.versions["2.0.0"];
        check_no_downgrade_history(
            "foo",
            &history,
            "2.0.0",
            picked,
            &TrustExcludeRules::default(),
            None,
        )
        .expect("strongest evidence so far must pass");
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

    /// A registry-shaped ISO-8601 timestamp `days` before now. Window tests
    /// must be relative to the wall-clock they run under, not fixed dates, so
    /// they stay correct at any point in time.
    fn iso_days_ago(days: u64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs();
        crate::types::format_iso8601_utc(now.saturating_sub(days * 86_400))
    }

    // nub's default `trustPolicyIgnoreAfter` (14 days, in minutes). The two
    // tests below pin the security contract of that default: aged → exempt,
    // fresh → still caught.
    const NUB_IGNORE_AFTER_MIN: u64 = 14 * 24 * 60;

    /// The 14-day window exempts an *aged* legitimate backport — the #270
    /// shape. An old-major maintenance release (`tailwind-merge@2.6.1`, no
    /// evidence) is published later in wall-clock than a newer major that
    /// adopted trusted publishing (`3.4.0`), so the date-ordered scan finds the
    /// stronger prior evidence and would fail. Aged well past the window, the
    /// pick clears the check instead.
    #[test]
    fn ignore_after_14d_exempts_aged_backport() {
        let prior_time = iso_days_ago(210);
        let picked_time = iso_days_ago(150);
        let p = packument(
            "tailwind-merge",
            vec![
                (
                    "3.4.0",
                    &prior_time,
                    with_trusted_publisher(version("tailwind-merge", "3.4.0")),
                ),
                ("2.6.1", &picked_time, version("tailwind-merge", "2.6.1")),
            ],
        );
        let picked = p.versions.get("2.6.1").unwrap();
        let result = check_no_downgrade(
            &p,
            "2.6.1",
            picked,
            &TrustExcludeRules::default(),
            Some(NUB_IGNORE_AFTER_MIN),
        );
        assert!(
            result.is_ok(),
            "a backport aged past the 14-day window must not trip the downgrade check"
        );
    }

    /// The window must NOT hide a *fresh* downgrade — the load-bearing security
    /// property. A version published inside the window with weaker evidence
    /// than a stronger earlier-published sibling is still a downgrade (the
    /// stolen-token-into-an-old-line attack shape), and must still fail. A
    /// regression here means the 14-day default opened the exact hole the
    /// analysis behind #270 warned about.
    #[test]
    fn ignore_after_14d_still_catches_fresh_downgrade() {
        let prior_time = iso_days_ago(2);
        let picked_time = iso_days_ago(1);
        let p = packument(
            "foo",
            vec![
                (
                    "2.0.0",
                    &prior_time,
                    with_trusted_publisher(version("foo", "2.0.0")),
                ),
                ("2.0.1", &picked_time, version("foo", "2.0.1")),
            ],
        );
        let picked = p.versions.get("2.0.1").unwrap();
        let err = check_no_downgrade(
            &p,
            "2.0.1",
            picked,
            &TrustExcludeRules::default(),
            Some(NUB_IGNORE_AFTER_MIN),
        )
        .expect_err("a fresh weak-evidence publish inside the window must still fail");
        match err {
            TrustCheckError::Downgrade(d) => {
                assert_eq!(d.prior_evidence, TrustEvidence::TrustedPublisher);
                assert_eq!(d.prior_version, "2.0.0");
                assert_eq!(d.current_evidence, None);
            }
            _ => panic!("expected Downgrade"),
        }
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
        // Every entry parses into exactly one rule — a malformed default
        // would otherwise silently drop protection or panic at first use.
        assert_eq!(r.len(), DEFAULT_TRUST_POLICY_EXCLUDES.len());
        for package in DEFAULT_TRUST_POLICY_EXCLUDES {
            let (name, versions) = split_name_and_versions(package);
            // Bare-name entries exempt every version. Version-scoped entries
            // deliberately do not, so they get their own targeted tests.
            if versions.is_some() {
                continue;
            }
            assert!(
                r.matches(name, &node_semver::Version::parse("1.0.0").unwrap()),
                "{name} should be globally excluded"
            );
        }
        assert!(!r.matches("left-pad", &node_semver::Version::parse("1.0.0").unwrap()));
    }

    #[test]
    fn default_excludes_scoped_octokit_endpoint_backport() {
        // Regression: @octokit backports fixes to older major lines without
        // provenance, so a legitimate older release (e.g. 9.0.6) published
        // after an attested newer major (10.1.0) tripped no-downgrade. The
        // scoped name must match every version, and the leading `@` must not
        // be misparsed as a version separator.
        let r = TrustExcludeRules::default();
        assert!(r.matches(
            "@octokit/endpoint",
            &node_semver::Version::parse("9.0.6").unwrap()
        ));
        assert!(r.matches(
            "@octokit/endpoint",
            &node_semver::Version::parse("10.1.0").unwrap()
        ));
        assert!(!r.matches(
            "@octokit/core",
            &node_semver::Version::parse("9.0.6").unwrap()
        ));
    }

    #[test]
    fn default_excludes_scoped_hono_node_server_backport() {
        // Regression: 1.19.15 was published without provenance after the
        // attested 2.0.10 release. Trusted publishing resumed with 1.19.17,
        // so the default must not disable protection for future releases.
        let r = TrustExcludeRules::default();
        assert!(r.matches(
            "@hono/node-server",
            &node_semver::Version::parse("1.19.15").unwrap()
        ));
        assert!(!r.matches(
            "@hono/node-server",
            &node_semver::Version::parse("1.19.17").unwrap()
        ));
        assert!(!r.matches(
            "@hono/node-server",
            &node_semver::Version::parse("2.0.10").unwrap()
        ));
        assert!(!r.matches("hono", &node_semver::Version::parse("1.19.15").unwrap()));
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
    fn exclude_parses_version_ranges() {
        let r = TrustExcludeRules::parse(["foo@^1.0.0 || ~2.1.0 || >=3.0.0 <4.0.0"]).unwrap();
        assert!(r.matches("foo", &node_semver::Version::parse("1.2.3").unwrap()));
        assert!(r.matches("foo", &node_semver::Version::parse("2.1.9").unwrap()));
        assert!(r.matches("foo", &node_semver::Version::parse("3.5.0").unwrap()));
        assert!(!r.matches("foo", &node_semver::Version::parse("2.2.0").unwrap()));
        assert!(!r.matches("foo", &node_semver::Version::parse("4.0.0").unwrap()));
    }

    #[test]
    fn exclude_rejects_invalid_version_ranges() {
        let err = TrustExcludeRules::parse(["foo@definitely-not-a-range"]).expect_err("bad range");
        assert!(matches!(
            err,
            TrustExcludeParseError::InvalidVersionUnion { .. }
        ));
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
            "bad@definitely-not-a-range",
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

    // ── backtracking past a refused pick ────────────────────────────

    /// No exemptions and no ignore-window, so every candidate faces the full
    /// chronological scan. The empty policy matters: `TrustExcludeRules::default`
    /// seeds 40-odd real package names.
    fn repick<'a>(
        packument: &'a Packument,
        range: &str,
        rejected: &str,
        cutoff: Option<&str>,
    ) -> Option<&'a VersionMetadata> {
        repick_past_downgrade(
            packument,
            &packument.name,
            range,
            rejected,
            false,
            cutoff,
            None,
            true,
            &TrustExcludeRules::empty(),
            None,
            &std::collections::BTreeMap::new(),
            |_, _| false,
        )
    }

    /// The `fastq` shape: two attested releases, one hand-published on top of
    /// them, and the range's head refused. The install must land on the newest
    /// release that still carries its evidence rather than abort.
    #[test]
    fn repick_walks_down_to_the_newest_still_trusted_version() {
        let p = packument(
            "fastq",
            vec![
                (
                    "1.20.0",
                    "2025-12-23T07:49:07.472Z",
                    with_trusted_publisher(version("fastq", "1.20.0")),
                ),
                (
                    "1.20.1",
                    "2025-12-23T07:58:58.958Z",
                    with_trusted_publisher(version("fastq", "1.20.1")),
                ),
                (
                    "1.20.2",
                    "2026-08-28T00:52:38.498Z",
                    version("fastq", "1.20.2"),
                ),
            ],
        );
        assert!(
            check_no_downgrade(
                &p,
                "1.20.2",
                p.versions.get("1.20.2").unwrap(),
                &TrustExcludeRules::empty(),
                None
            )
            .is_err()
        );
        let picked = repick(&p, "^1.20.0", "1.20.2", None).expect("1.20.1 satisfies and is signed");
        assert_eq!(picked.version, "1.20.1");
    }

    /// An unversioned request — `dlx`, a bare `add` — arrives as the literal
    /// range `latest`, which is not semver. It widens to everything at or below
    /// the tag, exactly as an age-gated `latest` does. A channel tag names one
    /// version and stays un-widened.
    #[test]
    fn repick_widens_a_refused_latest_but_not_another_tag() {
        let mut p = packument(
            "foo",
            vec![
                (
                    "1.1.0",
                    "2025-02-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "1.1.0")),
                ),
                ("1.2.0", "2025-03-01T00:00:00.000Z", version("foo", "1.2.0")),
            ],
        );
        p.dist_tags
            .insert("latest".to_string(), "1.2.0".to_string());
        p.dist_tags.insert("next".to_string(), "1.2.0".to_string());
        let picked = repick(&p, "latest", "1.2.0", None).expect("1.1.0 is under the tag");
        assert_eq!(picked.version, "1.1.0");
        assert!(repick(&p, "next", "1.2.0", None).is_none());
    }

    /// The gate is not weakened: when the range admits nothing that clears the
    /// trust check, the caller still gets its refusal. `>=1.1.0` cannot reach
    /// the attested 1.0.0 below it.
    #[test]
    fn repick_gives_up_when_no_satisfying_version_keeps_its_evidence() {
        let p = packument(
            "foo",
            vec![
                (
                    "1.0.0",
                    "2025-01-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "1.0.0")),
                ),
                ("1.1.0", "2025-02-01T00:00:00.000Z", version("foo", "1.1.0")),
                ("1.2.0", "2025-03-01T00:00:00.000Z", version("foo", "1.2.0")),
            ],
        );
        assert!(repick(&p, ">=1.1.0", "1.2.0", None).is_none());
    }

    /// Bounded at the refused version. 1.3.0 satisfies the range and is signed,
    /// but it sits ABOVE the pick — reaching it would hand back a release the
    /// publisher has already moved `dist-tags.latest` off.
    #[test]
    fn repick_never_climbs_above_the_refused_version() {
        let mut p = packument(
            "foo",
            vec![
                (
                    "1.0.0",
                    "2025-01-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "1.0.0")),
                ),
                (
                    "1.1.0",
                    "2025-02-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "1.1.0")),
                ),
                ("1.2.0", "2025-03-01T00:00:00.000Z", version("foo", "1.2.0")),
                (
                    "1.3.0",
                    "2025-04-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "1.3.0")),
                ),
            ],
        );
        p.dist_tags
            .insert("latest".to_string(), "1.2.0".to_string());
        let picked = repick(&p, "^1.0.0", "1.2.0", None).expect("1.1.0 is reachable");
        assert_eq!(picked.version, "1.1.0");

        // `resolution-mode=time-based` takes the FLOOR of the range, so its
        // backtrack runs the other way and 1.3.0 is what comes into reach. The
        // bound is "away from the refusal", not "downward".
        let picked = repick_past_downgrade(
            &p,
            &p.name,
            "^1.0.0",
            "1.2.0",
            true,
            None,
            None,
            true,
            &TrustExcludeRules::empty(),
            None,
            &std::collections::BTreeMap::new(),
            |_, _| false,
        )
        .expect("1.3.0 is reachable upward");
        assert_eq!(picked.version, "1.3.0");
    }

    /// The age gate still binds inside the backtrack: a signed release that is
    /// too new is no more installable here than it was at the original pick.
    #[test]
    fn repick_still_honors_the_age_cutoff() {
        let p = packument(
            "foo",
            vec![
                (
                    "1.0.0",
                    "2025-01-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "1.0.0")),
                ),
                (
                    "1.1.0",
                    "2026-08-28T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "1.1.0")),
                ),
                ("1.2.0", "2026-08-29T00:00:00.000Z", version("foo", "1.2.0")),
            ],
        );
        let picked = repick(&p, "^1.0.0", "1.2.0", Some("2026-01-01T00:00:00.000Z"))
            .expect("1.0.0 clears both gates");
        assert_eq!(picked.version, "1.0.0");
    }
}
