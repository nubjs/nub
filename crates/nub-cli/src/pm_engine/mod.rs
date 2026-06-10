//! Package-manager verbs through the embedded aube engine (vendor/aube,
//! linked as a library; no subprocess).
//!
//! This module is the shared plumbing; the verbs themselves live in four
//! per-family modules:
//!
//! - [`install_family`] — dependency-graph mutation and linking (`install`,
//!   `ci`, `add`, `remove`, `update`, `link`, `patch*`, …). `install`/`ci`
//!   are live (slice 2); the rest are registered stubs.
//! - [`info_family`] — read-only project/graph/registry queries (`list`,
//!   `why`, `outdated`, `audit`, `view`, …).
//! - [`publish_family`] — registry writes, packaging, and auth (`publish`,
//!   `pack`, `version`, `login`, `dist-tag`, …).
//! - [`store_config_family`] — store/cache forensics and settings
//!   (`store`, `cache`, `config`, `cat-file`, …).
//!
//! All engine output flows through [`present`]: miette reports are rendered
//! with the `ERR_AUBE_*` → `ERR_NUB_*` / `WARN_AUBE_*` → `WARN_NUB_*`
//! rewrite, engine doc URLs stripped, message-level `aube` verb spellings
//! rebranded, and exit codes mapped via the engine's own exit table.
//!
//! # Verb registry
//!
//! [`ENGINE_VERBS`] registers the complete aube verb surface (read from
//! `vendor/aube/crates/aube/src/lib.rs::Commands`) minus two exclusion sets:
//!
//! - **nub-reserved** (collision policy: nub verbs win): `run`
//!   (+`run-script`), `exec` (+`x`), `test` (+`t`), `start`, `stop`,
//!   `restart`, `install-test` (+`it`) — the script-runner family routes to
//!   nub's own runner or stays an error, exactly as today; `node`, `pm`,
//!   `watch`, `upgrade` are nub-native namespaces (so aube's `upgrade`
//!   alias on `update` is dropped — `nub update`/`up` is dependency update,
//!   `nub upgrade` is self-update). The `External` bare-script catch-all is
//!   also out: bare `nub <script>` stays banned.
//! - **tool-identity** (they describe the aube tool, not the project):
//!   `sponsors`, `diag`, `doctor`, `completion`, `usage`, and the internal
//!   `__node-gyp-bootstrap` re-entry verb (its engine entry point is
//!   crate-private at the pinned API; wiring it needs a small upstreamable
//!   fork export — tracked in install_family's module doc).
//!
//! `install`/`i`/`ci` are *not* in the registry: they are live clap verbs
//! in `cli.rs` (SUBCOMMANDS) dispatching straight to
//! [`install_family::run_install`] / [`install_family::run_ci`]. The
//! registry covers the not-yet-wired remainder; its entries currently
//! dispatch to per-family stubs that error with a "wired in phase Surface"
//! message plus the user's real-PM fallback command.

pub mod info_family;
pub mod install_family;
pub mod present;
pub mod publish_family;
pub mod store_config_family;

pub use install_family::{CiFlags, InstallFlags, run_ci, run_install};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use aube_lockfile::LockfileKind;

/// The four engine verb families. One module per family; each family module
/// owns the wiring (args parsing, options construction, output routing) for
/// its verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Install,
    Info,
    Publish,
    StoreConfig,
}

/// One registered engine verb: its canonical spelling, accepted aliases
/// (mirroring aube's clap aliases), owning family, and — documentation for
/// the Surface phase — the aube args type the wired implementation parses.
pub struct VerbSpec {
    pub canonical: &'static str,
    pub aliases: &'static [&'static str],
    pub family: Family,
    /// The `aube::commands::…` args type this verb will parse when wired.
    /// Doc-only today (stubs never parse); kept in the table so the family
    /// fill-in work is self-describing. Read by tests only until then.
    #[allow(dead_code)]
    pub aube_args: &'static str,
}

