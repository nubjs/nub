//! aube's command layer as a library.
//!
//! The `aube` / `aubr` / `aubx` binaries are thin wrappers over
//! [`cli_main`]; everything else lives here so the command layer can be
//! embedded by other tools — e.g. constructing
//! [`commands::install::InstallOptions`] and calling
//! [`commands::install::run`] in-process instead of shelling out to the
//! CLI. The public surface is deliberately small: [`commands`] (command
//! implementations and their args/options structs), [`cli_args`] (the
//! shared clap argument groups flattened into command args), and
//! [`cli_main`]. Everything else is crate-private plumbing shared by the
//! commands.
//!
//! The library makes no global-allocator choice — the mimalloc opt-in
//! lives in `src/main.rs` so embedders keep control of their own
//! allocator.

mod argv;
pub mod cli_args;
pub mod commands;
mod dep_chain;
mod deprecations;
mod dirs;
mod engines;
mod patches;
mod pnpmfile;
mod progress;
mod runtime;
mod self_version;
mod startup;
mod state;
mod update_check;
mod version;

use argv::{extract_config_overrides, lift_per_subcommand_flags, rewrite_multicall_argv};
use clap::{Parser, Subcommand, ValueEnum};
use miette::{Context, IntoDiagnostic};
use startup::{
    ColorMode, PackageManagerGuard, ci_renders_ansi, command_needs_package_manager_guard,
    compute_effective_filter, diag_config_from_flag, enforce_package_manager_guardrails,
    env_disables_color, init_logging, load_startup_settings, raise_nofile_limit,
    resolve_color_mode, resolve_loglevel, startup_cwd,
};
#[cfg(test)]
use startup::{PackageManagerGuardMode, PackageManagerStrictMode, package_manager_guard_mode};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "aube",
    about = "A fast Node.js package manager",
    version = version::VERSION_LONG.as_str(),
    disable_version_flag = true
)]
pub(crate) struct Cli {
    /// Change to directory before running (like `make -C` or `mise --cd`)
    #[arg(short = 'C', long = "dir", visible_aliases = ["cd", "prefix"], global = true, value_name = "DIR")]
    dir: Option<std::path::PathBuf>,

    /// Scope command execution to workspace packages matching PATTERN.
    ///
    /// Supports exact names (`my-pkg`), globs (`@scope/*`, `*-plugin`),
    /// paths (`./packages/api`), graph selectors (`pkg...`, `...pkg`),
    /// git-ref selectors (`[origin/main]`), and exclusions (`!pkg`).
    /// Repeatable; matches are OR-ed.
    ///
    /// Currently honored by `run`, `test`, `start`, `stop`, `restart`,
    /// `install`, `exec`, `list`, `publish`, `deploy`, `add`, `remove`,
    /// `update`, `why`, and implicit-script invocations.
    #[arg(short = 'F', long, global = true, value_name = "PATTERN")]
    filter: Vec<String>,

    /// Run the command across every workspace package.
    ///
    /// Equivalent to `--filter=*`; if `--filter` is also given,
    /// `--recursive` is a no-op and the explicit filter wins. Honored
    /// by the same commands as `--filter`.
    #[arg(short = 'r', long, global = true)]
    recursive: bool,

    /// Enable verbose/debug logging (shortcut for `--loglevel debug`)
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Print version and check for updates.
    ///
    /// Manual flag so we can run the async update notifier alongside
    /// the version print — clap's auto `Action::Version` exits inside
    /// `parse_from`, before the tokio runtime is built.
    #[arg(short = 'V', long = "version", global = true)]
    version: bool,

    /// Group workspace command output after each package finishes.
    ///
    /// Accepted for pnpm compatibility; aube's workspace fanout is
    /// currently sequential, so output is already grouped.
    #[arg(long, global = true, conflicts_with = "stream", hide = true)]
    aggregate_output: bool,

    /// Force colored output even when stderr is not a TTY.
    ///
    /// Overrides `NO_COLOR` / `CLICOLOR=0`. Mutually exclusive with
    /// `--no-color`.
    #[arg(long, global = true, conflicts_with = "no_color")]
    color: bool,

    /// Enable cold-install deep diagnostics. Modes:
    ///   summary  — sum_ms / mean / max / %wall table at end
    ///   trace    — summary + critical path + starvation + what-if + lifecycle
    ///   live     — like trace, plus print every span >= 100ms to stderr live
    ///   full     — like trace, plus write JSONL trace to a file (defaults to ./aube-diag.jsonl)
    ///
    /// Quick form: `--diag` with no value defaults to `trace`.
    /// Output file path can be set via `--diag-file`. Threshold for live
    /// mode via `--diag-threshold-ms`.
    #[arg(long, global = true, value_name = "MODE", num_args = 0..=1, default_missing_value = "trace")]
    diag: Option<String>,

    /// Path for `--diag full` JSONL trace (default: ./aube-diag.jsonl)
    #[arg(long, global = true, value_name = "PATH")]
    diag_file: Option<PathBuf>,

    /// Live-mode threshold: only print spans whose duration is >= N ms (default 100).
    #[arg(long, global = true, value_name = "MS")]
    diag_threshold_ms: Option<u64>,

    /// Error when a workspace selector matches no packages.
    ///
    /// Accepted globally; selected commands already fail on empty matches.
    #[arg(long, global = true)]
    fail_if_no_match: bool,

    /// Production-only variant of `--filter`.
    ///
    /// Same selector grammar as `--filter`, but graph walks (`pkg...`,
    /// `...pkg`) only follow `dependencies` / `optionalDependencies` /
    /// `peerDependencies` edges — `devDependencies` (and packages
    /// reachable solely through them) are skipped. Non-graph forms
    /// (exact name, glob, path, `[git-ref]`) behave identically to
    /// `--filter`. Repeatable; can be combined with `--filter`.
    #[arg(long, global = true, value_name = "PATTERN")]
    filter_prod: Vec<String>,

    /// Ignore workspace discovery for commands that support workspace fanout.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, global = true, hide = true)]
    ignore_workspace: bool,

    /// Include the workspace root in recursive workspace operations.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, global = true, hide = true)]
    include_workspace_root: bool,

    /// Set the log level. Logs at or above this level are shown.
    #[arg(long, global = true, value_name = "LEVEL", value_enum)]
    loglevel: Option<LogLevel>,

    /// Disable colored output.
    ///
    /// Overrides `FORCE_COLOR` / `CLICOLOR_FORCE` and sets `NO_COLOR=1`
    /// so downstream libraries (miette, clx, child processes) all see
    /// the same choice.
    #[arg(long, global = true)]
    no_color: bool,

    /// Output format: default, append-only, ndjson, silent.
    ///
    /// `default` renders the progress UI when stderr is a TTY;
    /// `append-only` disables the progress UI in favor of plain
    /// line-at-a-time logs; `ndjson` swaps the tracing fmt layer for
    /// the JSON formatter (one JSON object per log event on stderr)
    /// and is what tooling wrappers should consume; `silent`
    /// suppresses all non-error output (alias for `--loglevel silent`).
    #[arg(long, global = true, value_name = "NAME", value_enum)]
    reporter: Option<ReporterType>,

    /// Suppress all non-error output (alias for `--loglevel silent`)
    #[arg(long, global = true)]
    silent: bool,

    /// Stream workspace command output as each child process writes it.
    ///
    /// Accepted for pnpm compatibility; aube's workspace fanout is
    /// currently sequential.
    #[arg(long, global = true, conflicts_with = "aggregate_output", hide = true)]
    stream: bool,

    /// Route lifecycle and workspace command output through stderr.
    ///
    /// Accepted for pnpm compatibility.
    #[arg(long, global = true, hide = true)]
    use_stderr: bool,

    /// Prefer workspace packages when resolving dependencies.
    ///
    /// Parsed for pnpm compatibility; aube already resolves workspace
    /// packages when a workspace is present.
    #[arg(long, global = true, hide = true)]
    workspace_packages: bool,

    /// Run from the workspace root regardless of the current package.
    #[arg(long, global = true)]
    workspace_root: bool,

