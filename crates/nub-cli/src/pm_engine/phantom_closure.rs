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

use std::collections::{BTreeMap, HashSet};
use std::sync::{LazyLock, PoisonError, RwLock};

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

/// nub's own embedder-default names that may seed the eject set. Native
/// `install.linker.eject` entries are admitted separately; incumbent
/// `.npmrc`/env/workspace values remain ignored. Standalone aube installs no
/// hook and honors its full `diskMaterializePackages` knob unchanged.
///
/// SHARED with [`super::nub_setting_defaults`], which seeds exactly this name as
/// the embedder default — sourcing both from one const so a future internal
/// default can't be added in one place and silently dropped by the other.
pub(super) const NUB_INTERNAL_DISK_MATERIALIZE_SEED: &[&str] = &["vite"];

/// `install.linker.eject` from the project's `nub.jsonc`, published by
/// [`super::engine_session_inner`]. A process-global because the eject hook is a
/// bare `fn` the engine installs once and calls with only the resolved graph —
/// there is no seam to thread session state through. Poisoning is ignored
/// throughout: the guarded value is a plain name list, so a panic mid-write
/// cannot leave it inconsistent.
static NATIVE_CONFIG_SEED: LazyLock<RwLock<Vec<String>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

pub(super) fn set_native_config_seed(seed: Vec<String>) {
    *NATIVE_CONFIG_SEED
        .write()
        .unwrap_or_else(PoisonError::into_inner) = seed;
}

// WHY PLACEMENT EXISTS AT ALL (nub#457). A build script that READS or MUTATES the consuming
// project — a git-hook installer walking up from `cwd` to write `.git/hooks`, a generator
// emitting project-local files — cannot do so from the global store: the build runs DETACHED,
// so the upward walk lands on the per-package store wrapper (no package.json) and the script
// either crashes (`simple-git-hooks` postinstall → ENOENT) or silently emits shared-mutable
// wrong output. Disk-materializing project-local restores a real project above `cwd`.
//
// This used to be a curated 21-name list. It is now DERIVED: `expand()` seeds every package
// whose manifest declares preinstall/install/postinstall, which covers all 21 by construction
// — they were hook installers, so each declares one — plus the ones nobody wrote down.

/// Bumped on ANY change to the [`nested_optional_dep_pairs`] selection rules.
///
/// Load-bearing for warm trees, for the same reason as
/// [`project_context_eject_token`]: the pairs are injected INSIDE the expand hook,
/// past aube's settings fold, so without a moving token an already-installed
/// project keeps its `settings_hash`, `try_install_fast_path` reports
/// "Already up to date", and the link phase — where every nesting call site lives
/// — never runs at all. Verified: stripping the nest from a warm store cell and
/// re-installing left it stripped until this token moved. A graph-derived hash is
/// not available here (the fingerprint is computed without the resolved graph), so
/// a hand-bumped version is the form that composes.
pub(crate) const NESTED_OPTIONAL_DEP_POLICY_VERSION: u32 = 1;

/// Fingerprints the PLACEMENT POLICY into the install-state settings hash via
/// [`crate::dynamic_phantom::settings_token`] (the `extra_settings_fingerprint` hook).
///
/// Load-bearing for warm/upgrade trees (nub#457): the seed is injected INSIDE the expand
/// hook, past aube's `disk_materialize_packages` settings fold, so without a token here an
/// existing install keeps an identical `settings_hash`, the existence-gated fast path accepts
/// the stale symlinked tree, and the packages the current policy would place stay symlinked.
///
/// This used to hash a curated 21-name list, so editing the list moved the token. The seed is
/// now DERIVED per package from its manifest, so there is no list to fingerprint — the seed
/// follows the graph, which the lockfile already covers. A constant naming the policy is what
/// is left: it moves the hash exactly once, invalidating every tree built under the list, and
/// is stable afterwards.
pub(crate) fn project_context_eject_token() -> String {
    String::from("placement=declares-lifecycle-script+gyp-provider/v1")
}

/// Keep nub's internal and native-config seed names, dropping every
/// incumbent/user-source `diskMaterializePackages` entry.
fn nub_internal_seed(resolved_seed: &[String]) -> Vec<String> {
    let configured = NATIVE_CONFIG_SEED
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    resolved_seed
        .iter()
        .filter(|n| {
            NUB_INTERNAL_DISK_MATERIALIZE_SEED.contains(&n.as_str()) || configured.contains(n)
        })
        .cloned()
        .collect()
}

