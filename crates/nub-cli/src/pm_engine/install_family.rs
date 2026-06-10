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
//!   fd-captured** ([`with_fd_captured`]) and re-emitted through the
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
    _spec: &'static VerbSpec,
    typed: &str,
    args: &[String],
    pm_hint: &str,
) -> Result<i32> {
    match _spec.canonical {
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

/// Map an engine result to nub's exit contract: success → 0, failure →
/// rendered through the presentation layer + the engine's exit table.
fn finish(result: miette::Result<()>) -> Result<i32> {
    match result {
        Ok(()) => Ok(0),
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
    finish(
        session
            .runtime
            .block_on(aube::commands::add::run(verb, globals.effective_filter())),
    )
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
    finish(session.runtime.block_on(aube::commands::remove::run(
        verb,
        globals.effective_filter(),
    )))
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
    finish(session.runtime.block_on(aube::commands::update::run(
        verb,
        globals.effective_filter(),
    )))
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
    finish(session.runtime.block_on(aube::commands::dedupe::run(verb)))
}

fn run_prune(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::prune::PruneArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    // prune prints its summary via raw eprintln with a hardcoded `.aube`
    // store label (the walked directory is the *resolved* virtualStoreDir —
    // node_modules/.nub here — only the label lies). Capture + neutralize.
    let (result, captured) = with_fd_captured(2, || {
        session.runtime.block_on(aube::commands::prune::run(verb))
    });
    eprint!(
        "{}",
        present::rewrite(&captured).replace(" from .aube,", " from the virtual store,")
    );
    finish(result)
}

fn run_rebuild(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::rebuild::RebuildArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    finish(session.runtime.block_on(aube::commands::rebuild::run(
        verb,
        globals.effective_filter(),
    )))
}

fn run_fetch(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::fetch::FetchArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    finish(session.runtime.block_on(aube::commands::fetch::run(verb)))
}

fn run_link(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::link::LinkArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    finish(session.runtime.block_on(aube::commands::link::run(verb)))
}

fn run_unlink(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::unlink::UnlinkArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    // The unlink-all path ends with a raw `` Run `aube install` … `` hint
    // on stderr; capture + rewrite (no children, no progress UI here).
    let (result, captured) = with_fd_captured(2, || {
        session.runtime.block_on(aube::commands::unlink::run(verb))
    });
    eprint!("{}", present::rewrite(&captured));
    finish(result)
}

fn run_approve_builds(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::approve_builds::ApproveBuildsArgs) =
        parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    // The success summary ends with a raw `` Run `aube install` (or `aube
    // rebuild`) … `` println on stdout. Capture + rewrite; the interactive
    // picker is unaffected (it prompts on stderr and reads stdin).
    let (result, captured) = with_fd_captured(1, || {
        session
            .runtime
            .block_on(aube::commands::approve_builds::run(verb))
    });
    print!("{}", present::rewrite(&captured));
    finish(result)
}

fn run_ignored_builds(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, verb): (_, aube::commands::ignored_builds::IgnoredBuildsArgs) =
        parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    finish(
        session
            .runtime
            .block_on(aube::commands::ignored_builds::run(verb)),
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
    let session = super::engine_session(globals.dir.as_deref())?;
    // NOTE: on child failure the engine propagates the child's exit code via
    // std::process::exit — control does not return here on that path.
    finish(session.runtime.block_on(aube::commands::dlx::run(verb)))
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
    let session = super::engine_session(globals.dir.as_deref())?;
    // The engine maps the template to its create-* package (foo → create-foo,
    // @scope/foo → @scope/create-foo) and chains into dlx; like dlx, on child
    // failure the engine propagates the exit code via std::process::exit.
    finish(session.runtime.block_on(aube::commands::create::run(verb)))
}

// ───────────────────────── patch workflow ──────────────────────────

fn run_patch(typed: &str, args: &[String]) -> Result<i32> {
    let (globals, mut verb): (_, aube::commands::patch::PatchArgs) = parse_or_return!(typed, args);
    let session = super::engine_session(globals.dir.as_deref())?;
    // Default the edit dir nub-side: the engine's fallback tempdir is
    // `aube-patch-…` and that path IS the success output (module doc).
    if verb.edit_dir.is_none() {
        verb.edit_dir = Some(nub_patch_edit_parent(&verb.package));
    }
    // The success message ends with `` run "aube patch-commit '<dir>'" `` via
    // raw println; capture + rewrite (no children, no progress UI here).
    let (result, captured) = with_fd_captured(1, || {
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
            present::info(&summary);
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

    let (graph, kind) = match aube_lockfile::parse_for_import(&root, &manifest) {
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
    let mut remedy = format!("yarn {yarn_verb}");
    for pkg in packages {
        remedy.push(' ');
        remedy.push_str(pkg);
    }
    remedy
}

// ───────────────────────── fd capture ──────────────────────────

/// Run `f` with OS-level fd `fd` (1 = stdout, 2 = stderr) redirected into a
/// pipe; returns `f`'s result plus everything written, so the caller can
/// re-emit it through the brand rewrite. Only used for verbs that spawn no
/// children and render no progress UI (see the module doc; the config
/// family borrows it for `config get`'s registry-default substitution).
/// Any setup failure degrades to running `f` unredirected with an empty
/// capture — output then reaches the console directly (un-rewritten),
/// which beats losing it.
///
/// Captures are serialized process-wide: the fd table is process-global, so
/// two concurrent dup2 swaps of the same fd interleave into a torn state
/// (writes landing on a closed pipe). Production runs one capture per
/// command, so the lock is free there; it exists for the unit-test binary,
/// where parallel tests genuinely raced it (flaky
/// `fd_capture_round_trips_raw_prints`).
#[cfg(unix)]
pub(super) fn with_fd_captured<T>(fd: libc::c_int, f: impl FnOnce() -> T) -> (T, String) {
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

/// KNOWN GAP (non-unix): no fd capture — the engine's raw prints reach the
/// console un-rewritten, so `approve-builds`' final hint still names the
/// engine's verbs on Windows. Root fix is fork-side (embedder-aware tool
/// name in those prints); see the module doc.
#[cfg(not(unix))]
pub(super) fn with_fd_captured<T>(_fd: i32, f: impl FnOnce() -> T) -> (T, String) {
    (f(), String::new())
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
    args.network.registry = flags.registry.clone();
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
        let (value, captured) = with_fd_captured(1, || {
            let line = b"Run `aube install` to execute their scripts.\n";
            // SAFETY: plain write(2) on fd 1, which the helper owns here.
            let wrote = unsafe { libc::write(1, line.as_ptr().cast(), line.len()) };
            assert_eq!(wrote, line.len() as isize, "raw write must not short");
            42
        });
        assert_eq!(value, 42);
        assert_eq!(
            present::rewrite(&captured),
            "Run `nub install` to execute their scripts.\n"
        );
    }
}
