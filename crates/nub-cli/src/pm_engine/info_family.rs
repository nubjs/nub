//! Info family — read-only project/graph/registry queries through the
//! embedded aube engine.
//!
//! **Wired** (this file): `list` (+`ls`, and the hidden long forms
//! `la`/`ll`), `why` (+`w`), `outdated`, `audit`, `licenses` (the wrapper
//! also admits pnpm's documented `list` spelling beside the engine's `ls`),
//! `deprecations`, `peers`, `query`, `view` (+`info`/`show`/`v`), `check`,
//! `bin`, `root`, and `search` (native registry full-text search via the
//! `/-/v1/search` endpoint — same registry client as the publish family's
//! `whoami`/`owner`).
//! **Still a stub** (deliberately): `sbom` (below).
//!
//! `bin -g` / `root -g` print the engine's global-install layout (the
//! `PNPM_HOME`-compatible home, packages under its `global-aube/` subdir) —
//! real on-disk paths where the already-wired `add -g` installs, preserved
//! by the rewrite policy like the global-links residual in the install
//! family.
//!
//! Each wired verb parses its args with the engine's own clap `Args` struct
//! (flattened into a thin per-verb wrapper that adds `-C/--dir` and, for the
//! workspace-scoped verbs, the `-F/--filter`/`-r` globals aube hangs on its
//! top-level `Cli`), builds the shared [`super::engine_session`], runs the
//! corresponding `aube::commands::*::run` on the session runtime, and routes
//! every failure through [`super::present::emit_report`]. Stdout is the data
//! channel — the engine prints query results directly, exactly as `aube`
//! itself would.
//!
//! # No-lockfile pre-flight (brand boundary)
//!
//! The engine's lockfile-reading query verbs handle a missing lockfile with
//! a *direct* `eprintln!("No lockfile found. Run `aube install` …")` and
//! exit 0 — text that never becomes a `miette::Report`, so the presentation
//! rewrite can't touch it. To keep that engine spelling off nub's stderr,
//! each of those verbs pre-flights: resolve the directory whose lockfile the
//! engine will read (replicating the engine's private `dirs::project_root` /
//! `workspace_or_project_root` walk — see [`EngineRoot`]), and when it holds
//! no lockfile, emit the same message through [`super::present::info`]
//! (which rebrands the `aube install` hint) and exit 0 without entering the
//! engine. Known divergences, all confined to never-installed projects and
//! all exit-0-with-an-actionable-message: a `--filter` run that would have
//! reported "No projects matched…" (or errored under `--fail-if-no-match`)
//! reports "No lockfile found…" instead, because the engine only checks
//! selectors before the lockfile read and replicating selector resolution
//! here is not worth the drift risk.
//!
//! # Write gate
//!
//! `audit --fix=update` rewrites the lockfile (the only write in this
//! family). Same policy as `nub install`: a detected `yarn.lock` (classic or
//! berry) is never mutated by the embedded engine, so that combination is
//! refused up front. `--fix` / `--fix=override` only edit `package.json`
//! overrides and stay open.
//!
//! # `sbom` is deliberately NOT wired
//!
//! The engine embeds its own identity in the SBOM *document body* — CycloneDX
//! `metadata.tools[].name = "aube"`, SPDX `creators: ["Tool: aube-<ver>"]`,
//! and an `https://aube.jdx.dev/spdx/…` `documentNamespace` — printed on
//! stdout as data. That violates the no-engine-branding output contract, and
//! the presentation rewrite is the wrong tool (stripping a required SPDX
//! `documentNamespace` URL or rewriting names inside a structured document
//! would corrupt it). Wiring `sbom` needs an upstreamable fork seam that
//! derives the SBOM tool identity from the embedder product override (the
//! `set_user_agent_product` family). Investigated 2026-06-10: NOT a small
//! `ua::product_name()`-style fix — the tool-name sites (CycloneDX
//! `metadata.tools[].name`, sbom.rs:101; SPDX `creators`, sbom.rs:247)
//! would also need the *version* half of the registered token (a new
//! `ua::product_version()` accessor — `env!("CARGO_PKG_VERSION")` there is
//! aube's version, wrong under a registered name), and the SPDX
//! `documentNamespace` (sbom.rs:228, `https://aube.jdx.dev/spdx/…`) needs a
//! namespace-base seam plus a nub-side domain decision nobody has made.
//! Until then the verb errors with an honest "not yet supported" message
//! (run_verb below).
//!
//! # Known cosmetic gaps
//!
//! - Help text comes from the engine structs' doc comments, routed through
//!   the help-grade rewrite ([`present::rewrite_help`]): engine verb
//!   spellings rebrand ("`aube outdated -w`" reads "`nub outdated -w`") and
//!   config-location spellings map to nub's configured contract
//!   (`aube-workspace.yaml` → `pnpm-workspace.yaml`, `why`'s
//!   `.aube/<dep_path>` example → `.nub/<dep_path>`).
//! - `outdated` / `audit` / `peers check` signal "findings exist" via
//!   `std::process::exit(1)` *inside* the engine (pnpm-compat), after the
//!   report is fully printed — they bypass this file's `Result<i32>` return
//!   path but produce the correct stream content and exit codes.