/// The complete not-yet-wired aube verb surface, per the module doc's
/// exclusion rules. Spellings must be unique across canonicals + aliases and
/// disjoint from cli.rs's SUBCOMMANDS and PM_VERBS (asserted in tests here
/// and in cli.rs).
pub const ENGINE_VERBS: &[VerbSpec] = &[
    // ── install family: dependency-graph mutation + linking ────────────
    VerbSpec {
        canonical: "add",
        aliases: &["a"],
        family: Family::Install,
        aube_args: "commands::add::AddArgs",
    },
    VerbSpec {
        canonical: "remove",
        aliases: &["rm", "uninstall", "un", "uni"],
        family: Family::Install,
        aube_args: "commands::remove::RemoveArgs",
    },
    // aube also aliases `upgrade` here; that spelling is nub's self-update.
    VerbSpec {
        canonical: "update",
        aliases: &["up"],
        family: Family::Install,
        aube_args: "commands::update::UpdateArgs",
    },
    VerbSpec {
        canonical: "import",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::import::ImportArgs",
    },
    VerbSpec {
        canonical: "dedupe",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::dedupe::DedupeArgs",
    },
    VerbSpec {
        canonical: "prune",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::prune::PruneArgs",
    },
    VerbSpec {
        canonical: "rebuild",
        aliases: &["rb"],
        family: Family::Install,
        aube_args: "commands::rebuild::RebuildArgs",
    },
    VerbSpec {
        canonical: "fetch",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::fetch::FetchArgs",
    },
    VerbSpec {
        canonical: "link",
        aliases: &["ln"],
        family: Family::Install,
        aube_args: "commands::link::LinkArgs",
    },
    VerbSpec {
        canonical: "unlink",
        aliases: &["dislink"],
        family: Family::Install,
        aube_args: "commands::unlink::UnlinkArgs",
    },
    VerbSpec {
        canonical: "approve-builds",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::approve_builds::ApproveBuildsArgs",
    },
    VerbSpec {
        canonical: "ignored-builds",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::ignored_builds::IgnoredBuildsArgs",
    },
    VerbSpec {
        canonical: "patch",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::patch::PatchArgs",
    },
    VerbSpec {
        canonical: "patch-commit",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::patch_commit::PatchCommitArgs",
    },
    VerbSpec {
        canonical: "patch-remove",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::patch_remove::PatchRemoveArgs",
    },
    VerbSpec {
        canonical: "clean",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::clean::CleanArgs",
    },
    // `purge` is aube's alias-shaped variant of clean (commands::clean::run_purge).
    VerbSpec {
        canonical: "purge",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::clean::CleanArgs",
    },
    VerbSpec {
        canonical: "deploy",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::deploy::DeployArgs",
    },
    VerbSpec {
        canonical: "dlx",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::dlx::DlxArgs",
    },
    VerbSpec {
        canonical: "create",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::create::CreateArgs",
    },
    VerbSpec {
        canonical: "init",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::init::InitArgs",
    },
    // Workspace fanout meta-verb. Its nub-reserved nested legs (run/test/…)
    // stay errors when wired; the install-shaped legs fan out normally.
    VerbSpec {
        canonical: "recursive",
        aliases: &["multi", "m"],
        family: Family::Install,
        aube_args: "commands::recursive::RecursiveArgs",
    },
    // ── info family: read-only queries ──────────────────────────────────
    VerbSpec {
        canonical: "list",
        aliases: &["ls"],
        family: Family::Info,
        aube_args: "commands::list::ListArgs",
    },
    // `la`/`ll` are aube's hidden list-long variants (ListArgs + long=true).
    VerbSpec {
        canonical: "la",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::list::ListArgs",
    },
    VerbSpec {
        canonical: "ll",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::list::ListArgs",
    },
    VerbSpec {
        canonical: "why",
        aliases: &["w"],
        family: Family::Info,
        aube_args: "commands::why::WhyArgs",
    },
    VerbSpec {
        canonical: "outdated",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::outdated::OutdatedArgs",
    },
    VerbSpec {
        canonical: "audit",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::audit::AuditArgs",
    },
    VerbSpec {
        canonical: "licenses",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::licenses::LicensesArgs",
    },
    VerbSpec {
        canonical: "deprecations",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::deprecations::DeprecationsArgs",
    },
    VerbSpec {
        canonical: "peers",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::peers::PeersArgs",
    },
    VerbSpec {
        canonical: "query",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::query::QueryArgs",
    },
    VerbSpec {
        canonical: "check",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::check::CheckArgs",
    },
    VerbSpec {
        canonical: "bin",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::bin::BinArgs",
    },
    VerbSpec {
        canonical: "root",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::root::RootArgs",
    },
    VerbSpec {
        canonical: "sbom",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::sbom::SbomArgs",
    },
    VerbSpec {
        canonical: "view",
        aliases: &["info", "show", "v"],
        family: Family::Info,
        aube_args: "commands::view::ViewArgs",
    },
    // hidden npm-fallback upstream ("not implemented — use npm search").
    VerbSpec {
        canonical: "search",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::npm_fallback::FallbackArgs",
    },
    // ── publish family: registry writes, packaging, auth ────────────────
    VerbSpec {
        canonical: "publish",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::publish::PublishArgs",
    },
    VerbSpec {
        canonical: "pack",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::pack::PackArgs",
    },
    VerbSpec {
        canonical: "version",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::version::VersionArgs",
    },
    VerbSpec {
        canonical: "deprecate",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::deprecate::DeprecateArgs",
    },
    VerbSpec {
        canonical: "undeprecate",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::undeprecate::UndeprecateArgs",
    },
    VerbSpec {
        canonical: "dist-tag",
        aliases: &["dist-tags"],
        family: Family::Publish,
        aube_args: "commands::dist_tag::DistTagArgs",
    },
    VerbSpec {
        canonical: "unpublish",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::unpublish::UnpublishArgs",
    },
    VerbSpec {
        canonical: "login",
        aliases: &["adduser"],
        family: Family::Publish,
        aube_args: "commands::login::LoginArgs",
    },
    VerbSpec {
        canonical: "logout",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::logout::LogoutArgs",
    },
    // hidden npm-fallbacks upstream (whoami/owner/token/stage).
    VerbSpec {
        canonical: "whoami",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::npm_fallback::FallbackArgs",
    },
    VerbSpec {
        canonical: "owner",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::npm_fallback::FallbackArgs",
    },
    VerbSpec {
        canonical: "token",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::npm_fallback::FallbackArgs",
    },
    VerbSpec {
        canonical: "stage",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::npm_fallback::FallbackArgs",
    },
    // ── store/config family: store + cache forensics, settings ──────────
    VerbSpec {
        canonical: "store",
        aliases: &[],
        family: Family::StoreConfig,
        aube_args: "commands::store::StoreArgs",
    },
    VerbSpec {
        canonical: "cache",
        aliases: &[],
        family: Family::StoreConfig,
        aube_args: "commands::cache::CacheArgs",
    },
    VerbSpec {
        canonical: "cat-file",
        aliases: &[],
        family: Family::StoreConfig,
        aube_args: "commands::cat_file::CatFileArgs",
    },
    VerbSpec {
        canonical: "cat-index",
        aliases: &[],
        family: Family::StoreConfig,
        aube_args: "commands::cat_index::CatIndexArgs",
    },
    VerbSpec {
        canonical: "find-hash",
        aliases: &[],
        family: Family::StoreConfig,
        aube_args: "commands::find_hash::FindHashArgs",
    },
    VerbSpec {
        canonical: "config",
        aliases: &["c"],
        family: Family::StoreConfig,
        aube_args: "commands::config::ConfigArgs",
    },
    // hidden config get/set shorthands upstream.
    VerbSpec {
        canonical: "get",
        aliases: &[],
        family: Family::StoreConfig,
        aube_args: "commands::config::GetArgs",
    },
    VerbSpec {
        canonical: "set",
        aliases: &[],
        family: Family::StoreConfig,
        aube_args: "commands::config::SetArgs",
    },
    // hidden npm-fallbacks upstream (pkg/set-script).
    VerbSpec {
        canonical: "pkg",
        aliases: &[],
        family: Family::StoreConfig,
        aube_args: "commands::npm_fallback::FallbackArgs",
    },
    VerbSpec {
        canonical: "set-script",
        aliases: &[],
        family: Family::StoreConfig,
        aube_args: "commands::npm_fallback::FallbackArgs",
    },
];

