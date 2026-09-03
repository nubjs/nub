//! Workspace support for aube.
//!
//! Reads pnpm-workspace.yaml to discover workspace packages, falling back
//! to `package.json`'s `workspaces` field (yarn/npm/bun shape) when no
//! yaml is present. Supports the `workspace:` protocol for inter-package
//! dependencies.

pub mod selector;
pub mod topo;

use std::path::{Component, Path, PathBuf};

pub use aube_manifest::workspace::WorkspaceConfig;
pub use selector::{Selector, WorkspacePkg};

/// Whether workspace globs may select packages outside the workspace root.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspaceBoundary {
    /// Preserve pnpm-compatible behavior, including patterns such as `../**`.
    #[default]
    AllowOutsideRoot,
    /// Reject absolute and parent-relative patterns.
    ///
    /// Embedding hosts that discover projects inside a preselected repository
    /// should use this mode so workspace configuration cannot expand the scan
    /// beyond that repository.
    ConfinedToRoot,
}

/// Controls workspace package discovery.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct WorkspaceDiscoveryOptions {
    /// Boundary applied to positive and negative workspace patterns.
    pub boundary: WorkspaceBoundary,
}

impl WorkspaceDiscoveryOptions {
    /// Restrict discovery to packages beneath the workspace root.
    pub fn confined_to_root() -> Self {
        Self {
            boundary: WorkspaceBoundary::ConfinedToRoot,
        }
    }
}

/// Whether `project_dir` is the root of a workspace project — i.e.
/// the user has set up workspace mode via `aube-workspace.yaml` /
/// `pnpm-workspace.yaml` or `package.json#workspaces`, regardless of
/// whether the current `packages:` glob actually matches any
/// directories on disk.
///
/// Distinct from [`find_workspace_packages`] returning a non-empty
/// list: a workspace whose only sub-package was just `rm -rf`ed
/// still counts as a workspace project (the yaml is still on disk),
/// but `find_workspace_packages` would return an empty vec.
/// Callers that need to drive workspace-shaped behavior on the
/// "all packages currently absent" boundary (lockfile importer
/// pruning, workspace-yaml-only validation) need this stronger
/// signal.
pub fn is_workspace_project_root(project_dir: &Path) -> bool {
    if aube_manifest::workspace::workspace_yaml_names()
        .iter()
        .any(|name| project_dir.join(name).is_file())
    {
        return true;
    }
    package_json_workspace_patterns(project_dir)
        .map(|patterns| !patterns.is_empty())
        .unwrap_or(false)
}

/// Discover workspace package directories.
///
/// Precedence:
/// 1. `aube-workspace.yaml` / `pnpm-workspace.yaml` `packages:` (authoritative
///    when present — pnpm/aube projects keep yaml as source of truth).
/// 2. `package.json#workspaces` (yarn/npm/bun shape — array form or the
///    `{ packages: [...] }` object form).
pub fn find_workspace_packages(project_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    find_workspace_packages_with_options(project_dir, WorkspaceDiscoveryOptions::default())
}

