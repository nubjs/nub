//! Minimal override-key matcher for the importer-level drift check.
//!
//! pnpm rewrites an importer's recorded `specifier` when an override
//! fires on a direct dep — so a manifest that reads `"plist": "^3.0.4"`
//! with override `"plist@<3.0.5": ">=3.0.5"` produces a lockfile that
//! records `specifier: ">=3.0.5"`. `--frozen-lockfile` must apply the
//! same override to the manifest spec before comparing, otherwise
//! every pnpm-written lockfile with overrides reads stale on the next
//! frozen install.
//!
//! The full pnpm/yarn override grammar (parent chains `foo>bar`, yarn
//! wildcards `**/foo`) lives in `aube-resolver::override_rule`. Direct
//! deps of an importer have no ancestor chain by construction, so this
//! matcher only handles the two key shapes that can fire here:
//!
//! - bare name: `lodash`, `@babel/core`
//! - name + version range: `lodash@<4.17.21`, `@scope/pkg@^1`
//!
//! Keys with parent-chain syntax are ignored — they can't match a
//! direct-dep override application.
//!
//! Kept inside aube-lockfile (rather than reaching into aube-resolver)
//! to avoid a cross-crate dep cycle: aube-resolver already depends on
//! aube-lockfile.
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub(crate) struct DirectOverrideRule {
    pub name: String,
    pub version_req: Option<String>,
    pub replacement: String,
}

/// Parse and compile a raw `name → replacement` map into rules. Keys
/// with parent-chain selectors (`foo>bar`, `**/foo`, `parent/foo`) are
/// dropped — they only match transitive deps.
///
/// Output is sorted so version-keyed rules come before bare-name rules
/// for the same package. Mirrors pnpm's "more specific selector wins"
/// behavior: when a manifest has both `"plist": "9.9.9"` and
/// `"plist@<3": "2.0.0"`, pnpm picks the version-keyed one for any
/// matching range, and the lockfile records that replacement. A
/// bare-first iteration order would always shadow the version-keyed
/// rule and produce a false `Stale`.
pub(crate) fn compile(raw: &BTreeMap<String, String>) -> Vec<DirectOverrideRule> {
    let mut rules: Vec<DirectOverrideRule> = raw
        .iter()
        .filter_map(|(k, v)| {
            parse_key(k).map(|(n, r)| DirectOverrideRule {
                name: n,
                version_req: r,
                replacement: v.clone(),
            })
        })
        .collect();
    rules.sort_by_key(|r| r.version_req.is_none());
    rules
}

/// Find the first rule whose target matches `(name, spec)` and return
/// its replacement spec. A rule matches when (a) the target name is
/// equal and (b) either the rule has no version req, or the manifest
/// spec's lower-bound version satisfies the rule's req — same probe
/// `aube-resolver::override_rule` uses.
pub(crate) fn apply<'a>(
    rules: &'a [DirectOverrideRule],
    name: &str,
    spec: &str,
) -> Option<&'a str> {
    rules.iter().find_map(|rule| {
        if rule.name != name {
            return None;
        }
        match rule.version_req.as_deref() {
            None => Some(rule.replacement.as_str()),
            Some(req) if range_could_satisfy(spec, req) => Some(rule.replacement.as_str()),
            _ => None,
        }
    })
}

fn parse_key(key: &str) -> Option<(String, Option<String>)> {
    if key.is_empty() {
        return None;
    }
    // Multi-segment selectors are parent-chain rules (`foo>bar`,
    // `**/foo`, `parent/foo`) — they only fire on transitive deps.
    let segments = split_segments(key)?;
    if segments.len() != 1 {
        return None;
    }
    parse_segment(segments[0])
}

