//! Install family — dependency-graph mutation and linking through the
//! embedded aube engine. `nub install` / `nub ci` are live (slice 2); the
//! rest of the family (`add`, `remove`, `update`, `link`, `patch*`, …) is
//! registered in [`super::ENGINE_VERBS`] and stubbed pending the Surface
//! phase.
//!
//! The live dispatch shape mirrors aube's own CLI entry (`vendor/aube/
//! crates/aube/src/lib.rs::run_install_command`): build the CLI flag bag,
//! resolve the frozen mode, construct `InstallOptions`, run
//! `commands::install::run` on the shared [`super::EngineSession`] runtime.
//! The nub layer sits on top through the engine's embedder seams
//! (`super::engine_preflight`, called by `engine_session`) plus one hard
//! gate:
//!
//! 1. **Env families** — only the npm-compatible (`npm_config_*`) and
//!    ecosystem-neutral (`CI`, proxies, …) env surfaces are consulted;
//!    `AUBE_*` variables are invisible. Nub's contract is "configure me the
//!    way you configure npm/pnpm"; honoring another tool's branded env vars
//!    would create an accidental config surface.
//! 2. **Embedder defaults** (set-unless-user-set; CLI flags, env vars, and
//!    every config file all win over these) — see
//!    [`super::nub_setting_defaults`]: `defaultLockfileFormat=pnpm`,
//!    `virtualStoreDir`/`stateDir=node_modules/.nub`, `storeDir` under
//!    nub's XDG data namespace (`cacheDir` cannot ride this tier at the
//!    pinned API — see the KNOWN GAP note on `nub_setting_defaults`), and
//!    the lockfile-derived `nodeLinker` layout policy. (The state
//!    *basename* `.aube-state` is a fixed constant in the engine, so the
//!    one aube-named path that survives is `node_modules/.nub/.aube-state`.)
//! 3. **Yarn write gate** — aube's yarn.lock *write* fidelity is unproven, so
//!    any install that would mutate a detected `yarn.lock` (classic or berry)
//!    is refused. Frozen-satisfiable installs proceed (the lockfile-read path
//!    never rewrites the lockfile; only a re-resolve writes).
//!
//! Engine failures flow through [`super::present`]: rendered with the brand
//! rewrite, exit code mapped via the engine's own exit table.
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
//! - The engine's node-gyp shim re-invokes `current_exe()` (i.e. the nub
//!   binary) as `<exe> __node-gyp-bootstrap <dir>`; that verb is not wired
//!   (the engine entry `commands::install::node_gyp_bootstrap` is
//!   crate-private at the pinned API), so an allowlisted node-gyp dependency
//!   build fails at the shim. Fill-in needs a one-line upstreamable fork
//!   export of `print_bootstrapped_binary`.

use std::path::Path;

use anyhow::Result;
use aube::commands::install::{DepSelection, FrozenMode, InstallArgs, InstallOptions};
use aube_lockfile::LockfileKind;

use super::{EngineSession, VerbSpec, present, stub_error};

/// Stub dispatcher for the family's registered-but-unwired verbs (`add`,
/// `remove`, `update`, …). `install`/`ci` never arrive here — they are clap
/// verbs in cli.rs dispatching to [`run_install`] / [`run_ci`] directly.
/// Filling in a verb means: parse `args` with the spec's aube args type
/// (`clap::Parser::parse_from`), build an [`super::EngineSession`], call the
/// corresponding `aube::commands::*::run` on `session.runtime`, and route
/// failures through `present::emit_report`.
pub(crate) fn run_verb(
    _spec: &'static VerbSpec,
    typed: &str,
    args: &[String],
    pm_hint: &str,
) -> Result<i32> {
    Err(stub_error(typed, args, pm_hint))
}

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
    pub dir: Option<std::path::PathBuf>,
}

/// `nub ci` flags. `ci` is frozen + clean by definition, so only the script /
/// optional-dep knobs are configurable (mirrors `aube ci`'s `CiArgs`).
#[derive(Debug, Default)]
pub struct CiFlags {
    pub ignore_scripts: bool,
    pub no_optional: bool,
    pub dir: Option<std::path::PathBuf>,
}

