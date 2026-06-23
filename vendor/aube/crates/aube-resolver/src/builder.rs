use crate::FxHashMap;
use crate::{
    DependencyPolicy, MinimumReleaseAge, ReadPackageHook, ResolutionMode, ResolvedPackage,
    Resolver, SupportedArchitectures, override_rule,
};
use aube_registry::client::RegistryClient;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

impl Resolver {
    /// Stream-channel capacity between resolver and fetch coordinator.
    ///
    /// Was 64 (matched fetch concurrency). Bumped to 1024 because the
    /// channel is just a backpressure-bounded mpsc — its job is to
    /// absorb resolver bursts so the BFS loop never blocks on
    /// `send().await` while the fetch coordinator is mid-tarball. Each
    /// `ResolvedPackage` is ~200 bytes, so 1024 = ~200 KiB worst-case.
    /// Real install graphs sustain 5–10 in-flight packages per fetch
    /// permit, so 1024 covers ~20 000-pkg installs without backpressure
    /// while still bounding heap on a runaway producer.
    const DEFAULT_STREAM_CAPACITY: usize = 1024;

    pub fn new(client: Arc<RegistryClient>) -> Self {
        Self {
            client,
            // 1024 covers typical monorepo without rehash. 5000-pkg tail pays one grow.
            cache: FxHashMap::with_capacity_and_hasher(1024, Default::default()),
            resolved_tx: None,
            packument_cache_dir: None,
            packument_full_cache_dir: None,
            auto_install_peers: true,
            exclude_links_from_lockfile: false,
            supported_architectures: SupportedArchitectures::default(),
            overrides: BTreeMap::new(),
            override_rules: Vec::new(),
            ignored_optional_dependencies: BTreeSet::new(),
            resolution_mode: ResolutionMode::Highest,
            project_root: PathBuf::from("."),
            ignore_scripts: false,
            minimum_release_age: None,
            catalogs: BTreeMap::new(),
            read_package_hook: None,
            dependency_policy: DependencyPolicy::default(),
            vulnerable_ranges: BTreeMap::new(),
            git_shallow_hosts: Vec::new(),
            peers_suffix_max_length: 1000,
            dedupe_peer_dependents: true,
            dedupe_peers: false,
            resolve_peers_from_workspace_root: true,
            registry_supports_time_field: false,
            force_metadata_primer: false,
            packument_network_concurrency: None,
        }
    }

    /// Create a resolver that streams resolved packages through a channel.
    /// Returns `(resolver, receiver)`. The receiver yields packages as they're
    /// discovered, allowing tarball fetches to start during resolution.
    pub fn with_stream(client: Arc<RegistryClient>) -> (Self, mpsc::Receiver<ResolvedPackage>) {
        Self::with_stream_capacity(client, Self::DEFAULT_STREAM_CAPACITY)
    }

    /// Create a streaming resolver with a bounded resolved-package buffer.
    pub fn with_stream_capacity(
        client: Arc<RegistryClient>,
        capacity: usize,
    ) -> (Self, mpsc::Receiver<ResolvedPackage>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (
            Self {
                client,
                // 1024 covers typical monorepo without rehash. 5000-pkg tail pays one grow.
                cache: FxHashMap::with_capacity_and_hasher(1024, Default::default()),
                resolved_tx: Some(tx),
                packument_cache_dir: None,
                packument_full_cache_dir: None,
                auto_install_peers: true,
                exclude_links_from_lockfile: false,
                supported_architectures: SupportedArchitectures::default(),
                overrides: BTreeMap::new(),
                override_rules: Vec::new(),
                ignored_optional_dependencies: BTreeSet::new(),
                resolution_mode: ResolutionMode::Highest,
                project_root: PathBuf::from("."),
                ignore_scripts: false,
                minimum_release_age: None,
                catalogs: BTreeMap::new(),
                read_package_hook: None,
                dependency_policy: DependencyPolicy::default(),
                vulnerable_ranges: BTreeMap::new(),
                git_shallow_hosts: Vec::new(),
                peers_suffix_max_length: 1000,
                dedupe_peer_dependents: true,
                dedupe_peers: false,
                resolve_peers_from_workspace_root: true,
                registry_supports_time_field: false,
                force_metadata_primer: false,
                packument_network_concurrency: None,
            },
            rx,
        )
    }

    pub fn with_packument_network_concurrency(mut self, n: Option<usize>) -> Self {
        self.packument_network_concurrency = n.filter(|&n| n > 0);
        self
    }

    /// Enable disk-backed packument caching with ETag/Last-Modified revalidation.
    pub fn with_packument_cache(mut self, cache_dir: std::path::PathBuf) -> Self {
        self.packument_cache_dir = Some(cache_dir);
        self
    }

    /// Disk cache for full (non-corgi) packuments, used in
    /// `ResolutionMode::TimeBased` so we can read the `time:` map.
    pub fn with_packument_full_cache(mut self, cache_dir: std::path::PathBuf) -> Self {
        self.packument_full_cache_dir = Some(cache_dir);
        self
    }

