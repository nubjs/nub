//! Workspace member discovery and run scheduling: the members a workspace
//! declares, their dependency graph, and the topological waves a recursive
//! run executes in. `--filter` selection is the engine's
//! (`aube_workspace::selector`), shared with every PM verb.

use std::collections::HashSet;

use rustc_hash::FxHashMap;
use std::fs;
use std::path::{Path, PathBuf};

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
    let manifest: serde_json::Value = match serde_json::from_str(crate::strip_utf8_bom(&content)) {
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
        &include,
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
        if let Ok(content) = fs::read_to_string(&member_pkg)
            && let Ok(manifest) =
                serde_json::from_str::<serde_json::Value>(crate::strip_utf8_bom(&content))
        {
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

    members
}

/// Every directory [`collect_package_dirs`] lists, so a test can assert which
/// parts of its own (unique temp) tree the walk never touched. Test-only.
#[cfg(test)]
static LISTED_DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Walk the workspace tree, pushing the workspace-relative path of every directory
/// that contains a `package.json`. Never descends into `node_modules`, `.git`, or
/// any dot-directory; the workspace root itself (`rel == ""`) is never a candidate.
///
/// Only directories some `include` pattern could still match are entered (see
/// [`could_hold_member`]): `packages/*` reads `packages/` and nothing else. Without
/// that, the walk listed every directory down to the pattern's depth, a Rust
/// `target/` included, and a workspace rooted at the home folder reached into
/// `~/Library`.
fn collect_package_dirs(
    workspace_root: &Path,
    rel: PathBuf,
    depth: usize,
    max_depth: usize,
    include: &[String],
    out: &mut Vec<PathBuf>,
) {
    if depth > max_depth {
        return;
    }
    let abs = workspace_root.join(&rel);
    #[cfg(test)]
    LISTED_DIRS
        .lock()
        .expect("LISTED_DIRS lock poisoned")
        .push(abs.clone());
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
        if !include.iter().any(|p| could_hold_member(&child_rel, p)) {
            continue;
        }
        if entry.path().join("package.json").is_file() {
            out.push(child_rel.clone());
        }
        collect_package_dirs(
            workspace_root,
            child_rel,
            depth + 1,
            max_depth,
            include,
            out,
        );
    }
}

/// Whether `pattern` could match `rel` or a directory below it: each segment of
/// `rel` matches the pattern's segment at the same position, until a `**`, which
/// matches everything beneath. Only ever over-includes — a pattern that cannot be
/// split into segments is never pruned — so the members found are the same as
/// walking everything; the final include/exclude match still decides.
fn could_hold_member(rel: &Path, pattern: &str) -> bool {
    if !splits_into_segments(pattern) {
        return true;
    }
    let mut pattern_segments = pattern.split('/');
    for segment in rel.components() {
        let segment = segment.as_os_str().to_string_lossy();
        match pattern_segments.next() {
            None => return false,
            Some(p) if p.contains("**") => return true,
            Some(p) if !glob_match::glob_match(p, &segment) => return false,
            Some(_) => {}
        }
    }
    true
}

/// False when a `/` sits inside a brace group (`{libs/core,apps}`) or a
/// character class (`[^/]`), where splitting the pattern on `/` would cut the
/// group in half. A class runs to the next `]` that is not its first member.
fn splits_into_segments(pattern: &str) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    let mut brace_depth = 0usize;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '[' => {
                let mut end = i + 1;
                if matches!(chars.get(end), Some('!' | '^')) {
                    end += 1;
                }
                if chars.get(end) == Some(&']') {
                    end += 1;
                }
                while end < chars.len() && chars[end] != ']' {
                    if chars[end] == '/' {
                        return false;
                    }
                    end += 1;
                }
                i = end;
            }
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            '/' if brace_depth > 0 => return false,
            _ => {}
        }
        i += 1;
    }
    true
}

