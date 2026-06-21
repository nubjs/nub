//! Package-manager verbs through the embedded aube engine (vendor/aube,
//! linked as a library; no subprocess).
//!
//! This module is the shared plumbing; the verbs themselves live in four
//! per-family modules:
//!
//! - [`install_family`] — dependency-graph mutation and linking (`install`,
//!   `ci`, `add`, `remove`, `update`, `link`, `patch*`, …). All are wired to
//!   the embedded engine; `install`/`ci` dispatch via live clap verbs.
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
//!   `sponsors`, `diag`, `doctor`, `completion`, `usage`. The internal
//!   `__node-gyp-bootstrap` re-entry verb is also outside the registry but
//!   IS wired — as an early intercept in cli.rs dispatching to
//!   [`run_node_gyp_bootstrap`], because the engine's lazy node-gyp shims
//!   re-invoke `current_exe()` (= nub) with it mid-lifecycle-script.
//!
//! `install`/`i`/`ci` are *not* in the registry: they are live clap verbs
//! in `cli.rs` (SUBCOMMANDS) dispatching straight to
//! [`install_family::run_install`] / [`install_family::run_ci`]. `init` is
//! not in the registry either — the spelling is reserved for nub's own
//! project init; cli.rs's bareword arm answers it with a "coming" note.
//! Every other registered verb is wired to the engine through its family
//! module, except the deliberate exclusions — `recursive` (no meta-verb;
//! use `-r`/`--filter` on the verb), `clean`/`purge` (nub doesn't delete
//! node_modules for you), `deploy` (not yet wired), and `sbom` (engine
//! branding in the document body — info_family module doc) — which error
//! with honest per-verb messages in their family dispatchers.

mod bun_config;
pub mod config_scope;
pub mod identity;
pub mod info_family;
pub mod install_family;
pub mod log;
pub mod present;
pub mod publish_family;
pub mod store_config_family;
pub mod unsupported_config;
pub mod use_align;
pub mod use_nub;

pub use install_family::{
    CiFlags, InstallFlags, WorkspaceFilterFlags, run_ci, run_dlx_for_nubx, run_install,
};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use aube_lockfile::LockfileKind;