/// Resolve a typed verb (canonical or alias) to its registry entry.
pub fn lookup_verb(name: &str) -> Option<&'static VerbSpec> {
    ENGINE_VERBS
        .iter()
        .find(|spec| spec.canonical == name || spec.aliases.contains(&name))
}

/// Dispatch a registered engine verb to its family module. `typed` is the
/// spelling the user actually wrote (echoed in errors and the PM-fallback
/// hint); `pm_hint` is the project's detected package manager.
pub fn dispatch_verb(
    spec: &'static VerbSpec,
    typed: &str,
    args: &[String],
    pm_hint: &str,
) -> Result<i32> {
    match spec.family {
        Family::Install => install_family::run_verb(spec, typed, args, pm_hint),
        Family::Info => info_family::run_verb(spec, typed, args, pm_hint),
        Family::Publish => publish_family::run_verb(spec, typed, args, pm_hint),
        Family::StoreConfig => store_config_family::run_verb(spec, typed, args, pm_hint),
    }
}

/// The shared stub error for registered-but-unwired verbs: names the verb,
/// says when it lands ("phase Surface"), and gives the user's real-PM
/// command so nobody is left stranded mid-skeleton.
pub(crate) fn stub_error(typed: &str, args: &[String], pm_hint: &str) -> anyhow::Error {
    let mut fallback = format!("{pm_hint} {typed}");
    for arg in args {
        fallback.push(' ');
        fallback.push_str(arg);
    }
    anyhow::anyhow!(
        "nub {typed}: not wired to the embedded engine yet (wired in phase Surface)\n\
         \x20\x20run it with your package manager for now:\n\
         \x20\x20\x20\x20{fallback}"
    )
}