/// The hook entry: read the per-version scanner's sidecars (the store-IO half)
/// then hand off to the pure planner. Split so [`plan_from_flags`] — all the
/// closure/seed policy — is unit-tested with injected flags and never touches
/// the host store. The resolved seed is filtered through [`nub_internal_seed`].
/// The union [`plan_from_flags`] is seeded with, extracted so a test can pin the SET this
/// function actually produces.
///
/// ⛔ IT IS A NAMED FUNCTION FOR A TESTABILITY REASON, NOT A TIDINESS ONE. [`expand`] builds its
/// store handle from disk, so a unit test cannot call it — which tempts a test into unioning the
/// parts ITSELF and asserting the planner ejects them. That assertion is a tautology about
/// [`plan_from_flags`]: it holds whatever this function does, so it stays green when a seed source
/// is dropped here. MEASURED — deleting the nested-seed line while the test composed its own union
/// left all six tests passing.
fn eject_seeds(
    configured_seed: &[String],
    script_seeds: &[String],
    gyp_seeds: &[String],
    nested_seeds: &[String],
) -> Vec<String> {
    let mut all_seeds = configured_seed.to_vec();
    all_seeds.extend(script_seeds.iter().cloned());
    all_seeds.extend(gyp_seeds.iter().cloned());
    all_seeds.extend(nested_seeds.iter().cloned());
    all_seeds
}

fn expand(graph: &LockfileGraph, seed_names: &[String]) -> DiskMaterializePlan {
    // THE STORE IS BUILT FIRST because the script seed below needs it. Same handle the
    // nested-optional-dep predicate uses; no store means no manifest to read, which reports no
    // scripts and leaves every package on the unchanged sibling-symlink path.
    let store = crate::dynamic_phantom::store_v1_dir()
        .map(|store_v1| aube_store::Store::at(store_v1.join("files")));

    // EVERY PACKAGE THAT DECLARES A LIFECYCLE SCRIPT IS SEEDED, replacing a curated 21-name
    // list. A script cannot read the consuming project from the global store: it walks UP from
    // its own directory looking for the project root and lands in `~/.cache/` instead, so a
    // hook installer writes nothing and exits 0 — a silent no-op no grant can fix, because it
    // is a LAYOUT problem wearing a permissions costume.
    //
    // Keyed on the manifest, not on names. `declares_install_script` reads package.json out of
    // the CAS and checks preinstall/install/postinstall, so a package nobody thought to list is
    // covered and a listed package that stopped needing it costs nothing.
    //
    // This is where bun, yarn PnP and pnpm independently converged: bun excludes script-runners
    // from its global store ("a shared global copy would either diverge from the patch or be
    // mutated underneath other projects"), yarn PnP unplugs build-script packages, pnpm defaults
    // builds off under its global virtual store. None of them uses a name list. Bun had to seed
    // on trusted-ness instead because `hasInstallScript` is absent from `bun.lock`; nub reads
    // the manifest at plan time, so it can seed on the precise predicate.
    //
    // Over-seeding is the SAFE direction: an extra package materializes project-local and loses
    // store sharing. Under-seeding is a package that silently does nothing.
    let mut script_seeds: Vec<String> = Vec::new();
    if let Some(store) = store.as_ref() {
        let mut seen: HashSet<&str> = HashSet::new();
        for pkg in graph.packages.values() {
            if seen.insert(pkg.name.as_str()) && declares_install_script(store, pkg) {
                script_seeds.push(pkg.name.clone());
            }
        }
    }
    // A lifecycle-script package's own gyp providers move WITH it. See
    // `gyp_provider_seeds` for the invariant a one-sided eject breaks.
    let script_names: HashSet<&str> = script_seeds.iter().map(String::as_str).collect();
    let gyp_seeds = gyp_provider_seeds(graph, &script_names, &|pkg: &LockedPackage| {
        store
            .as_ref()
            .is_some_and(|store| ships_gyp_file(store, pkg))
    });

    // The seed SOURCES stay separate past this point: the plan takes their union,
    // but the report's labeller needs to tell "the user named it" from "its
    // manifest declares a lifecycle script" — different reasons.
    let configured_seed = nub_internal_seed(seed_names);
    // Store handle built ONCE and captured, matching `dynamic_phantom_flags`
    // below. No store (a `storeDir` override the sidecar helpers do not know
    // about) reports no scripts, which leaves every package on the unchanged
    // sibling-symlink path.
    let nested_optional_deps = nested_optional_dep_pairs(graph, &|pkg: &LockedPackage| {
        store
            .as_ref()
            .is_some_and(|store| declares_install_script(store, pkg))
    });
    // EVERY NESTED OPTIONAL DEP IS ALSO EJECTED PROJECT-LOCAL, and the nesting alone
    // is why that is not redundant. See `nested_optional_dep_pairs` for the whole
    // defect; the half this seed closes is that nesting adds a NEARER copy without
    // removing the FARTHER one. The importer's cell keeps a sibling symlink into the
    // peer's shared cell, and the project keeps a hidden-hoist alias to it, so the
    // moment the script consumes the nest — which is the normal outcome, the script
    // MOVES what it resolves — Node's walk continues onto shared state and the next
    // run mutates a directory every project reads. Ejecting the peer makes both of
    // those routes land on a project-local copy instead, which is what npm's flat
    // layout gives the same script and what makes the `"no-jail"` escape survivable.
    let nested_seeds: Vec<String> = nested_optional_deps
        .iter()
        .map(|(_, dep)| dep.clone())
        .collect();
    let all_seeds = eject_seeds(&configured_seed, &script_seeds, &gyp_seeds, &nested_seeds);

    let flags = dynamic_phantom_flags(graph);
    let mut plan = plan_from_flags(graph, &all_seeds, &flags);
    plan.nested_optional_deps = nested_optional_deps;
    // The install report's digest names what moved and why. Labelling runs as a
    // second pass over the FINISHED plan rather than inside the planner, so the
    // planner and its tests stay untouched and the reported SET can never
    // disagree with the executed one — only a label could ever be off.
    super::install_report::record_plan(label_plan(
        graph,
        &configured_seed,
        &script_seeds,
        &gyp_seeds,
        &nested_seeds,
        &flags,
        &plan,
    ));
    plan
}

