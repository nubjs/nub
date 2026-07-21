//! Install family — dependency-graph mutation and linking through the
//! embedded aube engine. Live verbs: `nub install` / `nub ci` (clap natives,
//! slice 2) plus the registry-dispatched `add`, `remove`, `update`, `link`,
//! `unlink`, `import`, `prune`, `dedupe`, `rebuild`, `fetch`,
//! `approve-builds`, `ignored-builds`, `dlx`, `create` (the dlx sugar), and
//! the patch workflow (`patch`, `patch-commit`, `patch-remove`).
//!
//! Deliberately excluded (honest per-verb errors in [`run_verb`], not
//! backlog): `recursive` (no meta-verb — its nested legs overlap nub's
//! reserved runner surface; the per-verb `-r`/`--filter` flags cover the
//! fanout), `clean`/`purge` (nub doesn't delete node_modules for you, and
//! their script-override semantics delegate to the *engine's* `run`,
//! colliding with the reserved script runner), `deploy` (not yet wired).
//! `init` is excluded one level up — it's nub-reserved and never enters the
//! engine registry (pm_engine module doc).
//!
//! # Wiring shape (registry verbs)
//!
//! Each verb parses its argv with **aube's own clap `Args` type** (full
//! upstream flag fidelity, zero hand-mirrored structs) plus
//! [`EngineGlobals`] — the subset of aube's global flags nub honors at the
//! verb position (`-C/--dir`, `-r`, `-F/--filter`, `--filter-prod`,
//! `--fail-if-no-match`, `--include-workspace-root`). It then builds the
//! shared [`super::EngineSession`] (chdir → lockfile detection → embedder
//! preflight → runtime), applies the write-tier policy, runs the
//! `aube::commands::<verb>::run` entry on the session runtime, and routes
//! every failure through [`super::present`] (brand rewrite + the engine's
//! own exit-code table). `--help` and usage errors are settled by clap at
//! the nub layer; the rendered text also flows through the rewrite.
//!
//! # Write-tier policy (yarn gate)
//!
//! aube's yarn.lock *write* fidelity is unproven, so anything that would
//! mutate `yarn.lock` (classic or berry) is refused. The gate keys on the
//! session's RESOLVED identity (declared-first, see
//! `super::resolve_identity_walk_up`), so it also covers the declared-yarn
//! project with no yarn.lock yet — there a first install would *create*
//! the gated file, refused in `run_install` / the patch chained-install
//! pre-flight: `install`/`ci` gate on drift or explicit-rewrite flags
//! (frozen-satisfiable installs proceed — reads never rewrite);
//! `add`/`remove`/`update` re-resolve by definition and are refused
//! outright (their `--global` forms never touch the project lockfile and
//! proceed); `dedupe` is refused except `--check` (which writes nothing);
//! `patch-commit`/`patch-remove` chain into a prefer-frozen install that
//! only rewrites on drift, so they gate exactly like `install` does
//! (drifted yarn.lock ⇒ refuse; satisfiable ⇒ proceed, no write). The
//! non-lockfile-writing verbs (`prune`, `rebuild`, `fetch`, `link`,
//! `unlink`, `approve-builds`, `ignored-builds`, `dlx`, `import`, `patch`)
//! are not gated — `import` *reads* yarn.lock and writes nub's own
//! pnpm-lock.yaml (and deliberately skips the session's identity errors:
//! it must keep working in the contradicted states it cleans up).
//!
//! # nub-side divergences from aube's dispatch (each deliberate)
//!
//! - **`import` is reimplemented at the nub layer** over aube-lockfile's
//!   public API (`parse_for_import` + `write_lockfile_as(Pnpm)`): upstream's
//!   `commands::import::run` hardcodes `aube-lock.yaml` as the target, which
//!   would drop an aube-branded lockfile into the user's project and print
//!   its name. nub's canonical lockfile is `pnpm-lock.yaml`
//!   (`defaultLockfileFormat=pnpm`), so `nub import` converts *to pnpm*,
//!   like `pnpm import`. (Upstream fork item: an import target-format knob
//!   honoring `defaultLockfileFormat` would let this fold back into the
//!   engine entry.) Approximations vs upstream: no project-file lock during
//!   the write (`take_project_lock` is crate-private; the write itself is
//!   atomic), and the manifest root is a bounded package.json walk-up.
//! - **`update --depth` is intercepted** (cleared before dispatch, nub-side
//!   warning emitted): the engine's own warning names aube and
//!   `aube-lock.yaml` via a raw `eprintln!` the presentation layer can't
//!   reach.
//! - **`dlx` with no command / leading `--help`** renders nub-side help: the
//!   engine's internal help path prints aube's own CLI help (the trailing
//!   var-arg swallows `--help` before clap can settle it).
//! - **`approve-builds`/`patch` stdout, `unlink`/`prune` stderr are
//!   fd-captured** ([`super::with_fd_captured`]) and re-emitted through the
//!   rewrite: those verbs print branded hint lines (`` Run `aube
//!   install`… ``, `…from .aube,…`, `` run "aube patch-commit …" ``) via
//!   raw `println!`/`eprintln!` that bypass the report path. Capture is
//!   safe exactly there: no child processes, no progress UI, and the
//!   interactive picker prompts on the *other* stream. On non-unix the
//!   capture is a documented no-op (see the cfg fallback).
//! - **`patch` defaults its edit dir at the nub layer** (`nub-patch-…`
//!   under the system tmpdir): unlike `dlx`'s never-printed scratch dir,
//!   the engine's `aube-patch-…` fallback path is *printed* in the success
//!   message, and the rewrite policy preserves on-disk names — so nub names
//!   the directory itself instead of letting an engine-branded path become
//!   user-facing output. An explicit `--edit-dir` is honored unchanged.
//!
//! # KNOWN GAPS / residuals (documented, deliberate)
//!
//! - On **Windows** the fd capture is a no-op, so `approve-builds`' final
//!   hint line still reads `aube install` and `prune`'s summary still says
//!   `.aube` there. Root fix is fork-side (derive the printed tool name /
//!   store label from the embedder identity — upstreamable as multicall
//!   correctness), tracked with the brand-toggle fork items.
//! - `link`/`unlink -g` use the engine's global-links registry under
//!   `<XDG_CACHE_HOME>/aube/global-links` (`global_links_dir()` derives from
//!   the leaf-fixed `aube_store::dirs::cache_dir()` — literally the
//!   `cacheDir` gap in `super::nub_setting_defaults`).
//!   Printed paths name that directory truthfully; the rewrite preserves
//!   on-disk names by design.
//! - `dlx` propagates the child's exit code via `std::process::exit`
//!   inside the engine (no return through nub's exit path), and its scratch
//!   project uses the engine's `aube-dlx-*` tempdir prefix + `aube-dlx`
//!   manifest name (on-disk temp state, never printed on success).
//! - `approve-builds` writes the policy through the engine's workspace-yaml
//!   selection (fork toggles decide the created filename / package.json
//!   namespace — not this layer's concern).
//! - (resolved) The engine's node-gyp shim re-invokes `current_exe()` as
//!   `<exe> __node-gyp-bootstrap <dir>`; the fork exports the entry point
//!   and cli.rs intercepts the verb → `pm_engine::run_node_gyp_bootstrap`.
//! - (resolved) `patch-commit`'s binary-file skip warning is a raw eprintln
//!   the rewrite can't reach (the verb chains into a real install, so
//!   stderr fd-capture is unsafe there); the fork now prints
//!   `ua::product_name()` in it (vendor 781ac4e), so it reads `nub can't
//!   diff binary files`. The `.aube_patch_state.json` sidecar inside the
//!   edit parent is on-disk temp state, never printed.
//!
//! KNOWN APPROXIMATIONS (install/ci, from slice 2):
//! - `preferFrozenLockfile` from `.npmrc` / workspace yaml is not consulted
//!   when defaulting the frozen mode (aube's `FileSources` is crate-private
//!   at the pinned API); without a CLI flag the mode falls back to aube's
//!   env-aware default (CI ⇒ frozen, else prefer-frozen).
//! - (resolved) The yarn gate now maps aube's frozen-drift failure by its
//!   stable `ERR_AUBE_OUTDATED_LOCKFILE` diagnostic code; the old message
//!   substring backstop is gone.

use std::path::{Path, PathBuf};

use anyhow::Result;
use aube::commands::install::{DepSelection, FrozenMode, InstallArgs, InstallOptions};
use aube_lockfile::LockfileKind;
use aube_workspace::selector::EffectiveFilter;
use clap::{Args as ClapArgs, FromArgMatches as _};
use miette::{IntoDiagnostic as _, WrapErr as _, miette};

use super::{EngineSession, VerbSpec, present, stub_error};

/// Dispatcher for the family's registry verbs. `install`/`ci` never arrive
/// here — they are clap verbs in cli.rs dispatching to [`run_install`] /
/// [`run_ci`] directly. The arms not yet wired fall through to the shared
/// stub error (verb + real-PM fallback).
pub(crate) fn run_verb(
    spec: &'static VerbSpec,
    typed: &str,
    args: &[String],
    pm_hint: &str,
) -> Result<i32> {
    match spec.canonical {
        "add" => run_add(typed, args),
        "remove" => run_remove(typed, args),
        "update" => run_update(typed, args),
        "import" => run_import(typed, args),
        "dedupe" => run_dedupe(typed, args),
        "prune" => run_prune(typed, args),
        "rebuild" => run_rebuild(typed, args),
        "fetch" => run_fetch(typed, args),
        "link" => run_link(typed, args),
        "unlink" => run_unlink(typed, args),
        "approve-builds" => run_approve_builds(typed, args),
        "ignored-builds" => run_ignored_builds(typed, args),
        "dlx" => run_dlx(typed, args),
        "create" => run_create(typed, args),
        "patch" => run_patch(typed, args),
        "patch-commit" => run_patch_commit(typed, args),
        "patch-remove" => run_patch_remove(typed, args),
        // Deliberate exclusions — each errors with an honest per-verb
        // message instead of dispatching (module doc has the reasons).
        "recursive" => Err(anyhow::anyhow!(
            "nub {typed}: not supported — nub has no recursive meta-verb.\n\
             \x20\x20Use the verb's own workspace flags instead: `nub -r <verb>` /\n\
             \x20\x20`nub <verb> -r` or `--filter <pattern>` (e.g. `nub run -r build`,\n\
             \x20\x20`nub update -r`)."
        )),
        "clean" | "purge" => Err(anyhow::anyhow!(
            "nub {typed}: not supported — nub does not delete node_modules for you.\n\
             \x20\x20Remove it directly (`rm -rf node_modules`) and reinstall with\n\
             \x20\x20`nub install`; `nub ci` does the clean + frozen install in one step."
        )),
        "deploy" => Err(anyhow::anyhow!(
            "nub {typed}: not yet supported — the engine's deploy (copy a workspace\n\
             \x20\x20package + its production deps into a self-contained directory) hasn't\n\
             \x20\x20been wired. For now: pnpm deploy"
        )),
        _ => Err(stub_error(typed, args, pm_hint)),
    }
}

// ───────────────────────── parse plumbing ──────────────────────────

