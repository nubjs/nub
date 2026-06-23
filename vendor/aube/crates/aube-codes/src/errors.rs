//! Error codes (`ERR_AUBE_*`).
//!
//! Each constant's *value* matches its identifier. The `ALL` slice
//! is the registry — it gates the generated docs page
//! (`docs/error-codes.md`, produced by the
//! `generate-error-codes-docs` binary) and the self-tests in
//! `lib.rs`. New codes go in both places: define a `pub const`,
//! then add a [`crate::CodeMeta`] entry to `ALL` carrying the
//! category, one-line description, and (optional) bespoke exit
//! code.

use crate::CodeMeta;

// ── lockfile ─────────────────────────────────────────────────────────
pub const ERR_AUBE_NO_LOCKFILE: &str = "ERR_AUBE_NO_LOCKFILE";
pub const ERR_AUBE_LOCKFILE_PARSE: &str = "ERR_AUBE_LOCKFILE_PARSE";
pub const ERR_AUBE_LOCKFILE_UNSUPPORTED_FORMAT: &str = "ERR_AUBE_LOCKFILE_UNSUPPORTED_FORMAT";
pub const ERR_AUBE_RESOLUTION_SHAPE_MISMATCH: &str = "ERR_AUBE_RESOLUTION_SHAPE_MISMATCH";
pub const ERR_AUBE_OUTDATED_LOCKFILE: &str = "ERR_AUBE_OUTDATED_LOCKFILE";
#[rustfmt::skip] pub const ERR_AUBE_LOCKFILE_DECLARATION_MISMATCH: &str = "ERR_AUBE_LOCKFILE_DECLARATION_MISMATCH";
pub const ERR_AUBE_LOCKFILE_AMBIGUOUS: &str = "ERR_AUBE_LOCKFILE_AMBIGUOUS";

// ── resolver ─────────────────────────────────────────────────────────
pub const ERR_AUBE_NO_MATCHING_VERSION: &str = "ERR_AUBE_NO_MATCHING_VERSION";
pub const ERR_AUBE_NO_MATURE_MATCHING_VERSION: &str = "ERR_AUBE_NO_MATURE_MATCHING_VERSION";
pub const ERR_AUBE_REGISTRY_ERROR: &str = "ERR_AUBE_REGISTRY_ERROR";
pub const ERR_AUBE_UNKNOWN_CATALOG: &str = "ERR_AUBE_UNKNOWN_CATALOG";
pub const ERR_AUBE_UNKNOWN_CATALOG_ENTRY: &str = "ERR_AUBE_UNKNOWN_CATALOG_ENTRY";
pub const ERR_AUBE_BLOCKED_EXOTIC_SUBDEP: &str = "ERR_AUBE_BLOCKED_EXOTIC_SUBDEP";
pub const ERR_AUBE_TRUST_DOWNGRADE: &str = "ERR_AUBE_TRUST_DOWNGRADE";
pub const ERR_AUBE_TRUST_MISSING_TIME: &str = "ERR_AUBE_TRUST_MISSING_TIME";
// `#[rustfmt::skip]` keeps the long names on a single visual line so the
// declaration list reads as a flat table — rustfmt would otherwise wrap
// to a `name: &str =\n    "name";` two-liner for any const past col 100.
#[rustfmt::skip] pub const ERR_AUBE_TRUST_EXCLUDE_INVALID_VERSION_UNION: &str = "ERR_AUBE_TRUST_EXCLUDE_INVALID_VERSION_UNION";
#[rustfmt::skip] pub const ERR_AUBE_TRUST_EXCLUDE_NAME_GLOB_WITH_VERSIONS: &str = "ERR_AUBE_TRUST_EXCLUDE_NAME_GLOB_WITH_VERSIONS";
pub const ERR_AUBE_PEER_CONTEXT_NOT_CONVERGED: &str = "ERR_AUBE_PEER_CONTEXT_NOT_CONVERGED";

