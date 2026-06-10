//! `nub install` / `nub ci` — dependency installation through the embedded
//! aube engine (vendor/aube, linked as a library; no subprocess).
//!
//! The dispatch shape mirrors aube's own CLI entry (`vendor/aube/crates/aube/
//! src/lib.rs::run_install_command`): build the CLI flag bag, resolve the
//! frozen mode, construct `InstallOptions`, run `commands::install::run` on a
//! multi-thread tokio runtime. The nub layer sits on top through the engine's
//! embedder seams (`engine_preflight`) plus one hard gate:
//!
//! 1. **Env families** — only the npm-compatible (`npm_config_*`) and
//!    ecosystem-neutral (`CI`, proxies, …) env surfaces are consulted;
//!    `AUBE_*` variables are invisible. Nub's contract is "configure me the
//!    way you configure npm/pnpm"; honoring another tool's branded env vars
//!    would create an accidental config surface.
//! 2. **Embedder defaults** (set-unless-user-set; CLI flags, env vars, and
//!    every config file all win over these):
//!    - `defaultLockfileFormat=pnpm` — a fresh project gets `pnpm-lock.yaml`,
//!      keeping the project portable to pnpm instead of aube-branded.
//!    - `virtualStoreDir=node_modules/.nub` + `stateDir=node_modules/.nub` —
//!      the isolated store materializes under `.nub`, with the engine's
//!      install-state sidecar tucked inside it. (The state *basename*
//!      `.aube-state` is a fixed constant in the engine, so the one
//!      aube-named path that survives is `node_modules/.nub/.aube-state`.)
//!    - **Layout policy**: the detected lockfile kind picks the default
//!      `nodeLinker` — flat-layout ecosystems (`package-lock.json`,
//!      `npm-shrinkwrap.json`, `yarn.lock`, `bun.lock`) default to `hoisted`;
//!      `pnpm-lock.yaml` / `aube-lock.yaml` / no lockfile keep the engine's
//!      `isolated` default.
//!    - User agent: lifecycle scripts see `npm_config_user_agent=nub/<ver> …`
//!      and registry requests carry the same product token.
//! 3. **Yarn write gate** — aube's yarn.lock *write* fidelity is unproven, so
//!    any install that would mutate a detected `yarn.lock` (classic or berry)
//!    is refused. Frozen-satisfiable installs proceed (the lockfile-read path
//!    never rewrites the lockfile; only a re-resolve writes).
//!
//! KNOWN APPROXIMATIONS:
//! - `preferFrozenLockfile` from `.npmrc` / workspace yaml is not consulted
//!   when defaulting the frozen mode (aube's `FileSources` is crate-private
//!   at the pinned API); without a CLI flag the mode falls back to aube's
//!   env-aware default (CI ⇒ frozen, else prefer-frozen).
//! - The yarn gate maps aube's frozen-drift failure by message substring
//!   ("lockfile is out of date") — the drift errors carry no stable
//!   `ERR_AUBE_*` code at the pinned API. Flag for upstream: give the
//!   frozen-drift and strict-no-lockfile errors diagnostic codes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use aube::commands::install::{DepSelection, FrozenMode, InstallArgs, InstallOptions};
use aube_lockfile::LockfileKind;

/// `nub install` flags, as parsed by nub's clap surface. A deliberate subset
/// of aube's `InstallArgs` — the flags with a nub-level contract; everything
/// else stays at aube's defaults.
#[derive(Debug, Default)]
pub struct InstallFlags {
    pub frozen_lockfile: bool,
    pub no_frozen_lockfile: bool,
    pub prefer_frozen_lockfile: bool,
    pub prod: bool,
    pub dev: bool,
    pub ignore_scripts: bool,
    pub no_optional: bool,
    pub offline: bool,
    pub prefer_offline: bool,
    pub lockfile_only: bool,
    pub force: bool,
    pub node_linker: Option<String>,
    pub dir: Option<PathBuf>,
}

/// `nub ci` flags. `ci` is frozen + clean by definition, so only the script /
/// optional-dep knobs are configurable (mirrors `aube ci`'s `CiArgs`).
#[derive(Debug, Default)]
pub struct CiFlags {
    pub ignore_scripts: bool,
    pub no_optional: bool,
    pub dir: Option<PathBuf>,
}

