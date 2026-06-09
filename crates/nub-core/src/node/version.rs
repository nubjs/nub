//! Node.js version parsing and comparison, backed by the `semver` crate.

use std::fmt;
use std::str::FromStr;

/// A parsed Node.js version. Wraps `semver::Version` for correct ordering,
/// display, and parsing (handles `v` prefix, prerelease, build metadata).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeVersion(pub semver::Version);

impl NodeVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self(semver::Version::new(
            major as u64,
            minor as u64,
            patch as u64,
        ))
    }

    pub fn major(&self) -> u64 {
        self.0.major
    }
    pub fn minor(&self) -> u64 {
        self.0.minor
    }
    pub fn patch(&self) -> u64 {
        self.0.patch
    }

    /// The minimum Node version Nub supports at all. Below this, Nub
    /// hard-errors before spawning — the user must upgrade Node or run
    /// plain `node` directly. See
    /// `wiki/research/supported-node-versions.md` for the rationale
    /// (no hook API exists below 18.19 that can carry Nub's feature
    /// surface).
    pub const MIN_SUPPORTED: Self = Self::new(18, 19, 0);

    /// The minimum Node version for Nub's fast-path augmented mode
    /// (sync `module.registerHooks`). Versions in
    /// `MIN_SUPPORTED..MIN_AUGMENTED` run in compatibility mode
    /// (async `module.register()`); the JS preload picks the
    /// registration shape based on `process.versions.node`.
    pub const MIN_AUGMENTED: Self = Self::new(22, 15, 0);

    /// True if this Node version is at or above the hard floor.
    pub fn is_supported(&self) -> bool {
        *self >= Self::MIN_SUPPORTED
    }

    pub fn supports_augmentation(&self) -> bool {
        *self >= Self::MIN_AUGMENTED
    }

    /// Classify the Node version into one of the three support tiers.
    ///
    /// - `FastPath` (>= 22.15.0): sync `module.registerHooks()` in-thread.
    /// - `Compat`   (18.19.0 ..= 22.14.x): async `module.register()` loader worker.
    /// - `Unsupported` (< 18.19.0): no hook API capable of carrying the
    ///   Nub feature surface; the spawn path refuses.
    ///
    /// Source of truth for the support model: `wiki/research/supported-node-versions.md`.
    pub fn tier(&self) -> SupportTier {
        if *self >= Self::MIN_AUGMENTED {
            SupportTier::FastPath
        } else if *self >= Self::MIN_SUPPORTED {
            SupportTier::Compat
        } else {
            SupportTier::Unsupported
        }
    }

    pub fn satisfies(&self, pin: &VersionPin) -> bool {
        match pin {
            VersionPin::Exact(v) => {
                self.0.major == v.0.major && self.0.minor == v.0.minor && self.0.patch == v.0.patch
            }
            VersionPin::MajorMinor(major, minor) => {
                self.0.major == *major as u64 && self.0.minor == *minor as u64
            }
            VersionPin::Major(major) => self.0.major == *major as u64,
            // node-semver `||` semantics: the version satisfies the range when it
            // matches ANY alternative.
            VersionPin::Range(alternatives) => alternatives.iter().any(|req| req.matches(&self.0)),
            // An alias (`latest`/`lts`/`lts/<codename>`/`rc/<major>`) can't be
            // satisfied by inspecting a concrete version — it must first resolve
            // to a concrete version against the dist index. Callers that handle
            // aliases do that resolution; the plain satisfies-check says no.
            VersionPin::Alias(_) => false,
        }
    }
}

/// The three Node-version support tiers. See [`NodeVersion::tier`].
///
/// Variants are exhaustive by design — adding a fourth tier should be
/// a deliberate change reviewed against the support-versions design
/// doc, not a quiet addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SupportTier {
    /// Node >= 22.15.0. Sync `module.registerHooks()` is available;
    /// hooks run in-thread with no IPC overhead.
    FastPath,
    /// Node 18.19.0 through 22.14.x. Only async `module.register()`
    /// is available; hooks run in a loader worker thread. Carries
    /// the non-silenceable compat-mode notice.
    Compat,
    /// Node < 18.19.0. No usable hook API; Nub refuses to spawn.
    Unsupported,
}

impl fmt::Display for NodeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0.major, self.0.minor, self.0.patch)
    }
}

impl FromStr for NodeVersion {
    type Err = VersionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
        let v = semver::Version::parse(s).map_err(|_| VersionParseError(s.to_string()))?;
        Ok(Self(v))
    }
}

