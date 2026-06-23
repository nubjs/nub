mod builder;
mod catalog;
mod direct_dep_info;
mod error;
mod local_source;
pub mod override_rule;
mod package_ext;
mod peer_context;
pub mod platform;
mod primer;
mod resolve;
mod semver_util;
mod trust;
mod types;

pub use direct_dep_info::DirectDepInfo;
pub use error::{AgeGateDetails, CatalogDetails, Error, ExoticSubdepDetails, NoMatchDetails};
pub use local_source::resolve_exec_script_path;
pub use package_ext::is_deprecation_allowed;
pub use peer_context::{
    AutoInstalledPeers, PeerContextOptions, UnmetPeer, apply_peer_contexts, detect_unmet_peers,
    hoist_auto_installed_peers, remove_auto_installed_peers,
};
pub use platform::{SupportedArchitectures, is_supported};
pub use primer::{PruneStats as PrimerPruneStats, prune_cache as prune_primer_cache};
pub use trust::{MissingTimeDetails as MissingTrustTimeDetails, TrustDowngradeDetails};
pub use trust::{TrustEvidence, TrustExcludeParseError, TrustExcludeRules};
pub use types::{
    DependencyPolicy, MinimumReleaseAge, PackageExtension, ReadPackageHook, ResolutionMode,
    ResolvedPackage, TrustPolicy,
};