/// Discover workspace package directories with host-selected policy.
///
/// This has the same source precedence and matching behavior as
/// [`find_workspace_packages`]. The options only constrain behavior that an
/// embedding host may need to make stricter than the package-manager CLI.
/// [`WorkspaceBoundary::ConfinedToRoot`] returns canonical package paths so a
/// validated symlink cannot be redirected before the host consumes the result.
pub fn find_workspace_packages_with_options(
    project_dir: &Path,
    options: WorkspaceDiscoveryOptions,
) -> Result<Vec<PathBuf>, Error> {
    let config = WorkspaceConfig::load(project_dir).map_err(|e| match e {
        aube_manifest::Error::Io(p, e) => Error::Io(p, e),
        aube_manifest::Error::YamlParse(p, e) => Error::Parse(p, e),
        aube_manifest::Error::Parse(pe) => Error::ParseDiag(pe),
    })?;

    let workspace_yaml = aube_manifest::workspace::workspace_yaml_existing(project_dir);
    let patterns: Vec<String> = if workspace_yaml.is_some() {
        config.packages.clone()
    } else {
        package_json_workspace_patterns(project_dir)?
    };

    if patterns.is_empty() {
        return Ok(vec![]);
    }

    let definition_path = workspace_yaml.unwrap_or_else(|| project_dir.join("package.json"));
    let confined_root = (options.boundary == WorkspaceBoundary::ConfinedToRoot)
        .then(|| {
            project_dir
                .canonicalize()
                .map_err(|error| Error::Io(project_dir.to_path_buf(), error))
        })
        .transpose()?;
    let mut neg_matchers = Vec::new();
    let mut positives = Vec::new();
    for raw in &patterns {
        let (negated, pattern) = raw
            .strip_prefix('!')
            .map_or((false, raw.as_str()), |pattern| (true, pattern));
        for expanded in expand_braces(pattern) {
            validate_workspace_pattern(&definition_path, &expanded, options.boundary)?;
            if negated {
                let mk = |p: &str| {
                    glob::Pattern::new(p)
                        .map_err(|e| Error::Parse(definition_path.clone(), e.to_string()))
                };
                // pnpm uses micromatch where `**` matches zero-or-more
                // path components, so `!**/example/**` excludes the
                // directory `example` itself. The `glob` crate requires
                // `**` to consume at least one component, so emit a
                // companion matcher with the trailing `/**` stripped to
                // catch the directory itself in addition to its descendants.
                neg_matchers.push(mk(&expanded)?);
                if let Some(self_form) = expanded.strip_suffix("/**") {
                    neg_matchers.push(mk(self_form)?);
                }
            } else {
                positives.push(expanded);
            }
        }
    }

    // Overlapping patterns are valid and common — a project may list
    // both `packages/*` and a specific `packages/slack` entry, or mix
    // a glob with an explicit nested path (`packages/sdk/js`). Dedupe
    // so downstream consumers (linker importer iteration, bin wiring,
    // filter matching) see each workspace package exactly once;
    // otherwise `link_workspace` tries to symlink the same top-level
    // dep twice and blows up with EEXIST.
    let mut seen = std::collections::HashSet::new();
    let mut packages = Vec::new();
    for pattern in &positives {
        for pkg_dir in expand_workspace_pattern(project_dir, &definition_path, pattern)? {
            // `pathdiff` produces the as-written-from-`project_dir`
            // form (`../sibling` for parent-tree matches), which is
            // what the negation matcher was compiled against. Falling
            // back to the absolute path for unrelated trees is fine
            // since the matchers are anchored against the relative
            // form and won't match an absolute path.
            let rel_owned = pathdiff::diff_paths(&pkg_dir, project_dir);
            let rel = rel_owned.as_deref().unwrap_or(&pkg_dir);
            if neg_matchers.iter().any(|m| m.matches_path(rel)) {
                continue;
            }
            let package_path = if let Some(confined_root) = &confined_root {
                let canonical_package = pkg_dir
                    .canonicalize()
                    .map_err(|error| Error::Io(pkg_dir.clone(), error))?;
                if !canonical_package.starts_with(confined_root) {
                    return Err(Error::Parse(
                        definition_path.clone(),
                        format!(
                            "workspace package {} resolves outside the workspace root",
                            pkg_dir.display()
                        ),
                    ));
                }
                canonical_package
            } else {
                pkg_dir
            };
            if seen.insert(package_path.clone()) {
                packages.push(package_path);
            }
        }
    }

    packages.sort_unstable();
    Ok(packages)
}

fn validate_workspace_pattern(
    definition_path: &Path,
    pattern: &str,
    boundary: WorkspaceBoundary,
) -> Result<(), Error> {
    if boundary == WorkspaceBoundary::ConfinedToRoot
        && (Path::new(pattern).is_absolute()
            || Path::new(pattern)
                .components()
                .any(|component| component == Component::ParentDir))
    {
        return Err(Error::Parse(
            definition_path.to_path_buf(),
            format!(
                "workspace pattern {pattern:?} must be relative and cannot escape the workspace root"
            ),
        ));
    }
    Ok(())
}