/// Serializes tests that touch the process-global engine state — the
/// `aube_util` embedder profile (`set_embedder`, set-once) and the
/// `EngineContext` posture (`update_engine_context`, last-write-wins RwLock).
/// Any test that drives `engine_brand_preflight` / a family `run_verb`
/// (which registers `NUB` and writes `read_branded_pnpm_config` from the test
/// process's cwd) races a test that READS that posture
/// (e.g. `info_family`'s `find_workspace_root`, gated on
/// `read_branded_pnpm_config`). Both sides take this lock so the global state
/// is stable for the reader's duration. Cheap (`std::sync::Mutex`), test-only.
#[cfg(test)]
pub(crate) static ENGINE_GLOBAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    // `init` is deliberately NOT registered: the spelling is reserved for
    // nub's own project init (the maintainer owns the verb), not the engine's
    // npm-style manifest scaffold. cli.rs answers `nub init` with a
    // "nub's own init is coming" note instead of a PM redirect.
    // Workspace fanout meta-verb. Registered so it errors with the honest
    // "use -r on the verb" message rather than the generic not-a-command
    // fallback (install_family::run_verb).
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
    // Native registry full-text search (formerly an npm-only fallback).
    VerbSpec {
        canonical: "search",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::search::SearchArgs",
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
    // Native account/registry verbs (formerly npm-only fallbacks upstream).
    // `stage` is intentionally absent: it is not a real npm/pnpm command, so
    // it falls through to the unknown-command path rather than being refused.
    VerbSpec {
        canonical: "whoami",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::whoami::WhoamiArgs",
    },
    VerbSpec {
        canonical: "owner",
        aliases: &["owners"],
        family: Family::Publish,
        aube_args: "commands::owner::OwnerArgs",
    },
    VerbSpec {
        canonical: "token",
        aliases: &[],
        family: Family::Publish,
        aube_args: "commands::token::TokenArgs",
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
    // Native package.json editors (formerly npm-only fallbacks).
    VerbSpec {
        canonical: "pkg",
        aliases: &[],
        family: Family::StoreConfig,
        aube_args: "commands::pkg::PkgArgs",
    },
    VerbSpec {
        canonical: "set-script",
        aliases: &["ss"],
        family: Family::StoreConfig,
        aube_args: "commands::set_script::SetScriptArgs",
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

/// The engine's hidden node-gyp re-entry verb: `__node-gyp-bootstrap
/// <project-dir>` resolves (bootstrapping on first use) the cached
/// node-gyp and prints its executable path on stdout. The lazy shims the
/// engine drops into a project's `.bin` re-invoke `current_exe()` with
/// this verb mid-lifecycle-script — and under nub, `current_exe()` IS
/// nub — so cli.rs intercepts the spelling before clap and lands here.
/// The printed path is data for the shim (it lands under nub's own cache
/// root via the `set_cache_root` registration), so stdout is passed
/// through; failures route through the brand rewrite like every other
/// engine report.
pub(crate) fn run_node_gyp_bootstrap(args: &[String]) -> Result<i32> {
    let [project_dir] = args else {
        anyhow::bail!("usage: nub __node-gyp-bootstrap <project-dir>");
    };
    // Register nub's static identity FIRST so the bootstrap's cache lands under
    // nub's namespace (`$XDG_CACHE/nub/pm/tools/node-gyp`, via the `set_cache_root`
    // the identity carries) rather than aube's. This re-entry runs as a fresh
    // child process spawned by the engine's lazy shim (`AUBE_NODE_GYP_EXE
    // __node-gyp-bootstrap <dir>`, where `current_exe()` is nub) before any other
    // preflight, so the namespace registration has to happen here.
    engine_brand_preflight();
    // The bootstrap entry (`pub`-widened in vendor/aube @ b1a90d5: `pub mod
    // node_gyp_bootstrap` + `pub async fn {ensure_cached, print_bootstrapped_binary}`)
    // resolves/bootstraps the cached node-gyp and prints its executable path on
    // stdout for the shim to exec. Drive it on a fresh runtime; route any failure
    // through the brand rewrite like every other engine report.
    let rt = build_runtime()?;
    let project = std::path::Path::new(project_dir);
    match rt
        .block_on(aube::commands::install::node_gyp_bootstrap::print_bootstrapped_binary(project))
    {
        Ok(()) => Ok(0),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

/// The shared stub error for registered-but-unwired verbs: names the verb
/// and gives the user's real-PM command so nobody is left stranded. Every
/// *current* registration has an explicit arm (wired or an honest per-verb
/// exclusion message), so this only fires for a future verb added to the
/// registry before its family arm — a safety net, not a backlog marker.
pub(crate) fn stub_error(typed: &str, args: &[String], pm_hint: &str) -> anyhow::Error {
    let fallback = std::iter::once(format!("{pm_hint} {typed}"))
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    anyhow::anyhow!(
        "nub {typed}: not wired to the embedded engine yet\n\
         \x20\x20run it with your package manager for now:\n\
         \x20\x20\x20\x20{fallback}"
    )
}

/// One prepared engine invocation: the project's resolved PM identity
/// (layout-policy input) plus the tokio runtime the command runs on. Every
/// family verb starts by calling [`engine_session`] instead of re-deriving
/// the preflight/runtime recipe.
pub(crate) struct EngineSession {
    pub(crate) detected: Option<DetectedLockfile>,
    pub(crate) runtime: tokio::runtime::Runtime,
}

/// Build the shared engine context for one verb invocation: apply `--dir`,
/// register the brand/seam toggles, resolve the project's PM identity
/// (declared-first, walking up), push the embedder setting defaults, and
/// construct the runtime. Idempotent at the seam level (every seam is a
/// `OnceLock`), which fits nub's one-command-per-process CLI shape.
///
/// Identity resolution is the engine's declaration-aware policy
/// (`aube_lockfile::resolve_project_lockfile_kind` — pin-over-inference per
/// wiki/commands/pm/identity-policy.md, Axiom 1), so a declared PM outranks
/// stray lockfiles, a declared-but-contradicted project errors loudly here
/// (rendered through [`present`], with the `nub pm use` remedy), and an
/// undeclared multi-lockfile project errors as ambiguous instead of
/// silently picking by filename precedence.
///
/// Ordering is load-bearing: the brand preflight must run before *any*
/// engine code touches project config — even identity resolution reads the
/// workspace yaml transitively (`resolve_project_lockfile_kind` →
/// `aube_lock_filename` → `git_branch_lockfile_enabled` → workspace-config
/// load), and the toggled getters freeze on first read. The embedder
/// defaults are the one seam that *needs* the resolution result, so they
/// land after it (they feed settings resolution, which no detection-path
/// code consults).
pub(crate) fn engine_session(dir: Option<&Path>) -> Result<EngineSession> {
    engine_session_inner(dir, ConfigScopeNoise::Warn)
}

/// [`engine_session`] for the read-only / non-graph-mutating families
/// (info, publish, store-config). The config-scoping FILTER still applies —
/// `why` / `outdated` should reflect the same effective override set a real
/// install would — but the user-facing scoping *warnings* and the
/// `catalog:`-under-the-wrong-PM hard error are suppressed: those are
/// install-time UX, and surfacing them on a `nub why` would be noise. See
/// the config-scoping policy ([`config_scope`]).
pub(crate) fn engine_session_quiet(dir: Option<&Path>) -> Result<EngineSession> {
    engine_session_inner(dir, ConfigScopeNoise::Silent)
}

/// Whether [`engine_session_inner`] emits the config-scoping warnings and
/// the catalog hard-error (install-family) or stays silent (read-only
/// families) while still applying the scoping filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigScopeNoise {
    Warn,
    Silent,
}

fn engine_session_inner(dir: Option<&Path>, noise: ConfigScopeNoise) -> Result<EngineSession> {
    // Initialize the diagnostics recorder from AUBE_DIAG_* env vars so that
    // `AUBE_DIAG_SUMMARY=1 nub install` works the same as `AUBE_DIAG_SUMMARY=1
    // aube install`. The OnceLock inside diag::init() makes this idempotent.
    aube_util::diag::init();
    apply_dir(dir)?;
    engine_brand_preflight();
    let cwd = std::env::current_dir()?;
    let detected = resolve_identity_walk_up(&cwd)?;
    // Role-first lifecycle UA (two-mode model, the maintainer 2026-06-10): in compat
    // mode nub plays the incumbent PM's role completely, so the UA dep
    // postinstalls sniff leads with that PM's token (`pnpm/<ver> nub/<ver>
    // node/v<ver> …`, nub always second); under nub identity or in a fresh
    // project the first token is nub's. Lifecycle-only — the registry UA and
    // stream-time tool naming stay on the `nub/<ver>` product identity set in
    // [`engine_brand_preflight`] (telemetry never lies).
    let lifecycle_ua = lifecycle_ua_product(detected.as_ref(), &cwd);
    aube_util::update_engine_context(|c| c.lifecycle_user_agent_product = Some(lifecycle_ua));
    // Config-scoping policy (CP-3): mirror the active PM's graph-shaping
    // config (pins/catalogs), never silently. Computed AFTER identity
    // resolves (it needs the role) and BEFORE the embedder defaults / engine
    // run so the scoped override source and trusted-deps toggle are in place
    // when the resolver reads them. Filter applies in every family; warnings
    // + the catalog hard-error are install-family only (see `noise`).
    apply_config_scope(detected.as_ref(), &cwd, noise)?;
    // Warn once at install time when the project requests Plug'n'Play
    // (nodeLinker: pnp in .yarnrc.yml) but nub will install a node_modules
    // tree instead — the embedder default below forces `hoisted` and the
    // .yarnrc.yml is not re-read anywhere in the install path, so without this
    // the divergence is entirely silent.
    if noise == ConfigScopeNoise::Warn {
        warn_if_pnp_requested(detected.as_ref(), &cwd);
    }
    // Truly-fresh = no PM-preference signal anywhere (no lockfile, no
    // declaration, no pnpm-named file). On this path nub claims identity via the
    // neutral lockfile: the embedder default flips to `lock.yaml`, the quiet
    // identity marker the classifier reads back as nub. Any incumbent signal
    // makes the project pnpm-shaped/compat and is respected untouched. nub does
    // NOT auto-stamp `package.json` here — that exclusivity claim is reserved
    // for the explicit `nub pm use nub` command.
    let truly_fresh = is_truly_fresh_project(&cwd, detected.as_ref());
    // Set-unless-user-set: ranks below CLI flags, env vars, and every
    // config file in the engine's settings precedence.
    aube_settings::set_embedder_defaults(nub_setting_defaults(detected.as_ref(), truly_fresh));
    // Route the engine's lifecycle scripts through nub's runtime augmentation
    // (project-pinned + augmented Node, shim on PATH, preload) — the SAME
    // augmentation `nub run` / `nub exec` apply, so run / exec / lifecycle
    // share one source. Closes the ABI bug where dep build scripts (node-gyp)
    // compiled against ambient Node instead of the project's. Default-empty
    // overlay when augmentation can't engage ⇒ behavior preserved.
    apply_lifecycle_augmentation(&cwd);
    Ok(EngineSession {
        detected,
        runtime: build_runtime()?,
    })
}

/// The config-derived install knobs the IMPLEMENT-wins resolve from the active
/// PM's persistent config: a dependency-selection axis pin, a frozen-install
/// request, and the yarn block-all-scripts opt-out. Composed onto the install
/// args by [`install_family::run_install`] / `run_ci`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct InstallConfigSignals {
    pub(crate) dep_selection: unsupported_config::DepSelectionConfig,
    pub(crate) frozen: bool,
    pub(crate) scripts_disabled: bool,
    /// yarn `enableNetwork: false` (Berry) — forces an offline install. OR'd
    /// onto the `--offline` CLI flag in `run_install`/`run_ci`.
    pub(crate) offline: bool,
}

/// Resolve the config-derived install knobs for one install/ci invocation from
/// the resolved session identity. Reads the active PM's persistent config
/// (npm `.npmrc` `omit`/`include`, bun bunfig `production`/`frozenLockfile`,
/// yarn `.yarnrc.yml` `enableImmutableInstalls`/`immutablePatterns`/
/// `enableScripts`). Returns all-default when no identity resolves.
pub(crate) fn install_config_signals(session: &EngineSession) -> InstallConfigSignals {
    let Some((role, root)) = session_role_root(session) else {
        return InstallConfigSignals::default();
    };
    let root = root.as_path();
    InstallConfigSignals {
        dep_selection: unsupported_config::dep_selection_from_config(role, root)
            .unwrap_or_default(),
        frozen: unsupported_config::frozen_from_config(role, root),
        scripts_disabled: unsupported_config::yarn_scripts_disabled(role, root),
        offline: unsupported_config::yarn_network_disabled(role, root),
    }
}

/// Resolve the active-PM [`Role`] + project root for a session, mirroring
/// [`install_config_signals`]'s identity resolution. `None` when no lockfile /
/// identity resolves (an undetected session has no PM config to read).
pub(crate) fn session_role_root(
    session: &EngineSession,
) -> Option<(config_scope::Role, std::path::PathBuf)> {
    let detected = session.detected.as_ref()?;
    let declared = nub_core::pm::resolve::declared_pm_raw(&detected.dir);
    let role = config_scope::role_of(
        declared.as_ref().map(|(n, _)| n.as_str()),
        Some(detected.kind),
    )
    .unwrap_or(config_scope::Role::Nub);
    Some((role, detected.dir.clone()))
}

/// Apply the config-scoping policy for one verb invocation: resolve the
/// active-PM role, scope the manifest's graph-shaping override fields to
/// that role's dialect, register the scoped source + trusted-deps toggle on
/// the aube seam, and — for install-family verbs (`noise == Warn`) — emit
/// the dim per-field ignore warnings and hard-error on a `catalog:`
/// specifier under a role that doesn't honor catalogs.
///
/// The override FILTER applies in every family (so read-only queries see the
/// same effective set an install would); only the warning/error surface is
/// gated by `noise`. A missing or unparseable root manifest is not an error
/// here — the engine surfaces that on its own path; we just leave the seams
/// at their upstream defaults.
fn apply_config_scope(
    detected: Option<&DetectedLockfile>,
    cwd: &Path,
    noise: ConfigScopeNoise,
) -> Result<()> {
    use config_scope::Role;

    let root = detected.map(|d| d.dir.as_path()).unwrap_or(cwd);
    let manifest_path = root.join("package.json");
    let Ok(manifest) = aube_manifest::PackageJson::from_path(&manifest_path) else {
        return Ok(());
    };

    let declared = nub_core::pm::resolve::declared_pm_raw(cwd);
    let role = config_scope::role_of(
        declared.as_ref().map(|(n, _)| n.as_str()),
        detected.map(|d| d.kind),
    )
    // Fresh, undeclared, no lockfile: nub identity (its un-branded
    // cross-tool fields), matching the brand-symmetric default.
    .unwrap_or(Role::Nub);
    let (major, minor) = declared
        .as_ref()
        .and_then(|(_, v)| v.as_deref())
        .map(parse_major_minor)
        .unwrap_or((None, None));

    // Scope the override sources to the role's dialect.
    let tagged = config_scope::gather_tagged_overrides(&manifest);
    let (effective, ignored) = config_scope::scope_overrides(role, major, minor, &tagged);

    // Register the scoped source as the engine's sole override source, and
    // the trusted-deps toggle (only bun, only below the major that dropped
    // it, honors `trustedDependencies`). Both are idempotent OnceLocks.
    let trusted = config_scope::honors_trusted_dependencies(role, major);
    aube_util::update_engine_context(|c| {
        c.embedder_overrides = Some(effective);
        c.trusted_dependencies_honored = trusted;
    });

    if noise == ConfigScopeNoise::Warn {
        // Catalog hard-error: a role that doesn't honor `catalog:` specifiers
        // (npm/yarn/bun, pnpm<9) must mirror the real PM and refuse, rather
        // than silently mis-resolve. nub-branded, role-named.
        if !role_honors_catalog(role, major, minor)
            && let Some(spec) = first_catalog_specifier(&manifest, root)
        {
            return Err(catalog_unsupported_error(role, &spec));
        }
        emit_scope_warnings(role, &ignored);

        // Curated unsupported-config scan: FATAL-abort on the genuinely-hard
        // load-bearing fields nub can't honor (a different graph the user
        // wouldn't know about), WARN on the non-load-bearing-but-unsupported
        // set. Runs last so the override scoping above is already applied.
        match unsupported_config::scan_unsupported_config(role, major, minor, root) {
            unsupported_config::ScanResult::Fatal(err) => return Err(err),
            unsupported_config::ScanResult::Warn(extra) => emit_scope_warnings(role, &extra),
        }
    }
    Ok(())
}

/// Does the active PM honor `catalog:` specifiers? pnpm@9+ and bun@1.2+
/// implement catalogs; npm and yarn do not. nub identity honors catalogs
/// (an un-branded cross-tool field, like `workspaces`). aube resolves both
/// dialects: pnpm's `pnpm.catalog(s)` / `pnpm-workspace.yaml` AND bun's
/// `workspaces.catalog(s)` in `package.json` (see aube's `discover_catalogs`),
/// so honoring bun here resolves the real catalog rather than mis-failing a
/// project that works under bun.
fn role_honors_catalog(role: config_scope::Role, major: Option<u64>, minor: Option<u64>) -> bool {
    use config_scope::Role;
    match role {
        // pnpm gained catalogs in 9.0.
        Role::Pnpm => major.map(|m| m >= 9).unwrap_or(true),
        // bun gained catalogs in 1.2.0. Absent/unparseable version → assume a
        // modern bun and honor (matching the pnpm "assume modern" default).
        Role::Bun => match (major, minor) {
            (Some(m), Some(mi)) => (m, mi) >= (1, 2),
            (Some(m), None) => m >= 2,
            _ => true,
        },
        Role::Nub => true,
        Role::Npm | Role::Yarn => false,
    }
}

/// Find the first `catalog:`-prefixed specifier anywhere the resolver would
/// seed one, returning `"<name>: <spec>"` for the error message. Pre-resolve
/// scan — the resolver would reach the same specifier later, but mirroring the
/// PM means refusing up front with a clear, role-named message instead of
/// silently dropping the dep from the written lockfile.
///
/// The resolver seeds `catalog:` refs from THREE places, all covered here:
///   1. the root manifest's `dependencies` / `devDependencies` /
///      `optionalDependencies` (NOT `peerDependencies` — the seed never reads a
///      peer range, see `aube-resolver/src/resolve/seed.rs`);
///   2. EVERY workspace-member manifest's same three dep maps (the seed iterates
///      all importers);
///   3. `overrides` VALUES like `{"pkg": "catalog:"}` (root-level; handled by the
///      override path in `aube-resolver/src/catalog.rs`).
fn first_catalog_specifier(manifest: &aube_manifest::PackageJson, root: &Path) -> Option<String> {
    // (1) root manifest dep maps.
    if let Some(hit) = first_catalog_in_dep_maps(manifest) {
        return Some(hit);
    }

    // (3) override values (root only — npm/pnpm/bun read overrides from the root
    // manifest). A `catalog:`-valued override is a real catalog reference.
    for o in config_scope::gather_tagged_overrides(manifest) {
        if o.value.starts_with("catalog:") {
            return Some(format!("override {}: {}", o.key, o.value));
        }
    }

    // (2) workspace-member manifests' dep maps. Each importer is seeded
    // independently, so a member-only `catalog:` ref must refuse too.
    if let Ok(members) = aube_workspace::find_workspace_packages(root) {
        for dir in members {
            let Ok(member) = aube_manifest::PackageJson::from_path(&dir.join("package.json"))
            else {
                continue;
            };
            if let Some(hit) = first_catalog_in_dep_maps(&member) {
                let label = member.name.as_deref().unwrap_or_else(|| {
                    dir.file_name().and_then(|n| n.to_str()).unwrap_or("member")
                });
                return Some(format!("{label} → {hit}"));
            }
        }
    }

    None
}

/// First `catalog:` specifier in a manifest's `dependencies` /
/// `devDependencies` / `optionalDependencies` (peerDependencies excluded — the
/// resolver never seeds catalog from it).
fn first_catalog_in_dep_maps(manifest: &aube_manifest::PackageJson) -> Option<String> {
    let maps = [
        &manifest.dependencies,
        &manifest.dev_dependencies,
        &manifest.optional_dependencies,
    ];
    for map in maps {
        for (name, spec) in map.iter() {
            if spec.starts_with("catalog:") {
                return Some(format!("{name}: {spec}"));
            }
        }
    }
    None
}

/// Hard error mirroring the active PM's refusal of a `catalog:` specifier —
/// nub-branded, role-named, with the remedy.
fn catalog_unsupported_error(role: config_scope::Role, spec: &str) -> anyhow::Error {
    let pm = role.display();
    anyhow::anyhow!(
        "nub: `catalog:` specifier ({spec}) is not supported — this project uses {pm}, \
         which doesn't implement catalogs (pnpm@9+ and bun@1.2+ do). Inline the version, or switch \
         the project to a PM that supports catalogs (`nub pm use pnpm`)."
    )
}

/// Parse the leading `<major>.<minor>` out of a declared `packageManager`
/// version token. Tolerant of ranges/dist-tags (`^9`, `latest`) — returns
/// `None` for any component it can't read, which the matrix treats as
/// "assume modern/honoring".
pub(crate) fn parse_major_minor(version: &str) -> (Option<u64>, Option<u64>) {
    let trimmed = version.trim_start_matches(['^', '~', '>', '=', '<', 'v', ' ']);
    let mut parts = trimmed.split('.');
    let major = parts.next().and_then(|p| {
        let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<u64>().ok()
    });
    let minor = parts.next().and_then(|p| {
        let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<u64>().ok()
    });
    (major, minor)
}

/// Emit one dim warning line per graph-shaping field nub ignored under the
/// active PM's dialect. Color-gated: dim only when stderr is a terminal (or
/// `FORCE_COLOR` set) and `NO_COLOR` is unset; otherwise plain. Suppressed
/// entirely when nothing was ignored (the common, portable case).
fn emit_scope_warnings(role: config_scope::Role, ignored: &[config_scope::IgnoredField]) {
    if ignored.is_empty() {
        return;
    }
    let pm = role.display();
    let dim = scope_warning_uses_dim();
    for f in ignored {
        let line = format!(
            "nub: `{}` ignored — this project uses {pm}, which doesn't apply it. {}.",
            f.field, f.fix
        );
        if dim {
            eprintln!("\x1b[2m{line}\x1b[0m");
        } else {
            eprintln!("{line}");
        }
    }
}

/// Whether the scoping warning should be dim-styled: stderr is a terminal
/// (or `FORCE_COLOR` is set) AND `NO_COLOR` is unset.
pub(crate) fn scope_warning_uses_dim() -> bool {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stderr().is_terminal() || std::env::var_os("FORCE_COLOR").is_some()
}

/// The pnpm version the role-first UA advertises for a pnpm-role project with
/// no pinned version — the engine's parity claim (full pnpm-v11 settings
/// catalog + v11 build-policy posture; see the pnpm-11 compat decision,
/// epics/v0.2-aube). A `packageManager`/`devEngines` pin always outranks this:
/// the UA impersonates the pinned version when one exists.
pub(crate) const PNPM_PARITY_VERSION: &str = "11.3.0";

/// Compose the lifecycle-script UA product tokens for the resolved role —
/// everything before the `<os> <arch>` tail the engine appends. The dialect is
/// the runner's (`crates/nub-core/src/workspace/scripts.rs`): pnpm's UA shape,
/// `node/v<ver>` included, so postinstall sniffers parse one format whether a
/// script ran under `nub run` or an engine verb.
///
/// Role resolution mirrors identity (declaration first, lockfile kind second,
/// fresh last): the declared name wins even when its pin is unusable for
/// provisioning — the project *said* who manages it — and the version token is
/// the pinned version when declared, else the engine's parity version for
/// pnpm and `?` (pnpm's own convention for an unknown version) for the roles
/// whose real tool nub does not embed.
fn lifecycle_ua_product(detected: Option<&DetectedLockfile>, cwd: &Path) -> String {
    let node_version = nub_core::node::discovery::discover_node(cwd)
        .map(|n| n.version.to_string())
        .unwrap_or_else(|_| "?".to_string());
    compose_lifecycle_ua(
        nub_core::pm::resolve::declared_pm_raw(cwd),
        detected.map(|d| d.kind),
        &node_version,
    )
}

/// Role-aware lifecycle UA *product tokens* for the `nub run` / `nub exec`
/// script path (`crates/nub-cli/src/cli.rs::build_script_command`), so a
/// run-script reports the same incumbent-first UA the engine's lifecycle path
/// already sends (`pnpm/<ver> nub/<ver> node/v<ver>` in compat mode, `nub/...`
/// first under nub identity / fresh). Resolves the project's PM identity by
/// walking up from `cwd` exactly as the engine does, then defers to the shared
/// composer — there is no second hardcoded UA. The caller (`npm_env`) appends
/// the `<os> <arch>` platform tail in the runner's vocabulary. `node_version`
/// is threaded in from the run path's single Node discovery so this does not
/// re-discover. Falls back to the nub-first product on an identity error
/// (a malformed/ambiguous lockfile is surfaced loudly elsewhere; the UA must
/// never panic a script spawn).
pub(crate) fn run_lifecycle_ua_product(cwd: &Path, node_version: &str) -> String {
    let detected = resolve_identity_walk_up(cwd).ok().flatten();
    compose_lifecycle_ua(
        nub_core::pm::resolve::declared_pm_raw(cwd),
        detected.map(|d| d.kind),
        node_version,
    )
}

/// Pure core of [`lifecycle_ua_product`] (unit-tested without a fixture).
fn compose_lifecycle_ua(
    declared: Option<(String, Option<String>)>,
    kind: Option<LockfileKind>,
    node_version: &str,
) -> String {
    let nub_version = env!("CARGO_PKG_VERSION");
    // The declared name is the role when it names an identity nub recognizes;
    // an unknown tool name (vlt, deno, …) falls through to the lockfile kind,
    // exactly like identity resolution does. Role mapping is shared with the
    // config-scoping policy ([`config_scope::role_of`]) so the two never
    // diverge; the UA needs the declared *version* token too, kept here.
    let declared_role = declared
        .as_ref()
        .filter(|(name, _)| matches!(name.as_str(), "npm" | "pnpm" | "yarn" | "bun" | "nub"))
        .map(|(name, version)| (name.clone(), version.clone()));
    let role = config_scope::role_of(declared.as_ref().map(|(n, _)| n.as_str()), kind).map(|r| {
        match r {
            config_scope::Role::Npm => "npm",
            config_scope::Role::Pnpm => "pnpm",
            config_scope::Role::Yarn => "yarn",
            config_scope::Role::Bun => "bun",
            config_scope::Role::Nub => "nub",
        }
        .to_string()
    });
    match role.as_deref() {
        // Compat mode: the incumbent's token first, nub always second. The
        // version is the pin when the declaration supplied one, else the
        // engine's parity version (pnpm) or `?` (roles nub doesn't embed).
        Some(other) if other != "nub" => {
            let version = declared_role
                .and_then(|(_, version)| version)
                .unwrap_or_else(|| match other {
                    "pnpm" => PNPM_PARITY_VERSION.to_string(),
                    _ => "?".to_string(),
                });
            format!("{other}/{version} nub/{nub_version} node/v{node_version}")
        }
        // Nub identity or a fresh project: nub first, byte-identical to the
        // runner's dialect (`nub/<v> npm/? node/v<ver>`).
        _ => format!("nub/{nub_version} npm/? node/v{node_version}"),
    }
}

/// Convert nub's runtime augmentation into the generic `(env_overlay,
/// path_prepends)` that aube applies to every lifecycle-script spawn. This is
/// the ONE augmentation source `nub run` / `nub exec` already use — feeding it
/// to the engine's lifecycle path makes run / exec / lifecycle scripts share
/// identical augmentation and closes the ABI bug where dep build scripts
/// (node-gyp) compiled against the *ambient* Node instead of the project's
/// provisioned one.
///
/// `node_execpath` is the resolved/provisioned Node binary; it pins
/// `npm_node_execpath` so node-gyp builds against the project's Node even when
/// no shim is set up (re-entrant / broken install). The shim dir (when present)
/// fronts PATH and backs `$NODE` so a bare `node` or `$NODE child.js` in a
/// build script re-enters nub augmented — identical to `nub run`'s spawn env.
fn augmentation_to_lifecycle_overlay(
    aug: &nub_core::node::spawn::AugmentationEnv,
    node_execpath: &str,
) -> (Vec<(std::ffi::OsString, std::ffi::OsString)>, Vec<PathBuf>) {
    use std::ffi::OsString;
    let mut overlay: Vec<(OsString, OsString)> = Vec::new();
    // $NODE → the shim (→ nub) so userland `$NODE child.js` / `spawn(env.NODE)`
    // in a build script stays augmented, exactly as build_script_command sets it.
    if let Some(node_shim) = aug.node_shim_exe() {
        overlay.push((OsString::from("NODE"), node_shim));
    }
    if let Some(opts) = &aug.node_options {
        overlay.push((OsString::from("NODE_OPTIONS"), OsString::from(opts)));
    }
    if let Some(node_path) = &aug.node_path {
        overlay.push((OsString::from("NODE_PATH"), node_path.clone()));
    }
    // localStorage-neutralize signal for dependency build scripts' node children
    // (webstorage flag-needed band, no user --localstorage-file); preload reads + deletes.
    aug.apply_localstorage_env(|k, v| {
        overlay.push((OsString::from(k), OsString::from(v)));
    });
    // Pin npm_node_execpath to the provisioned Node — the ABI fix. Independent
    // of the shim: it flows even on the no-shim path so node-gyp never falls
    // back to ambient. (npm_node_execpath stays the REAL binary, not the shim:
    // tooling derives Node's install prefix from it.)
    overlay.push((
        OsString::from("npm_node_execpath"),
        OsString::from(node_execpath),
    ));

    let prepends = aug
        .shim_dir
        .as_deref()
        .map(|d| vec![PathBuf::from(d)])
        .unwrap_or_default();
    (overlay, prepends)
}

/// Install nub's runtime augmentation onto the engine's lifecycle-script spawn
/// env (via aube's generic [`aube::set_script_settings`] overlay), so dependency
/// build scripts run under the project's provisioned + augmented Node — the same
/// env `nub run` / `nub exec` give scripts. No-op (overlay stays default-empty,
/// behavior preserved) when augmentation can't be computed (compat / re-entrant
/// / broken install). Called once per command from [`engine_session`].
fn apply_lifecycle_augmentation(cwd: &Path) {
    let Ok(nub_binary) = nub_core::node::spawn::current_nub_binary() else {
        return;
    };
    // The project's Node — pin-aware (`.nvmrc`/`.node-version`/`engines`), NOT
    // the ambient PATH node. This resolved version drives flag injection and its
    // path pins npm_node_execpath. Mirrors build_script_command's discovery.
    let node = nub_core::node::discovery::discover_node(cwd)
        .unwrap_or_else(|_| nub_core::node::discovery::ResolvedNode::fallback());
    let pnp_ctx = nub_core::pnp::detect(cwd);
    let Some(aug) = nub_core::node::spawn::compute_augmentation_env(
        &nub_binary,
        node.version,
        // Lifecycle scripts are never compat: PM verbs run augmented (there is
        // no `--node` lifecycle path).
        false,
        pnp_ctx.as_ref().map(|c| c.pnp_cjs.as_path()),
    ) else {
        return;
    };
    let (env_overlay, path_prepends) = augmentation_to_lifecycle_overlay(&aug, node.path.as_str());
    // Hand the overlay to the engine via the runtime context. aube copies
    // `env_overlay` / `path_prepends` from `engine_context()` into its
    // `ScriptSettings` at settings-resolution time (the spawn path composes them
    // onto PATH/env); aube's own ScriptSettings fields (node_options, the UA,
    // etc.) are filled by the engine separately, so setting just these two here
    // is the equivalent of the old snapshot-merge.
    aube_util::update_engine_context(|c| {
        c.env_overlay = env_overlay;
        c.path_prepends = path_prepends;
    });
}

/// `--dir` / `-C` (and the global `--cwd`, which dispatch applies earlier):
/// chdir before anything reads the project. Mirrors aube's global `-C`.
/// `pub(crate)` for the verbs that deliberately skip [`engine_session`]'s
/// identity resolution (`import` — see its module note).
pub(crate) fn apply_dir(dir: Option<&Path>) -> Result<()> {
    if let Some(dir) = dir {
        std::env::set_current_dir(dir)
            .with_context(|| format!("failed to change directory to {}", dir.display()))?;
    }
    Ok(())
}

pub(crate) struct DetectedLockfile {
    pub(crate) kind: LockfileKind,
    /// Directory the identity resolved in (project / workspace root).
    pub(crate) dir: PathBuf,
    /// True when the kind comes from the manifest declaration alone
    /// (`ResolvedLockfileKind::DeclaredFresh`) — no lockfile exists on disk
    /// yet. The yarn write gate branches on this: a fresh declared-yarn
    /// install would *create* yarn.lock, which is gated.
    pub(crate) fresh: bool,
}

/// Resolve the project's PM identity, walking up like the PM-redirect
/// detector does (a member dir inside a workspace has no lockfile or
/// declaration of its own; the root's governs the layout). Per level the
/// engine's declaration-aware policy applies — declaration first, lockfile
/// inference second; contradiction/ambiguity are loud errors carrying the
/// `nub pm use` remedy.
fn resolve_identity_walk_up(cwd: &Path) -> Result<Option<DetectedLockfile>> {
    use aube_lockfile::ResolvedLockfileKind;
    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        match aube_lockfile::resolve_project_lockfile_kind(&dir) {
            Ok(ResolvedLockfileKind::Existing(kind)) => {
                return Ok(Some(DetectedLockfile {
                    kind,
                    dir,
                    fresh: false,
                }));
            }
            Ok(ResolvedLockfileKind::DeclaredFresh(kind)) => {
                return Ok(Some(DetectedLockfile {
                    kind,
                    dir,
                    fresh: true,
                }));
            }
            // Nothing at this level decides the identity — keep walking.
            Ok(ResolvedLockfileKind::Fresh) => {}
            Err(err) => return Err(identity_error(err)),
        }
        if !dir.pop() {
            break;
        }
    }
    Ok(None)
}

/// Render the engine's structured identity errors (contradiction /
/// ambiguity) for nub's surface: same message and stable code (rewritten
/// `ERR_AUBE_*` → `ERR_NUB_*` by [`present`]), with nub's remedy in place
/// of the engine's (`aube import` is not the verb nub users reach for —
/// `nub pm use` is the one-command fix for both states). Exit code is the
/// generic 1 (the session-build path has no per-code exit channel); the
/// stable code string in the output is the contract scripts can branch on.
fn identity_error(err: aube_lockfile::Error) -> anyhow::Error {
    use aube_lockfile::Error as E;
    const REMEDY: &str = "set the declaration: nub pm use <pm> — or remove the stale lockfile";
    let report = match &err {
        E::DeclarationMismatch {
            declared,
            field,
            expected,
            found,
        } => miette::miette!(
            code = aube_codes::errors::ERR_AUBE_LOCKFILE_DECLARATION_MISMATCH,
            help = REMEDY,
            "package.json declares `{declared}` (via `{field}`), but {expected} is missing — \
             found {found} instead"
        ),
        E::AmbiguousLockfiles { found } => miette::miette!(
            code = aube_codes::errors::ERR_AUBE_LOCKFILE_AMBIGUOUS,
            help = REMEDY,
            "multiple lockfiles found: {found} — cannot tell which package manager owns this \
             project"
        ),
        // Any other detection failure (unreadable lockfile, parse error)
        // renders as-is through the same brand rewrite.
        other => miette::miette!("{other}"),
    };
    anyhow::anyhow!("{}", present::render_report(&report))
}

/// Register nub's brand/seam toggles on the engine's process-wide embedder
/// seams. Called once per command (via [`engine_session`]) **before any
/// engine code reads project state** — the getters behind these setters are
/// freeze-on-first-read `OnceLock`s, and even lockfile detection reads the
/// workspace config transitively (see the ordering note on
/// [`engine_session`]). Every seam is idempotent.
pub(crate) fn engine_brand_preflight() {
    // Static identity FIRST, before anything reads project state or branding.
    // The whole compile-time profile — name, `nub/<ver>` UA, `lock.yaml`
    // canonical lockfile, `["nub"]`/`["pnpm"]` detection names, and the five
    // embedder-fixed toggles (engines-self check OFF, runtime-switching OFF,
    // warm-store-verify OFF, canonical-lockfile-always-wins OFF, self-update
    // OFF) — lives on [`identity::NUB`] and is registered once here (set-once
    // OnceLock; idempotent). This replaces the old scatter of
    // `set_user_agent_product` / `set_aube_lock_base_filename` /
    // `set_detection_self_names` / `set_canonical_lockfile_always_wins` /
    // `set_aube_engine_check` / `set_runtime_switching_enabled` /
    // `set_warm_store_verify` / `set_package_manager_names` seam calls. It also
    // carries the cache/data namespaces (`nub/pm`, `nub`), reproducing the old
    // `set_cache_root($XDG_CACHE/nub/pm)` for the packument / git-clone /
    // node-gyp caches that derive from `aube_store::dirs::cache_dir()`.
    //
    // Env contract: the Nub profile does not read aube-branded settings env.
    // Standalone aube keeps `AUBE_*`; Nub resolves user-facing PM knobs through
    // neutral npm config env (`npm_config_*`) or Nub-owned env where the
    // embedder profile defines one. This preserves the public brand boundary
    // while keeping aube's standalone surface intact.
    //
    // Resolver primer cache (RESOLVED by the brand-boundary-env migration): the
    // primer now derives its cache dir from the embedder's `cache_namespace`, so
    // under nub it lands at `…/nub/pm/primer` (not aube's name), and its env
    // override is read via `config_env("CACHE_DIR")` → `NUB_CACHE_DIR` under nub
    // (the branded `AUBE_CACHE_DIR` is never read under nub).
    identity::register();
    // Config surface follows role (two-mode model, the maintainer 2026-06-10): under
    // NUB identity the pnpm surface is OFF — `pnpm-workspace.yaml` unread and
    // the `package.json#pnpm.*` namespace not consulted (the `manifest_namespace
    // = ""` root carries top-level `workspaces` (+ catalogs), `overrides`,
    // `patchedDependencies`, and the three-state `allowBuilds` map; the engine's
    // own branded YAML/namespace are `None`/`""` so they never apply). In compat
    // mode (any other role, incl. fresh) nub plays the incumbent completely:
    // `pnpm-workspace.yaml` + `pnpm.*` stay live. The pnpm-branded read-sites now
    // move together behind ONE EngineContext posture: `read_branded_pnpm_config`
    // gates the `pnpm-workspace.yaml` candidate, the `pnpm` package.json
    // namespace, AND pnpm's global `~/.config/pnpm/auth.ini` — true only in the
    // PnpmOrFresh arm. `read_manifest_root_config` is true only under nub
    // identity, where root-level config migrated by `nub pm use nub` is the
    // native surface. `pnpmfile_default_enabled` gates the cwd-default
    // `.pnpmfile`; true only in the PnpmOrFresh arm. The probe
    // is engine-free (plain manifest/lockfile-presence reads): ONE walk up the
    // tree, ONE `current_dir()` read (see [`resolve_config_surface`]).
    let surface = std::env::current_dir()
        .ok()
        .map(|cwd| resolve_config_surface(&cwd))
        .unwrap_or(ConfigSurface::PnpmOrFresh);
    let read_branded_pnpm_config = matches!(surface, ConfigSurface::PnpmOrFresh);
    let read_yarn_config = read_yarn_config_for_surface(&surface);
    // Classic Yarn (v1) reads `.yarnrc`; Yarn Berry (v2+) abandoned it for
    // `.yarnrc.yml` and ignores a stray legacy `.yarnrc`. Gate the engine's
    // classic-`.yarnrc` reader to provably-classic projects so a Berry project's
    // leftover `.yarnrc` doesn't silently override registry/auth. Only meaningful
    // under a yarn surface (where `read_yarn_config` is already true).
    let yarn_is_classic = match &surface {
        ConfigSurface::NonPnpmCompat { role: "yarn", dir } => yarn_surface_is_classic(dir),
        _ => false,
    };
    let bunfig = match &surface {
        ConfigSurface::NonPnpmCompat { role: "bun", dir } => {
            bun_config::load_bunfig_npmrc_entries(dir)
        }
        _ => bun_config::BunfigNpmrcEntries::default(),
    };
    let read_manifest_root_config = matches!(surface, ConfigSurface::NubIdentity(_));
    let pnpmfile_default_enabled = matches!(surface, ConfigSurface::PnpmOrFresh);
    // Honor Bun's `BUN_CONFIG_REGISTRY` / `BUN_CONFIG_TOKEN` only under a Bun
    // incumbent; under any other surface those Bun-named env vars are another
    // tool's state and must not be read (name-based policy, like `read_yarn_config`).
    let read_bun_config = read_bun_config_for_surface(&surface);
    aube_util::update_engine_context(|c| {
        c.read_branded_pnpm_config = read_branded_pnpm_config;
        // GLOBAL config is read PM-AGNOSTICALLY and UNGATED by cwd incumbency:
        // nub honors whatever global config the user already has from any tool —
        // npm's `~/.npmrc`, pnpm's global `config.yaml`, pnpm's global `auth.ini`.
        // Set `true` UNCONDITIONALLY (cwd-independent). The original bug was that
        // these global reads were GATED on the cwd-derived `read_branded_pnpm_config`
        // (so nub read pnpm's global config only when standing in a pnpm project);
        // the fix is to DECOUPLE them from the cwd, not to stop reading them. The
        // separate `read_pnpm_global_config` posture exists precisely so the GLOBAL
        // reads don't ride the project-scoped gate. (Global WRITES are neutral-only —
        // enforced in store_config_family: `config set -g` never writes a
        // pnpm-branded global file.)
        c.read_pnpm_global_config = true;
        c.read_yarn_config = read_yarn_config;
        c.yarn_is_classic = yarn_is_classic;
        c.read_bun_config = read_bun_config;
        c.read_manifest_root_config = read_manifest_root_config;
        c.pnpmfile_default_enabled = pnpmfile_default_enabled;
        c.synthetic_user_npmrc_entries = bunfig.user;
        c.synthetic_project_npmrc_entries = bunfig.project;
    });
    match surface {
        ConfigSurface::NubIdentity(dir) => {
            // A stray pnpm-workspace.yaml under nub identity (branch merge,
            // tutorial copy-paste) is ignore-with-warning, never read and never
            // silent: deterministic nub-pure behavior, one warning, remedies
            // named (the maintainer 2026-06-10, supersedes read-with-warning). The read
            // itself is already gated off by `read_branded_pnpm_config = false`.
            if dir.join("pnpm-workspace.yaml").is_file() {
                eprintln!(
                    "nub: pnpm-workspace.yaml is not read under nub identity — migrate it \
                 (`nub pm use nub`), delete it, or return to pnpm (`nub pm use pnpm`)."
                );
            }
        }
        ConfigSurface::NonPnpmCompat { role, .. } => {
            // Compat mode, but the incumbent is npm/yarn/bun — NOT pnpm. The
            // pnpm-specific config surface is theirs to ignore (gated off by
            // `read_branded_pnpm_config = false`): a stray `pnpm-workspace.yaml`,
            // a `package.json#pnpm.*` object, or pnpm's global `auth.ini` in an
            // npm/yarn/bun project is another tool's state. The cwd-default
            // `.pnpmfile.cjs`/`.mjs` is pnpm-proprietary too, and unlike a
            // workspace-yaml *it shapes resolution* — gated off here by
            // `pnpmfile_default_enabled = false`. Explicit
            // `--pnpmfile`/`--global-pnpmfile` overrides still load (a path named
            // on purpose). One dim warning when a present default file is
            // suppressed, matching the pnpm-workspace.yaml ignore-with-warning
            // pattern.
            if let Some(present) = std::env::current_dir()
                .ok()
                .and_then(|cwd| pnpmfile_default_path(&cwd))
            {
                let name = present
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(".pnpmfile.cjs");
                let line = format!(
                    "nub: `{name}` ignored — this project uses {role}, which doesn't apply pnpmfile hooks. \
                 Remove it, name it explicitly with `--pnpmfile`, or switch to pnpm (`nub pm use pnpm`)."
                );
                if scope_warning_uses_dim() {
                    eprintln!("\x1b[2m{line}\x1b[0m");
                } else {
                    eprintln!("{line}");
                }
            }
        }
        ConfigSurface::PnpmOrFresh => {
            // pnpm role (or fresh): play the incumbent completely.
            // `read_branded_pnpm_config = true` keeps `pnpm-workspace.yaml`, the
            // `pnpm` package.json namespace, and pnpm's `auth.ini` live. nub's own
            // branded YAML/namespace are `None`/`""` on the const, so an
            // `aube-workspace.yaml` or `aube` manifest object some other tool left
            // on disk is neither read nor chosen as a fresh-write target.
        }
    }
}

/// The cwd-default pnpmfile path if one exists, ignoring the engine's
/// detection gate. Lets the preflight discover that a default `.pnpmfile` is
/// present *before* the `pnpmfile_default_enabled = false` posture suppresses
/// it, so it can emit the one-line "ignored" warning naming the file.
/// Inlined here (the engine's `pnpmfile::default_path` is no longer re-exported
/// from the `aube` crate); mirrors aube's `.mjs`-over-`.cjs` precedence.
fn pnpmfile_default_path(cwd: &Path) -> Option<PathBuf> {
    for name in [".pnpmfile.mjs", ".pnpmfile.cjs"] {
        let p = cwd.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// The role-gated config surface for a project, resolved by ONE engine-free
/// walk up the directory tree. This is the single source of truth for the
/// brand/config-surface decision in [`engine_brand_preflight`], replacing
/// three separate walks (`nub_identity_dir` / `non_pnpm_role` /
/// `non_pnpm_role_display`) that re-derived overlapping slices of the same
/// classification.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigSurface {
    /// Project is under NUB identity. The pnpm-specific surface is OFF and
    /// the manifest config home is the root (`""`); the carried directory is
    /// the deciding level, so the caller can warn about a stray
    /// `pnpm-workspace.yaml` sitting beside it.
    NubIdentity(PathBuf),
    /// Compat mode with a NON-pnpm incumbent (npm / yarn / bun). The
    /// pnpm-specific surface is OFF (it's another tool's state); the carried
    /// name is the incumbent for user-facing warning text.
    NonPnpmCompat { role: &'static str, dir: PathBuf },
    /// pnpm role, or a fresh project (Axiom 4 gives fresh projects
    /// pnpm-format artifacts): play the pnpm incumbent completely — the
    /// pnpm-specific surface stays live.
    PnpmOrFresh,
}

/// Engine-free, single-walk resolution of the project's [`ConfigSurface`]
/// for `cwd` (walking up, same 16-level budget as identity resolution).
/// Drives the role-gated config surface in [`engine_brand_preflight`], which
/// must decide BEFORE any engine code reads project state — the config
/// getters freeze on first read, and full identity resolution itself reads
/// workspace config transitively, so it can't be the input here. Plain
/// `package.json` and lockfile-presence reads only.
///
/// This unifies what used to be three independent walks. It is
/// behavior-identical to the old layered logic: the per-level decision below
/// is the provable merge of the old `nub_identity_dir` (NUB-first) and
/// `non_pnpm_role` / `non_pnpm_role_display` (compat-classification) passes.
/// The key equivalence: the only level a single pass keeps walking past is a
/// COMPLETELY empty one (no declaration, no `lock.yaml`, no pnpm/foreign
/// lockfile) — and that is exactly the level both old passes also walked
/// past, so deciding terminally at the first non-empty level reproduces both.
///
/// Per level:
/// - A declaration decides by name: `nub` → nub identity; `npm`/`yarn`/`bun`
///   → non-pnpm compat (named); `pnpm`/anything-else → pnpm-shaped surface
///   (conservative — an unknown declared tool keeps the full compat surface).
/// - Undeclared: a lone `lock.yaml` (no pnpm/foreign lockfile beside it) →
///   nub identity. A `pnpm-lock.yaml` (alone, or beside anything — the
///   ambiguity state the engine errors on loudly right after) keeps the pnpm
///   surface. A foreign npm/yarn/bun lockfile with no pnpm-lock → non-pnpm
///   compat (named after the lockfile). A `lock.yaml` BESIDE a foreign one is
///   the ambiguity state — surface follows the foreign lockfile, exactly as
///   the old probes resolved it.
/// - A completely empty level → keep walking. Nothing anywhere splits on
///   whether a pnpm-NAMED file (`pnpm-workspace.yaml`, `.pnpmfile.cjs/.mjs`,
///   `.pnpmrc`) was seen in the walk: a truly-fresh project (no pnpm-named
///   file, no PM-preference signal of any kind) becomes NUB identity — nub
///   owns a project it scaffolds from nothing; a `pnpm-workspace.yaml`-present
///   project (a genuine pnpm signal, no lockfile yet) stays pnpm-shaped.
fn resolve_config_surface(cwd: &Path) -> ConfigSurface {
    // npm/yarn/bun-owned lockfiles, paired with the incumbent name (order
    // mirrors the old `non_pnpm_role_display` precedence for the name pick).
    const FOREIGN_NON_PNPM: &[(&str, &str)] = &[
        ("package-lock.json", "npm"),
        ("npm-shrinkwrap.json", "npm"),
        ("yarn.lock", "yarn"),
        ("bun.lock", "bun"),
        ("bun.lockb", "bun"),
    ];
    // A pnpm-NAMED file anywhere in the walk is a genuine pnpm signal even with
    // no lockfile yet, so the terminal default stays pnpm-shaped; absent any
    // such file the terminal default flips to NUB identity (truly-fresh).
    let mut saw_pnpm_named = false;
    let truly_fresh_root = cwd.to_path_buf();
    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        if !saw_pnpm_named && dir_has_pnpm_named_file(&dir) {
            saw_pnpm_named = true;
        }
        if let Some(decl) = aube_lockfile::declared_package_manager(&dir) {
            return match decl.name.as_str() {
                "nub" => ConfigSurface::NubIdentity(dir),
                "npm" => ConfigSurface::NonPnpmCompat { role: "npm", dir },
                "yarn" => ConfigSurface::NonPnpmCompat { role: "yarn", dir },
                "bun" => ConfigSurface::NonPnpmCompat { role: "bun", dir },
                // pnpm / unknown tool: keep the full pnpm-shaped surface.
                _ => ConfigSurface::PnpmOrFresh,
            };
        }
        let nub_lock = dir.join(use_align::NUB_LOCKFILE).is_file();
        let pnpm_lock = dir.join("pnpm-lock.yaml").is_file();
        let foreign = FOREIGN_NON_PNPM
            .iter()
            .find(|(f, _)| dir.join(f).is_file())
            .map(|(_, name)| *name);
        // A pnpm-lock.yaml present (even beside a foreign one — the ambiguity
        // the engine errors on) keeps the pnpm surface, outranking everything.
        if pnpm_lock {
            return ConfigSurface::PnpmOrFresh;
        }
        // A foreign npm/yarn/bun lockfile (with or without a lock.yaml beside
        // it — the ambiguity state) → non-pnpm compat.
        if let Some(name) = foreign {
            return ConfigSurface::NonPnpmCompat { role: name, dir };
        }
        // No pnpm/foreign lockfile: a lone lock.yaml decides nub identity.
        if nub_lock {
            return ConfigSurface::NubIdentity(dir);
        }
        // Completely empty level — keep walking.
        if !dir.pop() {
            break;
        }
    }
    // Nothing decided anywhere within the walk. A truly-fresh project (no
    // PM-preference signal of any kind, no pnpm-named file) becomes NUB
    // identity: the next install writes `lock.yaml` and stamps the manifest,
    // and the project self-reinforces as nub-identity thereafter. A
    // pnpm-named file seen in the walk keeps the pnpm-shaped surface.
    if saw_pnpm_named {
        ConfigSurface::PnpmOrFresh
    } else {
        ConfigSurface::NubIdentity(truly_fresh_root)
    }
}

/// Whether `dir` holds a pnpm-NAMED file — the name-based pnpm signal that
/// keeps a lockfile-less project pnpm-shaped (`resolve_config_surface`'s
/// terminal split). The `pnpm.*` package.json namespace is a pnpm signal too,
/// but it rides the `declared_package_manager` / lockfile checks already in
/// the walk; this covers the on-disk file names. Gates on NAME, not effect
/// (the brand-boundary rule): a generically-named field pnpm happens to read
/// is not a pnpm-named file.
fn dir_has_pnpm_named_file(dir: &Path) -> bool {
    const PNPM_NAMED: &[&str] = &[
        "pnpm-workspace.yaml",
        ".pnpmfile.cjs",
        ".pnpmfile.mjs",
        ".pnpmrc",
    ];
    PNPM_NAMED.iter().any(|name| dir.join(name).is_file())
}

fn read_yarn_config_for_surface(surface: &ConfigSurface) -> bool {
    matches!(surface, ConfigSurface::NonPnpmCompat { role: "yarn", .. })
}

/// Whether the engine should honor Bun's `BUN_CONFIG_REGISTRY` /
/// `BUN_CONFIG_TOKEN` environment variables — true only under a Bun incumbent.
/// Under any other surface those Bun-named env vars are another tool's state
/// (name-based policy, mirroring [`read_yarn_config_for_surface`]).
fn read_bun_config_for_surface(surface: &ConfigSurface) -> bool {
    matches!(surface, ConfigSurface::NonPnpmCompat { role: "bun", .. })
}

/// Whether the yarn-incumbent project rooted at `dir` is *classic* (v1) — the
/// gate for the engine's classic-`.yarnrc` reader.
///
/// Classic Yarn (v1) reads `.yarnrc`; Yarn Berry (v2+) abandoned it for
/// `.yarnrc.yml` and ignores any stray legacy `.yarnrc`. So `.yarnrc` may only
/// be honored for a provably-classic project. The rule is conservative:
/// **any Berry signal forces not-classic** (default to Berry), and classic is
/// returned only on an affirmative classic signal with no Berry signal present.
///
/// Berry signals (any ⇒ NOT classic): the identity probe resolves Berry
/// (committed `yarnPath`, a pinned major ≥ 2, or a versionless `yarn` beside a
/// `.yarnrc.yml`); a `.yarnrc.yml` present; a `.yarn/` release/state dir; or a
/// Berry-format `yarn.lock` (the `__metadata:` header).
///
/// Classic signals: the identity probe resolves classic yarn (e.g.
/// `packageManager: yarn@1`), or a classic-format `yarn.lock` (the
/// `# yarn lockfile v1` header) with none of the Berry signals.
fn yarn_surface_is_classic(dir: &Path) -> bool {
    // Berry config file — present iff the project is (or was set up as) Berry.
    if dir.join(".yarnrc.yml").is_file() {
        return false;
    }
    // Berry's release/state directory (`.yarn/releases`, `.yarn/cache`, …).
    if dir.join(".yarn").is_dir() {
        return false;
    }
    let yarn_lock = dir.join("yarn.lock");
    if let Ok(head) = read_file_head(&yarn_lock, 4096) {
        // Berry-format lockfile carries a `__metadata:` block; classic carries
        // the `# yarn lockfile v1` banner.
        if head.contains("__metadata:") {
            return false;
        }
        if head.contains("yarn lockfile v1") {
            // Classic lockfile and no Berry signal above → classic.
            return true;
        }
    }
    // No lockfile signal: fall back to the declared identity. `berry == false`
    // on a yarn identity (e.g. `packageManager: yarn@1.22.x`) is classic; an
    // absent/ambiguous identity stays NOT classic (conservative — Berry-leaning).
    matches!(
        nub_core::pm::resolve::project_pm_identity(dir),
        Some(id) if id.name == "yarn" && !id.berry
    )
}

/// Read up to `max_bytes` from the start of `path` as UTF-8 (lossy). Used for a
/// cheap header peek that avoids slurping a large lockfile.
fn read_file_head(path: &Path, max_bytes: usize) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max_bytes];
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Nub's replacement setting defaults, fed to the engine's embedder-defaults
/// tier (below all user sources — a user's `--node-linker`,
/// `npm_config_node_linker`, `.npmrc`, or workspace yaml all win):
///
/// - `defaultLockfileFormat` — a TRULY-fresh project (no PM-preference signal
///   of any kind) writes nub's neutral `lock.yaml` (`=aube`); every other
///   surface writes `pnpm-lock.yaml` (`=pnpm`) for drop-in interop. See
///   [`nub_setting_defaults`]'s `truly_fresh` arm.
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
/// - `cacheDir` is still NOT set here — the engine cache moves through the
///   `aube::set_cache_root` registration in [`engine_brand_preflight`]
///   instead. The settings accessor (`resolved_cache_dir`) only consults
///   the setting when `.npmrc` sets it *explicitly* (the embedder-defaults
///   tier never reaches it, verified empirically 2026-06-09), and the
///   non-settings consumers (git clone cache, node-gyp tool cache, primer,
///   adaptive state) never read the setting at all; the process-global
///   cache root covers every one of them.
/// - `defaultTrust=true` — the gated default-trust floor (curated list ∧
///   registry-resolved ∧ OSV MAL-* gate active ∧ past the cooling window)
///   is ON under nub in both modes; upstream aube keeps it off. Precedence
///   stays the settled chain (explicit `allowBuilds` true/false always wins
///   — `false` carves a package OUT of the floor; the map's *existence*
///   never disables it). Off-switch: `.npmrc default-trust=false` /
///   `npm_config_default_trust=false` — this is the embedder tier, below
///   every user source.
///
/// Emit a single install-time warning when the Yarn incumbent's effective
/// linker is PnP but nub will install a `node_modules` tree instead. Yarn
/// Berry defaults to PnP when no `nodeLinker` is configured, so absence is
/// warning-worthy for Berry but not for Yarn Classic.
fn warn_if_pnp_requested(detected: Option<&DetectedLockfile>, cwd: &Path) {
    let Some(kind @ (LockfileKind::Yarn | LockfileKind::YarnBerry)) = detected.map(|d| d.kind)
    else {
        return;
    };
    let root = detected.map(|d| d.dir.as_path()).unwrap_or(cwd);
    if effective_yarn_node_linker(root, cwd, kind == LockfileKind::YarnBerry).as_deref()
        == Some("pnp")
    {
        let line = "nub: this Yarn project uses Plug'n'Play (nodeLinker: pnp or Yarn Berry's default), \
                    which nub does not implement — installing a node_modules tree instead. \
                    [WARN_NUB_PNP_LINKER_DOWNGRADE]";
        if scope_warning_uses_dim() {
            eprintln!("\x1b[2m{line}\x1b[0m");
        } else {
            eprintln!("{line}");
        }
    }
}

fn effective_yarn_node_linker(root: &Path, cwd: &Path, berry_default_pnp: bool) -> Option<String> {
    if let Ok(value) = std::env::var("YARN_NODE_LINKER")
        && !value.trim().is_empty()
    {
        return Some(value.trim().to_ascii_lowercase());
    }

    let mut value = dirs_next::home_dir()
        .as_deref()
        .and_then(yarnrc_node_linker);
    for dir in yarnrc_walk_dirs(root, cwd) {
        if let Some(next) = yarnrc_node_linker(&dir) {
            value = Some(next);
        }
    }
    value.or_else(|| berry_default_pnp.then(|| "pnp".to_string()))
}

fn yarnrc_walk_dirs(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = if cwd.starts_with(root) {
        cwd.to_path_buf()
    } else {
        root.to_path_buf()
    };
    loop {
        dirs.push(current.clone());
        if !current.pop() {
            break;
        }
    }
    dirs.reverse();
    dirs
}

/// Read the `nodeLinker:` key from `.yarnrc.yml` at a directory, returning the
/// lowercased scalar value when present. Uses the same hand line-scan idiom as
/// `nub_core::pm::resolve::committed_yarn_path` — nub-cli takes no YAML
/// dependency. Only top-level, unindented `nodeLinker:` entries are recognized;
/// nested or multi-document forms are not.
fn yarnrc_node_linker(root: &Path) -> Option<String> {
    let content = std::fs::read_to_string(root.join(".yarnrc.yml")).ok()?;
    for line in content.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("nodeLinker:") {
            let value = strip_yarnrc_value(rest);
            if !value.is_empty() {
                return Some(value.to_ascii_lowercase());
            }
        }
    }
    None
}

/// Extract a scalar YAML value from the text after a `key:`. Strips surrounding
/// quotes and inline `# comments` on unquoted values. Mirrors the logic in
/// `nub_core::pm::resolve::strip_yaml_value` (duplicated here to avoid a
/// cross-crate dep for one small helper).
fn strip_yarnrc_value(rest: &str) -> &str {
    let rest = rest.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = rest.strip_prefix(quote) {
            if let Some(end) = inner.find(quote) {
                return &inner[..end];
            }
        }
    }
    // Unquoted: trim trailing `# comment`.
    rest.split('#').next().map(str::trim).unwrap_or(rest)
}

