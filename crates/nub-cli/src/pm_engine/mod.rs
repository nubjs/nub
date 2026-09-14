//! Package-manager verbs through the embedded aube engine (vendor/aube,
//! linked as a library; no subprocess).
//!
//! This module is the shared plumbing; the verbs themselves live in four
//! per-family modules:
//!
//! - [`install_family`] — what is left of dependency-graph mutation and
//!   linking. pnpm's grammar knows every verb in it (`install`, `ci`, `add`,
//!   `remove`, `update`, `link`, `patch*`, …), as it knows the read-only
//!   queries (`list`, `why`, `outdated`, `audit`, `view`, …), so the front
//!   door hands all of their command lines straight to the engine and the
//!   module keeps only [`install_family::run_verb`]'s refusals, the `nubx`
//!   dlx fallback, and the virgin `devEngines` stamp.
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
//!   [`run_node_gyp_bootstrap`], because nub's lazy node-gyp shims
//!   ([`node_gyp`]) re-invoke `current_exe()` (= nub) with it
//!   mid-lifecycle-script.
//!
//! `install`/`i`/`ci` are *not* in the registry: they are live parser verbs
//! in `cli.rs` (SUBCOMMANDS), and the front door routes them to the engine
//! like the rest of the family. `init` is not in the registry either — the
//! spelling is reserved for nub's own project init; cli.rs's bareword arm
//! answers it with a "coming" note.
//! Every other registered verb goes to the engine, and the front door is
//! what sends it there: `engine_takes` claims any command line pnpm's own
//! grammar can parse, so a family dispatcher only ever sees what is left.
//!
//! ONE exclusion still holds at run time — `recursive`, which has no
//! meta-verb here; the recursion goes on the verb, as `-r` or `--filter`.
//! It survives only because a bare `nub recursive` gives pnpm nothing to
//! parse, so the front door declines and nub's own dispatch takes it.
//!
//! The other four exclusions this doc used to list are dead letters, each
//! measured by running the built binary. `clean` and `purge` reach the
//! engine and really do remove `node_modules` now, which is the opposite of
//! the "nub doesn't delete node_modules for you" they were excluded for.
//! `deploy` is wired: with a filter it writes a complete deployment.
//! `sbom` writes a valid CycloneDX 1.7 document matching pnpm 12.4.1. Their
//! refusals remain reachable by calling a dispatcher directly, which is
//! what the tests do, and by nothing a user can type. `sbom`'s exclusion
//! had a reason that outlived it: the engine names ITSELF in the SBOM's
//! `metadata.tools`, a brand leak to fix where the name is written rather
//! than by refusing the verb.

mod bun_config;
mod compat_db;
mod config_read;
pub mod config_scope;
mod duplicate_home;
mod expo_compat;
mod host_settings;
pub mod identity;
pub mod install_family;
mod install_report;
pub mod log;
pub mod min_release_age;
pub(crate) mod node_gyp;
pub mod output;
pub mod phantom_closure;
pub mod platform_flags;
mod pnpm_engine;
mod project_identity;
pub(crate) use pnpm_engine::engine_install;
pub(crate) use pnpm_engine::run as run_pnpm_engine;
mod verb_routing;
pub(crate) use verb_routing::engine_takes;
pub mod present;
mod remix_compat;
pub mod verb_parse;
use nub_core::resource_limits;
pub mod migrate;
pub mod phantom_hooks;
pub mod store_config_family;
pub mod unsupported_config;
pub mod use_align;
pub mod use_nub;
pub mod vite_compat;

pub use install_family::run_dlx_for_nubx;
pub use min_release_age::AgeGateFlags;
pub use platform_flags::PlatformFlags;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use aube_lockfile::LockfileKind;

/// Serializes tests that touch the process-global engine state — the
/// `aube_util` embedder profile (`set_embedder`, set-once) and the
/// `EngineContext` posture (`update_engine_context`, last-write-wins RwLock).
/// Any test that drives `engine_brand_preflight` / a family `run_verb`
/// (which registers `NUB` and writes `read_branded_pnpm_config` from the test
/// process's cwd) races a test that READS that posture
/// (e.g. a workspace-root lookup, gated on
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
/// (mirroring aube's own aliases), owning family, and — documentation for
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
    // `x` is the short fetch-and-run spelling (the `x` in `nubx`/`bunx`; `bun x`
    // == `bunx` == dlx). It aliases dlx — NOT exec — so `nub x <tool>` fetches a
    // missing tool, matching the `nubx` standalone binary.
    VerbSpec {
        canonical: "dlx",
        aliases: &["x"],
        family: Family::Install,
        aube_args: "commands::dlx::DlxArgs",
    },
    VerbSpec {
        canonical: "create",
        aliases: &[],
        family: Family::Install,
        aube_args: "commands::create::CreateArgs",
    },
    // `init` is deliberately NOT registered: the spelling belongs to nub's
    // own project scaffold (src/init.rs, a native subcommand), not the engine's
    // npm-style manifest write — the fourth deliberate pnpm-compat exception
    // (AGENTS.md); design record in internal/commands/init.md.
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
        canonical: "peers",
        aliases: &[],
        family: Family::Info,
        aube_args: "commands::peers::PeersArgs",
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
        // Nothing reaches this arm any more, and that was measured rather
        // than reasoned: an instrumented copy of this match printed a marker
        // for every read-only verb of every family, and all sixteen Info
        // spellings went to the engine instead. `engine_takes` claims the
        // command line at the front door because pnpm's own grammar knows
        // them, and the help path renders from the engine too. The three that
        // did land here — `check`, `deprecations`, `query` — were aube
        // commands with no pnpm counterpart, and they went with aube. The
        // variant stays because the registry still classifies these verbs for
        // `lookup_verb`; an error beats a panic for a branch that is
        // unreachable by measurement rather than by type.
        Family::Info => anyhow::bail!("nub: internal: `{typed}` is served by the engine"),
        // Dead for the same reason as `Info`, and measured the same way: all
        // twelve publish verbs are in pnpm's grammar, so the front door takes
        // their command lines and this arm is never chosen.
        Family::Publish => anyhow::bail!("nub: internal: `{typed}` is served by the engine"),
        Family::StoreConfig => store_config_family::run_verb(spec, typed, args, pm_hint),
    }
}

/// nub's hidden node-gyp re-entry verb: `__node-gyp-bootstrap <project-dir>`
/// resolves (bootstrapping on first use) the cached node-gyp and prints its
/// executable path on stdout. The lazy shims [`node_gyp`] writes re-invoke
/// `current_exe()` with this verb mid-lifecycle-script — and `current_exe()` IS
/// nub — so cli.rs intercepts the spelling before the parser and lands here.
/// The printed path is data for the shim, so stdout carries it and nothing
/// else; the bootstrap install is silenced for that reason.
pub(crate) fn run_node_gyp_bootstrap(args: &[String]) -> Result<i32> {
    let [project_dir] = args else {
        anyhow::bail!("usage: nub __node-gyp-bootstrap <project-dir>");
    };
    // Register nub's static identity FIRST: this re-entry runs as a fresh child
    // process spawned by the shim mid-build, before any other preflight, and the
    // bootstrap install it is about to drive derives its brand-scoped paths from
    // the profile.
    engine_brand_preflight();
    let project = std::path::Path::new(project_dir);
    let binary = node_gyp::bootstrap(project)?;
    // node-gyp runs next, under the project's Node, so put that Node's headers
    // where node-gyp looks before it downloads them (`nub_core::node::headers`).
    // Plain discovery, never provisioning: the version logic fires only where
    // node-version-management puts it. Best effort, since node-gyp's own
    // download stays the fallback.
    if let Ok(node) = nub_core::node::discovery::discover_node(project) {
        nub_core::node::headers::seed_node_gyp_cache(
            node.path.as_std_path(),
            &node.version.to_string(),
        );
    }
    println!("{}", binary.display());
    Ok(0)
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

/// Whether an identity-resolution failure (multi-lockfile ambiguity, declared
/// PM contradicted by the on-disk lockfile) is a HARD ERROR or degrades to
/// no-identity. `Lenient` swallows it and proceeds with no resolved identity.
///
/// Only `Lenient` remains. The strict arm — the loud diagnostic carrying the
/// `nub pm use` remedy — belonged to the project-reading/-writing families,
/// whose verbs the engine now parses and dispatches at the CLI front door
/// (`verb_routing`); every session this module still builds is one of the
/// classes that was always lenient, because none of them reads or writes the
/// CWD project's lockfile: the transient-package families (dlx/create, see
/// [`engine_session_transient`]). The global-scope read class used to be the
/// other one; its verbs are all the engine's now, so its constructor went with
/// them. The parameter is kept so a caller that needs the strict diagnostic
/// can ask for it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityStrictness {
    Lenient,
}

/// Where the isolated layout's virtual store materializes. `Default` engages
/// the machine-global virtual store (the shared, cross-project fast path) per
/// the embedder default; `ProjectLocal` forces GVS off so the store lands
/// inside `node_modules/.store` (real dirs + relative symlinks) — a self-
/// contained, COPY-relocatable tree. Only `nub ci` requests `ProjectLocal`
/// (see [`engine_session_ci`]); isolation/phantom-dep protection is preserved
/// either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VirtualStoreLocality {
    Default,
    ProjectLocal,
}

/// The project `install` block, lowered into layout and resolution settings.
/// Keeping the groups explicit lets CI override store locality and keeps the
/// linker eject seed separate from ordinary engine settings.
#[derive(Debug, Default, PartialEq, Eq)]
struct NativeInstallSettings {
    /// How `node_modules` is arranged.
    layout: Vec<(String, String)>,
    /// Which version a range selects. The compatibility guarantee.
    resolution: Vec<(String, String)>,
    /// `linker.eject` — layout, but also seeded into the phantom closure, which
    /// is why it is carried out of band rather than only as a lowered setting.
    eject: Vec<String>,
}

/// The project's active-PM role, declaration-first (a declared PM outranks a
/// stray lockfile), falling back to the detected lockfile kind.
fn active_role(detected: Option<&DetectedLockfile>, cwd: &Path) -> Option<config_scope::Role> {
    let declared = detected
        .and_then(|detected| nub_core::pm::resolve::declared_pm_raw(&detected.dir))
        .or_else(|| nub_core::pm::resolve::declared_pm_raw(cwd));
    config_scope::role_of(
        declared.as_ref().map(|(name, _)| name.as_str()),
        detected.map(|detected| detected.kind),
    )
}

/// Whether Nub owns this project. Release-age fields in the `install` block use
/// this gate; layout fields apply under every identity.
fn native_pm_mode(detected: Option<&DetectedLockfile>, truly_fresh: bool, cwd: &Path) -> bool {
    if truly_fresh {
        return true;
    }
    // A project with no lockfile at all is not Nub's unless `truly_fresh` above
    // already said so — a lone `packageManager: nub` declaration does not make
    // an otherwise-unresolved tree nub-identity.
    detected.is_some() && active_role(detected, cwd) == Some(config_scope::Role::Nub)
}

/// The engine settings this project's own `nub.jsonc` supplies, plus whether
/// nub owns the project.
///
/// The config surface needs both to decide where a write belongs, and it runs
/// OUTSIDE the engine session that normally computes them — `nub config` builds
/// no install context, so `engine_context().project_config_settings` is empty
/// on that path and reading it would report "nothing is shadowed" for every
/// project. Resolved from the same lowering the session uses, so the answer
/// tracks the real injection instead of predicting it a second time.
///
/// The embedder defaults are resolved for real rather than passed empty. They
/// are not value-only: under `linker: global` an injected dependency puts
/// `hoist=true` in the defaults, and that SUPPRESSES the
/// `enableGlobalVirtualStore` push entirely. Passing `&[]` therefore invented a
/// supplied setting the real session never injects, and refused an `.npmrc`
/// write the install would have honored — a false refusal, which is the worst
/// failure this guard has, since it breaks a configuration that works.
pub(crate) fn project_supplied_settings(cwd: &Path) -> (Vec<String>, bool) {
    let detected = resolve_identity_walk_up(cwd, IdentityStrictness::Lenient).unwrap_or(None);
    let truly_fresh = is_truly_fresh_project(cwd, detected.as_ref());
    let native_mode = native_pm_mode(detected.as_ref(), truly_fresh, cwd);
    let defaults = nub_setting_defaults(
        detected.as_ref(),
        truly_fresh,
        cwd,
        VirtualStoreLocality::Default,
    );
    // The config verbs dispatch through `lookup_verb` and RETURN before the
    // parser match that initializes the snapshot for ordinary routes, so on this
    // path `effective_config` is unset unless it is asked for here. Without
    // this the whole check reported "nothing is shadowed" for every project —
    // inert, and silently so, because failing to recognize a shadow just lets
    // the write through.
    //
    // A failure reports "supplies nothing" rather than propagating: a
    // malformed `nub.jsonc` means we cannot know what it supplies, and refusing
    // every `config set` on the strength of an unparseable file would be a
    // worse answer than the `.npmrc` write this project already gets today.
    // Returning here rather than falling through also keeps that promise when
    // some earlier path in the same process already populated the snapshot.
    if crate::cli::initialize_config_snapshot(false, false).is_err() {
        return (Vec::new(), native_mode);
    }
    let Some(config) = crate::project_config::effective_config() else {
        return (Vec::new(), native_mode);
    };
    let mut supplied = Vec::new();
    if let Ok(lowered) =
        lower_native_install_settings_for_mode(Some(&config.values.install), &defaults)
    {
        supplied.extend(lowered.layout.iter().map(|(key, _)| key.clone()));
        // Release-age settings reach the engine only under nub's own identity
        // (`scoped_install_settings`), so under an incumbent they shadow
        // nothing and the `.npmrc` value is the one that gets read.
        if native_mode {
            supplied.extend(lowered.resolution.iter().map(|(key, _)| key.clone()));
        }
    }
    // `verifyDeps` is read by `crate::verify_deps` rather than through the
    // settings tier, and only an explicit value from a nub CONFIG FILE outranks
    // `.npmrc` there. `sources` also carries the CLI and environment overlays,
    // so testing "not defaulted" would let `NUB_VERIFY_DEPS` masquerade as a
    // `nub.jsonc` field: the write meant for runs WITHOUT that variable would be
    // refused, and the advice would name a field that still loses to the env.
    if config
        .sources
        .get(&crate::project_config::ConfigKey::VerifyDeps)
        .is_some_and(|s| {
            matches!(
                s.kind,
                crate::project_config::ConfigSourceKind::Project
                    | crate::project_config::ConfigSourceKind::Global
            )
        })
    {
        supplied.push("verifyDepsBeforeRun".to_string());
    }
    (supplied, native_mode)
}