/// `nub install` — route through the embedded aube install engine.
pub fn run_install(flags: InstallFlags) -> Result<i32> {
    apply_dir(flags.dir.as_deref())?;
    let cwd = std::env::current_dir()?;
    let detected = detect_lockfile_walk_up(&cwd);
    engine_preflight(detected.as_ref());

    // Mirror `run_install_command`: defaults from clap, nub's flags on top.
    let mut args = default_install_args();
    args.prod = flags.prod;
    args.dev = flags.dev;
    args.ignore_scripts = flags.ignore_scripts;
    args.no_optional = flags.no_optional;
    args.offline = flags.offline;
    args.prefer_offline = flags.prefer_offline;
    args.lockfile_only = flags.lockfile_only;
    args.force = flags.force;
    args.node_linker = flags.node_linker.clone();
    args.lockfile.frozen_lockfile = flags.frozen_lockfile;
    args.lockfile.no_frozen_lockfile = flags.no_frozen_lockfile;
    args.lockfile.prefer_frozen_lockfile = flags.prefer_frozen_lockfile;

    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    let global_frozen = args.lockfile.frozen_override();
    let cli_flags = args.to_cli_flag_bag(global_frozen, args.virtual_store.flags());

    // yaml_prefer_frozen: None — see KNOWN APPROXIMATIONS in the module doc.
    let mut opts = args.into_options(global_frozen, None, cli_flags, env_snapshot());

    let yarn = matches!(
        detected.as_ref().map(|d| d.kind),
        Some(LockfileKind::Yarn | LockfileKind::YarnBerry)
    );
    if yarn {
        let dir = &detected.as_ref().expect("yarn implies detection").dir;
        // Refuse upfront when the flags *ask* for a lockfile write…
        if flags.no_frozen_lockfile || flags.force || flags.lockfile_only {
            return Err(yarn_gate_error(
                "the requested install would rewrite yarn.lock",
            ));
        }
        // …or when the lockfile can't satisfy the manifest (the install would
        // have to re-resolve, which writes yarn.lock).
        if let Some(reason) = yarn_drift_reason(dir) {
            return Err(yarn_gate_error(&format!(
                "yarn.lock is out of date ({reason})"
            )));
        }
        // Belt-and-braces: force strict-frozen so anything the pre-flight
        // missed errors instead of rewriting yarn.lock. (`strict_no_lockfile`
        // stays as `into_options` resolved it — a missing yarn.lock can't
        // happen here; detection just saw one.)
        opts.mode = FrozenMode::Frozen;
    }

    run_engine(opts, yarn)
}

/// `nub ci` — frozen + clean install, npm-ci semantics. Constructed at the
/// nub layer as a field-for-field mirror of `aube ci`
/// (`vendor/aube/crates/aube/src/commands/ci.rs`) rather than calling
/// `commands::ci::run`: the ci entry point builds its `InstallOptions` with an
/// empty `cli_flags` bag, which would leave no channel for the nodeLinker
/// layout policy. Semantics shipped: delete `node_modules`, then install with
/// `FrozenMode::Frozen` + `strict_no_lockfile` (drift or no lockfile ⇒ hard
/// error), root lifecycle hooks on unless `--ignore-scripts`.
pub fn run_ci(flags: CiFlags) -> Result<i32> {
    apply_dir(flags.dir.as_deref())?;
    let cwd = std::env::current_dir()?;
    let detected = detect_lockfile_walk_up(&cwd);
    engine_preflight(detected.as_ref());

    // Clean first, like `aube ci` / `npm ci`. The project root for nub's
    // purposes is where the lockfile lives (fall back to cwd for the
    // no-lockfile case — the strict install below errors before linking).
    // Approximation: assumes the default `node_modules` modulesDir name.
    let root = detected
        .as_ref()
        .map(|d| d.dir.clone())
        .unwrap_or_else(|| cwd.clone());
    remove_node_modules(&root.join("node_modules"))?;

    let opts = InstallOptions {
        mode: FrozenMode::Frozen,
        dep_selection: DepSelection::from_flags(false, false, flags.no_optional),
        ignore_scripts: flags.ignore_scripts,
        strict_no_lockfile: true,
        cli_flags: Vec::new(),
        env_snapshot: env_snapshot(),
        // `nub ci` is the argumentless-install shape: root lifecycle hooks run.
        skip_root_lifecycle: false,
        ..InstallOptions::with_mode(FrozenMode::Frozen)
    };

    let yarn = matches!(
        detected.as_ref().map(|d| d.kind),
        Some(LockfileKind::Yarn | LockfileKind::YarnBerry)
    );
    if yarn {
        // `nub ci` never writes the lockfile (strict frozen), but the engine's
        // frozen drift check is blind to yarn formats (see yarn_drift_reason)
        // — a drifted yarn.lock would under-install and exit 0. `ci` means
        // "the lockfile is law", so surface the drift as the gate error.
        let dir = &detected.as_ref().expect("yarn implies detection").dir;
        if let Some(reason) = yarn_drift_reason(dir) {
            return Err(yarn_gate_error(&format!(
                "yarn.lock is out of date ({reason})"
            )));
        }
    }
    run_engine(opts, yarn)
}

