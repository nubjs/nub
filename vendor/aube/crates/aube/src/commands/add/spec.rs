use miette::miette;

/// Parsed result of a package spec like "lodash@^4" or "my-alias@npm:real-pkg@^2".
#[cfg_attr(test, derive(Debug))]
pub(super) struct ParsedPkgSpec {
    /// The name to use in package.json (alias if provided, otherwise the real name)
    pub(super) alias: Option<String>,
    /// The real package name on the registry
    pub(super) name: String,
    /// For `jsr:` specs, the JSR-style name (e.g. `@std/collections`).
    /// `name` has already been translated to the npm-compat form
    /// (`@jsr/std__collections`) so the registry fetch hits
    /// <https://npm.jsr.io>; we keep the original around so the
    /// manifest-write path can round-trip `jsr:…` back into
    /// `package.json`. `None` for non-jsr specs.
    pub(super) jsr_name: Option<String>,
    /// The version range
    pub(super) range: String,
    /// `true` when the user wrote an explicit `@<range>` (e.g. `lodash@latest`,
    /// `lodash@^4`). `false` when no version was given and the range was
    /// defaulted to `"latest"` by the parser. Used to decide whether the
    /// configured `tag` setting should override the range.
    pub(super) has_explicit_range: bool,
    /// Original verbatim spec when the user typed a git URL form
    /// (`kevva/is-negative`, `github:user/repo`, `git+https://…`, …).
    /// `Some(_)` flags the spec for the non-registry git branch:
    /// skip the packument fetch, write the verbatim string into
    /// `package.json`, and let the resolver dispatch the git path.
    pub(super) git_spec: Option<String>,
    /// Original verbatim spec when the user typed a `file:` / `link:`
    /// path form (`file:./pkg`, `link:../sibling`, `file:./bundle.tgz`).
    /// `Some(_)` flags the spec for the non-registry local branch:
    /// skip the packument fetch, write the verbatim string into
    /// `package.json`, and let the resolver dispatch the local path.
    pub(super) local_spec: Option<String>,
    /// Set when `linkWorkspacePackages=true` matched a local sibling
    /// for this spec. The string is the sibling's `package.json#version`
    /// (or `"0.0.0"` when the sibling has no version). The packument
    /// fetch is skipped and the manifest-write path branches on
    /// `saveWorkspaceProtocol` to choose between rolling
    /// (`workspace:^`), pinned (`workspace:^<version>`), or
    /// registry-style (`^<version>`) — the resolver picks up the
    /// sibling either way because aube already prefers workspace
    /// matches on bare semver ranges.
    pub(super) linked_workspace_version: Option<String>,
}