    /// Automatically answer yes to prompts.
    ///
    /// Parsed for pnpm compatibility; aube does not currently prompt
    /// on these paths.
    #[arg(short = 'y', long, global = true, hide = true)]
    yes: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Silent,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub(crate) enum ReporterType {
    Default,
    AppendOnly,
    Ndjson,
    Silent,
}

impl LogLevel {
    fn filter(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
            LogLevel::Silent => "off",
        }
    }
}

/// Redirects stderr (fd 2) to `/dev/null` for its lifetime, restoring the
/// original on drop. Used by `--silent` to suppress the ~230 direct
/// `eprintln!` calls scattered across command implementations without
/// rewriting them all. The guard must be dropped *before* `main` returns
/// so that any `miette` error report bubbled up through `?` is printed to
/// the real stderr. Stdout is left alone — `aube --silent config get foo`
/// should still emit data to a pipe.
struct SilentStderrGuard {
    saved: libc::c_int,
}

impl SilentStderrGuard {
    fn install() -> Option<Self> {
        unsafe {
            let saved = libc::dup(2);
            if saved < 0 {
                return None;
            }
            let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
            if devnull < 0 {
                libc::close(saved);
                return None;
            }
            if libc::dup2(devnull, 2) < 0 {
                libc::close(devnull);
                libc::close(saved);
                return None;
            }
            libc::close(devnull);
            Some(Self { saved })
        }
    }
}

impl Drop for SilentStderrGuard {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.saved, 2);
            libc::close(self.saved);
        }
    }
}

