//! Parsing and matching for pnpm/yarn override selector keys.
//!
//! The manifest layer hands us a `BTreeMap<String, String>` where keys
//! may be any of:
//!
//! - bare name: `lodash`, `@babel/core`
//! - version-pinned target: `lodash@<4.17.21`, `@scope/pkg@^1`
//! - pnpm parent chain: `foo>bar`, `foo@1>bar@<2`, `a>b>c`
//! - yarn wildcard / ancestor: `**/foo`, `parent/foo`, `@scope/parent/foo`
//!
//! Invalid or unparseable keys are silently dropped at parse time so
//! the resolver hot loop never has to deal with them.

use std::collections::BTreeMap;

/// A compiled override rule: zero or more ancestor segments, one target
/// segment, and the replacement spec (version, alias, etc.) taken from
/// the map value. Ancestor segments are stored outermost-first so
/// matching can walk them against a task's parent chain in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideRule {
    pub parents: Vec<Segment>,
    pub target: Segment,
    pub replacement: String,
    /// The raw selector key, preserved for debug tracing / error
    /// messages. Never used for matching.
    pub raw_key: String,
}

/// One parsed segment of a selector — a package name plus an optional
/// semver range. The `**` wildcard is represented by an empty `name`;
/// matching treats it as "absorb any number of ancestors" (including
/// zero) during parent-chain evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub name: String,
    pub version_req: Option<String>,
}

impl Segment {
    fn wildcard() -> Self {
        Segment {
            name: String::new(),
            version_req: None,
        }
    }

    pub fn is_wildcard(&self) -> bool {
        self.name.is_empty()
    }
}

/// Compile a map of raw selector keys → replacement specs into a list
/// of `OverrideRule`s. Keys that don't parse are logged at trace level
/// and dropped. The input map's BTreeMap ordering is preserved, which
/// happens to give scope-then-name lexicographic ordering — fine for
/// deterministic rule application since our matcher picks the first
/// hit and the manifest already merged the precedence tiers.
pub fn compile(raw: &BTreeMap<String, String>) -> Vec<OverrideRule> {
    let mut out = Vec::with_capacity(raw.len());
    for (k, v) in raw {
        match parse_key(k) {
            Some((parents, target)) => out.push(OverrideRule {
                parents,
                target,
                replacement: v.clone(),
                raw_key: k.clone(),
            }),
            None => {
                tracing::trace!("ignoring unparseable override selector {k:?}");
            }
        }
    }
    out
}

/// Parse one selector key into (parents, target).
fn parse_key(key: &str) -> Option<(Vec<Segment>, Segment)> {
    if key.is_empty() {
        return None;
    }
    let raw_segments = split_segments(key)?;
    if raw_segments.is_empty() {
        return None;
    }
    let mut segments = Vec::with_capacity(raw_segments.len());
    for seg in &raw_segments {
        segments.push(parse_segment(seg)?);
    }
    // The target must be a real package name; `**` as the target is
    // nonsense and yarn doesn't allow it.
    let target = segments.pop().unwrap();
    if target.is_wildcard() {
        return None;
    }
    Some((segments, target))
}

