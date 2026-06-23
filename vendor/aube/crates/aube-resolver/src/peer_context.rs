//! Peer-dependency post-processing over an already-resolved graph.
//!
//! Two user-visible passes live here:
//!
//! * [`hoist_auto_installed_peers`] — temporarily promotes peers declared
//!   by direct dependencies up to importer direct deps so the passes that
//!   walk importer scopes (peer-context resolution, reachability-based
//!   graph filtering) see them the way pnpm's `auto-install-peers=true`
//!   resolution does. The additions are scaffolding, not output: callers
//!   strip them again with [`remove_auto_installed_peers`] once those
//!   passes have run, because pnpm never serializes auto-installed peers
//!   into the lockfile's `importers:` section or links them into the
//!   project's top-level `node_modules/`. Idempotent on graphs that
//!   already ship with those hoists (npm v7+ output).
//! * [`apply_peer_contexts`] — computes pnpm-style `(peer@ver)` suffixes
//!   on contextualized `dep_path`s. Drives the sibling-symlink wiring in
//!   `aube-linker` so each subtree that pins different peer versions gets
//!   its own virtual-store entry.
//!
//! [`detect_unmet_peers`] reports what the two passes above couldn't wire
//! up, so the CLI can surface warnings.
//!
//! Call order from `Resolver::resolve`: `hoist_auto_installed_peers`
//! (fresh resolves only) → `apply_peer_contexts` →
//! `remove_auto_installed_peers` → `detect_unmet_peers`.

use crate::version_satisfies;
use crate::{FxHashMap, FxHashSet};
use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};
use std::collections::{BTreeMap, BTreeSet};

/// A peer dependency whose declared range doesn't match the version the
/// tree actually ends up providing. Emitted as a warning by `aube install`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmetPeer {
    /// dep_path of the package that declared the peer.
    pub from_dep_path: String,
    /// Human-friendly package name (pre-context) for display.
    pub from_name: String,
    /// Name of the peer being declared (e.g. `"react"`).
    pub peer_name: String,
    /// The declared peer range from the package's packument
    /// (e.g. `"^16.8.0 || ^17.0.0 || ^18.0.0"`).
    pub declared: String,
    /// What the tree actually provides, if anything. `None` means the
    /// peer is completely missing — rare in practice because the BFS
    /// auto-install path usually drags *some* version in, but it can
    /// happen for corner cases.
    pub found: Option<String>,
}

/// Scan the resolved graph and return every declared required peer whose
/// resolved version doesn't satisfy its declared range. Optional peers
/// (`peerDependenciesMeta.optional = true`) are skipped — pnpm treats
/// those as "warn suppressed" with `auto-install-peers=true`. The result
/// is purely informational; aube never fails an install on unmet peers,
/// matching pnpm.
///
/// The "found" version for each package comes from its own
/// `dependencies` map — the peer-context pass writes the resolved peer
/// tail there, so we don't have to re-walk ancestors. Any peer suffix on
/// the stored tail is stripped before the semver check so `18.2.0(foo@1)`
/// is treated as `18.2.0`.
pub fn detect_unmet_peers(graph: &LockfileGraph) -> Vec<UnmetPeer> {
    let mut unmet = Vec::new();
    for pkg in graph.packages.values() {
        for (peer_name, declared_range) in &pkg.peer_dependencies {
            let optional = pkg
                .peer_dependencies_meta
                .get(peer_name)
                .map(|m| m.optional)
                .unwrap_or(false);
            if optional {
                continue;
            }

            let found_tail = pkg.dependencies.get(peer_name);
            let found_version = found_tail.map(|t| canonical_tail(t).to_string());

            let satisfied = match &found_version {
                Some(v) => version_satisfies(v, declared_range),
                None => false,
            };
            if satisfied {
                continue;
            }

            unmet.push(UnmetPeer {
                from_dep_path: pkg.dep_path.clone(),
                from_name: pkg.name.clone(),
                peer_name: peer_name.clone(),
                declared: declared_range.clone(),
                found: found_version,
            });
        }
    }
    // Stable order for deterministic test output and readable warnings.
    unmet.sort_by(|a, b| {
        (a.from_dep_path.as_str(), a.peer_name.as_str())
            .cmp(&(b.from_dep_path.as_str(), b.peer_name.as_str()))
    });
    unmet
}

/// Per-importer names of the peers [`hoist_auto_installed_peers`]
/// synthesized into that importer's direct deps. Key: importer path.
/// Feed back into [`remove_auto_installed_peers`] after the passes
/// that needed the hoisted entries have run.
pub type AutoInstalledPeers = BTreeMap<String, BTreeSet<String>>;