/// Parse a package spec into its components.
///
/// Supported forms:
/// - `lodash` → name=lodash, range=latest
/// - `lodash@^4` → name=lodash, range=^4
/// - `@scope/pkg@latest` → name=@scope/pkg, range=latest
/// - `npm:real-pkg@^4` → name=real-pkg, range=^4 (no alias)
/// - `my-alias@npm:real-pkg@^4` → alias=my-alias, name=real-pkg, range=^4
/// - `jsr:@std/collections@^1` → alias=@std/collections,
///   name=@jsr/std__collections, range=^1 (jsr translation)
/// - `my-alias@jsr:@std/collections@^1` → alias=my-alias,
///   name=@jsr/std__collections, range=^1
/// - `kevva/is-negative` → git: bare GitHub shorthand, name derived
///   from the repo segment of the clone URL
/// - `github:user/repo`, `git+https://host/u/r.git#tag` → git: any
///   form `aube_lockfile::parse_git_spec` recognizes
/// - `my-alias@kevva/is-negative` → git with alias: manifest key is
///   `my-alias`, spec written verbatim
/// - `file:./pkg`, `link:../sibling`, `file:./bundle.tgz` → local:
///   manifest key derived from the path basename (alias overrides)
/// - `my-alias@file:./pkg`, `my-alias@link:../sibling` → local with
///   alias: manifest key is `my-alias`, spec written verbatim
pub(super) fn parse_pkg_spec(spec: &str) -> miette::Result<ParsedPkgSpec> {
    // Git specs route through their own branch — packument fetch is
    // skipped and the verbatim spec is written to `package.json`.
    // Try the full string first so explicit URL forms shadow the
    // alias check below; then peel a leading `alias@` and re-test.
    if aube_lockfile::parse_git_spec(spec).is_some() {
        return parse_git_pkg_spec(spec, None);
    }
    if let Some((alias, rest)) = split_git_alias(spec)
        && aube_lockfile::parse_git_spec(rest).is_some()
    {
        return parse_git_pkg_spec(rest, Some(alias.to_string()));
    }
    // Scoped alias form `@scope/alias@<git-or-local-spec>` — pnpm
    // supports this for both git and local specs. Routed before the
    // jsr/npm/scoped-name branches below so a scoped name with a
    // git/local tail isn't misclassified as a registry fetch.
    if let Some((alias, rest)) = split_scoped_alias(spec) {
        if aube_lockfile::parse_git_spec(rest).is_some() {
            return parse_git_pkg_spec(rest, Some(alias.to_string()));
        }
        if is_local_path_spec(rest) {
            return parse_local_pkg_spec(rest, Some(alias.to_string()));
        }
    }
    // Local path specs use the same skip-packument routing.
    if is_local_path_spec(spec) {
        return parse_local_pkg_spec(spec, None);
    }
    if let Some((alias, rest)) = split_local_alias(spec)
        && is_local_path_spec(rest)
    {
        return parse_local_pkg_spec(rest, Some(alias.to_string()));
    }

    // Handle full alias form: alias@jsr:@scope/name[@range]
    if let Some(jsr_idx) = spec.find("@jsr:") {
        let before = &spec[..jsr_idx];
        let after_jsr = &spec[jsr_idx + 5..]; // skip the 5-byte "@jsr:"
        let alias = if before.is_empty() {
            None
        } else {
            Some(before.to_string())
        };
        return parse_jsr_name_range(after_jsr, alias);
    }
    // Handle bare jsr: prefix: jsr:@scope/name[@range]
    if let Some(rest) = spec.strip_prefix("jsr:") {
        return parse_jsr_name_range(rest, None);
    }
    // Handle full alias form: alias@npm:real-pkg@range
    if let Some(npm_idx) = spec.find("@npm:") {
        // Everything before @npm: could be empty (bare npm:pkg@range) or an alias name
        let before = &spec[..npm_idx];
        let after_npm = &spec[npm_idx + 5..]; // skip the 5-byte "@npm:"

        let alias = if before.is_empty() {
            None
        } else {
            Some(before.to_string())
        };

        // after_npm is "real-pkg@range" or "@scope/pkg@range" or just "real-pkg"
        return Ok(parse_name_range(after_npm, alias));
    }

    // Handle bare npm: prefix: npm:pkg@range
    if let Some(rest) = spec.strip_prefix("npm:") {
        return Ok(parse_name_range(rest, None));
    }

    // Normal spec: name[@range]
    Ok(parse_name_range(spec, None))
}

/// Split `alias@<rest>` for the git-spec alias form. Returns
/// `Some((alias, rest))` when the input has a non-empty alias that
/// looks like a plain npm name. Scoped npm names (`@scope/pkg`)
/// start with `@` and never aliased a git spec in any package
/// manager. A `:` in the alias would mean we caught a protocol
/// prefix (`jsr:`, `npm:`, `github:`, `git+…`) — those cases are
/// handled by their own branches in `parse_pkg_spec` and must not
/// be reinterpreted as a git alias.
fn split_git_alias(spec: &str) -> Option<(&str, &str)> {
    split_protocol_alias(spec)
}

/// `true` when `spec` is a local-path form: an explicit `file:` /
/// `link:` prefix, or a bare path the user typed at the shell
/// (`./foo`, `/abs/foo`, `~/foo`, `C:/foo`). Mirrors pnpm's
/// `parseBareSpecifier` so `aube add /path/to/lib` no longer falls
/// through to the registry path and 405s. Any `file:` URL form that
/// `parse_git_spec` recognizes (e.g. `file:///host/repo.git`) is
/// treated as a git spec instead — same precedence the resolver's
/// `is_non_registry_specifier` uses.
fn is_local_path_spec(spec: &str) -> bool {
    if spec.starts_with("link:") {
        return true;
    }
    if spec.starts_with("file:") {
        // `file:` git URLs (`file:///host/repo.git`) belong on the git
        // branch, not here. The bare-path branch below has no such
        // collision because git URL forms always use a protocol.
        return aube_lockfile::parse_git_spec(spec).is_none();
    }
    looks_like_path(spec)
}

