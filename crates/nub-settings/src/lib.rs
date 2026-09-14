//! The settings schema `nub config` is routed by.
//!
//! Every setting nub honors is declared in the crate-local `settings.toml`:
//! its name, type, default, and the source surfaces (CLI flag, env var,
//! `.npmrc`, workspace YAML) that can populate it. `build.rs` turns that file
//! into [`meta::SETTINGS`], a `&'static [SettingMeta]` slice.
//!
//! The table is what makes `config set` ROUTING possible rather than merely
//! descriptive: whether a key is a scalar or a map, whether it is layout-
//! affecting, and which `.npmrc` spellings alias it all decide which file a
//! write lands in. That is why the schema is nub's own and not derived from
//! the engine, which exposes no equivalent.

pub mod meta;

pub use meta::{
    SettingMeta, all, find, is_layout_npmrc_key, is_supported, unsupported_advice,
    unsupported_for_key,
};

/// Settings the table declares but nub never consumes, each with the line
/// `config set` prints instead of writing the key through.
///
/// Without this, a setting whose only reader was the previous engine reads as
/// free-form config: `nub config set aubeNoAutoInstall true` used to write
/// that name into a user's `.npmrc`, inert and carrying a brand nub does not
/// own. A name here that no longer spells a real setting is a SILENT no-op —
/// the filter simply never matches — which is why `meta`'s own tests check
/// every entry against the table.
pub const UNSUPPORTED_SETTINGS: &[(&str, &str)] = &[
    // `commands::auto_install::ensure_installed`, reached only from the
    // engine's `run`/`exec`/`restart`. nub runs scripts through its own
    // frontend and gates freshness in `crate::verify_deps`. Before this
    // entry `nub config set aubeNoAutoInstall true` wrote the ENGINE's brand
    // into a user's `.npmrc` for a value nub never reads.
    (
        "aubeNoAutoInstall",
        "nub does not auto-install before a run. Use `verifyDeps` in nub.jsonc, or \
             `verify-deps-before-run` in .npmrc, to choose what happens when dependencies \
             are stale.",
    ),
    // Same dead gate: the flag that lets that auto-install skip a repeat.
    (
        "optimisticRepeatInstall",
        "nub does not auto-install before a run, so there is no repeat install to skip. \
             Use `verifyDeps` in nub.jsonc, or `verify-deps-before-run` in .npmrc, to choose \
             what happens when dependencies are stale.",
    ),
    // `commands::run`, the engine's script runner. nub runs scripts through
    // its own frontend (`cli::run_single_script`), which runs `pre`/`post`
    // unconditionally — the DECIDED behavior, not an oversight: the run docs
    // promise `npm run` semantics for the hooks and name `--ignore-scripts`
    // as the way to skip them. So the key can never decide anything here,
    // and the advice repeats the documented escape hatch.
    (
        "enablePrePostScripts",
        "nub always runs `pre`/`post` scripts for a named script, like npm. \
             Pass `--ignore-scripts` to `nub run` to skip the whole lifecycle.",
    ),
    // The previous engine's per-package eject list. The pnpm engine never reads
    // it; nub takes the same list from `install.linker.eject`.
    (
        "diskMaterializePackages",
        "nub does not read this setting. Name the package in `install.linker.eject` \
             in nub.jsonc to keep it out of the shared store.",
    ),
    // `update_check::check_and_notify`, reached from the engine's own CLI
    // dispatcher and `doctor`. nub's self-update is `nub upgrade`
    // (`self_update_enabled: false`), which never runs during another verb.
    (
        "updateNotifier",
        "nub does not check for its own updates while running a command. Run `nub upgrade` \
             when you want a new version.",
    ),
    // `runtime::RuntimeSettings::from_ctx`. `resolve_context` returns the
    // PATH fallback before consuming any of them under
    // `runtime_switching: false` — nub owns Node provisioning.
    (
        "runtimeInstaller",
        "nub provisions Node itself rather than delegating to another installer. \
             Manage versions with `nub node install` and `nub node pin`.",
    ),
    (
        "runtimeOnFail",
        "nub provisions Node itself and installs a missing pin on demand. \
             Manage versions with `nub node install` and `nub node pin`.",
    ),
    (
        "nodeDownloadMirrors",
        "nub provisions Node itself and does not read the engine's download mirrors. \
             Install the version another way and `nub node pin` it.",
    ),
    // `startup::StartupSettings` / `package_manager_guard_mode`, built by
    // the engine's own CLI dispatcher and by self-version switching. nub
    // resolves the `packageManager` pin in `nub_core::pm::resolve`.
    (
        "packageManagerStrict",
        "nub does not enforce another package manager's pin. `nub pm pin` records the \
             project's manager, and `nub pm which` reports the one in force.",
    ),
    (
        "packageManagerStrictVersion",
        "nub does not enforce another package manager's pinned version. `nub pm pin` \
             records the project's manager.",
    ),
    (
        "managePackageManagerVersions",
        "nub does not download or switch package-manager versions. `nub pm pin` records \
             the project's manager, and `nub upgrade` updates nub itself.",
    ),
    // `commands::npm_fallback`, the engine's shell-out dispatcher for verbs
    // it has no implementation of. Every verb nub routes runs in-process.
    (
        "npmPath",
        "nub never shells out to npm; every package-manager verb runs in-process.",
    ),
    // `commands::deploy`, which `install_family` refuses as a stub.
    (
        "deployAllFiles",
        "nub does not implement `deploy`. For now: pnpm deploy.",
    ),
    // Read by the engine's own CLI dispatcher to pick an output stream. nub
    // owns its output routing.
    (
        "useStderr",
        "nub chooses its own output streams. Redirect the command's stdout or stderr \
             in your shell instead.",
    ),
    // A parity no-op in the engine ITSELF, not just under nub: accepted and
    // wired to nothing. It carries the standing note in settings.toml that
    // the flag comes off once a caller starts gating on it.
    //
    // `ignoreCompatibilityDb` USED to sit here beside it, on the grounds that
    // nub shipped no compatibility database. It ships one now — the same
    // vendored Yarn + pnpm catalogs pnpm merges into every install — so the
    // setting has a real reader and must stay settable. Leaving it listed
    // would strip it from `meta::find`, make `config set` refuse it, and
    // fall the accessor through to the default: a database applied to every
    // install with no way to turn it off.
    ("useBetaCli", "nub has no beta-gated commands."),
    // `install::FrozenMode::default_for_env` asks `aube_util::env::is_ci()`,
    // which reads the `CI` ENVIRONMENT VARIABLE. No reader consults the
    // config key, so an `.npmrc` `ci=` line has never decided anything.
    (
        "ci",
        "nub detects CI from the `CI` environment variable, not from config. Set `CI=1`, \
             or pass `--frozen-lockfile` / `--no-frozen-lockfile` to pin the install mode.",
    ),
];