/// A version pin from `.nvmrc` / `.node-version` / `devEngines.runtime`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionPin {
    Exact(NodeVersion),
    MajorMinor(u32, u32),
    Major(u32),
    /// A dist-tag alias resolved against nodejs.org's `index.json`: `latest` /
    /// `node`, `lts` / `lts/*`, `lts/<codename>`, or `rc/<major>`. Stored
    /// lowercased; resolution lives in `version_management::node_index`.
    Alias(String),
    /// A semver range (`>=20 <23`, `^20.11`, `~22.1.0`, `20.x`,
    /// `^18 || >=20`) from `package.json#devEngines.runtime` /
    /// `package.json#engines.node` — the `engines.node` grammar. Never produced
    /// by `FromStr` (pin files keep the nvm grammar); constructed via
    /// [`VersionPin::parse_allowing_ranges`]. node-semver `||` alternatives are
    /// modeled as a non-empty list of `VersionReq`s with OR semantics (a version
    /// satisfies the pin when it matches ANY of them). Resolves to the newest
    /// published version satisfying it
    /// (`version_management::node_index::resolve_range`).
    Range(Vec<semver::VersionReq>),
}

impl VersionPin {
    /// Parse a `devEngines.runtime.version` / `engines.node` value: everything
    /// `FromStr` accepts (exact / major / major.minor / alias) plus semver
    /// ranges per the field's spec ("follows the `engines.node` format").
    /// Handled grammar beyond `FromStr`: space-separated AND-comparators
    /// (`>=20 <23`), operator-space form (`>= 20`), hyphen ranges (`20 - 22`),
    /// and `||` alternatives (`^18 || >=20`) — each alternative bridged to the
    /// comma form Cargo's `semver` crate wants. Every alternative must parse, or
    /// the whole spec errors (the caller warns and falls through to the next pin
    /// source — never silently half-honors a range).
    pub fn parse_allowing_ranges(s: &str) -> Result<Self, VersionParseError> {
        if let Ok(pin) = s.parse::<VersionPin>() {
            return Ok(pin);
        }
        let raw = s.trim();
        let alternatives: Result<Vec<semver::VersionReq>, _> = raw
            .split("||")
            .map(|alt| semver::VersionReq::parse(&normalize_node_range(alt.trim())))
            .collect();
        match alternatives {
            Ok(alts) if !alts.is_empty() => Ok(Self::Range(alts)),
            _ => Err(VersionParseError(raw.to_string())),
        }
    }
}

/// Bridge one node-semver alternative (no `||` — the caller splits those) to the
/// grammar Cargo's `semver` crate parses:
///   - hyphen ranges: `20.0.0 - 22.1.0` → `>=20.0.0, <=22.1.0`;
///   - space-separated AND-comparators: `>=20 <23` → `>=20, <23`;
///   - operator-space form (legal node-semver): `>= 20` → `>=20`.
///
/// Single comparators, bare versions, and already-comma'd specs pass through
/// untouched. A dangling trailing operator is kept verbatim so the parse fails
/// (better an honest error than a silently dropped comparator).
fn normalize_node_range(spec: &str) -> String {
    if spec.contains(',') {
        return spec.to_string();
    }
    if let Some((lo, hi)) = spec.split_once(" - ") {
        return format!(">={}, <={}", lo.trim(), hi.trim());
    }
    let mut comparators: Vec<String> = Vec::new();
    let mut pending_op: Option<&str> = None;
    for token in spec.split_whitespace() {
        if matches!(token, ">" | "<" | ">=" | "<=" | "=" | "^" | "~") {
            pending_op = Some(token);
        } else if let Some(op) = pending_op.take() {
            comparators.push(format!("{op}{token}"));
        } else {
            comparators.push(token.to_string());
        }
    }
    if let Some(op) = pending_op {
        comparators.push(op.to_string());
    }
    comparators.join(", ")
}

impl FromStr for VersionPin {
    type Err = VersionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let raw = s.trim();
        let lower = raw.to_ascii_lowercase();

        // Aliases are resolved later against the dist index (full support per
        // wiki/runtime/node-version-management.md §"Aliases") — keep them as a pin
        // rather than rejecting, so `.nvmrc`/`.node-version` aliases aren't ignored.
        if lower == "latest"
            || lower == "node"
            || lower == "lts"
            || lower.starts_with("lts/")
            || lower.starts_with("rc/")
        {
            return Ok(Self::Alias(lower));
        }

        let s = raw.strip_prefix('v').unwrap_or(raw);