/// `true` when `s` looks like a path the user typed at the shell:
/// absolute, relative, home-relative, or a Windows drive-letter form.
/// Deliberately narrower than pnpm's `includes(path.sep)` rule so
/// scoped registry names like `@babel/core` don't get mistaken for a
/// directory path. The drive-letter branch requires a `/` or `\` after
/// the colon so a single-character alias like `a:1.0.0` isn't
/// reclassified.
fn looks_like_path(s: &str) -> bool {
    if s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('/')
        || s.starts_with("~/")
        || s.starts_with("~\\")
        || s.starts_with('\\')
        || s.starts_with(".\\")
        || s.starts_with("..\\")
    {
        return true;
    }
    let bytes = s.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn is_tarball_suffix(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.ends_with(".tgz") || lower.ends_with(".tar.gz") || lower.ends_with(".tar")
}

/// Expand a leading `~/` (or `~\`) to the user's home directory.
/// Returns the input unchanged when there's no tilde. Used at parse
/// time so the verbatim spec written to the manifest is something the
/// resolver (which has no tilde-expansion of its own) can actually
/// open. Errors when `$HOME` is unavailable rather than letting the
/// literal `~` leak into a `cwd`-joined path the resolver can't make
/// sense of.
fn expand_tilde(s: &str) -> miette::Result<String> {
    let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) else {
        return Ok(s.to_string());
    };
    let home = aube_util::env::home_dir().ok_or_else(|| {
        miette!(
            "cannot expand `~/` in `{s}` — $HOME is not set; \
             pass an absolute path or set $HOME"
        )
    })?;
    Ok(home.join(rest).to_string_lossy().into_owned())
}

/// Normalize a bare local-path spec into its `file:` / `link:` form
/// (pnpm parity: directories default to `link:`, tarballs to `file:`).
/// Returns the input unchanged when it already carries an explicit
/// protocol prefix.
fn prefix_bare_local_path(spec: &str) -> miette::Result<String> {
    if spec.starts_with("file:") || spec.starts_with("link:") {
        return Ok(spec.to_string());
    }
    let expanded = expand_tilde(spec)?;
    let prefix = if is_tarball_suffix(&expanded) {
        "file:"
    } else {
        "link:"
    };
    Ok(format!("{prefix}{expanded}"))
}

/// Split `alias@<rest>` for the local-spec alias form. Mirrors
/// `split_git_alias` — same shape, different caller intent.
fn split_local_alias(spec: &str) -> Option<(&str, &str)> {
    split_protocol_alias(spec)
}

/// Shared helper for `split_git_alias` and `split_local_alias`. The
/// alias-form rules are identical for both: peel a leading `name@`
/// where `name` is a plain (non-scoped, non-protocol) npm-style id.
/// Scoped aliases (`@scope/alias@<spec>`) are caught by
/// [`split_scoped_alias`] one branch up; the leading `@` is rejected
/// here via the `at == 0` guard.
fn split_protocol_alias(spec: &str) -> Option<(&str, &str)> {
    let at = spec.find('@')?;
    if at == 0 {
        return None;
    }
    let alias = &spec[..at];
    if alias.contains(':') {
        return None;
    }
    Some((alias, &spec[at + 1..]))
}

/// Split `@scope/alias@<rest>` so scoped names can serve as the
/// manifest key for git/local specs. Returns `Some((alias, rest))`
/// when the input looks like a scoped npm name followed by `@<spec>`.
/// Mirrors pnpm's behavior: `@my-scope/alias@file:./pkg` writes the
/// manifest entry under `@my-scope/alias`.
fn split_scoped_alias(spec: &str) -> Option<(&str, &str)> {
    if !spec.starts_with('@') {
        return None;
    }
    let slash = spec.find('/')?;
    let after_slash = &spec[slash + 1..];
    let at_in_after = after_slash.find('@')?;
    let alias_end = slash + 1 + at_in_after;
    if alias_end == 0 {
        return None;
    }
    Some((&spec[..alias_end], &spec[alias_end + 1..]))
}

