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
//! `bin -g` / `root -g` print the engine's global-install layout, and the two
//! now resolve from different roots: `bin -g` gives the SHARED user-binary
//! directory already on PATH (`~/.local/bin` and its `XDG_BIN_HOME`
//! relatives), while the installs themselves live under `<data>/<ns>/global`.
//! Real on-disk paths where the already-wired `add -g` installs, preserved
//! by the rewrite policy like the global-links residual in the install
//! family.
//!
//! Each wired verb parses its args with the engine's own `usage_rs::Args`
//! struct (flattened into a thin per-verb wrapper that adds `-C/--dir` and, for
//! the workspace-scoped verbs, the `-F/--filter`/`-r` globals aube hangs on its
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
//!   `.aube/<dep_path>` example → `.store/<dep_path>`).
//! - `outdated` / `audit` / `peers check` signal "findings exist" via
//!   `std::process::exit(1)` *inside* the engine (pnpm-compat), after the
//!   report is fully printed — they bypass this file's `Result<i32>` return
//!   path but produce the correct stream content and exit codes.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use aube::commands::audit::FixMode;
use aube_lockfile::LockfileKind;
use aube_workspace::selector::EffectiveFilter;

use super::publish_family::{Parsed, separate_value_flags, verb_cli};
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
        "search" => super::publish_family::run_search(typed, args),
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
// Thin usage-rs roots: the engine's own `Args` struct (flattened, so flags and
// help stay byte-compatible with upstream), plus `-C/--dir` (aube's global)
// and `FilterFlags` on the verbs whose engine `run` takes an
// `EffectiveFilter`. Doc comments here become `--help` text — keep them
// engine-neutral. The command NAME is the spelling the user typed; see
// `publish_family`'s module doc for how the runtime identity is published.

// The workspace-scope globals aube hangs on its top-level `Cli`, re-homed
// per-verb (nub's engine verbs bypass nub's own top-level parser). Mirrors
// `vendor/aube/crates/aube/src/lib.rs::Cli` + `startup.rs::
// compute_effective_filter`. aube's global `--workspace-root` spelling is
// deliberately absent: it would collide with `outdated`'s own
// `-w/--workspace-root`, and root inclusion is reachable via
// `--include-workspace-root`. (Plain `//` comments: a rustdoc comment on a
// flattened `Args` struct becomes the command's `--help` about-text.)
#[derive(Debug, usage_rs::Args)]
struct FilterFlags {
    /// Scope to workspace packages matching PATTERN (repeatable).
    ///
    /// Supports exact names, globs (`@scope/*`), paths (`./packages/api`),
    /// graph selectors (`pkg...`, `...pkg`), git-ref selectors
    /// (`[origin/main]`), and exclusions (`!pkg`).
    #[usage(short = 'F', long, value_name = "PATTERN")]
    filter: Vec<String>,

    /// Run across every workspace package (same as `--filter=*`).
    #[usage(short = 'r', long)]
    recursive: bool,

    /// Production-only variant of `--filter`: graph walks skip
    /// devDependencies.
    #[usage(long, value_name = "PATTERN")]
    filter_prod: Vec<String>,

    /// Error when a workspace selector matches no packages (default: warn
    /// and exit 0).
    #[usage(long)]
    fail_if_no_match: bool,

