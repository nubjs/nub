//! Allowlist/denylist policy for running dependency lifecycle scripts.
//!
//! Mirrors pnpm's `createAllowBuildFunction` — given an `allowBuilds`
//! map (`Record<string, boolean>`) and a `dangerouslyAllowAllBuilds`
//! flag, produce a function from `(pkgName, version)` to an allow /
//! deny / unspecified decision. Unspecified means "fall through to the
//! caller's default," which for aube is always "deny."
//!
//! ## Entry shapes
//!
//! Keys in the `allowBuilds` map support three forms:
//!
//! - `"esbuild"` — bare name, matches every version of the package
//! - `"esbuild@0.19.0"` — exact version match
//! - `"esbuild@0.19.0 || 0.20.0"` — exact version union
//!
//! Semver ranges are intentionally *not* supported, matching pnpm's
//! `expandPackageVersionSpecs` behavior: if you pin a version in the
//! allowlist you're asserting a specific build has been audited, so
//! range matching would defeat the point.
//!
//! Name patterns may also contain `*` wildcards, mirroring pnpm's
//! `@pnpm/config.matcher`. `@babel/*` matches every package under the
//! `@babel` scope, `*-loader` matches any name ending in `-loader`,
//! and a bare `*` matches every package. `*` is the only supported
//! metacharacter and always matches a possibly-empty run of any
//! characters. Wildcards must stand alone — combining them with a
//! version spec (`@babel/*@1.0.0`) is rejected, since a wildcard
//! name can't be used to assert "this exact build was audited."

use aube_manifest::AllowBuildRaw;
use std::collections::{BTreeMap, HashSet};

/// The decision for a single `(name, version)` lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowDecision {
    /// Package is explicitly allowed — run its lifecycle scripts.
    Allow,
    /// Package is explicitly denied — skip even if a broader rule would allow.
    Deny,
    /// No rule matched; caller applies its default (aube denies).
    Unspecified,
}

/// Resolved policy for deciding whether a package may run its
/// lifecycle scripts.
#[derive(Debug, Clone, Default)]
pub struct BuildPolicy {
    allow_all: bool,
    /// Expanded allow-keys: bare names (match any version) and
    /// `name@version` strings (match that specific version).
    allowed: HashSet<String>,
    denied: HashSet<String>,
    /// Exact source-backed lockfile IDs such as `pkg@file+abc123`.
    /// These are intentionally separate from normal `name@version`
    /// entries so a typo like `pkg@latest` still warns.
    allowed_sources: HashSet<String>,
    denied_sources: HashSet<String>,
    /// Bare-name patterns containing `*` wildcards. Checked with a
    /// linear scan after the exact-match sets; wildcard rules are rare
    /// enough that the linear pass is cheaper than building an
    /// automaton.
    allowed_wildcards: Vec<String>,
    denied_wildcards: Vec<String>,
}

impl BuildPolicy {
    /// A policy that denies every package (the aube default).
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// A policy that allows every package, regardless of the map.
    /// Corresponds to `--dangerously-allow-all-builds`.
    pub fn allow_all() -> Self {
        Self {
            allow_all: true,
            ..Self::default()
        }
    }