fn lower_native_install_settings(
    install: &crate::project_config::InstallConfig,
    embedder_defaults: &[(String, String)],
) -> Result<NativeInstallSettings> {
    use crate::project_config::{Hoist, LinkerConfig};

    // Only `pnp` still bails here. The pairings the old flat trio rejected at
    // install time — hoist under a shared store, eject under a project-local one
    // — are now unrepresentable in `linker`, so they fail at the line the user
    // wrote instead of after a resolve.
    if matches!(install.linker, Some(LinkerConfig::Pnp)) {
        anyhow::bail!(
            "nub: `install.linker: \"pnp\"` is reserved and not supported yet [ERR_NUB_CONFIG_UNSUPPORTED]"
        );
    }

    let mut layout: Vec<(String, String)> = Vec::new();
    let mut push = |key: &str, value: &str| layout.push((key.to_string(), value.to_string()));
    let mut eject_patterns: Option<Vec<String>> = None;
    if let Some(linker) = install.linker.as_ref() {
        // Both symlink layouts are aube's `isolated`; they differ only in where
        // the store lives, which is `enableGlobalVirtualStore`.
        push(
            "nodeLinker",
            match linker {
                LinkerConfig::Global { .. } | LinkerConfig::Isolated { .. } => "isolated",
                LinkerConfig::Hoisted => "hoisted",
                LinkerConfig::Pnp => unreachable!("pnp rejected above"),
            },
        );
        match linker {
            LinkerConfig::Isolated { hoist } => {
                push("enableGlobalVirtualStore", "false");
                match hoist {
                    Some(Hoist::Bool(enabled)) => {
                        push("hoist", &enabled.to_string());
                        if *enabled {
                            push("hoistPattern", "*");
                        }
                    }
                    Some(Hoist::Patterns(patterns)) => {
                        push("hoist", "true");
                        push("hoistPattern", &patterns.join(","));
                    }
                    None => {}
                }
            }
            LinkerConfig::Global { eject } => {
                // Injected dependencies require the hidden hoist tree, which in
                // turn requires a project-local store. That unconditional engine
                // default wins over a request for the ordinary global layout:
                // never emit an explicit GVS=true that would turn the compatible
                // combination into the engine's `hoist`/GVS contradiction.
                let injected = embedder_defaults
                    .iter()
                    .any(|(key, value)| key == "hoist" && value == "true");
                if !injected {
                    // Written explicitly rather than left to the engine default.
                    // The projectConfig tier outranks lower settings, so silence
                    // here could quietly turn an explicit global request local.
                    push("enableGlobalVirtualStore", "true");
                }
                eject_patterns = eject.clone();
            }
            LinkerConfig::Hoisted | LinkerConfig::Pnp => {}
        }
    }

    // One nub knob, two aube settings — so write BOTH. Writing only the pattern
    // leaves `shamefullyHoist` at whatever a lower tier resolved, which inverts
    // the request: `.npmrc` `shamefully-hoist=true` under a `publicHoist`
    // pattern list would hoist everything, the opposite of naming patterns.
    // Naming patterns is always a narrowing, so the blanket flag goes off.
    if let Some(patterns) = install.public_hoist.as_ref() {
        push("shamefullyHoist", "false");
        push("publicHoistPattern", &patterns.join(","));
    }

    // ADDITIVE, deliberately: the merge seeds from the embedder's own ejects —
    // the packages known to break when their realpath is outside the project —
    // and only appends. A project can grow that set, never shrink it, so an
    // explicit `[]` writes the embedder's value back rather than clearing it.
    if let Some(patterns) = eject_patterns.as_ref() {
        for key in [
            "disableGlobalVirtualStoreForPackages",
            "diskMaterializePackages",
        ] {
            let mut merged = embedder_defaults
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, value)| {
                    value
                        .split(',')
                        .filter(|entry| !entry.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for pattern in patterns {
                if !merged.contains(pattern) {
                    merged.push(pattern.clone());
                }
            }
            push(key, &merged.join(","));
        }
    }

    // Everything above shapes the TREE; everything below shapes the RESOLVE.
    let mut resolution = Vec::new();
    if let Some(age) = install.minimum_release_age {
        let seconds = age.as_secs();
        // Round UP: a sub-minute remainder must not weaken the configured gate.
        let minutes = seconds / 60 + u64::from(seconds % 60 != 0);
        resolution.push(("minimumReleaseAge".to_string(), minutes.to_string()));
        resolution.push(("minimumReleaseAgeStrict".to_string(), "true".to_string()));
    }
    if let Some(exclude) = install.minimum_release_age_exclude.as_ref() {
        resolution.push(("minimumReleaseAgeExclude".to_string(), exclude.join(",")));
    }

    Ok(NativeInstallSettings {
        layout,
        resolution,
        eject: eject_patterns.unwrap_or_default(),
    })
}

/// Lower the project `install` block under every identity. The downstream
/// [`scoped_install_settings`] keeps layout everywhere and admits release-age
/// settings only under Nub's own identity.
///
/// Lowering unconditionally also means a malformed layout value is now rejected
/// in a compat project rather than silently skipped: a field Nub honors is a
/// field Nub validates.
fn lower_native_install_settings_for_mode(
    install: Option<&crate::project_config::InstallConfig>,
    embedder_defaults: &[(String, String)],
) -> Result<NativeInstallSettings> {
    match install {
        Some(install) => lower_native_install_settings(install, embedder_defaults),
        None => Ok(NativeInstallSettings::default()),
    }
}

/// Per-process, mtime-validated cache of parsed `aube_manifest::PackageJson`
/// keyed by file path. The PM-engine config phase parses the root manifest
/// through aube's parser several times per command — `apply_config_scope`, the
/// scan's `manifest_has_pnpm_overrides`, and `injected_deps_present`'s
/// `manifest_has_injected` — and `first_catalog_specifier` parses every member
/// manifest. This collapses repeat parses of one path to a single read. mtime
/// validation keeps it stale-proof (a mid-command engine rewrite re-reads).
///
/// A parse ERROR (or missing file) yields `None` and is NOT cached, matching
/// every call site's existing `let Ok(..) = .. else { skip }` handling exactly.
static AUBE_MANIFEST_CACHE: nub_core::config_cache::MtimeCache<aube_manifest::PackageJson> =
    nub_core::config_cache::MtimeCache::new();

/// Read + parse `path` as an `aube_manifest::PackageJson` through
/// [`AUBE_MANIFEST_CACHE`]. `None` on a missing/unparseable manifest (never
/// cached) — behavior-identical to a direct `PackageJson::from_path(path).ok()`,
/// just deduplicated across the repeat parses one command makes of the same path.
pub(crate) fn cached_aube_manifest(
    path: &Path,
) -> Option<std::sync::Arc<aube_manifest::PackageJson>> {
    AUBE_MANIFEST_CACHE.get_or_read(path, || aube_manifest::PackageJson::from_path(path).ok())
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

/// Whether the project's incumbent pnpm is provably v11+, the major at which
/// pnpm switched its env-var convention from `npm_config_*` to `pnpm_config_*`.
/// Reads the declared `packageManager`/`devEngines` pin (`declared_pm_raw`,
/// packageManager first) and requires the name to be LITERALLY "pnpm" — a
/// non-pnpm or unknown declared name (or a fresh project with no declaration)
/// yields `false`. An undeclared/unparseable version also yields `false`: the
/// dominant v9/v10 base ignores `pnpm_config_*`, so off is the safe default.
/// Mirrors `store_config_family::project_scalar_home`'s detection exactly.
fn pnpm_incumbent_major_is_v11_plus() -> bool {
    declared_pnpm_major().is_some_and(|major| major >= 11)
}

/// The declared incumbent pnpm major, or `None` when the project does not
/// declare pnpm as its package manager (a non-pnpm/unknown/absent
/// `packageManager`/`devEngines` name, or an undeclared/unparseable version).
/// Reads the pin packageManager-first, requiring the name to be LITERALLY
/// "pnpm" — the shared major detection behind [`pnpm_incumbent_major_is_v11_plus`]
/// and [`pnpm_npmrc_key_policy`], matching
/// `store_config_family::project_scalar_home`.
fn declared_pnpm_major() -> Option<u64> {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| nub_core::pm::resolve::declared_pm_raw(&cwd))
        .and_then(|(name, version)| (name == "pnpm").then_some(version).flatten())
        .and_then(|v| parse_major_minor(&v).0)
}

/// How a detected pnpm major reads project/user `.npmrc` for *settings*. pnpm
/// reversed this at v11: v9/v10 (and the unknown-major default) read the open
/// key space — every setting is readable from `.npmrc`; v11's base policy reads
/// only the auth/registry/network allowlist there. Nub later re-admits layout
/// keys because it does not take layout from branded YAML. Keyed on the major so
/// a future major slots in as one more arm rather than a new special case.
enum NpmrcKeyPolicy {
    /// pnpm ≤10 / npm / unknown-major default: the open `.npmrc` key space.
    Open,
    /// pnpm 11+: the auth/registry/network allowlist only (pnpm 11's
    /// `isNpmrcReadableKey`); layout/behavior keys are dropped.
    Pnpm11Allowlist,
}

/// Map a detected incumbent pnpm major to its [`NpmrcKeyPolicy`]. An unknown
/// major (`None`) defaults to `Open` — the dominant/most-compatible model per
/// AGENTS ("unknown major → the v10 open-`.npmrc` model").
fn pnpm_npmrc_key_policy(major: Option<u64>) -> NpmrcKeyPolicy {
    match major {
        Some(m) if m >= 11 => NpmrcKeyPolicy::Pnpm11Allowlist,
        _ => NpmrcKeyPolicy::Open,
    }
}

/// Whether the scoping warning should be dim-styled. Delegates to the CLI's single
/// color predicate so an explicit `--color`/`--no-color` governs the engine's
/// warnings too, and so `FORCE_COLOR=0` reads as OFF rather than as merely set.
pub(crate) fn scope_warning_uses_dim() -> bool {
    use std::io::IsTerminal;
    crate::cli::color_enabled(std::io::stderr().is_terminal())
}

/// The pnpm version the role-first UA advertises for a pnpm-role project with
/// no pinned version — the engine's parity claim (full pnpm-v11 settings
/// catalog + v11 build-policy posture; see the pnpm-11 compat decision,
/// epics/v0.2-aube). A `packageManager`/`devEngines` pin always outranks this:
/// the UA impersonates the pinned version when one exists.
pub(crate) const PNPM_PARITY_VERSION: &str = "11.3.0";

/// The project-local virtual-store directory leaf under `node_modules/`.
/// `.store` is the vendor-neutral isolated-store convention (npm isolated-mode
/// RFC-0042, Yarn-berry's pnpm-linker `pnpmStoreFolder`, cnpm), so tools that
/// walk `node_modules` for a project root — simple-git-hooks and its class —
/// recognize it as an install marker; the former `.nub` leaf was invisible to
/// them. Single source of truth for the name: `nub_setting_defaults` sets
/// `virtualStoreDir`/`stateDir` to `node_modules/<leaf>`, and vite_compat scans
/// it. Standalone aube is unaffected — its default stays `.aube`; this is a
/// nub-embedder value.
// @lat: [[research/store-marker-hardcoding#Synthesis / recommendation (recommend-only)]]
pub(crate) const PROJECT_VIRTUAL_STORE_LEAF: &str = ".store";

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
    let detected = resolve_identity_walk_up(cwd, IdentityStrictness::Lenient)
        .ok()
        .flatten();
    compose_lifecycle_ua(
        nub_core::pm::resolve::declared_pm_raw(cwd),
        detected.map(|d| d.kind),
        node_version,
    )
}