/// Temporarily promote direct dependencies' unmet peers to importer
/// direct deps, returning the graph plus a record of what was added.
///
/// Walks each importer's direct dependencies and hoists any peer they
/// declare that isn't already a direct dep of the importer up to the
/// importer's `dependencies` list — mirroring how pnpm's
/// `auto-install-peers=true` resolution treats missing peers as if the
/// root had requested them. The hoisted entries exist so that
/// [`apply_peer_contexts`] can resolve peers from the importer scope and
/// so reachability-based passes (`platform::filter_graph`'s GC) keep
/// peer-only packages alive. They are *not* part of the final graph:
/// pnpm never records auto-installed peers as importer specifiers in
/// `pnpm-lock.yaml` (a lockfile that carries them fails
/// `pnpm install --frozen-lockfile` with `ERR_PNPM_OUTDATED_LOCKFILE`)
/// and never links them into the project's top-level `node_modules/`,
/// so callers must strip the additions with
/// [`remove_auto_installed_peers`] once the peer-context pass has run.
/// Peers declared by transitive dependencies stay in the resolved graph
/// for peer-context sibling wiring and are never hoisted at all.
///
/// Public so lockfile-driven installs that need to re-derive peer
/// wiring (npm/bun formats, which don't record peer contexts)
/// can run this before [`apply_peer_contexts`] to match fresh-resolve
/// behavior. Idempotent in the npm case: npm v7+ already hoists
/// auto-installed peers into root's `dependencies`, so they arrive
/// pre-`satisfied` and no additions are emitted.
///
/// Algorithm:
///   1. For each importer, collect the set of names already in its
///      direct deps. Those are "satisfied" and need no hoist.
///   2. Visit only those direct dependency packages and examine their
///      `peer_dependencies` declarations. For each declared peer not
///      already satisfied by the importer, find a resolved version somewhere
///      in the graph and synthesize a `DirectDep` entry. Mark it as
///      satisfied so a second direct dep doesn't add a duplicate.
///   3. Stable: we walk in-order and take the first declared peer range
///      encountered per name as the specifier. Conflicting ranges across
///      the tree are not reconciled — first one wins. This matches pnpm
///      for the simple case; the complex case is deferred.
///
/// Leaves everything else about the graph untouched — no packages are
/// added or removed, only importer entries grow.
pub fn hoist_auto_installed_peers(mut graph: LockfileGraph) -> (LockfileGraph, AutoInstalledPeers) {
    let mut hoisted_names: AutoInstalledPeers = BTreeMap::new();
    let importer_paths: Vec<String> = graph.importers.keys().cloned().collect();
    for importer_path in importer_paths {
        let Some(direct_deps) = graph.importers.get(&importer_path) else {
            continue;
        };
        let mut satisfied: FxHashSet<String> = direct_deps.iter().map(|d| d.name.clone()).collect();

        // Additions are gathered into a separate vec so we don't mutate
        // the importer's direct-dep list while still borrowing from it.
        let mut additions: Vec<DirectDep> = Vec::new();

        for dep_path in direct_deps.iter().map(|d| &d.dep_path) {
            let Some(pkg) = graph.packages.get(dep_path) else {
                continue;
            };

            // Collect unmet peer declarations from this package.
            for (peer_name, peer_range) in &pkg.peer_dependencies {
                if satisfied.contains(peer_name) {
                    continue;
                }
                // Find any resolved version in the graph for this peer.
                // Prefer the one the package already wired via its own
                // dependencies map (the BFS auto-install result), and
                // fall back to scanning `graph.packages` for a name
                // match. If nothing matches, we quietly drop the peer —
                // that's the only path where aube stays stricter than
                // pnpm today; a future PR will emit an unmet warning.
                //
                // Fallback takes the semver-max version rather than
                // whatever `BTreeMap` iteration order surfaces first —
                // otherwise two resolved `react` entries like `18.0.0`
                // and `18.3.1` would pick the lexicographically-earlier
                // (older) one.
                let resolved_version = pkg.dependencies.get(peer_name).cloned().or_else(|| {
                    // Filter to parseable semver versions *before* the
                    // max_by — returning `Equal` on parse failure makes
                    // the comparator non-transitive, so an unparseable
                    // entry sitting between two valid ones would cause
                    // `max_by` to pick an iteration-order-dependent
                    // result instead of the true maximum.
                    graph
                        .packages
                        .values()
                        .filter(|p| p.name == *peer_name)
                        .filter_map(|p| {
                            node_semver::Version::parse(&p.version)
                                .ok()
                                .map(|v| (v, p.version.clone()))
                        })
                        .max_by(|a, b| a.0.cmp(&b.0))
                        .map(|(_, s)| s)
                });
                let Some(version) = resolved_version else {
                    continue;
                };
                let canonical_version = canonical_tail(&version).to_string();
                let synth_dep_path = format!("{peer_name}@{canonical_version}");
                if !graph.packages.contains_key(&synth_dep_path) {
                    // The peer version the package wired didn't match an
                    // actual package entry — bail out for this peer
                    // rather than writing a dangling DirectDep.
                    continue;
                }
                satisfied.insert(peer_name.clone());
                additions.push(DirectDep {
                    name: peer_name.clone(),
                    dep_path: synth_dep_path,
                    // Peers auto-hoisted to the root are in the prod
                    // graph by convention — matches what pnpm writes.
                    dep_type: DepType::Production,
                    specifier: Some(peer_range.clone()),
                });
            }
        }

        if !additions.is_empty() {
            tracing::debug!(
                "hoisted {} auto-installed peer(s) into importer {}",
                additions.len(),
                importer_path
            );
            hoisted_names.insert(
                importer_path.clone(),
                additions.iter().map(|d| d.name.clone()).collect(),
            );
            if let Some(deps) = graph.importers.get_mut(&importer_path) {
                deps.extend(additions);
                deps.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
    }
    (graph, hoisted_names)
}

/// Strip the importer entries [`hoist_auto_installed_peers`] synthesized,
/// once the peer-context and graph-filter passes that needed them have
/// run.
///
/// This is what keeps the final graph's `importers` a faithful mirror of
/// the manifests: auto-installed peers stay resolved in `packages` (and
/// in each consumer's peer-suffixed snapshot), but they never serialize
/// as importer specifiers and never get a top-level `node_modules/<peer>`
/// link — matching pnpm 10. Removing by name is safe because the hoist
/// only adds names the importer didn't already have.
pub fn remove_auto_installed_peers(graph: &mut LockfileGraph, hoisted: &AutoInstalledPeers) {
    for (importer_path, names) in hoisted {
        if let Some(deps) = graph.importers.get_mut(importer_path) {
            deps.retain(|d| !names.contains(&d.name));
        }
    }
}

/// Walk the resolved graph top-down from each importer and compute a
/// peer-dependency context for every package, producing a new graph whose
/// dep_paths carry pnpm-style `(peer@ver)` suffixes.
///
/// The goal is parity with pnpm's v9 lockfile output: the same
/// `name@version` can appear multiple times — once per distinct set of peer
/// resolutions — so different subtrees that pin incompatible peers get
/// isolated virtual-store entries and truly different sibling-symlink
/// neighborhoods.
///
/// Algorithm per visited package P, reached at some point in a DFS from an
/// importer with `ancestor_scope: name -> dep_path_tail`:
///
///  1. For each peer name declared by P, look it up in `ancestor_scope`
///     (nearest-ancestor-wins, since the scope is rebuilt per recursion).
///     If missing, fall back to P's own entry in `dependencies` — the BFS
///     enqueue above auto-installed it as a transitive, which matches
///     pnpm's `auto-install-peers=true` default.
///  2. Sort the (peer_name, resolution) pairs and serialize as
///     `(n1@v1)(n2@v2)…` for the suffix.
///  3. Produce a contextualized dep_path `name@version{suffix}`. If that
///     key is already in `out_packages` (or currently on the DFS stack via
///     `visiting`), short-circuit — we've already emitted this variant.
///  4. Build a new scope for P's children by merging the ancestor scope
///     with P's own `dependencies` (rewritten to point at contextualized
///     children) and the resolved peer map. Recurse.
///  5. Emit the contextualized LockedPackage.
///
/// Cycles: protected by `visiting` — if a package is re-entered via a
/// dependency cycle, we return the already-computed dep_path without
/// recursing again. The peer context is fixed at first visit; any cycle
/// traversal uses whatever context was live at that first visit.
///
/// Nested peer suffixes: pnpm writes `(react-dom@18.2.0(react@18.2.0))`
/// when a declared peer has its own resolved peers. A single top-down
/// DFS pass can't produce that form, because when a parent P records
/// a peer version in its children's scope, it only knows the canonical
/// tail — the peer's OWN suffix is computed later when the peer itself
/// gets visited. We solve this by running `apply_peer_contexts_once` in
/// a fixed-point loop: the second iteration's input has Pass 1's
/// contextualized tails in every `pkg.dependencies` map, so when a
/// descendant looks a peer up in ancestor scope it sees the full
/// nested tail and serializes it as such. Most peer chains converge in
/// 2–3 iterations; we cap at 16 as a safety belt.
///
/// Limitations (documented as follow-ups in the README):
///   - No per-peer range satisfaction — we take whatever the ancestor has,
///     even if it technically doesn't match P's declared peer range.
///
/// Knobs controlling the peer-context pass. Plumbed from four
/// pnpm-compatible settings (`dedupe-peer-dependents`, `dedupe-peers`,
/// `resolve-peers-from-workspace-root`, `peers-suffix-max-length`)
/// through the `Resolver`'s `with_*` setters.
#[derive(Debug, Clone, Copy)]
pub struct PeerContextOptions {
    /// When true, run the cross-subtree peer-variant collapse pass
    /// after every iteration of the fixed-point loop. Matches pnpm's
    /// default.
    pub dedupe_peer_dependents: bool,
    /// When true, emit suffixes as `(version)` instead of
    /// `(name@version)`. Affects both the package key, the reference
    /// tails stored in `dependencies`, and the cycle-break form of
    /// `contains_canonical_back_ref`.
    pub dedupe_peers: bool,
    /// When true, unresolved peers can be satisfied by a dep declared
    /// at the root importer (`"."`) even if no ancestor scope carries
    /// the peer. Runs between own-deps and graph-wide scan in the
    /// peer-context visitor — see `visit_peer_context` in this
    /// module for the owning implementation (intentionally crate-
    /// private; the public API here is the option flag itself).
    pub resolve_from_workspace_root: bool,
    /// Byte cap on the peer-ID suffix body after which the entire
    /// suffix is replaced by a parenthesized short hash `(<short-hash>)`
    /// (pnpm's `createPeerDepGraphHash`). pnpm's default is 1000.
    pub peers_suffix_max_length: usize,
}

impl Default for PeerContextOptions {
    fn default() -> Self {
        Self {
            dedupe_peer_dependents: true,
            dedupe_peers: false,
            resolve_from_workspace_root: true,
            peers_suffix_max_length: 1000,
        }
    }
}

/// Compute peer-context suffixes over an already-resolved graph.
///
/// Takes a *canonical* graph — one `LockedPackage` per `(name,
/// version)` with `peer_dependencies` populated — and produces a
/// *contextualized* graph whose keys and transitive references carry
/// `(peer@ver)` suffixes when packages resolve peers differently in
/// different subtrees. Drives the sibling-symlink wiring in
/// `aube-linker` for peers, so every fetch/materialize site sees a
/// per-context identity for any package whose peers disambiguate.
///
/// Public so lockfile-driven installs can run the pass over graphs
/// parsed from npm/yarn/bun lockfiles (which emit canonical form —
/// no peer suffixes — and would otherwise leave peer-dependent
/// packages without their peers as `.aube/<pkg>/node_modules/<peer>`
/// siblings). Fresh resolves call it internally from
/// `Resolver::resolve`.
pub fn apply_peer_contexts(
    canonical: LockfileGraph,
    options: &PeerContextOptions,
) -> Result<LockfileGraph, crate::Error> {
    const MAX_ITERATIONS: usize = 16;
    let mut current = canonical;
    let mut converged = false;
    // Hash both keys and dependency tails. A peer-context iteration can
    // rewrite a dependency value to point at an existing key without
    // adding a new key, so a key-only convergence test ships partially
    // rewritten tails. Linker reads tails directly to locate sibling
    // symlink targets, stale tails produce broken `node_modules`.
    let graph_hash = |g: &LockfileGraph| -> u64 {
        let total_deps: usize = g.packages.values().map(|p| p.dependencies.len()).sum();
        let mut tokens: Vec<&str> = Vec::with_capacity(g.packages.len() * 3 + total_deps * 2);
        for (k, pkg) in &g.packages {
            tokens.push(k.as_str());
            tokens.push("\x1f");
            for (name, tail) in &pkg.dependencies {
                tokens.push(name.as_str());
                tokens.push(tail.as_str());
            }
            tokens.push("\x1e");
        }
        aube_util::hash::ordered_seq_hash(tokens.iter().copied())
    };
    // Carry the post-iteration hash forward as the next iteration's
    // pre-hash. Saves one full graph walk per iteration (the loop runs
    // up to 16 times; each `graph_hash` allocates a Vec<&str> sized
    // to `pkgs * 3 + deps * 2` tokens — ~25k entries on a 1000-pkg
    // graph). One hash per iter instead of two.
    let mut before = graph_hash(&current);
    for i in 0..MAX_ITERATIONS {
        let after_once = apply_peer_contexts_once(current, options);
        let next = if options.dedupe_peer_dependents {
            dedupe_peer_variants(after_once)
        } else {
            after_once
        };
        let after = graph_hash(&next);
        if before == after {
            tracing::debug!("peer-context pass converged after {i} iteration(s)");
            current = next;
            converged = true;
            break;
        }
        current = next;
        before = after;
    }
    if !converged {
        // Iteration cap hit. Returning the partial graph would ship
        // broken node_modules. Now fatal.
        tracing::error!(
            code = aube_codes::errors::ERR_AUBE_PEER_CONTEXT_NOT_CONVERGED,
            max_iterations = MAX_ITERATIONS,
            "peer-context hit MAX_ITERATIONS={MAX_ITERATIONS} without convergence"
        );
        return Err(crate::Error::PeerContextDivergence(MAX_ITERATIONS));
    }
    // Propagate each package's peer-suffix segments up through its
    // non-peer-declaring ancestors so a parent that pulls in a peer-
    // bearing descendant carries the same `(peer@version)` suffix on
    // its own dep_path. Matches pnpm's lockfile shape — pnpm 9 emits
    // every peer-bearing package's resolved peer set on every
    // ancestor in the chain (importer rows included), even when the
    // ancestor itself doesn't declare those peers. Without the
    // propagation aube would tag the suffix only on the package that
    // declares peers, which differs from pnpm-lock.yaml in the
    // `importers:` section any time a non-peer-declaring middle node
    // sits between an importer and its peer-bearing descendant.
    //
    // Runs after the fixed-point loop converges so all self-suffixes
    // are stable, and before `dedupe_peer_suffixes` so the latter's
    // `(name@version)` → `(version)` collapse acts on the propagated
    // form too.
    let current = propagate_peer_suffixes_to_ancestors(current, options);
    // `dedupe-peers=true` rewrites the parenthesized peer suffix to
    // drop the `name@` prefix. Done as a post-pass rather than inline
    // so cycle detection during the fixed-point loop keeps the full
    // `name@version` form (otherwise unrelated same-version packages
    // would false-positive as back-references).
    let result = if options.dedupe_peers {
        dedupe_peer_suffixes(current)
    } else {
        current
    };
    Ok(result)
}

/// Cross-subtree peer-variant dedupe. When `dedupe-peer-dependents` is
/// on, packages that landed at different contextualized dep_paths but
/// resolved every declared peer to the *same* version (ignoring the
/// nested peer suffix on each peer tail) collapse into a single
/// canonical variant — chosen as the lexicographically smallest key in
/// the equivalence class. References in every surviving
/// `LockedPackage.dependencies` map and every `importers[*]` direct
/// dep get rewritten through the old→canonical map, and the
/// non-canonical entries are dropped from `packages`.
///
/// Packages whose `peer_dependencies` map is empty — i.e. the canonical
/// base already has only one variant — are skipped.
pub(crate) fn dedupe_peer_variants(graph: LockfileGraph) -> LockfileGraph {
    let canonical_base = |key: &str| -> String { canonical_tail(key).to_string() };
    // Only the peer-bearing part of the resolved peer tail is
    // comparable across subtrees — the nested suffix could differ even
    // for peer-equivalent variants on mid-iterations of the outer
    // fixed-point loop.
    let peer_base = |tail: &str| -> String { canonical_tail(tail).to_string() };

    // Group dep_paths by their peer-free base name.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in graph.packages.keys() {
        groups
            .entry(canonical_base(key))
            .or_default()
            .push(key.clone());
    }

    let mut rewrite: BTreeMap<String, String> = BTreeMap::new();
    for (_base, mut keys) in groups {
        if keys.len() < 2 {
            continue;
        }
        // Deterministic order for canonical selection + stable hashing.
        keys.sort();
        // Union-find over equivalence classes. Two variants are
        // equivalent when each declared peer name resolves to the same
        // peer base in both (or is missing from both).
        let mut parent: Vec<usize> = (0..keys.len()).collect();
        fn find(parent: &mut [usize], i: usize) -> usize {
            if parent[i] == i {
                i
            } else {
                let r = find(parent, parent[i]);
                parent[i] = r;
                r
            }
        }
        for i in 0..keys.len() {
            for j in (i + 1)..keys.len() {
                let pa = &graph.packages[&keys[i]];
                let pb = &graph.packages[&keys[j]];
                // Same canonical version is required — packages with
                // different versions but the same name would share no
                // canonical_base only if the name-without-version
                // collided, which doesn't happen (version is in the
                // base). Still, belt-and-suspenders.
                if pa.version != pb.version {
                    continue;
                }
                let peer_names: BTreeSet<&String> = pa
                    .peer_dependencies
                    .keys()
                    .chain(pb.peer_dependencies.keys())
                    .collect();
                let equivalent = peer_names.iter().all(|name| {
                    match (
                        pa.dependencies.get(name.as_str()),
                        pb.dependencies.get(name.as_str()),
                    ) {
                        (Some(va), Some(vb)) => peer_base(va) == peer_base(vb),
                        (None, None) => true,
                        _ => false,
                    }
                });
                if equivalent {
                    let ri = find(&mut parent, i);
                    let rj = find(&mut parent, j);
                    if ri != rj {
                        parent[ri] = rj;
                    }
                }
            }
        }
        // Build class → canonical (smallest key) mapping. Using
        // index-based iteration here because `find` takes a mutable
        // reference into `parent`, so holding an immutable borrow
        // from `keys.iter()` at the same time would double-borrow.
        #[allow(clippy::needless_range_loop)]
        {
            let mut class_rep: BTreeMap<usize, String> = BTreeMap::new();
            for i in 0..keys.len() {
                let root = find(&mut parent, i);
                class_rep
                    .entry(root)
                    .and_modify(|cur| {
                        if keys[i] < *cur {
                            *cur = keys[i].clone();
                        }
                    })
                    .or_insert_with(|| keys[i].clone());
            }
            for i in 0..keys.len() {
                let root = find(&mut parent, i);
                let canonical = class_rep[&root].clone();
                if keys[i] != canonical {
                    rewrite.insert(keys[i].clone(), canonical);
                }
            }
        }
    }

    if rewrite.is_empty() {
        return graph;
    }

    // Rewrite package dependency tails and keep only canonicals.
    let LockfileGraph {
        importers,
        packages,
        settings,
        overrides,
        package_extensions_checksum,
        pnpmfile_checksum,
        ignored_optional_dependencies,
        times,
        skipped_optional_dependencies,
        catalogs,
        bun_config_version,
        patched_dependencies,
        patched_dependency_hashes,
        trusted_dependencies,
        runtimes,
        extra_fields,
        workspace_extra_fields,
    } = graph;

    let mut new_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    for (key, mut pkg) in packages {
        if rewrite.contains_key(&key) {
            continue;
        }
        for (dep_name, dep_tail) in pkg.dependencies.iter_mut() {
            let dep_key = format!("{dep_name}@{dep_tail}");
            if let Some(canonical) = rewrite.get(&dep_key) {
                let new_tail = canonical
                    .strip_prefix(&format!("{dep_name}@"))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| canonical.clone());
                *dep_tail = new_tail;
            }
        }
        new_packages.insert(key, pkg);
    }

    let mut new_importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();
    for (importer_path, deps) in importers {
        let mut new_deps = Vec::with_capacity(deps.len());
        for mut dep in deps {
            if let Some(canonical) = rewrite.get(&dep.dep_path) {
                dep.dep_path = canonical.clone();
            }
            new_deps.push(dep);
        }
        new_importers.insert(importer_path, new_deps);
    }

    LockfileGraph {
        importers: new_importers,
        packages: new_packages,
        settings,
        overrides,
        package_extensions_checksum,
        pnpmfile_checksum,
        ignored_optional_dependencies,
        times,
        skipped_optional_dependencies,
        catalogs,
        bun_config_version,
        patched_dependencies,
        patched_dependency_hashes,
        trusted_dependencies,
        runtimes,
        extra_fields,
        workspace_extra_fields,
    }
}

