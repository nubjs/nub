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

pub use meta::{SettingMeta, all, find, is_supported, unsupported_advice, unsupported_for_key};

/// Settings the table declares but nub never consumes, each with the line
/// `config set` prints instead of writing the key through.
///
/// Without this, such a setting reads as free-form config and `config set`
/// writes it into a user's `.npmrc`, where nothing reads it. A name here that
/// no longer spells a real setting is a SILENT no-op — the filter simply never
/// matches.
pub const UNSUPPORTED_SETTINGS: &[(&str, &str)] = &[
    // Read only by the engine's script runner. `run` is a verb nub keeps, and
    // its frontend (`cli::run_single_script`) runs `pre`/`post`
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
    // The engine's check for a newer pnpm. nub updates itself through
    // `nub upgrade`, so the notice names a release and a command that do not
    // update nub.
    (
        "updateNotifier",
        "nub does not check for its own updates while running a command. Run `nub upgrade` \
             when you want a new version.",
    ),
    // Node provisioning is nub's (`nub node`). `runtimeInstaller` names no
    // pnpm 12 setting. `runtimeOnFail: download` does: the engine turns
    // `devEngines.runtime` into a Node download inside the dependency graph,
    // beside the Node nub provisions from the same field, so `host_settings`
    // keeps it out of every source as well as `config set` refusing it.
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
    // None of the three is a pnpm 12 setting. Its replacement, `pmOnFail`, is
    // read only when the embedder manages package-manager versions, and nub's
    // does not. nub resolves the `packageManager` pin in
    // `nub_core::pm::resolve`.
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
    // Not a pnpm 12 setting: the engine has no npm shell-out to point it at.
    (
        "npmPath",
        "nub never shells out to npm; every package-manager verb runs in-process.",
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
    // The engine derives the `ci` default from the process environment.
    (
        "ci",
        "nub detects CI from the `CI` environment variable, not from config. Set `CI=1`, \
             or pass `--frozen-lockfile` / `--no-frozen-lockfile` to pin the install mode.",
    ),
];
