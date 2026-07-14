//! Selective-subtree disk-materialization policy — nub's disk-materialize
//! expansion hook. Unconditionally on for users; off only under the internal A/B
//! seam ([`crate::dynamic_phantom::enabled`]).
//!
//! Disk-materializing a package project-local is only SOUND for a
//! transitively-consumed package if its whole ancestor-closure materializes with
//! it — otherwise a store-resident importer keeps resolving the un-materialized
//! shared-store copy, a silent singleton split (two realpaths, two module
//! instances). This hook expands aube's flat disk-materialize seed into a
//! graph-aware plan against the resolved lockfile — the ancestor-closure
//! (rung 1). Each seed grows to [`LockfileGraph::importer_closure`] — the seed
//! UNION every package that transitively imports it. Bounded to the affected
//! subtree by construction (unrelated top-level subtrees are not importers),
//! measured 0.3–2.1% of real large trees. Also SUBSUMES the #315
//! library-embedded-vite<8.1 residual: an embedded vite<8.1 (a framework's
//! transitive engine, no direct-dep symlink) is auto-detected and its
//! `[framework…vite]` closure ejected, so #318's dist sniff patch reaches a
//! now-project-local vite.
//!
//! Undeclared phantoms an ejected member imports are resolved by the linker's
//! COLLECTIVE project-local hidden hoist tree over the whole ejected set (see
//! `aube_linker::link_hidden_hoist`): each ejected member's realpath is
//! project-local, so Node's upward `node_modules` walk from inside it passes
//! through `.nub/node_modules/`, a blanket first-write-wins alias for every graph
//! package — detection-free and pnpm-parity. So this hook only needs to grow the
//! eject set; it records no per-importer target hoist. (This replaced the former
//! per-importer hoist-within mechanism.)
//!
//! The phantom importers are the DYNAMIC output of the
//! extract-time per-version scanner (`crate::dynamic_phantom`, the PRODUCER): it
//! scans each fetched version's real published code for unguarded undeclared
//! imports and writes a per-content verdict sidecar. This hook (the CONSUMER)
//! reads those sidecars — so there is no hand-maintained list of phantom classes;
//! the detection is per-version and auto-current. A precision SEED-SELECTION
//! filter (see [`should_seed`]) drops a flagged importer whose undeclared targets
//! are all already resolvable as its own DIRECT (depth-1) siblings and absent from
//! the project top level, so a directly-satisfied over-flag never ejects.
//!
//! Internal A/B seam off ⇒ no hook installed ⇒ aube's `expand_disk_materialize`
//! returns the seed verbatim ⇒ the disk-materialize pass is byte-for-byte the
//! pre-productionization pure-symlink behavior. All policy lives here; aube owns
//! only the neutral seam + the graph primitive.

use std::collections::HashSet;

use aube_linker::DiskMaterializePlan;
use aube_lockfile::{LockedPackage, LockfileGraph};
use rayon::prelude::*;

/// The SINGLE phantom-eject arm — [`crate::dynamic_phantom::enabled`] — shared
/// with the extract-time producer so detection (the scanner), transitive
/// soundness (this closure), and warm-tree invalidation (the fingerprint) can
/// never disagree. Unconditionally on for users; off only under the internal A/B
/// seam. The arm IS folded into the install-state fingerprint (via the embedder
/// `extra_settings_fingerprint` hook; see [`crate::dynamic_phantom::settings_fingerprint`]),
/// so flipping the seam on an already-installed tree re-links to the pure-symlink
/// shape rather than accepting a stale node_modules.
fn enabled() -> bool {
    crate::dynamic_phantom::enabled()
}

/// Register nub's disk-materialize expansion hook with the embedded engine.
/// No-op only under the internal A/B seam ([`enabled`] false), in which case
/// `aube_linker::expand_disk_materialize` stays the identity — byte-for-byte the
/// pure-symlink disk-materialize behavior. Set-once (idempotent); safe to call
/// once per engine-session build.
pub(crate) fn register() {
    if !enabled() {
        return;
    }
    aube_linker::set_disk_materialize_expand_hook(Box::new(expand));
}

/// nub's own embedder-default names that may seed the eject set. nub exposes NO
/// user-facing `diskMaterializePackages` knob (maintainer 2026-07-07): the dynamic
/// phantom detector is the sole eject mechanism, so a hand-listed package is
/// redundant. The resolved seed still carries nub's INTERNAL vite #315 embedder
/// default ([`super::mod::nub_setting_defaults`]) — kept so the phantom-eject
/// opt-out (no-hook) path still ejects vite<8.1 — but every OTHER name arrived
/// from a user source (`.npmrc`/env/`pnpm-workspace.yaml`) and is dropped by
/// [`nub_internal_seed`]. Standalone aube installs no hook and honors the full
/// seed verbatim, so its `diskMaterializePackages` knob is unaffected.
///
/// SHARED with [`super::nub_setting_defaults`], which seeds exactly this name as
/// the embedder default — sourcing both from one const so a future internal
/// default can't be added in one place and silently dropped by the other.
pub(super) const NUB_INTERNAL_DISK_MATERIALIZE_SEED: &[&str] = &["vite"];