/// One prepared engine invocation: the detected lockfile (layout policy
/// input) plus the tokio runtime the command runs on. Every family verb
/// starts by calling [`engine_session`] instead of re-deriving the
/// preflight/runtime recipe.
pub(crate) struct EngineSession {
    pub(crate) detected: Option<DetectedLockfile>,
    pub(crate) runtime: tokio::runtime::Runtime,
}

/// Build the shared engine context for one verb invocation: apply `--dir`,
/// register the brand/seam toggles, detect the project lockfile (walking
/// up), push the embedder setting defaults, and construct the runtime.
/// Idempotent at the seam level (every seam is a `OnceLock`), which fits
/// nub's one-command-per-process CLI shape.
///
/// Ordering is load-bearing: the brand preflight must run before *any*
/// engine code touches project config — even lockfile detection reads the
/// workspace yaml transitively (`detect_existing_lockfile_kind` →
/// `aube_lock_filename` → `git_branch_lockfile_enabled` → workspace-config
/// load), and the toggled getters freeze on first read. The embedder
/// defaults are the one seam that *needs* the detection result, so they
/// land after it (they feed settings resolution, which no detection-path
/// code consults).
pub(crate) fn engine_session(dir: Option<&Path>) -> Result<EngineSession> {
    apply_dir(dir)?;
    engine_brand_preflight();
    let cwd = std::env::current_dir()?;
    let detected = detect_lockfile_walk_up(&cwd);
    // Set-unless-user-set: ranks below CLI flags, env vars, and every
    // config file in the engine's settings precedence.
    aube::set_embedder_defaults(nub_setting_defaults(detected.as_ref()));
    Ok(EngineSession {
        detected,
        runtime: build_runtime()?,
    })
}

/// `--dir` / `-C` (and the global `--cwd`, which dispatch applies earlier):
/// chdir before anything reads the project. Mirrors aube's global `-C`.
fn apply_dir(dir: Option<&Path>) -> Result<()> {
    if let Some(dir) = dir {
        std::env::set_current_dir(dir)
            .with_context(|| format!("failed to change directory to {}", dir.display()))?;
    }
    Ok(())
}

pub(crate) struct DetectedLockfile {
    pub(crate) kind: LockfileKind,
    /// Directory the lockfile was found in (project / workspace root).
    pub(crate) dir: PathBuf,
}

