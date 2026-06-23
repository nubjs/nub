use aube_lockfile::dep_path_filename::{
    DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH, dep_path_to_filename,
};
use aube_lockfile::graph_hash::GraphHashes;
use aube_store::Store;
use std::path::{Path, PathBuf};

use crate::{
    HoistingLimits, LinkStrategy, Linker, NodeLinker, Patches, default_linker_parallelism,
};

impl Linker {
    pub fn new(store: &Store, strategy: LinkStrategy) -> Self {
        Self::new_with_gvs(store, strategy, !aube_util::env::is_ci())
    }

    pub(crate) fn new_with_gvs(
        store: &Store,
        strategy: LinkStrategy,
        use_global_virtual_store: bool,
    ) -> Self {
        Self {
            virtual_store: store.virtual_store_dir(),
            store: store.clone(),
            use_global_virtual_store,
            strategy,
            patches: Patches::new(),
            hashes: None,
            virtual_store_dir_max_length: DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
            shamefully_hoist: false,
            public_hoist_patterns: Vec::new(),
            public_hoist_negations: Vec::new(),
            hoist: true,
            hoist_patterns: vec![glob::Pattern::new("*").expect("'*' is a valid glob pattern")],
            hoist_negations: Vec::new(),
            hoist_workspace_packages: true,
            hoisting_limits: HoistingLimits::None,
            dedupe_direct_deps: false,
            node_linker: NodeLinker::Isolated,
            link_concurrency: None,
            virtual_store_only: false,
            modules_dir_name: "node_modules".to_string(),
            aube_dir_override: None,
        }
    }

    /// Select the layout mode. Defaults to `NodeLinker::Isolated`
    /// (pnpm's `.aube/`-backed virtual-store layout); `Hoisted`
    /// dispatches `link_all` / `link_workspace` to the flat
    /// node_modules materializer in `crate::hoisted`.
    pub fn with_node_linker(mut self, node_linker: NodeLinker) -> Self {
        self.node_linker = node_linker;
        self
    }

    /// Current layout mode. The install driver reads this after
    /// linking to decide how to resolve per-package directories for
    /// bin linking and lifecycle scripts — isolated uses the
    /// `.aube/<dep_path>` convention, hoisted consults the
    /// `HoistedPlacements` returned on `LinkStats`.
    pub fn node_linker(&self) -> NodeLinker {
        self.node_linker
    }

    /// Override the name of the project-level `node_modules` directory
    /// (pnpm's `modules-dir` setting). Empty strings are coerced back
    /// to the default so a `.npmrc` typo can't make the linker write
    /// into the project root itself. The setting only affects the
    /// outer directory name — the inner virtual-store layout still
    /// uses the literal `node_modules` that Node's resolver expects
    /// when walking up from inside a package.
    pub fn with_modules_dir_name(mut self, name: impl Into<String>) -> Self {
        let s = name.into();
        self.modules_dir_name = if s.trim().is_empty() {
            "node_modules".to_string()
        } else {
            s
        };
        self
    }

    /// Project-level modules directory name. `aube` reads this
    /// when it needs the same path the linker writes into — keeping
    /// the computation DRY with whatever the linker was built with.
    pub fn modules_dir_name(&self) -> &str {
        &self.modules_dir_name
    }

    /// Override the per-project virtual-store path (pnpm's
    /// `virtualStoreDir`). The supplied path should be *absolute* —
    /// `aube` resolves relative `.npmrc` / `pnpm-workspace.yaml`
    /// values against the project dir before handing them here.
    /// When not set, the linker derives the virtual store path as
    /// `<project_dir>/<modules_dir_name>/.aube` at link time, which
    /// matches the historical behavior.
    pub fn with_aube_dir_override(mut self, path: PathBuf) -> Self {
        self.aube_dir_override = Some(path);
        self
    }

    /// Compute the effective virtual-store path for `project_dir`.
    /// Consults the override installed by `with_aube_dir_override` if
    /// any; otherwise falls back to `<project_dir>/<modules_dir>/.<name>`.
    /// Used internally by `link_all`; also called by the install
    /// driver's "already linked" fast path so both sites land on the
    /// same directory when the user has overridden `virtualStoreDir`.
    pub fn aube_dir_for(&self, project_dir: &Path) -> PathBuf {
        self.aube_dir_override.clone().unwrap_or_else(|| {
            // Virtual-store leaf from the active embedder's name: `.<name>`.
            // Standalone aube → `.aube`.
            let leaf = format!(".{}", aube_util::embedder().name);
            project_dir.join(&self.modules_dir_name).join(leaf)
        })
    }

    /// Override the package-level linker worker count. Values below 1
    /// are ignored by the install driver before they reach this point.
    pub fn with_link_concurrency(mut self, concurrency: Option<usize>) -> Self {
        self.link_concurrency = concurrency;
        self
    }