// The subset of aube's *global* clap flags nub honors on engine verbs,
// parsed at the verb position (nub has no pre-verb engine flag surface).
// Spellings mirror `vendor/aube/crates/aube/src/lib.rs::Cli` exactly.
// Deliberately absent: `--workspace-root` (aube chdirs to the workspace
// root pre-dispatch; the helper is crate-private, and half-honoring the
// flag as filter-only would silently run against the wrong directory —
// the verb-level `-w/--workspace` on `add`/`remove` covers the use case),
// and the output/diag flags (`--loglevel`, `--reporter`, `--diag*`, …)
// which belong to a later output-integration slice.
// (Plain `//` comments: a rustdoc comment on a clap `Args` struct becomes
// the augmented command's `--help` about-text, clobbering the verb's own.)
#[derive(Debug, Default, clap::Args)]
struct EngineGlobals {
    /// Change to directory before running (like `make -C`)
    #[arg(short = 'C', long = "dir", visible_aliases = ["cd", "prefix"], value_name = "DIR")]
    dir: Option<PathBuf>,
    /// Scope to workspace packages matching PATTERN (repeatable)
    #[arg(short = 'F', long, value_name = "PATTERN")]
    filter: Vec<String>,
    /// Production-only variant of --filter
    #[arg(long, value_name = "PATTERN")]
    filter_prod: Vec<String>,
    /// Run across every workspace package (same as --filter=*)
    #[arg(short = 'r', long)]
    recursive: bool,
    /// Error when a workspace selector matches no packages
    #[arg(long)]
    fail_if_no_match: bool,
    /// Include the workspace root in recursive operations
    #[arg(long, hide = true)]
    include_workspace_root: bool,

    /// Output-verbosity flags (`--reporter`, `--silent`/`-s`, `--loglevel`),
    /// forwarded to the engine's text-mode renderers.
    #[command(flatten)]
    output: super::output::OutputFlags,
}

impl EngineGlobals {
    /// Mirror of aube's `compute_effective_filter`: `-r` is sugar for
    /// `--filter=*` and a no-op when an explicit selector is present.
    fn effective_filter(&self) -> EffectiveFilter {
        let mut filters = self.filter.clone();
        if self.recursive && filters.is_empty() && self.filter_prod.is_empty() {
            filters.push("*".to_string());
        }
        EffectiveFilter {
            filters,
            filter_prods: self.filter_prod.clone(),
            fail_if_no_match: self.fail_if_no_match,
            include_workspace_root: self.include_workspace_root,
        }
    }
}

/// A settled parse: either the verb's args (run it) or a final exit code
/// (clap already printed help / a usage error, through the rewrite).
enum ParsedVerb<A> {
    Run(EngineGlobals, A),
    Done(i32),
}

/// The clap `Command` for one engine verb: aube's own args type augmented
/// with [`EngineGlobals`]. Built by hand (no derive-level `Parser` wrapper)
/// so the command name can carry the user's spelling — usage and errors
/// read `nub add …`, never the engine's name.
fn verb_command<A: ClapArgs>(typed: &str) -> clap::Command {
    EngineGlobals::augment_args(A::augment_args(clap::Command::new(format!("nub {typed}"))))
}

/// Parse one verb's argv with aube's args type + the nub globals. Help and
/// usage errors are rendered through the help-grade rewrite (brand pass +
/// the config-vocabulary map — help describes nub's configured contract,
/// see [`present::rewrite_help`]): help → stdout exit 0; usage error →
/// stderr exit [`aube_codes::exit::EXIT_CLI_USAGE`], matching the engine's
/// own exit table.
fn parse_verb<A: ClapArgs>(typed: &str, args: &[String]) -> Result<ParsedVerb<A>> {
    let argv = std::iter::once(format!("nub {typed}")).chain(args.iter().cloned());
    match verb_command::<A>(typed).try_get_matches_from(argv) {
        Ok(matches) => Ok(ParsedVerb::Run(
            EngineGlobals::from_arg_matches(&matches)?,
            A::from_arg_matches(&matches)?,
        )),
        Err(err) => {
            let text = present::rewrite_help(err.render().to_string().trim_end());
            if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                println!("{text}");
                Ok(ParsedVerb::Done(0))
            } else {
                eprintln!("{text}");
                Ok(ParsedVerb::Done(aube_codes::exit::EXIT_CLI_USAGE))
            }
        }
    }
}

/// Sugar: early-return the settled exit code from a [`parse_verb`] call.
macro_rules! parse_or_return {
    ($typed:expr, $args:expr) => {
        match parse_verb($typed, $args)? {
            ParsedVerb::Run(globals, verb) => (globals, verb),
            ParsedVerb::Done(code) => return Ok(code),
        }
    };
}

/// Apply the forwarded output flags (`--reporter`/`--silent`/`--loglevel`) for
/// the duration of `f`, then restore. The returned [`output::OutputGuard`] is
/// held only across `f` (the engine run) and dropped before the caller's
/// `finish`/`finish_code` — so under `--silent` the progress/summary written
/// during the run is suppressed while a final error report still reaches the
/// real stderr. The progress-mode and log-level side effects persist (harmless
/// once the command is done).
fn with_output<T>(output: &super::output::OutputFlags, f: impl FnOnce() -> T) -> T {
    let _guard = output.apply();
    f()
}

/// Run an engine future to completion under the forwarded output flags, then
/// map its result to nub's exit contract via [`finish`]. The output guard is
/// dropped before `finish`, so errors print even under `--silent`.
fn finish_quieted(
    output: &super::output::OutputFlags,
    session: &EngineSession,
    fut: impl std::future::Future<Output = miette::Result<()>>,
) -> Result<i32> {
    finish(with_output(output, || session.runtime.block_on(fut)))
}

/// [`finish_quieted`] for engine verbs that return an explicit exit code.
fn finish_code_quieted(
    output: &super::output::OutputFlags,
    session: &EngineSession,
    fut: impl std::future::Future<Output = miette::Result<Option<i32>>>,
) -> Result<i32> {
    finish_code(with_output(output, || session.runtime.block_on(fut)))
}

/// Map an engine result to nub's exit contract: success → 0, failure →
/// rendered through the presentation layer + the engine's exit table.
fn finish(result: miette::Result<()>) -> Result<i32> {
    match result {
        Ok(()) => Ok(0),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

/// Same exit contract as [`finish`], for engine verbs that return an
/// explicit exit code (`process-exit-sweep`): `Some(code)` is the engine's
/// chosen code, `None` is plain success (0), `Err` renders via the
/// presentation layer + the engine's exit table.
fn finish_code(result: miette::Result<Option<i32>>) -> Result<i32> {
    match result {
        Ok(code) => Ok(code.unwrap_or(0)),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

// ───────────────────────── wired verbs ──────────────────────────

fn run_add(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::add::AddArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    if !verb.global && yarn_detected(&session) {
        return Err(yarn_gate_error(
            typed,
            "adding a dependency re-resolves and rewrites yarn.lock",
            &yarn_remedy("add", &verb.packages),
        ));
    }
    super::min_release_age::arm();
    let code = finish_quieted(
        &globals.output,
        &session,
        aube::commands::add::run(verb, globals.effective_filter()),
    )?;
    super::min_release_age::persist(&session.cwd, code == 0, &globals.output);
    stamp_if_virgin(&session, code);
    // `nub add vite` (or adding any dep to a vite project) changes the graph;
    // refresh `.modules.yaml` + the < 8.1 patch. Note: a FIRST `nub add vite`
    // can't have disk-materialized vite yet (the manifest didn't declare it at
    // settings time), so Unit A applies immediately (≥ 8.1) and the < 8.1 dist
    // patch lands on the next `nub install` once vite is a declared direct dep.
    apply_vite_compat(&session, code);
    Ok(code)
}

fn run_remove(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::remove::RemoveArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    if !verb.global && yarn_detected(&session) {
        return Err(yarn_gate_error(
            typed,
            "removing a dependency rewrites yarn.lock",
            &yarn_remedy("remove", &verb.packages),
        ));
    }
    let code = finish_quieted(
        &globals.output,
        &session,
        aube::commands::remove::run(verb, globals.effective_filter()),
    )?;
    stamp_if_virgin(&session, code);
    Ok(code)
}

fn run_update(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, mut verb): (_, aube::commands::update::UpdateArgs) =
        parse_or_return!(typed, args);
    // Intercept `--depth` (engine parity no-op): the engine's own warning
    // names aube + aube-lock.yaml via a raw eprintln the rewrite can't
    // reach. Same semantics, nub's wording.
    if let Some(depth) = verb.depth.take() {
        present::warn(&format!(
            "warn: --depth {depth} is ignored; nub only refreshes direct deps. \
             For a full refresh, delete the lockfile and run `nub install`."
        ));
    }
    let session = super::engine_session(globals.dir.as_deref())?;
    if !verb.global && yarn_detected(&session) {
        return Err(yarn_gate_error(
            typed,
            "updating dependencies re-resolves and rewrites yarn.lock",
            &yarn_remedy("upgrade", &verb.packages),
        ));
    }
    super::min_release_age::arm();
    let code = finish_code_quieted(
        &globals.output,
        &session,
        aube::commands::update::run(verb, globals.effective_filter()),
    )?;
    super::min_release_age::persist(&session.cwd, code == 0, &globals.output);
    stamp_if_virgin(&session, code);
    // `nub update` re-resolves; a vite bump can cross the 8.1 boundary, so
    // refresh `.modules.yaml` + the < 8.1 patch to match the new graph.
    apply_vite_compat(&session, code);
    Ok(code)
}

fn run_dedupe(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::dedupe::DedupeArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    // `--check` writes nothing (diff + exit code only) and stays usable on
    // yarn projects; a real dedupe re-resolves and rewrites the lockfile.
    if !verb.check && yarn_detected(&session) {
        return Err(yarn_gate_error(
            typed,
            "deduping re-resolves and rewrites yarn.lock",
            "yarn dedupe",
        ));
    }
    super::min_release_age::arm();
    let code = finish_quieted(&globals.output, &session, aube::commands::dedupe::run(verb))?;
    super::min_release_age::persist(&session.cwd, code == 0, &globals.output);
    Ok(code)
}

fn run_prune(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::prune::PruneArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    // prune prints its summary via raw eprintln with a hardcoded `.aube`
    // store label (the walked directory is the *resolved* virtualStoreDir —
    // node_modules/.store here — only the label lies). Capture + neutralize.
    // Under `--silent` the output guard redirects stderr, so the rebranded
    // reprint is suppressed too; the guard drops before `finish`.
    let result = with_output(&globals.output, || {
        let (result, captured) = super::with_fd_captured(2, || {
            session.runtime.block_on(aube::commands::prune::run(verb))
        });
        eprint!(
            "{}",
            present::rewrite(&captured).replace(" from .aube,", " from the virtual store,")
        );
        result
    });
    finish(result)
}

fn run_rebuild(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::rebuild::RebuildArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    finish_quieted(
        &globals.output,
        &session,
        aube::commands::rebuild::run(verb, globals.effective_filter()),
    )
}

fn run_fetch(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::fetch::FetchArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    finish_quieted(&globals.output, &session, aube::commands::fetch::run(verb))
}

fn run_link(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::link::LinkArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    finish_quieted(&globals.output, &session, aube::commands::link::run(verb))
}

fn run_unlink(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::unlink::UnlinkArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    // The unlink-all path ends with a raw `` Run `aube install` … `` hint
    // on stderr; capture + rewrite (no children, no progress UI here). Under
    // `--silent` the output guard redirects stderr, suppressing the reprint.
    let result = with_output(&globals.output, || {
        let (result, captured) = super::with_fd_captured(2, || {
            session.runtime.block_on(aube::commands::unlink::run(verb))
        });
        eprint!("{}", present::rewrite(&captured));
        result
    });
    finish(result)
}

fn run_approve_builds(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::approve_builds::ApproveBuildsArgs) =
        parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    // No fd capture: the engine's remaining prints brand through
    // `prog()`/`cmd()` (already profile-aware), and the verb now runs the
    // approved packages' build scripts in-process — buffering fd 1 for the
    // whole call would hold back live script output until the end, reading
    // as a hang during long native builds. Same shape as `run_rebuild`.
    finish_quieted(
        &globals.output,
        &session,
        aube::commands::approve_builds::run(verb),
    )
}

fn run_ignored_builds(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::ignored_builds::IgnoredBuildsArgs) =
        parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    finish_quieted(
        &globals.output,
        &session,
        aube::commands::ignored_builds::run(verb),
    )
}

fn run_dlx(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::dlx::DlxArgs) = parse_or_return!(typed, args);
    // Bare `nub dlx` / leading `--help`: the trailing var-arg swallowed the
    // flag, and the engine's internal help path would print *aube's* CLI
    // help. Render nub's own (the same surface we just parsed), rewritten.
    if verb.package.is_empty()
        && matches!(
            verb.params.first().map(String::as_str),
            None | Some("--help" | "-h")
        )
    {
        let help = verb_command::<aube::commands::dlx::DlxArgs>(typed).render_long_help();
        println!("{}", present::rewrite_help(help.to_string().trim_end()));
        return Ok(0);
    }
    // Transient fetch-and-run (see `engine_session_transient`): a dlx never
    // touches the CWD project's lockfile, so a multi-lockfile project must not
    // hard-error on identity ambiguity — it degrades to no-identity here.
    let session = super::engine_session_transient(globals.dir.as_deref())?;
    // NOTE: on child failure the engine propagates the child's exit code via
    // std::process::exit — control does not return here on that path. Output
    // flags quiet the fetch; the run tool's own output is preserved (the
    // silencer registers the saved fd for child stderr).
    finish_code_quieted(&globals.output, &session, aube::commands::dlx::run(verb))
}

