//! Typed `WorkspaceConfig`, loading helpers, and on-disk target detection.
//!
//! Owns the read path: the `WorkspaceConfig` deserializer, the
//! `load_raw` / `load_both` parsers, the per-process memoization
//! caches, and `config_write_target` (which decides whether a
//! workspace-level mutation should land in `package.json` or the
//! workspace yaml).

use crate::UpdateConfig;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Workspace-config YAML filenames, in probe/write precedence order: this
/// tool's branded YAML first (if it has one), then — when the engine context's
/// `read_branded_pnpm_config` is set (upstream default) — the shared
/// `pnpm-workspace.yaml` compatibility surface. Standalone aube:
/// `["aube-workspace.yaml", "pnpm-workspace.yaml"]`. An embedder under a
/// non-pnpm incumbent clears the posture, dropping `pnpm-workspace.yaml` so a
/// stray copy left by another tool isn't read.
pub fn workspace_yaml_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::with_capacity(2);
    if let Some(branded) = aube_util::embedder().workspace_yaml {
        names.push(branded);
    }
    if aube_util::engine_context().read_branded_pnpm_config {
        names.push("pnpm-workspace.yaml");
    }
    names
}

fn find_and_read(project_dir: &Path) -> Result<Option<(PathBuf, String)>, crate::Error> {
    for name in workspace_yaml_names().iter().copied() {
        let path = project_dir.join(name);
        if path.exists() {
            let content =
                std::fs::read_to_string(&path).map_err(|e| crate::Error::Io(path.clone(), e))?;
            return Ok(Some((path, content)));
        }
    }
    Ok(None)
}

/// Extra privileges granted to one package pattern under `jailBuilds`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JailBuildPermission {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
    #[serde(default)]
    pub network: bool,
}

