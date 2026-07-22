//! Hoisted (`node-linker=hoisted`) layout.
//!
//! Unlike the isolated layout — which materializes every package under
//! a per-project `.aube/<dep_path>/` virtual store and builds Node's
//! module graph out of symlinks — the hoisted layout writes real
//! package directories straight into `node_modules/`, nesting
//! conflicting versions under the parent that requires them. This
//! matches npm / yarn-classic's flat tree and is what certain legacy
//! toolchains (React Native's Metro, some Jest plugins) require.
//!
//! Placement algorithm (npm-style, per importer):
//!
//! 1. Start with a `TreeNode` for the importer — its `node_modules`
//!    directory and an empty child map.
//! 2. BFS from the importer's direct deps. For each `(requester, name,
//!    dep_path)` pair, walk up from the requester looking for the
//!    shallowest ancestor whose `children[name]` is either absent or
//!    points at the same `dep_path`. That ancestor becomes the
//!    placement site.
//! 3. If a matching entry already exists at that ancestor, reuse it
//!    (dedupe). Otherwise create a new child node and enqueue every
//!    transitive dep of the placed package with the new node as
//!    requester.
//! 4. Conflicting versions naturally nest: when walking up from the
//!    requester we stop as soon as we find a different `dep_path`
//!    under the same name, so the conflict forces the new entry to
//!    live below the blocker (typically inside the requester's own
//!    `node_modules/`).
//!
//! The planner operates purely on dep_path strings — the same keys
//! aube-lockfile uses — so peer-context dep_paths like
//! `react-router@6(react@18)` are treated as distinct and won't
//! collapse onto a plain `react-router@6` placement. The side effect
//! is that peer-variant conflicts nest deeper in hoisted mode than in
//! isolated mode, which is the correct-but-slightly-inefficient
//! fallback.
//!
//! The planner output (`PlacementPlan`) is consumed by the
//! materializer in `link_hoisted_importer` and also surfaced to the
//! install driver via `HoistedPlacements` so bin linking and
//! dependency lifecycle scripts can locate a package's on-disk
//! directory without recomputing the tree.

use crate::pool::with_link_pool;
use crate::{Error, HoistingLimits, LinkStats, Linker, apply_multi_file_patch};
use aube_lockfile::{DirectDep, LocalSource, LockedPackage, LockfileGraph};
use aube_store::PackageIndex;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

/// Map from lockfile `dep_path` to the absolute on-disk directories
/// where that package ended up. Most entries have exactly one path;
/// packages whose name conflicts with a shallower version end up
/// duplicated across multiple parent `node_modules/` directories so
/// each gets its own on-disk copy.
#[derive(Debug, Default, Clone)]
pub struct HoistedPlacements {
    by_dep_path: BTreeMap<String, Vec<PathBuf>>,
}

impl HoistedPlacements {
    /// Recompute hoisted placement paths for an already-linked graph
    /// without touching disk. Used by commands like `aube rebuild`
    /// that need to find package directories after install, but must
    /// not relink node_modules. `modules_dir_name` must match the
    /// `modulesDir` setting the install used, or the computed paths
    /// won't match what's on disk.
    pub fn from_graph(
        root_dir: &Path,
        graph: &LockfileGraph,
        modules_dir_name: &str,
        hoisting_limits: HoistingLimits,
    ) -> Result<Self, Error> {
        let mut placements = Self::default();
        // Same seeds the installer derives, so the recomputed paths match
        // what `link_workspace_hoisted` put on disk (see
        // `workspace_importer_seeds`).
        let seeds = workspace_importer_seeds(root_dir, graph, modules_dir_name);

        let record = |plan: &PlacementPlan, placements: &mut Self| {
            for node in &plan.nodes {
                let (Some(dep_path), Some(pkg_dir)) = (&node.dep_path, &node.pkg_dir) else {
                    continue;
                };
                if pkg_dir.exists() {
                    placements.record(dep_path, pkg_dir.clone());
                }
            }
        };

        // Mirror the installer's dispatch: the default (`None`) hoists a
        // multi-importer workspace into ONE shared tree; every other case
        // plans per importer. Recomputing with the wrong planner would
        // return paths that don't exist on disk.
        if matches!(hoisting_limits, HoistingLimits::None) && seeds.len() > 1 {
            let plan = plan_workspace(&seeds, graph)?;
            record(&plan, &mut placements);
        } else {
            for (nm, deps) in &seeds {
                let plan = plan_importer(nm, deps, graph, hoisting_limits)?;
                record(&plan, &mut placements);
            }
        }
        Ok(placements)
    }

    /// Shallowest placement for `dep_path`, or `None` if the dep is
    /// not in the hoisted tree (e.g. filtered by `--prod` /
    /// `--no-optional`). Used by the install driver as the canonical
    /// location for bin linking and lifecycle-script cwds.
    pub fn package_dir(&self, dep_path: &str) -> Option<&Path> {
        self.by_dep_path
            .get(dep_path)
            .and_then(|v| v.first())
            .map(|p| p.as_path())
    }

    /// Every placement site for `dep_path`. When a name conflicts
    /// with a shallower version the same dep_path may appear at
    /// multiple depths; lifecycle scripts run once per site so each
    /// copy has its native-build artifacts in place.
    pub fn all_package_dirs(&self, dep_path: &str) -> &[PathBuf] {
        self.by_dep_path
            .get(dep_path)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Iterate `(dep_path, placement_path)` pairs in BTree order.
    /// Primarily used by the top-level installer when it wants to
    /// walk every placed copy (e.g. the stale-directory sweep or the
    /// lifecycle-script dispatcher).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.by_dep_path
            .iter()
            .flat_map(|(k, v)| v.iter().map(move |p| (k.as_str(), p.as_path())))
    }

    pub(crate) fn record(&mut self, dep_path: &str, path: PathBuf) {
        self.by_dep_path
            .entry(dep_path.to_string())
            .or_default()
            .push(path);
    }
}

/// One node in the placement tree. A node is either the importer
/// root (`pkg_dir == None`) or a placed package. `nm_dir` is the
/// `node_modules/` directory underneath this node where its children
/// live — for the importer that's `<importer>/node_modules`, for a
/// placed package it's `<parent.nm_dir>/<name>/node_modules`.
struct TreeNode {
    pkg_dir: Option<PathBuf>,
    nm_dir: PathBuf,
    parent: Option<usize>,
    children: BTreeMap<String, usize>,
    dep_path: Option<String>,
}

/// Arena-backed placement tree.
///
/// A single plan can span a whole workspace: the arena root is the
/// workspace-root `node_modules`, and each additional physical importer
/// (a workspace member) is an *importer node* — a child of the root with
/// its own physical `nm_dir` (`<member>/node_modules`) and no `pkg_dir`
/// (it is an existing directory, never materialized). `importer_nodes`
/// lists the root plus every member node; the materializer sweeps and
/// tallies each importer's own `node_modules` from that list.
pub(crate) struct PlacementPlan {
    nodes: Vec<TreeNode>,
    root_idx: usize,
    importer_nodes: Vec<usize>,
}

struct PlaceOutcome {
    node_idx: usize,
    created: bool,
}

impl PlacementPlan {
    fn new(importer_nm: PathBuf) -> Self {
        let root = TreeNode {
            pkg_dir: None,
            nm_dir: importer_nm,
            parent: None,
            children: BTreeMap::new(),
            dep_path: None,
        };
        Self {
            nodes: vec![root],
            root_idx: 0,
            importer_nodes: vec![0],
        }
    }

