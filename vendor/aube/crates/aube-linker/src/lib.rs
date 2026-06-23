use aube_lockfile::graph_hash::GraphHashes;
use aube_store::Store;
use std::path::PathBuf;

#[cfg(test)]
use aube_store::PackageIndex;
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::path::Path;

mod builder;
mod clonedir;
mod error;
mod hoisted;
mod link;
mod materialize;
mod patches;
mod pool;
mod sweep;
pub mod sys;

#[cfg(test)]
mod public_hoist_tests;
#[cfg(test)]
mod tests;

pub use error::Error;
pub use hoisted::HoistedPlacements;
pub use link::build_nested_link_targets;
pub(crate) use materialize::{
    invalidate_stale_index_for_package, validate_index_key, validate_package_link_name,
};
pub use patches::Patches;
pub(crate) use patches::apply_multi_file_patch;
pub use pool::default_linker_parallelism;
pub use sweep::{is_physical_importer, mkdirp, remove_dir_all_with_retry, sweep_stale_tmp_dirs};
pub(crate) use sweep::{sweep_stale_top_level_entries, try_remove_entry};
pub use sys::{
    BinShimOptions, create_bin_shim, create_dir_link, normalize_path, parse_posix_shim_target,
    remove_bin_shim, validate_bin_name, validate_bin_target,
};

/// Strategy for arranging packages under `node_modules/`.
///
/// `Isolated` is pnpm's default layout — every package lives under
/// `.aube/<dep_path>/node_modules/<name>` and the top-level
/// `node_modules/<name>` entry is a symlink into that virtual store.
/// `Hoisted` flattens the tree npm-style: packages are materialized
/// directly into `node_modules/<name>/` with conflicting versions
/// nested under the requiring parent. `Hoisted` is slower to
/// materialize and uses more disk, but matches the layout a handful
/// of legacy toolchains still expect.
/// `FromStr` is case-insensitive so settings-file and CLI inputs like
/// `Isolated` or `HOISTED` parse the same as the canonical lowercase
/// spellings. Callers that accept user input should still `trim()`
/// before parsing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, strum::EnumString)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum NodeLinker {
    #[default]
    Isolated,
    Hoisted,
}

/// Limit how far packages may be promoted in `NodeLinker::Hoisted`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum HoistingLimits {
    /// Hoist as far as possible.
    #[default]
    None,
    /// Aube plans hoisted installs per physical importer, so this is
    /// currently equivalent to `None`.
    Workspaces,
    /// Do not hoist transitives above the direct dependency that
    /// introduced them.
    Dependencies,
}