/// Configuration from `pnpm-workspace.yaml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConfig {
    /// Workspace package globs (e.g., `["packages/*"]`).
    #[serde(default)]
    pub packages: Vec<String>,

    /// Include the root manifest in recursive/filter workspace operations.
    #[serde(default)]
    pub include_workspace_root: Option<bool>,

    /// Default catalog for dependency version pinning.
    #[serde(default)]
    pub catalog: BTreeMap<String, String>,

    /// Named catalogs for dependency version pinning.
    #[serde(default)]
    pub catalogs: BTreeMap<String, BTreeMap<String, String>>,

    // -- Node-Modules Settings --
    /// Linking strategy: "isolated" (default), "hoisted", or "pnp".
    #[serde(default)]
    pub node_linker: Option<String>,

    /// Whether to use the global virtual store (default: false in pnpm, true in aube).
    #[serde(default)]
    pub enable_global_virtual_store: Option<bool>,

    /// Package names whose presence in any importer forces
    /// per-project materialization (disabling the global virtual
    /// store for that install). Defaults to the common bundler /
    /// framework direct-devDeps whose module resolvers follow
    /// symlinks then walk up (Next.js's Turbopack, Vite, Rollup,
    /// Webpack, Parcel, Nuxt, VitePress) — the global virtual store
    /// makes `.aube/<pkg>` an absolute symlink that escapes the
    /// project's filesystem root, which those resolvers can't walk
    /// back from. Add more names as you discover tools with the same
    /// restriction; set to `[]` to disable the heuristic. Declared
    /// here so `settings.toml`'s workspaceYaml source stays in sync
    /// with the actual deserialize surface.
    #[serde(default)]
    pub disable_global_virtual_store_for_packages: Option<Vec<String>>,

    /// Package import method: "auto", "hardlink", "copy", "clone", "clone-or-copy".
    #[serde(default)]
    pub package_import_method: Option<String>,

    /// Path to the virtual store directory (default: "node_modules/.aube").
    #[serde(default)]
    pub virtual_store_dir: Option<String>,

    /// Top-level modules directory name (default: "node_modules").
    /// aube accepts the setting for parity but only honors the
    /// default value — see `settings.toml`'s `modulesDir` entry.
    #[serde(default)]
    pub modules_dir: Option<String>,

    /// Whether to shamefully hoist all packages to root node_modules.
    #[serde(default)]
    pub shamefully_hoist: Option<bool>,

    /// Master switch for the hidden modules directory at
    /// `node_modules/.aube/node_modules/`. Default true.
    #[serde(default)]
    pub hoist: Option<bool>,

    /// Patterns of packages to hoist.
    #[serde(default)]
    pub hoist_pattern: Option<Vec<String>>,

    /// Whether workspace packages get symlinked into each importer's
    /// `node_modules/`. Default true.
    #[serde(default)]
    pub hoist_workspace_packages: Option<bool>,

    /// Limit how far packages may be promoted in `nodeLinker=hoisted`.
    #[serde(default)]
    pub hoisting_limits: Option<String>,

    /// When true, the linker skips a workspace package's per-importer
    /// `node_modules/<name>` symlink if the workspace root already
    /// links the same package at the same resolved version. Default
    /// false (pnpm parity).
    #[serde(default)]
    pub dedupe_direct_deps: Option<bool>,

    /// When true, `aube deploy` copies every file in the source
    /// workspace package into the target directory instead of
    /// applying pack's `files` / `.npmignore` filter. Default false
    /// (pnpm parity). Declared as a typed field so the settings-meta
    /// parity test can see the workspace-yaml key.
    #[serde(default)]
    pub deploy_all_files: Option<bool>,

    /// Patterns of packages to hoist to the root node_modules.
    #[serde(default)]
    pub public_hoist_pattern: Option<Vec<String>>,

    // -- Store Settings --
    /// Path to the content-addressable store.
    #[serde(default)]
    pub store_dir: Option<String>,

    // -- Lockfile Settings --
    /// Whether to use a lockfile (default: true).
    #[serde(default)]
    pub lockfile: Option<bool>,

    /// Lockfile format written when the project has no lockfile yet
    /// (`aube` | `pnpm` | `npm` | `yarn` | `bun`, default `aube`).
    /// Same semantics as the `defaultLockfileFormat` setting resolved
    /// via `aube_settings::resolved`; declared here so the
    /// `workspace_yaml_keys_deserialize_onto_workspace_config` parity
    /// test sees a real field behind the workspaceYaml source.
    #[serde(default)]
    pub default_lockfile_format: Option<String>,

    /// Directory the lockfile is written to and read from. When unset
    /// or equal to the project root, behaves as before. When set to a
    /// different directory, the project becomes an importer keyed by
    /// its relative path (mirrors pnpm's `lockfile-dir`).
    #[serde(default)]
    pub lockfile_dir: Option<String>,

    /// Whether to prefer frozen lockfile (default: true).
    #[serde(default)]
    pub prefer_frozen_lockfile: Option<bool>,

    /// Write a per-branch lockfile (`pnpm-lock.<branch>.yaml`) instead of
    /// the default `pnpm-lock.yaml`. Reduces merge conflicts on long-lived
    /// branches. Forward slashes in branch names are encoded as `!`.
    #[serde(default)]
    pub git_branch_lockfile: Option<bool>,

    /// Branch-name glob list that triggers an automatic branch-lockfile
    /// merge on matching branches. Companion to `gitBranchLockfile`.
    /// See `settings.toml` for the pattern syntax (including `!`-prefix
    /// negations). Declared as a typed field so the settings-meta parity
    /// test can see the workspace-yaml key.
    #[serde(default)]
    pub merge_git_branch_lockfiles_branch_pattern: Option<Vec<String>>,

    /// Write per-package lockfiles instead of one shared workspace lockfile.
    /// Default `true` matches pnpm. The typed field is declared only so the
    /// settings-meta parity test can see the workspace-yaml key — the
    /// install path reads the value through `aube_settings::resolved`.
    #[serde(default)]
    pub shared_workspace_lockfile: Option<bool>,

    /// Cap on lockfile peer-ID suffix byte length before the resolver
    /// replaces the suffix with `_<sha256-hex>`. Default 1000 (pnpm
    /// parity). Same typed/raw duality as `child_concurrency` — see
    /// that field's comment.
    #[serde(default)]
    pub peers_suffix_max_length: Option<u64>,

    // -- Dependency Resolution --
    /// Override any dependency in the dependency graph.
    #[serde(default)]
    pub overrides: BTreeMap<String, String>,

    /// `name@version` → patch-file-path map. pnpm v10 moved this out
    /// of `package.json`'s `pnpm.patchedDependencies` so users can
    /// document *why* a patch exists with YAML comments; aube merges
    /// both locations, with workspace-yaml entries winning on key
    /// conflict (same precedence as `overrides`).
    #[serde(default, rename = "patchedDependencies")]
    pub patched_dependencies: BTreeMap<String, String>,

    /// os/cpu/libc widening set. pnpm v10 moved this alongside
    /// `overrides` — users generating a cross-platform lockfile on
    /// Linux CI want to widen in the workspace yaml (where the rest
    /// of their shared config lives) rather than `package.json`.
    /// Merged with `package.json`'s `pnpm.supportedArchitectures` /
    /// `aube.supportedArchitectures` at install time.
    #[serde(default, rename = "supportedArchitectures")]
    pub supported_architectures: Option<SupportedArchitectures>,

    /// Optional-dep names that should always be skipped, even when
    /// their platform matches. Merged with `package.json`'s
    /// `pnpm.ignoredOptionalDependencies` / `aube.*` at install time.
    /// Distinct from `--no-optional`, which drops *all* optional deps.
    #[serde(default, rename = "ignoredOptionalDependencies")]
    pub ignored_optional_dependencies: Vec<String>,

    /// Override for the pnpmfile path. pnpm lets users point at a
    /// non-default location; aube's default is `cwd/.pnpmfile.mjs`
    /// when present, otherwise `cwd/.pnpmfile.cjs`.
    /// Relative paths resolve against the workspace root.
    #[serde(default, rename = "pnpmfilePath")]
    pub pnpmfile_path: Option<String>,

    /// Extend package metadata during resolution.
    #[serde(default)]
    pub package_extensions: BTreeMap<String, yaml_serde::Value>,

    /// Package deprecation ranges whose warnings should be muted.
    #[serde(default)]
    pub allowed_deprecated_versions: BTreeMap<String, String>,

    /// Scope of install-time deprecation warnings: `none`, `direct`,
    /// `all`, or `summary`. Declared as a typed field so the
    /// settings-meta parity test sees the workspaceYaml key.
    #[serde(default)]
    pub deprecation_warnings: Option<String>,

    /// Update-time policy knobs.
    #[serde(default)]
    pub update_config: Option<UpdateConfig>,

    /// Node.js download mirror map (pnpm parity), keyed by channel:
    /// `release` is used for runtime downloads; `rc`/`nightly` are
    /// parsed but currently unused.
    #[serde(default)]
    pub node_download_mirrors: BTreeMap<String, String>,

    /// Who installs a missing Node.js runtime: `auto` (mise when
    /// present, else aube), `mise`, or `aube`. Declared as a typed
    /// field so the settings-meta parity test sees the workspaceYaml
    /// key.
    #[serde(default)]
    pub runtime_installer: Option<String>,

    /// Override for `devEngines.runtime`'s `onFail` policy:
    /// `download`, `error`, `warn`, or `ignore`.
    #[serde(default)]
    pub runtime_on_fail: Option<String>,

    /// Self-version switching toggle (pnpm parity). Declared as a
    /// typed field so the settings-meta parity test sees the
    /// workspaceYaml key.
    #[serde(default)]
    pub manage_package_manager_versions: Option<bool>,

    /// Trust-policy mode. Parsed for pnpm parity; resolver support is
    /// limited to accepting the configured policy surface until registry
    /// trust metadata is available.
    #[serde(default)]
    pub trust_policy: Option<String>,

    /// Packages exempt from trust-policy checks.
    #[serde(default)]
    pub trust_policy_exclude: Vec<String>,

    /// Ignore trust-policy checks for package versions older than this
    /// many minutes.
    #[serde(default)]
    pub trust_policy_ignore_after: Option<u64>,

    /// Reject transitive git/file/tarball dependency specs by default.
    #[serde(default)]
    pub block_exotic_subdeps: Option<bool>,

    // -- Build Settings --
    /// Whether to ignore all lifecycle scripts (default: false).
    #[serde(default)]
    pub ignore_scripts: Option<bool>,

    // -- aube-specific knobs --
    /// Skip the `aube run` / `aube exec` auto-install staleness check
    /// at the workspace level. Same semantics as the `aubeNoAutoInstall`
    /// setting resolved via `aube_settings::resolved`; both surfaces
    /// round-trip through `WorkspaceConfig` to keep the
    /// `workspace_yaml_keys_deserialize_onto_workspace_config` parity
    /// test happy. See `AUBE_NO_AUTO_INSTALL` env-var alias.
    #[serde(default)]
    pub aube_no_auto_install: Option<bool>,

    /// Bypass the project-level advisory lock on `node_modules/` for
    /// every mutating aube command in this workspace. Same semantics as
    /// the `aubeNoLock` setting resolved via `aube_settings::resolved`;
    /// useful for CI matrices or deliberately-parallel test rigs
    /// running from one shared workspace. See `AUBE_NO_LOCK` env-var
    /// alias.
    #[serde(default)]
    pub aube_no_lock: Option<bool>,

    /// Per-package allowlist for dependency lifecycle scripts. Keys are
    /// pnpm-style patterns (`name`, `name@version`, `name@v1 || v2`);
    /// values are `true` to allow or `false` to deny. Merged with
    /// `package.json`'s `pnpm.allowBuilds` — workspace-level entries
    /// take precedence for the same key.
    #[serde(default)]
    pub allow_builds: BTreeMap<String, yaml_serde::Value>,

    /// pnpm's canonical allowlist format: a flat list of package names
    /// whose lifecycle scripts are allowed to run. Merged with
    /// `allow_builds` into the same `BuildPolicy`. Workspace-level
    /// entries apply to every importer in the workspace.
    #[serde(default, rename = "onlyBuiltDependencies")]
    pub only_built_dependencies: Vec<String>,

    /// pnpm's canonical denylist: lifecycle scripts from these packages
    /// never run even if the allowlist includes them (explicit denies
    /// always win in `BuildPolicy::decide`).
    #[serde(default, rename = "neverBuiltDependencies")]
    pub never_built_dependencies: Vec<String>,

    /// Maximum number of dep lifecycle scripts running in parallel
    /// during the post-link `allowBuilds` phase. Defaults to 5 when
    /// unset. Mirrors pnpm's `childConcurrency` setting.
    ///
    /// The typed field isn't read directly by `install::run` —
    /// every int/string setting in aube has two faces: this struct
    /// field (for strict deserialization) and a
    /// `aube_settings::resolved::<name>` accessor that reads from the
    /// parallel raw YAML map in `ResolveCtx`. The duplication exists
    /// so the `meta::workspace_yaml_keys_deserialize_onto_workspace_config`
    /// test can catch settings.toml typos that would otherwise let a
    /// YAML key fall through to `extra` and be silently ignored. Same
    /// pattern as `minimum_release_age` / `auto_install_peers`.
    #[serde(default, rename = "childConcurrency")]
    pub child_concurrency: Option<u64>,

    /// Cap concurrent tarball downloads. When unset, aube uses an
    /// auto-scaled worker count x3 default, clamped to 16-64. Same
    /// typed/raw duality as `child_concurrency`.
    #[serde(default, rename = "networkConcurrency")]
    pub network_concurrency: Option<u64>,

    /// Cap package materialization/linking worker count. When unset,
    /// aube uses platform-aware defaults in `aube-linker`. Same
    /// typed/raw duality as `child_concurrency`.
    #[serde(default, rename = "linkConcurrency")]
    pub link_concurrency: Option<u64>,

    /// Whether to verify each tarball's SHA-512 against the lockfile
    /// integrity before importing into the store. Defaults to `true`
    /// (pnpm parity); `false` skips the check.
    #[serde(default, rename = "verifyStoreIntegrity")]
    pub verify_store_integrity: Option<bool>,

    /// Companion to `verifyStoreIntegrity`. When true, a missing
    /// `dist.integrity` on an imported packument is a hard error
    /// instead of a warning. Defaults to `false` for pnpm parity.
    #[serde(default, rename = "strictStoreIntegrity")]
    pub strict_store_integrity: Option<bool>,

    /// Cache post-build side effects for dependency packages.
    /// Accepted for pnpm-config parity but currently a no-op —
    /// aube skips dep lifecycle scripts by default.
    #[serde(default, rename = "sideEffectsCache")]
    pub side_effects_cache: Option<bool>,

    /// Run approved dependency lifecycle scripts in a restricted build
    /// jail. Same typed/raw duality as `child_concurrency`.
    #[serde(default, rename = "jailBuilds")]
    pub jail_builds: Option<bool>,

    /// Master switch that forces both `jailBuilds=true` and
    /// `trustPolicy=no-downgrade`. Same typed/raw duality as
    /// `child_concurrency`.
    #[serde(default)]
    pub paranoid: Option<bool>,

    /// Dependency package patterns that should run outside the jail even
    /// when `jailBuilds` is enabled. Same typed/raw duality as
    /// `child_concurrency`.
    #[serde(default, rename = "jailBuildExclusions")]
    pub jail_build_exclusions: Vec<String>,

    /// Extra env/path/network grants for packages that still run in the
    /// jail. Keys use the same package-pattern syntax as `allowBuilds`.
    #[serde(default, rename = "jailBuildPermissions")]
    pub jail_build_permissions: BTreeMap<String, JailBuildPermission>,

    // -- Catalog Settings --
    /// Drop catalog entries that no importer references after resolve.
    /// Wired through `aube_settings::resolved::cleanup_unused_catalogs`;
    /// the typed field exists only so `meta::workspace_yaml_keys_...`
    /// sees the key as a real field and doesn't fall through to `extra`.
    #[serde(default)]
    pub cleanup_unused_catalogs: Option<bool>,

    // -- Workspace-protocol settings --
    /// Resolve `aube add <name>` against local workspace siblings
    /// before falling back to the registry. The yaml value can be the
    /// booleans `true` / `false` or the string `"deep"`, so the typed
    /// field lands at `yaml_serde::Value` and the resolver normalizes
    /// via `aube_settings::resolved::LinkWorkspacePackages`.
    #[serde(default)]
    pub link_workspace_packages: Option<yaml_serde::Value>,

    /// Spec form written to `package.json` when `aube add` matches a
    /// workspace sibling. The yaml value can be the booleans `true` /
    /// `false` or the string `"rolling"`, so the typed field lands at
    /// `yaml_serde::Value` and the resolver normalizes via
    /// `aube_settings::resolved::SaveWorkspaceProtocol::from_str_normalized`.
    #[serde(default)]
    pub save_workspace_protocol: Option<yaml_serde::Value>,

    /// Version prefix written to `package.json` when `aube add` records a
    /// dep (`""` exact, `"~"`, or `"^"`). pnpm reads a top-level
    /// `savePrefix` from `pnpm-workspace.yaml`; declared here so the
    /// setting resolver can read it as a workspace-yaml source (the
    /// resolver validates the value, so the typed field is a plain
    /// `String`).
    #[serde(default)]
    pub save_prefix: Option<String>,

    // -- Peer Dependency Settings --
    /// Whether to auto-install peer dependencies (default: true).
    #[serde(default)]
    pub auto_install_peers: Option<bool>,

    /// Fail the install if any required peer dependency is missing or
    /// resolves outside its declared range. Default: false (warn only).
    #[serde(default)]
    pub strict_peer_dependencies: Option<bool>,

    /// Omit `link:` dependencies from the lockfile's importer
    /// dependency maps on write. Default: false.
    #[serde(default)]
    pub exclude_links_from_lockfile: Option<bool>,

    /// Collapse peer-equivalent subtree variants into a single
    /// canonical dep_path (cross-subtree intersection). Default: true.
    #[serde(default)]
    pub dedupe_peer_dependents: Option<bool>,

    /// Emit peer suffixes as `(version)` instead of `(name@version)`
    /// in the lockfile. Default: false.
    #[serde(default)]
    pub dedupe_peers: Option<bool>,

    /// Consult the root workspace importer's direct deps for peer
    /// resolution before falling back to a graph-wide scan.
    /// Default: true.
    #[serde(default)]
    pub resolve_peers_from_workspace_root: Option<bool>,

    /// Record the full registry tarball URL on every registry-sourced
    /// package in the lockfile's `resolution.tarball:` field. Default:
    /// false. Round-trips through the lockfile `settings:` header so
    /// the value is preserved once set.
    #[serde(default)]
    pub lockfile_include_tarball_url: Option<bool>,

    // -- Supply Chain Settings --
    /// Minimum age in minutes that a package version must have before
    /// it's eligible for resolution. pnpm v11 default is 1440 (1 day).
    #[serde(default)]
    pub minimum_release_age: Option<u64>,

    /// Package names exempt from `minimum_release_age`.
    #[serde(default)]
    pub minimum_release_age_exclude: Option<Vec<String>>,

    /// When true, fail the install if no version satisfies a range
    /// without violating `minimum_release_age`. Default: false (fall back
    /// to the lowest satisfying version that ignores the cutoff).
    #[serde(default)]
    pub minimum_release_age_strict: Option<bool>,

    /// OSV `MAL-*` advisory check policy for `aube add`. One of
    /// `"on"` (default, fail open on fetch error), `"required"` (fail
    /// closed on fetch error), or `"off"`.
    #[serde(default)]
    pub advisory_check: Option<String>,

    /// OSV `MAL-*` advisory check policy for plain reinstalls,
    /// backed by a local mirror of OSV's npm dump. Independent of
    /// `advisory_check`, which fires on `aube add`, `aube update`,
    /// and other fresh-resolution paths via the live OSV API.
    /// Values: `"on"` (refresh-on-stale, fail-open on refresh
    /// error), `"required"` (fail-closed on refresh error), or
    /// `"off"` (default).
    #[serde(default)]
    pub advisory_check_on_install: Option<String>,

    /// Bloom-filter prefilter for OSV `MAL-*` advisories on
    /// lockfile-driven installs. Downloads a sub-MB filter from
    /// `endevco/osv-bloom` (regenerated every 10 minutes) and
    /// probes the resolved graph against it. Only bloom hits
    /// escalate to the live OSV API for confirmation, so this
    /// stays cheap enough to enable per-install. Values: `"on"`
    /// (fail-open on refresh/probe error), `"required"`
    /// (fail-closed on refresh/probe error), or `"off"` (default).
    /// Coexists with `advisory_check` and `advisory_check_on_install`.
    #[serde(default)]
    pub advisory_bloom_check: Option<String>,

    /// Force the live-API OSV `MAL-*` check on every install
    /// (including frozen reinstalls). Default `false`. Set to
    /// `true` for hardened CI where every install must observe
    /// the latest advisories regardless of whether the lockfile
    /// changed. Trades per-install latency for freshness.
    #[serde(default)]
    pub advisory_check_every_install: Option<bool>,

    /// Weekly-downloads floor for `aube add`. Below this, aube prompts
    /// for confirmation (or fails non-interactively). 0 disables.
    #[serde(default)]
    pub low_download_threshold: Option<u64>,

    /// Glob patterns exempted from the `low_download_threshold` gate.
    /// Each entry is matched against the full registry name (e.g.
    /// `@scope/foo`). Matching names skip the weekly-downloads lookup;
    /// the OSV `MAL-*` check still runs.
    #[serde(default)]
    pub allowed_unpopular_packages: Option<Vec<String>>,

    /// Bun-compatible security scanner module spec (npm package name
    /// or path) loaded via a `node` bridge at install / add time.
    /// Empty string disables the integration.
    #[serde(default)]
    pub security_scanner: Option<String>,

    /// pnpm-style peer dependency escape hatches. Read by
    /// `PeerDependencyRules::resolve` during install; the actual matching
    /// logic lives in `aube`. We only need the container here so the
    /// settings-meta parity test can see the top-level key as a real
    /// field (not falling into `extra`). Leaves of the map are
    /// deserialized lazily on demand.
    #[serde(default)]
    pub peer_dependency_rules: Option<yaml_serde::Value>,

    /// Capture unknown fields for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, yaml_serde::Value>,
}