    /// Add a workspace-member importer node whose `node_modules` is
    /// `nm_dir`. The node hangs off the root so a member dep's ancestor
    /// walk reaches root (and can hoist there), but it is deliberately
    /// NOT inserted into `root.children`: it is not a package placement,
    /// so it must not shadow a same-named package at root nor appear in
    /// the root's stale-entry sweep set.
    fn add_importer_node(&mut self, nm_dir: PathBuf) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(TreeNode {
            pkg_dir: None,
            nm_dir,
            parent: Some(self.root_idx),
            children: BTreeMap::new(),
            dep_path: None,
        });
        self.importer_nodes.push(idx);
        idx
    }

    /// Package names placed directly in importer node `idx`'s
    /// `node_modules/`. Drives the per-importer stale-entry sweep in
    /// `materialize_plan`.
    pub(crate) fn child_names_of(&self, idx: usize) -> impl Iterator<Item = &str> {
        self.nodes[idx].children.keys().map(|s| s.as_str())
    }

    /// Place `(name, dep_path)` under the ancestor chain rooted at
    /// `requester`. Returns the resulting node index and whether a
    /// fresh entry was created (so the caller knows whether to
    /// enqueue transitive deps).
    fn place(
        &mut self,
        requester: usize,
        floor: usize,
        name: &str,
        dep_path: &str,
    ) -> Result<PlaceOutcome, Error> {
        crate::validate_package_link_name(name)?;
        debug_assert!(is_ancestor_or_self(&self.nodes, floor, requester));
        // Reuse a matching package anywhere already visible through
        // Node's ancestor lookup, even if the hoist limit would
        // prevent placing a new package that high.
        let mut cursor = requester;
        loop {
            if let Some(&existing) = self.nodes[cursor].children.get(name) {
                if self.nodes[existing].dep_path.as_deref() == Some(dep_path) {
                    return Ok(PlaceOutcome {
                        node_idx: existing,
                        created: false,
                    });
                }
                // A nearer same-name package blocks Node from
                // resolving to any matching package above it.
                break;
            }
            match self.nodes[cursor].parent {
                Some(p) => cursor = p,
                None => break,
            }
        }

        // Walk up from the requester looking for the shallowest
        // allowed ancestor that doesn't already host a different
        // version of `name`.
        let mut cursor = requester;
        let mut candidate = requester;
        loop {
            if self.nodes[cursor].children.contains_key(name) {
                // Conflict: must stay at or below `candidate`.
                break;
            }
            candidate = cursor;
            if cursor == floor {
                break;
            }
            match self.nodes[cursor].parent {
                Some(p) => cursor = p,
                None => break,
            }
        }

        let parent_nm = self.nodes[candidate].nm_dir.clone();
        // Never displace an existing occupant of `candidate`'s slot: the
        // placement walk stops at the shallowest ancestor NOT already
        // hosting `name`, so `candidate.children[name]` must be vacant. A
        // violation means a different version was pre-seated in a slot the
        // requester also claims and can't nest below (the root importer's
        // own direct dep vs a preplaced member version) — a correctness bug,
        // not something to silently overwrite. `preferred_root_versions`
        // prevents it by giving the root importer's direct deps priority.
        debug_assert!(
            !self.nodes[candidate].children.contains_key(name),
            "place() would overwrite an occupied slot for {name} at node {candidate}"
        );
        let pkg_dir = parent_nm.join(name);
        let nm_dir = pkg_dir.join("node_modules");
        let new_idx = self.nodes.len();
        self.nodes.push(TreeNode {
            pkg_dir: Some(pkg_dir),
            nm_dir,
            parent: Some(candidate),
            children: BTreeMap::new(),
            dep_path: Some(dep_path.to_string()),
        });
        self.nodes[candidate]
            .children
            .insert(name.to_string(), new_idx);
        Ok(PlaceOutcome {
            node_idx: new_idx,
            created: true,
        })
    }

    /// Pre-claim the root `node_modules/` slot for `name` with `dep_path`,
    /// returning the (possibly pre-existing) node index. Seats the
    /// most-referenced version of a multi-version name at root before the
    /// BFS so it wins the slot deterministically instead of by arrival order.
    fn preplace_root_child(&mut self, name: &str, dep_path: &str) -> usize {
        if let Some(&existing) = self.nodes[self.root_idx].children.get(name) {
            return existing;
        }
        let parent_nm = self.nodes[self.root_idx].nm_dir.clone();
        let pkg_dir = parent_nm.join(name);
        let nm_dir = pkg_dir.join("node_modules");
        let new_idx = self.nodes.len();
        self.nodes.push(TreeNode {
            pkg_dir: Some(pkg_dir),
            nm_dir,
            parent: Some(self.root_idx),
            children: BTreeMap::new(),
            dep_path: Some(dep_path.to_string()),
        });
        self.nodes[self.root_idx]
            .children
            .insert(name.to_string(), new_idx);
        new_idx
    }
}

fn is_ancestor_or_self(nodes: &[TreeNode], ancestor: usize, mut node: usize) -> bool {
    loop {
        if node == ancestor {
            return true;
        }
        let Some(parent) = nodes[node].parent else {
            return false;
        };
        node = parent;
    }
}

/// In full-hoist mode (`HoistingLimits::None`/`Workspaces`), the root slot
/// for a package name is claimed by whichever version the BFS reaches first.
/// When a name has multiple versions in the graph, an arbitrary arrival-order
/// winner forces every other version to nest under each of ITS consumers — so
/// a low-use version winning root duplicates the widely-used version across
/// dozens of `node_modules/` (the legacy npm-v1-lock blowup: babel-runtime@6.26.0
/// — used once — takes root, and babel-runtime@6.23.0, used by ~40 packages,
/// nests 40 times, each copy dragging its own transitive closure). npm /
/// yarn-classic avoid this by hoisting the MOST-REFERENCED version. Return,
/// for each name with MORE THAN ONE version reachable from the importer, the
/// dep_path with the most consumer edges (direct deps weighted so they always
/// own their name's slot; ties broken by dep_path for determinism).
/// Single-version names are omitted — the plain BFS already hoists them right.
///
/// Two priority tiers of direct edge:
/// - `root_direct` — the ROOT importer's own direct deps. The root package
///   resolves these from the top-level `node_modules`, so its declared
///   version MUST own the root slot; a member's differing version of the
///   same name nests under the member (npm/pnpm/yarn-nm guarantee). These
///   outrank everything.
/// - `other_direct` — every non-root importer's direct deps. They own a
///   name's root slot only when root does not itself declare that name; a
///   plurality/lexical contest picks the winner among them.
///
/// For a single importer, all its direct deps are `root_direct` and
/// `other_direct` is empty.
fn preferred_root_versions<'a>(
    root_direct: &[DirectDep],
    other_direct: impl IntoIterator<Item = &'a DirectDep>,
    graph: &LockfileGraph,
) -> Vec<(String, String)> {
    // name -> dep_path -> consumer-edge count over the reachable graph.
    let mut counts: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    // Weight tiers so the argmax respects priority: a ROOT direct edge beats
    // any number of member direct edges, which beat any number of transitive
    // consumers. `1<<60` dominates `member_count * (1<<40)` for any realistic
    // workspace (>1e6 members before it could be caught), and neither tier
    // can overflow u64 when summed.
    const ROOT_DIRECT_WEIGHT: u64 = 1 << 60;
    const DIRECT_WEIGHT: u64 = 1 << 40;
    let seed_direct = |dep: &DirectDep,
                       weight: u64,
                       counts: &mut BTreeMap<String, BTreeMap<String, u64>>,
                       seen: &mut BTreeSet<String>,
                       queue: &mut VecDeque<String>| {
        if !graph.packages.contains_key(&dep.dep_path) {
            return;
        }
        *counts
            .entry(dep.name.clone())
            .or_default()
            .entry(dep.dep_path.clone())
            .or_default() += weight;
        if seen.insert(dep.dep_path.clone()) {
            queue.push_back(dep.dep_path.clone());
        }
    };
    for dep in root_direct {
        seed_direct(dep, ROOT_DIRECT_WEIGHT, &mut counts, &mut seen, &mut queue);
    }
    for dep in other_direct {
        seed_direct(dep, DIRECT_WEIGHT, &mut counts, &mut seen, &mut queue);
    }
    while let Some(dep_path) = queue.pop_front() {
        let Some(pkg) = graph.packages.get(&dep_path) else {
            continue;
        };
        // `link:` targets own their own node_modules; their edges don't
        // materialize into this importer's hoisted tree.
        if matches!(pkg.local_source.as_ref(), Some(LocalSource::Link(_))) {
            continue;
        }
        for (dep_name, dep_tail) in &pkg.dependencies {
            // Resolve the edge to its graph key across ALL reader conventions:
            // the yarn readers store the VALUE as the full dep_path
            // (`is-plain-obj@4.1.0`), npm/pnpm as the tail, git/tarball as the
            // resolved URL. The former inline `name@tail` guess doubled the name
            // for the yarn convention (`is-plain-obj@is-plain-obj@4.1.0`) and
            // silently dropped the whole subtree — cal.com's missing execa
            // closure. `resolve_dep_edge` is the shared 3-convention resolver the
            // resolver/install walkers already use.
            let Some(child) = aube_lockfile::resolve_dep_edge(dep_name, dep_tail, |k| {
                graph.packages.contains_key(k)
            }) else {
                continue;
            };
            *counts
                .entry(dep_name.clone())
                .or_default()
                .entry(child.clone())
                .or_default() += 1;
            if seen.insert(child.clone()) {
                queue.push_back(child);
            }
        }
    }

    counts
        .into_iter()
        .filter(|(_, versions)| versions.len() > 1)
        .filter_map(|(name, versions)| {
            versions
                .into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)))
                .map(|(dep_path, _)| (name, dep_path))
        })
        .collect()
}

