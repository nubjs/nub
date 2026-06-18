//! Workspace package filtering with pnpm-compatible grammar.
//!
//! Supports:
//! - `--filter <name>` — exact package name match
//! - `--filter "<glob>"` — glob pattern against package name or relative dir
//! - `--filter <name>...` — package + all its dependencies
//! - `--filter ...<name>` — package + all its dependents
//! - `--filter "...<name>..."` — both directions
//! - `--filter !<name>` — exclude (subtract) a package from the selection
//! - repeated `--filter` — include filters union; `!` filters subtract from the
//!   union (or from the whole workspace if every filter is an exclusion)
//! - `-r` / `--recursive` — all workspace packages

use std::collections::{HashSet, VecDeque};

use rustc_hash::FxHashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// A parsed filter expression.
#[derive(Debug, Clone)]
pub struct Filter {
    pub pattern: String,
    pub include_dependencies: bool,
    pub include_dependents: bool,
    pub exclude_self: bool,
    pub git_ref: Option<String>,
    pub exclude: bool,
}

impl Filter {
    pub fn parse(s: &str) -> Self {
        let mut pattern = s.to_string();
        let mut include_dependencies = false;
        let mut include_dependents = false;
        let mut exclude_self = false;
        let mut git_ref = None;
        let mut exclude = false;

        // Exclude prefix: !pkg
        if pattern.starts_with('!') {
            exclude = true;
            pattern = pattern[1..].to_string();
        }

        // Trailing ellipsis + optional ^: pkg...  or  pkg...^ — pnpm: the package
        // AND its dependencies (the packages it depends on).
        if pattern.ends_with("...") {
            include_dependencies = true;
            pattern = pattern[..pattern.len() - 3].to_string();
            if pattern.ends_with('^') {
                exclude_self = true;
                pattern = pattern[..pattern.len() - 1].to_string();
            }
        }

        // Leading ellipsis + optional ^: ...pkg  or  ...^pkg — pnpm: the package
        // AND its dependents (the packages that depend on it).
        if pattern.starts_with("...") {
            include_dependents = true;
            pattern = pattern[3..].to_string();
            if pattern.starts_with('^') {
                exclude_self = true;
                pattern = pattern[1..].to_string();
            }
        }

        // Git ref: [origin/main] or [HEAD~2].
        if pattern.starts_with('[') {
            if let Some(close) = pattern.find(']') {
                git_ref = Some(pattern[1..close].to_string());
                pattern = pattern[close + 1..].to_string();
            }
        }

        // Directory selector: {packages/foo} → ./packages/foo
        if pattern.starts_with('{') && pattern.ends_with('}') {
            let inner = &pattern[1..pattern.len() - 1];
            if inner.starts_with('.') {
                pattern = inner.to_string();
            } else {
                pattern = format!("./{inner}");
            }
        }

        Self {
            pattern,
            include_dependencies,
            include_dependents,
            exclude_self,
            git_ref,
            exclude,
        }
    }
}

/// Resolve changed packages from a git ref.
pub fn packages_changed_since(
    workspace_root: &Path,
    members: &[WorkspacePackage],
    git_ref: &str,
) -> HashSet<usize> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", git_ref, "--", "."])
        .current_dir(workspace_root)
        .output();

    let Ok(output) = output else {
        return HashSet::new();
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let changed_files: Vec<PathBuf> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| workspace_root.join(l))
        .collect();

    let mut matched = HashSet::new();
    for (i, pkg) in members.iter().enumerate() {
        for file in &changed_files {
            if file.starts_with(&pkg.dir) {
                matched.insert(i);
                break;
            }
        }
    }
    matched
}

/// A workspace package with its manifest and location.
#[derive(Debug, Clone)]
pub struct WorkspacePackage {
    pub name: String,
    pub dir: PathBuf,
    pub manifest: serde_json::Value,
}