// ── registry / network ──────────────────────────────────────────────
pub const ERR_AUBE_PACKAGE_NOT_FOUND: &str = "ERR_AUBE_PACKAGE_NOT_FOUND";
pub const ERR_AUBE_VERSION_NOT_FOUND: &str = "ERR_AUBE_VERSION_NOT_FOUND";
pub const ERR_AUBE_UNAUTHORIZED: &str = "ERR_AUBE_UNAUTHORIZED";
pub const ERR_AUBE_FORBIDDEN: &str = "ERR_AUBE_FORBIDDEN";
pub const ERR_AUBE_OFFLINE: &str = "ERR_AUBE_OFFLINE";
pub const ERR_AUBE_INVALID_PACKAGE_NAME: &str = "ERR_AUBE_INVALID_PACKAGE_NAME";
pub const ERR_AUBE_REGISTRY_WRITE_REJECTED: &str = "ERR_AUBE_REGISTRY_WRITE_REJECTED";
pub const ERR_AUBE_MALICIOUS_PACKAGE: &str = "ERR_AUBE_MALICIOUS_PACKAGE";
pub const ERR_AUBE_LOW_DOWNLOAD_PACKAGE: &str = "ERR_AUBE_LOW_DOWNLOAD_PACKAGE";
pub const ERR_AUBE_ADVISORY_CHECK_FAILED: &str = "ERR_AUBE_ADVISORY_CHECK_FAILED";
pub const ERR_AUBE_SECURITY_SCANNER_FATAL: &str = "ERR_AUBE_SECURITY_SCANNER_FATAL";
pub const ERR_AUBE_SECURITY_SCANNER_FAILED: &str = "ERR_AUBE_SECURITY_SCANNER_FAILED";

// ── tarball / store ─────────────────────────────────────────────────
pub const ERR_AUBE_TARBALL_INTEGRITY: &str = "ERR_AUBE_TARBALL_INTEGRITY";
pub const ERR_AUBE_TARBALL_EXTRACT: &str = "ERR_AUBE_TARBALL_EXTRACT";
pub const ERR_AUBE_PKG_CONTENT_MISMATCH: &str = "ERR_AUBE_PKG_CONTENT_MISMATCH";
pub const ERR_AUBE_TARBALL_URL_MISMATCH: &str = "ERR_AUBE_TARBALL_URL_MISMATCH";
pub const ERR_AUBE_NO_HOME: &str = "ERR_AUBE_NO_HOME";
pub const ERR_AUBE_GIT_ERROR: &str = "ERR_AUBE_GIT_ERROR";

// ── linker ──────────────────────────────────────────────────────────
pub const ERR_AUBE_LINK_FAILED: &str = "ERR_AUBE_LINK_FAILED";
pub const ERR_AUBE_PATCH_FAILED: &str = "ERR_AUBE_PATCH_FAILED";
pub const ERR_AUBE_MISSING_PACKAGE_INDEX: &str = "ERR_AUBE_MISSING_PACKAGE_INDEX";
pub const ERR_AUBE_UNSAFE_INDEX_KEY: &str = "ERR_AUBE_UNSAFE_INDEX_KEY";
pub const ERR_AUBE_UNSAFE_PACKAGE_NAME: &str = "ERR_AUBE_UNSAFE_PACKAGE_NAME";
pub const ERR_AUBE_MISSING_STORE_FILE: &str = "ERR_AUBE_MISSING_STORE_FILE";

// ── scripts ─────────────────────────────────────────────────────────
pub const ERR_AUBE_SCRIPT_SPAWN: &str = "ERR_AUBE_SCRIPT_SPAWN";
pub const ERR_AUBE_SCRIPT_NON_ZERO_EXIT: &str = "ERR_AUBE_SCRIPT_NON_ZERO_EXIT";
#[rustfmt::skip] pub const ERR_AUBE_BUILD_POLICY_UNSUPPORTED_VALUE: &str = "ERR_AUBE_BUILD_POLICY_UNSUPPORTED_VALUE";
#[rustfmt::skip] pub const ERR_AUBE_BUILD_POLICY_INVALID_VERSION_UNION: &str = "ERR_AUBE_BUILD_POLICY_INVALID_VERSION_UNION";
#[rustfmt::skip] pub const ERR_AUBE_BUILD_POLICY_WILDCARD_WITH_VERSION: &str = "ERR_AUBE_BUILD_POLICY_WILDCARD_WITH_VERSION";

