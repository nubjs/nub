//! Package-manager verbs through the embedded pnpm engine (linked as a
//! library; no subprocess).
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
//! Engine-adjacent text nub relays passes through [`present`]'s credential
//! scrub; the engine brands its own output through the embedder profile.
//!
//! # Verb registry
//!
//! [`ENGINE_VERBS`] registers the package-manager verb surface minus two
//! exclusion sets:
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

mod compat_db;
pub(crate) mod config_read;
mod duplicate_home;
mod expo_compat;
mod host_settings;
pub mod install_family;
mod install_report;
pub mod log;
pub mod min_release_age;
pub(crate) mod node_gyp;
pub mod output;
pub mod phantom_closure;
pub mod platform_flags;
mod pnpm_engine;
pub(crate) mod project_identity;
pub(crate) mod recursive_projects;
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

/// One registered engine verb: its canonical spelling, accepted aliases and
/// owning family.
pub struct VerbSpec {
    pub canonical: &'static str,
    pub aliases: &'static [&'static str],
    pub family: Family,
}

/// The package-manager verb surface, per the module doc's
/// exclusion rules. Spellings must be unique across canonicals + aliases and
/// disjoint from cli.rs's SUBCOMMANDS and PM_VERBS (asserted in tests here
/// and in cli.rs).
pub const ENGINE_VERBS: &[VerbSpec] = &[
    // ── install family: dependency-graph mutation + linking ────────────
    VerbSpec {
        canonical: "add",
        aliases: &["a"],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "remove",
        aliases: &["rm", "uninstall", "un", "uni"],
        family: Family::Install,
    },
    // aube also aliases `upgrade` here; that spelling is nub's self-update.
    VerbSpec {
        canonical: "update",
        aliases: &["up"],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "import",
        aliases: &[],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "dedupe",
        aliases: &[],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "prune",
        aliases: &[],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "rebuild",
        aliases: &["rb"],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "fetch",
        aliases: &[],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "link",
        aliases: &["ln"],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "unlink",
        aliases: &["dislink"],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "approve-builds",
        aliases: &[],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "ignored-builds",
        aliases: &[],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "patch",
        aliases: &[],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "patch-commit",
        aliases: &[],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "patch-remove",
        aliases: &[],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "clean",
        aliases: &[],
        family: Family::Install,
    },
    // `purge` is aube's alias-shaped variant of clean (commands::clean::run_purge).
    VerbSpec {
        canonical: "purge",
        aliases: &[],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "deploy",
        aliases: &[],
        family: Family::Install,
    },
    // `x` is the short fetch-and-run spelling (the `x` in `nubx`/`bunx`; `bun x`
    // == `bunx` == dlx). It aliases dlx — NOT exec — so `nub x <tool>` fetches a
    // missing tool, matching the `nubx` standalone binary.
    VerbSpec {
        canonical: "dlx",
        aliases: &["x"],
        family: Family::Install,
    },
    VerbSpec {
        canonical: "create",
        aliases: &[],
        family: Family::Install,
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
    },
    // ── info family: read-only queries ──────────────────────────────────
    VerbSpec {
        canonical: "list",
        aliases: &["ls"],
        family: Family::Info,
    },
    // `la`/`ll` are aube's hidden list-long variants (ListArgs + long=true).
    VerbSpec {
        canonical: "la",
        aliases: &[],
        family: Family::Info,
    },
    VerbSpec {
        canonical: "ll",
        aliases: &[],
        family: Family::Info,
    },
    VerbSpec {
        canonical: "why",
        aliases: &["w"],
        family: Family::Info,
    },
    VerbSpec {
        canonical: "outdated",
        aliases: &[],
        family: Family::Info,
    },
    VerbSpec {
        canonical: "audit",
        aliases: &[],
        family: Family::Info,
    },
    VerbSpec {
        canonical: "licenses",
        aliases: &[],
        family: Family::Info,
    },
    VerbSpec {
        canonical: "peers",
        aliases: &[],
        family: Family::Info,
    },
    VerbSpec {
        canonical: "bin",
        aliases: &[],
        family: Family::Info,
    },
    VerbSpec {
        canonical: "root",
        aliases: &[],
        family: Family::Info,
    },
    VerbSpec {
        canonical: "sbom",
        aliases: &[],
        family: Family::Info,
    },
    VerbSpec {
        canonical: "view",
        aliases: &["info", "show", "v"],
        family: Family::Info,
    },
    // Native registry full-text search (formerly an npm-only fallback).
    VerbSpec {
        canonical: "search",
        aliases: &[],
        family: Family::Info,
    },
    // ── publish family: registry writes, packaging, auth ────────────────
    VerbSpec {
        canonical: "publish",
        aliases: &[],
        family: Family::Publish,
    },
    VerbSpec {
        canonical: "pack",
        aliases: &[],
        family: Family::Publish,
    },
    VerbSpec {
        canonical: "version",
        aliases: &[],
        family: Family::Publish,
    },
    VerbSpec {
        canonical: "deprecate",
        aliases: &[],
        family: Family::Publish,
    },
    VerbSpec {
        canonical: "undeprecate",
        aliases: &[],
        family: Family::Publish,
    },
    VerbSpec {
        canonical: "dist-tag",
        aliases: &["dist-tags"],
        family: Family::Publish,
    },
    VerbSpec {
        canonical: "unpublish",
        aliases: &[],
        family: Family::Publish,
    },
    VerbSpec {
        canonical: "login",
        aliases: &["adduser"],
        family: Family::Publish,
    },
    VerbSpec {
        canonical: "logout",
        aliases: &[],
        family: Family::Publish,
    },
    // Native account/registry verbs (formerly npm-only fallbacks upstream).
    // `stage` is intentionally absent: it is not a real npm/pnpm command, so
    // it falls through to the unknown-command path rather than being refused.
    VerbSpec {
        canonical: "whoami",
        aliases: &[],
        family: Family::Publish,
    },
    VerbSpec {
        canonical: "owner",
        aliases: &["owners"],
        family: Family::Publish,
    },
    VerbSpec {
        canonical: "token",
        aliases: &[],
        family: Family::Publish,
    },
    // ── store/config family: store + cache forensics, settings ──────────
    VerbSpec {
        canonical: "store",
        aliases: &[],
        family: Family::StoreConfig,
    },
    VerbSpec {
        canonical: "cache",
        aliases: &[],
        family: Family::StoreConfig,
    },
    VerbSpec {
        canonical: "cat-file",
        aliases: &[],
        family: Family::StoreConfig,
    },
    VerbSpec {
        canonical: "cat-index",
        aliases: &[],
        family: Family::StoreConfig,
    },
    VerbSpec {
        canonical: "find-hash",
        aliases: &[],
        family: Family::StoreConfig,
    },
    VerbSpec {
        canonical: "config",
        aliases: &["c"],
        family: Family::StoreConfig,
    },
    // hidden config get/set shorthands upstream.
    VerbSpec {
        canonical: "get",
        aliases: &[],
        family: Family::StoreConfig,
    },
    VerbSpec {
        canonical: "set",
        aliases: &[],
        family: Family::StoreConfig,
    },
    // Native package.json editors (formerly npm-only fallbacks).
    VerbSpec {
        canonical: "pkg",
        aliases: &[],
        family: Family::StoreConfig,
    },
    VerbSpec {
        canonical: "set-script",
        aliases: &["ss"],
        family: Family::StoreConfig,
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

/// The engine settings this project's own `nub.jsonc` supplies.
///
/// The config surface needs them to decide where a write belongs, and it runs
/// OUTSIDE the install that lowers them, so they are lowered here the way the
/// install lowers them ([`host_settings::supplied_settings`]) rather than
/// predicted a second time. Only a nub project reaches this: a pnpm project's
/// `config set` is pnpm's own command.
pub(crate) fn project_supplied_settings() -> Vec<String> {
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
        return Vec::new();
    }
    let Some(config) = crate::project_config::effective_config() else {
        return Vec::new();
    };
    let mut supplied: Vec<String> = host_settings::supplied_settings(&config.values.install)
        .into_iter()
        .map(|(key, _)| key)
        .collect();
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
    supplied
}

/// Per-process, mtime-validated cache of parsed `package.json` files keyed by
/// path. The framework gates and the member-pattern read parse the same root
/// and member manifests several times per command; this collapses the repeats
/// to one read, and the mtime check re-reads a manifest rewritten mid-command.
/// A missing or unparseable file yields `None` and is not cached.
static MANIFEST_CACHE: nub_core::config_cache::MtimeCache<serde_json::Value> =
    nub_core::config_cache::MtimeCache::new();

/// The fields a declared dependency is looked up in, in order.
/// `peerDependencies` is not one of them.
pub(crate) const DEPENDENCY_FIELDS: [&str; 3] =
    ["dependencies", "devDependencies", "optionalDependencies"];

/// `path` read as a JSON object through [`MANIFEST_CACHE`].
pub(crate) fn cached_manifest(path: &Path) -> Option<std::sync::Arc<serde_json::Value>> {
    MANIFEST_CACHE.get_or_read(path, || {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str::<serde_json::Value>(nub_core::strip_utf8_bom(&text))
            .ok()
            .filter(serde_json::Value::is_object)
    })
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

/// Whether the scoping warning should be dim-styled. Delegates to the CLI's single
/// color predicate so an explicit `--color`/`--no-color` governs the engine's
/// warnings too, and so `FORCE_COLOR=0` reads as OFF rather than as merely set.
pub(crate) fn scope_warning_uses_dim() -> bool {
    use std::io::IsTerminal;
    crate::cli::color_enabled(std::io::stderr().is_terminal())
}

/// The project-local virtual-store directory leaf under `node_modules/`.
/// `.store` is the vendor-neutral isolated-store convention (npm isolated-mode
/// RFC-0042, Yarn-berry's pnpm-linker `pnpmStoreFolder`, cnpm), so tools that
/// walk `node_modules` for a project root — simple-git-hooks and its class —
/// recognize it as an install marker; the former `.nub` leaf was invisible to
/// them. Single source of truth for the name: the engine profile's virtual
/// store directory and vite_compat's scan both read it.
// @lat: [[research/store-marker-hardcoding#Synthesis / recommendation (recommend-only)]]
pub(crate) const PROJECT_VIRTUAL_STORE_LEAF: &str = ".store";

/// The `npm_config_user_agent` a script nub launches sees — `nub run`, `nub
/// exec`, and a bin `nubx` runs — which is the string the install's own
/// lifecycle scripts see for the same project, so a tool sniffing the running
/// package manager gets one answer from every surface.
///
/// A pnpm project gets pnpm's string; a nub project gets nub's leading token on
/// the same tail ([`host_settings::lifecycle_user_agent`]). The identity is the
/// install's own, so the two cannot pick differently.
///
/// The leading token is the contested part: a `nub/`-first string is honest but
/// unrecognized by the whitelist detectors (`package-manager-detector`,
/// create-next-app), which fall back to npm and print npm commands. That cost
/// was weighed against masquerading as pnpm, and honesty won.
// @lat: [[research/npm-config-user-agent#Current behavior]]
pub(crate) fn script_user_agent(cwd: &Path) -> String {
    match project_identity::detect(cwd) {
        project_identity::ProjectIdentity::Nub => host_settings::lifecycle_user_agent(),
        project_identity::ProjectIdentity::Pnpm => {
            pnpm_user_agent(nub_core::pm::resolve::declared_pm_raw(cwd))
        }
    }
}

/// pnpm's own user agent, with the version an exact pnpm pin names.
///
/// An install under such a pin delegates to that pnpm, so its lifecycle
/// scripts see that version. A range, or no pin at all, runs the embedded
/// engine, whose version the string already carries.
fn pnpm_user_agent(declared: Option<(String, Option<String>)>) -> String {
    let engine = pnpm_config::default_user_agent();
    let pinned = declared
        .filter(|(name, _)| name == "pnpm")
        .and_then(|(_, version)| version)
        // `packageManager` may carry a `+sha512.…` integrity suffix.
        .map(|version| version.split('+').next().unwrap_or_default().to_owned())
        .filter(|version| semver::Version::parse(version).is_ok());
    match (pinned, engine.split_once(' ')) {
        (Some(version), Some((_, tail))) => format!("pnpm/{version} {tail}"),
        _ => engine,
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

/// Whether `dir` holds a pnpm-NAMED file. Gates on NAME, not effect (the
/// brand-boundary rule): a generically-named field pnpm happens to read is not
/// a pnpm-named file.
fn dir_has_pnpm_named_file(dir: &Path) -> bool {
    const PNPM_NAMED: &[&str] = &[
        "pnpm-workspace.yaml",
        ".pnpmfile.cjs",
        ".pnpmfile.mjs",
        ".pnpmrc",
    ];
    PNPM_NAMED.iter().any(|name| dir.join(name).is_file())
}

/// Every workspace member's directory under `root`, or none when `root` is not
/// a workspace.
///
/// The one place the member walk is spelled. Callers that need the manifests
/// read them through [`cached_manifest`], which is mtime-cached, so
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
    cached_manifest(&root.join("package.json"))
        .and_then(|manifest| {
            nub_core::workspace::filter::workspace_patterns_from_manifest(&manifest)
        })
        .unwrap_or_default()
}

/// The declared package, if any, whose resolver cannot reach a store shared
/// between projects — so this install must build its virtual store inside the
/// project. Either a framework nub knows about, or one the project names in
/// `disableGlobalVirtualStoreForPackages`.
///
/// pnpm 12 has no setting that names such packages, so nub matches them itself
/// and decides the store's locality directly ([`host_settings`]). The install
/// report reads this one definition too, because a list that drifted from the
/// predicate would put a framework's name in the report while the install built
/// the tree that framework cannot load.
///
/// Returns the first match rather than a bool: the reason is worth reporting,
/// and the frameworks nub knows about answer before the project's own list.
pub(crate) fn store_locality_breaker(root: &Path, members: &[PathBuf]) -> Option<String> {
    known_store_locality_breaker(root, members)
        .map(str::to_owned)
        .or_else(|| declared_store_opt_out(root, members, &store_opt_out_patterns(root)))
}

/// The frameworks nub knows break under a shared store, ordered so the
/// unconditional ones answer first.
fn known_store_locality_breaker(root: &Path, members: &[PathBuf]) -> Option<&'static str> {
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

/// The first dependency the root or a member declares that `patterns` names.
/// The package comes back rather than the pattern that caught it, because the
/// name is what the reader finds in their own manifest.
fn declared_store_opt_out(root: &Path, members: &[PathBuf], patterns: &[String]) -> Option<String> {
    let matcher = pnpm_config::matcher::create_matcher(patterns);
    if matcher.is_empty() {
        return None;
    }
    std::iter::once(root)
        .chain(members.iter().map(PathBuf::as_path))
        .flat_map(declared_dependency_names)
        .find(|name| matcher.matches(name))
}

/// The dependency names `dir`'s manifest declares, in the scopes the framework
/// gates read.
fn declared_dependency_names(dir: &Path) -> Vec<String> {
    let Some(manifest) = cached_manifest(&dir.join("package.json")) else {
        return Vec::new();
    };
    DEPENDENCY_FIELDS
        .into_iter()
        .filter_map(|field| manifest.get(field)?.as_object())
        .flat_map(|deps| deps.keys().cloned())
        .collect()
}

/// The packages the project names in `disableGlobalVirtualStoreForPackages`,
/// from the highest source that sets it: `npm_config_*` over the project's
/// `.npmrc` over the user's. pnpm 12 has no such setting, so the engine never
/// reads it. A comma-separated value and the `key[]=` list form both work.
fn store_opt_out_patterns(root: &Path) -> Vec<String> {
    const SPELLINGS: [&str; 3] = [
        "disableGlobalVirtualStoreForPackages",
        "disable-global-virtual-store-for-packages",
        "disable_global_virtual_store_for_packages",
    ];
    let names_the_setting = |key: &str| {
        SPELLINGS
            .iter()
            .any(|spelling| key.eq_ignore_ascii_case(spelling))
    };
    let items = |value: &str| {
        value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let mut patterns = Vec::new();
    for (_, text) in host_settings::npmrc_files(root) {
        for (key, raw) in host_settings::npmrc_entries(&text) {
            if names_the_setting(&key) {
                patterns = match raw {
                    host_settings::Raw::Scalar(value) => items(&value),
                    host_settings::Raw::List(values) => values,
                };
            }
        }
    }
    const PREFIX: &str = "npm_config_";
    for (name, value) in std::env::vars_os() {
        let (Some(name), Some(value)) = (name.to_str(), value.to_str()) else {
            continue;
        };
        if name
            .get(..PREFIX.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(PREFIX))
            && names_the_setting(&name[PREFIX.len()..])
        {
            patterns = items(value);
        }
    }
    patterns
}

/// Whether `dir` holds nub's own canonical lockfile under EITHER the current
/// name (`nub.lock`) or the legacy name (`lock.yaml`) still honored through
/// the rename transition.
fn nub_lockfile_present(dir: &Path) -> bool {
    dir.join(use_align::NUB_LOCKFILE).is_file()
        || dir.join(use_align::NUB_LEGACY_LOCKFILE).is_file()
}

/// Nub's PM cache root — the `pm` namespace under
/// [`nub_core::node::discovery::cache_dir`], so `$XDG_CACHE_HOME/nub/pm` or
/// `~/.cache/nub/pm`.
///
/// The one place the namespace is spelled. It is the root every PM-owned cache
/// tier hangs off (the node-gyp bootstrap's tool dir, dlx scratch), and it is
/// also the CLAMP for the identity walk: an install running inside it is
/// nub-internal by construction and must not inherit identity from whatever sits
/// above the cache (#489). Both readings have to name the same directory, which
/// is why they share this rather than each joining `pm` themselves.
pub(crate) fn pm_cache_dir() -> Option<PathBuf> {
    nub_core::node::discovery::cache_dir().map(|cache| cache.join("pm"))
}

/// The PM cache root to stop an identity walk at, or `None` when `cwd` is not
/// inside it and no clamp applies.
///
/// Read by the identity walk ([`project_identity::detect`]). A clamp naming a
/// different directory from the cache the install actually uses is no clamp at
/// all: when two walks existed they diverged once and only the config-read walk
/// was guarded, which left the install inheriting a pnpm identity from above the
/// cache silently.
///
/// The SPELLING is the subtle part. A temp dir is a symlink on macOS, so the
/// raw root and its canonicalization are different strings and at most one of
/// them is a prefix of `cwd` — the containment test has to keep using whichever
/// one matched, or it stops holding as the walk pops.
pub(crate) fn pm_cache_clamp(cwd: &Path) -> Option<PathBuf> {
    let root = pm_cache_dir()?;
    if cwd.starts_with(&root) {
        return Some(root);
    }
    let canon = std::fs::canonicalize(&root).ok()?;
    cwd.starts_with(&canon).then_some(canon)
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

    /// The frameworks whose resolver cannot reach a store shared between
    /// projects. `next` and `react-native` break at every version; `expo` only
    /// below SDK 56, where its Metro fork became store-aware, and a range whose
    /// major cannot be floored before resolution counts as below.
    #[test]
    fn a_framework_the_shared_store_cannot_serve_is_named() {
        let breaker = |manifest: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("package.json"), manifest).unwrap();
            store_locality_breaker(dir.path(), &[])
        };
        assert_eq!(
            breaker(r#"{"name":"x","dependencies":{"next":"15.0.0"}}"#),
            Some("next".to_owned())
        );
        assert_eq!(
            breaker(r#"{"name":"x","devDependencies":{"react-native":"0.76.0"}}"#),
            Some("react-native".to_owned())
        );
        for below in [
            r#"{"name":"x","dependencies":{"expo":"~52.0.0"}}"#,
            r#"{"name":"x","devDependencies":{"expo":"^51.0.0"}}"#,
            r#"{"name":"x","optionalDependencies":{"expo":"50.0.0"}}"#,
            r#"{"name":"x","dependencies":{"expo":"*"}}"#,
        ] {
            assert_eq!(breaker(below), Some("expo".to_owned()), "{below}");
        }
        for keeps in [
            r#"{"name":"x","dependencies":{"expo":"~56.0.0"}}"#,
            r#"{"name":"x","dependencies":{"expo":"^57.0.4"}}"#,
            r#"{"name":"x","dependencies":{"react":"19.2.0"}}"#,
        ] {
            assert_eq!(breaker(keeps), None, "{keeps}");
        }
    }

    /// A package the project names is matched against what the root and its
    /// members declare, `*` included, and the NAME comes back.
    #[test]
    fn a_package_the_project_names_is_matched_against_its_declarations() {
        let root = tempfile::tempdir().unwrap();
        let member = root.path().join("packages/app");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{"name":"ws","devDependencies":{"typescript":"5.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(
            member.join("package.json"),
            r#"{"name":"app","dependencies":{"@acme/bundler":"1.0.0"}}"#,
        )
        .unwrap();
        let named = |patterns: &[&str]| {
            let patterns: Vec<String> = patterns.iter().map(|p| (*p).to_owned()).collect();
            declared_store_opt_out(root.path(), std::slice::from_ref(&member), &patterns)
        };
        assert_eq!(named(&["@acme/*"]).as_deref(), Some("@acme/bundler"));
        assert_eq!(
            named(&["other", "typescript"]).as_deref(),
            Some("typescript")
        );
        assert_eq!(named(&["other"]), None);
        assert_eq!(named(&[]), None);
    }

    #[test]
    fn a_pnpm_projects_script_user_agent_names_only_an_exact_pnpm_pin() {
        let engine = pnpm_config::default_user_agent();
        let (_, tail) = engine
            .split_once(' ')
            .expect("the engine's user agent carries a tail");
        let pin = |name: &str, v: &str| Some((name.to_string(), Some(v.to_string())));

        assert_eq!(pnpm_user_agent(None), engine, "no pin: the engine's own");
        assert_eq!(
            pnpm_user_agent(pin("pnpm", "9.1.0+sha512.abc")),
            format!("pnpm/9.1.0 {tail}"),
            "an exact pin names its version, without the integrity suffix"
        );
        assert_eq!(
            pnpm_user_agent(pin("pnpm", "^12.0.0")),
            engine,
            "a range delegates to no other pnpm"
        );
        assert_eq!(
            pnpm_user_agent(pin("npm", "11.0.0")),
            engine,
            "another tool's pin names no pnpm version"
        );
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