    /// Include the workspace root alongside the selected packages.
    #[usage(long)]
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

/// The family's wrapper shape, on top of the shared root stamper: the engine
/// args, optionally the selector flags, and `-C/--dir`.
macro_rules! info_cli {
    ($name:ident, $spec:tt, $engine:ty) => {
        crate::pm_engine::publish_family::verb_cli! {
            $name, $spec, {
                #[usage(flatten)]
                args: $engine,
                /// Change to directory before running.
                #[usage(short = 'C', long = "dir", value_name = "DIR")]
                dir: Option<PathBuf>,
            }
        }
    };
    ($name:ident, $spec:tt, $engine:ty, filter) => {
        crate::pm_engine::publish_family::verb_cli! {
            $name, $spec, {
                #[usage(flatten)]
                args: $engine,
                #[usage(flatten)]
                filter: FilterFlags,
                /// Change to directory before running.
                #[usage(short = 'C', long = "dir", value_name = "DIR")]
                dir: Option<PathBuf>,
            }
        }
    };
}

info_cli!(ListCli, "nub list", aube::commands::list::ListArgs, filter);
info_cli!(WhyCli, "nub why", aube::commands::why::WhyArgs, filter);
info_cli!(
    OutdatedCli,
    "nub outdated",
    aube::commands::outdated::OutdatedArgs,
    filter
);
info_cli!(
    QueryCli,
    "nub query",
    aube::commands::query::QueryArgs,
    filter
);
info_cli!(AuditCli, "nub audit", aube::commands::audit::AuditArgs);
info_cli!(
    LicensesCli,
    "nub licenses",
    aube::commands::licenses::LicensesArgs
);
info_cli!(
    DeprecationsCli,
    "nub deprecations",
    aube::commands::deprecations::DeprecationsArgs
);
info_cli!(ViewCli, "nub view", aube::commands::view::ViewArgs);
info_cli!(CheckCli, "nub check", aube::commands::check::CheckArgs);
info_cli!(BinCli, "nub bin", aube::commands::bin::BinArgs);
info_cli!(RootCli, "nub root", aube::commands::root::RootArgs);

// `peers` is the family's one subcommand-bearing engine type, and usage
// refuses to flatten a group that declares subcommands (the parent's tables
// have nowhere to put them). The root re-declares the engine's own enum and
// rebuilds `PeersArgs` at dispatch, so the flag surface still comes from
// upstream.
verb_cli! {
    PeersCli, "nub peers", {
        #[usage(subcommand)]
        command: aube::commands::peers::PeersCommand,
        /// Change to directory before running.
        #[usage(short = 'C', long = "dir", value_name = "DIR")]
        dir: Option<PathBuf>,
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
    let mut cli = match ListCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
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
    let cli = match WhyCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
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
    let cli = match OutdatedCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
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
    let cli = match QueryCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
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
    let cli = match AuditCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
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
    let args = widen_licenses_marker(args);
    let cli = match LicensesCli::parse_argv(&format!("nub {typed}"), &args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
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
    let cli = match DeprecationsCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
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
    let cli = match PeersCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    let engine = aube::commands::peers::PeersArgs {
        command: cli.command,
    };
    // Missing lockfile is a miette error (`load_graph`) — rewrite covers it.
    finish_code(session.runtime.block_on(aube::commands::peers::run(engine)))
}

fn run_view(typed: &str, args: &[String]) -> Result<i32> {
    let cli = match ViewCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
    };
    // Pure registry query: no lockfile involvement at all, so identity
    // resolution is lenient (see `engine_session_global`) — a multi-lockfile
    // project must not block `nub view <pkg>`.
    let session = super::engine_session_global(cli.dir.as_deref())?;
    finish(
        session
            .runtime
            .block_on(aube::commands::view::run(cli.args)),
    )
}

/// Widen the `licenses` compat marker: pnpm's documented spelling is
/// `pnpm licenses list`, but the engine's hidden positional declares
/// `choices("ls")` alone.
///
/// usage builds its tables statically, so there is no `mut_arg` to widen the
/// choice set at run time — the argv is normalized instead. The engine ignores
/// the marker's value entirely (`let _ = args.subcommand`), so mapping `list`
/// onto `ls` is invisible downstream, and it keeps the wrapper flattening
/// upstream's struct rather than hand-mirroring its flags.
///
/// Only the token that would LAND on the positional is rewritten: the scan
/// skips a value-taking flag's value, so `--registry list` is untouched.
fn widen_licenses_marker(args: &[String]) -> Vec<String> {
    let value_flags = licenses_separate_value_flags();
    let mut out = args.to_vec();
    let mut i = 0;
    while i < out.len() {
        if out[i] == "--" {
            break;
        }
        if out[i].starts_with('-') {
            let takes_value = value_flags.iter().any(|flag| flag == &out[i]);
            i += if takes_value { 2 } else { 1 };
            continue;
        }
        if out[i] == "list" {
            out[i] = "ls".to_string();
        }
        break;
    }
    out
}

/// The `licenses` wrapper's separate-value flag spellings, derived from the
/// same tables the parse uses so a new value-taking flag can't make the marker
/// scan misread its value as the positional.
fn licenses_separate_value_flags() -> &'static [String] {
    static FLAGS: OnceLock<Vec<String>> = OnceLock::new();
    FLAGS.get_or_init(|| separate_value_flags(LicensesCli::command()))
}

fn run_check(typed: &str, args: &[String]) -> Result<i32> {
    let cli = match CheckCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
    };
    let session = super::engine_session_quiet(cli.dir.as_deref())?;
    // Reads the *resolved* virtual store (node_modules/.store under nub's
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
    let cli = match BinCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
    };
    // Pure path print (`<modulesDir>/.bin`, or the global bin dir under
    // `-g`); the directory need not exist and no lockfile is read, so identity
    // resolution is lenient (see `engine_session_global`).
    let session = super::engine_session_global(cli.dir.as_deref())?;
    finish(session.runtime.block_on(aube::commands::bin::run(cli.args)))
}

