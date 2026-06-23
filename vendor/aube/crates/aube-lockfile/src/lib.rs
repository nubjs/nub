pub mod bun;
pub mod dep_path_filename;
mod detect;
mod drift;
pub mod graph_hash;
mod io;
pub mod merge;
pub mod npm;
mod override_match;
pub mod pnpm;
mod source;
pub mod yarn;

pub use detect::{
    DeclarationSource, DeclaredPackageManager, ResolvedLockfileKind, declared_package_manager,
    resolve_project_lockfile_kind,
};
pub use drift::DriftStatus;
pub use io::{
    Error, LockfileKind, active_lockfile_has_conflict_markers, aube_lock_filename,
    build_canonical_map, detect_existing_lockfile_kind, parse_for_import, parse_json,
    parse_lockfile, parse_lockfile_with_kind, pnpm_lock_filename, read_lockfile, write_lockfile,
    write_lockfile_as, write_lockfile_preserving_existing,
};
pub(crate) use io::{atomic_write_lockfile, current_git_branch};
pub use merge::{MergeReport, merge_branch_lockfiles};
pub(crate) use source::normalize_git_fragment;
pub use source::{
    GitSource, HostedGit, HostedGitHost, LocalSource, RemoteTarballSource, git_commits_match,
    parse_git_spec, parse_hosted_git, resolve_dep_edge, shared_local_dep_path,
};

use smallvec::SmallVec;
use std::collections::{BTreeMap, BTreeSet};

/// Most npm packages declare zero or one entry in `os`, `cpu`,
/// `libc`. Two inline `SmallVec` slots cover empty on construction
/// (zero heap alloc) and one-entry push (still zero heap) for ~99%
/// of lockfile entries.
pub type PlatformList = SmallVec<[String; 2]>;

