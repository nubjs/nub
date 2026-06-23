use crate::LockedPackage;
use std::collections::BTreeMap;

pub(super) fn version_to_dep_path(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

pub(super) fn dep_path_tail<'a>(dep_path: &'a str, name: &str) -> &'a str {
    dep_path
        .strip_prefix(&format!("{name}@"))
        .unwrap_or(dep_path)
}

pub(super) fn peerless_dep_path(name: &str, value: &str) -> String {
    version_to_dep_path(name, value.split('(').next().unwrap_or(value))
}

/// Split a dep_path tail's peer suffix into outer-level paren segments,
/// each returned *with* its enclosing parens. `react-dom@18.2.0(react@18.2.0)`
/// yields `["(react@18.2.0)"]`; a nested form
/// `consumer@1.0.0(react-dom@18.2.0(react@18.2.0))` yields the single outer
/// segment `["(react-dom@18.2.0(react@18.2.0))"]` with the inner segment
/// preserved verbatim. The canonical `name@version` head (everything before
/// the first `(`) is skipped.
///
/// Mirrors the resolver's identically-named helper in
/// `aube-resolver/src/peer_context.rs`; duplicated here because the two
/// crates can't share it without a public surface neither wants to commit
/// to. Any change to the paren-balancing algorithm (a new peer-suffix
/// shape, percent-encoded git URLs, …) must be applied to both copies.
fn outer_paren_segments(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut segments = Vec::new();
    let mut i = 0;
    while i < bytes.len() && bytes[i] != b'(' {
        i += 1;
    }
    while i < bytes.len() {
        if bytes[i] != b'(' {
            i += 1;
            continue;
        }
        let start = i;
        let mut depth: i32 = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        segments.push(&s[start..i]);
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            break;
        }
    }
    segments
}

/// Rewrite the peer-suffix references of a dep_path (or a dep *value*,
/// which is a headless `version{suffix}`) by passing each peer
/// reference's flat `name@version` head through `translate`. The head
/// before the first `(` is left untouched — only the parenthesized peer
/// references are rewritten, and nested suffixes recurse.
///
/// `translate(head)` returns `Some(new_head)` to replace a *flat* peer
/// reference, or `None` to keep it verbatim. The two boundary passes use
/// it in opposite directions:
///
/// * writer: hashed `request@url+<hash>` → spec `request@https://…/tar.gz/<sha>`
///   (a `graph.packages` lookup yields the target's `local_source.specifier()`).
/// * reader: spec → hashed (`shared_local_dep_path` re-derives the dep_path),
///   keeping the in-memory graph FS-safe so a round-trip matches a fresh
///   resolve byte-for-byte.
///
/// Registry peers (`react@18.2.0`) translate to `None` and pass through
/// unchanged, so this is a no-op on graphs with no git / tarball peers.
pub(super) fn rewrite_peer_suffix(s: &str, translate: &impl Fn(&str) -> Option<String>) -> String {
    let Some(head_end) = s.find('(') else {
        return s.to_string();
    };
    let segments = outer_paren_segments(&s[head_end..]);
    if segments.is_empty() {
        // A `(` with no balanced segment is a malformed dep_path tail
        // (truncated / hand-corrupted lockfile). Preserve the bytes
        // verbatim instead of silently dropping everything after the `(`.
        warn_unbalanced_peer_suffix(s);
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..head_end]);
    for seg in segments {
        out.push('(');
        out.push_str(&rewrite_peer_reference(&seg[1..seg.len() - 1], translate));
        out.push(')');
    }
    out
}

/// Surface a dep_path tail / peer reference that opens a `(` it never
/// closes — a truncated or hand-corrupted lockfile entry that
/// [`outer_paren_segments`] returns no segment for. The callers preserve
/// the bytes verbatim; this warning keeps the corruption diagnosable
/// instead of letting the shortened key mis-resolve on the next install.
fn warn_unbalanced_peer_suffix(s: &str) {
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_LOCKFILE_MALFORMED_PEER_SUFFIX,
        dep_path = s,
        "unbalanced parentheses in pnpm dep_path peer suffix; preserving the key verbatim"
    );
}