/// Does `path` name a workspace member under `patterns`?
///
/// `path` is project-root-relative with `/` separators — the spelling an
/// importer key already uses. Purely lexical: no filesystem access, so a
/// caller holding only a lockfile and a manifest can ask the question.
///
/// This exists because a `package-lock.json` CANNOT answer it. npm keys
/// both a workspace member (`packages/app`) and a local directory
/// dependency (`vendor/local`) as a bare path carrying `name`/`version`,
/// and either may hold a root `node_modules/<name>` link record — a root
/// importer's own `file:./dep` is indistinguishable from a member by
/// shape alone. The manifest's `workspaces` patterns are the only real
/// source of truth, so the reader matches against them here rather than
/// guessing from the lockfile.
///
/// Reproduces the discovery walk's semantics: braces expand first, a `!`
/// prefix negates, and a match needs some positive pattern and no
/// negative one. Getting that wrong in the EXCLUSIVE direction is the
/// expensive one — a real member judged a non-member loses its importer
/// entry and its install breaks — while an over-inclusive answer only
/// reproduces the phantom-importer bug the caller is fixing.
///
/// The two halves match with DIFFERENT options, because the walk does.
/// It expands positives through `glob::glob` on the filesystem, where a
/// `*` cannot span a directory boundary, but tests negatives with
/// `Pattern::matches_path`, which by default lets one span freely. That
/// asymmetry is the walk's, not an oversight here; copying it is what
/// keeps the two answering alike.
pub fn matches_member_patterns(path: &str, patterns: &[String]) -> bool {
    let mut matched = false;
    for raw in patterns {
        let (negated, pattern) = raw
            .strip_prefix('!')
            .map_or((false, raw.as_str()), |p| (true, p));
        for expanded in expand_braces(pattern) {
            if negated {
                // Mirror the walk's negation matchers EXACTLY: the raw
                // spelling, default match options, and the companion with a
                // trailing `/**` stripped — the `glob` crate makes `**`
                // consume at least one component, while the micromatch
                // semantics pnpm uses let it match zero, so `!packages/**`
                // has to exclude `packages` itself as well.
                //
                // Normalising the spelling here, as the positive half does,
                // looks like the consistent choice and is a bug: it makes
                // `!./packages/skipped` exclude a member the walk KEEPS,
                // and over-exclusion is the direction that costs a real
                // member its importer entry. Agreement with the walk is the
                // whole contract, so a divergence that reads as an
                // improvement is still a divergence.
                let excluded = std::iter::once(expanded.as_str())
                    .chain(expanded.strip_suffix("/**"))
                    .any(|p| glob::Pattern::new(p).is_ok_and(|m| m.matches(path)));
                if excluded {
                    // A later exclusion wins outright, matching the walk.
                    return false;
                }
            } else if pattern_matches_path(&expanded, path) {
                matched = true;
            }
        }
    }
    matched
}

/// One expanded (brace-free, sign-free) POSITIVE pattern against one path.
///
/// Reproducing the walk lexically takes two corrections, both measured
/// against `find_workspace_packages` on the same inputs, and getting
/// either wrong desynchronises the two.
///
/// **Separators.** The walk expands positives through `glob::glob` on the
/// filesystem, which cannot let a `*` span a directory boundary. Matching
/// a string with `Pattern::matches` can: it defaults
/// `require_literal_separator` to false, so `packages/*` also matches
/// `packages/app/vendor/local` — a nested local dependency, which is
/// exactly the thing the caller is trying to exclude. Hence the strict
/// option here.
///
/// **Spelling.** The walk joins the pattern onto a real directory, so the
/// filesystem normalises `./packages/*` and `packages/*/` for it and both
/// find `packages/app`. A string comparison gets no such help and matches
/// NEITHER, which would drop a real member and break its install — the
/// expensive direction. So normalise the spelling first.
///
/// The `**` companion below is a third divergence, already handled: the
/// `glob` crate requires `**` to consume at least one component, while
/// the micromatch semantics pnpm and npm use let it match zero, so
/// `packages/**` names `packages` itself as well as its descendants.
fn pattern_matches_path(pattern: &str, path: &str) -> bool {
    // `**` is deliberately still allowed to cross separators — that is its
    // whole purpose, and the strict option only constrains `*` and `?`.
    let options = glob::MatchOptions {
        require_literal_separator: true,
        ..Default::default()
    };
    let normalized = normalize_member_pattern(pattern);
    let Ok(matcher) = glob::Pattern::new(normalized) else {
        return false;
    };
    if matcher.matches_with(path, options) {
        return true;
    }
    normalized
        .strip_suffix("/**")
        .and_then(|self_form| glob::Pattern::new(self_form).ok())
        .is_some_and(|m| m.matches_with(path, options))
}