    /// Override the global-virtual-store toggle set by `Linker::new`
    /// (which looks at `CI`). Callers use this to force per-project
    /// materialization when they've detected a consumer that breaks on
    /// directory symlinks escaping the project root — e.g. Next.js /
    /// Turbopack, which canonicalizes `node_modules/<pkg>` and rejects
    /// anything that lands outside its declared filesystem root.
    pub fn with_use_global_virtual_store(mut self, enabled: bool) -> Self {
        self.use_global_virtual_store = enabled;
        self
    }

    pub(crate) fn link_parallelism(&self) -> usize {
        self.link_concurrency
            .unwrap_or_else(default_linker_parallelism)
            .max(1)
    }

    /// Enable pnpm's `shamefully-hoist` mode. When true, every package
    /// in the graph gets a top-level `node_modules/<name>` symlink in
    /// addition to the direct-dep entries, producing npm's flat
    /// layout at the cost of phantom-dep correctness. First-write-wins
    /// on duplicate names, so root deps always take precedence.
    pub fn with_shamefully_hoist(mut self, shamefully_hoist: bool) -> Self {
        self.shamefully_hoist = shamefully_hoist;
        self
    }

    /// Configure pnpm's `public-hoist-pattern`. Each input is a glob
    /// matched against package names; a leading `!` flips it into a
    /// negation. After the usual direct-dep symlinks, every non-local
    /// package whose name matches at least one positive pattern and
    /// no negation gets a top-level `node_modules/<name>` symlink.
    /// Invalid patterns are silently dropped (same tolerance as
    /// pnpm), so a typo in `.npmrc` degrades to "not hoisted" instead
    /// of failing the install.
    pub fn with_public_hoist_pattern(mut self, patterns: &[String]) -> Self {
        push_glob_patterns(
            patterns,
            &mut self.public_hoist_patterns,
            &mut self.public_hoist_negations,
        );
        self
    }

    /// Toggle pnpm's `hoist` setting. When true (the default), the
    /// hidden modules tree at `node_modules/.aube/node_modules/` is
    /// populated via `with_hoist_pattern`. When false, that tree is
    /// skipped and any existing directory is swept so stale symlinks
    /// from a previous `hoist=true` run don't keep resolving.
    pub fn with_hoist(mut self, hoist: bool) -> Self {
        self.hoist = hoist;
        self
    }

    /// Configure pnpm's `hoist-pattern`. Each input is a glob matched
    /// against package names; a leading `!` flips it into a negation.
    /// Every non-local package in the graph whose name matches at
    /// least one positive pattern (and no negation) gets a
    /// `node_modules/.aube/node_modules/<name>` symlink — the hidden
    /// fallback dir for Node's parent-directory walk. Invalid
    /// patterns are silently dropped (pnpm parity). Supplying an
    /// empty list or only-negation list means "hoist nothing";
    /// leaving this unconfigured keeps the default `*` match.
    pub fn with_hoist_pattern(mut self, patterns: &[String]) -> Self {
        self.hoist_patterns.clear();
        self.hoist_negations.clear();
        push_glob_patterns(
            patterns,
            &mut self.hoist_patterns,
            &mut self.hoist_negations,
        );
        self
    }

    /// Toggle pnpm's `hoist-workspace-packages`. When false, the
    /// linker skips creating `node_modules/<ws-pkg>` symlinks for
    /// workspace packages in every importer, including the root.
    /// Cross-importer `workspace:` deps already resolve through the
    /// lockfile, so only direct `require('<ws-pkg>')` from a package
    /// that doesn't declare it stops working. Default true (pnpm
    /// parity).
    pub fn with_hoist_workspace_packages(mut self, on: bool) -> Self {
        self.hoist_workspace_packages = on;
        self
    }

    /// Configure pnpm's `hoistingLimits` for `node-linker=hoisted`.
    /// No-op for the default isolated linker.
    pub fn with_hoisting_limits(mut self, limits: HoistingLimits) -> Self {
        self.hoisting_limits = limits;
        self
    }

    /// Toggle pnpm's `dedupe-direct-deps`. When true, the linker
    /// skips creating a per-importer `node_modules/<name>` symlink for
    /// any direct dep whose root importer already declares the same
    /// package at the same resolved version — Node's parent-directory
    /// walk from inside the workspace package still resolves the same
    /// copy via the root-level symlink, so consumer code is
    /// unaffected. Default false (pnpm parity). No-op under
    /// `virtualStoreOnly=true` (no per-importer symlink pass runs)
    /// and under `NodeLinker::Hoisted` (each importer gets an
    /// independent flat tree — no shared root to dedupe against).
    pub fn with_dedupe_direct_deps(mut self, on: bool) -> Self {
        self.dedupe_direct_deps = on;
        self
    }

    /// Whether `pkg_name` should be symlinked into the hidden hoist
    /// tree. Returns false when `hoist == false` regardless of
    /// patterns, or when no positive pattern matches. Matching is
    /// case-insensitive, matching pnpm.
    pub(crate) fn hoist_matches(&self, pkg_name: &str) -> bool {
        self.hoist && matches_with_negations(pkg_name, &self.hoist_patterns, &self.hoist_negations)
    }