use std::path::{Path, PathBuf};

use anyhow::Result;
use aube::commands::audit::FixMode;
use aube_lockfile::LockfileKind;
use aube_workspace::selector::EffectiveFilter;
use clap::Parser;

use super::{VerbSpec, present, stub_error};

/// Family dispatcher. Wired verbs run the engine; the rest stub-error (see
/// the module doc for the `sbom` decision).
pub(crate) fn run_verb(
    spec: &'static VerbSpec,
    typed: &str,
    args: &[String],
    pm_hint: &str,
) -> Result<i32> {
    match spec.canonical {
        "list" => run_list(typed, args, /*force_long=*/ false),
        "la" | "ll" => run_list(typed, args, /*force_long=*/ true),
        "why" => run_why(typed, args),
        "outdated" => run_outdated(typed, args),
        "audit" => run_audit(typed, args),
        "licenses" => run_licenses(typed, args),
        "deprecations" => run_deprecations(typed, args),
        "peers" => run_peers(typed, args),
        "query" => run_query(typed, args),
        "view" => run_view(typed, args),
        "check" => run_check(typed, args),
        "bin" => run_bin(typed, args),
        "root" => run_root(typed, args),
        "search" => super::publish_family::run_async::<aube::commands::search::SearchArgs, _, _>(
            typed,
            args,
            aube::commands::search::run,
        ),
        // Deliberately not wired: brand leak in the document body (module
        // doc has the seam analysis). Honest message, no generic stub text.
        "sbom" => Err(anyhow::anyhow!(
            "nub {typed}: not yet supported — the engine stamps its own identity into\n\
             \x20\x20the SBOM document body, which nub won't emit until the identity\n\
             \x20\x20derives from the embedder. For now: npm sbom"
        )),
        _ => Err(stub_error(typed, args, pm_hint)),
    }
}

// ── per-verb wrappers ───────────────────────────────────────────────────────
//
// Thin clap wrappers: the engine's own Args struct (flattened, so flags and
// help stay byte-compatible with upstream), plus `-C/--dir` (aube's global)
// and `FilterFlags` on the verbs whose engine `run` takes an
// `EffectiveFilter`. Doc comments here become `--help` text — keep them
// engine-neutral.

// The workspace-scope globals aube hangs on its top-level `Cli`, re-homed
// per-verb (nub's engine verbs bypass nub's top-level clap). Mirrors
// `vendor/aube/crates/aube/src/lib.rs::Cli` + `startup.rs::
// compute_effective_filter`. aube's global `--workspace-root` spelling is
// deliberately absent: it would collide with `outdated`'s own
// `-w/--workspace-root`, and root inclusion is reachable via
// `--include-workspace-root`. (Plain `//` comments: a rustdoc comment on a
// flattened clap struct becomes the command's `--help` about-text.)
#[derive(Debug, clap::Args)]
struct FilterFlags {
    /// Scope to workspace packages matching PATTERN (repeatable).
    ///
    /// Supports exact names, globs (`@scope/*`), paths (`./packages/api`),
    /// graph selectors (`pkg...`, `...pkg`), git-ref selectors
    /// (`[origin/main]`), and exclusions (`!pkg`).
    #[arg(short = 'F', long, value_name = "PATTERN")]
    filter: Vec<String>,

    /// Run across every workspace package (same as `--filter=*`).
    #[arg(short = 'r', long)]
    recursive: bool,

    /// Production-only variant of `--filter`: graph walks skip
    /// devDependencies.
    #[arg(long, value_name = "PATTERN")]
    filter_prod: Vec<String>,

