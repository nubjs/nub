pub fn is_workspace_spec(spec: &str) -> bool {
    spec.starts_with("workspace:")
}

pub fn is_catalog_spec(spec: &str) -> bool {
    spec.starts_with("catalog:")
}

pub fn is_npm_spec(spec: &str) -> bool {
    spec.starts_with("npm:")
}

pub fn is_jsr_spec(spec: &str) -> bool {
    spec.starts_with("jsr:")
}

pub fn is_file_spec(spec: &str) -> bool {
    spec.starts_with("file:")
}

pub fn is_link_spec(spec: &str) -> bool {
    spec.starts_with("link:")
}

/// A `workspace:` specifier, classified by which member it names and
/// how. See [`parse_workspace_spec`].
#[derive(Debug, PartialEq, Eq)]
pub enum WorkspaceSpec<'a> {
    /// `workspace:<name>@<range>` — binds to the member `name`, which is
    /// independent of the dependency key. This is the alias form.
    Alias { name: &'a str, range: &'a str },
    /// `workspace:.<path>` / `workspace:..<path>` — binds to whatever
    /// package sits at that importer-relative directory.
    Path(&'a str),
    /// `workspace:<range>` — binds to the member the dependency key
    /// names. The plain, overwhelmingly common form.
    Range(&'a str),
}

/// Classify a `workspace:` specifier. `None` when `spec` does not carry
/// the protocol at all.
///
/// The tail after `workspace:` is one of three things, and telling them
/// apart is the whole job. pnpm settles it with a single regex plus one
/// escape hatch, and we mirror both so a manifest that installs there
/// installs here:
///
/// ```text
/// /^workspace:(?:(?<alias>[^._/][^@]*)@)?(?<version>.*)$/   spec-parser/src/index.ts
/// if (bareSpecifier.startsWith('workspace:.')) return null   npm-resolver/src/index.ts
/// ```
///
/// Three consequences worth stating, because each is a case that looks
/// like another:
///
/// * The `@` is REQUIRED to name a member. `workspace:pkg2` is a
///   *version* (the `pkg2` dist-tag), not an alias for member `pkg2` —
///   real pnpm rejects it with `no package named "<key>" is present in
///   the workspace` when the KEY is not a member.
/// * A leading `.` is the entire path test. `workspace:packages/pkg2` is
///   NOT a path; pnpm calls it `Invalid workspace: spec`. Bun and Yarn
///   v1 do read a bare directory as a locator, so the resolver keeps
///   accepting that shape when the dependency key already names the
///   member — this parser just does not classify it as a path.
/// * A scoped target survives because `@` is not in the excluded set:
///   `workspace:@acme/x@*` splits at the SECOND `@`.
pub fn parse_workspace_spec(spec: &str) -> Option<WorkspaceSpec<'_>> {
    let tail = spec.strip_prefix("workspace:")?;
    if tail.starts_with('.') {
        return Some(WorkspaceSpec::Path(tail));
    }
    // The alias part is `[^._/][^@]*` followed by `@`, so the split point
    // is the first `@` at index >= 1 — never index 0, which is what keeps
    // a scoped `@acme/x@*` splitting at its second `@`.
    if let Some(first) = tail.chars().next()
        && !matches!(first, '.' | '_' | '/')
        && let Some(rel) = tail[first.len_utf8()..].find('@')
    {
        let at = first.len_utf8() + rel;
        return Some(WorkspaceSpec::Alias {
            name: &tail[..at],
            range: &tail[at + 1..],
        });
    }
    Some(WorkspaceSpec::Range(tail))
}

/// The member a `workspace:` spec names, when it names one that the
/// dependency key does not. `None` for the plain and path forms, whose
/// member comes from the key or the directory instead.
pub fn workspace_alias_target(spec: &str) -> Option<&str> {
    match parse_workspace_spec(spec) {
        Some(WorkspaceSpec::Alias { name, .. }) => Some(name),
        _ => None,
    }
}