/// Represents a resolved dependency graph from any lockfile format.
#[derive(Debug, Clone, Default)]
pub struct LockfileGraph {
    /// Direct dependencies of the root project (and workspace packages).
    /// Key: importer path (e.g., "." for root), Value: list of (name, version) pairs.
    pub importers: BTreeMap<String, Vec<DirectDep>>,
    /// All resolved packages.
    pub packages: BTreeMap<String, LockedPackage>,
    /// Per-graph settings that round-trip through the lockfile header
    /// (pnpm v9's `settings:` block). Don't affect graph structure;
    /// stamped into the YAML when writing and read back when parsing,
    /// so subsequent installs see the same resolution-mode state.
    pub settings: LockfileSettings,
    /// Dependency overrides recorded in pnpm-lock.yaml's top-level
    /// `overrides:` block. Map of raw selector key → version specifier
    /// (or `npm:` alias). Keys are the user's verbatim selector
    /// strings — bare name, `foo>bar`, `foo@<2`, `**/foo`, or any
    /// combination. Round-tripped so subsequent installs can detect
    /// override drift on a string-compare of the key+value without
    /// re-running the resolver. The resolver parses these into
    /// `override_rule::OverrideRule`s at the start of each resolve
    /// pass.
    pub overrides: BTreeMap<String, String>,
    /// pnpm's top-level `packageExtensionsChecksum:` — a `sha256-`
    /// prefixed `object-hash` of the effective `packageExtensions`
    /// config. Lets pnpm detect that the extensions changed (and the
    /// graph must be re-resolved) without re-reading every manifest.
    /// `None` when there are no package extensions (pnpm omits the
    /// field). Only the pnpm reader/writer touches this; other formats
    /// leave it `None`. Computed via
    /// [`pnpm::package_extensions_checksum`].
    pub package_extensions_checksum: Option<String>,
    /// pnpm's top-level `pnpmfileChecksum:` — a `sha256-` prefixed hash
    /// of the local pnpmfile contents (CRLF-normalized). Lets pnpm
    /// detect that a `.pnpmfile.cjs`/`.mjs` hook changed without
    /// re-running it. `None` when no local pnpmfile participates (pnpm
    /// omits the field). pnpm-only, like `package_extensions_checksum`.
    /// Computed via [`pnpm::pnpmfile_checksum`].
    pub pnpmfile_checksum: Option<String>,
    /// Names listed in the root manifest's `pnpm.ignoredOptionalDependencies`.
    /// The resolver drops entries in this set from every `optionalDependencies`
    /// map before enqueueing, matching pnpm's read-package hook. Round-tripped
    /// through pnpm-lock.yaml's top-level `ignoredOptionalDependencies:` list
    /// so drift detection can notice when the user edits the field.
    pub ignored_optional_dependencies: BTreeSet<String>,
    /// Per-package publish timestamps, keyed by canonical `name@version`
    /// (no peer suffix). Round-trips through pnpm-lock.yaml's top-level
    /// `time:` block so `--resolution-mode=time-based` can compute a
    /// `publishedBy` cutoff from packages already in the lockfile
    /// without re-fetching packuments.
    pub times: BTreeMap<String, String>,
    /// Optional dependencies the resolver intentionally skipped on the
    /// platform that wrote this lockfile (either filtered by
    /// `os`/`cpu`/`libc`, or named in
    /// `pnpm.ignoredOptionalDependencies`). Keyed by importer path,
    /// inner map is name → specifier captured from `package.json` at
    /// resolve time.
    ///
    /// Drift detection uses this to distinguish "user just added a new
    /// optional dep" (which is real drift) from "this optional was
    /// already considered and consciously dropped on this platform"
    /// (which is *not* drift). Without it, every `--frozen-lockfile`
    /// install on a platform that skipped a fixture would hard-fail.
    pub skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>>,
    /// Resolved catalog entries, mirroring pnpm v9's top-level
    /// `catalogs:` block. Outer key is the catalog name (`default` for
    /// the unnamed `catalog:` field in `pnpm-workspace.yaml`); inner key
    /// is the package name. Each entry pairs the original specifier
    /// from the workspace catalog with the version the resolver chose
    /// for it. Round-tripped through the lockfile so drift detection
    /// can fire when a catalog spec changes without re-resolving.
    pub catalogs: BTreeMap<String, BTreeMap<String, CatalogEntry>>,
    /// bun's top-level `configVersion` — a second format counter bun
    /// added alongside `lockfileVersion` to track its own config-
    /// schema changes. Only the bun parser/writer ever touches this;
    /// other formats leave it `None`. Round-tripping the parsed
    /// value keeps the writer from silently downgrading the field
    /// (e.g. from `2` back to `1`) when bun bumps it in a future
    /// release.
    pub bun_config_version: Option<u32>,
    /// Top-level `patchedDependencies:` block mirrored by bun 1.1+ and
    /// pnpm 9+. Key: selector (`lodash@4.17.21`), value: relative patch
    /// file path (`patches/lodash@4.17.21.patch`). Round-tripped
    /// verbatim so a parse/write cycle doesn't silently drop user
    /// patches from the lockfile.
    pub patched_dependencies: BTreeMap<String, String>,
    /// Sidecar of [`Self::patched_dependencies`]: the sha256 hex of
    /// each patch file's contents, keyed by the same selector. pnpm 10
    /// records it as `patchedDependencies.<selector>.hash` and stamps
    /// it into `(patch_hash=<hash>)` dep-path suffixes; a lockfile
    /// that names a patch without the hash plumbing is rejected with
    /// `ERR_PNPM_LOCKFILE_CONFIG_MISMATCH`. Entries may be missing for
    /// selectors parsed from hash-less sources (bun.lock, pnpm v8's
    /// bare-path form) — the pnpm writer falls back to the bare-path
    /// form for those instead of inventing a hash.
    pub patched_dependency_hashes: BTreeMap<String, String>,
    /// Top-level `trustedDependencies:` block (bun) — a package-name
    /// allowlist for lifecycle script execution. Preserved so
    /// re-emitting a bun.lock doesn't strip the allowlist and cause
    /// subsequent installs to skip scripts the user explicitly
    /// approved.
    ///
    /// Kept as a `Vec` (not a set) so bun's original order round-trips
    /// byte-identically; bun emits the list in insertion order. The
    /// parser is responsible for deduping if the source lockfile
    /// carried a duplicate.
    pub trusted_dependencies: Vec<String>,
    /// Pinned runtimes (pnpm 10.14+ `devEngines.runtime` recording),
    /// keyed by runtime name (`node`). pnpm models a pinned runtime as
    /// a synthetic importer dep whose specifier/version carry a
    /// `runtime:` prefix plus a `packages:` entry keyed
    /// `<name>@runtime:<version>` holding a `variations` resolution
    /// with one downloadable artifact per platform. aube lifts that
    /// encoding into this typed map on parse and re-emits the pnpm
    /// shape on write (aube-lock.yaml and pnpm-lock.yaml share the
    /// writer). Foreign formats (npm/yarn/bun) have no runtime shape:
    /// their parsers leave this empty and their writers skip it.
    pub runtimes: BTreeMap<String, RuntimePin>,
    /// Top-level lockfile fields that aren't explicitly modeled on
    /// `LockfileGraph`. Populated by per-format parsers on best-effort
    /// basis so the writer can re-emit blocks a future lockfile
    /// version might add (or ones we haven't promoted to typed fields
    /// yet) without silently stripping them on round-trip. Each
    /// parser/writer is responsible for emitting values in its
    /// format's native serialization.
    pub extra_fields: BTreeMap<String, serde_json::Value>,
    /// Per-workspace-importer extras keyed by importer path (`""` for
    /// root in bun, `"."` for others). Stores anything in the
    /// workspace entry the typed model doesn't capture so a parse/
    /// write cycle doesn't drop fields the user (or bun) wrote there.
    pub workspace_extra_fields: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

/// One entry in a lockfile catalog: the workspace-declared range and the
/// resolved version. Mirrors pnpm v9's `catalogs:` block exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub specifier: String,
    pub version: String,
}

/// A pinned runtime (Node.js) recorded in the lockfile. Mirrors pnpm
/// 10.14+'s `devEngines.runtime` encoding: the manifest's requested
/// range plus the exact resolved version, and one downloadable
/// artifact per supported platform so any machine reading the
/// lockfile can fetch the same release without re-resolving.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimePin {
    /// The requested range from `devEngines.runtime.version`, without
    /// the `runtime:` prefix pnpm adds in the importer entry
    /// (`"^24.4.0"`).
    pub specifier: String,
    /// Exact resolved version (`"24.4.1"`).
    pub version: String,
    /// Whether the importer entry sits under `devDependencies`
    /// (devEngines-sourced pins do; pnpm only emits this form today).
    pub dev: bool,
    /// `hasBin` flag on the packages entry — always true for real
    /// runtime pins; round-tripped for byte fidelity.
    pub has_bin: bool,
    /// Per-platform artifacts from the `variations` resolution.
    pub variants: Vec<RuntimeVariant>,
}