    /// Error when a workspace selector matches no packages (default: warn
    /// and exit 0).
    #[arg(long)]
    fail_if_no_match: bool,

    /// Include the workspace root alongside the selected packages.
    #[arg(long)]
    include_workspace_root: bool,
}

/// Mirror of `compute_effective_filter`: `-r` is sugar for `--filter=*`,
/// no-op when an explicit `--filter`/`--filter-prod` already scopes the run.
fn effective_filter(flags: &FilterFlags) -> EffectiveFilter {
    let mut filters = flags.filter.clone();
    if flags.recursive && filters.is_empty() && flags.filter_prod.is_empty() {
        filters.push("*".to_string());
    }
    EffectiveFilter {
        filters,
        filter_prods: flags.filter_prod.clone(),
        fail_if_no_match: flags.fail_if_no_match,
        include_workspace_root: flags.include_workspace_root,
    }
}

macro_rules! verb_cli {
    ($(#[$doc:meta])* $name:ident, $engine:ty $(, filter: $filter:ident)?) => {
        $(#[$doc])*
        #[derive(Parser)]
        struct $name {
            #[command(flatten)]
            args: $engine,
            $(
                #[command(flatten)]
                $filter: FilterFlags,
            )?
            /// Change to directory before running.
            #[arg(short = 'C', long = "dir", value_name = "DIR")]
            dir: Option<PathBuf>,
        }
    };
}

verb_cli!(ListCli, aube::commands::list::ListArgs, filter: filter);
verb_cli!(WhyCli, aube::commands::why::WhyArgs, filter: filter);
verb_cli!(OutdatedCli, aube::commands::outdated::OutdatedArgs, filter: filter);
verb_cli!(QueryCli, aube::commands::query::QueryArgs, filter: filter);
verb_cli!(AuditCli, aube::commands::audit::AuditArgs);
verb_cli!(LicensesCli, aube::commands::licenses::LicensesArgs);
verb_cli!(
    DeprecationsCli,
    aube::commands::deprecations::DeprecationsArgs
);
verb_cli!(PeersCli, aube::commands::peers::PeersArgs);
verb_cli!(ViewCli, aube::commands::view::ViewArgs);
verb_cli!(CheckCli, aube::commands::check::CheckArgs);
verb_cli!(BinCli, aube::commands::bin::BinArgs);
verb_cli!(RootCli, aube::commands::root::RootArgs);

/// Outcome of a wrapper parse: real args, or "already handled" (help was
/// printed / a usage error was reported) with the exit code to return.
enum Parsed<T> {
    Args(T),
    Done(i32),
}

/// Parse a verb's argv against its wrapper, named for the spelling the user
/// typed (`nub ls --help` says `nub ls`). Help and usage errors carry the
/// engine structs' doc text, so they are rendered *plain* (ANSI styling
/// splitting a word would defeat the rewrite) and routed through the
/// help-grade rewrite (brand pass + config-vocabulary map — help describes
/// nub's configured contract, see [`present::rewrite_help`]).
fn parse_verb<T: Parser>(typed: &str, args: &[String]) -> Result<Parsed<T>> {
    parse_verb_with(typed, args, |cmd| cmd)
}

/// [`parse_verb`] with a pre-parse command tweak, for the one wrapper that
/// widens an engine arg (`licenses`' subcommand spellings) without
/// hand-mirroring the whole struct.
fn parse_verb_with<T: Parser>(
    typed: &str,
    args: &[String],
    tweak: impl FnOnce(clap::Command) -> clap::Command,
) -> Result<Parsed<T>> {
    let name = format!("nub {typed}");
    let mut cmd = tweak(T::command()).name(name.clone()).bin_name(name);
    let argv = std::iter::once("nub".to_string()).chain(args.iter().cloned());
    match cmd.try_get_matches_from_mut(argv) {
        Ok(matches) => Ok(Parsed::Args(T::from_arg_matches(&matches)?)),
        Err(err) => {
            let text = present::rewrite_help(err.render().to_string());
            if err.use_stderr() {
                eprintln!("{text}");
            } else {
                println!("{text}");
            }
            Ok(Parsed::Done(err.exit_code()))
        }
    }
}

/// Engine-run epilogue shared by every wired verb: success is exit 0,
/// failures render through the presentation layer (brand rewrite + the
/// engine's own exit-code table).
fn finish(result: miette::Result<()>) -> Result<i32> {
    match result {
        Ok(()) => Ok(0),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

/// Same exit contract as [`finish`], for engine verbs that return an explicit
/// exit code (`process-exit-sweep`): `Some(code)` is the engine's chosen code,
/// `None` is plain success (0), `Err` renders via the presentation layer.
fn finish_code(result: miette::Result<Option<i32>>) -> Result<i32> {
    match result {
        Ok(code) => Ok(code.unwrap_or(0)),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

// ── wired verbs ─────────────────────────────────────────────────────────────

fn run_list(typed: &str, args: &[String], force_long: bool) -> Result<i32> {
    let mut cli: ListCli = match parse_verb(typed, args)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    if force_long {
        // `la`/`ll` are aube's hidden list-long spellings (lib.rs forces
        // `long = true` and dispatches to list).
        cli.args.long = true;
    }
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    // `--json` (or `--format json`) must emit a parseable empty array on
    // stdout in the never-installed state, not empty-stdout + a prose note.
    let want_json = cli.args.json || cli.args.format == aube::commands::list::ListFormat::Json;
    let empty = if want_json {
        EmptyState::ListJson
    } else {
        EmptyState::Prose(MSG_POPULATE)
    };
    if !cli.args.global
        && let Some(code) = no_lockfile_short_circuit(EngineRoot::WorkspaceOrProject, empty)?
    {
        return Ok(code);
    }
    let filter = effective_filter(&cli.filter);
    finish(
        session
            .runtime
            .block_on(aube::commands::list::run(cli.args, filter)),
    )
}

fn run_why(typed: &str, args: &[String]) -> Result<i32> {
    let cli: WhyCli = match parse_verb(typed, args)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    if let Some(code) =
        no_lockfile_short_circuit(EngineRoot::WorkspaceOrProject, EmptyState::Prose(MSG_FIRST))?
    {
        return Ok(code);
    }
    let filter = effective_filter(&cli.filter);
    finish(
        session
            .runtime
            .block_on(aube::commands::why::run(cli.args, filter)),
    )
}

fn run_outdated(typed: &str, args: &[String]) -> Result<i32> {
    let cli: OutdatedCli = match parse_verb(typed, args)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    let filter = effective_filter(&cli.filter);
    // The engine reads at the project root, except: a `--filter` run
    // re-roots at the workspace root (`select_workspace_packages`), and `-w`
    // retargets to the workspace root when one exists.
    let root = if !filter.is_empty() || cli.args.workspace_root {
        EngineRoot::WorkspaceOrProject
    } else {
        EngineRoot::Project
    };
    // `--json` must emit a parseable empty object on stdout in the
    // never-installed state, not empty-stdout + a prose note.
    let empty = if cli.args.json {
        EmptyState::OutdatedJson
    } else {
        EmptyState::Prose(MSG_FIRST)
    };
    if let Some(code) = no_lockfile_short_circuit(root, empty)? {
        return Ok(code);
    }
    finish_code(
        session
            .runtime
            .block_on(aube::commands::outdated::run(cli.args, filter)),
    )
}

fn run_query(typed: &str, args: &[String]) -> Result<i32> {
    let cli: QueryCli = match parse_verb(typed, args)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    if let Some(code) =
        no_lockfile_short_circuit(EngineRoot::WorkspaceOrProject, EmptyState::Prose(MSG_FIRST))?
    {
        return Ok(code);
    }
    let filter = effective_filter(&cli.filter);
    finish(
        session
            .runtime
            .block_on(aube::commands::query::run(cli.args, filter)),
    )
}

fn run_audit(typed: &str, args: &[String]) -> Result<i32> {
    let cli: AuditCli = match parse_verb(typed, args)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    // Write gate: `--fix=update` rewrites the lockfile; a detected yarn.lock
    // is never mutated by the embedded engine (same policy + remedy shape as
    // the install gate). `--fix`/`--fix=override` only edit package.json.
    // No network happens before this point, so the refusal is instant.
    if cli.args.fix == Some(FixMode::Update)
        && matches!(
            session.detected.as_ref().map(|d| d.kind),
            Some(LockfileKind::Yarn | LockfileKind::YarnBerry)
        )
    {
        return Err(anyhow::anyhow!(
            "nub audit: refusing to modify yarn.lock — `--fix=update` rewrites the lockfile\n\
             \x20\x20yarn.lock write fidelity is unproven in the embedded engine, so commands\n\
             \x20\x20that would rewrite it are blocked. Use bare `--fix` (writes package.json\n\
             \x20\x20overrides only), or apply the update with yarn directly."
        ));
    }
    // Missing lockfile is a miette error here (`load_graph`), not a direct
    // eprintln — the presentation rewrite covers it; no pre-flight needed.
    finish_code(
        session
            .runtime
            .block_on(aube::commands::audit::run(cli.args)),
    )
}

fn run_licenses(typed: &str, args: &[String]) -> Result<i32> {
    let cli: LicensesCli = match parse_verb_with(typed, args, licenses_cmd_tweak)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    if let Some(code) =
        no_lockfile_short_circuit(EngineRoot::Project, EmptyState::Prose(MSG_FIRST))?
    {
        return Ok(code);
    }
    finish(
        session
            .runtime
            .block_on(aube::commands::licenses::run(cli.args)),
    )
}

fn run_deprecations(typed: &str, args: &[String]) -> Result<i32> {
    let cli: DeprecationsCli = match parse_verb(typed, args)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    if let Some(code) =
        no_lockfile_short_circuit(EngineRoot::Project, EmptyState::Prose(MSG_FIRST))?
    {
        return Ok(code);
    }
    // `deprecations` is the one info verb whose engine `run` returns its
    // exit code (`--exit-code` ⇒ Some(1) when deprecations are found).
    match session
        .runtime
        .block_on(aube::commands::deprecations::run(cli.args))
    {
        Ok(code) => Ok(code.unwrap_or(0)),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

fn run_peers(typed: &str, args: &[String]) -> Result<i32> {
    let cli: PeersCli = match parse_verb(typed, args)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    // Missing lockfile is a miette error (`load_graph`) — rewrite covers it.
    finish_code(
        session
            .runtime
            .block_on(aube::commands::peers::run(cli.args)),
    )
}

fn run_view(typed: &str, args: &[String]) -> Result<i32> {
    let cli: ViewCli = match parse_verb(typed, args)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    // Pure registry query: no lockfile involvement at all.
    finish(
        session
            .runtime
            .block_on(aube::commands::view::run(cli.args)),
    )
}

/// The `licenses` wrapper's pre-parse tweak: pnpm's documented spelling is
/// `pnpm licenses list`, but the engine's hidden pnpm-compat positional only
/// admits `ls`. Widen the wrapper to both — the engine ignores the marker's
/// value, so no normalization is needed after parse.
fn licenses_cmd_tweak(cmd: clap::Command) -> clap::Command {
    cmd.mut_arg("subcommand", |arg| arg.value_parser(["ls", "list"]))
}

fn run_check(typed: &str, args: &[String]) -> Result<i32> {
    let cli: CheckCli = match parse_verb(typed, args)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    // Reads the *resolved* virtual store (node_modules/.nub under nub's
    // defaults); a never-installed project reports `checked 0 packages`
    // rather than erroring, so no pre-flight applies. Broken links exit 1
    // via std::process::exit inside the engine (pnpm-compat), like
    // outdated/audit.
    finish_code(
        session
            .runtime
            .block_on(aube::commands::check::run(cli.args)),
    )
}

fn run_bin(typed: &str, args: &[String]) -> Result<i32> {
    let cli: BinCli = match parse_verb(typed, args)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    // Pure path print (`<modulesDir>/.bin`, or the global bin dir under
    // `-g`); the directory need not exist.
    finish(session.runtime.block_on(aube::commands::bin::run(cli.args)))
}

fn run_root(typed: &str, args: &[String]) -> Result<i32> {
    let cli: RootCli = match parse_verb(typed, args)? {
        Parsed::Args(c) => c,
        Parsed::Done(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    // Pure path print (`<modulesDir>`, or the global package dir under `-g`).
    finish(
        session
            .runtime
            .block_on(aube::commands::root::run(cli.args)),
    )
}

// ── no-lockfile pre-flight ──────────────────────────────────────────────────

/// Upstream literals from the engine's no-lockfile `eprintln!` paths. The
/// `aube install` spelling is intentional: the message goes out through
/// `present::info`, whose rewrite rebrands it — keeping the text
/// byte-identical to upstream apart from the brand.
const MSG_POPULATE: &str = "No lockfile found. Run `aube install` to populate node_modules.";
const MSG_FIRST: &str = "No lockfile found. Run `aube install` first.";

/// Which directory the engine will read the lockfile from — mirrors the
/// engine's private `dirs::project_root()` / `dirs::workspace_or_project_root()`
/// (vendor/aube/crates/aube/src/dirs.rs), which this file replicates because
/// `aube::dirs` is crate-private at the pinned API.
enum EngineRoot {
    /// Nearest ancestor with a `package.json` (licenses, deprecations,
    /// unfiltered outdated).
    Project,
    /// Workspace root when one exists, else the project root (list, why,
    /// query; outdated under `--filter`/`-w`).
    WorkspaceOrProject,
}

/// What the no-lockfile short-circuit emits in place of running the engine.
/// The default `Prose` path prints the engine's rebranded "No lockfile found…"
/// note to stderr (exit 0). The JSON variants exist because a `--json` query
/// must ALWAYS emit parseable JSON on stdout — never empty-stdout + a prose
/// stderr note — so `nub list --json | jq` / `nub outdated --json | jq`
/// behave like pnpm's, which emit the empty shape (an array of importer
/// headers for `list`, `{}` for `outdated`) in the never-installed state.
enum EmptyState<'a> {
    /// Rebranded engine note to stderr; exit 0 (`why`, `query`, `licenses`,
    /// `deprecations`, and the non-JSON `list`/`outdated` paths).
    Prose(&'a str),
    /// Empty `list --json` shape: a JSON array with one importer header
    /// (`{name, version, path}`) on stdout; exit 0.
    ListJson,
    /// Empty `outdated --json` shape: `{}` on stdout; exit 0.
    OutdatedJson,
}

/// When the engine's read directory holds no lockfile, emit the no-install
/// empty state (see [`EmptyState`]) and exit 0 — exactly what the engine
/// would do, minus the brand leak and the missing-JSON divergence. `None`
/// means "let the engine run": either a lockfile exists, no root resolves
/// (the engine's own error is brand-clean), or only a binary `bun.lockb`
/// exists (the engine's actionable error is brand-clean too).
fn no_lockfile_short_circuit(root: EngineRoot, empty: EmptyState<'_>) -> Result<Option<i32>> {
    let cwd = std::env::current_dir()?;
    let dir = match root {
        EngineRoot::Project => find_project_root(&cwd),
        EngineRoot::WorkspaceOrProject => {
            find_workspace_root(&cwd).or_else(|| find_project_root(&cwd))
        }
    };
    let Some(dir) = dir else {
        return Ok(None);
    };
    if aube_lockfile::detect_existing_lockfile_kind(&dir).is_none()
        && !dir.join("bun.lockb").exists()
    {
        match empty {
            EmptyState::Prose(msg) => present::info(msg),
            EmptyState::OutdatedJson => println!("{{}}"),
            EmptyState::ListJson => println!("{}", empty_list_json(&dir)),
        }
        return Ok(Some(0));
    }
    Ok(None)
}

/// Build the empty `list --json` shape: a one-element array whose object
/// carries the project's `name`/`version`/`path`, matching the importer
/// header nub's populated `list --json` emits (and pnpm's empty-state array).
/// Reads the project's `package.json` at `dir`; falls back to `(unnamed)` and
/// omits `version` when the manifest is missing/unreadable, mirroring the
/// engine's own `unwrap_or_else` for the name field.
fn empty_list_json(dir: &Path) -> String {
    let manifest = aube_manifest::PackageJson::from_path(&dir.join("package.json")).ok();
    let mut importer = serde_json::Map::new();
    importer.insert(
        "name".to_string(),
        serde_json::Value::String(
            manifest
                .as_ref()
                .and_then(|m| m.name.clone())
                .unwrap_or_else(|| "(unnamed)".to_string()),
        ),
    );
    if let Some(v) = manifest.as_ref().and_then(|m| m.version.clone()) {
        importer.insert("version".to_string(), serde_json::Value::String(v));
    }
    importer.insert(
        "path".to_string(),
        serde_json::Value::String(dir.display().to_string()),
    );
    serde_json::to_string_pretty(&serde_json::Value::Array(vec![serde_json::Value::Object(
        importer,
    )]))
    .unwrap_or_else(|_| "[]".to_string())
}

/// Replica of the engine's `dirs::find_project_root`: nearest ancestor with
/// a `package.json`, walking no further up than `$HOME` (so a scratch dir
/// can't attach to a stray home-level project).
fn find_project_root(start: &Path) -> Option<PathBuf> {
    let stop = home_boundary();
    for dir in start.ancestors() {
        if dir.join("package.json").is_file() {
            return Some(dir.to_path_buf());
        }
        if stop.as_deref() == Some(dir) {
            return None;
        }
    }
    None
}

/// Replica of the engine's `dirs::find_workspace_root`: nearest ancestor
/// with a workspace yaml (`pnpm-workspace.yaml` / `aube-workspace.yaml`) or
/// a `package.json` carrying a `workspaces` field, same `$HOME` cap.
fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let stop = home_boundary();
    for dir in start.ancestors() {
        if aube_manifest::workspace::workspace_yaml_existing(dir).is_some() {
            return Some(dir.to_path_buf());
        }
        let pkg = dir.join("package.json");
        if pkg.is_file()
            && aube_manifest::PackageJson::from_path(&pkg).is_ok_and(|m| m.workspaces.is_some())
        {
            return Some(dir.to_path_buf());
        }
        if stop.as_deref() == Some(dir) {
            return None;
        }
    }
    None
}

/// `$HOME` (Unix) / `USERPROFILE` (Windows) walk boundary, mirroring the
/// engine's `home_stop_boundary`. `None` ⇒ unbounded walk, same fallback.
fn home_boundary() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    fn parse<T: Parser>(argv: &[&str]) -> T {
        T::try_parse_from(argv).expect("argv must parse")
    }

    /// One representative parse per wrapper: proves the engine flag surface
    /// (positional + shorts + value enums + flattened NetworkArgs) and the
    /// nub-side additions (-C, filter flags) coexist on every verb.
    #[test]
    fn wrappers_parse_the_engine_arg_surface() {
        let list: ListCli = parse(&["nub", "lodash", "--json", "-r", "--depth", "2"]);
        assert_eq!(list.args.pattern.as_deref(), Some("lodash"));
        assert!(list.args.json && list.filter.recursive);

        let why: WhyCli = parse(&["nub", "debug", "--parseable", "-F", "app..."]);
        assert_eq!(why.args.package, "debug");
        assert!(why.args.parseable);
        assert_eq!(why.filter.filter, vec!["app..."]);

        let outdated: OutdatedCli = parse(&["nub", "-w", "--json", "-C", "/tmp/x"]);
        assert!(outdated.args.workspace_root && outdated.args.json);
        assert_eq!(outdated.dir.as_deref(), Some(Path::new("/tmp/x")));

        let audit: AuditCli = parse(&["nub", "--audit-level", "high", "--fix=update"]);
        assert_eq!(audit.args.fix, Some(FixMode::Update));

        let licenses: LicensesCli = parse(&["nub", "ls", "--json"]);
        assert_eq!(licenses.args.subcommand.as_deref(), Some("ls"));

        let deprecations: DeprecationsCli = parse(&["nub", "--exit-code", "--transitive"]);
        assert!(deprecations.args.exit_code && deprecations.args.transitive);

        let peers: PeersCli = parse(&["nub", "check", "--json"]);
        let aube::commands::peers::PeersCommand::Check(check) = peers.args.command;
        assert!(check.json);

        let query: QueryCli = parse(&["nub", ":deprecated", "--parseable"]);
        assert_eq!(query.args.selector, ":deprecated");

        let view: ViewCli = parse(&["nub", "react@next", "dist.tarball"]);
        assert_eq!(view.args.package, "react@next");
        assert_eq!(view.args.field.as_deref(), Some("dist.tarball"));

        let check: CheckCli = parse(&["nub", "--json"]);
        assert!(check.args.json);
        let bin: BinCli = parse(&["nub", "-g"]);
        assert!(bin.args.global);
        let root: RootCli = parse(&["nub", "--global", "-C", "/tmp/x"]);
        assert!(root.args.global);
        assert_eq!(root.dir.as_deref(), Some(Path::new("/tmp/x")));
    }

    /// The licenses wrapper admits pnpm's documented `list` spelling beside
    /// the engine's `ls` (and still rejects arbitrary positionals).
    #[test]
    fn licenses_wrapper_accepts_pnpms_list_spelling() {
        for sub in ["ls", "list"] {
            let parsed = parse_verb_with::<LicensesCli>(
                "licenses",
                &[sub.to_string(), "--json".to_string()],
                licenses_cmd_tweak,
            )
            .unwrap();
            match parsed {
                Parsed::Args(cli) => {
                    assert_eq!(cli.args.subcommand.as_deref(), Some(sub));
                    assert!(cli.args.json);
                }
                Parsed::Done(code) => panic!("licenses {sub} must parse, settled with {code}"),
            }
        }
        let bad = parse_verb_with::<LicensesCli>(
            "licenses",
            &["everything".to_string()],
            licenses_cmd_tweak,
        )
        .unwrap();
        assert!(
            matches!(bad, Parsed::Done(code) if code != 0),
            "an unknown subcommand positional must stay a usage error"
        );
    }

    #[test]
    fn effective_filter_mirrors_the_engine_compute() {
        let flags = |recursive, filter: &[&str], prod: &[&str]| FilterFlags {
            filter: filter.iter().map(|s| s.to_string()).collect(),
            recursive,
            filter_prod: prod.iter().map(|s| s.to_string()).collect(),
            fail_if_no_match: false,
            include_workspace_root: false,
        };
        // `-r` alone is `--filter=*`.
        assert_eq!(effective_filter(&flags(true, &[], &[])).filters, ["*"]);
        // An explicit selector wins; `-r` becomes a no-op.
        assert_eq!(
            effective_filter(&flags(true, &["app"], &[])).filters,
            ["app"]
        );
        // `--filter-prod` alone also suppresses the wildcard.
        let f = effective_filter(&flags(true, &[], &["lib"]));
        assert!(f.filters.is_empty());
        assert_eq!(f.filter_prods, ["lib"]);
    }

    /// Every wired verb's rendered help is fully engine-brand-free under the
    /// help-grade rewrite — verb spellings rebrand (`outdated`'s upstream
    /// docs literally say "`aube outdated -w`") and config-location
    /// spellings map to nub's contract (the flattened NetworkArgs docs name
    /// `aube-workspace.yaml`; `why --paths` names `.aube/<dep_path>`).
    #[test]
    fn help_text_is_rebranded_for_nub() {
        let render = |mut cmd: clap::Command, name: &str| {
            cmd = cmd.name(name.to_string()).bin_name(name.to_string());
            present::rewrite_help(cmd.render_long_help().to_string())
        };
        for (cmd, name) in [
            (ListCli::command(), "nub list"),
            (WhyCli::command(), "nub why"),
            (OutdatedCli::command(), "nub outdated"),
            (QueryCli::command(), "nub query"),
            (AuditCli::command(), "nub audit"),
            (LicensesCli::command(), "nub licenses"),
            (DeprecationsCli::command(), "nub deprecations"),
            (PeersCli::command(), "nub peers"),
            (ViewCli::command(), "nub view"),
            (CheckCli::command(), "nub check"),
            (BinCli::command(), "nub bin"),
            (RootCli::command(), "nub root"),
        ] {
            let help = render(cmd, name);
            assert!(help.contains(name), "usage must carry {name}: {help}");
            assert!(
                !help.to_lowercase().contains("aube"),
                "{name} help must be brand-clean: {help}"
            );
        }
    }

    /// The engine-root replica: a workspace member resolves to itself as
    /// project root and to the yaml dir as workspace root. (The `$HOME` walk
    /// boundary is environment-dependent and stays untested.)
    #[test]
    fn engine_root_replicas_resolve_like_the_engine() {
        // `find_workspace_root` discovers `pnpm-workspace.yaml` only when the
        // engine context's `read_branded_pnpm_config` posture is on (the
        // upstream default). That posture is process-global (a last-write-wins
        // RwLock) and other tests in this binary flip it to `false` by driving
        // `engine_brand_preflight` through a family dispatch. Serialize against
        // them on the shared lock and set the posture true while we hold it, so
        // the global state is stable for this test's reads.
        let _guard = crate::pm_engine::ENGINE_GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        aube_util::update_engine_context(|c| {
            c.read_branded_pnpm_config = true;
            c.read_manifest_root_config = false;
        });
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pkgs/*'\n",
        )
        .unwrap();
        let member = root.path().join("pkgs/app");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(member.join("package.json"), r#"{"name":"app"}"#).unwrap();

        assert_eq!(find_project_root(&member), Some(member.clone()));
        assert_eq!(
            find_workspace_root(&member),
            Some(root.path().to_path_buf())
        );
        // No markers anywhere up to the boundary ⇒ None (engine errors out
        // with its own brand-clean message; we let it run).
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(find_project_root(bare.path()), None);
    }
}