/// Yarn-kind drift pre-flight, at the nub layer.
///
/// Why this exists: aube's yarn parsers (classic and berry) synthesize the
/// root importer by cross-referencing the manifest's deps against the
/// lockfile's `name@range` keys, silently *dropping* any manifest dep the
/// lockfile doesn't satisfy — and they record `specifier: None` on every
/// `DirectDep`, which makes the engine's frozen drift check return Fresh
/// vacuously for yarn formats. Net effect at the pinned API: a drifted
/// yarn.lock under FrozenMode::Frozen "installs" without the new dep and
/// exits 0. This pre-flight redoes the comparison the engine can't: any
/// manifest direct dep missing from the parsed root importer means the
/// lockfile can't satisfy the manifest — i.e. a real install would have to
/// re-resolve and rewrite yarn.lock, which the gate forbids.
///
/// Scope: root importer only — yarn workspace member manifests are not
/// checked (aube's yarn readers only synthesize the "." importer today).
/// Parse/read failures return None (no drift claim); the engine surfaces
/// those errors itself with better diagnostics.
fn yarn_drift_reason(dir: &Path) -> Option<String> {
    let manifest = aube_manifest::PackageJson::from_path(&dir.join("package.json")).ok()?;
    let graph = aube_lockfile::parse_lockfile(dir, &manifest).ok()?;
    let satisfied: std::collections::HashSet<&str> =
        graph.root_deps().iter().map(|d| d.name.as_str()).collect();
    let manifest_deps = manifest
        .dependencies
        .iter()
        .chain(manifest.dev_dependencies.iter())
        .chain(manifest.optional_dependencies.iter());
    for (name, spec) in manifest_deps {
        if !satisfied.contains(name.as_str()) {
            return Some(format!("{name}@{spec} is not satisfied by yarn.lock"));
        }
    }
    None
}

/// Build the runtime, run the install, render failures. `yarn_gated` switches
/// the frozen-drift failure to the yarn write-gate message.
fn run_engine(opts: InstallOptions, yarn_gated: bool) -> Result<i32> {
    let runtime = build_runtime()?;
    let result = runtime.block_on(aube::commands::install::run(opts));
    drop(runtime);
    match result {
        Ok(()) => Ok(0),
        // Frozen-drift on a gated yarn project: the install *would* rewrite
        // yarn.lock if allowed to re-resolve. Surface the gate, not aube's
        // "run without --frozen-lockfile" hint (which would punch through it).
        Err(report) if yarn_gated && report.to_string().contains("lockfile is out of date") => Err(
            yarn_gate_error(&format!("yarn.lock is out of date ({report})")),
        ),
        Err(report) => {
            // miette's Debug render = the full fancy diagnostic (message,
            // code, help). Print it like aube's own main would, keep nub's
            // exit-code path. (Aube's bespoke ERR_AUBE_* → exit-code table is
            // not consulted here; engine failures exit 1.)
            eprintln!("{report:?}");
            Ok(1)
        }
    }
}

/// The yarn write gate. See the module doc; the message names the remedy.
fn yarn_gate_error(reason: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "nub install: refusing to modify yarn.lock — {reason}\n\
         \x20\x20yarn.lock write fidelity is unproven in the embedded engine, so installs\n\
         \x20\x20that would rewrite it are blocked. Run it with yarn directly:\n\
         \x20\x20\x20\x20yarn install"
    )
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