/// DLX fallback for the `nubx <tool> [args]` entry point: the bin was absent
/// from `node_modules/.bin`, so fetch it into a throwaway project and run it,
/// matching `npx` / `pnpm dlx`. Reuses the engine's `dlx` command end-to-end
/// (resolve → install into a scratch tempdir → exec the bin → drop the tempdir)
/// rather than reimplementing the fetch pipeline; the engine itself does a final
/// local-`.bin` recheck (a no-op here since the caller already missed) before
/// fetching, and resolves the project's Node pin via the user's cwd. We follow
/// `pnpm dlx` semantics deliberately: no interactive confirm-prompt — fetch+run.
///
/// `<tool>` is passed as the positional, so by default the engine derives the
/// actual bin name from the installed package's `bin` map when the command name
/// and package name differ (e.g. `@tanstack/cli` ships `tanstack`). nubx's own
/// flag handling already split off the bin; everything in `args` is forwarded to
/// the tool verbatim. The npx flags in `flags` steer the fetch: `-p`/`--package`
/// populates `DlxArgs.package` (the package(s) to fetch, with `<tool>` as the bin
/// to run from them) and `-q`/`--quiet` switches the engine's progress UI to
/// text mode.
/// Returns `(exit_code, fetched_ok)`. `fetched_ok` distinguishes "the tool was
/// resolved, installed, and executed" (engine `Ok` — whatever the tool's own exit
/// code) from "the fetch/install itself failed" (404 / resolution error /
/// binary-not-found — engine `Err`, surfaced here as a nonzero code). The consent
/// caller MUST gate its ledger write on `fetched_ok`: recording a failed fetch
/// would turn a one-time `y` on a not-yet-published spec into a permanent silent
/// run-grant that activates if the name is later (maliciously) published.
pub fn run_dlx_for_nubx(
    bin: &str,
    args: &[String],
    flags: &crate::cli::NubxDlxFlags,
) -> Result<(i32, bool)> {
    if flags.quiet {
        // Same knob aube's own startup flips for `--silent`: drop the animated
        // progress UI to plain text so a `-q` fetch stays quiet.
        clx::progress::set_output(clx::progress::ProgressOutput::Text);
    }
    let verb = nubx_dlx_args(bin, args, flags);
    // Transient fetch-and-run (see `engine_session_transient`): `nubx <tool>`
    // fetches a throwaway package and runs it; the CWD project's lockfile is
    // irrelevant, so a multi-lockfile project must not raise
    // ERR_NUB_LOCKFILE_AMBIGUOUS the way npm/pnpm/bun's npx/dlx/bunx don't.
    let session = super::engine_session_transient(None)?;
    // `Ok` = fetched + ran (the tool's own code via Ok(Some(code)), success via
    // Ok(None)); `Err` = the fetch/install failed before the tool ran. We surface
    // the Err's report exactly as `finish_code` would, but also report the
    // success bit so the consent caller never records a failed fetch.
    match session.runtime.block_on(aube::commands::dlx::run(verb)) {
        Ok(code) => Ok((code.unwrap_or(0), true)),
        Err(report) => Ok((present::emit_report(&report), false)),
    }
}

/// Build the `dlx` invocation for a `nubx <tool> [args]` fallback: `<tool>` is
/// the positional (so the engine derives the actual bin name from the package's
/// `bin` map, or runs it from `-p` packages) and `args` forward verbatim. The
/// dlx-only `-c` shell-mode and `--allow-build` are not in nubx's surface yet, so
/// they stay at their safe defaults — matching `npx <tool> [args]`.
fn nubx_dlx_args(
    bin: &str,
    args: &[String],
    flags: &crate::cli::NubxDlxFlags,
) -> aube::commands::dlx::DlxArgs {
    let mut params = Vec::with_capacity(args.len() + 1);
    params.push(bin.to_string());
    params.extend(args.iter().cloned());
    aube::commands::dlx::DlxArgs {
        params,
        shell_mode: false,
        package: flags.package.clone(),
        allow_build: Vec::new(),
        lockfile: Default::default(),
        network: Default::default(),
        virtual_store: Default::default(),
    }
}

fn run_create(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::create::CreateArgs) = parse_or_return!(typed, args);
    // Bare `nub create` / leading `--help`: the engine's internal help path
    // prints aube's own CLI help (CreateArgs collapses the template into a
    // trailing var-arg with the help flag disabled, so clap never settles
    // it). Render nub's own surface instead, rewritten — same shape as dlx.
    if matches!(
        verb.params.first().map(String::as_str),
        None | Some("--help" | "-h")
    ) {
        let help = verb_command::<aube::commands::create::CreateArgs>(typed).render_long_help();
        println!("{}", present::rewrite_help(help.to_string().trim_end()));
        return Ok(0);
    }
    // Transient: `nub create` chains into dlx (fetch the create-* package and
    // run it), so like dlx it must not hard-error on the CWD project's
    // lockfile ambiguity (see `engine_session_transient`).
    let session = super::engine_session_transient(globals.dir.as_deref())?;
    // The engine maps the template to its create-* package (foo → create-foo,
    // @scope/foo → @scope/create-foo) and chains into dlx; like dlx, on child
    // failure the engine propagates the exit code via std::process::exit.
    finish_code_quieted(&globals.output, &session, aube::commands::create::run(verb))
}

// ───────────────────────── patch workflow ──────────────────────────

fn run_patch(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, mut verb): (_, aube::commands::patch::PatchArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    // Default the edit dir nub-side: the engine's fallback tempdir is
    // `aube-patch-…` and that path IS the success output (module doc).
    verb.edit_dir
        .get_or_insert_with(|| nub_patch_edit_parent(&verb.package));
    // The success message ends with `` run "aube patch-commit '<dir>'" `` via
    // raw println; capture + rewrite (no children, no progress UI here).
    let (result, captured) = super::with_fd_captured(1, || {
        session.runtime.block_on(aube::commands::patch::run(verb))
    });
    print!("{}", present::rewrite(&captured));
    finish(result)
}

/// nub-named default edit parent for `nub patch`, mirroring the engine's
/// `<tmp>/<tool>-patch-<name>-<version>-<pid>/` shape (pid-suffixed so
/// concurrent patches don't collide). The spec is sanitized rather than
/// parsed — an invalid spec errors in the engine with its own diagnostic,
/// and the unused empty dir costs nothing.
fn nub_patch_edit_parent(spec: &str) -> PathBuf {
    let safe: String = spec
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '+'
            }
        })
        .collect();
    std::env::temp_dir().join(format!("nub-patch-{safe}-{}", std::process::id()))
}

fn run_patch_commit(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::patch_commit::PatchCommitArgs) =
        parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    patch_chained_install_yarn_gate(typed, &session)?;
    finish(
        session
            .runtime
            .block_on(aube::commands::patch_commit::run(verb)),
    )
}

fn run_patch_remove(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::patch_remove::PatchRemoveArgs) =
        parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    patch_chained_install_yarn_gate(typed, &session)?;
    finish(
        session
            .runtime
            .block_on(aube::commands::patch_remove::run(verb)),
    )
}

/// `patch-commit` / `patch-remove` end by chaining into a prefer-frozen
/// install. On a satisfiable yarn.lock that install reads without writing;
/// on a drifted one it would re-resolve and rewrite yarn.lock — the same
/// state `nub install` gates on, refused with the same message shape.
fn patch_chained_install_yarn_gate(typed: &str, session: &EngineSession) -> Result<()> {
    if yarn_detected(session) {
        let detected = session.detected.as_ref().expect("yarn implies detection");
        // Declared yarn, no yarn.lock yet: the chained install would create
        // one — the gated write (same guard as run_install's fresh arm).
        if detected.fresh {
            return Err(yarn_gate_error(
                typed,
                "this project declares yarn but has no yarn.lock yet — the chained \
                 install would create it",
                "yarn install",
            ));
        }
        if let Some(reason) = yarn_drift_reason(&detected.dir) {
            return Err(yarn_gate_error(
                typed,
                &format!("the chained install would re-resolve a stale yarn.lock ({reason})"),
                "yarn install",
            ));
        }
    }
    Ok(())
}

// ───────────────────────── import (nub-side) ──────────────────────────

/// `nub import` — convert a foreign lockfile to `pnpm-lock.yaml` (nub's
/// canonical format), like `pnpm import`. Reimplemented over aube-lockfile's
/// public API; see the module doc for why the engine entry is unusable
/// (hardcoded `aube-lock.yaml` target).
fn run_import(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::import::ImportArgs) = parse_or_return!(typed, args);
    // Upstream parity no-ops, kept so wrappers that pass them keep working:
    // import never chains into install (`--ignore-scripts`) and already
    // only writes the lockfile (`--lockfile-only`).
    let _ = (verb.ignore_scripts, verb.lockfile_only);
    // Deliberately NOT engine_session: its identity resolution errors on the
    // contradicted / multi-lockfile states import exists to clean up, and
    // import needs neither the runtime nor the layout policy — just the
    // chdir and the brand seams (registered before any engine read).
    super::apply_dir(globals.dir.as_deref())?;
    super::engine_brand_preflight();
    match import_to_pnpm_lock(verb.force) {
        Ok(summary) => {
            // import writes no progress UI; the only output is this summary,
            // suppressed under `--silent` to match `pnpm import --silent`.
            if !globals.output.is_silent() {
                present::info(&summary);
            }
            Ok(0)
        }
        Err(report) => Ok(present::emit_report(&report)),
    }
}