/// `nub install` — route through the embedded aube install engine.
pub fn run_install(flags: InstallFlags) -> Result<i32> {
    let session = super::engine_session(flags.dir.as_deref())?;

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
    let mut opts = args.into_options(global_frozen, None, cli_flags, super::env_snapshot());

    let yarn = matches!(
        session.detected.as_ref().map(|d| d.kind),
        Some(LockfileKind::Yarn | LockfileKind::YarnBerry)
    );
    if yarn {
        let dir = &session
            .detected
            .as_ref()
            .expect("yarn implies detection")
            .dir;
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

    run_engine(&session, opts, yarn)
}

/// `nub ci` — frozen + clean install, npm-ci semantics. Constructed at the
/// nub layer as a field-for-field mirror of `aube ci`
/// (`vendor/aube/crates/aube/src/commands/ci.rs`) rather than calling
/// `commands::ci::run`. (Historical reason: the ci entry point's empty
/// `cli_flags` bag left no channel for the layout policy, which then rode the
/// CLI tier. The policy now rides the embedder-defaults tier, so the mirror
/// persists to keep the nub-side yarn drift pre-flight and the clean step
/// explicit — `commands::ci::run` would be equivalent otherwise.) Semantics
/// shipped: delete `node_modules`, then install with `FrozenMode::Frozen` +
/// `strict_no_lockfile` (drift or no lockfile ⇒ hard error), root lifecycle
/// hooks on unless `--ignore-scripts`.
pub fn run_ci(flags: CiFlags) -> Result<i32> {
    let session = super::engine_session(flags.dir.as_deref())?;

    // Clean first, like `aube ci` / `npm ci`. The project root for nub's
    // purposes is where the lockfile lives (fall back to cwd for the
    // no-lockfile case — the strict install below errors before linking).
    // Approximation: assumes the default `node_modules` modulesDir name.
    let root = match session.detected.as_ref() {
        Some(d) => d.dir.clone(),
        None => std::env::current_dir()?,
    };
    remove_node_modules(&root.join("node_modules"))?;

    let opts = InstallOptions {
        mode: FrozenMode::Frozen,
        dep_selection: DepSelection::from_flags(false, false, flags.no_optional),
        ignore_scripts: flags.ignore_scripts,
        strict_no_lockfile: true,
        cli_flags: Vec::new(),
        env_snapshot: super::env_snapshot(),
        // `nub ci` is the argumentless-install shape: root lifecycle hooks run.
        skip_root_lifecycle: false,
        ..InstallOptions::with_mode(FrozenMode::Frozen)
    };

    let yarn = matches!(
        session.detected.as_ref().map(|d| d.kind),
        Some(LockfileKind::Yarn | LockfileKind::YarnBerry)
    );
    if yarn {
        // `nub ci` never writes the lockfile (strict frozen), but the engine's
        // frozen drift check is blind to yarn formats (see yarn_drift_reason)
        // — a drifted yarn.lock would under-install and exit 0. `ci` means
        // "the lockfile is law", so surface the drift as the gate error.
        let dir = &session
            .detected
            .as_ref()
            .expect("yarn implies detection")
            .dir;
        if let Some(reason) = yarn_drift_reason(dir) {
            return Err(yarn_gate_error(&format!(
                "yarn.lock is out of date ({reason})"
            )));
        }
    }
    run_engine(&session, opts, yarn)
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

/// Run the install on the session runtime, route failures through the
/// presentation layer. `yarn_gated` switches the frozen-drift failure to the
/// yarn write-gate message.
fn run_engine(session: &EngineSession, opts: InstallOptions, yarn_gated: bool) -> Result<i32> {
    let result = session.runtime.block_on(aube::commands::install::run(opts));
    match result {
        Ok(()) => Ok(0),
        // Frozen-drift on a gated yarn project: the install *would* rewrite
        // yarn.lock if allowed to re-resolve. Surface the gate, not aube's
        // "run without --frozen-lockfile" hint (which would punch through it).
        // KNOWN GAP: substring match — the drift errors carry no stable
        // diagnostic code at the pinned API (see module doc).
        Err(report) if yarn_gated && report.to_string().contains("lockfile is out of date") => Err(
            yarn_gate_error(&format!("yarn.lock is out of date ({report})")),
        ),
        // Everything else: render with the brand rewrite, exit with the
        // engine's own code for the diagnostic (EXIT_TABLE; generic 1
        // fallback) — matching aube's own cli_main behavior.
        Err(report) => Ok(present::emit_report(&report)),
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

/// Symlink-aware `node_modules` removal, mirroring `aube ci`'s
/// `remove_existing`: a symlinked node_modules is unlinked (not followed —
/// `remove_dir_all` on a symlink-to-dir would wipe the *target*).
fn remove_node_modules(nm: &Path) -> Result<()> {
    use anyhow::Context as _;
    let Ok(meta) = nm.symlink_metadata() else {
        return Ok(()); // nothing to remove
    };
    present::info("Removing existing node_modules...");
    if meta.file_type().is_symlink() {
        std::fs::remove_file(nm)
    } else {
        std::fs::remove_dir_all(nm)
    }
    .with_context(|| format!("failed to remove {}", nm.display()))
}