/// Render a workspace pattern in the same frame as a lockfile importer
/// key: no leading `./`, no trailing `/`.
///
/// npm accepts and preserves both spellings, and the discovery walk never
/// has to care because it hands the pattern to the filesystem. A lexical
/// matcher does.
fn normalize_member_pattern(pattern: &str) -> &str {
    let mut p = pattern;
    while let Some(rest) = p.strip_prefix("./") {
        p = rest;
    }
    p.strip_suffix('/').unwrap_or(p)
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let mut depth = 0;
    let mut open = None;
    let mut commas = Vec::new();
    for (index, character) in pattern.char_indices() {
        match character {
            '{' => {
                if depth == 0 {
                    open = Some(index);
                    commas.clear();
                }
                depth += 1;
            }
            ',' if depth == 1 => commas.push(index),
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let Some(open_index) = open else {
                        continue;
                    };
                    if commas.is_empty() {
                        open = None;
                        continue;
                    }
                    let mut boundaries = Vec::with_capacity(commas.len() + 2);
                    boundaries.push(open_index + 1);
                    boundaries.extend(commas.iter().map(|comma| comma + 1));
                    boundaries.push(index + 1);
                    return boundaries
                        .windows(2)
                        .flat_map(|window| {
                            let end = window[1] - 1;
                            let expanded = format!(
                                "{}{}{}",
                                &pattern[..open_index],
                                &pattern[window[0]..end],
                                &pattern[index + 1..]
                            );
                            expand_braces(&expanded)
                        })
                        .collect();
                }
            }
            _ => {}
        }
    }
    vec![pattern.to_string()]
}

fn expand_workspace_pattern(
    project_dir: &Path,
    definition_path: &Path,
    pattern: &str,
) -> Result<Vec<PathBuf>, Error> {
    let matcher = glob::Pattern::new(pattern)
        .map_err(|e| Error::Parse(definition_path.to_path_buf(), e.to_string()))?;
    if !pattern.contains("**") {
        return Ok(glob_workspace_pattern(project_dir, pattern));
    }

    let mut packages = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![workspace_pattern_root(project_dir, pattern)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            if entry.file_name() == "node_modules" {
                continue;
            }
            // Parent-relative globs (`../**`) anchor the walk above
            // `project_dir`, so the recursion can sweep back into the
            // project itself via `parent/our-project`. Dedupe by
            // canonical path to guarantee each directory is visited
            // once even when symlinks or `..` rejoin it under a new
            // name. Failing to canonicalize (race / permissions)
            // falls back to the raw path — losing dedupe but not
            // correctness.
            let dedupe_key = path.canonicalize().unwrap_or_else(|_| path.clone());
            if !visited.insert(dedupe_key) {
                continue;
            }
            stack.push(path.clone());
            // Render the path "as written from `project_dir`": a
            // sibling visited via `../**` reads as `../sibling`,
            // which is the form the matcher (compiled against the
            // raw pattern) needs to see. `pathdiff` is lexical, so
            // both inputs must agree on whether they're absolute —
            // which they do here because `project_dir` is the
            // user-supplied (or resolver-supplied) workspace root
            // and `path` came out of `read_dir` walking from it.
            let Some(rel_path) = pathdiff::diff_paths(&path, project_dir) else {
                continue;
            };
            if matcher.matches_path(&rel_path) && path.join("package.json").is_file() {
                packages.push(path);
            }
        }
    }
    Ok(packages)
}

fn glob_workspace_pattern(project_dir: &Path, pattern: &str) -> Vec<PathBuf> {
    let full_pattern = project_dir.join(pattern).join("package.json");
    let mut packages = Vec::new();
    if let Ok(entries) = glob::glob(full_pattern.to_str().unwrap_or_default()) {
        for entry in entries.flatten() {
            if entry.components().any(|c| c.as_os_str() == "node_modules") {
                continue;
            }
            if let Some(parent) = entry.parent() {
                packages.push(parent.to_path_buf());
            }
        }
    }
    packages
}

