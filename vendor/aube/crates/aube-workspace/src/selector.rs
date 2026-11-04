//! pnpm's `--filter` / `--filter-prod` selection of workspace projects.
//!
//! The grammar ([`parse_project_selector`]) and the directory glob
//! ([`dir_glob`]) are ports of pnpm 12's `workspace-projects-filter`, and
//! the selection below follows its `filter_workspace_projects`:
//!
//! - every workspace project is a candidate, the root included;
//! - selectors apply in order, in runs of the same polarity: an inclusion
//!   run adds, an exclusion run removes, and a first run that excludes
//!   starts from every project;
//! - a directory selector (`./packages/*`, `{packages/a}`, `../x`) is a glob
//!   against the project directory, resolved against the directory the
//!   command runs in, so `./packages` selects a project at `packages`
//!   itself and none below it;
//! - `--filter-prod` selectors walk production edges only, in a pass of
//!   their own whose result joins the regular one;
//! - commands that drop the root from a recursive run (`add`, `run`,
//!   `exec`, `test`) append pnpm's `!{<root>}` unless an inclusion selector,
//!   `includeWorkspaceRoot`, or a root-only workspace keeps it.

mod dir_glob;
mod project_selector;

pub use project_selector::{DependencyTraversal, ProjectSelector, parse_project_selector};

use aube_manifest::{PackageJson, WorkspaceConfig};
use dir_glob::DirGlob;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// The effective `--filter` / `--filter-prod` input for a single command
/// invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveFilter {
    /// Raw `--filter` / `-F` values (plus the `*` that stands for `-r`
    /// when it was passed without an explicit selector; see
    /// [`Self::recursive`]).
    pub filters: Vec<String>,
    /// Raw `--filter-prod` values. These apply the same selector forms as
    /// `filters` but restrict graph walks to production edges.
    pub filter_prods: Vec<String>,
    /// `--fail-if-no-match` — promote "no projects matched" from a warning
    /// to a hard error.
    pub fail_if_no_match: bool,
    /// `--include-workspace-root`: keep the root in a recursive selection
    /// that would otherwise drop it. As in pnpm, it adds nothing to a
    /// selection an inclusion selector already pins.
    pub include_workspace_root: bool,
    /// `-r` with no selector. The caller still puts a `*` in `filters` so
    /// the command takes its workspace path; this marks that `*` as "every
    /// project" rather than the name glob a user-written `*` is, which
    /// differs for an unnamed project and for the root's auto-exclusion.
    pub recursive: bool,
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
            ..Self::default()
        }
    }

    /// `-r` alone: no selector narrows the run.
    fn unnarrowed(&self) -> bool {
        self.recursive && self.filter_prods.is_empty() && self.filters == ["*"]
    }
}

/// How the command selecting projects treats the workspace root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSelection {
    /// The root is a candidate like any other project (`install`,
    /// `remove`, `update`, `ls`, ...).
    Keep,
    /// pnpm's `AutoExcludeRoot` for `add`, `run`, `exec` and `test`: a
    /// selection no inclusion selector pins leaves the root out.
    AutoExclude,
}