    /// Build from a raw `allowBuilds` map plus pnpm's canonical
    /// `onlyBuiltDependencies` / `neverBuiltDependencies` flat lists,
    /// plus the `dangerouslyAllowAllBuilds` flag.
    ///
    /// All three sources merge into one allow/deny set — pnpm uses the
    /// flat lists in most real-world projects, and aube's `allowBuilds`
    /// map is the superset format. Unrecognized `allowBuilds` value
    /// shapes are collected in the returned `warnings` vec so the
    /// caller can surface them through the progress UI.
    pub fn from_config(
        allow_builds: &BTreeMap<String, AllowBuildRaw>,
        only_built: &[String],
        never_built: &[String],
        dangerously_allow_all: bool,
    ) -> (Self, Vec<BuildPolicyError>) {
        if dangerously_allow_all {
            return (Self::allow_all(), Vec::new());
        }
        let mut allowed = HashSet::new();
        let mut denied = HashSet::new();
        let mut allowed_sources = HashSet::new();
        let mut denied_sources = HashSet::new();
        let mut allowed_wildcards = Vec::new();
        let mut denied_wildcards = Vec::new();
        let mut warnings = Vec::new();

        for (pattern, value) in allow_builds {
            let bool_value = match value {
                AllowBuildRaw::Bool(b) => *b,
                AllowBuildRaw::Other(raw) => {
                    // The canonical "set this to true or false" placeholder
                    // is what pnpm auto-seeds for unreviewed builds. Aube
                    // doesn't write it (we leave the manifest alone), but
                    // pnpm-managed projects can still carry these strings.
                    // Treat as Unspecified (skip silently); strict-dep-builds
                    // surfaces the dep via `unreviewed_dep_builds` instead.
                    // Any other string is a user-authored value we don't
                    // understand; warn so it isn't silently misread.
                    if raw == aube_manifest::workspace::ALLOW_BUILDS_REVIEW_PLACEHOLDER {
                        continue;
                    }
                    warnings.push(BuildPolicyError::UnsupportedValue {
                        pattern: pattern.clone(),
                        raw: raw.clone(),
                    });
                    continue;
                }
            };
            match expand_spec(pattern) {
                Ok(expanded) => {
                    let (exact, wild) = if bool_value {
                        (&mut allowed, &mut allowed_wildcards)
                    } else {
                        (&mut denied, &mut denied_wildcards)
                    };
                    let source = if bool_value {
                        &mut allowed_sources
                    } else {
                        &mut denied_sources
                    };
                    sort_entries(expanded, exact, source, wild);
                }
                Err(e) => warnings.push(e),
            }
        }

        // `onlyBuiltDependencies` / `neverBuiltDependencies` support the
        // same pattern forms as `allowBuilds` map keys (bare name, exact
        // version, exact version union), so route them through the same
        // `expand_spec` — a single `esbuild@0.20.0` pin works in either
        // format.
        for pattern in only_built {
            match expand_spec(pattern) {
                Ok(expanded) => sort_entries(
                    expanded,
                    &mut allowed,
                    &mut allowed_sources,
                    &mut allowed_wildcards,
                ),
                Err(e) => warnings.push(e),
            }
        }
        for pattern in never_built {
            match expand_spec(pattern) {
                Ok(expanded) => sort_entries(
                    expanded,
                    &mut denied,
                    &mut denied_sources,
                    &mut denied_wildcards,
                ),
                Err(e) => warnings.push(e),
            }
        }

        (
            Self {
                allow_all: false,
                allowed,
                denied,
                allowed_sources,
                denied_sources,
                allowed_wildcards,
                denied_wildcards,
            },
            warnings,
        )
    }

    /// Switch this policy into allow-all mode while keeping its
    /// explicit deny entries — the same shape [`Self::denylist`]
    /// builds. `decide` checks denies before the allow-all
    /// short-circuit, so `false` entries keep winning.
    ///
    /// This is the `dangerouslyAllowAllBuilds` composition for callers
    /// whose documented precedence puts explicit per-package entries
    /// above the allow-all posture (e.g. the `defaultTrust` floor's
    /// chain: explicit entry > allow-all > floor > default deny).
    /// [`Self::from_config`]'s allow-all short-circuit — which drops
    /// `false` entries to mirror pnpm's `createAllowBuildFunction` —
    /// stays the default behavior.
    pub fn allow_all_except_denied(mut self) -> Self {
        self.allow_all = true;
        self
    }

    /// Build an allow-all policy with explicit package-pattern denies.
    pub fn denylist(denied_patterns: &[String]) -> (Self, Vec<BuildPolicyError>) {
        let mut denied = HashSet::new();
        let mut denied_sources = HashSet::new();
        let mut denied_wildcards = Vec::new();
        let mut warnings = Vec::new();
        for pattern in denied_patterns {
            match expand_spec(pattern) {
                Ok(expanded) => sort_entries(
                    expanded,
                    &mut denied,
                    &mut denied_sources,
                    &mut denied_wildcards,
                ),
                Err(e) => warnings.push(e),
            }
        }
        (
            Self {
                allow_all: true,
                allowed: HashSet::new(),
                denied,
                allowed_sources: HashSet::new(),
                denied_sources,
                allowed_wildcards: Vec::new(),
                denied_wildcards,
            },
            warnings,
        )
    }

    /// Decide whether `(name, version)` may run lifecycle scripts.
    /// Explicit denies always win over allows (mirrors pnpm).
    pub fn decide(&self, name: &str, version: &str) -> AllowDecision {
        self.decide_package(name, version, None)
    }

    /// Decide whether a package may run lifecycle scripts, with an
    /// optional source identity for non-registry packages.
    ///
    /// Bare package-name approvals only apply to registry packages.
    /// Source-backed packages (`file:`, `git:`, direct tarball, etc.)
    /// require an exact source key such as `pkg@file+abc123`; otherwise
    /// a trusted package name could approve arbitrary untrusted bytes.
    pub fn decide_package(
        &self,
        name: &str,
        version: &str,
        source_key: Option<&str>,
    ) -> AllowDecision {
        // Reusable thread-local buffer for the `name@version` probe key.
        // Avoids a `format!` allocation on every call — ~2k throwaway
        // Strings on a typical install otherwise.
        thread_local! {
            static KEY_BUF: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
        }
        if self.denied.contains(name) {
            return AllowDecision::Deny;
        }
        if matches_any_wildcard(name, &self.denied_wildcards) {
            return AllowDecision::Deny;
        }
        if let Some(source_key) = source_key
            && self.denied_sources.contains(source_key)
        {
            return AllowDecision::Deny;
        }
        // Build the `name@version` probe key once and answer both the
        // deny and the allow lookups from a single buffer borrow.
        let (denied_versioned, allowed_versioned) = KEY_BUF.with(|buf| {
            let mut b = buf.borrow_mut();
            b.clear();
            use std::fmt::Write as _;
            let _ = write!(b, "{name}@{version}");
            let key = b.as_str();
            (self.denied.contains(key), self.allowed.contains(key))
        });
        if denied_versioned {
            return AllowDecision::Deny;
        }
        if self.allow_all {
            return AllowDecision::Allow;
        }
        if let Some(source_key) = source_key {
            return if self.allowed_sources.contains(source_key) {
                AllowDecision::Allow
            } else {
                AllowDecision::Unspecified
            };
        }
        if self.allowed.contains(name) || allowed_versioned {
            return AllowDecision::Allow;
        }
        if matches_any_wildcard(name, &self.allowed_wildcards) {
            return AllowDecision::Allow;
        }
        AllowDecision::Unspecified
    }