// Commands are listed in alphabetical order; validated by
// `cli_ordering_tests::test_cli_ordering`. Per-command arg fields are
// similarly sorted: positional first, then short flags by short option,
// then long-only flags alphabetically. The `External` catch-all is last
// because clap's external_subcommand must come after named variants; it
// has no fixed name so the sort check skips it.
#[derive(Subcommand)]
enum Commands {
    /// Bootstrap aube's cached node-gyp and print the executable path.
    #[command(name = "__node-gyp-bootstrap", hide = true)]
    NodeGypBootstrap { project_dir: PathBuf },
    /// Add a dependency
    #[command(visible_alias = "a")]
    Add(commands::add::AddArgs),
    /// Approve ignored dependency build scripts.
    ///
    /// Writes entries under `allowBuilds` in `aube-workspace.yaml` (or
    /// `pnpm-workspace.yaml` if present).
    ApproveBuilds(commands::approve_builds::ApproveBuildsArgs),
    /// Check installed packages against the registry advisory DB
    #[command(after_long_help = commands::audit::AFTER_LONG_HELP)]
    Audit(commands::audit::AuditArgs),
    /// Print the path to `node_modules/.bin`
    #[command(after_long_help = commands::bin::AFTER_LONG_HELP)]
    Bin(commands::bin::BinArgs),
    /// Inspect and manage the packument metadata cache
    Cache(commands::cache::CacheArgs),
    /// Print a file from the global store by integrity or hex hash
    CatFile(commands::cat_file::CatFileArgs),
    /// Print the cached package index JSON for `<name>@<version>`
    CatIndex(commands::cat_index::CatIndexArgs),
    /// Verify installed packages can resolve their declared deps.
    ///
    /// Walks the `node_modules/` symlink tree and confirms every
    /// dependency in each `package.json` resolves to a real entry.
    #[command(after_long_help = commands::check::AFTER_LONG_HELP)]
    Check(commands::check::CheckArgs),
    /// Clean install: delete node_modules, then install with frozen lockfile.
    ///
    /// Use in CI to guarantee a reproducible install from the committed lockfile.
    #[command(visible_alias = "clean-install", aliases = ["ic", "install-clean"])]
    Ci(commands::ci::CiArgs),
    /// Remove `node_modules` across every workspace project.
    ///
    /// `--lockfile` / `-l` also deletes lockfiles. A `clean` script in
    /// the root `package.json` overrides the built-in.
    Clean(commands::clean::CleanArgs),
    /// Generate shell completions (bash, zsh, fish)
    Completion(commands::completion::CompletionArgs),
    /// Read and write settings in `.npmrc`
    #[command(alias = "c")]
    Config(commands::config::ConfigArgs),
    /// Scaffold a project from a `create-*` starter kit (via dlx)
    Create(commands::create::CreateArgs),
    /// Re-resolve the lockfile to collapse duplicate versions
    Dedupe(commands::dedupe::DedupeArgs),
    /// Deploy a workspace package into a target directory with deps inlined
    Deploy(commands::deploy::DeployArgs),
    /// Mark published versions of a package as deprecated on the registry
    Deprecate(commands::deprecate::DeprecateArgs),
    /// Report deprecated packages in the resolved dependency graph
    Deprecations(commands::deprecations::DeprecationsArgs),
    /// Diagnostic trace analysis (compare/analyze JSONL traces)
    Diag(commands::diag::DiagArgs),
    /// Manage package distribution tags on the registry
    #[command(visible_alias = "dist-tags")]
    DistTag(commands::dist_tag::DistTagArgs),
    /// Fetch a package into a throwaway environment and run its binary
    Dlx(commands::dlx::DlxArgs),
    /// Run broad install-health diagnostics
    #[command(after_long_help = commands::doctor::AFTER_LONG_HELP)]
    Doctor(commands::doctor::DoctorArgs),
    /// Execute a locally installed binary
    #[command(visible_alias = "x")]
    Exec(commands::exec::ExecArgs),
    /// Download lockfile dependencies into the store without linking node_modules
    Fetch(commands::fetch::FetchArgs),
    /// List packages whose cached index references a given file hash
    #[command(after_long_help = commands::find_hash::AFTER_LONG_HELP)]
    FindHash(commands::find_hash::FindHashArgs),
    /// Alias for `config get` (hidden; prefer `config get`)
    #[command(hide = true)]
    Get(commands::config::GetArgs),
    /// Print packages whose install scripts were skipped by `pnpm.allowBuilds`
    #[command(after_long_help = commands::ignored_builds::AFTER_LONG_HELP)]
    IgnoredBuilds(commands::ignored_builds::IgnoredBuildsArgs),
    /// Convert a supported lockfile into aube-lock.yaml
    Import(commands::import::ImportArgs),
    /// Create a `package.json` in the current directory
    Init(commands::init::InitArgs),
    /// Install all dependencies
    #[command(alias = "i")]
    Install(commands::install::InstallArgs),
    /// Install dependencies, then run the `test` script (pnpm compat alias).
    ///
    /// Hidden from help because `aube test` already auto-installs.
    #[command(alias = "it", hide = true)]
    InstallTest(commands::run::ScriptArgs),
    /// Alias for `list --long` (hidden; prefer `list --long`)
    #[command(hide = true)]
    La(commands::list::ListArgs),
    /// Report the licenses of installed dependencies
    #[command(after_long_help = commands::licenses::AFTER_LONG_HELP)]
    Licenses(commands::licenses::LicensesArgs),
    /// Link a local package globally, or into the current project
    #[command(visible_alias = "ln")]
    Link(commands::link::LinkArgs),
    /// Print the installed dependency tree
    #[command(visible_alias = "ls", after_long_help = commands::list::AFTER_LONG_HELP)]
    List(commands::list::ListArgs),
    /// Alias for `list --long` (hidden; prefer `list --long`)
    #[command(hide = true)]
    Ll(commands::list::ListArgs),
    /// Store a registry auth token in the user's ~/.npmrc
    #[command(alias = "adduser")]
    Login(commands::login::LoginArgs),
    /// Remove a registry auth token from the user's ~/.npmrc
    Logout(commands::logout::LogoutArgs),
    /// Report dependencies whose installed version lags behind the registry
    #[command(after_long_help = commands::outdated::AFTER_LONG_HELP)]
    Outdated(commands::outdated::OutdatedArgs),
    /// Manage package owners (not implemented — use `npm owner`)
    #[command(hide = true)]
    Owner(commands::npm_fallback::FallbackArgs),
    /// Create a publishable `.tgz` tarball from the current project
    Pack(commands::pack::PackArgs),
    /// Extract a package into an edit directory so it can be patched
    Patch(commands::patch::PatchArgs),
    /// Generate a `.patch` file from a `aube patch` edit directory
    PatchCommit(commands::patch_commit::PatchCommitArgs),
    /// Remove patch entries from `pnpm.patchedDependencies`
    PatchRemove(commands::patch_remove::PatchRemoveArgs),
    /// Inspect peer-dependency resolution from the lockfile
    Peers(commands::peers::PeersArgs),
    /// Manage package.json entries (not implemented — use `npm pkg`)
    #[command(hide = true)]
    Pkg(commands::npm_fallback::FallbackArgs),
    /// Remove extraneous packages from project `node_modules`.
    ///
    /// Reads the lockfile, computes the packages still reachable from each
    /// importer, and removes stale top-level links, stale virtual-store entries,
    /// and dangling .bin links. Does not modify package.json or the lockfile.
    #[command(after_long_help = commands::prune::AFTER_LONG_HELP)]
    Prune(commands::prune::PruneArgs),
    /// Publish the current package to the registry
    Publish(commands::publish::PublishArgs),
    /// Alias for `clean` — remove `node_modules` across every workspace project.
    ///
    /// A `purge` script in the root `package.json` overrides the built-in.
    Purge(commands::clean::CleanArgs),
    /// Query packages in the resolved dependency graph
    #[command(after_long_help = commands::query::AFTER_LONG_HELP)]
    Query(commands::query::QueryArgs),
    /// Re-run root lifecycle scripts and allowlisted dependency builds
    #[command(visible_alias = "rb")]
    Rebuild(commands::rebuild::RebuildArgs),
    /// Run a supported command across workspace packages
    #[command(visible_aliases = ["multi", "m"])]
    Recursive(commands::recursive::RecursiveArgs),
    /// Remove a dependency
    #[command(visible_alias = "rm", aliases = ["uninstall", "un", "uni"])]
    Remove(commands::remove::RemoveArgs),
    /// Restart a package (shortcut for `run restart`; falls back to `stop` + `start`)
    Restart(commands::run::ScriptArgs),
    /// Print the path to `node_modules`
    #[command(after_long_help = commands::root::AFTER_LONG_HELP)]
    Root(commands::root::RootArgs),
    /// Run a script defined in package.json
    #[command(alias = "run-script")]
    Run(commands::run::RunArgs),
    /// Manage the project's Node.js runtime (pin, install, inspect)
    #[command(visible_alias = "rt")]
    Runtime(commands::runtime::RuntimeArgs),
    /// Generate a Software Bill of Materials (CycloneDX or SPDX)
    Sbom(commands::sbom::SbomArgs),
    /// Search the registry for packages (not implemented — use `npm search`)
    #[command(hide = true)]
    Search(commands::npm_fallback::FallbackArgs),
    /// Alias for `config set` (hidden; prefer `config set`)
    #[command(hide = true)]
    Set(commands::config::SetArgs),
    /// Set a `package.json` script (not implemented — use `npm set-script`)
    #[command(hide = true, name = "set-script")]
    SetScript(commands::npm_fallback::FallbackArgs),
    /// Show the companies sponsoring aube and the jdx.dev project family
    Sponsors(commands::sponsors::SponsorsArgs),
    /// Stage packages for publishing (not implemented — use `npm stage`)
    Stage(commands::npm_fallback::FallbackArgs),
    /// Start a package (shortcut for `run start`)
    Start(commands::run::ScriptArgs),
    /// Stop a package (shortcut for `run stop`)
    Stop(commands::run::ScriptArgs),
    /// Manage the global store
    Store(commands::store::StoreArgs),
    /// Run the `test` script (shortcut for `run test`)
    #[command(visible_alias = "t")]
    Test(commands::run::ScriptArgs),
    /// Manage registry auth tokens (not implemented — use `npm token`)
    #[command(hide = true)]
    Token(commands::npm_fallback::FallbackArgs),
    /// Clear an existing deprecation on the registry
    Undeprecate(commands::undeprecate::UndeprecateArgs),
    /// Unlink a package (remove linked entries from node_modules)
    #[command(alias = "dislink")]
    Unlink(commands::unlink::UnlinkArgs),
    /// Remove a package (or a single version) from the registry
    Unpublish(commands::unpublish::UnpublishArgs),
    /// Update dependencies
    #[command(aliases = ["up", "upgrade"])]
    Update(commands::update::UpdateArgs),
    /// Bump the version in package.json (and optionally create a git commit + tag)
    Version(commands::version::VersionArgs),
    /// Print package metadata from the registry
    #[command(visible_aliases = ["info", "show"], alias = "v", after_long_help = commands::view::AFTER_LONG_HELP)]
    View(commands::view::ViewArgs),
    /// Report the current registry user (not implemented — use `npm whoami`)
    #[command(hide = true)]
    Whoami(commands::npm_fallback::FallbackArgs),
    /// Print reverse dependency chains explaining why a package is installed
    #[command(visible_alias = "w", after_long_help = commands::why::AFTER_LONG_HELP)]
    Why(commands::why::WhyArgs),
    /// Catch-all for implicit script execution (e.g., `aube dev` = `aube run dev`)
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Library entry point. An embedder calls this with its own `&'static
/// Embedder` (and optional setting defaults); the `aube` binary passes
/// `&aube_util::AUBE` and no defaults, reproducing standalone behavior.
/// This is the whole embedding API: register-then-run in one call, so a host
/// never has to separately wire identity and defaults.
///
/// **Returns the exit code; it does not terminate the process.** It parses
/// argv, runs the selected command, renders any diagnostic to stderr, and
/// hands back the code the binary's `main` should exit with. Returning rather
/// than calling `std::process::exit` keeps it embed-safe: a host that drives
/// it in-process is not hard-killed by a non-zero result or an error. The
/// standalone binary does `std::process::exit(cli_main(..))`.
///
/// `#[must_use]`: the `i32` is the exit code, not a side effect. An
/// embedder migrating off the old `process::exit` entrypoint that drops
/// it would silently exit 0 on every failure — Rust won't warn on an
/// ignored return — so the lint nudges them to
/// `std::process::exit(cli_main(..))`.
#[must_use]
pub fn cli_main(embedder: &'static aube_util::Embedder) -> i32 {
    cli_main_with_defaults(embedder, Vec::new())
}

/// The clap [`Command`](clap::Command) for the CLI, with its version reset to
/// the plain package version (stripping the `-DEBUG` runtime suffix) so any
/// derived artifact stays byte-stable across profiles. This exposes the
/// command surface itself — not aube's own `usage` KDL subcommand, which is
/// aube-specific tooling that lives in the binary (`src/main.rs`), not in the
/// embeddable command layer. A downstream embedder builds its own top-level
/// usage/completions from this.
pub fn command() -> clap::Command {
    use clap::CommandFactory;
    Cli::command().version(env!("CARGO_PKG_VERSION"))
}

/// [`cli_main`] plus embedder-supplied setting defaults. The `defaults` are
/// `(canonical_setting_name, raw_value)` pairs registered at the lowest
/// precedence tier — below every user- and project-level source — for the
/// genuinely user-overridable knobs an embedder wants to re-default.
/// (Embedder-*fixed* behavior lives on [`aube_util::Embedder`] itself, not
/// here.) Standalone aube passes an empty vec, so per-setting built-in
/// defaults apply unchanged.
///
/// `#[must_use]` for the same reason as [`cli_main`]: the returned `i32`
/// is the exit code the embedder must hand to `std::process::exit`.
#[must_use]
pub fn cli_main_with_defaults(
    embedder: &'static aube_util::Embedder,
    defaults: Vec<(String, String)>,
) -> i32 {
    // Register the binary's embedder profile before anything reads branding,
    // and its setting defaults before anything resolves settings. Both are
    // idempotent — a no-op if already set (e.g. a test harness that
    // registered one first).
    aube_util::set_embedder(embedder);
    aube_settings::set_embedder_defaults(defaults);

    // Two-phase wrapper: `inner_main` runs the real CLI and returns
    // `Result<i32, miette::Report>` — the command's exit code on Ok. On
    // Err we render via miette's fancy handler (matching the previous
    // `Termination` behavior), then look up the diagnostic's `code()`
    // against `aube_codes::exit::EXIT_TABLE` to pick a bespoke exit code.
    // Codes outside the table fall through to `EXIT_GENERIC` (1).
    //
    // Chain a panic hook that flushes the diag buffer before the
    // default hook prints the panic. Without this, a debug-build panic
    // (release uses `panic = "abort"` so the hook would not run anyway)
    // would lose the BufWriter's 64 KiB tail and any unflushed events.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        aube_util::diag::flush();
        prev_hook(info);
    }));
    let result = inner_main();
    aube_util::diag::flush();
    // Drain any in-flight slow-metadata group whose debounce window
    // hasn't fired yet. install pipelines also flush at end-of-resolve
    // (for in-progress UX), but non-install commands — `aube add`,
    // `aube audit`, `aube deprecate`, `aube deprecations`, `aube view`,
    // etc. — never hit that path and would otherwise silently lose
    // their slow-fetch warnings to the accumulator.
    aube_registry::slow_metadata::flush_summary();
    // Return the exit code rather than terminating: only the binary's
    // `main` calls `std::process::exit`, so a host embedding the command
    // layer in-process isn't hard-killed by a non-zero result or an
    // error. The diagnostic still renders to stderr here (matching the
    // previous `Termination` behavior); only the exit itself moves out.
    match result {
        Ok(code) => code,
        Err(report) => {
            eprintln!("{report:?}");
            report_exit_code(&report)
        }
    }
}