/// Rewrite a single peer reference (the contents between one pair of
/// suffix parens). A flat reference (`request@url+<hash>`) is handed to
/// `translate`; a reference that carries its own nested suffix
/// (`request-promise@4.2.6(request@url+<hash>)`) keeps its head and
/// recurses, so the nested git / tarball peer is still translated.
fn rewrite_peer_reference(inner: &str, translate: &impl Fn(&str) -> Option<String>) -> String {
    let Some(nested_start) = inner.find('(') else {
        return translate(inner).unwrap_or_else(|| inner.to_string());
    };
    let segments = outer_paren_segments(&inner[nested_start..]);
    if segments.is_empty() {
        warn_unbalanced_peer_suffix(inner);
        return inner.to_string();
    }
    let mut out = String::with_capacity(inner.len());
    out.push_str(&inner[..nested_start]);
    for seg in segments {
        out.push('(');
        out.push_str(&rewrite_peer_reference(&seg[1..seg.len() - 1], translate));
        out.push(')');
    }
    out
}

pub(super) fn peerless_alias_target<'a>(
    packages: &'a BTreeMap<String, LockedPackage>,
    real_dep_path: &str,
) -> Option<&'a LockedPackage> {
    let (real_name, real_version) = parse_dep_path(real_dep_path)?;
    packages.get(&version_to_dep_path(&real_name, &real_version))
}

/// Parse a dep path like "@scope/name@1.0.0" or "name@1.0.0" into (name, version).
pub(super) fn parse_dep_path(dep_path: &str) -> Option<(String, String)> {
    // Strip leading "/" if present (pnpm v6-v8 format)
    let s = dep_path.strip_prefix('/').unwrap_or(dep_path);

    // Find the last '@' that separates name from version
    let at_idx = if s.starts_with('@') {
        // Scoped package: find '@' after the first '/'
        let after_scope = s.find('/')? + 1;
        after_scope + s[after_scope..].find('@')?
    } else {
        s.find('@')?
    };

    let name = s[..at_idx].to_string();
    let version_str = &s[at_idx + 1..];

    // Strip any peer suffix from version (e.g., "1.0.0(react@18.0.0)" -> "1.0.0")
    let version = version_str
        .split('(')
        .next()
        .unwrap_or(version_str)
        .to_string();

    Some((name, version))
}

/// Detect npm-aliased entries inside a snapshot's `dependencies` /
/// `optionalDependencies` map and rewrite them to aube's internal shape.
///
/// pnpm encodes a transitive npm alias as `<alias>: <real>@<resolved>(peers…)`
/// (e.g. `@isaacs/cliui@8.0.2` records `string-width-cjs: string-width@4.2.3`
/// for its `"string-width-cjs": "npm:string-width@^4.2.0"` dep). Aube's
/// linker keys sibling symlinks against `<dep_name>@<dep_value>`, so a raw
/// pnpm value yields a broken `string-width-cjs@string-width@4.2.3` virtual
/// store path. This helper rewrites the value to the bare resolved version
/// (preserving any peer-context suffix) and pushes onto `alias_remaps` so
/// the synthesis loop creates a `<alias>@<resolved>` `LockedPackage` with
/// `alias_of=Some(real)`. After that the linker resolves the alias symlink
/// to the synthetic dir and the resolver's lockfile-reuse path enqueues
/// transitives with `range = <resolved>` (not the malformed
/// `<real>@<resolved>` that no `<alias>` packument can satisfy).
pub(super) fn rewrite_snapshot_alias_deps(
    deps: &mut BTreeMap<String, String>,
    alias_remaps: &mut Vec<(String, String, String, String)>,
) {
    for (dep_name, dep_value) in deps.iter_mut() {
        let bare = dep_value.split('(').next().unwrap_or(dep_value);
        let Some((real_name, resolved)) = parse_dep_path(bare) else {
            continue;
        };
        if real_name == *dep_name {
            continue;
        }
        let peer_suffix = dep_value.find('(').map(|i| &dep_value[i..]).unwrap_or("");
        let alias_dep_path = format!("{dep_name}@{resolved}{peer_suffix}");
        let real_dep_path = dep_value.clone();
        alias_remaps.push((alias_dep_path, real_dep_path, dep_name.clone(), real_name));
        *dep_value = format!("{resolved}{peer_suffix}");
    }
}