impl RuntimePin {
    /// The variant whose target list matches `(os, cpu, libc)`. `libc`
    /// follows pnpm's convention: `Some("musl")` matches only
    /// musl-tagged targets; `None` matches targets without a libc tag.
    pub fn variant_for(&self, os: &str, cpu: &str, libc: Option<&str>) -> Option<&RuntimeVariant> {
        self.variants.iter().find(|v| {
            v.targets
                .iter()
                .any(|t| t.os == os && t.cpu == cpu && t.libc.as_deref() == libc)
        })
    }
}

/// One platform-specific artifact inside a runtime pin's `variations`
/// resolution. Field set mirrors pnpm's `BinaryResolution` +
/// `PlatformAssetResolution` pair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeVariant {
    /// Platforms this artifact serves (usually exactly one).
    pub targets: Vec<RuntimeTarget>,
    /// `"tarball"` or `"zip"`.
    pub archive: String,
    /// Download URL for the artifact.
    pub url: String,
    /// SRI integrity (`sha256-<base64>` — Node publishes SHA-256
    /// checksums for release artifacts).
    pub integrity: String,
    /// Executable map. pnpm writes either a bare string (`bin/node`,
    /// meaning the `node` bin) or a `name → path` map; both parse into
    /// this struct and the original shape round-trips via
    /// [`Self::bin_is_bare_string`].
    pub bin: BTreeMap<String, String>,
    /// True when the source lockfile wrote `bin:` as a bare string;
    /// preserved so a parse/write cycle stays byte-identical.
    pub bin_is_bare_string: bool,
    /// Top-level directory to strip when extracting (pnpm sets this on
    /// zip archives, whose entries are rooted at
    /// `node-v<V>-win-<arch>/`).
    pub prefix: Option<String>,
}

/// One `(os, cpu, libc)` triple a runtime variant targets. Values use
/// Node's `process.platform` / `process.arch` vocabulary (`win32`,
/// `darwin`, `linux`; `x64`, `arm64`), with `libc: Some("musl")` only
/// on musl builds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeTarget {
    pub os: String,
    pub cpu: String,
    pub libc: Option<String>,
}

/// Per-graph settings that mirror pnpm v9's `settings:` header.
/// Extend as more knobs become round-trip-aware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockfileSettings {
    /// pnpm's `auto-install-peers` — when false the resolver leaves
    /// unmet peers alone (just warns) instead of dragging them in.
    pub auto_install_peers: bool,
    /// pnpm's `exclude-links-from-lockfile` — not yet honored by aube
    /// but round-tripped for lockfile compatibility.
    pub exclude_links_from_lockfile: bool,
    /// pnpm's `lockfile-include-tarball-url` — when true the writer
    /// emits the full registry tarball URL in each package's
    /// `resolution.tarball:` field alongside `integrity:`. Makes the
    /// lockfile self-contained so air-gapped installs don't need to
    /// derive the URL from `.npmrc`. Round-tripped through the
    /// `settings:` header so it survives parse/write cycles without
    /// re-reading `.npmrc`.
    pub lockfile_include_tarball_url: bool,
}

impl Default for LockfileSettings {
    fn default() -> Self {
        Self {
            auto_install_peers: true,
            exclude_links_from_lockfile: false,
            lockfile_include_tarball_url: false,
        }
    }
}

/// A direct dependency of a workspace importer.
#[derive(Debug, Clone)]
pub struct DirectDep {
    pub name: String,
    /// The dep_path key in the lockfile (e.g., "is-odd@3.0.1")
    pub dep_path: String,
    pub dep_type: DepType,
    /// The specifier as written in package.json at the time the lockfile was
    /// generated (e.g., `"^4.17.0"`). Used by drift detection to compare against
    /// the current manifest. Only populated by formats that record it
    /// (pnpm-lock.yaml v9). `None` for npm/yarn/bun lockfiles.
    pub specifier: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepType {
    Production,
    Dev,
    Optional,
}

/// Render a `DepType` as the matching `package.json` field name
/// (`dependencies` / `devDependencies` / `optionalDependencies`).
/// Single source of truth so drift diagnostics, install summaries,
/// the `outdated` / `why` / `deprecations` renderers, and the
/// `outdated --json` shape all agree on the spelling.
pub fn dep_type_label(dt: DepType) -> &'static str {
    match dt {
        DepType::Production => "dependencies",
        DepType::Dev => "devDependencies",
        DepType::Optional => "optionalDependencies",
    }
}