/// Whether pnpm is the project's incumbent PM at `workspace_root` — the gate that
/// guards every read of a pnpm-NAMED path. Two signals prove it: a committed
/// `pnpm-lock.yaml` (it only exists if pnpm has run here), or the root manifest
/// declaring pnpm in `packageManager` / `devEngines.packageManager`, which covers
/// a fresh pnpm workspace before its first install (read through the CLI's own
/// declaration reader, [`crate::incumbent`]). Both are detection, not
/// config-consumption, so the brand hard gate permits them. A lone
/// `pnpm-workspace.yaml` with neither stays unread. Checked at the workspace root
/// only — the lockfile and the pin live beside `pnpm-workspace.yaml`.
pub(crate) fn pnpm_is_incumbent(workspace_root: &Path) -> bool {
    workspace_root.join("pnpm-lock.yaml").is_file()
        || crate::incumbent::declared_pm_name(workspace_root).as_deref() == Some("pnpm")
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
    fn discovery_lists_only_directories_a_pattern_can_reach() {
        let root = scaffold_workspace(
            "pruned",
            &["packages/*", "{apps,tools}/*", "libs/**"],
            &[
                "packages/a",
                "apps/web",
                "tools/cli",
                "libs/x/y",
                "unlisted/z",
            ],
        );
        for tree in ["target/debug/deps", "Library/Containers/app/Data"] {
            fs::create_dir_all(root.join(tree)).unwrap();
        }
        assert_eq!(
            discovered_names(&root),
            HashSet::from(["a", "web", "cli", "y"].map(String::from)),
        );
        let listed: Vec<PathBuf> = LISTED_DIRS
            .lock()
            .unwrap()
            .iter()
            .filter(|dir| dir.starts_with(&root))
            .cloned()
            .collect();
        for outside in ["target", "Library", "unlisted"] {
            assert!(
                !listed.iter().any(|dir| dir.starts_with(root.join(outside))),
                "`{outside}` matches no pattern and must not be listed: {listed:?}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_slash_inside_a_class_or_brace_group_still_finds_its_members() {
        // Splitting these on `/` would cut the group in half and prune the member.
        // One workspace each: an unprunable pattern beside the other would let its
        // member through even with the other's detection broken.
        for (tag, pattern, member, name) in [
            ("class", "packages/[^/]*", "packages/a", "a"),
            ("brace", "{libs/core,tools}", "libs/core", "core"),
        ] {
            let root = scaffold_workspace(&format!("unsplittable-{tag}"), &[pattern], &[member]);
            assert_eq!(
                discovered_names(&root),
                HashSet::from([name.to_string()]),
                "`{pattern}` must still find `{member}`"
            );
            let _ = fs::remove_dir_all(&root);
        }
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

    /// A cycle keeps the cyclic nodes out of every wave (they never reach
    /// zero in-degree), so the leftover branch dumps them in ONE final chunk
    /// after the acyclic prefix waves out normally. No node is dropped or
    /// duplicated even when the graph isn't a DAG.
    #[test]
    fn topological_chunks_cycle_dumps_remainder_in_one_chunk() {
        // D(3) is an acyclic head; C(2) depends on D; A(0)<->B(1) form a 2-cycle.
        let deps = vec![
            HashSet::from([1]), // 0 (A) -> B
            HashSet::from([0]), // 1 (B) -> A   (the cycle)
            HashSet::from([3]), // 2 (C) -> D
            HashSet::new(),     // 3 (D) no deps
        ];
        let nodes: HashSet<usize> = [0, 1, 2, 3].into();
        let chunks = topological_chunks(&nodes, &deps);

        // Acyclic prefix waves out in dependency order: D, then C.
        assert_eq!(chunks[0], vec![3], "D (no deps) is the first wave");
        assert_eq!(chunks[1], vec![2], "C waves once its dep D is emitted");

        // The cyclic remainder {A, B} is the single final chunk (order is the
        // node-set iteration order, so compare as a set).
        let leftover: HashSet<usize> = chunks.last().unwrap().iter().copied().collect();
        assert_eq!(
            leftover,
            HashSet::from([0, 1]),
            "the A<->B cycle is dumped together in the final chunk"
        );
        assert_eq!(
            chunks.len(),
            3,
            "two acyclic waves + one leftover cycle chunk"
        );

        // Every node appears exactly once across all chunks (none lost or dupes).
        let mut all: Vec<usize> = chunks.into_iter().flatten().collect();
        all.sort_unstable();
        assert_eq!(all, vec![0, 1, 2, 3]);
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