/// Resolve a diagnostic's exit code by walking its `code()` chain.
/// Falls back to `EXIT_GENERIC` (1) when no `code` is set or the
/// reported code has no entry in `aube_codes::exit::EXIT_TABLE`.
fn report_exit_code(report: &miette::Report) -> i32 {
    if let Some(code) = report.code() {
        let code = code.to_string();
        if let Some(exit) = aube_codes::exit::exit_code_for(&code) {
            return exit;
        }
    }
    aube_codes::exit::EXIT_GENERIC
}

fn inner_main() -> miette::Result<i32> {
    let mut argv: Vec<OsString> = std::env::args_os().collect();
    // pnpm-compat: pull `--config.<key>[=<value>]` out of argv before
    // clap parses it. Stripping here means the rest of the binary sees
    // a clean argv, and the parsed pairs feed every `ResolveCtx::cli`
    // through the process-global slot in `aube_settings`.
    let config_overrides = extract_config_overrides(&mut argv);
    aube_settings::set_global_cli_overrides(config_overrides);
    // Override the clap command name at runtime with the active embedder's
    // name. The `#[command(name = "aube")]` attribute is a compile-time
    // constant and can't read `embedder()`, so help/usage/error output would
    // otherwise always say "aube" even under an embedder. `get_matches_from`
    // keeps clap's parse-error / `--help` / `--version` print-and-exit
    // behavior, matching the previous `parse_from`.
    let cli = {
        use clap::{CommandFactory, FromArgMatches};
        let matches = Cli::command()
            .name(aube_util::embedder().name)
            .get_matches_from(lift_per_subcommand_flags(rewrite_multicall_argv(argv)));
        Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
    };

    // `--color` / `--no-color` take effect before anything else touches
    // color state: we translate the flags into the env vars that miette,
    // clx, `supports-color`, and spawned child processes all already
    // consult, so the choice is consistent across every output path and
    // inherits into `run` / `exec` / lifecycle scripts. The explicit flag
    // wins over whatever was in the environment — that's what pnpm does.
    //
    // This has to happen *before* we build the Tokio runtime: the Rust
    // 2024 contract on `std::env::set_var` requires that no other
    // threads exist, and a multi-threaded runtime spawns its worker
    // pool during `build()`. So we keep `main` synchronous, mutate env
    // here, and only then enter the async body.
    let color_mode = resolve_color_mode(&cli);
    if matches!(color_mode, ColorMode::Never) {
        // SAFETY: single-threaded `main` — no other threads exist yet.
        unsafe {
            std::env::set_var("NO_COLOR", "1");
            std::env::remove_var("FORCE_COLOR");
            std::env::remove_var("CLICOLOR_FORCE");
        }
    } else if matches!(color_mode, ColorMode::Always) {
        // SAFETY: single-threaded `main` — no other threads exist yet.
        unsafe {
            std::env::set_var("FORCE_COLOR", "1");
            std::env::set_var("CLICOLOR_FORCE", "1");
            std::env::remove_var("NO_COLOR");
        }
    } else if ci_renders_ansi() && !env_disables_color() {
        // Auto + a CI runner whose log viewer renders ANSI, and the
        // user hasn't opted out via NO_COLOR / CLICOLOR=0: stderr isn't
        // a TTY so console/clx would default to plain text. Flip color
        // on for stderr only via console's per-stream override — that's
        // the stream the install progress heartbeat writes to.
        // Deliberately *not* setting FORCE_COLOR / CLICOLOR_FORCE:
        // those are process-wide and would also colorize stdout (e.g.
        // `aube view --json > out.json` baking escapes into the file)
        // and propagate into lifecycle scripts.
        console::set_colors_enabled_stderr(true);
    }

    // `--use-stderr` / `.npmrc` `useStderr=true`: redirect stdout to stderr
    // so all output goes through a single fd. Resolved here (single-threaded)
    // before the tokio runtime spawns workers.
    //
    // Skip when `--silent` is active: the SilentStderrGuard later redirects
    // fd 2 to /dev/null, and if we dup2 first, fd 1 would capture the real
    // stderr and escape silencing.
    let is_silent = cli.silent || matches!(cli.reporter, Some(ReporterType::Silent));
    if !is_silent {
        let use_stderr_active = cli.use_stderr
            || startup_cwd(&cli).ok().is_some_and(|cwd| {
                let files = commands::FileSources::load(&cwd);
                let ws = std::collections::BTreeMap::new();
                let env_snap = aube_settings::values::capture_env();
                aube_settings::resolved::use_stderr(&files.ctx(&ws, &env_snap, &[]))
            });
        if use_stderr_active {
            // SAFETY: single-threaded `main` — no other threads exist yet.
            // `dup2(stderr, stdout)` makes fd 1 point at the same file as fd 2.
            unsafe {
                libc::dup2(2, 1);
            }
        }
    }

    /*
     * High core boxes don't need 64-128 worker threads for an I/O
     * pipeline. Default worker_threads = num_cpus and
     * max_blocking_threads = 512 are both wasteful. Cap workers at
     * 8 (install semaphore already gates network).
     *
     * Blocking pool sits at 128, raised from 64 after diag traces
     * showed AdaptiveLimit running 100+ concurrent tarball imports
     * (each holding a blocking slot for gzip + tar + CAS write)
     * while the linker is also fanning out hardlinks on the same
     * pool. 64 was saturating, queueing late tarballs behind
     * earlier finishers. 128 covers worst case fat tarball
     * pipeline plus linker plus side effects.
     *
     * AUBE_TOKIO_WORKERS / AUBE_TOKIO_BLOCKING for benchmarking.
     */
    let parse_env = |key: &str, default: usize| -> usize {
        std::env::var(key)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(default)
    };
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let workers = parse_env("AUBE_TOKIO_WORKERS", cpu_count.min(8));
    let blocking = parse_env("AUBE_TOKIO_BLOCKING", 128);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(blocking)
        .enable_all()
        .build()
        .into_diagnostic()
        .wrap_err("failed to build tokio runtime")?;
    let exit_code = runtime.block_on(async_main(cli))?;
    drop(runtime);
    // Return the command's exit code rather than terminating here: a
    // non-zero result (e.g. `run`/`exec` propagating a child's status)
    // must travel back up to the binary's `main`, which owns the single
    // `std::process::exit`. Exiting here would hard-kill a host that
    // embeds the command layer in-process. `None` means "no explicit
    // code" — the normal success exit of 0.
    Ok(exit_code.unwrap_or(0))
}