    /// True when the policy would allow something — any rule at all, or
    /// allow-all mode. Lets callers cheaply skip the whole dep-script
    /// phase when nothing could possibly run.
    pub fn has_any_allow_rule(&self) -> bool {
        self.allow_all
            || !self.allowed.is_empty()
            || !self.allowed_sources.is_empty()
            || !self.allowed_wildcards.is_empty()
    }

    /// Stable fingerprint of the resolved policy shape. Used by the
    /// install-state delta path: if approvals/denials changed, the
    /// next install must fall back to the full eligible build scan so
    /// newly approved unchanged packages still get a chance to run.
    pub fn fingerprint(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"build-policy-v1");
        hasher.update(if self.allow_all { b"\x01" } else { b"\x00" });
        hash_set(&mut hasher, b"allowed", &self.allowed);
        hash_set(&mut hasher, b"denied", &self.denied);
        hash_set(&mut hasher, b"allowed_sources", &self.allowed_sources);
        hash_set(&mut hasher, b"denied_sources", &self.denied_sources);
        hash_vec(&mut hasher, b"allowed_wildcards", &self.allowed_wildcards);
        hash_vec(&mut hasher, b"denied_wildcards", &self.denied_wildcards);
        hasher.finalize().to_hex().to_string()
    }

    /// Merge another resolved policy into this one. Denies from either
    /// policy still win at decision time.
    pub fn merge(&mut self, other: &Self) {
        self.allow_all |= other.allow_all;
        self.allowed.extend(other.allowed.iter().cloned());
        self.denied.extend(other.denied.iter().cloned());
        self.allowed_sources
            .extend(other.allowed_sources.iter().cloned());
        self.denied_sources
            .extend(other.denied_sources.iter().cloned());
        merge_unique(&mut self.allowed_wildcards, &other.allowed_wildcards);
        merge_unique(&mut self.denied_wildcards, &other.denied_wildcards);
    }
}

fn merge_unique(target: &mut Vec<String>, source: &[String]) {
    for value in source {
        if !target.iter().any(|existing| existing == value) {
            target.push(value.clone());
        }
    }
}

fn hash_set(hasher: &mut blake3::Hasher, tag: &[u8], set: &HashSet<String>) {
    let mut values: Vec<&String> = set.iter().collect();
    values.sort();
    hash_iter(hasher, tag, values.into_iter().map(String::as_str));
}

fn hash_vec(hasher: &mut blake3::Hasher, tag: &[u8], values: &[String]) {
    let mut values: Vec<&str> = values.iter().map(String::as_str).collect();
    values.sort();
    hash_iter(hasher, tag, values.into_iter());
}