fn workspace_pattern_root(project_dir: &Path, pattern: &str) -> PathBuf {
    let wildcard_idx = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
    let literal_prefix = &pattern[..wildcard_idx];
    // Take only the dir portion before the wildcard — e.g.
    // `packages/prefix-*/**/*` → `packages/`, not `packages/prefix-`,
    // because `prefix-` is an incomplete name segment that would
    // break `read_dir`. For `../**` the dir portion is `../`. For
    // `packages/**` it's `packages/`. For `**` it's empty.
    let dir_prefix = literal_prefix
        .rfind('/')
        .map_or("", |idx| &literal_prefix[..idx]);
    // Lexically apply the dir prefix to `project_dir` so a
    // parent-relative pattern (`../packages/**`) anchors the walk
    // above the workspace root instead of starting from it. Without
    // this the recursion would never see the parent tree's siblings
    // and `../**` patterns would silently match nothing. Lexical
    // (not canonicalized) so symlinked workspace setups behave the
    // same way they do for in-tree patterns.
    let mut anchor = PathBuf::from(project_dir);
    for component in Path::new(dir_prefix).components() {
        match component {
            Component::ParentDir => {
                anchor.pop();
            }
            Component::CurDir => {}
            Component::Normal(name) => anchor.push(name),
            // Absolute / prefix components in a workspace pattern
            // would be a user error (`pnpm-workspace.yaml#packages`
            // is documented as project-relative); leave them as-is
            // so the read_dir below fails cleanly rather than
            // silently rerooting the walk.
            other => anchor.push(other.as_os_str()),
        }
    }
    anchor
}