/// Build a `ParsedPkgSpec` for a git-form spec. The manifest key is
/// the user-supplied alias when given, otherwise the repo segment of
/// the clone URL (e.g. `kevva/is-negative` → `is-negative`). The
/// `range` field carries the verbatim spec so `git_spec` and `range`
/// agree, and the install pipeline's lockfile reader sees the same
/// string the user typed.
fn parse_git_pkg_spec(verbatim: &str, alias: Option<String>) -> miette::Result<ParsedPkgSpec> {
    let (clone_url, _committish, _subpath) = aube_lockfile::parse_git_spec(verbatim)
        .ok_or_else(|| miette!("expected git spec, got `{verbatim}`"))?;
    // Only derive a name from the URL when the user didn't supply an
    // alias — a trailing-slash or otherwise pathless URL would
    // otherwise hard-fail even though the alias makes the derivation
    // unnecessary.
    let name = match &alias {
        Some(a) => a.clone(),
        None => repo_name_from_clone_url(&clone_url).ok_or_else(|| {
            miette!(
                "could not derive a package name from git URL `{clone_url}`; \
                 pass an alias (e.g. `my-name@{verbatim}`)"
            )
        })?,
    };
    Ok(ParsedPkgSpec {
        alias,
        name,
        jsr_name: None,
        range: verbatim.to_string(),
        has_explicit_range: true,
        git_spec: Some(verbatim.to_string()),
        local_spec: None,
        linked_workspace_version: None,
    })
}

/// Build a `ParsedPkgSpec` for a local-path spec. The manifest key is
/// the user-supplied alias when given, otherwise the basename of the
/// path (e.g. `file:./packages/foo` → `foo`). The `range` field
/// carries the verbatim spec so `local_spec` and `range` agree, and
/// the install pipeline's lockfile reader sees the same string the
/// user typed.
///
/// Bare paths (`./foo`, `/abs/foo`, `~/foo`) are normalized into their
/// `file:` / `link:` form before being stored — pnpm parity for
/// `aube add /path/to/lib`. `~/` is expanded eagerly because the
/// resolver has no tilde handling.
fn parse_local_pkg_spec(input: &str, alias: Option<String>) -> miette::Result<ParsedPkgSpec> {
    let verbatim = prefix_bare_local_path(input)?;
    let path = verbatim
        .strip_prefix("file:")
        .or_else(|| verbatim.strip_prefix("link:"))
        .ok_or_else(|| miette!("expected file:/link: spec, got `{verbatim}`"))?;
    // Only derive a name from the path when the user didn't supply an
    // alias — a bare `file:` (empty path) would otherwise hard-fail
    // even though the alias makes the derivation unnecessary.
    let name = match &alias {
        Some(a) => a.clone(),
        None => basename_from_local_path(path).ok_or_else(|| {
            miette!(
                "could not derive a package name from local spec `{verbatim}`; \
                 pass an alias (e.g. `my-name@{verbatim}`)"
            )
        })?,
    };
    Ok(ParsedPkgSpec {
        alias,
        name,
        jsr_name: None,
        range: verbatim.clone(),
        has_explicit_range: true,
        git_spec: None,
        local_spec: Some(verbatim),
        linked_workspace_version: None,
    })
}

/// Pull the repo segment out of a git clone URL, stripping a trailing
/// `.git`. Used as the manifest key when the user didn't supply an
/// alias. `None` when the URL has no path segment to slice (which
/// shouldn't happen for `parse_git_spec` outputs but the caller guards
/// against the edge case anyway).
fn repo_name_from_clone_url(url: &str) -> Option<String> {
    let body = url.split_once('?').map(|(b, _)| b).unwrap_or(url);
    let body = body.split_once('#').map(|(b, _)| b).unwrap_or(body);
    let last = body.rsplit('/').next()?;
    let stripped = last.strip_suffix(".git").unwrap_or(last);
    if stripped.is_empty() {
        return None;
    }
    Some(stripped.to_string())
}

/// Derive the manifest key from the path portion of a `file:` /
/// `link:` spec. Strips a trailing `.tgz` / `.tar.gz` so a tarball
/// like `file:./bundle.tgz` lands as `"bundle"` in the manifest.
/// Returns `None` for empty / pathless inputs. Splits on both `/` and
/// `\` so a Windows path like `c:\projects\lib` resolves to `"lib"`,
/// not the whole string.
fn basename_from_local_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return None;
    }
    let last = trimmed.rsplit(['/', '\\']).next()?;
    // `.tar.gz` checked before `.tgz` / `.tar` so a doubly-suffixed
    // name strips both compression and archive in one pass.
    let stripped = last
        .strip_suffix(".tar.gz")
        .or_else(|| last.strip_suffix(".tgz"))
        .or_else(|| last.strip_suffix(".tar"))
        .unwrap_or(last);
    if stripped.is_empty() || stripped == "." || stripped == ".." {
        return None;
    }
    Some(stripped.to_string())
}