struct DetectedLockfile {
    kind: LockfileKind,
    /// Directory the lockfile was found in (project / workspace root).
    dir: PathBuf,
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

/// Configure the engine's process-wide embedder seams. Called once at the
/// top of `run_install` / `run_ci`, before any settings resolution; every
/// seam is an idempotent once-per-process `OnceLock`, which fits nub's
/// one-command-per-process CLI shape.
fn engine_preflight(detected: Option<&DetectedLockfile>) {
    // Env surface: npm-compatible (`npm_config_*`) + ecosystem-neutral
    // (`CI`, proxies, …). `AUBE_*` stays invisible — nub's config contract
    // is the npm ecosystem's, not another tool's branded variables.
    aube::set_env_families(aube::EnvFamilies::NPM.union(aube::EnvFamilies::EXTERNAL));
    // Lifecycle scripts (npm_config_user_agent) and registry requests
    // identify the running tool: `nub/<ver> …`.
    aube::set_user_agent_product(format!("nub/{}", env!("CARGO_PKG_VERSION")));
    // Set-unless-user-set: ranks below CLI flags, env vars, and every
    // config file in the engine's settings precedence.
    aube::set_embedder_defaults(nub_setting_defaults(detected));
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
/// - Layout policy: flat-layout lockfile kinds (npm/yarn/bun) default
///   `nodeLinker` to `hoisted`; pnpm/aube kinds and fresh projects keep the
///   engine's `isolated` default (no entry pushed, so user/env settings
///   resolve exactly as in stock aube).
fn nub_setting_defaults(detected: Option<&DetectedLockfile>) -> Vec<(String, String)> {
    let mut defaults = vec![
        ("defaultLockfileFormat".to_string(), "pnpm".to_string()),
        ("virtualStoreDir".to_string(), "node_modules/.nub".to_string()),
        ("stateDir".to_string(), "node_modules/.nub".to_string()),
    ];
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

/// aube's `InstallArgs` at clap defaults, via a throwaway parse (the struct
/// has no `Default` impl and ~30 fields; the parse keeps nub compiling
/// unchanged when upstream adds defaulted flags).
fn default_install_args() -> InstallArgs {
    use clap::Parser as _;
    #[derive(clap::Parser)]
    struct Defaults {
        #[command(flatten)]
        args: InstallArgs,
    }
    Defaults::parse_from(["nub-install-defaults"]).args
}

/// Process-env snapshot for `InstallOptions::env_snapshot` — same content as
/// `aube_settings::values::capture_env()` (a clone of `std::env::vars()`),
/// built locally because aube-settings isn't a direct nub dep.
fn env_snapshot() -> Vec<(String, String)> {
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

/// Symlink-aware `node_modules` removal, mirroring `aube ci`'s
/// `remove_existing`: a symlinked node_modules is unlinked (not followed —
/// `remove_dir_all` on a symlink-to-dir would wipe the *target*).
fn remove_node_modules(nm: &Path) -> Result<()> {
    let Ok(meta) = nm.symlink_metadata() else {
        return Ok(()); // nothing to remove
    };
    eprintln!("Removing existing node_modules...");
    if meta.file_type().is_symlink() {
        std::fs::remove_file(nm)
    } else {
        std::fs::remove_dir_all(nm)
    }
    .with_context(|| format!("failed to remove {}", nm.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_linker_default(defaults: &[(String, String)]) -> Option<&str> {
        defaults
            .iter()
            .find(|(k, _)| k == "nodeLinker")
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
                node_linker_default(&nub_setting_defaults(Some(&detected(kind)))),
                Some("hoisted"),
                "{kind:?} must default to the hoisted layout"
            );
        }

        // pnpm-shaped kinds and no lockfile ⇒ no entry (engine's isolated
        // default applies, user/env settings resolve as in stock aube).
        for kind in [LockfileKind::Pnpm, LockfileKind::Aube] {
            assert_eq!(
                node_linker_default(&nub_setting_defaults(Some(&detected(kind)))),
                None,
                "{kind:?} must not inject a nodeLinker default"
            );
        }
        assert_eq!(
            node_linker_default(&nub_setting_defaults(None)),
            None,
            "no lockfile must not inject a nodeLinker default"
        );
    }

    #[test]
    fn setting_defaults_always_carry_the_nub_identity_settings() {
        // Every install gets the pnpm lockfile default and the `.nub`
        // store/state location, regardless of detection. (These ride the
        // engine's embedder-defaults tier, so any user source overrides
        // them — precedence is covered by the engine's own tests and the
        // install_engine integration tests.)
        for detected in [None, Some(LockfileKind::Npm), Some(LockfileKind::Pnpm)] {
            let dir = tempfile::tempdir().unwrap();
            let detected = detected.map(|kind| DetectedLockfile {
                kind,
                dir: dir.path().to_path_buf(),
            });
            let defaults = nub_setting_defaults(detected.as_ref());
            let get = |key: &str| {
                defaults
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str())
            };
            assert_eq!(get("defaultLockfileFormat"), Some("pnpm"));
            assert_eq!(get("virtualStoreDir"), Some("node_modules/.nub"));
            assert_eq!(get("stateDir"), Some("node_modules/.nub"));
        }
    }
}