/// Enqueue every registry-resolvable transitive edge of `pkg` for
/// placement under `node_idx` with `child_floor`. `link:` packages own
/// their own `node_modules` (Node resolves through the live target), so
/// their edges are skipped rather than materialized into the tree.
fn enqueue_transitives(
    queue: &mut VecDeque<(usize, usize, String, String)>,
    node_idx: usize,
    child_floor: usize,
    pkg: &LockedPackage,
    graph: &LockfileGraph,
) {
    if matches!(pkg.local_source.as_ref(), Some(LocalSource::Link(_))) {
        return;
    }
    for (dep_name, dep_tail) in &pkg.dependencies {
        // 3-convention edge resolution: the yarn readers store the VALUE as
        // the full dep_path (`is-plain-obj@4.1.0`), npm/pnpm the bare tail,
        // git/tarball the resolved URL. A convention mismatch here silently
        // drops the dep's whole subtree, so route through the shared resolver.
        let Some(child_dep_path) =
            aube_lockfile::resolve_dep_edge(dep_name, dep_tail, |k| graph.packages.contains_key(k))
        else {
            continue;
        };
        queue.push_back((node_idx, child_floor, dep_name.clone(), child_dep_path));
    }
}

/// Seat the most-referenced version of every multi-version name at the
/// plan root up front and enqueue its transitives, so the majority
/// version wins the slot and only minority versions nest. Without this
/// the BFS arrival-order winner can be a low-use version, exploding the
/// widely-used one into dozens of nested copies (see
/// `preferred_root_versions`). No-op when nothing conflicts. `root_direct`
/// and `other_direct` carry the two priority tiers documented on
/// `preferred_root_versions`.
fn preplace_root_winners<'a>(
    plan: &mut PlacementPlan,
    queue: &mut VecDeque<(usize, usize, String, String)>,
    root_direct: &[DirectDep],
    other_direct: impl IntoIterator<Item = &'a DirectDep>,
    graph: &LockfileGraph,
) {
    let root_idx = plan.root_idx;
    for (name, dep_path) in preferred_root_versions(root_direct, other_direct, graph) {
        let node_idx = plan.preplace_root_child(&name, &dep_path);
        let Some(pkg) = graph.packages.get(&dep_path) else {
            continue;
        };
        enqueue_transitives(queue, node_idx, root_idx, pkg, graph);
    }
}

/// Build a placement plan for a single importer.
pub(crate) fn plan_importer(
    importer_nm: &Path,
    root_deps: &[DirectDep],
    graph: &LockfileGraph,
    hoisting_limits: HoistingLimits,
) -> Result<PlacementPlan, Error> {
    let mut plan = PlacementPlan::new(importer_nm.to_path_buf());
    let mut queue: VecDeque<(usize, usize, String, String)> = VecDeque::new();

    if matches!(
        hoisting_limits,
        HoistingLimits::None | HoistingLimits::Workspaces
    ) {
        // Single importer: all its direct deps own their own root slot.
        preplace_root_winners(&mut plan, &mut queue, root_deps, std::iter::empty(), graph);
    }

    // Seed the queue with the importer's direct deps in declaration
    // order. BFS makes shallower deps win placement ties over
    // deeper ones, which matches npm's first-writer-wins policy.
    for dep in root_deps {
        if !graph.packages.contains_key(&dep.dep_path) {
            continue;
        }
        queue.push_back((
            plan.root_idx,
            plan.root_idx,
            dep.name.clone(),
            dep.dep_path.clone(),
        ));
    }

    while let Some((requester, floor, name, dep_path)) = queue.pop_front() {
        let outcome = plan.place(requester, floor, &name, &dep_path)?;
        if !outcome.created {
            continue;
        }
        let Some(pkg) = graph.packages.get(&dep_path) else {
            continue;
        };
        let child_floor = match hoisting_limits {
            HoistingLimits::None | HoistingLimits::Workspaces => plan.root_idx,
            HoistingLimits::Dependencies => outcome.node_idx,
        };
        enqueue_transitives(&mut queue, outcome.node_idx, child_floor, pkg, graph);
    }

    Ok(plan)
}

/// Build ONE placement plan spanning an entire workspace, matching real
/// pnpm's `nodeLinker=hoisted` default (`hoistingLimits=none`): every
/// importer's dependencies compete for a single shared tree rooted at the
/// workspace-root `node_modules`, so a dependency shared across members
/// materializes ONCE at the root and only version conflicts nest under
/// the importer that needs them.
///
/// `importers` is `(node_modules_dir, direct_deps)` per physical
/// importer, ROOT FIRST. The root becomes the arena root; each member
/// becomes an importer node (see [`PlacementPlan::add_importer_node`])
/// whose deps are seeded with `floor = root`, so they hoist to the root
/// unless blocked by a nearer different-version placement. Deps that
/// resolve to a workspace sibling are not in `graph.packages` and are
/// skipped here — the caller symlinks them separately.
pub(crate) fn plan_workspace(
    importers: &[(PathBuf, Vec<DirectDep>)],
    graph: &LockfileGraph,
) -> Result<PlacementPlan, Error> {
    let (root_nm, root_deps) = importers
        .first()
        .expect("plan_workspace requires at least the root importer");
    let mut plan = PlacementPlan::new(root_nm.clone());
    let mut queue: VecDeque<(usize, usize, String, String)> = VecDeque::new();

    // (importer node index, its direct deps) — root plus every member.
    let mut seeds: Vec<(usize, &[DirectDep])> = Vec::with_capacity(importers.len());
    seeds.push((plan.root_idx, root_deps.as_slice()));
    for (nm_dir, deps) in &importers[1..] {
        let idx = plan.add_importer_node(nm_dir.clone());
        seeds.push((idx, deps.as_slice()));
    }

    // Deterministic root winner. The root importer's OWN direct deps own
    // their root slot unconditionally (a member's differing version nests
    // under the member); members' direct deps only win a name root doesn't
    // declare. Then seed each importer's direct deps under its own node with
    // `floor = root` so shared deps hoist to the single shared root.
    let member_direct = seeds.iter().skip(1).flat_map(|(_, deps)| deps.iter());
    preplace_root_winners(&mut plan, &mut queue, root_deps, member_direct, graph);

    for (importer_idx, deps) in &seeds {
        for dep in *deps {
            let Some(pkg) = graph.packages.get(&dep.dep_path) else {
                continue;
            };
            // A `link:` workspace sibling stays in the requiring member's
            // own `node_modules` (its target owns its deps; Node resolves
            // through the live symlink), matching pnpm's hoistWorkspacePackages
            // layout — do NOT hoist it to the shared root. A registry dep gets
            // `floor = root` so it hoists workspace-wide.
            let floor = if matches!(pkg.local_source.as_ref(), Some(LocalSource::Link(_))) {
                *importer_idx
            } else {
                plan.root_idx
            };
            queue.push_back((*importer_idx, floor, dep.name.clone(), dep.dep_path.clone()));
        }
    }

    while let Some((requester, floor, name, dep_path)) = queue.pop_front() {
        let outcome = plan.place(requester, floor, &name, &dep_path)?;
        if !outcome.created {
            continue;
        }
        let Some(pkg) = graph.packages.get(&dep_path) else {
            continue;
        };
        enqueue_transitives(&mut queue, outcome.node_idx, plan.root_idx, pkg, graph);
    }

    Ok(plan)
}