/// Where selectors resolve and how the root is treated.
#[derive(Debug, Clone, Copy)]
pub struct SelectOptions<'a> {
    /// The directory a relative directory selector resolves against: the
    /// one the command runs in.
    pub prefix: &'a Path,
    pub root: RootSelection,
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ParseError {
    #[error("empty --filter selector")]
    #[diagnostic(code(ERR_AUBE_FILTER_EMPTY))]
    Empty,
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

/// The workspace projects `filter` selects, in discovery order with the
/// root last. `workspace_pkgs` are the discovered members; the root joins
/// them as a candidate whenever it has a `package.json`.
pub fn select_workspace_projects(
    workspace_root: &Path,
    workspace_pkgs: &[PathBuf],
    filter: &EffectiveFilter,
    opts: SelectOptions<'_>,
) -> Result<Vec<SelectedPackage>, SelectError> {
    let mut packages = index_packages(workspace_pkgs);
    let root_idx = index_root(workspace_root, &mut packages)?;
    if filter.is_empty() {
        let keep_root = include_root_setting(workspace_root, filter)?;
        return Ok(packages
            .into_iter()
            .enumerate()
            .filter(|(idx, _)| keep_root || Some(*idx) != root_idx)
            .map(|(_, pkg)| pkg.selected)
            .collect());
    }

    let mut regular: Vec<ProjectSelector> = if filter.unnarrowed() {
        Vec::new()
    } else {
        parse_all(&filter.filters, opts.prefix, false)
    };
    let mut prod = parse_all(&filter.filter_prods, opts.prefix, true);
    let has_inclusion = regular.iter().chain(&prod).any(|s| !s.exclude);
    if opts.root == RootSelection::AutoExclude
        && root_idx.is_some()
        && !has_inclusion
        && !keeps_workspace_root(workspace_root, filter)?
    {
        let mut root_selector = ProjectSelector {
            exclude: true,
            parent_dir: Some(normalize(workspace_root)),
            ..ProjectSelector::default()
        };
        if prod.is_empty() {
            regular.push(root_selector);
        } else {
            root_selector.follow_prod_deps_only = true;
            prod.push(root_selector);
        }
    }

    let mut selected = BTreeSet::new();
    if regular.is_empty() && prod.is_empty() {
        selected.extend(0..packages.len());
    }
    for selectors in [&regular, &prod] {
        if !selectors.is_empty() {
            selected.extend(filter_projects(workspace_root, &packages, selectors)?);
        }
    }
    Ok(packages
        .into_iter()
        .enumerate()
        .filter_map(|(idx, pkg)| selected.contains(&idx).then_some(pkg.selected))
        .collect())
}

fn parse_all(raw: &[String], prefix: &Path, prod: bool) -> Vec<ProjectSelector> {
    raw.iter()
        .map(|raw| {
            let mut selector = parse_project_selector(raw, prefix);
            selector.follow_prod_deps_only = prod;
            selector
        })
        .collect()
}

/// pnpm's `filter_workspace_projects`: selectors in runs of one polarity,
/// a leading exclusion run starting from every project.
fn filter_projects(
    workspace_root: &Path,
    packages: &[IndexedPackage],
    selectors: &[ProjectSelector],
) -> Result<BTreeSet<usize>, SelectError> {
    let mut selected: BTreeSet<usize> = if selectors[0].exclude {
        (0..packages.len()).collect()
    } else {
        BTreeSet::new()
    };
    for run in selectors.chunk_by(|a, b| a.exclude == b.exclude) {
        let mut walk = BTreeSet::new();
        for selector in run {
            let entries = selector_entries(workspace_root, packages, selector)?;
            select_entries(packages, selector, &entries, &mut walk);
        }
        if run[0].exclude {
            selected.retain(|idx| !walk.contains(idx));
        } else {
            selected.extend(walk);
        }
    }
    Ok(selected)
}

/// The projects a selector names before its `...` / `^` traversal.
fn selector_entries(
    workspace_root: &Path,
    packages: &[IndexedPackage],
    selector: &ProjectSelector,
) -> Result<BTreeSet<usize>, SelectError> {
    let mut entries: Option<BTreeSet<usize>> = if let Some(diff) = &selector.diff {
        Some(changed_since(
            workspace_root,
            packages,
            diff,
            selector.parent_dir.as_deref(),
        )?)
    } else {
        selector.parent_dir.as_ref().map(|dir| {
            let glob = DirGlob::new(&dir.to_string_lossy());
            (0..packages.len())
                .filter(|&idx| {
                    glob.is_match(&normalize(&packages[idx].selected.dir).to_string_lossy())
                })
                .collect()
        })
    };
    if let Some(pattern) = &selector.name_pattern {
        let candidates: Vec<usize> = match &entries {
            Some(entries) => entries.iter().copied().collect(),
            None => (0..packages.len()).collect(),
        };
        entries = Some(match_names(packages, &candidates, pattern));
    }
    Ok(entries.unwrap_or_default())
}

/// pnpm's name match: a `*` / `?` glob, falling back to `@*/<pattern>`
/// when an unscoped pattern matches nothing and that names exactly one
/// project.
fn match_names(
    packages: &[IndexedPackage],
    candidates: &[usize],
    pattern: &str,
) -> BTreeSet<usize> {
    let matches: BTreeSet<usize> = candidates
        .iter()
        .copied()
        .filter(|&idx| {
            packages[idx]
                .selected
                .name
                .as_deref()
                .is_some_and(|name| glob_match(pattern, name))
        })
        .collect();
    if matches.is_empty() && !pattern.starts_with('@') && !pattern.contains('/') {
        let scoped = match_names(packages, candidates, &format!("@*/{pattern}"));
        return if scoped.len() == 1 {
            scoped
        } else {
            BTreeSet::new()
        };
    }
    matches
}

/// pnpm's `WalkState::select_entries`: the entries, their dependencies
/// and/or dependents, and with both, the dependents' dependencies too. `^`
/// drops an entry unless the walk reaches it again from another.
fn select_entries(
    packages: &[IndexedPackage],
    selector: &ProjectSelector,
    entries: &BTreeSet<usize>,
    walk: &mut BTreeSet<usize>,
) {
    let traversal = selector.traversal;
    let prod_only = selector.follow_prod_deps_only;
    if !traversal.include_dependencies && !traversal.include_dependents {
        walk.extend(entries);
        return;
    }
    let keep_entries = !traversal.exclude_self;
    if traversal.include_dependencies {
        walk.extend(walk_dependencies(packages, entries, prod_only));
        if keep_entries {
            walk.extend(entries);
        }
    }
    if traversal.include_dependents {
        let mut dependents = walk_dependents(packages, entries, prod_only);
        if keep_entries {
            dependents.extend(entries);
        }
        if traversal.include_dependencies {
            walk.extend(walk_dependencies(packages, &dependents, prod_only));
        }
        walk.extend(dependents);
    }
}

/// Lexically normalized absolute form, the shape a directory selector is
/// resolved to, so a glob compares like with like.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn index_packages(workspace_pkgs: &[PathBuf]) -> Vec<IndexedPackage> {
    workspace_pkgs
        .iter()
        .filter_map(|dir| {
            let manifest = PackageJson::from_path(&dir.join("package.json")).ok()?;
            Some(indexed(dir.clone(), manifest))
        })
        .collect()
}