/// A single resolved package in the lockfile.
///
/// The `dependencies` map keys are dep names and values are the dependency's
/// dep_path *tail* — i.e. the string that follows `<name>@`. For a plain
/// package this is just the version (`"4.17.21"`); for a package with its
/// own peer context it includes the suffix (`"18.2.0(prop-types@15.8.1)"`).
/// Combining the key with its value reproduces the full dep_path (which is
/// also the key in `LockfileGraph.packages`).
#[derive(Debug, Clone, Default)]
pub struct LockedPackage {
    /// Package name (e.g., "lodash")
    pub name: String,
    /// Exact resolved version (e.g., "4.17.21")
    pub version: String,
    /// Integrity hash (e.g., "sha512-...")
    pub integrity: Option<String>,
    /// Dependencies of this package (name -> dep_path tail, see struct docs)
    pub dependencies: BTreeMap<String, String>,
    /// Optional dependency edges for this package. Active optional edges are
    /// also mirrored in `dependencies` so graph walks and the linker continue
    /// to see them; this separate map lets platform filtering prune optional
    /// edges without touching regular dependencies.
    pub optional_dependencies: BTreeMap<String, String>,
    /// Peer dependency ranges as *declared* by the package (from its
    /// package.json / packument). These are the constraints; the resolved
    /// versions live in `dependencies` after the peer-context pass runs.
    pub peer_dependencies: BTreeMap<String, String>,
    /// `peerDependenciesMeta` entries, keyed by peer name.
    pub peer_dependencies_meta: BTreeMap<String, PeerDepMeta>,
    /// The dep_path key used in the lockfile. For packages with resolved
    /// peer contexts this includes the suffix, e.g.
    /// `"styled-components@6.1.0(react@18.2.0)"`.
    pub dep_path: String,
    /// Set for non-registry packages (those installed via `file:` or
    /// `link:` specifiers). `None` for the common case of a package
    /// resolved from an npm registry, where `integrity` is the full
    /// record of where the bits came from.
    pub local_source: Option<LocalSource>,
    /// `os` / `cpu` / `libc` arrays from the package's manifest. Used
    /// by the resolver to filter optional deps that can't run on the
    /// current (or user-overridden) platform. Empty arrays mean no
    /// constraint.
    pub os: PlatformList,
    pub cpu: PlatformList,
    pub libc: PlatformList,
    /// Names declared in the package's own `bundledDependencies`. These
    /// ship inside the parent tarball's `node_modules/`, so the resolver
    /// neither fetches nor recurses into them, and the linker avoids
    /// creating sibling symlinks that would shadow the bundled tree.
    /// An empty Vec means "no bundled deps"; `None` is kept as a
    /// distinct value only inside the resolver and collapsed to empty
    /// here because the lockfile round-trip doesn't need to preserve
    /// the "unset" vs "empty list" distinction.
    pub bundled_dependencies: Vec<String>,
    /// Full registry tarball URL for registry-sourced packages. Only
    /// populated when `LockfileSettings::lockfile_include_tarball_url`
    /// is active on this graph; otherwise `None` and the lockfile
    /// writer derives the URL at fetch time from the configured
    /// registry. `local_source`-backed packages (file:, link:, git:,
    /// remote tarball) already carry their own URL via `LocalSource`
    /// and don't populate this field.
    pub tarball_url: Option<String>,
    /// pnpm `resolution.gitHosted` for registry-keyed packages. Remote
    /// tarball sources carry the same flag on `RemoteTarballSource`,
    /// but registry entries keep `local_source: None`, so this field
    /// preserves third-party pnpm lockfiles that mark registry-shaped
    /// tarballs as hosted git.
    pub registry_git_hosted: bool,
    /// For npm-alias deps (`"h3-v2": "npm:h3@2.0.1-rc.20"`): the real
    /// package name on the registry (`"h3"`). `None` means the entry
    /// is not aliased and `name` already holds the registry name.
    ///
    /// Install semantics when `Some(real)`:
    /// - `name` is the *alias* — that's the folder under `node_modules/`,
    ///   the symlink name for transitive deps, and the key every package
    ///   that declares this dep refers to.
    /// - `alias_of` is the real package name used for tarball URL lookup,
    ///   store index keying, and packument fetches.
    /// - `version` is the real resolved version.
    ///
    /// `registry_name()` returns the right name for registry IO; every
    /// call site that talks to the registry or the CAS uses that helper.
    pub alias_of: Option<String>,
    /// Yarn berry's `checksum:` field, preserved verbatim when parsing a
    /// yarn 2+ lockfile (e.g. `"10c0/<blake2b-hex>"`). The format is
    /// yarn-specific — it uses a yarn-chosen hash family prefixed with
    /// the `cacheKey` that produced it — and doesn't share a hash
    /// algorithm with `integrity` (sha-512). When re-emitting a yarn
    /// berry lockfile we write this field back as-is; packages that
    /// didn't come through a berry parse (e.g. freshly-resolved entries
    /// in a new install) leave this `None` and the writer omits the
    /// `checksum:` field, which berry tolerates at the default
    /// `checksumBehavior: throw` when the cache is fresh.
    pub yarn_checksum: Option<String>,
    /// `engines:` from the package's manifest, round-tripped through
    /// the lockfile so pnpm-style writers can emit the same flow-form
    /// `engines: {node: '>=8'}` line pnpm writes. Empty map means
    /// "no engines declared" — the writer skips the field entirely.
    pub engines: BTreeMap<String, String>,
    /// `bin:` map from the package's manifest, normalized to
    /// `name → path`. An empty map means "no bins declared".
    ///
    /// pnpm-style writers derive `hasBin: true` from
    /// `!bin.is_empty()` (they don't preserve the names/paths); bun's
    /// format emits the full map on the package's meta block. Keeping
    /// the map here lets both writers render byte-identical output
    /// without an extra tarball-level re-parse.
    pub bin: BTreeMap<String, String>,
    /// Dependency ranges as declared in this package's own
    /// `package.json` — keyed by dep name, values are the raw
    /// specifiers (`"^4.1.0"`, `"~1.1.4"`, `"workspace:*"`, …).
    ///
    /// Distinct from [`Self::dependencies`], which stores the
    /// *resolved* dep_path tail (`"4.3.0"`). npm / yarn / bun
    /// lockfiles preserve the declared ranges on every nested
    /// package entry — rewriting them to the resolved pins is the
    /// biggest source of round-trip churn against those formats. This
    /// map lets writers emit the declared range when available and
    /// fall back to the resolved pin otherwise (e.g. when the source
    /// lockfile was pnpm, whose `snapshots:` only carries pins).
    ///
    /// Empty means "unknown" — writers should fall back to pins.
    /// Covers production *and* optional dependencies in one map since
    /// a package can't declare the same name twice across those
    /// sections.
    pub declared_dependencies: BTreeMap<String, String>,
    /// Package's `license` field, collapsed to the simple string
    /// form. Round-tripped so npm's lockfile keeps its per-entry
    /// `"license": "MIT"` line; pnpm / yarn / bun don't record
    /// licenses and leave this `None` on parse.
    pub license: Option<String>,
    /// Package's funding URL, extracted from whatever shape the
    /// manifest's `funding:` field took (string / object / array).
    /// Round-tripped so npm's lockfile keeps its per-entry
    /// `"funding": {"url": "…"}` block.
    pub funding_url: Option<String>,
    /// pnpm `snapshots:` `optional: true` flag, marking a package
    /// reachable only through optional edges (typically platform-
    /// specific binaries like `@reflink/reflink-darwin-arm64`). pnpm
    /// uses this on the next install to decide whether the entry
    /// should be skipped on a non-matching platform; dropping it on
    /// round-trip would let pnpm treat the package as required.
    /// Always `false` outside the pnpm parse/write path.
    pub optional: bool,
    /// pnpm `snapshots:` `transitivePeerDependencies:` list — peer
    /// names that bubble up transitively through this package. pnpm
    /// reads it during hoisting and as a resolver staleness signal
    /// (`resolveDependencies.ts`'s non-zero-length check); a missing
    /// list looks like a graph change and triggers needless re-
    /// resolution on the next pnpm install. Empty outside the pnpm
    /// parse/write path. Fresh resolves leave this empty too — pnpm
    /// recomputes it from the graph during `resolvePeers` when needed.
    pub transitive_peer_dependencies: Vec<String>,
    /// Per-package-meta extras preserved verbatim from the source
    /// lockfile. Captures fields the typed model doesn't yet cover
    /// (bun's `optionalPeers` and anything a future lockfile bump
    /// adds) so a parse/write cycle doesn't drop them. Each format's
    /// writer re-emits what makes sense there — bun inlines the extras
    /// back on the package-entry meta object, pnpm / yarn / npm
    /// currently ignore them.
    pub extra_meta: BTreeMap<String, serde_json::Value>,
    /// npm `hasInstallScript: true` — set when the package declares an
    /// install / preinstall / postinstall script. npm writes it on
    /// every such entry; round-tripped verbatim so a parse → re-emit
    /// cycle keeps it. `false` outside the npm parse/write path (other
    /// formats don't record it).
    pub has_install_script: bool,
    /// npm `hasShrinkwrap: true` — set when the package ships its own
    /// `npm-shrinkwrap.json`. Verbatim round-trip; `false` elsewhere.
    pub has_shrinkwrap: bool,
    /// npm `inBundle: true` — set when the package ships inside another
    /// package's tarball. Verbatim round-trip; `false` elsewhere.
    pub in_bundle: bool,
    /// npm `deprecated: "<message>"` — the registry's deprecation
    /// message, copied onto the lock so a later install can warn
    /// offline. Verbatim round-trip; `None` elsewhere.
    pub deprecated: Option<String>,
}