/// Pure core of [`lifecycle_ua_product`] (unit-tested without a fixture).
///
/// The leading token is the contested part: a `nub/`-first string is honest but
/// unrecognized by the whitelist detectors (`package-manager-detector`,
/// create-next-app), which fall back to npm and print npm commands. That cost
/// was weighed against masquerading as the incumbent, and honesty won.
// @lat: [[research/npm-config-user-agent#Current behavior]]
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
    runtime_json: Option<&str>,
) -> (Vec<(std::ffi::OsString, std::ffi::OsString)>, Vec<PathBuf>) {
    use std::ffi::OsString;
    let mut overlay: Vec<(OsString, OsString)> = Vec::new();
    // $NODE → the shim (→ nub) so userland `$NODE child.js` / `spawn(env.NODE)`
    // in a build script stays augmented, exactly as build_script_command sets it.
    let node_shim = aug.node_shim_exe();
    if let Some(node_shim) = &node_shim {
        overlay.push((OsString::from("NODE"), node_shim.clone()));
    }
    if let Some(opts) = &aug.node_options {
        overlay.push((OsString::from("NODE_OPTIONS"), OsString::from(opts)));
    }
    if let Some(node_path) = &aug.node_path {
        overlay.push((OsString::from("NODE_PATH"), node_path.clone()));
    }
    let mut set_env = |key: &str, value: &std::ffi::OsStr| {
        overlay.push((OsString::from(key), value.to_os_string()));
    };
    aug.apply_restore_markers(&mut set_env);
    let ambient_node = std::env::var_os("NODE");
    nub_core::node::spawn::apply_expected_augmentation_marker(
        "NODE",
        node_shim.as_deref().or(ambient_node.as_deref()),
        &mut set_env,
    );
    // Aube composes its lifecycle `.bin` chain after this overlay, so the exact
    // final PATH is not available here. Mark the ambient baseline: a fresh
    // boundary will then remove only Nub's exact shim component and preserve
    // both lifecycle `.bin` entries and any later user additions.
    let ambient_path = std::env::var_os("PATH");
    nub_core::node::spawn::apply_expected_augmentation_marker(
        "PATH",
        ambient_path.as_deref(),
        &mut set_env,
    );
    if let Some(runtime_json) = runtime_json {
        overlay.push((
            OsString::from(crate::project_config::RUNTIME_CONFIG_ENV),
            OsString::from(runtime_json),
        ));
    }
    // localStorage-neutralize signal for dependency build scripts' node children
    // (webstorage flag-needed band, no user --localstorage-file); preload reads + deletes.
    aug.apply_localstorage_env(|k, v| {
        overlay.push((OsString::from(k), OsString::from(v)));
    });
    aug.apply_threadpool_size(|k, v| {
        overlay.push((OsString::from(k), v.to_os_string()));
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

/// The directory lifecycle Node discovery is anchored at: the workspace root
/// when `cwd` sits in a member, else the project root, else `cwd` itself.
///
/// aube keys its install state and virtual store at the workspace root
/// (`dirs::workspace_or_project_root`), and an install from a member
/// materializes the ONE shared tree for the whole workspace — so the Node its
/// build scripts compile against, which is also the engine that keys the ABI
/// caches, must be the root's pin rather than whichever member the shell sits
/// in. Anchoring at the raw cwd instead lets a member's own `.nvmrc` flip the
/// engine key against a root-anchored state file, thrashing the warm path.
/// `detect_project` walks up by the same rule aube's root does.
fn lifecycle_node_anchor(cwd: &Path) -> PathBuf {
    nub_core::workspace::detect::detect_project(cwd)
        .map(|p| p.workspace_root.unwrap_or(p.root))
        .unwrap_or_else(|| cwd.to_path_buf())
}

pub(crate) struct DetectedLockfile {
    pub(crate) kind: LockfileKind,
    /// Directory the identity resolved in (project / workspace root).
    pub(crate) dir: PathBuf,
}

/// Resolve the project's PM identity, walking up like the PM-redirect
/// detector does (a member dir inside a workspace has no lockfile or
/// declaration of its own; the root's governs the layout). Per level the
/// engine's declaration-aware policy applies — declaration first, lockfile
/// inference second; contradiction/ambiguity are loud errors carrying the
/// `nub pm use` remedy.
fn resolve_identity_walk_up(
    cwd: &Path,
    strictness: IdentityStrictness,
) -> Result<Option<DetectedLockfile>> {
    use aube_lockfile::ResolvedLockfileKind;
    // The walk never escapes nub's own PM cache root. Installs running inside
    // it are nub-internal by construction (the node-gyp bootstrap's recursive
    // install, dlx scratch dirs) and must not inherit identity from whatever
    // sits above the cache — unbounded, a first-run bootstrap dir (manifest,
    // no lockfile yet) walked into $HOME and hard-failed the outer install on
    // unrelated-lockfile ambiguity (#489). The root is kept in whichever
    // spelling is an ancestor of `cwd` (raw, or canonicalized for the
    // symlinked-temp-dir case) so the containment test stays consistent as
    // `dir` pops.
    let clamp = aube_store::dirs::cache_dir().and_then(|root| {
        if cwd.starts_with(&root) {
            return Some(root);
        }
        let canon = std::fs::canonicalize(&root).ok()?;
        cwd.starts_with(&canon).then_some(canon)
    });
    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        match aube_lockfile::resolve_project_lockfile_kind(&dir) {
            // A declaration with no lockfile on disk yet decides the identity
            // exactly as an existing lockfile does. The two were distinguished
            // for the yarn write gate, which needed to know that a first
            // install would CREATE the gated file; that gate lived in the
            // nub-side verb runners and went with them.
            Ok(
                ResolvedLockfileKind::Existing(kind) | ResolvedLockfileKind::DeclaredFresh(kind),
            ) => {
                return Ok(Some(DetectedLockfile { kind, dir }));
            }
            // Nothing at this level decides the identity — keep walking.
            Ok(ResolvedLockfileKind::Fresh) => {}
            // An ambiguity / declaration-contradiction is a HARD error only
            // for the project-touching families (Strict). The transient
            // fetch-and-run families (Lenient) never read or write the CWD
            // lockfile, so the ambiguity is irrelevant — degrade to
            // no-identity and let the engine's fresh-project defaults stand.
            Err(_) if strictness == IdentityStrictness::Lenient => return Ok(None),
            Err(err) => return Err(identity_error(err)),
        }
        if !dir.pop() {
            break;
        }
        if let Some(root) = &clamp
            && !dir.starts_with(root)
        {
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
///
/// The two states get DIFFERENT remedies because "set the declaration" is a
/// dead end for one of them. A declaration naming nub is a self-name: the
/// engine reads it as "preserve whatever format this project already uses"
/// (`aube-lockfile`'s `is_self_name` carve-out), which resolves nothing when
/// two candidate lockfiles are present. Since `nub install` WRITES that
/// declaration itself, telling an ambiguous project to declare its manager
/// sends the common case in a circle — the state a hosted builder manufactures
/// when it runs its own install beside nub's lockfile.
fn identity_error(err: aube_lockfile::Error) -> anyhow::Error {
    use aube_lockfile::Error as E;
    const MISMATCH_REMEDY: &str =
        "set the declaration: nub pm use <pm> — or remove the stale lockfile";
    const AMBIGUITY_REMEDY: &str =
        "remove the stale lockfile, or run nub pm use <pm> naming a specific manager";
    let report = match &err {
        E::DeclarationMismatch {
            declared,
            field,
            expected,
            found,
        } => miette::miette!(
            code = aube_codes::errors::ERR_AUBE_LOCKFILE_DECLARATION_MISMATCH,
            help = MISMATCH_REMEDY,
            "package.json declares `{declared}` (via `{field}`), but {expected} is missing — \
             found {found} instead"
        ),
        E::AmbiguousLockfiles { found } => miette::miette!(
            code = aube_codes::errors::ERR_AUBE_LOCKFILE_AMBIGUOUS,
            help = AMBIGUITY_REMEDY,
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
/// seams. Called once per command (via [`engine_session_inner`]) **before any
/// engine code reads project state** — the getters behind these setters are
/// freeze-on-first-read `OnceLock`s, and even lockfile detection reads the
/// workspace config transitively (see the ordering note on
/// [`engine_session_inner`]). Every seam is idempotent.
pub(crate) fn engine_brand_preflight() {
    // Static identity FIRST, before anything reads project state or branding.
    // The whole compile-time profile — name, `nub/<ver>` UA, `nub.lock`
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
    // move together behind the same ConfigSurface decision: `read_branded_pnpm_config`
    // gates the project-local `pnpm-workspace.yaml` candidate and `pnpm`
    // package.json namespace; `read_pnpm_global_config` separately gates pnpm's
    // global `config.yaml` and `~/.config/pnpm/auth.ini`. The project-local gate
    // remains true on the conservative PnpmOrFresh surface, while the global
    // gate additionally requires a provable pnpm-v11+ incumbent because both
    // global files were introduced by pnpm 11; an unknown major follows the
    // dominant v10 model and leaves them unread.
    // `read_manifest_root_config` is true only under nub identity, where
    // root-level config migrated by `nub pm use nub` is the native surface.
    // `pnpmfile_default_enabled` gates the cwd-default `.pnpmfile`; true only in
    // the PnpmOrFresh arm. The probe
    // is engine-free (plain manifest/lockfile-presence reads): ONE walk up the
    // tree, ONE `current_dir()` read (see [`resolve_config_surface`]).
    let cwd = std::env::current_dir().ok();
    let surface = cwd
        .as_deref()
        .map(resolve_config_surface)
        .unwrap_or(ConfigSurface::PnpmOrFresh);
    let read_branded_pnpm_config = matches!(surface, ConfigSurface::PnpmOrFresh);
    let pnpm_v11_incumbent = cwd
        .as_deref()
        .is_some_and(|cwd| pnpm_v11_surface(&surface, cwd));
    // pnpm REVERSED its env-var convention at v11: pnpm ≤10 reads `npm_config_*`
    // registry-client env vars and IGNORES `pnpm_config_*`; pnpm 11 reads
    // `pnpm_config_*` / `PNPM_CONFIG_*` and IGNORES `npm_config_*`. Honor bare
    // `pnpm_config_<registry-client-key>` (registry, proxies, TLS knobs) only
    // under a provable pnpm-v11+ incumbent, so nub mirrors the project's actual
    // pnpm. Detection matches `store_config_family::project_scalar_home`: the
    // declared `packageManager`/`devEngines` pin, name LITERALLY "pnpm", major
    // ≥ 11. Unknown/undeclared version → off (the dominant v9/v10 base ignores
    // `pnpm_config_*`); the installed-PM `--version` probe and lockfile signal
    // are intentionally not consulted (they'd only move an unknown off its
    // already-correct default). `npm_config_*` keeps working universally.
    let read_pnpm_config_env_registry =
        read_branded_pnpm_config && pnpm_incumbent_major_is_v11_plus();
    // Start from pnpm 11's project/user `.npmrc` allowlist; pnpm ≤10 reads the
    // open key space. The layout exception is paired below through
    // `read_layout_from_workspace_yaml = false`, which makes the loader retain
    // `.npmrc` layout keys. Mirror the detected pnpm major (the per-major compat rule),
    // architected as the `major → policy` map in [`pnpm_npmrc_key_policy`].
    // Gated on `read_branded_pnpm_config` so it engages only under a pnpm
    // incumbent — a fresh/unknown-major project keeps the open v10 model.
    let npmrc_settings_allowlist = read_branded_pnpm_config
        && matches!(
            pnpm_npmrc_key_policy(declared_pnpm_major()),
            NpmrcKeyPolicy::Pnpm11Allowlist
        );
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
    // npm save-prefix convention: only under an npm incumbent does a bare-exact
    // `add pkg@1.2.3` get npm's `^` save-prefix (`"^1.2.3"`). pnpm/bun/nub-identity
    // preserve the literal bare version — matching each PM's real behavior.
    let npm_save_prefix_on_bare_exact =
        matches!(surface, ConfigSurface::NonPnpmCompat { role: "npm", .. });
    aube_util::update_engine_context(|c| {
        c.read_branded_pnpm_config = read_branded_pnpm_config;
        c.npmrc_settings_allowlist = npmrc_settings_allowlist;
        // `config.yaml` and `auth.ini` are pnpm-NAMED global files. Read them
        // only for a provable pnpm-v11+ incumbent; unknown majors use the v10
        // model and never inherit pnpm global state. Global writes remain
        // neutral-only (`config set -g` never writes pnpm's files).
        c.read_pnpm_global_config = pnpm_v11_incumbent;
        // Nub chooses the `node_modules` layout under every identity; branded
        // config files do not direct the tree, including pnpm's workspace YAML.
        // Mirroring an incumbent's layout only reads coherently if its DEFAULT
        // is mirrored too — npm and bun default to hoisted, so honoring that
        // would end nub's isolated default — and honoring the written key while
        // ignoring the identical unwritten intent is a seam users cannot
        // predict. Dropping the whole category is the only version without one.
        //
        // Load-bearing pairing: this same `false` makes
        // `apply_npmrc_settings_allowlist` keep layout keys, so a pnpm 11
        // project still configures layout through `.npmrc` / `--node-linker`
        // rather than being left with no surface at all.
        c.read_layout_from_workspace_yaml = false;
        c.read_pnpm_config_env_registry = read_pnpm_config_env_registry;
        c.read_yarn_config = read_yarn_config;
        c.yarn_is_classic = yarn_is_classic;
        c.read_bun_config = read_bun_config;
        c.read_manifest_root_config = read_manifest_root_config;
        c.pnpmfile_default_enabled = pnpmfile_default_enabled;
        c.synthetic_user_npmrc_entries = bunfig.user;
        c.synthetic_project_npmrc_entries = bunfig.project;
        c.npm_save_prefix_on_bare_exact = npm_save_prefix_on_bare_exact;
        // pnpm's `namedRegistries` alias routing is a pnpm-compat surface, so it
        // engages under the same posture as the pnpm-branded config reads
        // (pnpm incumbent or fresh nub-as-pnpm-drop-in). Distinct EngineContext
        // bool because that posture defaults `true` in standalone aube, which
        // would activate the feature there and break default-preservation.
        c.named_registries_enabled = read_branded_pnpm_config;
        // nub treats packageExtensions as a checksummed, drift-enforced config
        // like pnpm: it stamps `packageExtensionsChecksum` on its own generic
        // lockfile (nub.lock) and re-resolves / frozen-fails on a mismatch.
        // Unconditional (not surface-gated) — harmless under lockfile kinds that
        // carry no checksum (npm/yarn/bun locks), where stored and computed both
        // resolve to `None`. Standalone aube leaves the default `false`.
        c.enforce_package_extensions_checksum = true;
        // The bundled compatibility database, on top of the vendored Yarn and
        // pnpm catalogs the engine already applies. Lowest precedence and purely
        // additive, so a curated upstream rule always wins on a key both carry —
        // and since extensions merge per DEPENDENCY NAME rather than per
        // selector, an entry this database extends beyond Yarn's still lands.
        //
        // Read only when resolving a package, never by the lockfile checksum, so
        // refreshing the dataset cannot drift an existing lockfile. Gated with
        // the vendored catalogs by the one `ignoreCompatibilityDb` escape hatch.
        c.bundled_package_extensions = Some(compat_db::bundled_package_extensions().clone());
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
            // a `package.json#pnpm.*` object, or pnpm's global `config.yaml` /
            // `auth.ini` in an npm/yarn/bun project is another tool's state. The cwd-default
            // `.pnpmfile.cjs`/`.mjs` is pnpm-proprietary too, and unlike a
            // workspace-yaml *it shapes resolution* — gated off here by
            // `pnpmfile_default_enabled = false`. Explicit
            // `--pnpmfile`/`--global-pnpmfile` overrides still load (a path named
            // on purpose). One dim warning when a present default file is
            // suppressed, matching the pnpm-workspace.yaml ignore-with-warning
            // pattern.
            //
            // The warning USED to be suppressed for a user who had already named
            // a pnpmfile explicitly — its own remedy says to do that, so telling
            // them to is a contradiction. The suppression rode a process flag set
            // by the nub-side `add`/`install` runners, and the engine now parses
            // those verbs at the front door, so nothing sets it. Restoring the
            // suppression means reading the flags where the engine parses them.
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
            // `pnpm` package.json namespace live. The separate global gate keeps
            // pnpm 11's `config.yaml`/`auth.ini` live only when that major is
            // provable. nub's own branded YAML/namespace are `None`/`""` on the
            // const, so an `aube-workspace.yaml` or `aube` manifest object some
            // other tool left on disk is neither read nor chosen as a
            // fresh-write target.
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

/// Whether the project is a provable pnpm-v11+ incumbent — the gate on the two
/// engine postures that pnpm 11 introduced together: the pnpm-NAMED global
/// files (`config.yaml` + `auth.ini`), and `pnpm-workspace.yaml` as the home for
/// layout settings that v10 keeps in `.npmrc`. They stay SEPARATE EngineContext
/// fields (the engine loads global files by a different path) but share one
/// predicate, because they share one version boundary.
///
/// Keyed on the declared major like the sibling env/npmrc policies: a lockfile
/// or a pnpm-named project file proves the incumbent's NAME but not its major,
/// so it correctly stays on the unknown-major v10 default. nub, npm, yarn, and
/// bun identities never reach either surface.
///
/// Takes `cwd` rather than reading it, so `engine_brand_preflight` keeps to its
/// one `current_dir()` read.
fn pnpm_v11_surface(surface: &ConfigSurface, cwd: &Path) -> bool {
    matches!(surface, ConfigSurface::PnpmOrFresh)
        && nub_core::pm::resolve::declared_pm_raw(cwd)
            .and_then(|(name, version)| (name == "pnpm").then_some(version).flatten())
            .and_then(|version| parse_major_minor(&version).0)
            .is_some_and(|major| major >= 11)
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
        let nub_lock = nub_lockfile_present(&dir);
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
    // identity: the next install writes `nub.lock` and stamps the manifest,
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

/// Every workspace member's directory under `root`, or none when `root` is not
/// a workspace.
///
/// The one place the member walk is spelled. Callers that need the manifests
/// read them through [`cached_aube_manifest`], which is mtime-cached, so
/// sharing one discovery across several scans costs nothing beyond the walk
/// itself.
pub(crate) fn workspace_members(root: &Path) -> Vec<PathBuf> {
    // Memoized because the walk underneath is not: the engine's walk globs
    // the tree afresh on every call, and two callers now ask the same
    // question per install — the embedder defaults and the settings merge.
    // Membership cannot change inside one process, so the first answer for a
    // root is the answer. Keyed by the root rather than cached in a single
    // slot: `--dir` and a workspace member's own cwd resolve to different
    // roots in one run, and a one-slot cache would hand the second the first
    // one's members.
    type Members = std::collections::HashMap<PathBuf, Vec<PathBuf>>;
    static MEMBERS: std::sync::LazyLock<std::sync::RwLock<Members>> =
        std::sync::LazyLock::new(|| std::sync::RwLock::new(Members::new()));
    // Poisoning is ignored both ways: the guarded value is a plain map of
    // discovered paths, so a panic mid-write cannot leave it inconsistent.
    if let Some(hit) = MEMBERS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(root)
    {
        return hit.clone();
    }
    let found = discover_workspace_members(root);
    MEMBERS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), found.clone());
    found
}

/// Expand the project's member globs through the engine's own walk.
///
/// The engine unconditionally reports the workspace root as a project of its
/// own (pnpm/pnpm#1986), which this drops: every caller here already takes the
/// root as a separate argument and scans it in its own right, so letting it
/// through would have each of them read the root manifest twice.
fn discover_workspace_members(root: &Path) -> Vec<PathBuf> {
    let patterns = workspace_patterns(root);
    // An explicit empty pattern list means "the root alone" to the engine,
    // which after the filter above is no members at all — but the walk still
    // costs a full glob of the tree to arrive there, so stop here instead.
    if patterns.is_empty() {
        return Vec::new();
    }
    let opts = pnpm_workspace::FindWorkspaceProjectsOpts {
        patterns: Some(patterns),
    };
    let Ok(projects) = pnpm_workspace::find_workspace_projects(root, &opts) else {
        return Vec::new();
    };
    let mut members: Vec<PathBuf> = projects
        .into_iter()
        .map(|project| project.root_dir)
        .filter(|dir| dir != root)
        .collect();
    members.sort_unstable();
    members
}

/// The member globs to expand under `root`, chosen by the project's identity.
///
/// The engine takes its patterns from the caller rather than discovering them
/// itself, and that is what lets the SOURCE obey the brand boundary. The
/// vendored engine's walk read `pnpm-workspace.yaml` first whatever the
/// project's identity, so a nub project that happened to carry one took its
/// members from a pnpm-named file — the one behavior this swap deliberately
/// changes. Under pnpm the precedence is pnpm's own and unchanged.
fn workspace_patterns(root: &Path) -> Vec<String> {
    if project_identity::detect(root) == project_identity::ProjectIdentity::Pnpm {
        // A malformed workspace manifest reads as absent here rather than as
        // an error: the engine raises it on the install path with its own
        // diagnostic and its own source span, and a membership scan that
        // several read-only surfaces call is not the place to surface it.
        if let Ok(Some(manifest)) = pnpm_workspace::read_workspace_manifest(root) {
            return pnpm_workspace::workspace_package_patterns(&manifest);
        }
    }
    cached_aube_manifest(&root.join("package.json"))
        .and_then(|pkg| pkg.workspaces.as_ref().map(|w| w.patterns().to_vec()))
        .unwrap_or_default()
}

/// The declared framework, if any, whose resolver cannot reach a store shared
/// between projects — so this install must build its virtual store inside the
/// project.
///
/// The same behavioural list [`nub_setting_defaults`] hands the vendored engine
/// as `disableGlobalVirtualStoreForPackages`, asked the other way round. That
/// setting names candidates and lets the engine match them against what the
/// project declares; pnpm 12 has no such setting, so under it nub has to do the
/// matching itself and decide the store's locality directly
/// ([`host_settings`]). Both callers read this one definition, because a list
/// that drifted from the predicate would put a framework's name in the install
/// report while the install built the tree that framework cannot load.
///
/// Returns the first match rather than a bool: the reason is worth reporting,
/// and the names are ordered so the unconditional ones answer first.
pub(crate) fn store_locality_breaker(root: &Path, members: &[PathBuf]) -> Option<&'static str> {
    // `next` and `react-native` break at every version, so declaring one is
    // the whole test. `declared_direct_ranges` is the shared dependency-scope
    // scan the version gates use (dependencies / devDependencies /
    // optionalDependencies, root and members alike, peer excluded); here only
    // whether it found anything matters, not what the range says.
    for name in ["next", "react-native"] {
        if !expo_compat::declared_direct_ranges(root, members, name).is_empty() {
            return Some(name);
        }
    }
    if expo_compat::expo_below_gvs_floor(root, members) {
        return Some("expo");
    }
    if remix_compat::remix_needs_project_local_store(root, members) {
        return Some("remix");
    }
    None
}

/// The defaults a config READ reports, for the project containing `cwd`.
///
/// The same list the install resolves against, anchored the same way
/// [`project_supplied_settings`] anchors it. `config get` answers what an
/// install would USE, so a setting nub defaults reports that default rather
/// than `undefined` — `minimumReleaseAge` is `1440` in a project that has
/// never configured it, because that is the quarantine the next install
/// applies. A setting absent from this list has no nub default and stays
/// unset, which is what keeps a layout key out of the answer.
pub(crate) fn nub_config_defaults(cwd: &Path) -> Vec<(String, String)> {
    let detected = resolve_identity_walk_up(cwd, IdentityStrictness::Lenient).unwrap_or(None);
    let truly_fresh = is_truly_fresh_project(cwd, detected.as_ref());
    nub_setting_defaults(
        detected.as_ref(),
        truly_fresh,
        cwd,
        VirtualStoreLocality::Default,
    )
}

/// - Layout policy: EVERY project defaults to the isolated layout
///   (`nodeLinker=isolated`) — strict (no phantom deps) and GVS-fast; a project
///   that relies on phantom deps opts back into the flat tree with
///   `install.linker: "hoisted"`. Hoisting is left GVS-AWARE via the engine's
///   `gvs_over_default_hoist` profile: a NON-injected project leaves `hoist` at
///   the built-in default (`true`), and under that profile a DEFAULT hoist no
///   longer vetoes the shared store — so the global virtual store engages
///   wherever it's active (off-CI, no next/nuxt/parcel trigger, no explicit
///   `enableGlobalVirtualStore=false`) with NO hidden tree, and the pnpm-parity
///   hidden hoist tree (`node_modules/.store/node_modules/`) is built wherever GVS
///   is OFF (CI, `nub ci`, a trigger, an explicit GVS opt-out, dlx), restoring
///   ambient `@types/*` resolution for store-resident packages (#286). The ONE
///   carve-out is injected deps (`dependenciesMeta.*.injected`): they need the
///   hidden tree unconditionally, so nub pushes an EXPLICIT `hoist=true`, which
///   DOES veto GVS under the profile (per-project + hidden tree, always). A
///   higher-precedence CLI/environment values still win over native config;
///   incumbent projects keep resolving their own PM config unchanged.
/// - Fresh-write lockfile format: a TRULY-fresh project (no PM-preference
///   signal of any kind — `truly_fresh`) writes nub's neutral `nub.lock`
///   (`defaultLockfileFormat=nub`); every other surface writes `pnpm-lock.yaml`,
///   keeping a pnpm-incumbent / mixed project drop-in interoperable. A
///   user-set `defaultLockfileFormat` (env/.npmrc/yaml) still wins on either
///   path — this is only the embedder-tier default.
fn nub_setting_defaults(
    detected: Option<&DetectedLockfile>,
    truly_fresh: bool,
    cwd: &Path,
    store_locality: VirtualStoreLocality,
) -> Vec<(String, String)> {
    // `nub`, not `aube`: same format, but the value is user-visible — `nub
    // config` lists it — so the engine's brand must not be the answer to "what
    // will the first install write". The engine accepts both spellings.
    let fresh_format = if truly_fresh { "nub" } else { "pnpm" };
    // The phantom-adapter disk-materialize list is no longer hand-curated: the
    // dynamic per-version scanner (`dynamic_phantom` → `phantom_closure`, on by
    // default) detects undeclared-import phantoms per content-fingerprint and
    // ejects them through #319's ancestor-closure, subsuming the old 14-entry
    // const (incl. the singleton-hazard adapters the closure now makes sound). So
    // this embedder default carries ONLY the #315 vite eject; empty otherwise
    // (aube's `parse_string_list` drops the empty entry). nub exposes no direct
    // user-facing `diskMaterializePackages` knob: native
    // `install.linker.eject` contributes an explicit seed, while nub's
    // hook drops incumbent `.npmrc`/env/workspace seed names and retains only the
    // native seed plus this internal vite entry. Standalone aube installs no hook
    // and still honors its setting unchanged.
    //
    // Vite symlink-GVS compat (#315): eject the `vite` package project-local so
    // its dist can be patched with the backported fs.allow sniff (< 8.1) without
    // touching the shared CAS store. Scoped to a DIRECT-dep vite (a raw `vite`
    // app / `vite dev` CLI project) — that is the only shape whose loaded Vite is
    // the ejected project-local copy the backport reaches; a library-embedded
    // framework (Astro/SvelteKit) loads its Vite from the shared store via a
    // sibling symlink, so ejecting for it would be wasted dedup (Unit A's
    // `.modules.yaml` covers those for Vite ≥ 8.1). Only under the machine-global
    // store (`Default`) — `nub ci`'s project-local store is already under the
    // workspace root, so Vite serves it with no override. This seed also carries
    // #315 on the phantom-eject opt-out path, where the closure hook is not
    // installed. The post-install writer/patcher lives in [`vite_compat`]; this is
    // its materialization half.
    let disk_materialize = if store_locality == VirtualStoreLocality::Default
        && vite_compat::enabled()
        && vite_compat::manifest_declares_vite(detected.map(|d| d.dir.as_path()).unwrap_or(cwd))
    {
        // Draw from the hook's allowlist so the embedder default and the seed the
        // hook keeps ([`phantom_closure::nub_internal_seed`]) can't drift.
        phantom_closure::NUB_INTERNAL_DISK_MATERIALIZE_SEED.join(",")
    } else {
        String::new()
    };
    // The whole-install GVS-off list. `next`/`react-native` eject unconditionally
    // (a resolver that canonicalizes node_modules by realpath — Turbopack, bare-RN
    // Metro — can't reach the machine-global store at any version). `expo` is
    // version-gated: it gained store-awareness only in SDK 56 (On-demand
    // Filesystem), so a project declaring `expo` below the floor is ejected while
    // 56+ keeps GVS. See [`expo_compat`]. `remix` is version-gated the other way
    // round: Remix 3's unbundled asset server serves npm packages to the browser
    // only from mounts relative to the project root, so a `remix` major ≥ 3 is
    // ejected while the bundler-built earlier majors keep GVS. See
    // [`remix_compat`]. The list stays curated and small because there is no
    // manifest signal for "this tool canonicalizes symlinks"; it is unavoidably
    // a behavioral-property list.
    // The incumbent's root when detected, else the cwd — a fresh project
    // (`detected.is_none()`) is rooted at the cwd. Workspace discovery expands
    // the member globs against the disk, so it runs ONCE here and the three
    // manifest scans below (the two version gates and the injected-deps check)
    // share the result.
    let gvs_root = detected.map(|d| d.dir.as_path()).unwrap_or(cwd);
    let workspace_members = workspace_members(gvs_root);
    let mut gvs_off: Vec<&str> = vec!["next", "react-native"];
    if expo_compat::expo_below_gvs_floor(gvs_root, &workspace_members) {
        gvs_off.push("expo");
    }
    if remix_compat::remix_needs_project_local_store(gvs_root, &workspace_members) {
        gvs_off.push("remix");
    }
    let store_dir = format!("node_modules/{PROJECT_VIRTUAL_STORE_LEAF}");
    let mut defaults = vec![
        (
            "defaultLockfileFormat".to_string(),
            fresh_format.to_string(),
        ),
        ("defaultTrust".to_string(), "true".to_string()),
        // Nub advertises a real 24-hour trust floor, not an advisory one. Pin
        // both halves at Nub's embedder tier rather than inheriting the engine's
        // current built-in values: 1440 minutes, and fail closed when no mature
        // version satisfies the range. Explicit user config still wins.
        ("minimumReleaseAge".to_string(), "1440".to_string()),
        ("minimumReleaseAgeStrict".to_string(), "true".to_string()),
        ("virtualStoreDir".to_string(), store_dir.clone()),
        ("stateDir".to_string(), store_dir),
        (
            "disableGlobalVirtualStoreForPackages".to_string(),
            // The whole-install GVS-off last resort — reserved for a store-LOCALITY
            // break (a tool whose resolver can't reach the machine-global store),
            // NOT a phantom-dep class (those the dynamic detector + closure now
            // eject per-package). `nuxt` was DROPPED: its walk-up is only 2 phantom
            // edges (~2.3% closure), so it's closure-ejectable at rung 1 —
            // symlink-GVS works (see [[nuxt-gvs-unlock]]). `parcel` was DROPPED: its
            // real break was never `.parcelrc` walk-up but a GVS store-dir over-split
            // — the prewarm hashed the widened (all-platform) graph while the link
            // phase hashed the host-filtered one, so parcel's native subtree
            // materialized `@parcel/core` at two byte-identical store dirs; two
            // consumers loaded different copies → two serializer registries →
            // `DataCloneError` at worker-farm startup. Fixed at the source (the
            // prewarm now host-filters its graph to match the link phase), verified
            // green across parcel 2.9–2.13 + 2.16. What remains:
            // - `next` — Turbopack canonicalizes through symlinks and chroots to a
            //   single project root; irreducible, so the store must be project-local.
            //   (It hit the same store-split, now also fixed, but the chroot break is
            //   independent and keeps it here pending its own verification.)
            // - `react-native` — bare RN's Metro (`@react-native/metro-config`)
            //   crawls by realpath and only sees the project root, so the global
            //   store is out of scope and even DECLARED deps (`@babel/runtime`)
            //   report unresolved.
            // - `expo` — the SAME break, but ONLY below SDK 56. Expo's Metro fork
            //   became store-aware in SDK 56 (the On-demand Filesystem); pre-56
            //   Expo uses the eager realpath crawl and breaks under GVS, so it is
            //   ejected version-conditionally ([`expo_compat`]) rather than by a
            //   flat name — 56+ keeps GVS.
            gvs_off.join(","),
        ),
        ("diskMaterializePackages".to_string(), disk_materialize),
    ];
    if let Some(data) = nub_data_dir() {
        defaults.push((
            "storeDir".to_string(),
            data.join("store").to_string_lossy().into_owned(),
        ));
    }
    // Scan for injected deps at the same root, so a fresh project is still
    // excluded from the GVS default below if it declares injected deps.
    let injected = unsupported_config::injected_deps_present(gvs_root, &workspace_members);
    // EVERY project defaults to isolated. Hoisting is left GVS-AWARE via the
    // engine's `gvs_over_default_hoist` profile (nub's identity sets it): a
    // NON-injected project pushes NO `hoist`, so it resolves to the built-in
    // default (true) which — under the profile — does NOT veto the shared store,
    // so GVS engages wherever active (no hidden tree) and the pnpm-parity hidden
    // hoist tree is built by the linker wherever GVS is off (CI/`nub ci`/a
    // next/nuxt/parcel trigger/explicit GVS opt-out/dlx) — restoring ambient
    // `@types/*` for store-resident packages (#286). Injected deps
    // (`dependenciesMeta.*.injected`) are the ONE carve-out: they need the
    // hidden tree unconditionally, so nub pushes an EXPLICIT `hoist=true`, which
    // DOES veto GVS under the profile (per-project + hidden tree, always) —
    // proven, where injected-under-GVS is not. Higher-precedence CLI/environment
    // values and native install config still override these defaults.
    defaults.push(("nodeLinker".to_string(), "isolated".to_string()));
    if injected {
        defaults.push(("hoist".to_string(), "true".to_string()));
    }
    // `nub ci` forces the virtual store PROJECT-LOCAL by pushing an explicit
    // `enableGlobalVirtualStore=false` (GVS off). With GVS off and the default
    // `hoist=true`, the isolated linker yields real `node_modules/.store/<dep>`
    // dirs + relative symlinks AND the pnpm-parity hidden hoist tree
    // (`node_modules/.store/node_modules/`) — a self-contained tree that survives a
    // multi-stage-Docker `COPY --from`, unlike the machine-global store the
    // default install engages (#241). Isolation (phantom-dep protection) is
    // unchanged; only the store LOCATION moves. Embedder-tier, so an explicit
    // user `enableGlobalVirtualStore`/`nodeLinker` still wins.
    if store_locality == VirtualStoreLocality::ProjectLocal {
        defaults.push(("enableGlobalVirtualStore".to_string(), "false".to_string()));
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
/// not nub's to claim. On the truly-fresh path nub writes `nub.lock` and
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

/// Whether `dir` holds nub's own canonical lockfile under EITHER the current
/// name (`nub.lock`) or the legacy name (`lock.yaml`) still honored through
/// the rename transition. The bespoke nub-side identity probes
/// (`resolve_config_surface`) check the file directly rather than through the
/// engine's candidate set, so they consult both names here.
fn nub_lockfile_present(dir: &Path) -> bool {
    dir.join(use_align::NUB_LOCKFILE).is_file()
        || dir.join(use_align::NUB_LEGACY_LOCKFILE).is_file()
}

/// Nub's XDG data root (`$XDG_DATA_HOME/nub`, `%LOCALAPPDATA%\nub` on Windows,
/// else `~/.local/share/nub`), the data-dir sibling of
/// `nub_core::node::discovery::cache_dir`.
///
/// The Windows `%LOCALAPPDATA%` branch mirrors the engine's own data-path
/// resolvers (`aube_store::dirs::store_dir`, `aube_runtime` `data_dir`): without
/// it the CAS `storeDir` default fell through to the Unix `.local/share` leaf on
/// Windows (`%USERPROFILE%\.local\share\nub\store`), split from the cache tier at
/// `%LOCALAPPDATA%\nub\pm` — #451. This feeds the `storeDir` embedder default and
/// the phantom scanner's store path; both stay consistent because both call here.
pub(crate) fn nub_data_dir() -> Option<PathBuf> {
    nub_data_dir_from(
        std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        dirs_next::home_dir(),
        cfg!(windows),
    )
}

/// Pure resolver for [`nub_data_dir`] — precedence: `$XDG_DATA_HOME/nub`; then,
/// on Windows, `%LOCALAPPDATA%\nub`; then `<home>/.local/share/nub`.
/// `local_app_data` is consulted only when `windows`, so the unix XDG
/// `.local/share` convention is preserved everywhere else. Split out
/// env-injected so the precedence is unit-testable off Windows (mirrors
/// `nub_core::node::discovery::windows_cache_dir`).
fn nub_data_dir_from(
    xdg_data_home: Option<PathBuf>,
    local_app_data: Option<PathBuf>,
    home: Option<PathBuf>,
    windows: bool,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_data_home {
        return Some(xdg.join("nub"));
    }
    if windows && let Some(local) = local_app_data {
        return Some(local.join("nub"));
    }
    home.map(|h| h.join(".local/share").join("nub"))
}

/// Multi-thread runtime mirroring aube's own `cli_main` shape
/// (`vendor/aube/crates/aube/src/lib.rs`): workers capped at 8 (the install
/// semaphore already gates network), blocking pool at 128 (tarball decode +
/// linker fan-out). The AUBE_TOKIO_* benchmark overrides are not honored here.
///
/// CONSTRAINT-AWARE sizing: on a resource-constrained box (a tight cgroup
/// `pids.max` or low `RLIMIT_NPROC`) the unbounded `128` blocking pool plus the
/// parallel native postinstalls can drive the total thread+process count past
/// the kernel ceiling; tokio then PANICS when `clone(2)` returns `EAGAIN` growing
/// its blocking pool, which under `panic = "abort"` aborts the whole install
/// (the `nub ci` exit-101). When [`resource_limits::spawn_headroom`] detects a
/// constraint we shrink BOTH pools to fit the headroom; on an unconstrained box
/// it returns `None` and we keep the full-speed defaults — so normal-box install
/// performance is untouched.
///
/// UNREFERENCED, and deliberately kept rather than deleted. The node-gyp
/// bootstrap was its last caller; it drives the pnpm engine now, which builds
/// its own tokio runtime. The tokio sizing here is therefore aube's alone, but
/// the two caps it composes are not: the pnpm engine fans work out over the same
/// rayon GLOBAL pool and honours `childConcurrency`, whose own EAGAIN diagnostic
/// names `RLIMIT_NPROC` — so a constrained box has lost this protection in the
/// engine migration, and re-wiring the caps into `pnpm_engine::session_prologue`
/// changes every PM command's concurrency, which is its own decision to make.
#[expect(
    dead_code,
    reason = "pending the concurrency-cap rewiring described above"
)]
fn build_runtime() -> Result<tokio::runtime::Runtime> {
    // Use `resource_limits::available_cores()` (NOT a local `unwrap_or(4)`) so this
    // `raw_cpu` matches the `cores` the `cpu_budget()` gate compares against — the
    // two must agree on the same `unwrap_or(1)` fallback or the gate could disagree
    // with the pool sizing in the rare `available_parallelism()`-errors case.
    let raw_cpu = resource_limits::available_cores();
    // The effective CPU budget auto-detected from a cgroup CFS quota — `None` on an
    // unconstrained box, where we keep the full core count. This is the PROACTIVE
    // CPU axis, composed below with the REACTIVE PID headroom: a CPU-bound pool is
    // bounded by the smaller of the two.
    let cpu_budget = resource_limits::cpu_budget();
    let cpu = cpu_budget.unwrap_or(raw_cpu);
    let mut workers = cpu.min(8);
    let mut blocking = 128usize;
    // The rayon GLOBAL pool is sized off the most restrictive of the two axes and
    // capped EXACTLY ONCE below: `build_global` errors if the pool already exists,
    // so the first cap wins — computing the final value first lets the PID share
    // tighten rayon BELOW the CPU budget (and vice versa) instead of whichever cap
    // happened to run first sticking. Start from the CPU budget (None → raw cores).
    let mut rayon_target = cpu;

    // Captured for the diagnostic seam below, which cannot otherwise distinguish "no
    // constraint detected" from "detected, but every derived pool saturated its own cap".
    let detected_headroom = resource_limits::spawn_headroom();

    if let Some(headroom) = detected_headroom {
        // ONE headroom budget split across the four concurrent OS-thread/process
        // consumers (tokio workers + tokio blocking pool + rayon GLOBAL pool +
        // parallel build-scripts) so their SUM stays under the ceiling — capping
        // each independently at `min(headroom, …)` would let the sum blow past it.
        let (w, b, rayon_threads, child) = resource_limits::split_budget(headroom);
        workers = workers.min(w);
        blocking = blocking.min(b);
        // Compose with the CPU budget: rayon is bounded by the SMALLER of the
        // CPU-quota-derived and PID-headroom-derived shares (both feed the same
        // pool). Capped once after this block.
        rayon_target = rayon_target.min(rayon_threads);
        // Lower the parallel build-script count to match (single-threaded here, so
        // the env mutation is race-free, and it runs BEFORE the env_snapshot the
        // engine resolves settings from).
        apply_constrained_child_concurrency(child);
        tracing::debug!(
            headroom,
            cpu_budget = cpu,
            workers,
            blocking,
            rayon = rayon_target,
            child_concurrency = child,
            "constrained box detected: capping install runtime pools (tokio + rayon) + build-script concurrency under the PID/thread + CPU-quota ceilings"
        );
    } else if let Some(budget) = cpu_budget {
        // CPU-quota constraint with NO PID constraint: the box has a CFS quota but
        // generous PIDs. Size the CPU-bound pools
        // (workers + rayon, already set above) to the quota; the IO-bound blocking
        // pool and child concurrency keep their full defaults (PID headroom is fine).
        tracing::debug!(
            cpu_budget = budget,
            workers,
            rayon = rayon_target,
            "CPU-quota constraint detected: capping CPU-bound install pools (tokio workers + rayon) to the effective CPU budget"
        );
    }

    // Internal diagnostic seam (NOT a public knob — `__NUB_*` per the brand
    // boundary's internal exemption): when set, print the resolved pool sizing to
    // real stderr, BEFORE the install's fd-capture can swallow a `tracing` line.
    // Lets a constrained-box test assert the detected budget deterministically.
    //
    // `headroom` is printed RAW because the derived pool sizes are lossy: every one of
    // them saturates its own cap, so `blocking=128` means "no constraint detected" OR
    // "detected, with headroom >= 384" (`min(128, max(h/3, 4))`), and `workers`/`rayon`
    // clamp against the core count on top of that. Reading detection state off the
    // derived numbers alone is ambiguous above that band and cost a verification round
    // — a successful read of a large limit was briefly mistaken for lost detection, and
    // only strace resolved it. The raw value makes the seam self-sufficient.
    if std::env::var_os("__NUB_PRINT_CPU_BUDGET").is_some() {
        eprintln!(
            "__nub_cpu_budget raw_cpu={raw_cpu} cpu_budget={cpu_budget:?} headroom={detected_headroom:?} workers={workers} blocking={blocking} rayon={rayon_target}"
        );
    }

    // Size the rayon GLOBAL pool whenever EITHER axis constrains below the raw core
    // count. The embedded engine fans CAS writes / delta / fetch out over rayon's
    // IMPLICIT global pool, whose lazy init otherwise spins up
    // `available_parallelism()` threads (v1-quota-blind, PID-unbounded) and PANICS
    // on thread-create EAGAIN — the SAME exit-101 abort, relocated to rayon. Done
    // once here, before any engine `par_iter` touches the pool.
    if rayon_target < raw_cpu {
        cap_rayon_global_pool(rayon_target);
    }

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(blocking)
        .enable_all()
        .build()
        .context("failed to build the install engine's tokio runtime")
}

/// Lower the parallel build-script process count (aube's `child_concurrency`,
/// default 5) so the native-postinstall fan-out — each spawning Go/Rust
/// grandchildren — stays under the PID ceiling. nub exports this through the
/// NEUTRAL `npm_config_child_concurrency` setting (which aube honors, same as
/// npm/pnpm), so standalone aube is UNCHANGED and no engine-brand var leaks; only
/// nub, on a detected constraint, asks for fewer parallel builds. A user/CI-set
/// value (any of the recognized keys) is left untouched.
///
/// SAFETY: called from `build_runtime` BEFORE the runtime is built and before any
/// engine work — single-threaded at that point, so the `set_var` doesn't race
/// other threads reading the environment.
fn apply_constrained_child_concurrency(capped: usize) {
    // Respect an explicit user/CI choice — never override a value they set. Only
    // the NEUTRAL keys are honored: nub respects ZERO AUBE_*-branded env vars
    // (AGENTS.md brand boundary), so `AUBE_CHILD_CONCURRENCY` is deliberately NOT
    // read here even to defer to it.
    const KEYS: [&str; 2] = [
        "npm_config_child_concurrency",
        "NPM_CONFIG_CHILD_CONCURRENCY",
    ];
    if KEYS.iter().any(|k| std::env::var_os(k).is_some()) {
        return;
    }
    // SAFETY: see the doc comment — single-threaded at call time.
    unsafe {
        std::env::set_var("npm_config_child_concurrency", capped.to_string());
    }
}

/// Size rayon's IMPLICIT GLOBAL thread pool under a detected PID/thread
/// constraint, so the engine's `par_iter` CAS/delta/fetch fan-out can't lazily
/// spin the pool up to `available_parallelism()` (which honors the cgroup CPU
/// quota, NOT the PID quota) and PANIC on a thread-create EAGAIN — the same
/// exit-101 abort the tokio cap closes, relocated to rayon.
///
/// `build_global` initializes the process-wide pool and ERRORS if it already
/// exists. We tolerate that: a prior install in the same process (or rayon
/// already lazily initialized) means the pool is set, and we can't resize it —
/// the cap is best-effort, applied the first time on a constrained box. Setting
/// `RAYON_NUM_THREADS` is NOT used (it only takes effect before first use and we
/// can't guarantee that across an embedder), so the explicit `build_global` is
/// the reliable lever when we own first-touch (pre-runtime, pre-engine here).
fn cap_rayon_global_pool(threads: usize) {
    // `build_global` errors if the global pool is already initialized — tolerate
    // it (can't resize an existing pool); the cap took effect on first touch.
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build_global();
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
        // `Builder::spawn` (returns `io::Result`) over `thread::spawn` (which
        // PANICS — and under `panic = "abort"` aborts the process — on
        // thread-create EAGAIN under thread/PID pressure). On spawn failure we
        // fall back to running `f` first and draining inline afterwards: this
        // capture path is `config get registry`, whose output is a few bytes
        // (well under the pipe buffer), so the post-`f` drain cannot deadlock.
        // Probe thread availability with a trivial throwaway thread BEFORE moving
        // the reader into a doomed spawn (`Builder::spawn` consumes its closure
        // even on `Err`, so we can't recover the reader after a failed spawn).
        let can_spawn = std::thread::Builder::new()
            .name("nub-fd-capture-probe".into())
            .spawn(|| {})
            .map(|p| {
                let _ = p.join();
            })
            .is_ok();
        if can_spawn {
            let mut reader = std::fs::File::from_raw_fd(read_end);
            let drain = std::thread::Builder::new()
                .name("nub-fd-capture".into())
                .spawn(move || {
                    let mut buf = Vec::new();
                    let _ = reader.read_to_end(&mut buf);
                    buf
                });
            match drain {
                Ok(drain) => {
                    let result = f();
                    flush(fd); // post-run: push f's buffered tail into the pipe
                    libc::dup2(saved, fd);
                    libc::close(saved);
                    // fd restored + our write end closed ⇒ the drain sees EOF.
                    let bytes = drain.join().unwrap_or_default();
                    (result, String::from_utf8_lossy(&bytes).into_owned())
                }
                Err(_) => {
                    // Probe passed but the real spawn raced to failure: `reader`
                    // is consumed, so close our read end and skip the capture.
                    let result = f();
                    flush(fd);
                    libc::dup2(saved, fd);
                    libc::close(saved);
                    (result, String::new())
                }
            }
        } else {
            // No drain thread available. Run `f`, restore the fd, then drain the
            // (few-byte `config get registry`) capture inline — too small to fill
            // the pipe buffer, so a post-`f` drain cannot deadlock.
            let result = f();
            flush(fd);
            libc::dup2(saved, fd);
            libc::close(saved);
            let mut reader = std::fs::File::from_raw_fd(read_end);
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            (result, String::from_utf8_lossy(&buf).into_owned())
        }
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
    use crate::project_config::{Hoist, InstallConfig, LinkerConfig};

    fn get<'a>(defaults: &'a [(String, String)], key: &str) -> Option<&'a str> {
        defaults
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    // #451: the Windows data root must be %LOCALAPPDATA%\nub, never the Unix
    // `.local/share` leaf — otherwise the CAS store splits from the cache tier.
    #[test]
    fn nub_data_dir_precedence() {
        let xdg = PathBuf::from("/xdg-data");
        let lad = PathBuf::from(r"C:\Users\u\AppData\Local");
        let home = PathBuf::from("/home/u");
        let call = |x, l, windows| nub_data_dir_from(x, l, Some(home.clone()), windows);

        // XDG_DATA_HOME wins on every platform.
        assert_eq!(
            call(Some(xdg.clone()), Some(lad.clone()), true),
            Some(xdg.join("nub"))
        );
        assert_eq!(call(Some(xdg.clone()), None, false), Some(xdg.join("nub")));

        // Windows, no XDG → %LOCALAPPDATA%\nub, never `.local/share`.
        assert_eq!(call(None, Some(lad.clone()), true), Some(lad.join("nub")));

        // Unix, no XDG → <home>/.local/share/nub; LOCALAPPDATA ignored.
        assert_eq!(
            call(None, Some(lad.clone()), false),
            Some(home.join(".local/share").join("nub"))
        );

        // Windows with neither XDG nor LOCALAPPDATA → the home fallback.
        assert_eq!(
            call(None, None, true),
            Some(home.join(".local/share").join("nub"))
        );
    }

    fn lower(install: InstallConfig, defaults: &[(String, String)]) -> NativeInstallSettings {
        lower_native_install_settings(&install, defaults).unwrap()
    }

    fn with_linker(linker: LinkerConfig) -> InstallConfig {
        InstallConfig {
            linker: Some(linker),
            ..InstallConfig::default()
        }
    }

    #[test]
    fn global_linker_yields_to_injected_deps_hidden_tree() {
        let injected = [("hoist".to_string(), "true".to_string())];
        let injected = lower_native_install_settings(
            &with_linker(LinkerConfig::Global { eject: None }),
            &injected,
        )
        .expect("injected deps must not make the documented global linker fail");
        assert_eq!(get(&injected.layout, "nodeLinker"), Some("isolated"));
        assert_eq!(
            get(&injected.layout, "enableGlobalVirtualStore"),
            None,
            "the injected-deps hidden tree must keep the store project-local"
        );

        let clean = lower(with_linker(LinkerConfig::Global { eject: None }), &[]);
        assert_eq!(
            get(&clean.layout, "enableGlobalVirtualStore"),
            Some("true"),
            "without injected deps the same layout pins the shared store"
        );
    }

    #[test]
    fn linker_strategy_lowers_to_a_layout_plus_a_store_location() {
        // Both symlink strategies are aube's `isolated` layout — they differ
        // only in WHERE the store lives, so each must PIN that half rather than
        // inherit it. Leaving the `Global` arm silent would let a lower-tier
        // `enableGlobalVirtualStore=false` win over an explicit request.
        for (linker, node_linker, gvs) in [
            (
                LinkerConfig::Global { eject: None },
                "isolated",
                Some("true"),
            ),
            (
                LinkerConfig::Isolated { hoist: None },
                "isolated",
                Some("false"),
            ),
            (LinkerConfig::Hoisted, "hoisted", None),
        ] {
            let lowered = lower(with_linker(linker.clone()), &[]);
            assert_eq!(
                get(&lowered.layout, "nodeLinker"),
                Some(node_linker),
                "{linker:?}"
            );
            assert_eq!(
                get(&lowered.layout, "enableGlobalVirtualStore"),
                gvs,
                "{linker:?}"
            );
        }
    }

    #[test]
    fn isolated_hoist_lowers_to_the_pnpm_hoist_pattern_pair() {
        for (hoist, enabled, pattern) in [
            (Hoist::Bool(true), "true", Some("*")),
            (Hoist::Bool(false), "false", None),
            (
                Hoist::Patterns(vec!["@types/*".into(), "!@types/node".into()]),
                "true",
                Some("@types/*,!@types/node"),
            ),
        ] {
            let lowered = lower(
                with_linker(LinkerConfig::Isolated {
                    hoist: Some(hoist.clone()),
                }),
                &[],
            );
            assert_eq!(get(&lowered.layout, "hoist"), Some(enabled), "{hoist:?}");
            assert_eq!(get(&lowered.layout, "hoistPattern"), pattern, "{hoist:?}");
        }
    }

    #[test]
    fn global_eject_extends_both_embedder_default_lists() {
        let defaults = vec![
            (
                "disableGlobalVirtualStoreForPackages".into(),
                "next,react-native".into(),
            ),
            ("diskMaterializePackages".into(), "vite".into()),
        ];
        let lowered = lower(
            with_linker(LinkerConfig::Global {
                eject: Some(vec!["@corp/tool-*".into(), "next".into()]),
            }),
            &defaults,
        );

        // Additive and deduped on top of the compat-required embedder ejects —
        // a project can add to them but cannot un-eject one.
        assert_eq!(
            get(&lowered.layout, "disableGlobalVirtualStoreForPackages"),
            Some("next,react-native,@corp/tool-*")
        );
        assert_eq!(
            get(&lowered.layout, "diskMaterializePackages"),
            Some("vite,@corp/tool-*,next")
        );
        // Also carried out of band: the phantom closure is seeded from it.
        assert_eq!(
            lowered.eject,
            vec!["@corp/tool-*".to_string(), "next".to_string()]
        );
    }

    #[test]
    fn public_hoist_lowers_independently_of_the_linker_strategy() {
        // Writes BOTH aube keys. Writing only `publicHoistPattern` would leave
        // `shamefullyHoist` at a lower tier's value, which inverts the request.
        // `["*"]` is the everything-spelling — pnpm's own representation of
        // `shamefully-hoist` — so it still travels as a pattern, not the flag.
        for (public_hoist, shamefully, patterns) in [
            (vec!["*".to_string()], "false", "*"),
            (Vec::new(), "false", ""),
            (
                vec!["@types/*".to_string(), "eslint-*".to_string()],
                "false",
                "@types/*,eslint-*",
            ),
        ] {
            let lowered = lower(
                InstallConfig {
                    public_hoist: Some(public_hoist.clone()),
                    ..InstallConfig::default()
                },
                &[],
            );
            assert_eq!(
                get(&lowered.layout, "shamefullyHoist"),
                Some(shamefully),
                "{public_hoist:?}"
            );
            assert_eq!(
                get(&lowered.layout, "publicHoistPattern"),
                Some(patterns),
                "{public_hoist:?}"
            );
            // It sits outside `linker` because it means the same thing under
            // every strategy — including none written at all.
            assert_eq!(get(&lowered.layout, "nodeLinker"), None, "{public_hoist:?}");
        }
    }

    #[test]
    fn minimum_release_age_rounds_up_to_whole_minutes() {
        // Positive sub-minute durations must remain a full minute: rounding down
        // would silently weaken the configured release-age gate.
        for seconds in [1, 30, 59, 60] {
            let lowered = lower(
                InstallConfig {
                    minimum_release_age: Some(std::time::Duration::from_secs(seconds)),
                    ..InstallConfig::default()
                },
                &[],
            );
            assert_eq!(
                get(&lowered.resolution, "minimumReleaseAge"),
                Some("1"),
                "{seconds}s must conservatively round up to one minute"
            );
        }

        let lowered = lower(
            InstallConfig {
                minimum_release_age: Some(std::time::Duration::from_secs(61)),
                minimum_release_age_exclude: Some(vec!["@internal/*".into(), "left-pad".into()]),
                ..InstallConfig::default()
            },
            &[],
        );
        assert_eq!(get(&lowered.resolution, "minimumReleaseAge"), Some("2"));
        assert_eq!(
            get(&lowered.resolution, "minimumReleaseAgeStrict"),
            Some("true")
        );
        assert_eq!(
            get(&lowered.resolution, "minimumReleaseAgeExclude"),
            Some("@internal/*,left-pad")
        );
    }

    #[test]
    fn explicit_false_and_empty_install_values_survive_lowering() {
        // An explicitly-written `false` / `[]` is a VALUE, not an absence: it
        // must reach the engine so it overrides what the default would be.
        let lowered = lower(
            InstallConfig {
                linker: Some(LinkerConfig::Isolated {
                    hoist: Some(Hoist::Bool(false)),
                }),
                minimum_release_age_exclude: Some(Vec::new()),
                ..InstallConfig::default()
            },
            &[],
        );
        assert_eq!(get(&lowered.layout, "hoist"), Some("false"));
        assert_eq!(
            get(&lowered.resolution, "minimumReleaseAgeExclude"),
            Some("")
        );

        // `eject: []` still WRITES both list keys — at the embedder's own value,
        // since the merge is additive — where no `eject` key writes neither.
        let defaults = vec![("diskMaterializePackages".to_string(), "vite".to_string())];
        let empty = lower(
            with_linker(LinkerConfig::Global {
                eject: Some(Vec::new()),
            }),
            &defaults,
        );
        let absent = lower(with_linker(LinkerConfig::Global { eject: None }), &defaults);
        assert_eq!(get(&empty.layout, "diskMaterializePackages"), Some("vite"));
        assert_eq!(get(&absent.layout, "diskMaterializePackages"), None);
    }

    #[test]
    fn pnp_linker_is_reserved_and_aborts_the_install() {
        // `pnp` parses but has no write path, so the reservation is enforced
        // here, at lowering. The CODE is asserted, not just the failure: it is
        // what tells an author their `linker` was understood and refused rather
        // than mis-parsed into something else.
        let err = lower_native_install_settings(&with_linker(LinkerConfig::Pnp), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("ERR_NUB_CONFIG_UNSUPPORTED"), "{err}");
    }

    /// Layout applies under every identity, so `nub.jsonc` is the canonical
    /// place to configure it regardless of incumbent — which means a reserved
    /// layout value is refused under an incumbent too. A field Nub honors is a
    /// field Nub validates; the old behavior lowered nothing in
    /// compat mode and so accepted `pnp` there in silence.
    #[test]
    fn a_reserved_linker_value_is_refused_under_an_incumbent_too() {
        let install = with_linker(LinkerConfig::Pnp);
        let err = lower_native_install_settings_for_mode(Some(&install), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("ERR_NUB_CONFIG_UNSUPPORTED"), "{err}");
    }

    #[test]
    fn install_config_mode_gate_accepts_only_nub_identity_or_truly_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let detected = |kind| DetectedLockfile {
            kind,
            dir: dir.path().to_path_buf(),
        };
        assert!(native_pm_mode(None, true, dir.path()));
        assert!(native_pm_mode(
            Some(&detected(LockfileKind::Aube)),
            false,
            dir.path()
        ));
        for kind in [
            LockfileKind::Npm,
            LockfileKind::Pnpm,
            LockfileKind::Yarn,
            LockfileKind::YarnBerry,
            LockfileKind::Bun,
        ] {
            assert!(!native_pm_mode(Some(&detected(kind)), false, dir.path()));
        }
    }

    #[test]
    fn project_snapshot_reaches_install_lowering() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("nub.jsonc"),
            r#"{
              "install": {
                "linker": { "strategy": "isolated", "hoist": false },
                "publicHoist": ["@types/*"],
                "minimumReleaseAge": "3d",
                "minimumReleaseAgeExclude": ["@internal/*"]
              }
            }"#,
        )
        .unwrap();
        // `load_effective_config` also reads the GLOBAL layer, which resolves
        // through `XDG_CONFIG_HOME` — a process-global. `with_config_home` points
        // it at an empty dir, so the developer's real `~/.config/nub/nub.jsonc`
        // cannot merge into the snapshot asserted below, and holds the
        // process-wide env lock every `unsafe set_var` in the suite serializes on
        // (an unsynchronized `getenv` against one is a data race under 2024).
        let snapshot = crate::config::with_config_home(|_| {
            crate::project_config::load_effective_config(
                dir.path(),
                crate::project_config::ConfigOverlays::default(),
            )
        })
        .unwrap();
        assert_eq!(
            snapshot
                .project
                .as_ref()
                .map(|project| project.source.root.as_path()),
            Some(dir.path())
        );

        let lowered = lower_native_install_settings(&snapshot.values.install, &[]).unwrap();
        assert_eq!(get(&lowered.layout, "nodeLinker"), Some("isolated"));
        assert_eq!(
            get(&lowered.layout, "enableGlobalVirtualStore"),
            Some("false")
        );
        assert_eq!(get(&lowered.layout, "hoist"), Some("false"));
        assert_eq!(get(&lowered.layout, "publicHoistPattern"), Some("@types/*"));
        assert_eq!(get(&lowered.resolution, "minimumReleaseAge"), Some("4320"));
        assert_eq!(
            get(&lowered.resolution, "minimumReleaseAgeExclude"),
            Some("@internal/*")
        );
    }

    #[test]
    fn pnpm_npmrc_key_policy_narrows_only_at_v11() {
        // pnpm reversed its `.npmrc` settings-reading at v11: ≤10 reads the
        // open key space, 11+ restricts to the auth/registry allowlist. An
        // unknown major defaults to Open (the dominant/most-compatible model).
        assert!(matches!(
            pnpm_npmrc_key_policy(Some(9)),
            NpmrcKeyPolicy::Open
        ));
        assert!(matches!(
            pnpm_npmrc_key_policy(Some(10)),
            NpmrcKeyPolicy::Open
        ));
        assert!(matches!(
            pnpm_npmrc_key_policy(Some(11)),
            NpmrcKeyPolicy::Pnpm11Allowlist
        ));
        assert!(matches!(
            pnpm_npmrc_key_policy(Some(12)),
            NpmrcKeyPolicy::Pnpm11Allowlist
        ));
        assert!(matches!(pnpm_npmrc_key_policy(None), NpmrcKeyPolicy::Open));
    }

    #[test]
    fn setting_defaults_pick_the_layout_from_the_lockfile_kind() {
        let dir = tempfile::tempdir().unwrap();
        let detected = |kind| DetectedLockfile {
            kind,
            dir: dir.path().to_path_buf(),
        };

        // Every non-injected incumbent kind AND a fresh project default to
        // isolated with NO `hoist` push: hoist resolves to the built-in default
        // (true), which under nub's `gvs_over_default_hoist` profile lets GVS
        // engage while the linker builds the hidden tree only where GVS is off.
        // The tempdir has no injected manifest.
        for kind in [
            LockfileKind::Npm,
            LockfileKind::NpmShrinkwrap,
            LockfileKind::Yarn,
            LockfileKind::YarnBerry,
            LockfileKind::Bun,
            LockfileKind::Pnpm,
            LockfileKind::Aube,
        ] {
            let defaults = nub_setting_defaults(
                Some(&detected(kind)),
                false,
                dir.path(),
                VirtualStoreLocality::Default,
            );
            assert_eq!(
                get(&defaults, "nodeLinker"),
                Some("isolated"),
                "{kind:?} must default to the isolated layout"
            );
            assert_eq!(
                get(&defaults, "hoist"),
                None,
                "{kind:?} must NOT push hoist — a default hoist keeps GVS engaged"
            );
        }

        // A fresh project (no lockfile) gets the same GVS default.
        let fresh = nub_setting_defaults(None, true, dir.path(), VirtualStoreLocality::Default);
        assert_eq!(
            get(&fresh, "nodeLinker"),
            Some("isolated"),
            "fresh ⇒ isolated"
        );
        assert_eq!(
            get(&fresh, "hoist"),
            None,
            "fresh ⇒ no hoist push (GVS engages by default)"
        );
    }

    #[test]
    fn phantom_disk_materialize_list_is_retired_in_favor_of_the_detector() {
        // The 14-entry hand-curated adapter list is gone: the dynamic per-version
        // scanner + #319 ancestor-closure (on by default) detect and eject those
        // phantoms, so nub no longer seeds them as an embedder default. A fresh,
        // non-vite project's `diskMaterializePackages` default is therefore
        // empty (the vite #315 seed is the only remaining embedder-default entry,
        // and this fixture declares no vite).
        let dir = tempfile::tempdir().unwrap();
        let fresh = nub_setting_defaults(None, true, dir.path(), VirtualStoreLocality::Default);
        let list = get(&fresh, "diskMaterializePackages").unwrap_or_default();
        let names: Vec<&str> = list.split(',').filter(|s| !s.is_empty()).collect();
        for retired in [
            "@hookform/resolvers",
            "@prisma/client",
            "next-themes",
            "@react-pdf/renderer",
            "preact",
            "drizzle-orm",
        ] {
            assert!(
                !names.contains(&retired),
                "{retired} must no longer be a hardcoded default (detector subsumes it): {list:?}"
            );
        }
    }

    #[test]
    fn disable_gvs_default_drops_nuxt_and_parcel_and_keeps_the_store_locality_breakers() {
        // The whole-install GVS-off list is now reserved for store-LOCALITY
        // breaks. `nuxt` is closure-ejectable at rung 1 ([[nuxt-gvs-unlock]]) so
        // it was dropped; `parcel` was dropped once its real break — a GVS
        // store-dir over-split from the prewarm/link graph mismatch — was fixed at
        // the source (see `run_gvs_prewarm_materializer`). `next` (Turbopack
        // chroot) and `react-native` (bare-RN Metro realpath crawl) remain
        // irreducible store-locality breaks.
        let dir = tempfile::tempdir().unwrap();
        let fresh = nub_setting_defaults(None, true, dir.path(), VirtualStoreLocality::Default);
        assert_eq!(
            get(&fresh, "disableGlobalVirtualStoreForPackages"),
            Some("next,react-native"),
        );
    }

    #[test]
    fn expo_below_sdk_56_is_gvs_ejected_but_56_plus_keeps_gvs() {
        // Expo's Metro fork became store-aware in SDK 56 (On-demand Filesystem),
        // so `expo` ejects version-conditionally: below the floor joins the
        // store-locality breakers, at/above it keeps GVS. A range whose major
        // can't be floored pre-resolution ejects (safe direction).
        let disable_list = |manifest: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("package.json"), manifest).unwrap();
            let defaults =
                nub_setting_defaults(None, false, dir.path(), VirtualStoreLocality::Default);
            get(&defaults, "disableGlobalVirtualStoreForPackages")
                .unwrap()
                .to_string()
        };

        // Below the floor → ejected (dependencies and devDependencies both count).
        assert_eq!(
            disable_list(r#"{"name":"x","dependencies":{"expo":"~52.0.0"}}"#),
            "next,react-native,expo",
        );
        assert_eq!(
            disable_list(r#"{"name":"x","devDependencies":{"expo":"^51.0.0"}}"#),
            "next,react-native,expo",
        );
        assert_eq!(
            disable_list(r#"{"name":"x","optionalDependencies":{"expo":"50.0.0"}}"#),
            "next,react-native,expo",
        );
        // Unfloorable range → eject-on-ambiguity.
        assert_eq!(
            disable_list(r#"{"name":"x","dependencies":{"expo":"*"}}"#),
            "next,react-native,expo",
        );
        // At/above the floor → GVS stays on (expo absent from the list).
        assert_eq!(
            disable_list(r#"{"name":"x","dependencies":{"expo":"~56.0.0"}}"#),
            "next,react-native",
        );
        assert_eq!(
            disable_list(r#"{"name":"x","dependencies":{"expo":"^57.0.4"}}"#),
            "next,react-native",
        );
        // Not an Expo project → unchanged.
        assert_eq!(
            disable_list(r#"{"name":"x","dependencies":{"react":"19.2.0"}}"#),
            "next,react-native",
        );
    }

    #[test]
    fn injected_deps_keep_the_hidden_hoist_tree_and_opt_out_of_gvs() {
        // The ONE carve-out: a project declaring `dependenciesMeta.*.injected`
        // needs the hidden hoist tree unconditionally, so nub pushes an EXPLICIT
        // `hoist=true` — which vetoes GVS under the `gvs_over_default_hoist`
        // profile (per-project + hidden tree, always). Covered for both a
        // detected incumbent and a fresh project rooted at cwd.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"x","dependenciesMeta":{"foo":{"injected":true}}}"#,
        )
        .unwrap();
        let detected = DetectedLockfile {
            kind: LockfileKind::Npm,
            dir: dir.path().to_path_buf(),
        };
        for defaults in [
            nub_setting_defaults(
                Some(&detected),
                false,
                dir.path(),
                VirtualStoreLocality::Default,
            ),
            nub_setting_defaults(None, true, dir.path(), VirtualStoreLocality::Default), // fresh, injected at cwd
        ] {
            assert_eq!(
                get(&defaults, "nodeLinker"),
                Some("isolated"),
                "injected ⇒ isolated (like every project)"
            );
            assert_eq!(
                get(&defaults, "hoist"),
                Some("true"),
                "injected ⇒ explicit hoist=true (vetoes GVS, forces the hidden tree)"
            );
        }
    }

    #[test]
    fn ci_forces_project_local_store_but_keeps_isolation() {
        // `nub ci` (VirtualStoreLocality::ProjectLocal) pushes
        // `enableGlobalVirtualStore=false` so the frozen node_modules is
        // COPY-relocatable (#241), while keeping the isolated layout (phantom-dep
        // protection). It pushes NO `hoist`: with GVS off the default hoist=true
        // makes the linker build the pnpm-parity hidden tree. A plain install
        // (Default) leaves GVS on.
        let dir = tempfile::tempdir().unwrap();
        let detected = DetectedLockfile {
            kind: LockfileKind::Npm,
            dir: dir.path().to_path_buf(),
        };

        let ci = nub_setting_defaults(
            Some(&detected),
            false,
            dir.path(),
            VirtualStoreLocality::ProjectLocal,
        );
        assert_eq!(
            get(&ci, "enableGlobalVirtualStore"),
            Some("false"),
            "ci must force the store project-local (GVS off)"
        );
        assert_eq!(
            get(&ci, "nodeLinker"),
            Some("isolated"),
            "ci keeps the isolated layout"
        );
        assert_eq!(
            get(&ci, "hoist"),
            None,
            "ci pushes no hoist — the default builds the hidden tree with GVS off"
        );

        let plain = nub_setting_defaults(
            Some(&detected),
            false,
            dir.path(),
            VirtualStoreLocality::Default,
        );
        assert_eq!(
            get(&plain, "enableGlobalVirtualStore"),
            None,
            "a plain install never forces GVS off (stays on outside CI)"
        );
    }

    #[test]
    fn fresh_write_format_flips_with_truly_fresh() {
        let dir = tempfile::tempdir().unwrap();
        // A truly-fresh project writes nub's neutral `nub.lock`
        // (`defaultLockfileFormat=nub`); every other surface keeps the
        // pnpm-lock fresh-write default for drop-in interop. The value is
        // spelled `nub`, not the engine's `aube`, because `nub config` lists
        // it — both spellings select the same format.
        assert_eq!(
            get(
                &nub_setting_defaults(None, true, dir.path(), VirtualStoreLocality::Default),
                "defaultLockfileFormat"
            ),
            Some("nub"),
            "truly-fresh must write nub's nub.lock"
        );
        assert_eq!(
            get(
                &nub_setting_defaults(None, false, dir.path(), VirtualStoreLocality::Default),
                "defaultLockfileFormat"
            ),
            Some("pnpm"),
            "a project with a pnpm signal keeps the pnpm-lock fresh-write default"
        );
        let pnpm = DetectedLockfile {
            kind: LockfileKind::Pnpm,
            dir: dir.path().to_path_buf(),
        };
        assert_eq!(
            get(
                &nub_setting_defaults(
                    Some(&pnpm),
                    false,
                    dir.path(),
                    VirtualStoreLocality::Default
                ),
                "defaultLockfileFormat"
            ),
            Some("pnpm"),
            "an incumbent lockfile is never the truly-fresh path"
        );
    }

    #[test]
    fn setting_defaults_always_carry_the_nub_identity_settings() {
        // Every engine command gets the `.store` store/state location and the
        // nub-namespaced global dirs regardless of detection; the non-truly-
        // fresh surfaces also get the pnpm lockfile fresh-write default (the
        // truly-fresh `nub`/`nub.lock` flip is covered separately). (These
        // ride the engine's
        // embedder-defaults tier, so any user source overrides them —
        // precedence is covered by the engine's own tests and the
        // install_engine integration tests.)
        for detected in [None, Some(LockfileKind::Npm), Some(LockfileKind::Pnpm)] {
            let dir = tempfile::tempdir().unwrap();
            let detected = detected.map(|kind| DetectedLockfile {
                kind,
                dir: dir.path().to_path_buf(),
            });
            // `truly_fresh = false` here: this exercises the
            // identity-settings invariants for the non-truly-fresh surfaces
            // (the truly-fresh lockfile-format flip is covered separately in
            // `fresh_write_format_flips_with_truly_fresh`).
            let defaults = nub_setting_defaults(
                detected.as_ref(),
                false,
                dir.path(),
                VirtualStoreLocality::Default,
            );
            assert_eq!(get(&defaults, "defaultLockfileFormat"), Some("pnpm"));
            assert_eq!(
                get(&defaults, "minimumReleaseAge"),
                Some("1440"),
                "Nub's release-age floor must default to 24 hours"
            );
            assert_eq!(
                get(&defaults, "minimumReleaseAgeStrict"),
                Some("true"),
                "Nub's 24-hour release-age floor must fail closed by default"
            );
            assert_eq!(
                get(&defaults, "virtualStoreDir"),
                Some("node_modules/.store")
            );
            assert_eq!(get(&defaults, "stateDir"), Some("node_modules/.store"));
            // The global store lands in a nub-owned namespace (dev boxes
            // always resolve a home dir, so the entry is present here). Which
            // namespace — cache, not data — is pinned end-to-end by
            // `store_path_prints_the_nub_namespaced_store`, where the whole
            // resolved path is compared against a fixture-pinned XDG root;
            // this asserts only the leaf nub owns on every platform.
            let store = get(&defaults, "storeDir").expect("storeDir default");
            // Normalize separators: on Windows the default resolves with
            // `\` components (and a mixed `/` from the XDG-style fallback).
            let store = store.replace('\\', "/");
            assert!(
                store.ends_with("nub/store") && !store.contains("aube"),
                "storeDir must live under a nub-owned namespace: {store}"
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
            let defaults =
                nub_setting_defaults(d.as_ref(), false, dir.path(), VirtualStoreLocality::Default);
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
        // A lone nub.lock (nub's canonical name) → nub identity.
        let d = root(&[
            ("package.json", "{}"),
            ("nub.lock", "lockfileVersion: '9.0'\n"),
        ]);
        assert_eq!(
            resolve_config_surface(d.path()),
            ConfigSurface::NubIdentity(d.path().to_path_buf())
        );
        // A lone legacy lock.yaml (read-both through the transition) → nub
        // identity too, carrying the deciding dir.
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

    /// The gate on pnpm-11-only branded config: the global
    /// `config.yaml`/`auth.ini` pair and scalar settings in
    /// `pnpm-workspace.yaml`. Living in the user config home does not make the
    /// global files PM-agnostic or version-agnostic, and only a declared major
    /// proves v11.
    #[test]
    fn pnpm_v11_surfaces_need_a_declared_v11_incumbent() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().to_path_buf();
        let manifest = |json: &str| std::fs::write(root.path().join("package.json"), json).unwrap();

        manifest(r#"{"packageManager":"pnpm@10.0.0"}"#);
        assert!(!pnpm_v11_surface(&ConfigSurface::PnpmOrFresh, root.path()));

        // The extra space is load-bearing, not style. `ROOT_MANIFEST_CACHE` keys
        // freshness on `{mtime, size}`, and its own docs say a same-mtime,
        // same-size content edit is deliberately NOT distinguished — the size half
        // is what covers "tests that rewrite-then-reread the same path". Written
        // without it, this rewrite is byte-identical in LENGTH to the v10 line
        // above, so on a filesystem whose mtime granularity is coarser than the gap
        // between two writes (Windows) both halves of the stamp collide, the stale
        // v10 value is served, and this assertion fails. Every other rewrite in
        // this test already differs in length by accident.
        manifest(r#"{"packageManager": "pnpm@11.0.0"}"#);
        assert!(pnpm_v11_surface(&ConfigSurface::PnpmOrFresh, root.path()));
        assert!(
            !pnpm_v11_surface(&ConfigSurface::NubIdentity(dir.clone()), root.path()),
            "Nub identity must not read pnpm's global files or workspace settings"
        );
        for role in ["npm", "yarn", "bun"] {
            assert!(
                !pnpm_v11_surface(
                    &ConfigSurface::NonPnpmCompat {
                        role,
                        dir: dir.clone(),
                    },
                    root.path(),
                ),
                "{role} identity must not read pnpm's global files or workspace settings"
            );
        }

        manifest(r#"{"packageManager":"vlt@1.0.0"}"#);
        assert!(
            !pnpm_v11_surface(&ConfigSurface::PnpmOrFresh, root.path()),
            "the conservative CLI surface for an unknown tool is not pnpm incumbency"
        );

        manifest("{}");
        std::fs::write(
            root.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();
        assert!(
            !pnpm_v11_surface(&ConfigSurface::PnpmOrFresh, root.path()),
            "a pnpm lockfile proves the incumbent name, not the major; unknown defaults to v10"
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
        // nub augmented), NODE_OPTIONS (preload + source maps; feature flags ride argv), NODE_PATH
        // (vendored helper resolution), npm_node_execpath PINNED to the
        // provisioned Node (the ABI fix — node-gyp must compile against the
        // project's Node, not ambient), and the shim dir leading PATH.
        let aug = AugmentationEnv {
            node_options: Some("--require=/rt/preload.cjs".to_string()),
            shim_dir: Some("/shim".to_string()),
            node_path: Some(OsString::from("/rt/node_path")),
            neutralize_localstorage: true,
            threadpool_size: Some("8".to_string()),
        };
        let runtime_json = r#"{"nodeCompat":false}"#;
        let (overlay, prepends) =
            augmentation_to_lifecycle_overlay(&aug, "/pinned/bin/node", Some(runtime_json));

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
            find(crate::project_config::RUNTIME_CONFIG_ENV).as_deref(),
            Some(runtime_json),
            "the Node-shim continuation must inherit the source-anchored runtime snapshot"
        );
        assert_eq!(
            find("__NUB_AUGMENTED_NODE").as_deref(),
            Some(expected_shim_node.as_str()),
            "a fresh boundary may restore NODE only while it still holds the lifecycle shim"
        );
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
        assert_eq!(
            find("UV_THREADPOOL_SIZE").as_deref(),
            Some("8"),
            "the threadpool size must reach lifecycle node children"
        );
        assert_eq!(
            find("__NUB_AUGMENTED_UV_THREADPOOL_SIZE").as_deref(),
            Some("8"),
            "a compat boundary may remove the pool size only while it still holds nub's value"
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
            threadpool_size: None,
        };
        let (overlay, prepends) = augmentation_to_lifecycle_overlay(&aug, "/pinned/bin/node", None);
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
    fn x_is_an_alias_of_dlx() {
        // `nub x <tool>` is the short fetch-and-run spelling — it resolves to the
        // same `dlx` engine verb as `nub dlx`, so both share one dispatch path
        // (`run_dlx` → `aube::commands::dlx`). It is NOT exec: `x` fetches a
        // missing tool, exec does not.
        let spec = lookup_verb("x").expect("x must be a registered verb");
        assert_eq!(spec.canonical, "dlx");
        assert_eq!(lookup_verb("dlx").map(|s| s.canonical), Some("dlx"));
    }

    #[test]
    fn verb_registry_excludes_reserved_and_tool_identity_verbs() {
        // nub-reserved spellings (collision policy) and aube tool-identity
        // verbs must never enter the registry — `upgrade` in particular is
        // nub's self-update, not aube's update alias. (`x` is deliberately
        // ABSENT: it is a registered alias of `dlx` — asserted by
        // `x_is_an_alias_of_dlx` — not a reserved exclusion.)
        for verb in [
            "run",
            "run-script",
            "exec",
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

    /// The member walk runs on the engine, which reports the workspace root as
    /// a project of its own. Every caller takes the root separately, so
    /// `discover_workspace_members` drops it — and a member that a pattern does
    /// match is still returned.
    #[test]
    fn the_member_walk_returns_members_without_the_workspace_root() {
        let d = workspace(&[
            ("package.json", r#"{"name":"root","workspaces":["pkgs/*"]}"#),
            ("pkgs/a/package.json", r#"{"name":"pkg-a"}"#),
            ("pkgs/b/package.json", r#"{"name":"pkg-b"}"#),
        ]);
        let members = discover_workspace_members(d.path());
        let names: Vec<_> = members
            .iter()
            .map(|m| m.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["a", "b"], "members: {members:?}");
    }

    /// The pattern SOURCE is identity-gated, which is the whole reason nub
    /// passes patterns to the engine instead of letting it discover them. A
    /// project declaring nub is nub's however many pnpm-named files sit beside
    /// it, so its members come from the neutral `workspaces` field alone — the
    /// vendored engine read `pnpm-workspace.yaml` here whatever the identity.
    #[test]
    fn a_nub_project_takes_no_members_from_pnpm_workspace_yaml() {
        // One tree, two declarations: the members live only in the pnpm-named
        // file, so whether they are found is entirely a question of identity.
        let tree = |declared: &str| {
            workspace(&[
                (
                    "package.json",
                    &format!(r#"{{"name":"root","packageManager":"{declared}"}}"#),
                ),
                ("pkgs/a/package.json", r#"{"name":"pkg-a"}"#),
                ("pnpm-workspace.yaml", "packages:\n  - pkgs/*\n"),
            ])
        };

        let nub = tree("nub@0.9.0");
        let found = discover_workspace_members(nub.path());
        assert!(
            found.is_empty(),
            "a nub project must not read pnpm-workspace.yaml: {found:?}"
        );

        // The control: the identical tree under pnpm, where reading that file
        // is exactly what parity requires. Without it, the assertion above
        // would pass just as well if the walk were simply broken.
        let pnpm = tree("pnpm@12.4.1");
        let found = discover_workspace_members(pnpm.path());
        assert_eq!(found.len(), 1, "pnpm must read its own file: {found:?}");
        assert_eq!(found[0].file_name().unwrap(), "a");
    }

    #[test]
    fn a_member_install_resolves_the_workspace_roots_node_pin() {
        // aube anchors install state and the virtual store at the workspace root,
        // and a member install materializes that ONE shared tree. So the engine
        // published for it has to name the root's Node: keyed off the member's own
        // pin, the ABI caches describe a Node the root-anchored state file never
        // saw, and alternating root/member installs each rebuild the other's tree.
        let d = workspace(&[
            (
                "package.json",
                r#"{"name":"ws","workspaces":["packages/*"]}"#,
            ),
            (".nvmrc", "22.15.0\n"),
            ("packages/member/package.json", r#"{"name":"member"}"#),
            ("packages/member/.nvmrc", "20.19.0\n"),
        ]);
        let member = d.path().join("packages/member");
        let pin_at = |dir: &Path| {
            nub_core::node::discovery::resolve_pin_chain(dir)
                .expect("pin chain resolves")
                .pin
                .expect("a pin file is in scope")
                .0
        };

        // Control: from the raw cwd the member's own pin wins — the key the engine
        // carried before discovery was anchored.
        assert_eq!(pin_at(&member), "20.19.0");
        assert_eq!(
            pin_at(&lifecycle_node_anchor(&member)),
            "22.15.0",
            "a member install builds the workspace's one shared tree, so anchoring \
             Node discovery anywhere but the root keys the ABI caches to a Node the \
             install state never saw"
        );
    }
}
