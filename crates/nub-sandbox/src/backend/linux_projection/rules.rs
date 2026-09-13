use std::io;
use std::path::Path;

use globset::GlobMatcher;

use crate::matcher::path::compile_glob;
use crate::matcher::path::PathMatcher;
use crate::policy::{Effect, FsAccess, FsRuleSet};

// A policy can be authored with brace alternatives that cross a directory
// boundary. `globset` deliberately keeps its parsed token stream private, so
// expand only that syntactic construct before deriving the search-only parent
// patterns. Keeping the expansion bounded makes compilation proportional to a
// policy-sized input rather than permitting a crafted sequence of braces to
// allocate without limit.
const MAX_TRAVERSAL_ALTERNATIVES: usize = 1024;

pub(super) struct Rules {
    matcher: PathMatcher,
    traversal: Vec<GlobMatcher>,
}

impl Rules {
    pub(super) fn compile(set: &FsRuleSet) -> io::Result<Self> {
        let mut traversal = Vec::new();
        for rule in &set.entries {
            let pattern = rule.matcher.as_str();
            // Public filesystem compilation produces a positive union. Do not
            // reinterpret legacy deny rules, malformed globs, or unresolved roots.
            if rule.effect != Effect::Allow || !pattern.starts_with('/') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "projection requires resolved absolute positive filesystem grants",
                ));
            }
            // Validate with the authority matcher first. The traversal compiler
            // must accept precisely its grammar, including its platform flags.
            compile_glob(pattern).map_err(io::Error::other)?;
            for expanded in expand_braces(pattern)? {
                for prefix in ancestor_prefixes(&expanded) {
                    traversal.push(compile_glob(prefix).map_err(io::Error::other)?);
                }
            }
        }
        Ok(Self {
            matcher: PathMatcher::new(set),
            traversal,
        })
    }

    pub(super) fn access(&self, path: &Path) -> Option<FsAccess> {
        let decision = self.matcher.decide_verified_path(path);
        (decision.effect == Effect::Allow).then_some(decision.access)
    }

    pub(super) fn traversable(&self, path: &Path) -> bool {
        path == Path::new("/")
            || self.access(path).is_some()
            || self.traversal.iter().any(|glob| glob.is_match(path))
    }
}

/// Expands globset's brace alternates while leaving every other glob token
/// intact for `compile_glob`. Braces inside a class and escaped braces are
/// literals, just as they are in globset's parser.
fn expand_braces(pattern: &str) -> io::Result<Vec<String>> {
    let Some((open, close)) = outer_brace(pattern) else {
        return Ok(vec![pattern.to_owned()]);
    };
    let before = &pattern[..open];
    let after = &pattern[close + 1..];
    let mut alternatives = split_alternatives(&pattern[open + 1..close]);
    // globset's default drops empty alternate branches. It does retain an
    // entirely empty group, which is equivalent to an empty substitution.
    if alternatives
        .iter()
        .any(|alternative| !alternative.is_empty())
    {
        alternatives.retain(|alternative| !alternative.is_empty());
    }

    let mut expanded = Vec::new();
    for alternative in alternatives {
        let mut joined = String::with_capacity(before.len() + alternative.len() + after.len());
        joined.push_str(before);
        joined.push_str(alternative);
        joined.push_str(after);
        for expansion in expand_braces(&joined)? {
            if expanded.len() == MAX_TRAVERSAL_ALTERNATIVES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "projection traversal has too many brace alternatives",
                ));
            }
            expanded.push(expansion);
        }
    }
    Ok(expanded)
}