/// Curated "project-context" packages (nub#457): each one's build script READS or
/// MUTATES the CONSUMING project rather than only its own dir — git-hook installers
/// walk up from `cwd` to the project root and write `.git/hooks`; the rest generate
/// project-local files or resolve peers/bridges against the host tree. Under GVS a
/// build runs in the shared global store, DETACHED from any project, so a
/// cwd/upward-walk lands on the per-package store wrapper (which has no
/// package.json) and the script crashes — #457: `simple-git-hooks` postinstall →
/// ENOENT — or silently emits shared-mutable wrong output. Ejecting them
/// (disk-materialize project-local, via the SAME importer-closure + expand hook the
/// phantom seeds use) restores a real project above `cwd`.
///
/// DELIBERATELY curated, NOT "all script-havers": self-contained builds (node-gyp
/// native compiles, prebuilt-binary downloaders like esbuild) read only their own
/// dir, produce identical output anywhere, and share it cross-project via the
/// side-effects cache — ejecting them would erode the symlink-in-store sharing win
/// for zero correctness gain, so they STAY in GVS (built once, shared). Every name
/// here is verified category-C (build reads/mutates the host project); absent names
/// cost nothing (the seed union is presence-gated against the resolved graph).
///
/// Gate: this rides the same [`enabled`] seam as phantom eject, so disabling the
/// internal eject seam also disables curated eject (reintroducing #457) — an
/// accepted property of that internal-only escape hatch.
pub(super) const NUB_PROJECT_CONTEXT_EJECT: &[&str] = &[
    // Git-hook installers — walk up from cwd to the project root, write `.git/hooks`.
    "simple-git-hooks",
    "lefthook",
    "@evilmartians/lefthook",
    "@arkweid/lefthook",
    "pre-commit",
    "pre-push",
    "ghooks",
    "git-validate",
    "shared-git-hooks",
    "yorkie",
    "git-commit-msg-linter",
    // Other host-project reader/mutators (generate project-local files, resolve
    // peers/bridges against the consuming tree).
    "install-peers",
    "msw",
    "@bahmutov/add-typescript-to-cypress",
    "@cypress/snapshot",
    "cordova.plugins.diagnostic",
    "vue-demi",
    "vue-inbrowser-compiler-demi",
    "@intlify/vue-i18n-bridge",
    "@intlify/vue-router-bridge",
    "storage-engine",
];

/// Stable, order-independent fingerprint token for the curated eject list, folded
/// into the install-state settings hash via [`crate::dynamic_phantom::settings_token`]
/// (the `extra_settings_fingerprint` embedder hook). Load-bearing for warm/upgrade
/// trees (nub#457): the curated seed is injected INSIDE the expand hook, after
/// aube's `disk_materialize_packages` settings fold, so without this token an
/// existing install — one from a nub predating the list, or after any FUTURE list
/// edit — keeps an identical `settings_hash`, and aube's existence-gated fast path
/// accepts the stale symlinked tree and skips the relink, leaving #457 unfixed.
/// Folding this token forces the relink that converts the stale symlink to an
/// ejected dir. Hashing the SORTED names makes the token move on any add/remove and
/// hold steady on a pure reorder; FNV-1a keeps it dependency-free and stable across
/// platforms/releases (std's `DefaultHasher` is not guaranteed stable across Rust
/// versions).
pub(crate) fn project_context_eject_token() -> String {
    eject_list_token(NUB_PROJECT_CONTEXT_EJECT)
}