    /// Whether `pkg_name` should be promoted to the root
    /// `node_modules` under the configured `public-hoist-pattern`.
    /// Names with no positive match are rejected; a name that
    /// matches a positive pattern is still rejected if any negation
    /// also matches. Matching is case-insensitive.
    pub(crate) fn public_hoist_matches(&self, pkg_name: &str) -> bool {
        matches_with_negations(
            pkg_name,
            &self.public_hoist_patterns,
            &self.public_hoist_negations,
        )
    }

    /// Override the virtual-store directory name length cap. Primarily
    /// a hook for tests and for parity with pnpm's
    /// `virtual-store-dir-max-length` config; most callers should
    /// leave it at the default.
    pub fn with_virtual_store_dir_max_length(mut self, max_length: usize) -> Self {
        self.virtual_store_dir_max_length = max_length;
        self
    }

    /// Toggle pnpm's `virtual-store-only`. When enabled, `link_all` /
    /// `link_workspace` still populate `.aube/<dep_path>/node_modules`
    /// (and the shared global virtual store under
    /// `~/.cache/aube/virtual-store/`) but skip the pass that writes
    /// top-level `node_modules/<name>` symlinks and the hoisting
    /// passes that target the same directory. No-op under
    /// `NodeLinker::Hoisted` — that layout is inherently a flat
    /// top-level materialization.
    pub fn with_virtual_store_only(mut self, only: bool) -> Self {
        self.virtual_store_only = only;
        self
    }

    /// Whether this linker will skip the top-level `node_modules/<name>`
    /// symlink pass. Exposed so the install driver can omit root-level
    /// bin linking and lifecycle-script invocations when the user has
    /// asked for a virtual-store-only install — both operate on the
    /// top-level tree that won't exist.
    pub fn virtual_store_only(&self) -> bool {
        self.virtual_store_only
    }

    /// Install a set of pre-computed graph hashes. Every virtual-store
    /// path the linker constructs after this point will use the
    /// hashed subdir name for the matching `dep_path`. Callers
    /// normally derive the hashes once per install via
    /// `aube_lockfile::graph_hash::compute_graph_hashes` and pass the
    /// result in here.
    pub fn with_graph_hashes(mut self, hashes: GraphHashes) -> Self {
        self.hashes = Some(hashes);
        self
    }

    /// Directory name for `dep_path` inside the global virtual store.
    /// Applies the graph hash (if any) to fold in build state, then
    /// runs the result through `dep_path_to_filename` so the final
    /// name is both filesystem-safe and bounded.
    pub(crate) fn virtual_store_subdir(&self, dep_path: &str) -> String {
        let hashed = match &self.hashes {
            Some(h) => h.hashed_dep_path(dep_path),
            None => dep_path.to_string(),
        };
        dep_path_to_filename(&hashed, self.virtual_store_dir_max_length)
    }

    /// Directory name for `dep_path` inside a project's local
    /// `node_modules/.aube/`. Same filename-bounding as the global
    /// store, but without the graph-hash fold — local `.aube/` is
    /// keyed by dep_path alone because node's resolver walks by
    /// dep_path and never inspects the shared-store identity.
    pub(crate) fn aube_dir_entry_name(&self, dep_path: &str) -> String {
        dep_path_to_filename(dep_path, self.virtual_store_dir_max_length)
    }

    /// Whether this linker populates the project's `.aube/` entries as
    /// symlinks into the shared virtual store (true) or materializes a
    /// per-project copy (false). Callers that want to mutate package
    /// directories after linking — e.g. running allowBuilds lifecycle
    /// scripts — need to know because shared-store writes leak across
    /// projects.
    pub fn uses_global_virtual_store(&self) -> bool {
        self.use_global_virtual_store
    }

    /// Install a set of patch contents to apply at materialize time.
    /// Replaces any previously installed patches. Pair with
    /// `with_graph_hashes` whose `patch_hash` callback returns the same
    /// per-`(name, version)` digest, so the patched bytes land at a
    /// distinct virtual-store path from the unpatched ones.
    pub fn with_patches(mut self, patches: Patches) -> Self {
        self.patches = patches;
        self
    }
}

fn push_glob_patterns(
    raw: &[String],
    positives: &mut Vec<glob::Pattern>,
    negations: &mut Vec<glob::Pattern>,
) {
    for r in raw {
        let (neg, body) = match r.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, r.as_str()),
        };
        let Ok(pat) = glob::Pattern::new(body) else {
            continue;
        };
        if neg {
            negations.push(pat);
        } else {
            positives.push(pat);
        }
    }
}

fn matches_with_negations(
    name: &str,
    positives: &[glob::Pattern],
    negations: &[glob::Pattern],
) -> bool {
    if positives.is_empty() {
        return false;
    }
    let opts = glob::MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    positives.iter().any(|p| p.matches_with(name, opts))
        && !negations.iter().any(|p| p.matches_with(name, opts))
}
