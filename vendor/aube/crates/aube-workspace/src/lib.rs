//! Workspace support for aube.
//!
//! Reads pnpm-workspace.yaml to discover workspace packages, falling back
//! to `package.json`'s `workspaces` field (yarn/npm/bun shape) when no
//! yaml is present. Supports the `workspace:` protocol for inter-package
//! dependencies.

pub mod selector;
pub mod topo;

use std::path::{Path, PathBuf};

pub use aube_manifest::workspace::WorkspaceConfig;
pub use selector::{Selector, WorkspacePkg};

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
    let config = WorkspaceConfig::load(project_dir).map_err(|e| match e {
        aube_manifest::Error::Io(p, e) => Error::Io(p, e),
        aube_manifest::Error::YamlParse(p, e) => Error::Parse(p, e),
        aube_manifest::Error::Parse(pe) => Error::ParseDiag(pe),
    })?;

    let patterns: Vec<String> = if !config.packages.is_empty() {
        config.packages.clone()
    } else {
        package_json_workspace_patterns(project_dir)?
    };

    if patterns.is_empty() {
        return Ok(vec![]);
    }

    let mut neg_matchers = Vec::new();
    let mut positives = Vec::new();
    for raw in &patterns {
        if let Some(rest) = raw.strip_prefix('!') {
            let mk = |p: &str| {
                glob::Pattern::new(p).map_err(|e| {
                    Error::Parse(project_dir.join("pnpm-workspace.yaml"), e.to_string())
                })
            };
            // pnpm uses micromatch where `**` matches zero-or-more
            // path components, so `!**/example/**` excludes the
            // directory `example` itself. The `glob` crate requires
            // `**` to consume at least one component, so emit a
            // companion matcher with the trailing `/**` stripped to
            // catch the directory itself in addition to its descendants.
            neg_matchers.push(mk(rest)?);
            if let Some(self_form) = rest.strip_suffix("/**") {
                neg_matchers.push(mk(self_form)?);
            }
        } else {
            positives.push(raw.as_str());
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
        for pkg_dir in expand_workspace_pattern(project_dir, pattern)? {
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
            if seen.insert(pkg_dir.clone()) {
                packages.push(pkg_dir);
            }
        }
    }

    packages.sort_unstable();
    Ok(packages)
}

fn expand_workspace_pattern(project_dir: &Path, pattern: &str) -> Result<Vec<PathBuf>, Error> {
    let matcher = glob::Pattern::new(pattern)
        .map_err(|e| Error::Parse(project_dir.join("pnpm-workspace.yaml"), e.to_string()))?;
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
        use std::path::Component;
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