impl LockedPackage {
    /// The package name to use for registry / store operations — the real
    /// name behind an npm-alias when aliased, otherwise just `name`. Used
    /// at every site that derives a tarball URL, a packument URL, or an
    /// aube-store cache key so aliased entries hit the actual package
    /// instead of the alias-qualified name.
    pub fn registry_name(&self) -> &str {
        self.alias_of.as_deref().unwrap_or(&self.name)
    }

    /// Canonical `"name@version"` key used as a handle in patches,
    /// approve-builds prompts, lockfile canonical maps, and display
    /// paths. Not the dep-path — that includes peer-context suffixes.
    pub fn spec_key(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    /// Exact approval key for non-registry package sources.
    ///
    /// Name-wide build approvals are only trustworthy for packages
    /// fetched from a registry. Source-backed entries need to be
    /// approved by their source identity as pnpm records it in
    /// lockfile keys / `allowBuilds` placeholders.
    pub fn source_approval_key(&self) -> Option<String> {
        self.local_source
            .as_ref()
            .map(|source| format!("{}@{}", self.registry_name(), source.specifier()))
    }

    /// Declared peer ranges with pnpm's meta-only peers folded in as `*`.
    ///
    /// pnpm records a `peerDependencies: { x: '*' }` entry for every
    /// `peerDependenciesMeta` key a package ships without an explicit
    /// range (debug's optional `supports-color`, typescript-eslint's
    /// optional `typescript`, …). This returns `peer_dependencies` with
    /// those meta-only keys added as `*` — both what the pnpm writer emits
    /// in `packages:` and the "declared peers" set the transitive-peer
    /// pass subtracts resolved deps from. Centralizing the rule keeps the
    /// writer and the resolver's transitive-peer pass from drifting.
    pub fn peer_dependencies_with_meta_defaults(&self) -> BTreeMap<String, String> {
        let mut deps = self.peer_dependencies.clone();
        for name in self.peer_dependencies_meta.keys() {
            deps.entry(name.clone()).or_insert_with(|| "*".to_string());
        }
        deps
    }
}

#[cfg(test)]
mod locked_package_tests {
    use super::*;
    use std::path::PathBuf;