fn indexed(dir: PathBuf, manifest: PackageJson) -> IndexedPackage {
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
    IndexedPackage {
        selected: SelectedPackage {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            private,
            dir,
            manifest,
        },
        all_deps,
        prod_deps,
    }
}

/// Add the workspace root to the candidates (when it has a manifest and
/// discovery did not already list it), returning its index.
fn index_root(
    workspace_root: &Path,
    packages: &mut Vec<IndexedPackage>,
) -> Result<Option<usize>, SelectError> {
    if let Some(idx) = packages
        .iter()
        .position(|pkg| normalize(&pkg.selected.dir) == normalize(workspace_root))
    {
        return Ok(Some(idx));
    }
    let manifest_path = workspace_root.join("package.json");
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let manifest = PackageJson::from_path(&manifest_path).map_err(SelectError::Manifest)?;
    packages.push(indexed(workspace_root.to_path_buf(), manifest));
    Ok(Some(packages.len() - 1))
}

/// `--include-workspace-root` or the `includeWorkspaceRoot` setting.
fn include_root_setting(
    workspace_root: &Path,
    filter: &EffectiveFilter,
) -> Result<bool, SelectError> {
    Ok(filter.include_workspace_root
        || WorkspaceConfig::load(workspace_root)
            .map_err(SelectError::WorkspaceConfig)?
            .include_workspace_root
            .unwrap_or(false))
}

