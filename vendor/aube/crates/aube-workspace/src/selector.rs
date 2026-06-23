//! Pnpm-style `--filter` selectors for workspace packages.
//!
//! Supported forms:
//! - `name` — exact package-name match
//! - `@scope/*`, `foo-*`, `*-plugin` — glob-style name match
//! - `./path` or `path/` — match packages whose directory is at or under the
//!   given path (relative to the workspace root)
//! - `foo...` / `foo^...` — include dependencies / only dependencies
//! - `...foo` / `...^foo` — include dependents / only dependents
//! - `[origin/main]` — packages touched since a git ref
//! - `!foo` — exclude a selector from the final set
//!
//! `--filter-prod <pattern>` selectors go through the same parser but mark
//! each selector as `prod_only`. Graph traversal (`foo...`, `...foo`) then
//! walks only `dependencies` / `optionalDependencies` / `peerDependencies`
//! edges, skipping `devDependencies` and everything reachable solely
//! through them — matching pnpm's `--filter-prod` semantics.

use aube_manifest::{PackageJson, WorkspaceConfig};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The effective `--filter` / `--filter-prod` input for a single command
/// invocation. Commands receive this after `compute_effective_filter` in
/// `aube/src/main.rs` has merged the global `-r` wildcard into
/// `filters` and pulled `filter_prods` from the separate global flag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveFilter {
    /// Raw `--filter` / `-F` values (plus the `*` wildcard when `-r` was
    /// passed without an explicit selector).
    pub filters: Vec<String>,
    /// Raw `--filter-prod` values. These apply the same selector forms as
    /// `filters` but restrict graph walks to production edges.
    pub filter_prods: Vec<String>,
    /// `--fail-if-no-match` — promote "no projects matched" from a warning
    /// to a hard error. pnpm's default is to warn and exit 0; this flag
    /// (mirrored) opts into the strict behavior for CI use.
    pub fail_if_no_match: bool,
    /// Include the workspace root alongside the selected workspace packages.
    pub include_workspace_root: bool,
}

impl EffectiveFilter {
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty() && self.filter_prods.is_empty()
    }

    /// Build an `EffectiveFilter` from just `--filter` values. Useful for
    /// tests and for callers that never cared about `--filter-prod`.
    pub fn from_filters<I>(filters: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        Self {
            filters: filters.into_iter().map(Into::into).collect(),
            filter_prods: Vec::new(),
            fail_if_no_match: false,
            include_workspace_root: false,
        }
    }
}

/// A single parsed selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    base: BaseSelector,
    include_dependencies: bool,
    include_dependents: bool,
    exclude_self: bool,
    exclude: bool,
    /// Originates from `--filter-prod`: graph walks skip `devDependencies`.
    prod_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BaseSelector {
    /// Exact package-name match.
    Name(String),
    /// Glob against the package name (e.g. `@scope/*`).
    NameGlob(String),
    /// Path selector rooted at the workspace directory. Matches packages
    /// whose directory equals or is nested under this path.
    Path(PathBuf),
    /// Packages with files changed relative to a git ref.
    ChangedSince(String),
}

impl Selector {
    /// Parse a raw `--filter` argument.
    pub fn parse(raw: &str) -> Result<Self, ParseError> {
        Self::parse_kind(raw, false)
    }

    /// Parse a raw `--filter-prod` argument. Same grammar as `parse`, but
    /// the resulting selector's graph walks skip `devDependencies`.
    pub fn parse_prod(raw: &str) -> Result<Self, ParseError> {
        Self::parse_kind(raw, true)
    }