/// Discover all workspace member packages.
pub fn discover_members(workspace_root: &Path) -> Vec<WorkspacePackage> {
    let pkg_path = workspace_root.join("package.json");
    let content = match fs::read_to_string(&pkg_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let manifest: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let patterns = match workspace_patterns_from_manifest(&manifest) {
        Some(patterns) => patterns,
        _ => {
            // Fall back to `pnpm-workspace.yaml` ONLY when pnpm is the incumbent
            // PM. The brand hard gate (AGENTS.md): when the project's PM is not
            // pnpm, nub must never read a pnpm-NAMED path, `pnpm-workspace.yaml`
            // included — the gate is on the NAME, impact-irrelevant. A committed
            // `pnpm-lock.yaml` at the workspace root is the incumbent signal
            // (file-presence detection, not config-consumption — the same signal
            // the CLI's `detect_package_manager` keys pnpm off). No lockfile →
            // pnpm is not provably incumbent → leave the file unread.
            if pnpm_is_incumbent(workspace_root) {
                if let Some(patterns) = read_pnpm_workspace(workspace_root) {
                    patterns
                } else {
                    return vec![];
                }
            } else {
                return vec![];
            }
        }
    };

    expand_member_patterns(workspace_root, &patterns)
}

fn workspace_patterns_from_manifest(manifest: &serde_json::Value) -> Option<Vec<String>> {
    match manifest.get("workspaces") {
        Some(serde_json::Value::Array(arr)) => Some(string_array_values(arr)),
        Some(serde_json::Value::Object(obj)) => obj
            .get("packages")
            .and_then(|v| v.as_array())
            .map(|arr| string_array_values(arr)),
        _ => None,
    }
}

fn string_array_values(arr: &[serde_json::Value]) -> Vec<String> {
    arr.iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
}

/// Maximum directory depth crawled for a `**` pattern. `**` is recursive but we
/// cap the walk so a pathological tree (or an accidental `**` against a deep
/// vendor dir) can't hang discovery. pnpm/npm rely on the same `node_modules`
/// pruning we do below; the depth cap is belt-and-suspenders.
const MAX_GLOB_DEPTH: usize = 24;

/// Turn workspace patterns into concrete member directories.
///
/// Each pattern is matched against the *relative directory path* (slash-normalized)
/// of every candidate package — the same shape npm's `map-workspaces` globs. This
/// replaces an earlier trim-`/*`-then-`read_dir` heuristic that (a) dropped explicit
/// non-glob members (`"libs/core"` was read as a parent to scan, never as the member)
/// and (b) failed to recurse for `**` (`"packages/**"` only ever saw one level).
///
/// Semantics, matching npm/pnpm:
/// - a bare path (`libs/core`) selects exactly that directory if it holds a `package.json`;
/// - a single `*` segment (`packages/*`) selects one level of children;
/// - `**` (`packages/**`) recurses;
/// - `node_modules`, `.git`, and any dot-directory are never crawled into.
///
/// A package is included when at least one non-negated pattern matches its relative
/// path and no negated (`!`) pattern matches it. Patterns are normalized like npm:
/// leading `./` / `/` is stripped, trailing `/` is ignored.
fn expand_member_patterns(workspace_root: &Path, patterns: &[String]) -> Vec<WorkspacePackage> {
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    let mut max_depth = 1usize;

    for raw in patterns {
        let mut pat = raw.as_str();
        let negated = pat.starts_with('!');
        if negated {
            pat = pat.trim_start_matches('!');
        }
        // Strip a leading `./` or `/`, and a trailing `/`, like npm's map-workspaces.
        let pat = pat
            .trim_start_matches("./")
            .trim_start_matches('/')
            .trim_end_matches('/');
        if pat.is_empty() {
            continue;
        }
        let pat = pat.replace('\\', "/");
        // How deep can this pattern reach? `**` ⇒ recursive (capped); otherwise the
        // number of path segments is the exact depth a match can live at.
        let depth = if pat.contains("**") {
            MAX_GLOB_DEPTH
        } else {
            pat.split('/').count()
        };
        if !negated {
            max_depth = max_depth.max(depth);
        }
        if negated {
            exclude.push(pat);
        } else {
            include.push(pat);
        }
    }

    if include.is_empty() {
        return vec![];
    }

    // Collect every relative dir that holds a package.json, then keep those an
    // include pattern matches and no exclude pattern matches.
    let mut candidates = Vec::new();
    collect_package_dirs(
        workspace_root,
        PathBuf::new(),
        0,
        max_depth,
        &mut candidates,
    );

    let mut members = Vec::new();
    for rel in candidates {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let matches_include = include.iter().any(|p| glob_match::glob_match(p, &rel_str));
        if !matches_include {
            continue;
        }
        let matches_exclude = exclude.iter().any(|p| glob_match::glob_match(p, &rel_str));
        if matches_exclude {
            continue;
        }
        let dir = workspace_root.join(&rel);
        let member_pkg = dir.join("package.json");
        if let Ok(content) = fs::read_to_string(&member_pkg) {
            if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                let name = manifest
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                members.push(WorkspacePackage {
                    name,
                    dir,
                    manifest,
                });
            }
        }
    }

    members
}

/// Walk the workspace tree, pushing the workspace-relative path of every directory
/// that contains a `package.json`. Never descends into `node_modules`, `.git`, or
/// any dot-directory; the workspace root itself (`rel == ""`) is never a candidate.
fn collect_package_dirs(
    workspace_root: &Path,
    rel: PathBuf,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<PathBuf>,
) {
    if depth > max_depth {
        return;
    }
    let abs = workspace_root.join(&rel);
    let Ok(entries) = fs::read_dir(&abs) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Prune dirs npm/pnpm never treat as members.
        if name == "node_modules" || name.starts_with('.') {
            continue;
        }
        let child_rel = rel.join(name.as_ref());
        if entry.path().join("package.json").is_file() {
            out.push(child_rel.clone());
        }
        collect_package_dirs(workspace_root, child_rel, depth + 1, max_depth, out);
    }
}

/// Whether pnpm is the project's incumbent PM at `workspace_root` — the gate that
/// guards every read of a pnpm-NAMED path. A committed `pnpm-lock.yaml` is the
/// signal: it can only exist if pnpm has run here, so its presence proves pnpm is
/// incumbent. This is detection (file presence), not config-consumption, so the
/// brand hard gate permits it; it mirrors the pnpm branch of the CLI's
/// `detect_package_manager`. Checked at the workspace root only — the lockfile
/// lives beside `pnpm-workspace.yaml`.
pub(crate) fn pnpm_is_incumbent(workspace_root: &Path) -> bool {
    workspace_root.join("pnpm-lock.yaml").is_file()
}

fn read_pnpm_workspace(workspace_root: &Path) -> Option<Vec<String>> {
    let path = workspace_root.join("pnpm-workspace.yaml");
    let content = fs::read_to_string(&path).ok()?;
    let mut patterns = Vec::new();
    let mut in_packages = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "packages:" {
            in_packages = true;
            continue;
        }
        if in_packages {
            if let Some(rest) = trimmed.strip_prefix("- ") {
                let pattern = rest.trim().trim_matches('\'').trim_matches('"');
                patterns.push(pattern.to_string());
            } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                break;
            }
        }
    }
    if patterns.is_empty() {
        None
    } else {
        Some(patterns)
    }
}