/// Physical importers as `(node_modules_dir, direct_deps)` in the
/// canonical order [`plan_workspace`] expects — ROOT first, then the
/// remaining physical importers in `graph.importers` (BTreeMap) order.
///
/// Both the installer ([`link_hoisted_importer`]'s workspace caller) and
/// the on-disk recompute ([`HoistedPlacements::from_graph`]) derive the
/// workspace plan from THIS, so they agree on the tree deterministically.
/// Deps are passed raw: the planners skip edges absent from
/// `graph.packages` (workspace siblings, dev-stripped deps), so no
/// pre-filter is needed here.
pub(crate) fn workspace_importer_seeds(
    root_dir: &Path,
    graph: &LockfileGraph,
    modules_dir_name: &str,
) -> Vec<(PathBuf, Vec<DirectDep>)> {
    let importer_nm = |importer_path: &str| -> PathBuf {
        if importer_path == "." {
            root_dir.join(modules_dir_name)
        } else {
            // Collapse `..` lexically so a parent-relative importer key
            // (`../sibling`, when `pnpm-workspace.yaml#packages` uses
            // `../**`) recomputes the same on-disk dir the linker writes.
            aube_util::path::normalize_lexical(&root_dir.join(importer_path).join(modules_dir_name))
        }
    };
    let physical: Vec<(&String, &Vec<DirectDep>)> = graph
        .importers
        .iter()
        .filter(|(p, _)| crate::is_physical_importer(p))
        .collect();
    // The workspace root (`.`) must be an importer: `plan_workspace` treats
    // the first seed as the arena root, so its absence would silently root
    // the whole shared tree under a member's node_modules. The root package
    // is always an importer in every install path today.
    debug_assert!(
        physical.iter().any(|(p, _)| p.as_str() == "."),
        "workspace_importer_seeds: no '.' importer to anchor the shared tree"
    );
    let mut seeds: Vec<(PathBuf, Vec<DirectDep>)> = Vec::with_capacity(physical.len());
    // Root first: it becomes the arena root in `plan_workspace`.
    if let Some((_, deps)) = physical.iter().find(|(p, _)| p.as_str() == ".") {
        seeds.push((importer_nm("."), (*deps).clone()));
    }
    for (path, deps) in &physical {
        if path.as_str() != "." {
            seeds.push((importer_nm(path), (*deps).clone()));
        }
    }
    seeds
}

/// Materialize a planned tree onto disk for a single importer.
///
/// Called by `Linker::link_all` and `Linker::link_workspace` when the
/// linker is configured with `NodeLinker::Hoisted`. The importer's
/// existing `node_modules/` is swept of any top-level entries the
/// plan doesn't claim (direct deps from a previous install may have
/// changed); placed packages are then materialized in two passes —
/// local (`file:`/`link:`) first, then registry packages via the
/// standard reflink/hardlink/copy file-linker.
///
/// Every placed package is recorded in `placements` so the install
/// driver can later resolve `dep_path -> on-disk dir` for bin
/// linking and lifecycle scripts without recomputing the plan.
pub(crate) struct HoistedImporterDirs<'a> {
    pub(crate) root: &'a Path,
    pub(crate) importer: &'a Path,
}

/// What one placement node contributed to the importer-wide totals.
/// Returned by the parallel per-node materializer so the serial caller
/// can fold stats and placement sites in deterministic node order —
/// `placements` is a single `(dep_path, pkg_dir)` for a materialized or
/// `link:` node, empty only on the can't-happen skip paths.
struct NodeOutcome {
    stats: LinkStats,
    placement: Option<(String, PathBuf)>,
}

/// Materialize ONE placement node onto disk. Pure per-node work — it
/// writes only inside `pkg_dir` (or the symlink at `pkg_dir` for a
/// `link:` dep), so nodes at the same placement depth run concurrently
/// without touching each other's directories. Destructive ordering
/// across depths (a parent that ships a bundled `node_modules/<dep>` is
/// wiped + replaced by the deeper real placement) is the caller's
/// responsibility, enforced by a barrier between depth levels.
fn materialize_hoisted_node(
    linker: &Linker,
    dep_path: &str,
    pkg_dir: PathBuf,
    pkg: &LockedPackage,
    package_indices: &BTreeMap<String, PackageIndex>,
    root_dir: &Path,
    nm: &Path,
) -> Result<NodeOutcome, Error> {
    let mut stats = LinkStats::default();

    // `link:` dep: symlink the package dir straight at the target.
    // `link:` packages were excluded from the dependency plan (their
    // target owns its deps); `portal:` stays on the materialized path.
    // Counting toward `top_level_linked` is the caller's job (it adds
    // the root's child count once), so no stat bump here.
    if let Some(LocalSource::Link(rel)) = pkg.local_source.as_ref() {
        if let Some(parent) = pkg_dir.parent() {
            crate::mkdirp(parent)?;
        }
        crate::try_remove_entry(&pkg_dir);
        let abs_target = root_dir.join(rel);
        let link_parent = pkg_dir.parent().unwrap_or(nm);
        let rel_target = pathdiff::diff_paths(&abs_target, link_parent).unwrap_or(abs_target);
        crate::sys::create_dir_link(&rel_target, &pkg_dir)
            .map_err(|e| Error::Io(pkg_dir.clone(), e))?;
        return Ok(NodeOutcome {
            stats,
            placement: Some((dep_path.to_string(), pkg_dir)),
        });
    }

    // Registry (or `file:`) package — needs a PackageIndex to find the
    // store-backed file set. `package_indices` is sparse on warm
    // installs, so lazy-load from the store on miss. `registry_name()`
    // is the lookup key for npm-aliased packages, and integrity is part
    // of the cache key so a same-name dep from a different source can't
    // pick up a registry entry's file list.
    let owned_index;
    let index = match package_indices.get(dep_path) {
        Some(i) => i,
        None => {
            owned_index = linker
                .store
                .load_index(
                    pkg.registry_name(),
                    &pkg.version,
                    linker.index_read_key(pkg),
                )
                .ok_or_else(|| Error::MissingPackageIndex(dep_path.to_string()))?;
            &owned_index
        }
    };

    // Wipe any previous contents (a version change, or a bundled copy
    // a shallower package shipped at this path) so stale files don't
    // survive. Must precede the clone: `clonefile(2)` requires its
    // destination not pre-exist, and the per-file fallback wants a clean
    // dir too.
    crate::try_remove_entry(&pkg_dir);

    // Whole-dir `clonefile(2)` fast path (macOS+APFS, same volume) —
    // the identical primitive the isolated linker uses. The hoisted
    // per-file fill is bound by APFS serializing file creation, so
    // parallelizing it barely moves the needle on macOS; cloning the
    // package's extracted-tree tier in ONE syscall replaces the entire
    // per-file loop and is what makes this layout fast. The tree holds
    // exactly the package's own files (no transitive symlinks — hoisted
    // nests real dirs instead), so the clone reproduces the per-file
    // result byte-for-byte, +x bits included. Any miss (tier unbuilt,
    // non-APFS, cross-volume, non-macOS, or a clone error) returns false
    // and falls through to the unchanged per-file path below.
    let tree_key = linker.virtual_store_subdir(dep_path);
    let tree_src = linker.store.tree_path(&tree_key);
    let pkg_nm_parent = pkg_dir.parent().unwrap_or(&pkg_dir).to_path_buf();
    let used_clonedir = linker.try_clonedir_fill(
        &pkg_dir,
        &pkg_nm_parent,
        &tree_src,
        dep_path,
        pkg,
        index,
        &mut stats,
    )?;

    if !used_clonedir {
        // Per-file fallback: batch-create every intermediate parent
        // directory in one pass, then link each file.
        let mut parents: BTreeSet<PathBuf> = BTreeSet::new();
        parents.insert(pkg_dir.clone());
        for rel_path in index.keys() {
            crate::validate_index_key(rel_path)?;
            let target = pkg_dir.join(rel_path);
            if let Some(parent) = target.parent() {
                parents.insert(parent.to_path_buf());
            }
        }
        for parent in &parents {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(parent.clone(), e))?;
        }

        for (rel_path, stored) in index {
            // Key already validated in the parent-collection loop above;
            // the index is immutable between the two.
            let target = pkg_dir.join(rel_path);
            if let Err(e) = linker.link_file_fresh(stored, rel_path, &target) {
                if let Error::MissingStoreFile { .. } = &e {
                    crate::invalidate_stale_index_for_package(
                        &linker.store,
                        pkg,
                        linker.index_read_key(pkg),
                    );
                }
                return Err(e);
            }
            stats.files_linked += 1;
            linker.note_files_linked(1);
            if stored.executable {
                #[cfg(unix)]
                xx::file::make_executable(&target).map_err(|e| Error::Xx(e.to_string()))?;
            }
        }
    }

    let patch_key = pkg.spec_key();
    if let Some(patch_text) = linker.patches.get(&patch_key) {
        apply_multi_file_patch(&pkg_dir, patch_text)
            .map_err(|msg| Error::Patch(patch_key.clone(), msg))?;
    }

    stats.packages_linked += 1;
    Ok(NodeOutcome {
        stats,
        placement: Some((dep_path.to_string(), pkg_dir)),
    })
}