/// - Layout policy: flat-layout lockfile kinds (npm/yarn/bun) default
///   `nodeLinker` to `hoisted`; pnpm/aube kinds and fresh projects keep the
///   engine's `isolated` default (no entry pushed, so user/env settings
///   resolve exactly as in stock aube).
/// - Fresh-write lockfile format: a TRULY-fresh project (no PM-preference
///   signal of any kind — `truly_fresh`) writes nub's neutral `lock.yaml`
///   (`defaultLockfileFormat=aube`, which under the nub embedder resolves to
///   the `lock.yaml` basename); every other surface writes `pnpm-lock.yaml`,
///   keeping a pnpm-incumbent / mixed project drop-in interoperable. A
///   user-set `defaultLockfileFormat` (env/.npmrc/yaml) still wins on either
///   path — this is only the embedder-tier default.
fn nub_setting_defaults(
    detected: Option<&DetectedLockfile>,
    truly_fresh: bool,
) -> Vec<(String, String)> {
    let fresh_format = if truly_fresh { "aube" } else { "pnpm" };
    let mut defaults = vec![
        (
            "defaultLockfileFormat".to_string(),
            fresh_format.to_string(),
        ),
        ("defaultTrust".to_string(), "true".to_string()),
        (
            "virtualStoreDir".to_string(),
            "node_modules/.nub".to_string(),
        ),
        ("stateDir".to_string(), "node_modules/.nub".to_string()),
        (
            "disableGlobalVirtualStoreForPackages".to_string(),
            "next,nuxt,parcel".to_string(),
        ),
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
    // `dependenciesMeta.*.injected` is materialized only under the isolated
    // linker (aube needs the `.nub/` virtual store to sibling-link packed
    // copies). When a flat-layout incumbent declares injected deps, keep the
    // engine's `isolated` default instead of forcing `hoisted` — otherwise the
    // injection directive is silently dropped and peer deps resolve against the
    // wrong tree. A user-set `nodeLinker` (env/.npmrc/yaml) still wins; this
    // only declines to push the embedder-tier hoisted default.
    let injected = detected
        .map(|d| unsupported_config::injected_deps_present(&d.dir))
        .unwrap_or(false);
    if hoisted_kind && !injected {
        defaults.push(("nodeLinker".to_string(), "hoisted".to_string()));
    }
    defaults
}

/// Whether the project at `cwd` is TRULY fresh — no package-manager preference
/// signal of any kind in the walk-up. `detected.is_none()` already establishes
/// that no lockfile (of any format) and no `packageManager`/`devEngines`
/// declaration exists anywhere up the tree (`resolve_identity_walk_up` returns
/// `None` only then); this additionally requires that no pnpm-NAMED file
/// (`pnpm-workspace.yaml`, `.pnpmfile.*`, `.pnpmrc`) sits in the walk — a
/// pnpm-named file is a genuine pnpm signal that makes the project pnpm-shaped,
/// not nub's to claim. On the truly-fresh path nub writes `lock.yaml` and
/// stamps the manifest with its own identity; any incumbent signal is
/// respected and left untouched.
fn is_truly_fresh_project(cwd: &Path, detected: Option<&DetectedLockfile>) -> bool {
    if detected.is_some() {
        return false;
    }
    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        if dir_has_pnpm_named_file(&dir) {
            return false;
        }
        if !dir.pop() {
            break;
        }
    }
    true
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

// ───────────────────────── fd capture ──────────────────────────

/// Run `f` with OS-level fd `fd` (1 = stdout, 2 = stderr) redirected into a
/// pipe; returns `f`'s result plus everything written, so the caller can
/// re-emit it through the brand rewrite. Only used for verbs that spawn no
/// children and render no progress UI (the install family captures it for
/// the verbs that print engine branding; the config family borrows it for
/// `config get`'s registry-default substitution). Any setup failure degrades
/// to running `f` unredirected with an empty capture — output then reaches
/// the console directly (un-rewritten), which beats losing it.
///
/// Captures are serialized process-wide: the fd table is process-global, so
/// two concurrent dup2 swaps of the same fd interleave into a torn state
/// (writes landing on a closed pipe). Production runs one capture per
/// command, so the lock is free there; it exists for the unit-test binary,
/// where parallel tests genuinely raced it (flaky
/// `fd_capture_round_trips_raw_prints`).
#[cfg(unix)]
pub(crate) fn with_fd_captured<T>(fd: libc::c_int, f: impl FnOnce() -> T) -> (T, String) {
    use std::io::{Read as _, Write as _};
    use std::os::unix::io::FromRawFd as _;

    static FD_SWAP: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = FD_SWAP.lock().unwrap_or_else(|p| p.into_inner());

    let flush = |fd: libc::c_int| {
        // Rust's stdout is buffered; push pending bytes to whichever target
        // fd 1 currently points at. stderr is unbuffered.
        if fd == 1 {
            let _ = std::io::stdout().flush();
        }
    };

    // SAFETY: plain POSIX fd plumbing on fds this function owns end-to-end.
    unsafe {
        let mut ends = [0 as libc::c_int; 2];
        if libc::pipe(ends.as_mut_ptr()) != 0 {
            return (f(), String::new());
        }
        let (read_end, write_end) = (ends[0], ends[1]);
        flush(fd); // pre-swap: drain pending bytes to the real target
        let saved = libc::dup(fd);
        if saved < 0 || libc::dup2(write_end, fd) < 0 {
            libc::close(read_end);
            libc::close(write_end);
            if saved >= 0 {
                libc::close(saved);
            }
            return (f(), String::new());
        }
        libc::close(write_end);
        // Drain concurrently so a full pipe buffer can never deadlock `f`.
        let mut reader = std::fs::File::from_raw_fd(read_end);
        let drain = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            buf
        });
        let result = f();
        flush(fd); // post-run: push f's buffered tail into the pipe
        libc::dup2(saved, fd);
        libc::close(saved);
        // fd restored + our write end closed ⇒ the drain thread sees EOF.
        let bytes = drain.join().unwrap_or_default();
        (result, String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Windows counterpart of the Unix [`with_fd_captured`]. This is load-bearing
/// for `config get registry`: its default-registry substitution detects the
/// engine's `undefined` print *from the capture*, so an empty capture (the old
/// no-op stub) silently dropped the substitution and surfaced `undefined`.
///
/// Two redirections are needed, because two distinct writer families have to
/// land in the pipe:
///
/// 1. **The Win32 std handle.** Rust's `println!`/`print!` (and so the engine's
///    `println!("undefined")`) do NOT go through CRT fd 1 on Windows — Rust's
///    `std::io::Stdout` writes to `GetStdHandle(STD_OUTPUT_HANDLE)` directly
///    via `WriteFile`/`WriteConsole`, bypassing the CRT fd table entirely. So
///    a CRT `_dup2` of fd 1 alone (the obvious mirror of the Unix path) would
///    capture nothing from `println!`. We must also point the std handle at the
///    pipe with `SetStdHandle`; Rust re-reads the handle on every write (it does
///    not cache it), so the swap takes effect immediately.
/// 2. **CRT fd 1.** Any engine code that writes via raw CRT fds (`libc::write`,
///    C stdio) goes through the fd table, which `SetStdHandle` does NOT affect.
///    The `_dup2` swap covers that family, mirroring the Unix path.
///
/// The captured payloads here are tiny (`undefined\n`, a registry URL, a short
/// hint), so — unlike the Unix path's concurrent drain — the pipe is given a
/// large buffer and read after `f` returns and the redirections are restored;
/// the small fixed payloads can't fill a 1 MiB pipe, so no deadlock. Any setup
/// failure degrades to running `f` unredirected with an empty capture, exactly
/// like the Unix path.
#[cfg(windows)]
pub(crate) fn with_fd_captured<T>(fd: i32, f: impl FnOnce() -> T) -> (T, String) {
    use std::io::Write as _;

    // Minimal kernel32 / msvcrt surface not exposed by the `libc` crate.
    // `_get_osfhandle` (CRT) maps a CRT fd to its underlying OS HANDLE so we
    // can hand the pipe's write end to `SetStdHandle`.
    type Handle = *mut core::ffi::c_void;
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // -11
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4; // -12
    const INVALID_HANDLE_VALUE: Handle = (-1isize) as Handle;
    unsafe extern "system" {
        fn GetStdHandle(nStdHandle: u32) -> Handle;
        fn SetStdHandle(nStdHandle: u32, hHandle: Handle) -> i32;
    }

    static FD_SWAP: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = FD_SWAP.lock().unwrap_or_else(|p| p.into_inner());

    let flush = |fd: libc::c_int| {
        if fd == 1 {
            let _ = std::io::stdout().flush();
        } else if fd == 2 {
            let _ = std::io::stderr().flush();
        }
    };
    let std_handle_id = match fd {
        1 => Some(STD_OUTPUT_HANDLE),
        2 => Some(STD_ERROR_HANDLE),
        _ => None,
    };

    // Generous pipe buffer: the payloads captured here are a few bytes, so a
    // 1 MiB buffer makes the read-after-restore approach deadlock-free.
    const PIPE_BUF: libc::c_uint = 1 << 20;
    // _O_BINARY (0x8000): no CRLF translation, so the capture is byte-exact.
    const O_BINARY: libc::c_int = 0x8000;

    // SAFETY: plain CRT/Win32 fd-and-handle plumbing on objects this function
    // owns end-to-end; the swaps are serialized by FD_SWAP and fully restored.
    unsafe {
        let mut ends = [0 as libc::c_int; 2];
        if libc::pipe(ends.as_mut_ptr(), PIPE_BUF, O_BINARY) != 0 {
            return (f(), String::new());
        }
        let (read_end, write_end) = (ends[0], ends[1]);

        // The pipe's write end as an OS HANDLE, for the std-handle swap.
        // `_get_osfhandle` returns -1 or -2 on a bad fd; `write_end` is a fresh
        // valid pipe fd, so this is real, but guard the sentinels regardless.
        let osf = libc::get_osfhandle(write_end);
        let write_handle = if osf == -1 || osf == -2 {
            INVALID_HANDLE_VALUE
        } else {
            osf as Handle
        };

        flush(fd); // pre-swap: drain pending bytes to the real target
        let saved = libc::dup(fd);
        if saved < 0 || libc::dup2(write_end, fd) < 0 {
            libc::close(read_end);
            libc::close(write_end);
            if saved >= 0 {
                libc::close(saved);
            }
            return (f(), String::new());
        }

        // Redirect the Win32 std handle too (this is what Rust's `println!`
        // actually targets). Save the prior handle so we can restore it.
        let saved_std = std_handle_id.map(|id| GetStdHandle(id));
        if let (Some(id), Some(h)) = (std_handle_id, saved_std) {
            // Only swap when we hold a usable handle and the pipe handle is
            // valid; on failure we still have the CRT fd swap (best effort).
            if h != INVALID_HANDLE_VALUE && write_handle != INVALID_HANDLE_VALUE {
                let _ = SetStdHandle(id, write_handle);
            }
        }

        let result = f();
        flush(fd); // push f's buffered tail into the pipe

        // Restore the std handle first (so subsequent prints during teardown go
        // to the real target), then the CRT fd, then close our write end so the
        // read sees EOF.
        if let (Some(id), Some(h)) = (std_handle_id, saved_std) {
            if h != INVALID_HANDLE_VALUE {
                let _ = SetStdHandle(id, h);
            }
        }
        libc::dup2(saved, fd);
        libc::close(saved);
        libc::close(write_end);

        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = libc::read(
                read_end,
                chunk.as_mut_ptr() as *mut libc::c_void,
                chunk.len() as libc::c_uint,
            );
            if n <= 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n as usize]);
        }
        libc::close(read_end);
        (result, String::from_utf8_lossy(&buf).into_owned())
    }
}