pub const YARN_EXEC_WRAPPER: &str = r#"
const env = JSON.parse(process.env.AUBE_YARN_EXEC_ENV);
globalThis.execEnv = env;
for (const name of ['fs', 'path', 'child_process', 'os', 'crypto', 'url', 'util', 'stream', 'buffer']) {
  globalThis[name] = require(name);
}
(async () => {
  await import(url.pathToFileURL(process.argv[1]).href);
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
"#;

use semver_util::version_satisfies;

#[cfg(test)]
use aube_lockfile::{DirectDep, LocalSource, LockedPackage, LockfileGraph};
#[cfg(test)]
use aube_manifest::PackageJson;
#[cfg(test)]
use error::{
    RegistryErrorKind, build_age_gate, build_no_match, classify_registry_error,
    format_registry_help,
};
#[cfg(test)]
use local_source::{dep_path_for, should_block_exotic_subdep};
#[cfg(test)]
use package_ext::{
    apply_package_extensions, apply_package_extensions_to_deps, package_selector_matches,
    pick_override_spec,
};
#[cfg(test)]
use peer_context::{
    apply_dedupe_peers_to_key, contains_canonical_back_ref, dedupe_peer_suffixes,
    dedupe_peer_variants, effective_peer_suffix, is_hashed_peer_suffix,
};
#[cfg(test)]
use semver_util::{PickResult, pick_version, strip_alias_prefix};
#[cfg(test)]
use types::format_iso8601_utc;

use aube_lockfile::DepType;
use aube_registry::Packument;
use aube_registry::client::RegistryClient;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

// Re-export shared aube-util collection aliases under the original
// FxHashMap name to avoid touching every call site.
pub(crate) use aube_util::collections::FxMap as FxHashMap;
pub(crate) use aube_util::collections::FxSet as FxHashSet;

/// BFS dependency resolver.
pub struct Resolver {
    client: Arc<RegistryClient>,
    cache: FxHashMap<String, Packument>,
    /// Optional channel to stream resolved packages as they're discovered.
    resolved_tx: Option<mpsc::Sender<ResolvedPackage>>,
    /// Optional disk cache directory for packuments (with ETag revalidation).
    packument_cache_dir: Option<std::path::PathBuf>,
    /// Separate disk cache for full (non-corgi) packuments; only used
    /// when `resolution_mode` is `TimeBased` (which needs the `time:`
    /// map). Defaults to the sibling `packuments-full-v1/` directory
    /// next to `packument_cache_dir`.
    packument_full_cache_dir: Option<std::path::PathBuf>,
    /// When true (pnpm's default), a package's declared `peerDependencies`
    /// are enqueued like regular transitives and — if not already
    /// satisfied by the importer — hoisted to the importer's direct deps.
    /// When false, peers neither get auto-installed as transitives nor
    /// hoisted; unmet peers still surface as warnings via
    /// `detect_unmet_peers`, but the user is on the hook for adding them
    /// explicitly to `package.json`.
    auto_install_peers: bool,
    /// pnpm's `exclude-links-from-lockfile`. Round-tripped through the
    /// lockfile's `settings:` header; when true, the pnpm writer omits
    /// `link:` deps from the importer `dependencies:` maps so a
    /// sibling symlink change doesn't churn the lockfile. Defaults to
    /// false (pnpm's default). Does not affect resolution itself, only
    /// the `canonical.settings.exclude_links_from_lockfile` flag the
    /// writer reads.
    exclude_links_from_lockfile: bool,
    /// User-declared override for the host platform triple, used when
    /// deciding whether an optional dep's `os`/`cpu`/`libc` constraints
    /// are satisfied. Empty fields fall back to the host.
    supported_architectures: SupportedArchitectures,
    /// Raw dependency override map from the manifest (selector key →
    /// replacement spec). Round-tripped verbatim through the lockfile
    /// for drift detection; the compiled form in `override_rules` is
    /// what the resolver hot loop actually consults.
    overrides: BTreeMap<String, String>,
    /// Compiled view of `overrides`. Built by `with_overrides`.
    /// Unparseable selector keys are dropped at compile time so the
    /// matcher never has to think about them.
    override_rules: Vec<override_rule::OverrideRule>,
    /// Names listed in the root manifest's `pnpm.ignoredOptionalDependencies`.
    /// Any optional dep (root or transitive) whose name is in this set is
    /// dropped before enqueueing — the resolver never fetches or locks it.
    /// Mirrors pnpm's `createOptionalDependenciesRemover` read-package hook.
    ignored_optional_dependencies: BTreeSet<String>,
    /// pnpm's `resolution-mode` — `Highest` (default) or `TimeBased`.
    resolution_mode: ResolutionMode,
    /// Project root used to resolve `file:` / `link:` paths to the
    /// target directory. Defaults to the current working directory;
    /// callers set it via `with_project_root`.
    project_root: PathBuf,
    /// When true, resolver-time `exec:` generators are blocked the
    /// same way fetch-time execution is blocked.
    ignore_scripts: bool,
    /// pnpm v11's `minimumReleaseAge` triplet. `None` disables the
    /// supply-chain age gate entirely (matching `minimumReleaseAge: 0`).
    minimum_release_age: Option<MinimumReleaseAge>,
    /// Workspace catalog ranges. Outer key is the catalog name
    /// (`default` for the unnamed `catalog:` field in
    /// `pnpm-workspace.yaml`); inner key is the package name; value is
    /// the version range. When the resolver encounters a `catalog:` or
    /// `catalog:<name>` task range, it rewrites the task in place to
    /// the matching range *before* the override / npm-alias passes,
    /// while preserving the original `catalog:...` text in
    /// `original_specifier` so the lockfile importer keeps the
    /// reference verbatim.
    catalogs: BTreeMap<String, BTreeMap<String, String>>,
    /// Optional `readPackage` hook, invoked once per resolved package
    /// before its transitive deps are enqueued. See [`ReadPackageHook`].
    /// Wired up by `aube` when a `.pnpmfile.cjs` is detected and
    /// `--ignore-pnpmfile` was not set.
    read_package_hook: Option<Box<dyn ReadPackageHook>>,
    dependency_policy: DependencyPolicy,
    /// Advisory ranges to avoid when resolving audit fixes. The map is
    /// keyed by registry package name and values are npm semver ranges
    /// from `vulnerable_versions`. When a clean satisfying version
    /// exists, it wins over locked/sibling reuse and the normal highest
    /// pick; if not, resolution falls back to the ordinary pick so the
    /// caller can report the advisory as remaining.
    vulnerable_ranges: BTreeMap<String, Vec<String>>,
    /// Hosts for which aube performs shallow git clones, mirroring
    /// pnpm's `git-shallow-hosts`. When a git dep's URL host is in
    /// this list, the store attempts `git fetch --depth 1 origin
    /// <sha>` (falling back to a full fetch if the server refuses);
    /// otherwise it goes straight to a full fetch. Defaults to an
    /// empty list — `aube` populates it from the generated
    /// `aube_settings::resolved::git_shallow_hosts` accessor (which
    /// carries the pnpm-compat default list baked in from
    /// `settings.toml`) via [`Self::with_git_shallow_hosts`]. Library
    /// callers who construct a `Resolver` directly must set it
    /// explicitly if they want the pnpm list; keeping the list in
    /// one place (`settings.toml`) avoids drift.
    git_shallow_hosts: Vec<String>,
    /// pnpm's `peersSuffixMaxLength`. When the peer-ID suffix body on a
    /// `dep_path` (the `(name@version)(…)` portion without its outer
    /// parens) would exceed this many bytes, the post-pass replaces the
    /// whole suffix with a parenthesized short hash `(<short-hash>)` —
    /// the first 32 chars of SHA-256 of the body, matching pnpm's
    /// `createPeerDepGraphHash` lockfile format. Default 1000.
    peers_suffix_max_length: usize,
    /// pnpm's `dedupe-peer-dependents`. When true (pnpm's default),
    /// the peer-context post-pass collapses multiple dep_path variants
    /// of the same canonical package into a single entry when their
    /// peer resolutions are pairwise-equivalent. When false, every
    /// distinct ancestor scope gets its own variant — useful for
    /// debugging peer-context divergence or mimicking pnpm v6/v7
    /// behavior.
    dedupe_peer_dependents: bool,
    /// pnpm's `dedupe-peers`. When true, peer suffixes in the lockfile
    /// emit just the resolved version — `(18.2.0)` — instead of the
    /// full `(react@18.2.0)` form. Shorter dep_paths at the cost of
    /// peer-name fidelity in the snapshot. Defaults to false.
    dedupe_peers: bool,
    /// pnpm's `resolve-peers-from-workspace-root`. When true (pnpm's
    /// default), an importer's unresolved peer can be satisfied by a
    /// dependency declared in the root importer's `package.json`, even
    /// when no ancestor scope carries that dep. Common monorepo knob:
    /// the workspace root pins shared peers like `react`, and every
    /// subpackage can peer on it without hoisting the version into
    /// every sibling.
    resolve_peers_from_workspace_root: bool,
    /// pnpm's `registry-supports-time-field`. When true, the resolver
    /// trusts the abbreviated (corgi) packument to carry the `time:`
    /// map and keeps using the cheap `fetch_packument_cached` path
    /// even under time-aware resolution (`TimeBased` or
    /// `minimumReleaseAge`). Defaults to false — the same assumption
    /// pnpm and npmjs.org ship with — so the resolver falls back to
    /// the full-packument fetch to get `time:` reliably. No effect
    /// when neither time-based resolution nor `minimumReleaseAge` is
    /// active, since the abbreviated path is already the only one
    /// running.
    registry_supports_time_field: bool,
    /// Use the bundled metadata primer even when the configured
    /// registry is not npmjs.org. Intended for npm-compatible mirrors
    /// and controlled benchmarks; tarball URLs are rewritten to the
    /// active registry before cache seeding so installs still fetch
    /// package bytes from the configured source.
    force_metadata_primer: bool,
    pub(crate) packument_network_concurrency: Option<usize>,
}

pub(crate) struct ResolveTask {
    pub(crate) name: String,
    pub(crate) range: String,
    dep_type: DepType,
    is_root: bool,
    /// The parent dep_path, for wiring up transitive dep references
    parent: Option<String>,
    /// Which importer this task belongs to (e.g., "." or "packages/app")
    pub(crate) importer: String,
    /// The original specifier from package.json before any rewrites
    /// (e.g. `"npm:real-pkg@^2.0.0"` for an alias, or `"^4.17.0"` for a normal range).
    /// Only set for root deps; recorded into the lockfile for drift detection.
    pub(crate) original_specifier: Option<String>,
    /// Real registry package name for npm-alias tasks.
    ///
    /// When a task arrives with `range` like `"npm:h3@2.0.1-rc.20"`,
    /// the preprocessing loop strips the prefix and sets this field to
    /// the real package name (`"h3"`) while *keeping* `name` as the
    /// user-facing alias (`"h3-v2"`, the key the package.json used).
    /// Every identity-facing site — dep_path formation, direct-dep
    /// records, parent `dependencies` wiring, the resolved-versions
    /// dedupe map — uses `name`, so the alias survives all the way
    /// to the linker and ends up as `node_modules/<alias>/` with
    /// `LockedPackage.alias_of = Some(real_name)`. Only registry
    /// I/O (packument fetch, tarball URL derivation) consults this
    /// field.
    ///
    /// `None` for ordinary (non-aliased) tasks — `name` is already
    /// the registry name and nothing downstream needs to distinguish.
    real_name: Option<String>,
    /// Outermost-first chain of `(name, version)` ancestors above this
    /// task in the dependency graph, used by `parent>child` override
    /// selectors. Empty for root/importer deps. Each child-enqueue
    /// site is responsible for extending its parent's chain with the
    /// parent's own `(name, version)` frame.
    pub(crate) ancestors: Vec<(String, String)>,
    /// `true` when an override rewrote `range` to a `link:`/`file:`
    /// path. Override paths are anchored at the project root (where the
    /// override is declared), not at the consuming workspace package or
    /// transitive parent — same convention pnpm follows. Without this
    /// signal the local-source resolver would re-anchor `link:./libs/x`
    /// against the importer or parent dir and walk to a phantom path.
    pub(crate) range_from_override: bool,
}

impl ResolveTask {
    /// Name to use for registry operations (packument fetch, tarball
    /// URL). Returns `real_name` for aliased tasks and `name`
    /// otherwise. Every call site that talks to the registry goes
    /// through this accessor so alias handling stays localized.
    fn registry_name(&self) -> &str {
        self.real_name.as_deref().unwrap_or(&self.name)
    }

    /// Construct a root-importer task for `(name, range)` under
    /// `importer`, with the appropriate `dep_type` and no parent/ancestry.
    /// Every root-dep enqueue site uses this shape; the factory keeps
    /// the literal in one place so a new field added to `ResolveTask`
    /// lands consistently across prod/dev/optional loops.
    fn root(name: String, range: String, dep_type: DepType, importer: String) -> Self {
        let original = range.clone();
        Self {
            name,
            range,
            dep_type,
            is_root: true,
            parent: None,
            importer,
            original_specifier: Some(original),
            real_name: None,
            ancestors: Vec::new(),
            range_from_override: false,
        }
    }

    /// Construct a transitive (non-root) task discovered by walking a
    /// parent package's dependency map. Carries the parent dep_path
    /// and inherited ancestor chain for overrides.
    fn transitive(
        name: String,
        range: String,
        dep_type: DepType,
        parent: String,
        importer: String,
        ancestors: Vec<(String, String)>,
    ) -> Self {
        Self {
            name,
            range,
            dep_type,
            is_root: false,
            parent: Some(parent),
            importer,
            original_specifier: None,
            real_name: None,
            ancestors,
            range_from_override: false,
        }
    }
}

#[cfg(test)]
mod tests;