fn parse_name_range(s: &str, alias: Option<String>) -> ParsedPkgSpec {
    // Handle scoped packages: @scope/name@range
    if s.starts_with('@') {
        if let Some(slash_idx) = s.find('/') {
            let after_slash = &s[slash_idx + 1..];
            if let Some(at_idx) = after_slash.find('@') {
                return ParsedPkgSpec {
                    alias,
                    name: s[..slash_idx + 1 + at_idx].to_string(),
                    jsr_name: None,
                    range: after_slash[at_idx + 1..].to_string(),
                    has_explicit_range: true,
                    git_spec: None,
                    local_spec: None,
                    linked_workspace_version: None,
                };
            }
        }
        return ParsedPkgSpec {
            alias,
            name: s.to_string(),
            jsr_name: None,
            range: "latest".to_string(),
            has_explicit_range: false,
            git_spec: None,
            local_spec: None,
            linked_workspace_version: None,
        };
    }

    // Unscoped: name@range
    if let Some(at_idx) = s.find('@') {
        ParsedPkgSpec {
            alias,
            name: s[..at_idx].to_string(),
            jsr_name: None,
            range: s[at_idx + 1..].to_string(),
            has_explicit_range: true,
            git_spec: None,
            local_spec: None,
            linked_workspace_version: None,
        }
    } else {
        ParsedPkgSpec {
            alias,
            name: s.to_string(),
            jsr_name: None,
            range: "latest".to_string(),
            has_explicit_range: false,
            git_spec: None,
            local_spec: None,
            linked_workspace_version: None,
        }
    }
}