fn import_to_pnpm_lock(force: bool) -> miette::Result<String> {
    let cwd = std::env::current_dir().into_diagnostic()?;
    let root = find_manifest_root(&cwd).ok_or_else(|| {
        miette!(
            "no package.json found in {} or any parent directory",
            cwd.display()
        )
    })?;
    let manifest = aube_manifest::PackageJson::from_path(&root.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;

    // pnpm-lock.yaml is the *target*, never a source. An existing one is
    // moved aside for the parse (so detection falls through to the foreign
    // formats) and deleted on success / restored on failure — gated on
    // `--force`, mirroring upstream's existence check on its own target.
    let target_name = aube_lockfile::pnpm_lock_filename(&root);
    let target = root.join(&target_name);
    let backup = if target.exists() {
        if !force {
            return Err(miette!(
                "{target_name} already exists\nRemove it first, or pass --force to overwrite"
            ));
        }
        let aside = root.join(format!("{target_name}.import-backup"));
        std::fs::rename(&target, &aside)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to move {target_name} aside"))?;
        Some(aside)
    } else {
        None
    };
    let restore = |aside: &Option<PathBuf>| {
        if let Some(aside) = aside {
            let _ = std::fs::rename(aside, &target);
        }
    };

    let (mut graph, kind) = match aube_lockfile::parse_for_import(&root, &manifest) {
        Ok(pair) => pair,
        Err(aube_lockfile::Error::NotFound(_)) => {
            restore(&backup);
            return Err(miette!(
                "no source lockfile found\n\
                 Expected one of: package-lock.json, npm-shrinkwrap.json, yarn.lock, bun.lock"
            ));
        }
        Err(e) => {
            restore(&backup);
            return Err(miette::Report::new(e)).wrap_err("failed to parse source lockfile");
        }
    };

    // npm/bun lockfiles serialize a flat, pre-hoisted tree with no peer context.
    // A pnpm-lock is expected to carry peer-context suffixes, and the install
    // path deliberately skips its peer pass for pnpm incumbents on exactly that
    // assumption (aube's `apply_lockfile_graph_platform_rules`). Writing a
    // suffix-less pnpm-lock here violates that invariant: under the isolated
    // store layout a peer-dependent package lands with no sibling peer link and
    // dies at runtime with `Cannot find package` (#453). Re-establish the edges
    // with the same pass the install path runs — `hoist_auto_installed_peers`
    // then `apply_peer_contexts`, both pure offline graph transforms (no
    // registry). `filter_graph` is intentionally omitted: an imported lockfile
    // stays cross-platform, so no platform pruning. Yarn is excluded because
    // real `yarn.lock` files don't record per-entry `peerDependencies`, so the
    // pass would be a no-op without a packument fetch — a separate, deeper change.
    //
    // Uses pnpm's default peer options (import has no settings ctx here); those
    // defaults match aube's own settings defaults, so a project without peer-knob
    // overrides gets install-identical suffixes. A project that DOES override a
    // knob (e.g. `dedupe-peers`) won't see it reflected in the import, but a later
    // `nub install` reads this as a pnpm incumbent and skips its own peer pass, so
    // the suffixes are consumed as-is — a fidelity gap, never a runtime break.
    if matches!(
        kind,
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap | LockfileKind::Bun
    ) {
        let (hoisted, auto_installed) = aube_resolver::hoist_auto_installed_peers(graph);
        match aube_resolver::apply_peer_contexts(
            hoisted,
            &aube_resolver::PeerContextOptions::default(),
        ) {
            Ok(contextualized) => {
                graph = contextualized;
                aube_resolver::remove_auto_installed_peers(&mut graph, &auto_installed);
            }
            // Restore a moved-aside pnpm-lock like every other error path here;
            // a bare `?` would orphan the user's original as `.import-backup`.
            Err(e) => {
                restore(&backup);
                return Err(miette!("peer-context pass failed: {e}"));
            }
        }
    }

    match aube_lockfile::write_lockfile_as(&root, &graph, &manifest, LockfileKind::Pnpm) {
        Ok(_) => {
            if let Some(aside) = backup {
                let _ = std::fs::remove_file(aside);
            }
            Ok(format!(
                "Imported {} packages from {} to {target_name}",
                graph.packages.len(),
                kind.filename(),
            ))
        }
        Err(e) => {
            restore(&backup);
            Err(miette::Report::new(e)).wrap_err_with(|| format!("failed to write {target_name}"))
        }
    }
}

/// Nearest ancestor (inclusive) carrying a `package.json`, bounded like
/// `super::detect_lockfile_walk_up`. Approximation of aube's
/// `dirs::project_root` (which is crate-private); the home-dir boundary is
/// not enforced here.
/// Run the Vite symlink-GVS compat post-step ([`super::vite_compat::apply`]) for
/// a successful engine verb (`install`/`add`/`update`). Resolves the root where
/// the install's `node_modules` actually lives: the identity walk-up dir
/// (`detected.dir` — the workspace root for a monorepo member) when IT holds the
/// tree, else the cwd, so a stray ancestor lockfile can't point the writer at a
/// `node_modules`-less parent. `nub ci` (project-local store) does not call this
/// — its store is already under the workspace root, so Vite serves it unchanged.
fn apply_vite_compat(session: &EngineSession, code: i32) {
    if code != 0 {
        return;
    }
    let has_node_modules = |d: &Path| d.join("node_modules").is_dir();
    let root = session
        .detected
        .as_ref()
        .map(|d| d.dir.clone())
        .filter(|d| has_node_modules(d))
        .or_else(|| has_node_modules(&session.cwd).then(|| session.cwd.clone()))
        .or_else(|| find_manifest_root(&session.cwd))
        .unwrap_or_else(|| session.cwd.clone());
    super::vite_compat::apply(&root);
}

fn find_manifest_root(cwd: &Path) -> Option<PathBuf> {
    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        if dir.join("package.json").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

// ───────────────────────── yarn write gate ──────────────────────────

/// Did the session's lockfile walk-up land on a yarn.lock (classic/berry)?
fn yarn_detected(session: &EngineSession) -> bool {
    matches!(
        session.detected.as_ref().map(|d| d.kind),
        Some(LockfileKind::Yarn | LockfileKind::YarnBerry)
    )
}

/// The yarn write gate. See the module doc; the message names the remedy.
fn yarn_gate_error(verb: &str, reason: &str, remedy: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "nub {verb}: refusing to modify yarn.lock — {reason}\n\
         \x20\x20yarn.lock write fidelity is unproven in the embedded engine, so commands\n\
         \x20\x20that would rewrite it are blocked. Run it with yarn directly:\n\
         \x20\x20\x20\x20{remedy}"
    )
}

/// `yarn <verb> <packages…>` remedy line for the gate message.
fn yarn_remedy(yarn_verb: &str, packages: &[String]) -> String {
    std::iter::once(format!("yarn {yarn_verb}"))
        .chain(packages.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

// ───────────────────────── install / ci (slice 2) ──────────────────────────

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
    pub registry: Option<String>,
    pub dir: Option<std::path::PathBuf>,
    /// Workspace selectors (`--filter`/`-r`/…), routed through the same
    /// `EffectiveFilter` path the registry verbs (`add`/`remove`/`update`) use.
    pub filter: WorkspaceFilterFlags,
    /// Output-verbosity flags (`--reporter`/`--silent`/`--loglevel`).
    pub output: super::output::OutputFlags,
}

/// `nub ci` flags. `ci` is frozen + clean by definition, so only the script /
/// optional-dep / registry knobs are configurable (mirrors `aube ci`'s
/// `CiArgs`, whose flattened NetworkArgs carries `--registry` upstream).
#[derive(Debug, Default)]
pub struct CiFlags {
    pub ignore_scripts: bool,
    pub no_optional: bool,
    pub registry: Option<String>,
    pub dir: Option<std::path::PathBuf>,
    /// Workspace selectors (`--filter`/`-r`/…) — same path as `install`.
    pub filter: WorkspaceFilterFlags,
    /// Output-verbosity flags (`--reporter`/`--silent`/`--loglevel`).
    pub output: super::output::OutputFlags,
}

/// The workspace-selection flags nub honors on `install`/`ci`, mirroring the
/// [`EngineGlobals`] subset the registry verbs already expose. Lives here (not
/// in cli.rs) so the [`WorkspaceFilterFlags::effective_filter`] desugaring is
/// one definition the install/ci path shares with the engine-verb path.
#[derive(Debug, Default)]
pub struct WorkspaceFilterFlags {
    pub filter: Vec<String>,
    pub filter_prod: Vec<String>,
    pub recursive: bool,
    pub fail_if_no_match: bool,
    pub include_workspace_root: bool,
}

impl WorkspaceFilterFlags {
    /// Mirror of aube's `compute_effective_filter` (and [`EngineGlobals`]):
    /// `-r` is sugar for `--filter=*`, a no-op when an explicit selector is
    /// already present.
    fn effective_filter(&self) -> EffectiveFilter {
        let mut filters = self.filter.clone();
        if self.recursive && filters.is_empty() && self.filter_prod.is_empty() {
            filters.push("*".to_string());
        }
        EffectiveFilter {
            filters,
            filter_prods: self.filter_prod.clone(),
            fail_if_no_match: self.fail_if_no_match,
            include_workspace_root: self.include_workspace_root,
        }
    }
}

/// `nub install` — route through the embedded aube install engine.
pub fn run_install(flags: InstallFlags) -> Result<i32> {
    let session = super::engine_session(flags.dir.as_deref())?;
    if let Some(err) = pnpm_lockfile_version_preflight(&session) {
        return Err(err);
    }

    // Config-derived install knobs (the IMPLEMENT wins): the active PM's
    // persistent config can pin a dep-axis (`omit`/`production`), request a
    // frozen install (bun `frozenLockfile` / yarn `--immutable`), or disable
    // all build scripts (yarn `enableScripts: false`). Resolved here, applied
    // below — the explicit CLI flag always wins (it's OR'd in, never overridden).
    let config = super::install_config_signals(&session);

    // FAIL LOUD when STRICT offline mode is active AND a yarn `yarn-offline-mirror`
    // is configured: nub can't read a configured mirror directory, so in strict
    // offline mode (where the mirror is the user's intended package source)
    // silently hitting the public registry would diverge. The mirror is moot
    // online, so gate on the effective strict-offline state (yarn
    // `enableNetwork: false` / Berry `--offline`, or the `--offline` CLI flag).
    // `--prefer-offline` is deliberately EXCLUDED: it permits network fallback,
    // so it is not strict offline and must not trip the mirror fatal.
    if let Some(err) = offline_mirror_preflight(&session, flags.offline || config.offline) {
        return Err(err);
    }

    // Mirror `run_install_command`: defaults from clap, nub's flags on top.
    let mut args = default_install_args();
    args.prod = flags.prod || config.dep_selection.prod;
    args.dev = flags.dev || config.dep_selection.dev;
    args.ignore_scripts = flags.ignore_scripts;
    args.no_optional = flags.no_optional || config.dep_selection.no_optional;
    args.offline = flags.offline || config.offline;
    args.prefer_offline = flags.prefer_offline;
    args.lockfile_only = flags.lockfile_only;
    args.force = flags.force;
    args.node_linker = flags.node_linker.clone();
    args.network.registry = flags.registry.clone();
    // Config-requested frozen seeds the strict mode unless the CLI explicitly
    // opted out (`--no-frozen-lockfile`).
    args.lockfile.frozen_lockfile =
        flags.frozen_lockfile || (config.frozen && !flags.no_frozen_lockfile);
    args.lockfile.no_frozen_lockfile = flags.no_frozen_lockfile;
    args.lockfile.prefer_frozen_lockfile = flags.prefer_frozen_lockfile;

    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    let global_frozen = args.lockfile.frozen_override();
    let cli_flags = args.to_cli_flag_bag(global_frozen, args.virtual_store.flags());

    // yaml_prefer_frozen: None — see KNOWN APPROXIMATIONS in the module doc.
    let mut opts = args.into_options(global_frozen, None, cli_flags, super::env_snapshot());
    // yarn `enableScripts: false` — honor the security opt-out by forcing a
    // block-all-builds policy that overrides even nub's curated default-trust floor.
    if config.scripts_disabled {
        opts.build_policy_override =
            Some(std::sync::Arc::new(aube_scripts::BuildPolicy::deny_all()));
    }
    // Workspace scoping (`--filter`/`-r`/…) rides the engine's own
    // `workspace_filter` — the same field `aube install --filter` sets in
    // `run_install_command` (vendor/aube lib.rs) and feeds to
    // `discover_workspace_plan`.
    opts.workspace_filter = flags.filter.effective_filter();

    let yarn = yarn_detected(&session);
    if yarn {
        let detected = session.detected.as_ref().expect("yarn implies detection");
        // A declared-yarn project with NO yarn.lock yet (identity resolution's
        // DeclaredFresh): the first install would *create* yarn.lock, which is
        // exactly the write the gate exists to block.
        if detected.fresh {
            return Err(yarn_gate_error(
                "install",
                "this project declares yarn but has no yarn.lock yet — a fresh install \
                 would create it",
                "yarn install",
            ));
        }
        let dir = &detected.dir;
        // Refuse upfront when the flags *ask* for a lockfile write…
        if flags.no_frozen_lockfile || flags.force || flags.lockfile_only {
            return Err(yarn_gate_error(
                "install",
                "the requested install would rewrite yarn.lock",
                "yarn install",
            ));
        }
        // …or when the lockfile can't satisfy the manifest (the install would
        // have to re-resolve, which writes yarn.lock).
        if let Some(reason) = yarn_drift_reason(dir) {
            return Err(yarn_gate_error(
                "install",
                &format!("yarn.lock is out of date ({reason})"),
                "yarn install",
            ));
        }
        // Belt-and-braces: force strict-frozen so anything the pre-flight
        // missed errors instead of rewriting yarn.lock. (`strict_no_lockfile`
        // stays as `into_options` resolved it — a missing yarn.lock can't
        // happen past the fresh guard above; detection just saw one.)
        opts.mode = FrozenMode::Frozen;
    }

    super::min_release_age::arm();
    let code = run_engine(&session, opts, yarn, &flags.output)?;
    super::min_release_age::persist(&session.cwd, code == 0, &flags.output);
    // Vite symlink-GVS serving compat (#315): after a successful install, write
    // `node_modules/.modules.yaml` and (for Vite < 8.1) patch the ejected vite
    // dist so its dev server serves the machine-global store. Gated internally on
    // vite-in-graph only (unconditional — no user opt-out); best-effort, never
    // fails the install.
    apply_vite_compat(&session, code);
    // Virgin install only: stamp a caret RANGE into `devEngines.packageManager`
    // so the project advertises nub the standard, cross-tool way WITHOUT locking
    // itself to one exact nub version. nub's canonical lockfile is deliberately
    // NEUTRAL (`nub.lock`), so — unlike every other PM, whose branded lockfile
    // is itself the repo's PM signal — nub leaves no signal downstream tools
    // (turbo, pmd, nypm) can read; the `devEngines.packageManager` object IS that
    // signal, and detectors key on its `name`, so a `^` range is signal-equivalent
    // to an exact pin while staying a non-locking floor (upgrades just work). It is
    // NOT the exact `packageManager: nub@<v>` field: that hard, corepack-visible
    // pin freezes the repo at one nub version and stays the OPT-IN gesture of an
    // explicit `nub pm use nub@<exact>`. The write is gated on
    // `session.truly_fresh`, captured BEFORE the engine wrote `nub.lock`: `true`
    // ONLY when nub is the FIRST package manager to touch the project (no foreign
    // lockfile, no pre-existing nub lockfile, no `packageManager`/`devEngines`
    // declaration). Any incumbent signal ⇒ `false` ⇒ no write — nub never imposes
    // its brand on another PM's project (the symmetric brand boundary).
    stamp_if_virgin(&session, code);
    Ok(code)
}

/// Stamp the virgin `devEngines.packageManager` range after a successful
/// package.json-modifying engine verb (`install`/`add`/`remove`/`update`).
/// Gated on `session.truly_fresh` — captured at session build, BEFORE the verb
/// wrote the lockfile — so it fires exactly once, on the FIRST package-manager
/// operation in a project nub is first to touch, and the gate self-corrects:
/// a non-virgin project (any lockfile/declaration present) never stamps,
/// whichever verb ran. `import` is intentionally not a caller — it converts an
/// existing FOREIGN lockfile, so its project is non-virgin by construction.
fn stamp_if_virgin(session: &EngineSession, code: i32) {
    if code == 0 && session.truly_fresh {
        stamp_virgin_dev_engines(&session.cwd);
    }
}

/// Write `devEngines.packageManager = {name:"nub", version:"^<x.y.z>",
/// onFail:"warn"}` into the project manifest of a VIRGIN install — the
/// non-locking cross-tool PM signal (see the call-site comment). Best-effort: a
/// successful install must not fail on a manifest the stamp can't reach (no
/// `package.json` — nub never scaffolds one — or an unwritable file).
/// Format-preserving + atomic via the shared manifest editor; it ranks the new
/// key by insertion order at the manifest's tail, leaving the user's existing
/// keys untouched. The caller has already proven virginity (no prior
/// `devEngines`), so the object is created wholesale.
fn stamp_virgin_dev_engines(cwd: &Path) {
    // Skip silently when the install ran without a `package.json` (the editor
    // refuses to scaffold one); the install already succeeded, so this is a
    // no-op, not an error.
    let Some(root) = find_manifest_root(cwd) else {
        return;
    };
    // Defensive symmetric-brand-boundary guard. The `truly_fresh` gate derives
    // virginity from aube's lockfile detection, which does NOT recognize two
    // foreign PM signals: bun's pre-1.2 BINARY lockfile `bun.lockb` (only the
    // text `bun.lock` is a detection candidate) and a lone yarn-berry
    // `.yarnrc.yml` config with no `yarn.lock` yet. Without this re-check a nub
    // `devEngines.packageManager` stamp could land in a bun/yarn-owned project —
    // exactly the brand imposition the virgin predicate forbids. Walk up like
    // `is_truly_fresh_project` does for pnpm-named files, and bail on a hit.
    if dir_walk_up_has_any(cwd, &["bun.lockb", ".yarnrc.yml"]) {
        return;
    }
    // Stamp ONLY when this op wrote nub's OWN canonical (neutral) lockfile. The
    // stamp's whole purpose is to supply the PM signal that nub's UNBRANDED
    // lockfile otherwise withholds; when a virgin project resolves to a FOREIGN
    // lockfile format instead (e.g. `default_lockfile_format=pnpm`), that
    // lockfile IS the signal — so the stamp is unneeded, AND a nub claim beside a
    // pnpm/npm-format lock misrepresents the project. Keyed off the
    // canonical-lockfile NAME accessor (rename-safe; resolves the embedder
    // profile + git-branch variant).
    if !root.join(aube_lockfile::aube_lock_filename(&root)).exists() {
        return;
    }
    let range = format!("^{}", env!("CARGO_PKG_VERSION"));
    let _ = nub_core::pm::resolve::edit_root_manifest(&root, |obj| {
        let dev = obj
            .entry("devEngines")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(dev) = dev.as_object_mut() {
            // Never overwrite an existing `devEngines.packageManager`. The
            // `truly_fresh` gate keys on lockfiles + pnpm-named files, NOT on the
            // manifest's declaration fields, so a hand-written foreign
            // `devEngines.packageManager` (e.g. `{name:"pnpm"}`) can coexist with
            // a virgin lockfile state — and clobbering it would impose nub's
            // brand over another PM's declaration (the symmetric brand boundary).
            dev.entry("packageManager").or_insert_with(
                || serde_json::json!({ "name": "nub", "version": range, "onFail": "warn" }),
            );
        }
    });
}

/// Bounded ancestor walk (inclusive, 16 levels like the identity/manifest
/// walk-ups) testing whether any directory carries one of `names`.
fn dir_walk_up_has_any(cwd: &Path, names: &[&str]) -> bool {
    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        if names.iter().any(|n| dir.join(n).exists()) {
            return true;
        }
        if !dir.pop() {
            break;
        }
    }
    false
}

/// FAIL LOUD if `offline_active` and the session's yarn project configures a
/// classic `.yarnrc` `yarn-offline-mirror` nub can't honor. No-op when offline
/// mode is off (the mirror is moot online) or the session has no resolved
/// identity. Shared by `run_install` and `run_ci`.
fn offline_mirror_preflight(
    session: &EngineSession,
    offline_active: bool,
) -> Option<anyhow::Error> {
    if !offline_active {
        return None;
    }
    let (role, root) = super::session_role_root(session)?;
    super::unsupported_config::offline_mirror_fatal(role, &root)
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
    // `engine_session_ci` forces a project-local virtual store (GVS off) so
    // ci's frozen node_modules is COPY-relocatable across multi-stage Docker
    // (#241); isolation/phantom-dep protection is preserved.
    let session = super::engine_session_ci(flags.dir.as_deref())?;
    // `ci` is a frozen, ephemeral install — it NEVER writes the lockfile, so
    // the writer's `lock.yaml` → `nub.lock` migration (which rides a real
    // write) never fires. Read-both still lets it install from an existing
    // `lock.yaml`; it just leaves the file untouched.
    if let Some(err) = pnpm_lockfile_version_preflight(&session) {
        return Err(err);
    }

    // `--registry`: mirror `aube ci`'s `args.network.install_overrides()`
    // (the registry override is process-global; only set when given so the
    // settings-tier resolution stays untouched otherwise).
    if flags.registry.is_some() {
        let mut network = default_install_args().network;
        network.registry = flags.registry.clone();
        network.install_overrides();
    }

    // Clean first, like `aube ci` / `npm ci`. The project root for nub's
    // purposes is where the lockfile lives (fall back to cwd for the
    // no-lockfile case — the strict install below errors before linking).
    // Approximation: assumes the default `node_modules` modulesDir name.
    let root = match session.detected.as_ref() {
        Some(d) => d.dir.clone(),
        None => std::env::current_dir()?,
    };
    remove_node_modules(&root.join("node_modules"))?;

    // Config-derived knobs (ci is frozen by definition, so the dep-axis, the
    // yarn block-all-scripts opt-out, and the yarn `enableNetwork: false`
    // offline mode apply — `ci` has no `--offline` CLI flag, so offline mode
    // here comes solely from config).
    let config = super::install_config_signals(&session);

    // Same offline-mirror fail-loud gate as `run_install`: in offline mode a
    // configured `yarn-offline-mirror` nub can't honor is fatal (moot online).
    if let Some(err) = offline_mirror_preflight(&session, config.offline) {
        return Err(err);
    }

    let dep = config.dep_selection;
    let opts = InstallOptions {
        mode: FrozenMode::Frozen,
        // The explicit CLI flag always wins (OR'd in, never overridden) — same
        // contract as `run_install`.
        dep_selection: DepSelection::from_flags(
            dep.prod,
            dep.dev,
            dep.no_optional || flags.no_optional,
        ),
        ignore_scripts: flags.ignore_scripts,
        build_policy_override: config
            .scripts_disabled
            .then(|| std::sync::Arc::new(aube_scripts::BuildPolicy::deny_all())),
        // yarn `enableNetwork: false` ⇒ offline (serve cached only, error on a
        // miss). `ci` builds opts directly, so set the mode explicitly here.
        network_mode: if config.offline {
            aube_registry::NetworkMode::Offline
        } else {
            aube_registry::NetworkMode::Online
        },
        strict_no_lockfile: true,
        cli_flags: Vec::new(),
        env_snapshot: super::env_snapshot(),
        // `nub ci` is the argumentless-install shape: root lifecycle hooks run.
        skip_root_lifecycle: false,
        // Workspace scoping (`--filter`/`-r`/…) — same `workspace_filter`
        // channel as `run_install`, into `discover_workspace_plan`.
        workspace_filter: flags.filter.effective_filter(),
        ..InstallOptions::with_mode(FrozenMode::Frozen)
    };

    let yarn = yarn_detected(&session);
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
            return Err(yarn_gate_error(
                "ci",
                &format!("yarn.lock is out of date ({reason})"),
                "yarn install",
            ));
        }
    }
    run_engine(&session, opts, yarn, &flags.output)
}