/// pnpm's `keeps_workspace_root`: the root stays when it is asked for, or
/// when the workspace lists no packages but the root.
fn keeps_workspace_root(
    workspace_root: &Path,
    filter: &EffectiveFilter,
) -> Result<bool, SelectError> {
    if include_root_setting(workspace_root, filter)? {
        return Ok(true);
    }
    let patterns = crate::workspace_patterns(workspace_root).map_err(SelectError::Discovery)?;
    Ok(patterns.is_empty() || patterns == ["."])
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

/// pnpm's `[<since>]` selection (`get_changed_projects`): the files `git
/// diff` reports between the merge base of `rev` and `HEAD` and the
/// working tree, so uncommitted and staged edits count, each credited to
/// the project that most closely contains it. A `{dir}` part limits the
/// diff to that pathspec, glob included.
fn changed_since(
    workspace_root: &Path,
    packages: &[IndexedPackage],
    rev: &str,
    pathspec_dir: Option<&Path>,
) -> Result<BTreeSet<usize>, SelectError> {
    // `--end-of-options` already keeps a `-`-led rev from reaching git as
    // an option (the CVE-2017-1000117 class); refusing it here as well
    // gives a clear error. NUL never appears in a legitimate ref.
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
    let repo_root = repo_root(workspace_root);
    let base = merge_base(workspace_root, rev)?;
    // Relative to the workspace root, where git runs, so the pathspec and
    // git's own view of the worktree agree however the path was spelled.
    let pathspec = match pathspec_dir {
        Some(dir) => pathdiff::diff_paths(dir, normalize(workspace_root))
            .filter(|rel| !rel.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from(".")),
        None => PathBuf::from("."),
    };
    let output = Command::new("git")
        .args(["diff", "--name-only", "-z", "--no-renames", "--no-relative"])
        .args(["--end-of-options", &base, "--"])
        .arg(&pathspec)
        .current_dir(workspace_root)
        .output()
        .map_err(SelectError::GitIo)?;
    if !output.status.success() {
        return Err(SelectError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let dirs: Vec<PathBuf> = packages
        .iter()
        .map(|pkg| normalize(&pkg.selected.dir))
        .collect();
    let mut out = BTreeSet::new();
    for file in String::from_utf8_lossy(&output.stdout).split('\0') {
        if file.is_empty() {
            continue;
        }
        let changed = repo_root.join(file);
        let owner = changed
            .ancestors()
            .skip(1)
            .find_map(|dir| dirs.iter().position(|pkg| pkg == dir));
        if let Some(idx) = owner {
            out.insert(idx);
        }
    }
    Ok(out)
}

/// The commit `rev` and `HEAD` share, so commits made only on the `rev`
/// side stay out of the diff. git exits 1 with no merge base (a shallow
/// clone, unrelated histories) and 128 for a bad rev; both fall back to
/// `rev` itself, so the diff reports a bad revision.
fn merge_base(workspace_root: &Path, rev: &str) -> Result<String, SelectError> {
    let output = Command::new("git")
        .args(["merge-base", "--end-of-options", rev, "HEAD"])
        .current_dir(workspace_root)
        .output()
        .map_err(SelectError::GitIo)?;
    match output.status.code() {
        Some(0) => {
            let base = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(if base.is_empty() {
                rev.to_string()
            } else {
                base
            })
        }
        Some(1 | 128) => Ok(rev.to_string()),
        _ => Err(SelectError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )),
    }
}

/// The nearest ancestor holding `.git`, found lexically so it compares
/// with the project directories however a symlinked temp dir resolves.
fn repo_root(workspace_root: &Path) -> PathBuf {
    let start = normalize(workspace_root);
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(&start)
        .to_path_buf()
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
    #[error("failed to read the workspace package patterns: {0}")]
    Discovery(#[source] crate::Error),
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

    /// A workspace of root, `a`, `b` (depends on `a`) and `c` (depends on
    /// `b`) under `packages/`, returning the root and the member dirs.
    fn workspace() -> (tempfile::TempDir, Vec<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"root","private":true}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n",
        )
        .unwrap();
        let mut members = Vec::new();
        for (name, dep) in [("a", None), ("b", Some("a")), ("c", Some("b"))] {
            let member = root.join("packages").join(name);
            std::fs::create_dir_all(&member).unwrap();
            let deps = dep.map_or(String::new(), |dep| {
                format!(r#","dependencies":{{"{dep}":"workspace:*"}}"#)
            });
            std::fs::write(
                member.join("package.json"),
                format!(r#"{{"name":"{name}","version":"1.0.0"{deps}}}"#),
            )
            .unwrap();
            members.push(member);
        }
        (dir, members)
    }

    fn names(
        root: &Path,
        members: &[PathBuf],
        filter: EffectiveFilter,
        mode: RootSelection,
    ) -> Vec<String> {
        select_workspace_projects(
            root,
            members,
            &filter,
            SelectOptions {
                prefix: root,
                root: mode,
            },
        )
        .unwrap()
        .into_iter()
        .filter_map(|pkg| pkg.name)
        .collect()
    }

    fn select(root: &Path, members: &[PathBuf], filters: &[&str]) -> Vec<String> {
        names(
            root,
            members,
            EffectiveFilter::from_filters(filters.iter().copied()),
            RootSelection::Keep,
        )
    }

    #[test]
    fn a_directory_selector_is_a_glob_over_project_directories() {
        let (dir, members) = workspace();
        let root = dir.path();
        assert_eq!(select(root, &members, &["./packages/*"]), ["a", "b", "c"]);
        assert_eq!(select(root, &members, &["{packages/[ab]}"]), ["a", "b"]);
        assert_eq!(select(root, &members, &["b{./packages/*}"]), ["b"]);
        // No project sits at `packages` itself.
        assert!(select(root, &members, &["./packages"]).is_empty());
        assert_eq!(select(root, &members, &["./packages/**"]), ["a", "b", "c"]);
        assert_eq!(select(root, &members, &["."]), ["root"]);
    }

    #[test]
    fn selectors_apply_in_runs_and_a_leading_exclusion_starts_from_every_project() {
        let (dir, members) = workspace();
        let root = dir.path();
        assert_eq!(select(root, &members, &["!b"]), ["a", "c", "root"]);
        assert_eq!(
            select(root, &members, &["!a", "a"]),
            ["a", "b", "c", "root"]
        );
        assert_eq!(select(root, &members, &["a", "!a"]), Vec::<String>::new());
        assert_eq!(select(root, &members, &["*"]), ["a", "b", "c", "root"]);
    }

    #[test]
    fn traversal_follows_pnpm() {
        let (dir, members) = workspace();
        let root = dir.path();
        assert_eq!(select(root, &members, &["c..."]), ["a", "b", "c"]);
        assert_eq!(select(root, &members, &["c^..."]), ["a", "b"]);
        assert_eq!(select(root, &members, &["...{./packages/b}"]), ["b", "c"]);
        // Both directions add the dependents' dependencies too.
        assert_eq!(select(root, &members, &["...b..."]), ["a", "b", "c"]);
        // The location fallback keeps no traversal.
        assert_eq!(select(root, &members, &["./packages/c..."]), ["c"]);
    }

    #[test]
    fn an_auto_excluding_command_drops_the_root_unless_kept() {
        let (dir, members) = workspace();
        let root = dir.path();
        let recursive = EffectiveFilter {
            filters: vec!["*".into()],
            recursive: true,
            ..EffectiveFilter::default()
        };
        assert_eq!(
            names(root, &members, recursive.clone(), RootSelection::Keep),
            ["a", "b", "c", "root"]
        );
        assert_eq!(
            names(
                root,
                &members,
                recursive.clone(),
                RootSelection::AutoExclude
            ),
            ["a", "b", "c"]
        );
        let kept = EffectiveFilter {
            include_workspace_root: true,
            ..recursive
        };
        assert_eq!(
            names(root, &members, kept, RootSelection::AutoExclude),
            ["a", "b", "c", "root"]
        );
        let exclusion = EffectiveFilter::from_filters(["!a"]);
        assert_eq!(
            names(root, &members, exclusion, RootSelection::AutoExclude),
            ["b", "c"]
        );
        // An inclusion selector pins the set, the root included when named.
        let named = EffectiveFilter::from_filters(["*"]);
        assert_eq!(
            names(root, &members, named, RootSelection::AutoExclude),
            ["a", "b", "c", "root"]
        );
    }

    #[test]
    fn prod_graph_walk_skips_dev_deps() {
        let mk_pkg = |name: &str, prod: &[&str], dev: &[&str]| -> IndexedPackage {
            let mut pkg = indexed(
                PathBuf::from(format!("/ws/{name}")),
                PackageJson {
                    name: Some(name.to_string()),
                    ..PackageJson::default()
                },
            );
            pkg.prod_deps = prod.iter().map(|s| (*s).to_string()).collect();
            pkg.all_deps = prod.iter().chain(dev).map(|s| (*s).to_string()).collect();
            pkg
        };
        let packages = vec![
            mk_pkg("api", &["lib"], &["tooling"]),
            mk_pkg("lib", &[], &[]),
            mk_pkg("tooling", &[], &[]),
        ];
        let api = BTreeSet::from([0]);
        assert_eq!(
            walk_dependencies(&packages, &api, false),
            BTreeSet::from([1, 2])
        );
        assert_eq!(
            walk_dependencies(&packages, &api, true),
            BTreeSet::from([1])
        );
        let tooling = BTreeSet::from([2]);
        assert_eq!(
            walk_dependents(&packages, &tooling, false),
            BTreeSet::from([0])
        );
        assert!(walk_dependents(&packages, &tooling, true).is_empty());
    }

    #[test]
    fn changed_since_rejects_option_shaped_and_nul_revs() {
        // A rev beginning with `-` would reach `git diff` as an option
        // (CVE-2017-1000117 class); the check runs before git does.
        for rev in ["--upload-pack=/tmp/evil", "main\0evil"] {
            match changed_since(Path::new("/nonexistent"), &[], rev, None).unwrap_err() {
                SelectError::GitFailed(msg) => assert!(msg.contains("refusing"), "{msg}"),
                other => panic!("expected GitFailed, got {other:?}"),
            }
        }
    }
}