/// Split a selector key into its segment strings. pnpm uses `>` as a
/// hard separator between `name[@versionreq]` segments; yarn uses `/`
/// and expects scoped names to count as a single segment. We
/// auto-detect: `>` wins if present, otherwise fall back to yarn-style
/// slash tokenization that respects scopes.
///
/// The `>` split isn't a blind `str::split('>')` because a `>`
/// character is also legal *inside* a version req (`>=2`, `>1.0.0`).
/// We walk the key instead, tracking whether we're sitting inside a
/// version req (entered via an `@` that isn't the scope prefix) and
/// only splitting on `>` that *starts a new segment* — i.e. isn't
/// followed by something that looks like a semver comparator
/// continuation (`=`, a digit, whitespace, or a leading `v`). Any
/// other `>` ends the current version req and introduces the next
/// segment.
fn split_segments(key: &str) -> Option<Vec<&str>> {
    if key.contains('>') {
        let mut parts: Vec<&str> = Vec::new();
        let bytes = key.as_bytes();
        let mut start = 0;
        let mut i = 0;
        // `in_req` flips true as soon as we see the `@` that
        // introduces a segment's version req. It resets to false on
        // every segment boundary.
        let mut in_req = false;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'@' && !in_req && i != start {
                // A non-leading `@` inside a segment starts the
                // version req. (A leading `@` is the scope prefix
                // and is handled by `parse_segment`.)
                in_req = true;
            } else if c == b'>' {
                if in_req {
                    // Could be part of a semver comparator: `>=2`,
                    // `>1.0`, `> 1` (whitespace), `>v2`. If so, keep
                    // consuming as part of the version req.
                    let looks_like_comparator_cont = bytes
                        .get(i + 1)
                        .is_some_and(|&n| matches!(n, b'=' | b' ' | b'v') || n.is_ascii_digit());
                    if looks_like_comparator_cont {
                        i += 1;
                        continue;
                    }
                }
                // Segment boundary.
                if start == i {
                    return None; // empty segment, e.g. `>foo` or `foo>>bar`
                }
                parts.push(&key[start..i]);
                start = i + 1;
                in_req = false;
            }
            i += 1;
        }
        if start >= bytes.len() {
            return None; // key ended on `>`
        }
        parts.push(&key[start..]);
        return Some(parts);
    }
    // Yarn slash form. Walk byte-by-byte so we can tell scope `/` from
    // ancestor `/`: a `/` inside a segment that started with `@` and
    // hasn't seen a `/` yet is a scope separator.
    let bytes = key.as_bytes();
    let mut out: Vec<&str> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' {
            let current = &key[start..i];
            let looks_like_scope = current.starts_with('@') && !current[1..].contains('/');
            if !looks_like_scope {
                if current.is_empty() {
                    return None;
                }
                out.push(current);
                start = i + 1;
            }
        }
        i += 1;
    }
    let tail = &key[start..];
    if tail.is_empty() {
        return None;
    }
    out.push(tail);
    Some(out)
}

/// Parse one segment string into a `Segment`. Handles `**` (wildcard),
/// scoped and unscoped names, and an optional `@version-req` suffix.
fn parse_segment(seg: &str) -> Option<Segment> {
    if seg == "**" {
        return Some(Segment::wildcard());
    }
    // Scoped: `@scope/name` or `@scope/name@<req>`
    if let Some(after_at) = seg.strip_prefix('@') {
        let slash = after_at.find('/')?;
        let rest = &after_at[slash + 1..];
        if rest.is_empty() {
            return None;
        }
        // Is there a version req? The first `@` in `rest` (if any) marks it.
        if let Some(at) = rest.find('@') {
            let pkg_tail = &rest[..at];
            let req = &rest[at + 1..];
            if pkg_tail.is_empty() || req.is_empty() {
                return None;
            }
            Some(Segment {
                name: format!("@{}/{}", &after_at[..slash], pkg_tail),
                version_req: Some(req.to_string()),
            })
        } else {
            Some(Segment {
                name: format!("@{after_at}"),
                version_req: None,
            })
        }
    } else if let Some(at) = seg.find('@') {
        let name = &seg[..at];
        let req = &seg[at + 1..];
        if name.is_empty() || req.is_empty() {
            return None;
        }
        Some(Segment {
            name: name.to_string(),
            version_req: Some(req.to_string()),
        })
    } else {
        Some(Segment {
            name: seg.to_string(),
            version_req: None,
        })
    }
}

/// An ancestor frame recorded while the resolver walks back from a
/// task through its parent dep_paths. Outermost-first order mirrors
/// how selectors are written (`root>mid>leaf`).
#[derive(Debug, Clone)]
pub struct AncestorFrame<'a> {
    pub name: &'a str,
    pub version: &'a str,
}