/// Split `key` on pnpm `>` chain separators (and yarn `/` ancestors),
/// while keeping `>` characters that belong to a version comparator
/// (`>=`, `>1.0.0`, `> 1`) attached to the segment they qualify.
/// Mirrors `aube-resolver::override_rule::split_segments`.
fn split_segments(key: &str) -> Option<Vec<&str>> {
    if key.contains('>') {
        let bytes = key.as_bytes();
        let mut parts: Vec<&str> = Vec::new();
        let mut start = 0;
        let mut i = 0;
        let mut in_req = false;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'@' && !in_req && i != start {
                in_req = true;
            } else if c == b'>' {
                if in_req {
                    let comparator_cont = bytes
                        .get(i + 1)
                        .is_some_and(|&n| matches!(n, b'=' | b' ' | b'v') || n.is_ascii_digit());
                    if comparator_cont {
                        i += 1;
                        continue;
                    }
                }
                if start == i {
                    return None;
                }
                parts.push(&key[start..i]);
                start = i + 1;
                in_req = false;
            }
            i += 1;
        }
        if start >= bytes.len() {
            return None;
        }
        parts.push(&key[start..]);
        return Some(parts);
    }
    // Yarn slash form: split on `/` except the scope-introducing `/`.
    let bytes = key.as_bytes();
    let mut out: Vec<&str> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' {
            let current = &key[start..i];
            let scope = current.starts_with('@') && !current[1..].contains('/');
            if !scope {
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

/// Parse a single segment `name[@range]` (scoped or unscoped) into its
/// (name, req) pair. Wildcards (`**`) are rejected — they're a
/// parent-chain construct that has no meaning at the importer level.
fn parse_segment(seg: &str) -> Option<(String, Option<String>)> {
    if seg == "**" {
        return None;
    }
    if let Some(after_at) = seg.strip_prefix('@') {
        let slash = after_at.find('/')?;
        let rest = &after_at[slash + 1..];
        if rest.is_empty() {
            return None;
        }
        if let Some(at) = rest.find('@') {
            let pkg_tail = &rest[..at];
            let req = &rest[at + 1..];
            if pkg_tail.is_empty() || req.is_empty() {
                return None;
            }
            Some((
                format!("@{}/{}", &after_at[..slash], pkg_tail),
                Some(req.to_string()),
            ))
        } else {
            Some((format!("@{after_at}"), None))
        }
    } else if let Some(at) = seg.find('@') {
        if at == 0 {
            return None;
        }
        let name = &seg[..at];
        let req = &seg[at + 1..];
        if name.is_empty() || req.is_empty() {
            return None;
        }
        Some((name.to_string(), Some(req.to_string())))
    } else {
        Some((seg.to_string(), None))
    }
}

/// Lower-bound probe. Mirrors `aube-resolver::override_rule::range_could_satisfy`
/// without the cross-crate dep. A range whose extractable lower bound
/// satisfies the req counts as a hit. Ranges we can't parse fall through
/// to "probably matches" so a user override is never silently dropped.
///
/// Exclusive `>X.Y.Z` is special-cased: trimming the prefix yields the
/// boundary itself, which fails any `<X.Y.Z` req and would otherwise
/// fall through to "probably matches" — spuriously firing the override
/// for two ranges with empty intersection. The caller signals the
/// exclusive form via a separate try with `bumped_lower_bound` first.
fn range_could_satisfy(task_range: &str, req: &str) -> bool {
    let Ok(r) = node_semver::Range::parse(req) else {
        return true;
    };
    if let Ok(v) = node_semver::Version::parse(task_range)
        && v.satisfies(&r)
    {
        return true;
    }
    let trimmed = task_range.trim();
    let exclusive = trimmed.starts_with('>') && !trimmed.starts_with(">=");
    if let Some(candidate) = lower_bound_version(trimmed)
        && let Ok(mut v) = node_semver::Version::parse(&candidate)
    {
        if exclusive {
            // Probe just above the exclusive boundary: `>3.0.5` covers
            // every version above 3.0.5, so use 3.0.5 + minimum patch
            // bump as the representative point.
            v.patch += 1;
        }
        return v.satisfies(&r);
    }
    true
}

fn lower_bound_version(range: &str) -> Option<String> {
    let s = range
        .trim()
        .trim_start_matches(['^', '~', '=', '>', 'v', ' ']);
    let end = s.find([' ', ',', '<', '|', '>']).unwrap_or(s.len());
    let v = &s[..end];
    if v.is_empty() || !v.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn bare_name_matches_any_spec() {
        let rules = compile(&map(&[("lodash", "4.17.21")]));
        assert_eq!(apply(&rules, "lodash", "^4.17.0"), Some("4.17.21"));
        assert_eq!(apply(&rules, "lodash", "*"), Some("4.17.21"));
        assert_eq!(apply(&rules, "other", "^1"), None);
    }

    #[test]
    fn scoped_bare_name() {
        let rules = compile(&map(&[("@babel/core", "7.20.0")]));
        assert_eq!(apply(&rules, "@babel/core", "^7"), Some("7.20.0"));
    }

    #[test]
    fn version_qualified_filters_by_range() {
        let rules = compile(&map(&[("plist@<3.0.5", ">=3.0.5")]));
        assert_eq!(apply(&rules, "plist", "^3.0.4"), Some(">=3.0.5"));
        assert_eq!(apply(&rules, "plist", "^4.0.0"), None);
    }

    #[test]
    fn scoped_with_range() {
        let rules = compile(&map(&[("@scope/pkg@^1", "1.5.0")]));
        assert_eq!(apply(&rules, "@scope/pkg", "^1.0.0"), Some("1.5.0"));
        assert_eq!(apply(&rules, "@scope/pkg", "^2.0.0"), None);
    }

    #[test]
    fn parent_chain_keys_dropped() {
        let rules = compile(&map(&[
            ("foo>bar", "1.0.0"),
            ("**/foo", "1.0.0"),
            ("parent/foo", "1.0.0"),
        ]));
        assert!(rules.is_empty());
    }

    #[test]
    fn empty_or_malformed_keys_dropped() {
        let rules = compile(&map(&[
            ("", "1"),
            ("@scope", "1"),
            ("foo@", "1"),
            ("@", "1"),
        ]));
        assert!(rules.is_empty());
    }

    #[test]
    fn version_keyed_rule_wins_over_bare_when_both_match() {
        // pnpm picks the more specific selector, so a manifest with
        // both `"plist": "9.9.9"` and `"plist@<3": "2.0.0"` and a dep
        // spec of `^2.0.0` (covered by `<3`) should resolve to
        // `2.0.0`. Bare-first iteration order would silently shadow
        // the version-keyed rule and produce a false `Stale`.
        let rules = compile(&map(&[("plist", "9.9.9"), ("plist@<3", "2.0.0")]));
        assert_eq!(apply(&rules, "plist", "^2.0.0"), Some("2.0.0"));
        // Spec outside the version-keyed rule's range falls through to
        // the bare rule.
        assert_eq!(apply(&rules, "plist", "^4.0.0"), Some("9.9.9"));
    }

    #[test]
    fn key_with_gte_comparator_parses() {
        // `lodash@>=4.17.21` is a single segment whose `>=` is a
        // comparator, not a chain separator. Pre-fix, the parser
        // rejected any key containing `>` and silently dropped this.
        let rules = compile(&map(&[("lodash@>=4.17.21", "4.18.0")]));
        assert_eq!(apply(&rules, "lodash", "4.17.21"), Some("4.18.0"));
        // Lower-bound probe is conservative — concrete version that
        // doesn't satisfy the req falls through, which is fine.
        assert_eq!(apply(&rules, "lodash", "4.0.0"), None);
    }

    #[test]
    fn key_with_gt_comparator_parses() {
        let rules = compile(&map(&[("lodash@>1.0.0", "1.5.0")]));
        assert_eq!(apply(&rules, "lodash", "1.2.0"), Some("1.5.0"));
    }

    #[test]
    fn exclusive_gt_spec_against_lt_req_does_not_overlap() {
        // `>3.0.5` and `<3.0.5` have empty intersection. A naive
        // lower-bound probe would extract `3.0.5` from `>3.0.5` and
        // see it fail `<3.0.5` (boundary excluded), then fall through
        // to "probably matches". The fix is to bump the candidate
        // above the exclusive boundary before satisfaction-probing.
        assert!(!range_could_satisfy(">3.0.5", "<3.0.5"));
        // Sanity: the inclusive case still doesn't overlap.
        assert!(range_could_satisfy(">3.0.5", ">=3.0.5"));
    }
}