/// `(importer, optional-dep)` pairs whose dependency is additionally materialized
/// INSIDE the importer's own package directory. THE CANONICAL STATEMENT of this
/// policy — the linker side carries only the mechanism.
///
/// THE DEFECT THIS CLOSES. The ecosystem idiom for shipping a platform binary is
/// an optionalDependency plus a postinstall that `require.resolve`s it and
/// `rename()`s the file into the importer's own `bin/`. Under the isolated linker
/// that resolve returns a realpath in a SEPARATE virtual-store cell, and `rename`
/// needs write on the SOURCE's parent to unlink the dirent — so the move lands on
/// a cell shared by every project on the machine. Both outcomes are wrong and they
/// are one bug: under the build jail the store is read-only and the script fails
/// outright (`bun`: "Your package manager doesn't seem to support bun"); without
/// the jail the script SUCCEEDS and empties the peer package's canonical cell, so
/// every other project depending on it installs a binary-less directory while
/// `nub install` reports success. Both reproduced. Nesting a copy inside the
/// importer makes the resolved realpath a path the importer may write, so the
/// move consumes that copy instead of the peer's canonical one.
///
/// NESTING ALONE IS NOT ENOUGH, and the second half is why every pair here is ALSO
/// seeded into the eject set by [`expand`]. Nesting adds a NEARER copy; it removes
/// no farther one. The importer's cell keeps its sibling symlink into the peer's
/// shared cell and the project keeps a hidden-hoist alias to it, so as soon as the
/// script consumes the nest — the normal outcome, since it MOVES what it resolves —
/// Node's walk continues onto shared state. Measured on `bun@1.4.0`: the install
/// itself passes, then `nub approve-builds bun` re-runs the postinstall, finds the
/// nest empty and dies on `file-write-unlink` against the peer's cell; adding the
/// `"no-jail"` escape the CLI prints for that refusal makes the same run exit 0 and
/// DELETE the peer's binary, after which an unrelated project installing
/// `@oven/bun-darwin-aarch64` gets an empty `bin/` from a green `nub install` and
/// the linker's warm short-circuit never refills it. Ejecting the peer replaces the
/// shared target of both routes with a project-local copy, so the importer's cell
/// holds no path to shared state at all — the layout npm gives the same script.
///
/// Each conjunct is a SAFETY guard, and each is load-bearing:
/// - the importer runs an install script — otherwise nothing moves anything, and
///   the extra copy would buy a second realpath for free;
/// - the dep is a graph LEAF — the nested copy carries no dependency edges, so a
///   dep with its own deps would resolve them out of the nest into the importer's
///   sibling set, a different closure than it would see un-nested;
/// - no second DECLARED consumer, by importer count or top-level presence — two
///   resolution paths for one package is a double load, which for the native
///   `.node` addons this class ships is a double registration. Deliberately about
///   DECLARED consumers only: the hidden hoist tree aliases every graph package,
///   so an undeclared walk-up can still reach the peer cell. That is unchanged by
///   this policy — the importer, the only declared consumer, resolves the nearer
///   nested copy.
fn nested_optional_dep_pairs(
    graph: &LockfileGraph,
    runs_install_script: &dyn Fn(&LockedPackage) -> bool,
) -> Vec<(String, String)> {
    use aube_lockfile::resolve_dep_edge;

    // Count BOTH maps: the npm/pnpm/bun readers mirror an active optional edge into
    // `dependencies`, but the yarn-berry reader keeps the two disjoint. Counting
    // only `dependencies` would score a berry optional dep at zero importers, and
    // the sole-importer guard below would silently skip every yarn project.
    let mut importers_of: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for pkg in graph.packages.values() {
        let mut counted: HashSet<String> = HashSet::new();
        for (child_name, child_tail) in pkg
            .dependencies
            .iter()
            .chain(pkg.optional_dependencies.iter())
        {
            if let Some(child_key) =
                resolve_dep_edge(child_name, child_tail, |k| graph.packages.contains_key(k))
                && counted.insert(child_key.clone())
            {
                *importers_of.entry(child_key).or_default() += 1;
            }
        }
    }
    let top_level: HashSet<&str> = graph
        .importers
        .values()
        .flat_map(|deps| deps.iter().map(|d| d.name.as_str()))
        .collect();

    let mut pairs = Vec::new();
    for pkg in graph.packages.values() {
        if pkg.optional_dependencies.is_empty() {
            continue;
        }
        // `runs_install_script` reads the manifest out of the CAS, so it runs LAST —
        // after the free graph guards have already rejected most candidates.
        let mut script_checked = None;
        for (dep_name, dep_tail) in &pkg.optional_dependencies {
            if top_level.contains(dep_name.as_str()) {
                continue;
            }
            let Some(dep_key) =
                resolve_dep_edge(dep_name, dep_tail, |k| graph.packages.contains_key(k))
            else {
                continue;
            };
            if importers_of.get(&dep_key).copied().unwrap_or(0) != 1 {
                continue;
            }
            let Some(dep_pkg) = graph.packages.get(&dep_key) else {
                continue;
            };
            if !dep_pkg.dependencies.is_empty() || !dep_pkg.optional_dependencies.is_empty() {
                continue;
            }
            if !*script_checked.get_or_insert_with(|| runs_install_script(pkg)) {
                break;
            }
            pairs.push((pkg.name.clone(), dep_name.clone()));
        }
    }
    pairs
}