/// The raw selection set for a single filter: initial pattern / glob / git-ref
/// matches, expanded by the `...` (dependencies), `<name>...` (dependents), and
/// `^` (exclude-self) rules.
///
/// This deliberately does NOT apply the `!` exclusion inversion or the final
/// topological sort. The combiner ([`apply_filters`]) needs the un-inverted set
/// so it can union the include filters and subtract the exclude filters
/// coherently — pnpm's multiple-`--filter` semantics. For a lone `!pkg` the
/// inversion happens once, in `apply_filters`, against the whole workspace.
fn raw_matched_set(
    members: &[WorkspacePackage],
    filter: &Filter,
    name_to_idx: &FxHashMap<&str, usize>,
    workspace_root: Option<&Path>,
) -> HashSet<usize> {
    // Find initial matches.
    let mut matched: HashSet<usize> = if let Some(ref git_ref) = filter.git_ref {
        let ws = workspace_root.unwrap_or(Path::new("."));
        packages_changed_since(ws, members, git_ref)
    } else if filter.pattern.is_empty() {
        (0..members.len()).collect()
    } else {
        let mut m = HashSet::new();
        for (i, pkg) in members.iter().enumerate() {
            // Directory selectors match the dir RELATIVE to the workspace root.
            let rel_dir = workspace_root
                .and_then(|root| pkg.dir.strip_prefix(root).ok())
                .unwrap_or(pkg.dir.as_path())
                .to_string_lossy();
            if matches_pattern(&pkg.name, rel_dir.as_ref(), &filter.pattern) {
                m.insert(i);
            }
        }
        // pnpm scope resolution (parseProjectSelector / matchPackagesByGlob): a
        // bare unscoped name with no exact/glob/dir match also selects a SCOPED
        // package whose unscoped part equals it — but only if EXACTLY ONE does.
        // Two scoped packages sharing an unscoped name (`@foo/bar` + `@types/bar`
        // for `bar`) are ambiguous → select none. An exact match always wins (it
        // is already in `m`, so this only runs when `m` is empty).
        if m.is_empty() && is_bare_name(&filter.pattern) {
            let unscoped: Vec<usize> = members
                .iter()
                .enumerate()
                .filter(|(_, p)| unscoped_name(&p.name) == filter.pattern)
                .map(|(i, _)| i)
                .collect();
            if unscoped.len() == 1 {
                m.insert(unscoped[0]);
            }
            // 0 → no match; 2+ → ambiguous, select none.
        }
        m
    };

    let initial_matches: HashSet<usize> = matched.clone();

    // Expand to dependencies if requested.
    if filter.include_dependencies {
        let deps = build_dep_graph(members, name_to_idx);
        let initial: Vec<usize> = matched.iter().copied().collect();
        for idx in initial {
            traverse_deps(&deps, idx, &mut matched);
        }
    }

    // Expand to dependents if requested.
    if filter.include_dependents {
        let reverse_deps = build_reverse_dependency_graph(members, name_to_idx);
        let initial: Vec<usize> = matched.iter().copied().collect();
        for idx in initial {
            traverse_deps(&reverse_deps, idx, &mut matched);
        }
    }

    // Exclude self: remove the originally matched packages, keep only the expanded set.
    if filter.exclude_self {
        for idx in &initial_matches {
            matched.remove(idx);
        }
    }

    matched
}

/// Apply a single filter to workspace members, returning the matched set in
/// topological order (dependencies first). This is the single-`--filter` base
/// case; [`apply_filters`] generalizes it to several filters at once.
pub fn apply_filter(
    members: &[WorkspacePackage],
    filter: &Filter,
    workspace_root: Option<&Path>,
) -> Vec<usize> {
    apply_filters(members, std::slice::from_ref(filter), workspace_root)
}

/// Combine several `--filter` expressions, pnpm-style: the **include** filters
/// (everything without a leading `!`) contribute a UNION of their selections,
/// and the **exclude** filters (`!pkg`) SUBTRACT their selections from that
/// union. With no include filters the base is the whole workspace, so `!pkg`
/// alone — or several `!` filters — select the complement (this preserves the
/// single-filter exclusion behavior). An empty `filters` slice selects the
/// whole workspace. Returns the surviving members in topological order
/// (dependencies first).
pub fn apply_filters(
    members: &[WorkspacePackage],
    filters: &[Filter],
    workspace_root: Option<&Path>,
) -> Vec<usize> {
    let name_to_idx: FxHashMap<&str, usize> = members
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.as_str(), i))
        .collect();

    let has_includes = filters.iter().any(|f| !f.exclude);

    // Base set: the union of every include filter's selection, or — when there
    // are only exclude filters (or none) — the whole workspace.
    let mut selected: HashSet<usize> = if has_includes {
        let mut s = HashSet::new();
        for f in filters.iter().filter(|f| !f.exclude) {
            s.extend(raw_matched_set(members, f, &name_to_idx, workspace_root));
        }
        s
    } else {
        (0..members.len()).collect()
    };

    // Subtract each exclude filter's selection (pattern + any expansion).
    for f in filters.iter().filter(|f| f.exclude) {
        for idx in raw_matched_set(members, f, &name_to_idx, workspace_root) {
            selected.remove(&idx);
        }
    }

    // Topological sort of the surviving packages (dependencies first).
    let deps = build_dep_graph(members, &name_to_idx);
    topological_sort(&selected, &deps)
}

/// A bare, unscoped package identifier (no scope, no glob/dir/git syntax) — the
/// only pattern shape eligible for pnpm's unscoped-name → scoped-package
/// resolution in [`raw_matched_set`].
fn is_bare_name(pattern: &str) -> bool {
    !pattern.is_empty()
        && !pattern.contains('@')
        && !pattern.contains('/')
        && !pattern.contains('*')
        && !pattern.contains('?')
        && !pattern.contains('{')
        && !pattern.starts_with('.')
        && !pattern.starts_with('[')
}