pub(crate) fn link_hoisted_importer(
    linker: &Linker,
    dirs: HoistedImporterDirs<'_>,
    root_deps: &[DirectDep],
    graph: &LockfileGraph,
    package_indices: &BTreeMap<String, PackageIndex>,
    stats: &mut LinkStats,
    placements: &mut HoistedPlacements,
) -> Result<(), Error> {
    let nm = dirs.importer.join(linker.modules_dir_name());
    let plan = plan_importer(&nm, root_deps, graph, linker.hoisting_limits)?;
    materialize_plan(
        linker,
        dirs.root,
        &plan,
        graph,
        package_indices,
        stats,
        placements,
    )
}

/// Materialize a whole placement plan onto disk and record every placed
/// package in `placements`. The plan may span multiple physical importer
/// `node_modules` (a workspace-spanning `plan_workspace`) or a single one
/// (`plan_importer`); either way each importer node's `node_modules` is
/// created + swept of entries the plan no longer claims, then every
/// placed package is materialized depth level by depth level.
pub(crate) fn materialize_plan(
    linker: &Linker,
    root_dir: &Path,
    plan: &PlacementPlan,
    graph: &LockfileGraph,
    package_indices: &BTreeMap<String, PackageIndex>,
    stats: &mut LinkStats,
    placements: &mut HoistedPlacements,
) -> Result<(), Error> {
    // Per importer: ensure its `node_modules` exists and sweep any
    // top-level entries no longer claimed there. Dotfiles (`.aube`,
    // `.bin`, …) are preserved — `.aube` in particular may hold a
    // previous isolated tree; we leave it rather than wiping the other
    // layout's bytes. A member dep that hoisted to the root is no longer
    // claimed under the member, so this is exactly what converges an
    // old per-importer tree (react duplicated under every member) onto
    // the shared-root layout: the stale member copies are swept here.
    for &imp_idx in &plan.importer_nodes {
        let nm = plan.nodes[imp_idx].nm_dir.clone();
        crate::mkdirp(&nm)?;
        let keep: std::collections::HashSet<&str> = plan.child_names_of(imp_idx).collect();
        crate::sweep_stale_top_level_entries(&nm, &keep, None);
    }

    // Materialize every placed node, parallelized across packages.
    //
    // Correctness hinges on ONE ordering rule: a package may ship a
    // bundled `node_modules/<dep>` inside its own tarball, and the real
    // deeper placement of `<dep>` then wipes + replaces it (child wins).
    // So a node must be fully materialized before any DESCENDANT's
    // destructive wipe + fill runs. We get that for free by processing
    // the placement tree one depth level at a time: BFS gives every
    // child a higher arena index than its parent, so `depth[idx]` folds
    // in a single forward pass, and within a level every node owns a
    // disjoint directory subtree (no two same-depth nodes are
    // ancestor/descendant, and importer nodes have disjoint `nm_dir`
    // subtrees), so their fills never collide. The `collect()` after each
    // level is the barrier between depths. Importer nodes (root + members)
    // carry no `pkg_dir`, so they are skipped below and never materialized.
    //
    // Placements are folded back in level-then-index order so
    // `package_dir()` returns the shallowest site for a dep duplicated
    // across nested `node_modules`.
    let mut depth = vec![0usize; plan.nodes.len()];
    let mut max_depth = 0usize;
    for idx in 0..plan.nodes.len() {
        if let Some(parent) = plan.nodes[idx].parent {
            depth[idx] = depth[parent] + 1;
            max_depth = max_depth.max(depth[idx]);
        }
    }

    // Fallback `nm` for the (unreachable) `link:` node whose `pkg_dir` has
    // no parent; every real placement lives under an importer's tree.
    let root_nm = plan.nodes[plan.root_idx].nm_dir.clone();
    let parallelism = linker.link_parallelism();
    for level in 1..=max_depth {
        // Serial prep: clone the (dep_path, pkg_dir) each node needs and
        // resolve its `LockedPackage`. Cheap; the expensive file I/O runs
        // in the par_iter below.
        let tasks: Vec<(String, PathBuf, &LockedPackage)> = (0..plan.nodes.len())
            .filter(|&idx| depth[idx] == level)
            .filter_map(|idx| {
                let node = &plan.nodes[idx];
                let (Some(dep_path), Some(pkg_dir)) = (&node.dep_path, &node.pkg_dir) else {
                    return None;
                };
                let pkg = graph.packages.get(dep_path)?;
                Some((dep_path.clone(), pkg_dir.clone(), pkg))
            })
            .collect();
        if tasks.is_empty() {
            continue;
        }

        let results: Vec<Result<NodeOutcome, Error>> = with_link_pool(parallelism, || {
            use rayon::prelude::*;
            tasks
                .par_iter()
                .map(|(dep_path, pkg_dir, pkg)| {
                    materialize_hoisted_node(
                        linker,
                        dep_path,
                        pkg_dir.clone(),
                        pkg,
                        package_indices,
                        root_dir,
                        &root_nm,
                    )
                })
                .collect()
        });

        for result in results {
            let outcome = result?;
            stats.files_linked += outcome.stats.files_linked;
            stats.packages_linked += outcome.stats.packages_linked;
            if let Some((dep_path, pkg_dir)) = outcome.placement {
                placements.record(&dep_path, pkg_dir);
            }
        }
    }

    for &imp_idx in &plan.importer_nodes {
        stats.top_level_linked += plan.nodes[imp_idx].children.len();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{DepType, LockedPackage};

    fn dep(name: &str, dep_path: &str) -> DirectDep {
        DirectDep {
            name: name.to_string(),
            dep_path: dep_path.to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }
    }

    fn pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            dep_path: format!("{name}@{version}"),
            dependencies: deps
                .iter()
                .map(|(dep_name, tail)| ((*dep_name).to_string(), (*tail).to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn package_dir(plan: &PlacementPlan, dep_path: &str) -> PathBuf {
        plan.nodes
            .iter()
            .find(|node| node.dep_path.as_deref() == Some(dep_path))
            .and_then(|node| node.pkg_dir.clone())
            .unwrap_or_else(|| panic!("{dep_path} was not placed"))
    }

    /// Like `pkg`, but with an explicit `dep_path` so a single name can carry
    /// peer-context-suffixed variants (`router@6(react@18)` vs `…(react@17)`).
    fn pkg_dp(name: &str, dep_path: &str, deps: &[(&str, &str)]) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: dep_path
                .rsplit_once('@')
                .map_or(dep_path, |(_, v)| v)
                .to_string(),
            dep_path: dep_path.to_string(),
            dependencies: deps
                .iter()
                .map(|(dep_name, tail)| ((*dep_name).to_string(), (*tail).to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn workspace_shared_dep_hoists_to_root_once() {
        // Two members both directly depend on is-odd@3.0.1. Real pnpm's
        // hoisted default (`hoistingLimits=none`) hoists it ONCE to the
        // workspace root; each member resolves it via Node's
        // parent-directory walk. The old per-importer planner duplicated
        // it under every member — issue #484: react duplicated across
        // members → two React module identities → invalid-hook errors.
        let root_nm = PathBuf::from("/ws/node_modules");
        let a_nm = PathBuf::from("/ws/packages/a/node_modules");
        let b_nm = PathBuf::from("/ws/packages/b/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("is-odd@3.0.1".into(), pkg("is-odd", "3.0.1", &[]));
        let seeds = vec![
            (root_nm.clone(), vec![]),
            (a_nm.clone(), vec![dep("is-odd", "is-odd@3.0.1")]),
            (b_nm, vec![dep("is-odd", "is-odd@3.0.1")]),
        ];

        let plan = plan_workspace(&seeds, &graph).unwrap();

        assert_eq!(package_dir(&plan, "is-odd@3.0.1"), root_nm.join("is-odd"));
        assert_eq!(
            plan.nodes
                .iter()
                .filter(|n| n.dep_path.as_deref() == Some("is-odd@3.0.1"))
                .count(),
            1,
            "a dep shared across members must materialize once at root, not per member"
        );
    }

    #[test]
    fn workspace_version_conflict_nests_under_requiring_member() {
        // root + member a want left-pad@1.3.0; member b wants @1.1.3. The
        // majority (1.3.0, two direct consumers) wins the shared root; the
        // minority nests under member b only — matching pnpm, which keeps
        // the shared version at root and nests the conflict.
        let root_nm = PathBuf::from("/ws/node_modules");
        let a_nm = PathBuf::from("/ws/packages/a/node_modules");
        let b_nm = PathBuf::from("/ws/packages/b/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("left-pad@1.3.0".into(), pkg("left-pad", "1.3.0", &[]));
        graph
            .packages
            .insert("left-pad@1.1.3".into(), pkg("left-pad", "1.1.3", &[]));
        let seeds = vec![
            (root_nm.clone(), vec![dep("left-pad", "left-pad@1.3.0")]),
            (a_nm, vec![dep("left-pad", "left-pad@1.3.0")]),
            (b_nm.clone(), vec![dep("left-pad", "left-pad@1.1.3")]),
        ];

        let plan = plan_workspace(&seeds, &graph).unwrap();

        assert_eq!(
            package_dir(&plan, "left-pad@1.3.0"),
            root_nm.join("left-pad")
        );
        assert_eq!(
            package_dir(&plan, "left-pad@1.1.3"),
            b_nm.join("left-pad"),
            "the conflicting minority version nests under the member that needs it"
        );
        assert_eq!(
            plan.nodes
                .iter()
                .filter(|n| n.dep_path.as_deref() == Some("left-pad@1.3.0"))
                .count(),
            1,
            "the shared majority version exists once, at root"
        );
    }

    #[test]
    fn workspace_shared_transitive_hoists_to_root() {
        // member a → foo → shared; member b → bar → shared. `shared` is a
        // transitive of two different members at a single version, so it
        // hoists once to the shared root rather than duplicating under
        // each member's own tree.
        let root_nm = PathBuf::from("/ws/node_modules");
        let a_nm = PathBuf::from("/ws/packages/a/node_modules");
        let b_nm = PathBuf::from("/ws/packages/b/node_modules");
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "foo@1.0.0".into(),
            pkg("foo", "1.0.0", &[("shared", "1.0.0")]),
        );
        graph.packages.insert(
            "bar@1.0.0".into(),
            pkg("bar", "1.0.0", &[("shared", "1.0.0")]),
        );
        graph
            .packages
            .insert("shared@1.0.0".into(), pkg("shared", "1.0.0", &[]));
        let seeds = vec![
            (root_nm.clone(), vec![]),
            (a_nm, vec![dep("foo", "foo@1.0.0")]),
            (b_nm, vec![dep("bar", "bar@1.0.0")]),
        ];

        let plan = plan_workspace(&seeds, &graph).unwrap();

        assert_eq!(package_dir(&plan, "foo@1.0.0"), root_nm.join("foo"));
        assert_eq!(package_dir(&plan, "bar@1.0.0"), root_nm.join("bar"));
        assert_eq!(package_dir(&plan, "shared@1.0.0"), root_nm.join("shared"));
        assert_eq!(
            plan.nodes
                .iter()
                .filter(|n| n.dep_path.as_deref() == Some("shared@1.0.0"))
                .count(),
            1,
        );
    }

    #[test]
    fn workspace_root_direct_dep_owns_root_slot_over_member_version() {
        // The root importer directly depends on left@1.0.0; a member depends
        // on left@2.0.0. The ROOT's declared version must own the shared root
        // slot (the root package resolves `left` from there) and the member's
        // version nests under the member. Regression guard: an earlier
        // revision gave every direct edge equal weight, so a member's version
        // could win the cross-importer contest (here 2.0.0 wins the lexical
        // tiebreak) and displace root's — producing a SECOND node writing
        // `root_nm/left`, i.e. a duplicate racing on one directory.
        let root_nm = PathBuf::from("/ws/node_modules");
        let a_nm = PathBuf::from("/ws/packages/a/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("left@1.0.0".into(), pkg("left", "1.0.0", &[]));
        graph
            .packages
            .insert("left@2.0.0".into(), pkg("left", "2.0.0", &[]));
        let seeds = vec![
            (root_nm.clone(), vec![dep("left", "left@1.0.0")]),
            (a_nm.clone(), vec![dep("left", "left@2.0.0")]),
        ];

        let plan = plan_workspace(&seeds, &graph).unwrap();

        // Exactly one node writes root_nm/left, and it is the root's 1.0.0.
        let root_left: Vec<&str> = plan
            .nodes
            .iter()
            .filter(|n| n.pkg_dir == Some(root_nm.join("left")))
            .filter_map(|n| n.dep_path.as_deref())
            .collect();
        assert_eq!(
            root_left,
            vec!["left@1.0.0"],
            "root's declared version owns the root slot, with no duplicate node"
        );
        // The member's conflicting version nests under the member.
        assert_eq!(package_dir(&plan, "left@2.0.0"), a_nm.join("left"));
    }

    #[test]
    fn workspace_link_sibling_stays_member_local() {
        // A `link:` workspace sibling must NOT hoist to the shared root: it
        // is symlinked into the requiring member's own `node_modules` (its
        // target owns its deps), matching pnpm's hoistWorkspacePackages
        // layout. A registry dep of the same member still hoists to root.
        // Regression guard: an earlier revision hoisted `link:` siblings to
        // the workspace root, emptying the member's `node_modules` and
        // diverging from pnpm (issue #484 super-calendar: @super-calendar/*
        // landed at root instead of under packages/native).
        let root_nm = PathBuf::from("/ws/node_modules");
        let a_nm = PathBuf::from("/ws/packages/a/node_modules");
        let mut graph = LockfileGraph::default();
        let mut core = pkg("@scope/core", "0.0.0", &[]);
        core.dep_path = "@scope/core@0.0.0".into();
        core.local_source = Some(LocalSource::Link(PathBuf::from("packages/core")));
        graph.packages.insert("@scope/core@0.0.0".into(), core);
        graph
            .packages
            .insert("date-fns@4.0.0".into(), pkg("date-fns", "4.0.0", &[]));
        let seeds = vec![
            (root_nm.clone(), vec![]),
            (
                a_nm.clone(),
                vec![
                    dep("@scope/core", "@scope/core@0.0.0"),
                    dep("date-fns", "date-fns@4.0.0"),
                ],
            ),
        ];

        let plan = plan_workspace(&seeds, &graph).unwrap();

        // The link: sibling stays under the member; the registry dep hoists.
        assert_eq!(
            package_dir(&plan, "@scope/core@0.0.0"),
            a_nm.join("@scope/core"),
            "a link: sibling must stay in the requiring member's node_modules"
        );
        assert_eq!(
            package_dir(&plan, "date-fns@4.0.0"),
            root_nm.join("date-fns")
        );
        // Nothing named @scope/core at the workspace root.
        assert!(
            !plan.nodes.iter().any(|n| n.parent == Some(plan.root_idx)
                && n.dep_path.as_deref() == Some("@scope/core@0.0.0")),
            "the link: sibling must not be hoisted to the workspace root"
        );
    }

    #[test]
    fn from_graph_recomputes_workspace_spanning_hoist() {
        // `from_graph` (rebuild/prune's on-disk recompute) must dispatch to
        // the SAME workspace-spanning planner the installer uses under the
        // `none` default, or it would look for a shared dep under a member
        // where the installer never wrote it.
        let root = tempfile::tempdir().unwrap();
        let root_nm = root.path().join("node_modules");
        let lodash_dir = root_nm.join("lodash");
        std::fs::create_dir_all(&lodash_dir).unwrap();
        // Member dirs exist but hold no lodash — it hoisted to the root.
        std::fs::create_dir_all(root.path().join("packages/a/node_modules")).unwrap();
        std::fs::create_dir_all(root.path().join("packages/b/node_modules")).unwrap();

        let mut graph = LockfileGraph::default();
        graph.importers.insert(".".into(), vec![]);
        graph
            .importers
            .insert("packages/a".into(), vec![dep("lodash", "lodash@4.0.0")]);
        graph
            .importers
            .insert("packages/b".into(), vec![dep("lodash", "lodash@4.0.0")]);
        graph
            .packages
            .insert("lodash@4.0.0".into(), pkg("lodash", "4.0.0", &[]));

        let placements = HoistedPlacements::from_graph(
            root.path(),
            &graph,
            "node_modules",
            HoistingLimits::None,
        )
        .unwrap();

        assert_eq!(
            placements.package_dir("lodash@4.0.0"),
            Some(lodash_dir.as_path())
        );
        assert_eq!(
            placements.all_package_dirs("lodash@4.0.0").len(),
            1,
            "recompute must find the shared dep once, at the workspace root"
        );
    }

    #[test]
    fn full_hoist_places_transitive_closure_for_dep_path_edge_values() {
        // The yarn readers store a dependency edge's VALUE as the full dep_path
        // (`bar@1.0.0`), where npm/pnpm store the bare tail (`1.0.0`). The
        // placement BFS must resolve BOTH conventions to the child's graph key.
        // The old inline `name@tail` guess doubled the name for the yarn
        // convention (`bar@bar@1.0.0`), missed `graph.packages`, and silently
        // dropped the entire subtree — cal.com's missing execa/micromatch
        // closure under `nodeLinker: node-modules`.
        let nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        // `foo`'s edge value is the full dep_path (the yarn convention); `baz`
        // is a deeper transitive to prove the whole subtree is walked.
        graph.packages.insert(
            "foo@1.0.0".into(),
            pkg("foo", "1.0.0", &[("bar", "bar@1.0.0")]),
        );
        graph.packages.insert(
            "bar@1.0.0".into(),
            pkg("bar", "1.0.0", &[("baz", "baz@1.0.0")]),
        );
        graph
            .packages
            .insert("baz@1.0.0".into(), pkg("baz", "1.0.0", &[]));
        let root_deps = vec![dep("foo", "foo@1.0.0")];

        let plan = plan_importer(&nm, &root_deps, &graph, HoistingLimits::None).unwrap();

        // The full closure hoists to root; nothing is dropped.
        assert_eq!(package_dir(&plan, "foo@1.0.0"), nm.join("foo"));
        assert_eq!(package_dir(&plan, "bar@1.0.0"), nm.join("bar"));
        assert_eq!(package_dir(&plan, "baz@1.0.0"), nm.join("baz"));
    }

    #[test]
    fn full_hoist_direct_dep_wins_root_over_transitive_majority() {
        // The importer DIRECTLY depends on foo@1.0.0, while five transitive
        // packages each depend on foo@2.0.0 (the transitive majority). The
        // direct edge must own the root slot regardless of the rival's
        // consumer count — the importer resolves `foo` from root and must see
        // its declared 1.0.0. foo@2.0.0 nests under each of its consumers.
        let nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("foo@1.0.0".into(), pkg("foo", "1.0.0", &[]));
        graph
            .packages
            .insert("foo@2.0.0".into(), pkg("foo", "2.0.0", &[]));
        for t in ["t1", "t2", "t3", "t4", "t5"] {
            graph
                .packages
                .insert(format!("{t}@1.0.0"), pkg(t, "1.0.0", &[("foo", "2.0.0")]));
        }
        // foo declared first as a direct dep; the five transitive consumers
        // of foo@2.0.0 follow. Arrival order does NOT decide root here.
        let mut root_deps = vec![dep("foo", "foo@1.0.0")];
        for t in ["t1", "t2", "t3", "t4", "t5"] {
            root_deps.push(dep(t, &format!("{t}@1.0.0")));
        }

        let plan = plan_importer(&nm, &root_deps, &graph, HoistingLimits::None).unwrap();

        // The direct dep wins the root slot; the transitive majority never does.
        assert_eq!(package_dir(&plan, "foo@1.0.0"), nm.join("foo"));
        assert_eq!(
            plan.nodes
                .iter()
                .filter(|n| n.parent == Some(plan.root_idx)
                    && n.dep_path.as_deref() == Some("foo@1.0.0"))
                .count(),
            1,
            "exactly one root-level foo, and it is the direct 1.0.0"
        );
        assert_eq!(
            plan.nodes
                .iter()
                .filter(|n| n.parent == Some(plan.root_idx)
                    && n.dep_path.as_deref() == Some("foo@2.0.0"))
                .count(),
            0,
            "foo@2.0.0 must never sit at root when foo@1.0.0 is a direct dep"
        );
        // The transitive majority nests under each of its five consumers.
        let nested_v2: BTreeSet<PathBuf> = plan
            .nodes
            .iter()
            .filter(|n| n.dep_path.as_deref() == Some("foo@2.0.0"))
            .map(|n| n.pkg_dir.clone().unwrap())
            .collect();
        let expected_v2: BTreeSet<PathBuf> = ["t1", "t2", "t3", "t4", "t5"]
            .iter()
            .map(|t| nm.join(format!("{t}/node_modules/foo")))
            .collect();
        assert_eq!(
            nested_v2, expected_v2,
            "foo@2.0.0 must nest under every consumer, never at root"
        );
    }

    #[test]
    fn full_hoist_seats_majority_version_at_root_over_arrival_order() {
        // `bad` (one consumer of shared@2.0.0) is declared first, so a plain
        // arrival-order BFS would seat shared@2.0.0 at root and force the
        // three shared@1.0.0 consumers to each nest a copy. The majority
        // (shared@1.0.0, three consumers) must win the root slot instead,
        // leaving a single nested shared@2.0.0 under `bad`.
        let nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "bad@1.0.0".into(),
            pkg("bad", "1.0.0", &[("shared", "2.0.0")]),
        );
        for c in ["a", "b", "c"] {
            graph.packages.insert(
                format!("{c}@1.0.0"),
                pkg(c, "1.0.0", &[("shared", "1.0.0")]),
            );
        }
        graph
            .packages
            .insert("shared@1.0.0".into(), pkg("shared", "1.0.0", &[]));
        graph
            .packages
            .insert("shared@2.0.0".into(), pkg("shared", "2.0.0", &[]));
        let root_deps = vec![
            dep("bad", "bad@1.0.0"),
            dep("a", "a@1.0.0"),
            dep("b", "b@1.0.0"),
            dep("c", "c@1.0.0"),
        ];

        let plan = plan_importer(&nm, &root_deps, &graph, HoistingLimits::None).unwrap();

        assert_eq!(package_dir(&plan, "shared@1.0.0"), nm.join("shared"));
        assert_eq!(
            package_dir(&plan, "shared@2.0.0"),
            nm.join("bad/node_modules/shared")
        );
        // Exactly one copy of the majority version, at root.
        assert_eq!(
            plan.nodes
                .iter()
                .filter(|n| n.dep_path.as_deref() == Some("shared@1.0.0"))
                .count(),
            1
        );
    }

    #[test]
    fn full_hoist_three_way_conflict_promotes_the_most_used() {
        // Three versions of `shared`: 1.0.0 has two consumers, 2.0.0 and 3.0.0
        // one each. The two-consumer version wins root; the other two nest.
        let nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("a@1.0.0".into(), pkg("a", "1.0.0", &[("shared", "1.0.0")]));
        graph
            .packages
            .insert("b@1.0.0".into(), pkg("b", "1.0.0", &[("shared", "1.0.0")]));
        graph
            .packages
            .insert("d@1.0.0".into(), pkg("d", "1.0.0", &[("shared", "2.0.0")]));
        graph
            .packages
            .insert("e@1.0.0".into(), pkg("e", "1.0.0", &[("shared", "3.0.0")]));
        for v in ["1.0.0", "2.0.0", "3.0.0"] {
            graph
                .packages
                .insert(format!("shared@{v}"), pkg("shared", v, &[]));
        }
        // Minorities declared first — arrival order must NOT decide root.
        let root_deps = vec![
            dep("d", "d@1.0.0"),
            dep("e", "e@1.0.0"),
            dep("a", "a@1.0.0"),
            dep("b", "b@1.0.0"),
        ];

        let plan = plan_importer(&nm, &root_deps, &graph, HoistingLimits::None).unwrap();

        assert_eq!(package_dir(&plan, "shared@1.0.0"), nm.join("shared"));
        assert_eq!(
            package_dir(&plan, "shared@2.0.0"),
            nm.join("d/node_modules/shared")
        );
        assert_eq!(
            package_dir(&plan, "shared@3.0.0"),
            nm.join("e/node_modules/shared")
        );
    }

    #[test]
    fn full_hoist_materializes_single_version_dep_reachable_only_through_a_winner() {
        // `deep` has a single version whose ONLY consumer is the promoted
        // majority version of `shared`. Pre-placing the winner must still
        // pull `deep` into the tree — the regression this guards is dropping
        // a package reachable solely through a pre-placed winner.
        let nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("a@1.0.0".into(), pkg("a", "1.0.0", &[("shared", "1.0.0")]));
        graph
            .packages
            .insert("b@1.0.0".into(), pkg("b", "1.0.0", &[("shared", "1.0.0")]));
        graph.packages.insert(
            "bad@1.0.0".into(),
            pkg("bad", "1.0.0", &[("shared", "2.0.0")]),
        );
        graph.packages.insert(
            "shared@1.0.0".into(),
            pkg("shared", "1.0.0", &[("deep", "1.0.0")]),
        );
        graph
            .packages
            .insert("shared@2.0.0".into(), pkg("shared", "2.0.0", &[]));
        graph
            .packages
            .insert("deep@1.0.0".into(), pkg("deep", "1.0.0", &[]));
        let root_deps = vec![
            dep("bad", "bad@1.0.0"),
            dep("a", "a@1.0.0"),
            dep("b", "b@1.0.0"),
        ];

        let plan = plan_importer(&nm, &root_deps, &graph, HoistingLimits::None).unwrap();

        // Winner at root, its sole-consumer transitive hoisted alongside it.
        assert_eq!(package_dir(&plan, "shared@1.0.0"), nm.join("shared"));
        assert_eq!(package_dir(&plan, "deep@1.0.0"), nm.join("deep"));
        // The minority still nests.
        assert_eq!(
            package_dir(&plan, "shared@2.0.0"),
            nm.join("bad/node_modules/shared")
        );
    }

    #[test]
    fn full_hoist_promotes_majority_peer_variant() {
        // Two peer-context variants of one name are distinct dep_paths. The
        // variant with more consumers wins root; the minority peer variant
        // nests — promotion keys on the full dep_path, not the bare name.
        let nm = PathBuf::from("/project/node_modules");
        let major = "router@6.0.0(react@18.0.0)";
        let minor = "router@6.0.0(react@17.0.0)";
        let mut graph = LockfileGraph::default();
        for app in ["app1", "app2"] {
            graph.packages.insert(
                format!("{app}@1.0.0"),
                pkg(app, "1.0.0", &[("router", "6.0.0(react@18.0.0)")]),
            );
        }
        graph.packages.insert(
            "app3@1.0.0".into(),
            pkg("app3", "1.0.0", &[("router", "6.0.0(react@17.0.0)")]),
        );
        graph
            .packages
            .insert(major.into(), pkg_dp("router", major, &[]));
        graph
            .packages
            .insert(minor.into(), pkg_dp("router", minor, &[]));
        let root_deps = vec![
            dep("app3", "app3@1.0.0"),
            dep("app1", "app1@1.0.0"),
            dep("app2", "app2@1.0.0"),
        ];

        let plan = plan_importer(&nm, &root_deps, &graph, HoistingLimits::None).unwrap();

        assert_eq!(package_dir(&plan, major), nm.join("router"));
        assert_eq!(
            package_dir(&plan, minor),
            nm.join("app3/node_modules/router")
        );
    }

    #[test]
    fn dependencies_limit_keeps_transitives_under_their_direct_dep() {
        let nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "app@1.0.0".into(),
            pkg("app", "1.0.0", &[("left-pad", "1.0.0")]),
        );
        graph.packages.insert(
            "left-pad@1.0.0".into(),
            pkg("left-pad", "1.0.0", &[("repeat", "1.0.0")]),
        );
        graph
            .packages
            .insert("repeat@1.0.0".into(), pkg("repeat", "1.0.0", &[]));
        let root_deps = vec![dep("app", "app@1.0.0")];

        let unlimited = plan_importer(&nm, &root_deps, &graph, HoistingLimits::None).unwrap();
        assert_eq!(
            package_dir(&unlimited, "left-pad@1.0.0"),
            nm.join("left-pad")
        );
        assert_eq!(package_dir(&unlimited, "repeat@1.0.0"), nm.join("repeat"));

        let limited = plan_importer(&nm, &root_deps, &graph, HoistingLimits::Dependencies).unwrap();
        assert_eq!(
            package_dir(&limited, "left-pad@1.0.0"),
            nm.join("app/node_modules/left-pad")
        );
        assert_eq!(
            package_dir(&limited, "repeat@1.0.0"),
            nm.join("app/node_modules/left-pad/node_modules/repeat")
        );
    }

    #[test]
    fn dependencies_limit_reuses_matching_direct_dependency_above_floor() {
        let nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "app@1.0.0".into(),
            pkg("app", "1.0.0", &[("shared", "1.0.0")]),
        );
        graph
            .packages
            .insert("shared@1.0.0".into(), pkg("shared", "1.0.0", &[]));
        let root_deps = vec![dep("shared", "shared@1.0.0"), dep("app", "app@1.0.0")];

        let limited = plan_importer(&nm, &root_deps, &graph, HoistingLimits::Dependencies).unwrap();

        assert_eq!(package_dir(&limited, "shared@1.0.0"), nm.join("shared"));
        assert_eq!(
            limited
                .nodes
                .iter()
                .filter(|node| node.dep_path.as_deref() == Some("shared@1.0.0"))
                .count(),
            1
        );
    }

    #[test]
    fn dependencies_limit_does_not_reuse_above_version_blocker() {
        let nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "app@1.0.0".into(),
            pkg("app", "1.0.0", &[("shared", "2.0.0"), ("tool", "1.0.0")]),
        );
        graph.packages.insert(
            "tool@1.0.0".into(),
            pkg("tool", "1.0.0", &[("shared", "1.0.0")]),
        );
        graph
            .packages
            .insert("shared@1.0.0".into(), pkg("shared", "1.0.0", &[]));
        graph
            .packages
            .insert("shared@2.0.0".into(), pkg("shared", "2.0.0", &[]));
        let root_deps = vec![dep("shared", "shared@1.0.0"), dep("app", "app@1.0.0")];

        let limited = plan_importer(&nm, &root_deps, &graph, HoistingLimits::Dependencies).unwrap();

        let shared_v1_dirs: Vec<_> = limited
            .nodes
            .iter()
            .filter(|node| node.dep_path.as_deref() == Some("shared@1.0.0"))
            .filter_map(|node| node.pkg_dir.as_ref())
            .collect();
        assert_eq!(shared_v1_dirs.len(), 2);
        assert!(shared_v1_dirs.contains(&&nm.join("shared")));
        assert!(shared_v1_dirs.contains(&&nm.join("app/node_modules/tool/node_modules/shared")));
    }

    #[test]
    fn from_graph_respects_dependencies_limit() {
        let root = tempfile::tempdir().unwrap();
        let nm = root.path().join("node_modules");
        let app_dir = nm.join("app");
        let left_pad_dir = app_dir.join("node_modules/left-pad");
        std::fs::create_dir_all(&left_pad_dir).unwrap();

        let mut graph = LockfileGraph::default();
        graph
            .importers
            .insert(".".into(), vec![dep("app", "app@1.0.0")]);
        graph.packages.insert(
            "app@1.0.0".into(),
            pkg("app", "1.0.0", &[("left-pad", "1.0.0")]),
        );
        graph
            .packages
            .insert("left-pad@1.0.0".into(), pkg("left-pad", "1.0.0", &[]));

        let placements = HoistedPlacements::from_graph(
            root.path(),
            &graph,
            "node_modules",
            HoistingLimits::Dependencies,
        )
        .unwrap();

        assert_eq!(
            placements.package_dir("left-pad@1.0.0"),
            Some(left_pad_dir.as_path())
        );
    }
}