/// `supportedArchitectures.{os,cpu,libc}` arrays from
/// pnpm-workspace.yaml. Same three-axis shape pnpm uses; each entry
/// can be a concrete token (`"linux"`) or the literal `"current"`,
/// which the resolver expands to the host triple.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SupportedArchitectures {
    #[serde(default)]
    pub os: Vec<String>,
    #[serde(default)]
    pub cpu: Vec<String>,
    #[serde(default)]
    pub libc: Vec<String>,
}

impl SupportedArchitectures {
    pub fn is_empty(&self) -> bool {
        self.os.is_empty() && self.cpu.is_empty() && self.libc.is_empty()
    }
}

impl WorkspaceConfig {
    /// Convert the raw `allow_builds` map to the same shape used for
    /// `package.json`'s `pnpm.allowBuilds`, so callers can merge both
    /// sources uniformly.
    pub fn allow_builds_raw(&self) -> BTreeMap<String, crate::AllowBuildRaw> {
        self.allow_builds
            .iter()
            .map(|(k, v)| {
                let raw = match v {
                    yaml_serde::Value::Bool(b) => crate::AllowBuildRaw::Bool(*b),
                    // Strings are stored verbatim. `yaml_serde::to_string`
                    // would re-encode the value as YAML — quoting strings
                    // that need quoting, adding a trailing newline — and
                    // that wrapped form would defeat the read-side
                    // equality check against the canonical review
                    // placeholder, plus surface extra quotes in any
                    // warning the user sees.
                    yaml_serde::Value::String(s) => crate::AllowBuildRaw::Other(s.clone()),
                    other => {
                        // Render via YAML serialization so the user sees
                        // the same text they wrote (`[a, b]`) rather than
                        // yaml_serde's Debug form. Matches the JSON side
                        // in `AllowBuildRaw::from_json`.
                        let rendered = yaml_serde::to_string(other)
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        crate::AllowBuildRaw::Other(rendered)
                    }
                };
                (k.clone(), raw)
            })
            .collect()
    }