/// Single pass of the peer-context computation. See `apply_peer_contexts`
/// for the wrapping fixed-point loop.
///
/// Algorithm per visited package P, reached at some point in a DFS from an
/// importer with `ancestor_scope: name -> dep_path_tail`:
///
///  1. For each peer name declared by P, look it up in `ancestor_scope`
///     (nearest-ancestor-wins, since the scope is rebuilt per recursion).
///     If missing, fall back to P's own entry in `dependencies` — the BFS
///     enqueue auto-installed it as a transitive, matching pnpm's
///     `auto-install-peers=true` default.
///  2. Sort the (peer_name, resolution) pairs and serialize as
///     `(n1@v1)(n2@v2)…` for the suffix.
///  3. Produce a contextualized dep_path `name@version{suffix}`. If that
///     key is already in `out_packages` (or currently on the DFS stack via
///     `visiting`), short-circuit — we've already emitted this variant.
///  4. Build a new scope for P's children by merging the ancestor scope
///     with P's own `dependencies` and the resolved peer map. Recurse.
///  5. Emit the contextualized LockedPackage.
///
/// Cycles: protected by `visiting` — if a package is re-entered via a
/// dependency cycle, we return the already-computed dep_path without
/// recursing again. The peer context is fixed at first visit; any cycle
/// traversal uses whatever context was live at that first visit.
fn apply_peer_contexts_once(
    canonical: LockfileGraph,
    options: &PeerContextOptions,
) -> LockfileGraph {
    let mut out_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut new_importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();

    // Name-indexed view of the canonical graph, shared across
    // every `visit_peer_context` call in this pass. Peer-resolution
    // scan-by-name is the resolver's hottest inner loop. Without
    // this, each peer runs `O(|graph|)` per package per fixed-point
    // iter. Prebuilt index drops the scan to O(1) average.
    //
    // Pre-size to the package count: most graphs have one entry per
    // name and only a handful of multi-version names, so capacity
    // headroom is small and the upper bound saves 8+ rehashes on
    // medium graphs (default 16 → 2048 covers ~1200 pkgs).
    let mut name_index: FxHashMap<&str, Vec<&LockedPackage>> =
        FxHashMap::with_capacity_and_hasher(canonical.packages.len(), Default::default());
    for pkg in canonical.packages.values() {
        name_index.entry(pkg.name.as_str()).or_default().push(pkg);
    }

    // Root-importer scope used by `resolve-peers-from-workspace-root`.
    // Computed once from the canonical input so it reflects the
    // contextualized state of every root dep on fixed-point iterations
    // 2+ — same logic as per-importer `importer_scope` below.
    let root_scope: FxHashMap<String, String> = canonical
        .importers
        .get(".")
        .map(|deps| scope_map_from_deps(deps))
        .unwrap_or_default();

    for (importer_path, direct_deps) in &canonical.importers {
        // An importer's own direct deps are in scope for its children's
        // peer resolution — this is how pnpm's "auto-install at the root"
        // path gets peer links that point at root-level packages.
        //
        // Use the *full contextualized tail* off each DirectDep rather
        // than the package's plain version. On Pass 1 of the fixed-point
        // loop the tail is canonical and equal to `p.version`; on Pass 2+
        // it's already contextualized, and passing the plain version
        // would make descendants look up keys that don't exist in the
        // (now-nested) graph.
        let importer_scope = scope_map_from_deps(direct_deps);

        let mut new_deps = Vec::with_capacity(direct_deps.len());
        for dep in direct_deps {
            // `visiting` is the DFS stack guard for this particular descent
            // — reset per direct dep so we don't incorrectly flag a package
            // as a cycle when it's reached again from a sibling subtree.
            // The shared `out_packages` still dedupes across siblings since
            // the second visit hits the `contains_key` short-circuit below.
            //
            // Invariant (see `visit_peer_context` for the detailed handling):
            // a dep_path returned from the cycle-break branch may not yet
            // be present in `out_packages` at the moment of return, because
            // the package is still being assembled up the call stack. The
            // parent that records the returned tail will complete its own
            // insertion before the recursion unwinds, so by the time
            // anything reads the graph, every referenced dep_path exists.
            let mut visiting: FxHashSet<String> = FxHashSet::default();
            let new_dep_path = visit_peer_context(
                &dep.dep_path,
                &canonical,
                &name_index,
                &importer_scope,
                &root_scope,
                &mut out_packages,
                &mut visiting,
                options,
            )
            .unwrap_or_else(|| dep.dep_path.clone());
            new_deps.push(DirectDep {
                name: dep.name.clone(),
                dep_path: new_dep_path,
                dep_type: dep.dep_type,
                specifier: dep.specifier.clone(),
            });
        }
        new_importers.insert(importer_path.clone(), new_deps);
    }

    // Any canonical package that was never reached by the DFS (orphaned
    // from every importer) is dropped — that matches the filter_deps
    // semantics and avoids emitting dead entries into the lockfile.

    LockfileGraph {
        importers: new_importers,
        packages: out_packages,
        // The post-pass is pure — settings + overrides carry through
        // from the input graph untouched.
        settings: canonical.settings,
        overrides: canonical.overrides,
        package_extensions_checksum: canonical.package_extensions_checksum,
        pnpmfile_checksum: canonical.pnpmfile_checksum,
        ignored_optional_dependencies: canonical.ignored_optional_dependencies,
        runtimes: canonical.runtimes,
        times: canonical.times,
        skipped_optional_dependencies: canonical.skipped_optional_dependencies,
        catalogs: canonical.catalogs,
        bun_config_version: canonical.bun_config_version,
        patched_dependencies: canonical.patched_dependencies,
        patched_dependency_hashes: canonical.patched_dependency_hashes,
        trusted_dependencies: canonical.trusted_dependencies,
        extra_fields: canonical.extra_fields,
        workspace_extra_fields: canonical.workspace_extra_fields,
    }
}