    fn parse_kind(raw: &str, prod_only: bool) -> Result<Self, ParseError> {
        if raw.is_empty() {
            return Err(ParseError::Empty);
        }
        let (exclude, raw) = raw
            .strip_prefix('!')
            .map(|s| (true, s))
            .unwrap_or((false, raw));
        if raw.is_empty() {
            return Err(ParseError::Empty);
        }

        let (include_dependents, raw) = raw
            .strip_prefix("...")
            .map(|s| (true, s))
            .unwrap_or((false, raw));
        let (exclude_self_from_dependents, raw) = raw
            .strip_prefix('^')
            .map(|s| (true, s))
            .unwrap_or((false, raw));
        let (include_dependencies, raw) = raw
            .strip_suffix("...")
            .map(|s| (true, s))
            .unwrap_or((false, raw));
        let (exclude_self_from_dependencies, raw) = raw
            .strip_suffix('^')
            .map(|s| (true, s))
            .unwrap_or((false, raw));
        if raw.is_empty() {
            return Err(ParseError::Empty);
        }
        let exclude_self = exclude_self_from_dependents || exclude_self_from_dependencies;

        let base = if raw.starts_with('[') && raw.ends_with(']') && raw.len() > 2 {
            BaseSelector::ChangedSince(raw[1..raw.len() - 1].to_string())
        }
        // Path-style selectors: leading `./`, `../`, `/`, or a trailing `/`.
        // The pnpm-style `./packages/**` "directory and all descendants"
        // form already enters this branch via the `./` prefix; we then
        // strip the `/**` suffix so it collapses to the same `./packages`
        // path. We deliberately do NOT also accept a bare `/**` suffix
        // here — `@scope/**` and `name/**` are name globs and must keep
        // routing through the `NameGlob` branch below.
        else if raw.starts_with("./")
            || raw.starts_with("../")
            || raw.starts_with('/')
            || raw.ends_with('/')
        {
            let trimmed = raw.strip_suffix("/**").unwrap_or(raw).trim_end_matches('/');
            // Strip a leading `./` so the stored PathBuf has no CurDir
            // component. `Path::components` normalizes mid-path `CurDir`
            // already, but keeping the stored form canonical makes the
            // matcher's `starts_with` contract obvious at a glance.
            let normalized = trimmed.strip_prefix("./").unwrap_or(trimmed);
            BaseSelector::Path(PathBuf::from(normalized))
        } else
        // Only `*` and `?` are recognized glob metacharacters. Bracket
        // expressions (`[ab]-pkg`) are intentionally treated as literal
        // names — `glob_match` doesn't implement them, so detecting `[`
        // here would only produce silent mismatches.
        if raw.contains('*') || raw.contains('?') {
            BaseSelector::NameGlob(raw.to_string())
        } else {
            BaseSelector::Name(raw.to_string())
        };

        Ok(Selector {
            base,
            include_dependencies,
            include_dependents,
            exclude_self,
            exclude,
            prod_only,
        })
    }

    /// Test whether this selector matches a workspace package.
    pub fn matches(&self, pkg: &WorkspacePkg<'_>) -> bool {
        match &self.base {
            BaseSelector::Name(n) => pkg.name == Some(n.as_str()),
            BaseSelector::NameGlob(pat) => match pkg.name {
                Some(name) => glob_match(pat, name),
                None => false,
            },
            BaseSelector::Path(p) => {
                let target = pkg.workspace_root.join(p);
                // Normalize both sides by stripping trailing slashes; we
                // don't canonicalize because the directories must exist on
                // disk for the workspace walk to have found them anyway.
                pkg.dir.starts_with(&target)
            }
            BaseSelector::ChangedSince(_) => false,
        }
    }
}

/// Lightweight view of a workspace package for matching.
pub struct WorkspacePkg<'a> {
    pub name: Option<&'a str>,
    pub dir: &'a Path,
    pub workspace_root: &'a Path,
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ParseError {
    #[error("empty --filter selector")]
    #[diagnostic(code(ERR_AUBE_FILTER_EMPTY))]
    Empty,
}