    /// Load workspace config from `aube-workspace.yaml` (preferred) or
    /// `pnpm-workspace.yaml` (pnpm compatibility) in the given directory.
    /// Returns `Default` if neither file exists. If both exist, the aube
    /// file wins and the pnpm file is ignored.
    ///
    /// Memoized per-process. `find_workspace_packages`, lockfile-dir
    /// resolution, catalog cleanup, jail-builds, and write-target
    /// picking all hit this 4-8× per command with the same cwd.
    /// Matches the existing `RAW_CACHE` pattern for the raw map.
    pub fn load(project_dir: &Path) -> Result<Self, crate::Error> {
        if let Some(hit) = typed_cache_lookup(project_dir) {
            return Ok(hit);
        }
        let value = Self::load_uncached(project_dir)?;
        typed_cache_insert(project_dir, value.clone());
        Ok(value)
    }

    fn load_uncached(project_dir: &Path) -> Result<Self, crate::Error> {
        let Some((path, content)) = find_and_read(project_dir)? else {
            return Ok(Self::default());
        };
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        crate::parse_yaml(&path, content)
    }
}

type TypedCacheMap = std::collections::HashMap<std::path::PathBuf, WorkspaceConfig>;

static TYPED_CACHE: std::sync::OnceLock<std::sync::Mutex<TypedCacheMap>> =
    std::sync::OnceLock::new();