/// The unscoped part of a package name: `@org/foo` → `foo`, `bar` → `bar`.
fn unscoped_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// `rel_dir` is the member's directory **relative to the workspace root** (the
/// caller computes it), which is what pnpm matches directory selectors against.
fn matches_pattern(name: &str, rel_dir: &str, pattern: &str) -> bool {
    // Exact package-name match.
    if name == pattern {
        return true;
    }

    // Directory / path-glob selector. The `{dir}` and `./dir` forms both parse to
    // a leading "./". A BARE dir selects ONLY the package whose own directory IS
    // that dir — pnpm's default glob dir-filtering (`useGlobDirFiltering`,
    // `matchProjectsByGlob`) glob-matches the rel dir against the literal selector,
    // and a literal has no `**`, so it never matches a nested child. Recursion is
    // opt-in via an explicit glob (`./dir/*`, `./dir/**`), handled just above by
    // the glob branch of this same block. Matching
    // the workspace-relative dir (not the absolute path) is what makes
    // `--filter ./packages/*` and `--filter ./packages/**` select child packages.
    if let Some(p) = pattern.strip_prefix("./") {
        let p = p.trim_end_matches('/');
        if p.is_empty() {
            return false;
        }
        if p.contains('*') || p.contains('?') || p.contains('{') {
            return glob_match::glob_match(p, rel_dir);
        }
        return rel_dir == p;
    }

    // Name glob (non-path patterns) match the package NAME.
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('{') {
        return glob_match::glob_match(pattern, name);
    }

    false
}

/// Build a dependency graph: index → set of dependency indices.
pub fn build_dep_graph(
    members: &[WorkspacePackage],
    name_to_idx: &FxHashMap<&str, usize>,
) -> Vec<HashSet<usize>> {
    members
        .iter()
        .map(|pkg| {
            let mut deps = HashSet::new();
            for field in &["dependencies", "devDependencies", "peerDependencies"] {
                if let Some(obj) = pkg.manifest.get(*field).and_then(|v| v.as_object()) {
                    for dep_name in obj.keys() {
                        if let Some(&idx) = name_to_idx.get(dep_name.as_str()) {
                            deps.insert(idx);
                        }
                    }
                }
            }
            deps
        })
        .collect()
}

fn build_reverse_dependency_graph(
    members: &[WorkspacePackage],
    name_to_idx: &FxHashMap<&str, usize>,
) -> Vec<HashSet<usize>> {
    let forward = build_dep_graph(members, name_to_idx);
    let mut reverse: Vec<HashSet<usize>> = vec![HashSet::new(); members.len()];
    for (i, deps) in forward.iter().enumerate() {
        for &dep in deps {
            reverse[dep].insert(i);
        }
    }
    reverse
}

fn traverse_deps(graph: &[HashSet<usize>], start: usize, visited: &mut HashSet<usize>) {
    let mut queue = VecDeque::new();
    queue.push_back(start);
    while let Some(idx) = queue.pop_front() {
        if idx < graph.len() {
            for &dep in &graph[idx] {
                if visited.insert(dep) {
                    queue.push_back(dep);
                }
            }
        }
    }
}

/// Topological sort producing chunks (levels). Each chunk contains
/// packages that can run in parallel — all their deps are in earlier
/// chunks. Kahn's algorithm collecting one wave per level.
///
/// Proper O(V+E) Kahn's: in-degrees are computed once from the dependency
/// edges restricted to `nodes`, a reverse-adjacency (dependents) list is
/// precomputed so emitting a node decrements only its actual successors, and
/// each wave is the set of currently-zero-in-degree nodes. This replaces the
/// earlier O(remaining × deps)-per-wave rescan that re-examined every
/// remaining node on every level.
pub fn topological_chunks(nodes: &HashSet<usize>, deps: &[HashSet<usize>]) -> Vec<Vec<usize>> {
    // In-degree per node: number of its dependencies that are also in `nodes`.
    // Reverse adjacency: for each node, the in-set nodes that depend on it, so
    // emitting a node touches only its dependents (not every remaining node).
    let mut in_degree: FxHashMap<usize, usize> = FxHashMap::default();
    let mut dependents: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
    for &node in nodes {
        let count = if node < deps.len() {
            deps[node]
                .iter()
                .filter(|d| nodes.contains(d))
                .inspect(|&&dep| dependents.entry(dep).or_default().push(node))
                .count()
        } else {
            0
        };
        in_degree.insert(node, count);
    }

    // Seed the ready queue with every zero-in-degree node — the first wave.
    let mut ready: Vec<usize> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(&n, _)| n)
        .collect();

    let mut chunks = Vec::new();
    let mut emitted = 0usize;

    while !ready.is_empty() {
        let wave = std::mem::take(&mut ready);
        emitted += wave.len();
        // Emitting this wave: decrement each emitted node's dependents; any that
        // reach zero in-degree become the next wave.
        for &node in &wave {
            if let Some(succ) = dependents.get(&node) {
                for &other in succ {
                    if let Some(deg) = in_degree.get_mut(&other) {
                        *deg -= 1;
                        if *deg == 0 {
                            ready.push(other);
                        }
                    }
                }
            }
        }
        chunks.push(wave);
    }

    // Cycle: any node never reached zero in-degree → dump the remainder in one
    // chunk (preserving the prior cycle-handling contract).
    if emitted < nodes.len() {
        let leftover: Vec<usize> = nodes
            .iter()
            .copied()
            .filter(|n| in_degree.get(n).copied().unwrap_or(0) != 0)
            .collect();
        chunks.push(leftover);
    }

    chunks
}