/// Finds the first brace pair outside a character class and outside escaping.
/// Full syntax validation happens through `compile_glob` before this helper is
/// called, so this only needs to reproduce the lexical boundaries.
fn outer_brace(pattern: &str) -> Option<(usize, usize)> {
    let bytes = pattern.as_bytes();
    let mut escaped = false;
    let mut class = false;
    let mut class_first = false;
    let mut depth = 0;
    let mut open = None;
    for (index, &byte) in bytes.iter().enumerate() {
        // globset's class parser does not honor backslash escaping. Handle the
        // class before the outer lexer so `\\]` closes a class exactly as it
        // does there.
        if class {
            if byte == b']' && !class_first {
                class = false;
            } else {
                class_first = false;
            }
            continue;
        }
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            continue;
        }
        match byte {
            b'[' => {
                class = true;
                class_first = true;
            }
            b'{' => {
                if depth == 0 {
                    open = Some(index);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return open.map(|start| (start, index));
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits a brace group's top-level alternatives without treating nested
/// braces, classes, or escaped commas as separators.
fn split_alternatives(group: &str) -> Vec<&str> {
    let bytes = group.as_bytes();
    let mut alternatives = Vec::new();
    let mut start = 0;
    let mut escaped = false;
    let mut class = false;
    let mut class_first = false;
    let mut depth = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        if class {
            if byte == b']' && !class_first {
                class = false;
            } else {
                class_first = false;
            }
            continue;
        }
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            continue;
        }
        match byte {
            b'[' => {
                class = true;
                class_first = true;
            }
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b',' if depth == 0 => {
                alternatives.push(&group[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    alternatives.push(&group[start..]);
    alternatives
}

/// Returns valid glob prefixes ending at physical path separators. A slash in
/// a character class is not a usable filesystem-name boundary; an escaped
/// slash is, but its escape is omitted from the preceding prefix so it remains
/// a valid glob.
fn ancestor_prefixes(pattern: &str) -> impl Iterator<Item = &str> {
    let bytes = pattern.as_bytes();
    let mut prefixes = Vec::new();
    let mut escaped = false;
    let mut class = false;
    let mut class_first = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if class {
            if byte == b']' && !class_first {
                class = false;
            } else {
                class_first = false;
            }
            continue;
        }
        if escaped {
            if byte == b'/' && index > 1 {
                prefixes.push(&pattern[..index - 1]);
            }
            escaped = false;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            continue;
        }
        match byte {
            b'[' => {
                class = true;
                class_first = true;
            }
            b'/' if index != 0 => prefixes.push(&pattern[..index]),
            _ => {}
        }
    }
    prefixes.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{CanonGlob, FsOrigin, FsRule};

    pub(super) fn set(pattern: &str, access: FsAccess) -> FsRuleSet {
        FsRuleSet {
            entries: vec![FsRule {
                matcher: CanonGlob(pattern.into()),
                effect: Effect::Allow,
                access,
                origin: FsOrigin::Authored,
            }],
            default_effect: Effect::Deny,
        }
    }

    #[test]
    fn future_embedded_patterns_have_traversal_but_not_sibling_authority() {
        let rules = Rules::compile(&set("/app/pkg-*/lib/**/*.so", FsAccess::ReadWrite)).unwrap();
        for path in [
            "/",
            "/app",
            "/app/pkg-new",
            "/app/pkg-new/lib",
            "/app/pkg-new/lib/deep",
        ] {
            assert!(rules.traversable(Path::new(path)), "{path}");
            assert_eq!(rules.access(Path::new(path)), None, "{path}");
        }
        assert_eq!(
            rules.access(Path::new("/app/pkg-new/lib/deep/future.so")),
            Some(FsAccess::ReadWrite)
        );
        assert_eq!(
            rules.access(Path::new("/app/pkg-new/lib/deep/future.so-near")),
            None
        );
        assert!(!rules.traversable(Path::new("/app/pkg-new-nearby/wrong")));
    }

    #[test]
    fn brace_alternatives_crossing_directories_only_grant_search() {
        let rules = Rules::compile(&set("/app/{a/{b,c},d/e}/file", FsAccess::ReadWrite)).unwrap();
        for path in [
            "/", "/app", "/app/a", "/app/a/b", "/app/a/c", "/app/d", "/app/d/e",
        ] {
            assert!(rules.traversable(Path::new(path)), "{path}");
            assert_eq!(rules.access(Path::new(path)), None, "{path}");
        }
        assert_eq!(
            rules.access(Path::new("/app/a/b/file")),
            Some(FsAccess::ReadWrite)
        );
        assert_eq!(
            rules.access(Path::new("/app/d/e/file")),
            Some(FsAccess::ReadWrite)
        );
        for path in ["/app/x", "/app/a/no", "/app/a/b/sibling"] {
            assert!(!rules.traversable(Path::new(path)), "{path}");
            assert_eq!(rules.access(Path::new(path)), None, "{path}");
        }
    }

    #[test]
    fn class_slashes_do_not_create_invalid_prefix_globs() {
        let rules = Rules::compile(&set("/app/[a/b]/file", FsAccess::Read)).unwrap();
        assert!(rules.traversable(Path::new("/app")));
        assert!(rules.traversable(Path::new("/app/a")));
        assert_eq!(rules.access(Path::new("/app/a")), None);
        assert!(!rules.traversable(Path::new("/app/z")));
        assert_eq!(rules.access(Path::new("/app/a/file")), Some(FsAccess::Read));
    }

    #[test]
    fn escaped_literals_and_unix_bytes_keep_compile_glob_semantics() {
        let literal = Rules::compile(&set(r"/app/\{literal\}/file", FsAccess::Read)).unwrap();
        assert!(literal.traversable(Path::new("/app/{literal}")));
        assert_eq!(literal.access(Path::new("/app/{literal}")), None);
        assert_eq!(
            literal.access(Path::new("/app/{literal}/file")),
            Some(FsAccess::Read)
        );

        let escaped_separator =
            Rules::compile(&set(r"/app/foo\/bar/file", FsAccess::Read)).unwrap();
        assert!(escaped_separator.traversable(Path::new("/app/foo")));
        assert!(escaped_separator.traversable(Path::new("/app/foo/bar")));
        assert_eq!(escaped_separator.access(Path::new("/app/foo/bar")), None);

        #[cfg(target_os = "linux")]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            let wildcard = Rules::compile(&set("/app/*/file", FsAccess::Read)).unwrap();
            let bad_name = Path::new("/app").join(OsString::from_vec(vec![0xFF]));
            assert!(wildcard.traversable(&bad_name));
            assert_eq!(wildcard.access(&bad_name), None);
        }
    }

    #[test]
    fn read_grants_do_not_subtract_write_and_invalid_rules_fail_acquisition() {
        let mut set = set("/app/**", FsAccess::ReadWrite);
        let mut read = set.entries[0].clone();
        read.access = FsAccess::Read;
        set.entries.push(read);
        assert_eq!(
            Rules::compile(&set).unwrap().access(Path::new("/app/new")),
            Some(FsAccess::ReadWrite)
        );
        set.entries[1].effect = Effect::Deny;
        assert!(Rules::compile(&set).is_err());
        set.entries[1].effect = Effect::Allow;
        set.entries[1].matcher = CanonGlob("/app/[".into());
        assert!(Rules::compile(&set).is_err());
    }

    #[test]
    fn explicitly_unconstrained_filesystem_preserves_write_authority() {
        let rules = Rules::compile(&FsRuleSet {
            entries: Vec::new(),
            default_effect: Effect::Allow,
        })
        .unwrap();
        assert_eq!(
            rules.access(Path::new("/future/nested/file")),
            Some(FsAccess::ReadWrite)
        );
        assert!(rules.traversable(Path::new("/future/nested")));
    }
}