    fn pkg() -> LockedPackage {
        LockedPackage {
            name: "pkg".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-abc".to_string()),
            dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            dep_path: "pkg@1.0.0".to_string(),
            local_source: None,
            os: PlatformList::default(),
            cpu: PlatformList::default(),
            libc: PlatformList::default(),
            bundled_dependencies: Vec::new(),
            tarball_url: None,
            registry_git_hosted: false,
            alias_of: None,
            yarn_checksum: None,
            engines: BTreeMap::new(),
            bin: BTreeMap::new(),
            declared_dependencies: BTreeMap::new(),
            license: None,
            funding_url: None,
            optional: false,
            transitive_peer_dependencies: Vec::new(),
            extra_meta: BTreeMap::new(),
            has_install_script: false,
            has_shrinkwrap: false,
            in_bundle: false,
            deprecated: None,
        }
    }

    #[test]
    fn source_approval_key_ignores_registry_git_hosted_packages() {
        let mut pkg = pkg();
        pkg.registry_git_hosted = true;

        assert_eq!(pkg.source_approval_key(), None);
    }

    #[test]
    fn source_approval_key_uses_source_spec_for_local_sources() {
        let mut pkg = pkg();
        pkg.dep_path = "pkg@file+abc(peer@1.0.0)".to_string();
        pkg.local_source = Some(LocalSource::Directory(PathBuf::from("vendor/pkg")));

        assert_eq!(
            pkg.source_approval_key(),
            Some("pkg@file:vendor/pkg".to_string())
        );
    }

    #[test]
    fn source_approval_key_uses_raw_remote_tarball_url() {
        let mut pkg = pkg();
        pkg.dep_path = "pkg@url+abc123".to_string();
        pkg.local_source = Some(LocalSource::RemoteTarball(RemoteTarballSource {
            url: "https://example.com/pkg.tgz".to_string(),
            integrity: "sha512-tarball".to_string(),
            git_hosted: false,
        }));

        assert_eq!(
            pkg.source_approval_key(),
            Some("pkg@https://example.com/pkg.tgz".to_string())
        );
    }
}

/// Metadata about a single declared peer dependency. Matches the shape of
/// `peerDependenciesMeta` in package.json.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerDepMeta {
    /// When true, an unmet peer is silently allowed rather than warned about.
    pub optional: bool,
}

impl LockfileGraph {
    /// Get all direct dependencies of the root project.
    pub fn root_deps(&self) -> &[DirectDep] {
        self.importers.get(".").map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Get a package by its dep_path key.
    pub fn get_package(&self, dep_path: &str) -> Option<&LockedPackage> {
        self.packages.get(dep_path)
    }

    /// BFS the transitive closure of `roots` through `self.packages`,
    /// returning every reachable dep_path (roots included). Missing
    /// roots are skipped silently — a root without a matching package
    /// is treated as a leaf, which matches what `filter_deps` /
    /// `subset_to_importer` need when a retained importer points at a
    /// package that was never fully installed (e.g. optional deps
    /// filtered out on this platform).
    ///
    /// `LockedPackage.dependencies` maps `child_name → dep_path tail`,
    /// so each child's full key reconstructs as `{child_name}@{tail}`.
    fn transitive_closure<'a>(
        &self,
        roots: impl IntoIterator<Item = &'a str>,
    ) -> std::collections::HashSet<String> {
        let mut reachable: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        for root in roots {
            if reachable.insert(root.to_string()) {
                queue.push_back(root.to_string());
            }
        }
        while let Some(dep_path) = queue.pop_front() {
            let Some(pkg) = self.packages.get(&dep_path) else {
                continue;
            };
            for (child_name, child_version) in &pkg.dependencies {
                let child_key = format!("{child_name}@{child_version}");
                if reachable.insert(child_key.clone()) {
                    queue.push_back(child_key);
                }
            }
        }
        reachable
    }