/// Parse the `@scope/name[@range]` tail of a `jsr:` spec and translate
/// the JSR-style scoped name into the npm-compat form served at
/// <https://npm.jsr.io>. JSR packages always use scoped names — we
/// reject anything that doesn't start with `@scope/` so the user gets a
/// real error instead of a `latest` lookup against a garbled package
/// name.
///
/// If `alias` is `None`, we default the manifest key to the JSR name
/// itself so `aube add jsr:@std/collections` lands as
/// `"@std/collections": "jsr:…"` — matching pnpm's behavior.
fn parse_jsr_name_range(s: &str, alias: Option<String>) -> miette::Result<ParsedPkgSpec> {
    let inner = parse_name_range(s, None);
    let jsr_name = inner.name.clone();
    let npm_name = aube_registry::jsr::jsr_to_npm_name(&jsr_name).ok_or_else(|| {
        miette!(
            "invalid jsr: spec — expected `jsr:@scope/name[@range]`, got `jsr:{s}` \
             (JSR packages must be scoped, e.g. `jsr:@std/collections`)"
        )
    })?;
    let final_alias = alias.or_else(|| Some(jsr_name.clone()));
    Ok(ParsedPkgSpec {
        alias: final_alias,
        name: npm_name,
        jsr_name: Some(jsr_name),
        range: inner.range,
        has_explicit_range: inner.has_explicit_range,
        git_spec: None,
        local_spec: None,
        linked_workspace_version: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pkg_spec_name_only() {
        let s = parse_pkg_spec("lodash").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "latest");
        assert!(s.alias.is_none());
        assert!(s.jsr_name.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_with_version() {
        let s = parse_pkg_spec("lodash@^4.17.0").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "^4.17.0");
        assert!(s.alias.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_exact_version() {
        let s = parse_pkg_spec("lodash@4.17.21").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "4.17.21");
    }

    #[test]
    fn test_parse_pkg_spec_scoped() {
        let s = parse_pkg_spec("@babel/core").unwrap();
        assert_eq!(s.name, "@babel/core");
        assert_eq!(s.range, "latest");
    }

    #[test]
    fn test_parse_pkg_spec_scoped_with_version() {
        let s = parse_pkg_spec("@babel/core@^7.24.0").unwrap();
        assert_eq!(s.name, "@babel/core");
        assert_eq!(s.range, "^7.24.0");
    }

    #[test]
    fn test_parse_pkg_spec_dist_tag() {
        let s = parse_pkg_spec("lodash@latest").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "latest");
    }

    #[test]
    fn test_parse_pkg_spec_npm_bare() {
        // npm:string-width@^4.2.0 — no alias, just resolves real package
        let s = parse_pkg_spec("npm:string-width@^4.2.0").unwrap();
        assert_eq!(s.name, "string-width");
        assert_eq!(s.range, "^4.2.0");
        assert!(s.alias.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_npm_alias_full() {
        // string-width-cjs@npm:string-width@^4.2.0
        let s = parse_pkg_spec("string-width-cjs@npm:string-width@^4.2.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("string-width-cjs"));
        assert_eq!(s.name, "string-width");
        assert_eq!(s.range, "^4.2.0");
    }

    #[test]
    fn test_parse_pkg_spec_npm_alias_scoped() {
        // my-react@npm:@preact/compat@^17.0.0
        let s = parse_pkg_spec("my-react@npm:@preact/compat@^17.0.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("my-react"));
        assert_eq!(s.name, "@preact/compat");
        assert_eq!(s.range, "^17.0.0");
    }

    #[test]
    fn test_parse_pkg_spec_npm_alias_no_version() {
        // my-lodash@npm:lodash
        let s = parse_pkg_spec("my-lodash@npm:lodash").unwrap();
        assert_eq!(s.alias.as_deref(), Some("my-lodash"));
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "latest");
    }

    #[test]
    fn test_parse_pkg_spec_jsr_bare_no_range() {
        // jsr:@std/collections — default alias is the JSR name itself
        let s = parse_pkg_spec("jsr:@std/collections").unwrap();
        assert_eq!(s.alias.as_deref(), Some("@std/collections"));
        assert_eq!(s.name, "@jsr/std__collections");
        assert_eq!(s.jsr_name.as_deref(), Some("@std/collections"));
        assert_eq!(s.range, "latest");
        assert!(!s.has_explicit_range);
    }

    #[test]
    fn test_parse_pkg_spec_jsr_bare_with_range() {
        let s = parse_pkg_spec("jsr:@std/collections@^1.0.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("@std/collections"));
        assert_eq!(s.name, "@jsr/std__collections");
        assert_eq!(s.jsr_name.as_deref(), Some("@std/collections"));
        assert_eq!(s.range, "^1.0.0");
        assert!(s.has_explicit_range);
    }

    #[test]
    fn test_parse_pkg_spec_jsr_aliased() {
        let s = parse_pkg_spec("collections@jsr:@std/collections@^1.0.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("collections"));
        assert_eq!(s.name, "@jsr/std__collections");
        assert_eq!(s.jsr_name.as_deref(), Some("@std/collections"));
        assert_eq!(s.range, "^1.0.0");
    }

    #[test]
    fn test_parse_pkg_spec_jsr_rejects_unscoped() {
        let err = parse_pkg_spec("jsr:collections").unwrap_err();
        assert!(
            err.to_string().contains("JSR packages must be scoped"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_pkg_spec_jsr_rejects_path_shapes() {
        for spec in [
            "jsr:@std/../collections",
            "jsr:@std/collections/more",
            "jsr:@std//collections",
            "jsr:@std/co llections",
        ] {
            let err = parse_pkg_spec(spec).unwrap_err();
            assert!(
                err.to_string().contains("invalid jsr: spec"),
                "{spec} produced unexpected error: {err}"
            );
        }
    }

    #[test]
    fn test_parse_pkg_spec_git_bare_github_shorthand() {
        let s = parse_pkg_spec("kevva/is-negative").unwrap();
        assert_eq!(s.git_spec.as_deref(), Some("kevva/is-negative"));
        assert_eq!(s.name, "is-negative");
        assert_eq!(s.range, "kevva/is-negative");
        assert!(s.alias.is_none());
        assert!(s.has_explicit_range);
    }

    #[test]
    fn test_parse_pkg_spec_git_github_protocol() {
        let s = parse_pkg_spec("github:user/repo").unwrap();
        assert_eq!(s.git_spec.as_deref(), Some("github:user/repo"));
        assert_eq!(s.name, "repo");
        assert_eq!(s.range, "github:user/repo");
        assert!(s.alias.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_git_url_with_committish() {
        let spec = "git+https://github.com/owner/repo.git#tag/with/slash";
        let s = parse_pkg_spec(spec).unwrap();
        assert_eq!(s.git_spec.as_deref(), Some(spec));
        // Repo segment derived before the `#`/`?` slice, then `.git` stripped.
        assert_eq!(s.name, "repo");
        assert_eq!(s.range, spec);
    }

    #[test]
    fn test_parse_pkg_spec_git_alias() {
        let s = parse_pkg_spec("my-alias@kevva/is-negative").unwrap();
        assert_eq!(s.git_spec.as_deref(), Some("kevva/is-negative"));
        assert_eq!(s.alias.as_deref(), Some("my-alias"));
        assert_eq!(s.name, "my-alias");
        assert_eq!(s.range, "kevva/is-negative");
    }

    #[test]
    fn test_parse_pkg_spec_git_alias_skips_url_derivation() {
        // When an alias is given, the manifest key comes from the alias
        // — `repo_name_from_clone_url` should not be consulted, so a
        // pathless URL like `git+https://example.com/` doesn't error
        // out the way it does without an alias.
        let s = parse_pkg_spec("my-alias@git+https://example.com/").unwrap();
        assert_eq!(s.git_spec.as_deref(), Some("git+https://example.com/"));
        assert_eq!(s.alias.as_deref(), Some("my-alias"));
        assert_eq!(s.name, "my-alias");
    }

    #[test]
    fn test_parse_pkg_spec_file_relative() {
        let s = parse_pkg_spec("file:./local/pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:./local/pkg"));
        assert_eq!(s.name, "pkg");
        assert_eq!(s.range, "file:./local/pkg");
        assert!(s.alias.is_none());
        assert!(s.has_explicit_range);
    }

    #[test]
    fn test_parse_pkg_spec_link_relative() {
        let s = parse_pkg_spec("link:./local/pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("link:./local/pkg"));
        assert_eq!(s.name, "pkg");
        assert_eq!(s.range, "link:./local/pkg");
        assert!(s.alias.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_file_absolute() {
        let s = parse_pkg_spec("file:/abs/pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:/abs/pkg"));
        assert_eq!(s.name, "pkg");
    }

    #[test]
    fn test_parse_pkg_spec_file_tarball_strips_extension() {
        // Basename of `bundle.tgz` lands as `"bundle"` — the manifest
        // key shouldn't carry the archive suffix.
        let s = parse_pkg_spec("file:./bundle.tgz").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:./bundle.tgz"));
        assert_eq!(s.name, "bundle");
    }

    #[test]
    fn test_parse_pkg_spec_file_alias() {
        let s = parse_pkg_spec("my-alias@file:./pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:./pkg"));
        assert_eq!(s.alias.as_deref(), Some("my-alias"));
        assert_eq!(s.name, "my-alias");
        assert_eq!(s.range, "file:./pkg");
    }

    #[test]
    fn test_parse_pkg_spec_link_alias() {
        let s = parse_pkg_spec("my-alias@link:./pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("link:./pkg"));
        assert_eq!(s.alias.as_deref(), Some("my-alias"));
        assert_eq!(s.name, "my-alias");
        assert_eq!(s.range, "link:./pkg");
    }

    #[test]
    fn test_parse_pkg_spec_local_alias_skips_basename_derivation() {
        // When an alias is given, the manifest key comes from the
        // alias — `basename_from_local_path` should not be consulted,
        // so a pathless spec like `file:` doesn't error out the way
        // it does without an alias.
        let s = parse_pkg_spec("my-alias@file:").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:"));
        assert_eq!(s.alias.as_deref(), Some("my-alias"));
        assert_eq!(s.name, "my-alias");
    }

    #[test]
    fn test_parse_pkg_spec_scoped_not_git_or_local() {
        // Scoped npm names must not be misclassified as git or local specs.
        let s = parse_pkg_spec("@scope/pkg").unwrap();
        assert!(s.git_spec.is_none());
        assert!(s.local_spec.is_none());
        assert_eq!(s.name, "@scope/pkg");
        assert_eq!(s.range, "latest");
    }

    #[test]
    fn test_parse_pkg_spec_bare_user_repo_not_local() {
        // `user/repo` is a git shorthand (handled by the git-spec
        // branch) and must not collide with the file/link path here.
        let s = parse_pkg_spec("kevva/is-negative").unwrap();
        assert!(s.local_spec.is_none());
        assert!(s.git_spec.is_some());
    }

    #[test]
    fn test_parse_pkg_spec_scoped_alias_for_local() {
        // `@scope/alias@file:./pkg` — the scoped name is the manifest
        // key, the local spec is preserved verbatim.
        let s = parse_pkg_spec("@my-scope/alias@file:./pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:./pkg"));
        assert_eq!(s.alias.as_deref(), Some("@my-scope/alias"));
        assert_eq!(s.name, "@my-scope/alias");
    }

    #[test]
    fn test_parse_pkg_spec_scoped_alias_for_git() {
        // Same shape with a git spec on the right-hand side.
        let s = parse_pkg_spec("@my-scope/alias@kevva/is-negative").unwrap();
        assert_eq!(s.git_spec.as_deref(), Some("kevva/is-negative"));
        assert_eq!(s.alias.as_deref(), Some("@my-scope/alias"));
        assert_eq!(s.name, "@my-scope/alias");
    }

    #[test]
    fn test_parse_pkg_spec_file_uncompressed_tarball_strips_extension() {
        // `.tar` (uncompressed) should strip alongside `.tgz` /
        // `.tar.gz` so the manifest key isn't littered with archive
        // suffixes.
        let s = parse_pkg_spec("file:./bundle.tar").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:./bundle.tar"));
        assert_eq!(s.name, "bundle");
    }

    #[test]
    fn test_parse_pkg_spec_bare_absolute_path() {
        // discussions/497 — `aube add /path/to/library-foo/` used to
        // fall through to the registry and 405. It now normalizes to
        // `link:/path/to/library-foo/` and routes through the local
        // branch.
        let s = parse_pkg_spec("/path/to/library-foo/").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("link:/path/to/library-foo/"));
        assert_eq!(s.name, "library-foo");
    }

    #[test]
    fn test_parse_pkg_spec_bare_relative_path() {
        for input in ["./lib", "../lib", "../../foo/bar"] {
            let s = parse_pkg_spec(input).unwrap();
            let local = s.local_spec.expect("relative path should detect as local");
            assert_eq!(local, format!("link:{input}"));
        }
    }

    #[test]
    fn test_parse_pkg_spec_bare_tilde_path_expands() {
        // Resolver has no tilde handling, so the verbatim spec stored
        // in the manifest must already be absolute. The exact home
        // path differs by platform (`/home/…` on Linux,
        // `C:\Users\…` on Windows), and `Path::join` mixes separators
        // (`C:\Users\runneradmin\proj/lib`) — only what matters for
        // this test is that the literal `~` is gone and the basename
        // resolved to `lib`.
        let s = parse_pkg_spec("~/proj/lib").unwrap();
        let local = s.local_spec.expect("~/ path should detect as local");
        assert!(
            local.starts_with("link:"),
            "expected link: prefix in `{local}`"
        );
        assert!(
            !local.contains('~'),
            "tilde must be expanded eagerly, got `{local}`"
        );
        assert_eq!(s.name, "lib");
    }

    #[test]
    fn test_parse_pkg_spec_bare_tarball_uses_file_protocol() {
        // pnpm parity: bare paths default to `link:` for directories
        // and `file:` for tarballs. Basename-strip is shared with the
        // explicit `file:./foo.tgz` branch.
        let s = parse_pkg_spec("./vendor/local-helper-1.0.0.tgz").unwrap();
        assert_eq!(
            s.local_spec.as_deref(),
            Some("file:./vendor/local-helper-1.0.0.tgz")
        );
        assert_eq!(s.name, "local-helper-1.0.0");
    }

    #[test]
    fn test_parse_pkg_spec_short_alias_not_drive_letter() {
        // `a:1.0.0` looks superficially like a Windows drive form
        // (`<letter>:`) but has no path separator after the colon —
        // it's a single-character npm alias and must NOT be
        // reclassified as a local path.
        let s = parse_pkg_spec("a:1.0.0").unwrap();
        assert!(s.local_spec.is_none());
        assert!(s.git_spec.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_windows_drive_letter() {
        for input in ["C:/projects/lib", "c:\\projects\\lib"] {
            let s = parse_pkg_spec(input).unwrap();
            let local = s
                .local_spec
                .expect("drive-letter path should detect as local");
            assert_eq!(local, format!("link:{input}"));
            // Basename derivation must split on `\` too — `lib`, not
            // the whole `c:\projects\lib`.
            assert_eq!(s.name, "lib");
        }
    }

    #[test]
    fn test_parse_pkg_spec_windows_backslash_relative() {
        // `..\lib` and `.\lib` are the Windows shells' path-typing
        // convention; both must route through the local branch and
        // basename-derive to `lib`, not the verbatim string.
        for input in ["..\\lib", ".\\lib"] {
            let s = parse_pkg_spec(input).unwrap();
            assert!(s.local_spec.is_some(), "`{input}` should detect as local");
            assert_eq!(s.name, "lib", "wrong basename for `{input}`");
        }
    }
}