// ── workspace / filter ──────────────────────────────────────────────
pub const ERR_AUBE_WORKSPACE_PARSE: &str = "ERR_AUBE_WORKSPACE_PARSE";
pub const ERR_AUBE_FILTER_EMPTY: &str = "ERR_AUBE_FILTER_EMPTY";
pub const ERR_AUBE_FILTER_GIT_IO: &str = "ERR_AUBE_FILTER_GIT_IO";
pub const ERR_AUBE_FILTER_GIT_FAILED: &str = "ERR_AUBE_FILTER_GIT_FAILED";

// ── manifest ────────────────────────────────────────────────────────
pub const ERR_AUBE_MANIFEST_PARSE: &str = "ERR_AUBE_MANIFEST_PARSE";
pub const ERR_AUBE_MANIFEST_YAML_PARSE: &str = "ERR_AUBE_MANIFEST_YAML_PARSE";

// ── engine / cli ────────────────────────────────────────────────────
pub const ERR_AUBE_UNSUPPORTED_ENGINE: &str = "ERR_AUBE_UNSUPPORTED_ENGINE";
pub const ERR_AUBE_RECURSIVE_NOT_SUPPORTED: &str = "ERR_AUBE_RECURSIVE_NOT_SUPPORTED";
pub const ERR_AUBE_UNKNOWN_COMMAND: &str = "ERR_AUBE_UNKNOWN_COMMAND";
pub const ERR_AUBE_NPM_ONLY_COMMAND: &str = "ERR_AUBE_NPM_ONLY_COMMAND";
pub const ERR_AUBE_COMPLETION_FAILED: &str = "ERR_AUBE_COMPLETION_FAILED";
pub const ERR_AUBE_REMOVE_PRIOR_INSTALL_DIR: &str = "ERR_AUBE_REMOVE_PRIOR_INSTALL_DIR";
pub const ERR_AUBE_CONFIG_NESTED_AUBE_KEY: &str = "ERR_AUBE_CONFIG_NESTED_AUBE_KEY";
pub const ERR_AUBE_CONFLICTING_BUILD_FLAGS: &str = "ERR_AUBE_CONFLICTING_BUILD_FLAGS";

// ── node runtime (devEngines.runtime / .node-version / .nvmrc) ──────
#[rustfmt::skip] pub const ERR_AUBE_RUNTIME_VERSION_UNSATISFIED: &str = "ERR_AUBE_RUNTIME_VERSION_UNSATISFIED";
#[rustfmt::skip] pub const ERR_AUBE_RUNTIME_NO_MATCHING_VERSION: &str = "ERR_AUBE_RUNTIME_NO_MATCHING_VERSION";
pub const ERR_AUBE_RUNTIME_DOWNLOAD_FAILED: &str = "ERR_AUBE_RUNTIME_DOWNLOAD_FAILED";
#[rustfmt::skip] pub const ERR_AUBE_RUNTIME_CHECKSUM_MISMATCH: &str = "ERR_AUBE_RUNTIME_CHECKSUM_MISMATCH";
pub const ERR_AUBE_RUNTIME_EXTRACT_FAILED: &str = "ERR_AUBE_RUNTIME_EXTRACT_FAILED";
#[rustfmt::skip] pub const ERR_AUBE_RUNTIME_MISE_INSTALL_FAILED: &str = "ERR_AUBE_RUNTIME_MISE_INSTALL_FAILED";
#[rustfmt::skip] pub const ERR_AUBE_RUNTIME_UNSUPPORTED_PLATFORM: &str = "ERR_AUBE_RUNTIME_UNSUPPORTED_PLATFORM";
pub const ERR_AUBE_RUNTIME_IO: &str = "ERR_AUBE_RUNTIME_IO";

// ── misc tracing::error! sites (non-fatal but high-severity) ────────
pub const ERR_AUBE_PATCHES_TRACKING_WRITE: &str = "ERR_AUBE_PATCHES_TRACKING_WRITE";
pub const ERR_AUBE_UNSAFE_SHEBANG_INTERPRETER: &str = "ERR_AUBE_UNSAFE_SHEBANG_INTERPRETER";