    /// Set the resolution mode. Defaults to `Highest` (pnpm's classic
    /// behavior). `TimeBased` switches direct deps to lowest-satisfying
    /// and constrains transitives by a publish-date cutoff.
    pub fn with_resolution_mode(mut self, mode: ResolutionMode) -> Self {
        self.resolution_mode = mode;
        self
    }

    /// Configure pnpm v11's `minimumReleaseAge` family of settings.
    /// Pass `None` (or a config with `minutes == 0`) to disable.
    pub fn with_minimum_release_age(mut self, mra: Option<MinimumReleaseAge>) -> Self {
        self.minimum_release_age = mra.filter(|m| m.minutes > 0);
        self
    }

    /// Whether the resolver should keep the picked versions' publish
    /// times in the in-memory output graph (`graph.times`).
    ///
    /// This governs the *in-memory* map only. It is intentionally WIDER
    /// than the lockfile-`time:`-persistence gate: `minimumReleaseAge`
    /// and `trustPolicy=no-downgrade` both need the publish dates during
    /// the resolve, and the embedder's `defaultTrust` floor reads
    /// `graph.times` to enforce its cooling-window gate against
    /// allowlisted native packages. Dropping the times here (as a narrow
    /// `TimeBased`-only gate would) leaves `graph.times` empty under the
    /// default `resolution-mode=highest` + `minimumReleaseAge=1440`
    /// install, which fails the floor closed.
    ///
    /// pnpm writes `time:` to the *lockfile* only under
    /// `resolution-mode=time-based` — it enforces `minimumReleaseAge` /
    /// `trustPolicy` from a separate on-disk metadata cache, so its
    /// lockfiles stay `time:`-free even with both policies active. aube
    /// mirrors that, but the gate lives at the WRITE site (the
    /// `persist_times` flag threaded into the lockfile writers), NOT
    /// here: the in-memory times are always available to in-process
    /// consumers, and the writer independently decides whether to
    /// serialize a `time:` block.
    pub(crate) fn should_keep_in_memory_times(&self) -> bool {
        self.resolution_mode == ResolutionMode::TimeBased
            || self.minimum_release_age.is_some()
            || self.dependency_policy.trust_policy == crate::TrustPolicy::NoDowngrade
    }

    /// Override the default `auto-install-peers=true` behavior. pnpm reads
    /// this from `.npmrc` or `pnpm-workspace.yaml`; aube's install command
    /// plumbs the resolved value through here before running resolution.
    pub fn with_auto_install_peers(mut self, auto_install_peers: bool) -> Self {
        self.auto_install_peers = auto_install_peers;
        self
    }

    /// Configure pnpm's `peersSuffixMaxLength`. When the peer suffix body
    /// on a `dep_path` would exceed this many bytes, the post-pass
    /// replaces the whole suffix with a parenthesized short hash
    /// `(<short-hash>)` (pnpm's `createPeerDepGraphHash`). Default 1000
    /// (pnpm's default).
    pub fn with_peers_suffix_max_length(mut self, max_length: usize) -> Self {
        self.peers_suffix_max_length = max_length;
        self
    }

    /// Override the default `dedupe-peer-dependents=true` behavior. When
    /// false, the peer-context pass keeps every distinct ancestor-scope
    /// variant of a package instead of collapsing peer-equivalent ones
    /// into a single dep_path. Plumbed from `.npmrc` /
    /// `pnpm-workspace.yaml` via the install command.
    pub fn with_dedupe_peer_dependents(mut self, value: bool) -> Self {
        self.dedupe_peer_dependents = value;
        self
    }

    /// Override the default `dedupe-peers=false` behavior. When true,
    /// peer suffixes in the lockfile drop the peer name and emit only
    /// the resolved version — `(18.2.0)` instead of `(react@18.2.0)`.
    /// Plumbed from `.npmrc` / `pnpm-workspace.yaml` via the install
    /// command.
    pub fn with_dedupe_peers(mut self, value: bool) -> Self {
        self.dedupe_peers = value;
        self
    }

    /// Override the default `resolve-peers-from-workspace-root=true`
    /// behavior. When false, peer resolution stops at the importer's
    /// own scope + BFS-auto-installed transitives instead of consulting
    /// the workspace root's direct deps as a fallback tier. Plumbed
    /// from `.npmrc` / `pnpm-workspace.yaml` via the install command.
    pub fn with_resolve_peers_from_workspace_root(mut self, value: bool) -> Self {
        self.resolve_peers_from_workspace_root = value;
        self
    }

    /// Configure pnpm's `registry-supports-time-field`. When true,
    /// the resolver keeps using the abbreviated (corgi) packument
    /// path even when `time:` is needed, saving one full-packument
    /// fetch per distinct package. Safe for registries that embed
    /// `time` in their abbreviated responses (Verdaccio 5.15.1+, JSR,
    /// most in-house mirrors); leave at the default `false` for
    /// npmjs.org.
    pub fn with_registry_supports_time_field(mut self, value: bool) -> Self {
        self.registry_supports_time_field = value;
        self
    }