fn typed_cache_lookup(project_dir: &Path) -> Option<WorkspaceConfig> {
    let cache = TYPED_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    cache.lock().ok()?.get(project_dir).cloned()
}

fn typed_cache_insert(project_dir: &Path, value: WorkspaceConfig) {
    let cache = TYPED_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Ok(mut map) = cache.lock() {
        map.insert(project_dir.to_path_buf(), value);
    }
}

/// Load the workspace yaml as a raw top-level key/value map, without
/// coercing into `WorkspaceConfig`'s typed fields. Intended for
/// metadata-driven setting resolution (see `aube-settings`), where
/// the caller walks a list of aliases from
/// `SettingMeta::workspace_yaml_keys` and pulls out whichever key is
/// present.
///
/// Returns an empty map if no file exists — same semantics as `load`.
/// File-precedence rules match `load`: `aube-workspace.yaml` wins
/// over `pnpm-workspace.yaml`.
// Process-wide memoization for the raw workspace-yaml map. Hot-path
// callers (`with_settings_ctx`, `aube_lock_filename`, `take_project_lock`,
// and the install-path `load_both` caller) all hit this with the same
// cwd. Same pattern as `aube_lockfile::aube_lock_filename`. Both
// `load_raw` and `load_both` populate + read this cache so a later
// `load_raw` after `load_both` doesn't re-read the file.
type RawCacheMap =
    std::collections::HashMap<std::path::PathBuf, BTreeMap<String, yaml_serde::Value>>;