/// Whether `pkg`'s published manifest declares a lifecycle script that runs at
/// install time. Read from the CAS copy of its `package.json` rather than
/// `LockedPackage::has_install_script`, which only npm's lockfile format
/// populates — under an aube/pnpm/bun lockfile that field is uniformly false.
///
/// ⛔ AN INSTALL-TIME SCRIPT NEED NOT BE DECLARED AT ALL. npm gives any package shipping a
/// root `binding.gyp` an implicit `node-gyp rebuild` when it declares neither `install` nor
/// `preinstall`, and that implicit build has exactly the layout problem the explicit one has —
/// so reading `scripts` alone under-seeds the whole native-addon ecosystem.
///
/// MEASURED 2026-08-28 against the build-jail corpus, on the published TARBALLS (the registry's
/// packument metadata is NOT ground truth here — it reports a synthesized `scripts.install` for
/// exactly these packages, which is what made this look like a nub defect for an afternoon):
///
/// | ejected before this fix | tarball scripts |
/// | --- | --- |
/// | `gl@8.1.6`, `@google-cloud/profiler@0.0.2` | explicit `install` |
/// | `@stdlib/math-base-special-sqrt@0.0.6`, `tree-sitter-ruby@0.0.4`, `farmhash@1.2.1`, `lzo@0.1.1` | **NONE** |
///
/// A perfect split: every package that ejected declared the script, every one that did not relied
/// on the implicit build. Ten of twelve corpus records with a failing gyp ran from the machine-global
/// store because of this, and `@stdlib/math-base-special-sqrt@0.0.6` then failed
/// `MODULE_NOT_FOUND` on `@stdlib/complex-float32` — a properly DECLARED dependency — because its
/// `binding.gyp` resolves siblings with `resolve.sync`, which does not canonicalize a symlink
/// basedir the way node's own `require` does. Ejecting it builds the addon; real pnpm builds it too.
///
/// Best-effort: an unreadable or unparseable manifest reports NO script, EXCEPT that a root
/// `binding.gyp` still counts — a package can build with no readable manifest at all, and
/// over-seeding is the safe direction here (it costs store sharing; under-seeding costs a
/// silently broken install).
fn declares_install_script(store: &aube_store::Store, pkg: &LockedPackage) -> bool {
    let Some(index) = store.load_index(pkg.registry_name(), &pkg.version, pkg.integrity.as_deref())
    else {
        return false;
    };
    // npm's rule is keyed on the package ROOT, so an exact match — a nested `deps/foo/x.gyp` is a
    // build input, not a trigger. (`ships_gyp_file` is deliberately laxer: it answers a different
    // question, "can this dep emit a .target.mk", for which any `.gyp` counts.)
    let implicit_gyp_build = index.contains_key("binding.gyp");
    let manifest = index
        .get("package.json")
        .and_then(|entry| std::fs::read(&entry.store_path).ok());
    builds_at_install_time(implicit_gyp_build, manifest.as_deref())
}

/// The decision itself, split from the store I/O so it is unit-testable without a CAS — the same
/// shape [`super::nub_data_dir_from`] uses for env precedence. `manifest` is the raw
/// `package.json` bytes, `None` when the index has none or it could not be read.
fn builds_at_install_time(implicit_gyp_build: bool, manifest: Option<&[u8]>) -> bool {
    let explicit = manifest
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
        .and_then(|json| {
            json.get("scripts")
                .and_then(serde_json::Value::as_object)
                .map(|scripts| {
                    ["preinstall", "install", "postinstall"]
                        .iter()
                        .any(|k| scripts.contains_key(*k))
                })
        })
        .unwrap_or(false);
    // An unreadable or script-less manifest falls through to the implicit build rather than to
    // `false`: a package can build with no readable manifest at all, and over-seeding costs only
    // store sharing where under-seeding costs a silently broken install.
    explicit || implicit_gyp_build
}