/// Yarn-kind drift pre-flight, at the nub layer.
///
/// Why this exists: the yarn readers reconstruct importers by
/// cross-referencing current manifests against the lockfile, silently dropping
/// any dependency the lockfile cannot satisfy. Berry also records no direct
/// dependency specifiers, so generic frozen drift reports those reconstructed
/// importers fresh. This pre-flight compares each reconstructed importer back
/// to its current manifest before the engine can under-install it.
///
/// Parse/read failures return None (no drift claim); the engine surfaces those
/// errors itself with better diagnostics.
fn yarn_drift_reason(dir: &Path) -> Option<String> {
    let root_manifest = aube_manifest::PackageJson::from_path(&dir.join("package.json")).ok()?;
    let graph = aube_lockfile::yarn::parse(&dir.join("yarn.lock"), &root_manifest).ok()?;

    for (importer, locked_deps) in &graph.importers {
        let manifest = if importer == "." {
            root_manifest.clone()
        } else {
            let importer_path = Path::new(importer);
            // Workspace patterns may intentionally select a sibling via `..`;
            // keep that supported relative form while rejecting other shapes.
            if importer_path.as_os_str().is_empty()
                || importer_path.components().any(|component| {
                    !matches!(
                        component,
                        std::path::Component::Normal(_) | std::path::Component::ParentDir
                    )
                })
            {
                return None;
            }
            let importer_dir = aube_util::path::normalize_lexical(&dir.join(importer_path));
            aube_manifest::PackageJson::from_path(&importer_dir.join("package.json")).ok()?
        };
        let is_satisfied = |name: &str, dep_type: aube_lockfile::DepType| {
            locked_deps
                .iter()
                .any(|dep| dep.name == name && dep.dep_type == dep_type)
        };
        let format_reason = |name: &str, spec: &str| {
            let missing = format!("{name}@{spec} is not satisfied by yarn.lock");
            if importer == "." {
                missing
            } else {
                format!("{importer}: {missing}")
            }
        };

        if let Some((name, spec)) = manifest
            .dependencies
            .iter()
            .find(|(name, _)| !is_satisfied(name, aube_lockfile::DepType::Production))
        {
            return Some(format_reason(name, spec));
        }

        if let Some((name, spec)) = manifest.dev_dependencies.iter().find(|(name, _)| {
            !manifest.dependencies.contains_key(name.as_str())
                && !is_satisfied(name, aube_lockfile::DepType::Dev)
        }) {
            return Some(format_reason(name, spec));
        }

        let skipped = graph.skipped_optional_dependencies.get(importer);
        if let Some((name, spec)) = manifest.optional_dependencies.iter().find(|(name, _)| {
            !manifest.dependencies.contains_key(name.as_str())
                && !manifest.dev_dependencies.contains_key(name.as_str())
                && !is_satisfied(name, aube_lockfile::DepType::Optional)
                && !skipped.is_some_and(|deps| deps.contains_key(name.as_str()))
        }) {
            return Some(format_reason(name, spec));
        }
    }
    None
}