#[cfg(test)]
mod rewrite_peer_suffix_tests {
    use super::rewrite_peer_suffix;

    // Stand-in for the writer's `graph.packages` lookup / reader's
    // `shared_local_dep_path`: only the one git/tarball peer translates.
    fn translate(head: &str) -> Option<String> {
        (head == "request@url+1ff5271859b51655")
            .then(|| "request@https://codeload.github.com/owner/request/tar.gz/abc".to_string())
    }

    #[test]
    fn no_suffix_is_unchanged() {
        assert_eq!(
            rewrite_peer_suffix("lodash@4.18.1", &translate),
            "lodash@4.18.1"
        );
        // A headless dep value with no suffix passes through too.
        assert_eq!(rewrite_peer_suffix("4.18.1", &translate), "4.18.1");
    }

    #[test]
    fn flat_local_peer_renders_as_spec() {
        assert_eq!(
            rewrite_peer_suffix(
                "request-promise-core@1.1.4(request@url+1ff5271859b51655)",
                &translate
            ),
            "request-promise-core@1.1.4(request@https://codeload.github.com/owner/request/tar.gz/abc)"
        );
        // Headless dep-value form is rewritten identically.
        assert_eq!(
            rewrite_peer_suffix("1.1.4(request@url+1ff5271859b51655)", &translate),
            "1.1.4(request@https://codeload.github.com/owner/request/tar.gz/abc)"
        );
    }

    #[test]
    fn registry_peer_is_left_untouched() {
        // No translation entry → verbatim, so this is a no-op for graphs
        // with only registry peers.
        let s = "react-dom@18.2.0(react@18.2.0)";
        assert_eq!(rewrite_peer_suffix(s, &translate), s);
    }

    #[test]
    fn nested_suffix_translates_only_the_inner_local_peer() {
        // A registry peer (`request-promise`) keeps its head; the nested
        // git/tarball peer inside it still renders as the spec.
        assert_eq!(
            rewrite_peer_suffix(
                "consumer@1.0.0(request-promise@4.2.6(request@url+1ff5271859b51655))",
                &translate
            ),
            "consumer@1.0.0(request-promise@4.2.6(request@https://codeload.github.com/owner/request/tar.gz/abc))"
        );
    }

    #[test]
    fn multiple_segments_each_handled_independently() {
        assert_eq!(
            rewrite_peer_suffix(
                "pkg@1.0.0(react@18.2.0)(request@url+1ff5271859b51655)",
                &translate
            ),
            "pkg@1.0.0(react@18.2.0)(request@https://codeload.github.com/owner/request/tar.gz/abc)"
        );
    }

    #[test]
    fn unbalanced_parens_preserved_verbatim_not_dropped() {
        // A truncated / hand-corrupted tail (open `(`, no matching close)
        // must be preserved byte-for-byte. Silently dropping the suffix
        // would shorten the key and mis-resolve it against a different
        // package. A single unclosed paren:
        let flat = "request-promise-core@1.1.4(request@bad";
        assert_eq!(rewrite_peer_suffix(flat, &translate), flat);
        // A nested open paren with one missing close is unbalanced too:
        let nested = "consumer@1.0.0(request-promise@4.2.6(request@bad)";
        assert_eq!(rewrite_peer_suffix(nested, &translate), nested);
    }
}