/// Direct dependencies of a lifecycle-script package that ship a `.gyp` file, seeded
/// so they materialize project-local alongside the build that consumes them.
///
/// THE INVARIANT A ONE-SIDED EJECT BREAKS. gyp writes each dependency gyp file's
/// `.target.mk` at `depth(".") + generator_output("build") + RelativePath(dep_gyp_dir,
/// depth)`, and `build/` absorbs exactly one `..`, so the write lands one level
/// shallower than the source-tree mirror it is meant to be. nub-sandbox's
/// `store_entry_write_root` grants precisely that landing site — but its arithmetic
/// resolves to the package's own store-entry root only while the package and its gyp
/// provider sit in the SAME virtual store. The lifecycle-script seed above breaks that
/// on its own: it moves the script package project-local and leaves `node-addon-api`
/// machine-global, so the climb walks out of the project's store and lands under the
/// PROJECT ROOT, which no build-jail grant covers and none should. gyp's
/// `EnsureDirExists` is a bare `except OSError: pass`, so the denial surfaces one line
/// later as `FileNotFoundError` on a `.target.mk` and reads as a node-gyp bug.
/// Reproduced with `sharp` on macOS and Linux; npm never hits it because a hoisted
/// `../node-addon-api` escapes only as far as the consuming package's own directory.
///
/// Keyed on the DEP shipping a `.gyp`, not on the consumer shipping a `binding.gyp`:
/// only a package that supplies a gyp file can produce a `.target.mk`, so this is the
/// exact surface, and the seed stays a handful of packages rather than every
/// dependency of every hook installer.
fn gyp_provider_seeds(
    graph: &LockfileGraph,
    script_names: &HashSet<&str>,
    ships_gyp: &dyn Fn(&LockedPackage) -> bool,
) -> Vec<String> {
    use aube_lockfile::resolve_dep_edge;

    let mut seeds = Vec::new();
    let mut checked: HashSet<&str> = HashSet::new();
    for pkg in graph.packages.values() {
        if !script_names.contains(pkg.name.as_str()) {
            continue;
        }
        for (dep_name, dep_tail) in pkg
            .dependencies
            .iter()
            .chain(pkg.optional_dependencies.iter())
        {
            let Some(dep_key) =
                resolve_dep_edge(dep_name, dep_tail, |k| graph.packages.contains_key(k))
            else {
                continue;
            };
            let Some(dep_pkg) = graph.packages.get(&dep_key) else {
                continue;
            };
            // The CAS read is the expensive half, so dedupe by name before paying it.
            if !checked.insert(dep_pkg.name.as_str()) {
                continue;
            }
            if ships_gyp(dep_pkg) {
                seeds.push(dep_pkg.name.clone());
            }
        }
    }
    seeds
}

/// Whether `pkg` publishes a gyp file a dependant's `binding.gyp` could name as a
/// gyp `dependencies` entry. Read from the CAS index because the plan is computed
/// before anything is linked; an unreadable index reports none, which leaves the
/// package on the unchanged sibling-symlink path.
fn ships_gyp_file(store: &aube_store::Store, pkg: &LockedPackage) -> bool {
    store
        .load_index(pkg.registry_name(), &pkg.version, pkg.integrity.as_deref())
        .is_some_and(|index| index.keys().any(|path| path.ends_with(".gyp")))
}