/// Links packages from the global store into a project's node_modules/.
///
/// Uses pnpm-compatible symlink layout backed by a global virtual store:
/// - Packages are materialized once in `~/.cache/aube/virtual-store/`
///   (or `$XDG_CACHE_HOME/aube/virtual-store/`)
/// - Per-project `.aube/` entries are symlinks into the global virtual store
/// - Top-level `node_modules/<name>` entries are symlinks to
///   `.aube/<dep_path>/node_modules/<name>` (matching pnpm)
/// - Transitive deps live as sibling symlinks inside `.aube/<dep_path>/node_modules/`
///   so Node's directory walk finds them when resolving from inside the package
pub struct Linker {
    virtual_store: PathBuf,
    /// Keep a handle to the global CAS so the linker can lazy-load a
    /// `PackageIndex` on demand when the install driver skipped
    /// `load_index` on the fast path but a stale symlink or missing
    /// virtual-store entry forces a (re)materialization. Without this,
    /// the optimistic no-op short-circuit in `install.rs` wouldn't be
    /// safe against graph-hash changes (e.g. patches added,
    /// `allowBuilds` entries flipped, engine version bumped).
    pub(crate) store: Store,
    use_global_virtual_store: bool,
    strategy: LinkStrategy,
    /// Per-`name@version` patch contents applied at materialize
    /// time. Empty when the project has no `pnpm.patchedDependencies`.
    pub(crate) patches: Patches,
    /// Optional content-addressed hashes for global-store subdir
    /// naming. When set, every path inside `self.virtual_store` uses
    /// `hashes.hashed_dep_path(dep_path)` as the dep's leaf name,
    /// which folds the recursive dep-graph hash (and the engine
    /// string, for packages that transitively require building) into
    /// the filesystem path. Packages with different builds can't
    /// collide in the shared store because they end up at different
    /// paths. When `None`, the linker falls back to the raw dep_path
    /// (backwards-compatible with pre-hash callers and with the
    /// per-project `.aube/` layout, which always uses dep_path).
    hashes: Option<GraphHashes>,
    /// Cap on the length of a single virtual-store directory name.
    /// Matches pnpm's `virtual-store-dir-max-length` config (default
    /// 120). Every dep_path the linker writes to disk gets routed
    /// through `dep_path_to_filename(_, this)`, which truncates and
    /// hashes names longer than this cap so peer-heavy graphs (e.g.
    /// anything pulling in the ESLint + TypeScript matrix) don't
    /// overflow Linux's 255-byte `NAME_MAX`.
    virtual_store_dir_max_length: usize,
    /// pnpm's `shamefully-hoist`: after creating the usual top-level
    /// symlinks for direct deps, walk every package in the graph and
    /// create a `node_modules/<name>` symlink for any name that
    /// isn't already claimed. Mirrors pnpm's "flat node_modules"
    /// compatibility escape hatch. First-write-wins on name clashes.
    shamefully_hoist: bool,
    /// pnpm's `public-hoist-pattern`: glob list matched against
    /// package names. Any non-local package in the graph whose name
    /// matches at least one positive pattern (and no `!`-prefixed
    /// negation) gets a top-level `node_modules/<name>` symlink in
    /// addition to the direct-dep entries. First-write-wins, so
    /// direct deps and earlier hoist passes keep priority. Empty list
    /// disables the feature entirely. Frameworks like Next.js,
    /// Storybook, and Jest rely on this to resolve transitive deps
    /// from the project root.
    public_hoist_patterns: Vec<glob::Pattern>,
    public_hoist_negations: Vec<glob::Pattern>,
    /// pnpm's `hoist`: master switch for the hidden modules directory
    /// at `node_modules/.aube/node_modules/`. When true (the default),
    /// every non-local package whose name matches `hoist_patterns`
    /// (and no `hoist_negations`) gets a symlink into that hidden
    /// directory so Node's parent-directory walk can satisfy
    /// undeclared deps in third-party packages. When false, the
    /// hidden tree is skipped entirely and any existing
    /// `.aube/node_modules/` is wiped so stale entries don't linger.
    hoist: bool,
    /// pnpm's `hoist-pattern`: glob list matched against package names
    /// for hidden-hoist promotion. Populated with `*` in `new()` so a
    /// default-constructed linker matches everything (pnpm parity).
    /// `with_hoist_pattern` replaces both positive and negative
    /// patterns in full, so passing `[]` or only-negation means
    /// "hoist nothing". Only consulted when `hoist == true`.
    hoist_patterns: Vec<glob::Pattern>,
    hoist_negations: Vec<glob::Pattern>,
    /// pnpm's `hoist-workspace-packages`: when false, workspace
    /// packages are not symlinked into the root `node_modules/`.
    /// Other workspace packages can still resolve them through the
    /// lockfile's workspace protocol, but plain `require('<ws-pkg>')`
    /// from the root stops working. Default true.
    hoist_workspace_packages: bool,
    /// pnpm's `hoistingLimits` for `node-linker=hoisted`. Isolated
    /// linking ignores this setting because hidden/public hoisting is
    /// controlled by the separate `hoist*` family above.
    pub(crate) hoisting_limits: HoistingLimits,
    /// pnpm's `dedupe-direct-deps`: when true, the linker skips
    /// creating a per-importer `node_modules/<name>` symlink for a
    /// direct dep whose root importer already declares the same
    /// package at the same resolved version. The root-level symlink
    /// still exists, so Node's parent-directory walk from inside the
    /// workspace package resolves the same copy — callers just avoid
    /// the duplicate per-importer link. Default false (pnpm parity).
    dedupe_direct_deps: bool,
    /// Active layout mode. `NodeLinker::Isolated` (default) routes
    /// through the existing `.aube/` virtual-store paths;
    /// `NodeLinker::Hoisted` dispatches to `hoisted::link_hoisted_importer`
    /// which writes real package directories flat into `node_modules/`.
    /// Mode is per-install, not per-package — switching between
    /// modes leaves the opposite layout on disk so subsequent
    /// installs in the other mode reuse what's already there (and
    /// pay the materialization cost once).
    pub(crate) node_linker: NodeLinker,
    /// pnpm's `modules-dir`: the *project-level* directory that holds
    /// the top-level `<name>` entries the user sees under the project
    /// root. Defaults to `"node_modules"`, which is also what Node.js
    /// itself expects for the walk from `<project>/src/file.js` up to
    /// the project root. The virtual-store tree under
    /// `<modules_dir>/.aube/<dep_path>/node_modules/<name>` keeps its
    /// inner `node_modules/` name literal — Node requires the exact
    /// string `node_modules` when resolving sibling deps from inside a
    /// package — so this setting only affects the *outer* directory
    /// name, matching pnpm's behavior. Users who change it are
    /// responsible for setting `NODE_PATH` (or using a custom
    /// resolver) so Node can still find their deps.
    pub(crate) modules_dir_name: String,
    /// pnpm's `virtual-store-dir`: absolute path of the per-project
    /// virtual store (what pnpm calls `node_modules/.pnpm`). `None`
    /// means "derive from `modules_dir_name` at link time":
    /// `<project_dir>/<modules_dir_name>/.<name>` (standalone aube →
    /// `.aube`), matching the default behavior every caller expected
    /// before this knob existed. When set by the install driver via
    /// `with_aube_dir_override`, it overrides that derivation — the
    /// linker writes its `.<name>/<dep_path>/` tree into the supplied
    /// path instead. The
    /// path is *absolute*; relative overrides from `.npmrc` /
    /// `pnpm-workspace.yaml` get resolved against the project dir by
    /// the caller (see
    /// `aube_cli::commands::resolve_virtual_store_dir`).
    pub(crate) aube_dir_override: Option<std::path::PathBuf>,
    /// Cap for package-level filesystem materialization/linking work.
    /// This is deliberately separate from Rayon's global thread-count
    /// environment: aube is tuning metadata/syscall pressure, not CPU
    /// parallelism. Defaults are platform-aware and can be overridden by
    /// the install driver via the `linkConcurrency` setting.
    link_concurrency: Option<usize>,
    /// pnpm's `virtual-store-only`: when true, the linker still
    /// populates `.aube/<dep_path>/node_modules/<name>` (and, in
    /// global-store mode, the shared virtual store under
    /// `~/.cache/aube/virtual-store/`), but skips the final pass that
    /// creates the top-level `node_modules/<name>` symlinks. The
    /// `shamefullyHoist` and `publicHoistPattern` hoist passes are
    /// also skipped because both target the same top-level directory.
    /// Useful for CI jobs that pre-populate a shared store without
    /// exposing the graph to Node's resolver. No-op under
    /// `NodeLinker::Hoisted` — that layout *is* a flat top-level
    /// materialization, so "only the virtual store" doesn't apply.
    virtual_store_only: bool,
}