/// Stable category labels that group codes in the generated docs and
/// in `EXIT_TABLE`'s 10-wide allocation ranges. Public so the docs
/// generator can iterate them in a deterministic order.
pub mod category {
    pub const LOCKFILE: &str = "Lockfile";
    pub const RESOLVER: &str = "Resolver";
    pub const TARBALL_STORE: &str = "Tarball / store";
    pub const REGISTRY_NETWORK: &str = "Registry / network";
    pub const SCRIPTS: &str = "Scripts / build";
    pub const LINKER: &str = "Linker";
    pub const MANIFEST_WORKSPACE: &str = "Manifest / workspace";
    pub const ENGINE_CLI: &str = "Engine / CLI";
    pub const MISC_SAFETY: &str = "Misc / safety";
    /// Add-time / install-time supply-chain policy errors. Paired
    /// with [`crate::warnings::category::SUPPLY_CHAIN`].
    pub const SUPPLY_CHAIN: &str = "Supply chain (add-time)";
}

/// Registry of every error code with its category, description, and
/// (optional) bespoke exit code. Walked by the
/// `generate-error-codes-docs` binary and by the self-tests in
/// `lib.rs` and `exit.rs`. New codes must be added here.
pub const ALL: &[CodeMeta] = &[
    // Lockfile
    CodeMeta {
        name: ERR_AUBE_NO_LOCKFILE,
        category: category::LOCKFILE,
        description: "An operation that required a lockfile (`--frozen-lockfile`, `aube fetch`, etc.) found none in the project.",
        exit_code: Some(10),
    },
    CodeMeta {
        name: ERR_AUBE_LOCKFILE_PARSE,
        category: category::LOCKFILE,
        description: "Lockfile is structurally invalid — version guard failed, YAML shape is wrong, or `yaml_serde` couldn't round-trip the contents.",
        exit_code: Some(11),
    },
    CodeMeta {
        name: ERR_AUBE_LOCKFILE_UNSUPPORTED_FORMAT,
        category: category::LOCKFILE,
        description: "Lockfile filename was recognized but its format isn't supported on this aube version.",
        exit_code: Some(12),
    },
    CodeMeta {
        name: ERR_AUBE_RESOLUTION_SHAPE_MISMATCH,
        category: category::LOCKFILE,
        description: "A registry-style lockfile dependency path is backed by a git, local directory, or direct tarball resolution.",
        exit_code: Some(13),
    },
    CodeMeta {
        name: ERR_AUBE_OUTDATED_LOCKFILE,
        category: category::LOCKFILE,
        description: "A `--frozen-lockfile` install found the lockfile out of date with `package.json` / `pnpm-workspace.yaml` (same condition as pnpm's `ERR_PNPM_OUTDATED_LOCKFILE`).",
        exit_code: Some(16),
    },
    CodeMeta {
        name: ERR_AUBE_LOCKFILE_DECLARATION_MISMATCH,
        category: category::LOCKFILE,
        description: "`package.json` declares a package manager (`packageManager` or `devEngines.packageManager`) but the only lockfile(s) on disk belong to other tools. Regenerate the declared tool's lockfile, convert with `aube import`, or fix the declaration.",
        exit_code: Some(17),
    },
    CodeMeta {
        name: ERR_AUBE_LOCKFILE_AMBIGUOUS,
        category: category::LOCKFILE,
        description: "Lockfiles from two or more package managers coexist and `package.json` doesn't declare which tool owns the project. Remove the stale lockfile(s) or add a `packageManager` declaration.",
        exit_code: Some(18),
    },
    // Resolver
    CodeMeta {
        name: ERR_AUBE_NO_MATCHING_VERSION,
        category: category::RESOLVER,
        description: "No published version of the named package satisfies the requested range.",
        exit_code: Some(20),
    },
    CodeMeta {
        name: ERR_AUBE_NO_MATURE_MATCHING_VERSION,
        category: category::RESOLVER,
        description: "A version satisfying the range exists but every candidate was younger than `minimumReleaseAge` and `minimumReleaseAgeStrict=true`.",
        exit_code: Some(21),
    },
    CodeMeta {
        name: ERR_AUBE_BLOCKED_EXOTIC_SUBDEP,
        category: category::RESOLVER,
        description: "Transitive dep used a `git:` / `file:` / `tarball` specifier and `blockExoticSubdeps=true`.",
        exit_code: Some(22),
    },
    CodeMeta {
        name: ERR_AUBE_TRUST_DOWNGRADE,
        category: category::RESOLVER,
        description: "Picked version dropped trust evidence the prior version had (`trustPolicy=no-downgrade`).",
        exit_code: Some(23),
    },
    CodeMeta {
        name: ERR_AUBE_TRUST_MISSING_TIME,
        category: category::RESOLVER,
        description: "Registry's packument has no `time` entry for the picked version (`trustPolicy=no-downgrade`).",
        exit_code: Some(24),
    },
    CodeMeta {
        name: ERR_AUBE_UNKNOWN_CATALOG,
        category: category::RESOLVER,
        description: "A `catalog:<name>` reference was used but the catalog isn't defined.",
        exit_code: Some(25),
    },
    CodeMeta {
        name: ERR_AUBE_UNKNOWN_CATALOG_ENTRY,
        category: category::RESOLVER,
        description: "The catalog exists but has no entry for the requested package.",
        exit_code: Some(26),
    },
    CodeMeta {
        name: ERR_AUBE_PEER_CONTEXT_NOT_CONVERGED,
        category: category::RESOLVER,
        description: "Peer-context fixed-point loop hit `MAX_ITERATIONS=16` without converging — usually mutually-recursive peers.",
        exit_code: Some(27),
    },
    CodeMeta {
        name: ERR_AUBE_REGISTRY_ERROR,
        category: category::RESOLVER,
        description: "Generic registry error from inside the resolver.",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_TRUST_EXCLUDE_INVALID_VERSION_UNION,
        category: category::RESOLVER,
        description: "A `trustPolicyExclude` pattern had a non-exact version.",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_TRUST_EXCLUDE_NAME_GLOB_WITH_VERSIONS,
        category: category::RESOLVER,
        description: "A `trustPolicyExclude` pattern combined a name glob with versions.",
        exit_code: None,
    },
    // Tarball / store
    CodeMeta {
        name: ERR_AUBE_TARBALL_INTEGRITY,
        category: category::TARBALL_STORE,
        description: "Downloaded tarball's hash didn't match the lockfile's / packument's `dist.integrity`.",
        exit_code: Some(30),
    },
    CodeMeta {
        name: ERR_AUBE_TARBALL_EXTRACT,
        category: category::TARBALL_STORE,
        description: "Tarball couldn't be extracted (corrupt gzip, unexpected entry shape, etc.).",
        exit_code: Some(31),
    },
    CodeMeta {
        name: ERR_AUBE_PKG_CONTENT_MISMATCH,
        category: category::TARBALL_STORE,
        description: "Tarball's `package.json` declared a different `(name, version)` than the resolver expected (`strictStorePkgContentCheck=true`).",
        exit_code: Some(32),
    },
    CodeMeta {
        name: ERR_AUBE_GIT_ERROR,
        category: category::TARBALL_STORE,
        description: "Git operation failed during a `git:` dep prepare or checkout.",
        exit_code: Some(33),
    },
    CodeMeta {
        name: ERR_AUBE_TARBALL_URL_MISMATCH,
        category: category::TARBALL_STORE,
        description: "A registry lockfile entry's explicit tarball URL didn't match the registry metadata for that `(name, version)`.",
        exit_code: Some(34),
    },
    CodeMeta {
        name: ERR_AUBE_NO_HOME,
        category: category::TARBALL_STORE,
        description: "`HOME` (or platform equivalent) is unset, so aube can't locate its store.",
        exit_code: None,
    },
    // Registry / network
    CodeMeta {
        name: ERR_AUBE_PACKAGE_NOT_FOUND,
        category: category::REGISTRY_NETWORK,
        description: "Registry returned 404 for the package name.",
        exit_code: Some(40),
    },
    CodeMeta {
        name: ERR_AUBE_VERSION_NOT_FOUND,
        category: category::REGISTRY_NETWORK,
        description: "Package exists but the requested version doesn't.",
        exit_code: Some(41),
    },
    CodeMeta {
        name: ERR_AUBE_UNAUTHORIZED,
        category: category::REGISTRY_NETWORK,
        description: "Registry returned 401 — missing or invalid auth. Run `aube login`.",
        exit_code: Some(42),
    },
    CodeMeta {
        name: ERR_AUBE_FORBIDDEN,
        category: category::REGISTRY_NETWORK,
        // The registry/network exit band (40–49) is full; a 403 is an
        // auth-class rejection like 401, but distinct enough to warrant
        // its own code so CI can branch on it. Falls back to
        // EXIT_GENERIC — precedent: ERR_AUBE_SECURITY_SCANNER_FAILED.
        description: "Registry returned 403 — authenticated but forbidden (insufficient permissions, token scope, or blocked by policy). The registry's message is included.",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_OFFLINE,
        category: category::REGISTRY_NETWORK,
        description: "Offline mode and the requested resource isn't in the local cache.",
        exit_code: Some(43),
    },
    CodeMeta {
        name: ERR_AUBE_INVALID_PACKAGE_NAME,
        category: category::REGISTRY_NETWORK,
        description: "A name doesn't match npm's grammar — rejected before any I/O so a hostile manifest can't use the cache-path builder as a write primitive.",
        exit_code: Some(44),
    },
    CodeMeta {
        name: ERR_AUBE_REGISTRY_WRITE_REJECTED,
        category: category::REGISTRY_NETWORK,
        description: "Registry rejected a publish/deprecate/owner write with a non-2xx response.",
        exit_code: Some(45),
    },
    CodeMeta {
        name: ERR_AUBE_MALICIOUS_PACKAGE,
        category: category::REGISTRY_NETWORK,
        description: "`aube add` refused a package because OSV reports it as malicious (`MAL-*` advisory). Hard block — confirmed-malicious advisories aren't a judgement call.",
        exit_code: Some(46),
    },
    CodeMeta {
        name: ERR_AUBE_LOW_DOWNLOAD_PACKAGE,
        category: category::REGISTRY_NETWORK,
        description: "`aube add` refused a package whose weekly downloads fall below `lowDownloadThreshold` in a non-interactive context (or when stdin is not a TTY). Pass `--allow-low-downloads` to bypass.",
        exit_code: Some(47),
    },
    CodeMeta {
        name: ERR_AUBE_ADVISORY_CHECK_FAILED,
        category: category::REGISTRY_NETWORK,
        description: "`aube add` couldn't reach the OSV advisory API and `advisoryCheck = required` is set. Distinct from `ERR_AUBE_MALICIOUS_PACKAGE` so CI tooling can tell a network outage from a confirmed malicious advisory.",
        exit_code: Some(49),
    },
    CodeMeta {
        name: ERR_AUBE_SECURITY_SCANNER_FATAL,
        category: category::SUPPLY_CHAIN,
        description: "User-configured `securityScanner` returned a `fatal`-level advisory against a package the user is trying to add. Bun-style pluggable scanner contract; the scanner itself decides what counts as fatal.",
        exit_code: Some(48),
    },
    CodeMeta {
        name: ERR_AUBE_SECURITY_SCANNER_FAILED,
        category: category::SUPPLY_CHAIN,
        description: "User-configured `securityScanner` couldn't be spawned, exited non-zero, timed out, or emitted unparseable JSON. Fail-closed by design: a configured scanner that can't run is treated as a refusal, not a free pass. Set `securityScanner = \"\"` to disable the integration when bootstrapping or recovering from a broken scanner.",
        exit_code: None,
    },
    // Scripts / build
    CodeMeta {
        name: ERR_AUBE_SCRIPT_NON_ZERO_EXIT,
        category: category::SCRIPTS,
        description: "A lifecycle script (`preinstall` / `install` / `postinstall` / a `package.json` script) exited non-zero.",
        exit_code: Some(50),
    },
    CodeMeta {
        name: ERR_AUBE_SCRIPT_SPAWN,
        category: category::SCRIPTS,
        description: "Couldn't spawn a script's interpreter (shell missing, jail setup failed, etc.).",
        exit_code: Some(51),
    },
    CodeMeta {
        name: ERR_AUBE_BUILD_POLICY_UNSUPPORTED_VALUE,
        category: category::SCRIPTS,
        description: "An entry in `allowBuilds` had a value that wasn't `true`/`false`.",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_BUILD_POLICY_INVALID_VERSION_UNION,
        category: category::SCRIPTS,
        description: "An `allowBuilds` pattern's version union was unparseable.",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_BUILD_POLICY_WILDCARD_WITH_VERSION,
        category: category::SCRIPTS,
        description: "An `allowBuilds` pattern combined a wildcard name with a version union.",
        exit_code: None,
    },
    // Linker
    CodeMeta {
        name: ERR_AUBE_PATCH_FAILED,
        category: category::LINKER,
        description: "Applying a `pnpm.patchedDependencies` patch failed.",
        exit_code: Some(60),
    },
    CodeMeta {
        name: ERR_AUBE_LINK_FAILED,
        category: category::LINKER,
        description: "Symlink / junction / hardlink couldn't be created — usually permissions or filesystem support.",
        exit_code: Some(61),
    },
    CodeMeta {
        name: ERR_AUBE_MISSING_PACKAGE_INDEX,
        category: category::LINKER,
        description: "Internal: a caller skipped `load_index` but the package wasn't already materialized.",
        exit_code: Some(62),
    },
    CodeMeta {
        name: ERR_AUBE_MISSING_STORE_FILE,
        category: category::LINKER,
        description: "A package index references a CAS shard that doesn't exist on disk. Re-run install to re-fetch.",
        exit_code: Some(63),
    },
    // Manifest / workspace
    CodeMeta {
        name: ERR_AUBE_MANIFEST_PARSE,
        category: category::MANIFEST_WORKSPACE,
        description: "A `package.json` had a syntax error. miette renders a pointer at the offending byte.",
        exit_code: Some(70),
    },
    CodeMeta {
        name: ERR_AUBE_WORKSPACE_PARSE,
        category: category::MANIFEST_WORKSPACE,
        description: "An `aube-workspace.yaml` / `pnpm-workspace.yaml` was structurally invalid.",
        exit_code: Some(71),
    },
    CodeMeta {
        name: ERR_AUBE_MANIFEST_YAML_PARSE,
        category: category::MANIFEST_WORKSPACE,
        description: "A workspace YAML helper file was structurally invalid (no source pointer available).",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_FILTER_EMPTY,
        category: category::MANIFEST_WORKSPACE,
        description: "`--filter` was passed an empty selector.",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_FILTER_GIT_IO,
        category: category::MANIFEST_WORKSPACE,
        description: "A `--filter ...[ref]` selector failed to spawn `git`.",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_FILTER_GIT_FAILED,
        category: category::MANIFEST_WORKSPACE,
        description: "The git subprocess for a `--filter ...[ref]` selector exited non-zero.",
        exit_code: None,
    },
    // Engine / CLI
    CodeMeta {
        name: ERR_AUBE_UNSUPPORTED_ENGINE,
        category: category::ENGINE_CLI,
        description: "One or more packages declared an `engines` constraint incompatible with the running Node/aube and `engine-strict=true`.",
        exit_code: Some(80),
    },
    CodeMeta {
        name: ERR_AUBE_UNKNOWN_COMMAND,
        category: category::ENGINE_CLI,
        description: "The named subcommand isn't a built-in aube command and isn't a script in the manifest.",
        exit_code: Some(81),
    },
    CodeMeta {
        name: ERR_AUBE_NPM_ONLY_COMMAND,
        category: category::ENGINE_CLI,
        description: "The user invoked an npm-only command (`whoami`, `token`, `owner`, `search`, `pkg`, `set-script`) — aube doesn't implement these; use npm.",
        exit_code: Some(82),
    },
    CodeMeta {
        name: ERR_AUBE_RECURSIVE_NOT_SUPPORTED,
        category: category::ENGINE_CLI,
        description: "A command was invoked under `--recursive` but doesn't support recursive execution.",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_COMPLETION_FAILED,
        category: category::ENGINE_CLI,
        description: "`aube completion` couldn't invoke `usage` to render the shell completions.",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_REMOVE_PRIOR_INSTALL_DIR,
        category: category::ENGINE_CLI,
        description: "Couldn't clean up a prior global install dir before re-installing.",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_CONFIG_NESTED_AUBE_KEY,
        category: category::ENGINE_CLI,
        description: "`aube config set <prefix>.<sub> …` was used for a key whose prefix is an aube map setting (e.g. `allowBuilds.<pkg>`). Such nested writes would otherwise land in `.npmrc` where aube doesn't read them and npm warns/errors about the unknown key — set the map in workspace yaml or `package.json#aube.<prefix>` instead.",
        exit_code: None,
    },
    CodeMeta {
        name: ERR_AUBE_CONFLICTING_BUILD_FLAGS,
        category: category::ENGINE_CLI,
        description: "`aube add` was passed the same package name in both `--allow-build` and `--deny-build`.",
        exit_code: None,
    },
    // Node runtime (devEngines.runtime / .node-version / .nvmrc)
    CodeMeta {
        name: ERR_AUBE_RUNTIME_VERSION_UNSATISFIED,
        category: category::ENGINE_CLI,
        description: "The project requires a Node.js version that isn't available and policy forbids fetching it (`devEngines.runtime` with `onFail: \"error\"`, `runtimeOnFail=error`, or offline mode with nothing installed).",
        exit_code: Some(83),
    },
    CodeMeta {
        name: ERR_AUBE_RUNTIME_NO_MATCHING_VERSION,
        category: category::ENGINE_CLI,
        description: "No Node.js release in the dist index satisfies the requested version (bad range, unknown LTS codename, or no build for this platform).",
        exit_code: Some(84),
    },
    CodeMeta {
        name: ERR_AUBE_RUNTIME_DOWNLOAD_FAILED,
        category: category::ENGINE_CLI,
        description: "Fetching the Node.js dist index, checksum file, or release archive failed after retries.",
        exit_code: Some(85),
    },
    CodeMeta {
        name: ERR_AUBE_RUNTIME_CHECKSUM_MISMATCH,
        category: category::ENGINE_CLI,
        description: "A downloaded Node.js archive's SHA-256 didn't match the lockfile pin or `SHASUMS256.txt`. The archive is discarded and never retried automatically.",
        exit_code: Some(86),
    },
    CodeMeta {
        name: ERR_AUBE_RUNTIME_EXTRACT_FAILED,
        category: category::ENGINE_CLI,
        description: "A Node.js release archive was corrupt or contained unsafe entry paths.",
        exit_code: Some(87),
    },
    CodeMeta {
        name: ERR_AUBE_RUNTIME_MISE_INSTALL_FAILED,
        category: category::ENGINE_CLI,
        description: "Delegating a Node.js install to mise failed — `mise install node@<version>` exited non-zero or the install wasn't discoverable afterwards. Fatal only under `runtimeInstaller=mise`; `auto` falls back to aube's own download.",
        exit_code: Some(88),
    },
    CodeMeta {
        name: ERR_AUBE_RUNTIME_UNSUPPORTED_PLATFORM,
        category: category::ENGINE_CLI,
        description: "No official Node.js build exists for this OS/architecture/libc. Set `nodeDownloadMirrors` to a mirror that carries one, or install Node via mise/system.",
        exit_code: Some(89),
    },
    CodeMeta {
        name: ERR_AUBE_RUNTIME_IO,
        category: category::ENGINE_CLI,
        description: "A filesystem operation in the runtime store failed (lock acquisition, staging, or publishing an install). Not a download failure — the message names the failing path.",
        exit_code: None,
    },
    // Misc / safety
    CodeMeta {
        name: ERR_AUBE_UNSAFE_INDEX_KEY,
        category: category::MISC_SAFETY,
        description: "A package index key tried to escape its directory (path traversal defense in depth).",
        exit_code: Some(90),
    },
    CodeMeta {
        name: ERR_AUBE_UNSAFE_SHEBANG_INTERPRETER,
        category: category::MISC_SAFETY,
        description: "A `#!` shebang named an unsafe interpreter when generating a shim — substituted with `node` instead. Surfaced as `tracing::error!` but install continues.",
        exit_code: Some(91),
    },
    CodeMeta {
        name: ERR_AUBE_UNSAFE_PACKAGE_NAME,
        category: category::MISC_SAFETY,
        description: "A package/dependency alias would escape its `node_modules` slot if linked.",
        exit_code: Some(92),
    },
    CodeMeta {
        name: ERR_AUBE_PATCHES_TRACKING_WRITE,
        category: category::MISC_SAFETY,
        description: "Couldn't write `.aube-applied-patches.json` after applying patches. Non-fatal; next install may miss stale patched entries.",
        exit_code: None,
    },
];