    /// Force the bundled metadata primer on for npm-compatible
    /// mirrors. Normally the primer only seeds npmjs.org cache entries
    /// because it was generated from npmjs metadata.
    pub fn with_force_metadata_primer(mut self, value: bool) -> Self {
        self.force_metadata_primer = value;
        self
    }

    /// Configure pnpm's `exclude-links-from-lockfile` setting. Only
    /// affects lockfile serialization — the resolver still builds the
    /// same graph either way, but the value is stamped into
    /// `LockfileGraph::settings` so the pnpm writer can filter `link:`
    /// importer entries on write.
    pub fn with_exclude_links_from_lockfile(mut self, value: bool) -> Self {
        self.exclude_links_from_lockfile = value;
        self
    }

    /// Override the host platform triple used when filtering optional
    /// dependencies. See [`platform::SupportedArchitectures`].
    pub fn with_supported_architectures(mut self, value: SupportedArchitectures) -> Self {
        self.supported_architectures = value;
        self
    }

    /// Provide dependency overrides. The map's keys are selector
    /// strings — bare name, `parent>child`, `foo@<2`, `**/foo`, or any
    /// combination thereof — and values are version specifiers (or
    /// `npm:` aliases). Keys are compiled into `override_rule`
    /// structures; unparseable keys are dropped. Whenever the resolver
    /// encounters a task matching a rule (by name + ancestor chain +
    /// optional version constraints), the requested range is replaced
    /// with the rule's replacement before any packument fetch or
    /// version pick. Workspace + manifest sources are merged by the
    /// caller.
    pub fn with_overrides(mut self, overrides: BTreeMap<String, String>) -> Self {
        self.override_rules = override_rule::compile(&overrides);
        self.overrides = overrides;
        self
    }

    /// Provide workspace catalog ranges. Outer key is the catalog name
    /// (`default` for the unnamed `catalog:` field in
    /// `pnpm-workspace.yaml`); inner key is the package name. The
    /// resolver rewrites `catalog:` and `catalog:<name>` task ranges
    /// against this map before the override / npm-alias passes, and
    /// records the picks in the output graph's `catalogs` field.
    pub fn with_catalogs(mut self, catalogs: BTreeMap<String, BTreeMap<String, String>>) -> Self {
        self.catalogs = catalogs;
        self
    }

    /// Set the project root used to resolve `file:` / `link:` paths.
    /// `file:./vendor/foo` resolves against this directory, and a
    /// matching directory / tarball is read to drive resolution of the
    /// local package's transitive deps.
    pub fn with_project_root(mut self, project_root: PathBuf) -> Self {
        self.project_root = project_root;
        self
    }

    pub fn with_ignore_scripts(mut self, ignore_scripts: bool) -> Self {
        self.ignore_scripts = ignore_scripts;
        self
    }

    /// Names to strip from every `optionalDependencies` map before
    /// enqueueing (pnpm's `pnpm.ignoredOptionalDependencies`). Applied
    /// to both root and transitive optional deps. Empty by default.
    pub fn with_ignored_optional_dependencies(mut self, ignored: BTreeSet<String>) -> Self {
        self.ignored_optional_dependencies = ignored;
        self
    }

    /// Install a `readPackage` hook. The resolver calls it once per
    /// version-picked packument before enqueueing transitives; see
    /// [`ReadPackageHook`] for what mutations are honored.
    pub fn with_read_package_hook(mut self, hook: Box<dyn ReadPackageHook>) -> Self {
        self.read_package_hook = Some(hook);
        self
    }

    /// Configure dependency resolution policy settings such as
    /// `packageExtensions`, `allowedDeprecatedVersions`, `trustPolicy*`,
    /// and `blockExoticSubdeps`.
    pub fn with_dependency_policy(mut self, policy: DependencyPolicy) -> Self {
        self.dependency_policy = policy;
        self
    }

    /// Prefer non-vulnerable versions for the supplied audit ranges.
    /// Used by `audit --fix=update` to reuse the normal resolver while
    /// steering only vulnerable packages away from affected versions.
    pub fn with_vulnerable_ranges(mut self, ranges: BTreeMap<String, Vec<String>>) -> Self {
        self.vulnerable_ranges = ranges;
        self
    }

    /// Set the `git-shallow-hosts` list used when cloning git deps.
    /// When a git URL's host matches an entry here (exact match,
    /// same as pnpm), aube attempts a shallow fetch by SHA; other
    /// hosts get a plain `git fetch origin`. An empty list forces
    /// every git dep through the full-fetch path.
    pub fn with_git_shallow_hosts(mut self, hosts: Vec<String>) -> Self {
        self.git_shallow_hosts = hosts;
        self
    }
}