        let parts: Vec<&str> = s.split('.').collect();
        match parts.len() {
            1 => {
                let major = parts[0]
                    .parse()
                    .map_err(|_| VersionParseError(s.to_string()))?;
                Ok(Self::Major(major))
            }
            2 => {
                let major = parts[0]
                    .parse()
                    .map_err(|_| VersionParseError(s.to_string()))?;
                let minor = parts[1]
                    .parse()
                    .map_err(|_| VersionParseError(s.to_string()))?;
                Ok(Self::MajorMinor(major, minor))
            }
            3 => {
                let v = NodeVersion::from_str(s)?;
                Ok(Self::Exact(v))
            }
            _ => Err(VersionParseError(s.to_string())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VersionParseError(pub String);

impl fmt::Display for VersionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid Node version: '{}'", self.0)
    }
}

impl std::error::Error for VersionParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_version() {
        let v: NodeVersion = "v22.15.0".parse().unwrap();
        assert_eq!(v, NodeVersion::new(22, 15, 0));
    }

    #[test]
    fn parse_without_v_prefix() {
        let v: NodeVersion = "24.3.1".parse().unwrap();
        assert_eq!(v, NodeVersion::new(24, 3, 1));
    }

    #[test]
    fn version_ordering() {
        assert!(NodeVersion::new(22, 15, 0) < NodeVersion::new(23, 0, 0));
        assert!(NodeVersion::new(22, 14, 0) < NodeVersion::new(22, 15, 0));
        assert!(NodeVersion::new(22, 15, 0) < NodeVersion::new(22, 15, 1));
    }

    #[test]
    fn supports_augmentation() {
        assert!(NodeVersion::new(22, 15, 0).supports_augmentation());
        assert!(NodeVersion::new(23, 0, 0).supports_augmentation());
        assert!(!NodeVersion::new(22, 14, 0).supports_augmentation());
        assert!(!NodeVersion::new(20, 0, 0).supports_augmentation());
    }

    #[test]
    fn is_supported_floor() {
        // 18.19.0 is the floor; anything below is hard-errored.
        assert!(NodeVersion::new(18, 19, 0).is_supported());
        assert!(NodeVersion::new(18, 19, 1).is_supported());
        assert!(NodeVersion::new(22, 14, 0).is_supported());
        assert!(NodeVersion::new(22, 15, 0).is_supported());
        assert!(NodeVersion::new(24, 0, 0).is_supported());
        assert!(!NodeVersion::new(18, 18, 99).is_supported());
        assert!(!NodeVersion::new(16, 10, 0).is_supported());
    }

    #[test]
    fn pin_exact() {
        let pin: VersionPin = "22.15.0".parse().unwrap();
        assert!(NodeVersion::new(22, 15, 0).satisfies(&pin));
        assert!(!NodeVersion::new(22, 15, 1).satisfies(&pin));
    }

    #[test]
    fn pin_major_minor() {
        let pin: VersionPin = "22.15".parse().unwrap();
        assert!(NodeVersion::new(22, 15, 0).satisfies(&pin));
        assert!(NodeVersion::new(22, 15, 3).satisfies(&pin));
        assert!(!NodeVersion::new(22, 16, 0).satisfies(&pin));
    }

    #[test]
    fn pin_major_only() {
        let pin: VersionPin = "22".parse().unwrap();
        assert!(NodeVersion::new(22, 0, 0).satisfies(&pin));
        assert!(NodeVersion::new(22, 99, 0).satisfies(&pin));
        assert!(!NodeVersion::new(23, 0, 0).satisfies(&pin));
    }

    #[test]
    fn display_version() {
        assert_eq!(NodeVersion::new(22, 15, 0).to_string(), "22.15.0");
    }

    #[test]
    fn aliases_parse_to_the_alias_variant() {
        // Aliases are now first-class pins (resolved later against the dist
        // index), not rejected. Stored lowercased.
        let parse = |s: &str| s.parse::<VersionPin>().unwrap();
        assert_eq!(parse("lts/iron"), VersionPin::Alias("lts/iron".into()));
        assert_eq!(parse("lts/Iron"), VersionPin::Alias("lts/iron".into()));
        assert_eq!(parse("lts"), VersionPin::Alias("lts".into()));
        assert_eq!(parse("latest"), VersionPin::Alias("latest".into()));
        assert_eq!(parse("node"), VersionPin::Alias("node".into()));
        assert_eq!(parse("rc/22"), VersionPin::Alias("rc/22".into()));
        // Numeric pins still parse numerically.
        assert_eq!(parse("22"), VersionPin::Major(22));
        assert_eq!(parse("22.13"), VersionPin::MajorMinor(22, 13));
        assert!(matches!(parse("22.13.0"), VersionPin::Exact(_)));
    }