static RAW_CACHE: std::sync::OnceLock<std::sync::Mutex<RawCacheMap>> = std::sync::OnceLock::new();

fn raw_cache_lookup(project_dir: &Path) -> Option<BTreeMap<String, yaml_serde::Value>> {
    let cache = RAW_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    cache.lock().ok()?.get(project_dir).cloned()
}

fn raw_cache_insert(project_dir: &Path, value: BTreeMap<String, yaml_serde::Value>) {
    let cache = RAW_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Ok(mut map) = cache.lock() {
        map.insert(project_dir.to_path_buf(), value);
    }
}

pub fn load_raw(project_dir: &Path) -> Result<BTreeMap<String, yaml_serde::Value>, crate::Error> {
    if let Some(hit) = raw_cache_lookup(project_dir) {
        return Ok(hit);
    }
    let Some((path, content)) = find_and_read(project_dir)? else {
        raw_cache_insert(project_dir, BTreeMap::new());
        return Ok(BTreeMap::new());
    };
    if content.trim().is_empty() {
        raw_cache_insert(project_dir, BTreeMap::new());
        return Ok(BTreeMap::new());
    }
    let parsed: BTreeMap<String, yaml_serde::Value> = crate::parse_yaml(&path, content)?;
    raw_cache_insert(project_dir, parsed.clone());
    Ok(parsed)
}

