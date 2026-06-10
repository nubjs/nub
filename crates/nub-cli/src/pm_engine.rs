//! `nub install` / `nub ci` — dependency installation through the embedded
//! aube engine (vendor/aube, linked as a library; no subprocess).
//!
//! The dispatch shape mirrors aube's own CLI entry (`vendor/aube/crates/aube/
//! src/lib.rs::run_install_command`): build the CLI flag bag, resolve the
//! frozen mode, construct `InstallOptions`, run `commands::install::run` on a
//! multi-thread tokio runtime. Two nub-layer policies sit on top:
//!
//! 1. **Layout policy** — the detected lockfile kind picks the default
//!    `nodeLinker`: flat-layout ecosystems (`package-lock.json`,
//!    `npm-shrinkwrap.json`, `yarn.lock`, `bun.lock`) default to `hoisted`;
//!    `pnpm-lock.yaml` / `aube-lock.yaml` / no lockfile keep aube's `isolated`
//!    default. An explicit `--node-linker` or a project-level setting
//!    (`.npmrc` `node-linker`, workspace-yaml `nodeLinker`) wins over the
//!    policy.
//! 2. **Yarn write gate** — aube's yarn.lock *write* fidelity is unproven, so
//!    any install that would mutate a detected `yarn.lock` (classic or berry)
//!    is refused. Frozen-satisfiable installs proceed (the lockfile-read path
//!    never rewrites the lockfile; only a re-resolve writes).
//!
//! KNOWN APPROXIMATIONS (for the integrate agent; the fork's programmatic
//! settings overlay landing this phase replaces both):
//! - The layout-policy default rides the *CLI* tier of aube's settings
//!   precedence (`cli > env > npmrc > workspace > default`), guarded by a
//!   nub-side scan for project-level `node-linker` settings. An env-var
//!   override (`npm_config_node_linker`) is NOT detected by the scan and
//!   would be shadowed by the policy default. Proper fix: inject at the
//!   *default* tier via the settings overlay.
//! - `preferFrozenLockfile` from `.npmrc` / workspace yaml is not consulted
//!   when defaulting the frozen mode (aube's `FileSources` is crate-private
//!   at the pinned API, b15cdcb); without a CLI flag the mode falls back to
//!   aube's env-aware default (CI ⇒ frozen, else prefer-frozen).
//! - The yarn gate maps aube's frozen-drift failure by message substring
//!   ("lockfile is out of date") — the drift errors carry no stable
//!   `ERR_AUBE_*` code at b15cdcb. Flag for upstream: give the frozen-drift
//!   and strict-no-lockfile errors diagnostic codes.

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
    let mut cli_flags = args.to_cli_flag_bag(global_frozen, args.virtual_store.flags());
    apply_node_linker_policy(&mut cli_flags, detected.as_ref());

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

    // Clean first, like `aube ci` / `npm ci`. The project root for nub's
    // purposes is where the lockfile lives (fall back to cwd for the
    // no-lockfile case — the strict install below errors before linking).
    // Approximation: assumes the default `node_modules` modulesDir name.
    let root = detected
        .as_ref()
        .map(|d| d.dir.clone())
        .unwrap_or_else(|| cwd.clone());
    remove_node_modules(&root.join("node_modules"))?;

    let mut cli_flags: Vec<(String, String)> = Vec::new();
    apply_node_linker_policy(&mut cli_flags, detected.as_ref());

    let opts = InstallOptions {
        mode: FrozenMode::Frozen,
        dep_selection: DepSelection::from_flags(false, false, flags.no_optional),
        ignore_scripts: flags.ignore_scripts,
        strict_no_lockfile: true,
        cli_flags,
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

/// The layout policy: default `nodeLinker` from the detected lockfile kind —
/// hoisted for the flat-layout ecosystems, aube's isolated default otherwise.
/// Only applies when nothing else set it: an explicit `--node-linker` is
/// already in the bag (and wins by ordering — aube's settings resolver takes
/// the first matching CLI entry), and a project-level setting disables the
/// policy entirely. For pnpm/aube/none kinds the policy pushes nothing, so
/// project/env settings resolve exactly as in stock aube.
fn apply_node_linker_policy(
    cli_flags: &mut Vec<(String, String)>,
    detected: Option<&DetectedLockfile>,
) {
    let Some(detected) = detected else { return };
    let hoisted_kind = matches!(
        detected.kind,
        LockfileKind::Npm
            | LockfileKind::NpmShrinkwrap
            | LockfileKind::Yarn
            | LockfileKind::YarnBerry
            | LockfileKind::Bun
    );
    if !hoisted_kind {
        return;
    }
    if cli_flags.iter().any(|(k, _)| k == "node-linker") {
        return; // explicit --node-linker wins
    }
    if project_sets_node_linker(&detected.dir) {
        return; // project settings win
    }
    cli_flags.push(("node-linker".to_string(), "hoisted".to_string()));
}

/// Best-effort scan for a project-level `nodeLinker` setting so the layout
/// policy doesn't shadow it. Checks the lockfile dir's `.npmrc`
/// (`node-linker=` key) and workspace yaml (`nodeLinker:` key) — the two
/// project-scoped sources in aube's settings precedence. Env/user/global
/// tiers are not scanned (see KNOWN APPROXIMATIONS in the module doc).
fn project_sets_node_linker(dir: &Path) -> bool {
    let npmrc_sets = std::fs::read_to_string(dir.join(".npmrc")).is_ok_and(|s| {
        s.lines().any(|l| {
            let l = l.trim();
            !l.starts_with('#')
                && !l.starts_with(';')
                && l.split('=').next().map(str::trim) == Some("node-linker")
        })
    });
    if npmrc_sets {
        return true;
    }
    ["aube-workspace.yaml", "pnpm-workspace.yaml"]
        .iter()
        .any(|f| {
            std::fs::read_to_string(dir.join(f))
                .is_ok_and(|s| s.lines().any(|l| l.trim_start().starts_with("nodeLinker:")))
        })
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

    #[test]
    fn node_linker_policy_defaults_hoisted_only_for_flat_ecosystem_lockfiles() {
        let dir = tempfile::tempdir().unwrap();
        let detected = |kind| DetectedLockfile {
            kind,
            dir: dir.path().to_path_buf(),
        };

        // npm / yarn / bun kinds ⇒ hoisted default lands in the bag.
        for kind in [
            LockfileKind::Npm,
            LockfileKind::YarnBerry,
            LockfileKind::Bun,
        ] {
            let mut bag = Vec::new();
            apply_node_linker_policy(&mut bag, Some(&detected(kind)));
            assert_eq!(
                bag,
                vec![("node-linker".to_string(), "hoisted".to_string())],
                "{kind:?} must default to the hoisted layout"
            );
        }

        // pnpm-shaped kinds and no lockfile ⇒ policy stays silent (isolated default).
        for kind in [LockfileKind::Pnpm, LockfileKind::Aube] {
            let mut bag = Vec::new();
            apply_node_linker_policy(&mut bag, Some(&detected(kind)));
            assert!(bag.is_empty(), "{kind:?} must not inject a node-linker");
        }
        let mut bag = Vec::new();
        apply_node_linker_policy(&mut bag, None);
        assert!(bag.is_empty(), "no lockfile must not inject a node-linker");
    }

    #[test]
    fn explicit_flag_and_project_settings_beat_the_layout_policy() {
        let dir = tempfile::tempdir().unwrap();
        let detected = DetectedLockfile {
            kind: LockfileKind::Npm,
            dir: dir.path().to_path_buf(),
        };

        // Explicit --node-linker already in the bag ⇒ no policy entry appended.
        let mut bag = vec![("node-linker".to_string(), "isolated".to_string())];
        apply_node_linker_policy(&mut bag, Some(&detected));
        assert_eq!(
            bag.len(),
            1,
            "an explicit --node-linker must win over the policy"
        );

        // Project .npmrc node-linker ⇒ policy stays out of the way.
        std::fs::write(dir.path().join(".npmrc"), "node-linker = isolated\n").unwrap();
        let mut bag = Vec::new();
        apply_node_linker_policy(&mut bag, Some(&detected));
        assert!(
            bag.is_empty(),
            ".npmrc node-linker must disable the policy default"
        );
        std::fs::remove_file(dir.path().join(".npmrc")).unwrap();

        // Workspace yaml nodeLinker ⇒ same.
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "nodeLinker: isolated\n",
        )
        .unwrap();
        let mut bag = Vec::new();
        apply_node_linker_policy(&mut bag, Some(&detected));
        assert!(
            bag.is_empty(),
            "workspace nodeLinker must disable the policy default"
        );
    }
}