    #[test]
    fn parse_allowing_ranges_covers_the_engines_grammar() {
        let p = |s: &str| VersionPin::parse_allowing_ranges(s).unwrap();
        // Exact / major shorthand still route to the concrete variants.
        assert_eq!(p("22.13.0"), VersionPin::Exact(NodeVersion::new(22, 13, 0)));
        assert_eq!(p("22"), VersionPin::Major(22));
        // node-semver space-separated comparators bridge to a working Range.
        let range = p(">=20 <23");
        assert!(matches!(range, VersionPin::Range(_)), "got {range:?}");
        assert!(NodeVersion::new(22, 14, 0).satisfies(&range));
        assert!(!NodeVersion::new(23, 0, 0).satisfies(&range));
        assert!(!NodeVersion::new(18, 19, 0).satisfies(&range));
        // Caret and x-wildcard forms (common devEngines spellings).
        let caret = p("^20.11");
        assert!(NodeVersion::new(20, 18, 1).satisfies(&caret));
        assert!(!NodeVersion::new(21, 0, 0).satisfies(&caret));
        let wild = p("20.x");
        assert!(NodeVersion::new(20, 18, 1).satisfies(&wild));
        assert!(!NodeVersion::new(22, 0, 0).satisfies(&wild));
        // Garbage errors — the caller falls through to the next pin source.
        assert!(VersionPin::parse_allowing_ranges("not-a-version").is_err());
    }

    #[test]
    fn parse_allowing_ranges_handles_operator_space_or_alternatives_and_hyphen() {
        let p = |s: &str| VersionPin::parse_allowing_ranges(s).unwrap();
        // Operator-space form is legal node-semver (`>= 20`), not a parse failure.
        let spaced = p(">= 20");
        assert!(NodeVersion::new(22, 0, 0).satisfies(&spaced));
        assert!(!NodeVersion::new(18, 19, 0).satisfies(&spaced));
        // `||` alternatives: satisfied by ANY side, by neither gap version.
        let or = p("^18.19 || >=22");
        assert!(NodeVersion::new(18, 20, 0).satisfies(&or));
        assert!(NodeVersion::new(23, 1, 0).satisfies(&or));
        assert!(
            !NodeVersion::new(20, 11, 0).satisfies(&or),
            "20.x falls in the gap between ^18.19 and >=22"
        );
        // Hyphen ranges are inclusive on both ends.
        let hyphen = p("20.0.0 - 22.13.0");
        assert!(NodeVersion::new(20, 0, 0).satisfies(&hyphen));
        assert!(NodeVersion::new(22, 13, 0).satisfies(&hyphen));
        assert!(!NodeVersion::new(22, 14, 0).satisfies(&hyphen));
        // One bad alternative poisons the whole spec — never half-honor a range.
        assert!(VersionPin::parse_allowing_ranges(">=20 || nonsense").is_err());
        assert!(VersionPin::parse_allowing_ranges(">=").is_err());
    }

    #[test]
    fn prerelease_version_parses() {
        let v: NodeVersion = "23.0.0-rc.1".parse().unwrap();
        assert_eq!(v.major(), 23);
    }

    // ── Tier boundary cases ────────────────────────────────────────────
    // Each test pins one boundary of the three-tier model so a future
    // edit that moves the floor or fast-path line trips a clear failure.

    #[test]
    fn tier_18_18_is_unsupported() {
        // One patch below the hard floor (18.19.0) — must refuse.
        assert_eq!(
            NodeVersion::new(18, 18, 99).tier(),
            SupportTier::Unsupported
        );
        assert_eq!(NodeVersion::new(18, 18, 0).tier(), SupportTier::Unsupported);
    }

    #[test]
    fn tier_18_19_0_is_compat() {
        // Exact floor — the first version that runs at all.
        assert_eq!(NodeVersion::new(18, 19, 0).tier(), SupportTier::Compat);
    }

    #[test]
    fn tier_22_14_99_is_compat() {
        // One patch below the fast-path floor — still compat tier.
        assert_eq!(NodeVersion::new(22, 14, 99).tier(), SupportTier::Compat);
    }

    #[test]
    fn tier_22_15_0_is_fast_path() {
        // Exact fast-path floor — sync `registerHooks` becomes available.
        assert_eq!(NodeVersion::new(22, 15, 0).tier(), SupportTier::FastPath);
    }

    #[test]
    fn tier_24_x_is_fast_path() {
        // Modern Node — default user experience.
        assert_eq!(NodeVersion::new(24, 0, 0).tier(), SupportTier::FastPath);
        assert_eq!(NodeVersion::new(24, 14, 0).tier(), SupportTier::FastPath);
    }

    #[test]
    fn is_supported_false_for_18_18_x() {
        assert!(!NodeVersion::new(18, 18, 0).is_supported());
        assert!(!NodeVersion::new(18, 18, 99).is_supported());
    }

    #[test]
    fn is_supported_true_for_18_19_and_above() {
        assert!(NodeVersion::new(18, 19, 0).is_supported());
        assert!(NodeVersion::new(22, 14, 0).is_supported());
        assert!(NodeVersion::new(22, 15, 0).is_supported());
        assert!(NodeVersion::new(24, 14, 0).is_supported());
    }
}