/// Load the workspace yaml once and return both the typed
/// `WorkspaceConfig` view and the raw `BTreeMap` view, parsed from
/// the same file contents. Callers that need both (e.g. `install::run`,
/// which wants typed `allow_builds_raw()` *and* the raw map for
/// metadata-driven setting resolution) avoid the two-read hit this
/// way. Errors propagate instead of being silently swallowed.
#[allow(clippy::type_complexity)]
pub fn load_both(
    project_dir: &Path,
) -> Result<(WorkspaceConfig, BTreeMap<String, yaml_serde::Value>), crate::Error> {
    let Some((path, content)) = find_and_read(project_dir)? else {
        raw_cache_insert(project_dir, BTreeMap::new());
        return Ok((WorkspaceConfig::default(), BTreeMap::new()));
    };
    if content.trim().is_empty() {
        raw_cache_insert(project_dir, BTreeMap::new());
        return Ok((WorkspaceConfig::default(), BTreeMap::new()));
    }
    let value: yaml_serde::Value = crate::parse_yaml(&path, content.clone())?;
    let typed: WorkspaceConfig = yaml_serde::from_value(value.clone())
        .map_err(|e| crate::Error::parse_yaml_err(&path, content.clone(), &e))?;
    let raw: BTreeMap<String, yaml_serde::Value> = yaml_serde::from_value(value)
        .map_err(|e| crate::Error::parse_yaml_err(&path, content, &e))?;
    raw_cache_insert(project_dir, raw.clone());
    Ok((typed, raw))
}