async fn async_main(cli: Cli) -> miette::Result<Option<i32>> {
    // Default log level is `warn` so routine install output doesn't collide
    // with the clx progress display. `-v` / `--verbose` and `--loglevel debug`
    // turn on debug logging, and in that mode we also force clx into Text
    // output so the progress UI never renders over the log lines. `--silent`
    // (and `--loglevel silent`) turn logging off entirely and disable the
    // progress UI.
    // `--reporter=silent` is equivalent to `--silent`; all other reporter
    // values leave the log level alone and only affect output routing.
    if let Some(dir) = &cli.dir {
        std::env::set_current_dir(dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to change directory to {}", dir.display()))?;
    }

    if cli.version {
        println!("{}", crate::version::VERSION_LONG.as_str());
        let cwd =
            crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
        update_check::check_and_notify(&cwd).await;
        return Ok(None);
    }

    if cli.workspace_root {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = commands::find_workspace_root(&start)?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to change directory to {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }

    let settings = load_startup_settings()?;
    let effective_level = resolve_loglevel(&cli, settings.loglevel.as_deref());
    init_logging(&cli, effective_level);
    // Skip diag init for the `diag` subcommand itself — the analyzer
    // would otherwise truncate the JSONL file it's about to read.
    if !matches!(cli.command, Some(Commands::Diag(_))) {
        match diag_config_from_flag(&cli) {
            Some(cfg_opt) => aube_util::diag::init_with_config(cfg_opt),
            None => aube_util::diag::init(),
        }
    }
    raise_nofile_limit();

    // `--silent` suppresses non-error stderr output from every command,
    // including the ~230 direct `eprintln!` calls in command bodies. The
    // guard restores fd 2 on drop (before main returns), so miette still
    // prints error reports to the real stderr. We also register the
    // saved fd with aube-scripts so child processes spawned via
    // `aube_scripts::child_stderr()` (lifecycle scripts, `aube run`,
    // `aube exec`, `aube dlx`) keep writing to the real terminal — only
    // aube's own output is silenced, matching `pnpm --loglevel silent`.
    let _silent_guard = matches!(effective_level, LogLevel::Silent)
        .then(SilentStderrGuard::install)
        .flatten();
    if let Some(ref guard) = _silent_guard {
        aube_scripts::set_saved_stderr_fd(guard.saved);
    }

    commands::set_skip_auto_install_on_package_manager_mismatch(false);
    if command_needs_package_manager_guard(cli.command.as_ref()) {
        // Self-version switch first: when the project pins aube and
        // the pinned version resolves, this re-execs and never
        // returns. The guard below then only sees matching (or
        // policy-softened) states.
        self_version::maybe_switch(&settings).await?;
        let guard = enforce_package_manager_guardrails(&settings, cli.command.as_ref())?;
        commands::set_skip_auto_install_on_package_manager_mismatch(
            guard == PackageManagerGuard::WarnRunOnly,
        );
    }

    // `--recursive` / `-r` is sugar for `--filter=*`. When a filter is
    // already set, `-r` is a no-op — the explicit scope wins.
    let effective_filter = compute_effective_filter(&cli);

    commands::set_global_output_flags(commands::GlobalOutputFlags {
        ndjson: matches!(cli.reporter, Some(ReporterType::Ndjson)),
        silent: matches!(effective_level, LogLevel::Silent),
    });

    match cli.command {
        Some(Commands::NodeGypBootstrap { project_dir }) => {
            commands::install::node_gyp_bootstrap::print_bootstrapped_binary(&project_dir).await?
        }
        Some(Commands::Add(args)) => {
            commands::add::run(args, effective_filter.clone()).await?;
        }
        Some(Commands::ApproveBuilds(args)) => commands::approve_builds::run(args).await?,
        Some(Commands::Audit(args)) => {
            if let Some(code) = commands::audit::run(args).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Bin(args)) => commands::bin::run(args).await?,
        Some(Commands::Cache(args)) => commands::cache::run(args).await?,
        Some(Commands::CatFile(args)) => commands::cat_file::run(args).await?,
        Some(Commands::CatIndex(args)) => commands::cat_index::run(args).await?,
        Some(Commands::Check(args)) => {
            if let Some(code) = commands::check::run(args).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Ci(args)) => commands::ci::run(args).await?,
        Some(Commands::Clean(args)) => {
            if let Some(code) = commands::clean::run(args).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Completion(args)) => commands::completion::run(args).await?,
        Some(Commands::Config(args)) => commands::config::run(args).await?,
        Some(Commands::Create(args)) => {
            if let Some(code) = commands::create::run(args).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Dedupe(args)) => commands::dedupe::run(args).await?,
        Some(Commands::Deploy(args)) => {
            commands::deploy::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Deprecate(args)) => commands::deprecate::run(args).await?,
        Some(Commands::Deprecations(args)) => {
            if let Some(code) = commands::deprecations::run(args).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Diag(args)) => commands::diag::run(args).await?,
        Some(Commands::DistTag(args)) => commands::dist_tag::run(args).await?,
        Some(Commands::Dlx(args)) => {
            if let Some(code) = commands::dlx::run(args).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Doctor(args)) => {
            if let Some(code) = commands::doctor::run(args).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Exec(args)) => {
            if let Some(code) = commands::exec::run(args, effective_filter.clone()).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Fetch(args)) => commands::fetch::run(args).await?,
        Some(Commands::FindHash(args)) => commands::find_hash::run(args).await?,
        Some(Commands::Get(args)) => commands::config::get(args)?,
        Some(Commands::IgnoredBuilds(args)) => commands::ignored_builds::run(args).await?,
        Some(Commands::Import(args)) => commands::import::run(args).await?,
        Some(Commands::Init(args)) => commands::init::run(args).await?,
        Some(Commands::Install(args)) => {
            run_install_command(args, effective_filter.clone(), cli.workspace_root).await?;
        }
        Some(Commands::InstallTest(args)) => {
            if let Some(code) = commands::install_test::run(args).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::La(mut args)) | Some(Commands::Ll(mut args)) => {
            args.long = true;
            commands::list::run(args, effective_filter.clone()).await?;
        }
        Some(Commands::Licenses(args)) => commands::licenses::run(args).await?,
        Some(Commands::Link(args)) => commands::link::run(args).await?,
        Some(Commands::List(args)) => commands::list::run(args, effective_filter.clone()).await?,
        Some(Commands::Login(args)) => commands::login::run(args).await?,
        Some(Commands::Logout(args)) => commands::logout::run(args).await?,
        Some(Commands::Outdated(args)) => {
            if let Some(code) = commands::outdated::run(args, effective_filter.clone()).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Owner(args)) => {
            return Ok(Some(commands::npm_fallback::run("owner", &args)?));
        }
        Some(Commands::Pack(args)) => commands::pack::run(args).await?,
        Some(Commands::Patch(args)) => commands::patch::run(args).await?,
        Some(Commands::PatchCommit(args)) => commands::patch_commit::run(args).await?,
        Some(Commands::PatchRemove(args)) => commands::patch_remove::run(args).await?,
        Some(Commands::Peers(args)) => {
            if let Some(code) = commands::peers::run(args).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Pkg(args)) => {
            return Ok(Some(commands::npm_fallback::run("pkg", &args)?));
        }
        Some(Commands::Prune(args)) => commands::prune::run(args).await?,
        Some(Commands::Publish(args)) => {
            commands::publish::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Purge(args)) => {
            if let Some(code) = commands::clean::run_purge(args).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Query(args)) => commands::query::run(args, effective_filter.clone()).await?,
        Some(Commands::Rebuild(args)) => {
            commands::rebuild::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Remove(args)) => {
            commands::remove::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Recursive(args)) => {
            let argv = commands::recursive::argv(
                args,
                commands::recursive::RecursiveGlobals {
                    filters: effective_filter.clone(),
                    color: cli.color,
                    no_color: cli.no_color,
                },
            )?;
            // The reconstructed argv may carry pre-subcommand-positioned
            // flags that moved off `global = true` (e.g. `--registry`,
            // `--frozen-lockfile`). Run the same lift-pass we use on the
            // outer argv so the nested clap parse sees them after the
            // subcommand.
            let nested_argv: Vec<OsString> =
                lift_per_subcommand_flags(argv.into_iter().map(OsString::from).collect());
            let nested = Cli::try_parse_from(nested_argv).into_diagnostic()?;
            let nested_filter = compute_effective_filter(&nested);
            match nested.command {
                Some(Commands::Add(args)) => {
                    commands::add::run(args, nested_filter).await?;
                }
                Some(Commands::Deploy(args)) => commands::deploy::run(args, nested_filter).await?,
                Some(Commands::Exec(args)) => {
                    if let Some(code) = commands::exec::run(args, nested_filter).await? {
                        return Ok(Some(code));
                    }
                }
                Some(Commands::Install(args)) => {
                    run_install_command(args, nested_filter, nested.workspace_root).await?;
                }
                Some(Commands::List(args)) => commands::list::run(args, nested_filter).await?,
                Some(Commands::La(mut args)) | Some(Commands::Ll(mut args)) => {
                    args.long = true;
                    commands::list::run(args, nested_filter).await?;
                }
                Some(Commands::Outdated(args)) => {
                    if let Some(code) = commands::outdated::run(args, nested_filter).await? {
                        return Ok(Some(code));
                    }
                }
                Some(Commands::Publish(args)) => {
                    commands::publish::run(args, nested_filter).await?
                }
                Some(Commands::Rebuild(args)) => {
                    commands::rebuild::run(args, nested_filter).await?
                }
                Some(Commands::Remove(args)) => commands::remove::run(args, nested_filter).await?,
                Some(Commands::Restart(args)) => {
                    if let Some(code) = commands::restart::run(args, nested_filter).await? {
                        return Ok(Some(code));
                    }
                }
                Some(Commands::Run(args)) => {
                    if let Some(code) = commands::run::run(args, nested_filter).await? {
                        return Ok(Some(code));
                    }
                }
                Some(Commands::Start(args)) => {
                    if let Some(code) = run_script_lifecycle("start", args, &nested_filter).await? {
                        return Ok(Some(code));
                    }
                }
                Some(Commands::Stop(args)) => {
                    if let Some(code) = run_script_lifecycle("stop", args, &nested_filter).await? {
                        return Ok(Some(code));
                    }
                }
                Some(Commands::Test(args)) => {
                    if let Some(code) = run_script_lifecycle("test", args, &nested_filter).await? {
                        return Ok(Some(code));
                    }
                }
                Some(Commands::Update(args)) => {
                    if let Some(code) = commands::update::run(args, nested_filter).await? {
                        return Ok(Some(code));
                    }
                }
                Some(Commands::Why(args)) => commands::why::run(args, nested_filter).await?,
                Some(Commands::External(args)) => {
                    let script = &args[0];
                    let script_args: Vec<String> = args[1..].to_vec();
                    if let Some(code) = commands::run::run_script(
                        script,
                        &script_args,
                        false,
                        false,
                        &nested_filter,
                    )
                    .await?
                    {
                        return Ok(Some(code));
                    }
                }
                Some(_) | None => {
                    return Err(miette::miette!(
                        code = aube_codes::errors::ERR_AUBE_RECURSIVE_NOT_SUPPORTED,
                        "{} recursive: command does not support recursive execution",
                        aube_util::embedder().name,
                    ));
                }
            }
        }
        Some(Commands::Restart(args)) => {
            if let Some(code) = commands::restart::run(args, effective_filter.clone()).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Root(args)) => commands::root::run(args).await?,
        Some(Commands::Run(args)) => {
            if let Some(code) = commands::run::run(args, effective_filter.clone()).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Runtime(args)) => commands::runtime::run(args).await?,
        Some(Commands::Sbom(args)) => commands::sbom::run(args).await?,
        Some(Commands::Search(args)) => {
            return Ok(Some(commands::npm_fallback::run("search", &args)?));
        }
        Some(Commands::Set(args)) => commands::config::set(args)?,
        Some(Commands::SetScript(args)) => {
            return Ok(Some(commands::npm_fallback::run("set-script", &args)?));
        }
        Some(Commands::Sponsors(args)) => commands::sponsors::run(args).await?,
        Some(Commands::Stage(args)) => {
            return Ok(Some(commands::npm_fallback::run("stage", &args)?));
        }
        Some(Commands::Start(args)) => {
            if let Some(code) = run_script_lifecycle("start", args, &effective_filter).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Stop(args)) => {
            if let Some(code) = run_script_lifecycle("stop", args, &effective_filter).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Store(args)) => commands::store::run(args).await?,
        Some(Commands::Test(args)) => {
            if let Some(code) = run_script_lifecycle("test", args, &effective_filter).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Token(args)) => {
            return Ok(Some(commands::npm_fallback::run("token", &args)?));
        }
        Some(Commands::Undeprecate(args)) => commands::undeprecate::run(args).await?,
        Some(Commands::Unlink(args)) => commands::unlink::run(args).await?,
        Some(Commands::Unpublish(args)) => commands::unpublish::run(args).await?,
        Some(Commands::Update(args)) => {
            if let Some(code) = commands::update::run(args, effective_filter.clone()).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Version(args)) => commands::version::run(args).await?,
        Some(Commands::View(args)) => commands::view::run(args).await?,
        Some(Commands::Whoami(args)) => {
            return Ok(Some(commands::npm_fallback::run("whoami", &args)?));
        }
        Some(Commands::Why(args)) => commands::why::run(args, effective_filter.clone()).await?,
        Some(Commands::External(args)) => {
            // Implicit run: `aube dev` = `aube run dev`.
            //
            // External is clap's catch-all, so a typo like `aube fooefjwol`
            // lands here too. If the name isn't an actual script in the
            // local `package.json` (or there's no `package.json` at all),
            // print `aube --help` and bail instead of routing it into the
            // script runner and surfacing a confusing "script not found"
            // or "failed to read package.json" — the user typed something
            // we don't recognize and help is the most useful reply.
            //
            // The pre-check only fires when *no* workspace filter is
            // active: `-r` / `-F` fan implicit scripts out across
            // sub-packages, and the script may live in one of the
            // matched workspaces while the root `package.json` has no
            // `scripts` entry at all. In that mode we hand off to
            // `run_script` unchanged and let the filtered runner
            // produce its own per-package diagnostics.
            let script = &args[0];
            let script_args: Vec<String> = args[1..].to_vec();
            if effective_filter.is_empty() {
                let initial_cwd = crate::dirs::cwd()?;
                let script_exists = crate::dirs::find_project_root(&initial_cwd)
                    .and_then(|cwd| {
                        aube_manifest::PackageJson::from_path(&cwd.join("package.json")).ok()
                    })
                    .map(|m| m.scripts.contains_key(script))
                    .unwrap_or(false);
                if !script_exists {
                    use clap::CommandFactory;
                    let mut cmd = Cli::command();
                    cmd.print_help().ok();
                    eprintln!();
                    return Err(miette::miette!(
                        code = aube_codes::errors::ERR_AUBE_UNKNOWN_COMMAND,
                        "unknown command: {script}"
                    ));
                }
            }
            if let Some(code) =
                commands::run::run_script(script, &script_args, false, false, &effective_filter)
                    .await?
            {
                return Ok(Some(code));
            }
        }
        None => {
            // Bare `aube` prints `--help` and exits 0, matching pnpm.
            // pnpm's bare invocation does not run an install; users who
            // want that behavior should type `aube install` explicitly.
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            cmd.print_help().ok();
            println!();
        }
    }

    Ok(None)
}

/// Run a lifecycle script (`start` / `stop` / `test` / `restart`).
///
/// `ScriptArgs` carries the moved-off-global `LockfileArgs` /
/// `NetworkArgs` / `VirtualStoreArgs` flattens for these commands, so we
/// drain them into the process-global slots before delegating to the
/// shared `run_script` helper. Auto-install (triggered by `run_script`
/// when the named script doesn't exist locally) reads the slots through
/// `ensure_installed`.
async fn run_script_lifecycle(
    name: &str,
    args: commands::run::ScriptArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<Option<i32>> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    commands::run::run_script(name, &args.args, args.no_install, false, filter).await
}

async fn run_install_command(
    args: commands::install::InstallArgs,
    filter: aube_workspace::selector::EffectiveFilter,
    workspace_root_already: bool,
) -> miette::Result<()> {
    // `-w` on install is a short alias for the global
    // `--workspace-root` flag. Handle the chdir here when the global
    // flag wasn't already set.
    if args.workspace_root_short && !workspace_root_already {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = commands::find_workspace_root(&start)?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to change directory to {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    let global_frozen = args.lockfile.frozen_override();
    let global_gvs = args.virtual_store.flags();
    // Match `install::run`'s precedence so settings here resolve from
    // the same root the install will operate against. Workspace-first
    // means `aube install` from inside a member loads `.npmrc` /
    // workspace yaml from the workspace root, not the member; without
    // this the two diverged when both roots existed.
    let cwd = crate::dirs::workspace_or_project_root()?;
    let files = commands::FileSources::load(&cwd);
    let raw_ws = aube_manifest::workspace::load_raw(&cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env = aube_settings::values::capture_env();
    let cli_flags = args.to_cli_flag_bag(global_frozen, global_gvs);
    let ctx = files.ctx(&raw_ws, &env, &cli_flags);
    let yaml_prefer_frozen = aube_settings::resolved::prefer_frozen_lockfile(&ctx);
    let mut opts = args.into_options(global_frozen, yaml_prefer_frozen, cli_flags, env);
    opts.workspace_filter = filter;
    commands::install::run(opts).await?;
    Ok(())
}

#[cfg(test)]
mod cli_spec_tests {
    use super::*;

    #[test]
    fn install_accepts_subcommand_registry_flag() {
        let cli = Cli::try_parse_from([
            "aube",
            "install",
            "--registry",
            "https://registry.example.com/",
        ])
        .expect("install --registry should parse");

        let Some(Commands::Install(install_args)) = cli.command else {
            panic!("expected install subcommand");
        };
        assert_eq!(
            install_args.network.registry.as_deref(),
            Some("https://registry.example.com/")
        );
    }

    #[test]
    fn pre_subcommand_registry_lifts_to_install() {
        // pnpm-compat: `--registry=URL install` continues to parse via
        // `lift_per_subcommand_flags`, which shifts the flag past the
        // subcommand before clap sees argv.
        let argv = lift_per_subcommand_flags(
            [
                "aube",
                "--registry",
                "https://registry.example.com/",
                "install",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        );
        let cli = Cli::try_parse_from(argv)
            .expect("pre-subcommand --registry should still parse via the rewriter");
        let Some(Commands::Install(install_args)) = cli.command else {
            panic!("expected install subcommand");
        };
        assert_eq!(
            install_args.network.registry.as_deref(),
            Some("https://registry.example.com/")
        );
    }

    #[test]
    fn dlx_accepts_allow_build_before_command() {
        let cli =
            Cli::try_parse_from(["aube", "dlx", "--allow-build=esbuild", "vite", "--version"])
                .expect("dlx --allow-build should parse");

        let Some(Commands::Dlx(dlx_args)) = cli.command else {
            panic!("expected dlx subcommand");
        };
        assert_eq!(dlx_args.allow_build, ["esbuild"]);
        assert_eq!(dlx_args.params, ["vite", "--version"]);
    }

    #[test]
    fn dlx_rejects_empty_allow_build_value() {
        let err = match Cli::try_parse_from(["aube", "dlx", "--allow-build=", "vite"]) {
            Ok(_) => panic!("empty --allow-build should fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("The --allow-build flag is missing a package name"),
            "{err}"
        );
    }

    #[test]
    fn lifter_does_not_eat_lifted_flag_as_kept_flag_value() {
        // Regression: `aube --dir /tmp --frozen-lockfile install` would
        // previously lose `--frozen-lockfile` if `--dir`'s value was
        // omitted because the rewriter unconditionally consumed the next
        // token as the kept flag's value.
        let argv = lift_per_subcommand_flags(
            ["aube", "--dir", "--frozen-lockfile", "install"]
                .into_iter()
                .map(OsString::from)
                .collect(),
        );
        // After the lift, `--frozen-lockfile` should sit after `install`,
        // NOT have been consumed as `--dir`'s value.
        let strs: Vec<&str> = argv.iter().filter_map(|t| t.to_str()).collect();
        let install_idx = strs
            .iter()
            .position(|s| *s == "install")
            .expect("install subcommand should survive the lift");
        assert!(
            strs[install_idx + 1..].contains(&"--frozen-lockfile"),
            "--frozen-lockfile should land after the subcommand: {strs:?}"
        );
    }

    #[test]
    fn short_command_aliases_parse() {
        let cli = Cli::try_parse_from(["aube", "a", "react"]).expect("a should parse as add");
        assert!(matches!(cli.command, Some(Commands::Add(_))));

        let cli =
            Cli::try_parse_from(["aube", "x", "vitest", "--run"]).expect("x should parse as exec");
        let Some(Commands::Exec(args)) = cli.command else {
            panic!("x should dispatch to exec");
        };
        assert_eq!(args.bin, "vitest");
        assert_eq!(args.args, vec!["--run"]);

        let cli = Cli::try_parse_from(["aube", "w", "react"]).expect("w should parse as why");
        assert!(matches!(cli.command, Some(Commands::Why(_))));
    }
}

#[cfg(test)]
mod multicall_tests {
    use super::*;

    fn os(strs: &[&str]) -> Vec<OsString> {
        strs.iter().map(OsString::from).collect()
    }

    fn temp_shim(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        std::fs::write(dir.path().join(name), "#!/tmp/aube.exe\n").expect("shim should be written");
        dir
    }

    #[test]
    fn aube_passes_through_unchanged() {
        assert_eq!(
            rewrite_multicall_argv(os(&["aube", "install"])),
            os(&["aube", "install"])
        );
    }

    #[test]
    fn aubr_rewrites_to_run() {
        assert_eq!(
            rewrite_multicall_argv(os(&["aubr", "build"])),
            os(&["aube", "run", "build"])
        );
    }

    #[test]
    fn aubx_rewrites_to_dlx() {
        assert_eq!(
            rewrite_multicall_argv(os(&["aubx", "cowsay", "hi"])),
            os(&["aube", "dlx", "cowsay", "hi"])
        );
    }

    #[test]
    fn absolute_path_and_exe_suffix_are_handled() {
        // argv[0] can be an absolute path (exec-style invocation) or carry
        // a `.exe` suffix on Windows. `Path::file_stem` takes care of both
        // so dispatch stays purely basename-driven.
        assert_eq!(
            rewrite_multicall_argv(os(&["/usr/local/bin/aubr", "test"])),
            os(&["aube", "run", "test"])
        );
        assert_eq!(
            rewrite_multicall_argv(os(&["aubx.exe", "pkg"])),
            os(&["aube", "dlx", "pkg"])
        );
    }

    #[test]
    fn bare_shim_invocation_passes_through_to_subcommand() {
        // `aubr` with no further args becomes `aube run`, which clap
        // parses as the `run` subcommand with no positional — same as
        // the user typing `aube run` directly.
        assert_eq!(rewrite_multicall_argv(os(&["aubr"])), os(&["aube", "run"]));
    }

    #[test]
    fn version_flag_short_circuits_to_top_level() {
        // `aubr --version` / `aubx --version` should print the aube
        // version, not trip the `run` / `dlx` parsers.
        assert_eq!(
            rewrite_multicall_argv(os(&["aubr", "--version"])),
            os(&["aube", "--version"])
        );
        assert_eq!(
            rewrite_multicall_argv(os(&["aubx", "--version"])),
            os(&["aube", "--version"])
        );
        assert_eq!(
            rewrite_multicall_argv(os(&["aubr", "-V"])),
            os(&["aube", "-V"])
        );
        assert_eq!(
            rewrite_multicall_argv(os(&["aubx.exe", "-V"])),
            os(&["aube", "-V"])
        );
    }

    #[test]
    fn npm_interpreter_shim_path_is_dropped() {
        let dir = temp_shim("aube");
        let shim = dir.path().join("aube");
        let shim_os = shim.clone().into_os_string();
        assert_eq!(
            rewrite_multicall_argv(vec![
                OsString::from("aube.exe"),
                shim.into_os_string(),
                OsString::from("--version"),
            ]),
            vec![shim_os, OsString::from("--version")]
        );
    }

    #[test]
    fn npm_interpreter_shim_preserves_multicall_dispatch() {
        let dir = temp_shim("aubr");
        let shim = dir.path().join("aubr");
        assert_eq!(
            rewrite_multicall_argv(vec![
                OsString::from("aubr.exe"),
                shim.into_os_string(),
                OsString::from("build"),
            ]),
            os(&["aube", "run", "build"])
        );
    }

    #[test]
    fn extract_config_overrides_strips_equals_form() {
        let mut argv = os(&["aube", "install", "--config.strict-dep-builds=true"]);
        let parsed = extract_config_overrides(&mut argv);
        assert_eq!(argv, os(&["aube", "install"]));
        assert_eq!(
            parsed,
            vec![("strict-dep-builds".to_string(), "true".to_string())]
        );
    }

    #[test]
    fn extract_config_overrides_strips_bool_form() {
        let mut argv = os(&["aube", "--config.strictDepBuilds", "install"]);
        let parsed = extract_config_overrides(&mut argv);
        assert_eq!(argv, os(&["aube", "install"]));
        assert_eq!(
            parsed,
            vec![("strictDepBuilds".to_string(), "true".to_string())]
        );
    }

    #[test]
    fn extract_config_overrides_handles_multiple_and_preserves_order() {
        let mut argv = os(&[
            "aube",
            "--config.foo=1",
            "install",
            "--config.bar=two",
            "--config.foo=3",
        ]);
        let parsed = extract_config_overrides(&mut argv);
        assert_eq!(argv, os(&["aube", "install"]));
        assert_eq!(
            parsed,
            vec![
                ("foo".to_string(), "1".to_string()),
                ("bar".to_string(), "two".to_string()),
                ("foo".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn extract_config_overrides_stops_at_double_dash() {
        let mut argv = os(&["aube", "exec", "--", "node", "--config.foo=should-stay"]);
        let parsed = extract_config_overrides(&mut argv);
        assert!(parsed.is_empty());
        assert_eq!(
            argv,
            os(&["aube", "exec", "--", "node", "--config.foo=should-stay"])
        );
    }

    #[test]
    fn extract_config_overrides_preserves_argv_when_absent() {
        let mut argv = os(&["aube", "install", "--frozen-lockfile"]);
        let parsed = extract_config_overrides(&mut argv);
        assert!(parsed.is_empty());
        assert_eq!(argv, os(&["aube", "install", "--frozen-lockfile"]));
    }
}

#[cfg(test)]
mod package_manager_guard_tests {
    use super::*;

    #[test]
    fn run_like_commands_warn_instead_of_erroring() {
        let run = Cli::try_parse_from(["aube", "run", "test"]).expect("run should parse");
        let test = Cli::try_parse_from(["aube", "test"]).expect("test should parse");

        assert_eq!(
            package_manager_guard_mode(run.command.as_ref()),
            PackageManagerGuardMode::WarnAndSkipAutoInstall
        );
        assert_eq!(
            package_manager_guard_mode(test.command.as_ref()),
            PackageManagerGuardMode::WarnAndSkipAutoInstall
        );
    }

    #[test]
    fn install_still_errors_on_mismatch() {
        let cli = Cli::try_parse_from(["aube", "install"]).expect("install should parse");
        assert_eq!(
            package_manager_guard_mode(cli.command.as_ref()),
            PackageManagerGuardMode::Error
        );
    }

    #[test]
    fn install_test_still_errors_on_mismatch() {
        let cli = Cli::try_parse_from(["aube", "install-test"]).expect("install-test should parse");
        assert_eq!(
            package_manager_guard_mode(cli.command.as_ref()),
            PackageManagerGuardMode::Error
        );
    }

    #[test]
    fn package_manager_strict_mode_parses_canonical_spellings() {
        for (input, expected) in [
            ("off", PackageManagerStrictMode::Off),
            ("warn", PackageManagerStrictMode::Warn),
            ("error", PackageManagerStrictMode::Error),
            ("  ERROR\n", PackageManagerStrictMode::Error),
        ] {
            assert_eq!(PackageManagerStrictMode::parse(input), Some(expected));
        }
    }

    #[test]
    fn package_manager_strict_mode_parses_bool_back_compat() {
        // `true`/`false` (and the shell-style `1`/`0` admitted by the
        // generic bool parser) need to keep working so projects on the
        // pre-tri-state default don't break.
        for (input, expected) in [
            ("true", PackageManagerStrictMode::Error),
            ("false", PackageManagerStrictMode::Off),
            ("1", PackageManagerStrictMode::Error),
            ("0", PackageManagerStrictMode::Off),
        ] {
            assert_eq!(PackageManagerStrictMode::parse(input), Some(expected));
        }
    }

    #[test]
    fn package_manager_strict_mode_returns_none_for_typos() {
        // Caller turns `None` into a startup warning + default. The
        // unit test pins the precondition: parse must NOT silently
        // coerce a typo to the default.
        assert!(PackageManagerStrictMode::parse("errror").is_none());
        assert!(PackageManagerStrictMode::parse("warning").is_none());
        assert!(PackageManagerStrictMode::parse("").is_none());
    }
}

#[cfg(test)]
mod cli_ordering_tests {
    use super::*;
    use clap::CommandFactory;
    use std::collections::BTreeMap;

    /// Validate that aube's CLI commands and arguments are ordered:
    /// - Subcommands alphabetical by name
    /// - Short flags alphabetical by short option
    /// - Long-only flags alphabetical by long name *within each help-heading
    ///   bucket* (the unheaded default counts as one bucket)
    ///
    /// We can't use `clap_sort::assert_sorted` directly because flags from
    /// flattened `cli_args::*Args` groups carry their own `help_heading`
    /// (e.g. "Lockfile", "Network", "Virtual store") and clap-sort enforces
    /// strict alphabetical across the full long-only set, which would
    /// require interleaving group flags between per-command flags. The
    /// help-grouped layout is the whole point of the move, so we sort
    /// within heading buckets instead.
    #[test]
    fn test_cli_ordering() {
        check_command_sorted(&Cli::command(), &[]);
    }

    fn check_command_sorted(cmd: &clap::Command, path: &[&str]) {
        let mut current_path: Vec<&str> = path.to_vec();
        current_path.push(cmd.get_name());

        // Subcommands alphabetical
        let names: Vec<_> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert!(
            names == sorted,
            "Subcommands in '{}' are not sorted alphabetically!\nActual: {:?}\nExpected: {:?}",
            current_path.join(" "),
            names,
            sorted,
        );

        // Short flags alphabetical, long-only alphabetical within heading.
        let mut shorts: Vec<char> = Vec::new();
        let mut by_heading: BTreeMap<Option<&str>, Vec<&str>> = BTreeMap::new();
        for arg in cmd.get_arguments() {
            if let Some(s) = arg.get_short() {
                shorts.push(s);
            } else if let Some(l) = arg.get_long() {
                by_heading
                    .entry(arg.get_help_heading())
                    .or_default()
                    .push(l);
            }
        }
        let mut sorted_shorts = shorts.clone();
        sorted_shorts.sort_by_key(|c| (c.to_ascii_lowercase(), c.is_uppercase()));
        assert!(
            shorts == sorted_shorts,
            "Short flags in '{}' are not sorted!\nActual: {:?}\nExpected: {:?}",
            current_path.join(" "),
            shorts,
            sorted_shorts,
        );
        for (heading, longs) in &by_heading {
            let mut sorted_longs = longs.clone();
            sorted_longs.sort();
            assert!(
                longs == &sorted_longs,
                "Long-only flags under heading {:?} in '{}' are not sorted!\nActual: {:?}\nExpected: {:?}",
                heading,
                current_path.join(" "),
                longs,
                sorted_longs,
            );
        }

        for sub in cmd.get_subcommands() {
            check_command_sorted(sub, &current_path);
        }
    }
}