/// Attach a reason to every package the plan materializes. Mirrors the planner's
/// seed conditions in the same order, then falls back to the closure edge —
/// a plan member nothing seeded directly is there because it imports one that
/// was, which is the fact worth printing.
///
/// `seed_names` is the CONFIGURED seed only and `script_seeds` the manifest-derived
/// one: [`expand`] hands the planner their union, but "named by config" and "its
/// build script reads the project" are different answers to the reader's question.
fn label_plan(
    graph: &LockfileGraph,
    seed_names: &[String],
    script_seeds: &[String],
    gyp_seeds: &[String],
    nested_seeds: &[String],
    flags: &[FlaggedImporter],
    plan: &DiskMaterializePlan,
) -> Vec<super::install_report::Materialized> {
    use super::install_report::{Materialized, Reason};

    let planned: HashSet<&str> = plan.names.iter().map(String::as_str).collect();
    let script_seeded: HashSet<&str> = script_seeds.iter().map(String::as_str).collect();
    let gyp_seeded: HashSet<&str> = gyp_seeds.iter().map(String::as_str).collect();
    let nested_seeded: HashSet<&str> = nested_seeds.iter().map(String::as_str).collect();
    let root_provided: HashSet<&str> = graph
        .importers
        .values()
        .flat_map(|deps| deps.iter().map(|d| d.name.as_str()))
        .collect();
    let is_top_level = |name: &str| root_provided.contains(name);
    let seed_matcher = aube_linker::PackageNameMatcher::new(seed_names);

    // One representative version per planned name — the digest is name-keyed
    // (so is the linker's eject), so a duplicated name needs one printable spec.
    let mut version_of: BTreeMap<&str, &str> = BTreeMap::new();
    for pkg in graph.packages.values() {
        if planned.contains(pkg.name.as_str()) {
            version_of.entry(&pkg.name).or_insert(&pkg.version);
        }
    }

    version_of
        .iter()
        .map(|(&name, &version)| {
            let flag = flags.iter().find(|flag| flag.name == name);
            let reason = if let Some(flag) = flag.filter(|flag| {
                !flag.targets.is_empty()
                    && should_seed(
                        &flag.targets,
                        &direct_dep_names(&flag.dep_path, graph),
                        is_top_level,
                    )
            }) {
                Reason::Undeclared(flag.targets.clone())
            } else if flag.is_some_and(|flag| {
                flag.type_peers
                    .iter()
                    .any(|peer| is_top_level(&types_package_name(peer)))
            }) {
                Reason::PeerTypes
            } else if script_seeded.contains(name) {
                Reason::ProjectContext
            } else if gyp_seeded.contains(name) {
                Reason::GypProvider
            } else if nested_seeded.contains(name) {
                Reason::MutatedByImporter
            } else if name == "vite" && super::vite_compat::vite_lt_8_1(version) {
                Reason::LegacyVite
            } else if seed_matcher.matches(name) {
                Reason::Configured
            } else {
                // The closure edge: whichever declared dependency of this package
                // is itself in the plan is why this one had to come along.
                graph
                    .packages
                    .values()
                    .filter(|pkg| pkg.name == name)
                    .flat_map(|pkg| pkg.dependencies.keys())
                    .filter(|dep| dep.as_str() != name)
                    .find_map(|dep| {
                        version_of
                            .get(dep.as_str())
                            .map(|dep_version| Reason::ImporterOf(format!("{dep}@{dep_version}")))
                    })
                    .unwrap_or(Reason::Closure)
            };
            Materialized {
                name: name.to_string(),
                version: version.to_string(),
                reason,
            }
        })
        .collect()
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

    // Seed set by NAME: every dynamically-flagged importer that SURVIVES the
    // precision seed-selection filter. Embedded vite<8.1 and caller-supplied
    // package-name patterns are seeded by dep_path below.
    let mut seed_names_set: HashSet<&str> = HashSet::new();

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

    // (The curated project-context eject list that used to seed here is GONE. Every package
    // declaring a lifecycle script is seeded in `expand()` from the manifest instead, which
    // covers all 21 former names by construction — they were hook installers, so each declares
    // one — and covers the ones nobody had thought to write down.)

    // Seed DEP_PATHS: every graph package whose name is a seed, plus every
    // embedded vite<8.1 copy (auto-detected — the #315 residual). Seeding by
    // dep_path keeps the reverse walk anchored to the real copies present.
    let mut seed_dep_paths: HashSet<&str> = HashSet::new();
    // Compiled once, not per comparison: this loop runs over every package in
    // the graph, and matching inline re-parsed each pattern on every one.
    let seed_matcher = aube_linker::PackageNameMatcher::new(&seed_names);
    for (dep_path, pkg) in &graph.packages {
        if seed_names_set.contains(pkg.name.as_str())
            || seed_matcher.matches(&pkg.name)
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

    // Rung-1 names: every closure member's name (the executor is name-keyed).
    // Seeding is dep_path-anchored, so a seed name with no copy in the resolved
    // graph contributes nothing — there is no package for the executor to eject.
    let mut names: HashSet<String> = HashSet::new();
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
        ..DiskMaterializePlan::default()
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

    /// npm hands any package with a root `binding.gyp` an implicit `node-gyp rebuild` unless it
    /// declares `install`/`preinstall`. Missing that under-seeded every native addon that does not
    /// spell the script out — measured as 10 of 12 failing-gyp records in the build-jail corpus.
    #[test]
    fn an_implicit_gyp_build_counts_as_an_install_time_build() {
        let no_scripts = br#"{"name":"x","scripts":{"test":"make test"}}"#;
        assert!(builds_at_install_time(true, Some(no_scripts)));
        // The control that keeps the seed narrow: no gyp, no explicit script, no eject. Without
        // this the assertion above passes just as happily for a predicate that returns `true`.
        assert!(!builds_at_install_time(false, Some(no_scripts)));
    }

    #[test]
    fn an_explicit_script_still_counts_without_any_gyp() {
        for key in ["preinstall", "install", "postinstall"] {
            let manifest = format!(r#"{{"scripts":{{"{key}":"do-thing"}}}}"#);
            assert!(
                builds_at_install_time(false, Some(manifest.as_bytes())),
                "{key} should count on its own",
            );
        }
    }

    /// A package can build with no readable manifest, so the gyp decides — but an absent manifest
    /// must not manufacture a build for a package that ships no gyp either.
    #[test]
    fn an_unreadable_manifest_falls_through_to_the_gyp() {
        assert!(builds_at_install_time(true, None));
        assert!(builds_at_install_time(true, Some(b"{ not json")));
        assert!(!builds_at_install_time(false, None));
        assert!(!builds_at_install_time(false, Some(b"{ not json")));
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

    // Project-context eject (nub#457): a build that reads/mutates the consuming project must be
    // placed project-local, or its upward walk from `cwd` never reaches the project.

    #[test]
    fn a_seeded_project_context_leaf_ejects_from_gvs() {
        // This used to assert that `simple-git-hooks` seeds itself BY NAME, from a curated list
        // `plan_from_flags` consulted directly. The list is gone: `expand()` now seeds every
        // package whose MANIFEST declares a lifecycle script, which needs the store to read
        // manifests from and so cannot be reached from a unit test with a synthetic graph.
        //
        // What `plan_from_flags` still owns is the CLOSURE — given a seed, produce the plan —
        // and that is what this pins. A git-hook leaf has no importers, so its closure is
        // itself. The SEED SOURCE is verified end-to-end instead: simple-git-hooks@2.13.1
        // installs with `.store/simple-git-hooks@2.13.1` a REAL DIRECTORY rather than a symlink
        // into the global store, and its postinstall writes a 226-byte `.git/hooks/pre-commit`.
        let g = graph(&[("simple-git-hooks@2.13.1", "simple-git-hooks", &[])]);
        let plan = plan_from_flags(&g, &["simple-git-hooks".to_string()], &[]);
        assert!(
            plan.names.iter().any(|n| n == "simple-git-hooks"),
            "a seeded project-context leaf ejects: {:?}",
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
    fn placement_policy_token_is_stable_across_calls() {
        // The token feeds the install-state fingerprint (#457 warm-tree fix). It used to hash a
        // curated 21-name list, so editing the list moved it; the seed is now DERIVED per
        // package from its manifest, so there is no list left to fingerprint. What remains must
        // be DETERMINISTIC — a warm tree that relinked on every install would be a performance
        // bug, and one that never relinked would leave stale placement.
        assert_eq!(
            project_context_eject_token(),
            project_context_eject_token(),
            "deterministic across calls"
        );
        assert!(
            !project_context_eject_token().is_empty(),
            "an empty token cannot invalidate a tree built under the previous policy"
        );
    }

    #[test]
    fn configured_seed_patterns_match_wildcards_and_literals() {
        let g = graph(&[
            ("app@1.0.0", "app", &[("is-number", "7.0.0")]),
            ("is-number@7.0.0", "is-number", &[]),
            ("left-pad@1.3.0", "left-pad", &[]),
        ]);
        for pattern in ["is-*", "is-number"] {
            let plan = plan_from_flags(&g, &[pattern.to_string()], &[]);
            let names: HashSet<&str> = plan.names.iter().map(String::as_str).collect();
            assert!(
                names.contains("is-number"),
                "pattern {pattern} seeds the package"
            );
            assert!(
                names.contains("app"),
                "pattern {pattern} expands its importer closure"
            );
            assert!(!names.contains("left-pad"));
        }
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

    /// Each conjunct of the gyp-provider seed, on one graph: the dep must ship a
    /// gyp file AND its consumer must run an install script. Both halves matter —
    /// seeding on either alone would eject most of a large graph.
    #[test]
    fn only_a_gyp_shipping_dep_of_a_script_package_is_seeded() {
        let g = graph(&[
            (
                "sharp@0.32.6",
                "sharp",
                &[("node-addon-api", "6.1.0"), ("color", "4.2.3")],
            ),
            ("node-addon-api@6.1.0", "node-addon-api", &[]),
            ("color@4.2.3", "color", &[]),
            // `nan` ships a gyp file too, but nothing that runs a build depends on it.
            ("quiet@1.0.0", "quiet", &[("nan", "2.22.2")]),
            ("nan@2.22.2", "nan", &[]),
        ]);
        let ships_gyp = |pkg: &LockedPackage| matches!(pkg.name.as_str(), "node-addon-api" | "nan");

        let seeds = gyp_provider_seeds(&g, &names_ref(&["sharp"]), &ships_gyp);

        assert_eq!(
            seeds,
            vec!["node-addon-api".to_string()],
            "expected only sharp's gyp-shipping dep; color ships no gyp and nan's \
             consumer runs no install script"
        );
    }

    fn names_ref<'a>(xs: &[&'a str]) -> HashSet<&'a str> {
        xs.iter().copied().collect()
    }
}

#[cfg(test)]
mod nested_optional_dep_tests {
    use super::*;
    use aube_lockfile::{DirectDep, LockedPackage, LockfileGraph};

    /// Build a graph where `importer` has `dep` as an ACTIVE optional edge —
    /// mirrored into `dependencies`, which is how the resolver records one.
    fn graph_with_optional(importer: &str, dep: &str, dep_is_leaf: bool) -> LockfileGraph {
        let mut g = LockfileGraph::default();
        let mut parent = LockedPackage {
            name: importer.to_string(),
            version: "1.0.0".to_string(),
            dep_path: format!("{importer}@1.0.0"),
            ..Default::default()
        };
        parent
            .dependencies
            .insert(dep.to_string(), "1.0.0".to_string());
        parent
            .optional_dependencies
            .insert(dep.to_string(), "1.0.0".to_string());
        g.packages.insert(format!("{importer}@1.0.0"), parent);

        let mut child = LockedPackage {
            name: dep.to_string(),
            version: "1.0.0".to_string(),
            dep_path: format!("{dep}@1.0.0"),
            ..Default::default()
        };
        if !dep_is_leaf {
            child
                .dependencies
                .insert("grandchild".to_string(), "1.0.0".to_string());
            g.packages.insert(
                "grandchild@1.0.0".to_string(),
                LockedPackage {
                    name: "grandchild".to_string(),
                    version: "1.0.0".to_string(),
                    dep_path: "grandchild@1.0.0".to_string(),
                    ..Default::default()
                },
            );
        }
        g.packages.insert(format!("{dep}@1.0.0"), child);

        g.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: importer.to_string(),
                dep_path: format!("{importer}@1.0.0"),
                dep_type: aube_lockfile::DepType::Production,
                specifier: None,
            }],
        );
        g
    }

    fn pairs(g: &LockfileGraph, has_script: bool) -> Vec<(String, String)> {
        nested_optional_dep_pairs(g, &move |_: &LockedPackage| has_script)
    }

    #[test]
    fn a_script_running_importer_nests_its_leaf_optional_dep() {
        let g = graph_with_optional("bun", "@oven/bun-darwin-aarch64", true);
        assert_eq!(
            pairs(&g, true),
            vec![("bun".to_string(), "@oven/bun-darwin-aarch64".to_string())]
        );
    }

    #[test]
    fn a_nested_optional_dep_is_also_ejected_project_local() {
        // Nesting adds a nearer copy; it removes no farther one. The importer's cell
        // keeps a sibling symlink into the peer's SHARED cell and the project keeps a
        // hidden-hoist alias to it, so the run after the script consumes the nest
        // walks onto shared state. Ejecting the peer is what makes both of those
        // routes land project-local. Measured on `bun@1.4.0`: without this seed the
        // second run's refusal names `~/.cache/nub/pm/store/…` and the `"no-jail"`
        // escape deletes the binary from it; with it, both name the project's own
        // `.store/…` and the shared cell is untouched.
        let g = graph_with_optional("bun", "@oven/bun-darwin-aarch64", true);
        let nested_seeds: Vec<String> = pairs(&g, true).into_iter().map(|(_, dep)| dep).collect();
        let script_seeds = vec!["bun".to_string()];

        // CONTROL FIRST — the importer's own eject must not already cover the peer.
        // The closure walks toward IMPORTERS, so it never descends into a dependency;
        // without this the assertion below could pass without the seed doing anything.
        let script_only = plan_from_flags(&g, &script_seeds, &[]);
        assert!(
            !script_only
                .names
                .contains(&"@oven/bun-darwin-aarch64".to_string()),
            "control: seeding only the importer must leave the peer in the shared store"
        );

        // ⛔ THE UNION COMES FROM `eject_seeds`, NOT FROM THIS TEST. Composing it here would
        // assert only that the planner ejects what it is handed — true however `expand`
        // seeds it, so the test would stay green if the nested source were dropped.
        // MEASURED: it did exactly that.
        let all_seeds = eject_seeds(&[], &script_seeds, &[], &nested_seeds);
        let plan = plan_from_flags(&g, &all_seeds, &[]);
        assert!(
            plan.names.contains(&"@oven/bun-darwin-aarch64".to_string()),
            "the peer a build script MOVES files out of must be ejected project-local"
        );

        let labelled = label_plan(&g, &[], &script_seeds, &[], &nested_seeds, &[], &plan);
        let peer = labelled
            .iter()
            .find(|m| m.name == "@oven/bun-darwin-aarch64")
            .expect("the peer must appear in the install report");
        assert_eq!(
            peer.reason.to_string(),
            "its importer's build script moves its files",
            "the digest has to say why the package moved, or it reads as unexplained"
        );
    }

    #[test]
    fn no_install_script_means_no_nesting() {
        // Nothing moves the resolved file, so the second realpath a nested copy
        // creates would be pure cost.
        let g = graph_with_optional("rollup", "@rollup/rollup-darwin-arm64", true);
        assert!(pairs(&g, false).is_empty());
    }

    #[test]
    fn a_non_leaf_optional_dep_is_never_nested() {
        // The nested copy carries no dependency edges, so a dep with its own
        // deps would resolve them out of the nest into the importer's sibling
        // set — a different closure than it would see un-nested.
        let g = graph_with_optional("importer", "has-own-deps", false);
        assert!(pairs(&g, true).is_empty());
    }

    #[test]
    fn a_second_route_to_the_dep_blocks_nesting() {
        // Two realpaths for one package is a double load, which for the native
        // addons this class ships means a double registration.
        let mut g = graph_with_optional("bun", "@oven/bun-darwin-aarch64", true);
        let mut other = LockedPackage {
            name: "other".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "other@1.0.0".to_string(),
            ..Default::default()
        };
        other
            .dependencies
            .insert("@oven/bun-darwin-aarch64".to_string(), "1.0.0".to_string());
        g.packages.insert("other@1.0.0".to_string(), other);
        assert!(pairs(&g, true).is_empty());
    }

    #[test]
    fn a_top_level_dep_is_never_nested() {
        // The project's own `node_modules/<dep>` symlink is the other route.
        let mut g = graph_with_optional("bun", "@oven/bun-darwin-aarch64", true);
        g.importers.get_mut(".").unwrap().push(DirectDep {
            name: "@oven/bun-darwin-aarch64".to_string(),
            dep_path: "@oven/bun-darwin-aarch64@1.0.0".to_string(),
            dep_type: aube_lockfile::DepType::Production,
            specifier: None,
        });
        assert!(pairs(&g, true).is_empty());
    }
}