fn eject_list_token(names: &[&str]) -> String {
    let mut sorted: Vec<&str> = names.to_vec();
    sorted.sort_unstable();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for name in sorted {
        for byte in name.bytes().chain(std::iter::once(0x1f)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// Keep only nub's own embedder-default seed names, dropping every user-source
/// entry — the mechanism that retires the user-facing `diskMaterializePackages`
/// knob under nub while leaving standalone aube's byte-for-byte.
fn nub_internal_seed(resolved_seed: &[String]) -> Vec<String> {
    resolved_seed
        .iter()
        .filter(|n| NUB_INTERNAL_DISK_MATERIALIZE_SEED.contains(&n.as_str()))
        .cloned()
        .collect()
}

/// The hook entry: read the per-version scanner's sidecars (the store-IO half)
/// then hand off to the pure planner. Split so [`plan_from_flags`] — all the
/// closure/seed policy — is unit-tested with injected flags and never touches
/// the host store. The resolved seed is first filtered to nub's internal names
/// ([`nub_internal_seed`]) so a user's `diskMaterializePackages` value has no
/// effect under nub.
fn expand(graph: &LockfileGraph, seed_names: &[String]) -> DiskMaterializePlan {
    plan_from_flags(
        graph,
        &nub_internal_seed(seed_names),
        &dynamic_phantom_flags(graph),
    )
}

/// One dynamically-flagged importer the planner may seed. Two INDEPENDENT reasons
/// to eject, either sufficient: an undeclared-phantom carrier (`targets`, the
/// runtime + `.d.ts`-undeclared class) and/or a `.d.ts` peer-type carrier
/// (`type_peers`, nub#450). `dep_path`/`name` locate it in the graph.
pub(super) struct FlaggedImporter {
    pub dep_path: String,
    pub name: String,
    pub targets: Vec<String>,
    pub type_peers: Vec<String>,
}

/// Pure planner: resolved graph + flat seed + dynamic phantom flags → graph-aware
/// materialization plan. See the module docs for the two rungs. `flags` is each
/// surviving-candidate importer, supplied by [`dynamic_phantom_flags`] in
/// production and injected directly in tests.
fn plan_from_flags(
    graph: &LockfileGraph,
    seed_names: &[String],
    flags: &[FlaggedImporter],
) -> DiskMaterializePlan {
    // Drop the version-BLIND `vite` name-seed and decide vite version-aware here.
    // The embedder default (mod.rs) seeds the literal `vite` for ANY direct-dep
    // vite, but vite ≥ 8.1 reads `.modules.yaml` from the shared store natively
    // (#318 Unit A, written post-install regardless of eject) and needs NO eject —
    // a name-seed would drag vite + its whole ancestor-closure project-local for
    // zero benefit. vite < 8.1 is independently dep_path-auto-seeded below (the
    // `vite_lt_8_1` check), which fires for every < 8.1 copy the name-seed caught,
    // so pruning the name-seed loses nothing for < 8.1 and stops the ≥ 8.1
    // over-eject. This is the ONLY version-aware chokepoint: the mod.rs seed runs
    // pre-resolve and can't see the concrete version. (Under the internal A/B seam
    // no hook installs, so the raw name-seed still over-ejects vite ≥ 8.1 — an
    // accepted cost of that internal-only path.)
    // Provenance-blind: this also strips a user's explicit `vite` in
    // `diskMaterializePackages`, which is fine — vite ≥ 8.1 works symlinked and
    // vite < 8.1 is re-seeded below regardless of source, so a working vite is
    // served either way.
    let seed_names: Vec<&str> = seed_names
        .iter()
        .map(String::as_str)
        .filter(|&name| name != "vite")
        .collect();

    // Top-level presence: default-hoist top level = the importer DIRECT deps. See
    // `should_seed` for why this gate is load-bearing and its non-default-hoist
    // scope caveat.
    let root_provided: HashSet<&str> = graph
        .importers
        .values()
        .flat_map(|deps| deps.iter().map(|d| d.name.as_str()))
        .collect();
    let is_top_level = |name: &str| root_provided.contains(name);

    // Seed set by NAME: the caller's disk-materialize list ∪ every dynamically-
    // flagged importer that SURVIVES the precision seed-selection filter. Embedded
    // vite<8.1 is seeded by dep_path below.
    let mut seed_names_set: HashSet<&str> = seed_names.iter().copied().collect();

    // Dynamic phantom source (the per-version scanner's sidecars) — the replacement
    // for the retired hand-curated map. Each SURVIVING flagged importer seeds the
    // eject closure by NAME; the collective project-local hidden hoist tree the
    // linker builds over the ejected set (see `aube_linker::link_hidden_hoist`)
    // then resolves every undeclared phantom for those members via Node's walk-up,
    // so no per-importer target hoist is recorded.
    for flag in flags {
        // (a) Undeclared phantoms — the existing runtime + `.d.ts`-undeclared class.
        // Guard on non-empty targets: `should_seed` returns SEED for an empty target
        // set (its "can't prove safe" default), which is correct for a phantom flag
        // that lost its targets but WRONG for a pure type-peer flag (no undeclared
        // phantom at all) — that must seed only via path (b).
        let seed_for_targets = !flag.targets.is_empty()
            && should_seed(
                &flag.targets,
                &direct_dep_names(&flag.dep_path, graph),
                is_top_level,
            );
        // (b) `.d.ts` peer-type coupling (nub#450): a declared peer imported from
        // the type surface breaks the type-checker only when the peer's types come
        // from a SEPARATE top-level `@types/<peer>` the store realpath can't reach.
        // Ejecting the importer makes its realpath project-local so the collective
        // hidden tree provides `@types/<peer>`. Gate on the `@types/<peer>` actually
        // being a top-level package: this both makes the eject load-bearing and
        // BOUNDS it to the peer-typed set (a peer that ships its own types, e.g.
        // `vue`, has no top-level `@types/vue`, so it never seeds — GVS stays on).
        let seed_for_type_peers = flag
            .type_peers
            .iter()
            .any(|peer| is_top_level(&types_package_name(peer)));
        if seed_for_targets || seed_for_type_peers {
            seed_names_set.insert(flag.name.as_str());
        }
    }

    // Curated project-context eject (nub#457): union every `NUB_PROJECT_CONTEXT_EJECT`
    // name PRESENT in the resolved graph. Their builds read/mutate the consuming
    // project, so a GVS store-detached materialization crashes or emits
    // shared-mutable wrong output; seeding them here routes them through the SAME
    // importer-closure + disk-materialize path as the phantom seeds (a git-hook leaf
    // has ~0 importers, so its closure is just itself). The presence gate keeps an
    // absent name free.
    let graph_names: HashSet<&str> = graph.packages.values().map(|p| p.name.as_str()).collect();
    for &name in NUB_PROJECT_CONTEXT_EJECT {
        if graph_names.contains(name) {
            seed_names_set.insert(name);
        }
    }

    // Seed DEP_PATHS: every graph package whose name is a seed, plus every
    // embedded vite<8.1 copy (auto-detected — the #315 residual). Seeding by
    // dep_path keeps the reverse walk anchored to the real copies present.
    let mut seed_dep_paths: HashSet<&str> = HashSet::new();
    for (dep_path, pkg) in &graph.packages {
        if seed_names_set.contains(pkg.name.as_str())
            || (pkg.name == "vite" && super::vite_compat::vite_lt_8_1(&pkg.version))
        {
            seed_dep_paths.insert(dep_path.as_str());
        }
    }

    // Rung 1 — reverse-BFS ancestor-closure = the affected subtree.
    let closure = graph.importer_closure(seed_dep_paths.iter().copied());

    // Bounded-subtree guard: the closure must stay a small slice of the tree. A
    // closure approaching the whole tree means a foundational seed — a bug, since
    // phantom breakers are empirically never foundational. Surface it loudly;
    // never silently degrade to whole-tree materialization (that is `disableGVS`,
    // a separate last-resort lever). The `total >= 20` floor avoids a spurious
    // warning on a tiny fixture where a legitimate 2-3 package closure is
    // naturally a large fraction (e.g. a 3-package firebase repro).
    let total = graph.packages.len().max(1);
    if total >= 20 && closure.len() * 2 > total {
        tracing::warn!(
            "selective-subtree closure spans {}/{} packages ({:.0}%) — unexpectedly \
             large; a seed may be foundational (should not happen for a phantom breaker)",
            closure.len(),
            total,
            closure.len() as f64 / total as f64 * 100.0,
        );
    }

    // Rung-1 names: every closure member's name ∪ the original seed names (the
    // executor is name-keyed). Original seed names are kept even if absent from
    // the graph — the executor simply never matches an absent name.
    let mut names: HashSet<String> = seed_names.iter().map(|s| s.to_string()).collect();
    for dep_path in &closure {
        if let Some(pkg) = graph.packages.get(dep_path) {
            names.insert(pkg.name.clone());
        }
    }

    // Undeclared phantoms — every class the retired per-importer hoist used to
    // place (a scanner-flagged undeclared import; a statically-imported but
    // optional peer like `vue-router/vite` → `@vue/compiler-sfc`) — are now
    // resolved uniformly by the linker's collective project-local hidden hoist
    // tree over the ejected set: each ejected member's realpath is project-local,
    // so Node's upward walk from inside it passes through `.nub/node_modules/`,
    // which carries a blanket first-write-wins alias for every graph package.
    // Detection-free and pnpm-parity, so this planner only needs to grow the
    // eject set (rung 1) — it records no per-importer target hoist.
    DiskMaterializePlan {
        names: names.into_iter().collect(),
    }
}

/// The dynamic analogue of the retired hand-curated phantom-class map: for each
/// resolved package whose per-content sidecar (written by the extract-time
/// PRODUCER, [`crate::dynamic_phantom`]) reports an unguarded phantom, its
/// `(dep_path, name, undeclared-target-names)`.
///
/// Reads the SAME store handle + sidecar dir the producer wrote, via the shared
/// [`crate::dynamic_phantom`] path helpers, so the two cannot drift. Best-effort:
/// a package absent from the default store (a `store-dir` override moves the CAS),
/// or a failed on-demand scan degrades to "not flagged" — a scan miss never
/// itself forces materialization. Fans out across rayon. A warm sidecar is a
/// cached-JSON index load + a blake3 fingerprint + a small sidecar read; a
/// missing, torn, corrupt, or not-yet-written sidecar is scanned here and cached
/// when publication succeeds.
///
/// Empty under the internal A/B seam ([`enabled`] false). In production `expand`
/// installs the hook only when armed, so this gate is belt-and-suspenders; the
/// pure planning logic is tested through [`plan_from_flags`] with injected flags,
/// so the unit tests never reach this store-IO path.
fn dynamic_phantom_flags(graph: &LockfileGraph) -> Vec<FlaggedImporter> {
    if !enabled() {
        return Vec::new();
    }
    let (Some(store_v1), Some(sidecar_dir)) = (
        crate::dynamic_phantom::store_v1_dir(),
        crate::dynamic_phantom::phantom_cache_dir(),
    ) else {
        return Vec::new();
    };
    // `Store::at` takes the CAS `files/` root; `store_v1` is its parent — the same
    // derivation the extract-time producer uses, so both key the index identically.
    let store = aube_store::Store::at(store_v1.join("files"));
    // BTreeMap has no rayon bridge; collect the resolved set first.
    let packages: Vec<(&String, &LockedPackage)> = graph.packages.iter().collect();
    packages
        .into_par_iter()
        .filter_map(|(dep_path, pkg)| {
            // `registry_name()` + `integrity` key the index the same way the
            // linker does, so npm-alias deps resolve to the right blob.
            let index =
                store.load_index(pkg.registry_name(), &pkg.version, pkg.integrity.as_deref())?;
            // Read the cached verdict, or SCAN the loaded index on-demand when no
            // sidecar exists yet — the warm-cache-first-install gap. The extract
            // hook writes a sidecar only on a fresh FETCH, so a warm-cached package
            // with no sidecar (GC'd, or cached by a pre-eject-default nub) would
            // otherwise reach this decision with no verdict and never seed its
            // eject (leaving it symlinked, its phantom 404'ing).
            // `cached_or_scan_verdict` scans + caches here so the decision is
            // correct at the point the resolved graph + CAS index are both in hand.
            let result = crate::dynamic_phantom::cached_or_scan_verdict(&sidecar_dir, &index)?;
            // A package flags for EITHER an undeclared phantom (`targets`) OR a
            // `.d.ts` peer-type coupling (`type_coupled_peers`, nub#450). react-pdf
            // has NO undeclared phantom — only the react peer typed in its `.d.ts` —
            // so gating on `has_unguarded_phantom` alone would drop it.
            if !result.has_unguarded_phantom && result.type_coupled_peers.is_empty() {
                return None;
            }
            let targets: Vec<String> = result.targets.into_iter().map(|t| t.name).collect();
            Some(FlaggedImporter {
                dep_path: dep_path.clone(),
                name: pkg.name.clone(),
                targets,
                type_peers: result.type_coupled_peers,
            })
        })
        .collect()
}

/// The DefinitelyTyped package name for a runtime package: `react` → `@types/react`,
/// and a scoped `@scope/name` → `@types/scope__name` (the `__` mangling). This is
/// the package whose top-level presence makes a `.d.ts` peer-type eject
/// load-bearing (nub#450).
fn types_package_name(pkg: &str) -> String {
    match pkg.strip_prefix('@').and_then(|r| r.split_once('/')) {
        Some((scope, name)) => format!("@types/{scope}__{name}"),
        None => format!("@types/{pkg}"),
    }
}

/// Whether a dynamically-flagged package must SEED the closure — the precision
/// filter, applied as seed-selection. DEFAULT is SEED (eject); a flag is
/// downgraded to a SKIP (not seeded) only when it can PROVE every undeclared
/// target is BOTH a DIRECT (depth-1) sibling of the importer AND absent from the
/// project top level.
///
/// SAFETY INVARIANT (non-negotiable): a wrong SKIP is a real phantom BREAK,
/// strictly worse than a redundant over-eject. So every uncertainty — a
/// target-less flag, a depth-≥2 / absent target, a top-level target — falls
/// through to SEED. The filter only ever REMOVES an over-seed it can prove safe.
///
/// Why the top-level gate is load-bearing: under GVS there is no hidden hoist
/// tree, so an ejected (project-local) realpath additionally reaches the PROJECT
/// top level in its `node_modules` walk, while a skipped (shared-store) realpath
/// reaches only its own siblings. The eject therefore changes resolution for
/// exactly one class of target — those present at the project top level: a
/// top-level target resolves only when ejected (skipping it 404s), while a
/// non-top-level target is unresolvable in either state (skipping it is a true
/// no-op). (Corpus: es-abstract / typed-array-byte-length are transitively
/// satisfied → SKIP; @hookform/resolvers / swiper / @firebase/database are real
/// breakers → SEED.)
///
/// SCOPE CAVEAT (default-off flag): `is_top_level` here sees only the importer
/// DIRECT deps (`graph.importers`) — exactly the DEFAULT hoist config's top level.
/// The expand seam has no access to the linker's `public-hoist` / `shamefully-
/// hoist` config, so under a NON-default hoist config a target hoisted there (but
/// not a direct importer dep) is invisible to this check and could permit a skip
/// the linker-side gate would have ejected. Acceptable under the experimental flag
/// (the validated corpus is default-hoist); threading the hoist config into the
/// seam is the productionization fix.
fn should_seed(
    targets: &[String],
    reachable: &HashSet<String>,
    is_top_level: impl Fn(&str) -> bool,
) -> bool {
    if targets.is_empty() {
        return true;
    }
    !targets
        .iter()
        .all(|t| reachable.contains(t) && !is_top_level(t))
}

/// The set of package NAMES that are DIRECT (depth-1) declared dependencies of the
/// package at `root_dep_path` and resolve in `graph.packages` — precisely the
/// siblings symlinked into that package's own private `node_modules` under nub's
/// isolated (GVS) layout, and therefore the ONLY names a phantom import from the
/// un-ejected (store-resident) copy can satisfy.
///
/// Depth-1 ONLY, deliberately. Under GVS a store-resident package's realpath lives
/// in the GLOBAL store, so Node's ancestor `node_modules` walk from its files
/// reaches only its own direct siblings — never a transitive dep's private tree (a
/// different store path) and never the project top level (the walk ascends the
/// store, not the project). A target declared by a TRANSITIVE (depth-≥2) dep is
/// thus NOT resolvable from the un-ejected copy; the earlier multi-hop BFS counted
/// it reachable, which let a depth-≥2 phantom target (`@crawlee/basic` →
/// `@crawlee/core` → `@apify/datastructures`, #280) wrongly SKIP its eject and
/// break. Depth-≥2 and absent targets now fall through to SEED — the safe
/// direction (the ejected copy resolves the target via the collective hidden tree).
///
/// Reads `dependencies` ONLY: per [`LockedPackage`]'s contract that map is the
/// resolved edge set with ACTIVE optionals and RESOLVED peer versions already
/// MIRRORED in — exactly the depth-1 siblings on disk. A name enters only when its
/// full dep_path (`{name}@{tail}`, the tail carrying any peer suffix) resolves in
/// `graph.packages`; an unresolvable edge is dropped, erring toward SEED.
fn direct_dep_names(root_dep_path: &str, graph: &LockfileGraph) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(pkg) = graph.packages.get(root_dep_path) else {
        return names;
    };
    for (child_name, child_tail) in &pkg.dependencies {
        let child_key = format!("{child_name}@{child_tail}");
        if graph.packages.contains_key(&child_key) {
            names.insert(child_name.clone());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test-graph edge: `(dep_path, name, [(child_name, child_tail)])`.
    type Edge<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

    fn graph(edges: &[Edge]) -> LockfileGraph {
        let mut g = LockfileGraph::default();
        for (dep_path, name, deps) in edges {
            let mut pkg = LockedPackage {
                name: name.to_string(),
                dep_path: dep_path.to_string(),
                ..Default::default()
            };
            // A real graph carries `version`; the vite<8.1 seed test needs it, so
            // parse it off the dep_path tail.
            if let Some((_, tail)) = split(dep_path) {
                pkg.version = tail.split('(').next().unwrap_or(tail).to_string();
            }
            for (cn, ct) in *deps {
                pkg.dependencies.insert(cn.to_string(), ct.to_string());
            }
            g.packages.insert(dep_path.to_string(), pkg);
        }
        g
    }

    fn split(dep_path: &str) -> Option<(&str, &str)> {
        let core_end = dep_path.find('(').unwrap_or(dep_path.len());
        let at = dep_path[..core_end].rfind('@')?;
        if at == 0 {
            return None;
        }
        Some((&dep_path[..at], &dep_path[at + 1..]))
    }

    fn names(xs: &[&str]) -> HashSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    /// A flagged importer carrying undeclared phantom `targets` (no peer-types).
    fn flag(dep_path: &str, name: &str, targets: &[&str]) -> FlaggedImporter {
        FlaggedImporter {
            dep_path: dep_path.to_string(),
            name: name.to_string(),
            targets: targets.iter().map(|s| s.to_string()).collect(),
            type_peers: Vec::new(),
        }
    }

    /// A flagged importer carrying only `.d.ts` type-coupled peers (nub#450).
    fn type_flag(dep_path: &str, name: &str, type_peers: &[&str]) -> FlaggedImporter {
        FlaggedImporter {
            dep_path: dep_path.to_string(),
            name: name.to_string(),
            targets: Vec::new(),
            type_peers: type_peers.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Register `names` as the root importer's direct deps — what `is_top_level`
    /// reads (the `@types/<peer>` presence gate for the type-peer eject).
    fn top_level(g: &mut LockfileGraph, names: &[&str]) {
        g.importers.insert(
            ".".to_string(),
            names
                .iter()
                .map(|n| aube_lockfile::DirectDep {
                    name: n.to_string(),
                    dep_path: format!("{n}@0.0.0"),
                    dep_type: aube_lockfile::DepType::Dev,
                    specifier: None,
                })
                .collect(),
        );
    }

    // Rung-1 vite seeding is independent of the dynamic source, so these inject
    // an EMPTY flag set and exercise the pure planner (`plan_from_flags`) end to
    // end — no host-store IO.

    #[test]
    fn embedded_vite_lt_8_1_seeds_its_framework_closure() {
        // astro → vite@6.4.3 (embedded, <8.1). The closure disk-materializes
        // BOTH so the ejected vite is project-local for the #318 patch.
        let g = graph(&[
            ("astro@5.0.0", "astro", &[("vite", "6.4.3")]),
            ("vite@6.4.3", "vite", &[]),
            // an unrelated modern vite direct dep must NOT drag anything in
            ("lodash@4.17.21", "lodash", &[]),
        ]);
        let plan = plan_from_flags(&g, &[], &[]);
        let plan_names: HashSet<&str> = plan.names.iter().map(String::as_str).collect();
        assert!(plan_names.contains("vite"), "embedded vite<8.1 seeded");
        assert!(plan_names.contains("astro"), "framework in the closure");
        assert!(
            !plan_names.contains("lodash"),
            "unrelated dep stays symlinked"
        );
    }

    #[test]
    fn direct_dep_vite_ge_8_1_not_ejected_even_with_name_seed() {
        // Regression for the version-blind over-eject: a direct-dep vite carries
        // the embedder's `vite` name-seed (mod.rs), but vite ≥ 8.1 reads
        // `.modules.yaml` natively (#318) and must stay symlinked. The planner
        // prunes the name-seed, so a ≥ 8.1 direct dep yields an EMPTY plan — no
        // eject of vite or its ancestor closure. (Passing the real production
        // `["vite"]` seed is load-bearing: the old `&[]` seed masked the bug.)
        let g = graph(&[
            ("app@1.0.0", "app", &[("vite", "8.1.3")]),
            ("vite@8.1.3", "vite", &[]),
        ]);
        let plan = plan_from_flags(&g, &["vite".to_string()], &[]);
        assert!(
            plan.names.is_empty(),
            "vite>=8.1 needs no eject (Unit A covers it); got {:?}",
            plan.names
        );
    }

    #[test]
    fn direct_dep_vite_lt_8_1_still_ejects_despite_name_seed_prune() {
        // The prune must not weaken the < 8.1 path: a direct-dep vite < 8.1 carries
        // the `vite` name-seed AND is caught by the version-aware `vite_lt_8_1`
        // dep_path auto-seed. Dropping the name-seed loses nothing — vite and its
        // importer closure still disk-materialize so the #318 dist patch reaches a
        // now-project-local copy.
        let g = graph(&[
            ("app@1.0.0", "app", &[("vite", "7.0.0")]),
            ("vite@7.0.0", "vite", &[]),
        ]);
        let plan = plan_from_flags(&g, &["vite".to_string()], &[]);
        let names: HashSet<&str> = plan.names.iter().map(String::as_str).collect();
        assert!(names.contains("vite"), "vite<8.1 still ejects: {names:?}");
        assert!(
            names.contains("app"),
            "its importer closure ejects with it: {names:?}"
        );
    }

    #[test]
    fn mixed_embedded_lt_and_direct_ge_vite_is_not_worse_than_pre_fix() {
        // Embedded vite<8.1 (astro→6.4.3) + a direct vite>=8.1 in one graph. The
        // <8.1 copy seeds its closure via `vite_lt_8_1`, which re-adds "vite" to
        // `names`; because the executor is NAME-keyed, "vite" materializes BOTH
        // copies — identical to pre-fix (which ejected every vite too). Locks in
        // that the prune never regresses the mixed case.
        let g = graph(&[
            ("astro@5.0.0", "astro", &[("vite", "6.4.3")]),
            ("vite@6.4.3", "vite", &[]),
            ("app@1.0.0", "app", &[("vite", "8.1.3")]),
            ("vite@8.1.3", "vite", &[]),
        ]);
        let plan = plan_from_flags(&g, &["vite".to_string()], &[]);
        let names: HashSet<&str> = plan.names.iter().map(String::as_str).collect();
        assert!(names.contains("vite"), "name-keyed vite still materializes");
        assert!(names.contains("astro"), "the <8.1 framework closure ejects");
    }

    #[test]
    fn no_seeds_yields_empty_plan() {
        let g = graph(&[("lodash@4.17.21", "lodash", &[])]);
        let plan = plan_from_flags(&g, &[], &[]);
        assert!(plan.names.is_empty());
    }

    // Curated project-context eject (nub#457): a build that reads/mutates the
    // consuming project ejects from GVS; a self-contained build stays symlinked.

    #[test]
    fn project_context_pkg_ejects_from_gvs() {
        // `simple-git-hooks` postinstall walks up from cwd to the project root; under
        // GVS the store-detached copy crashes (#457 ENOENT). As a curated
        // project-context leaf it seeds its own closure (no importers, no phantom
        // flag) purely by name.
        let g = graph(&[("simple-git-hooks@2.13.1", "simple-git-hooks", &[])]);
        let plan = plan_from_flags(&g, &[], &[]);
        assert!(
            plan.names.iter().any(|n| n == "simple-git-hooks"),
            "a curated project-context package ejects: {:?}",
            plan.names
        );
    }

    #[test]
    fn self_contained_build_stays_symlinked() {
        // esbuild ships a prebuilt-binary downloader — self-contained, output shared
        // cross-project via the side-effects cache — so it is deliberately NOT
        // curated and stays in GVS (symlinked, built once). No phantom flag, so
        // nothing seeds it: the eject is curated, not blanket-on-every-script-haver.
        let g = graph(&[("esbuild@0.20.0", "esbuild", &[])]);
        let plan = plan_from_flags(&g, &[], &[]);
        assert!(
            plan.names.is_empty(),
            "a self-contained build is not ejected: {:?}",
            plan.names
        );
    }

    #[test]
    fn eject_list_token_is_order_independent_and_edit_sensitive() {
        // The token feeds the install-state fingerprint (#457 warm-tree fix): a pure
        // reorder is a no-op (it's a set), while any add/remove MUST move it so an
        // existing install relinks. And it is deterministic — a stable fingerprint,
        // not a per-run value.
        assert_eq!(
            eject_list_token(&["a", "b", "c"]),
            eject_list_token(&["c", "a", "b"]),
            "a pure reorder does not change the token"
        );
        assert_ne!(
            eject_list_token(&["a", "b"]),
            eject_list_token(&["a", "b", "c"]),
            "an added name changes the token"
        );
        assert_eq!(
            project_context_eject_token(),
            project_context_eject_token(),
            "deterministic across calls"
        );
    }

    #[test]
    fn user_seed_names_are_dropped_only_internal_vite_survives() {
        // The user-facing `diskMaterializePackages` knob is retired under nub: a
        // value the user set in `.npmrc`/env/`pnpm-workspace.yaml` reaches the hook
        // merged with nub's internal vite embedder default, and the filter must keep
        // ONLY vite. So a hand-listed `lodash`/`@hookform/resolvers` is dropped
        // (the detector, not a manual list, ejects real phantoms).
        let kept = nub_internal_seed(&[
            "lodash".to_string(),
            "vite".to_string(),
            "@hookform/resolvers".to_string(),
        ]);
        assert_eq!(kept, vec!["vite".to_string()]);
        assert!(nub_internal_seed(&["lodash".to_string()]).is_empty());
    }

    #[test]
    fn dynamic_flag_seeds_importer() {
        // The now-default path: a phantom adapter (`@hookform/resolvers`)
        // statically imports an undeclared `zod`. The target isn't reachable within
        // the adapter's own subtree, so `should_seed` SEEDS it (ejects it). The
        // collective hidden tree then resolves the undeclared `zod` at link time —
        // the planner only needs to grow the eject set, not record a target hoist.
        let g = graph(&[
            ("@hookform/resolvers@1.0.0", "@hookform/resolvers", &[]),
            ("zod@3.0.0", "zod", &[]),
        ]);
        let flags = vec![flag(
            "@hookform/resolvers@1.0.0",
            "@hookform/resolvers",
            &["zod"],
        )];
        let plan = plan_from_flags(&g, &[], &flags);
        let names: HashSet<&str> = plan.names.iter().map(String::as_str).collect();
        assert!(
            names.contains("@hookform/resolvers"),
            "the flagged phantom importer is seeded (ejected)"
        );
    }

    #[test]
    fn optional_peer_host_still_ejects_via_embedded_vite() {
        // Nuxt shape at the planner boundary: vue-router embeds vite<8.1 (so its
        // ancestor-closure ejects) and declares `@vue/compiler-sfc` an OPTIONAL
        // peer that its `/vite` subpath statically imports. The eject is what
        // matters — once vue-router is project-local, the collective hidden tree
        // resolves the undeclared `@vue/compiler-sfc` for it (the reachability the
        // store-resident realpath walk lacked under GVS, the `nuxt prepare` crash).
        // The planner records no per-importer hoist.
        let mut g = graph(&[
            ("vue-router@5.1.0", "vue-router", &[("vite", "7.0.0")]),
            ("vite@7.0.0", "vite", &[]),
            ("@vue/compiler-sfc@3.5.39", "@vue/compiler-sfc", &[]),
        ]);
        let vr = g.packages.get_mut("vue-router@5.1.0").unwrap();
        vr.peer_dependencies
            .insert("@vue/compiler-sfc".to_string(), "^3.5.34".to_string());
        vr.peer_dependencies_meta.insert(
            "@vue/compiler-sfc".to_string(),
            aube_lockfile::PeerDepMeta { optional: true },
        );

        let plan = plan_from_flags(&g, &[], &[]);
        let names: HashSet<&str> = plan.names.iter().map(String::as_str).collect();
        assert!(
            names.contains("vue-router"),
            "the embedded-vite<8.1 closure ejects vue-router: {names:?}"
        );
    }

    // Precision seed-selection (`should_seed`) — the port of the retired link-time
    // filter, tested as a pure function.

    #[test]
    fn seed_unless_every_target_in_closure_and_non_top_level() {
        let closure = names(&["a", "b", "for-each"]);
        let none_top_level = |_: &str| false;
        // All targets in-closure, none at the project top level → safe SKIP.
        assert!(!should_seed(
            &["a".to_string(), "b".to_string()],
            &closure,
            none_top_level
        ));
        // A target outside the closure → SEED (can't prove safe).
        assert!(should_seed(
            &["a".to_string(), "missing".to_string()],
            &closure,
            none_top_level
        ));
        // A target-less flag → SEED (can't prove safe on incomplete info).
        assert!(should_seed(&[], &closure, none_top_level));
    }

    #[test]
    fn top_level_target_forces_seed_even_when_in_closure() {
        // The wrong-skip hole: `for-each` is in-closure BUT also at the project
        // top level, so an ejected (project-local) copy resolves it while a
        // skipped (shared-store) one 404s — the eject is load-bearing, so the
        // top-level gate must veto the skip.
        let closure = names(&["for-each", "a"]);
        let for_each_top_level = |n: &str| n == "for-each";
        assert!(should_seed(
            &["for-each".to_string()],
            &closure,
            for_each_top_level
        ));
        // A different, non-top-level in-closure target stays safe to skip.
        assert!(!should_seed(
            &["a".to_string()],
            &closure,
            for_each_top_level
        ));
    }

    // Direct-dep reachability (`direct_dep_names`) — the depth-1 sibling name set
    // the precision filter consults; a store-resident copy resolves ONLY these.

    #[test]
    fn direct_dep_names_are_depth1_only_not_transitive() {
        // P → a → shared; P → styled(peer suffix in the tail). `a` and `styled` are
        // direct (depth-1) siblings; `shared` sits at depth 2 behind `a` and is NOT
        // a sibling of `p` under isolated layout, so it must NOT count as reachable
        // — the exact distinction the #280 fix turns on. The peer-suffixed tail
        // (`(react@18.2.0)`) must still reconstruct to key `graph.packages`.
        let g = graph(&[
            (
                "p@1.0.0",
                "p",
                &[("a", "1.0.0"), ("styled", "6.0.0(react@18.2.0)")],
            ),
            ("a@1.0.0", "a", &[("shared", "2.0.0")]),
            ("shared@2.0.0", "shared", &[]),
            ("styled@6.0.0(react@18.2.0)", "styled", &[]),
        ]);
        let r = direct_dep_names("p@1.0.0", &g);
        assert!(r.contains("a"), "direct dep");
        assert!(r.contains("styled"), "peer-suffixed tail reconstructs");
        assert!(
            !r.contains("shared"),
            "depth-2 transitive dep is NOT a depth-1 sibling"
        );
        assert!(!r.contains("p"), "root itself is not among its own deps");
    }

    #[test]
    fn direct_dep_names_drop_unresolvable_edge_toward_seed() {
        // P declares `a@9.9.9`, absent from the graph → the edge does not resolve,
        // so `a` stays out of the depth-1 set → a phantom target of `a` SEEDS (the
        // safe direction). `deep` is depth-2 and never a sibling regardless.
        let g = graph(&[
            ("p@1.0.0", "p", &[("a", "9.9.9")]),
            ("a@1.0.0", "a", &[("deep", "1.0.0")]),
            ("deep@1.0.0", "deep", &[]),
        ]);
        let r = direct_dep_names("p@1.0.0", &g);
        assert!(!r.contains("a"), "unresolved edge is not counted reachable");
        assert!(!r.contains("deep"), "depth-2 dep is not a sibling anyway");
    }

    #[test]
    fn depth2_phantom_target_seeds_not_skipped() {
        // #280 @crawlee shape at the planner boundary: importer `basic` → direct dep
        // `core`, and `core` declares `datastructures` (depth 2 from `basic`).
        // `basic` phantom-imports `datastructures`, which is NOT a symlinked sibling
        // in `basic`'s own private node_modules under isolated layout, so the
        // un-ejected copy cannot resolve it — the flag MUST SEED (eject), never skip
        // as "transitively reachable". `datastructures` is absent from the (empty)
        // project top level, so only the depth fix drives the seed. FAILS before the
        // fix (multi-hop BFS marks `datastructures` reachable → SKIP → `basic` absent
        // from the plan); passes after. Once `basic` is ejected the collective hidden
        // tree resolves the undeclared `datastructures` for it.
        let g = graph(&[
            ("basic@1.0.0", "basic", &[("core", "1.0.0")]),
            ("core@1.0.0", "core", &[("datastructures", "2.0.0")]),
            ("datastructures@2.0.0", "datastructures", &[]),
        ]);
        let flags = vec![flag("basic@1.0.0", "basic", &["datastructures"])];
        let plan = plan_from_flags(&g, &[], &flags);
        let names: HashSet<&str> = plan.names.iter().map(String::as_str).collect();
        assert!(
            names.contains("basic"),
            "depth-2 phantom target must SEED the importer, not skip: {names:?}"
        );
    }

    #[test]
    fn depth1_phantom_target_still_skips() {
        // The precision win must survive the fix: an importer whose undeclared
        // target IS its own direct (depth-1) sibling — resolved into its private
        // node_modules already — needs no eject, so the flag still SKIPs (empty
        // plan). Guards against the fix collapsing into "always seed".
        let g = graph(&[
            ("adapter@1.0.0", "adapter", &[("helper", "1.0.0")]),
            ("helper@1.0.0", "helper", &[]),
        ]);
        let flags = vec![flag("adapter@1.0.0", "adapter", &["helper"])];
        let plan = plan_from_flags(&g, &[], &flags);
        assert!(
            plan.names.is_empty(),
            "a depth-1-satisfied phantom target stays skipped: {:?}",
            plan.names
        );
    }

    // Type-coupled peers (nub#450): the `.d.ts` peer-type eject path.

    #[test]
    fn type_peer_ejects_only_when_types_pkg_is_top_level() {
        // react-pdf shape: it declares `react` a peer and imports it from its
        // `.d.ts`. The eject is load-bearing ONLY because the project has a
        // top-level `@types/react` the store realpath can't otherwise reach.
        let mut g = graph(&[
            ("@react-pdf/renderer@4.5.1", "@react-pdf/renderer", &[]),
            ("@types/react@18.3.0", "@types/react", &[]),
        ]);
        top_level(&mut g, &["@react-pdf/renderer", "react", "@types/react"]);
        let flags = vec![type_flag(
            "@react-pdf/renderer@4.5.1",
            "@react-pdf/renderer",
            &["react"],
        )];
        let plan = plan_from_flags(&g, &[], &flags);
        let names: HashSet<&str> = plan.names.iter().map(String::as_str).collect();
        assert!(
            names.contains("@react-pdf/renderer"),
            "peer-type importer seeds when @types/react is top-level: {names:?}"
        );
    }

    #[test]
    fn type_peer_does_not_eject_without_top_level_types_pkg() {
        // A self-typed peer (vue ships its own types → no `@types/vue` in the tree)
        // must NOT eject — this is the bound that keeps GVS on for the common case.
        let mut g = graph(&[
            ("some-vue-lib@1.0.0", "some-vue-lib", &[]),
            ("vue@3.5.0", "vue", &[]),
        ]);
        // vue is top-level but ships its own types → NO `@types/vue` in the tree.
        top_level(&mut g, &["some-vue-lib", "vue"]);
        let flags = vec![type_flag("some-vue-lib@1.0.0", "some-vue-lib", &["vue"])];
        let plan = plan_from_flags(&g, &[], &flags);
        assert!(
            plan.names.is_empty(),
            "no top-level @types/vue → no eject (GVS stays on): {:?}",
            plan.names
        );
    }

    #[test]
    fn type_peer_handles_scoped_types_mangling() {
        // A scoped peer's DefinitelyTyped name mangles the slash: `@scope/pkg` →
        // `@types/scope__pkg`. The top-level check must use the mangled name.
        let mut g = graph(&[
            ("uses-babel@1.0.0", "uses-babel", &[]),
            ("@types/babel__core@7.0.0", "@types/babel__core", &[]),
        ]);
        top_level(&mut g, &["uses-babel", "@types/babel__core"]);
        let flags = vec![type_flag(
            "uses-babel@1.0.0",
            "uses-babel",
            &["@babel/core"],
        )];
        let plan = plan_from_flags(&g, &[], &flags);
        let names: HashSet<&str> = plan.names.iter().map(String::as_str).collect();
        assert!(
            names.contains("uses-babel"),
            "scoped @types/scope__name mangling drives the seed: {names:?}"
        );
        assert_eq!(types_package_name("@babel/core"), "@types/babel__core");
        assert_eq!(types_package_name("react"), "@types/react");
    }
}