/// KNOWN GAP (neither unix nor windows): no fd capture — the engine's raw
/// prints reach the console un-rewritten. No such target exists in nub's
/// support matrix today; the stub keeps the build total.
#[cfg(not(any(unix, windows)))]
pub(crate) fn with_fd_captured<T>(_fd: i32, f: impl FnOnce() -> T) -> (T, String) {
    (f(), String::new())
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
            fresh: false,
        };

        // Flat-layout kinds ⇒ nodeLinker defaults to hoisted.
        for kind in [
            LockfileKind::Npm,
            LockfileKind::YarnBerry,
            LockfileKind::Bun,
        ] {
            assert_eq!(
                get(
                    &nub_setting_defaults(Some(&detected(kind)), false),
                    "nodeLinker"
                ),
                Some("hoisted"),
                "{kind:?} must default to the hoisted layout"
            );
        }

        // pnpm-shaped kinds and no lockfile ⇒ no entry (engine's isolated
        // default applies, user/env settings resolve as in stock aube).
        for kind in [LockfileKind::Pnpm, LockfileKind::Aube] {
            assert_eq!(
                get(
                    &nub_setting_defaults(Some(&detected(kind)), false),
                    "nodeLinker"
                ),
                None,
                "{kind:?} must not inject a nodeLinker default"
            );
        }
        assert_eq!(
            get(&nub_setting_defaults(None, true), "nodeLinker"),
            None,
            "no lockfile must not inject a nodeLinker default"
        );
    }

    #[test]
    fn fresh_write_format_flips_with_truly_fresh() {
        // A truly-fresh project writes nub's neutral `lock.yaml`
        // (`defaultLockfileFormat=aube`); every other surface keeps the
        // pnpm-lock fresh-write default for drop-in interop.
        assert_eq!(
            get(&nub_setting_defaults(None, true), "defaultLockfileFormat"),
            Some("aube"),
            "truly-fresh must write nub's lock.yaml"
        );
        assert_eq!(
            get(&nub_setting_defaults(None, false), "defaultLockfileFormat"),
            Some("pnpm"),
            "a project with a pnpm signal keeps the pnpm-lock fresh-write default"
        );
        let dir = tempfile::tempdir().unwrap();
        let pnpm = DetectedLockfile {
            kind: LockfileKind::Pnpm,
            dir: dir.path().to_path_buf(),
            fresh: false,
        };
        assert_eq!(
            get(
                &nub_setting_defaults(Some(&pnpm), false),
                "defaultLockfileFormat"
            ),
            Some("pnpm"),
            "an incumbent lockfile is never the truly-fresh path"
        );
    }

    #[test]
    fn setting_defaults_always_carry_the_nub_identity_settings() {
        // Every engine command gets the `.nub` store/state location and the
        // nub-namespaced global dirs regardless of detection; the non-truly-
        // fresh surfaces also get the pnpm lockfile fresh-write default (the
        // truly-fresh `aube`/`lock.yaml` flip is covered separately). (These
        // ride the engine's
        // embedder-defaults tier, so any user source overrides them —
        // precedence is covered by the engine's own tests and the
        // install_engine integration tests.)
        for detected in [None, Some(LockfileKind::Npm), Some(LockfileKind::Pnpm)] {
            let dir = tempfile::tempdir().unwrap();
            let detected = detected.map(|kind| DetectedLockfile {
                kind,
                dir: dir.path().to_path_buf(),
                fresh: false,
            });
            // `truly_fresh = false` here: this exercises the
            // identity-settings invariants for the non-truly-fresh surfaces
            // (the truly-fresh lockfile-format flip is covered separately in
            // `fresh_write_format_flips_with_truly_fresh`).
            let defaults = nub_setting_defaults(detected.as_ref(), false);
            assert_eq!(get(&defaults, "defaultLockfileFormat"), Some("pnpm"));
            assert_eq!(get(&defaults, "virtualStoreDir"), Some("node_modules/.nub"));
            assert_eq!(get(&defaults, "stateDir"), Some("node_modules/.nub"));
            // The global store lands in nub's XDG data namespace (dev boxes
            // always resolve a home dir, so the entry is present here).
            let store = get(&defaults, "storeDir").expect("storeDir default");
            // Normalize separators: on Windows the default resolves with
            // `\` components (and a mixed `/` from the XDG-style fallback).
            let store = store.replace('\\', "/");
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

    #[test]
    fn setting_defaults_never_inherit_incumbent_allow_all_build_policy() {
        // Security invariant: nub must NEVER inherit an allow-all build-script
        // default from any incumbent.  Every incumbent — npm, pnpm, yarn,
        // yarn-berry, bun, aube, npm-shrinkwrap, and fresh/no-lockfile — must
        // produce `defaultTrust = "true"` (the safe explicit allowlist posture)
        // and must NOT carry `dangerouslyAllowAllBuilds` in the defaults map.
        // This test guards against a future incumbent-specialisation refactor
        // silently leaking an unsafe default.
        let dir = tempfile::tempdir().unwrap();
        let detected = |kind| DetectedLockfile {
            kind,
            dir: dir.path().to_path_buf(),
            fresh: false,
        };

        let all_kinds = [
            Some(LockfileKind::Npm),
            Some(LockfileKind::NpmShrinkwrap),
            Some(LockfileKind::Pnpm),
            Some(LockfileKind::Yarn),
            Some(LockfileKind::YarnBerry),
            Some(LockfileKind::Bun),
            Some(LockfileKind::Aube),
            None, // fresh / nub-identity project
        ];

        for kind in all_kinds {
            let d = kind.map(detected);
            let defaults = nub_setting_defaults(d.as_ref(), false);
            let label = format!("{kind:?}");

            // Must always carry the safe explicit-allowlist posture.
            assert_eq!(
                get(&defaults, "defaultTrust"),
                Some("true"),
                "{label}: defaultTrust must be \"true\""
            );

            // Must never inject an allow-all build-script key.
            assert_eq!(
                get(&defaults, "dangerouslyAllowAllBuilds"),
                None,
                "{label}: dangerouslyAllowAllBuilds must not appear in defaults"
            );
        }
    }

    #[test]
    fn yarnrc_node_linker_reads_unquoted_and_quoted_and_ignores_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".yarnrc.yml");

        // Unquoted value.
        std::fs::write(&path, "nodeLinker: pnp\n").unwrap();
        assert_eq!(yarnrc_node_linker(dir.path()).as_deref(), Some("pnp"));

        // Single-quoted value.
        std::fs::write(&path, "nodeLinker: 'pnp'\n").unwrap();
        assert_eq!(yarnrc_node_linker(dir.path()).as_deref(), Some("pnp"));

        // Double-quoted value.
        std::fs::write(&path, "nodeLinker: \"pnp\"\n").unwrap();
        assert_eq!(yarnrc_node_linker(dir.path()).as_deref(), Some("pnp"));

        // Hoisted — must read the value but it won't trigger the warning.
        std::fs::write(&path, "nodeLinker: hoisted\n").unwrap();
        assert_eq!(yarnrc_node_linker(dir.path()).as_deref(), Some("hoisted"));

        // Inline comment stripped on unquoted values.
        std::fs::write(&path, "nodeLinker: pnp # managed by yarn\n").unwrap();
        assert_eq!(yarnrc_node_linker(dir.path()).as_deref(), Some("pnp"));

        // Indented (nested) — must not match.
        std::fs::write(&path, "  nodeLinker: pnp\n").unwrap();
        assert_eq!(yarnrc_node_linker(dir.path()), None);

        // Absent file.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(yarnrc_node_linker(dir.path()), None);

        // Case-normalised to lowercase.
        std::fs::write(&path, "nodeLinker: PnP\n").unwrap();
        assert_eq!(yarnrc_node_linker(dir.path()).as_deref(), Some("pnp"));
    }

    #[test]
    fn warn_if_pnp_requested_fires_only_for_yarn_with_pnp_linker() {
        let dir = tempfile::tempdir().unwrap();
        let yarnrc = dir.path().join(".yarnrc.yml");
        static YARN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        // Helper that mirrors the gating logic without actually calling
        // eprintln! — we check the predicate, not the output channel.
        let would_warn = |kind: Option<LockfileKind>, content: Option<&str>| {
            let _ = std::fs::remove_file(&yarnrc);
            if let Some(c) = content {
                std::fs::write(&yarnrc, c).unwrap();
            }
            let detected = kind.map(|k| DetectedLockfile {
                kind: k,
                dir: dir.path().to_path_buf(),
                fresh: false,
            });
            let is_yarn = matches!(
                detected.as_ref().map(|d| d.kind),
                Some(LockfileKind::Yarn | LockfileKind::YarnBerry)
            );
            let root = detected
                .as_ref()
                .map(|d| d.dir.as_path())
                .unwrap_or(dir.path());
            is_yarn
                && effective_yarn_node_linker(
                    root,
                    root,
                    detected
                        .as_ref()
                        .is_some_and(|d| d.kind == LockfileKind::YarnBerry),
                )
                .as_deref()
                    == Some("pnp")
        };

        // Yarn + pnp → warn.
        assert!(would_warn(
            Some(LockfileKind::Yarn),
            Some("nodeLinker: pnp\n")
        ));
        assert!(would_warn(
            Some(LockfileKind::YarnBerry),
            Some("nodeLinker: pnp\n")
        ));

        // Yarn + hoisted → no warn.
        assert!(!would_warn(
            Some(LockfileKind::Yarn),
            Some("nodeLinker: hoisted\n")
        ));

        // Yarn Classic + no .yarnrc.yml → no warn; Yarn Berry defaults to PnP.
        assert!(!would_warn(Some(LockfileKind::Yarn), None));
        assert!(would_warn(Some(LockfileKind::YarnBerry), None));

        // A nearer rc file overrides an ancestor rc file.
        std::fs::write(&yarnrc, "nodeLinker: pnp\n").unwrap();
        let child = dir.path().join("packages/app");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join(".yarnrc.yml"), "nodeLinker: node-modules\n").unwrap();
        assert_ne!(
            effective_yarn_node_linker(dir.path(), &child, true).as_deref(),
            Some("pnp")
        );

        // A parent rc above the detected project root is still visible to the
        // Yarn translator, so the warning scan must not stop at the root.
        let outside = tempfile::tempdir().unwrap();
        let repo = outside.path().join("repo");
        let workspace = repo.join("packages/app");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(outside.path().join(".yarnrc.yml"), "nodeLinker: pnp\n").unwrap();
        assert_eq!(
            effective_yarn_node_linker(&repo, &workspace, false).as_deref(),
            Some("pnp")
        );

        // Yarn env config outranks the Berry default and rc files.
        let _env = YARN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("YARN_NODE_LINKER", "node-modules") };
        assert_ne!(
            effective_yarn_node_linker(dir.path(), dir.path(), true).as_deref(),
            Some("pnp")
        );
        unsafe { std::env::remove_var("YARN_NODE_LINKER") };

        // Non-yarn kinds + pnp → no warn (npm/bun projects can't have .yarnrc.yml pnp).
        assert!(!would_warn(
            Some(LockfileKind::Npm),
            Some("nodeLinker: pnp\n")
        ));
        assert!(!would_warn(
            Some(LockfileKind::Bun),
            Some("nodeLinker: pnp\n")
        ));
        assert!(!would_warn(
            Some(LockfileKind::Pnpm),
            Some("nodeLinker: pnp\n")
        ));

        // No lockfile → no warn.
        assert!(!would_warn(None, Some("nodeLinker: pnp\n")));
    }

    // The brand-surface toggles (workspace-yaml list, manifest config
    // namespace, engines.aube check, packageManager acceptance set) are
    // process-global OnceLocks that freeze on first read, so in-process
    // assertions here would race other tests in this binary. They are
    // covered behaviorally through the spawned binary instead:
    // `tests/info_engine.rs::aube_workspace_yaml_is_not_consulted` and the
    // engines.aube case in `tests/install_engine.rs`.

    #[test]
    fn config_surface_resolves_identity_then_compat_role_in_one_walk() {
        let root = |files: &[(&str, &str)]| {
            let dir = tempfile::tempdir().unwrap();
            for (name, body) in files {
                std::fs::write(dir.path().join(name), body).unwrap();
            }
            dir
        };
        let is_nub = |s: ConfigSurface| matches!(s, ConfigSurface::NubIdentity(_));

        // ── declaration decides by name ──────────────────────────────────
        let d = root(&[("package.json", r#"{"packageManager":"nub@0.1.0"}"#)]);
        assert!(is_nub(resolve_config_surface(d.path())));
        // A pnpm declaration beats a lock.yaml for the config surface.
        let d = root(&[
            ("package.json", r#"{"packageManager":"pnpm@10.0.0"}"#),
            ("lock.yaml", "lockfileVersion: '9.0'\n"),
        ]);
        assert_eq!(resolve_config_surface(d.path()), ConfigSurface::PnpmOrFresh);
        // npm/yarn/bun declarations → non-pnpm compat, named after the tool.
        for (pm, name) in [
            ("npm@10.0.0", "npm"),
            ("yarn@4.0.0", "yarn"),
            ("bun@1.1.0", "bun"),
        ] {
            let d = root(&[("package.json", &format!(r#"{{"packageManager":"{pm}"}}"#))]);
            assert_eq!(
                resolve_config_surface(d.path()),
                ConfigSurface::NonPnpmCompat {
                    role: name,
                    dir: d.path().to_path_buf()
                },
                "{pm} is a non-pnpm compat role"
            );
        }
        // pnpm keeps the full surface; an unknown tool stays conservative
        // (full pnpm-shaped surface), never gated off by mistake.
        for pm in ["pnpm@9.0.0", "vlt@1.0.0"] {
            let d = root(&[("package.json", &format!(r#"{{"packageManager":"{pm}"}}"#))]);
            assert_eq!(
                resolve_config_surface(d.path()),
                ConfigSurface::PnpmOrFresh,
                "{pm} keeps the pnpm surface"
            );
        }

        // ── undeclared: lockfile presence decides ────────────────────────
        // A lone lock.yaml → nub identity, carrying the deciding dir.
        let d = root(&[
            ("package.json", "{}"),
            ("lock.yaml", "lockfileVersion: '9.0'\n"),
        ]);
        assert_eq!(
            resolve_config_surface(d.path()),
            ConfigSurface::NubIdentity(d.path().to_path_buf())
        );
        // lock.yaml beside a pnpm-lock.yaml is the ambiguity state → pnpm
        // surface (resolution errors loudly right after).
        let d = root(&[
            ("package.json", "{}"),
            ("lock.yaml", "lockfileVersion: '9.0'\n"),
            ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
        ]);
        assert_eq!(resolve_config_surface(d.path()), ConfigSurface::PnpmOrFresh);
        // lock.yaml beside a FOREIGN npm/yarn/bun lockfile (also ambiguity) →
        // non-pnpm compat, named after the foreign lockfile. This pins the
        // merged-walk contract: the old `nub_identity_dir` returned None here
        // and `non_pnpm_role` returned true → the surface is NonPnpmCompat.
        let d = root(&[
            ("package.json", "{}"),
            ("lock.yaml", "lockfileVersion: '9.0'\n"),
            ("yarn.lock", "# yarn\n"),
        ]);
        assert_eq!(
            resolve_config_surface(d.path()),
            ConfigSurface::NonPnpmCompat {
                role: "yarn",
                dir: d.path().to_path_buf()
            }
        );
        // A lone foreign lockfile → non-pnpm compat.
        let d = root(&[("package.json", "{}"), ("yarn.lock", "# yarn\n")]);
        assert_eq!(
            resolve_config_surface(d.path()),
            ConfigSurface::NonPnpmCompat {
                role: "yarn",
                dir: d.path().to_path_buf()
            }
        );
        // A pnpm-lock.yaml beside a foreign one → pnpm surface (pnpm-lock
        // outranks the foreign lockfile in the merged walk).
        let d = root(&[
            ("package.json", "{}"),
            ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
            ("yarn.lock", "# yarn\n"),
        ]);
        assert_eq!(resolve_config_surface(d.path()), ConfigSurface::PnpmOrFresh);
        // bun.lockb (binary) is a foreign bun lockfile for surface purposes.
        let d = root(&[("package.json", "{}"), ("bun.lockb", "\0")]);
        assert_eq!(
            resolve_config_surface(d.path()),
            ConfigSurface::NonPnpmCompat {
                role: "bun",
                dir: d.path().to_path_buf()
            }
        );

        // ── fresh + walk-up ──────────────────────────────────────────────
        // Truly fresh (no PM signal of any kind) → nub claims identity.
        let d = root(&[("package.json", "{}")]);
        assert_eq!(
            resolve_config_surface(d.path()),
            ConfigSurface::NubIdentity(d.path().to_path_buf())
        );
        // A pnpm-workspace.yaml with no lockfile is a genuine pnpm signal →
        // stays pnpm-shaped (not nub's to claim).
        let d = root(&[
            ("package.json", "{}"),
            ("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n"),
        ]);
        assert_eq!(resolve_config_surface(d.path()), ConfigSurface::PnpmOrFresh);
        // Other pnpm-NAMED files (no lockfile) are pnpm signals too.
        for named in [".pnpmfile.cjs", ".pnpmfile.mjs", ".pnpmrc"] {
            let d = root(&[("package.json", "{}"), (named, "\n")]);
            assert_eq!(
                resolve_config_surface(d.path()),
                ConfigSurface::PnpmOrFresh,
                "{named} keeps the pnpm-shaped surface"
            );
        }
        // Walks up from a member dir to the deciding root (nub identity).
        let d = root(&[
            ("package.json", r#"{"packageManager":"nub@0.1.0"}"#),
            ("lock.yaml", "lockfileVersion: '9.0'\n"),
        ]);
        let member = d.path().join("packages/a");
        std::fs::create_dir_all(&member).unwrap();
        assert_eq!(
            resolve_config_surface(&member),
            ConfigSurface::NubIdentity(d.path().to_path_buf())
        );
        // Walks up to a non-pnpm compat root too.
        let d = root(&[
            ("package.json", r#"{"packageManager":"yarn@4.0.0"}"#),
            ("yarn.lock", "# yarn\n"),
        ]);
        let member = d.path().join("packages/a");
        std::fs::create_dir_all(&member).unwrap();
        assert_eq!(
            resolve_config_surface(&member),
            ConfigSurface::NonPnpmCompat {
                role: "yarn",
                dir: d.path().to_path_buf()
            }
        );
    }

    #[test]
    fn yarn_config_read_gate_is_yarn_incumbent_only() {
        let root = tempfile::tempdir().unwrap();
        assert!(read_yarn_config_for_surface(
            &ConfigSurface::NonPnpmCompat {
                role: "yarn",
                dir: root.path().to_path_buf(),
            }
        ));
        assert!(!read_yarn_config_for_surface(&ConfigSurface::NubIdentity(
            root.path().to_path_buf()
        )));
        assert!(!read_yarn_config_for_surface(
            &ConfigSurface::NonPnpmCompat {
                role: "npm",
                dir: root.path().to_path_buf(),
            }
        ));
        assert!(!read_yarn_config_for_surface(
            &ConfigSurface::NonPnpmCompat {
                role: "bun",
                dir: root.path().to_path_buf(),
            }
        ));
        assert!(!read_yarn_config_for_surface(&ConfigSurface::PnpmOrFresh));
    }

    #[test]
    fn bun_config_read_gate_is_bun_incumbent_only() {
        // The gate controlling `EngineContext::read_bun_config`: Bun's
        // `BUN_CONFIG_REGISTRY`/`BUN_CONFIG_TOKEN` env vars are honored only when
        // Bun is the incumbent, and ignored under nub identity or any other
        // (non-bun) compat role, where they are another tool's state.
        let root = tempfile::tempdir().unwrap();
        assert!(read_bun_config_for_surface(&ConfigSurface::NonPnpmCompat {
            role: "bun",
            dir: root.path().to_path_buf(),
        }));
        assert!(!read_bun_config_for_surface(&ConfigSurface::NubIdentity(
            root.path().to_path_buf()
        )));
        assert!(!read_bun_config_for_surface(
            &ConfigSurface::NonPnpmCompat {
                role: "npm",
                dir: root.path().to_path_buf(),
            }
        ));
        assert!(!read_bun_config_for_surface(
            &ConfigSurface::NonPnpmCompat {
                role: "yarn",
                dir: root.path().to_path_buf(),
            }
        ));
        assert!(!read_bun_config_for_surface(&ConfigSurface::PnpmOrFresh));
    }

    #[test]
    fn lifecycle_ua_is_role_first_in_compat_and_nub_first_otherwise() {
        let nub = env!("CARGO_PKG_VERSION");
        let pin = |name: &str, v: Option<&str>| Some((name.to_string(), v.map(str::to_string)));

        // Compat, pinned: the incumbent's token first with the PINNED version,
        // nub always second, runner dialect (node/v token present).
        assert_eq!(
            compose_lifecycle_ua(
                pin("pnpm", Some("9.1.0")),
                Some(LockfileKind::Pnpm),
                "22.15.0"
            ),
            format!("pnpm/9.1.0 nub/{nub} node/v22.15.0")
        );
        // Compat, unpinned pnpm (lockfile-inferred): the engine's parity version.
        assert_eq!(
            compose_lifecycle_ua(None, Some(LockfileKind::Pnpm), "22.15.0"),
            format!("pnpm/{PNPM_PARITY_VERSION} nub/{nub} node/v22.15.0")
        );
        // npm/bun roles: pnpm's own `?` convention when no version is declared.
        assert_eq!(
            compose_lifecycle_ua(None, Some(LockfileKind::Npm), "22.15.0"),
            format!("npm/? nub/{nub} node/v22.15.0")
        );
        assert_eq!(
            compose_lifecycle_ua(
                pin("bun", Some("1.2.0")),
                Some(LockfileKind::Bun),
                "22.15.0"
            ),
            format!("bun/1.2.0 nub/{nub} node/v22.15.0")
        );
        // Declaration outranks a stray foreign lockfile for the role, exactly
        // like identity resolution.
        assert_eq!(
            compose_lifecycle_ua(
                pin("npm", Some("11.0.0")),
                Some(LockfileKind::Npm),
                "22.15.0"
            ),
            format!("npm/11.0.0 nub/{nub} node/v22.15.0")
        );
        // Unknown declared tool falls through to the lockfile kind.
        assert_eq!(
            compose_lifecycle_ua(pin("vlt", None), Some(LockfileKind::Yarn), "22.15.0"),
            format!("yarn/? nub/{nub} node/v22.15.0")
        );

        // Nub identity (declared, or the lock.yaml kind) and fresh projects:
        // nub first, byte-identical to the runner's dialect.
        let nub_first = format!("nub/{nub} npm/? node/v22.15.0");
        assert_eq!(
            compose_lifecycle_ua(
                pin("nub", Some("0.1.0")),
                Some(LockfileKind::Aube),
                "22.15.0"
            ),
            nub_first
        );
        assert_eq!(
            compose_lifecycle_ua(None, Some(LockfileKind::Aube), "22.15.0"),
            nub_first
        );
        assert_eq!(compose_lifecycle_ua(None, None, "22.15.0"), nub_first);
    }

    #[test]
    fn lifecycle_overlay_carries_full_augmentation() {
        use nub_core::node::spawn::AugmentationEnv;
        use std::ffi::OsString;

        // A populated augmentation (what `nub run`/`exec` compute) must convert
        // into the generic overlay aube applies to every lifecycle spawn:
        // NODE → the node shim (so a build script's `$NODE child.js` re-enters
        // nub augmented), NODE_OPTIONS (preload + injected flags), NODE_PATH
        // (vendored helper resolution), npm_node_execpath PINNED to the
        // provisioned Node (the ABI fix — node-gyp must compile against the
        // project's Node, not ambient), and the shim dir leading PATH.
        let aug = AugmentationEnv {
            node_options: Some("--require=/rt/preload.cjs".to_string()),
            shim_dir: Some("/shim".to_string()),
            node_path: Some(OsString::from("/rt/node_path")),
            neutralize_localstorage: true,
        };
        let (overlay, prepends) = augmentation_to_lifecycle_overlay(&aug, "/pinned/bin/node");

        let find = |k: &str| {
            overlay
                .iter()
                .find(|(key, _)| key == OsString::from(k).as_os_str())
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        let expected_shim_node = std::path::Path::new("/shim")
            .join(if cfg!(windows) { "node.exe" } else { "node" })
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            find("NODE").as_deref(),
            Some(expected_shim_node.as_str()),
            "NODE must point at the shim, not the raw binary"
        );
        assert_eq!(
            find("NODE_OPTIONS").as_deref(),
            Some("--require=/rt/preload.cjs")
        );
        assert_eq!(find("NODE_PATH").as_deref(), Some("/rt/node_path"));
        assert_eq!(
            find("npm_node_execpath").as_deref(),
            Some("/pinned/bin/node"),
            "npm_node_execpath must pin the provisioned Node (ABI fix)"
        );
        assert_eq!(
            prepends,
            vec![std::path::PathBuf::from("/shim")],
            "shim dir leads PATH so a bare `node` in a build script is augmented"
        );
        assert_eq!(
            find("__NUB_NEUTRALIZE_LOCALSTORAGE").as_deref(),
            Some("1"),
            "neutralize signal must flow to build-script node children when set"
        );
    }

    /// No shim set up (re-entrant / broken install) → no NODE override and no
    /// PATH prepend, but the pinned npm_node_execpath still flows so the ABI
    /// pin survives even when augmentation can't fully engage.
    #[test]
    fn lifecycle_overlay_without_shim_still_pins_execpath() {
        use nub_core::node::spawn::AugmentationEnv;
        use std::ffi::OsString;
        let aug = AugmentationEnv {
            node_options: None,
            shim_dir: None,
            node_path: None,
            neutralize_localstorage: false,
        };
        let (overlay, prepends) = augmentation_to_lifecycle_overlay(&aug, "/pinned/bin/node");
        assert!(prepends.is_empty());
        assert!(
            !overlay
                .iter()
                .any(|(k, _)| k == OsString::from("NODE").as_os_str()),
            "no shim ⇒ no NODE override (the inherited NODE_OPTIONS preload still augments)"
        );
        assert_eq!(
            overlay
                .iter()
                .find(|(k, _)| k == OsString::from("npm_node_execpath").as_os_str())
                .map(|(_, v)| v.to_string_lossy().into_owned())
                .as_deref(),
            Some("/pinned/bin/node")
        );
    }

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
            "init", // reserved for nub's own project init (cli.rs answers it)
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
        assert!(
            msg.contains("not wired to the embedded engine yet"),
            "{msg}"
        );
        assert!(msg.contains("pnpm rm lodash"), "{msg}");
    }

    /// Build a workspace fixture on disk and return its root tempdir. Each
    /// `(relpath, body)` writes a file (creating parent dirs), so members live
    /// at e.g. `("pkgs/a/package.json", …)`.
    fn workspace(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, body) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        dir
    }

    fn root_manifest(root: &Path) -> aube_manifest::PackageJson {
        aube_manifest::PackageJson::from_path(&root.join("package.json")).unwrap()
    }

    #[test]
    fn catalog_preflight_covers_root_member_and_override_but_not_peers() {
        // (1) Root dep map — the original, already-covered path: a `catalog:`
        // in the root's dependencies is found.
        let d = workspace(&[("package.json", r#"{"dependencies":{"debug":"catalog:"}}"#)]);
        assert_eq!(
            first_catalog_specifier(&root_manifest(d.path()), d.path()),
            Some("debug: catalog:".to_string())
        );

        // (2) Workspace-MEMBER dep map — the bug: a `catalog:` only in a member
        // manifest used to bypass the preflight and get silently dropped.
        let d = workspace(&[
            ("package.json", r#"{"name":"root","workspaces":["pkgs/*"]}"#),
            (
                "pkgs/a/package.json",
                r#"{"name":"pkg-a","dependencies":{"debug":"catalog:"}}"#,
            ),
        ]);
        let hit = first_catalog_specifier(&root_manifest(d.path()), d.path())
            .expect("member catalog: must be found");
        assert!(
            hit.contains("pkg-a") && hit.contains("debug: catalog:"),
            "member hit should name the member and the spec: {hit}"
        );

        // (3) Override VALUE — the other bug: `"overrides": {"pkg":"catalog:"}`.
        let d = workspace(&[(
            "package.json",
            r#"{"name":"root","overrides":{"left-pad":"catalog:"}}"#,
        )]);
        let hit = first_catalog_specifier(&root_manifest(d.path()), d.path())
            .expect("override catalog: value must be found");
        assert!(
            hit.contains("override") && hit.contains("left-pad") && hit.contains("catalog:"),
            "override hit should name the key and the spec: {hit}"
        );

        // Exclusion: peerDependencies are NOT seeded with catalog refs by the
        // resolver, so a `catalog:` peer must NOT trip the preflight (matches
        // aube-resolver/src/resolve/seed.rs, which never reads peer ranges).
        let d = workspace(&[(
            "package.json",
            r#"{"peerDependencies":{"react":"catalog:"}}"#,
        )]);
        assert_eq!(
            first_catalog_specifier(&root_manifest(d.path()), d.path()),
            None,
            "a catalog: in peerDependencies must not trip the preflight"
        );
    }

    #[test]
    fn member_and_override_catalog_produce_the_role_named_hard_error() {
        // Whatever the source (member dep or override value), the surfaced
        // error must be the clean role-named refusal — NOT a silent drop and
        // NOT a generic ERR_NUB_UNKNOWN_CATALOG. We assert the wiring:
        // first_catalog_specifier → catalog_unsupported_error(npm, spec).
        use config_scope::Role;

        let d = workspace(&[
            (
                "package.json",
                r#"{"name":"root","packageManager":"npm@10.0.0","workspaces":["pkgs/*"]}"#,
            ),
            (
                "pkgs/a/package.json",
                r#"{"name":"pkg-a","dependencies":{"debug":"catalog:"}}"#,
            ),
        ]);
        let spec = first_catalog_specifier(&root_manifest(d.path()), d.path()).unwrap();
        let err = catalog_unsupported_error(Role::Npm, &spec).to_string();
        assert!(err.contains("npm"), "error must name the npm role: {err}");
        assert!(
            err.contains("catalog:") && err.contains("pnpm"),
            "error must explain the catalog/pnpm remedy: {err}"
        );

        let d = workspace(&[(
            "package.json",
            r#"{"name":"root","packageManager":"npm@10.0.0","overrides":{"left-pad":"catalog:"}}"#,
        )]);
        let spec = first_catalog_specifier(&root_manifest(d.path()), d.path()).unwrap();
        let err = catalog_unsupported_error(Role::Npm, &spec).to_string();
        assert!(err.contains("npm") && err.contains("left-pad"), "{err}");
    }

    #[test]
    fn bun_catalog_is_honored_from_1_2_and_refused_below() {
        // Bun added catalogs in 1.2.0, and aube resolves bun's
        // `workspaces.catalog` format — so a bun-incumbent project with a
        // `catalog:` ref must NOT hard-error on modern bun, mirroring real bun.
        // bun@<1.2 (the pre-catalog era) still refuses.
        use config_scope::Role;

        assert!(
            role_honors_catalog(Role::Bun, Some(1), Some(2)),
            "bun@1.2 implements catalogs"
        );
        assert!(
            role_honors_catalog(Role::Bun, Some(1), Some(5)),
            "bun@1.5 implements catalogs"
        );
        assert!(
            role_honors_catalog(Role::Bun, Some(2), None),
            "bun@2 implements catalogs"
        );
        assert!(
            role_honors_catalog(Role::Bun, None, None),
            "an undeclared/unparseable bun version assumes modern bun and honors"
        );
        assert!(
            !role_honors_catalog(Role::Bun, Some(1), Some(1)),
            "bun@1.1 predates catalogs and must refuse"
        );
        assert!(
            !role_honors_catalog(Role::Bun, Some(1), Some(0)),
            "bun@1.0 predates catalogs and must refuse"
        );

        // A bun-incumbent fixture with a real `catalog:` ref: the preflight
        // must not surface the hard-error when the version honors catalogs.
        let d = workspace(&[(
            // The object form of `workspaces` requires `packages` (aube-manifest
            // refuses a `packages`-less object so a `pacakges` typo fails loudly);
            // bun's real object form always carries it alongside `catalog`.
            "package.json",
            r#"{"name":"root","packageManager":"bun@1.2.3","workspaces":{"packages":["pkgs/*"],"catalog":{"is-odd":"3.0.1"}},"dependencies":{"is-odd":"catalog:"}}"#,
        )]);
        let m = root_manifest(d.path());
        // The specifier is present...
        assert!(first_catalog_specifier(&m, d.path()).is_some());
        // ...but a catalog-honoring bun does not refuse it.
        assert!(role_honors_catalog(Role::Bun, Some(1), Some(2)));

        // npm / yarn never honor catalogs.
        assert!(!role_honors_catalog(Role::Npm, Some(10), Some(0)));
        assert!(!role_honors_catalog(Role::Yarn, Some(4), Some(0)));
    }
}