fn hash_iter<'a>(hasher: &mut blake3::Hasher, tag: &[u8], values: impl Iterator<Item = &'a str>) {
    let values: Vec<&str> = values.collect();
    hasher.update(tag);
    hasher.update(&(values.len() as u64).to_le_bytes());
    for value in values {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
}

/// True when a package-pattern entry matches `(name, version)`.
pub fn pattern_matches(pattern: &str, name: &str, version: &str) -> Result<bool, BuildPolicyError> {
    let with_version = format!("{name}@{version}");
    for expanded in expand_spec(pattern)? {
        if expanded.contains('*') {
            if matches_wildcard(name, &expanded) {
                return Ok(true);
            }
        } else if expanded == name || expanded == with_version {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Split one entry list from `expand_spec` across the exact-match set
/// and the wildcard list. Wildcards are identified by a literal `*` in
/// the string; since `expand_spec` rejects `wildcard@version`, a `*`
/// can only appear in a bare name.
fn sort_entries(
    entries: Vec<String>,
    exact: &mut HashSet<String>,
    sources: &mut HashSet<String>,
    wildcards: &mut Vec<String>,
) {
    for entry in entries {
        if entry.contains('*') {
            if !wildcards.iter().any(|p| p == &entry) {
                wildcards.push(entry);
            }
        } else if is_source_key(&entry) {
            sources.insert(entry);
        } else {
            exact.insert(entry);
        }
    }
}

/// Match `name` against a `*`-wildcard pattern. `*` matches any
/// (possibly-empty) run of characters — including `/`, so `@babel/*`
/// matches every package in the scope. Called only for patterns known
/// to contain at least one `*`; a pattern with no `*` is routed to the
/// exact-match set instead.
///
/// The algorithm is greedy-leftmost for the middle segments with the
/// prefix anchored on the left and the suffix anchored on the right.
/// That works for plain `*` globs (no `?`, no character classes): if
/// any valid assignment of middle positions exists, the leftmost
/// valid assignment is one of them, and greedy finds it. A fixed
/// right anchor is what makes this safe — `ends_with(last)` is
/// independent of greedy choices, and everything between the last
/// greedy hit and the suffix anchor is a free `*`.
fn matches_any_wildcard(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| matches_wildcard(name, p))
}

fn matches_wildcard(name: &str, pattern: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    // `split` on a pattern with N wildcards yields N+1 parts, so the
    // two-element case is the minimum we see here.
    let (first, rest) = match parts.split_first() {
        Some(pair) => pair,
        None => return false,
    };
    let Some(after_prefix) = name.strip_prefix(first) else {
        return false;
    };
    let (last, middle) = match rest.split_last() {
        Some(pair) => pair,
        // `rest` is never empty here — the caller guarantees the
        // pattern contains at least one `*`, so `parts.len() >= 2`.
        // Fail closed rather than silently allow if that invariant
        // ever drifts: a default-allow here would be a security bypass.
        None => {
            debug_assert!(false, "matches_wildcard called with no-wildcard pattern");
            return false;
        }
    };

    let mut remaining = after_prefix;
    for mid in middle {
        match remaining.find(mid) {
            Some(idx) => remaining = &remaining[idx + mid.len()..],
            None => return false,
        }
    }
    remaining.len() >= last.len() && remaining.ends_with(last)
}

#[derive(Debug, Clone, thiserror::Error, miette::Diagnostic)]
pub enum BuildPolicyError {
    #[error("build policy entry {pattern:?} has unsupported value {raw:?}: expected true/false")]
    #[diagnostic(code(ERR_AUBE_BUILD_POLICY_UNSUPPORTED_VALUE))]
    UnsupportedValue { pattern: String, raw: String },
    #[error("build policy pattern {0:?} contains an invalid version union")]
    #[diagnostic(code(ERR_AUBE_BUILD_POLICY_INVALID_VERSION_UNION))]
    InvalidVersionUnion(String),
    #[error("build policy pattern {0:?} mixes a wildcard name with a version union")]
    #[diagnostic(code(ERR_AUBE_BUILD_POLICY_WILDCARD_WITH_VERSION))]
    WildcardWithVersion(String),
}

/// Parse one entry from the allowBuilds map into the set of strings
/// that will be matched at decide-time. Mirrors pnpm's
/// `expandPackageVersionSpecs`.
fn expand_spec(pattern: &str) -> Result<Vec<String>, BuildPolicyError> {
    let (name, versions_part) = split_name_and_versions(pattern);

    if versions_part.is_empty() {
        return Ok(vec![name.to_string()]);
    }
    if name.contains('*') {
        return Err(BuildPolicyError::WildcardWithVersion(pattern.to_string()));
    }

    let mut out = Vec::new();
    for raw in versions_part.split("||") {
        let trimmed = raw.trim();
        if is_source_version(trimmed) && !versions_part.contains("||") {
            out.push(format!("{name}@{trimmed}"));
            return Ok(out);
        }
        if trimmed.is_empty() || !is_exact_semver(trimmed) {
            return Err(BuildPolicyError::InvalidVersionUnion(pattern.to_string()));
        }
        out.push(format!("{name}@{trimmed}"));
    }
    Ok(out)
}

fn is_source_key(key: &str) -> bool {
    let (_, version) = split_name_and_versions(key);
    is_source_version(version)
}

fn is_source_version(version: &str) -> bool {
    [
        "file+",
        "link+",
        "portal+",
        "exec+",
        "git+",
        "url+",
        "file:",
        "link:",
        "portal:",
        "exec:",
        "git:",
        "http://",
        "https://",
        "github:",
        "workspace:",
    ]
    .iter()
    .any(|prefix| version.starts_with(prefix))
}

/// Split `pattern` into `(name, version_spec)`, respecting a leading
/// `@` for scoped packages so `@scope/foo@1.0.0` parses correctly.
fn split_name_and_versions(pattern: &str) -> (&str, &str) {
    let scoped = pattern.starts_with('@');
    let search_from = if scoped { 1 } else { 0 };
    match pattern[search_from..].find('@') {
        Some(rel) => {
            let at = search_from + rel;
            (&pattern[..at], &pattern[at + 1..])
        }
        None => (pattern, ""),
    }
}

/// Minimal exact-semver validator — accepts `MAJOR.MINOR.PATCH` plus an
/// optional `-prerelease` / `+build` tail. We intentionally don't pull
/// in the `semver` crate here because the file is tiny and this is the
/// only place in aube-scripts that cares about semver shape.
fn is_exact_semver(s: &str) -> bool {
    // Strip build metadata; it doesn't affect equality for our purposes.
    let core = s.split('+').next().unwrap_or(s);
    // Strip pre-release; the shape just needs to parse as numeric triple.
    let main = core.split('-').next().unwrap_or(core);
    let parts: Vec<&str> = main.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(pairs: &[(&str, bool)]) -> BuildPolicy {
        let map: BTreeMap<String, AllowBuildRaw> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), AllowBuildRaw::Bool(*v)))
            .collect();
        let (p, errs) = BuildPolicy::from_config(&map, &[], &[], false);
        assert!(errs.is_empty(), "unexpected warnings: {errs:?}");
        p
    }

    #[test]
    fn bare_name_allows_any_version() {
        let p = policy(&[("esbuild", true)]);
        assert_eq!(p.decide("esbuild", "0.19.0"), AllowDecision::Allow);
        assert_eq!(p.decide("esbuild", "0.25.0"), AllowDecision::Allow);
        assert_eq!(p.decide("rollup", "4.0.0"), AllowDecision::Unspecified);
    }

    #[test]
    fn bare_name_does_not_allow_source_backed_package() {
        let p = policy(&[("esbuild", true)]);
        assert_eq!(
            p.decide_package("esbuild", "0.25.0", Some("esbuild@file+abc123")),
            AllowDecision::Unspecified
        );
    }

    #[test]
    fn exact_source_key_allows_source_backed_package() {
        let p = policy(&[("esbuild@file+abc123", true)]);
        assert_eq!(
            p.decide_package("esbuild", "0.25.0", Some("esbuild@file+abc123")),
            AllowDecision::Allow
        );
        assert_eq!(
            p.decide_package("esbuild", "0.25.0", Some("esbuild@file+def456")),
            AllowDecision::Unspecified
        );
        assert_eq!(p.decide("esbuild", "0.25.0"), AllowDecision::Unspecified);
    }

    #[test]
    fn source_keys_accept_url_and_git_tails() {
        let p = policy(&[
            ("native@url+abc123", true),
            ("gitdep@git+def456", true),
            ("raw-url@https://example.com/pkg.tgz", true),
            ("raw-git@github:owner/repo", true),
        ]);
        assert_eq!(
            p.decide_package("native", "1.0.0", Some("native@url+abc123")),
            AllowDecision::Allow
        );
        assert_eq!(
            p.decide_package("gitdep", "1.0.0", Some("gitdep@git+def456")),
            AllowDecision::Allow
        );
        assert_eq!(
            p.decide_package(
                "raw-url",
                "1.0.0",
                Some("raw-url@https://example.com/pkg.tgz")
            ),
            AllowDecision::Allow
        );
        assert_eq!(
            p.decide_package("raw-git", "1.0.0", Some("raw-git@github:owner/repo")),
            AllowDecision::Allow
        );
    }

    #[test]
    fn source_backed_package_name_deny_still_wins() {
        let p = policy(&[("esbuild", false), ("esbuild@file+abc123", true)]);
        assert_eq!(
            p.decide_package("esbuild", "0.25.0", Some("esbuild@file+abc123")),
            AllowDecision::Deny
        );
    }

    #[test]
    fn exact_version_is_strict() {
        let p = policy(&[("esbuild@0.19.0", true)]);
        assert_eq!(p.decide("esbuild", "0.19.0"), AllowDecision::Allow);
        assert_eq!(p.decide("esbuild", "0.19.1"), AllowDecision::Unspecified);
    }

    #[test]
    fn version_union_splits() {
        let p = policy(&[("esbuild@0.19.0 || 0.20.1", true)]);
        assert_eq!(p.decide("esbuild", "0.19.0"), AllowDecision::Allow);
        assert_eq!(p.decide("esbuild", "0.20.1"), AllowDecision::Allow);
        assert_eq!(p.decide("esbuild", "0.20.0"), AllowDecision::Unspecified);
    }

    #[test]
    fn scoped_package_parses() {
        let p = policy(&[("@swc/core@1.3.0", true)]);
        assert_eq!(p.decide("@swc/core", "1.3.0"), AllowDecision::Allow);
        assert_eq!(p.decide("@swc/core", "1.4.0"), AllowDecision::Unspecified);
    }

    #[test]
    fn scoped_bare_name() {
        let p = policy(&[("@swc/core", true)]);
        assert_eq!(p.decide("@swc/core", "1.3.0"), AllowDecision::Allow);
    }

    #[test]
    fn pattern_matches_scoped_names_and_versions() {
        assert!(pattern_matches("@swc/core", "@swc/core", "1.3.0").unwrap());
        assert!(pattern_matches("@swc/core@1.3.0", "@swc/core", "1.3.0").unwrap());
        assert!(!pattern_matches("@swc/core@1.3.0", "@swc/core", "1.3.1").unwrap());
        assert!(pattern_matches("@swc/*", "@swc/core", "1.3.0").unwrap());
        assert!(pattern_matches("aube-test-*", "aube-test-native", "1.0.0").unwrap());
    }

    #[test]
    fn dangerously_allow_all_bypasses_deny_list() {
        // pnpm's `createAllowBuildFunction` short-circuits to `() => true`
        // when `dangerouslyAllowAllBuilds` is set, dropping the entire
        // allowBuilds map — including any `false` entries. Pin that
        // behavior so a future refactor doesn't accidentally start
        // honoring deny rules under allow-all.
        let mut map = BTreeMap::new();
        map.insert("esbuild".into(), AllowBuildRaw::Bool(false));
        let (p, errs) = BuildPolicy::from_config(&map, &[], &[], true);
        assert!(errs.is_empty());
        assert_eq!(p.decide("esbuild", "0.19.0"), AllowDecision::Allow);
    }

    #[test]
    fn allow_all_except_denied_keeps_explicit_false_entries_winning() {
        // The composition used when the defaultTrust floor is active:
        // dangerously-allow-all still runs everything *except* what
        // the user explicitly carved out with `false`.
        let p = policy(&[("esbuild", false), ("@babel/*", false)]).allow_all_except_denied();
        assert_eq!(p.decide("anything-else", "1.0.0"), AllowDecision::Allow);
        assert_eq!(p.decide("esbuild", "0.19.0"), AllowDecision::Deny);
        assert_eq!(p.decide("@babel/core", "7.0.0"), AllowDecision::Deny);
    }

    #[test]
    fn deny_wins_over_allow_when_both_listed() {
        let map: BTreeMap<String, AllowBuildRaw> = [
            ("esbuild".to_string(), AllowBuildRaw::Bool(true)),
            ("esbuild@0.19.0".to_string(), AllowBuildRaw::Bool(false)),
        ]
        .into_iter()
        .collect();
        let (p, errs) = BuildPolicy::from_config(&map, &[], &[], false);
        assert!(errs.is_empty());
        assert_eq!(p.decide("esbuild", "0.19.0"), AllowDecision::Deny);
        assert_eq!(p.decide("esbuild", "0.19.1"), AllowDecision::Allow);
    }

    #[test]
    fn deny_all_is_default() {
        let p = BuildPolicy::deny_all();
        assert_eq!(p.decide("anything", "1.0.0"), AllowDecision::Unspecified);
        assert!(!p.has_any_allow_rule());
    }

    #[test]
    fn allow_all_flag() {
        let p = BuildPolicy::allow_all();
        assert_eq!(p.decide("anything", "1.0.0"), AllowDecision::Allow);
        assert!(p.has_any_allow_rule());
    }

    #[test]
    fn invalid_version_union_reports_warning() {
        let map: BTreeMap<String, AllowBuildRaw> = [(
            "esbuild@not-a-version".to_string(),
            AllowBuildRaw::Bool(true),
        )]
        .into_iter()
        .collect();
        let (p, errs) = BuildPolicy::from_config(&map, &[], &[], false);
        assert_eq!(errs.len(), 1);
        // The broken entry should not leak into the allowed set.
        assert_eq!(p.decide("esbuild", "0.19.0"), AllowDecision::Unspecified);
    }

    #[test]
    fn source_specs_cannot_be_union_members() {
        let map: BTreeMap<String, AllowBuildRaw> = [(
            "dependency@https://example.com/dep.tgz || 1.0.0".to_string(),
            AllowBuildRaw::Bool(true),
        )]
        .into_iter()
        .collect();
        let (_, errs) = BuildPolicy::from_config(&map, &[], &[], false);
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0], BuildPolicyError::InvalidVersionUnion(_)));
    }

    #[test]
    fn semver_then_source_spec_union_is_also_rejected() {
        let map: BTreeMap<String, AllowBuildRaw> = [(
            "dependency@1.0.0 || https://example.com/dep.tgz".to_string(),
            AllowBuildRaw::Bool(true),
        )]
        .into_iter()
        .collect();
        let (_, errs) = BuildPolicy::from_config(&map, &[], &[], false);
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0], BuildPolicyError::InvalidVersionUnion(_)));
    }

    #[test]
    fn non_bool_value_reports_warning() {
        let map: BTreeMap<String, AllowBuildRaw> =
            [("esbuild".to_string(), AllowBuildRaw::Other("maybe".into()))]
                .into_iter()
                .collect();
        let (_, errs) = BuildPolicy::from_config(&map, &[], &[], false);
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn only_built_dependencies_allowlist_coexists_with_allow_builds() {
        // pnpm's canonical `onlyBuiltDependencies` flat list is additive
        // with `allowBuilds`, so both sources populate the same allowed
        // set. Same pattern vocabulary — bare name or exact version.
        let map = BTreeMap::new();
        let only_built = vec!["esbuild".to_string(), "@swc/core@1.3.0".to_string()];
        let (p, errs) = BuildPolicy::from_config(&map, &only_built, &[], false);
        assert!(errs.is_empty());
        assert_eq!(p.decide("esbuild", "0.19.0"), AllowDecision::Allow);
        assert_eq!(p.decide("@swc/core", "1.3.0"), AllowDecision::Allow);
        assert_eq!(p.decide("@swc/core", "1.4.0"), AllowDecision::Unspecified);
        assert!(p.has_any_allow_rule());
    }

    #[test]
    fn never_built_dependencies_denies() {
        let map = BTreeMap::new();
        let only_built = vec!["esbuild".to_string()];
        let never_built = vec!["esbuild@0.19.0".to_string()];
        let (p, errs) = BuildPolicy::from_config(&map, &only_built, &never_built, false);
        assert!(errs.is_empty());
        assert_eq!(p.decide("esbuild", "0.19.0"), AllowDecision::Deny);
        assert_eq!(p.decide("esbuild", "0.20.0"), AllowDecision::Allow);
    }

    #[test]
    fn never_built_beats_allow_builds_map() {
        // Cross-source precedence: a bare-name deny in
        // `neverBuiltDependencies` overrides a bare-name allow in the
        // `allowBuilds` map. Mirrors the in-map deny-wins test above.
        let map: BTreeMap<String, AllowBuildRaw> =
            [("esbuild".to_string(), AllowBuildRaw::Bool(true))]
                .into_iter()
                .collect();
        let never_built = vec!["esbuild".to_string()];
        let (p, errs) = BuildPolicy::from_config(&map, &[], &never_built, false);
        assert!(errs.is_empty());
        assert_eq!(p.decide("esbuild", "0.19.0"), AllowDecision::Deny);
    }

    #[test]
    fn merge_deduplicates_wildcards() {
        let mut p = policy(&[("@babel/*", true), ("*-internal", false)]);
        let other = policy(&[
            ("@babel/*", true),
            ("@types/*", true),
            ("*-internal", false),
        ]);
        p.merge(&other);
        p.merge(&other);

        assert_eq!(p.allowed_wildcards, vec!["@babel/*", "@types/*"]);
        assert_eq!(p.denied_wildcards, vec!["*-internal"]);
        assert_eq!(p.decide("@types/node", "1.0.0"), AllowDecision::Allow);
        assert_eq!(p.decide("pkg-internal", "1.0.0"), AllowDecision::Deny);
    }

    #[test]
    fn splits_scoped_correctly() {
        assert_eq!(
            split_name_and_versions("@swc/core@1.3.0"),
            ("@swc/core", "1.3.0")
        );
        assert_eq!(split_name_and_versions("@swc/core"), ("@swc/core", ""));
        assert_eq!(
            split_name_and_versions("esbuild@0.19.0"),
            ("esbuild", "0.19.0")
        );
        assert_eq!(split_name_and_versions("esbuild"), ("esbuild", ""));
    }

    #[test]
    fn wildcard_scope_allows_every_scope_member() {
        let p = policy(&[("@babel/*", true)]);
        assert_eq!(p.decide("@babel/core", "7.0.0"), AllowDecision::Allow);
        assert_eq!(
            p.decide("@babel/preset-env", "7.22.0"),
            AllowDecision::Allow
        );
        assert_eq!(p.decide("@swc/core", "1.3.0"), AllowDecision::Unspecified);
        assert_eq!(
            p.decide("babel-loader", "9.0.0"),
            AllowDecision::Unspecified
        );
        assert!(p.has_any_allow_rule());
    }

    #[test]
    fn wildcard_suffix_matches_any_prefix() {
        let p = policy(&[("*-loader", true)]);
        assert_eq!(p.decide("css-loader", "6.0.0"), AllowDecision::Allow);
        assert_eq!(p.decide("babel-loader", "9.0.0"), AllowDecision::Allow);
        assert_eq!(
            p.decide("loader-utils", "3.0.0"),
            AllowDecision::Unspecified
        );
    }

    #[test]
    fn bare_star_matches_everything_and_is_distinct_from_allow_all() {
        // `*` in the allowlist behaves like "allow every package" but
        // is still a normal allow rule — deny entries still override
        // it, unlike `dangerouslyAllowAllBuilds` which short-circuits.
        let map: BTreeMap<String, AllowBuildRaw> = [
            ("*".to_string(), AllowBuildRaw::Bool(true)),
            ("sketchy-pkg".to_string(), AllowBuildRaw::Bool(false)),
        ]
        .into_iter()
        .collect();
        let (p, errs) = BuildPolicy::from_config(&map, &[], &[], false);
        assert!(errs.is_empty());
        assert_eq!(p.decide("esbuild", "0.19.0"), AllowDecision::Allow);
        assert_eq!(p.decide("sketchy-pkg", "1.0.0"), AllowDecision::Deny);
    }

    #[test]
    fn denied_wildcard_blocks_allowed_exact() {
        let map: BTreeMap<String, AllowBuildRaw> = [
            ("@babel/core".to_string(), AllowBuildRaw::Bool(true)),
            ("@babel/*".to_string(), AllowBuildRaw::Bool(false)),
        ]
        .into_iter()
        .collect();
        let (p, errs) = BuildPolicy::from_config(&map, &[], &[], false);
        assert!(errs.is_empty());
        assert_eq!(p.decide("@babel/core", "7.0.0"), AllowDecision::Deny);
        assert_eq!(p.decide("@babel/traverse", "7.0.0"), AllowDecision::Deny);
    }

    #[test]
    fn wildcard_with_version_is_rejected() {
        let map: BTreeMap<String, AllowBuildRaw> =
            [("@babel/*@7.0.0".to_string(), AllowBuildRaw::Bool(true))]
                .into_iter()
                .collect();
        let (p, errs) = BuildPolicy::from_config(&map, &[], &[], false);
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0], BuildPolicyError::WildcardWithVersion(_)));
        // The rejected entry should not leak through as either an
        // exact or a wildcard allow.
        assert_eq!(p.decide("@babel/core", "7.0.0"), AllowDecision::Unspecified);
    }

    #[test]
    fn wildcards_flow_through_flat_lists_too() {
        let only_built = vec!["@types/*".to_string()];
        let never_built = vec!["*-internal".to_string()];
        let (p, errs) =
            BuildPolicy::from_config(&BTreeMap::new(), &only_built, &never_built, false);
        assert!(errs.is_empty());
        assert_eq!(p.decide("@types/node", "20.0.0"), AllowDecision::Allow);
        assert_eq!(p.decide("@types/react", "18.0.0"), AllowDecision::Allow);
        assert_eq!(p.decide("acme-internal", "1.0.0"), AllowDecision::Deny);
    }

    #[test]
    fn matches_wildcard_handles_all_positions() {
        assert!(matches_wildcard("@babel/core", "@babel/*"));
        assert!(matches_wildcard("@babel/", "@babel/*"));
        assert!(!matches_wildcard("@babe/core", "@babel/*"));

        assert!(matches_wildcard("css-loader", "*-loader"));
        assert!(matches_wildcard("-loader", "*-loader"));
        assert!(!matches_wildcard("loader-x", "*-loader"));

        assert!(matches_wildcard("foobar", "foo*bar"));
        assert!(matches_wildcard("foo-x-bar", "foo*bar"));
        assert!(!matches_wildcard("foobaz", "foo*bar"));

        assert!(matches_wildcard("@x/anything", "*"));
        assert!(matches_wildcard("", "*"));

        // Adjacent wildcards collapse to a single match, same as glob.
        assert!(matches_wildcard("anything", "**"));
    }

    #[test]
    fn matches_wildcard_multi_segment_greedy_is_correct() {
        // Three+ wildcards exercise the greedy-leftmost middle-segment
        // scan with a fixed-right suffix anchor. Each case either has a
        // valid assignment (should match) or none (should not), and
        // greedy-leftmost finds it whenever one exists — the fixed
        // right anchor prevents greedy from eating characters the
        // suffix needs.
        assert!(matches_wildcard("abca", "*a*bc*a"));
        assert!(matches_wildcard("xabcaYa", "*a*bc*a"));
        assert!(matches_wildcard("abcaXa", "*a*bc*a"));
        assert!(matches_wildcard("ababab", "*ab*ab*"));
        assert!(matches_wildcard("abcd", "a*b*c*d"));
        assert!(matches_wildcard("a1b2c3d", "a*b*c*d"));

        // Needs two non-overlapping occurrences of the middle / last
        // anchors but the input only provides enough characters for
        // one, so no assignment exists.
        assert!(!matches_wildcard("aab", "*ab*ab"));
        assert!(!matches_wildcard("abab", "*abc*abc"));

        // Four wildcards still obey the same rules.
        assert!(matches_wildcard(
            "@acme/core-loader-plugin",
            "@acme/*-*-plugin"
        ));
        assert!(!matches_wildcard(
            "@acme/core-plugin-extra",
            "@acme/*-*-plugin"
        ));
    }

    #[test]
    fn semver_shape() {
        assert!(is_exact_semver("1.2.3"));
        assert!(is_exact_semver("0.19.0"));
        assert!(is_exact_semver("1.0.0-alpha"));
        assert!(is_exact_semver("1.0.0+build.42"));
        assert!(!is_exact_semver("1.2"));
        assert!(!is_exact_semver("^1.2.3"));
        assert!(!is_exact_semver("1.x.0"));
    }
}