/// Flat topological sort (convenience wrapper over chunks).
fn topological_sort(nodes: &HashSet<usize>, deps: &[HashSet<usize>]) -> Vec<usize> {
    topological_chunks(nodes, deps)
        .into_iter()
        .flatten()
        .collect()
}

/// Resolve workspace concurrency per pnpm semantics.
/// Default: min(4, available_parallelism). ≤0: cores - abs(n). Positive: direct.
pub fn resolve_workspace_concurrency(opt: Option<i32>) -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    match opt {
        None => cores.clamp(1, 4),
        Some(n) if n <= 0 => cores.saturating_sub(n.unsigned_abs() as usize).max(1),
        Some(n) => n as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_filter() {
        let f = Filter::parse("@org/api");
        assert_eq!(f.pattern, "@org/api");
        assert!(!f.include_dependencies);
        assert!(!f.include_dependents);
    }

    #[test]
    fn parse_deps_filter() {
        // Trailing `...` (pnpm: package + its dependencies).
        let f = Filter::parse("@org/api...");
        assert_eq!(f.pattern, "@org/api");
        assert!(f.include_dependencies);
        assert!(!f.include_dependents);
    }

    #[test]
    fn parse_dependents_filter() {
        // Leading `...` (pnpm: package + its dependents).
        let f = Filter::parse("...@org/api");
        assert_eq!(f.pattern, "@org/api");
        assert!(!f.include_dependencies);
        assert!(f.include_dependents);
    }

    #[test]
    fn parse_both_directions() {
        let f = Filter::parse("...@org/api...");
        assert_eq!(f.pattern, "@org/api");
        assert!(f.include_dependencies);
        assert!(f.include_dependents);
    }

    #[test]
    fn parse_exclude_filter() {
        let f = Filter::parse("!@org/api");
        assert_eq!(f.pattern, "@org/api");
        assert!(f.exclude);
        assert!(!f.exclude_self);
    }

    fn pkg(name: &str) -> WorkspacePackage {
        WorkspacePackage {
            name: name.to_string(),
            dir: PathBuf::from(name),
            manifest: serde_json::json!({}),
        }
    }

    fn pkg_with_deps(name: &str, deps: &[&str]) -> WorkspacePackage {
        let deps_obj: serde_json::Map<String, serde_json::Value> = deps
            .iter()
            .map(|d| (d.to_string(), serde_json::json!("workspace:*")))
            .collect();
        WorkspacePackage {
            name: name.to_string(),
            dir: PathBuf::from(name),
            manifest: serde_json::json!({ "dependencies": deps_obj }),
        }
    }

    /// A package with an explicit (workspace-relative) directory, for directory
    /// selector tests. (`apply_filter` is called with `workspace_root: None`, so
    /// these dirs are already the workspace-relative form `matches_pattern` sees.)
    fn pkg_in(name: &str, dir: &str) -> WorkspacePackage {
        WorkspacePackage {
            name: name.to_string(),
            dir: PathBuf::from(dir),
            manifest: serde_json::json!({}),
        }
    }

    fn selected_names<'a>(members: &'a [WorkspacePackage], filter: &str) -> HashSet<&'a str> {
        let f = Filter::parse(filter);
        apply_filter(members, &f, None)
            .iter()
            .map(|&i| members[i].name.as_str())
            .collect()
    }

    // pnpm unscoped-name → scoped-package resolution (parseProjectSelector / index.ts).

    #[test]
    fn unscoped_name_selects_the_single_scoped_package() {
        // `-F bar` selects `@foo/bar` when no exact `bar` exists (the only package
        // whose unscoped name is `bar`).
        let members = vec![pkg("@foo/bar")];
        assert_eq!(selected_names(&members, "bar"), HashSet::from(["@foo/bar"]));
    }

    #[test]
    fn unscoped_name_prefers_the_exact_match() {
        // When both `bar` and `@foo/bar` exist, `-F bar` picks ONLY the exact
        // `bar` — the scoped one is not an exact match and exact wins.
        let members = vec![pkg("@foo/bar"), pkg("bar")];
        assert_eq!(selected_names(&members, "bar"), HashSet::from(["bar"]));
    }

    #[test]
    fn unscoped_name_rejects_ambiguous_scoped_matches() {
        // Two scoped packages with the same unscoped name (`@foo/bar` +
        // `@types/bar`) is ambiguous for `-F bar` → select none.
        let members = vec![pkg("@foo/bar"), pkg("@types/bar")];
        assert!(selected_names(&members, "bar").is_empty());
    }

    #[test]
    fn dir_selector_matches_workspace_relative_path() {
        // Directory selectors match the dir relative to the workspace root, with
        // pnpm's default glob dir-filtering semantics: a BARE dir selects ONLY the
        // package whose own directory IS that dir — it never recurses into nested
        // packages (pnpm `--filter ./apps` over `apps/web` selects {}). Recursion
        // is opt-in via an explicit `*`/`**` glob; an exact package dir picks one.
        let members = vec![
            pkg_in("@x/a", "packages/a"),
            pkg_in("@x/b", "packages/b"),
            pkg_in("@x/c", "tools/c"),
        ];
        // A bare parent dir is NOT a package dir, so it selects nothing — the bare
        // form is exact-package-dir-only, no recursion (pnpm parity, D1).
        assert!(selected_names(&members, "./packages").is_empty());
        // Path-glob forms DO reach the children.
        assert_eq!(
            selected_names(&members, "./packages/**"),
            HashSet::from(["@x/a", "@x/b"])
        );
        assert_eq!(
            selected_names(&members, "./packages/*"),
            HashSet::from(["@x/a", "@x/b"])
        );
        // Exact package dir picks one.
        assert_eq!(
            selected_names(&members, "./packages/a"),
            HashSet::from(["@x/a"])
        );
        // The `{dir}` form parses to `./dir`; bare, so it too matches only an exact
        // package dir. `{packages}` is not a package → nothing.
        assert!(selected_names(&members, "{packages}").is_empty());
        // But `{packages/a}` names a package dir exactly → that package.
        assert_eq!(
            selected_names(&members, "{packages/a}"),
            HashSet::from(["@x/a"])
        );
    }

    #[test]
    fn bare_dir_selector_matches_package_not_its_nested_child() {
        // The sharper D1 case: a dir that is BOTH a package AND the parent of a
        // nested package. pnpm `--filter ./packages/group` selects only `group`
        // (the package whose dir IS `packages/group`), never the nested
        // `groupchild`. Recursion would need `./packages/group/*`.
        let members = vec![
            pkg_in("group", "packages/group"),
            pkg_in("groupchild", "packages/group/child"),
        ];
        assert_eq!(
            selected_names(&members, "./packages/group"),
            HashSet::from(["group"])
        );
        // The explicit glob reaches the nested child (and not the parent itself).
        assert_eq!(
            selected_names(&members, "./packages/group/*"),
            HashSet::from(["groupchild"])
        );
    }

    /// End-to-end ellipsis DIRECTION, matching pnpm (the contract nub claims
    /// parity with). Dependency chain app → lib → core. This asserts the
    /// observable SELECTED SET, not just the parsed labels, so the direction can
    /// never silently re-invert (the labels and the graph-walk could agree with
    /// each other yet both be wrong vs pnpm — exactly the bug this guards).
    #[test]
    fn ellipsis_direction_matches_pnpm() {
        let members = vec![
            pkg_with_deps("app", &["lib"]),
            pkg_with_deps("lib", &["core"]),
            pkg("core"),
        ];

        // Trailing `lib...` → lib + its DEPENDENCIES (core). pnpm: {lib, core}.
        assert_eq!(
            selected_names(&members, "lib..."),
            HashSet::from(["lib", "core"]),
            "trailing ellipsis selects the package and its dependencies"
        );

        // Leading `...lib` → lib + its DEPENDENTS (app). pnpm: {lib, app}.
        assert_eq!(
            selected_names(&members, "...lib"),
            HashSet::from(["lib", "app"]),
            "leading ellipsis selects the package and its dependents"
        );

        // Both directions `...lib...` → app + lib + core.
        assert_eq!(
            selected_names(&members, "...lib..."),
            HashSet::from(["app", "lib", "core"]),
            "both-direction ellipsis selects dependents and dependencies"
        );

        // `^` exclude-self rides on the same flag: `lib^...` → dependencies only
        // (core, lib excluded); `...^lib` → dependents only (app, lib excluded).
        assert_eq!(
            selected_names(&members, "lib^..."),
            HashSet::from(["core"]),
            "trailing ^... selects dependencies only, excluding lib itself"
        );
        assert_eq!(
            selected_names(&members, "...^lib"),
            HashSet::from(["app"]),
            "leading ...^ selects dependents only, excluding lib itself"
        );
    }

    #[test]
    fn exclude_filter_selects_complement() {
        // `!b` selects every package except b — it must not select b itself.
        let members = vec![pkg("a"), pkg("b"), pkg("c")];
        let selected = selected_names(&members, "!b");
        assert_eq!(selected, HashSet::from(["a", "c"]));
        assert!(!selected.contains("b"), "!b must exclude b, not select it");
    }

    #[test]
    fn exclude_filter_unknown_package_is_noop() {
        // `!nope` matches no package, so nothing is subtracted — all remain.
        let members = vec![pkg("a"), pkg("b")];
        assert_eq!(selected_names(&members, "!nope"), HashSet::from(["a", "b"]));
    }

    #[test]
    fn exclude_filter_subtracts_dependency_expansion() {
        // a depends on b; trailing `a...` is a + its dependencies (b), so `!a...`
        // excludes both, leaving c. (pnpm: trailing ellipsis = dependencies.)
        let members = vec![pkg_with_deps("a", &["b"]), pkg("b"), pkg("c")];
        assert_eq!(selected_names(&members, "!a..."), HashSet::from(["c"]));
    }

    fn selected_names_multi<'a>(
        members: &'a [WorkspacePackage],
        filters: &[&str],
    ) -> HashSet<&'a str> {
        let parsed: Vec<Filter> = filters.iter().map(|s| Filter::parse(s)).collect();
        apply_filters(members, &parsed, None)
            .iter()
            .map(|&i| members[i].name.as_str())
            .collect()
    }

    #[test]
    fn multiple_includes_union() {
        // The A29 headline: two `--filter`s union, rather than the last winning.
        let members = vec![pkg("a"), pkg("b"), pkg("c")];
        assert_eq!(
            selected_names_multi(&members, &["a", "b"]),
            HashSet::from(["a", "b"])
        );
    }

    #[test]
    fn multiple_includes_union_with_dependency_expansion() {
        // Trailing `a...` brings in a's dep b; unioned with the c filter → {a, b, c}.
        let members = vec![pkg_with_deps("a", &["b"]), pkg("b"), pkg("c")];
        assert_eq!(
            selected_names_multi(&members, &["a...", "c"]),
            HashSet::from(["a", "b", "c"])
        );
    }

    #[test]
    fn exclude_subtracts_from_union_of_includes() {
        // Includes union to {a, b, c}; `!b` subtracts b from that union, not from
        // the whole workspace (d is never selected because no include picks it).
        let members = vec![pkg("a"), pkg("b"), pkg("c"), pkg("d")];
        assert_eq!(
            selected_names_multi(&members, &["a", "b", "c", "!b"]),
            HashSet::from(["a", "c"])
        );
    }

    #[test]
    fn only_excludes_subtract_from_whole_workspace() {
        // With no include filter, the base is the whole workspace; the `!`
        // filters subtract from it. `!a !b` over {a, b, c} leaves c.
        let members = vec![pkg("a"), pkg("b"), pkg("c")];
        assert_eq!(
            selected_names_multi(&members, &["!a", "!b"]),
            HashSet::from(["c"])
        );
    }

    #[test]
    fn parse_exclude_self_deps() {
        // Trailing `^...` (pnpm: dependencies only, package itself excluded).
        let f = Filter::parse("@org/api^...");
        assert_eq!(f.pattern, "@org/api");
        assert!(f.include_dependencies);
        assert!(f.exclude_self);
        assert!(!f.include_dependents);
    }

    #[test]
    fn parse_exclude_self_dependents() {
        // Leading `...^` (pnpm: dependents only, package itself excluded).
        let f = Filter::parse("...^@org/api");
        assert_eq!(f.pattern, "@org/api");
        assert!(f.include_dependents);
        assert!(f.exclude_self);
        assert!(!f.include_dependencies);
    }

    #[test]
    fn parse_dir_selector() {
        let f = Filter::parse("{packages/foo}");
        assert_eq!(f.pattern, "./packages/foo");
        assert!(!f.include_dependencies);
    }

    #[test]
    fn parse_gitref_only() {
        let f = Filter::parse("[master]");
        assert_eq!(f.git_ref, Some("master".to_string()));
        assert_eq!(f.pattern, "");
        assert!(!f.include_dependencies);
    }

    #[test]
    fn parse_gitref_with_deps_trailing() {
        // Trailing `...` → dependencies.
        let f = Filter::parse("[master]...");
        assert_eq!(f.git_ref, Some("master".to_string()));
        assert!(f.include_dependencies);
        assert!(!f.include_dependents);
    }

    #[test]
    fn parse_gitref_with_deps_leading() {
        // Leading `...` → dependents.
        let f = Filter::parse("...[master]");
        assert_eq!(f.git_ref, Some("master".to_string()));
        assert!(f.include_dependents);
        assert!(!f.include_dependencies);
    }

    #[test]
    fn parse_gitref_both_directions() {
        let f = Filter::parse("...[master]...");
        assert_eq!(f.git_ref, Some("master".to_string()));
        assert!(f.include_dependencies);
        assert!(f.include_dependents);
    }

    #[test]
    fn parse_dir_with_deps() {
        // Leading `...` on a dir selector → dependents (pnpm direction).
        let f = Filter::parse("...{./foo}");
        assert_eq!(f.pattern, "./foo");
        assert!(f.include_dependents);
        assert!(!f.include_dependencies);
    }

    #[test]
    fn glob_match_star() {
        assert!(glob_match::glob_match("@org/*", "@org/api"));
        assert!(glob_match::glob_match("@org/*", "@org/web"));
        assert!(!glob_match::glob_match("@org/*", "@other/api"));
    }

    #[test]
    fn glob_match_exact() {
        assert!(glob_match::glob_match("foo", "foo"));
        assert!(!glob_match::glob_match("foo", "bar"));
    }

    #[test]
    fn glob_match_double_star() {
        assert!(glob_match::glob_match("packages/**", "packages/foo/bar"));
        assert!(!glob_match::glob_match("packages/**", "other/foo"));
    }

    #[test]
    fn glob_match_question() {
        assert!(glob_match::glob_match("@org/ap?", "@org/api"));
        assert!(!glob_match::glob_match("@org/ap?", "@org/apple"));
    }

    #[test]
    fn glob_match_braces() {
        assert!(glob_match::glob_match("@org/{api,web}", "@org/api"));
        assert!(glob_match::glob_match("@org/{api,web}", "@org/web"));
        assert!(!glob_match::glob_match("@org/{api,web}", "@org/lib"));
    }

    /// Scaffold a workspace on disk: a root `package.json` carrying `workspaces`,
    /// plus a member `package.json` (named by its dir) at each given relative path.
    /// Returns the root; caller removes it. `tag` keeps concurrent tests isolated.
    fn scaffold_workspace(tag: &str, ws: &[&str], member_dirs: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nub-disc-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let ws_json: Vec<serde_json::Value> = ws.iter().map(|s| serde_json::json!(s)).collect();
        fs::write(
            root.join("package.json"),
            serde_json::json!({ "name": "root", "workspaces": ws_json }).to_string(),
        )
        .unwrap();
        for rel in member_dirs {
            let dir = root.join(rel);
            fs::create_dir_all(&dir).unwrap();
            let name = Path::new(rel).file_name().unwrap().to_string_lossy();
            fs::write(
                dir.join("package.json"),
                serde_json::json!({ "name": name }).to_string(),
            )
            .unwrap();
        }
        root
    }

    fn discovered_names(root: &Path) -> HashSet<String> {
        discover_members(root).into_iter().map(|m| m.name).collect()
    }

    #[test]
    fn discovers_explicit_non_glob_member() {
        // A bare path names the member directly — the old trim+read_dir heuristic
        // scanned `libs/core`'s *children* and so never found `libs/core` itself.
        let root = scaffold_workspace(
            "explicit",
            &["libs/core", "packages/*"],
            &["libs/core", "packages/a"],
        );
        assert_eq!(
            discovered_names(&root),
            HashSet::from(["core".to_string(), "a".to_string()]),
            "explicit `libs/core` and one-level `packages/*` must both resolve"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovers_object_form_workspace_packages() {
        let root = scaffold_workspace(
            "object-form",
            &["libs/core", "packages/*"],
            &["libs/core", "packages/a"],
        );
        fs::write(
            root.join("package.json"),
            serde_json::json!({
                "name": "root",
                "workspaces": {
                    "packages": ["libs/core", "packages/*"],
                    "catalog": { "react": "^18.0.0" }
                }
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            discovered_names(&root),
            HashSet::from(["core".to_string(), "a".to_string()]),
            "object-form `workspaces.packages` must drive member discovery"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn double_star_discovers_nested_members() {
        // `packages/**` must reach nested members (`packages/group/deep`), which the
        // one-level read_dir could never see. `*` would stop at the first level.
        let root = scaffold_workspace(
            "recursive",
            &["packages/**"],
            &["packages/top", "packages/group/deep"],
        );
        assert_eq!(
            discovered_names(&root),
            HashSet::from(["top".to_string(), "deep".to_string()]),
            "`packages/**` must discover both the shallow and the nested member"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn single_star_stops_at_one_level_and_skips_node_modules() {
        // `packages/*` selects only direct children, and `node_modules` is never a
        // member even though it carries a package.json.
        let root = scaffold_workspace(
            "onelevel",
            &["packages/*"],
            &[
                "packages/a",
                "packages/a/nested",
                "packages/node_modules/dep",
            ],
        );
        assert_eq!(
            discovered_names(&root),
            HashSet::from(["a".to_string()]),
            "`packages/*` keeps only `packages/a`; nested + node_modules are excluded"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn topological_chunks_simple() {
        // A depends on B, B depends on C.
        let deps = vec![
            HashSet::from([1]), // 0 (A) depends on 1 (B)
            HashSet::from([2]), // 1 (B) depends on 2 (C)
            HashSet::new(),     // 2 (C) no deps
        ];
        let nodes: HashSet<usize> = [0, 1, 2].into();
        let chunks = topological_chunks(&nodes, &deps);
        // Should produce 3 chunks: [C], [B], [A]
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], vec![2]); // C first
        assert_eq!(chunks[1], vec![1]); // then B
        assert_eq!(chunks[2], vec![0]); // then A

        // Flat sort should match
        let order = topological_sort(&nodes, &deps);
        // C should come before B, B before A.
        let pos_c = order.iter().position(|&x| x == 2).unwrap();
        let pos_b = order.iter().position(|&x| x == 1).unwrap();
        let pos_a = order.iter().position(|&x| x == 0).unwrap();
        assert!(pos_c < pos_b);
        assert!(pos_b < pos_a);
    }

    #[test]
    fn topological_chunks_parallel() {
        // A and B are independent, C depends on both.
        let deps = vec![
            HashSet::new(),        // 0 (A) no deps
            HashSet::new(),        // 1 (B) no deps
            HashSet::from([0, 1]), // 2 (C) depends on A and B
        ];
        let nodes: HashSet<usize> = [0, 1, 2].into();
        let chunks = topological_chunks(&nodes, &deps);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].contains(&0) && chunks[0].contains(&1));
        assert_eq!(chunks[1], vec![2]);
    }

    #[test]
    fn resolve_concurrency_defaults() {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        assert_eq!(resolve_workspace_concurrency(None), cores.clamp(1, 4));
        assert_eq!(resolve_workspace_concurrency(Some(8)), 8);
        assert_eq!(resolve_workspace_concurrency(Some(0)), cores.max(1));
        assert_eq!(
            resolve_workspace_concurrency(Some(-1)),
            cores.saturating_sub(1).max(1)
        );
    }

    // --- pnpm-workspace.yaml brand hard gate (AGENTS.md) ---------------------
    //
    // `pnpm-workspace.yaml` is a pnpm-NAMED path: nub may read it ONLY when pnpm
    // is the project's incumbent PM. The incumbent signal is a committed
    // `pnpm-lock.yaml` at the workspace root. These two tests pin both sides of
    // the gate end to end through `discover_members`.

    fn ws_fixture(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-ws-gate-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pkgs/a")).unwrap();
        // A member named only via pnpm-workspace.yaml, never via package.json#workspaces.
        std::fs::write(
            dir.join("pkgs/a/package.json"),
            r#"{"name":"@x/a","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("pnpm-workspace.yaml"), "packages:\n  - 'pkgs/*'\n").unwrap();
        dir
    }

    #[test]
    fn pnpm_workspace_yaml_unread_when_pnpm_not_incumbent() {
        // Stray `pnpm-workspace.yaml`, no `pnpm-lock.yaml`, no package.json#workspaces:
        // the incumbent is npm/nub, so the pnpm-named file must NOT be read. With no
        // other member source, discovery finds nothing.
        let dir = ws_fixture("no-lock");
        std::fs::write(dir.join("package.json"), r#"{"name":"root"}"#).unwrap();

        let members = discover_members(&dir);
        assert!(
            members.is_empty(),
            "pnpm-workspace.yaml must be ignored without pnpm-lock.yaml; got members: {:?}",
            members.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pnpm_workspace_yaml_read_when_pnpm_lock_present() {
        // A committed `pnpm-lock.yaml` proves pnpm is incumbent → the
        // pnpm-workspace.yaml member (`@x/a`) is discovered.
        let dir = ws_fixture("with-lock");
        std::fs::write(dir.join("package.json"), r#"{"name":"root"}"#).unwrap();
        std::fs::write(dir.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();

        let names: Vec<String> = discover_members(&dir).into_iter().map(|m| m.name).collect();
        assert!(
            names.iter().any(|n| n == "@x/a"),
            "pnpm-workspace.yaml member must be discovered when pnpm-lock.yaml is present; got {names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