/// Test a rule against a target task plus its ancestor chain. The
/// target's version constraint (if any) is matched against
/// `task_range` via true range intersection (`range_could_satisfy`),
/// mirroring pnpm's `semver.intersects` — the override fires whenever
/// the declared range could resolve to a version the selector covers.
pub fn matches(
    rule: &OverrideRule,
    task_name: &str,
    task_range: &str,
    ancestors: &[AncestorFrame<'_>],
) -> bool {
    if rule.target.name != task_name {
        return false;
    }
    if let Some(ref req) = rule.target.version_req
        && !range_could_satisfy(task_range, req)
    {
        return false;
    }
    match_parent_chain(&rule.parents, ancestors)
}

fn match_parent_chain(parents: &[Segment], ancestors: &[AncestorFrame<'_>]) -> bool {
    if parents.is_empty() {
        return true;
    }
    // pnpm's `>` is the *direct dependency of* relationship — `a>b>c`
    // means c's immediate parent is b, and b's immediate parent is a.
    // Anchor the match at the innermost ancestor (`ancestors.last()`,
    // which is the target's direct parent) and walk the selector's
    // segments from the right. `**` wildcards absorb any number of
    // ancestors further up the chain toward the root.
    match_from_right(parents, ancestors)
}

fn match_from_right(parents: &[Segment], anc: &[AncestorFrame<'_>]) -> bool {
    let Some((last, rest)) = parents.split_last() else {
        // All selector segments consumed. Any remaining ancestors
        // above are free — the selector doesn't pin the root.
        return true;
    };
    if last.is_wildcard() {
        // Absorb 0..=anc.len() innermost ancestors so the next (more
        // outer) selector segment is free to anchor anywhere above.
        for take in 0..=anc.len() {
            let head = &anc[..anc.len() - take];
            if match_from_right(rest, head) {
                return true;
            }
        }
        return false;
    }
    let Some((frame, head)) = anc.split_last() else {
        return false;
    };
    if last.name != frame.name {
        return false;
    }
    if let Some(ref req) = last.version_req
        && !version_in_req(frame.version, req)
    {
        return false;
    }
    match_from_right(rest, head)
}

fn version_in_req(version: &str, req: &str) -> bool {
    let Ok(v) = node_semver::Version::parse(version) else {
        return false;
    };
    let Ok(r) = node_semver::Range::parse(req) else {
        return false;
    };
    v.satisfies(&r)
}

/// Does the task's declared range overlap the selector req? This is a
/// true range intersection — "is there any version satisfying both the
/// declared range and the selector range" — matching pnpm's
/// `isIntersectingRange` (`semver.intersects`) in
/// `createVersionsOverrider`. A range-selector override fires whenever
/// the declared range *could resolve* to a version the selector covers,
/// even when the declared range's lower bound sits below the selector
/// (e.g. declared `^7.0.0` vs selector `>=7.5.0`: 7.8.4 satisfies both,
/// so the override applies — a security/compat downgrade keyed on the
/// range no longer silently drops).
///
/// For a selector req we can't parse as a range we return `true`
/// (overriding too aggressively beats silently ignoring); for a task
/// range we can't parse we also don't block the override.
fn range_could_satisfy(task_range: &str, req: &str) -> bool {
    let Ok(selector) = node_semver::Range::parse(req) else {
        return true;
    };
    // A concrete pinned version as the task range: membership in the
    // selector is the intersection question.
    if let Ok(v) = node_semver::Version::parse(task_range) {
        return v.satisfies(&selector);
    }
    let Ok(declared) = node_semver::Range::parse(task_range) else {
        // Couldn't make sense of task_range. Don't block the override.
        return true;
    };
    declared.allows_any(&selector)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(key: &str, value: &str) -> OverrideRule {
        let mut m = BTreeMap::new();
        m.insert(key.to_string(), value.to_string());
        compile(&m).into_iter().next().unwrap()
    }

    fn anc(name: &str, version: &str) -> AncestorFrame<'static> {
        // Leak for test convenience — these live for the whole test binary.
        let name: &'static str = Box::leak(name.to_string().into_boxed_str());
        let version: &'static str = Box::leak(version.to_string().into_boxed_str());
        AncestorFrame { name, version }
    }

    #[test]
    fn parses_bare_name() {
        let r = rule("lodash", "4.17.21");
        assert_eq!(r.target.name, "lodash");
        assert_eq!(r.target.version_req, None);
        assert!(r.parents.is_empty());
    }

    #[test]
    fn parses_scoped_name() {
        let r = rule("@babel/core", "7.20.0");
        assert_eq!(r.target.name, "@babel/core");
    }

    #[test]
    fn parses_target_version_req() {
        let r = rule("lodash@<4.17.21", "4.17.21");
        assert_eq!(r.target.name, "lodash");
        assert_eq!(r.target.version_req.as_deref(), Some("<4.17.21"));
    }

    #[test]
    fn parses_pnpm_parent_chain() {
        let r = rule("foo>bar", "1.0.0");
        assert_eq!(r.parents.len(), 1);
        assert_eq!(r.parents[0].name, "foo");
        assert_eq!(r.target.name, "bar");
    }

    #[test]
    fn parses_pnpm_parent_chain_with_versions() {
        let r = rule("foo@^1>bar@<2", "1.5.0");
        assert_eq!(r.parents[0].name, "foo");
        assert_eq!(r.parents[0].version_req.as_deref(), Some("^1"));
        assert_eq!(r.target.name, "bar");
        assert_eq!(r.target.version_req.as_deref(), Some("<2"));
    }

    #[test]
    fn parses_yarn_wildcard() {
        let r = rule("**/foo", "1.0.0");
        assert_eq!(r.parents.len(), 1);
        assert!(r.parents[0].is_wildcard());
        assert_eq!(r.target.name, "foo");
    }

    #[test]
    fn parses_yarn_ancestor() {
        let r = rule("parent/foo", "1.0.0");
        assert_eq!(r.parents.len(), 1);
        assert_eq!(r.parents[0].name, "parent");
        assert_eq!(r.target.name, "foo");
    }

    #[test]
    fn parses_yarn_scoped_ancestor() {
        let r = rule("@scope/parent/foo", "1.0.0");
        assert_eq!(r.parents.len(), 1);
        assert_eq!(r.parents[0].name, "@scope/parent");
        assert_eq!(r.target.name, "foo");
    }

    #[test]
    fn parses_target_with_gte_comparator() {
        // `>=` must NOT be treated as a chain separator.
        let r = rule("is-number@>=8", "7.0.0");
        assert!(r.parents.is_empty());
        assert_eq!(r.target.name, "is-number");
        assert_eq!(r.target.version_req.as_deref(), Some(">=8"));
    }

    #[test]
    fn parses_target_with_gt_digit_comparator() {
        // `>1` looks like a comparator (`>` + digit), not a chain.
        let r = rule("lodash@>1.0.0", "1.5.0");
        assert!(r.parents.is_empty());
        assert_eq!(r.target.name, "lodash");
        assert_eq!(r.target.version_req.as_deref(), Some(">1.0.0"));
    }

    #[test]
    fn parses_parent_chain_with_comparator_in_target() {
        // Mixed case: parent chain followed by a target with a `>=`
        // comparator. The parent `>` still splits, but the `>=` on
        // the target stays attached.
        let r = rule("foo>bar@>=2", "2.5.0");
        assert_eq!(r.parents.len(), 1);
        assert_eq!(r.parents[0].name, "foo");
        assert_eq!(r.target.name, "bar");
        assert_eq!(r.target.version_req.as_deref(), Some(">=2"));
    }

    #[test]
    fn parses_parent_with_comparator_then_child() {
        // Parent version req also uses `>=`; chain `>` still splits.
        let r = rule("foo@>=1>bar", "1.0.0");
        assert_eq!(r.parents.len(), 1);
        assert_eq!(r.parents[0].name, "foo");
        assert_eq!(r.parents[0].version_req.as_deref(), Some(">=1"));
        assert_eq!(r.target.name, "bar");
    }

    #[test]
    fn rejects_empty_segments() {
        let mut m = BTreeMap::new();
        m.insert("foo>".to_string(), "1".to_string());
        m.insert(">foo".to_string(), "1".to_string());
        m.insert("".to_string(), "1".to_string());
        assert!(compile(&m).is_empty());
    }

    #[test]
    fn bare_name_rule_matches_anywhere() {
        let r = rule("foo", "1.0.0");
        assert!(matches(&r, "foo", "^1", &[]));
        assert!(matches(&r, "foo", "^1", &[anc("bar", "2.0.0")]));
        assert!(!matches(&r, "other", "^1", &[]));
    }

    #[test]
    fn parent_chain_requires_matching_ancestor() {
        let r = rule("parent>foo", "1.0.0");
        assert!(matches(&r, "foo", "^1", &[anc("parent", "1.0.0")]));
        assert!(!matches(&r, "foo", "^1", &[anc("other", "1.0.0")]));
        assert!(!matches(&r, "foo", "^1", &[]));
    }

    #[test]
    fn wildcard_absorbs_any_ancestor_depth() {
        let r = rule("**/foo", "1.0.0");
        assert!(matches(&r, "foo", "^1", &[]));
        assert!(matches(
            &r,
            "foo",
            "^1",
            &[anc("a", "1.0.0"), anc("b", "1.0.0")]
        ));
    }

    #[test]
    fn target_version_req_filters_by_range() {
        let r = rule("foo@<2", "1.9.0");
        // Range whose lower bound is 1.x matches <2.
        assert!(matches(&r, "foo", "^1.0.0", &[]));
        // Range whose lower bound is 3.x does not.
        assert!(!matches(&r, "foo", "^3.0.0", &[]));
    }

    #[test]
    fn target_version_req_matches_when_range_overlaps_above_lower_bound() {
        // A range-selector override whose lower bound is *above* the
        // declared range's lower bound, but whose range still overlaps
        // it, must apply — matching pnpm's `semver.intersects`. Declared
        // `^7.0.0` (>=7.0.0 <8.0.0) overlaps selector `>=7.5.0` (a real
        // version like 7.8.4 satisfies both), so a security/compat
        // downgrade keyed on the range fires instead of silently
        // dropping. Regression for the lower-bound-probe limitation.
        let r = rule("semver@>=7.5.0", "7.3.8");
        assert!(matches(&r, "semver", "^7.0.0", &[]));
    }

    #[test]
    fn target_version_req_does_not_match_disjoint_range() {
        // Guardrail: a selector that genuinely cannot share any version
        // with the declared range must still be skipped. `^6.0.0`
        // (>=6.0.0 <7.0.0) does not intersect `>=8`.
        let r = rule("is-number@>=8", "7.0.0");
        assert!(!matches(&r, "is-number", "^6.0.0", &[]));
    }

    #[test]
    fn parent_version_req_filters_ancestors() {
        let r = rule("parent@^1>foo", "1.0.0");
        assert!(matches(&r, "foo", "^1", &[anc("parent", "1.5.0")]));
        assert!(!matches(&r, "foo", "^1", &[anc("parent", "2.0.0")]));
    }

    #[test]
    fn parent_chain_anchors_at_direct_parent() {
        // `b>c` matches when c's *direct* parent is b, regardless of
        // what's above b. Ancestors are outermost-first, so `a` is
        // further from c (root side) and `b` is the innermost frame
        // (c's immediate parent).
        let r = rule("b>c", "1.0.0");
        let ancestors = [anc("a", "1.0.0"), anc("b", "1.0.0")];
        assert!(matches(&r, "c", "^1", &ancestors));
    }

    #[test]
    fn parent_chain_rejects_skipped_ancestor() {
        // pnpm's `>` is a direct-dep relation. `a>c` must NOT match
        // when c's direct parent is b, even if `a` is higher up.
        let r = rule("a>c", "1.0.0");
        let ancestors = [anc("a", "1.0.0"), anc("b", "1.0.0")];
        assert!(!matches(&r, "c", "^1", &ancestors));
    }

    #[test]
    fn wildcard_in_parent_chain_absorbs_skipped_ancestors() {
        // `a>**>c` matches when a is somewhere above c, regardless
        // of what sits between them.
        let r = rule("a>**>c", "1.0.0");
        let ancestors = [anc("a", "1.0.0"), anc("x", "1.0.0"), anc("b", "1.0.0")];
        assert!(matches(&r, "c", "^1", &ancestors));
    }

    #[test]
    fn range_intersection_overlap_and_disjoint() {
        // Overlapping ranges intersect; disjoint ones don't — the core
        // of pnpm-compatible range-selector matching.
        assert!(range_could_satisfy("^7.0.0", ">=7.5.0")); // overlap above lower bound
        assert!(range_could_satisfy("^1.0.0", "<2")); // classic
        assert!(range_could_satisfy("1.2.3", ">=1.2.0")); // concrete version in range
        assert!(!range_could_satisfy("^6.0.0", ">=8")); // disjoint
        assert!(!range_could_satisfy("^3.0.0", "<2")); // disjoint
        assert!(!range_could_satisfy("1.2.3", ">=2.0.0")); // concrete version outside
    }
}