// ───────────────────────── pnpm lockfile-version gate ──────────────────────────

/// The pnpm `lockfileVersion` major that nub's embedded reader understands.
/// pnpm 9+ writes `lockfileVersion: '9.0'`; pnpm 8 wrote `'6.0'`, pnpm 7
/// `'5.4'`, and so on. The reader only models the v9 `importers:` shape — a
/// v6/v5.4 lockfile's top-level `dependencies:` map deserializes as an empty
/// project, so an install against it would silently link nothing. This gate
/// turns that silent no-op into an upfront refusal.
const PNPM_SUPPORTED_LOCKFILE_MAJOR: u64 = 9;

/// Pre-flight: refuse an install when the active `pnpm-lock.yaml` is a
/// `lockfileVersion` nub can't read (anything but v9 today), instead of
/// treating the unreadable lockfile as an empty project and linking nothing.
///
/// Returns `Some(err)` to abort, `None` to proceed. Fires only for a real
/// on-disk `pnpm-lock.yaml` that is the project's *resolved* lockfile (kind
/// `Pnpm`, not `fresh`); every other identity (npm/yarn/bun/aube, or a
/// declared-but-absent pnpm lockfile) is out of scope. The check is read-only
/// — it never touches `node_modules` or the lockfile, so on refusal both are
/// left exactly as found. Any read/parse hiccup (unreadable file, malformed
/// YAML, missing `lockfileVersion`) returns `None`: those are the engine's to
/// diagnose with its own richer errors, not this narrow guard's.
fn pnpm_lockfile_version_preflight(session: &EngineSession) -> Option<anyhow::Error> {
    let detected = session.detected.as_ref()?;
    if detected.kind != LockfileKind::Pnpm || detected.fresh {
        return None;
    }
    let path = detected
        .dir
        .join(aube_lockfile::pnpm_lock_filename(&detected.dir));
    let content = std::fs::read_to_string(&path).ok()?;
    let version = parse_pnpm_lockfile_version(&content)?;
    let major = version
        .split('.')
        .next()
        .and_then(|m| m.parse::<u64>().ok());
    if major == Some(PNPM_SUPPORTED_LOCKFILE_MAJOR) {
        return None;
    }
    Some(unsupported_lockfile_version_error(&version))
}