/// DFS helper for `apply_peer_contexts`. Returns the peer-contextualized
/// dep_path of the visited package, or `None` if the canonical package is
/// missing (shouldn't happen in practice but we degrade gracefully).
/// Does `value` contain a peer-suffix reference to `canonical` as a
/// proper name@version boundary (i.e. preceded by `(` and followed by
/// `(` / `)` / end-of-string)? Used by the peer-context pass to detect
/// when a nested tail loops back to the current package so it can
/// short-circuit the chain instead of growing the suffix forever.
/// Everything before the first `(` — i.e. the canonical `name@version`
/// part of a dep-path with the peer-context suffix stripped. Returns
/// the original string when no `(` is present. Borrowed; callers that
/// need owned bump with `.to_string()`.
fn canonical_tail(s: &str) -> &str {
    s.split('(').next().unwrap_or(s)
}

/// Build a `name → contextualized tail` map from a direct-dep slice.
/// The tail is the dep_path with the `{name}@` prefix stripped, which
/// on pass 1 is equal to `pkg.version` and on pass 2+ carries the
/// nested peer-context suffix. Used both for the root scope and for
/// each importer's own scope inside `apply_peer_contexts_once`.
fn scope_map_from_deps(deps: &[DirectDep]) -> FxHashMap<String, String> {
    let mut out = FxHashMap::with_capacity_and_hasher(deps.len(), Default::default());
    for d in deps {
        let prefix_len = d.name.len() + 1;
        let tail = if d.dep_path.len() > prefix_len
            && d.dep_path.as_bytes().get(d.name.len()) == Some(&b'@')
            && d.dep_path.as_bytes().starts_with(d.name.as_bytes())
        {
            d.dep_path[prefix_len..].to_string()
        } else {
            d.dep_path.clone()
        };
        out.insert(d.name.clone(), tail);
    }
    out
}