pub fn split_name_spec(input: &str) -> (&str, Option<&str>) {
    if let Some(rest) = input.strip_prefix('@') {
        if let Some(slash) = rest.find('/') {
            let after_slash = &rest[slash + 1..];
            if let Some(at) = after_slash.find('@') {
                let name_end = 1 + slash + 1 + at;
                return (&input[..name_end], Some(&input[name_end + 1..]));
            }
        }
        return (input, None);
    }
    if let Some(at) = input.find('@') {
        return (&input[..at], Some(&input[at + 1..]));
    }
    (input, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_catalog_detect() {
        assert!(is_workspace_spec("workspace:*"));
        assert!(is_workspace_spec("workspace:^"));
        assert!(!is_workspace_spec("^1.0.0"));
        assert!(is_catalog_spec("catalog:"));
        assert!(is_catalog_spec("catalog:default"));
        assert!(!is_catalog_spec("cat:foo"));
    }

    fn alias(name: &'static str, range: &'static str) -> WorkspaceSpec<'static> {
        WorkspaceSpec::Alias { name, range }
    }

    #[test]
    fn classifies_every_workspace_tail_shape() {
        let cases: &[(&str, WorkspaceSpec<'_>)] = &[
            // The alias form needs the `@`; the split is the FIRST `@`
            // at index >= 1.
            ("workspace:pkg2@*", alias("pkg2", "*")),
            ("workspace:pkg2@^1.0.0", alias("pkg2", "^1.0.0")),
            ("workspace:@acme/x@*", alias("@acme/x", "*")),
            ("workspace:pkg2@", alias("pkg2", "")),
            // No `@` means the tail is a version, even when it looks like
            // a package name — the case real pnpm rejects when the key is
            // not a member.
            ("workspace:pkg2", WorkspaceSpec::Range("pkg2")),
            (
                "workspace:packages/pkg2",
                WorkspaceSpec::Range("packages/pkg2"),
            ),
            ("workspace:*", WorkspaceSpec::Range("*")),
            ("workspace:^", WorkspaceSpec::Range("^")),
            ("workspace:", WorkspaceSpec::Range("")),
            ("workspace:^2.0.0", WorkspaceSpec::Range("^2.0.0")),
            // A leading `.` is the whole path test.
            ("workspace:../pkg2", WorkspaceSpec::Path("../pkg2")),
            ("workspace:./pkg2", WorkspaceSpec::Path("./pkg2")),
            // The excluded first-character set cannot open an alias.
            ("workspace:_priv@1", WorkspaceSpec::Range("_priv@1")),
            ("workspace:/abs@1", WorkspaceSpec::Range("/abs@1")),
        ];
        for (spec, want) in cases {
            assert_eq!(
                parse_workspace_spec(spec).as_ref(),
                Some(want),
                "{spec} classified wrongly"
            );
        }
        assert!(
            parse_workspace_spec("^1.0.0").is_none(),
            "a spec without the protocol prefix is not a workspace spec"
        );
        assert_eq!(workspace_alias_target("workspace:pkg2@*"), Some("pkg2"));
        assert_eq!(workspace_alias_target("workspace:*"), None);
        assert_eq!(workspace_alias_target("workspace:../pkg2"), None);
    }

    #[test]
    fn split_plain_name() {
        assert_eq!(split_name_spec("lodash"), ("lodash", None));
        assert_eq!(split_name_spec("lodash@4.17.0"), ("lodash", Some("4.17.0")));
    }

    #[test]
    fn split_scoped_name() {
        assert_eq!(split_name_spec("@babel/core"), ("@babel/core", None));
        assert_eq!(
            split_name_spec("@babel/core@7.0.0"),
            ("@babel/core", Some("7.0.0"))
        );
    }

    #[test]
    fn split_empty_version_kept_as_some() {
        assert_eq!(split_name_spec("lodash@"), ("lodash", Some("")));
    }
}