/// Path to the existing workspace yaml in `project_dir`, if any.
/// `aube-workspace.yaml` wins over `pnpm-workspace.yaml` so an
/// aube-native project's preferences override a co-existing pnpm
/// fallback. Returns `None` when neither file exists — read-or-skip
/// callers (catalog cleanup, ancestor walks) treat that as "nothing
/// to read or rewrite".
pub fn workspace_yaml_existing(project_dir: &Path) -> Option<PathBuf> {
    for name in workspace_yaml_names().iter().copied() {
        let path = project_dir.join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Resolve which workspace-yaml path a writer should mutate in
/// `project_dir`. Existing `aube-workspace.yaml` wins over
/// `pnpm-workspace.yaml`; when neither exists, falls back to
/// `aube-workspace.yaml` — aube's own filename, parallel to the
/// `aube-lock.yaml` shape we use for the lockfile.
///
/// Background: aube reads both `aube-workspace.yaml` (preferred)
/// and `pnpm-workspace.yaml` (fallback) for backward compatibility
/// with pnpm-style repos that already ship the latter. The
/// generated default flips to the aube-prefixed name so a fresh
/// project's filesystem layout matches aube's overall naming
/// (`aube-lock.yaml`, `aube-workspace.yaml`) rather than mixing
/// vendor namespaces.
///
/// Most writers should go through [`config_write_target`] instead,
/// which only resolves to the workspace yaml when one already exists
/// on disk. This raw helper is for the rare caller that genuinely
/// needs a workspace yaml path even on a fresh project (e.g. the
/// node-gyp bootstrap dummy file).
pub fn workspace_yaml_target(project_dir: &Path) -> PathBuf {
    workspace_yaml_existing(project_dir).unwrap_or_else(|| {
        // First preferred name (branded YAML if this tool has one, else the
        // shared `pnpm-workspace.yaml`).
        let fresh = workspace_yaml_names()
            .first()
            .copied()
            .unwrap_or("pnpm-workspace.yaml");
        project_dir.join(fresh)
    })
}

/// Where the next mutation of a workspace-level setting should land.
/// `pnpm.<key>` in `package.json` and the workspace yaml hold the same
/// shape for almost every aube-mutated setting (`patchedDependencies`,
/// `allowBuilds`, future settings) so there is one rule that applies
/// to all of them — see [`config_write_target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigWriteTarget {
    /// Mutate `pnpm.<key>` in `package.json` via [`edit_setting_map`].
    PackageJson,
    /// Mutate the existing workspace yaml at this path via
    /// [`edit_workspace_yaml`].
    WorkspaceYaml(PathBuf),
}

/// Pick which file a workspace-level config write should mutate. Pure
/// file-existence rule: when the workspace yaml exists, write there
/// (the pnpm v10+ canonical home, where YAML comments can document
/// each entry); otherwise write to `package.json`. We deliberately do
/// not introspect contents — a project with a workspace yaml gets all
/// its workspace-level config there even when prior entries lived in
/// `package.json`.
///
/// Used by every aube command that mutates a setting which can live in
/// either file (`aube patch-commit`, `aube patch-remove`,
/// `aube approve-builds`, install-time auto-deny seeding, …).
pub fn config_write_target(project_dir: &Path) -> ConfigWriteTarget {
    match workspace_yaml_existing(project_dir) {
        Some(path) => ConfigWriteTarget::WorkspaceYaml(path),
        None => ConfigWriteTarget::PackageJson,
    }
}