/// Find the project's existing lockfile, walking up like the PM-redirect
/// detector does (a member dir inside a workspace has no lockfile of its own;
/// the root's lockfile governs the layout).
fn detect_lockfile_walk_up(cwd: &Path) -> Option<DetectedLockfile> {
    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        if let Some(kind) = aube_lockfile::detect_existing_lockfile_kind(&dir) {
            return Some(DetectedLockfile { kind, dir });
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Register nub's brand/seam toggles on the engine's process-wide embedder
/// seams. Called once per command (via [`engine_session`]) **before any
/// engine code reads project state** — the getters behind these setters are
/// freeze-on-first-read `OnceLock`s, and even lockfile detection reads the
/// workspace config transitively (see the ordering note on
/// [`engine_session`]). Every seam is idempotent.
fn engine_brand_preflight() {
    // Env surface: npm-compatible (`npm_config_*`) + ecosystem-neutral
    // (`CI`, proxies, …). `AUBE_*` stays invisible — nub's config contract
    // is the npm ecosystem's, not another tool's branded variables.
    aube::set_env_families(aube::EnvFamilies::NPM.union(aube::EnvFamilies::EXTERNAL));
    // Lifecycle scripts (npm_config_user_agent) and registry requests
    // identify the running tool: `nub/<ver> …`.
    aube::set_user_agent_product(format!("nub/{}", env!("CARGO_PKG_VERSION")));
    // Workspace yamls: `pnpm-workspace.yaml` only. An `aube-workspace.yaml`
    // some other tool left on disk is neither read nor chosen as the
    // fresh-write target (`approve-builds` writes its allowlist there).
    aube::set_workspace_yaml_names(&["pnpm-workspace.yaml"]);
    // package.json config namespace: `pnpm` only — an `aube` object in a
    // manifest is another tool's state; nub neither consults nor mutates
    // it (`remove`'s sidecar pruning, `--allow-build`'s fallback writes).
    aube::set_manifest_config_namespaces(&["pnpm"]);
    // `engines.aube` pins gate a tool nub's users aren't running; skip
    // them like the engine already skips `engines.pnpm`. `engines.node`
    // stays validated.
    aube::set_aube_engine_check(false);
    // `packageManager` acceptance: nub is the running tool, pnpm the
    // compatible drop-in. Inert through nub's dispatch today (the
    // guardrail runs in aube's own CLI entry, which nub bypasses), but any
    // future engine path that consults the registered names must see
    // nub's, never the engine's.
    aube::set_package_manager_names(aube::PackageManagerNames {
        self_names: vec!["nub".to_string()],
        self_version: env!("CARGO_PKG_VERSION").to_string(),
        compatible_names: vec!["pnpm".to_string()],
    });
}

/// Nub's replacement setting defaults, fed to the engine's embedder-defaults
/// tier (below all user sources — a user's `--node-linker`,
/// `npm_config_node_linker`, `.npmrc`, or workspace yaml all win):
///
/// - `defaultLockfileFormat=pnpm` — fresh projects write `pnpm-lock.yaml`.
/// - `virtualStoreDir` / `stateDir` = `node_modules/.nub` — the isolated
///   store (and the engine's install-state sidecar) live under `.nub`.
///   Corner: this replaces the engine's `<modulesDir>/.aube` derivation, so
///   a project that renames `modulesDir` without setting `virtualStoreDir`
///   gets the store at `node_modules/.nub` rather than `<modulesDir>/.nub`.
/// - `storeDir` = `$XDG_DATA_HOME/nub/store` (else `~/.local/share/nub/store`)
///   — the global CAS store lives in nub's own XDG namespace, not aube's
///   (the engine appends its `v1` schema suffix, so content lands at
///   `…/nub/store/v1`). Skipped when no home directory resolves — the
///   engine then falls back to its own default, which fails the same way
///   nub would.
/// - **KNOWN GAP — `cacheDir` is deliberately NOT set here.** The
///   packument/dlx cache call sites use `aube_store::dirs::cache_dir()`
///   (a leaf-fixed `<XDG_CACHE_HOME>/aube`) directly, and the one settings
///   accessor (`resolved_cache_dir`, vendor/aube/crates/aube/src/commands/
///   settings_context.rs) only consults the setting when `.npmrc` sets it
///   *explicitly* — the embedder-defaults tier never reaches it. Verified
///   empirically 2026-06-09: with a `cacheDir` embedder default set, a real
///   install still wrote `$XDG_CACHE_HOME/aube/packuments-full-v1/`.
///   Pushing the default would be a silent no-op, so we don't. Re-homing
///   the cache to `$XDG_CACHE_HOME/nub/pm` needs an upstreamable fork
///   change (route the `dirs::cache_dir()` call sites through the settings
///   ctx, or honor non-file sources in `resolved_cache_dir`); until then
///   the engine cache stays at `<xdg-cache>/aube` — on-disk state only,
///   never printed. A user's explicit `cache-dir` in `.npmrc` works today.
/// - Layout policy: flat-layout lockfile kinds (npm/yarn/bun) default
///   `nodeLinker` to `hoisted`; pnpm/aube kinds and fresh projects keep the
///   engine's `isolated` default (no entry pushed, so user/env settings
///   resolve exactly as in stock aube).
fn nub_setting_defaults(detected: Option<&DetectedLockfile>) -> Vec<(String, String)> {
    let mut defaults = vec![
        ("defaultLockfileFormat".to_string(), "pnpm".to_string()),
        (
            "virtualStoreDir".to_string(),
            "node_modules/.nub".to_string(),
        ),
        ("stateDir".to_string(), "node_modules/.nub".to_string()),
    ];
    if let Some(data) = nub_data_dir() {
        defaults.push((
            "storeDir".to_string(),
            data.join("store").to_string_lossy().into_owned(),
        ));
    }
    let hoisted_kind = matches!(
        detected.map(|d| d.kind),
        Some(
            LockfileKind::Npm
                | LockfileKind::NpmShrinkwrap
                | LockfileKind::Yarn
                | LockfileKind::YarnBerry
                | LockfileKind::Bun
        )
    );
    if hoisted_kind {
        defaults.push(("nodeLinker".to_string(), "hoisted".to_string()));
    }
    defaults
}

/// Nub's XDG data root (`$XDG_DATA_HOME/nub` or `~/.local/share/nub`), the
/// data-dir sibling of `nub_core::node::discovery::cache_dir`.
fn nub_data_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs_next::home_dir().map(|h| h.join(".local/share")))?;
    Some(base.join("nub"))
}