/// Parse the combined `--filter` + `--filter-prod` inputs into a single
/// ordered selector list where each selector carries its `prod_only`
/// flag. Exclusions (`!pkg`) from either list apply to the whole set.
pub fn parse_effective(filter: &EffectiveFilter) -> Result<Vec<Selector>, ParseError> {
    let mut out = Vec::with_capacity(filter.filters.len() + filter.filter_prods.len());
    for raw in &filter.filters {
        out.push(Selector::parse(raw)?);
    }
    for raw in &filter.filter_prods {
        out.push(Selector::parse_prod(raw)?);
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct SelectedPackage {
    pub name: Option<String>,
    pub version: Option<String>,
    pub private: bool,
    pub dir: PathBuf,
    pub manifest: PackageJson,
}

struct IndexedPackage {
    selected: SelectedPackage,
    /// Every declared workspace-sibling-capable dep name (prod + dev +
    /// optional + peer). Used by default graph walks.
    all_deps: BTreeSet<String>,
    /// Production subset: `dependencies` + `optionalDependencies` +
    /// `peerDependencies`. `--filter-prod` graph walks use this.
    prod_deps: BTreeSet<String>,
}

pub fn select_workspace_packages(
    workspace_root: &Path,
    workspace_pkgs: &[PathBuf],
    filter: &EffectiveFilter,
) -> Result<Vec<SelectedPackage>, SelectError> {
    let selectors = parse_effective(filter).map_err(SelectError::Parse)?;
    let packages = index_packages(workspace_pkgs);
    if selectors.is_empty() {
        let mut selected: Vec<_> = packages.into_iter().map(|p| p.selected).collect();
        include_workspace_root(workspace_root, filter, &mut selected)?;
        return Ok(selected);
    }

    let has_positive = selectors.iter().any(|s| !s.exclude);
    let mut included: BTreeSet<usize> = if has_positive {
        BTreeSet::new()
    } else {
        (0..packages.len()).collect()
    };
    let mut excluded: BTreeSet<usize> = BTreeSet::new();
    for selector in &selectors {
        let matches = expand_selector(workspace_root, &packages, selector)?;
        if selector.exclude {
            excluded.extend(matches);
        } else {
            included.extend(matches);
        }
    }
    for idx in excluded {
        included.remove(&idx);
    }

    let mut selected: Vec<_> = packages
        .into_iter()
        .enumerate()
        .filter_map(|(idx, pkg)| included.contains(&idx).then_some(pkg.selected))
        .collect();
    include_workspace_root(workspace_root, filter, &mut selected)?;
    Ok(selected)
}

fn index_packages(workspace_pkgs: &[PathBuf]) -> Vec<IndexedPackage> {
    let mut packages = Vec::new();
    for dir in workspace_pkgs {
        let Ok(manifest) = PackageJson::from_path(&dir.join("package.json")) else {
            continue;
        };
        let prod_deps: BTreeSet<String> = manifest
            .dependencies
            .keys()
            .chain(manifest.optional_dependencies.keys())
            .chain(manifest.peer_dependencies.keys())
            .cloned()
            .collect();
        let all_deps: BTreeSet<String> = prod_deps
            .iter()
            .cloned()
            .chain(manifest.dev_dependencies.keys().cloned())
            .collect();
        let private = manifest
            .extra
            .get("private")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        packages.push(IndexedPackage {
            selected: SelectedPackage {
                name: manifest.name.clone(),
                version: manifest.version.clone(),
                private,
                dir: dir.clone(),
                manifest,
            },
            all_deps,
            prod_deps,
        });
    }
    packages
}

fn include_workspace_root(
    workspace_root: &Path,
    filter: &EffectiveFilter,
    selected: &mut Vec<SelectedPackage>,
) -> Result<(), SelectError> {
    let include_workspace_root = filter.include_workspace_root
        || WorkspaceConfig::load(workspace_root)
            .map_err(SelectError::WorkspaceConfig)?
            .include_workspace_root
            .unwrap_or(false);
    if !include_workspace_root || !workspace_root.join("package.json").is_file() {
        return Ok(());
    }
    if selected.iter().any(|pkg| pkg.dir == workspace_root) {
        return Ok(());
    }

    let manifest = PackageJson::from_path(&workspace_root.join("package.json"))
        .map_err(SelectError::Manifest)?;
    selected.push(SelectedPackage {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        private: manifest
            .extra
            .get("private")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        dir: workspace_root.to_path_buf(),
        manifest,
    });
    Ok(())
}

fn expand_selector(
    workspace_root: &Path,
    packages: &[IndexedPackage],
    selector: &Selector,
) -> Result<BTreeSet<usize>, SelectError> {
    let mut seeds = match &selector.base {
        BaseSelector::ChangedSince(rev) => changed_since(workspace_root, packages, rev)?,
        _ => packages
            .iter()
            .enumerate()
            .filter_map(|(idx, pkg)| {
                let view = WorkspacePkg {
                    name: pkg.selected.name.as_deref(),
                    dir: &pkg.selected.dir,
                    workspace_root,
                };
                selector.matches(&view).then_some(idx)
            })
            .collect(),
    };
    let original_seeds = seeds.clone();

    if selector.include_dependencies {
        seeds.extend(walk_dependencies(
            packages,
            &original_seeds,
            selector.prod_only,
        ));
    }
    if selector.include_dependents {
        seeds.extend(walk_dependents(
            packages,
            &original_seeds,
            selector.prod_only,
        ));
    }
    if selector.exclude_self {
        for idx in original_seeds {
            seeds.remove(&idx);
        }
    }
    Ok(seeds)
}

fn name_index(packages: &[IndexedPackage]) -> BTreeMap<&str, usize> {
    packages
        .iter()
        .enumerate()
        .filter_map(|(idx, pkg)| pkg.selected.name.as_deref().map(|n| (n, idx)))
        .collect()
}

/// Return the edge set to traverse from a package. `prod_only=true` drops
/// `devDependencies` edges — matching pnpm's `--filter-prod` semantics.
fn outgoing_deps(pkg: &IndexedPackage, prod_only: bool) -> &BTreeSet<String> {
    if prod_only {
        &pkg.prod_deps
    } else {
        &pkg.all_deps
    }
}

fn walk_dependencies(
    packages: &[IndexedPackage],
    seeds: &BTreeSet<usize>,
    prod_only: bool,
) -> BTreeSet<usize> {
    let names = name_index(packages);
    let mut out = BTreeSet::new();
    let mut q: VecDeque<usize> = seeds.iter().copied().collect();
    while let Some(idx) = q.pop_front() {
        for dep_name in outgoing_deps(&packages[idx], prod_only) {
            let Some(dep_idx) = names.get(dep_name.as_str()).copied() else {
                continue;
            };
            if out.insert(dep_idx) {
                q.push_back(dep_idx);
            }
        }
    }
    out
}

fn walk_dependents(
    packages: &[IndexedPackage],
    seeds: &BTreeSet<usize>,
    prod_only: bool,
) -> BTreeSet<usize> {
    let mut rev_index: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (idx, pkg) in packages.iter().enumerate() {
        for dep_name in outgoing_deps(pkg, prod_only) {
            rev_index.entry(dep_name.as_str()).or_default().push(idx);
        }
    }
    let mut out = BTreeSet::new();
    let mut q: VecDeque<usize> = seeds.iter().copied().collect();
    while let Some(idx) = q.pop_front() {
        let Some(name) = packages[idx].selected.name.as_deref() else {
            continue;
        };
        if let Some(dependents) = rev_index.get(name) {
            for &dep_idx in dependents {
                if out.insert(dep_idx) {
                    q.push_back(dep_idx);
                }
            }
        }
    }
    out
}

fn changed_since(
    workspace_root: &Path,
    packages: &[IndexedPackage],
    rev: &str,
) -> Result<BTreeSet<usize>, SelectError> {
    // `git diff <revspec> -- <paths>` can't accept a `--` terminator
    // before the revspec, so a rev that begins with `-` would land as
    // an option. Reject at the boundary as defense against the
    // CVE-2017-1000117 class of argv injection. NUL is rejected too
    // because it never appears in a legitimate ref.
    if rev.starts_with('-') {
        return Err(SelectError::GitFailed(format!(
            "refusing to pass revspec starting with `-` to git: {rev:?}"
        )));
    }
    if rev.contains('\0') {
        return Err(SelectError::GitFailed(
            "refusing to pass revspec containing NUL byte to git".to_string(),
        ));
    }
    let revspec = format!("{rev}...HEAD");
    let git_root = git_root(workspace_root)?;
    let output = Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg(&revspec)
        .arg("--")
        .current_dir(&git_root)
        .output()
        .map_err(SelectError::GitIo)?;
    if !output.status.success() {
        return Err(SelectError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let mut out = BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let changed = git_root.join(line);
        for (idx, pkg) in packages.iter().enumerate() {
            if changed.starts_with(&pkg.selected.dir) {
                out.insert(idx);
            }
        }
    }
    Ok(out)
}

fn git_root(workspace_root: &Path) -> Result<PathBuf, SelectError> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(workspace_root)
        .output()
        .map_err(SelectError::GitIo)?;
    if !output.status.success() {
        return Err(SelectError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum SelectError {
    #[error("{0}")]
    #[diagnostic(transparent)]
    Parse(#[from] ParseError),
    #[error("failed to load workspace config: {0}")]
    WorkspaceConfig(#[source] aube_manifest::Error),
    #[error("failed to read workspace root package.json: {0}")]
    Manifest(#[source] aube_manifest::Error),
    #[error("failed to run git for [ref] filter: {0}")]
    #[diagnostic(code(ERR_AUBE_FILTER_GIT_IO))]
    GitIo(std::io::Error),
    #[error("git [ref] filter failed: {0}")]
    #[diagnostic(code(ERR_AUBE_FILTER_GIT_FAILED))]
    GitFailed(String),
}

/// Minimal glob matcher supporting `*` (any run of chars) and `?` (one
/// char). We deliberately avoid pulling in the `glob` crate's `Pattern`
/// here because it's tuned for paths, not package names, and chokes on
/// `/` inside scoped names like `@babel/*`.
fn glob_match(pattern: &str, s: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = s.chars().collect();
    fn inner(pat: &[char], text: &[char]) -> bool {
        let mut pi = 0;
        let mut ti = 0;
        let mut star: Option<(usize, usize)> = None;
        while ti < text.len() {
            if pi < pat.len() && (pat[pi] == '?' || pat[pi] == text[ti]) {
                pi += 1;
                ti += 1;
            } else if pi < pat.len() && pat[pi] == '*' {
                star = Some((pi, ti));
                pi += 1;
            } else if let Some((sp, st)) = star {
                pi = sp + 1;
                ti = st + 1;
                star = Some((sp, ti));
            } else {
                return false;
            }
        }
        while pi < pat.len() && pat[pi] == '*' {
            pi += 1;
        }
        pi == pat.len()
    }
    inner(&pat, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forms() {
        assert_eq!(
            Selector::parse("foo").unwrap(),
            Selector {
                base: BaseSelector::Name("foo".into()),
                include_dependencies: false,
                include_dependents: false,
                exclude_self: false,
                exclude: false,
                prod_only: false,
            }
        );
        assert_eq!(
            Selector::parse("@babel/core").unwrap(),
            Selector {
                base: BaseSelector::Name("@babel/core".into()),
                include_dependencies: false,
                include_dependents: false,
                exclude_self: false,
                exclude: false,
                prod_only: false,
            }
        );
        assert_eq!(
            Selector::parse("@babel/*").unwrap(),
            Selector {
                base: BaseSelector::NameGlob("@babel/*".into()),
                include_dependencies: false,
                include_dependents: false,
                exclude_self: false,
                exclude: false,
                prod_only: false,
            }
        );
        assert_eq!(
            Selector::parse("./packages/a").unwrap(),
            Selector {
                base: BaseSelector::Path(PathBuf::from("packages/a")),
                include_dependencies: false,
                include_dependents: false,
                exclude_self: false,
                exclude: false,
                prod_only: false,
            }
        );
        // `./packages/**` is the pnpm "directory and all descendants"
        // form; aube collapses it to the same path because the matcher
        // is already "at or under".
        assert_eq!(
            Selector::parse("./packages/**").unwrap(),
            Selector {
                base: BaseSelector::Path(PathBuf::from("packages")),
                include_dependencies: false,
                include_dependents: false,
                exclude_self: false,
                exclude: false,
                prod_only: false,
            }
        );
        // A bare `<name>/**` (no path-y prefix) must keep routing
        // through `NameGlob` — `@scope/**` is a scoped name glob, not
        // a path.
        assert_eq!(
            Selector::parse("@scope/**").unwrap(),
            Selector {
                base: BaseSelector::NameGlob("@scope/**".into()),
                include_dependencies: false,
                include_dependents: false,
                exclude_self: false,
                exclude: false,
                prod_only: false,
            }
        );
        assert!(Selector::parse("").is_err());
    }

    #[test]
    fn parse_prod_sets_prod_only() {
        let sel = Selector::parse_prod("foo...").unwrap();
        assert!(sel.prod_only);
        assert!(sel.include_dependencies);
        assert_eq!(sel.base, BaseSelector::Name("foo".into()));
        assert!(!Selector::parse("foo...").unwrap().prod_only);
    }

    #[test]
    fn prod_graph_walk_skips_dev_deps() {
        // Build three workspace packages:
        //   api      → depends on lib (prod) and tooling (dev)
        //   lib      → no deps
        //   tooling  → no deps
        // A regular `api...` walk should reach both `lib` and `tooling`;
        // a `--filter-prod` `api...` walk should reach only `lib`.
        let mk_pkg = |name: &str, prod: &[&str], dev: &[&str]| -> IndexedPackage {
            let manifest = aube_manifest::PackageJson {
                name: Some(name.to_string()),
                ..aube_manifest::PackageJson::default()
            };
            IndexedPackage {
                selected: SelectedPackage {
                    name: Some(name.to_string()),
                    version: None,
                    private: false,
                    dir: PathBuf::from(format!("/ws/{name}")),
                    manifest,
                },
                all_deps: prod
                    .iter()
                    .chain(dev.iter())
                    .map(|s| (*s).to_string())
                    .collect(),
                prod_deps: prod.iter().map(|s| (*s).to_string()).collect(),
            }
        };
        let packages = vec![
            mk_pkg("api", &["lib"], &["tooling"]),
            mk_pkg("lib", &[], &[]),
            mk_pkg("tooling", &[], &[]),
        ];

        let mut seeds = BTreeSet::new();
        seeds.insert(0); // api

        let full = walk_dependencies(&packages, &seeds, false);
        assert_eq!(full, BTreeSet::from([1, 2]));

        let prod = walk_dependencies(&packages, &seeds, true);
        assert_eq!(prod, BTreeSet::from([1]));

        // walk_dependents with prod_only should also skip dev edges: the
        // only dependent of `tooling` is `api` via a dev edge, so prod
        // mode should return an empty set.
        let mut tool_seeds = BTreeSet::new();
        tool_seeds.insert(2);
        let tool_all = walk_dependents(&packages, &tool_seeds, false);
        assert_eq!(tool_all, BTreeSet::from([0]));
        let tool_prod = walk_dependents(&packages, &tool_seeds, true);
        assert!(tool_prod.is_empty());
    }

    #[test]
    fn parse_graph_forms() {
        let deps = Selector::parse("foo...").unwrap();
        assert_eq!(deps.base, BaseSelector::Name("foo".into()));
        assert!(deps.include_dependencies);
        assert!(!deps.include_dependents);
        assert!(!deps.exclude_self);

        let only_deps = Selector::parse("foo^...").unwrap();
        assert!(only_deps.include_dependencies);
        assert!(only_deps.exclude_self);

        let dependents = Selector::parse("...foo").unwrap();
        assert!(dependents.include_dependents);

        let only_dependents = Selector::parse("...^foo").unwrap();
        assert!(only_dependents.include_dependents);
        assert!(only_dependents.exclude_self);
    }

    #[test]
    fn glob_matches_names() {
        assert!(glob_match("@babel/*", "@babel/core"));
        assert!(glob_match("@babel/*", "@babel/preset-env"));
        assert!(!glob_match("@babel/*", "@babel-x/core"));
        assert!(glob_match("foo-*", "foo-bar"));
        assert!(glob_match("*-plugin", "a-plugin"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("foo", "foobar"));
    }

    #[test]
    fn path_selector_matches_nested() {
        let root = Path::new("/ws");
        let sel = Selector::parse("./packages").unwrap();
        let dir = PathBuf::from("/ws/packages/a");
        assert!(sel.matches(&WorkspacePkg {
            name: Some("a"),
            dir: &dir,
            workspace_root: root,
        }));
    }

    #[test]
    fn include_workspace_root_adds_root_even_when_filter_matches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"root","version":"0.0.0","private":true}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\nincludeWorkspaceRoot: true\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("packages/app")).unwrap();
        std::fs::write(
            dir.path().join("packages/app/package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();

        let selected = select_workspace_packages(
            dir.path(),
            &[dir.path().join("packages/app")],
            &EffectiveFilter::from_filters(["missing"]),
        )
        .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name.as_deref(), Some("root"));
        assert_eq!(selected[0].dir, dir.path());
    }

    #[test]
    fn include_workspace_root_yaml_applies_with_no_selectors() {
        // Regression: when both `filters` and `filter_prods` are empty,
        // `select_workspace_packages` short-circuits — root inclusion
        // (CLI flag or YAML) must still apply on that path.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"root","version":"0.0.0","private":true}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\nincludeWorkspaceRoot: true\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("packages/app")).unwrap();
        std::fs::write(
            dir.path().join("packages/app/package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();

        let selected = select_workspace_packages(
            dir.path(),
            &[dir.path().join("packages/app")],
            &EffectiveFilter::default(),
        )
        .unwrap();

        let names: Vec<_> = selected
            .iter()
            .filter_map(|pkg| pkg.name.as_deref())
            .collect();
        assert_eq!(names, vec!["app", "root"]);
    }

    #[test]
    fn include_workspace_root_flag_adds_root_to_regular_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"root","version":"0.0.0","private":true}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("packages/app")).unwrap();
        std::fs::write(
            dir.path().join("packages/app/package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();
        let mut filter = EffectiveFilter::from_filters(["app"]);
        filter.include_workspace_root = true;

        let selected =
            select_workspace_packages(dir.path(), &[dir.path().join("packages/app")], &filter)
                .unwrap();
        let names: Vec<_> = selected
            .iter()
            .filter_map(|pkg| pkg.name.as_deref())
            .collect();

        assert_eq!(names, vec!["app", "root"]);
    }

    #[test]
    fn changed_since_rejects_dash_prefixed_rev() {
        // CVE-2017-1000117 class: a rev beginning with `-` would be
        // interpreted by `git diff` as an option because the
        // subcommand does not accept a `--` terminator before the
        // revspec. Reject at the boundary before the format! call.
        // The validation runs before `git_root`, so `workspace_root`
        // does not need to exist for this test.
        let err =
            changed_since(Path::new("/nonexistent"), &[], "--upload-pack=/tmp/evil").unwrap_err();
        let msg = match err {
            SelectError::GitFailed(m) => m,
            other => panic!("expected GitFailed, got {other:?}"),
        };
        assert!(msg.contains("refusing"), "unexpected error: {msg}");
    }

    #[test]
    fn changed_since_rejects_nul_in_rev() {
        let err = changed_since(Path::new("/nonexistent"), &[], "main\0evil").unwrap_err();
        let msg = match err {
            SelectError::GitFailed(m) => m,
            other => panic!("expected GitFailed, got {other:?}"),
        };
        assert!(msg.contains("refusing"), "unexpected error: {msg}");
    }
}