/// Strategy for linking files from the store to node_modules.
#[derive(Debug, Clone, Copy)]
pub enum LinkStrategy {
    /// Copy-on-write clone (APFS clonefile, btrfs/xfs FICLONE). The
    /// `auto` probe selects this whenever the destination filesystem
    /// supports it (every non-Windows CoW filesystem) — a clone is
    /// ~2.5x cheaper than a hard link on node_modules' small-file
    /// profile and gives each materialized file an independent inode,
    /// so an in-place patch can't corrupt the shared store entry. Also
    /// reachable via explicit `packageImportMethod = clone` /
    /// `clone-or-copy`.
    Reflink,
    /// Hard link (ext4, NTFS)
    Hardlink,
    /// Full copy (fallback)
    Copy,
}

#[derive(Debug, Default)]
pub struct LinkStats {
    pub packages_linked: usize,
    pub packages_cached: usize,
    pub files_linked: usize,
    pub top_level_linked: usize,
    /// Populated only when the linker ran in `NodeLinker::Hoisted`
    /// mode. Maps lockfile `dep_path` → list of on-disk directories
    /// where that package was materialized (most entries have one
    /// path; name conflicts produce multiple nested copies). The
    /// install driver uses this to locate packages for bin linking
    /// and lifecycle scripts without recomputing the placement tree.
    /// `None` means "isolated layout — use the `.aube/<dep_path>`
    /// convention".
    pub hoisted_placements: Option<HoistedPlacements>,
}