/// Read just the top-level `lockfileVersion` scalar from a `pnpm-lock.yaml`,
/// normalized to a dotted string. pnpm has written it both quoted
/// (`'9.0'` / `'6.0'`) and bare-numeric (`5.4`), so we accept either YAML
/// scalar shape and render it back as a dotted version string.
fn parse_pnpm_lockfile_version(content: &str) -> Option<String> {
    let root: serde_yaml::Value = serde_yaml::from_str(content).ok()?;
    let version = root.get("lockfileVersion")?;
    match version {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The hard-error for an unreadable `pnpm-lock.yaml` version. Carries the
/// engine's stable `ERR_AUBE_LOCKFILE_UNSUPPORTED_FORMAT` code (rewritten to
/// `ERR_NUB_*` by [`present`], exit 12 via the engine's table) and names the
/// detected version plus the re-lock remedy.
fn unsupported_lockfile_version_error(version: &str) -> anyhow::Error {
    let pnpm_era = pnpm_era_for_lockfile_version(version);
    let report = miette::miette!(
        code = aube_codes::errors::ERR_AUBE_LOCKFILE_UNSUPPORTED_FORMAT,
        help = "Re-lock under pnpm 9+ (`pnpm install`), then `nub install`.",
        "pnpm-lock.yaml is lockfileVersion {version}{pnpm_era}; nub reads v9 (pnpm 9+)."
    );
    anyhow::anyhow!("{}", present::render_report(&report))
}

/// Map a pnpm `lockfileVersion` to a parenthetical naming the pnpm release
/// that wrote it, for the refusal message. Only the versions a user is
/// realistically carrying are named; anything else gets no parenthetical
/// (the version number alone is unambiguous).
fn pnpm_era_for_lockfile_version(version: &str) -> &'static str {
    match version {
        "6.0" | "6" => " (pnpm 8)",
        "5.4" => " (pnpm 7)",
        "5.3" => " (pnpm 6)",
        _ => "",
    }
}

/// Run the install on the session runtime, route failures through the
/// presentation layer. `yarn_gated` switches the frozen-drift failure to the
/// yarn write-gate message.
fn run_engine(
    session: &EngineSession,
    opts: InstallOptions,
    yarn_gated: bool,
    output: &super::output::OutputFlags,
) -> Result<i32> {
    // Hold the output guard only across the engine run (so `--silent` suppresses
    // the progress/summary written during install) and drop it before the match
    // below, so a final error report still reaches the real stderr.
    let result = with_output(output, || {
        let result = session.runtime.block_on(aube::commands::install::run(opts));
        // Flush the diagnostics recorder (summary table, critical-path, etc.) so
        // that AUBE_DIAG_* env vars work end-to-end via `nub install`. aube's own
        // CLI entry flushes from lib.rs; the library path needs an explicit call.
        aube_util::diag::flush();
        result
    });
    match result {
        Ok(()) => Ok(0),
        // Frozen-drift on a gated yarn project: the install *would* rewrite
        // yarn.lock if allowed to re-resolve. Surface the gate, not aube's
        // "run without --frozen-lockfile" hint (which would punch through it).
        // Matched on the engine's stable drift code (both frozen-drift sites
        // carry it), not the human message.
        Err(report)
            if yarn_gated
                && report.code().is_some_and(|code| {
                    code.to_string() == aube_codes::errors::ERR_AUBE_OUTDATED_LOCKFILE
                }) =>
        {
            Err(yarn_gate_error(
                "install",
                &format!("yarn.lock is out of date ({report})"),
                "yarn install",
            ))
        }
        // Everything else: render with the brand rewrite, exit with the
        // engine's own code for the diagnostic (EXIT_TABLE; generic 1
        // fallback) — matching aube's own cli_main behavior.
        Err(report) => Ok(present::emit_report(&report)),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse<A: ClapArgs>(typed: &str, args: &[&str]) -> (EngineGlobals, A) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse_verb::<A>(typed, &args).unwrap() {
            ParsedVerb::Run(globals, verb) => (globals, verb),
            ParsedVerb::Done(code) => panic!("expected a parse, clap settled with exit {code}"),
        }
    }

    #[test]
    fn yarn_drift_preflight_accepts_satisfied_classic_workspace_importers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"root","private":true,"packageManager":"yarn@1.22.22","workspaces":["packages/*"],"dependencies":{"@fixture/utils":"1.0.0"}}"#,
        )
        .unwrap();
        for (member, manifest) in [
            (
                "app",
                r#"{"name":"@fixture/app","version":"1.0.0","dependencies":{"@fixture/utils":"1.0.0"}}"#,
            ),
            ("utils", r#"{"name":"@fixture/utils","version":"1.0.0"}"#),
        ] {
            let member_dir = root.join("packages").join(member);
            std::fs::create_dir_all(&member_dir).unwrap();
            std::fs::write(member_dir.join("package.json"), manifest).unwrap();
        }
        std::fs::write(root.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();
        std::fs::write(
            root.join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\nimporters:\n  .: {}\n",
        )
        .unwrap();

        assert_eq!(
            yarn_drift_reason(root),
            None,
            "the preflight must parse the exact yarn.lock even with a stray foreign lockfile"
        );
    }

    #[test]
    fn yarn_drift_preflight_checks_the_effective_dependency_type() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"root","private":true,"packageManager":"yarn@4.17.0","dependencies":{"foo":"2.0.0"},"devDependencies":{"foo":"1.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("yarn.lock"),
            r#"__metadata:
  version: 10
  cacheKey: 10c0

"foo@npm:1.0.0":
  version: 1.0.0
  resolution: "foo@npm:1.0.0"
  languageName: node
  linkType: hard

"root@workspace:.":
  version: 0.0.0-use.local
  resolution: "root@workspace:."
  dependencies:
    foo: "npm:1.0.0"
  languageName: unknown
  linkType: soft
"#,
        )
        .unwrap();

        assert_eq!(
            yarn_drift_reason(root).as_deref(),
            Some("foo@2.0.0 is not satisfied by yarn.lock"),
            "a satisfied lower-priority dev edge must not mask production drift"
        );
    }

    #[test]
    fn yarn_drift_preflight_checks_parent_relative_workspace_importers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let sibling = tmp.path().join("sibling");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"root","private":true,"packageManager":"yarn@4.17.0","workspaces":["../sibling"]}"#,
        )
        .unwrap();
        std::fs::write(
            sibling.join("package.json"),
            r#"{"name":"@fixture/sibling","version":"1.0.0","dependencies":{"is-odd":"3.0.1"}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("yarn.lock"),
            r#"__metadata:
  version: 10
  cacheKey: 10c0

"@fixture/sibling@workspace:../sibling":
  version: 0.0.0-use.local
  resolution: "@fixture/sibling@workspace:../sibling"
  languageName: unknown
  linkType: soft

"root@workspace:.":
  version: 0.0.0-use.local
  resolution: "root@workspace:."
  languageName: unknown
  linkType: soft
"#,
        )
        .unwrap();

        assert_eq!(
            yarn_drift_reason(&root).as_deref(),
            Some("../sibling: is-odd@3.0.1 is not satisfied by yarn.lock"),
            "supported parent-relative workspace importers must not bypass drift validation"
        );
    }

    /// The aube args types parse through nub's verb_command with their
    /// upstream spellings intact — spot-checked on the daily drivers
    /// (deeper flag semantics are upstream's tests; this guards the
    /// augment/flatten wiring and the alias spellings nub advertises).
    #[test]
    fn verb_args_parse_with_aubes_upstream_flag_spellings() {
        let (_, add): (_, aube::commands::add::AddArgs) = parse(
            "add",
            &["-D", "-E", "--allow-build=esbuild", "lodash", "react"],
        );
        assert!(add.save_dev && add.save_exact);
        assert_eq!(add.allow_build, ["esbuild"]);
        assert_eq!(add.packages, ["lodash", "react"]);

        let (_, rm): (_, aube::commands::remove::RemoveArgs) = parse("rm", &["-g", "lodash"]);
        assert!(rm.global);
        assert_eq!(rm.packages, ["lodash"]);

        let (_, up): (_, aube::commands::update::UpdateArgs) =
            parse("up", &["--latest", "--no-save", "react"]);
        assert!(up.latest && up.no_save);
        assert_eq!(up.packages, ["react"]);

        let (_, dlx): (_, aube::commands::dlx::DlxArgs) =
            parse("dlx", &["-p", "cowsay", "-c", "cowsay hi", "|", "tr"]);
        assert!(dlx.shell_mode);
        assert_eq!(dlx.package, ["cowsay"]);
        // trailing var-arg: everything after the first positional rides along
        assert_eq!(dlx.params, ["cowsay hi", "|", "tr"]);

        // one representative for the rest of the family's flag surfaces
        let (_, dedupe): (_, aube::commands::dedupe::DedupeArgs) = parse("dedupe", &["--check"]);
        assert!(dedupe.check);
        let (_, prune): (_, aube::commands::prune::PruneArgs) =
            parse("prune", &["--prod", "--no-optional"]);
        assert!(prune.prod && prune.no_optional);
        let (_, fetch): (_, aube::commands::fetch::FetchArgs) = parse("fetch", &["-P"]);
        assert!(fetch.prod && !fetch.dev);
        let (_, link): (_, aube::commands::link::LinkArgs) = parse("link", &["../sibling"]);
        assert_eq!(link.package.as_deref(), Some("../sibling"));
        let (_, ab): (_, aube::commands::approve_builds::ApproveBuildsArgs) =
            parse("approve-builds", &["--all"]);
        assert!(ab.all);
        let (_, imp): (_, aube::commands::import::ImportArgs) = parse("import", &["--force"]);
        assert!(imp.force);
        let (_, rb): (_, aube::commands::rebuild::RebuildArgs) = parse("rb", &["esbuild"]);
        assert_eq!(rb.packages, ["esbuild"]);

        let (_, patch): (_, aube::commands::patch::PatchArgs) =
            parse("patch", &["lodash@4.17.21", "--edit-dir", "/tmp/edit"]);
        assert_eq!(patch.package, "lodash@4.17.21");
        assert_eq!(patch.edit_dir.as_deref(), Some(Path::new("/tmp/edit")));
        let (_, pc): (_, aube::commands::patch_commit::PatchCommitArgs) = parse(
            "patch-commit",
            &["/tmp/edit/user", "--patches-dir", "fixes"],
        );
        assert_eq!(pc.edit_dir, Path::new("/tmp/edit/user"));
        assert_eq!(pc.patches_dir, Path::new("fixes"));
        let (_, pr): (_, aube::commands::patch_remove::PatchRemoveArgs) =
            parse("patch-remove", &["lodash@4.17.21"]);
        assert_eq!(pr.packages, ["lodash@4.17.21"]);
    }

    /// A bare `nubx <tool>` DLX fallback (run when the bin is absent from
    /// `node_modules/.bin`) hands the tool to the engine's `dlx` as a plain
    /// positional with args forwarded verbatim — `npx`/`pnpm dlx` semantics:
    /// the tool name doubles as the package (no `-p`, so the engine resolves the
    /// real bin name from the package's `bin` map), nothing is run through `sh -c`
    /// (no `-c`), and no lifecycle scripts are auto-approved (no `--allow-build`).
    #[test]
    fn nubx_dlx_fallback_forwards_tool_and_args_with_no_dlx_flags() {
        let flags = crate::cli::NubxDlxFlags::default();
        let verb = nubx_dlx_args(
            "cowsay",
            &["-f".into(), "tux".into(), "hi there".into()],
            &flags,
        );
        // Tool is the positional; args ride after it untouched (a tool flag like
        // `-f` is the tool's, never consumed by nubx/dlx).
        assert_eq!(verb.params, ["cowsay", "-f", "tux", "hi there"]);
        // With no `-p`, the tool name is the package — the engine derives the bin.
        assert!(verb.package.is_empty(), "no -p: tool name is the package");
        assert!(
            !verb.shell_mode,
            "no -c: tool argv must round-trip, not sh -c"
        );
        assert!(verb.allow_build.is_empty(), "no scripts auto-approved");

        // A tool with no args still produces a single-positional invocation.
        let bare = nubx_dlx_args("serve", &[], &flags);
        assert_eq!(bare.params, ["serve"]);
    }

    /// `nubx -p <spec> <bin> [args]` populates `DlxArgs.package` with the spec(s)
    /// and keeps `<bin>` as the positional, so the engine fetches the package and
    /// runs the named bin from it (npx's package≠bin decoupling).
    #[test]
    fn nubx_dlx_package_flag_drives_the_fetch_set() {
        let flags = crate::cli::NubxDlxFlags {
            package: vec!["@tanstack/cli".into()],
            ..Default::default()
        };
        let verb = nubx_dlx_args("tanstack", &["--help".into()], &flags);
        assert_eq!(verb.package, ["@tanstack/cli"], "-p spec drives the fetch");
        assert_eq!(
            verb.params,
            ["tanstack", "--help"],
            "the positional bin + its args still ride params[0..]"
        );
    }

    /// The nub-side default edit parent is nub-named (the engine's fallback
    /// would print an `aube-patch-…` path — module doc) and survives scoped
    /// package specs.
    #[test]
    fn patch_edit_parent_is_nub_named_and_filesystem_safe() {
        let dir = nub_patch_edit_parent("@scope/pkg@1.0.0");
        let leaf = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            leaf.starts_with("nub-patch-+scope+pkg+1.0.0-"),
            "scoped spec must sanitize into the nub-named parent: {leaf}"
        );
        assert!(!leaf.contains('/') && !leaf.contains('@'), "{leaf}");
    }

    /// The nub-honored global flags ride every engine verb and merge into
    /// the EffectiveFilter exactly like aube's compute_effective_filter
    /// (`-r` = `--filter=*`, explicit selectors win).
    #[test]
    fn engine_globals_parse_and_merge_into_the_effective_filter() {
        let (globals, _): (_, aube::commands::add::AddArgs) =
            parse("add", &["-C", "/tmp", "-r", "lodash"]);
        assert_eq!(globals.dir.as_deref(), Some(Path::new("/tmp")));
        assert_eq!(globals.effective_filter().filters, ["*"]);

        let (globals, _): (_, aube::commands::remove::RemoveArgs) = parse(
            "remove",
            &["-r", "--filter", "app...", "--fail-if-no-match", "lodash"],
        );
        let filter = globals.effective_filter();
        assert_eq!(filter.filters, ["app..."], "explicit --filter beats -r");
        assert!(filter.fail_if_no_match);
    }

    /// Usage errors and --help settle at the nub layer: help goes through
    /// the help-grade rewrite (aube's doc comments name the engine and its
    /// config files — `aube/pnpm`, `aube-workspace.yaml`, `aube-lock.yaml`,
    /// `$AUBE_HOME` all appear upstream), errors carry the engine's
    /// CLI-usage exit code. Sweeps every wired verb's rendered help.
    #[test]
    fn clap_outcomes_are_rewritten_and_exit_like_the_engine() {
        // An unknown flag is a usage error → EXIT_CLI_USAGE.
        let args = vec!["--definitely-not-a-flag".to_string()];
        match parse_verb::<aube::commands::prune::PruneArgs>("prune", &args).unwrap() {
            ParsedVerb::Done(code) => assert_eq!(code, aube_codes::exit::EXIT_CLI_USAGE),
            ParsedVerb::Run(..) => panic!("unknown flag must not parse"),
        }
        fn help_of<A: ClapArgs>(typed: &str) -> String {
            present::rewrite_help(verb_command::<A>(typed).render_long_help().to_string())
        }
        use aube::commands as c;
        for (typed, help) in [
            ("add", help_of::<c::add::AddArgs>("add")),
            ("remove", help_of::<c::remove::RemoveArgs>("remove")),
            ("update", help_of::<c::update::UpdateArgs>("update")),
            ("import", help_of::<c::import::ImportArgs>("import")),
            ("dedupe", help_of::<c::dedupe::DedupeArgs>("dedupe")),
            ("prune", help_of::<c::prune::PruneArgs>("prune")),
            ("rebuild", help_of::<c::rebuild::RebuildArgs>("rebuild")),
            ("fetch", help_of::<c::fetch::FetchArgs>("fetch")),
            ("link", help_of::<c::link::LinkArgs>("link")),
            ("unlink", help_of::<c::unlink::UnlinkArgs>("unlink")),
            (
                "approve-builds",
                help_of::<c::approve_builds::ApproveBuildsArgs>("approve-builds"),
            ),
            (
                "ignored-builds",
                help_of::<c::ignored_builds::IgnoredBuildsArgs>("ignored-builds"),
            ),
            ("dlx", help_of::<c::dlx::DlxArgs>("dlx")),
            ("patch", help_of::<c::patch::PatchArgs>("patch")),
            (
                "patch-commit",
                help_of::<c::patch_commit::PatchCommitArgs>("patch-commit"),
            ),
            (
                "patch-remove",
                help_of::<c::patch_remove::PatchRemoveArgs>("patch-remove"),
            ),
        ] {
            assert!(
                !help.to_lowercase().contains("aube"),
                "nub {typed} help must be brand-clean: {help}"
            );
            assert!(
                help.contains(&format!("nub {typed}")),
                "usage names nub {typed}: {help}"
            );
        }
    }

    /// The pnpm lockfile-version scalar is read from either YAML shape pnpm
    /// has shipped — quoted (`'9.0'` / `'6.0'`) and bare-numeric (`5.4`) —
    /// and missing/garbage returns None (the engine diagnoses those).
    #[test]
    fn pnpm_lockfile_version_parses_quoted_and_numeric_scalars() {
        assert_eq!(
            parse_pnpm_lockfile_version("lockfileVersion: '9.0'\nimporters:\n"),
            Some("9.0".to_string())
        );
        assert_eq!(
            parse_pnpm_lockfile_version("lockfileVersion: '6.0'\ndependencies:\n"),
            Some("6.0".to_string())
        );
        assert_eq!(
            parse_pnpm_lockfile_version("lockfileVersion: 5.4\ndependencies:\n"),
            Some("5.4".to_string())
        );
        assert_eq!(parse_pnpm_lockfile_version("importers:\n  .: {}\n"), None);
    }

    /// The refusal names the detected version, the pnpm era that wrote it,
    /// the v9 requirement, and the re-lock remedy — and carries the engine's
    /// stable unsupported-format code rewritten to nub's namespace (the
    /// contract scripts branch on).
    #[test]
    fn unsupported_lockfile_version_error_names_version_era_and_remedy() {
        let msg = unsupported_lockfile_version_error("6.0").to_string();
        assert!(msg.contains("lockfileVersion 6.0 (pnpm 8)"), "{msg}");
        assert!(msg.contains("nub reads v9 (pnpm 9+)"), "{msg}");
        assert!(msg.contains("Re-lock under pnpm 9+"), "{msg}");
        assert!(
            msg.contains("ERR_NUB_LOCKFILE_UNSUPPORTED_FORMAT") && !msg.contains("ERR_AUBE_"),
            "code must be rebranded to nub's namespace: {msg}"
        );

        // An unrecognized version still refuses, just without the era hint.
        let msg = unsupported_lockfile_version_error("4.0").to_string();
        assert!(msg.contains("lockfileVersion 4.0;"), "{msg}");
    }

    /// The yarn gate names the verb it refused and a copy-pasteable yarn
    /// remedy (the daily mutating drivers each pass their own).
    #[test]
    fn yarn_gate_error_names_verb_and_remedy() {
        let err = yarn_gate_error(
            "add",
            "adding a dependency re-resolves and rewrites yarn.lock",
            &yarn_remedy("add", &["lodash".to_string()]),
        );
        let msg = err.to_string();
        assert!(
            msg.contains("nub add: refusing to modify yarn.lock"),
            "{msg}"
        );
        assert!(msg.contains("yarn add lodash"), "{msg}");
    }

    /// fd capture round-trips engine prints so the rewrite can reach raw
    /// println/eprintln sites (unix; the non-unix fallback is a documented
    /// pass-through). Writes at the fd level — libtest's output capture
    /// hooks Rust's `print!` machinery thread-locally, so a `println!` here
    /// would be swallowed before it ever reached fd 1 (the production
    /// engine prints run uncaptured and do reach the fd).
    #[cfg(unix)]
    #[test]
    fn fd_capture_round_trips_raw_prints() {
        let (value, captured) = crate::pm_engine::with_fd_captured(1, || {
            let line = b"Run `aube install` to execute their scripts.\n";
            // SAFETY: plain write(2) on fd 1, which the helper owns here.
            let wrote = unsafe { libc::write(1, line.as_ptr().cast(), line.len()) };
            assert_eq!(wrote, line.len() as isize, "raw write must not short");
            42
        });
        assert_eq!(value, 42);
        // `contains`, not `ends_with`/equality: fd 1 redirection is
        // process-global, so the libtest harness's own progress lines
        // ("test … ok") from parallel tests can land anywhere in the capture
        // window — before OR after our write — so neither a prefix nor a
        // suffix check is stable. The contract under test is narrow and fully
        // pinned by presence: the raw write survives the capture, the rewrite
        // reaches it (the rewritten `nub install` line is present), and the
        // rewrite neutralized the engine's `aube` brand (the un-rewritten
        // `aube install` form is absent).
        let rewritten = present::rewrite(&captured);
        assert!(
            rewritten.contains("Run `nub install` to execute their scripts.\n"),
            "captured+rewritten stream must contain the rewritten engine line, got: {rewritten:?}"
        );
        assert!(
            !rewritten.contains("Run `aube install`"),
            "rewrite must neutralize the engine's aube brand, got: {rewritten:?}"
        );
    }

    /// The virgin stamp writes the `devEngines.packageManager` caret range
    /// object, appended at the manifest tail (the `preserve_order` editor never
    /// reflows the user's existing keys), and NEVER the hard `packageManager`
    /// pin. Gating on virginity is the caller's job; this asserts the write
    /// itself. Seed nub's OWN canonical lockfile in `dir` so the stamp's
    /// "nub wrote its neutral format" gate passes deterministically, whichever
    /// embedder profile the test binary happens to have registered.
    fn seed_nub_lockfile(dir: &Path) {
        std::fs::write(
            dir.join(aube_lockfile::aube_lock_filename(dir)),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();
    }

    #[test]
    fn virgin_stamp_writes_dev_engines_range_at_tail() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\n  \"name\": \"app\",\n  \"version\": \"1.0.0\"\n}\n",
        )
        .unwrap();
        seed_nub_lockfile(dir.path());

        stamp_virgin_dev_engines(dir.path());

        let written = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            manifest.pointer("/devEngines/packageManager"),
            Some(&serde_json::json!({
                "name": "nub",
                "version": format!("^{}", env!("CARGO_PKG_VERSION")),
                "onFail": "warn"
            })),
            "value must be the non-locking caret range on the running nub version"
        );
        assert!(
            manifest.get("packageManager").is_none(),
            "the virgin stamp must NOT write the hard packageManager pin: {written:?}"
        );
        // Tail-append + format preservation: the pre-existing keys keep their
        // order and the new key lands last, so the diff is one added block.
        assert!(
            written.contains("\"version\": \"1.0.0\",\n  \"devEngines\""),
            "stamp must append after the user's keys, not reflow them: {written:?}"
        );
    }

    /// The stamp NEVER overwrites an existing `devEngines.packageManager`. The
    /// `truly_fresh` gate keys on lockfiles + pnpm-named files, not on manifest
    /// declarations, so a hand-written foreign `devEngines.packageManager` can
    /// reach this path — and imposing nub's brand over it would break the
    /// symmetric brand boundary. A sibling `devEngines` entry is left intact.
    #[test]
    fn virgin_stamp_never_overwrites_an_existing_dev_engines_pin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"app","devEngines":{"packageManager":{"name":"pnpm","version":"^10"},"runtime":{"name":"node"}}}"#,
        )
        .unwrap();
        seed_nub_lockfile(dir.path());

        stamp_virgin_dev_engines(dir.path());

        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest.pointer("/devEngines/packageManager/name"),
            Some(&serde_json::json!("pnpm")),
            "a foreign devEngines.packageManager must not be clobbered by the nub stamp"
        );
        assert_eq!(
            manifest.pointer("/devEngines/runtime/name"),
            Some(&serde_json::json!("node")),
            "a sibling devEngines entry survives"
        );
    }

    /// A successful install in a directory with NO `package.json` must not fail
    /// or scaffold one — the stamp is best-effort and silently no-ops (nub never
    /// creates a manifest).
    #[test]
    fn virgin_stamp_is_silent_noop_without_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        // Hermeticity guard: the stamp walks UP for a manifest root, so if the
        // tempdir sits under a checkout (TMPDIR inside a repo) it could reach an
        // ancestor `package.json` and the no-op intent wouldn't hold. Assert the
        // precondition and skip rather than risk touching an unrelated manifest.
        if find_manifest_root(dir.path()).is_some() {
            return;
        }
        stamp_virgin_dev_engines(dir.path());
        assert!(
            !dir.path().join("package.json").exists(),
            "stamp must never scaffold a missing package.json"
        );
    }

    /// The shared gate every modifying verb (`install`/`add`/`remove`/`update`)
    /// routes through: stamp iff the op SUCCEEDED (`code == 0`) AND the project
    /// was virgin at session build (`truly_fresh`). A failed op or a non-virgin
    /// project never stamps — this is what makes `nub add <pkg>` as the first
    /// command stamp, while a 2nd op (or any incumbent PM) does not.
    #[test]
    fn stamp_if_virgin_gates_on_success_and_freshness() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("package.json");
        let seed = "{\n  \"name\": \"app\"\n}\n";
        let session = |truly_fresh: bool| EngineSession {
            detected: None,
            runtime: tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
            truly_fresh,
            cwd: dir.path().to_path_buf(),
        };
        let stamped = |p: &Path| std::fs::read_to_string(p).unwrap().contains("devEngines");

        seed_nub_lockfile(dir.path());

        std::fs::write(&pkg, seed).unwrap();
        stamp_if_virgin(&session(true), 0);
        assert!(stamped(&pkg), "virgin + success must stamp");

        std::fs::write(&pkg, seed).unwrap();
        stamp_if_virgin(&session(true), 1);
        assert!(!stamped(&pkg), "a failed op (code != 0) must not stamp");

        std::fs::write(&pkg, seed).unwrap();
        stamp_if_virgin(&session(false), 0);
        assert!(!stamped(&pkg), "a non-virgin project must not stamp");
    }

    /// Stamp ONLY when nub wrote its own neutral lockfile. A virgin project that
    /// resolved to a FOREIGN lockfile format (e.g. `default_lockfile_format=pnpm`
    /// writes `pnpm-lock.yaml`, not nub's lockfile) is NOT stamped — that
    /// lockfile is already the PM signal, and a nub claim beside it would
    /// misrepresent the project.
    #[test]
    fn virgin_stamp_skips_when_nub_wrote_no_neutral_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\n  \"name\": \"app\"\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();

        stamp_virgin_dev_engines(dir.path());

        let written = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        assert!(
            !written.contains("devEngines"),
            "a foreign-format lockfile must not get a nub stamp: {written:?}"
        );
    }

    /// Symmetric brand boundary: a foreign PM signal aube's detection misses
    /// (`bun.lockb`, pre-1.2 bun) still blocks the stamp, so nub never imposes
    /// its `devEngines` marker on a bun-owned project.
    #[test]
    fn virgin_stamp_skips_an_undetected_foreign_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\n  \"name\": \"app\"\n}\n",
        )
        .unwrap();
        seed_nub_lockfile(dir.path()); // isolate the bun.lockb guard, not the no-lockfile path
        std::fs::write(dir.path().join("bun.lockb"), b"\0bun").unwrap();

        stamp_virgin_dev_engines(dir.path());

        let written = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        assert!(
            !written.contains("devEngines"),
            "a bun.lockb project must not be stamped: {written:?}"
        );
    }
}