fn run_root(typed: &str, args: &[String]) -> Result<i32> {
    let cli = match RootCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(c) => c,
        Parsed::Exit(code) => return Ok(code),
    };
    // Pure path print (`<modulesDir>`, or the global package dir under `-g`);
    // no lockfile is read, so identity resolution is lenient (see
    // `engine_session_global`).
    let session = super::engine_session_global(cli.dir.as_deref())?;
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
    use super::*;

    /// Parse a wrapper's argv (the words AFTER the verb), panicking on any
    /// outcome that is not a real parse.
    macro_rules! parse {
        ($root:ident, $argv:expr) => {
            match $root::parse_argv(concat!("nub ", stringify!($root)), &owned($argv)) {
                Parsed::Ok(cli) => cli,
                Parsed::Exit(code) => panic!("argv must parse, settled with exit {code}"),
            }
        };
    }

    fn owned(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|s| s.to_string()).collect()
    }

    /// One representative parse per wrapper: proves the engine flag surface
    /// (positional + shorts + value enums + flattened NetworkArgs) and the
    /// nub-side additions (-C, filter flags) coexist on every verb.
    #[test]
    fn wrappers_parse_the_engine_arg_surface() {
        let list = parse!(ListCli, &["lodash", "--json", "-r", "--depth", "2"]);
        assert_eq!(list.args.pattern.as_deref(), Some("lodash"));
        assert!(list.args.json && list.filter.recursive);

        let why = parse!(WhyCli, &["debug", "--parseable", "-F", "app..."]);
        assert_eq!(why.args.package, "debug");
        assert!(why.args.parseable);
        assert_eq!(why.filter.filter, vec!["app..."]);

        let outdated = parse!(OutdatedCli, &["-w", "--json", "-C", "/tmp/x"]);
        assert!(outdated.args.workspace_root && outdated.args.json);
        assert_eq!(outdated.dir.as_deref(), Some(Path::new("/tmp/x")));

        let audit = parse!(AuditCli, &["--audit-level", "high", "--fix=update"]);
        assert_eq!(audit.args.fix, Some(FixMode::Update));

        let licenses = parse!(LicensesCli, &["ls", "--json"]);
        assert_eq!(licenses.args.subcommand.as_deref(), Some("ls"));

        let deprecations = parse!(DeprecationsCli, &["--exit-code", "--transitive"]);
        assert!(deprecations.args.exit_code && deprecations.args.transitive);

        let peers = parse!(PeersCli, &["check", "--json"]);
        let aube::commands::peers::PeersCommand::Check(check) = peers.command;
        assert!(check.json);

        let query = parse!(QueryCli, &[":deprecated", "--parseable"]);
        assert_eq!(query.args.selector, ":deprecated");

        let view = parse!(ViewCli, &["react@next", "dist.tarball"]);
        assert_eq!(view.args.package.as_deref(), Some("react@next"));
        assert_eq!(view.args.field.as_deref(), Some("dist.tarball"));

        let check = parse!(CheckCli, &["--json"]);
        assert!(check.args.json);
        let bin = parse!(BinCli, &["-g"]);
        assert!(bin.args.global);
        let root = parse!(RootCli, &["--global", "-C", "/tmp/x"]);
        assert!(root.args.global);
        assert_eq!(root.dir.as_deref(), Some(Path::new("/tmp/x")));
    }

    /// The licenses wrapper admits pnpm's documented `list` spelling beside
    /// the engine's `ls` (and still rejects arbitrary positionals). The
    /// normalizer maps `list` onto the engine's only declared choice, and the
    /// engine ignores the marker's value.
    #[test]
    fn licenses_wrapper_accepts_pnpms_list_spelling() {
        for sub in ["ls", "list"] {
            let argv = widen_licenses_marker(&owned(&[sub, "--json"]));
            match LicensesCli::parse_argv("nub licenses", &argv) {
                Parsed::Ok(cli) => {
                    assert_eq!(cli.args.subcommand.as_deref(), Some("ls"));
                    assert!(cli.args.json);
                }
                Parsed::Exit(code) => panic!("licenses {sub} must parse, settled with {code}"),
            }
        }
        // The marker only claims the token that would land on the positional:
        // a value-taking flag's value is left alone.
        assert_eq!(
            widen_licenses_marker(&owned(&["--registry", "list"])),
            owned(&["--registry", "list"])
        );

        let bad = LicensesCli::parse_argv("nub licenses", &owned(&["everything"]));
        assert!(
            matches!(bad, Parsed::Exit(code) if code != 0),
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
        for (help, name) in [
            (ListCli::long_help("nub list"), "nub list"),
            (WhyCli::long_help("nub why"), "nub why"),
            (OutdatedCli::long_help("nub outdated"), "nub outdated"),
            (QueryCli::long_help("nub query"), "nub query"),
            (AuditCli::long_help("nub audit"), "nub audit"),
            (LicensesCli::long_help("nub licenses"), "nub licenses"),
            (
                DeprecationsCli::long_help("nub deprecations"),
                "nub deprecations",
            ),
            (PeersCli::long_help("nub peers"), "nub peers"),
            (ViewCli::long_help("nub view"), "nub view"),
            (CheckCli::long_help("nub check"), "nub check"),
            (BinCli::long_help("nub bin"), "nub bin"),
            (RootCli::long_help("nub root"), "nub root"),
        ] {
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