/// Read the `workspaces` field from `<project_dir>/package.json`. Returns
/// an empty vec if the file is missing or the field is absent — a bare
/// package.json without `workspaces` is a single-package project, not an
/// error. Parse errors propagate so typos surface instead of silently
/// yielding an empty workspace.
fn package_json_workspace_patterns(project_dir: &Path) -> Result<Vec<String>, Error> {
    let path = project_dir.join("package.json");
    if !path.is_file() {
        return Ok(vec![]);
    }
    let pkg = aube_manifest::PackageJson::from_path(&path).map_err(|e| match e {
        aube_manifest::Error::Io(p, e) => Error::Io(p, e),
        aube_manifest::Error::Parse(pe) => Error::ParseDiag(pe),
        aube_manifest::Error::YamlParse(p, e) => Error::Parse(p, e),
    })?;
    Ok(pkg
        .workspaces
        .as_ref()
        .map(|w| w.patterns().to_vec())
        .unwrap_or_default())
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("I/O error at {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("failed to parse {0}: {1}")]
    #[diagnostic(code(ERR_AUBE_WORKSPACE_PARSE))]
    Parse(PathBuf, String),
    /// Parse failure that came in via `aube_manifest::Error::Parse` and
    /// still carries its `NamedSource` + `SourceSpan`. Forwarded via
    /// `#[diagnostic(transparent)]` so `miette`'s `fancy` handler draws
    /// a pointer at the offending byte of the offending `package.json`.
    #[error(transparent)]
    #[diagnostic(transparent)]
    ParseDiag(Box<aube_manifest::ParseError>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn names(packages: Vec<PathBuf>) -> BTreeSet<String> {
        packages
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// `matches_member_patterns` must answer exactly what the walk finds,
    /// so every case asks BOTH on the same inputs. The expected column
    /// pins the WALK — a positive control proving the fixture is live and
    /// the case is not passing vacuously — and the matcher is then checked
    /// against the walk's answer rather than against a second hand-written
    /// list the two could drift away from together.
    ///
    /// The tree carries the shapes that decide an npm lockfile importer: a
    /// real member, a local directory dependency NESTED beneath it (npm
    /// keys the two identically, which is the whole reason this function
    /// exists), a second member for a negation to exclude, and a manifest
    /// on `packages` itself, which is what makes the `**`-companion
    /// matcher observable at all.
    #[test]
    fn member_matching_agrees_with_workspace_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let universe = [
            "packages",
            "packages/app",
            "packages/app/vendor/local",
            "packages/skipped",
        ];
        for sub in universe {
            write(
                &dir.path().join(sub).join("package.json"),
                &format!(
                    r#"{{"name":"{}","version":"1.0.0"}}"#,
                    sub.replace('/', "-")
                ),
            );
        }

        let cases: [(&[&str], &[&str]); 6] = [
            // A single `*` must not span a directory boundary, or the
            // nested local dep is retained as a phantom importer.
            (&["packages/*"], &["packages/app", "packages/skipped"]),
            // Two spellings npm accepts and preserves. The walk hands each
            // to the filesystem, which normalises it; a lexical matcher
            // gets no such help, so both matched NOTHING before the fix --
            // dropping real members.
            (&["./packages/*"], &["packages/app", "packages/skipped"]),
            (&["packages/*/"], &["packages/app", "packages/skipped"]),
            (&["packages/*", "!packages/skipped"], &["packages/app"]),
            // The walk does NOT normalise a negation's spelling, so this
            // one excludes nothing. Normalising it here would drop a real
            // member -- the expensive direction.
            (
                &["packages/*", "!./packages/skipped"],
                &["packages/app", "packages/skipped"],
            ),
            // `!packages/**` excludes `packages` itself, not just its
            // descendants: the `glob` crate needs `**` to consume a
            // component, so the walk compiles a companion matcher with the
            // trailing `/**` stripped, and so must this.
            (&["*", "!packages/**"], &[]),
        ];

        for (patterns, expected_walk) in cases {
            let quoted = patterns
                .iter()
                .map(|p| format!(r#""{p}""#))
                .collect::<Vec<_>>()
                .join(",");
            write(
                &dir.path().join("package.json"),
                &format!(r#"{{"name":"r","version":"1.0.0","workspaces":[{quoted}]}}"#),
            );
            let walked: BTreeSet<String> = find_workspace_packages(dir.path())
                .unwrap()
                .iter()
                .filter_map(|p| pathdiff::diff_paths(p, dir.path()))
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .collect();
            assert_eq!(
                walked,
                expected_walk.iter().map(|s| s.to_string()).collect(),
                "the walk itself must see {expected_walk:?} for {patterns:?}"
            );

            let owned: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
            for candidate in universe {
                assert_eq!(
                    matches_member_patterns(candidate, &owned),
                    walked.contains(candidate),
                    "lexical matching disagreed with the walk for {patterns:?} \
                     on {candidate}; the walk saw {walked:?}"
                );
            }
        }
    }

    #[test]
    fn finds_packages_from_pnpm_workspace_yaml() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        );
        write(&dir.path().join("packages/a/package.json"), "{}");
        write(&dir.path().join("packages/b/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["a", "b"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn falls_back_to_package_json_workspaces_array() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["apps/*","packages/*"]}"#,
        );
        write(&dir.path().join("apps/example/package.json"), "{}");
        write(&dir.path().join("packages/ui/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["example", "ui"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn falls_back_to_package_json_workspaces_object() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":{"packages":["apps/*"]}}"#,
        );
        write(&dir.path().join("apps/example/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["example"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn expands_brace_workspace_patterns() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"workspaces":["{apps,packages}/{web,lib}","!{apps,packages}/excluded/**"]}"#,
        );
        write(&dir.path().join("apps/web/package.json"), "{}");
        write(&dir.path().join("packages/lib/package.json"), "{}");
        write(
            &dir.path().join("packages/excluded/example/package.json"),
            "{}",
        );

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["lib", "web"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn confined_discovery_rejects_parent_relative_patterns() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - '../packages/**'\n",
        );

        let err = find_workspace_packages_with_options(
            dir.path(),
            WorkspaceDiscoveryOptions::confined_to_root(),
        )
        .unwrap_err();

        assert!(matches!(err, Error::Parse(_, _)));
        assert!(err.to_string().contains("cannot escape the workspace root"));
    }

    #[test]
    fn confined_discovery_validates_expanded_brace_alternatives() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"workspaces":["{../outside,packages}/*"]}"#,
        );

        let err = find_workspace_packages_with_options(
            dir.path(),
            WorkspaceDiscoveryOptions::confined_to_root(),
        )
        .unwrap_err();

        assert!(matches!(err, Error::Parse(_, _)));
        assert!(err.to_string().contains("../outside/*"));
        assert!(err.to_string().contains("cannot escape the workspace root"));
    }

    #[cfg(unix)]
    #[test]
    fn confined_discovery_rejects_symlinks_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let outside = dir.path().join("outside");
        write(
            &workspace.join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        );
        write(&outside.join("package.json"), "{}");
        std::fs::create_dir_all(workspace.join("packages")).unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("packages/linked")).unwrap();

        let err = find_workspace_packages_with_options(
            &workspace,
            WorkspaceDiscoveryOptions::confined_to_root(),
        )
        .unwrap_err();

        assert!(matches!(err, Error::Parse(_, _)));
        assert!(err.to_string().contains("resolves outside"));
    }

    #[cfg(unix)]
    #[test]
    fn confined_discovery_returns_the_validated_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let package = workspace.join("packages/app");
        let link = workspace.join("aliases/app");
        let outside = dir.path().join("outside");
        write(
            &workspace.join("package.json"),
            r#"{"workspaces":["aliases/*"]}"#,
        );
        write(&package.join("package.json"), "{}");
        write(&outside.join("package.json"), "{}");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&package, &link).unwrap();

        let found = find_workspace_packages_with_options(
            &workspace,
            WorkspaceDiscoveryOptions::confined_to_root(),
        )
        .unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        assert_eq!(found, vec![package.canonicalize().unwrap()]);
        assert_ne!(found, vec![link.canonicalize().unwrap()]);
    }

    #[test]
    fn empty_workspace_yaml_remains_authoritative() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("pnpm-workspace.yaml"), "packages: []\n");
        write(
            &dir.path().join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        );
        write(&dir.path().join("packages/app/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn yaml_wins_when_both_present() {
        // If pnpm-workspace.yaml defines packages, the fallback to
        // package.json#workspaces is not consulted — pnpm-style
        // projects treat yaml as source of truth.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'from-yaml/*'\n",
        );
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["from-json/*"]}"#,
        );
        write(&dir.path().join("from-yaml/y/package.json"), "{}");
        write(&dir.path().join("from-json/j/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(names(found), ["y"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn negation_patterns_exclude_matched_directories() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '!**/example/**'\n  - '!**/test/**'\n",
        );
        write(&dir.path().join("packages/keep/package.json"), "{}");
        write(&dir.path().join("packages/example/package.json"), "{}");
        write(&dir.path().join("packages/test/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["keep"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn negation_pattern_excludes_exact_path() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '!packages/legacy'\n",
        );
        write(&dir.path().join("packages/keep/package.json"), "{}");
        write(&dir.path().join("packages/legacy/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["keep"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn negation_excluding_everything_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '!packages/*'\n",
        );
        write(&dir.path().join("packages/a/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn negation_pattern_order_does_not_matter() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - '!**/example/**'\n  - 'packages/*'\n",
        );
        write(&dir.path().join("packages/keep/package.json"), "{}");
        write(&dir.path().join("packages/example/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["keep"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn negation_filters_recursive_positive_glob() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/**'\n  - '!**/test/**'\n",
        );
        write(&dir.path().join("packages/a/package.json"), "{}");
        write(&dir.path().join("packages/a/test/package.json"), "{}");
        write(&dir.path().join("packages/b/sub/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["a", "sub"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn negation_does_not_falsely_match_underscore_dirs() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '!**/_'\n",
        );
        write(&dir.path().join("packages/keep/package.json"), "{}");
        write(&dir.path().join("packages/_/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        let kept = names(found);
        assert!(
            kept.contains("keep"),
            "underscore-targeted negation must not exclude unrelated dirs; got {kept:?}"
        );
        assert!(
            !kept.contains("_"),
            "literal-underscore directory must still be excluded; got {kept:?}"
        );
    }

    #[test]
    fn negation_with_invalid_glob_errors() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '![bad'\n",
        );
        let err = find_workspace_packages(dir.path()).unwrap_err();
        assert!(
            matches!(err, Error::Parse(_, _)),
            "expected Error::Parse, got {err:?}"
        );
    }

    #[test]
    fn missing_package_json_without_yaml_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let found = find_workspace_packages(dir.path()).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn package_json_without_workspaces_field_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("package.json"), r#"{"name":"solo"}"#);
        let found = find_workspace_packages(dir.path()).unwrap();
        assert!(found.is_empty());
    }

    /// Real projects (opencode, for one) list both a glob and an
    /// explicit nested path that the glob already matches. Without
    /// dedup, `link_workspace` later symlinks the same workspace dep
    /// twice into a downstream importer's `node_modules` and fails
    /// with EEXIST on the second write.
    #[test]
    fn overlapping_patterns_dedupe_matched_packages() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*","packages/slack","packages/sdk/js"]}"#,
        );
        write(&dir.path().join("packages/slack/package.json"), "{}");
        write(&dir.path().join("packages/sdk/js/package.json"), "{}");
        write(&dir.path().join("packages/other/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        // slack is matched by both `packages/*` and the explicit
        // `packages/slack`; must appear exactly once.
        let slack_count = found
            .iter()
            .filter(|p| p.ends_with("packages/slack"))
            .count();
        assert_eq!(slack_count, 1, "slack appeared {slack_count} times");
        // Nested-path entries (`packages/sdk/js`) that a simple
        // `packages/*` glob would NOT match still show up via the
        // explicit pattern.
        assert_eq!(
            names(found),
            ["js", "other", "slack"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
    }

    #[test]
    fn recursive_glob_skips_node_modules_subtrees() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/**/*'\n",
        );
        write(&dir.path().join("packages/a/package.json"), "{}");
        write(&dir.path().join("packages/nested/b/package.json"), "{}");
        write(
            &dir.path()
                .join("packages/a/node_modules/not-a-workspace/package.json"),
            "{}",
        );

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["a", "b"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn recursive_glob_with_mid_component_wildcard_uses_parent_root() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/prefix-*/**/*'\n",
        );
        write(&dir.path().join("packages/prefix-a/pkg/package.json"), "{}");
        write(
            &dir.path().join("packages/prefix-b/nested/app/package.json"),
            "{}",
        );
        write(&dir.path().join("packages/other/nope/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["app", "pkg"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn parent_relative_recursive_glob_finds_siblings() {
        // pnpm/test/monorepo/index.ts:996 — `pnpm-workspace.yaml`
        // sits in `monorepo/workspace/` and `packages: ['../**']`
        // sweeps every sibling under `monorepo/`. Aube anchors the
        // walker via the literal prefix so the recursion drops out
        // to the parent dir and `pathdiff` renders the relative
        // import key correctly.
        let dir = tempfile::tempdir().unwrap();
        let workspace_root = dir.path().join("monorepo/workspace");
        write(&workspace_root.join("package.json"), "{}");
        write(
            &workspace_root.join("pnpm-workspace.yaml"),
            "packages:\n  - '../**'\n  - '!../store/**'\n",
        );
        write(
            &dir.path().join("monorepo/package-1/package.json"),
            r#"{"name":"package-1"}"#,
        );
        write(
            &dir.path().join("monorepo/package-2/package.json"),
            r#"{"name":"package-2"}"#,
        );
        write(
            &dir.path().join("monorepo/store/excluded/package.json"),
            r#"{"name":"excluded"}"#,
        );

        let found = find_workspace_packages(&workspace_root).unwrap();
        let canonical: BTreeSet<PathBuf> = found
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect();
        let monorepo = dir.path().join("monorepo").canonicalize().unwrap();
        // Both siblings discovered, `store/excluded` pruned by the
        // negation pattern. The workspace dir itself (`..`/workspace)
        // is also visited by the walker but `install/mod.rs` skips
        // the empty-rel-path importer when it builds `manifests`,
        // so we just assert it doesn't break the discovery.
        assert!(canonical.contains(&monorepo.join("package-1")));
        assert!(canonical.contains(&monorepo.join("package-2")));
        assert!(!canonical.contains(&monorepo.join("store/excluded")));
    }

    #[test]
    fn parent_relative_glob_does_not_loop_on_self_visit() {
        // The walker re-encounters `monorepo/workspace` while
        // recursing under the parent dir. The visited set keys on
        // canonical paths so the second visit terminates instead
        // of re-walking the workspace root in an unbounded loop.
        let dir = tempfile::tempdir().unwrap();
        let workspace_root = dir.path().join("ws");
        write(&workspace_root.join("package.json"), "{}");
        write(
            &workspace_root.join("pnpm-workspace.yaml"),
            "packages:\n  - '../**'\n",
        );
        // Many siblings — without dedupe the workspace dir would
        // be re-walked from inside the parent recursion and the
        // visited set would balloon. Cap the assertion at a sane
        // upper bound rather than guessing the exact count, since
        // tempdir layout (e.g. macOS `/private/var` symlink) can
        // affect whether the workspace itself ends up in the set.
        for i in 0..20 {
            write(&dir.path().join(format!("sib-{i}/package.json")), "{}");
        }
        let found = find_workspace_packages(&workspace_root).unwrap();
        assert!(
            (20..=21).contains(&found.len()),
            "expected 20 or 21 packages (siblings + optional self), got {}",
            found.len()
        );
    }

    #[test]
    fn is_workspace_project_root_detects_yaml_only() {
        // pnpm-workspace.yaml present, packages: empty (or matches
        // nothing on disk). `find_workspace_packages` returns [] —
        // but the project IS still a workspace, and downstream
        // workspace-shaped behavior (lockfile importer pruning,
        // workspace yaml-only validation) needs to know.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'absent/*'\n",
        );
        assert!(is_workspace_project_root(dir.path()));
        assert!(find_workspace_packages(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn is_workspace_project_root_detects_package_json_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        );
        assert!(is_workspace_project_root(dir.path()));
    }

    #[test]
    fn is_workspace_project_root_returns_false_when_neither_marker_present() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"single","version":"1.0.0"}"#,
        );
        assert!(!is_workspace_project_root(dir.path()));
    }
}