/// Process-env snapshot for `InstallOptions::env_snapshot` — same content as
/// `aube_settings::values::capture_env()` (a clone of `std::env::vars()`),
/// built locally because aube-settings isn't a direct nub dep.
pub(crate) fn env_snapshot() -> Vec<(String, String)> {
    std::env::vars().collect()
}

/// Multi-thread runtime mirroring aube's own `cli_main` shape
/// (`vendor/aube/crates/aube/src/lib.rs`): workers capped at 8 (the install
/// semaphore already gates network), blocking pool at 128 (tarball decode +
/// linker fan-out). The AUBE_TOKIO_* benchmark overrides are not honored here.
fn build_runtime() -> Result<tokio::runtime::Runtime> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(128)
        .enable_all()
        .build()
        .context("failed to build the install engine's tokio runtime")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(defaults: &'a [(String, String)], key: &str) -> Option<&'a str> {
        defaults
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn setting_defaults_pick_the_layout_from_the_lockfile_kind() {
        let dir = tempfile::tempdir().unwrap();
        let detected = |kind| DetectedLockfile {
            kind,
            dir: dir.path().to_path_buf(),
        };

        // Flat-layout kinds ⇒ nodeLinker defaults to hoisted.
        for kind in [
            LockfileKind::Npm,
            LockfileKind::YarnBerry,
            LockfileKind::Bun,
        ] {
            assert_eq!(
                get(&nub_setting_defaults(Some(&detected(kind))), "nodeLinker"),
                Some("hoisted"),
                "{kind:?} must default to the hoisted layout"
            );
        }

        // pnpm-shaped kinds and no lockfile ⇒ no entry (engine's isolated
        // default applies, user/env settings resolve as in stock aube).
        for kind in [LockfileKind::Pnpm, LockfileKind::Aube] {
            assert_eq!(
                get(&nub_setting_defaults(Some(&detected(kind))), "nodeLinker"),
                None,
                "{kind:?} must not inject a nodeLinker default"
            );
        }
        assert_eq!(
            get(&nub_setting_defaults(None), "nodeLinker"),
            None,
            "no lockfile must not inject a nodeLinker default"
        );
    }

    #[test]
    fn setting_defaults_always_carry_the_nub_identity_settings() {
        // Every engine command gets the pnpm lockfile default, the `.nub`
        // store/state location, and the nub-namespaced global dirs,
        // regardless of detection. (These ride the engine's
        // embedder-defaults tier, so any user source overrides them —
        // precedence is covered by the engine's own tests and the
        // install_engine integration tests.)
        for detected in [None, Some(LockfileKind::Npm), Some(LockfileKind::Pnpm)] {
            let dir = tempfile::tempdir().unwrap();
            let detected = detected.map(|kind| DetectedLockfile {
                kind,
                dir: dir.path().to_path_buf(),
            });
            let defaults = nub_setting_defaults(detected.as_ref());
            assert_eq!(get(&defaults, "defaultLockfileFormat"), Some("pnpm"));
            assert_eq!(get(&defaults, "virtualStoreDir"), Some("node_modules/.nub"));
            assert_eq!(get(&defaults, "stateDir"), Some("node_modules/.nub"));
            // The global store lands in nub's XDG data namespace (dev boxes
            // always resolve a home dir, so the entry is present here).
            let store = get(&defaults, "storeDir").expect("storeDir default");
            assert!(
                store.ends_with("nub/store") && !store.contains("aube"),
                "storeDir must live under nub's data namespace: {store}"
            );
            // `cacheDir` must NOT be pushed: the engine's cache paths bypass
            // the settings tier at the pinned API, so the entry would be a
            // silent no-op — see the KNOWN GAP note on nub_setting_defaults.
            assert_eq!(get(&defaults, "cacheDir"), None);
        }
    }

    // The brand-surface toggles (workspace-yaml list, manifest config
    // namespace, engines.aube check, packageManager acceptance set) are
    // process-global OnceLocks that freeze on first read, so in-process
    // assertions here would race other tests in this binary. They are
    // covered behaviorally through the spawned binary instead:
    // `tests/info_engine.rs::aube_workspace_yaml_is_not_consulted` and the
    // engines.aube case in `tests/install_engine.rs`.

    #[test]
    fn verb_registry_spellings_are_unique_and_resolvable() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for spec in ENGINE_VERBS {
            for spelling in std::iter::once(&spec.canonical).chain(spec.aliases) {
                assert!(
                    seen.insert(*spelling),
                    "duplicate engine verb spelling: {spelling}"
                );
                assert_eq!(
                    lookup_verb(spelling).map(|s| s.canonical),
                    Some(spec.canonical),
                    "{spelling} must resolve to {}",
                    spec.canonical
                );
            }
        }
        assert!(lookup_verb("definitely-not-a-verb").is_none());
    }

    #[test]
    fn verb_registry_excludes_reserved_and_tool_identity_verbs() {
        // nub-reserved spellings (collision policy) and aube tool-identity
        // verbs must never enter the registry — `upgrade` in particular is
        // nub's self-update, not aube's update alias.
        for verb in [
            "run",
            "run-script",
            "exec",
            "x",
            "test",
            "t",
            "start",
            "stop",
            "restart",
            "install-test",
            "it",
            "node",
            "pm",
            "watch",
            "upgrade",
            "install",
            "i",
            "ci",
            "sponsors",
            "diag",
            "doctor",
            "completion",
            "usage",
            "__node-gyp-bootstrap",
        ] {
            assert!(
                lookup_verb(verb).is_none(),
                "{verb} must not be a registered engine verb"
            );
        }
    }

    #[test]
    fn stub_error_names_the_verb_and_the_pm_fallback() {
        let err = stub_error("rm", &["lodash".to_string()], "pnpm");
        let msg = err.to_string();
        assert!(msg.contains("nub rm"), "{msg}");
        assert!(msg.contains("wired in phase Surface"), "{msg}");
        assert!(msg.contains("pnpm rm lodash"), "{msg}");
    }
}