    /// Clone only the `packages` entries whose keys are in `reachable`.
    /// Paired with `transitive_closure` to produce the pruned
    /// `LockfileGraph.packages` for `filter_deps` / `subset_to_importer`.
    fn packages_restricted_to(
        &self,
        reachable: &std::collections::HashSet<String>,
    ) -> BTreeMap<String, LockedPackage> {
        self.packages
            .iter()
            .filter(|(dep_path, _)| reachable.contains(*dep_path))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Produce a new `LockfileGraph` containing only the direct deps that match
    /// `keep` and the transitive deps reachable from them.
    ///
    /// Used by `install --prod` to drop `DepType::Dev` roots and everything
    /// only reachable through them, and by `install --no-optional` for optional
    /// deps. The filter runs over every importer's direct-dep list, so workspace
    /// projects behave correctly.
    ///
    /// Packages that are reachable from a retained root through a transitive
    /// chain are kept even if a pruned dev dep also happened to depend on them —
    /// the check is "is this package reachable from any retained root?", not
    /// "was this package introduced by a retained root?".
    pub fn filter_deps<F>(&self, keep: F) -> LockfileGraph
    where
        F: Fn(&DirectDep) -> bool,
    {
        // Filter each importer's DirectDep list.
        let importers: BTreeMap<String, Vec<DirectDep>> = self
            .importers
            .iter()
            .map(|(path, deps)| {
                let filtered: Vec<DirectDep> = deps.iter().filter(|d| keep(d)).cloned().collect();
                (path.clone(), filtered)
            })
            .collect();

        // BFS from every retained root across every importer.
        let reachable = self.transitive_closure(
            importers
                .values()
                .flat_map(|deps| deps.iter().map(|d| d.dep_path.as_str())),
        );
        let packages = self.packages_restricted_to(&reachable);

        LockfileGraph {
            importers,
            packages,
            // Preserve the source graph's settings — filter is a
            // structural operation, not a resolution-mode reset.
            // Writing the filtered graph (e.g. from `aube prune`) must
            // emit the same `settings:` header the user chose.
            settings: self.settings.clone(),
            // Overrides are part of the user's resolution intent and
            // should survive structural filters like `aube prune`.
            overrides: self.overrides.clone(),
            // Config checksums describe the inputs that produced the
            // graph, not its shape — a structural filter must carry
            // them through unchanged.
            package_extensions_checksum: self.package_extensions_checksum.clone(),
            pnpmfile_checksum: self.pnpmfile_checksum.clone(),
            ignored_optional_dependencies: self.ignored_optional_dependencies.clone(),
            // Times follow the same round-trip invariant as settings:
            // filter doesn't change what versions are locked, so the
            // per-package publish timestamps carry through unchanged.
            times: self.times.clone(),
            skipped_optional_dependencies: self.skipped_optional_dependencies.clone(),
            catalogs: self.catalogs.clone(),
            bun_config_version: self.bun_config_version,
            patched_dependencies: self.patched_dependencies.clone(),
            patched_dependency_hashes: self.patched_dependency_hashes.clone(),
            trusted_dependencies: self.trusted_dependencies.clone(),
            // Runtime pins are graph-wide resolution intent, same as
            // overrides/catalogs — structural filters carry them.
            runtimes: self.runtimes.clone(),
            extra_fields: self.extra_fields.clone(),
            workspace_extra_fields: self.workspace_extra_fields.clone(),
        }
    }

    /// Produce a new `LockfileGraph` rooted at the importer at
    /// `importer_path`, with its transitive closure preserved and every
    /// other importer dropped. The retained importer is remapped to
    /// `"."` because the consumer installs the result as a standalone
    /// project.
    ///
    /// Used by `aube deploy`: reading the source workspace lockfile
    /// and subsetting it to the deployed package lets a frozen install
    /// in the target reproduce the workspace's exact versions without
    /// re-resolving against the registry. `keep` filters the importer's
    /// direct deps the same way `filter_deps` does, so `--prod` /
    /// `--dev` / `--no-optional` deploys drop the matching roots.
    ///
    /// Returns `None` if `importer_path` is not present in
    /// `self.importers`. Graph-wide metadata (`settings`, `overrides`,
    /// `times`, `catalogs`, `ignored_optional_dependencies`) is copied
    /// verbatim — structural pruning, not a resolution-mode reset.
    /// Callers targeting a non-workspace install may want to clear
    /// workspace-scope fields that would otherwise trigger drift
    /// detection against a rewritten target manifest.
    pub fn subset_to_importer<F>(&self, importer_path: &str, keep: F) -> Option<LockfileGraph>
    where
        F: Fn(&DirectDep) -> bool,
    {
        let src_deps = self.importers.get(importer_path)?;
        let kept: Vec<DirectDep> = src_deps.iter().filter(|d| keep(d)).cloned().collect();

        // BFS the transitive closure from retained roots, scoped to
        // just this importer's kept direct deps.
        let reachable = self.transitive_closure(kept.iter().map(|d| d.dep_path.as_str()));
        let packages = self.packages_restricted_to(&reachable);

        // Per-importer metadata: keep only the retained importer's
        // entry, rekeyed to `.`. The source workspace's other
        // importers are meaningless in a target that has exactly one.
        let mut skipped_optional_dependencies = BTreeMap::new();
        if let Some(skipped) = self.skipped_optional_dependencies.get(importer_path) {
            skipped_optional_dependencies.insert(".".to_string(), skipped.clone());
        }

        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), kept);