/// True when `s` is a single hashed peer suffix `(<32 lowercase hex>)`
/// as emitted by [`effective_peer_suffix`] once a suffix exceeds
/// `peersSuffixMaxLength`. The hashed form discards the textual peer
/// set, so the propagation pass recognizes such keys and leaves them
/// untouched (their per-peer contribution can't be recovered). A real
/// peer segment always contains `@`, so the all-hex check can't
/// false-positive on a `(name@version)` group.
pub(crate) fn is_hashed_peer_suffix(s: &str) -> bool {
    let Some(inner) = s.strip_prefix('(').and_then(|x| x.strip_suffix(')')) else {
        return false;
    };
    inner.len() == 32
        && inner
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// pnpm's `createShortHash`: the lowercase SHA-256 hex digest of
/// `input`, truncated to its first 32 characters (16 bytes).
fn short_peer_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Final peer-context tail for an already-built `(name@version)…`
/// `suffix`, mirroring pnpm's `createPeerDepGraphHash`. pnpm derives
/// `dirName` by joining the sorted peer ids with `)(` — i.e. the suffix
/// without its outer parens — hashes it with `createShortHash` when it
/// exceeds `peersSuffixMaxLength`, and always re-wraps the result in a
/// single `(...)`. Keeping that shape means a capped suffix aube writes
/// into `pnpm-lock.yaml` is `(<short-hash>)` — byte-compatible with
/// pnpm — never a bare `_<hex>` marker.
pub(crate) fn effective_peer_suffix(suffix: &str, max_length: usize) -> String {
    // `dir_name` == pnpm's `dirName`: the suffix without the outer `(`
    // and `)` that wrap the first and last peer segment. `suffix` is
    // always a concatenation of `(…)` groups here, so stripping one
    // byte off each end is safe; an empty suffix degrades to empty.
    let dir_name = suffix
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(suffix);
    if dir_name.len() > max_length {
        format!("({})", short_peer_hash(dir_name))
    } else {
        suffix.to_string()
    }
}

pub(crate) fn contains_canonical_back_ref(value: &str, canonical: &str) -> bool {
    let bytes = value.as_bytes();
    let target = canonical.as_bytes();
    if target.is_empty() || target.len() > bytes.len() {
        return false;
    }
    let mut i = 0;
    while i + target.len() <= bytes.len() {
        if &bytes[i..i + target.len()] == target {
            let before = if i == 0 { b'\0' } else { bytes[i - 1] };
            let after = bytes.get(i + target.len()).copied().unwrap_or(b'\0');
            let before_ok = before == b'(';
            let after_ok = after == b'(' || after == b')' || after == b'\0';
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Split a dep_path tail's peer suffix into outer-level paren segments
/// (each ending in a balanced `)`). Returns each segment with its parens
/// included — `react-dom@18.2.0(react@18.2.0)(scheduler@1.0.0)` yields
/// `["(react@18.2.0)", "(scheduler@1.0.0)"]`; nested forms like
/// `consumer@1.0.0(react-dom@18.2.0(react@18.2.0))` yield the single
/// segment `["(react-dom@18.2.0(react@18.2.0))"]` with the inner
/// `(react@18.2.0)` preserved verbatim inside it.
///
/// Used by `propagate_peer_suffixes_to_ancestors` to lift a child's
/// peer segments onto its non-peer-declaring ancestors.
fn outer_paren_segments(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut segments = Vec::new();
    let mut i = 0;
    // Skip canonical `name@version` head — anything up to the first `(`.
    while i < bytes.len() && bytes[i] != b'(' {
        i += 1;
    }
    while i < bytes.len() {
        if bytes[i] != b'(' {
            i += 1;
            continue;
        }
        let start = i;
        let mut depth: i32 = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        segments.push(&s[start..i]);
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            // Unbalanced — bail out of further segmenting. Shouldn't
            // happen on output of `apply_peer_contexts_once`, where every
            // suffix segment is balanced by construction.
            break;
        }
    }
    segments
}

/// Extract the peer name from a paren segment like `(@scope/name@1.2.3)`
/// or `(name@1.2.3(nested@9.9.9))`. The peer name is everything between
/// the opening `(` and the LAST `@` that occurs before any nested `(`.
/// Scoped packages contain two `@`s (`@scope/name@version`) and we want
/// the rightmost outer one.
///
/// Returns `None` if the segment doesn't start with `(` or has no
/// usable `@` separator.
fn peer_name_from_segment(seg: &str) -> Option<&str> {
    let inner = seg.strip_prefix('(')?;
    // Scan for the last `@` that occurs before any `(` (the version-or-
    // nested boundary). For a flat segment `name@version` everything
    // between `(` and the last `@` is the name; for a nested segment
    // `name@version(inner)` the last `@` BEFORE the first inner `(` is
    // the boundary. We search up to the first `(` (or end-of-string).
    let scan_end = inner.find('(').unwrap_or(inner.len());
    let head = &inner[..scan_end];
    head.rfind('@').map(|idx| &head[..idx])
}

/// Collect every peer name reachable from a set of outer-paren segments,
/// recursing into nested `(name@version(...))` forms so that a self
/// segment like `(helper@1.0.0(core@1.0.0))` reports both `helper` and
/// `core`. Used by `propagate_peer_suffixes_to_ancestors` to suppress
/// flat-segment additions for peer names already encoded transitively
/// in a package's own (possibly nested) self-suffix.
fn peer_names_in_segments_recursive(segments: &[&str]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for seg in segments {
        if let Some(name) = peer_name_from_segment(seg) {
            names.insert(name.to_string());
        }
        // Recurse into the nested portion (everything after the first
        // inner `(` and before the final `)`).
        let Some(inner) = seg.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
            continue;
        };
        if let Some(open) = inner.find('(') {
            let nested = &inner[open..];
            let nested_segments = outer_paren_segments(nested);
            for nested_name in peer_names_in_segments_recursive(&nested_segments) {
                names.insert(nested_name);
            }
        }
    }
    names
}

/// Walk the resolved graph from each node and accumulate the union of
/// peer-suffix segments contributed by self + every reachable
/// descendant — for *every* node, regardless of whether it declares
/// its own peers — then rewrite each node's dep_path to embed that
/// union.
///
/// Why: pnpm's lockfile shape tags intermediaries with the same
/// `(peer@version)` suffix their peer-bearing descendants produced —
/// so a parent that pulls in a peer-bearing child carries the
/// resolved peer set on its own dep_path. aube's
/// `apply_peer_contexts_once` only emits the suffix on the package
/// that *declares* the peer; without this post-pass an importer row
/// for `parent → leaf(peer)` would render `parent: 1.0.0` (no
/// suffix) where pnpm renders `parent: 1.0.0(peer@v)`.
///
/// pnpm-parity: **a package's key suffix is the UNION of its own
/// resolved peers and every descendant peer that bubbles up** —
/// declaring your own `peerDependencies` does *not* suppress
/// descendant-peer propagation. This matches pnpm's
/// `resolvePeersOfNode`
/// (`installing/deps-resolver/src/resolvePeers.ts`): children's
/// resolved peers (`unknownResolvedPeersOfChildren`) are accumulated
/// unconditionally and the package's own declared peers are merged on
/// top (`allResolvedPeers`); there is no branch that stops descendant
/// propagation when the package declares peers. The previous code
/// early-returned on `has_own_peers`, dropping descendant peers from
/// a peer-declarer's key — that diverged from pnpm and churned the
/// lockfile (different `node_modules/.pnpm` paths). Two observable
/// shapes the corrected behavior lines up with:
///   - `@testing-library/react@14.0.0(react@18.2.0)(react-dom@18.2.0(react@18.2.0))`
///     — declares peers; its descendant peers union into the suffix
///     too (here `react`/`react-dom` are exactly the declared set, so
///     the union adds nothing visible, but a descendant peer the
///     package does *not* declare would still bubble up).
///   - `abc-parent-with-missing-peers@1.0.0(peer-a@…)(peer-b@…)(peer-c@…)`
///     — no declared peers, picks up descendant peers from `abc`.
///
/// Algorithm:
///  1. Build a forward dep map: `pkg_key → [child_key]` from each
///     LockedPackage's `dependencies`.
///  2. Memoized DFS. For each node, compute
///     `cumulative_segments = outer_paren_segments(node.key)`, then
///     union in `⋃ cumulative(child)` for every node.
///  3. Cycles short-circuit via a `visiting` guard — cycle members
///     can't add new peers from each other beyond what reaches them
///     through non-cycle paths, so returning the empty set on
///     re-entry is safe (the non-cycle entry path computes the full
///     set).
///  4. Dedupe by peer name. Suppressed names: every peer name reachable
///     transitively in self-segments (so `(helper@1(core@1))` covers
///     `core` and a flat `(core@1)` from descendants is dropped); the
///     package's own canonical name (mutual-peer cycle break); and
///     every direct-child name of the node — pnpm's
///     `resolvePeersOfChildren` only bubbles up child-resolved peers
///     whose alias is NOT a direct child (`if (!children[alias])`), so
///     a descendant peer the node *also* directly depends on is
///     resolved locally and does not over-union onto the node's key.
///  5. Build a rewrite map `old_key → new_key` and apply to package
///     keys, dep edges (each dep's stored tail), and importer
///     dep_paths.
fn propagate_peer_suffixes_to_ancestors(
    graph: LockfileGraph,
    options: &PeerContextOptions,
) -> LockfileGraph {
    // Forward dep map. Edges that don't resolve to a present package
    // (e.g. an unresolved peer that `detect_unmet_peers` will warn
    // about) are dropped — they can't contribute cumulative peers.
    let mut forward: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, pkg) in &graph.packages {
        let children: Vec<String> = pkg
            .dependencies
            .iter()
            .map(|(n, t)| format!("{n}@{t}"))
            .filter(|k| graph.packages.contains_key(k))
            .collect();
        forward.insert(key.clone(), children);
    }

    // Memoized DFS. `cumulative` stores the by-name segment map per
    // package key; `visiting` is the cycle-break stack.
    let mut cumulative: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut visiting: BTreeSet<String> = BTreeSet::new();

    fn collect(
        key: &str,
        forward: &BTreeMap<String, Vec<String>>,
        cumulative: &mut BTreeMap<String, BTreeMap<String, String>>,
        visiting: &mut BTreeSet<String>,
    ) -> BTreeMap<String, String> {
        if let Some(c) = cumulative.get(key) {
            return c.clone();
        }
        if !visiting.insert(key.to_string()) {
            // Cycle: contribute nothing. Whichever cycle member is
            // first reached from outside the cycle will compute the
            // full set; the visit guard cap on the others prevents
            // infinite recursion. Edge case: a fully-isolated cycle
            // never gets a non-cycle entry, in which case all members
            // compute empty cumulatives — that's identical to their
            // canonical state, so they get no rewrite. Acceptable.
            return BTreeMap::new();
        }

        // Self-suffix segments. Each segment becomes one (name → segment)
        // entry. Nested segments like `(react-dom@18.2.0(react@18.2.0))`
        // are preserved as a single segment with the nested form intact.
        let self_segments = outer_paren_segments(key);
        let mut acc: BTreeMap<String, String> = BTreeMap::new();
        for seg in &self_segments {
            if let Some(name) = peer_name_from_segment(seg) {
                acc.entry(name.to_string())
                    .or_insert_with(|| seg.to_string());
            }
        }

        // pnpm-parity: descendant peers union into EVERY node's suffix,
        // including packages that declare their own peers — pnpm's
        // `resolvePeersOfNode` accumulates children's resolved peers
        // unconditionally and merges the package's own declared peers on
        // top (`allResolvedPeers`), with no "has own peers ⇒ stop
        // propagating" branch. So there is no early return here; the
        // child-contribution merge below runs for all nodes. (A package's
        // *own* declared peers already appear in `acc` as self-segments
        // — those were emitted by `apply_peer_contexts_once` into the
        // node key — so the union is self ∪ descendants for every node.)

        // Names suppressed when merging child contributions:
        //   1. Every peer name reachable transitively in self segments —
        //      e.g. a self segment `(helper@1.0.0(core@1.0.0))` covers
        //      both `helper` and `core`, so a descendant flat-listing
        //      `(core@1.0.0)` shouldn't double-emit. Pnpm lists each
        //      peer name once; we match.
        //   2. The package's own canonical name — for mutual-peer
        //      cycles `a` peers on `b` and `b` peers on `a`, the
        //      descendant set lifts `(a@…)` back up onto `a` itself,
        //      which would write `a@1.0.0(a@…)(b@…)`. Self-listing
        //      isn't valid pnpm shape; suppress it. (Reachable here
        //      only when this branch handles a node with no declared
        //      peers — but defensive in case future graph shapes
        //      surface a self-cycle through a peer-less node.)
        //   3. Every DIRECT-child name of this node — pnpm's
        //      `resolvePeersOfChildren` builds the set that bubbles up
        //      (`unknownResolvedPeersOfChildren`) by keeping only
        //      child-resolved peers whose alias is NOT itself a direct
        //      child of the node: `for (const [alias, v] of
        //      allResolvedPeers) { if (!children[alias]) { … } }`
        //      (`installing/deps-resolver/src/resolvePeers.ts`). A peer
        //      a node ALSO directly depends on is resolved locally and
        //      does not bubble onto the node's own key. Without this,
        //      a descendant peer that the node directly depends on
        //      would over-union onto the node's suffix (e.g. emit
        //      `mid@1.0.0(desc-peer@…)` when `desc-peer` is mid's own
        //      direct dep — pnpm emits `mid@1.0.0`). Child keys are
        //      `name@version(...)`; strip to the bare name.
        let canonical_name = canonical_tail(key)
            .rsplit_once('@')
            .map(|(name, _ver)| name.to_string())
            .unwrap_or_default();
        let mut suppressed: BTreeSet<String> = peer_names_in_segments_recursive(&self_segments);
        if !canonical_name.is_empty() {
            suppressed.insert(canonical_name);
        }
        if let Some(children) = forward.get(key) {
            for child in children {
                if let Some((child_name, _ver)) = canonical_tail(child).rsplit_once('@')
                    && !child_name.is_empty()
                {
                    suppressed.insert(child_name.to_string());
                }
            }
        }

        // Child contributions.
        if let Some(children) = forward.get(key) {
            for child in children {
                let child_peers = collect(child, forward, cumulative, visiting);
                for (name, seg) in child_peers {
                    if suppressed.contains(&name) {
                        continue;
                    }
                    acc.entry(name).or_insert(seg);
                }
            }
        }
        visiting.remove(key);
        cumulative.insert(key.to_string(), acc.clone());
        acc
    }

    // Compute cumulative for every package + every importer DirectDep
    // root. Done in stable order so the lex-smaller old-key tiebreaker
    // below is deterministic.
    let pkg_keys: Vec<String> = graph.packages.keys().cloned().collect();
    for key in &pkg_keys {
        collect(key, &forward, &mut cumulative, &mut visiting);
    }
    for deps in graph.importers.values() {
        for dep in deps {
            collect(&dep.dep_path, &forward, &mut cumulative, &mut visiting);
        }
    }

    // Build rewrite map. A package's new key is its canonical_base
    // (`name@version`) plus the cumulative segments concatenated in
    // peer-name lex order — same order `apply_peer_contexts_once`
    // already produces for self segments, so when a package's
    // cumulative is identical to its self set the rewrite is a no-op
    // and we skip it.
    //
    // Hashed-suffix keys (`name@version(<short-hash>)`, produced when a
    // package's own peer suffix exceeded `peersSuffixMaxLength`) are
    // left untouched. The hash form discards the textual peer set
    // by design — `outer_paren_segments` can't recover its
    // contribution, so any rewrite we built for it would either drop
    // the hash entirely (losing identity) or merge an incomplete
    // descendant set with the hashed self. Preserving the original
    // form is the conservative choice; pnpm's parity gap in that
    // regime is bounded by the hash collision space anyway.
    //
    // If the propagated suffix itself exceeds the cap, hash it the
    // same way `visit_peer_context` does for self suffixes — keeps
    // dep_path keys bounded across the whole graph.
    let mut rewrite: BTreeMap<String, String> = BTreeMap::new();
    for key in &pkg_keys {
        let Some(segments) = cumulative.get(key) else {
            continue;
        };
        // Git / remote-tarball (globally-shareable) packages keep a bare
        // dep_path keyed solely by their content-pinned URL — pnpm never
        // appends a `(peer@ver)` suffix to a non-registry depPath. Every
        // git/tarball key in a real pnpm-lock.yaml is bare even when its
        // subtree resolves peers: e.g. `<pkg>@<url>` sits bare above a
        // registry descendant like `<child>@6.5.1(@types/node@…)`.
        // Absorbing a descendant's `(@types/node@…)` here would (a) diverge
        // from the lockfile and (b) give the same content-identical tarball
        // a different dep_path per consuming subtree, splitting the single
        // shared global-virtual-store entry into duplicates (so one
        // content-pinned singleton would load twice → "Cannot find
        // module"). The descendant peers still propagate onto this node's
        // *registry* ancestors through `cumulative`, so a registry parent
        // keeps its own `(@types/node@…)` suffix; only the git/tarball node
        // itself stays bare.
        if graph
            .packages
            .get(key)
            .and_then(|p| p.local_source.as_ref())
            .is_some_and(|s| s.is_globally_shareable())
        {
            continue;
        }
        let canonical = canonical_tail(key);
        if is_hashed_peer_suffix(&key[canonical.len()..]) {
            // Original key already carries the hashed suffix `(…)` — see
            // comment above. Its textual peer set is irrecoverable, so
            // leave the key untouched.
            continue;
        }
        let suffix: String = segments.values().cloned().collect();
        let effective_suffix = effective_peer_suffix(&suffix, options.peers_suffix_max_length);
        let new_key = format!("{canonical}{effective_suffix}");
        if new_key != *key {
            rewrite.insert(key.clone(), new_key);
        }
    }

    if rewrite.is_empty() {
        return graph;
    }

    // Helper: rewrite a `dependencies` tail (the part after `name@`).
    // Reconstruct the target's old full key, look up its rewrite, and
    // strip the `name@` prefix off the result to recover the new tail.
    // Targets without a rewrite keep the original tail.
    let rewrite_tail = |child_name: &str, tail: &str| -> String {
        let old_key = format!("{child_name}@{tail}");
        match rewrite.get(&old_key) {
            Some(new_key) => new_key
                .strip_prefix(&format!("{child_name}@"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| tail.to_string()),
            None => tail.to_string(),
        }
    };

    let LockfileGraph {
        importers,
        packages,
        settings,
        overrides,
        package_extensions_checksum,
        pnpmfile_checksum,
        ignored_optional_dependencies,
        times,
        skipped_optional_dependencies,
        catalogs,
        bun_config_version,
        patched_dependencies,
        patched_dependency_hashes,
        trusted_dependencies,
        runtimes,
        extra_fields,
        workspace_extra_fields,
    } = graph;

    let mut new_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    for (old_key, mut pkg) in packages {
        let new_key = rewrite.get(&old_key).cloned().unwrap_or(old_key);
        for (name, tail) in pkg.dependencies.iter_mut() {
            *tail = rewrite_tail(name, tail);
        }
        for (name, tail) in pkg.optional_dependencies.iter_mut() {
            *tail = rewrite_tail(name, tail);
        }
        pkg.dep_path = new_key.clone();
        // Two old keys mapping to one new key: the lex-smaller old key
        // wins. Because `packages` is a `BTreeMap` we iterate
        // `(old_key, pkg)` pairs in lex order — the first insertion
        // for any given `new_key` is therefore the one whose old_key
        // sorts lowest, and `or_insert` makes every subsequent
        // collision a no-op. Bodies are equal in the common case
        // anyway (same canonical_base + same cumulative ⇒ same dep
        // tree), so this is effectively cosmetic determinism.
        new_packages.entry(new_key).or_insert(pkg);
    }

    let new_importers: BTreeMap<String, Vec<DirectDep>> = importers
        .into_iter()
        .map(|(path, deps)| {
            let rewritten = deps
                .into_iter()
                .map(|d| {
                    let new_dep_path = rewrite.get(&d.dep_path).cloned().unwrap_or(d.dep_path);
                    DirectDep {
                        name: d.name,
                        dep_path: new_dep_path,
                        dep_type: d.dep_type,
                        specifier: d.specifier,
                    }
                })
                .collect();
            (path, rewritten)
        })
        .collect();

    LockfileGraph {
        importers: new_importers,
        packages: new_packages,
        settings,
        overrides,
        package_extensions_checksum,
        pnpmfile_checksum,
        ignored_optional_dependencies,
        times,
        skipped_optional_dependencies,
        catalogs,
        bun_config_version,
        patched_dependencies,
        patched_dependency_hashes,
        trusted_dependencies,
        runtimes,
        extra_fields,
        workspace_extra_fields,
    }
}

/// Dedupe-peers post-pass: strip the `name@` prefix from every
/// parenthesized peer segment in every dep_path key and reference,
/// turning `react-dom@18.2.0(react@18.2.0)` into
/// `react-dom@18.2.0(18.2.0)`. Nested segments get the same treatment
/// so `a@1(b@2(c@3))` becomes `a@1(2(3))`.
///
/// Running this as a final post-pass (instead of inline during suffix
/// assembly in `visit_peer_context`) keeps cycle detection correct:
/// the detection path works against the full `name@version` form
/// throughout the fixed-point loop, and only the serialized output
/// gets the shorter form. A version-only inline approach would
/// false-positive on unrelated packages that coincidentally share a
/// version with the current package's canonical base.
///
/// Pure: no-op when `dedupe_peers` is off (caller gates the call);
/// otherwise rewrites every package key, every `LockedPackage.dep_path`
/// and `LockedPackage.dependencies` value, and every `importers[*]`
/// DirectDep `dep_path` through the same `apply_dedupe_peers_to_tail`
/// helper. Package bodies (integrity, metadata, etc.) are cloned
/// verbatim.
pub(crate) fn dedupe_peer_suffixes(graph: LockfileGraph) -> LockfileGraph {
    // Pass 1: compute the intended deduped key for each package and
    // tally how many distinct full-form keys map to it. Stripping
    // `name@` from suffix segments is lossy — two variants whose peer
    // *names* differ but whose peer *versions* coincide would collapse
    // onto the same deduped key (e.g. `consumer@1.0.0(foo@1.0.0)` and
    // `consumer@1.0.0(bar@1.0.0)` both → `consumer@1.0.0(1.0.0)`).
    // `dedupe_peer_variants` already merged the peer-equivalent
    // duplicates, so any remaining collision here represents genuinely
    // distinct variants — losing one would silently drop its
    // dependency wiring. We detect those collisions and keep both
    // sides in full form.
    let mut target_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut intended: BTreeMap<String, String> = BTreeMap::new();
    for key in graph.packages.keys() {
        let new_key = apply_dedupe_peers_to_key(key);
        *target_counts.entry(new_key.clone()).or_insert(0) += 1;
        intended.insert(key.clone(), new_key);
    }
    let rewrite: BTreeMap<String, String> = intended
        .into_iter()
        .map(|(old, new)| {
            if target_counts.get(&new).copied().unwrap_or(0) > 1 {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_PEER_DEDUPE_COLLISION,
                    "dedupe-peers: collision on {new} — keeping {old} in full form to avoid \
                     dropping a distinct peer-variant"
                );
                (old.clone(), old)
            } else {
                (old, new)
            }
        })
        .collect();

    // Rewrite a `(child_name, tail)` reference by reconstructing the
    // target's full-form key, looking up its effective rewrite, and
    // stripping `child_name@` off the result to recover the tail.
    // Tails always follow their target package's rewrite decision,
    // so references stay consistent when a collision forces a target
    // back to full form.
    let rewrite_tail = |child_name: &str, tail: &str| -> String {
        let old_key = format!("{child_name}@{tail}");
        match rewrite.get(&old_key) {
            Some(new_key) => new_key
                .strip_prefix(&format!("{child_name}@"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| tail.to_string()),
            None => apply_dedupe_peers_to_tail(tail),
        }
    };

    let mut new_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    for (old_key, pkg) in graph.packages {
        let new_key = rewrite
            .get(&old_key)
            .cloned()
            .unwrap_or_else(|| old_key.clone());
        let new_dependencies: BTreeMap<String, String> = pkg
            .dependencies
            .into_iter()
            .map(|(n, v)| {
                let new_v = rewrite_tail(&n, &v);
                (n, new_v)
            })
            .collect();
        let new_optional_dependencies: BTreeMap<String, String> = pkg
            .optional_dependencies
            .into_iter()
            .map(|(n, v)| {
                let new_v = rewrite_tail(&n, &v);
                (n, new_v)
            })
            .collect();
        new_packages.insert(
            new_key.clone(),
            LockedPackage {
                name: pkg.name,
                version: pkg.version,
                integrity: pkg.integrity,
                dependencies: new_dependencies,
                optional_dependencies: new_optional_dependencies,
                peer_dependencies: pkg.peer_dependencies,
                peer_dependencies_meta: pkg.peer_dependencies_meta,
                dep_path: new_key,
                local_source: pkg.local_source,
                os: pkg.os,
                cpu: pkg.cpu,
                libc: pkg.libc,
                bundled_dependencies: pkg.bundled_dependencies,
                optional: pkg.optional,
                transitive_peer_dependencies: pkg.transitive_peer_dependencies,
                tarball_url: pkg.tarball_url,
                registry_git_hosted: pkg.registry_git_hosted,
                alias_of: pkg.alias_of,
                yarn_checksum: pkg.yarn_checksum,
                engines: pkg.engines,
                bin: pkg.bin,
                declared_dependencies: pkg.declared_dependencies,
                license: pkg.license,
                funding_url: pkg.funding_url,
                extra_meta: pkg.extra_meta,
                has_install_script: pkg.has_install_script,
                has_shrinkwrap: pkg.has_shrinkwrap,
                in_bundle: pkg.in_bundle,
                deprecated: pkg.deprecated,
            },
        );
    }

    let new_importers: BTreeMap<String, Vec<DirectDep>> = graph
        .importers
        .into_iter()
        .map(|(path, deps)| {
            let rewritten = deps
                .into_iter()
                .map(|d| {
                    let new_dep_path = rewrite
                        .get(&d.dep_path)
                        .cloned()
                        .unwrap_or_else(|| apply_dedupe_peers_to_key(&d.dep_path));
                    DirectDep {
                        name: d.name,
                        dep_path: new_dep_path,
                        dep_type: d.dep_type,
                        specifier: d.specifier,
                    }
                })
                .collect();
            (path, rewritten)
        })
        .collect();

    LockfileGraph {
        importers: new_importers,
        packages: new_packages,
        settings: graph.settings,
        overrides: graph.overrides,
        package_extensions_checksum: graph.package_extensions_checksum,
        pnpmfile_checksum: graph.pnpmfile_checksum,
        ignored_optional_dependencies: graph.ignored_optional_dependencies,
        runtimes: graph.runtimes,
        times: graph.times,
        skipped_optional_dependencies: graph.skipped_optional_dependencies,
        catalogs: graph.catalogs,
        bun_config_version: graph.bun_config_version,
        patched_dependencies: graph.patched_dependencies,
        patched_dependency_hashes: graph.patched_dependency_hashes,
        trusted_dependencies: graph.trusted_dependencies,
        extra_fields: graph.extra_fields,
        workspace_extra_fields: graph.workspace_extra_fields,
    }
}

/// Strip `name@` from inside every parenthesized segment of a full
/// dep_path key (e.g. `react-dom@18.2.0(react@18.2.0)` →
/// `react-dom@18.2.0(18.2.0)`). The first `name@version` outside any
/// parens is preserved verbatim — that's the canonical head of the
/// dep_path and `dedupe-peers` only affects the peer suffix.
pub(crate) fn apply_dedupe_peers_to_key(key: &str) -> String {
    let mut parts = key.split('(');
    let Some(first) = parts.next() else {
        return key.to_string();
    };
    let mut out = String::with_capacity(key.len());
    out.push_str(first);
    for part in parts {
        out.push('(');
        // In a well-formed key, `part` looks like `name@version)` /
        // `name@version` / `version)` / ... We strip everything up to
        // and including the LAST `@` (scoped packages like
        // `@types/react@18.2.0` contain two `@`s; the separator is the
        // rightmost one). We only strip if that `@` comes before the
        // first `)` or `(` (i.e. the segment actually starts with
        // `name@`, not the outer parens closing with no name inside).
        if let Some(at_idx) = part.rfind('@') {
            let close_idx = part.find([')', '(']).unwrap_or(usize::MAX);
            if at_idx < close_idx {
                out.push_str(&part[at_idx + 1..]);
                continue;
            }
        }
        out.push_str(part);
    }
    out
}

/// Same as [`apply_dedupe_peers_to_key`] but for dep-tail values
/// stored in `LockedPackage.dependencies` (e.g. `18.2.0(react@18.2.0)`
/// → `18.2.0(18.2.0)`). Tails differ from keys only by lacking the
/// leading `name@` prefix — both use the same parens-based suffix
/// shape, so the algorithm is identical.
fn apply_dedupe_peers_to_tail(tail: &str) -> String {
    apply_dedupe_peers_to_key(tail)
}

#[allow(clippy::too_many_arguments)]
fn visit_peer_context<'g>(
    input_dep_path: &str,
    graph: &'g LockfileGraph,
    name_index: &FxHashMap<&'g str, Vec<&'g LockedPackage>>,
    ancestor_scope: &FxHashMap<String, String>,
    root_scope: &FxHashMap<String, String>,
    out_packages: &mut BTreeMap<String, LockedPackage>,
    visiting: &mut FxHashSet<String>,
    options: &PeerContextOptions,
) -> Option<String> {
    let pkg = graph.packages.get(input_dep_path)?;

    // The input key may already carry a peer suffix (fixed-point loop
    // Pass 2+). Drop it before we build a new one — otherwise we'd
    // append the new suffix on top of the old and grow unboundedly
    // across iterations (classic mutual-peer-cycle blow-up).
    //
    // Both suffix forms are parenthesized — the normal nested
    // `(name@version)(…)` and the capped `(<short-hash>)` that
    // `effective_peer_suffix` emits past `peersSuffixMaxLength` — so
    // splitting on the first `(` strips either one. Otherwise each
    // pass would re-hash the already-hashed key and grow it (covered
    // by the `peer_suffix_is_hashed_when_exceeding_cap` unit test).
    let canonical_base = canonical_tail(input_dep_path).to_string();

    // Compute peer context: walk declared peers, resolve from ancestors
    // (nearest wins — the scope is rebuilt as we recurse) or from the
    // package's own dependency map as the auto-install fallback. Both
    // sides may produce nested tails on the second and later iterations
    // of the fixed-point loop.
    // Resolution source priority for each declared peer:
    //   1. Ancestor scope — if the ancestor's version actually
    //      satisfies the declared peer range. Different subtrees
    //      naturally see different ancestors (lib-a in subtree-A
    //      and lib-b in subtree-B keep their own peer pins), so
    //      preferring the closest ancestor here doesn't conflate
    //      cross-subtree variants.
    //   2. The current package's own `pkg.dependencies` entry — the
    //      BFS peer-walk enqueued this peer with the declared range,
    //      so whatever got picked there is guaranteed to satisfy.
    //      Captures the case where a single subtree holds two
    //      consumers with conflicting peer ranges (lib-a@^17 next to
    //      a parent that pins react@18): the BFS auto-installs the
    //      satisfying version into lib-a's own deps, which beats the
    //      ancestor's incompatible version.
    //   3. Ancestor scope — even when the version doesn't satisfy
    //      the declared range. This mirrors what Node's module
    //      resolution would surface (`require('peer')` from the
    //      package would walk up node_modules and find the parent's
    //      version). pnpm and bun do the same and emit an unmet-peer
    //      warning rather than picking a more-distant matching
    //      version. `detect_unmet_peers` flags the mismatch after
    //      the pass.
    //   4. The current package's own `pkg.dependencies` entry,
    //      ignoring range satisfaction — symmetric to (3) for the
    //      BFS-installed case.
    //   5. Workspace root scope (compatible) — `resolve-peers-from-
    //      workspace-root` fallback for monorepos that pin shared
    //      peers at the root.
    //   6. A graph-wide scan: any package whose name matches and
    //      whose version satisfies the declared range. Last resort
    //      for nested-context callers when nothing closer has it.
    //   7. Workspace root scope, ignoring range satisfaction.
    //
    // If nothing in the graph holds a version of this peer at all,
    // it's left out of the context entirely — `detect_unmet_peers`
    // will surface it as a warning after the pass.
    //
    // Only peers the package actually *declares* in `peerDependencies`
    // build a dep_path suffix here. A name present solely in
    // `peerDependenciesMeta` (a meta-only optional peer — the way
    // `follow-redirects` declares `debug`, for instance) is deliberately
    // NOT folded in: pnpm treats such a peer as resolvable but then
    // collapses the binding back out via `dedupe-peer-dependents`
    // whenever a peer-free path exists, so the realistic lockfile leaves
    // the whole chain bare even when a distant ancestor carries that peer
    // as a plain dependency. Eagerly binding the meta-only peer from that
    // ancestor scope produced `(peer@ver)`-suffixed variants that aube's
    // dedupe pass (which only collapses *declared*-peer variants) never
    // merged, so the same subtree hashed differently per install scope
    // (whole-workspace vs single-member), splitting a shared
    // global-virtual-store singleton in two and surfacing at runtime as a
    // duplicate-instance "Cannot find module". Matching pnpm's *deduped*
    // output — bare — keeps the singleton intact.
    let mut peer_context: Vec<(String, String)> = Vec::new();
    for (peer_name, declared_range) in &pkg.peer_dependencies {
        let satisfies_declared = |v: &str| -> bool {
            // The tail may carry a nested peer suffix on fixed-point
            // iterations 2+; strip it before checking the semver.
            let canonical = canonical_tail(v);
            version_satisfies(canonical, declared_range)
        };

        let from_ancestor = ancestor_scope
            .get(peer_name)
            .filter(|v| satisfies_declared(v))
            .cloned();
        let from_ancestor_incompatible = ancestor_scope.get(peer_name).cloned();

        let from_pkg_deps = pkg
            .dependencies
            .get(peer_name)
            .filter(|v| satisfies_declared(v))
            .cloned();
        let from_pkg_deps_incompatible = pkg.dependencies.get(peer_name).cloned();

        // `resolve-peers-from-workspace-root`: fall back to the root
        // importer's direct deps before the graph-wide scan. Common in
        // monorepos where the workspace root pins shared peers (e.g.
        // `react`) that leaf packages peer on without declaring them
        // in their own subtree. Skipped when the setting is off —
        // matches pnpm's `resolve-peers-from-workspace-root=false`.
        let from_root = if options.resolve_from_workspace_root {
            root_scope
                .get(peer_name)
                .filter(|v| satisfies_declared(v))
                .cloned()
        } else {
            None
        };
        let from_root_incompatible = if options.resolve_from_workspace_root {
            root_scope.get(peer_name).cloned()
        } else {
            None
        };

        // Return the full dep_path TAIL (the part after `name@`), not
        // just `p.version`. On fixed-point iteration 2+, the input
        // graph's keys are contextualized — e.g. `react-dom` lives at
        // `react-dom@18.2.0(react@18.2.0)`. Downstream code
        // reconstructs the child lookup key with
        // `format!("{child_name}@{tail}")` and needs the tail to
        // match whatever the graph has keyed it under, otherwise the
        // lookup returns None and the peer gets silently dropped
        // from `new_dependencies`. The semver check is against the
        // package's canonical `version` field, not the tail, because
        // the tail may carry a peer suffix that isn't valid semver.
        let from_graph_scan = || {
            name_index
                .get(peer_name.as_str())
                .into_iter()
                .flat_map(|bucket| bucket.iter().copied())
                .filter(|p| version_satisfies(&p.version, declared_range))
                .filter_map(|p| {
                    let tail = p
                        .dep_path
                        .strip_prefix(&format!("{}@", p.name))
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| p.version.clone());
                    node_semver::Version::parse(&p.version)
                        .ok()
                        .map(|ver| (ver, tail))
                })
                .max_by(|a, b| a.0.cmp(&b.0))
                .map(|(_, tail)| tail)
        };

        // pnpm resolves an *optional* peer (one flagged
        // `peerDependenciesMeta.optional`) only from the resolution path it
        // is actually on — the nearest ancestor, the package's own
        // auto-installed deps, or the workspace root — and otherwise leaves
        // it unresolved so it surfaces under `transitivePeerDependencies`.
        // It never reaches for a range-incompatible version or scans the
        // whole graph for an unrelated copy. Mirroring that is what lets
        // `typescript` (an optional peer the root provides) take a dep-path
        // suffix while debug's optional `supports-color` (which nothing on
        // the path provides) bubbles up instead of binding to a cousin.
        let is_optional = pkg
            .peer_dependencies_meta
            .get(peer_name)
            .is_some_and(|m| m.optional);
        let resolved = if is_optional {
            from_ancestor.or(from_pkg_deps).or(from_root)
        } else {
            from_ancestor
                .or(from_pkg_deps)
                .or(from_ancestor_incompatible)
                .or(from_pkg_deps_incompatible)
                .or(from_root)
                .or_else(from_graph_scan)
                .or(from_root_incompatible)
        };
        if let Some(version) = resolved {
            peer_context.push((peer_name.clone(), version));
        }
    }
    peer_context.sort_by(|a, b| a.0.cmp(&b.0));

    // For the SUFFIX we build a cycle-broken copy: any peer value that
    // nests a reference back to the current package's canonical base
    // gets stripped to its plain version. Without this, mutual peer
    // cycles (a peers on b, b peers on a) grow the suffix one level
    // per iteration of the fixed-point loop and never converge.
    //
    // The non-cycle paths are untouched, so a regular nested chain
    // like `(react-dom@18.2.0(react@18.2.0))` still serializes fully.
    // We deliberately keep the full nested tails in `peer_context` for
    // downstream scope propagation and child lookups — suffix cycle-
    // breaking is cosmetic and should not change what packages exist
    // or which snapshot entries reference each other.
    //
    // Cycle detection is always done against the full `name@version`
    // canonical base — even when `dedupe-peers=true` is on, because
    // the version-only form is ambiguous (two unrelated packages at
    // the same version would false-positive). `dedupe-peers` is
    // applied as a post-pass over the final graph in
    // `dedupe_peer_suffixes` after cycle detection is done.
    let suffix: String = peer_context
        .iter()
        .map(|(n, v)| {
            let cycles_back = contains_canonical_back_ref(v, &canonical_base);
            let display_v = if cycles_back {
                canonical_tail(v).to_string()
            } else {
                v.clone()
            };
            format!("({n}@{display_v})")
        })
        .collect();
    // pnpm's `peersSuffixMaxLength`: when the suffix body exceeds the
    // cap, `effective_peer_suffix` replaces the whole suffix with a
    // parenthesized short hash `(<hash>)` so the lockfile key stays
    // bounded and byte-compatible with pnpm's `createPeerDepGraphHash`.
    let effective_suffix = effective_peer_suffix(&suffix, options.peers_suffix_max_length);
    let contextualized = format!("{canonical_base}{effective_suffix}");

    if out_packages.contains_key(&contextualized) || visiting.contains(&contextualized) {
        return Some(contextualized);
    }
    visiting.insert(contextualized.clone());

    // Build the scope for P's children. This is ancestor_scope, overlaid
    // with P's own dependencies and its resolved peer map. Children see
    // their grandparents too — this mirrors pnpm's all-the-way-up peer
    // walk.
    //
    // We deliberately do NOT strip any existing peer-context suffix
    // off the tails we put into the scope. On the first pass the
    // values are plain (BFS output has no suffixes), so preserving
    // them is a no-op; on subsequent passes (see the fixed-point loop
    // in `apply_peer_contexts`) the input graph already carries
    // contextualized tails, and keeping them in scope is exactly how
    // nested peer suffixes propagate down to consumers — a package
    // that peers on `react-dom` and reaches it through a parent whose
    // `react-dom` entry is already `18.2.0(react@18.2.0)` will see
    // that nested tail in its own scope, and its own suffix will
    // serialize as `(react-dom@18.2.0(react@18.2.0))`. That's the
    // nested form pnpm writes.
    let mut child_scope = ancestor_scope.clone();
    for (name, version) in &pkg.dependencies {
        child_scope.insert(name.clone(), version.clone());
    }
    for (name, version) in &peer_context {
        child_scope.insert(name.clone(), version.clone());
    }

    // Recurse into each child, rewriting its dependency map entry to
    // point at the contextualized dep_path's tail. A child whose visit
    // fails (orphaned / missing) keeps its own tail.
    //
    // For declared peer names, the peer context (filled from the
    // ancestor scope) is authoritative — we override whatever the BFS
    // peer walk auto-installed. Otherwise the snapshot suffix and the
    // actual wired `dependencies[peer]` could disagree, which made the
    // sibling symlink target inconsistent with the peer-context claim.
    // When the ancestor's version doesn't satisfy the declared range,
    // `detect_unmet_peers` will flag it as a warning after the pass.
    let peer_context_versions: FxHashMap<String, String> = peer_context.iter().cloned().collect();

    let mut new_dependencies: BTreeMap<String, String> = BTreeMap::new();
    let mut visited_dep_names: FxHashSet<String> = FxHashSet::default();

    for (child_name, child_version_tail) in &pkg.dependencies {
        // If this child is a declared peer, its tail comes from the
        // peer context (which may be nested). Otherwise we use the
        // tail we already have — also possibly nested on a 2nd pass.
        let lookup_tail = match peer_context_versions.get(child_name) {
            Some(v) => v.clone(),
            None => child_version_tail.clone(),
        };
        let child_canonical_dep_path = format!("{child_name}@{lookup_tail}");
        let child_new = visit_peer_context(
            &child_canonical_dep_path,
            graph,
            name_index,
            &child_scope,
            root_scope,
            out_packages,
            visiting,
            options,
        );
        let new_tail = match child_new {
            Some(new_dep_path) => new_dep_path
                .strip_prefix(&format!("{child_name}@"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| lookup_tail.clone()),
            None => lookup_tail.clone(),
        };
        new_dependencies.insert(child_name.clone(), new_tail);
        visited_dep_names.insert(child_name.clone());
    }

    // Peers that were satisfied purely from the ancestor scope may not
    // have been in `pkg.dependencies` at all (no auto-install needed).
    // Wire them as deps now so the linker creates the sibling symlink
    // and the lockfile snapshot records them.
    for (peer_name, peer_version) in &peer_context {
        if visited_dep_names.contains(peer_name) {
            continue;
        }
        let child_canonical_dep_path = format!("{peer_name}@{peer_version}");
        let child_new = visit_peer_context(
            &child_canonical_dep_path,
            graph,
            name_index,
            &child_scope,
            root_scope,
            out_packages,
            visiting,
            options,
        );
        if let Some(new_dep_path) = child_new {
            let new_tail = new_dep_path
                .strip_prefix(&format!("{peer_name}@"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| peer_version.clone());
            new_dependencies.insert(peer_name.clone(), new_tail);
        }
    }

    visiting.remove(&contextualized);
    let new_optional_dependencies: BTreeMap<String, String> = pkg
        .optional_dependencies
        .keys()
        .filter_map(|name| {
            new_dependencies
                .get(name)
                .map(|tail| (name.clone(), tail.clone()))
        })
        .collect();

    out_packages.insert(
        contextualized.clone(),
        LockedPackage {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            integrity: pkg.integrity.clone(),
            dependencies: new_dependencies,
            optional_dependencies: new_optional_dependencies,
            peer_dependencies: pkg.peer_dependencies.clone(),
            peer_dependencies_meta: pkg.peer_dependencies_meta.clone(),
            dep_path: contextualized.clone(),
            local_source: pkg.local_source.clone(),
            os: pkg.os.clone(),
            cpu: pkg.cpu.clone(),
            libc: pkg.libc.clone(),
            bundled_dependencies: pkg.bundled_dependencies.clone(),
            optional: pkg.optional,
            transitive_peer_dependencies: pkg.transitive_peer_dependencies.clone(),
            tarball_url: pkg.tarball_url.clone(),
            registry_git_hosted: pkg.registry_git_hosted,
            alias_of: pkg.alias_of.clone(),
            yarn_checksum: pkg.yarn_checksum.clone(),
            engines: pkg.engines.clone(),
            bin: pkg.bin.clone(),
            declared_dependencies: pkg.declared_dependencies.clone(),
            license: pkg.license.clone(),
            funding_url: pkg.funding_url.clone(),
            extra_meta: pkg.extra_meta.clone(),
            has_install_script: pkg.has_install_script,
            has_shrinkwrap: pkg.has_shrinkwrap,
            in_bundle: pkg.in_bundle,
            deprecated: pkg.deprecated.clone(),
        },
    );
    Some(contextualized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{DepType, DirectDep, PeerDepMeta};

    fn locked(name: &str, deps: &[(&str, &str)]) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            dep_path: format!("{name}@1.0.0"),
            dependencies: deps
                .iter()
                .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
                .collect(),
            ..Default::default()
        }
    }

    /// `root -> app -> {plugin, sibling}` and `sibling -> theme`. `theme`
    /// is only ever a *cousin* of `plugin` (never an ancestor, the root,
    /// or one of plugin's own deps), so the single way to reach it from
    /// plugin's peer is the graph-wide scan.
    fn graph_with_cousin_peer() -> LockfileGraph {
        let mut g = LockfileGraph::default();
        g.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "app".to_string(),
                dep_path: "app@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        );
        for p in [
            locked("app", &[("plugin", "1.0.0"), ("sibling", "1.0.0")]),
            locked("plugin", &[]),
            locked("sibling", &[("theme", "1.0.0")]),
            locked("theme", &[]),
        ] {
            g.packages.insert(p.dep_path.clone(), p);
        }
        g
    }

    #[test]
    fn optional_peer_is_not_bound_via_graph_scan() {
        let mut g = graph_with_cousin_peer();
        let plugin = g.packages.get_mut("plugin@1.0.0").expect("plugin present");
        plugin
            .peer_dependencies
            .insert("theme".to_string(), "*".to_string());
        plugin
            .peer_dependencies_meta
            .insert("theme".to_string(), PeerDepMeta { optional: true });

        let out = apply_peer_contexts(g, &PeerContextOptions::default()).expect("peer pass");

        assert!(
            out.packages.contains_key("plugin@1.0.0"),
            "plugin keeps bare key"
        );
        assert!(
            !out.packages.contains_key("plugin@1.0.0(theme@1.0.0)"),
            "an optional peer reachable only via the graph scan must stay \
             unresolved so it surfaces under transitivePeerDependencies"
        );
    }

    #[test]
    fn required_peer_still_binds_via_graph_scan() {
        // Same shape, but `theme` is a *required* peer (no meta entry):
        // the graph-wide scan still binds it, proving the narrowing above
        // is specific to optional peers and not a regression.
        let mut g = graph_with_cousin_peer();
        let plugin = g.packages.get_mut("plugin@1.0.0").expect("plugin present");
        plugin
            .peer_dependencies
            .insert("theme".to_string(), "*".to_string());

        let out = apply_peer_contexts(g, &PeerContextOptions::default()).expect("peer pass");

        assert!(
            out.packages.contains_key("plugin@1.0.0(theme@1.0.0)"),
            "a required peer should still resolve through the graph-wide scan"
        );
    }
}