        Some(LockfileGraph {
            importers,
            packages,
            settings: self.settings.clone(),
            overrides: self.overrides.clone(),
            // The deployed subset inherits the source workspace's
            // config checksums: the same `packageExtensions`/pnpmfile
            // governed the resolution being subsetted.
            package_extensions_checksum: self.package_extensions_checksum.clone(),
            pnpmfile_checksum: self.pnpmfile_checksum.clone(),
            ignored_optional_dependencies: self.ignored_optional_dependencies.clone(),
            times: self.times.clone(),
            skipped_optional_dependencies,
            catalogs: self.catalogs.clone(),
            bun_config_version: self.bun_config_version,
            patched_dependencies: self.patched_dependencies.clone(),
            patched_dependency_hashes: self.patched_dependency_hashes.clone(),
            trusted_dependencies: self.trusted_dependencies.clone(),
            runtimes: self.runtimes.clone(),
            extra_fields: self.extra_fields.clone(),
            workspace_extra_fields: self.workspace_extra_fields.clone(),
        })
    }

    /// Overlay per-package metadata fields from `prior` onto `self`
    /// for every `(name, version)` that survives in both graphs.
    /// Carries forward only fields the abbreviated packument (npm
    /// corgi) doesn't ship — `license`, `funding_url`, and the
    /// bun-format `configVersion` — so a fresh re-resolve against
    /// the same spec set doesn't lose them.
    ///
    /// Keyed by canonical `name@version`, so a peer-context rewrite
    /// between the old and new graph still lines up. `self`'s own
    /// values win when set (fresh registry data is authoritative);
    /// `prior`'s fill in only the `None` / empty slots. Safe to call
    /// on any pair of graphs — parsing the old lockfile is the
    /// caller's concern.
    pub fn overlay_metadata_from(&mut self, prior: &LockfileGraph) {
        // Build a canonical `name@version → prior pkg` lookup once so
        // repeated peer-context variants in `self.packages` all hit
        // the same prior entry.
        let prior_index = build_canonical_map(prior);
        for pkg in self.packages.values_mut() {
            let key = pkg.spec_key();
            let Some(prior_pkg) = prior_index.get(&key) else {
                continue;
            };
            if pkg.license.is_none() && prior_pkg.license.is_some() {
                pkg.license = prior_pkg.license.clone();
            }
            if pkg.funding_url.is_none() && prior_pkg.funding_url.is_some() {
                pkg.funding_url = prior_pkg.funding_url.clone();
            }
            // Per-entry extras (`deprecated`, `optionalPeers`,
            // format-specific fields bun/npm/yarn wrote into the
            // meta block) can't be recovered from a fresh resolve,
            // so carry them forward when the newer graph doesn't
            // already carry its own. `self`-side keys always win.
            for (k, v) in &prior_pkg.extra_meta {
                pkg.extra_meta.entry(k.clone()).or_insert_with(|| v.clone());
            }
            // npm's per-entry verbatim flags aren't repopulated by a
            // fresh corgi resolve (they come from the package's own
            // manifest / the registry's packument flag), so carry them
            // forward to keep the round-trip stable. `self`-side values
            // win when already set.
            if !pkg.has_install_script && prior_pkg.has_install_script {
                pkg.has_install_script = true;
            }
            if !pkg.has_shrinkwrap && prior_pkg.has_shrinkwrap {
                pkg.has_shrinkwrap = true;
            }
            if !pkg.in_bundle && prior_pkg.in_bundle {
                pkg.in_bundle = true;
            }
            if pkg.deprecated.is_none() && prior_pkg.deprecated.is_some() {
                pkg.deprecated = prior_pkg.deprecated.clone();
            }
            if pkg.bundled_dependencies.is_empty() && !prior_pkg.bundled_dependencies.is_empty() {
                pkg.bundled_dependencies = prior_pkg.bundled_dependencies.clone();
            }
        }
        if self.bun_config_version.is_none() {
            self.bun_config_version = prior.bun_config_version;
        }
        // `patched_dependencies` (+ its hash sidecar) is deliberately
        // NOT carried forward: the patch config in the manifest /
        // workspace yaml is authoritative, and the CLI's
        // record-patches pass re-stamps it on every write path. A
        // carry-forward would resurrect entries the user just
        // `patch-remove`d.
        if self.trusted_dependencies.is_empty() {
            self.trusted_dependencies = prior.trusted_dependencies.clone();
        }
        // Runtime pins can't be recovered from a fresh package resolve
        // (they come from devEngines resolution, a separate pass), so a
        // re-resolved graph that hasn't re-pinned yet inherits the
        // prior pin. The install driver overwrites it when the
        // devEngines range drifted.
        if self.runtimes.is_empty() {
            self.runtimes = prior.runtimes.clone();
        }
        if self.extra_fields.is_empty() {
            self.extra_fields = prior.extra_fields.clone();
        }
        if self.workspace_extra_fields.is_empty() {
            self.workspace_extra_fields = prior.workspace_extra_fields.clone();
        }
    }
}
