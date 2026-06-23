//! CLI argument parsing and dispatch.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

/// Stable, branded error codes for nub-cli's own (non-engine) failure paths.
/// The engine's `ERR_AUBE_*` codes are rewritten to `ERR_NUB_*` at presentation
/// (see `pm_engine::present`); these are nub's native equivalents, embedded
/// directly in the user-facing message text since these paths surface as
/// `anyhow` errors rather than miette reports. Keep the `ERR_NUB_*` spelling so
/// the brand boundary holds and the codes read identically to the engine's.
const ERR_NUB_MANIFEST_UNREADABLE: &str = "ERR_NUB_MANIFEST_UNREADABLE";
const ERR_NUB_MANIFEST_PARSE: &str = "ERR_NUB_MANIFEST_PARSE";
/// No `package.json` at or above the cwd. The install path surfaces the same root
/// cause as a coded miette diagnostic from the engine; `nub run` reuses this code
/// so both spellings read consistently (was a bare `Error: no package.json found`).
const ERR_NUB_NO_MANIFEST: &str = "ERR_NUB_NO_MANIFEST";

static SHOW_WARNINGS: AtomicBool = AtomicBool::new(false);
/// `--silent` suppresses Nub's own preamble (the `$ <command>` script echo),
/// never the script's stdout. Set once at startup; read at each script-run echo.
static SILENT: AtomicBool = AtomicBool::new(false);
/// `--reporter=ndjson`: emit one JSON object per line (start / log / end / summary
/// events) on stdout for structural CI parsing. Set once when the `run` command
/// parses its `--reporter` flag; read at each output-emission site.
static REPORTER_NDJSON: AtomicBool = AtomicBool::new(false);
/// `--reporter-hide-prefix`: drop the `<dir> <script>:` lead from each streamed
/// output line so CI annotation matchers (e.g. GitHub Actions, which parse
/// `error: file:line`) see the child's raw output. Affects the per-line prefix
/// only; the `$ <cmd>` echo is left intact.
static HIDE_STREAM_PREFIX: AtomicBool = AtomicBool::new(false);

/// `nubx`-only: turn on the `npx`/`pnpm dlx` fallback in [`run_exec`]. Set once
/// in [`run_nubx`] before it desugars to the `exec` dispatch; read in `run_exec`
/// when the bin is absent from `node_modules/.bin`. Plain `nub exec` leaves this
/// off and keeps its deliberate no-network behavior (a 127 + install suggestion);
/// only the user-facing `nubx <tool>` entry point fetches-and-runs an uninstalled
/// tool, matching `npx`/`pnpm dlx` (local-first, DLX as the fallback).
static NUBX_DLX_FALLBACK: AtomicBool = AtomicBool::new(false);

/// The `nubx` npx-parity flags that steer the DLX (fetch-and-run) fallback in
/// [`run_exec`]. Only the `nubx` entry point populates this; plain `nub exec`
/// passes `None` and keeps its no-network behavior. Defaults match a bare
/// `nubx <tool>` (no `-p`, fetch allowed, progress shown).
#[derive(Clone, Debug, Default)]
pub struct NubxDlxFlags {
    /// `-p`/`--package <spec>`: packages to fetch; the positional becomes the
    /// bin to run from them. Non-empty forces the fetch path.
    pub package: Vec<String>,
    /// `--no-install`/`--no`: refuse to fetch — error on a local miss.
    pub no_install: bool,
    /// `-q`/`--quiet`: suppress the fetch progress output.
    pub quiet: bool,
}

/// `--reporter <MODE>` for `nub run`. `default` is the existing prefixed /
/// streamed / aggregated human output; `silent` is `-s`; `ndjson` is machine
/// output (see [`emit_ndjson`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum ReporterMode {
    Default,
    Silent,
    Ndjson,
}

fn reporter_is_ndjson() -> bool {
    REPORTER_NDJSON.load(Ordering::Relaxed)
}

fn reporter_hide_prefix() -> bool {
    HIDE_STREAM_PREFIX.load(Ordering::Relaxed)
}

/// pnpm honors `npm_config_reporter=silent` (and the `-s` / `--reporter=silent`
/// flags) to suppress the `> name@ <script>` run preamble. nub already routes
/// `npm_config_*` knobs (registry/cache) through the embedder bridge but the run
/// preamble (`$ <cmd>`) keyed only on the explicit `-s`/`--reporter` flag — so an
/// env-set `npm_config_reporter=silent` was ignored. Read it here when no
/// explicit `--reporter` flag was given. Matches pnpm: only `reporter=silent`
/// (not `loglevel`) suppresses the run preamble.
fn npm_config_reporter_is_silent() -> bool {
    std::env::var("npm_config_reporter")
        .map(|v| v.trim().eq_ignore_ascii_case("silent"))
        .unwrap_or(false)
}

/// `--shell-emulator`: run script bodies through a detected POSIX `sh` instead of
/// the platform default. The default is already `sh` on Unix (so the flag is a
/// no-op there); on Windows the default is `cmd`, which can't run POSIX-isms
/// (`FOO=1 cmd`, `$VAR`, `&&`), so the flag routes the body through a `sh` found
/// on PATH / in a Git-for-Windows install. Set once when `run` parses the flag.
static SHELL_EMULATOR: AtomicBool = AtomicBool::new(false);

fn shell_emulator_enabled() -> bool {
    SHELL_EMULATOR.load(Ordering::Relaxed)
}

/// Locate a POSIX `sh` for `--shell-emulator`. Searches PATH (`sh`/`sh.exe`), then
/// the standard Git-for-Windows install dirs. Platform-independent — on Unix it
/// finds `/bin/sh`, on Windows the Git/WSL `sh.exe`. `None` if none is found
/// (the caller turns that into an actionable error).
fn find_posix_sh() -> Option<String> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in ["sh", "sh.exe"] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate.to_string_lossy().into_owned());
                }
            }
        }
    }
    // Standard Git-for-Windows locations (these paths simply don't exist on Unix).
    for p in [
        r"C:\Program Files\Git\bin\sh.exe",
        r"C:\Program Files\Git\usr\bin\sh.exe",
        r"C:\Program Files (x86)\Git\bin\sh.exe",
    ] {
        if std::path::Path::new(p).is_file() {
            return Some(p.to_string());
        }
    }
    None
}

/// Emit one ndjson event on stdout (`--reporter=ndjson`). The base shape is the
/// `{ level, name, script, time, msg }` that `@pnpm/cli.default-reporter`
/// consumes, plus an `event` discriminator (`start`/`log`/`end`/`summary`) and an
/// optional `exitCode`. `println!` locks stdout per call, so a whole JSON line is
/// atomic even when concurrent workers emit interleaved.
fn ndjson_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn emit_ndjson(
    event: &str,
    level: &str,
    name: &str,
    script: &str,
    msg: Option<&str>,
    exit_code: Option<i32>,
) {
    let mut obj = serde_json::json!({
        "event": event, "level": level, "name": name, "script": script, "time": ndjson_now_ms(),
    });
    if let Some(m) = msg {
        obj["msg"] = serde_json::Value::String(m.to_string());
    }
    if let Some(c) = exit_code {
        obj["exitCode"] = serde_json::Value::from(c);
    }
    if let Ok(s) = serde_json::to_string(&obj) {
        println!("{s}");
    }
}

/// The terminal `summary` event for `--reporter=ndjson` (one per `nub run`).
fn emit_ndjson_summary(passed: usize, failed: usize) {
    let obj = serde_json::json!({
        "event": "summary",
        "level": if failed == 0 { "info" } else { "error" },
        "time": ndjson_now_ms(),
        "passed": passed,
        "failed": failed,
    });
    if let Ok(s) = serde_json::to_string(&obj) {
        println!("{s}");
    }
}

/// Vars parsed from `--env-file`, captured once at startup. Applied to each
/// spawned child via `Command::env` (see [`overlay_env_file_vars`] /
/// [`apply_env_file_vars`]) rather than mutating nub's own process environment —
/// the latter required `unsafe { env::set_var }` and would be a data race if any
/// dependency spun up a thread during init (A19). Set once; never mutated after.
static ENV_FILE_VARS: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Whether at least one `--env-file` flag was present on the command line.
/// Distinct from `ENV_FILE_VARS` being empty: a user may pass `--env-file` to an
/// empty file (zero vars) yet still have signalled intent. When set, nub's eager
/// `.env*` auto-discovery is suppressed and only the explicit file(s) load —
/// passing `--env-file` opts out of auto-discovery entirely (the maintainer, 2026-06-15).
static ENV_FILE_PRESENT: OnceLock<bool> = OnceLock::new();

/// True iff the user passed one or more `--env-file` flags. Gates whether the
/// eager `.env*` auto-discovery runs; see [`ENV_FILE_PRESENT`].
fn env_file_flag_present() -> bool {
    ENV_FILE_PRESENT.get().copied().unwrap_or(false)
}

/// Overlay the `--env-file` vars onto an env map bound for a child's
/// `Command::env`. Shell env still wins (skip keys already in this process's
/// environment); `--env-file` overrides `.env` (insert over existing entries).
/// `--env-file` vars are thus handled uniformly with `.env` vars — both flow
/// through the same per-spawn `Command::env` application.
fn overlay_env_file_vars(env_map: &mut HashMap<String, String>) {
    if let Some(vars) = ENV_FILE_VARS.get() {
        for (k, v) in vars {
            if env::var_os(k).is_none() {
                env_map.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Build the env map for a child, merging eager `.env*` auto-discovery with the
/// explicit `--env-file` vars. The gate (the maintainer, 2026-06-15): when the user passed
/// any `--env-file` flag, auto-discovery is **suppressed entirely** — none of the
/// four auto files (`.env.<NODE_ENV>.local`, `.env.local`, `.env.<NODE_ENV>`,
/// `.env`) load, and only the explicit file(s) reach the child. With no
/// `--env-file`, the autos load as before.
///
/// `auto_env` is the already-loaded `.env*` map (callers pass `load_env_files`'s
/// result, which honors NODE_ENV + ${VAR} expansion); `env_file_present` is the
/// flag-presence signal; `explicit_vars` is `ENV_FILE_VARS` (the parsed
/// `--env-file` contents). This is a pure function over its inputs so the
/// suppression contract can be unit-tested without spawning a child.
fn merge_child_env(
    auto_env: HashMap<String, String>,
    env_file_present: bool,
    explicit_vars: &HashMap<String, String>,
) -> HashMap<String, String> {
    // Explicit `--env-file` opts out of auto-discovery: start from an empty map,
    // not the auto-loaded one, so none of the four `.env*` files leak through.
    let mut env_map = if env_file_present {
        HashMap::new()
    } else {
        auto_env
    };
    // Overlay the explicit vars: shell env still wins; `--env-file` overrides any
    // `.env` value that survives (only relevant when no flag was passed).
    for (k, v) in explicit_vars {
        if env::var_os(k).is_none() {
            env_map.insert(k.clone(), v.clone());
        }
    }
    env_map
}

/// Apply the `--env-file` vars directly to a child command, for spawn paths
/// (`nubx` non-node launchers, the dlx fallback) that don't build an env map.
/// Same precedence as [`overlay_env_file_vars`].
fn apply_env_file_vars(cmd: &mut std::process::Command) {
    if let Some(vars) = ENV_FILE_VARS.get() {
        for (k, v) in vars {
            if env::var_os(k).is_none() {
                cmd.env(k, v);
            }
        }
    }
}

/// The invocation context derived from argv[0].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Argv0 {
    /// Invoked as `nub` — full CLI with subcommand dispatch.
    Nub,
    /// Invoked as `nubx` — enter `exec` directly.
    Nubx,
    /// Invoked as `node` via the PATH shim — augmented top-level execution.
    Node,
    /// Invoked as `npm`/`npx`/`pnpm`/`pnpx`/`yarn`/`yarnpkg` via a
    /// `~/.nub/shims` hardlink (`nub pm shim`) — the PM-shim dispatch
    /// ([`run_pm_shim`]). Spec: wiki/research/package-manager-shims.md.
    PmShim(nub_core::pm::shim::ShimName),
}

impl Argv0 {
    pub fn detect() -> Self {
        let argv0 = env::args_os().next().unwrap_or_default();
        let basename = PathBuf::from(&argv0)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        Self::classify(&basename)
    }

    /// Map an invoked basename (already `.exe`-stripped by `file_stem`) to a
    /// dispatch mode. `make install-dev` installs the dev binary under
    /// `nub-dev` / `nubx-dev` so it can sit alongside a released `nub` on
    /// PATH; the `-dev` suffix is stripped here so a dev symlink dispatches
    /// exactly like its release-named counterpart — notably `nubx-dev` must
    /// engage nubx mode, not silently fall through to plain `nub`.
    fn classify(basename: &str) -> Self {
        let name = basename.strip_suffix("-dev").unwrap_or(basename);
        match name {
            "nubx" => Self::Nubx,
            "node" => Self::Node,
            // `file_stem` already stripped any `.exe`, so the same parse serves
            // the Windows shim names.
            other => match nub_core::pm::shim::ShimName::parse(other) {
                Some(name) => Self::PmShim(name),
                None => Self::Nub,
            },
        }
    }
}

/// Nub — TypeScript-first developer supertool.
///
/// A Rust CLI that augments your Node.js with TypeScript execution,
/// auto-flag injection, .env loading, and more. Drop-in replacement
/// for `node` — anything `node <args>` accepts, `nub <args>` also
/// accepts, plus subcommands.
#[derive(Parser, Debug)]
#[command(
    name = "nub",
    about = "The all-in-one Node.js toolkit",
    long_about = None,
    disable_help_subcommand = true,
    disable_version_flag = true,
    args_conflicts_with_subcommands = true,
)]
pub struct Cli {
    /// Print version.
    #[arg(short = 'v', short_alias = 'V', long)]
    pub version: bool,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// Run as if started in <DIR>.
    #[arg(long, global = true, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Suppress Nub's non-error output.
    #[arg(short = 's', long, global = true)]
    pub silent: bool,

    /// Increase Nub's log verbosity (repeatable).
    #[arg(long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Color mode for Nub's output.
    #[arg(long, global = true, default_value = "auto", default_missing_value = "always", num_args = 0..=1, require_equals = true)]
    pub color: ColorWhen,

    /// Enable watch mode (alias for `nub watch`).
    #[arg(long)]
    pub watch: bool,

    /// File to execute, or `-` for stdin. When no subcommand matches,
    /// the first positional is treated as a file path and everything
    /// after it passes through to Node.
    #[arg(trailing_var_arg = true)]
    pub args: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run a package.json script (workspace-aware).
    Run {
        /// Script name from package.json#scripts. Omitted → list available scripts.
        script: Option<String>,

        /// Disable Nub's runtime augmentation for this invocation.
        #[arg(long)]
        node: bool,

        /// Run in all workspace packages. `--workspaces` is the npm-style alias.
        #[arg(short = 'r', long = "recursive", visible_alias = "workspaces")]
        recursive: bool,

        /// Filter workspace packages by name or glob. Repeatable: multiple
        /// `--filter`s union; `!`-prefixed filters subtract. `-F` is the alias.
        #[arg(short = 'F', long)]
        filter: Vec<String>,

        /// npm-style member selection: alias for `--filter <name>`. Long-only
        /// (the short `-w` is pnpm's `--workspace-root`). Repeatable.
        #[arg(long = "workspace", value_name = "NAME")]
        workspace: Vec<String>,

        /// Run from the workspace root regardless of cwd.
        #[arg(short = 'w', long)]
        workspace_root: bool,

        /// Add the workspace root package to the recursive set (npm-style;
        /// distinct from `--workspace-root`, which targets *only* the root).
        #[arg(long)]
        include_workspace_root: bool,

        /// Error if the filter selects zero packages. (Nub also errors on a
        /// zero-match filter by default; this is the explicit form.)
        #[arg(long)]
        fail_if_no_match: bool,

        /// Skip `pre<x>` / `post<x>` lifecycle hooks for every script run.
        #[arg(long)]
        ignore_scripts: bool,

        /// Override the shell used to invoke the script command.
        #[arg(long, value_name = "PATH")]
        script_shell: Option<String>,

        /// Run script bodies through a POSIX `sh` (found on PATH / a Git-for-Windows
        /// install) instead of the platform default. No-op on Unix (already `sh`);
        /// on Windows it replaces `cmd` so POSIX-isms (`FOO=1 …`, `$VAR`, `&&`) work.
        #[arg(long = "shell-emulator")]
        shell_emulator: bool,

        /// Buffer each package's output and flush it on completion (no
        /// interleaving). Default on CI / non-TTY.
        #[arg(long)]
        aggregate_output: bool,

        /// Skip topological predecessors of <pkg> (CI restart-after-failure).
        #[arg(long, value_name = "PKG")]
        resume_from: Option<String>,

        /// Max concurrent packages per topological chunk.
        #[arg(long, value_name = "N")]
        workspace_concurrency: Option<i32>,

        /// Run all packages concurrently with no topological ordering.
        #[arg(long)]
        parallel: bool,

        /// Stop the run on first failure. This is the default; the flag is
        /// accepted for explicitness/muscle-memory and is a no-op on its own.
        #[arg(long)]
        bail: bool,

        /// Don't stop on first failure; collect all results.
        #[arg(long = "no-bail")]
        no_bail: bool,

        /// Reverse topological order (dependents before dependencies).
        #[arg(long)]
        reverse: bool,

        /// Skip topological sort; treat all packages as one flat set.
        #[arg(long = "no-sort")]
        no_sort: bool,

        /// Run packages strictly one at a time, ignoring topological order
        /// (equivalent to `--no-sort --workspace-concurrency 1`).
        #[arg(long, conflicts_with = "parallel")]
        sequential: bool,

        /// Stream output with package-name prefix.
        #[arg(long)]
        stream: bool,

        /// Output reporter: `default` (prefixed/aggregated), `silent` (= `-s`),
        /// or `ndjson` (one JSON object per line for CI parsing).
        #[arg(long, value_enum, value_name = "MODE")]
        reporter: Option<ReporterMode>,

        /// Drop the `<dir> <script>:` prefix from each streamed output line so CI
        /// annotation matchers see the child's raw output. Pairs with `--stream`.
        #[arg(long = "reporter-hide-prefix")]
        reporter_hide_prefix: bool,

        /// Skip packages that don't have the named script.
        #[arg(long)]
        if_present: bool,

        /// Remaining arguments forwarded to the script.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Run a file in watch mode (restarts on change).
    Watch {
        /// File to watch and execute.
        file: String,

        /// Remaining arguments forwarded to the script.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Run a node_modules/.bin binary (same as nubx).
    Exec {
        /// Binary name to execute.
        bin: String,

        /// Disable Nub's runtime augmentation for this invocation.
        #[arg(long)]
        node: bool,

        /// Run the bin in every workspace package. `--workspaces` is the npm-style alias.
        #[arg(short = 'r', long = "recursive", visible_alias = "workspaces")]
        recursive: bool,

        /// Filter workspace packages by name or glob. Repeatable: multiple
        /// `--filter`s union; `!`-prefixed filters subtract. `-F` is the alias.
        #[arg(short = 'F', long)]
        filter: Vec<String>,

        /// npm-style member selection: alias for `--filter <name>`. Long-only
        /// (the short `-w` is pnpm's `--workspace-root`). Repeatable.
        #[arg(long = "workspace", value_name = "NAME")]
        workspace: Vec<String>,

        /// Run from the workspace root regardless of cwd.
        #[arg(short = 'w', long)]
        workspace_root: bool,

        /// Add the workspace root package to the recursive set (npm-style;
        /// distinct from `--workspace-root`, which targets *only* the root).
        #[arg(long)]
        include_workspace_root: bool,

        /// Error if the filter selects zero packages. (Nub also errors on a
        /// zero-match filter by default; this is the explicit form.)
        #[arg(long)]
        fail_if_no_match: bool,

        /// Max concurrent packages per topological chunk.
        #[arg(long, value_name = "N")]
        workspace_concurrency: Option<i32>,

        /// Run the bin in all packages concurrently with no topological ordering.
        #[arg(long)]
        parallel: bool,

        /// Remaining arguments forwarded to the binary.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Run a tool from `node_modules/.bin`, fetching it on a local miss
    /// (`npx`/`pnpm dlx`). This is the grammar behind the `nubx` entry point;
    /// it carries the workspace fan-out flags `nub exec` has PLUS the npx
    /// fetch-path flags (`-p`, `--no-install`, `-q`, …) that only make sense
    /// when a tool may be fetched. Hidden from `nub`'s own subcommand list —
    /// it is reachable only as the `nubx` argv0.
    #[command(hide = true)]
    Nubx {
        /// Binary (or package, with `-p`) name to execute.
        bin: String,

        /// Disable Nub's runtime augmentation for this invocation.
        #[arg(long)]
        node: bool,

        // ── workspace fan-out flags (preserved from `nub exec`) ──
        /// Run the bin in every workspace package. `--workspaces` is the npm-style alias.
        #[arg(short = 'r', long = "recursive", visible_alias = "workspaces")]
        recursive: bool,

        /// Filter workspace packages by name or glob. Repeatable: multiple
        /// `--filter`s union; `!`-prefixed filters subtract. `-F` is the alias.
        #[arg(short = 'F', long)]
        filter: Vec<String>,

        /// npm-style member selection: alias for `--filter <name>`. Long-only
        /// (the short `-w` is pnpm's `--workspace-root`). Repeatable.
        #[arg(long = "workspace", value_name = "NAME")]
        workspace: Vec<String>,

        /// Run from the workspace root regardless of cwd.
        #[arg(short = 'w', long)]
        workspace_root: bool,

        /// Add the workspace root package to the recursive set (npm-style;
        /// distinct from `--workspace-root`, which targets *only* the root).
        #[arg(long)]
        include_workspace_root: bool,

        /// Error if the filter selects zero packages.
        #[arg(long)]
        fail_if_no_match: bool,

        /// Max concurrent packages per topological chunk.
        #[arg(long, value_name = "N")]
        workspace_concurrency: Option<i32>,

        /// Run the bin in all packages concurrently with no topological ordering.
        #[arg(long)]
        parallel: bool,

        // ── npx fetch-path flags ──
        /// Fetch package SPEC and run a bin from it (the bin name may differ
        /// from the package). Repeatable. Forces the fetch path; `npx -p`.
        #[arg(short = 'p', long = "package", value_name = "SPEC")]
        package: Vec<String>,

        /// Never fetch: if the tool isn't installed locally, error instead of
        /// fetching it (`npx --no-install` / `--yes=false`).
        #[arg(long = "no-install")]
        no_install: bool,

        /// Alias of `--no-install`: refuse to fetch a missing tool (`npx --no`).
        #[arg(long = "no")]
        no_fetch: bool,

        /// Suppress the fetch progress output (`npx -q`/`--quiet`).
        #[arg(short = 'q', long)]
        quiet: bool,

        /// Accepted for `npx` parity; a no-op — nubx never prompts before fetching.
        #[arg(short = 'y', long)]
        yes: bool,

        /// Accepted for `npx` parity. Removed from npm v9+; nubx warns and ignores it.
        #[arg(long = "ignore-existing")]
        ignore_existing: bool,

        /// Remaining arguments forwarded to the binary.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Upgrade Nub to the latest version.
    Upgrade {
        /// Target version (default: latest).
        #[arg(long)]
        version: Option<String>,

        /// Show what would happen without performing the upgrade.
        #[arg(long)]
        dry_run: bool,

        /// Skip confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },

    /// Show help for a subcommand.
    Help {
        /// Subcommand to show help for.
        command: Option<String>,
    },

    /// Manage Node versions (install / ls / uninstall / pin).
    ///
    /// `nub node <file>` is NOT a passthrough — to run a file use `nub <file>`.
    /// The `--node` compat flag lives only on `run` / `nubx`, never here.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },

    /// Install dependencies from package.json via the embedded engine.
    ///
    /// Respects the project's existing lockfile (pnpm-lock.yaml,
    /// package-lock.json, …) for both resolution and layout; see
    /// src/pm_engine/ for the layout policy and the yarn write gate.
    #[command(visible_alias = "i")]
    Install {
        /// Hard-fail if the lockfile is out of date (default in CI).
        #[arg(long)]
        frozen_lockfile: bool,

        /// Re-resolve and rewrite the lockfile even when it's stale.
        #[arg(long, conflicts_with = "frozen_lockfile")]
        no_frozen_lockfile: bool,

        /// Use the lockfile when fresh, re-resolve when stale (default outside CI).
        #[arg(
            long,
            conflicts_with_all = ["frozen_lockfile", "no_frozen_lockfile"]
        )]
        prefer_frozen_lockfile: bool,

        /// Skip devDependencies; install only production deps.
        #[arg(short = 'P', long, visible_alias = "production")]
        prod: bool,

        /// Install only devDependencies.
        #[arg(short = 'D', long, conflicts_with = "prod")]
        dev: bool,

        /// Skip all lifecycle scripts (root and dependency).
        #[arg(long)]
        ignore_scripts: bool,

        /// Skip optionalDependencies.
        #[arg(long)]
        no_optional: bool,

        /// Never hit the network; fail if a package isn't cached.
        #[arg(long)]
        offline: bool,

        /// Use cached packages when available, network otherwise.
        #[arg(long, conflicts_with = "offline")]
        prefer_offline: bool,

        /// Resolve and write the lockfile, but skip linking node_modules.
        #[arg(long)]
        lockfile_only: bool,

        /// Re-resolve and relink even when the install state says up-to-date.
        #[arg(long)]
        force: bool,

        /// node_modules layout: `isolated` (pnpm-style) or `hoisted` (npm-style).
        /// Overrides the lockfile-derived default.
        #[arg(long, value_name = "MODE")]
        node_linker: Option<String>,

        /// Registry URL for this invocation (metadata, tarballs, audit).
        /// Overrides `registry` from `.npmrc`.
        #[arg(long, value_name = "URL")]
        registry: Option<String>,

        /// Run as if started in <DIR> (the pnpm spelling of `--cwd`).
        #[arg(short = 'C', long = "dir", value_name = "DIR")]
        dir: Option<PathBuf>,

        /// Scope to workspace packages matching PATTERN (repeatable). `-F` alias.
        #[arg(short = 'F', long, value_name = "PATTERN")]
        filter: Vec<String>,

        /// Production-only variant of `--filter`.
        #[arg(long, value_name = "PATTERN")]
        filter_prod: Vec<String>,

        /// Run across every workspace package (same as `--filter=*`).
        #[arg(short = 'r', long)]
        recursive: bool,

        /// Error when a workspace selector matches no packages.
        #[arg(long)]
        fail_if_no_match: bool,

        /// Include the workspace root in recursive operations.
        #[arg(long)]
        include_workspace_root: bool,
    },

    /// Clean install for CI: delete node_modules, install strictly from the
    /// lockfile (drift or a missing lockfile is a hard error).
    Ci {
        /// Skip all lifecycle scripts (root and dependency).
        #[arg(long)]
        ignore_scripts: bool,

        /// Skip optionalDependencies.
        #[arg(long)]
        no_optional: bool,

        /// Registry URL for this invocation (metadata, tarballs, audit).
        /// Overrides `registry` from `.npmrc`.
        #[arg(long, value_name = "URL")]
        registry: Option<String>,

        /// Run as if started in <DIR> (the pnpm spelling of `--cwd`).
        #[arg(short = 'C', long = "dir", value_name = "DIR")]
        dir: Option<PathBuf>,

        /// Scope to workspace packages matching PATTERN (repeatable). `-F` alias.
        #[arg(short = 'F', long, value_name = "PATTERN")]
        filter: Vec<String>,

        /// Production-only variant of `--filter`.
        #[arg(long, value_name = "PATTERN")]
        filter_prod: Vec<String>,

        /// Run across every workspace package (same as `--filter=*`).
        #[arg(short = 'r', long)]
        recursive: bool,

        /// Error when a workspace selector matches no packages.
        #[arg(long)]
        fail_if_no_match: bool,

        /// Include the workspace root in recursive operations.
        #[arg(long)]
        include_workspace_root: bool,
    },
}

/// The `nub node` version-management verbs. Spec: `wiki/commands/node-versions.md`.
/// Every verb wraps existing `nub-core` machinery (resolver / cache / downloader)
/// — no new runtime engine.
#[derive(Subcommand, Debug)]
pub enum NodeCommand {
    /// Provision one or more versions into nub's cache. Bare form reads the
    /// project pin. A version already on PATH (system/nvm) is reported + skipped.
    Install {
        /// Version(s) / alias(es) (`22`, `lts`, `22.13.0`, `latest`). Omitted →
        /// read the project's `.node-version` / `.nvmrc`.
        specs: Vec<String>,
    },
    /// List versions in nub's cache, newest first, marking the active one.
    Ls,
    /// Remove a version from nub's cache. Errors if the cwd resolves to it.
    Uninstall {
        /// The concrete version to remove (e.g. `22.13.0`).
        version: String,
    },
    /// Write the project's Node pin (`.node-version`, or `.nvmrc` in place).
    Pin {
        /// Version / alias to record (`22`, `lts`, `22.13.0`).
        version: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ColorWhen {
    Auto,
    Always,
    Never,
}

/// Top-level entry point. Returns the process exit code.
pub fn run() -> Result<i32> {
    // Reclaim the PATH shim temp dir exactly once, on return. The shim is
    // created lazily/idempotently by the spawn paths and is process-wide
    // (PID-keyed); it must outlive every — possibly parallel — child, so
    // cleanup belongs here at the top level, not per-spawn (which would race
    // concurrent workspace scripts). Drop runs on every return path, including
    // errors, because `run()` returns to `main` rather than calling `exit`.
    struct ShimCleanup;
    impl Drop for ShimCleanup {
        fn drop(&mut self) {
            nub_core::node::spawn::cleanup_shim();
        }
    }
    let _shim_cleanup = ShimCleanup;

    // Reap PATH shim dirs leaked by runs that were killed / crashed before their
    // own cleanup ran. Detached background thread (NOT on the spawn/teardown hot
    // path) so its directory scan adds zero latency to the run; best-effort, any
    // dir it misses is collected by a later invocation.
    nub_core::node::spawn::spawn_stale_shim_reaper();

    let argv0 = Argv0::detect();

    match argv0 {
        Argv0::Nubx => run_nubx(),
        Argv0::Node => run_as_node(),
        Argv0::PmShim(name) => {
            // The PM owns its argv — no nub flag parsing, everything after
            // argv[0] is handed through verbatim.
            let args: Vec<String> = env::args().skip(1).collect();
            run_pm_shim(name, &args)
        }
        Argv0::Nub => {
            // Running from the shim dir's own `nub` hardlink (it's first on
            // PATH once shims are installed): defer to the real binary so an
            // upgraded nub takes effect — the hardlink pins the OLD bytes, and
            // without this even `nub pm shim` (the re-link) would run stale.
            // Post-uninstall there's no other nub → fall through and run self.
            if let Some(real) = nub_core::pm::shim::nub_passthrough_target() {
                let args: Vec<String> = env::args().skip(1).collect();
                return exec_program(&real, &args, &[]);
            }
            run_nub()
        }
    }
}

/// Workspace execution options extracted from clap flags.
struct WorkspaceOpts {
    recursive: bool,
    /// Union of `--filter`/`-F` selectors and the `--workspace <name>` aliases
    /// (npm member selection desugars to a name filter).
    filter: Vec<String>,
    workspace_root: bool,
    /// Add the workspace-root package to the recursive set (`--include-workspace-root`).
    include_workspace_root: bool,
    /// `--fail-if-no-match`: the explicit, self-documenting form of the
    /// zero-match-filter error (which Nub also raises by default).
    fail_if_no_match: bool,
    workspace_concurrency: Option<i32>,
    parallel: bool,
    bail: bool,
    reverse: bool,
    sort: bool,
    stream: bool,
    if_present: bool,
    /// `--ignore-scripts`: skip every `pre<x>`/`post<x>` lifecycle hook.
    ignore_scripts: bool,
    /// `--script-shell <path>`: override the shell that runs each script body.
    script_shell: Option<String>,
    /// `--aggregate-output`: buffer each package's output, flush on completion.
    aggregate_output: bool,
    /// `--resume-from <pkg>`: drop topological predecessors of `<pkg>`.
    resume_from: Option<String>,
}

/// Per-script execution knobs that ride alongside the script name down through
/// the lifecycle (`run_single_script`*) and command-build (`build_script_command`)
/// paths. Distinct from [`WorkspaceOpts`] (which governs *which* packages run and
/// *how* they're scheduled); these govern *how a single script body is invoked*.
/// Derived once per run and threaded by `&` — no per-call recomputation.
#[derive(Clone, Copy, Default)]
struct ScriptExecOpts<'a> {
    /// `--ignore-scripts`: skip `pre<x>` / `post<x>` lifecycle hooks.
    ignore_scripts: bool,
    /// `--script-shell <path>`: override the script shell (wins over `.npmrc`).
    script_shell: Option<&'a str>,
}

/// Known subcommand names that clap should handle. `install`/`i`/`ci` route
/// to the embedded aube install engine (src/pm_engine/).
const SUBCOMMANDS: &[&str] = &[
    "run", "watch", "exec", "upgrade", "help", "node", "pm", "agent", "install", "i", "ci",
];

/// `pnpm install <pkg>` (and the `i` alias) is the add-to-dependencies form —
/// pnpm routes `install` with a package positional (or `-g`) through its `add`
/// command. Nub's argumentless `install` is a native clap command (no
/// positionals), and the global form `install -g <pkg>` is an add too, so detect
/// that compatibility shape before clap rejects the package positional / `-g` /
/// save flags as unknown and translate it into an engine `add` invocation.
///
/// nub's CLI frontend targets pnpm compatibility ONLY (not npm), so this routing
/// honors exactly the spellings `pnpm install <pkg>` accepts — no npm-isms
/// (`--omit`, `--no-save`, `-S`/`--save`, the npm `-w <name>` member selector).
///
/// Routes to `add` when `install`/`i` carries a positional package OR `-g`/
/// `--global` (before any `--` separator). Plain `nub install` (no positionals,
/// no `-g`) and `nub install <native-flags>` (e.g. `--frozen-lockfile`, `-r`,
/// `-F foo`, `-P` with no package) stay on the native install path. A `--`
/// separator stops the scan, so `nub install -- -g` keeps `-g` literal.
///
/// pnpm's save spellings are translated to the equivalent the engine `add`
/// accepts (aube's `AddArgs`, whose save shorts are uppercase `-D`/`-E`/`-O`):
/// - pnpm lowercase save shorts → aube long forms: `-d` → `--save-dev`,
///   `-o` → `--save-optional`, `-e` → `--save-exact`.
/// - `-p`/`-P`/`--save-prod` → dropped (save-to-`dependencies` is the `add`
///   default; this matches `pnpm add`'s default behavior).
/// - Everything else (`-D`/`--save-dev`, `-E`/`--save-exact`, `-O`/
///   `--save-optional`, `--save-peer`, `-g`/`--global`, `-w` (pnpm's boolean
///   `--workspace-root`), `-r`/`-F`/`--filter`/`-C`, positionals, …) is already
///   an `add`-accepted pnpm spelling — forwarded verbatim.
fn install_to_add_args(rest: &[String]) -> Option<Vec<String>> {
    let subcommand = rest.first()?.as_str();
    if !matches!(subcommand, "install" | "i") {
        return None;
    }

    // First pass over the pre-`--` args: decide whether this is an add (a
    // package positional or `-g`/`--global` present). Native install flags
    // alone keep `install` on its own path.
    let body = &rest[1..];
    let mut saw_separator_at: Option<usize> = None;
    let mut route_to_add = false;
    {
        // Value-taking flags whose argument must NOT be mistaken for a package
        // positional during the route decision. (pnpm `-w` is a boolean
        // `--workspace-root`, NOT a value flag — it is deliberately absent.)
        const VALUE_FLAGS: &[&str] = &[
            "-F",
            "--filter",
            "--filter-prod",
            "-C",
            "--dir",
            "--registry",
            "--node-linker",
        ];
        let mut i = 0;
        while i < body.len() {
            let arg = &body[i];
            if arg == "--" {
                saw_separator_at = Some(i);
                break;
            }
            if matches!(arg.as_str(), "-g" | "--global") {
                route_to_add = true;
            }
            let bare = arg.split('=').next().unwrap_or("");
            if !arg.starts_with('-') {
                // A bare token in operand position is a package spec.
                route_to_add = true;
            } else if VALUE_FLAGS.contains(&bare) && !arg.contains('=') {
                // Skip the separate value so it isn't read as a positional.
                i += 1;
            }
            i += 1;
        }
    }
    if !route_to_add {
        return None;
    }

    // Second pass: translate pnpm save spellings into the engine `add` grammar.
    let scan_end = saw_separator_at.unwrap_or(body.len());
    let mut out: Vec<String> = vec!["add".to_string()];
    for arg in &body[..scan_end] {
        match arg.as_str() {
            // Dropped: save-to-dependencies is the default for `add`.
            "-p" | "-P" | "--save-prod" => {}
            // pnpm lowercase save shorts → aube's long forms (aube's shorts are
            // uppercase `-D`/`-O`/`-E`).
            "-d" => out.push("--save-dev".to_string()),
            "-o" => out.push("--save-optional".to_string()),
            "-e" => out.push("--save-exact".to_string()),
            // Everything else is already an `add`-accepted pnpm spelling.
            other => out.push(other.to_string()),
        }
    }
    // Anything after `--` is forwarded literally (e.g. package specs that
    // start with a dash).
    if let Some(sep) = saw_separator_at {
        out.extend(body[sep..].iter().cloned());
    }
    Some(out)
}

/// PM-management verbs nub recognizes only to redirect. The pure-passthrough
/// frontend (A2) was disabled 2026-06-09 in favor of the normalized standard
/// surface (wiki/research/package-manager-normalized-surface.md):
/// `install`/`i`/`ci` graduated into SUBCOMMANDS (live engine dispatch), and
/// the rest of the aube verb surface graduated into the engine verb registry
/// (`pm_engine::ENGINE_VERBS` — stubs today, family fill-ins next). What's
/// left here is the rump of PM verbs that exist in *other* package managers
/// but not in the embedded engine; they error with the project's real PM
/// command instead of dispatching anything. Must stay disjoint from
/// SUBCOMMANDS and the engine registry (asserted in tests).
const PM_VERBS: &[&str] = &[
    // yarn (berry) / bun lockfile migration verb; the engine spells the
    // equivalent `import`, which is engine-routed.
    "migrate",
];

/// Whether a leading flag-run may be reordered to sit *after* `verb`. pnpm
/// accepts `pnpm -r <verb>` / `pnpm --filter <x> <verb>` for every workspace-
/// aware verb — not just `run`/`exec`, but the install family (`install`/`ci`/
/// `add`/…) AND the read-only info family (`list`/`ls`/`why`/`outdated`/
/// `licenses`/`audit`/…). A leading run-flag before one of these must reorder
/// into canonical subcommand-first order; otherwise the verb token falls
/// through to the Node-passthrough/file-runner path and Node fails on, e.g.,
/// `--require list` (`Cannot find module 'list'`) or `node: bad option:
/// --filter`, falsely reporting success.
///
/// The recognized set is exactly the PM verbs nub can dispatch — the same
/// authority the bareword arm consults (`SUBCOMMANDS` + the engine verb
/// registry). Tying the two together is what keeps them from drifting: a verb
/// nub dispatches in first position must also be reorderable behind a leading
/// workspace flag, or the two surfaces disagree (the install family worked only
/// because it was hand-listed here; the info family crashed purely because it
/// was absent). Verbs that don't actually accept `-r`/`--filter` are still safe
/// to reorder — clap (or the engine) rejects the unsupported flag downstream
/// with a proper non-zero error, exactly as pnpm does.
fn is_normalizable_leading_flag_verb(verb: &str) -> bool {
    SUBCOMMANDS.contains(&verb) || crate::pm_engine::lookup_verb(verb).is_some()
}

/// Accept pnpm's flag-before-subcommand order. pnpm takes `pnpm -r run build` AND
/// `pnpm run -r build`; nub's pre-parse otherwise only recognizes a subcommand in
/// first position, so a leading run-flag (`-r`, `--filter`, …) falls through to
/// the Node-passthrough path and Node fails on `--require run` (`Cannot find
/// module 'run'`). If the args begin with a run of the `run`/`exec` flags
/// (consuming the value of value-taking ones) immediately followed by a verb
/// [`is_normalizable_leading_flag_verb`] recognizes, reorder into canonical
/// subcommand-first order (`run -r build`, `install -r`, `list -r`). Anything
/// else — a Node flag, a file,
/// `nub <file>`, eval — leaves the args untouched, so file/passthrough/eval
/// dispatch is unaffected. Returns `None` when no normalization applies. Keep the
/// flag lists in sync with the `Run` subcommand's `#[arg]` set.
fn normalize_leading_run_flags(args: &[String]) -> Option<Vec<String>> {
    const BOOL_FLAGS: &[&str] = &[
        "-r",
        "--recursive",
        "--workspaces",
        "-w",
        "--workspace-root",
        "--include-workspace-root",
        "--fail-if-no-match",
        "--ignore-scripts",
        "--aggregate-output",
        "--parallel",
        "--bail",
        "--no-bail",
        "--reverse",
        "--no-sort",
        "--sequential",
        "--stream",
        "--node",
    ];
    const VALUE_FLAGS: &[&str] = &[
        "-F",
        "--filter",
        "--workspace",
        "--script-shell",
        "--resume-from",
        "--workspace-concurrency",
    ];
    let mut i = 0;
    let mut leading: Vec<String> = Vec::new();
    while i < args.len() {
        let bare = args[i].split('=').next().unwrap_or("");
        if BOOL_FLAGS.contains(&bare) {
            leading.push(args[i].clone());
            i += 1;
        } else if VALUE_FLAGS.contains(&bare) {
            let has_inline_value = args[i].contains('=');
            leading.push(args[i].clone());
            i += 1;
            if !has_inline_value && i < args.len() {
                leading.push(args[i].clone()); // the space-separated value
                i += 1;
            }
        } else {
            break; // not a run-flag — stop scanning
        }
    }
    if !leading.is_empty()
        && args
            .get(i)
            .is_some_and(|v| is_normalizable_leading_flag_verb(v))
    {
        let mut out = Vec::with_capacity(args.len());
        out.push(args[i].clone()); // subcommand first
        out.extend(leading); // then the moved run-flags
        out.extend(args[i + 1..].iter().cloned()); // then the subcommand's argv
        Some(out)
    } else {
        None
    }
}

fn run_nub() -> Result<i32> {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    // Accept pnpm's `nub -r run build` order (run-flags before the subcommand).
    let raw_args = normalize_leading_run_flags(&raw_args).unwrap_or(raw_args);

    // Pre-parse: extract nub-owned flags before clap sees them.
    // Everything clap doesn't own passes through to Node verbatim.
    let mut cwd: Option<PathBuf> = None;
    let mut version = false;
    let mut watch = false;
    let mut show_help = false;
    // `-h` → curated page, `--help` → verbose reference (the two intentionally
    // diverge; see run_help). Recorded so the short-circuit can pick the page.
    let mut help_verbose = false;
    let mut show_warnings = false;
    let mut silent = false;
    // Top-level `--node`: provision the project's Node (version management stays
    // on) but run with zero augmentation — the compat escape hatch. Routed to
    // `run_file_with_compat(_, true)`. See wiki/commands/node.md.
    let mut compat = false;
    let mut rest: Vec<String> = Vec::new();
    let mut subcommand_found = false;
    let mut env_file_vars: std::collections::HashMap<String, String> = Default::default();
    // Tracks whether any `--env-file` flag appeared, independent of whether it
    // yielded vars (an empty file still counts as intent). Drives suppression of
    // the eager `.env*` auto-discovery.
    let mut env_file_present = false;

    let mut i = 0;
    while i < raw_args.len() {
        let arg = &raw_args[i];
        // Once a subcommand (`run`/`exec`/`watch`/…) has been seen, stop matching
        // Nub's own flags: everything after it is that subcommand's argv and is
        // handed to clap verbatim, whose `trailing_var_arg` forwards post-
        // positional flags to the script/bin. Without this, `nub exec tsc
        // --version` would print Nub's version instead of tsc's, and
        // `nub run build --watch` would steal `--watch` from the script (the
        // three-position rule — see wiki/commands/run.md).
        if subcommand_found {
            rest.push(arg.clone());
            i += 1;
            continue;
        }
        match arg.as_str() {
            "--version" | "-v" | "-V" => version = true,
            "-h" => show_help = true,
            "--help" => {
                show_help = true;
                help_verbose = true;
            }
            "--watch" => watch = true,
            "--node" => compat = true,
            "--silent" | "-s" => silent = true,
            // `--verbose` is the user-facing spelling; `--show-warnings` is its
            // legacy twin. Both raise nub's diagnostic verbosity (e.g. the full
            // transport-error chain behind the one-line offline message).
            "--verbose" | "--show-warnings" => show_warnings = true,
            "--cwd" => {
                i += 1;
                if i < raw_args.len() {
                    cwd = Some(PathBuf::from(&raw_args[i]));
                }
            }
            s if s == "--color" || s.starts_with("--color=") || s == "--no-color" => {
                // --color (no value), --color=always, --no-color: all consumed, not forwarded
            }
            s if s == "--env-file" || s.starts_with("--env-file=") => {
                // Presence of the flag (even pointing at an empty/unreadable file)
                // opts the run out of eager `.env*` auto-discovery — only the
                // explicit file(s) load (the maintainer, 2026-06-15).
                env_file_present = true;
                let file_path = if s.starts_with("--env-file=") {
                    s.strip_prefix("--env-file=").unwrap().to_string()
                } else {
                    i += 1;
                    if i < raw_args.len() {
                        raw_args[i].clone()
                    } else {
                        String::new()
                    }
                };
                if !file_path.is_empty() {
                    // read_env_file refuses non-regular files (e.g. /dev/zero) and
                    // oversized files, so a hostile --env-file can't hang or OOM.
                    if let Some(content) =
                        nub_core::workspace::env::read_env_file(std::path::Path::new(&file_path))
                    {
                        // Route through parse_env (not dotenvy directly) so the
                        // explicit --env-file flag strips backtick-quoted values
                        // like Node's parser, matching the .env auto-load path.
                        for (k, v) in nub_core::workspace::env::parse_env(&content) {
                            // First-writer-wins across multiple `--env-file` flags
                            // (`or_insert`). Whether nub should instead be
                            // last-writer-wins (matching Node's own `--env-file`
                            // merge) is a separate open question, intentionally left
                            // unchanged by the auto-discovery-suppression change.
                            env_file_vars.entry(k).or_insert(v);
                        }
                    } else {
                        eprintln!("nub: cannot read env file: {file_path}");
                    }
                }
            }
            "-e" | "--eval" | "-p" | "--print" => {
                // Pass eval flags THROUGH to Node verbatim — `nub -e '<code>'`
                // becomes `node -e '<code>'`. This is what preserves Node's
                // `[eval]` process identity byte-for-byte: `process.argv` has no
                // script path, `process.argv[1]` is `undefined`, `require.main`
                // is `undefined`, `__filename`/`module.id` are `[eval]`,
                // `__dirname` is `.`, and Error stack frames read `at [eval]:…`.
                //
                // The previous implementation wrote the code to a temp `.ts` file
                // and ran THAT file so the preload hooks could transpile
                // non-erasable TS (enums, namespaces, parameter properties). But
                // running a real file leaked the tempfile path into every one of
                // those identity surfaces (a clear violation of the "drop-in
                // `node`" contract) AND broke `--input-type=module -e` entirely
                // (a real file can't carry `--input-type`, so Node threw
                // ERR_INPUT_TYPE_NOT_ALLOWED). Node's own `-e` does strip-only TS
                // — erasable type syntax works, non-erasable does not — and the
                // `-`/stdin path already behaves the same way, so passing through
                // is consistent with both Node and nub's other eval surface.
                //
                // Augmentation (fetch, the version-gated globals, env loading)
                // rides on the `--import` preload + injected NODE_OPTIONS, NOT on
                // the tempfile, so it is unaffected by this change. Forward the
                // flag, its code argument (if any), and the remaining argv. With
                // no code argument the bare flag still goes through so Node
                // produces its native behavior (`-e`/`--eval` → exit 9 "requires
                // an argument"; `-p`/`--print` reads from stdin).
                rest.push(arg.clone());
                if i + 1 < raw_args.len() {
                    rest.extend(raw_args[i + 1..].iter().cloned());
                }
                break;
            }
            _ => {
                // Check if this is the first positional and matches a subcommand
                // (nub-native, a verb registered to the embedded PM engine, or
                // the engine's hidden node-gyp re-entry verb — its lazy shims
                // re-invoke current_exe() with it mid-lifecycle-script).
                if rest.is_empty()
                    && !arg.starts_with('-')
                    && (SUBCOMMANDS.contains(&arg.as_str())
                        || arg == "__node-gyp-bootstrap"
                        || crate::pm_engine::lookup_verb(arg).is_some())
                {
                    subcommand_found = true;
                }
                rest.push(arg.clone());
                // Once we've seen a subcommand or a non-flag positional,
                // grab everything remaining.
                if !subcommand_found && !arg.starts_with('-') {
                    rest.extend(raw_args[i + 1..].iter().cloned());
                    break;
                }
            }
        }
        i += 1;
    }

    // Expand ${VAR} references in --env-file values now that all files have been
    // parsed (multiple --env-file flags may cross-reference each other), matching
    // the expansion load_env_files applies on the non-watch run path.
    nub_core::workspace::env::expand_env_map(&mut env_file_vars);

    // Capture --env-file vars for per-child Command::env application (A19): no
    // process-env mutation, so no `unsafe { env::set_var }` and no data race if a
    // dep threads during init. Shell-wins / `.env`-override precedence is applied
    // at each spawn site via overlay_env_file_vars / apply_env_file_vars.
    let _ = ENV_FILE_VARS.set(env_file_vars);
    // Record flag presence so the run/watch paths can suppress eager `.env*`
    // auto-discovery when the user named explicit env file(s).
    let _ = ENV_FILE_PRESENT.set(env_file_present);

    SHOW_WARNINGS.store(show_warnings, Ordering::Relaxed);
    SILENT.store(silent, Ordering::Relaxed);

    if let Some(ref dir) = cwd {
        env::set_current_dir(dir)?;
    }

    // `--node` wins over nub's help/version short-circuit: `nub --node -h` and
    // `nub --node -v` print the resolved Node's own help/version (the project's
    // pinned Node, vanilla), not nub's. Strip nub-owned `--node` and pass the
    // Node-native flag straight through to compat-mode spawn. `nub run --node` /
    // `nubx --node` carry their own grammar and never reach this top-level path.
    if compat && (version || show_help) {
        let flag = if version {
            "-v"
        } else if help_verbose {
            "--help"
        } else {
            "-h"
        };
        return run_file_with_compat(&[flag.to_string()], true);
    }

    if version {
        print_version();
        return Ok(0);
    }

    if show_help {
        // `nub <sub> -h`/`--help` → that subcommand's help; otherwise top-level.
        // Any recognized command routes (native, node/pm/agent, or an engine verb);
        // unknown words fall through to the top-level page.
        let sub = rest
            .first()
            .map(String::as_str)
            .filter(|s| is_help_routable(s));
        run_help(sub, help_verbose);
        return Ok(0);
    }

    if watch {
        let file = rest.first().cloned().unwrap_or_default();
        if file.is_empty() {
            bail!("nub --watch requires a file argument");
        }
        return run_watch(&file, &rest[1..]);
    }

    // If a subcommand was found, delegate to clap for structured parsing.
    if subcommand_found {
        return dispatch_subcommand(rest);
    }

    // No subcommand — check if this is a file path or a bareword.
    if rest.is_empty() {
        // Piped/redirected stdin with no script arg: execute stdin, like Node
        // does (`echo 'code' | node` runs the code). This is the no-positional,
        // non-TTY case only — reuse the existing `nub -` stdin path by injecting
        // the `-` positional and routing to the same runner. The interactive-TTY
        // case (bare `nub` at a terminal) shows the top-level help so a first-time
        // user gets oriented to nub's verbs; Node would start a REPL there, which
        // nub deliberately does not implement yet.
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return run_file_with_compat(&["-".to_string()], compat);
        }
        // Orient the first-time user instead of exiting silently — the curated
        // page (same as `nub -h`), returning cleanly rather than process-exiting.
        run_help(None, false);
        Ok(0)
    } else {
        let first = &rest[0];
        // A leading `-` is treated as Node passthrough (`nub --inspect file.js`).
        let is_node_passthrough = first.starts_with('.')
            || first.starts_with('/')
            || first.starts_with('-')
            || std::path::Path::new(first).extension().is_some();

        if is_node_passthrough {
            run_file_with_compat(&rest, compat)
        } else {
            // `init` is reserved for nub's own project init (not the PM
            // engine's npm-style manifest scaffold) — deliberately absent
            // from ENGINE_VERBS, answered with a "coming" note rather than
            // a PM redirect so nobody scaffolds the wrong shape meanwhile.
            if first == "init" {
                bail!(
                    "nub: \"init\" is reserved — nub's own project init is coming and \
                     hasn't shipped yet\n\
                     \x20\x20(to run a package.json script named init: nub run init)"
                );
            }
            // No magic auto-run (deliberate divergence from pnpm/bun, which run
            // `<pm> dev` as the dev script). But when the bareword is almost
            // certainly a script — it's defined in the local package.json#scripts,
            // or it's a conventional script name — lead with a confident `nub run`
            // hint instead of the neutral two-option message.
            const COMMON_SCRIPTS: &[&str] = &["dev", "build", "test", "start", "lint"];
            let is_known_script = env::current_dir()
                .ok()
                .and_then(|cwd| nub_core::workspace::detect::detect_project(&cwd))
                .is_some_and(|p| {
                    nub_core::workspace::scripts::resolve_script(&p.manifest, first).is_some()
                });
            if is_known_script || COMMON_SCRIPTS.contains(&first.as_str()) {
                bail!(
                    "nub: \"{first}\" is not a nub command — did you mean `nub run {first}`?\n\
                     \x20\x20(to run a file instead: nub ./{first})"
                );
            }
            // PM-management verbs are not nub commands: the A2 pure-passthrough
            // frontend is disabled pending the normalized standard surface (see
            // PM_VERBS). Redirect with the exact command to paste, nub-identity-
            // aware: a foreign-PM project gets that PM's verb; a fresh / nub-
            // identity project gets nub's own equivalent. The only PM_VERB is
            // `migrate` (yarn/bun lockfile migration), which nub spells `import`
            // — so the nub-identity redirect names `nub import`, never a
            // nonexistent `nub migrate`. If a future PM_VERB has no nub
            // equivalent, add it here rather than emitting `nub <verb>`.
            if PM_VERBS.contains(&first.as_str()) {
                let pm = suggest_package_manager(&env::current_dir()?);
                if pm == "nub" {
                    let nub_verb = match first.as_str() {
                        "migrate" => "import",
                        // No nub equivalent: fall back to the lockfile-detected
                        // foreign PM rather than suggesting a command nub lacks.
                        other => {
                            let foreign = detect_package_manager(&env::current_dir()?);
                            bail!(
                                "nub: \"{other}\" is not a nub command — run it with your \
                                 package manager:\n\x20\x20{foreign} {}",
                                rest.join(" ")
                            );
                        }
                    };
                    let nub_args: Vec<&str> = std::iter::once(nub_verb)
                        .chain(rest.iter().skip(1).map(String::as_str))
                        .collect();
                    bail!(
                        "nub: \"{first}\" is not a nub command — nub spells it `{nub_verb}`:\n\
                         \x20\x20nub {}",
                        nub_args.join(" ")
                    );
                }
                bail!(
                    "nub: \"{first}\" is not a nub command — run it with your package manager:\n\
                     \x20\x20{pm} {}",
                    rest.join(" ")
                );
            }
            bail!(
                "nub: \"{first}\" is not a nub command — see `nub --help`\n\
                 \x20\x20(to run a script: nub run {first} · to run a file: nub ./{first})"
            );
        }
    }
}

/// Value-consuming Nub flags per subcommand: the long/short forms that take a
/// separate-token value (`--filter @org/api`, `--cwd /tmp`). Used by
/// [`split_subcommand_argv`] to find where the subcommand's *positional*
/// (script/bin/file) sits, so a value like `build` in `run --filter build dev`
/// is recognized as the filter's value, not the script. Flags that only take an
/// attached value (`--color=never`, via `require_equals`) are NOT listed here —
/// they never swallow the next token. Globals (`--cwd`) appear under every
/// subcommand because clap accepts them anywhere before the positional.
///
/// Every separate-token value flag on `run` is listed here. The run-flag set
/// adds `--workspace <name>`, `--resume-from <pkg>`, `--script-shell <path>`,
/// and the `-F` short for `--filter` — each takes a following token, so each
/// must appear below or the positional split would mis-bind its value as the
/// script name (`nub run --workspace foo build` ⇒ member `foo`, script `build`,
/// not script `foo`).
fn value_consuming_flags(subcommand: &str) -> &'static [&'static str] {
    match subcommand {
        "run" => &[
            "--filter",
            "-F",
            "--workspace",
            "--resume-from",
            "--script-shell",
            "--cwd",
            "--workspace-concurrency",
        ],
        // Exec's workspace value-flags must be listed so `nubx --filter @org/api
        // tsc` binds `@org/api` to the filter, not the bin positional. (Exec's
        // workspace scope is exactly -r/--filter/--parallel; --workspace and
        // --workspace-concurrency take a following token like their `run` twins.)
        "exec" => &[
            "--filter",
            "-F",
            "--workspace",
            "--workspace-concurrency",
            "--cwd",
        ],
        // nubx carries exec's workspace value-flags PLUS npx's `-p`/`--package`
        // (repeatable, takes the package spec as a following token). They must be
        // listed so `nubx -p left-pad cowsay` binds `left-pad` to the package, not
        // the bin positional.
        "nubx" => &[
            "--filter",
            "-F",
            "--workspace",
            "--workspace-concurrency",
            "--package",
            "-p",
            "--cwd",
        ],
        "watch" => &["--cwd"],
        _ => &[],
    }
}

/// Split a subcommand's argv into the clap-parseable *prefix* (subcommand name +
/// position-1/2 Nub flags + the positional) and the *verbatim suffix* (position 3
/// — everything after the positional, forwarded to the script/bin unchanged).
///
/// This is the load-bearing fix for clap leading-flag theft: clap's global args
/// and auto-`--help` match anywhere in argv, so re-parsing the whole remainder
/// let `nub exec eslint --help` print Nub's help and `nub run build --node`
/// enable compat (both wrong — the flag is in position 3, the script's). By
/// finding the positional boundary ourselves and feeding clap only the prefix
/// (which has nothing *after* the positional to steal), every post-positional
/// token reaches the script/bin verbatim. Mirrors `run_nubx`'s manual split,
/// generalized to all three forwarding subcommands.
///
/// The positional is the first token after the subcommand that is neither a
/// flag (`-…`) nor the value of a preceding value-consuming flag. `--` (the
/// explicit separator) forces it: the token after `--` is the positional. If no
/// positional is present (`nub run`, `nub exec --help` with no bin), the suffix
/// is empty and the prefix is the whole input — clap then handles the help /
/// no-script cases as before.
fn split_subcommand_argv(rest: Vec<String>) -> (Vec<String>, Vec<String>) {
    let subcommand = rest[0].as_str();
    let value_flags = value_consuming_flags(subcommand);

    let mut i = 1; // skip the subcommand name itself
    while i < rest.len() {
        let arg = &rest[i];
        if arg == "--" {
            // Explicit separator: the next token is the positional; everything
            // after that is the verbatim suffix. Keep `--` in the prefix so clap
            // still binds the positional, and forward from the token after it.
            // (`nub run -- build --flag` → script `build`, args `["--flag"]`.)
            let positional_idx = i + 1;
            if positional_idx < rest.len() {
                let prefix = rest[..=positional_idx].to_vec();
                let suffix = rest[positional_idx + 1..].to_vec();
                return (prefix, suffix);
            }
            return (rest, Vec::new());
        }
        if arg.starts_with('-') {
            // A flag. If it consumes a separate-token value, skip that value too
            // so we don't mistake it for the positional.
            if value_flags.contains(&arg.as_str()) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // First bare token after the subcommand = the positional. The prefix is
        // everything up to and including it; the rest is forwarded verbatim — but
        // a single `--` immediately after the positional is the conventional
        // end-of-options separator (npm/pnpm/yarn/cargo all drop it), so consume
        // it. (`nub run build -- a b c` → args `["a","b","c"]`.) Only that first
        // `--` is stripped; any later `--` is a literal argument.
        let prefix = rest[..=i].to_vec();
        let mut start = i + 1;
        if rest.get(start).is_some_and(|t| t == "--") {
            start += 1;
        }
        let suffix = rest[start..].to_vec();
        return (prefix, suffix);
    }
    // No positional found (e.g. `nub run`, `nub exec --help`): hand the whole
    // input to clap, which lists scripts / shows help as appropriate.
    (rest, Vec::new())
}

/// Parse a forwarding subcommand (`run`/`exec`/`watch`) by splitting off the
/// verbatim position-3 suffix first, clap-parsing only the prefix, then
/// appending the suffix to the parsed `args`. `upgrade`/`help` have no
/// positional-forwarding semantics and go straight to clap.
fn dispatch_subcommand(rest: Vec<String>) -> Result<i32> {
    let subcommand = rest[0].clone();

    // `node` is a non-forwarding command group with bespoke bare-usage + invalid-
    // positional messages (spec: wiki/commands/node-versions.md). Handle it with a
    // manual sub-verb match rather than clap's generic "invalid subcommand" error,
    // so `nub node script.ts` yields the exact "use 'nub <file>'" guidance and bare
    // `nub node` prints the verb list instead of a clap usage error.
    if subcommand == "node" {
        return run_node(&rest[1..]);
    }

    // `pm` is the package-manager management group (`which`/`switch`/`update`/
    // `cache`). Like `node`, it's a non-forwarding manual sub-verb match rather
    // than a clap `Command` variant, so its bare-usage / invalid-verb messages
    // read like `nub node`'s and it never reaches clap dispatch.
    if subcommand == "pm" {
        return run_pm(&rest[1..]);
    }

    // `agent` is the AI-agent onboarding group (`docs`/`skill` — both print-only,
    // offline fallbacks for the homepage prompt's fetch of start.md/skill.md).
    // Like `node`/`pm`, it's a non-forwarding manual sub-verb match — its
    // bare-usage and invalid-verb messages read consistently and it never reaches
    // clap dispatch. Spec: .fray/ai-friendliness.md.
    if subcommand == "agent" {
        return crate::agent::run(&rest[1..]);
    }

    // The engine's lazy node-gyp shims re-invoke `current_exe()` (= nub)
    // with this hidden verb mid-lifecycle-script; intercept it before clap
    // (it's internal plumbing, not a documented verb) and dispatch straight
    // to the engine's bootstrap entry point.
    if subcommand == "__node-gyp-bootstrap" {
        return crate::pm_engine::run_node_gyp_bootstrap(&rest[1..]);
    }

    // Compatibility alias: npm and pnpm treat `install <pkg>` / `i <pkg>` (and
    // the global form `install -g <pkg>`) as a package add. Route that shape
    // through the engine's `add` implementation, translating npm save/spec/
    // workspace spellings, before the native argumentless-install clap variant
    // rejects the positional / `-g` / unknown save flags. Plain `nub install`
    // and `nub install <native-flags>` stay on the native install path.
    if let Some(add_argv) = install_to_add_args(&rest)
        && let Some(spec) = crate::pm_engine::lookup_verb("add")
    {
        let pm = suggest_package_manager(&env::current_dir()?);
        // `add_argv[0]` is the canonical verb ("add"); the engine wants the
        // args after the verb. Report the user's actual typed spelling so
        // usage/errors still read `nub install …`.
        return crate::pm_engine::dispatch_verb(spec, &subcommand, &add_argv[1..], &pm);
    }

    // Verbs registered to the embedded PM engine (the aube verb surface minus
    // nub-reserved and tool-identity verbs — see pm_engine::ENGINE_VERBS).
    // Dispatched before clap: these aren't clap variants; each family module
    // owns its own args parsing (today: stubs that error with the user's
    // real-PM fallback). `install`/`i`/`ci` are NOT in the registry — they
    // are live clap verbs handled below.
    if let Some(spec) = crate::pm_engine::lookup_verb(&subcommand) {
        // The PM hint is only consumed by the unwired-verb stub fallback
        // (`{pm} {verb}`); use the nub-identity-aware suggestion so a fresh /
        // nub-identity project gets a `nub`-flavored hint (the verb *is* a
        // future nub verb) while a foreign-PM project keeps its own PM. Wired
        // verbs ignore the hint entirely (they re-resolve identity per call).
        let pm = suggest_package_manager(&env::current_dir()?);
        return crate::pm_engine::dispatch_verb(spec, &subcommand, &rest[1..], &pm);
    }

    let forwards = matches!(subcommand.as_str(), "run" | "exec" | "watch" | "nubx");

    let (prefix, suffix) = if forwards {
        split_subcommand_argv(rest)
    } else {
        (rest, Vec::new())
    };

    let mut clap_args = vec!["nub".to_string()];
    clap_args.extend(prefix);
    let cli = Cli::parse_from(&clap_args);

    // Position-2 global flags (e.g. `nub run --silent build`) parse into the top-
    // level `Cli` fields; apply the ones with observable effects. `--cwd` is
    // applied here (the top-level pre-parse only handles position-1 `--cwd`).
    if cli.silent {
        SILENT.store(true, Ordering::Relaxed);
    }
    if cli.verbose > 0 {
        SHOW_WARNINGS.store(true, Ordering::Relaxed);
    }
    if let Some(ref dir) = cli.cwd {
        env::set_current_dir(dir)?;
    }

    match cli.command {
        Some(Command::Run {
            script,
            node,
            recursive,
            mut filter,
            workspace,
            workspace_root,
            include_workspace_root,
            fail_if_no_match,
            workspace_concurrency,
            parallel,
            bail: _bail,
            no_bail,
            reverse,
            no_sort,
            sequential,
            stream,
            reporter,
            reporter_hide_prefix,
            shell_emulator,
            if_present,
            ignore_scripts,
            script_shell,
            aggregate_output,
            resume_from,
            mut args,
        }) => {
            args.extend(suffix);
            // `--reporter`: `silent` is `-s`; `ndjson` switches every output site to
            // machine JSON (set the global once, read at each emission site).
            match reporter {
                Some(ReporterMode::Silent) => SILENT.store(true, Ordering::Relaxed),
                Some(ReporterMode::Ndjson) => REPORTER_NDJSON.store(true, Ordering::Relaxed),
                // No explicit `--reporter` flag: honor `npm_config_reporter=silent`
                // from the environment (pnpm parity — suppresses the run preamble).
                Some(ReporterMode::Default) | None => {
                    if npm_config_reporter_is_silent() {
                        SILENT.store(true, Ordering::Relaxed);
                    }
                }
            }
            if reporter_hide_prefix {
                HIDE_STREAM_PREFIX.store(true, Ordering::Relaxed);
            }
            if shell_emulator {
                SHELL_EMULATOR.store(true, Ordering::Relaxed);
            }
            // `--workspace <name>` is npm's member selection; it desugars to a
            // name filter and composes with any `--filter`/`-F` selectors.
            filter.extend(workspace);
            // `--include-workspace-root` / `--resume-from` imply a workspace run
            // even without `-r`/`--filter` (they only mean anything across the
            // member set); promote to recursive so run_script routes correctly.
            let recursive = recursive
                || parallel
                || sequential
                || include_workspace_root
                || resume_from.is_some();
            let ws_opts = WorkspaceOpts {
                recursive,
                filter,
                workspace_root,
                include_workspace_root,
                fail_if_no_match,
                // `--sequential` serializes: one package at a time.
                workspace_concurrency: if sequential {
                    Some(1)
                } else {
                    workspace_concurrency
                },
                parallel,
                bail: !no_bail,
                reverse,
                // `--sequential` also drops topological ordering (flat set).
                sort: !no_sort && !parallel && !sequential,
                // Keep `stream` as the *explicit* `--stream` request so the
                // non-TTY aggregate default (which checks `!stream`) isn't
                // defeated by `--parallel`. The prefixed-path decision below
                // ORs in `parallel`/concurrency separately.
                stream,
                if_present,
                ignore_scripts,
                script_shell,
                aggregate_output,
                resume_from,
            };
            run_script(script.as_deref(), node, &ws_opts, &args)
        }
        Some(Command::Watch { file, mut args }) => {
            args.extend(suffix);
            run_watch(&file, &args)
        }
        Some(Command::Exec {
            bin,
            node,
            recursive,
            mut filter,
            workspace,
            workspace_root,
            include_workspace_root,
            fail_if_no_match,
            workspace_concurrency,
            parallel,
            mut args,
        }) => {
            args.extend(suffix);
            // `--workspace <name>` desugars to a name filter, exactly as on `run`.
            filter.extend(workspace);
            // `--include-workspace-root`/`--parallel` imply a workspace run even
            // without `-r`/`--filter` (they only mean anything across the member
            // set); promote to recursive so run_exec_target routes correctly.
            let recursive = recursive || parallel || include_workspace_root;
            // Exec scope is exactly `-r`/`--filter`/`--parallel`; the script-only
            // WorkspaceOpts fields ride at inert defaults. `bail: false` is the one
            // load-bearing choice: `nub exec -r tsc` runs the bin in EVERY selected
            // member and aggregates failures (a non-zero overall exit), rather than
            // stopping at the first — so a member missing the bin, or a tool that
            // exits non-zero, never masks the others. (Exec has no `--no-bail` flag
            // to flip this; the aggregate-all behavior is the only mode.)
            let ws_opts = WorkspaceOpts {
                recursive,
                filter,
                workspace_root,
                include_workspace_root,
                fail_if_no_match,
                workspace_concurrency,
                parallel,
                bail: false,
                reverse: false,
                sort: !parallel,
                stream: false,
                if_present: false,
                ignore_scripts: false,
                script_shell: None,
                aggregate_output: false,
                resume_from: None,
            };
            // The workspace branch engages only on -r/--filter/--parallel;
            // a plain `nub exec tsc` stays the single-package path unchanged.
            if ws_opts.recursive || !ws_opts.filter.is_empty() || ws_opts.parallel {
                run_workspace_target(
                    WorkspaceTarget::Bin {
                        name: &bin,
                        args: &args,
                    },
                    node,
                    &ws_opts,
                )
            } else {
                run_exec(&bin, node, &args)
            }
        }
        Some(Command::Nubx {
            bin,
            node,
            recursive,
            mut filter,
            workspace,
            workspace_root,
            include_workspace_root,
            fail_if_no_match,
            workspace_concurrency,
            parallel,
            package,
            no_install,
            no_fetch,
            quiet,
            yes: _,
            ignore_existing,
            mut args,
        }) => {
            args.extend(suffix);
            filter.extend(workspace);
            let recursive = recursive || parallel || include_workspace_root;
            let workspace_run = recursive || !filter.is_empty() || parallel;

            // `--ignore-existing` was removed from npm v9+; accept it for muscle
            // memory but warn that it does nothing (matches real npx, which prints
            // a removed-argument notice and proceeds).
            if ignore_existing {
                eprintln!("nubx: --ignore-existing was removed in npm v9 and is ignored.");
            }
            // `-y`/`--yes` is a no-op: nubx (like `pnpm dlx`) never prompts before
            // fetching, so there is nothing to confirm.

            if workspace_run {
                // The npx fetch-path flags only make sense for the single-tool
                // fetch fallback, never a workspace fan-out across installed bins.
                if !package.is_empty() || no_install || no_fetch || quiet {
                    bail!(
                        "nubx: the workspace flags (-r/--filter/--parallel) run a \
                         locally-installed bin across packages and cannot be combined \
                         with the fetch flags (-p/--package, --no-install/--no, -q)."
                    );
                }
                let ws_opts = WorkspaceOpts {
                    recursive,
                    filter,
                    workspace_root,
                    include_workspace_root,
                    fail_if_no_match,
                    workspace_concurrency,
                    parallel,
                    bail: false,
                    reverse: false,
                    sort: !parallel,
                    stream: false,
                    if_present: false,
                    ignore_scripts: false,
                    script_shell: None,
                    aggregate_output: false,
                    resume_from: None,
                };
                run_workspace_target(
                    WorkspaceTarget::Bin {
                        name: &bin,
                        args: &args,
                    },
                    node,
                    &ws_opts,
                )
            } else {
                let dlx_flags = NubxDlxFlags {
                    package,
                    // `--no` is npx's alias for `--no-install` in nubx's
                    // no-prompt model: both refuse to fetch a missing tool.
                    no_install: no_install || no_fetch,
                    quiet,
                };
                run_exec_with_dlx(&bin, node, &args, Some(&dlx_flags))
            }
        }
        Some(Command::Upgrade {
            version,
            dry_run,
            yes,
        }) => run_upgrade(version.as_deref(), dry_run, yes),
        Some(Command::Help { command }) => {
            // `nub help <cmd>` routes to that command's help; `nub help` alone →
            // the curated top-level page. Same router as `nub <cmd> -h`.
            let sub = command.as_deref().filter(|s| is_help_routable(s));
            run_help(sub, false);
            Ok(0)
        }
        Some(Command::Install {
            frozen_lockfile,
            no_frozen_lockfile,
            prefer_frozen_lockfile,
            prod,
            dev,
            ignore_scripts,
            no_optional,
            offline,
            prefer_offline,
            lockfile_only,
            force,
            node_linker,
            registry,
            dir,
            filter,
            filter_prod,
            recursive,
            fail_if_no_match,
            include_workspace_root,
        }) => crate::pm_engine::run_install(crate::pm_engine::InstallFlags {
            frozen_lockfile,
            no_frozen_lockfile,
            prefer_frozen_lockfile,
            prod,
            dev,
            ignore_scripts,
            no_optional,
            offline,
            prefer_offline,
            lockfile_only,
            force,
            node_linker,
            registry,
            dir,
            filter: crate::pm_engine::WorkspaceFilterFlags {
                filter,
                filter_prod,
                recursive,
                fail_if_no_match,
                include_workspace_root,
            },
        }),
        Some(Command::Ci {
            ignore_scripts,
            no_optional,
            registry,
            dir,
            filter,
            filter_prod,
            recursive,
            fail_if_no_match,
            include_workspace_root,
        }) => crate::pm_engine::run_ci(crate::pm_engine::CiFlags {
            ignore_scripts,
            no_optional,
            registry,
            dir,
            filter: crate::pm_engine::WorkspaceFilterFlags {
                filter,
                filter_prod,
                recursive,
                fail_if_no_match,
                include_workspace_root,
            },
        }),
        // `node` is intercepted at the top of `dispatch_subcommand` (manual
        // sub-verb match in `run_node`) and never reaches clap here.
        Some(Command::Node { .. }) => unreachable!("`node` is handled before clap dispatch"),
        None => unreachable!(),
    }
}

fn run_nubx() -> Result<i32> {
    // nubx <bin> [args...] routes through the dedicated `Nubx` clap grammar
    // (its own workspace + npx flag surface; NOT `nub exec`). The three-position
    // rule is identical: a flag before the bin (`nubx --node eslint`, `nubx -p
    // left-pad pad`) is nubx's; a flag after the bin (`nubx eslint --node`,
    // `nubx eslint --help`) reaches the bin verbatim.
    let args: Vec<String> = env::args().skip(1).collect();

    // `--help`/`--version` are nubx's own flags only when they appear BEFORE the
    // bin positional (the three-position rule: a flag after the bin reaches the
    // bin verbatim — `nubx eslint --help` is eslint's help). When no bin name has
    // been seen yet and one of these leading flags appears, honor it like
    // `nub --help`/`nub --version` instead of bailing on "missing binary name"
    // (the bail fired before this check, so `nubx --help`/`--version` errored).
    for arg in &args {
        if arg == "--" || !arg.starts_with('-') {
            break; // bin positional (or its `--` separator) — stop scanning
        }
        match arg.as_str() {
            "--help" | "-h" => {
                run_help(Some("nubx"), arg == "--help");
                return Ok(0);
            }
            "--version" | "-v" | "-V" => {
                print_version();
                return Ok(0);
            }
            _ => {}
        }
    }

    if args.is_empty() || args.iter().all(|a| a.starts_with('-') && a != "--") {
        // No bin name at all (empty, or only leading flags like `nubx --node`).
        bail!(
            "nubx: missing binary name\nUsage: nubx [-p <spec>] [--node] [--no-install] <bin> [args...]"
        );
    }

    // `nubx` is nub's `npx`/`pnpm dlx`: a locally-installed bin runs locally (no
    // network), and a bin absent from `node_modules/.bin` transparently falls
    // back to DLX (fetch into a throwaway project + run). Arm that fallback for
    // the exec path below — `nub exec` itself stays no-network.
    NUBX_DLX_FALLBACK.store(true, Ordering::Relaxed);

    let mut rest = vec!["nubx".to_string()];
    rest.extend(args);
    dispatch_subcommand(rest)
}

fn run_as_node() -> Result<i32> {
    // Invoked as `node` via PATH shim. Default = full augmentation (Colin,
    // 2026-06-20: go all-in on the hijack). The single opt-out is `--node`: when
    // it appears anywhere BEFORE a `--` separator, strip it and run the
    // resolved/pinned Node VANILLA (`compat_mode=true` — no preload, no flag
    // injection, no `.env`, no NODE_OPTIONS/NODE_PATH/PATH-shim), exactly like
    // `nub --node`/`nub run --node`. Version resolution/pinning stays on in BOTH
    // cases — that's the legitimate job of the hijack. A `--node` AFTER `--` is a
    // literal program arg and is passed through untouched.
    let args: Vec<String> = env::args().skip(1).collect();
    let mut stripped: Vec<String> = Vec::with_capacity(args.len());
    let mut compat = false;
    let mut saw_separator = false;
    for arg in &args {
        if !saw_separator && arg == "--node" {
            compat = true;
            continue; // strip nub-owned --node before it reaches real node
        }
        if arg == "--" {
            saw_separator = true;
        }
        stripped.push(arg.clone());
    }
    if compat {
        run_file_with_compat(&stripped, true)
    } else {
        run_file(&args)
    }
}

// ── Subcommand implementations ───────────────────────────────────────

/// The project/tree-wide augmentation opt-out: a truthy `NODE_COMPAT` env var
/// forces compat mode (zero runtime augmentation — identical to the `--node`
/// flag) across every runtime entrypoint. Because env vars inherit to
/// descendants, setting it once (shell, `.envrc`, CI) covers the whole process
/// tree — the persistent form of `--node`. Version resolution/provisioning
/// stays ON (compat = no augmentation, not no-pinning), exactly like `--node`.
/// Truthy = `1`/`true`/`yes` (case-insensitive); empty/`0`/`false`/unset = off.
/// Brand-clean: `NODE_*` prefix (Node doesn't claim the name), not `NUB_*`/`AUBE_*`.
fn node_compat_env() -> bool {
    match env::var("NODE_COMPAT") {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => false,
    }
}

fn run_file(args: &[String]) -> Result<i32> {
    run_file_with_compat(args, false)
}

fn run_file_with_compat(args: &[String], compat_mode: bool) -> Result<i32> {
    let cwd = env::current_dir()?;
    run_file_in_dir(args, compat_mode, &cwd)
}

/// Run a file with the project context (Node pin, `.env`, PnP, webstorage) and
/// the spawned child's working directory all keyed to `cwd` — an EXPLICIT cwd
/// that overrides the process's `env::current_dir()`. The plain `nub <file>` path
/// passes the process cwd (a no-op override); the workspace-bin path threads each
/// member's dir so a node bin (`eslint`/`tsc`/`vitest`) run via `nub exec -r` sees
/// the member's `.env`, Node pin, and `.bin` chain — not the workspace root's. The
/// child's cwd is set on `SpawnConfig` so the override reaches Node itself, not
/// just nub's discovery (spawn_node otherwise inherits the parent's cwd).
fn run_file_in_dir(args: &[String], compat_mode: bool, cwd: &Path) -> Result<i32> {
    // A truthy `NODE_COMPAT` forces compat tree-wide (the persistent `--node`);
    // OR it into the per-invocation bit so `NODE_COMPAT=1 nub foo.ts` /
    // `NODE_COMPAT=1 node foo.ts` (via the hijack → run_file → here) run vanilla.
    let compat_mode = compat_mode || node_compat_env();
    // Preflight the project manifest first: an EACCES/unparseable package.json
    // otherwise reads as "no project" through every Option-returning reader on
    // this path (pin resolution, .env, PnP), so the run silently drops the
    // project context with no diagnostic. Surface the coded cause up front.
    check_manifest_json(cwd)?;
    // Fire point: `nub <file>` (and the hijack-descendant `node`, which routes
    // through run_as_node → run_file). A pinned-but-uncached version is downloaded
    // + installed from nodejs.org here, uv-style. (`nub run`/`nub exec` keep plain
    // discover_node — they don't version-check.)
    let node = nub_core::node::discovery::discover_or_provision_node(cwd)?;

    if !compat_mode {
        if let Some(w) = nub_core::node::discovery::engines_disagreement_warning(cwd, &node) {
            eprintln!("{w}");
        }
        nub_core::node::discovery::check_min_version(&node)?;
    }

    // .env loading: eager for all non-compat invocations per wiki/runtime/env-loading.md —
    // UNLESS the user passed `--env-file`, which suppresses auto-discovery entirely
    // (only the named file(s) load; the maintainer, 2026-06-15). `merge_child_env` applies the
    // gate. --env-file vars apply even in compat mode (explicit user flag).
    let project = nub_core::workspace::detect::detect_project(cwd);
    let auto_env = if !compat_mode {
        project
            .as_ref()
            .map(|p| nub_core::workspace::env::load_env_files(&p.root))
            .unwrap_or_default()
    } else {
        Default::default()
    };
    let env_vars = merge_child_env(
        auto_env,
        env_file_flag_present(),
        ENV_FILE_VARS.get().unwrap_or(&HashMap::new()),
    );

    let nub_binary = nub_core::node::spawn::current_nub_binary()?;
    // Yarn PnP: inject the user's own `.pnp.cjs` (spawn.rs gates this on
    // `!compat_mode`, so `--node` skips it regardless).
    let pnp_ctx = nub_core::pnp::detect(cwd);
    let config = nub_core::node::spawn::SpawnConfig {
        node: &node,
        user_args: args,
        compat_mode,
        show_warnings: SHOW_WARNINGS.load(Ordering::Relaxed),
        nub_binary: &nub_binary,
        env_vars: &env_vars,
        pnp: pnp_ctx.as_ref().map(|c| c.pnp_cjs.as_path()),
        cwd,
    };

    let result = nub_core::node::spawn::spawn_node(&config)?;
    // PATH shim cleanup is handled once at the top level (see `run`).
    Ok(nub_core::node::spawn::exit_code(&result))
}

fn run_script(
    script: Option<&str>,
    compat_mode: bool,
    ws: &WorkspaceOpts,
    args: &[String],
) -> Result<i32> {
    // Truthy `NODE_COMPAT` forces compat tree-wide (the persistent `--node`).
    let compat_mode = compat_mode || node_compat_env();
    let cwd = env::current_dir()?;
    // Preflight: a package.json that exists but is unreadable (EACCES) or
    // unparseable would otherwise be swallowed by detect_project into the
    // misleading "no package.json found" below. Surface the real, coded cause.
    check_manifest_json(&cwd)?;
    let project =
        nub_core::workspace::detect::detect_project(&cwd).ok_or_else(|| no_manifest_error(&cwd))?;

    // No script name (`nub run`): list available scripts instead of a raw clap
    // "required argument" error — same shape as the missing-named-script path.
    let Some(script) = script else {
        // `nub run` with no script name mirrors `pnpm run` with no args: it is
        // not an error (exit 0), it lists the package's runnable scripts. This
        // is distinct from nub's "no implicit script shortcuts" stance (which
        // bans bareword `nub test`/`nub start`); the explicit no-arg `run` verb
        // legitimately mirrors pnpm here so CI that probes `pnpm run` and
        // branches on the exit code sees the same success.
        match project.manifest.get("scripts") {
            Some(serde_json::Value::Object(map)) if !map.is_empty() => {
                println!("Available scripts:");
                println!("{}", list_scripts(&project.manifest));
            }
            _ => {
                println!("There are no scripts specified.");
            }
        }
        return Ok(0);
    };

    // Workspace-wide execution: -r, --filter, or --parallel.
    if ws.recursive || !ws.filter.is_empty() || ws.parallel {
        return run_workspace_target(WorkspaceTarget::Script(script, args), compat_mode, ws);
    }

    // `-w` / `--workspace-root` alone: run the script in the workspace ROOT
    // package only, regardless of cwd (run.md §--workspace-root: "targets *only*
    // the root"). Without this, standalone `-w` fell through to the single-package
    // path below and silently ran the cwd member's script instead of the root's.
    if ws.workspace_root {
        let ws_root = project
            .workspace_root
            .clone()
            .unwrap_or_else(|| project.root.clone());
        let root_project =
            nub_core::workspace::detect::detect_project(&ws_root).ok_or_else(|| {
                anyhow::anyhow!("--workspace-root: no package.json at {}", ws_root.display())
            })?;
        let cmd = nub_core::workspace::scripts::resolve_script(&root_project.manifest, script)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "missing script: \"{script}\" in the workspace root\n\nAvailable scripts:\n{}",
                    list_scripts(&root_project.manifest)
                )
            })?;
        let exec = ScriptExecOpts {
            ignore_scripts: ws.ignore_scripts,
            script_shell: ws.script_shell.as_deref(),
        };
        return run_single_script(script, &cmd, &root_project, compat_mode, args, &exec);
    }

    // Single-package execution. The selector may be an exact script name (one
    // script) or a `/regexp/` literal (every matching script, in package.json
    // order) — pnpm parity (`exec/commands/src/run.ts` getSpecifiedScripts).
    run_selected_scripts(script, &project, compat_mode, args, ws)
}

/// Run a `nub run <selector>` in a single package: resolve the selector to one or
/// more scripts (exact name, or every script matching a `/regexp/` literal) and
/// run them. One match → the inherit-stdio single-script path (unchanged). More
/// than one → each script in package.json order with a `<dir> <script>:` prefix,
/// bailing on the first failure unless `--no-bail`. Mirrors pnpm's
/// regex-selector run; nub runs the matched set sequentially (pnpm's
/// `--sequential` discipline) so output ordering is deterministic — `--parallel`
/// opts into concurrent execution.
fn run_selected_scripts(
    selector: &str,
    project: &nub_core::workspace::detect::Project,
    compat_mode: bool,
    args: &[String],
    ws: &WorkspaceOpts,
) -> Result<i32> {
    use nub_core::workspace::scripts::ScriptSelection;

    let scripts = match nub_core::workspace::scripts::select_scripts(&project.manifest, selector) {
        ScriptSelection::Matched(s) => s,
        ScriptSelection::UnsupportedRegexFlags => {
            bail!("RegExp flags are not supported in script command selector");
        }
        ScriptSelection::None => {
            if ws.if_present {
                return Ok(0);
            }
            bail!(
                "missing script: \"{selector}\"\n\nAvailable scripts:\n{}",
                list_scripts(&project.manifest)
            );
        }
    };

    let exec = ScriptExecOpts {
        ignore_scripts: ws.ignore_scripts,
        script_shell: ws.script_shell.as_deref(),
    };

    // Exact single match: the existing inherit-stdio path, byte-for-byte
    // unchanged (preserves the `$ <cmd>` echo + pre/post lifecycle).
    if scripts.len() == 1 {
        let name = &scripts[0];
        let cmd = nub_core::workspace::scripts::resolve_script(&project.manifest, name)
            .expect("selected script is present in the manifest");
        return run_single_script(name, &cmd, project, compat_mode, args, &exec);
    }

    // Multiple matched scripts (a `/regexp/` selector). Run each through the
    // prefixed path so every line is labeled with its script — the same
    // presentation nub's workspace runs use. Each matched script gets its own
    // pre/post lifecycle and the forwarded user args, matching pnpm.
    //
    // pnpm runs the matched set concurrently by default (workspace-concurrency,
    // default min(4, cpus)); `--sequential` (or `--workspace-concurrency 1`)
    // serializes. nub mirrors that: concurrent by default with the same cap,
    // sequential (in package.json order) when the concurrency resolves to 1.
    let aggregate = false;
    let concurrency = resolve_run_concurrency(ws);

    if concurrency <= 1 || scripts.len() <= 1 {
        // Sequential, package.json order. Bail (default) stops after the first
        // failure; `--no-bail` runs the whole set and returns the last failure.
        let mut overall = 0;
        for (idx, name) in scripts.iter().enumerate() {
            let cmd = nub_core::workspace::scripts::resolve_script(&project.manifest, name)
                .expect("selected script is present in the manifest");
            let code = run_single_script_prefixed(
                name,
                &cmd,
                project,
                compat_mode,
                args,
                ".",
                idx,
                &exec,
                aggregate,
            )?;
            if code != 0 {
                overall = code;
                if ws.bail {
                    break;
                }
            }
        }
        return Ok(overall);
    }

    // Concurrent: a bounded worker pool over the matched scripts, each prefixed.
    // `std::thread::scope` lets the workers borrow `project`/`args` without a
    // `'static` clone. A non-zero exit from any script is the overall exit.
    use std::sync::atomic::{AtomicI32, Ordering as AtomicOrdering};
    let failed = AtomicI32::new(0);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let exec_ref = &exec;
    std::thread::scope(|scope| {
        let num_workers = concurrency.min(scripts.len());
        let scripts_ref = &scripts;
        let next_ref = &next;
        let failed_ref = &failed;
        let handles: Vec<_> = (0..num_workers)
            .map(|_| {
                scope.spawn(move || {
                    loop {
                        let idx = next_ref.fetch_add(1, AtomicOrdering::Relaxed);
                        if idx >= scripts_ref.len() {
                            break;
                        }
                        let name = &scripts_ref[idx];
                        if let Some(cmd) =
                            nub_core::workspace::scripts::resolve_script(&project.manifest, name)
                        {
                            let code = run_single_script_prefixed(
                                name,
                                &cmd,
                                project,
                                compat_mode,
                                args,
                                ".",
                                idx,
                                exec_ref,
                                aggregate,
                            )
                            .unwrap_or(1);
                            if code != 0 {
                                failed_ref.store(code, AtomicOrdering::Relaxed);
                            }
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
    });
    Ok(failed.load(AtomicOrdering::Relaxed))
}

/// Effective concurrency for a single-package multi-script (regex) run. Mirrors
/// pnpm: `--sequential` → 1; an explicit `--workspace-concurrency N` → N (clamped
/// to ≥1); otherwise the default `min(4, available_parallelism)`.
fn resolve_run_concurrency(ws: &WorkspaceOpts) -> usize {
    if let Some(n) = ws.workspace_concurrency {
        return (n.max(1)) as usize;
    }
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    cpus.min(4)
}

/// What a workspace run executes in each selected member: either a package.json
/// script (`nub run -r build`) or a `node_modules/.bin` binary (`nub exec -r tsc`,
/// `nubx -r eslint`). Both share the entire scheduling machinery in
/// [`run_workspace_target`] — discovery, filtering, the dependency graph, chunking,
/// concurrency — and diverge only at the per-member leaf ([`run_one_member`]).
#[derive(Clone, Copy)]
enum WorkspaceTarget<'a> {
    /// A package.json script name + the user args forwarded to it.
    Script(&'a str, &'a [String]),
    /// A `.bin` binary name + the user args forwarded to it.
    Bin { name: &'a str, args: &'a [String] },
}

impl WorkspaceTarget<'_> {
    /// The label used in stream prefixes / recursion-reentry keying. For a script
    /// it's the script name; for a bin it's the bin name.
    fn label(&self) -> &str {
        match self {
            WorkspaceTarget::Script(name, _) => name,
            WorkspaceTarget::Bin { name, .. } => name,
        }
    }
}

fn run_workspace_target(
    target: WorkspaceTarget,
    compat_mode: bool,
    ws: &WorkspaceOpts,
) -> Result<i32> {
    // Truthy `NODE_COMPAT` forces compat tree-wide (the persistent `--node`).
    let compat_mode = compat_mode || node_compat_env();
    let cwd = env::current_dir()?;
    // See run_script: surface an unreadable/unparseable manifest with its coded
    // cause instead of the misleading "no package.json found".
    check_manifest_json(&cwd)?;
    let project =
        nub_core::workspace::detect::detect_project(&cwd).ok_or_else(|| no_manifest_error(&cwd))?;
    let project = &project;
    let ws_root = project
        .workspace_root
        .as_deref()
        .or(if ws.workspace_root {
            Some(project.root.as_path())
        } else {
            None
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "not in a workspace (no package.json#workspaces or pnpm-workspace.yaml found)"
            )
        })?;

    let mut members = nub_core::workspace::filter::discover_members(ws_root);
    if members.is_empty() && !ws.include_workspace_root {
        bail!("no workspace packages found under {}", ws_root.display());
    }

    // --include-workspace-root: the root package is not a glob-discovered member,
    // so synthesize it and always add it to the run set (npm semantics: it's an
    // *addition* to the recursive set, distinct from --workspace-root which
    // targets only the root). Its index is the appended slot.
    let root_idx = if ws.include_workspace_root {
        if let Ok(content) = std::fs::read_to_string(ws_root.join("package.json")) {
            if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                let name = manifest
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("workspace-root")
                    .to_string();
                members.push(nub_core::workspace::filter::WorkspacePackage {
                    name,
                    dir: ws_root.to_path_buf(),
                    manifest,
                });
                Some(members.len() - 1)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // Resolve filter(s) to matched indices. Multiple `--filter`s union their
    // includes and subtract their `!` exclusions (pnpm semantics). The
    // `--workspace <name>` aliases were already folded into `ws.filter`.
    let mut matched_set: std::collections::HashSet<usize> = if !ws.filter.is_empty() {
        let filters: Vec<_> = ws
            .filter
            .iter()
            .map(|s| nub_core::workspace::filter::Filter::parse(s))
            .collect();
        let v = nub_core::workspace::filter::apply_filters(&members, &filters, Some(ws_root));
        v.into_iter().collect()
    } else {
        // No filter: every member (the root is opt-in via --include-workspace-root).
        (0..members.len())
            .filter(|i| Some(*i) != root_idx)
            .collect()
    };

    // --include-workspace-root always adds the root regardless of the filter set.
    if let Some(idx) = root_idx {
        matched_set.insert(idx);
    }

    // Zero-match handling. A filter that selects nothing is a clean exit-0
    // no-op (matching pnpm: `No projects matched the filters in "<dir>"`), not
    // an error — CI commonly runs `--filter <maybe-empty>` and expects success.
    // `--fail-if-no-match` is the opt-in that turns the empty selection back
    // into a hard error (pnpm's `--fail-if-no-match` semantics, exit 1).
    if matched_set.is_empty() {
        if ws.fail_if_no_match {
            if !ws.filter.is_empty() {
                bail!(
                    "no packages matched the filter{}: {}",
                    if ws.filter.len() == 1 { "" } else { "s" },
                    ws.filter.join(", ")
                );
            }
            bail!("no packages to run (--fail-if-no-match)");
        }
        if !ws.filter.is_empty() {
            eprintln!(
                "No projects matched the filters in \"{}\"",
                ws_root.display()
            );
        }
        return Ok(0);
    }

    // pnpm's "Scope:" header (reportScope.ts): how many workspace projects this
    // run touches out of the total. `total` counts the workspace root too — it
    // is in `members` only when `--include-workspace-root` appended it
    // (`root_idx`), otherwise add one for it. Suppressed for a single selected
    // project (pnpm prints nothing then), and reads "all N" when every project
    // is selected.
    let total_projects = if root_idx.is_some() {
        members.len()
    } else {
        members.len() + 1
    };
    let selected = matched_set.len();
    if selected > 1 {
        if selected == total_projects {
            eprintln!("Scope: all {total_projects} workspace projects");
        } else {
            eprintln!("Scope: {selected} of {total_projects} workspace projects");
        }
    }

    // Build dependency graph for topological chunking.
    let name_to_idx: rustc_hash::FxHashMap<&str, usize> = members
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.as_str(), i))
        .collect();
    let dep_graph = nub_core::workspace::filter::build_dep_graph(&members, &name_to_idx);

    // Compute chunks.
    let mut chunks = if ws.sort && !ws.parallel {
        nub_core::workspace::filter::topological_chunks(&matched_set, &dep_graph)
    } else {
        // --no-sort or --parallel: one big chunk with everything.
        vec![matched_set.into_iter().collect()]
    };

    if ws.reverse {
        chunks.reverse();
    }

    // --resume-from <pkg>: drop every topological *predecessor* of <pkg>, i.e.
    // keep <pkg> and everything scheduled at or after it. Chunks are already in
    // execution order (topo, or reversed), so we drop whole leading chunks until
    // the one containing <pkg>, then trim that chunk to <pkg> + the rest of its
    // wave. Restart-after-CI-failure: the predecessors already succeeded.
    if let Some(ref resume_pkg) = ws.resume_from {
        let resume_idx = members
            .iter()
            .position(|m| m.name == *resume_pkg)
            .ok_or_else(|| {
                anyhow::anyhow!("--resume-from: no workspace package named \"{resume_pkg}\"")
            })?;
        let chunk_pos = chunks.iter().position(|c| c.contains(&resume_idx));
        match chunk_pos {
            Some(pos) => {
                chunks.drain(..pos);
                // Within the resume chunk, keep <pkg> and its co-wave peers but
                // not packages that already ran in an earlier (drained) position.
                // Co-wave peers have no ordering dependency on <pkg>, so running
                // them is correct (they are not predecessors).
                // (No intra-chunk trim needed: a wave has no internal order.)
            }
            None => {
                // <pkg> isn't in the selected/matched set: nothing to resume to.
                bail!("--resume-from: \"{resume_pkg}\" is not in the selected package set");
            }
        }
    }

    // Resolve concurrency. --parallel defaults to unlimited but
    // --workspace-concurrency can still cap it.
    let concurrency = if ws.parallel {
        ws.workspace_concurrency
            .and_then(|n| if n > 0 { Some(n as usize) } else { None })
            .unwrap_or(usize::MAX)
    } else {
        nub_core::workspace::filter::resolve_workspace_concurrency(ws.workspace_concurrency)
    };

    // Output discipline. `--aggregate-output` is also the CI / non-TTY default
    // (per run.md "Defaults"): when stdout isn't a TTY (or $CI is set) and the
    // user didn't ask to stream, buffer each package's output so logs don't
    // interleave. An explicit `--stream` opts back into live interleaving.
    let non_tty =
        !std::io::IsTerminal::is_terminal(&std::io::stdout()) || std::env::var_os("CI").is_some();
    // ndjson emits one self-describing JSON object per line live, so buffering
    // (aggregate) is both unnecessary and would withhold the events.
    let aggregate = !reporter_is_ndjson() && (ws.aggregate_output || (non_tty && !ws.stream));

    // Per-script knobs shared by every package's lifecycle (hooks + shell).
    let exec = ScriptExecOpts {
        ignore_scripts: ws.ignore_scripts,
        script_shell: ws.script_shell.as_deref(),
    };

    // Execute chunks.
    let mut total_failed = 0;
    // Packages that actually ran the script (had it declared). Drives the
    // pnpm "none of the selected packages has a <script> script" error below:
    // a recursive run where every selected package skipped the script is the
    // only missing-script failure, and `--if-present` (or `test`) waives even
    // that.
    let mut ran_count = 0usize;
    let bail = ws.bail;

    for chunk in &chunks {
        if bail && total_failed > 0 {
            break;
        }

        if concurrency <= 1 || chunk.len() <= 1 {
            // Sequential execution within this chunk.
            for &idx in chunk {
                if bail && total_failed > 0 {
                    break;
                }
                let leaf = MemberLeaf {
                    compat_mode,
                    stream: ws.stream
                        || ws.parallel
                        || concurrency > 1
                        || aggregate
                        || reporter_is_ndjson(),
                    color_idx: idx,
                    exec: &exec,
                    aggregate,
                };
                match run_one_member(target, &members[idx], ws_root, &leaf) {
                    MemberOutcome::Ran(0) => ran_count += 1,
                    MemberOutcome::Ran(_) => {
                        ran_count += 1;
                        total_failed += 1;
                    }
                    MemberOutcome::SkippedMissingScript => {}
                }
            }
        } else {
            // Channel-based work queue: N worker threads pull tasks as
            // slots free up (pLimit-style, not batch-based).
            use std::sync::Arc;
            use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
            use std::sync::mpsc;
            use std::thread;

            let failed = Arc::new(AtomicUsize::new(0));
            let ran = Arc::new(AtomicUsize::new(0));
            let (tx, rx) = mpsc::channel::<usize>();
            let rx = Arc::new(std::sync::Mutex::new(rx));

            let num_workers = concurrency.min(chunk.len());
            let workers: Vec<_> = (0..num_workers)
                .map(|_| {
                    let rx = Arc::clone(&rx);
                    let failed = Arc::clone(&failed);
                    let ran = Arc::clone(&ran);
                    // Clone the selected members so they cross the thread boundary
                    // (the borrowed `&members` slice can't be `move`d); paired with
                    // their original index for the prefix color. `WorkspacePackage`
                    // derives Clone, so this replaces the prior 4-field tuple
                    // snapshot with one structured clone.
                    let members_snapshot: Vec<(
                        usize,
                        nub_core::workspace::filter::WorkspacePackage,
                    )> = chunk
                        .iter()
                        .map(|&idx| (idx, members[idx].clone()))
                        .collect();
                    let ws_root_buf = ws_root.to_path_buf();
                    // Owned target + per-script knobs so they cross the thread
                    // boundary; reconstituted into the borrowed forms inside the
                    // worker (the borrowed forms can't be `move`d).
                    let target = OwnedTarget::from(target);
                    let ignore_scripts = exec.ignore_scripts;
                    let script_shell = ws.script_shell.clone();

                    thread::spawn(move || {
                        let exec = ScriptExecOpts {
                            ignore_scripts,
                            script_shell: script_shell.as_deref(),
                        };
                        let target = target.borrow();
                        loop {
                            let work_idx = match rx.lock() {
                                Ok(guard) => match guard.recv() {
                                    Ok(idx) => idx,
                                    Err(_) => break,
                                },
                                Err(_) => break,
                            };
                            if bail && failed.load(AtomicOrdering::Relaxed) > 0 {
                                continue;
                            }
                            let Some((_, member)) =
                                members_snapshot.iter().find(|(i, _)| *i == work_idx)
                            else {
                                continue;
                            };
                            let leaf = MemberLeaf {
                                compat_mode,
                                // The concurrent path always streams (prefixed) —
                                // its whole reason for existing is interleaved output.
                                stream: true,
                                color_idx: work_idx,
                                exec: &exec,
                                aggregate,
                            };
                            match run_one_member(target, member, &ws_root_buf, &leaf) {
                                MemberOutcome::Ran(0) => {
                                    ran.fetch_add(1, AtomicOrdering::Relaxed);
                                }
                                MemberOutcome::Ran(_) => {
                                    ran.fetch_add(1, AtomicOrdering::Relaxed);
                                    failed.fetch_add(1, AtomicOrdering::Relaxed);
                                }
                                MemberOutcome::SkippedMissingScript => {}
                            }
                        }
                    })
                })
                .collect();

            for &idx in chunk {
                let _ = tx.send(idx);
            }
            drop(tx);

            for w in workers {
                let _ = w.join();
            }

            total_failed += failed.load(AtomicOrdering::Relaxed);
            ran_count += ran.load(AtomicOrdering::Relaxed);
        }
    }

    // A recursive run where no selected package declared the script is a
    // clean exit-0 no-op with an informational notice on stdout, matching
    // pnpm 10.x's observed behavior — it prints "None of the selected
    // packages has a \"<script>\" script" and exits 0 rather than failing.
    // `--if-present` and `test` (npm/pnpm treat a missing `test` as success)
    // suppress even the notice. Only `Script` targets reach this; a `Bin` run
    // already errors per-member on a missing bin.
    if let WorkspaceTarget::Script(script, _) = target {
        if ran_count == 0 && total_failed == 0 && !ws.if_present && script != "test" {
            println!("None of the selected packages has a \"{script}\" script");
        }
    }

    if reporter_is_ndjson() {
        let total_pkgs: usize = chunks.iter().map(|c| c.len()).sum();
        emit_ndjson_summary(total_pkgs.saturating_sub(total_failed), total_failed);
    }
    if total_failed > 0 { Ok(1) } else { Ok(0) }
}

/// Recursion guard (the pnpm `runRecursive.ts:108-110` idea, brand-clean): true
/// when THIS package's script is already running in an ancestor `nub run` — the
/// inherited env names the same package + script. Skipping it stops a
/// `"build": "nub run -r build"` script from looping forever. Keys off the
/// `npm_package_name` / `npm_lifecycle_event` nub already sets per package — no
/// new env sentinel (a `NUB_*` var is brand-forbidden by AGENTS.md even for
/// internal use; a pnpm-named one carries ~zero real interop). Member names are
/// workspace-unique, and a name match avoids the symlink/relative-cwd fragility a
/// directory comparison carries. The top-level invocation has no matching
/// inherited env, so only the nested re-entry self-skips (silent, like pnpm).
/// Used by BOTH the sequential and concurrent run paths.
fn is_workspace_recursion_reentry(script: &str, pkg_name: &str) -> bool {
    std::env::var("npm_lifecycle_event").as_deref() == Ok(script)
        && std::env::var("npm_package_name").as_deref() == Ok(pkg_name)
}

/// Owned mirror of [`WorkspaceTarget`] so the target can cross the `thread::spawn`
/// boundary in the concurrent path (the borrowed form holds non-`'static`
/// references). Reconstituted into a borrowed `WorkspaceTarget` via [`borrow`]
/// inside each worker.
enum OwnedTarget {
    Script(String, Vec<String>),
    Bin(String, Vec<String>),
}

impl OwnedTarget {
    fn from(target: WorkspaceTarget) -> Self {
        match target {
            WorkspaceTarget::Script(name, args) => {
                OwnedTarget::Script(name.to_string(), args.to_vec())
            }
            WorkspaceTarget::Bin { name, args } => {
                OwnedTarget::Bin(name.to_string(), args.to_vec())
            }
        }
    }

    fn borrow(&self) -> WorkspaceTarget<'_> {
        match self {
            OwnedTarget::Script(name, args) => WorkspaceTarget::Script(name, args),
            OwnedTarget::Bin(name, args) => WorkspaceTarget::Bin { name, args },
        }
    }
}

/// Per-member execution knobs shared by both the sequential and concurrent
/// chunk loops. Bundles the genuinely-distinct inputs (compat, the streaming /
/// aggregate output discipline, the prefix-color index, the per-script exec
/// knobs) so [`run_one_member`] has one stable signature both loops call.
#[derive(Clone, Copy)]
struct MemberLeaf<'a> {
    compat_mode: bool,
    /// Scripts only: pipe + prefix each output line vs. inherit stdio with a
    /// single header. Bins always inherit stdio (see [`run_one_workspace_bin`]).
    stream: bool,
    /// Prefix color slot (pnpm-style per-member cycling).
    color_idx: usize,
    exec: &'a ScriptExecOpts<'a>,
    /// Scripts only: buffer + flush each member's output as one block.
    aggregate: bool,
}

/// What happened when a single member's [`WorkspaceTarget`] was run.
/// Distinguishes a member that lacked the named script — a pnpm-style silent
/// *skip* in a recursive run, never a failure — from one that actually ran (and
/// may have failed). The chunk loops fold `Ran(code != 0)` into `total_failed`
/// and count every `Ran` toward `ran_count`; `SkippedMissingScript` does
/// neither, so a `nub -r run <script>` over a workspace where only some packages
/// declare `<script>` runs them and exits 0, exactly like pnpm's `runRecursive`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MemberOutcome {
    /// The target ran; carries its exit code (0 = success).
    Ran(i32),
    /// The member has no such script — skipped, not counted as a failure.
    /// Reachable only for `Script` targets; a missing `Bin` is still an error
    /// (`nub exec` has no `--if-present` skip).
    SkippedMissingScript,
}

/// Run a workspace [`WorkspaceTarget`] in one member. The single per-member leaf
/// both chunk loops call: it owns the recursion-reentry skip, the per-target
/// dispatch, and the failure-print. Returns a [`MemberOutcome`] so the caller
/// can tell a missing-script skip apart from a real failure. The two targets
/// diverge only in their resolution + launch:
///   - `Script`: resolve from package.json#scripts, run the pre/main/post
///     lifecycle (streamed-prefixed or inherited-with-header per `leaf.stream`).
///   - `Bin`: resolve `<member>/node_modules/.bin/<name>` (walking up), launch
///     with inherited stdio.
fn run_one_member(
    target: WorkspaceTarget,
    member: &nub_core::workspace::filter::WorkspacePackage,
    ws_root: &Path,
    leaf: &MemberLeaf,
) -> MemberOutcome {
    // Recursion guard: skip a member whose own script is already running in an
    // ancestor `nub run` (see `is_workspace_recursion_reentry`). Scripts only —
    // `nub exec` sets no `npm_lifecycle_event`, so a bin re-entry can't false-match.
    if is_workspace_recursion_reentry(target.label(), &member.name) {
        return MemberOutcome::Ran(0);
    }
    match target {
        WorkspaceTarget::Script(script, args) => {
            run_one_workspace_script(script, args, member, ws_root, leaf)
        }
        WorkspaceTarget::Bin { name, args } => {
            MemberOutcome::Ran(run_one_workspace_bin(name, args, member, leaf))
        }
    }
}

/// Per-member leaf for a `Script` target. Resolves the named script in the
/// member, runs its pre/main/post lifecycle, and prints a failure line. A
/// member that doesn't declare `<script>` is a silent skip
/// ([`MemberOutcome::SkippedMissingScript`]) — pnpm's recursive-run semantics,
/// where only the packages that have the script run and the run exits 0 as long
/// as *some* package did (the all-missing case is caught by the caller). The
/// streamed vs. inherited disposition follows `leaf.stream`.
fn run_one_workspace_script(
    script: &str,
    args: &[String],
    member: &nub_core::workspace::filter::WorkspacePackage,
    ws_root: &Path,
    leaf: &MemberLeaf,
) -> MemberOutcome {
    let Some(cmd) = nub_core::workspace::scripts::resolve_script(&member.manifest, script) else {
        return MemberOutcome::SkippedMissingScript;
    };
    let fake_project = nub_core::workspace::detect::Project {
        root: member.dir.clone(),
        workspace_root: Some(ws_root.to_path_buf()),
        manifest: member.manifest.clone(),
    };
    let prefix = member_prefix(&member.dir, ws_root, &member.name);
    if leaf.stream {
        match run_single_script_prefixed(
            script,
            &cmd,
            &fake_project,
            leaf.compat_mode,
            args,
            &prefix,
            leaf.color_idx,
            leaf.exec,
            leaf.aggregate,
        ) {
            Ok(0) => {
                // pnpm prints a per-package "Done" suffix on success.
                let done_prefix = format_stream_prefix(&prefix, script, leaf.color_idx);
                eprintln!("{done_prefix}Done");
                MemberOutcome::Ran(0)
            }
            Ok(code) => {
                let err_prefix = format_stream_prefix(&prefix, script, leaf.color_idx);
                eprintln!("{err_prefix}exit {code}");
                MemberOutcome::Ran(code)
            }
            Err(e) => {
                let err_prefix = format_stream_prefix(&prefix, script, leaf.color_idx);
                eprintln!("{err_prefix}error: {e}");
                MemberOutcome::Ran(1)
            }
        }
    } else {
        eprintln!("  {} {script}", member.name);
        match run_single_script(
            script,
            &cmd,
            &fake_project,
            leaf.compat_mode,
            args,
            leaf.exec,
        ) {
            Ok(0) => MemberOutcome::Ran(0),
            Ok(code) => {
                eprintln!("  {} {script} — exit {code}", member.name);
                MemberOutcome::Ran(code)
            }
            Err(e) => {
                eprintln!("  {} {script} — error: {e}", member.name);
                MemberOutcome::Ran(1)
            }
        }
    }
}

/// Per-member leaf for a `Bin` target. Resolves `<member>/node_modules/.bin/<name>`
/// via `find_bin` (which walks up, so a hoisted root `.bin` entry counts — pnpm
/// PATH-chain semantics) and launches it with the member's own cwd so it sees the
/// member's `.env`, Node pin, and `.bin` chain — not the workspace root's. A member
/// missing the bin is a per-member error counted into the total, NOT a silent skip
/// (exec has no `--if-present`).
///
/// OUTPUT GAP (deliberate): bins inherit stdio — `launch_bin` and its augmented
/// node re-entry write straight to the parent's fd, so there is no pipe to
/// per-line-prefix the way the script path's `spawn_script_prefixed` does. We emit
/// one header line before launch (mirroring the non-stream script path's
/// `  <member> <script>` header, with the bin name in place of the script) and let
/// the bin's output flow through raw. Under `-r`/`--parallel` concurrency this
/// output can interleave across members; that is the accepted cost of not owning a
/// pipe, and matches how pnpm streams native tool output. The streaming params on
/// [`MemberLeaf`] are therefore unused here — they are script-only.
fn run_one_workspace_bin(
    bin: &str,
    args: &[String],
    member: &nub_core::workspace::filter::WorkspacePackage,
    leaf: &MemberLeaf,
) -> i32 {
    let Some(bin_path) = nub_core::workspace::scripts::find_bin(bin, &member.dir) else {
        eprintln!("{} | missing bin \"{bin}\"", member.name);
        return 1;
    };
    eprintln!("  {} {bin}", member.name);
    match launch_bin(&bin_path, args, leaf.compat_mode, &member.dir) {
        Ok(0) => 0,
        Ok(code) => {
            eprintln!("  {} {bin} — exit {code}", member.name);
            code
        }
        Err(e) => {
            eprintln!("  {} {bin} — error: {e}", member.name);
            1
        }
    }
}

fn run_single_script(
    script: &str,
    cmd: &str,
    project: &nub_core::workspace::detect::Project,
    compat_mode: bool,
    args: &[String],
    exec: &ScriptExecOpts,
) -> Result<i32> {
    // Run pre-script if it exists (unless --ignore-scripts).
    if !exec.ignore_scripts {
        let pre_name = format!("pre{script}");
        if let Some(pre_cmd) =
            nub_core::workspace::scripts::resolve_script(&project.manifest, &pre_name)
        {
            let code = spawn_script(&pre_cmd, project, compat_mode, &[], &pre_name, exec)?;
            if code != 0 {
                return Ok(code);
            }
        }
    }

    let code = spawn_script(cmd, project, compat_mode, args, script, exec)?;

    // Run post-script if it exists (unless --ignore-scripts).
    if code == 0 && !exec.ignore_scripts {
        let post_name = format!("post{script}");
        if let Some(post_cmd) =
            nub_core::workspace::scripts::resolve_script(&project.manifest, &post_name)
        {
            let post_code = spawn_script(&post_cmd, project, compat_mode, &[], &post_name, exec)?;
            if post_code != 0 {
                return Ok(post_code);
            }
        }
    }

    Ok(code)
}

/// Stdio disposition for a spawned package script.
#[derive(Clone, Copy)]
enum StreamMode {
    /// Inherit the parent's stdio (single-package `nub run`).
    Inherit,
    /// Pipe stdout/stderr so each line can be prefixed (workspace / `--stream`).
    Prefixed,
}

/// Build the shell `Command` for a package script with Nub's augmentation
/// applied exactly once: `NODE_OPTIONS` (injected flags + preload + webstorage),
/// the PATH shim prepended to the `node_modules/.bin` walk-up chain, `.env`
/// files, and the `npm_*` lifecycle vars.
///
/// This is the single augmentation path shared by inherited and prefixed
/// (streamed) execution — there is no second, divergent block. The PATH shim
/// temp dir is process-wide and reclaimed once on exit (see
/// [`nub_core::node::spawn::cleanup_shim`]), so no per-call guard is returned.
fn build_script_command(
    cmd: &str,
    project: &nub_core::workspace::detect::Project,
    compat_mode: bool,
    args: &[String],
    lifecycle_event: &str,
    stream: StreamMode,
    script_shell_override: Option<&str>,
) -> Result<std::process::Command> {
    use std::process::Command as StdCommand;

    // `.env` is NODE-SCOPED, not process-scoped (security + correctness, decided
    // 2026-06-10): nub does NOT eager-inject auto-loaded `.env*` into the whole
    // `nub run` script process. Each `node` a script spawns loads `.env` itself at
    // its own startup via the node-hijack (the `nub <file>` / `run_as_node` path
    // calls load_env_files) — so node tools (tsc/prisma) still get `.env`, but a
    // NON-node tool (`printenv`/aws/terraform) never receives the project's
    // secrets (matches npm/pnpm; the prior eager injection leaked them). It also
    // dissolves the NODE_ENV-cascade bug (bun#9635): the inner node reads the
    // right `.env.[NODE_ENV]` after an inline `NODE_ENV=…` is set, instead of the
    // outer load freezing the wrong file's values into the process. The explicit
    // `--env-file` FLAG is a distinct, user-set surface and still flows process-
    // wide (overlay below) — it's not auto-discovery. See wiki/runtime/env-loading.md.
    let mut env_vars: HashMap<String, String> = Default::default();
    // The explicit `--env-file` FLAG (a user-set surface, captured at startup)
    // still flows process-wide — it is not auto-`.env` discovery and applies in
    // every mode. Shell env still wins; applied here so it flows through the same
    // Command::env loop below (A19).
    overlay_env_file_vars(&mut env_vars);
    let bin_path =
        nub_core::workspace::scripts::bin_path(&project.root, project.workspace_root.as_deref());

    // Resolve Node once, up front: its path fills `npm_node_execpath` (A13/A38 —
    // threaded in, not a `node -e process.execPath` subprocess per `nub run`) and
    // its version drives flag injection in `compute_augmentation_env` below.
    let cwd = std::env::current_dir().unwrap_or_else(|_| project.root.clone());
    let node = nub_core::node::discovery::discover_node(&cwd)
        .unwrap_or_else(|_| nub_core::node::discovery::ResolvedNode::fallback());

    // Role-aware lifecycle UA: a `nub run`/`nub exec` script must report the
    // same incumbent-first `npm_config_user_agent` the engine's lifecycle path
    // already sends (so only-allow / which-pm-runs see `pnpm/<ver> nub/<v> …`
    // in a pnpm project, not a hardcoded `nub/<v> npm/?`). The role resolver
    // walks up from `cwd`; the version token is the run path's already-resolved
    // Node, threaded in so it isn't re-discovered.
    let ua_product = crate::pm_engine::run_lifecycle_ua_product(&cwd, &node.version.to_string());
    let npm_env = nub_core::workspace::scripts::npm_env(
        &project.manifest,
        &project.root,
        lifecycle_event,
        Some(cmd),
        node.path.as_str(),
        &ua_product,
    );

    // Shell precedence: the explicit `--script-shell <path>` flag wins, then a
    // `.npmrc` `script-shell=` setting, then the platform default. A custom
    // POSIX shell uses `-c`; only the implicit Windows `cmd` default uses `/d /s /c`.
    let custom_shell = script_shell_override
        .map(str::to_string)
        .or_else(|| nub_core::workspace::scripts::script_shell(&project.root));
    // `--shell-emulator`: with no explicit shell set, route the body through a
    // detected POSIX `sh` so Windows' `cmd` default is replaced (a no-op on Unix,
    // where the default is already `sh`). Error actionably if none is found.
    let custom_shell = match custom_shell {
        Some(s) => Some(s),
        None if shell_emulator_enabled() => Some(find_posix_sh().ok_or_else(|| {
            anyhow::anyhow!(
                "--shell-emulator: no POSIX `sh` found on PATH. On Windows, install Git for Windows (provides sh.exe) or use WSL; or pass --script-shell <path-to-sh>."
            )
        })?),
        None => None,
    };
    let (shell, shell_flag) = if let Some(ref s) = custom_shell {
        (s.as_str(), "-c")
    } else if cfg!(windows) {
        // npm's exact flags: `/d` (skip AutoRun), `/s` (strip outer quotes), `/c`.
        ("cmd", "/d /s /c")
    } else {
        ("sh", "-c")
    };
    // The implicit Windows `cmd` default must spawn with verbatim arguments — the
    // script body is passed to cmd.exe exactly as written (npm's
    // `windowsVerbatimArguments: true`), so Rust's MSVCRT re-quoting never mangles
    // a `node -e "…"` body or undoes the per-arg cmd escaping below. A custom POSIX
    // shell (e.g. Git-Bash `sh.exe` via `--shell-emulator`) does its own parsing,
    // so it takes the normal escaped-`arg` path.
    let cmd_verbatim = custom_shell.is_none() && cfg!(windows);

    // Append the user's extra args the way npm does (@npmcli/promise-spawn):
    // each arg is escaped for the target shell and spliced onto the UNescaped
    // script body, so multi-word / metachar args reach the script as single
    // literal tokens while the body's own globs/expansions still run. A raw
    // join (the prior behavior) let the shell re-split/expand the args. Compat,
    // not security — the args are the user's own argv (A42).
    let full_cmd = if args.is_empty() {
        cmd.to_string()
    } else {
        use nub_core::workspace::shell_escape;
        let use_cmd = shell_escape::is_cmd(shell);
        let double_escape = use_cmd && shell_escape::body_targets_batch_file(cmd);
        let mut full = cmd.to_string();
        for arg in args {
            full.push(' ');
            if use_cmd {
                full.push_str(&shell_escape::cmd(arg, double_escape));
            } else {
                full.push_str(&shell_escape::sh(arg));
            }
        }
        full
    };

    // Augmentation: NODE_OPTIONS + PATH shim so child `node` processes inside
    // the script inherit transpilation, polyfills, flag injection, and
    // webstorage. Computed once; `None` in compat or re-entrant invocations.
    // `node` was resolved above (its path fed npm_node_execpath).
    let nub_binary = nub_core::node::spawn::current_nub_binary()?;
    let pnp_ctx = nub_core::pnp::detect(&project.root);
    let aug = nub_core::node::spawn::compute_augmentation_env(
        &nub_binary,
        node.version,
        compat_mode,
        pnp_ctx.as_ref().map(|c| c.pnp_cjs.as_path()),
    );

    let mut command = StdCommand::new(shell);
    // Windows cmd: split the multi-token flag (`/d /s /c`) and pass the body
    // verbatim via `raw_arg`, the Rust equivalent of Node's
    // `windowsVerbatimArguments: true`, so MSVCRT re-quoting never mangles a
    // `node -e "…"` body or undoes the per-arg cmd escaping. A custom POSIX shell
    // (e.g. Git-Bash `sh.exe`) does its own parsing → the escaped-`arg` path.
    #[cfg(windows)]
    let spawned = if cmd_verbatim {
        use std::os::windows::process::CommandExt;
        for flag in shell_flag.split(' ') {
            command.arg(flag);
        }
        command.raw_arg(&full_cmd);
        true
    } else {
        false
    };
    #[cfg(not(windows))]
    let spawned = {
        let _ = cmd_verbatim;
        false
    };
    if !spawned {
        command.arg(shell_flag).arg(&full_cmd);
    }
    command.current_dir(&project.root);

    // PATH: shim dir (when augmenting) → `.bin` walk-up chain → system PATH.
    // `bin_path` is already `<.bin dirs>:<system PATH>`, so prepending the bare
    // shim dir gives `shim:.bin:system` — `.bin` BEFORE the system PATH so a local
    // tool shadows a global one (npm/pnpm parity), with the system PATH appearing
    // exactly once.
    let path: std::ffi::OsString = match aug.as_ref().and_then(|a| a.shim_dir.as_deref()) {
        Some(shim) => {
            let mut combined = std::ffi::OsString::from(shim);
            if !bin_path.is_empty() {
                combined.push(nub_core::PATH_LIST_SEPARATOR);
                combined.push(std::ffi::OsString::from(bin_path.clone()));
            }
            combined
        }
        None => std::ffi::OsString::from(bin_path.clone()),
    };
    command.env("PATH", path);

    // Default-on compile cache for script children (same decision as spawn_node,
    // 2026-06-10): a script's node subtree inherits this env, so heavyweight
    // single-file tools it launches (tsc/eslint/prettier-class bundles) load
    // their V8 blobs instead of reparsing. User-set values are untouched —
    // they're already in the inherited env and this only fills the unset case.
    if std::env::var_os("NODE_COMPILE_CACHE").is_none() {
        if let Some(dir) = nub_core::node::spawn::default_compile_cache_dir() {
            command.env("NODE_COMPILE_CACHE", dir);
        }
    }

    // $NODE: npm/pnpm point this at the node binary running the script so userland
    // `$NODE child.js` / `spawn(process.env.NODE, …)` invoke "the same Node." When
    // we augment, point it at the PATH-shim `node` (→ nub) instead of the raw binary
    // so an absolute-path `$NODE` re-enters nub and the child stays transpiled —
    // identical to bare `node child.js` (which hits the shim via PATH). Falls back to
    // the real binary with no shim (compat / re-entrant), where the inherited
    // NODE_OPTIONS preload still augments it. `npm_node_execpath` deliberately stays
    // the real binary (set in npm_env) — tooling derives Node's install prefix from
    // it, and the shim dir has no such layout. Set before the .env/npm_env loops so a
    // user `.env`-set NODE still wins (shell/.env precedence).
    let node_env = aug
        .as_ref()
        .and_then(|a| a.node_shim_exe())
        .unwrap_or_else(|| std::ffi::OsString::from(node.path.as_str()));
    command.env("NODE", node_env);

    if let Some(node_opts) = aug.as_ref().and_then(|a| a.node_options.as_ref()) {
        command.env("NODE_OPTIONS", node_opts);
    }
    if let Some(node_path) = aug.as_ref().and_then(|a| a.node_path.as_ref()) {
        command.env("NODE_PATH", node_path);
    }
    // localStorage-neutralize signal for the script subtree's node children (webstorage
    // flag-needed band, no user --localstorage-file): the preload reads + deletes it.
    if let Some(aug) = aug.as_ref() {
        aug.apply_localstorage_env(|k, v| {
            command.env(k, v);
        });
    }

    // `npm_config_node_gyp`: npm/pnpm always point this at a runnable node-gyp
    // (their bundled `node-gyp/bin/node-gyp.js`) so a script's `node
    // $npm_config_node_gyp …` resolves without a global node-gyp install. nub
    // hands out the engine's lazy `node-gyp.js` shim, which trampolines back
    // into nub via `AUBE_NODE_GYP_EXE` (handled by `__node-gyp-bootstrap`) to
    // bootstrap the real node-gyp on first use. Mirrors what the engine's
    // lifecycle path stamps (`aube-scripts::apply_script_settings_env`) so the
    // run and lifecycle paths agree. Set before the `npm_env` loop so a
    // user-set value still wins; the bootstrap markers are nub-internal env
    // (brand-exempt) and only meaningful to the shim. Failure to write the
    // (cheap) shim degrades to leaving the var unset — same as a plain Node.
    if let Ok(node_gyp_js) = aube::commands::install::node_gyp_bootstrap::lazy_js_shim_path() {
        command.env("npm_config_node_gyp", node_gyp_js);
        command.env("AUBE_NODE_GYP_EXE", &nub_binary);
        command.env("AUBE_NODE_GYP_PROJECT_DIR", &project.root);
    }

    // `npm_config_registry`: pnpm always exports the resolved registry to a
    // script's environment (defaulting to `https://registry.npmjs.org/`); npm
    // exports it whenever a registry is configured. nub left it unset, so a
    // script reading `$npm_config_registry` (publish wrappers, custom fetch
    // tooling) saw `undefined` under nub but a real URL under npm/pnpm. Read
    // the resolved registry from the project's `.npmrc`/config chain — the same
    // `NpmConfig` the engine resolves installs against — and export it. Set
    // before the `npm_env` loop so a user-set value still wins.
    let registry = aube_registry::config::NpmConfig::load(&project.root)
        .registry_for("")
        .to_string();
    command.env("npm_config_registry", registry);

    for (k, v) in &env_vars {
        command.env(k, v);
    }
    for (k, v) in &npm_env {
        command.env(k, v);
    }

    if let StreamMode::Prefixed = stream {
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
    }

    Ok(command)
}

fn spawn_script(
    cmd: &str,
    project: &nub_core::workspace::detect::Project,
    compat_mode: bool,
    args: &[String],
    lifecycle_event: &str,
    exec: &ScriptExecOpts,
) -> Result<i32> {
    let mut command = build_script_command(
        cmd,
        project,
        compat_mode,
        args,
        lifecycle_event,
        StreamMode::Inherit,
        exec.script_shell,
    )?;
    // Echo the command before running it, like npm/pnpm (and like Nub's own
    // workspace/streaming path). Single-package runs inherit stdio with no
    // per-package prefix, so just `$ <command>`, to stderr so it never pollutes
    // the script's stdout. Runs once per lifecycle script (pre/main/post).
    // Suppressed by `--silent`.
    if !SILENT.load(Ordering::Relaxed) {
        eprintln!("$ {cmd}");
    }
    // Forward terminating signals to the `sh -c <script>` child while it runs, so
    // `docker stop` / Ctrl-C / systemd reach the workload — not just Nub's leader.
    // A raw `command.status()` left the child orphaned on SIGTERM (the file-run
    // path already forwards via spawn_node; this path did not).
    let status = nub_core::node::spawn::status_forwarding_signals(&mut command)?;
    Ok(nub_core::node::spawn::exit_code_from_status(&status))
}

/// Streamed analog of [`run_single_script`]: runs the `pre<x>` → `<x>` →
/// `post<x>` lifecycle in order, each step through [`spawn_script_prefixed`] so
/// every output line keeps its `<dir> <script>: ` prefix. Returns the exit code
/// of the first failing step (pre or main short-circuits; post runs only when
/// main succeeded), matching npm/pnpm sequencing exactly — the same sequencing
/// `run_single_script` gives the non-streamed path. Without this, the default
/// concurrent/`--stream` `-r` path would run ONLY the main script and silently
/// skip pre/post hooks (the failure mode `run.md` records as having killed
/// `node --run`).
///
/// `args` flow only to the main script; pre/post receive `&[]`, like npm.
/// The `$ <cmd>` echo for each lifecycle step is emitted here (suppressed by
/// `--silent`) so both stream call sites get identical sequencing + echoing
/// from one place rather than duplicating it.
#[allow(clippy::too_many_arguments)]
fn run_single_script_prefixed(
    script: &str,
    cmd: &str,
    project: &nub_core::workspace::detect::Project,
    compat_mode: bool,
    args: &[String],
    prefix: &str,
    color_idx: usize,
    exec: &ScriptExecOpts,
    aggregate: bool,
) -> Result<i32> {
    // Each lifecycle step echoes `$ <cmd>` under its OWN name (prebuild /
    // build / postbuild), matching pnpm — the output-line prefix below already
    // uses the lifecycle name, so the echo lines up with it.
    let echo_cmd = |lifecycle_name: &str, lifecycle_cmd: &str| {
        // ndjson reports the `$ cmd` step via a JSON `start` event in
        // spawn_script_prefixed, so suppress the human echo to keep stdout pure JSON.
        if !SILENT.load(Ordering::Relaxed) && !reporter_is_ndjson() {
            let cmd_prefix = format_stream_prefix_sep(prefix, lifecycle_name, color_idx, "$ ");
            eprintln!("{cmd_prefix}{lifecycle_cmd}");
        }
    };

    // --ignore-scripts skips pre/post for the whole lifecycle; only the main
    // body runs (matching npm's interpretation, which run.md adopts).
    let run_hooks = !exec.ignore_scripts;

    // pre<script>: no user args, short-circuits the run on failure.
    if run_hooks {
        let pre_name = format!("pre{script}");
        if let Some(pre_cmd) =
            nub_core::workspace::scripts::resolve_script(&project.manifest, &pre_name)
        {
            echo_cmd(&pre_name, &pre_cmd);
            let (code, _) = spawn_script_prefixed(
                &pre_cmd,
                project,
                compat_mode,
                &[],
                prefix,
                &pre_name,
                color_idx,
                exec,
                aggregate,
            )?;
            if code != 0 {
                return Ok(code);
            }
        }
    }

    echo_cmd(script, cmd);
    let (code, _) = spawn_script_prefixed(
        cmd,
        project,
        compat_mode,
        args,
        prefix,
        script,
        color_idx,
        exec,
        aggregate,
    )?;
    if code != 0 {
        return Ok(code);
    }

    // post<script>: runs only after the main script succeeded; no user args.
    if run_hooks {
        let post_name = format!("post{script}");
        if let Some(post_cmd) =
            nub_core::workspace::scripts::resolve_script(&project.manifest, &post_name)
        {
            echo_cmd(&post_name, &post_cmd);
            let (post_code, _) = spawn_script_prefixed(
                &post_cmd,
                project,
                compat_mode,
                &[],
                prefix,
                &post_name,
                color_idx,
                exec,
                aggregate,
            )?;
            if post_code != 0 {
                return Ok(post_code);
            }
        }
    }

    Ok(code)
}

/// Serializes `--aggregate-output` flushes so one package's buffered block can't
/// interleave with another's when concurrent workers finish near-simultaneously.
/// Held only for the duration of a single flush (microseconds), so it does not
/// serialize the script *runs* themselves — only their final output emission.
static AGGREGATE_FLUSH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Spawn a script with piped stdout/stderr, prefixing each output line
/// with `<prefix> <script>: `. Returns (exit_code, collected_output).
///
/// `aggregate` selects the output discipline: `false` streams each line live
/// (interleaved across packages, the TTY default); `true` buffers the whole
/// run's output and flushes it as one contiguous block under
/// [`AGGREGATE_FLUSH_LOCK`] after the child exits (the CI / non-TTY default),
/// so a reader sees each package's output uninterrupted.
#[allow(clippy::too_many_arguments)]
fn spawn_script_prefixed(
    cmd: &str,
    project: &nub_core::workspace::detect::Project,
    compat_mode: bool,
    args: &[String],
    prefix: &str,
    script_name: &str,
    color_idx: usize,
    exec: &ScriptExecOpts,
    aggregate: bool,
) -> Result<(i32, String)> {
    use std::io::{BufRead, BufReader, Write};

    let mut command = build_script_command(
        cmd,
        project,
        compat_mode,
        args,
        script_name,
        StreamMode::Prefixed,
        exec.script_shell,
    )?;

    nub_core::node::spawn::group_on_spawn(&mut command);
    let mut child = command.spawn()?;
    // Relay docker stop / Ctrl-C to the streamed child's whole process group too
    // (workspace `-r` runs) — the `sh -c` won't pass a forwarded signal to node.
    nub_core::node::spawn::track_child_group(child.id());
    let mut output_buf = String::new();

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let prefix_out = format_stream_prefix(prefix, script_name, color_idx);
    let prefix_err = prefix_out.clone();

    // `--reporter=ndjson`: every output site emits a JSON object on stdout instead
    // of the prefixed human line. The package `name` is the manifest name (falling
    // back to the display prefix for an unnamed root package). Emitted from
    // spawn_script_prefixed so BOTH the sequential and concurrent run paths get it.
    let ndjson = reporter_is_ndjson();
    let pkg_name = project
        .manifest
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(prefix)
        .to_string();
    if ndjson {
        emit_ndjson("start", "info", &pkg_name, script_name, None, None);
    }
    let (name_out, script_out) = (pkg_name.clone(), script_name.to_string());
    let (name_err, script_err) = (pkg_name.clone(), script_name.to_string());

    // In aggregate mode the reader threads collect prefixed lines instead of
    // emitting them live; the parent flushes the buffered blocks once, below.
    let out_handle = std::thread::spawn(move || {
        let mut lines = Vec::new();
        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if ndjson {
                    emit_ndjson("log", "info", &name_out, &script_out, Some(&line), None);
                    continue;
                }
                let prefixed = format!("{prefix_out}{line}");
                if !aggregate {
                    println!("{prefixed}");
                }
                lines.push(prefixed);
            }
        }
        lines
    });

    let err_handle = std::thread::spawn(move || {
        let mut lines = Vec::new();
        if let Some(stderr) = stderr {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if ndjson {
                    emit_ndjson("log", "error", &name_err, &script_err, Some(&line), None);
                    continue;
                }
                let prefixed = format!("{prefix_err}{line}");
                if !aggregate {
                    eprintln!("{prefixed}");
                }
                lines.push(prefixed);
            }
        }
        lines
    });

    let status = child.wait()?;
    nub_core::node::spawn::untrack_child();
    let out_lines = out_handle.join().unwrap_or_default();
    let err_lines = err_handle.join().unwrap_or_default();
    let exit_code = nub_core::node::spawn::exit_code_from_status(&status);
    if ndjson {
        emit_ndjson(
            "end",
            if exit_code == 0 { "info" } else { "error" },
            &pkg_name,
            script_name,
            None,
            Some(exit_code),
        );
    }

    if aggregate && (!out_lines.is_empty() || !err_lines.is_empty()) {
        // One contiguous block per package: stdout then stderr, all under the
        // flush lock so concurrent workers never tear each other's output.
        let _guard = AGGREGATE_FLUSH_LOCK.lock();
        let stdout = std::io::stdout();
        let mut so = stdout.lock();
        for line in &out_lines {
            let _ = writeln!(so, "{line}");
        }
        let _ = so.flush();
        let stderr = std::io::stderr();
        let mut se = stderr.lock();
        for line in &err_lines {
            let _ = writeln!(se, "{line}");
        }
        let _ = se.flush();
    }

    for line in &out_lines {
        output_buf.push_str(line);
        output_buf.push('\n');
    }

    Ok((exit_code, output_buf))
}

/// The per-package label that leads each prefixed output line: the member's
/// directory relative to the workspace root. The workspace-root package itself
/// (`--include-workspace-root`) sits *at* the root, so its relative path is
/// empty — fall back to the package name so its lines aren't unlabeled.
fn member_prefix(dir: &std::path::Path, ws_root: &Path, name: &str) -> String {
    let rel = dir
        .strip_prefix(ws_root)
        .unwrap_or(dir)
        .to_string_lossy()
        .to_string();
    if rel.is_empty() {
        name.to_string()
    } else {
        // Forward slashes in the label on every OS (pnpm parity): the relative
        // path is `packages\core` on Windows, but the displayed prefix contract
        // is `packages/core` regardless of the host separator.
        rel.replace('\\', "/")
    }
}

/// Format a stream prefix with pnpm-compatible colors.
/// pnpm cycles through: cyan(36), magenta(35), blue(34), yellow(33), green(32), red(31).
/// The script name is always bright cyan(96).
fn format_stream_prefix(dir: &str, script: &str, idx: usize) -> String {
    // `--reporter-hide-prefix`: emit raw lines (no `<dir> <script>: ` lead) so CI
    // annotation matchers see the child's own output.
    if reporter_hide_prefix() {
        return String::new();
    }
    format_stream_prefix_sep(dir, script, idx, ": ")
}

fn format_stream_prefix_sep(dir: &str, script: &str, idx: usize, sep: &str) -> String {
    const DIR_COLORS: &[u8] = &[36, 35, 34, 33, 32, 31];
    let use_color = std::io::IsTerminal::is_terminal(&std::io::stderr())
        || std::env::var_os("FORCE_COLOR").is_some();
    if use_color {
        let c = DIR_COLORS[idx % DIR_COLORS.len()];
        format!("\x1b[{c}m{dir}\x1b[39m \x1b[96m{script}\x1b[39m{sep}")
    } else {
        format!("{dir} {script}{sep}")
    }
}

fn list_scripts(manifest: &serde_json::Value) -> String {
    match manifest.get("scripts") {
        Some(serde_json::Value::Object(map)) => map
            .keys()
            .map(|k| format!("  - {k}"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => "  (none)".to_string(),
    }
}

fn run_watch(file: &str, args: &[String]) -> Result<i32> {
    let cwd = env::current_dir()?;
    // Fire point (running a file): provision a pinned-but-uncached version.
    let node = nub_core::node::discovery::discover_or_provision_node(&cwd)?;
    if let Some(w) = nub_core::node::discovery::engines_disagreement_warning(&cwd, &node) {
        eprintln!("{w}");
    }
    nub_core::node::discovery::check_min_version(&node)?;

    // `nub watch` has no `--node` flag (the watch loop is nub's, so there's no
    // "vanilla watch" CLI escape — use `node --watch` in your shell for that).
    // But a truthy `NODE_COMPAT` is the AMBIENT tree-wide augmentation opt-out and
    // must be honored here too: run the pinned Node with `--watch` and ONLY the
    // user's argv — no flag injection, no preload, no eager `.env*` — matching the
    // zero-augmentation contract of `--node`/compat everywhere else. Version
    // provisioning above still applies (compat = no augmentation, not no-pinning).
    if node_compat_env() {
        let mut node_args = vec!["--watch".to_string(), "--watch-preserve-output".to_string()];
        node_args.push(file.to_string());
        node_args.extend(args.iter().cloned());
        let status = std::process::Command::new(node.path.as_str())
            .args(&node_args)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()?;
        return Ok(nub_core::node::spawn::exit_code_from_status(&status));
    }

    // Auto-loaded `.env*` files are handed to the watched Node as `--env-file`
    // args (below) so Node watches each path and re-reads it on every restart.
    // However Node's own `--env-file` parser does NOT expand `${VAR}` cross-
    // references, which would make `nub watch` inconsistent with `nub <file>`
    // (which uses `load_env_files` and gets full expansion). To close the gap we
    // also load and expand the env files here via `load_env_files` and inject the
    // expanded values via `cmd.env()` below. Node's `--env-file` never overrides a
    // var already present in the inherited environment (shell-wins), so the pre-
    // expanded `cmd.env()` values win. Unexpanded vars not covered by expansion
    // still reach the child through the `--env-file` args as before.
    //
    // Live-reload trade-off: if a `${REF}` source var changes while the watcher is
    // running, the pre-expanded dependents (in `cmd.env()`) won't update until nub
    // is restarted. This is the same trade-off `nub <file>` already has — the file
    // runner also pre-expands once and doesn't re-expand on changes.
    //
    // Precedence among the `.env*` files: Node is *last*-writer-wins, so we pass
    // `discover_env_files`' highest-priority-first list in reverse for nub's
    // first-writer-wins precedence to line up.
    //
    // Auto-discovery suppression (the maintainer, 2026-06-15): when the user passed
    // `--env-file`, the eager `.env*` discovery is suppressed on the watch path too
    // — neither the `discover_env_files` `--env-file` Node args nor the pre-expanded
    // `load_env_files` map are populated, so only the user's explicit file(s) reach
    // the child. This keeps `nub --watch --env-file …` consistent with the direct
    // file runner.
    let env_file_present = env_file_flag_present();
    let project = nub_core::workspace::detect::detect_project(&cwd);
    let env_file_paths = if env_file_present {
        Vec::new()
    } else {
        project
            .as_ref()
            .map(|p| nub_core::workspace::env::discover_env_files(&p.root))
            .unwrap_or_default()
    };
    // Pre-expand env vars (same path as the direct file runner). Auto `.env*` is
    // suppressed when `--env-file` is present; `merge_child_env` applies the gate
    // and overlays the explicit `--env-file` vars (shell still wins).
    let auto_env = project
        .as_ref()
        .map(|p| nub_core::workspace::env::load_env_files(&p.root))
        .unwrap_or_default();
    let env_vars = merge_child_env(
        auto_env,
        env_file_present,
        ENV_FILE_VARS.get().unwrap_or(&HashMap::new()),
    );

    let nub_binary = nub_core::node::spawn::current_nub_binary()?;
    let preload_path = nub_core::node::spawn::find_public_preload(&nub_binary);

    let mut node_args = vec!["--watch".to_string(), "--watch-preserve-output".to_string()];

    let node_options = env::var("NODE_OPTIONS").ok();
    let inject = nub_core::node::flags::compute_inject_flags(
        node.version.clone(),
        args,
        node_options.as_deref(),
        false,
    );
    for flag in &inject {
        node_args.push(flag.to_string());
    }

    // Reverse so the highest-priority `.env*` file lands last (Node's
    // last-writer-wins ⇒ nub's first-writer-wins precedence).
    for path in env_file_paths.iter().rev() {
        node_args.push(format!("--env-file={}", path.display()));
    }

    if let Some(preload) = &preload_path {
        // Tier-aware preload channel: `--require <cjs>` on the fast tier (keeps
        // Node's synchronous CJS entry path — the R1 fix), `--import <url>` on the
        // compat tier. Same choice the direct-spawn path makes; see
        // PreloadInjection. (Windows paths are \\?\-stripped + forward-slashed
        // inside to_file_url, so the compat URL is valid.)
        let inj = nub_core::node::spawn::preload_injection(preload, &node.version);
        node_args.push(inj.flag.to_string());
        node_args.push(inj.value);
    }

    node_args.push(file.to_string());
    node_args.extend(args.iter().cloned());

    let mut cmd = std::process::Command::new(node.path.as_str());
    cmd.args(&node_args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    // Inject pre-expanded .env* vars (including --env-file flag overrides,
    // already merged into env_vars via merge_child_env above). Node's own
    // --env-file args above still deliver the raw file content for vars that don't
    // need expansion, but these cmd.env() values win (shell-wins: Node's --env-file
    // never overrides an already-set env var).
    for (k, v) in &env_vars {
        cmd.env(k, v);
    }
    // This path spawns `node` directly and otherwise relies on OS env inheritance
    // for NODE_OPTIONS, so an inherited below-floor version-gated flag (e.g.
    // --experimental-webstorage on Node <22.4) would reach the child unstripped and
    // abort it with exit 9 ("not allowed in NODE_OPTIONS"). Snip such flags against
    // this watch invocation's resolved Node version before spawning — mirror of the
    // two direct-spawn sites in spawn.rs. See flags::strip_unsupported_node_options.
    if let Some(existing) = &node_options {
        let stripped =
            nub_core::node::flags::strip_unsupported_node_options(existing, &node.version);
        cmd.env("NODE_OPTIONS", stripped);
    }
    let status = cmd.status()?;

    // PATH shim cleanup is handled once at the top level (see `run`).
    Ok(nub_core::node::spawn::exit_code_from_status(&status))
}

fn run_exec(bin: &str, compat_mode: bool, args: &[String]) -> Result<i32> {
    run_exec_with_dlx(bin, compat_mode, args, None)
}

/// `run_exec`, plus the `nubx` npx-parity flags that steer the DLX fallback.
/// `dlx_flags` is `Some` only for the `nubx` entry point (which arms
/// `NUBX_DLX_FALLBACK`); plain `nub exec` passes `None` and never fetches.
///
/// Resolution order: a `-p`/`--package` spec forces the fetch path (the package
/// may ship a bin under a different name, so a coincidentally-matching local bin
/// must not shadow it — npx behaves the same). Otherwise local-first: a
/// `node_modules/.bin/<bin>` (or PnP-resolved bin) wins; on a miss the nubx
/// fallback fetches unless `--no-install`/`--no` forbids it.
fn run_exec_with_dlx(
    bin: &str,
    compat_mode: bool,
    args: &[String],
    dlx_flags: Option<&NubxDlxFlags>,
) -> Result<i32> {
    // Truthy `NODE_COMPAT` forces compat tree-wide (the persistent `--node`).
    let compat_mode = compat_mode || node_compat_env();
    let cwd = env::current_dir()?;

    // `-p <spec>` forces the fetch path: the bin to run may not match any local
    // bin, and npx never lets a local bin shadow an explicit `--package`.
    let force_fetch = dlx_flags.is_some_and(|f| !f.package.is_empty());

    if !force_fetch {
        if let Some(bin_path) = nub_core::workspace::scripts::find_bin(bin, &cwd) {
            return launch_bin(&bin_path, args, compat_mode, &cwd);
        }

        // Yarn PnP has no node_modules/.bin, so find_bin misses. Hand off to the
        // pnp-bin-run.cjs runner through nub's normal augmented path: that re-injects
        // --require .pnp.cjs (cwd is still a PnP tree), so the runner can resolve the
        // bin via pnpapi and load it with require() — the way `yarn exec` does, which
        // reads zip-stored bins on every tier (running the bin as a node *entry* breaks
        // on the compat tier, where --import forces it through the ESM loader). The
        // runner prints its own not-found message + exit 127 on a miss. Skipped in
        // compat mode (--node).
        if !compat_mode && nub_core::pnp::detect(&cwd).is_some() {
            if let Some(runner) = pnp_bin_runner_path() {
                let mut cmd_args = vec![runner, bin.to_string()];
                cmd_args.extend(args.iter().cloned());
                return run_file_with_compat(&cmd_args, compat_mode);
            }
        }
    }

    // `--no-install`/`--no`: the tool isn't local (we just missed) and the user
    // forbade fetching — error like `npx --no-install` rather than reaching the
    // registry. Exit 127 (command not found), matching the not-installed path.
    if dlx_flags.is_some_and(|f| f.no_install) {
        let what = dlx_flags
            .and_then(|f| f.package.first())
            .map(String::as_str)
            .unwrap_or(bin);
        eprintln!("nubx: `{what}` is not installed and --no-install forbids fetching it.");
        return Ok(127);
    }

    // Not in node_modules/.bin. The `nubx` entry point (and only it) falls back
    // to DLX here — fetch the tool into a throwaway project and run it, matching
    // `npx`/`pnpm dlx` (local-first, DLX as the fallback). nub is a complete
    // package manager, so `nubx <uninstalled-tool>` fetches-and-runs rather than
    // shelling out to the project's foreign PM. We follow `pnpm dlx` semantics:
    // no interactive confirm-prompt, just fetch+run (nub is pnpm-compatible).
    if NUBX_DLX_FALLBACK.load(Ordering::Relaxed) {
        let flags = dlx_flags.cloned().unwrap_or_default();
        return crate::pm_engine::run_dlx_for_nubx(bin, args, &flags);
    }

    // Plain `nub exec`. Per exec.md (decision 2026-05-26): `nub exec` does NOT
    // run a `dlx`/`npx` network fetch itself — that hits the registry and can
    // block on an interactive install prompt in CI, the exact failure that
    // decision removed. Print the install / run-ad-hoc suggestion and exit
    // non-zero (127, the conventional "command not found").
    let pm = suggest_package_manager(&cwd);
    let (add_cmd, dlx_cmd) = match pm.as_str() {
        "pnpm" => (format!("pnpm add -D {bin}"), format!("pnpm dlx {bin}")),
        "yarn" => (format!("yarn add -D {bin}"), format!("yarn dlx {bin}")),
        "bun" => (format!("bun add -d {bin}"), format!("bunx {bin}")),
        "npm" => (format!("npm install -D {bin}"), format!("npx {bin}")),
        // No lockfile and no foreign pin → suggest nub's own surface.
        _ => (format!("nub add -D {bin}"), format!("nubx {bin}")),
    };
    eprintln!("nub: `{bin}` is not installed in node_modules/.bin.");
    eprintln!("     install it ({add_cmd}), or run it ad-hoc with: {dlx_cmd}");
    Ok(127)
}

/// Absolute path to `runtime/pnp-bin-run.cjs` (sibling of nub's preload). `None`
/// only on a broken install where the runtime dir can't be located.
fn pnp_bin_runner_path() -> Option<String> {
    let nub_binary = nub_core::node::spawn::current_nub_binary().ok()?;
    let preload = nub_core::node::spawn::find_public_preload(&nub_binary)?;
    let runtime_dir = Path::new(&preload).parent()?;
    Some(
        runtime_dir
            .join("pnp-bin-run.cjs")
            .to_string_lossy()
            .into_owned(),
    )
}

/// Launch a resolved `node_modules/.bin` entry, shebang/extension-aware (A40).
/// A node script (`.js`/`.mjs`/`.cjs`, or a `#!…node` shebang) runs through the
/// augmented `node <path>` path; a Windows `.cmd`/`.bat`/`.ps1` runs through its
/// interpreter; anything else — a Windows `.exe`, or a POSIX binary / non-node
/// shebang (`#!/bin/sh`, …) — execs directly. The non-node launchers still get
/// nub's augmentation env so any `node` they spawn is transpile-enabled.
fn launch_bin(bin_path: &Path, args: &[String], compat_mode: bool, cwd: &Path) -> Result<i32> {
    if is_node_bin(bin_path) {
        let mut cmd_args = vec![bin_path.to_string_lossy().to_string()];
        cmd_args.extend(args.iter().cloned());
        // Run IN `cwd`, not the process cwd: a workspace-bin run (`nub exec -r`)
        // passes each member's dir so the node bin sees the member's `.env` / Node
        // pin / `.bin` chain. The single-package path passes the process cwd (a
        // no-op override). run_file_in_dir threads cwd onto SpawnConfig so the
        // child's working directory is set, not just nub's discovery.
        return run_file_in_dir(&cmd_args, compat_mode, cwd);
    }

    let mut cmd = bin_launcher(bin_path, args);
    // --env-file first (applies in compat too); aug's NODE_OPTIONS/PATH/NODE_PATH
    // are set after so nub's values win over any same-named env-file keys (A19).
    apply_env_file_vars(&mut cmd);
    if !compat_mode {
        apply_exec_augmentation(&mut cmd, cwd);
    }
    let status = cmd.status()?;
    Ok(nub_core::node::spawn::exit_code_from_status(&status))
}

/// True if the `.bin` entry should run under Node — a `.js`/`.mjs`/`.cjs`, or
/// (the typical Unix symlink) an extensionless file whose shebang names `node`.
fn is_node_bin(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some("js") | Some("mjs") | Some("cjs") => return true,
        // Windows shims / native executables are never run via `node`.
        Some("cmd") | Some("bat") | Some("ps1") | Some("exe") | Some("com") => return false,
        _ => {}
    }
    // Peek the shebang: `#!/usr/bin/env node`, `#!/usr/local/bin/node`, etc.
    // Match only on the shebang LINE — a `#!/bin/sh` shim (e.g. aube's `.bin`
    // entries) routinely names node in its body (`NODE_PATH=…`, `exec …/node …`),
    // and running such a script through `node` parses sh-as-JS (SyntaxError).
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 128];
    let n = std::io::Read::read(&mut f, &mut buf).unwrap_or(0);
    let head = &buf[..n];
    let shebang = match head.iter().position(|&b| b == b'\n') {
        Some(i) => &head[..i],
        None => head,
    };
    shebang.starts_with(b"#!") && shebang.windows(4).any(|w| w == b"node")
}

/// Build the OS launcher for a non-node `.bin` entry. Windows `.cmd`/`.bat` need
/// `cmd /C` and `.ps1` needs PowerShell (neither runs via bare `CreateProcess`);
/// a Windows `.exe` and any POSIX entry exec directly (the kernel honors the
/// shebang).
fn bin_launcher(path: &Path, args: &[String]) -> std::process::Command {
    #[cfg(windows)]
    {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("cmd") | Some("bat") => {
                let mut c = std::process::Command::new("cmd");
                c.arg("/C").arg(path).args(args);
                return c;
            }
            Some("ps1") => {
                let mut c = std::process::Command::new("powershell");
                c.arg("-NoProfile")
                    .arg("-ExecutionPolicy")
                    .arg("Bypass")
                    .arg("-File")
                    .arg(path)
                    .args(args);
                return c;
            }
            _ => {}
        }
    }
    let mut c = std::process::Command::new(path);
    c.args(args);
    c
}

/// Apply nub's augmentation env (NODE_OPTIONS preload + PATH shim + `.bin`
/// chain) to a non-node launcher, so any `node` the tool spawns is transpile-
/// enabled — the same env `nub run` gives a script. No-op if augmentation can't
/// be set up (e.g. preload not found).
fn apply_exec_augmentation(cmd: &mut std::process::Command, cwd: &Path) {
    let Ok(nub_binary) = nub_core::node::spawn::current_nub_binary() else {
        return;
    };
    let node = nub_core::node::discovery::discover_node(cwd)
        .unwrap_or_else(|_| nub_core::node::discovery::ResolvedNode::fallback());
    let pnp_ctx = nub_core::pnp::detect(cwd);
    let Some(aug) = nub_core::node::spawn::compute_augmentation_env(
        &nub_binary,
        node.version,
        false,
        pnp_ctx.as_ref().map(|c| c.pnp_cjs.as_path()),
    ) else {
        return;
    };
    // $NODE for tools that spawn a child node via `process.env.NODE` — point it at
    // the shim (→ nub) so the child stays augmented, matching `nub run` (see
    // build_script_command). Computed before `aug.shim_dir` is consumed below.
    let node_env = aug
        .node_shim_exe()
        .unwrap_or_else(|| std::ffi::OsString::from(node.path.as_str()));
    cmd.env("NODE", node_env);
    // localStorage-neutralize signal (webstorage flag-needed band, no user
    // --localstorage-file); applied before the partial moves of aug below.
    aug.apply_localstorage_env(|k, v| {
        cmd.env(k, v);
    });
    if let Some(node_options) = aug.node_options {
        cmd.env("NODE_OPTIONS", node_options);
    }
    if let Some(node_path) = aug.node_path {
        cmd.env("NODE_PATH", node_path);
    }
    let bin_chain = nub_core::workspace::scripts::bin_path(cwd, None);
    // shim dir → `.bin` chain → system PATH (`.bin` before the system PATH so a
    // local tool shadows a global one; `bin_chain` already ends with the system
    // PATH, so it appears exactly once).
    let path = match aug.shim_dir {
        Some(shim) => {
            let mut combined = std::ffi::OsString::from(shim);
            if !bin_chain.is_empty() {
                combined.push(nub_core::PATH_LIST_SEPARATOR);
                combined.push(std::ffi::OsString::from(bin_chain));
            }
            combined
        }
        None => std::ffi::OsString::from(bin_chain),
    };
    cmd.env("PATH", path);
}

// dlx removed per the maintainer 2026-05-26 (exec.md). nubx is local-bin-only; on a miss it
// SUGGESTS the PM dlx command and exits non-zero (127) — it never runs a fetch.

/// Walk up from `cwd` (bounded to 16 levels) looking for a lockfile, returning
/// the owning PM's name as soon as one is found. Shared by
/// [`detect_package_manager`] and [`suggest_package_manager`]; the two differ
/// only in their no-lockfile fallback.
fn lockfile_package_manager(cwd: &Path) -> Option<&'static str> {
    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        if dir.join("pnpm-lock.yaml").is_file() {
            return Some("pnpm");
        }
        if dir.join("yarn.lock").is_file() {
            return Some("yarn");
        }
        if dir.join("bun.lockb").is_file() || dir.join("bun.lock").is_file() {
            return Some("bun");
        }
        if dir.join("package-lock.json").is_file() {
            return Some("npm");
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// The package manager a redirect/hint should name, keyed off a committed
/// lockfile (the strongest signal that the project genuinely *is* that PM). With
/// no lockfile, falls back to npm. Callers that want the no-lockfile case to
/// honor the *declared* pin (or default to nub) use [`suggest_package_manager`].
fn detect_package_manager(cwd: &Path) -> String {
    lockfile_package_manager(cwd).unwrap_or("npm").to_string()
}

/// Like [`detect_package_manager`], but when there's no lockfile yet (a fresh
/// project that has never installed) it prefers the *declared* PM identity
/// (`packageManager` / `devEngines.packageManager` / a committed yarnPath) over a
/// blind npm fallback, and defaults to `nub` when even the pin is absent (or names
/// nub itself). Used by the `nubx` not-installed hint, where suggesting the wrong
/// PM's `add`/`dlx` (npm in a nub/pnpm context) is the bug this fixes.
fn suggest_package_manager(cwd: &Path) -> String {
    if let Some(pm) = lockfile_package_manager(cwd) {
        return pm.to_string();
    }
    // No lockfile: prefer the explicitly declared PM, else nub itself.
    match nub_core::pm::resolve::project_pm_identity(cwd).map(|id| id.name) {
        Some(name) if name != "nub" => name,
        _ => "nub".to_string(),
    }
}

/// The GitHub repo that hosts Nub's release artifacts. The self-owned tarball
/// channel downloads from here; mirror of install.sh.
const RELEASE_REPO: &str = "nubjs/nub";

/// Internal, test-only override for the GitHub *releases-download* base
/// (`https://github.com/<repo>/releases/download`). When set, `tarball_url` /
/// `checksum_url` point at it instead of github.com, so the self-owned
/// download+verify+swap path can be driven against a local `file://` fixture
/// with no network. UNSET in all normal operation (default behavior is
/// byte-identical); this is NOT a documented user knob — it exists purely so the
/// upgrade flow is end-to-end testable without publishing a real release. The
/// value is used verbatim as a URL prefix: the artifact for `v<version>` /
/// `<target>` is `<base>/v<version>/nub-<target>.tar.gz` (mirroring GitHub's
/// release-asset layout), so a fixture must reproduce that path shape.
const RELEASE_DOWNLOAD_BASE_ENV: &str = "NUB_RELEASE_BASE_URL";

/// Internal, test-only override for the "resolve `latest`" endpoint
/// (default: GitHub's `…/releases/latest` API). When set, `resolve_version`
/// fetches the JSON `{ "tag_name": "v<X.Y.Z>" }` from here instead. Same
/// contract as [`RELEASE_DOWNLOAD_BASE_ENV`]: unset in normal operation, never a
/// documented user surface, present only to make the upgrade resolve path
/// testable against a `file://` fixture.
const RELEASE_LATEST_API_ENV: &str = "NUB_RELEASE_LATEST_URL";

/// The releases-download base URL — the test seam's override if set, else the
/// canonical github.com path. Centralized so the override is read in exactly one
/// place and the default is the single source of truth.
fn release_download_base() -> String {
    std::env::var(RELEASE_DOWNLOAD_BASE_ENV)
        .unwrap_or_else(|_| format!("https://github.com/{RELEASE_REPO}/releases/download"))
}

/// The "resolve latest" API URL — the test seam's override if set, else GitHub's
/// `releases/latest` endpoint.
fn release_latest_api() -> String {
    std::env::var(RELEASE_LATEST_API_ENV)
        .unwrap_or_else(|_| format!("https://api.github.com/repos/{RELEASE_REPO}/releases/latest"))
}

/// The npm package users `npm install -g`. The bare `nub` name is an unrelated
/// third-party package — emitting it would clobber a working install, so every
/// npm-channel command must target the scoped `@nubjs/nub`.
const NPM_PACKAGE: &str = "@nubjs/nub";

/// How the running `nub` binary got onto disk. Detection is a single rung: the
/// canonicalized path of the binary matched against known install-layout shapes.
/// PM-owned installs delegate to the PM; the self-owned `~/.nub` curl-install
/// layout swaps in place from the GitHub release tarball.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UpgradeChannel {
    /// npm-family global install (the path contains `/node_modules/`).
    Npm,
    /// Homebrew install (path under a Homebrew prefix).
    Homebrew,
    /// The curl/`~/.nub` self-owned install — Nub owns the binary and swaps it
    /// in place. `install_dir` is the `…/.nub` root (parent of `bin/`).
    SelfOwned { install_dir: PathBuf },
    /// Couldn't tell — print the manual-instruction message and exit non-zero.
    Unknown,
}

/// Classify the install channel from the canonicalized binary path. Pure (no
/// I/O) so the routing matrix is unit-testable; the actual `current_nub_binary`
/// canonicalization happens in [`run_upgrade`]. Order matters: a `~/.nub` binary
/// pulled in as an npm dep would live under `node_modules`, so npm wins first;
/// the self-owned layout is `…/.nub/bin/nub`, never under `node_modules`.
fn detect_channel(bin_path: &Path) -> UpgradeChannel {
    let s = bin_path.to_string_lossy();
    if s.contains("/node_modules/") || s.contains("\\node_modules\\") {
        return UpgradeChannel::Npm;
    }
    if s.contains("/homebrew/") || s.contains("/Cellar/") || s.contains("/linuxbrew/") {
        return UpgradeChannel::Homebrew;
    }
    // Self-owned: the binary sits at `<install_dir>/bin/nub`, where install_dir
    // ends in `.nub` (install.sh: `$HOME/.nub/bin/nub`). Derive install_dir by
    // walking up from the binary so a copied-but-intact `.nub` tree still swaps.
    let install_dir = bin_path
        .parent()
        .filter(|bin_dir| bin_dir.file_name().is_some_and(|n| n == "bin"))
        .and_then(Path::parent)
        .filter(|install_dir| install_dir.file_name().is_some_and(|n| n == ".nub"));
    match install_dir {
        Some(dir) => UpgradeChannel::SelfOwned {
            install_dir: dir.to_path_buf(),
        },
        None => UpgradeChannel::Unknown,
    }
}

/// The release-artifact platform token for the current build, mirroring
/// install.sh's `uname -ms` → target mapping. `None` on a platform Nub doesn't
/// publish a tarball for (the self-owned channel then falls back to a clear
/// error rather than fetching a 404). musl vs glibc on Linux is a runtime
/// distinction install.sh makes via `ldd`/`/etc/alpine-release`; we encode the
/// glibc default here and document musl as the known gap (see note below).
fn platform_target() -> Option<&'static str> {
    // NOTE: a glibc build and a musl build of the same arch are the same Rust
    // `target_env` only under `target_env = "musl"`, so this distinguishes them
    // correctly when Nub itself is built for musl. The detection matches the
    // tarball names release.yml publishes.
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "x86_64") if cfg!(target_env = "musl") => Some("linux-x64-musl"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") if cfg!(target_env = "musl") => Some("linux-arm64-musl"),
        ("linux", "aarch64") => Some("linux-arm64"),
        _ => None,
    }
}

/// The exact npm command `nub upgrade` runs / suggests on an npm install. Pure
/// and centralized so there is a single place the scoped `@nubjs/nub` name is
/// emitted — the regression guard test pins it.
fn npm_upgrade_command(target: &str) -> String {
    format!("npm install -g {NPM_PACKAGE}@{target}")
}

/// Build the OS-correct `Command` that runs the npm-channel self-upgrade.
///
/// npm is not a native executable: on Windows it is the `npm.cmd` batch shim,
/// which the OS can only launch through `cmd.exe`, and on POSIX it is the `npm`
/// shell wrapper. We invoke npm via the platform shell rather than as a bare
/// program so PATH-resolution of that wrapper works the same way a user's own
/// `npm install -g …` would.
///
/// The Windows arm (`cmd /C npm install -g …`) is what fixes the npm-channel
/// upgrade on a plain Windows box: the prior `sh -c …` form needs an `sh` that
/// Windows does not ship (Git-Bash/MSYS is not guaranteed), so the only upgrade
/// path npm-installed Windows users have was failing with "program not found"
/// (CI's Windows runners carry Git-Bash, which masked it). Args are passed as
/// discrete tokens, not a single joined string, so there is no shell-quoting to
/// get wrong — the `@nubjs/nub@<target>` spec is one argv element either way.
fn npm_upgrade_command_invocation(target: &str) -> std::process::Command {
    let spec = format!("{NPM_PACKAGE}@{target}");
    if cfg!(windows) {
        // `cmd /C npm …` so the `npm.cmd` batch shim resolves + runs. `/C`
        // carries the command and its args as separate tokens.
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "npm", "install", "-g"]).arg(spec);
        cmd
    } else {
        let mut cmd = std::process::Command::new("npm");
        cmd.args(["install", "-g"]).arg(spec);
        cmd
    }
}

/// GitHub release tarball URL for a resolved version + platform target. Mirrors
/// install.sh's `url=` line so the self-owned channel pulls the same artifact
/// the installer did. The base is [`release_download_base`] so the test seam can
/// redirect it to a local fixture; unset → the canonical github.com URL.
fn tarball_url(version: &str, target: &str) -> String {
    format!("{}/v{version}/nub-{target}.tar.gz", release_download_base())
}

/// SHA-256 checksum sidecar URL for the tarball. release.yml publishes a
/// `<archive>.sha256` next to each archive; the self-owned channel fetches it
/// and verifies the download before extracting.
fn checksum_url(version: &str, target: &str) -> String {
    format!("{}.sha256", tarball_url(version, target))
}

fn run_upgrade(version: Option<&str>, dry_run: bool, _yes: bool) -> Result<i32> {
    let nub_binary = nub_core::node::spawn::current_nub_binary()?;
    let bin_str = nub_binary.to_string_lossy().into_owned();
    let channel = detect_channel(&nub_binary);
    let target = version.unwrap_or("latest");

    if dry_run {
        match &channel {
            UpgradeChannel::Npm => {
                println!("would upgrade to {target} via npm");
                println!("  command: {}", npm_upgrade_command(target));
            }
            UpgradeChannel::Homebrew => {
                println!("would upgrade to {target} via homebrew");
                println!("  command: brew upgrade nub");
            }
            UpgradeChannel::SelfOwned { install_dir } => {
                println!("would upgrade to {target} via self-owned (~/.nub)");
                if cfg!(windows) {
                    println!(
                        "  note: a real self-owned upgrade is unsupported on Windows; \
                         reinstall via `{}` instead.",
                        npm_upgrade_command("latest")
                    );
                }
                match platform_target() {
                    Some(plat) => {
                        // Resolve `latest` to a concrete tag so the printed URL is
                        // the real artifact, not a bogus `vlatest`. A dry-run is an
                        // explicit user action where one GitHub API call is fine;
                        // if it fails (offline), fall back to the literal spec and
                        // say so rather than fabricate a version.
                        let resolved = resolve_version(target);
                        let ver = match &resolved {
                            Ok(v) => v.as_str(),
                            Err(_) => target,
                        };
                        if resolved.is_err() && target == "latest" {
                            println!("  (could not resolve `latest`; showing literal)");
                        }
                        println!("  platform: {plat}");
                        println!("  tarball:  {}", tarball_url(ver, plat));
                        println!("  sha256:   {}", checksum_url(ver, plat));
                        println!("  install:  {}", install_dir.display());
                    }
                    None => println!(
                        "  (no published tarball for this platform: {}/{})",
                        std::env::consts::OS,
                        std::env::consts::ARCH
                    ),
                }
            }
            UpgradeChannel::Unknown => {
                println!("would upgrade to {target}, but the install channel is unknown");
                println!("  binary: {bin_str}");
                println!("  manual: {}", npm_upgrade_command(target));
            }
        }
        return Ok(0);
    }

    match channel {
        UpgradeChannel::Npm => {
            let cmd = npm_upgrade_command(target);
            println!("running `{cmd}`");
            let status = npm_upgrade_command_invocation(target).status()?;
            let code = nub_core::node::spawn::exit_code_from_status(&status);
            // npm wrote a NEW inode; existing shim hardlinks still carry the
            // old bytes until `nub pm shim` re-links them.
            if code == 0 {
                if let Some(msg) = shim_relink_reminder() {
                    eprintln!("{msg}");
                }
            }
            Ok(code)
        }
        UpgradeChannel::Homebrew => {
            println!("running `brew upgrade nub`");
            let status = std::process::Command::new("brew")
                .arg("upgrade")
                .arg("nub")
                .status()?;
            let code = nub_core::node::spawn::exit_code_from_status(&status);
            if code == 0 {
                if let Some(msg) = shim_relink_reminder() {
                    eprintln!("{msg}");
                }
            }
            Ok(code)
        }
        UpgradeChannel::SelfOwned { install_dir } => {
            perform_selfowned_upgrade(&install_dir, target)?;
            // nub owns the swapped-in binary's path — re-link the shims to the
            // new inode in place (the post-upgrade re-link story).
            relink_shims_after_selfowned(&install_dir);
            Ok(0)
        }
        UpgradeChannel::Unknown => {
            bail!(
                "nub upgrade: could not detect install channel.\n\
                 Binary at: {bin_str}\n\
                 Upgrade manually: {}",
                npm_upgrade_command(target)
            );
        }
    }
}

/// Resolve a `latest`/explicit version spec to a concrete `X.Y.Z` string via the
/// GitHub releases API. `latest` is the dist-tag-equivalent; an explicit version
/// passes through (callers strip a leading `v`). Network-hard: see the
/// manual-verification note on [`perform_selfowned_upgrade`].
fn resolve_version(spec: &str) -> Result<String> {
    if spec != "latest" {
        return Ok(spec.trim_start_matches('v').to_string());
    }
    let api = release_latest_api();
    // GitHub requires a User-Agent; curl supplies one. Parse the tag_name out of
    // the JSON the same way install.sh does (no full JSON parse needed for one
    // field, but serde_json is already a dep so use it for robustness).
    let out = std::process::Command::new("curl")
        .args(["--fail", "--silent", "--location", &api])
        .output()
        .context("nub upgrade: failed to invoke curl to resolve latest version")?;
    if !out.status.success() {
        bail!("nub upgrade: failed to query latest release from {api}");
    }
    let body: serde_json::Value = serde_json::from_slice(&out.stdout)
        .context("nub upgrade: could not parse GitHub releases API response")?;
    let tag = body
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow::anyhow!("nub upgrade: no tag_name in latest release response"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// The actionable message shown when a self-owned (`~/.nub`) upgrade is attempted
/// on Windows, where renaming `bin/`+`runtime/` out from under the running
/// `nub.exe` is unsafe (ERROR_SHARING_VIOLATION). Pure (takes `is_windows`
/// explicitly) so the fail-fast contract is unit-testable on any host: returns
/// `Some(msg)` to refuse, `None` to proceed. Self-owned only — npm/homebrew
/// channels delegate to a package manager and are not affected.
fn windows_selfowned_unsupported(is_windows: bool) -> Option<String> {
    if !is_windows {
        return None;
    }
    Some(format!(
        "nub upgrade: self-upgrade isn't supported on Windows yet.\n\
         A running nub.exe can't replace its own files in place, so swapping the \
         install would risk leaving it half-updated.\n\
         To update, reinstall instead:\n  \
         npm install -g {NPM_PACKAGE}@latest\n\
         or re-run the Windows installer."
    ))
}

/// Download + SHA-256-verify + atomic-swap a release tarball into a self-owned
/// `~/.nub` install. Mirrors install.sh's layout exactly: the archive contains
/// `bin/` + `runtime/`, extracted into `<install_dir>` after replacing the prior
/// `bin/`+`runtime/`.
///
/// Atomicity contract ([upgrade.md#atomicity]): on any failure post-download the
/// existing install is untouched. We stage the extraction in a sibling temp dir
/// on the same filesystem, verify the SHA-256 before extracting, then swap the
/// new `bin`/`runtime` into place via directory rename (atomic per-dir on POSIX);
/// the prior dirs move aside to `.old` first and are GC'd on success.
///
/// MANUAL-VERIFICATION NOTE: the download/verify/rename path is network- and
/// release-artifact-hard to unit-test (it needs a live GitHub release + a real
/// platform tarball). It is verified ad hoc via `nub upgrade --dry-run` (which
/// prints channel, URL, sha source, and install dir) and by running a real
/// `nub upgrade` against a published release once one exists. The pure pieces —
/// `detect_channel`, `tarball_url`, `checksum_url`, `platform_target`,
/// `sha256_hex`, and sidecar parsing — are individually exercised; the glue here
/// is kept deliberately linear and small so its correctness is reviewable by eye.
fn perform_selfowned_upgrade(install_dir: &Path, version_spec: &str) -> Result<()> {
    // Windows fail-fast: the self-owned swap renames `bin/`+`runtime/` out from
    // under the running `nub.exe`. A running executable cannot be renamed or
    // deleted while in use on Windows (ERROR_SHARING_VIOLATION), so the swap can
    // fail mid-flight and leave a half-replaced — potentially unbootable —
    // install. We do not yet implement the rename-self-to-`.old` dance that would
    // make this safe, so refuse BEFORE touching the filesystem. See
    // upgrade.md#windows. This guards the real swap only; the npm/homebrew
    // channels (which shell out to a package manager) are unaffected.
    if let Some(msg) = windows_selfowned_unsupported(cfg!(windows)) {
        bail!("{msg}");
    }
    let target = platform_target().ok_or_else(|| {
        anyhow::anyhow!(
            "nub upgrade: no published tarball for this platform ({}/{}). \
             Reinstall via the install script instead.",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let version = resolve_version(version_spec)?;
    let url = tarball_url(&version, target);
    let sha_url = checksum_url(&version, target);

    println!("upgrading to v{version} ({target})");

    // Stage downloads + extraction in a sibling temp dir on the same filesystem
    // as the install so the final swap is a same-filesystem rename (atomic).
    let staging = tempfile::Builder::new()
        .prefix(".nub-upgrade-")
        .tempdir_in(install_dir)
        .context("nub upgrade: could not create staging directory")?;
    let archive_path = staging.path().join("nub.tar.gz");

    curl_download(&url, &archive_path)
        .with_context(|| format!("nub upgrade: failed to download {url}"))?;

    let expected = fetch_expected_sha256(&sha_url)
        .with_context(|| format!("nub upgrade: failed to fetch checksum {sha_url}"))?;
    let actual = sha256_hex(&std::fs::read(&archive_path)?);
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!(
            "nub upgrade: checksum mismatch for {url}\n  expected: {expected}\n  actual:   {actual}\n\
             Refusing to install a corrupted or tampered archive."
        );
    }

    // Extract into a fresh `staged/` subdir (contains bin/ + runtime/), matching
    // install.sh's `tar -xzf … -C $install_dir`.
    let staged_root = staging.path().join("staged");
    std::fs::create_dir_all(&staged_root)?;
    let tar_status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&staged_root)
        .status()
        .context("nub upgrade: failed to invoke tar")?;
    if !tar_status.success() {
        bail!("nub upgrade: failed to extract archive {url}");
    }
    let new_bin = staged_root.join("bin");
    let new_runtime = staged_root.join("runtime");
    if !new_bin.join("nub").is_file() {
        bail!("nub upgrade: downloaded archive did not contain bin/nub");
    }

    // Swap: move old bin/runtime aside, rename new into place, then GC the old.
    // If the second rename fails we restore the first so the install is intact.
    swap_dir(install_dir, "bin", &new_bin)?;
    if let Err(e) = swap_dir(install_dir, "runtime", &new_runtime) {
        // bin already swapped; runtime failed. The new bin pairs with the new
        // runtime layout, so leave bin swapped (versions match) and surface the
        // error — a partial-but-consistent bin/ with a stale runtime/ would be
        // worse. In practice both renames are same-filesystem and don't fail
        // independently; this branch is the documented tail risk.
        bail!("nub upgrade: swapped bin/ but failed to swap runtime/: {e}");
    }

    // The release tarball ships `bin/nub` (and `bin/nubx`) at mode 0644 — the
    // upload-artifact → download-artifact round-trip in CI strips the executable
    // bit, so the published archive is non-executable. install.sh heals fresh
    // installs with its own `chmod +x`; the self-owned upgrade path must do the
    // same or every upgrade leaves `~/.nub/bin/nub` as `-rw-r--r--` and the next
    // invocation is "command not found" / a silent fall-back to a stale npm binary.
    // Set +x on the freshly-swapped-in binary before it can be invoked.
    ensure_bin_executable(&install_dir.join("bin").join("nub"))?;

    // The release tarball ships only `bin/nub`; recreate the `nubx` alias that
    // install.sh creates (relative symlink → nub; the CLI dispatches on argv[0],
    // so the alias name is what matters — see Argv0::detect). Without this, every
    // self-owned upgrade would silently drop nubx. POSIX-only: Windows self-owned
    // upgrades already bailed at the top of this fn.
    #[cfg(unix)]
    {
        let nubx = install_dir.join("bin").join("nubx");
        let _ = std::fs::remove_file(&nubx);
        std::os::unix::fs::symlink("nub", &nubx).with_context(|| {
            format!(
                "nub upgrade: failed to create nubx symlink at {}",
                nubx.display()
            )
        })?;
    }

    println!("installed v{version} to {}", install_dir.display());
    Ok(())
}

/// Atomically replace `<install_dir>/<name>` with `new_src` via rename: move any
/// existing dir to a `.old` sibling (which we then remove), rename the staged dir
/// into place. Same-filesystem, so each rename is atomic on POSIX.
fn swap_dir(install_dir: &Path, name: &str, new_src: &Path) -> Result<()> {
    let dest = install_dir.join(name);
    if dest.exists() {
        let backup = install_dir.join(format!(".{name}.old"));
        let _ = std::fs::remove_dir_all(&backup);
        std::fs::rename(&dest, &backup)
            .with_context(|| format!("could not move aside existing {}", dest.display()))?;
        std::fs::rename(new_src, &dest)
            .with_context(|| format!("could not install new {}", dest.display()))?;
        let _ = std::fs::remove_dir_all(&backup);
    } else {
        std::fs::rename(new_src, &dest)
            .with_context(|| format!("could not install {}", dest.display()))?;
    }
    Ok(())
}

/// Set the executable bit (0o755) on a freshly-installed binary. The release
/// archive ships the binary at 0o644 — CI's upload/download-artifact round-trip
/// strips the +x install.sh would otherwise rely on — so the self-owned upgrade
/// path must re-apply it or the upgraded `nub` is non-executable. No-op on
/// Windows (executability is by extension, not a mode bit).
#[cfg_attr(windows, allow(unused_variables, clippy::unnecessary_wraps))]
fn ensure_bin_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(path, perms).with_context(|| {
            format!(
                "nub upgrade: failed to set executable permissions on {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

/// Download `url` to `dest` via curl (the same tool install.sh uses — keeps Nub
/// free of a bundled HTTP/TLS stack and inherits the user's CA + proxy config).
fn curl_download(url: &str, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--output",
        ])
        .arg(dest)
        .arg(url)
        .status()
        .context("failed to invoke curl")?;
    if !status.success() {
        bail!("curl exited with failure downloading {url}");
    }
    Ok(())
}

/// Fetch the `.sha256` sidecar and parse out the hex digest. The sidecar is the
/// `shasum`/`sha256sum` format: `<hex>␠␠<filename>`; we take the first field.
fn fetch_expected_sha256(sha_url: &str) -> Result<String> {
    let out = std::process::Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location", sha_url])
        .output()
        .context("failed to invoke curl for checksum")?;
    if !out.status.success() {
        bail!("curl exited with failure fetching checksum {sha_url}");
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let hex = body
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty checksum file at {sha_url}"))?;
    Ok(hex.to_string())
}

/// Lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Print nub's version exactly like `nub --version` (and now `nubx --version`).
/// Copy node's own format: a bare `v<semver>` on stdout, so `$(nub --version)`
/// drops into anything that already parses `node --version`. The resolved Node
/// rides on STDERR — informative for a human, invisible to `$(...)` — and is
/// best-effort: discovery failure never fails `--version` (no pin resolution
/// context is required to report nub's own version). Supersedes the 2026-06-04
/// "pure --version, no node info" record (accf251); ruled by the maintainer 2026-06-11.
///
/// CRITICAL: a version query must be near-INSTANT and must NEVER spawn a Node
/// subprocess, hit the network, or provision. So the resolved-Node line uses
/// `discover_node_cached` (NOT `discover_node`): it learns the Node version only
/// for free — from the mtime-valid discovery cache for a PATH node, or from a
/// store/nvm directory name — and reports nothing when the version would cost a
/// spawn. This fixes the multi-second `nub --version` hang seen when the box's
/// `node` startup was slow: the old `discover_node` blocked on a `node --version`
/// spawn purely to print this courtesy line. The bare `v<semver>` on stdout is
/// unchanged; only the best-effort stderr line is now spawn-free, so on a cold
/// cache it may be omitted until a real run warms it.
fn print_version() {
    println!("v{}", env!("CARGO_PKG_VERSION"));
    if let Ok(cur) = env::current_dir() {
        if let Some(node) = nub_core::node::discovery::discover_node_cached(&cur) {
            let provenance = match &node.pin_source {
                Some(src) => format!("resolved from {src}"),
                None => "from PATH".to_string(),
            };
            eprintln!("» node v{} ({provenance})", node.version);
        }
    }
}

/// Native clap subcommands whose `--help` is rendered by clap directly.
const CLAP_HELP_COMMANDS: &[&str] = &[
    "run", "watch", "exec", "nubx", "upgrade", "install", "i", "ci",
];

/// True for any word `nub <word> -h` / `nub help <word>` can route to a real help
/// page: a native clap command, the `node`/`pm`/`agent` groups, or an engine verb
/// (canonical or alias). Unknown words fall through to the top-level page instead
/// of exiting silently — the routing inconsistency the help-router fix addresses.
fn is_help_routable(word: &str) -> bool {
    CLAP_HELP_COMMANDS.contains(&word)
        || matches!(word, "node" | "pm" | "agent")
        || crate::pm_engine::lookup_verb(word).is_some()
}

/// The help router. `command = None` prints the top-level page (`-h` curated,
/// `--help` verbose); a command routes to its own help, consistently across the
/// `nub <cmd> -h`, `nub help <cmd>`, and leaf forms. Engine verbs dispatch their
/// real `--help` through the embedded engine; `node`/`pm`/`agent` use their
/// bespoke usage; native verbs render clap's help.
fn run_help(command: Option<&str>, verbose: bool) {
    let Some(cmd) = command else {
        print!(
            "{}",
            if verbose {
                render_verbose_help()
            } else {
                render_curated_help()
            }
        );
        return;
    };

    // `node` / `pm` / `agent`: bespoke usage (their own help guards print the
    // verb listing). Route through the same entry points the live commands use so
    // `nub help node` and `nub node --help` agree.
    match cmd {
        "node" => {
            let _ = run_node(&["--help".to_string()]);
            return;
        }
        "pm" => {
            let _ = run_pm(&["--help".to_string()]);
            return;
        }
        "agent" => {
            let _ = crate::agent::run(&["--help".to_string()]);
            return;
        }
        _ => {}
    }

    // Engine verbs (`add`/`remove`/`why`/…): dispatch the verb's own `--help` so
    // `nub help add` matches `nub add --help` exactly (both go through the family
    // module's help rendering). Previously `nub help add` exited silently.
    if let Some(spec) = crate::pm_engine::lookup_verb(cmd) {
        let pm = env::current_dir()
            .ok()
            .map(|d| suggest_package_manager(&d))
            .unwrap_or_else(|| "npm".to_string());
        let _ = crate::pm_engine::dispatch_verb(spec, cmd, &["--help".to_string()], &pm);
        return;
    }

    // Native clap commands (and any other word): clap renders the help. For a word
    // clap doesn't recognize this still falls back to the top-level help via the
    // `is_help_routable` gate at the call sites, so this only sees real commands.
    let result = Cli::try_parse_from(["nub", cmd, "--help"]);
    if let Err(e) = result {
        let _ = e.print();
    }
}

/// Bold a header for the help pages when stdout is a TTY (or FORCE_COLOR is set)
/// and NO_COLOR is unset. Plain text otherwise — same predicate family as the
/// stream-prefix coloring, so piped/redirected help stays clean.
fn help_bold(s: &str) -> String {
    let color = (std::io::IsTerminal::is_terminal(&std::io::stdout())
        || std::env::var_os("FORCE_COLOR").is_some())
        && std::env::var_os("NO_COLOR").is_none();
    if color {
        format!("\x1b[1m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// `nub -h` — the curated, human-readable page. It starts with Nub's headline
/// runtime/run surfaces, then mirrors pnpm's grouped package-manager help with
/// the full recognized PM surface. The footer points at `nub --help` for the
/// expanded Node flag and environment-variable reference.
fn render_curated_help() -> String {
    let v = env!("CARGO_PKG_VERSION");
    format!(
        "\
nub {v} — the all-in-one Node.js toolkit

{usage}
  nub <file> [args...]        run a file (.js/.ts/.jsx/.tsx), TypeScript just works
  nub --node <file> [args]    run on the project's pinned Node, vanilla (no augmentation)
  nub <command> [args...]     run a command (below)
  nub help <command>          show help for a command

{headline}
  <file> [args...]            run a JavaScript or TypeScript file
  run <script>                run a package.json script
  watch <file>                run a file and restart on changes
  nubx <pkg>                  fetch and run a package binary

{runtime}
  -                           read script from stdin
  --                          end of nub options; the rest is the script's argv
  -e, --eval <code>           evaluate <code>
  -p, --print <code>          evaluate <code> and print the result
  -c, --check                 syntax-check the file without running it
  -r, --require <m>           preload a CommonJS module before the script
  --import <m>                preload an ES module before the script
  --node                      run on plain Node (no augmentation)
  -v, --version               print the nub version

{pm}
  Manage dependencies:
    add, a                    add dependencies (-D dev, -O optional, -g global)
    install, i                install from package.json + lockfile
    ci                        clean, strict install from the lockfile
    import                    generate a lockfile from another PM's lockfile
    link, ln / unlink         link or unlink local packages
    remove, rm                remove dependencies
    update, up                update dependencies within range
    dedupe                    remove duplicated packages
    prune                     remove extraneous packages
    rebuild, rb               rebuild native modules
    fetch                     fetch packages into the store
    patch / patch-commit / patch-remove   author package patches
    approve-builds / ignored-builds       manage build-script approval

  Review dependencies:
    list, ls / la / ll        list installed dependencies
    why, w                    explain why a package is installed
    outdated                  list dependencies with newer versions available
    audit                     check installed packages for known vulnerabilities
    licenses                  list dependency licenses
    deprecations / peers      inspect registry deprecations or peer issues
    view, info, show, v       show registry metadata for a package
    search                    search the registry through npm fallback
    query / check / bin / root / sbom

  Run scripts and bins:
    run <script>              run a package.json script
    exec <bin>                run a node_modules/.bin binary
    dlx / create              fetch and run a package or create template

  Publish and registry:
    publish / pack / version  publish, pack, or bump the package version
    dist-tag / deprecate / undeprecate / unpublish
    login / logout / whoami / owner / token / stage

  Store and config:
    store / cache             inspect and maintain the content-addressable store
    cat-file / cat-index / find-hash
    config, c / get / set / pkg / set-script

{toolchain}
  node                        manage Node versions (install / ls / uninstall / pin)
  pm                          manage the project's package manager (which / use / shim)
  upgrade                     upgrade nub itself

{footer}
",
        v = v,
        usage = help_bold("Usage:"),
        headline = help_bold("Headline commands:"),
        runtime = help_bold("Runtime options (passed through to Node):"),
        pm = help_bold("Package manager commands:"),
        toolchain = help_bold("Manage the toolchain:"),
        footer = help_bold(
            "See `nub --help` for the expanded Node flag + environment-variable reference."
        )
    )
}

/// `nub --help` — the verbose reference: nub's command surface plus a fuller
/// Node runtime flag and environment-variable reference. The power-user / agent
/// form. For the exact, version-correct list of the project's resolved Node, the
/// footer points at `nub --node --help` (which passes through to that Node).
fn render_verbose_help() -> String {
    let v = env!("CARGO_PKG_VERSION");
    format!(
        "\
nub {v} — the all-in-one Node.js toolkit

{usage}
  nub [options] <file> [args...]
  nub [options] --node <file> [args...]
  nub [options] -e <code>  |  -p <code>  |  - (stdin)
  nub <command> [args...]
  nub help <command>

{commands}
  Run code:
    run <script>             run a package.json script (workspace-aware)
    exec <bin> / nubx <bin>  run a node_modules/.bin binary
    nubx <pkg> / dlx <pkg>   fetch-and-run a package's bin
    watch <file>             run a file in watch mode

  Manage dependencies:
    install, i               install from package.json + lockfile
    ci                       clean, strict install from the lockfile
    add, a                   add dependencies
    remove, rm               remove dependencies
    update, up               update within range
    import                   generate a lockfile from another PM's lockfile
    dedupe                   remove duplicated packages
    prune                    remove extraneous packages
    rebuild, rb              rebuild native modules
    link, ln / unlink        link / unlink a local package
    patch / patch-commit / patch-remove   author a package patch
    approve-builds / ignored-builds       manage build-script approval

  Inspect dependencies:
    list, ls / why / outdated / audit / licenses
    view / search / bin / root / query / check / sbom

  Publish and registry:
    publish / pack / version / dist-tag
    login / logout / whoami / owner / token

  Manage the toolchain:
    node                     manage Node versions (install / ls / uninstall / pin)
    pm                       manage the project's package manager
    upgrade                  upgrade nub itself

  Store and config:
    store / cache            manage the content-addressable store
    config / get / set       manage configuration

{nubopts}
  --cwd <dir>          run as if started in <dir>
  -s, --silent         suppress nub's non-error output
  --verbose            increase nub's log verbosity (repeatable)
  --color[=<when>]     color mode: auto (default), always, never
  --env-file <file>    load environment variables from <file>
  --node               run on plain Node, no augmentation (the compat escape hatch)
  -v, --version        print the nub version
  -h, --help           print help (`-h` curated, `--help` this full reference)

{noderun}
  -                              script read from stdin
  --                             indicate the end of node options
  -e, --eval <code>              evaluate <code>
  -p, --print <code>             evaluate <code> and print the result
  -c, --check                    syntax-check the script without executing it
  -r, --require <module>         preload a CommonJS module
  --import <module>              preload an ES module
  -C, --conditions <name>        additional conditional export/import conditions
  --input-type <type>            'module' or 'commonjs' for --eval / stdin input
  --watch                        run in watch mode
  --watch-path <path>            path to watch (repeatable)
  --watch-preserve-output        preserve output across watch restarts
  --env-file <file>              load environment variables from a file
  --enable-source-maps           enable source-map support for stack traces
  --inspect[=[host:]port]        activate the inspector
  --inspect-brk[=[host:]port]    activate the inspector and break at start
  --inspect-wait[=[host:]port]   activate the inspector and wait until attached
  --cpu-prof / --heap-prof       write a V8 CPU / heap profile on exit
  --prof                         generate V8 profiler tick data
  --title <title>                set process.title on startup
  --max-old-space-size <mb>      set V8's old-space size limit
  --stack-trace-limit <n>        set Error.stackTraceLimit
  --no-warnings                  silence all process warnings
  --disable-warning <code|type>  silence a specific warning
  --throw-deprecation            throw on use of deprecated APIs
  --pending-deprecation          emit pending deprecation warnings
  --no-deprecation               silence deprecation warnings
  --redirect-warnings <file>     write warnings to a file instead of stderr
  --trace-warnings               show stack traces for process warnings
  --trace-deprecation            show stack traces for deprecations
  --trace-exit / --trace-uncaught / --trace-sync-io
  --report-on-fatalerror / --report-uncaught-exception / --report-on-signal
  --report-dir <dir> / --report-filename <file>
  --preserve-symlinks / --preserve-symlinks-main
  --use-largepages <mode> / --zero-fill-buffers
  --v8-options                   print V8 command-line options
  --                             (and every other `node` flag — passthrough)

{nodeenv}
  NODE_OPTIONS               space-separated list of node CLI options applied at startup
  NODE_ENV                   the environment ('production' / 'development' / …)
  NODE_PATH                  ':'-separated directories prepended to the module search path
  NODE_EXTRA_CA_CERTS        path to additional CA certificates, read once at startup
  NODE_TLS_REJECT_UNAUTHORIZED  set to 0 to disable TLS certificate validation
  NODE_NO_WARNINGS           set to 1 to silence process warnings
  NODE_PENDING_DEPRECATION   set to 1 to emit pending deprecation warnings
  NODE_PRESERVE_SYMLINKS     set to 1 to preserve symlinks when resolving modules
  NODE_REDIRECT_WARNINGS     write warnings to the given path instead of stderr
  NODE_V8_COVERAGE           directory to write V8 coverage JSON to
  NODE_DEBUG                 ','-separated core modules that should print debug output
  NODE_COMPILE_CACHE         directory for the on-disk module compile cache
  UV_THREADPOOL_SIZE         number of threads in libuv's thread pool
  FORCE_COLOR / NO_COLOR     force / disable colored output
  TZ                         the timezone configuration

{footer1}
{footer2}
",
        v = v,
        usage = help_bold("Usage:"),
        commands = help_bold("Commands:"),
        nubopts = help_bold("Nub options:"),
        noderun = help_bold("Common Node runtime flags (passed through to Node):"),
        nodeenv = help_bold("Common environment variables:"),
        footer1 = help_bold(
            "For the exact flag set of this project's pinned Node, run `nub --node --help`."
        ),
        footer2 = "Documentation: https://nubjs.com/docs"
    )
}

/// Discover the project's Node for the read-only status paths (`nub node` /
/// `nub node which`), with the `PinnedNotFound` remedy rewritten to nub's model.
///
/// The nub-core `DiscoveryError::PinnedNotFound` text is now nub-correct at the
/// source (it points at `nub node install`, not nvm/compat mode). This remap adds
/// the status-specific guidance that the root error doesn't carry — *which fields*
/// establish a pin — since the read-only status paths don't auto-provision and the
/// user is most likely here to debug where the pin came from.
fn discover_node_for_status(cwd: &Path) -> Result<nub_core::node::discovery::ResolvedNode> {
    use nub_core::node::discovery::DiscoveryError;
    nub_core::node::discovery::discover_node(cwd).map_err(|e| match e {
        DiscoveryError::PinnedNotFound { pin, shell_version } => anyhow::anyhow!(
            "pinned Node version {pin} not found\n\
             \x20\x20Active shell Node: {shell_version} (does not satisfy the pin)\n\
             \x20\x20Provision it: nub node install {pin} (or run a file — nub installs the pin on demand)\n\
             \x20\x20The pin comes from .node-version / .nvmrc / engines.node / devEngines.runtime."
        ),
        other => anyhow::Error::new(other),
    })
}

/// `nub node …` — the version-management command group (install / ls / uninstall
/// / pin). Non-forwarding; manual sub-verb match so the bare-usage and the
/// `nub node <file>` error read exactly as the spec specifies.
/// Spec: `wiki/commands/node-versions.md`.
fn run_node(args: &[String]) -> Result<i32> {
    let cwd = env::current_dir()?;
    let store = nub_core::node::discovery::node_store_dir().ok_or_else(|| {
        anyhow::anyhow!("could not locate nub's cache directory (no $HOME / $XDG_CACHE_HOME)")
    })?;

    // The verb listing — shared by `--help`/`-h`/`help` and the bare form, so the
    // two never drift. Bare `nub node` prepends the resolved-Node status block.
    const NODE_HELP: &str = "nub node — manage Node versions\n\n\
         Usage: nub node <command>\n\n\
         Commands:\n\
         \x20 which                    print the resolved Node binary path (why → stderr)\n\
         \x20 install [<version>...]   provision version(s) into nub's cache (bare: the project pin)\n\
         \x20 ls                       list versions in nub's cache\n\
         \x20 uninstall <version>      remove a version from nub's cache\n\
         \x20 pin <version>            write the project's Node pin";

    // `nub node --help`/`-h`/`help`: short usage listing the verbs.
    let verb = args.first().map(String::as_str);
    if matches!(verb, Some("--help") | Some("-h") | Some("help")) {
        println!("{NODE_HELP}");
        return Ok(0);
    }

    // Bare `nub node`: the idiomatic command-group behavior — print the resolved
    // Node status block first, then the verb listing. Discovery is best-effort:
    // if no Node resolves, still print the help rather than erroring out.
    if verb.is_none() {
        if let Ok(node) = discover_node_for_status(&cwd) {
            println!("node {}", node.version);
            println!("  path      {}", node.path);
            println!("  resolved  {}", resolution_source(&cwd));
            println!();
        }
        println!("{NODE_HELP}");
        return Ok(0);
    }

    // Past the guard above, a verb is present.
    match verb.expect("verb present after the help/bare guard") {
        "which" => {
            // Path → stdout, so `NODE=$(nub node which)` captures just the path.
            // Resolution explainer → stderr (diagnostics), suppressible with
            // `2>/dev/null`. Path is written (and flushed) first so an interactive
            // run shows it above the explainer.
            let node = discover_node_for_status(&cwd)?;
            println!("{}", node.path);
            use std::io::Write as _;
            std::io::stdout().flush().ok();
            eprintln!("» resolved from {}", resolution_source(&cwd));
            Ok(0)
        }
        "install" => {
            use nub_core::version_management::manage::{self, InstallOutcome};
            let specs = &args[1..];
            let report = |outcome: &InstallOutcome| match outcome {
                InstallOutcome::AlreadyCached(v) => {
                    eprintln!("Node {v} is already in nub's cache.");
                }
                InstallOutcome::AlreadyOnPath(v) => {
                    eprintln!(
                        "Node {v} is already available on PATH — skipped (already installed)."
                    );
                }
                InstallOutcome::Installed(_) => { /* provision_node already printed the ✓ line */
                }
            };
            if specs.is_empty() {
                let outcome = manage::install_from_pin(&store, &cwd)?;
                report(&outcome);
            } else {
                for spec in specs {
                    let outcome = manage::install_one(spec, &store, &cwd)?;
                    report(&outcome);
                }
            }
            Ok(0)
        }
        "ls" => {
            use nub_core::version_management::manage;
            let entries = manage::ls(&store, &cwd);
            if entries.is_empty() {
                eprintln!(
                    "No Node versions in nub's cache. Install one with `nub node install <version>`."
                );
                return Ok(0);
            }
            for e in &entries {
                let mark = if e.active { "→ " } else { "  " };
                println!("{mark}{}", e.version);
            }
            Ok(0)
        }
        "uninstall" => {
            use nub_core::version_management::manage;
            let Some(version) = args.get(1) else {
                bail!("nub node uninstall requires a version (e.g. nub node uninstall 22.13.0)");
            };
            let removed = manage::uninstall(version, &store, &cwd)?;
            eprintln!("Removed Node {removed} from nub's cache.");
            Ok(0)
        }
        "pin" => {
            use nub_core::version_management::manage;
            let Some(version) = args.get(1) else {
                bail!("nub node pin requires a version (e.g. nub node pin 22)");
            };
            let result = manage::pin(version, &cwd)?;
            println!("pinned Node {} → {}", result.spec, result.path.display());
            Ok(0)
        }
        // `nub node <file>` (or any non-verb positional) is an error, NOT a
        // passthrough — the exact wording is locked by the spec
        // (node-versions.md line 25). The literal `<file>` placeholder is part of
        // the locked string; do NOT interpolate the typed token (it would both
        // drop trailing args and diverge from the spec).
        _ => {
            bail!(
                "nub node takes a subcommand (which, install, ls, uninstall, pin). \
                 To run a file, use 'nub <file>'."
            );
        }
    }
}

/// nub's PM store root — `<cache_dir>/pm/…`, a sibling of the Node store
/// (`<cache_dir>/node/…`). `provision_pm` takes the cache *root* (it appends
/// `pm/<pm>/<version>` itself), so this returns the root, not the `pm` subdir.
fn pm_store_root() -> Result<PathBuf> {
    nub_core::node::discovery::cache_dir().ok_or_else(|| {
        anyhow::anyhow!("could not locate nub's cache directory (no $HOME / $XDG_CACHE_HOME)")
    })
}

/// The coded "no package.json" error for the script-run paths (`nub run`), so a
/// missing manifest reads with the same `ERR_NUB_*` framing the install path
/// surfaces for the same root cause — not a bare `Error: no package.json found`.
/// `nub run` only consults `package.json#scripts`, so this names the manifest
/// (not the workspace-yaml the install path also accepts).
fn no_manifest_error(cwd: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "{ERR_NUB_NO_MANIFEST}: no package.json found in {} or any parent directory",
        cwd.display()
    )
}

/// Preflight a project's `package.json` for parseability. Resolution treats an
/// unparseable manifest as "no PM pinned" (every read swallows the parse error
/// into `None` — `detect_project` / `root_manifest`), which misdiagnoses a typo'd
/// brace as an unpinned project. This walks up from `cwd` to the nearest
/// `package.json` and, if it exists but doesn't parse, errors with the file path
/// and serde's reason (line/column) instead. A missing manifest is not this
/// function's concern — that genuinely IS unpinned, and the caller's own context
/// covers it.
fn check_manifest_json(cwd: &Path) -> Result<()> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let pkg = d.join("package.json");
        if pkg.is_file() {
            let content = match std::fs::read_to_string(&pkg) {
                Ok(content) => content,
                // A package.json that EXISTS but can't be read (most commonly
                // EACCES — wrong owner/mode in CI or a root-owned tree) otherwise
                // gets swallowed into "no package.json found" by every Option-
                // returning reader downstream (`detect_project`), misdiagnosing a
                // permission problem as an unconfigured project. Surface it with
                // nub's stable code and the actionable OS reason instead.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    bail!(
                        "{ERR_NUB_MANIFEST_UNREADABLE}: cannot read {} ({e})\n\
                         \x20\x20Check the file's permissions/ownership so nub can read it.",
                        pkg.display()
                    );
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("reading {}", pkg.display()));
                }
            };
            if let Err(e) = serde_json::from_str::<serde_json::Value>(&content) {
                bail!(
                    "{ERR_NUB_MANIFEST_PARSE}: package.json is not valid JSON ({}): {e}",
                    pkg.display()
                );
            }
            return Ok(());
        }
        dir = d.parent();
    }
    Ok(())
}

/// Collapse a registry transport failure (offline, DNS down, connection refused,
/// TLS handshake) into ONE human sentence naming the registry. `provision_pm` and
/// the registry resolver surface these as a deep anyhow chain — `failed to
/// provision … : fetching packument <url>: GET <url>: error sending request …:
/// … dns error: failed to lookup address …` — five levels of reqwest/hyper/DNS
/// internals that bury the actionable fact (the network is unreachable). When the
/// chain has that shape we replace it; the full chain stays available under
/// `--verbose` (the `SHOW_WARNINGS` flag, set by `--verbose`/`--show-warnings`).
/// A NON-transport error (a 404 for a bad version, a checksum mismatch) is passed
/// through untouched — it's already actionable and specific.
fn humanize_transport_error(err: anyhow::Error, registry: &str) -> anyhow::Error {
    // Walk the cause chain looking for the transport signature. reqwest stamps
    // connect/DNS/timeout faults with these phrases; matching on the rendered
    // chain keeps us off reqwest's private error types (it's a transitive dep of
    // nub-core, not a direct dep here).
    let rendered = format!("{err:#}").to_lowercase();
    const TRANSPORT_NEEDLES: &[&str] = &[
        "dns error",
        "failed to lookup address",
        "error sending request",
        "connection refused",
        "connection reset",
        "tcp connect error",
        "timed out",
        "network is unreachable",
        "could not connect",
    ];
    let is_transport = TRANSPORT_NEEDLES.iter().any(|n| rendered.contains(n));
    if !is_transport {
        return err;
    }
    let one_liner = anyhow::anyhow!(
        "cannot reach the registry {registry} — check your connection, or set a mirror \
         (e.g. `npm config set registry <url>`)"
    );
    if SHOW_WARNINGS.load(Ordering::Relaxed) {
        // Keep the underlying chain attached so `--verbose` users can still see it.
        one_liner.context(format!("transport detail: {err:#}"))
    } else {
        one_liner
    }
}

/// `provision_pm` with the transport-failure shape humanized (item: offline UX).
/// Used on the read-only `nub pm which` path, where a provision is a side effect
/// of resolving the path, not an explicit online action.
fn provision_pm_humanized(
    pin: &nub_core::pm::resolve::PmPin,
    store: &Path,
    cwd: &Path,
    resolved_from: Option<&str>,
) -> Result<nub_core::pm::provision::ProvisionedPm> {
    nub_core::pm::provision::provision_pm(pin, store, cwd, resolved_from)
        .map_err(|e| humanize_transport_error(e, &nub_core::pm::registry::registry_base(cwd)))
}

/// `nub pm <verb>` — the package-manager management group. Manual sub-verb match
/// (mirroring [`run_node`]'s shape): bare / `help` list the verbs, an unknown
/// token errors naming the set. The verbs operate on the project's PM *identity*
/// (`which`/`use`/`update`) and nub's PM cache (`cache`); none mutate
/// `package.json` implicitly — only the explicit declaration-writing verbs
/// (`use` / `update`) write, both through the shared resolve → provision →
/// write-the-declaration flow ([`resolve_provision_declare`]). Eager
/// auto-pinning is deliberately NOT wired anywhere: explicit `use`/`update` IS
/// the v0 policy (wiki/commands/pm/identity-policy.md, Axiom 3).
fn run_pm(args: &[String]) -> Result<i32> {
    use nub_core::pm::Pm;
    use nub_core::pm::resolve::{self, PmTarget};
    use std::io::Write as _;

    let cwd = env::current_dir()?;

    let verb = args.first().map(String::as_str);
    if matches!(verb, None | Some("help") | Some("--help") | Some("-h")) {
        println!(
            "nub pm — manage the project's package manager\n\n\
             Usage: nub pm <command>\n\n\
             Commands:\n\
             \x20 which              print the resolved package-manager path (why → stderr)\n\
             \x20 use <pm>[@<spec>]  declare the project's package manager (npm|pnpm|yarn|bun|nub;\n\
             \x20                    default: latest) — writes packageManager and aligns the lockfile;\n\
             \x20                    `use nub` migrates the full config surface, `use pnpm` reverses it\n\
             \x20 update             re-resolve within the pinned range and bump the pin (alias: up)\n\
             \x20 cache [clear]      list cached package managers (or clear the cache)\n\
             \x20 shim               link npm/pnpm/yarn shims into ~/.nub/shims (re-run after `nub upgrade`)\n\
             \x20 unshim             remove the shims and their PATH block"
        );
        return Ok(0);
    }

    match verb.expect("verb present after the help/bare guard") {
        // Path → stdout (so `PM=$(nub pm which)` captures just the path); the
        // provenance explainer → stderr. Byte-for-byte the `nub node which` shape.
        "which" => {
            // A malformed package.json resolves as "no PM pinned" otherwise — a
            // misleading diagnosis. Surface the parse failure (and its location)
            // before resolution silently swallows it.
            check_manifest_json(&cwd)?;
            // A `nub@` self-pin (`pm use nub`) isn't a provisionable target, so
            // `resolve_target_with_source` returns None — reporting "no PM
            // pinned" there is wrong. nub IS the manager: name it and stop.
            if resolve::project_pm_identity(&cwd).is_some_and(|id| id.name == "nub") {
                let exe = nub_core::node::spawn::current_nub_binary()
                    .unwrap_or_else(|_| PathBuf::from("nub"));
                println!("{}", exe.display());
                std::io::stdout().flush().ok();
                eprintln!("» this project uses nub (resolved from packageManager)");
                return Ok(0);
            }
            // A project may DECLARE a manager nub doesn't provision (bun — and
            // any name outside nub's provisioning scope). The install path
            // honors the declared identity (`declared_pm_raw`); `which` must
            // agree rather than mislabel a genuinely-pinned project as
            // "unpinned". `resolve_target_with_source` returns None for such a
            // pin (it only resolves provisionable targets), so consult the
            // name-level identity probe before falling through to the
            // no-pin diagnosis. Berry (committed yarnPath / pinned yarn) and
            // every provisionable PM still resolve below — this branch fires
            // only for a declared-but-unprovisionable identity.
            if let Some(id) = resolve::project_pm_identity(&cwd) {
                if resolve::resolve_target_with_source(&cwd).is_none() && !id.berry {
                    let (_, version) =
                        resolve::declared_pm_raw(&cwd).unwrap_or_else(|| (id.name.clone(), None));
                    println!("{}", id.name);
                    std::io::stdout().flush().ok();
                    let pin = match &version {
                        Some(v) => format!("{}@{v}", id.name),
                        None => id.name.clone(),
                    };
                    eprintln!(
                        "» this project is pinned to {pin} (resolved from packageManager) — \
                         nub doesn't provision {}, run it with its own installer",
                        id.name
                    );
                    return Ok(0);
                }
            }
            let res = resolve::resolve_target_with_source(&cwd)
                .context("no package manager is pinned (no .yarnrc.yml yarnPath, packageManager, or devEngines.packageManager) — declare one with `nub pm use <pm>`")?;
            // Drain the structured advisories first (disagreement / range /
            // ignored-field) so they precede the path on stderr.
            for w in &res.warnings {
                eprintln!("{w}");
            }
            let (path, provenance) = match res.target {
                PmTarget::YarnPath(release) => (
                    release,
                    format!(
                        "resolved from {}",
                        res.source.expect("YarnPath carries a source")
                    ),
                ),
                PmTarget::Provision(pin) => {
                    let store = pm_store_root()?;
                    let source = res.source.expect("Provision carries a source");
                    let prov =
                        provision_pm_humanized(&pin, &store, &cwd, Some(&source.to_string()))?;
                    let provenance =
                        format!("resolved from {source} ({}@{})", pin.pm, prov.version);
                    (prov.bin, provenance)
                }
                PmTarget::BerryNoYarnPath => bail!(berry_no_yarn_path_msg()),
            };
            println!("{}", path.display());
            std::io::stdout().flush().ok();
            eprintln!("» {provenance}");
            Ok(0)
        }
        // `nub pm use <pm>[@<spec>]` — THE identity-setting verb (spec:
        // wiki/commands/pm/identity-policy.md §`nub pm use`). Declarative
        // contract: after it runs, the project's identity is <pm> and the
        // artifacts agree — `packageManager` written (the field's only
        // sanctioned writer), `devEngines.packageManager` maintained beside
        // it ({name, ^range, onFail:warn} — the 2026-06-10 ruling that
        // killed never-create), and the lockfile aligned (kept / converted / strays
        // removed) through the engine's gated writers. Idempotent: rerunning
        // is a no-op (a bare spec refreshes the pin to latest). Replaces the
        // old `pin` (version-only) and `switch` (cross-PM, declaration-only)
        // verbs — one command owns identity.
        "use" => {
            let Some(arg) = args.get(1) else {
                bail!(
                    "nub pm use requires a package manager — nub pm use <pm>[@<spec>] \
                     (e.g. nub pm use pnpm, nub pm use npm@10, nub pm use pnpm@latest)"
                );
            };
            let (name, spec) = split_pm_arg(arg)?;
            run_pm_use(name, spec.unwrap_or("latest"), &cwd)
        }
        // Re-resolve WITHIN THE PINNED INTENT and bump the pin: the
        // devEngines.packageManager range when one is present (so `^9.1.0`
        // floats inside 9.x, never silently across majors), else the registry
        // latest. Always rewrites `packageManager` — the hash is recomputed from
        // the freshly fetched artifact, and a legacy hashless pin gets upgraded
        // to the exact+hash shape even when the version is already newest. The
        // devEngines half is rewritten only when it carries nub's own ^<exact>
        // shape; a hand-written range is the user's intent and stays verbatim.
        "update" | "up" => {
            check_manifest_json(&cwd)?;
            let res = resolve::resolve_pin_with_source(&cwd).context(
                "no package manager is pinned to update — declare one with `nub pm use <pm>`",
            )?;
            for w in &res.warnings {
                eprintln!("{w}");
            }
            let pin = res.pin;
            if pin.pm == Pm::YarnBerry {
                bail!(
                    "the pinned yarn is Berry (yarn 2+) — nub can't provision or update Berry \
                     releases. Use `yarn set version <v>` (it manages the committed release), \
                     or pin classic yarn@1."
                );
            }
            let name = pin.pm.to_string();
            let range = dev_engines_range(&cwd, &name);
            let spec = range.clone().unwrap_or_else(|| "latest".to_string());
            // The pair semantics: devEngines = intent, packageManager = resolved
            // record. A nub-shaped range (^x.y.z — what pin/update themselves
            // write) is re-derived from the new exact; a hand-written one
            // (">=9 <10", "~9.2") survives untouched. The update just resolved
            // WITHIN that range, so the new exact satisfies it by construction.
            let keep_dev_engines = range
                .as_deref()
                .is_some_and(|r| !resolve::nub_shaped_range(r));
            let current = pin
                .version
                .as_deref()
                .map(|v| v.split_once('+').map_or(v, |(bare, _)| bare).to_string());
            // A nub-shaped devEngines range moves with the pin (same writer as
            // `use`, onFail:"warn"); a hand-written one is the user's stated
            // constraint update floats WITHIN — never rewritten.
            let (version, _) = resolve_provision_declare(&name, &spec, &cwd, !keep_dev_engines)?;
            match current {
                Some(cur) if cur == version => eprintln!(
                    "{name} is already on the newest version ({version}); pin hash refreshed."
                ),
                Some(cur) => eprintln!("updated {name} {cur} → {version}"),
                None => eprintln!("updated {name} → {version}"),
            }
            Ok(0)
        }
        // List the cached package managers (`<pm>@<version>` per line), or clear
        // the cache. `clear` is positional (no flag struct) — `nub pm cache clear`.
        "cache" => {
            let pm_cache = pm_store_root()?.join("pm");
            if args.get(1).map(String::as_str) == Some("clear") {
                if pm_cache.is_dir() {
                    std::fs::remove_dir_all(&pm_cache)
                        .with_context(|| format!("clearing {}", pm_cache.display()))?;
                }
                eprintln!(
                    "cleared nub's package-manager cache ({}).",
                    pm_cache.display()
                );
                return Ok(0);
            }
            let entries = list_pm_cache(&pm_cache);
            if entries.is_empty() {
                eprintln!("No package managers in nub's cache.");
            } else {
                for entry in entries {
                    println!("{entry}");
                }
            }
            Ok(0)
        }
        // Install / remove the PM shims (spec: wiki/research/package-manager-shims.md).
        "shim" => run_pm_shim_install(),
        "unshim" => run_pm_unshim(),
        // `pin`/`switch` were replaced by `use` (2026-06-10, identity-policy
        // ratification) — name the successor instead of the generic unknown.
        "pin" | "switch" => bail!(
            "`nub pm {}` was replaced by `nub pm use <pm>[@<spec>]` — one verb declares \
             the package manager and aligns the lockfile.",
            verb.expect("verb present in this arm")
        ),
        _ => {
            bail!("nub pm takes a subcommand (which, use, update (up), cache, shim, unshim).")
        }
    }
}

/// The shared "a bare Berry pin can't be provisioned" error: nub can't synthesize
/// a Yarn Berry release, so the project must commit one (`.yarn/releases/*.cjs` +
/// a `yarnPath:` in `.yarnrc.yml`) or pin classic `yarn@1`.
fn berry_no_yarn_path_msg() -> String {
    "yarn 2+ (Berry) requires a committed release — nub can't provision it. \
     Commit a release (\".yarn/releases/yarn-<v>.cjs\" + \"yarnPath:\" in .yarnrc.yml), \
     or pin classic yarn@1."
        .to_string()
}

/// The Berry refusal for `use`/`update`, aware of whether a `yarnPath` release is
/// ALREADY committed. Without one, the standard message applies (commit a release
/// or pin classic). With one, that message would instruct the user to do what
/// they already did — instead, point at `yarn set version`, the tool that
/// actually manages the committed release nub defers to. The refusal itself
/// stands in both cases: nub doesn't provision Berry, so it can't compute an
/// honest `+sha512` for the pin (wiki/research/package-manager-provisioning.md
/// §What pin writes).
fn berry_pin_refusal(cwd: &Path) -> String {
    match nub_core::pm::resolve::committed_yarn_path(cwd) {
        Some(release) => format!(
            "this project runs yarn Berry from its committed release ({}) — nub doesn't \
             provision Berry, so it can't pin a Berry version. Use `yarn set version <v>` \
             (it updates the committed release and the packageManager field).",
            release.display()
        ),
        None => berry_no_yarn_path_msg(),
    }
}

/// Split a `<pm>[@<spec>]` argument (`nub pm use`). The name must be a `use`
/// target (npm | pnpm | yarn | bun | nub — bun is declaration+lockfile only,
/// no provisioning); the spec stays RAW — exact, range, or dist-tag — and is
/// resolved against the registry before anything is written (never a range
/// into `packageManager`). Berry (`yarn@<2+>`) is refused later, by the shared
/// flow, once a concrete major is known. `use nub` (the full switch into nub
/// identity, 2026-06-10 reversal) takes no version spec: it pins the running
/// nub's own version — there is nothing to resolve.
fn split_pm_arg(arg: &str) -> Result<(&str, Option<&str>)> {
    let (name, spec) = match arg.split_once('@') {
        Some((n, s)) => (n, Some(s.trim())),
        None => (arg, None),
    };
    if name == "nub" && spec.is_some() {
        bail!(
            "`nub pm use nub` takes no version — it pins the running nub ({}); \
             update nub itself with `nub upgrade`.",
            env!("CARGO_PKG_VERSION")
        );
    }
    if !matches!(name, "npm" | "pnpm" | "yarn" | "bun" | "nub") {
        bail!(
            "unsupported package manager \"{name}\" — nub pm use takes npm, pnpm, yarn, bun, or nub"
        );
    }
    if spec.is_some_and(str::is_empty) {
        bail!("\"{arg}\" has an empty version spec — use <pm>@<spec> (e.g. {name}@latest)");
    }
    Ok((name, spec))
}

/// The shared resolve → provision → write-the-declaration body of `use` /
/// `update` (the ratified pin flow, 2026-06-09, re-ratified under the identity
/// policy 2026-06-10 — wiki/commands/pm/identity-policy.md §`nub pm use`):
///
///   1. resolve the raw spec (exact / range / dist-tag) against the registry to
///      a concrete version — never a range into `packageManager`;
///   2. fetch the resolved tarball ONCE, verify it against the registry dist
///      integrity, and sha512 the verified bytes (pin-implies-fetch: the
///      committed hash is computed from the artifact, never copied out of
///      registry metadata, so the pin is a registry-independent trust anchor);
///   3. provision the exact version into nub's store FROM THAT SAME verified
///      tarball — no second download (a warm store is a silent cache hit that
///      extracts nothing). Skipped for bun: nub declares it but doesn't provision
///      or run it (out of scope for v0.x);
///   4. write the declaration via [`nub_core::pm::resolve::write_declared_pm`]
///      — `packageManager: <name>@<exact>+sha512.<hex>` plus, when
///      `maintain_dev_engines`, `devEngines.packageManager {name, ^range,
///      onFail:"warn"}`. `use` always maintains the pair; `update` passes
///      false on a hand-written devEngines range (the user's stated intent —
///      only the resolved record advances) and true on nub's own ^<exact>
///      shape, which moves with the pin.
///
/// yarn >= 2 (Berry) refuses before anything is written — berry isn't the npm
/// `yarn` tarball, so a pin nub can't provision would be a lie. A cold-cache run
/// downloads the tarball EXACTLY ONCE: [`fetch_verify_and_hash_tarball`] fetches +
/// verifies it for the pin hash, and [`provision::provision_pm_from_tarball`]
/// installs from that same verified file rather than re-downloading (the prior
/// double download — hash fetch + provision's own fetch — was a real cold-cache
/// bug, fixed 2026-06-11).
fn resolve_provision_declare(
    name: &str,
    spec: &str,
    cwd: &Path,
    maintain_dev_engines: bool,
) -> Result<(String, nub_core::pm::resolve::DeclaredPmWrite)> {
    use nub_core::pm::{Pm, provision, registry, resolve};

    // Fail before any network when there's nowhere to write the declaration
    // (the same never-scaffold rule write_declared_pm enforces — but only
    // after a multi-MB provision, which would be rude).
    if nub_core::workspace::detect::detect_project(cwd).is_none() {
        bail!(
            "no package.json found from {} — the declaration is written into the project manifest",
            cwd.display()
        );
    }

    // Refuse Berry before the network when the spec itself names a 2+ major
    // (`yarn@4.2.2`): the registry's `yarn` package is classic-only, so the
    // resolve would otherwise die with an unhelpful "no version satisfies".
    if name == "yarn" && leading_major(spec).is_some_and(|m| m >= 2) {
        bail!(berry_pin_refusal(cwd));
    }

    // Warm exact re-pin short-circuit: when the user asks for the EXACT version
    // the manifest already pins (with a `+sha512.<hex>` suffix nub itself wrote)
    // AND that version is already extracted in the store, the pin hash and the
    // bytes both already exist — re-fetching the tarball just to recompute a hash
    // we have on disk is pure waste. Skip the network entirely and reuse the
    // committed hex. This skips ONLY the fetch+provision; the declaration /
    // devEngines / lockfile-alignment work downstream still runs, so `use` stays
    // idempotent. Guarded so it CANNOT misfire: the spec must be a concrete
    // semver (ranges/dist-tags still resolve+fetch — they might point somewhere
    // new), the committed pin must name the SAME pm@version with a hash, and the
    // store must be warm (a cold store still needs bytes to install). bun is
    // excluded — it has no store, so its `short_circuit_pm` is `None` and the
    // declaration-only path below handles it. yarn@>=2 already bailed above.
    let short_circuit_pm = match name {
        "npm" => Some(Pm::Npm),
        "pnpm" => Some(Pm::Pnpm),
        // yarn@>=2 (Berry) already bailed above; only yarn classic reaches here.
        // A defensive `None` (not `unreachable!`) keeps a future gate change from
        // turning a Berry exact into a panic — it just declines the short-circuit.
        "yarn" if leading_major(spec).is_some_and(|m| m >= 2) => None,
        "yarn" => Some(Pm::Yarn),
        _ => None,
    };
    if let Some(pm) = short_circuit_pm {
        if semver::Version::parse(spec).is_ok() {
            if let Some((decl_name, decl_version, decl_hex)) =
                resolve::declared_package_manager_exact_hash(cwd)
            {
                let store = pm_store_root()?;
                if decl_name == name
                    && decl_version == spec
                    && provision::pm_version_cached(pm, spec, &store)
                {
                    // No `Fetching` line — nothing was fetched. Run the normal
                    // declaration/devEngines write with the on-disk hash.
                    let write = resolve::write_declared_pm(
                        name,
                        &decl_version,
                        &decl_hex,
                        cwd,
                        maintain_dev_engines,
                    )?;
                    return Ok((decl_version, write));
                }
            }
        }
    }

    // The full authed config from the PROJECT dir — pin was the one remaining
    // no-auth caller of the registry stack, 401ing against private mirrors that
    // every other path (shim provision, update) already authenticated against.
    let cfg = registry::registry_config(cwd);
    let dist = registry::resolve_version_authed(&cfg, name, spec)
        .map_err(|e| humanize_transport_error(e, &cfg.base))?;

    let pm = match name {
        "npm" => Some(Pm::Npm),
        "pnpm" => Some(Pm::Pnpm),
        // A tag/range only resolves to a concrete major now — re-apply the
        // classic/Berry split on the resolved version.
        "yarn" if leading_major(&dist.version).is_some_and(|m| m >= 2) => {
            bail!(berry_pin_refusal(cwd))
        }
        "yarn" => Some(Pm::Yarn),
        // bun: declaration + lockfile only — no provisioning, no shim target.
        "bun" => None,
        other => unreachable!("split_pm_arg admits only npm/pnpm/yarn/bun, got {other}"),
    };

    // ONE download serves both the pin hash and the store install: fetch + verify
    // the tarball here, compute the sha512 from the verified bytes, then hand that
    // same file to the provisioner instead of letting it re-download. (`dist` was
    // resolved against the project's authed/mirror registry config above — the
    // contract `provision_pm_from_tarball` requires.)
    let fetched = fetch_verify_and_hash_tarball(name, &dist, cfg.auth.as_ref())
        .map_err(|e| humanize_transport_error(e, &cfg.base))?;
    let hex = &fetched.hex;

    if let Some(pm) = pm {
        let store = pm_store_root()?;
        // No pin-hash re-verify is needed: the tarball was already checked against
        // dist.integrity, and `hex` was computed from those exact bytes, so a pin
        // check here would compare the file to its own digest. The provisioner
        // extracts from `fetched.path` and prints nothing — the `Fetching…` line
        // above is the whole install announce.
        provision::provision_pm_from_tarball(pm, &dist, &fetched.path, &store)
            .map_err(|e| humanize_transport_error(e, &cfg.base))?;
    }

    let write = resolve::write_declared_pm(name, &dist.version, hex, cwd, maintain_dev_engines)?;
    Ok((dist.version, write))
}

/// `nub pm use <pm>[@<spec>]` — the four spec'd steps, in refuse-early order:
/// the lockfile-alignment PLAN is computed first (pure — its refusals fire
/// before any network or write), then resolve/provision/declare, then the
/// plan executes, then the file-by-file summary prints. A failure at any step
/// leaves earlier artifacts consistent: a failed conversion keeps the source
/// lockfile on disk (the declaration already names the target, and rerunning
/// `use` resumes the migration — idempotence is the contract, not atomicity).
fn run_pm_use(name: &str, spec: &str, cwd: &Path) -> Result<i32> {
    use crate::pm_engine::use_align::{self, AlignPlan};
    use nub_core::pm::resolve;

    // A malformed package.json reads as "no project" downstream — surface the
    // parse failure (with its location) instead.
    check_manifest_json(cwd)?;
    let Some(project) = nub_core::workspace::detect::detect_project(cwd) else {
        bail!(
            "no package.json found from {} — the declaration is written into the project manifest",
            cwd.display()
        );
    };
    // The declaration and the lockfiles live at the workspace root — the same
    // dir write_declared_pm targets.
    let root = project.workspace_root.unwrap_or(project.root);

    // `use nub` — the full switch into nub identity (no registry resolve, no
    // provisioning: the target is the running binary). Whole flow lives in
    // pm_engine::use_nub (manifest fields, lockfile rename/convert,
    // workspace-yaml migration, printed summary).
    if name == "nub" {
        return crate::pm_engine::use_nub::run_use_nub(&root);
    }

    let plan = use_align::plan_alignment(&root, name)?;

    // Refuse a conversion the target format can't faithfully represent
    // (today: `use yarn` over a `workspace:`-protocol graph) BEFORE touching
    // the manifest — a half-switch that pins yarn but writes no lockfile is
    // exactly the silent-broken state we must avoid. The brand preflight must
    // be registered first: the source parse reads workspace config, whose
    // names freeze on first read.
    crate::pm_engine::engine_brand_preflight();
    use_align::refuse_unconvertible(&root, name, &plan)?;

    let (version, write) = resolve_provision_declare(name, spec, cwd, true)?;

    // A committed Berry release outranks packageManager in resolution —
    // `use` never edits settings files (.yarnrc.yml), so say so out loud
    // instead of leaving the declaration silently shadowed.
    if let Some(release) = resolve::committed_yarn_path(cwd) {
        eprintln!(
            "nub: .yarnrc.yml yarnPath still points at {} — it outranks packageManager; \
             remove it to complete the move to {name}.",
            release.display()
        );
    }

    // Step 4 — the file-by-file summary (stdout). Nothing silent.
    println!("using {name}@{version}");
    println!("  package.json: packageManager = {name}@{version} (+sha512)");
    if let Some(range) = &write.dev_engines_range {
        println!(
            "  package.json: devEngines.packageManager = {{ name: \"{name}\", version: \"{range}\", onFail: \"warn\" }}"
        );
    }
    match plan {
        AlignPlan::Fresh => {
            println!(
                "  no lockfile — the next install writes {}",
                use_align::lockfile_name(name)
            );
        }
        AlignPlan::Keep { kept, remove } => {
            let kept_name = kept.file_name().unwrap_or_default().to_string_lossy();
            println!("  {kept_name}: kept (already {name}'s format)");
            for path in remove {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                println!(
                    "  {}: removed (stale — {kept_name} is authoritative)",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }
        AlignPlan::Convert {
            from,
            from_kind,
            remove,
        } => {
            // Conversion goes through the engine's gated writers; the brand
            // preflight must be registered before any engine code reads
            // project state (workspace-yaml names freeze on first read).
            crate::pm_engine::engine_brand_preflight();
            let written = use_align::convert_lockfile(&root, &from, from_kind, name)?;
            println!(
                "  {}: written (converted from {})",
                written.file_name().unwrap_or_default().to_string_lossy(),
                from.file_name().unwrap_or_default().to_string_lossy()
            );
            // Sources are removed only after the write succeeded — migrated,
            // not abandoned (a leftover would recreate the ambiguity).
            for path in remove {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                println!(
                    "  {}: removed (migrated)",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }
        // nub → pnpm: lock.yaml renamed back, byte-identical (the two-mode
        // eject — the format was never forked, so the rename IS the eject).
        AlignPlan::Rename { from, remove } => {
            let to = root.join(use_align::lockfile_name(name));
            std::fs::rename(&from, &to).with_context(|| {
                format!(
                    "renaming {} to {}",
                    from.display(),
                    use_align::lockfile_name(name)
                )
            })?;
            println!(
                "  {}: renamed from {} (bytes unchanged)",
                use_align::lockfile_name(name),
                from.file_name().unwrap_or_default().to_string_lossy()
            );
            for path in remove {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                println!(
                    "  {}: removed (migrated)",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }
    }

    // `use pnpm` regenerates pnpm-workspace.yaml from the nub-mode package.json
    // homes (workspaces + catalogs, top-level overrides/patchedDependencies/
    // allowBuilds/auditConfig) — the exact reverse of `use nub`'s migration.
    // No-op on a project that never carried them.
    if name == "pnpm" {
        for line in crate::pm_engine::use_nub::regenerate_workspace_yaml(&root)? {
            println!("  {line}");
        }
    }
    Ok(0)
}

/// The verified tarball of a resolved PM version, held on disk so the SAME bytes
/// serve both the pin hash and the store install — the single-download artifact of
/// the pin flow (the cold-cache double download was the bug: hash fetch + provision
/// fetch downloaded identical bytes twice, 2026-06-11). `_dir` is the owning temp
/// dir: dropping it deletes `path`, so a `FetchedTarball` must outlive every use of
/// `path`.
struct FetchedTarball {
    /// sha512 hex of the verified bytes — the digest `write_declared_pm` commits.
    hex: String,
    /// The on-disk tarball, already verified against `dist.integrity`. Fed to
    /// [`provision::provision_pm_from_tarball`] so the store install re-uses these
    /// bytes instead of re-downloading.
    path: PathBuf,
    /// Owns `path`'s lifetime — kept as a field, never read.
    _dir: tempfile::TempDir,
}

/// Download the resolved tarball ONCE to a temp file, verify it against the registry
/// dist integrity, and return the verified file plus the sha512 hex of its bytes —
/// the digest `write_declared_pm` commits. The fetch happens even when the version
/// is already in nub's store (an honest hash needs the bytes: pin-implies-fetch, and
/// the store keeps extracted trees, not tarballs); the returned file then feeds the
/// store install directly, so a cold-cache `use` downloads exactly once. The
/// `Fetching <pm> <version> (N MB)…` line IS the install's progress announce — the
/// provision-from-tarball path that follows prints nothing, so there is no duplicate
/// `Using/Installing` block.
fn fetch_verify_and_hash_tarball(
    name: &str,
    dist: &nub_core::pm::registry::VersionDist,
    auth: Option<&nub_core::version_management::download::Auth>,
) -> Result<FetchedTarball> {
    use sha2::{Digest, Sha512};

    let dir = tempfile::tempdir().context("creating a temp dir for the pin fetch")?;
    let path = dir.path().join("pm.tgz");
    let mut announced = false;
    nub_core::version_management::download::download_to_file_auth(
        &dist.tarball,
        &path,
        auth,
        |_done, total| {
            if !announced {
                announced = true;
                match total {
                    Some(t) => {
                        eprintln!("Fetching {name} {} ({} MB)...", dist.version, t / 1_000_000)
                    }
                    None => eprintln!("Fetching {name} {}...", dist.version),
                }
            }
        },
    )
    .with_context(|| format!("downloading {name} {}", dist.version))?;
    nub_core::pm::registry::verify_integrity(&path, &dist.integrity)
        .with_context(|| format!("verifying {name} {}", dist.version))?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let hex = nub_core::pm::hex_lower(&Sha512::digest(&bytes));
    Ok(FetchedTarball {
        hex,
        path,
        _dir: dir,
    })
}

/// The leading numeric major of a version/spec (`4.2.2` → 4, `9` → 9; `^9` /
/// `latest` → None). The yarn classic-vs-Berry gate: only a spec that *names* a
/// concrete major can be classified before resolution.
fn leading_major(spec: &str) -> Option<u32> {
    let digits: String = spec.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// `devEngines.packageManager.version` from the root manifest, when the field
/// names the same PM as the pin — `nub pm update`'s re-resolve constraint (a
/// user-stated range nub reads but never writes; Axiom 3). `None` (field
/// absent, different PM named, or no version) → update resolves `latest`. The
/// root manifest is the workspace root when one exists — the same file
/// `resolve_pin` reads and `write_declared_pm` writes.
fn dev_engines_range(cwd: &Path, pm_name: &str) -> Option<String> {
    let project = nub_core::workspace::detect::detect_project(cwd)?;
    let manifest: serde_json::Value = match &project.workspace_root {
        Some(ws) if *ws != project.root => {
            serde_json::from_str(&std::fs::read_to_string(ws.join("package.json")).ok()?).ok()?
        }
        _ => project.manifest,
    };
    let dev = manifest.get("devEngines")?.get("packageManager")?;
    if dev.get("name").and_then(serde_json::Value::as_str) != Some(pm_name) {
        return None;
    }
    dev.get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// List nub's cached package managers as sorted `<pm>@<version>` strings, reading
/// the `<cache>/pm/<pm>/<version>/` layout `provision_pm` writes. The in-progress
/// `.tmp-*` work dirs are skipped. Deliberately the listing only — no richer entry
/// struct (the `nub node ls` active-marker model doesn't apply: a PM has no
/// "currently active" version independent of the project pin).
fn list_pm_cache(pm_cache: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(pms) = std::fs::read_dir(pm_cache) else {
        return out;
    };
    for pm_entry in pms.flatten() {
        if !pm_entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let pm_name = pm_entry.file_name().to_string_lossy().into_owned();
        let Ok(versions) = std::fs::read_dir(pm_entry.path()) else {
            continue;
        };
        for v in versions.flatten() {
            if !v.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let version = v.file_name().to_string_lossy().into_owned();
            if version.starts_with(".tmp-") {
                continue;
            }
            out.push(format!("{pm_name}@{version}"));
        }
    }
    out.sort();
    out
}

// ── PM shims (`nub pm shim` / `unshim` + the argv0 dispatch) ──────────
//
// Spec: wiki/research/package-manager-shims.md (mechanism + strict-by-default
// agreement check, ratified 2026-06-09). The library core — shim dir, profile
// block, decision matrix, PATH scan — lives in `nub_core::pm::shim`; this
// section owns argv handling, the messages, and the final exec.

/// `nub pm shim`: hardlink the running nub under the six PM names
/// in `~/.nub/shims`, write the marked PATH block into the shell profile
/// (install.sh's mechanism), and verify reachability. Idempotent — re-running
/// re-links, which is also how shims are refreshed after `nub upgrade`.
fn run_pm_shim_install() -> Result<i32> {
    use nub_core::pm::shim::{self, ProfileOutcome, ShimAction};

    // Canonicalized, so a symlinked `nub` on PATH links the real bytes (the
    // same posture as every other `current_nub_binary` call site).
    let nub_binary = nub_core::node::spawn::current_nub_binary()?;
    let dir = shim::shim_dir()?;
    let report = shim::install_shims(&nub_binary)?;

    let count = |action: ShimAction| report.iter().filter(|s| s.action == action).count();
    let (created, relinked, current) = (
        count(ShimAction::Created),
        count(ShimAction::Relinked),
        count(ShimAction::Current),
    );
    let mut parts = Vec::new();
    if created > 0 {
        parts.push(format!("{created} created"));
    }
    if relinked > 0 {
        parts.push(format!("{relinked} re-linked"));
    }
    if current > 0 {
        parts.push(format!("{current} already current"));
    }
    println!(
        "{} entries in {} ({})",
        report.len(),
        dir.display(),
        parts.join(", ")
    );
    if report.iter().any(|s| s.copied) {
        println!(
            "  note: {} is on a different filesystem than the nub binary — \
             copies were made instead of hardlinks",
            dir.display()
        );
    }

    // A shim dir on a `noexec` mount installs cleanly (linking is allowed) and
    // then fails EVERY invocation with a bare "Permission denied" — warn now,
    // naming the dir and the fix, instead of letting each later call fail
    // cryptically. Best-effort (the filesystem's mount-flag word, see
    // shim::dir_is_noexec); the install itself is never failed over the probe.
    if shim::dir_is_noexec(&dir) {
        eprintln!(
            "warning: {} is on a filesystem mounted noexec — the shims are installed but every \
             invocation will fail with \"Permission denied\". Remount without noexec, or use a \
             HOME on an exec-allowed filesystem.",
            dir.display()
        );
    }

    // The PATH block. Windows profile/registry editing is out of scope for v0 —
    // print the line to add instead (honest, not automated).
    if cfg!(windows) {
        println!(
            "  PATH: add {} to your PATH (PATH editing isn't automated on Windows yet)",
            dir.display()
        );
        return Ok(0);
    }
    match shim::add_path_block()? {
        ProfileOutcome::Added(profile) => println!(
            "  added {} to PATH in {}\n  \
             restart your shell, or run: source {}\n  \
             (login/non-interactive profiles are wired automatically too, so\n  \
             IDE- and GUI-spawned package managers see the shims)",
            dir.display(),
            profile.display(),
            profile.display()
        ),
        ProfileOutcome::AlreadyPresent(profile) => {
            println!("  PATH: already present in {}", profile.display())
        }
        // No writable profile for this shell: print the line and exit 0 (the
        // spec's manual fallback — the shims themselves are installed).
        ProfileOutcome::Manual { line } => println!(
            "  PATH: no known shell profile to edit — add this line to your shell config:\n    {line}"
        ),
    }

    // Reachability (Volta's check_shim_reachable idea): meaningful only once
    // the shim dir is on THIS process's PATH — right after a fresh install the
    // block hasn't been sourced yet, and "nothing resolves" would be a false
    // alarm the source hint above already covers.
    if path_contains_dir(&dir) {
        for r in shim::check_shims_reachable(&dir) {
            if r.ok {
                continue;
            }
            match &r.first_hit {
                Some(hit) => eprintln!(
                    "warning: {} resolves to {} which shadows the shim — move {} earlier \
                     in PATH, or remove that binary",
                    r.name,
                    hit.display(),
                    dir.display()
                ),
                None => eprintln!(
                    "warning: {} resolves to nothing on PATH even though {} is on it — \
                     is the shim dir readable?",
                    r.name,
                    dir.display()
                ),
            }
        }
    }
    Ok(0)
}

/// `nub pm unshim`: delete the shim dir and strip the marked PATH block from
/// every known profile. Touches only profiles + the shim dir, so it keeps
/// working from any nub still on PATH (`~/.nub/bin`). Idempotent.
fn run_pm_unshim() -> Result<i32> {
    use nub_core::pm::shim;

    let dir = shim::shim_dir()?;
    let existed = shim::remove_shims()?;
    let changed = shim::remove_path_block()?;
    if existed {
        println!("removed {}", dir.display());
    } else {
        println!("{} was already gone", dir.display());
    }
    for profile in &changed {
        println!("  PATH: removed the shims block from {}", profile.display());
    }
    if changed.is_empty() {
        println!("  PATH: no profile carried the shims block");
    }
    Ok(0)
}

/// Whether `dir` is one of the current process's `PATH` entries (compared
/// canonicalized, so a symlinked entry still counts).
fn path_contains_dir(dir: &Path) -> bool {
    let Ok(canon) = dir.canonicalize() else {
        return false;
    };
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|d| d.canonicalize().ok().as_deref() == Some(&canon))
}

/// The fully resolved plan for one shim invocation — the spawn-free seam (the
/// `bin_launcher` pattern): [`shim_plan`] computes it, [`run_pm_shim`] acts on
/// it, and tests assert the exact program + argv without exec'ing anything.
#[derive(Debug, PartialEq, Eq)]
enum ShimPlan {
    /// Replace this process with `program args…` (Unix `exec`; spawn+wait
    /// where exec doesn't exist). `env` is applied to the exec'd image —
    /// today the PM exec's `NODE_COMPILE_CACHE` (see [`exec_under_project_node`]).
    Exec {
        program: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// The strict agreement check refused: print `message` on stderr, exit 1.
    Refuse { message: String },
}

/// The corepack-style "which PM am I running" notice for the shim-dispatch
/// path: a dim, **stderr-only** `pnpm@11.0.1 (via nub shim)`, printed once
/// before exec. Stderr-only so a bare `pnpm --version | …` stays uncluttered;
/// dim (reusing the PM engine's TTY/`FORCE_COLOR`/`NO_COLOR` predicate) so it
/// reads as a notice, not output. Only the shim's pinned-PM dispatch calls it —
/// direct `nub install`/`nub run` never do (they aren't "via nub shim").
fn emit_shim_version_line(pm: nub_core::pm::Pm, version: &str) {
    let line = shim_version_line(pm, version);
    if crate::pm_engine::scope_warning_uses_dim() {
        eprintln!("\x1b[2m{line}\x1b[0m");
    } else {
        eprintln!("{line}");
    }
}

/// The `<pm>@<version> (via nub shim)` text — split out so the exact wording
/// (which the docs reproduce verbatim) is asserted without capturing stderr.
fn shim_version_line(pm: nub_core::pm::Pm, version: &str) -> String {
    format!("{pm}@{version} (via nub shim)")
}

/// Entry point for an `npm`/`pnpm`/`yarn`/… argv0 invocation through the shim.
fn run_pm_shim(invoked: nub_core::pm::shim::ShimName, args: &[String]) -> Result<i32> {
    let cwd = env::current_dir()?;
    match shim_plan(invoked, args, &cwd)? {
        ShimPlan::Refuse { message } => {
            eprintln!("{message}");
            Ok(1)
        }
        ShimPlan::Exec { program, args, env } => exec_program(&program, &args, &env),
    }
}

/// Resolve the invocation to a [`ShimPlan`]: pin resolve at the workspace root
/// (the same [`resolve_target`] the `nub pm` verbs use) → the pure decision
/// core → provision / PATH-scan as the decision directs. May provision (network
/// on a cold cache); never spawns.
fn shim_plan(
    invoked: nub_core::pm::shim::ShimName,
    args: &[String],
    cwd: &Path,
) -> Result<ShimPlan> {
    use nub_core::pm::Pm;
    use nub_core::pm::resolve::{self, PmTarget};
    use nub_core::pm::shim::{self, Nesting, ShimDecision};

    let target = resolve::resolve_target(cwd);
    let pin_state = shim_pin_state(cwd, target.as_ref());

    // Nested re-entry: when a running PM (e.g. a pnpm postinstall) spawns this
    // shim as a DIFFERENT PM, `npm_config_user_agent`/`npm_execpath` are set in
    // our environment — the ecosystem-standard "a PM is running above me" marker
    // (brand-safe: npm-owned vars, not a NUB_* sentinel). A name mismatch then
    // falls through to the system PM instead of refusing, so the install the
    // user issued one layer up isn't broken by its own lifecycle script. A
    // top-level invocation (no marker) keeps full strict behavior.
    let nesting = Nesting::from_env(|k| env::var(k).ok());

    match shim::decide(
        invoked,
        &pin_state,
        args.first().map(String::as_str),
        nesting,
    ) {
        ShimDecision::Refuse {
            pinned_pm,
            provenance,
        } => Ok(ShimPlan::Refuse {
            message: shim_refusal_message(invoked, pinned_pm, provenance, args),
        }),
        ShimDecision::RefuseNubPinned { provenance } => Ok(ShimPlan::Refuse {
            message: shim_nub_refusal_message(invoked, provenance, args),
        }),
        // A Berry pin never provisions: exec the committed `yarnPath` release
        // under the project's Node, or surface the no-release error.
        ShimDecision::RunPinned {
            pm: Pm::YarnBerry, ..
        } => {
            let Some(PmTarget::YarnPath(release)) = target else {
                bail!(berry_no_yarn_path_msg());
            };
            exec_under_project_node(cwd, release, args)
        }
        ShimDecision::RunPinned { bin_entry, .. } => {
            let Some(PmTarget::Provision(mut pin)) = target else {
                unreachable!("RunPinned with a provisionable pm implies a Provision target")
            };
            // A name-only pin (devEngines.packageManager without a version)
            // constrains the NAME, not the version — prefer the user's own
            // matching PM on PATH: zero network (a spec like "latest" or a
            // lockfile family re-resolves against the registry on EVERY
            // invocation) and no run-to-run drift as new versions publish.
            // Provision the lockfile-implied family / registry latest only on
            // a true PATH miss.
            if pin.version.is_none() {
                let shim_dir = shim::shim_dir()?;
                if let Some(system) = shim::find_system_pm(invoked.as_str(), &shim_dir) {
                    return Ok(ShimPlan::Exec {
                        program: system,
                        args: args.to_vec(),
                        env: Vec::new(),
                    });
                }
                pin.version = Some(
                    lockfile_family_spec(pin.pm, &shim_lockfile_root(cwd))
                        .unwrap_or_else(|| "latest".to_string()),
                );
            }
            // Cache-first: an exact pin already in the store is zero-network.
            // The corepack-style notice (a dim, stderr-only `pnpm@9.5.0 (via nub
            // shim)`) prints BEFORE the install readout via the `on_resolved`
            // hook — fired the moment the concrete version is known, ahead of any
            // `Installing…`/`Installed…` progress. Passing the hook also suppresses
            // provisioning's own `Using <pm> <version>` line (redundant with this
            // header). Direct `nub install`/`nub run` pass no hook — they aren't
            // "via nub shim" and keep their `Using…` line.
            let pm = pin.pm;
            let prov = nub_core::pm::provision::provision_pm_announced(
                &pin,
                &pm_store_root()?,
                cwd,
                None,
                Some(&|version: &str| emit_shim_version_line(pm, version)),
            )?;
            let bin = shim::sibling_bin(&prov.bin, bin_entry)?;
            exec_under_project_node(cwd, bin, args)
        }
        ShimDecision::FallThrough { invoked } => {
            // The recursion guard: the next real <invoked> on PATH, skipping
            // the shim dir itself.
            let shim_dir = shim::shim_dir()?;
            if let Some(system) = shim::find_system_pm(invoked.as_str(), &shim_dir) {
                return Ok(ShimPlan::Exec {
                    program: system,
                    args: args.to_vec(),
                    env: Vec::new(),
                });
            }
            // True PATH miss: provision a dynamic default of the INVOKED PM —
            // announced, never a baked version, and the shim never writes a pin.
            let root = shim_lockfile_root(cwd);
            let (spec, why) = dynamic_default_spec(invoked.pm(), &root)?;
            eprintln!(
                "nub: no {} on PATH — provisioning {}@{spec} ({why}); one-time default, no pin written",
                invoked.as_str(),
                invoked.pm()
            );
            let pin = resolve::PmPin {
                pm: invoked.pm(),
                version: Some(spec),
            };
            let prov = nub_core::pm::provision::provision_pm(&pin, &pm_store_root()?, cwd, None)?;
            let bin = shim::sibling_bin(&prov.bin, invoked.bin_entry())?;
            exec_under_project_node(cwd, bin, args)
        }
    }
}

/// Derive the decision core's [`PinState`] from the resolved [`PmTarget`].
/// `resolve_target` doesn't report WHICH field carried the pin, so provenance
/// is derived here: a committed `yarnPath` short-circuit is `YarnPath`; for the
/// field-borne pins, `packageManager` presence at the workspace-root manifest
/// wins over `devEngines` (mirroring `resolve_pin`'s precedence).
fn shim_pin_state(
    cwd: &Path,
    target: Option<&nub_core::pm::resolve::PmTarget>,
) -> nub_core::pm::shim::PinState {
    use nub_core::pm::Pm;
    use nub_core::pm::resolve::PmTarget;
    use nub_core::pm::shim::{PinProvenance, PinState};

    match target {
        // `resolve_target` rejects a `nub@…` pin (nub isn't a provisionable
        // `Pm` — it never provisions itself), so a nub-pinned project arrives
        // here as `None`. Recognize it before falling through to `Unpinned`,
        // else a foreign-PM shim would provision a competing PM in nub's own
        // project. `project_pm_identity` reads the raw pin name with no
        // allowlist filter, so `nub` flows through.
        None => match nub_core::pm::resolve::project_pm_identity(cwd) {
            Some(id) if id.name == "nub" => PinState::NubPinned {
                provenance: field_pin_provenance(cwd),
            },
            _ => PinState::Unpinned,
        },
        Some(PmTarget::YarnPath(_)) => PinState::Pinned {
            pm: Pm::YarnBerry,
            provenance: PinProvenance::YarnPath,
        },
        Some(PmTarget::Provision(pin)) => PinState::Pinned {
            pm: pin.pm,
            provenance: field_pin_provenance(cwd),
        },
        // Berry pinned by field, no committed release: still a yarn pin at the
        // name level (npm/pnpm refuse; invoked yarn surfaces the no-release
        // error from the RunPinned arm).
        Some(PmTarget::BerryNoYarnPath) => PinState::Pinned {
            pm: Pm::YarnBerry,
            provenance: field_pin_provenance(cwd),
        },
    }
}

/// Which manifest field carries the pin: `packageManager` if present at the
/// workspace root (it wins in `resolve_pin`), else `devEngines.packageManager`.
/// Only called when a field-borne pin resolved, so the binary split is total.
fn field_pin_provenance(cwd: &Path) -> nub_core::pm::shim::PinProvenance {
    use nub_core::pm::shim::PinProvenance;
    let has_package_manager_field = nub_core::workspace::detect::detect_project(cwd)
        .and_then(|project| {
            let manifest: serde_json::Value = match &project.workspace_root {
                Some(ws) if *ws != project.root => {
                    serde_json::from_str(&std::fs::read_to_string(ws.join("package.json")).ok()?)
                        .ok()?
                }
                _ => project.manifest,
            };
            Some(manifest.get("packageManager").is_some())
        })
        .unwrap_or(false);
    if has_package_manager_field {
        PinProvenance::PackageManagerField
    } else {
        PinProvenance::DevEngines
    }
}

/// The strict refusal (decision 1): name the pinned PM, its provenance, the
/// command to paste, and the escapes. Exit code is the caller's (1).
fn shim_refusal_message(
    invoked: nub_core::pm::shim::ShimName,
    pinned: nub_core::pm::Pm,
    provenance: nub_core::pm::shim::PinProvenance,
    args: &[String],
) -> String {
    let invoked = invoked.as_str();
    // The redirect must never synthesize a verb the pinned PM lacks: a blind
    // `<pm> <args…>` swap suggested `pnpm ci`, but pnpm has no `ci`.
    // `safe_redirect` returns `<pm> <same-verb> <args…>` only when the verb
    // exists, else a verbless `use <pm>` — and `None` when there is no verb at
    // all (empty argv / flags-only, e.g. `npm --version`): a read-only
    // invocation needs no redirect, so the "run instead" line is dropped
    // entirely rather than echoing argv back as advice.
    let run_instead = match nub_core::pm::shim::safe_redirect(pinned, args) {
        Some(paste) => format!("\x20 run instead:  {paste}\n"),
        None => String::new(),
    };
    format!(
        "nub: the nub package-manager shims on your PATH (installed via `nub pm shim`) intercepted this.\n\
         This project pins {pinned} (via {provenance}) — refusing to run {invoked}.\n\
         A different package manager here would write a competing lockfile and node_modules.\n\
         \n\
         {run_instead}\
         \x20 to bypass:    invoke the system {invoked} by absolute path, or remove the shims: nub pm unshim"
    )
}

/// The refusal for a nub-pinned project (`pm use nub`): a foreign-PM shim was
/// invoked where nub is the manager. Redirect to `nub <same args>` — nub is a
/// full PM, so the verb carries over verbatim (no verb-absence dance). Never
/// provisions the foreign PM. Exit code is the caller's (1).
fn shim_nub_refusal_message(
    invoked: nub_core::pm::shim::ShimName,
    provenance: nub_core::pm::shim::PinProvenance,
    args: &[String],
) -> String {
    let invoked = invoked.as_str();
    // Flags-only / empty argv (e.g. `pnpm --version`) has no verb to carry —
    // drop the redirect line rather than echoing argv back as advice, matching
    // `shim_refusal_message`'s read-only handling.
    let run_instead = match args.first().filter(|a| !a.starts_with('-')) {
        Some(_) => format!("\x20 run instead:  nub {}\n", args.join(" ")),
        None => String::new(),
    };
    format!(
        "nub: the nub package-manager shims on your PATH (installed via `nub pm shim`) intercepted this.\n\
         This project uses nub (via {provenance}) — refusing to run {invoked}.\n\
         A different package manager here would write a competing lockfile and node_modules.\n\
         \n\
         {run_instead}\
         \x20 to bypass:    invoke the system {invoked} by absolute path, or remove the shims: nub pm unshim"
    )
}

/// The dir whose lockfile governs inference: the workspace root when one
/// exists, else the nearest project root, else `cwd` itself.
fn shim_lockfile_root(cwd: &Path) -> PathBuf {
    match nub_core::workspace::detect::detect_project(cwd) {
        Some(p) => p.workspace_root.clone().unwrap_or(p.root),
        None => cwd.to_path_buf(),
    }
}

/// The lockfile-implied version family of `pm` itself, when the committed
/// lockfile belongs to that PM (`lockfile_version::infer`); `None` otherwise
/// (no lockfile, or it belongs to a different PM — bun included). The single
/// home for the name-level family rule (Display collapses classic/Berry yarn);
/// both [`lockfile_family_spec`] and [`dynamic_default_spec`] route their
/// "same PM → its range" branch through here so the comparison can't drift.
fn lockfile_family(pm: nub_core::pm::Pm, root: &Path) -> Option<String> {
    use nub_core::pm::lockfile_version::{LockfileHint, infer};
    match infer(root) {
        Some(LockfileHint::Pm(hint)) if hint.pm.to_string() == pm.to_string() => Some(hint.range),
        _ => None,
    }
}

/// The lockfile-implied version family of `pm` itself; `None` → caller defaults
/// to `latest`. Thin alias over [`lockfile_family`] for the cache-first PATH-miss
/// site, which wants silent fallthrough with no bun bail and no why-string.
fn lockfile_family_spec(pm: nub_core::pm::Pm, root: &Path) -> Option<String> {
    lockfile_family(pm, root)
}

/// The dynamic-default spec for a PATH miss (decision 3): the lockfile-implied
/// family of the invoked PM, else the registry `latest`; a bun lockfile errors
/// naming bun (nub never provisions bun). Returns `(spec, why)` — `why` feeds
/// the stderr announcement. Layers the bun-bail and the why-strings on top of
/// the shared [`lockfile_family`] name rule.
fn dynamic_default_spec(pm: nub_core::pm::Pm, root: &Path) -> Result<(String, String)> {
    use nub_core::pm::lockfile_version::{LockfileHint, infer};
    // The bun lockfile is a hard error before the name rule: nub never
    // provisions bun, so we refuse rather than silently fall to `latest`.
    if let Some(LockfileHint::Bun) = infer(root) {
        bail!(
            "this project has a bun lockfile (bun.lock / bun.lockb) — nub never provisions bun. \
             Install bun yourself, or remove the bun lockfile to use {pm}."
        );
    }
    Ok(match lockfile_family(pm, root) {
        Some(range) => {
            let why = format!("the committed lockfile implies {pm} {range}");
            (range, why)
        }
        None => {
            // No lockfile, or one belonging to a different PM — both land on
            // `latest`, but the why-string distinguishes them for the user.
            let why = match infer(root) {
                Some(LockfileHint::Pm(hint)) => format!(
                    "the committed lockfile belongs to {}; using the registry latest",
                    hint.pm
                ),
                _ => "no lockfile to infer a version from; using the registry latest".to_string(),
            };
            ("latest".to_string(), why)
        }
    })
}

/// Exec plan for a node-runnable PM entry (`<pm-bin>.cjs`, a committed Berry
/// release): `node <bin> <args…>` under the PROJECT's resolved/provisioned
/// Node (`discover_or_provision_node` — never the shell's `node`).
fn exec_under_project_node(cwd: &Path, bin: PathBuf, args: &[String]) -> Result<ShimPlan> {
    let node = nub_core::node::discovery::discover_or_provision_node(cwd)?;
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(bin.to_string_lossy().into_owned());
    argv.extend(args.iter().cloned());
    // V8 compile cache for the PM bundle. pnpm/npm/yarn are multi-MB single-file
    // bundles whose parse+compile dominates their startup; corepack enables the
    // compile cache for the PM it runs (Module.enableCompileCache in its runner)
    // and is measurably faster for it. NODE_COMPILE_CACHE is Node's own env
    // surface (22.1+; older Node ignores it) — set it to a nub-owned dir so PM
    // cache artifacts never pollute a user's cache dir, and never override a
    // value the user already set (their program, their cache policy).
    let mut env = Vec::new();
    if std::env::var_os("NODE_COMPILE_CACHE").is_none() {
        if let Ok(store) = pm_store_root() {
            let dir = store.join("v8-compile-cache");
            let _ = std::fs::create_dir_all(&dir);
            env.push((
                "NODE_COMPILE_CACHE".to_string(),
                dir.to_string_lossy().into_owned(),
            ));
        }
    }
    Ok(ShimPlan::Exec {
        program: node.path.into_std_path_buf(),
        args: argv,
        env,
    })
}

/// The final act: replace this process's image (Unix `exec` — one process, the
/// PM owns the terminal/signals). Returns only on failure.
#[cfg(unix)]
fn exec_program(program: &Path, args: &[String], envs: &[(String, String)]) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let err = cmd.exec();
    Err(anyhow::Error::new(err).context(format!("could not exec {}", program.display())))
}

/// No `exec` on Windows: spawn + wait, forwarding the exit code. Exit-code
/// fidelity through this path is asserted on the windows-latest CI leg
/// (`tests/pm_shim_windows.rs` — both the `.cmd` fall-through and the
/// node-run pinned PM).
#[cfg(not(unix))]
fn exec_program(program: &Path, args: &[String], envs: &[(String, String)]) -> Result<i32> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let status = cmd
        .status()
        .with_context(|| format!("could not run {}", program.display()))?;
    Ok(nub_core::node::spawn::exit_code_from_status(&status))
}

/// After a successful upgrade through a channel nub doesn't own (npm /
/// homebrew), existing shims still hardlink the PRE-upgrade inode — remind the
/// user to re-link. `None` when no shim dir exists (nothing to remind about).
fn shim_relink_reminder() -> Option<String> {
    let dir = nub_core::pm::shim::shim_dir().ok()?;
    dir.is_dir().then(|| {
        format!(
            "note: the PM shims in {} still run the previous nub until re-linked — run `nub pm shim`.",
            dir.display()
        )
    })
}

/// The self-owned channel owns the new binary's path, so re-link in place
/// right after the swap (best-effort: a failure downgrades to the reminder).
fn relink_shims_after_selfowned(install_dir: &Path) {
    let Ok(dir) = nub_core::pm::shim::shim_dir() else {
        return;
    };
    if !dir.is_dir() {
        return;
    }
    let new_bin = install_dir.join("bin").join("nub");
    match nub_core::pm::shim::install_shims(&new_bin) {
        Ok(_) => println!("re-linked the PM shims in {}", dir.display()),
        Err(e) => {
            eprintln!("could not re-link the PM shims: {e:#} — run `nub pm shim`.")
        }
    }
}

/// Human description of WHERE the resolved Node version requirement came from:
/// the pin source plus its content (`package.json#devEngines.runtime (>=22)`,
/// `.node-version (26)`), or `node on PATH` when no source pins. Used by
/// `nub node` (status) and `nub node which` — routed through the SAME
/// `resolve_pin_chain` the run path resolves with, so the reported source can't
/// drift from the version that actually governs (the spec's "flag the
/// resolution source in any user-facing message" rule). Chain warnings are not
/// re-printed here (the `discover_node` call that precedes every caller already
/// printed them); a chain refusal can't reach here for the same reason, but is
/// named honestly rather than misreported as PATH.
fn resolution_source(cwd: &Path) -> String {
    match nub_core::node::discovery::resolve_pin_chain(cwd) {
        Ok(chain) => match chain.pin {
            Some((raw, _pin, source)) => format!("{source} ({raw})"),
            None => "node on PATH".to_string(),
        },
        Err(_) => "package.json#devEngines.runtime (refused — non-Node runtime)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    // `--env-file` opts the run out of eager `.env*` auto-discovery: with the flag
    // present, only the explicit file(s) reach the child; with it absent, the autos
    // load as before (the maintainer, 2026-06-15). `merge_child_env` is the gate, so locking
    // the contract here covers both the run and watch paths (both call it).
    #[test]
    fn env_file_flag_suppresses_dotenv_auto_discovery() {
        // Stand-ins for the auto-loaded `.env`/`.env.local` map and the explicit
        // `--env-file` contents. Keys are nub-test-private so the shell-wins guard
        // (`env::var_os`) inside merge_child_env never trips on a real env var.
        let auto = || {
            HashMap::from([
                ("NUB_TEST_FOO".to_string(), "from_dotenv".to_string()),
                ("NUB_TEST_BAR".to_string(), "from_local".to_string()),
            ])
        };
        let explicit = HashMap::from([("NUB_TEST_BAZ".to_string(), "from_explicit".to_string())]);

        // (a) flag present → autos suppressed, only the explicit var survives.
        let merged = merge_child_env(auto(), true, &explicit);
        assert_eq!(
            merged.get("NUB_TEST_BAZ").map(String::as_str),
            Some("from_explicit"),
            "explicit --env-file var must reach the child"
        );
        assert!(
            !merged.contains_key("NUB_TEST_FOO") && !merged.contains_key("NUB_TEST_BAR"),
            "all four auto `.env*` files are suppressed when --env-file is present; got {merged:?}"
        );

        // (b) no flag → autos load, explicit map is empty so nothing else appears.
        let merged = merge_child_env(auto(), false, &HashMap::new());
        assert_eq!(
            merged.get("NUB_TEST_FOO").map(String::as_str),
            Some("from_dotenv")
        );
        assert_eq!(
            merged.get("NUB_TEST_BAR").map(String::as_str),
            Some("from_local")
        );

        // (c) explicit `--env-file` overrides a same-key value — and it is the
        // explicit file's value that wins, never a leaked auto value (autos are gone).
        let explicit_override =
            HashMap::from([("NUB_TEST_FOO".to_string(), "from_explicit".to_string())]);
        let merged = merge_child_env(auto(), true, &explicit_override);
        assert_eq!(
            merged.get("NUB_TEST_FOO").map(String::as_str),
            Some("from_explicit"),
            "explicit file's value wins; no auto `.env` leakage"
        );
        assert!(
            !merged.contains_key("NUB_TEST_BAR"),
            "non-overridden auto var stays suppressed; got {merged:?}"
        );
    }

    #[test]
    fn aube_lockfile_detects_pnpm_lock_in_project_dir() {
        // Linkage spike for the vendored aube workspace (vendor/aube submodule):
        // proves the cross-workspace path dep on aube-lockfile compiles and links
        // by exercising its lockfile-kind detection against a real temp dir.
        use aube_lockfile::{LockfileKind, detect_existing_lockfile_kind};

        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            detect_existing_lockfile_kind(dir.path()),
            None,
            "empty project dir must detect no lockfile"
        );
        std::fs::write(
            dir.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .expect("write pnpm-lock.yaml");
        assert_eq!(
            detect_existing_lockfile_kind(dir.path()),
            Some(LockfileKind::Pnpm),
            "pnpm-lock.yaml on disk must detect as LockfileKind::Pnpm"
        );
    }

    #[test]
    fn aube_lib_seam_exposes_install_entry_point() {
        // Embedding-seam spike for the aube *library* target (vendor/aube fork,
        // lib split landed in nubjs/aube@b15cdcb): proves nub can construct the
        // install options and reach `commands::install::run` without shelling
        // out. No network, no install run — this is a link/shape check only.
        use aube::commands::install::{FrozenMode, InstallOptions};

        let opts = InstallOptions::with_mode(FrozenMode::Prefer);
        assert!(
            matches!(opts.mode, FrozenMode::Prefer),
            "with_mode must store the requested frozen mode"
        );
        // Name the async entry point so the seam (not just the options struct)
        // must resolve and link.
        let _entry = aube::commands::install::run;
    }

    #[test]
    fn is_node_bin_classifies_by_shebang_line_not_body() {
        // aube's `.bin` entries are `#!/bin/sh` shim scripts whose BODY mentions
        // node (`NODE_PATH=…`, `exec "$basedir/node" …`). Those must run via the sh
        // interpreter (the kernel honors the shebang), NOT through `node <shim>` —
        // feeding the sh script to node throws `SyntaxError: Invalid or unexpected
        // token`. is_node_bin must key off the shebang LINE naming node, not any
        // occurrence of "node" in the first 128 bytes.
        let dir = tempfile::tempdir().expect("tempdir");

        let sh_shim = dir.path().join("cowsay");
        std::fs::write(
            &sh_shim,
            "#!/bin/sh\n# aube-bin-shim v1\nexport NODE_PATH=\"$basedir/..\"\nexec \"$basedir/node\" \"$basedir/../cli.js\" \"$@\"\n",
        )
        .expect("write sh shim");
        assert!(
            !is_node_bin(&sh_shim),
            "#!/bin/sh shim that references node in its body must NOT run under node"
        );

        let node_shim = dir.path().join("tsc");
        std::fs::write(&node_shim, "#!/usr/bin/env node\nconsole.log(1)\n")
            .expect("write node shim");
        assert!(
            is_node_bin(&node_shim),
            "#!/usr/bin/env node entry must run under node"
        );
    }

    #[cfg(unix)]
    #[test]
    fn find_posix_sh_locates_sh() {
        // `--shell-emulator` needs a POSIX `sh`. On any Unix box `sh` is on PATH,
        // so the detector must find it. (The Windows Git-for-Windows search path is
        // exercised on the windows-latest CI leg — Docker on the dev box is Linux
        // only and can't stand in for it.)
        let sh = find_posix_sh().expect("find_posix_sh must locate sh on Unix");
        let stem = std::path::Path::new(&sh)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        assert_eq!(stem, "sh", "resolved path should be an `sh`: {sh}");
    }

    #[test]
    fn flag_before_subcommand_normalizes_to_canonical_order() {
        // pnpm's `nub -r run build` order reorders to nub's `run -r build`;
        // value-taking flags carry their value; Node flags / files / eval / the
        // already-canonical order are left untouched (None).
        fn norm(a: &[&str]) -> Option<Vec<String>> {
            normalize_leading_run_flags(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        }
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<String>>();

        assert_eq!(
            norm(&["-r", "run", "build"]),
            Some(v(&["run", "-r", "build"]))
        );
        assert_eq!(
            norm(&["--filter", "foo", "run", "build"]),
            Some(v(&["run", "--filter", "foo", "build"]))
        );
        assert_eq!(
            norm(&["--filter=foo", "run", "build"]),
            Some(v(&["run", "--filter=foo", "build"]))
        );
        assert_eq!(
            norm(&["-r", "-F", "x", "exec", "tsc"]),
            Some(v(&["exec", "-r", "-F", "x", "tsc"]))
        );

        // Left untouched (None):
        assert_eq!(norm(&["run", "-r", "build"]), None); // already canonical
        assert_eq!(norm(&["--inspect", "run", "build"]), None); // Node flag, not a run-flag
        assert_eq!(norm(&["-r", "app.ts"]), None); // run-flag but no run/exec follows
        assert_eq!(norm(&["app.ts"]), None); // bare file
        assert_eq!(norm(&["-e", "code"]), None); // eval
    }

    #[test]
    fn subcommand_run_parses() {
        let cli = parse(&["nub", "run", "dev"]).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Run { ref script, .. }) if script.as_deref() == Some("dev"))
        );
    }

    #[test]
    fn install_parses_with_the_i_alias_and_engine_flags() {
        // `nub i -P --node-linker hoisted` ≡ `nub install …` (npm/pnpm muscle
        // memory); the engine flags land on the variant, and the frozen
        // lockfile flags stay mutually exclusive.
        let cli = parse(&["nub", "i", "-P", "--node-linker", "hoisted"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Install { prod: true, ref node_linker, .. })
                if node_linker.as_deref() == Some("hoisted")
        ));
        assert!(
            parse(&[
                "nub",
                "install",
                "--frozen-lockfile",
                "--no-frozen-lockfile"
            ])
            .is_err(),
            "the frozen-lockfile flags are mutually exclusive"
        );
    }

    #[test]
    fn install_routes_to_add_args() {
        let args = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // Global add (the #29 form) — preserved.
        assert_eq!(
            install_to_add_args(&args(&["install", "-g", "is-number@7.0.0"])),
            Some(args(&["add", "-g", "is-number@7.0.0"])),
            "nub install -g <pkg> routes through the engine's global add path"
        );
        assert_eq!(
            install_to_add_args(&args(&["i", "--global", "is-number@7.0.0"])),
            Some(args(&["add", "--global", "is-number@7.0.0"])),
            "nub i --global <pkg> routes through the same alias"
        );

        // Local add (the P0) — a bare package positional routes to add.
        assert_eq!(
            install_to_add_args(&args(&["install", "express"])),
            Some(args(&["add", "express"])),
            "nub install <pkg> is the add-to-dependencies form"
        );
        assert_eq!(
            install_to_add_args(&args(&["i", "lodash"])),
            Some(args(&["add", "lodash"])),
            "nub i <pkg> routes to add"
        );

        // pnpm save flags forwarded or translated to aube's `add` grammar.
        assert_eq!(
            install_to_add_args(&args(&["install", "--save-dev", "vitest"])),
            Some(args(&["add", "--save-dev", "vitest"])),
            "--save-dev forwards to add verbatim"
        );
        assert_eq!(
            install_to_add_args(&args(&["install", "-D", "vitest"])),
            Some(args(&["add", "-D", "vitest"])),
            "-D (aube's save-dev short) forwards verbatim"
        );
        assert_eq!(
            install_to_add_args(&args(&["install", "-d", "vitest"])),
            Some(args(&["add", "--save-dev", "vitest"])),
            "pnpm lowercase -d → aube's --save-dev long form"
        );
        assert_eq!(
            install_to_add_args(&args(&["install", "-o", "fsevents"])),
            Some(args(&["add", "--save-optional", "fsevents"])),
            "pnpm lowercase -o → aube's --save-optional long form"
        );
        assert_eq!(
            install_to_add_args(&args(&["install", "-e", "react@18.0.0"])),
            Some(args(&["add", "--save-exact", "react@18.0.0"])),
            "pnpm lowercase -e → aube's --save-exact long form"
        );
        assert_eq!(
            install_to_add_args(&args(&["install", "-P", "express"])),
            Some(args(&["add", "express"])),
            "-P (save-prod) is the add default, so it is dropped"
        );
        assert_eq!(
            install_to_add_args(&args(&["install", "-p", "express"])),
            Some(args(&["add", "express"])),
            "pnpm -p (save-prod) is dropped — add saves to dependencies by default"
        );

        // pnpm `-w` is the boolean `--workspace-root` — forwarded verbatim, NOT
        // an npm-style member selector (no `--filter` translation).
        assert_eq!(
            install_to_add_args(&args(&["install", "-w", "express"])),
            Some(args(&["add", "-w", "express"])),
            "pnpm -w (--workspace-root boolean) forwards verbatim, not translated to --filter"
        );

        // A `--` separator stops the scan and keeps tokens literal.
        assert_eq!(
            install_to_add_args(&args(&["install", "--", "-g"])),
            None,
            "a leading separator makes -g positional, not an install-global flag"
        );
        assert_eq!(
            install_to_add_args(&args(&["install", "express", "--", "-some-weird-spec"])),
            Some(args(&["add", "express", "--", "-some-weird-spec"])),
            "tokens after -- are forwarded literally"
        );

        // Native install path is preserved (no package, no -g).
        assert_eq!(
            install_to_add_args(&args(&["install"])),
            None,
            "plain nub install stays on the native argumentless install path"
        );
        assert_eq!(
            install_to_add_args(&args(&["install", "--frozen-lockfile"])),
            None,
            "nub install with only native flags stays native"
        );
        assert_eq!(
            install_to_add_args(&args(&["install", "-F", "pkg-a", "-r"])),
            None,
            "nub install with workspace selectors but no package stays a native install"
        );
    }

    #[test]
    fn install_help_does_not_advertise_unapproved_gvs_flags() {
        let err = parse(&["nub", "install", "--help"]).expect_err("--help exits through clap");
        let help = err.render().to_string();
        assert!(
            help.contains("--node-linker") && help.contains("--registry"),
            "sanity-check install help rendered: {help}"
        );
        for flag in [
            "--enable-global-virtual-store",
            "--disable-global-virtual-store",
            "--enable-gvs",
            "--disable-gvs",
        ] {
            assert!(
                !help.contains(flag),
                "nub install help must not advertise unapproved GVS flag {flag}:\n{help}"
            );
            assert!(
                parse(&["nub", "install", flag]).is_err(),
                "nub install must reject unapproved GVS flag {flag}"
            );
        }
    }

    #[test]
    fn subcommand_run_without_script_parses_to_none() {
        // `nub run` (no script) must parse — not a clap "required arg" error —
        // so run_script can list available scripts (A46).
        let cli = parse(&["nub", "run"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Run { script: None, .. })
        ));
    }

    #[test]
    fn subcommand_run_with_node_flag() {
        let cli = parse(&["nub", "run", "--node", "build"]).unwrap();
        match cli.command {
            Some(Command::Run {
                node, ref script, ..
            }) => {
                assert!(node);
                assert_eq!(script.as_deref(), Some("build"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn subcommand_run_with_filter() {
        let cli = parse(&["nub", "run", "--filter", "@org/api", "dev"]).unwrap();
        match cli.command {
            Some(Command::Run {
                ref filter,
                ref script,
                ..
            }) => {
                assert_eq!(filter.as_slice(), ["@org/api"]);
                assert_eq!(script.as_deref(), Some("dev"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn subcommand_run_collects_repeated_filters() {
        // Each `--filter` appends; clap must not let the last one win (A29).
        let cli = parse(&["nub", "run", "--filter", "a", "--filter", "!b", "build"]).unwrap();
        match cli.command {
            Some(Command::Run {
                ref filter,
                ref script,
                ..
            }) => {
                assert_eq!(filter.as_slice(), ["a", "!b"]);
                assert_eq!(script.as_deref(), Some("build"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn subcommand_run_recursive() {
        let cli = parse(&["nub", "run", "-r", "build"]).unwrap();
        match cli.command {
            Some(Command::Run { recursive, .. }) => assert!(recursive),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn run_value_consuming_flags_lists_every_separate_token_value_flag() {
        // The positional-split (`split_subcommand_argv`) relies on this list to
        // know which flags swallow a following token. Every separate-token value
        // flag on `run` (`--workspace <name>`, `--resume-from <pkg>`,
        // `--script-shell <path>`, `-F <selector>`, plus filter/cwd/concurrency)
        // MUST appear, or its value mis-binds as the script positional. This test
        // is the regression guard for that coupling: it fails loudly if a new
        // value flag is added to `Command::Run` without registering it here.
        let flags = value_consuming_flags("run");
        for required in [
            "--filter",
            "-F",
            "--workspace",
            "--resume-from",
            "--script-shell",
            "--workspace-concurrency",
        ] {
            assert!(
                flags.contains(&required),
                "{required} missing from value_consuming_flags(\"run\")"
            );
        }
    }

    #[test]
    fn run_workspace_value_does_not_bind_as_script_via_positional_split() {
        // End-to-end of the coupling: with `--workspace` registered as value-
        // consuming, the split must treat `foo` as the flag's value and `build`
        // as the positional/script — not `foo` as the script. Verified through
        // the same split path the dispatcher uses.
        let rest = vec![
            "run".into(),
            "--workspace".into(),
            "foo".into(),
            "build".into(),
            "--extra".into(),
        ];
        let (prefix, suffix) = split_subcommand_argv(rest);
        // prefix ends at the positional (`build`); `--extra` forwards verbatim.
        assert_eq!(prefix, ["run", "--workspace", "foo", "build"]);
        assert_eq!(suffix, ["--extra"]);
        let cli = Cli::parse_from(std::iter::once("nub".to_string()).chain(prefix)).command;
        match cli {
            Some(Command::Run {
                script, workspace, ..
            }) => {
                assert_eq!(
                    script.as_deref(),
                    Some("build"),
                    "build must be the script, not foo"
                );
                assert_eq!(
                    workspace,
                    ["foo"],
                    "foo must bind as the --workspace member"
                );
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn run_strips_the_post_script_dashdash_separator() {
        // `nub run build -- a b c` must forward `["a","b","c"]`, not
        // `["--","a","b","c"]`: the first `--` after the script positional is the
        // conventional end-of-options separator (npm/pnpm/yarn/cargo all drop it).
        // Only that first `--` is consumed; a later `--` is a literal argument.
        let (_, suffix) = split_subcommand_argv(
            ["run", "build", "--", "a", "b", "c"]
                .map(String::from)
                .to_vec(),
        );
        assert_eq!(suffix, ["a", "b", "c"]);

        let (_, suffix) = split_subcommand_argv(
            ["run", "build", "--", "a", "--", "b"]
                .map(String::from)
                .to_vec(),
        );
        assert_eq!(suffix, ["a", "--", "b"]);

        // No separator: args forward verbatim, including a literal `--` mid-stream.
        let (_, suffix) =
            split_subcommand_argv(["run", "build", "a", "b"].map(String::from).to_vec());
        assert_eq!(suffix, ["a", "b"]);
    }

    #[test]
    fn subcommand_exec_parses() {
        let cli = parse(&["nub", "exec", "vitest"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Exec { ref bin, .. }) if bin == "vitest"));
    }

    #[test]
    fn subcommand_exec_with_node_flag() {
        let cli = parse(&["nub", "exec", "--node", "vitest"]).unwrap();
        match cli.command {
            Some(Command::Exec { node, ref bin, .. }) => {
                assert!(node);
                assert_eq!(bin, "vitest");
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    #[test]
    fn subcommand_watch_parses() {
        let cli = parse(&["nub", "watch", "server.ts"]).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Watch { ref file, .. }) if file == "server.ts")
        );
    }

    #[test]
    fn subcommand_upgrade_parses() {
        let cli = parse(&["nub", "upgrade", "--dry-run"]).unwrap();
        match cli.command {
            Some(Command::Upgrade { dry_run, .. }) => assert!(dry_run),
            other => panic!("expected Upgrade, got {other:?}"),
        }
    }

    // P0 regression guard: a self-owned upgrade must leave bin/nub EXECUTABLE.
    // The release tarball ships the binary at 0644 (CI's artifact round-trip
    // strips +x), so after the staging-extract + swap_dir the freshly-installed
    // `nub` is non-executable until ensure_bin_executable re-applies the mode —
    // omit that step and every `nub upgrade` ends in "command not found". This
    // replays the swap sequence on a 0644 staged binary and asserts the mode bit.
    #[cfg(unix)]
    #[test]
    fn self_owned_upgrade_makes_binary_executable() {
        use std::os::unix::fs::PermissionsExt;

        let install = tempfile::tempdir().expect("install dir");
        // A prior install already has a (executable) bin/ in place, so the swap
        // exercises the move-aside branch of swap_dir, matching a real upgrade.
        let old_bin = install.path().join("bin");
        std::fs::create_dir_all(&old_bin).unwrap();
        std::fs::write(old_bin.join("nub"), b"#!old\n").unwrap();

        // Staged new bin/, as `tar -xzf` lands it from a 0644 archive.
        let staged_bin = install.path().join("staged-bin");
        std::fs::create_dir_all(&staged_bin).unwrap();
        let staged_nub = staged_bin.join("nub");
        std::fs::write(&staged_nub, b"#!new\n").unwrap();
        std::fs::set_permissions(&staged_nub, std::fs::Permissions::from_mode(0o644)).unwrap();

        swap_dir(install.path(), "bin", &staged_bin).expect("swap bin");
        let live_nub = install.path().join("bin").join("nub");
        // Precondition: the swapped-in binary really is non-executable (0644) —
        // proves the bug exists absent the fix, not a vacuous pass.
        assert_eq!(
            std::fs::metadata(&live_nub).unwrap().permissions().mode() & 0o111,
            0,
            "staged 0644 binary must arrive non-executable before the chmod"
        );

        ensure_bin_executable(&live_nub).expect("chmod +x");
        assert_ne!(
            std::fs::metadata(&live_nub).unwrap().permissions().mode() & 0o100,
            0,
            "after upgrade, bin/nub must have the owner-execute bit set"
        );
    }

    // BLOCKER regression guard: the npm-channel upgrade must target the scoped
    // `@nubjs/nub`. The bare `nub` package on npm is unrelated third-party code;
    // emitting `npm install -g nub@…` would clobber a working install with a
    // stranger's package. This is the single point the package name is built.
    #[test]
    fn npm_upgrade_targets_the_scoped_nubjs_package() {
        assert_eq!(
            npm_upgrade_command("latest"),
            "npm install -g @nubjs/nub@latest"
        );
        assert_eq!(
            npm_upgrade_command("1.2.3"),
            "npm install -g @nubjs/nub@1.2.3"
        );
    }

    // The npm-channel runner must launch npm in an OS-correct way: on Windows
    // `npm` is the `npm.cmd` batch shim, only launchable through `cmd.exe`, so a
    // bare `sh -c …` (no `sh` on a plain Windows box) breaks the only upgrade
    // path npm-installed Windows users have. Assert the program + argv shape for
    // the current host (the inactive branch is cfg-pruned, so this exercises
    // whichever arm this build compiles). The package spec arrives as ONE argv
    // token regardless of OS, so there is no shell-quoting to get wrong.
    #[test]
    fn npm_upgrade_invocation_is_os_correct() {
        let cmd = npm_upgrade_command_invocation("latest");
        let prog = cmd.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if cfg!(windows) {
            assert_eq!(prog, "cmd");
            assert_eq!(
                args,
                vec!["/C", "npm", "install", "-g", "@nubjs/nub@latest"]
            );
        } else {
            assert_eq!(prog, "npm");
            assert_eq!(args, vec!["install", "-g", "@nubjs/nub@latest"]);
        }
        // No `sh` on any host — that was the Windows-breaking form.
        assert_ne!(prog, "sh");
    }

    // The channel router decides delegate-vs-self-swap. node_modules ⇒ npm even
    // when nested; the `~/.nub/bin/nub` curl layout ⇒ self-owned with the .nub
    // root as install_dir; an arbitrary path ⇒ Unknown (manual instructions).
    #[test]
    fn detect_channel_routes_by_install_layout() {
        assert_eq!(
            detect_channel(Path::new("/usr/lib/node_modules/@nubjs/nub/bin/nub")),
            UpgradeChannel::Npm
        );
        assert_eq!(
            detect_channel(Path::new("/opt/homebrew/Cellar/nub/0.0.6/bin/nub")),
            UpgradeChannel::Homebrew
        );
        match detect_channel(Path::new("/home/u/.nub/bin/nub")) {
            UpgradeChannel::SelfOwned { install_dir } => {
                assert_eq!(install_dir, Path::new("/home/u/.nub"));
            }
            other => panic!("expected SelfOwned, got {other:?}"),
        }
        assert_eq!(
            detect_channel(Path::new("/some/random/place/nub")),
            UpgradeChannel::Unknown
        );
    }

    // Fail-fast contract: on Windows the self-owned swap must refuse before any
    // filesystem op (a running nub.exe can't replace its own files), and the
    // message must hand the user a concrete recovery path. On non-Windows the
    // guard is a no-op so the real swap proceeds. Tested via the pure helper so
    // both branches run regardless of the host OS.
    #[test]
    fn windows_selfowned_upgrade_is_refused_with_recovery_steps() {
        assert!(windows_selfowned_unsupported(false).is_none());
        let msg = windows_selfowned_unsupported(true).expect("must refuse on Windows");
        assert!(msg.contains("isn't supported on Windows"));
        assert!(msg.contains("npm install -g @nubjs/nub@latest"));
        assert!(msg.contains("re-run the Windows installer"));
    }

    /// Serializes the tests that read or mutate the release-URL env seams
    /// (`NUB_RELEASE_BASE_URL` / `NUB_RELEASE_LATEST_URL`). Those vars are
    /// process-global, so a test that sets them mustn't run concurrently with one
    /// that asserts the unset-default URL shape.
    static RELEASE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // Verification correctness: the checksum sidecar URL is the tarball URL plus
    // `.sha256`, and the digest helper matches the well-known empty-input vector.
    // Asserts the DEFAULT (env-seam unset) URLs, so it holds the release-env lock
    // to never observe the seam mid-override from the e2e test below.
    #[test]
    fn tarball_and_checksum_urls_pair_up() {
        let _g = RELEASE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let url = tarball_url("0.0.6", "darwin-arm64");
        assert_eq!(
            url,
            "https://github.com/nubjs/nub/releases/download/v0.0.6/nub-darwin-arm64.tar.gz"
        );
        assert_eq!(
            checksum_url("0.0.6", "darwin-arm64"),
            format!("{url}.sha256")
        );
        // SHA-256 of the empty string — pins the digest formatting (lowercase hex).
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // END-TO-END upgrade test against a LOCAL fake release — closes the "upgrade
    // was untestable, so we shipped a broken one" gap. Drives the real self-owned
    // path (`perform_selfowned_upgrade`) — resolve `latest`, download, verify the
    // SHA-256, extract, atomic-swap, +x, recreate the nubx alias — entirely
    // against `file://` fixtures via the internal `NUB_RELEASE_*` seams, so it
    // touches NO network. Asserts: (a) `latest` resolves to the tag the fake
    // channel advertises, (b) the swapped-in binary is the fixture's bytes,
    // executable, with the nubx alias present, and (c) the npm-channel install
    // invocation is OS-correct for the resolved version. A wrong-checksum sub-case
    // proves the verify step actually rejects a tampered artifact.
    //
    // Unix-only: the self-owned swap path (symlink, mode bits, the Windows
    // fail-fast bail) is POSIX; the Windows arm is covered by
    // `windows_selfowned_upgrade_is_refused_with_recovery_steps`.
    #[cfg(unix)]
    #[test]
    fn self_owned_upgrade_runs_end_to_end_against_a_local_fake_release() {
        use std::os::unix::fs::PermissionsExt;

        // Only run where this build actually publishes a tarball — the path under
        // test bails early otherwise, and there'd be nothing to exercise.
        let Some(target) = platform_target() else {
            eprintln!("skipping: no published tarball target for this platform");
            return;
        };
        let _g = RELEASE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        const FAKE_VERSION: &str = "9.9.9"; // not a real release — proves the channel, not the net
        let fixture = tempfile::tempdir().expect("fixture root");

        // 1. Build the artifact the fake channel serves: a tar.gz containing the
        //    install.sh layout (bin/nub + runtime/), with sentinel bytes so we can
        //    prove the SWAPPED-IN binary is the downloaded one, not the old one.
        const NEW_NUB_BYTES: &[u8] = b"#!/bin/sh\necho fake-upgraded-nub 9.9.9\n";
        let build = fixture.path().join("build");
        std::fs::create_dir_all(build.join("bin")).unwrap();
        std::fs::create_dir_all(build.join("runtime")).unwrap();
        std::fs::write(build.join("bin").join("nub"), NEW_NUB_BYTES).unwrap();
        std::fs::write(build.join("runtime").join("VERSION"), FAKE_VERSION).unwrap();

        // Lay the asset down at GitHub's release-asset path shape so the seam's
        // `<base>/v<version>/nub-<target>.tar.gz` resolves: download_base/v9.9.9/.
        let asset_dir = fixture
            .path()
            .join("releases")
            .join("download")
            .join(format!("v{FAKE_VERSION}"));
        std::fs::create_dir_all(&asset_dir).unwrap();
        let archive = asset_dir.join(format!("nub-{target}.tar.gz"));
        let tar_ok = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&build)
            .args(["bin", "runtime"])
            .status()
            .expect("tar the fixture archive");
        assert!(tar_ok.success(), "fixture archive must tar cleanly");

        // The sidecar carries the real digest of the archive we just wrote, in the
        // `<hex>␠␠<name>` shasum format the fetch parses.
        let archive_bytes = std::fs::read(&archive).unwrap();
        let digest = sha256_hex(&archive_bytes);
        std::fs::write(
            asset_dir.join(format!("nub-{target}.tar.gz.sha256")),
            format!("{digest}  nub-{target}.tar.gz\n"),
        )
        .unwrap();

        // 2. The fake "latest" API response: `latest` must resolve to v9.9.9.
        let latest_json = fixture.path().join("latest.json");
        std::fs::write(&latest_json, format!(r#"{{"tag_name":"v{FAKE_VERSION}"}}"#)).unwrap();

        // 3. A self-owned install with a DIFFERENT old binary in place, so the swap
        //    has something to replace and we can prove the bytes changed.
        let install = fixture.path().join(".nub");
        let old_bin = install.join("bin");
        std::fs::create_dir_all(&old_bin).unwrap();
        std::fs::write(old_bin.join("nub"), b"#!/bin/sh\necho OLD\n").unwrap();
        std::fs::create_dir_all(install.join("runtime")).unwrap();
        std::fs::write(install.join("runtime").join("VERSION"), "0.0.1").unwrap();

        // 4. Point the seams at the fixtures and run the REAL upgrade path. `file://`
        //    URLs (curl supports them) keep this entirely off the network. SAFETY:
        //    serialized by RELEASE_ENV_LOCK; restored in this same scope below.
        let base_url = format!("file://{}/releases/download", fixture.path().display());
        let latest_url = format!("file://{}", latest_json.display());
        unsafe {
            std::env::set_var(RELEASE_DOWNLOAD_BASE_ENV, &base_url);
            std::env::set_var(RELEASE_LATEST_API_ENV, &latest_url);
        }

        // (a) `latest` resolves to the fake channel's advertised tag.
        let resolved = resolve_version("latest").expect("resolve latest from fake channel");
        assert_eq!(
            resolved, FAKE_VERSION,
            "`latest` must resolve to the tag the fake channel advertises, got {resolved}"
        );

        // The full download→verify→extract→swap→chmod→symlink path.
        let result = perform_selfowned_upgrade(&install, "latest");

        // Wrong-checksum sub-case: corrupt the archive after digesting it and prove
        // the verify step REFUSES it (run before asserting the happy path so a
        // mistakenly-passing verify can't be masked by the good run). Use a second
        // install so the good run's result is independent.
        let bad_install = fixture.path().join(".nub-bad");
        std::fs::create_dir_all(bad_install.join("bin")).unwrap();
        std::fs::write(bad_install.join("bin").join("nub"), b"OLD\n").unwrap();
        std::fs::create_dir_all(bad_install.join("runtime")).unwrap();
        std::fs::write(
            asset_dir.join(format!("nub-{target}.tar.gz.sha256")),
            format!("{}  nub-{target}.tar.gz\n", "0".repeat(64)),
        )
        .unwrap();
        let bad = perform_selfowned_upgrade(&bad_install, "latest");

        // Restore env BEFORE asserting so a failed assertion can't leak the seam.
        unsafe {
            std::env::remove_var(RELEASE_DOWNLOAD_BASE_ENV);
            std::env::remove_var(RELEASE_LATEST_API_ENV);
        }

        // (b) The happy path installed the fixture's bytes, executable, with nubx.
        result.expect("upgrade against the local fake release must succeed");
        let live_nub = install.join("bin").join("nub");
        assert_eq!(
            std::fs::read(&live_nub).unwrap(),
            NEW_NUB_BYTES,
            "swapped-in bin/nub must be the downloaded fixture bytes, not the old binary"
        );
        assert_ne!(
            std::fs::metadata(&live_nub).unwrap().permissions().mode() & 0o111,
            0,
            "upgraded bin/nub must be executable (the 0644-from-CI bug fix)"
        );
        let nubx = install.join("bin").join("nubx");
        assert_eq!(
            std::fs::read_link(&nubx).unwrap(),
            Path::new("nub"),
            "self-owned upgrade must recreate the nubx → nub alias"
        );
        assert_eq!(
            std::fs::read(install.join("runtime").join("VERSION")).unwrap(),
            FAKE_VERSION.as_bytes(),
            "runtime/ must be swapped to the new release's payload"
        );

        // (c) The npm-channel install invocation is OS-correct for the resolved
        //     version — the same regression the broken release exposed, now pinned
        //     against a version that came from the (fake) channel rather than a
        //     hard-coded literal.
        let cmd = npm_upgrade_command_invocation(&resolved);
        let prog = cmd.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if cfg!(windows) {
            assert_eq!(prog, "cmd");
            assert_eq!(args, vec!["/C", "npm", "install", "-g", "@nubjs/nub@9.9.9"]);
        } else {
            assert_eq!(prog, "npm");
            assert_eq!(args, vec!["install", "-g", "@nubjs/nub@9.9.9"]);
        }

        // Wrong-checksum verdict: the tampered sidecar must have been rejected, and
        // the bad install left untouched (still the old bytes).
        let err = bad.expect_err("a checksum mismatch must abort the upgrade");
        assert!(
            err.to_string().contains("checksum mismatch"),
            "mismatch must surface as a checksum error, got: {err}"
        );
        assert_eq!(
            std::fs::read(bad_install.join("bin").join("nub")).unwrap(),
            b"OLD\n",
            "a rejected upgrade must leave the existing install untouched"
        );
    }

    #[test]
    fn global_cwd_flag() {
        let cli = parse(&["nub", "--cwd", "/tmp", "run", "dev"]).unwrap();
        assert_eq!(cli.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
    }

    #[test]
    fn global_silent_flag() {
        let cli = parse(&["nub", "--silent", "run", "dev"]).unwrap();
        assert!(cli.silent);
    }

    #[test]
    fn global_verbose_flag_repeatable() {
        let cli = parse(&["nub", "--verbose", "--verbose", "run", "dev"]).unwrap();
        assert_eq!(cli.verbose, 2);
    }

    #[test]
    fn global_color_flag() {
        // `--color` uses the optional-value idiom (require_equals + a
        // default_missing_value of "always"), so a value must be attached with
        // `=`; bare `--color` means "always". Space-separated `--color never`
        // would parse `never` as a positional, not the flag's value.
        let cli = parse(&["nub", "--color=never", "run", "dev"]).unwrap();
        assert!(matches!(cli.color, ColorWhen::Never));
    }

    #[test]
    fn file_execution_no_subcommand() {
        let cli = parse(&["nub", "script.ts"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.args, vec!["script.ts"]);
    }

    #[test]
    fn stdin_passthrough() {
        let cli = parse(&["nub", "-"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.args, vec!["-"]);
    }

    #[test]
    fn file_with_trailing_args() {
        let cli = parse(&["nub", "server.ts", "--port", "3000"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.args, vec!["server.ts", "--port", "3000"]);
    }

    #[test]
    fn top_level_watch_flag() {
        let cli = parse(&["nub", "--watch", "server.ts"]).unwrap();
        assert!(cli.watch);
        assert_eq!(cli.args, vec!["server.ts"]);
    }

    #[test]
    fn version_long_flag() {
        let cli = parse(&["nub", "--version"]).unwrap();
        assert!(cli.version);
    }

    #[test]
    fn version_short_v_mirrors_node() {
        let cli = parse(&["nub", "-v"]).unwrap();
        assert!(cli.version);
    }

    #[test]
    fn version_short_uppercase_v() {
        let cli = parse(&["nub", "-V"]).unwrap();
        assert!(cli.version);
    }

    #[test]
    fn help_flag_short_circuits() {
        let err = parse(&["nub", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn curated_and_verbose_help_diverge() {
        // `-h` is the curated, human-readable page; `--help` is the exhaustive
        // reference. They intentionally differ — the verbose page carries Node
        // flags + env vars the curated page deliberately omits.
        let curated = render_curated_help();
        let verbose = render_verbose_help();
        assert_ne!(curated, verbose);
        // Curated leads with the file runner and points at the verbose form.
        assert!(curated.contains("TypeScript just works"));
        assert!(curated.contains("nub --help"));
        // Verbose carries the Node flag + env-var reference the curated page omits.
        assert!(verbose.contains("NODE_OPTIONS"));
        assert!(verbose.contains("--enable-source-maps"));
        assert!(!curated.contains("NODE_OPTIONS"));
    }

    #[test]
    fn help_routes_every_real_command_word() {
        // The router fix: `nub help <cmd>` / `nub <cmd> -h` must reach a real page
        // for native verbs, the node/pm/agent groups, AND engine verbs (canonical
        // or alias) — the engine verbs (`add`, `why`, `rm`, …) previously fell
        // through to a silent exit.
        for word in [
            "run", "install", "node", "pm", "agent", "add", "rm", "why", "publish",
        ] {
            assert!(
                is_help_routable(word),
                "`{word}` should route to a help page"
            );
        }
        // Unknown words fall through to the top-level page rather than erroring.
        assert!(!is_help_routable("definitely-not-a-command"));
    }

    #[test]
    fn argv0_detection() {
        assert_eq!(Argv0::detect(), Argv0::Nub);
    }

    #[test]
    fn argv0_classify_maps_release_and_dev_names() {
        // Release names dispatch to their mode…
        assert_eq!(Argv0::classify("nub"), Argv0::Nub);
        assert_eq!(Argv0::classify("nubx"), Argv0::Nubx);
        assert_eq!(Argv0::classify("node"), Argv0::Node);
        // …and the `-dev` symlinks `make install-dev` creates map identically
        // (the bug this guards: `nubx-dev` used to fall through to plain Nub).
        assert_eq!(Argv0::classify("nub-dev"), Argv0::Nub);
        assert_eq!(Argv0::classify("nubx-dev"), Argv0::Nubx);
        assert_eq!(Argv0::classify("node-dev"), Argv0::Node);
        // A PM-shim name still classifies as a shim, dev-suffixed or not.
        assert!(matches!(Argv0::classify("pnpm"), Argv0::PmShim(_)));
        assert!(matches!(Argv0::classify("pnpm-dev"), Argv0::PmShim(_)));
    }

    // ── nub pm verbs + PM-verb redirect ─────────────────────────────────

    /// A unique temp project dir under the system temp root (never under $HOME, so
    /// the manifest walk-up can't escape into a stray ancestor `package.json`).
    fn pm_tmpdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-cli-pm-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A committed yarn-classic release fixture: `packageManager: yarn@1.x` plus a
    /// `.yarn/releases/*.cjs` + a `yarnPath:` so [`resolve_target`] short-circuits
    /// to `YarnPath` — the hermetic pinned-PM path (no network, no provisioning).
    /// Returns the project dir and the absolute committed-release path.
    fn yarn_path_fixture(tag: &str) -> (PathBuf, PathBuf) {
        let dir = pm_tmpdir(tag);
        std::fs::write(
            dir.join("package.json"),
            r#"{"packageManager":"yarn@1.22.19"}"#,
        )
        .unwrap();
        let releases = dir.join(".yarn/releases");
        std::fs::create_dir_all(&releases).unwrap();
        let release = releases.join("yarn-1.22.19.cjs");
        std::fs::write(&release, "// yarn classic\n").unwrap();
        std::fs::write(
            dir.join(".yarnrc.yml"),
            "yarnPath: .yarn/releases/yarn-1.22.19.cjs\n",
        )
        .unwrap();
        (dir, release)
    }

    /// Serializes the handful of tests that mutate the process cwd (cwd is
    /// process-global, so they can't run concurrently with each other).
    fn with_cwd<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static CWD_LOCK: Mutex<()> = Mutex::new(());
        let _g = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // `run_pm` (the usual `f` here) drives `engine_brand_preflight`, which
        // writes the process-global engine context (registers `NUB`, sets
        // `read_branded_pnpm_config` from `dir`). Serialize against tests that
        // READ that context so we never flip it mid-read. See
        // `pm_engine::ENGINE_GLOBAL_LOCK`.
        let _ctx = crate::pm_engine::ENGINE_GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = env::current_dir().unwrap();
        env::set_current_dir(dir).unwrap();
        let out = f();
        env::set_current_dir(prev).unwrap();
        out
    }

    #[test]
    fn pm_verbs_and_reserved_verbs_stay_disjoint() {
        // Three verb sets, three dispatch paths: SUBCOMMANDS (clap natives),
        // the engine verb registry (pm_engine::ENGINE_VERBS, family
        // dispatch), and PM_VERBS (redirect-only rump). Any overlap makes a
        // later arm unreachable. `install`/`i`/`ci` graduated from PM_VERBS
        // to native verbs (the embedded aube engine, src/pm_engine/) — they
        // must stay native and out of the registry.
        for verb in [
            "run", "exec", "node", "pm", "watch", "upgrade", "help", "install", "i", "ci",
        ] {
            assert!(
                SUBCOMMANDS.contains(&verb),
                "{verb} must be a reserved native verb"
            );
        }
        for verb in PM_VERBS {
            assert!(
                !SUBCOMMANDS.contains(verb),
                "{verb} is in both PM_VERBS and SUBCOMMANDS — the redirect arm would be unreachable"
            );
            assert!(
                crate::pm_engine::lookup_verb(verb).is_none(),
                "{verb} is in both PM_VERBS and ENGINE_VERBS — the redirect arm would be unreachable"
            );
        }
        for verb in SUBCOMMANDS {
            assert!(
                crate::pm_engine::lookup_verb(verb).is_none(),
                "{verb} is in both SUBCOMMANDS and ENGINE_VERBS — engine dispatch would shadow the native verb"
            );
        }
    }

    #[test]
    fn excluded_engine_verbs_error_with_honest_per_verb_messages() {
        // Dispatching these verbs runs the family `run_verb` path, which calls
        // `engine_brand_preflight` and writes the process-global engine context
        // (registering `NUB`, flipping `read_branded_pnpm_config` from this
        // process's cwd). Serialize with the context-reading tests so it can't
        // race their reads. See `pm_engine::ENGINE_GLOBAL_LOCK`.
        let _guard = crate::pm_engine::ENGINE_GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // The deliberately-excluded verbs must fail loud with a message that
        // names the verb's actual status — not the generic "wired in phase
        // Surface" stub text (everything destined for wiring IS wired; these
        // are exclusions, not backlog). Reasons: install_family module doc.
        for (verb, expect) in [
            ("deploy", "not yet supported"),
            ("recursive", "not supported"),
            ("multi", "not supported"), // recursive alias keeps the message
            ("clean", "not supported"),
            ("purge", "not supported"),
            ("sbom", "not yet supported"),
        ] {
            let spec = crate::pm_engine::lookup_verb(verb)
                .unwrap_or_else(|| panic!("{verb} must be registered"));
            let err = crate::pm_engine::dispatch_verb(spec, verb, &[], "pnpm")
                .expect_err("excluded verbs must error");
            let msg = err.to_string();
            assert!(msg.contains(&format!("nub {verb}")), "{verb}: {msg}");
            assert!(msg.contains(expect), "{verb}: {msg}");
            assert!(
                !msg.contains("wired in phase Surface"),
                "{verb} must not use the generic stub text: {msg}"
            );
        }
        // recursive's remedy points at the per-verb workspace flags.
        let spec = crate::pm_engine::lookup_verb("recursive").unwrap();
        let msg = crate::pm_engine::dispatch_verb(spec, "recursive", &[], "pnpm")
            .expect_err("recursive must error")
            .to_string();
        assert!(msg.contains("-r"), "{msg}");
    }

    // `init` reservation: the registry exclusion is asserted in
    // pm_engine::tests::verb_registry_excludes_reserved_and_tool_identity_verbs
    // and the bareword arm's "nub's own init is coming" answer is covered
    // through the spawned binary in tests/pm_verbs.rs (the arm lives inside
    // run_nub's argv pre-parse, which has no injectable entry point here).

    /// A project dir whose `.npmrc` points the registry at an unroutable port, so
    /// any code path that should NOT reach the network fails fast (connection
    /// refused) instead of touching the real registry — the same trick as
    /// nub-core's `pm::provision` tests.
    fn offline_project(tag: &str, manifest: &str) -> PathBuf {
        let dir = pm_tmpdir(tag);
        std::fs::write(dir.join("package.json"), manifest).unwrap();
        std::fs::write(dir.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
        dir
    }

    /// An ambient `npm_config_registry` outranks the test `.npmrc` and would
    /// re-route the dead-registry assertions to a real registry. Process-global
    /// env is flaky to mutate under the parallel harness, so those legs skip.
    fn ambient_registry_override() -> bool {
        std::env::var("npm_config_registry").is_ok_and(|v| !v.trim().is_empty())
    }

    #[test]
    fn use_plans_lockfile_refusals_before_network_and_a_failed_resolve_writes_nothing() {
        // (a) `use yarn` over a pnpm lockfile no longer refuses at the PLAN
        // stage — the classic-yarn write gate is lifted (the classic writer is
        // proven frozen-accepted by real yarn). The convert path now proceeds
        // to a real resolve; with a dead registry it fails at the *network*
        // (not the old write-gate refusal), and a failed convert writes
        // nothing — proof the gate is gone but the resolve-before-write
        // invariant still holds.
        let before = r#"{"packageManager":"pnpm@9.1.0"}"#;
        if !ambient_registry_override() {
            let dir = offline_project("use-yarn-gate", before);
            std::fs::write(dir.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
            let err = format!(
                "{:#}",
                with_cwd(&dir, || run_pm(&["use".into(), "yarn".into()])).unwrap_err()
            );
            assert!(
                err.contains("cannot reach the registry") && err.contains("127.0.0.1:1"),
                "use yarn must now reach the resolver (gate lifted), failing at the \
                 dead registry, not the old write-gate refusal, got: {err}"
            );
            assert!(
                !err.contains("refuses to write yarn.lock"),
                "the classic-yarn write gate must be lifted, got: {err}"
            );
        }

        // (b) Multiple foreign lockfiles without the target's → the ambiguity
        // refusal, naming the files and the remedy — also pre-network.
        let dir = offline_project("use-ambig", before);
        std::fs::write(dir.join("package-lock.json"), "{}").unwrap();
        std::fs::write(dir.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();
        let err = with_cwd(&dir, || run_pm(&["use".into(), "pnpm".into()]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("package-lock.json")
                && err.contains("yarn.lock")
                && err.contains("nub pm use pnpm"),
            "the multi-lockfile refusal must name the files + remedy, got: {err}"
        );

        // (c) Resolve-before-write: a clean `use` that dies at the (dead)
        // registry leaves the manifest untouched and creates no lockfile.
        if !ambient_registry_override() {
            let dir = offline_project("use-offline", before);
            let err = format!(
                "{:#}",
                with_cwd(&dir, || run_pm(&["use".into(), "pnpm@9.2.0".into()])).unwrap_err()
            );
            assert!(
                err.contains("cannot reach the registry") && err.contains("127.0.0.1:1"),
                "an unresolvable spec must fail with the humanized offline message, got: {err}"
            );
            assert_eq!(
                std::fs::read_to_string(dir.join("package.json")).unwrap(),
                before,
                "a failed resolve must write nothing"
            );
            assert!(
                !dir.join("pnpm-lock.yaml").exists(),
                "a failed use must not create a lockfile"
            );
        }
    }

    #[test]
    fn use_and_update_refuse_berry_pointing_at_the_committed_release_tool() {
        // `use yarn@<2+>` refuses before anything is written — nub can't
        // provision Berry, so a pin it can't honestly hash would be a lie.
        let before = r#"{"packageManager":"yarn@1.22.19"}"#;
        let dir = offline_project("use-berry", before);
        let err = with_cwd(&dir, || run_pm(&["use".into(), "yarn@4.2.2".into()]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Berry") && err.contains("committed release"),
            "use yarn@4.2.2 must refuse with the berry message, got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("package.json")).unwrap(),
            before,
            "a refused berry use must write nothing"
        );

        // With a yarnPath already committed, the refusal must NOT instruct the
        // user to commit one (they did) — it points at `yarn set version`, the
        // tool that manages the committed release.
        let dir = offline_project("berry-has-yarnpath", r#"{"packageManager":"yarn@4.2.2"}"#);
        std::fs::write(
            dir.join(".yarnrc.yml"),
            "yarnPath: .yarn/releases/yarn-4.2.2.cjs\n",
        )
        .unwrap();
        let err = with_cwd(&dir, || run_pm(&["use".into(), "yarn@4.9.0".into()]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("yarn set version") && err.contains("committed release"),
            "the with-yarnPath refusal must point at `yarn set version`, got: {err}"
        );
        assert!(
            !err.contains("Commit a release"),
            "must not instruct committing a release that already exists, got: {err}"
        );

        // `update` on a Berry-pinned project refuses too, pointing at the tool
        // that actually manages committed releases.
        let dir = offline_project("update-berry", r#"{"packageManager":"yarn@4.2.2"}"#);
        let err = with_cwd(&dir, || run_pm(&["update".into()]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("yarn set version"),
            "update on a Berry pin must point at `yarn set version`, got: {err}"
        );
    }

    #[test]
    fn use_args_error_naming_the_form_the_supported_set_and_the_gated_nub() {
        let dir = offline_project("pm-args", r#"{"name":"app"}"#);
        let run = |args: &[&str]| {
            with_cwd(&dir, || {
                run_pm(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            })
            .unwrap_err()
            .to_string()
        };

        assert!(
            run(&["use"]).contains("<pm>[@<spec>]"),
            "bare use names the form"
        );
        assert!(
            run(&["use", "vlt"]).contains("npm, pnpm, yarn, bun, or nub"),
            "an unsupported PM names the use target set"
        );
        // `use nub` is a live target (the full switch) but takes no version:
        // it pins the running nub.
        let err = run(&["use", "nub@1.2.3"]);
        assert!(
            err.contains("takes no version") && err.contains("nub upgrade"),
            "`use nub@<v>` must refuse with the self-version rule: {err}"
        );
        assert!(
            run(&["use", "pnpm@"]).contains("empty version spec"),
            "a trailing @ is named, not treated as latest"
        );
        // The removed verbs name their successor — a clean break, not an alias.
        for verb in ["pin", "switch"] {
            let err = run(&[verb, "pnpm@9.1.0"]);
            assert!(
                err.contains("replaced by `nub pm use"),
                "`{verb}` must name the successor verb, got: {err}"
            );
        }
        let err = run(&["frobnicate"]);
        assert!(
            err.contains("which, use, update (up), cache"),
            "the unknown-verb error names the full verb set, got: {err}"
        );
    }

    #[test]
    fn up_is_an_alias_for_update_and_no_pin_names_the_use_remedy() {
        let dir = offline_project("up-alias", r#"{"name":"app"}"#);
        for verb in ["update", "up"] {
            let err = with_cwd(&dir, || run_pm(&[verb.into()]))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("no package manager is pinned to update")
                    && err.contains("nub pm use"),
                "`{verb}` with no pin must name the state and the remedy, got: {err}"
            );
        }
    }

    #[test]
    fn dev_engines_range_is_updates_spec_only_when_it_names_the_pinned_pm() {
        // The pair: devEngines carries the range update re-resolves within.
        let dir = pm_tmpdir("dev-range");
        std::fs::write(
            dir.join("package.json"),
            r#"{"packageManager":"pnpm@9.1.0+sha512.aa","devEngines":{"packageManager":{"name":"pnpm","version":"^9.1.0","onFail":"download"}}}"#,
        )
        .unwrap();
        assert_eq!(dev_engines_range(&dir, "pnpm").as_deref(), Some("^9.1.0"));
        assert_eq!(
            dev_engines_range(&dir, "yarn"),
            None,
            "a devEngines entry naming a different PM is not the pin's range"
        );

        // Legacy single-field pin (no devEngines) → no range → update uses latest.
        let dir = pm_tmpdir("dev-range-legacy");
        std::fs::write(
            dir.join("package.json"),
            r#"{"packageManager":"pnpm@9.1.0"}"#,
        )
        .unwrap();
        assert_eq!(dev_engines_range(&dir, "pnpm"), None);

        // From a workspace member the range is read at the root — the same file
        // resolve_pin reads and write_declared_pm writes.
        let root = pm_tmpdir("dev-range-ws");
        std::fs::write(
            root.join("package.json"),
            r#"{"workspaces":["packages/*"],"devEngines":{"packageManager":{"name":"pnpm","version":"^9"}}}"#,
        )
        .unwrap();
        let member = root.join("packages").join("app");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(member.join("package.json"), r#"{"name":"@mono/app"}"#).unwrap();
        assert_eq!(dev_engines_range(&member, "pnpm").as_deref(), Some("^9"));
    }

    #[test]
    fn cache_lists_versions_and_clear_removes_only_the_pm_dir() {
        // Seed a fake cache: <root>/pm/pnpm/{9.1.0,10.0.0} + <root>/node/22.0.0.
        let root = pm_tmpdir("cache");
        let pm = root.join("pm");
        for v in ["10.0.0", "9.1.0"] {
            std::fs::create_dir_all(pm.join("pnpm").join(v).join("package")).unwrap();
        }
        std::fs::create_dir_all(pm.join("pnpm").join(".tmp-9.9.9-123")).unwrap(); // work dir
        let node = root.join("node/22.0.0");
        std::fs::create_dir_all(&node).unwrap();

        // Listing is sorted `<pm>@<version>`, work dirs excluded.
        assert_eq!(list_pm_cache(&pm), vec!["pnpm@10.0.0", "pnpm@9.1.0"]);

        // Clear removes the pm dir; the sibling node/ dir survives untouched.
        std::fs::remove_dir_all(&pm).unwrap();
        assert!(!pm.exists(), "the pm cache dir is gone after clear");
        assert!(node.exists(), "the sibling node/ store must be untouched");
    }

    #[test]
    fn which_honors_a_declared_unprovisionable_pm() {
        // A `packageManager: "bun@x"` pin is a genuine identity nub doesn't
        // provision — `resolve_target_with_source` returns None for it, so the
        // old `which` path mislabeled the project "no PM pinned". The install
        // path already honors the declared identity; `which` must agree. It
        // reports bun (exit 0), not the no-pin error.
        let dir = pm_tmpdir("which-bun");
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"app","packageManager":"bun@1.2.20"}"#,
        )
        .unwrap();
        // The identity probe sees bun; the provisioning resolver does not — the
        // exact gap the `which` branch bridges.
        let id = nub_core::pm::resolve::project_pm_identity(&dir).expect("bun identity");
        assert_eq!(id.name, "bun");
        assert!(!id.berry);
        assert!(
            nub_core::pm::resolve::resolve_target_with_source(&dir).is_none(),
            "bun is unprovisionable, so the target resolver yields None"
        );
        let code = with_cwd(&dir, || run_pm(&["which".into()])).expect("which must not error");
        assert_eq!(
            code, 0,
            "a declared bun pin reports bun (exit 0), not an error"
        );
    }

    #[test]
    fn which_with_no_pin_errors_naming_the_remedy() {
        let dir = pm_tmpdir("which-none");
        std::fs::write(dir.join("package.json"), r#"{"name":"app"}"#).unwrap();
        let err = with_cwd(&dir, || run_pm(&["which".into()]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no package manager is pinned") && err.contains("nub pm use"),
            "which-no-pin must name the unpinned state and the remedy, got: {err}"
        );
    }

    #[test]
    fn check_manifest_json_flags_malformed_but_passes_valid_and_missing() {
        // Malformed → a coded JSON-parse error naming the file (not "unpinned").
        let bad = pm_tmpdir("manifest-bad");
        std::fs::write(bad.join("package.json"), "{ \"name\": ").unwrap();
        let err = check_manifest_json(&bad).unwrap_err().to_string();
        assert!(
            err.contains("package.json is not valid JSON") && err.contains("package.json"),
            "malformed must name the parse failure + path, got: {err}"
        );
        assert!(
            err.contains(ERR_NUB_MANIFEST_PARSE),
            "malformed must carry the branded parse code, got: {err}"
        );

        // Valid manifest and a dir with no manifest at all are both Ok — a missing
        // package.json is genuinely unpinned, which the caller's own context covers.
        let good = pm_tmpdir("manifest-good");
        std::fs::write(good.join("package.json"), r#"{"name":"app"}"#).unwrap();
        assert!(check_manifest_json(&good).is_ok());
        assert!(check_manifest_json(&pm_tmpdir("manifest-none")).is_ok());

        // A package.json that EXISTS but can't be read (EACCES) must surface a
        // coded permission error — NOT get swallowed into "no package.json found"
        // by the Option-returning readers downstream. Unix-only: Windows ACLs
        // don't map onto a chmod, and PermissionDenied isn't reachable via mode.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let denied = pm_tmpdir("manifest-eacces");
            let pkg = denied.join("package.json");
            std::fs::write(&pkg, r#"{"name":"app"}"#).unwrap();
            std::fs::set_permissions(&pkg, std::fs::Permissions::from_mode(0o000)).unwrap();
            let err = check_manifest_json(&denied).unwrap_err().to_string();
            // Restore before asserting so the tempdir cleanup can remove it.
            std::fs::set_permissions(&pkg, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(
                err.contains(ERR_NUB_MANIFEST_UNREADABLE),
                "EACCES must carry the branded unreadable code, got: {err}"
            );
            assert!(
                err.contains("permissions") || err.contains("ownership"),
                "EACCES must offer an actionable remedy, got: {err}"
            );
        }
    }

    #[test]
    fn humanize_transport_error_collapses_only_the_network_shape() {
        // A reqwest-shaped transport chain → one sentence naming the registry; the
        // deep DNS/connect internals are dropped (and only restored under verbose).
        let chain = anyhow::anyhow!("dns error: failed to lookup address information")
            .context("error sending request for url (https://registry.npmjs.org/pnpm)")
            .context("fetching packument https://registry.npmjs.org/pnpm");
        let humanized = humanize_transport_error(chain, "https://registry.npmjs.org").to_string();
        assert!(
            humanized.contains("cannot reach the registry https://registry.npmjs.org")
                && !humanized.contains("dns error"),
            "the transport stack must collapse to one registry-named sentence, got: {humanized}"
        );

        // A non-transport error (a real 404 / version miss) is actionable already —
        // pass it through untouched, never masked as a connectivity problem.
        let not_transport = anyhow::anyhow!("no version satisfies \"99.0.0\"");
        let passed = humanize_transport_error(not_transport, "https://registry.npmjs.org");
        assert_eq!(
            passed.to_string(),
            "no version satisfies \"99.0.0\"",
            "a specific, actionable error must not be rewritten as offline"
        );
    }

    #[test]
    fn resolution_source_reports_the_chain_winner_not_just_pin_files() {
        // `nub node which`/status must report the SAME source the run path
        // resolves with: devEngines.runtime (#1) outranks the .node-version (#2)
        // beside it, so the report must name the field, not the file.
        let dir = pm_tmpdir("res-src");
        std::fs::write(
            dir.join("package.json"),
            r#"{"devEngines":{"runtime":{"name":"node","version":">=22"}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join(".node-version"), "20.11.0\n").unwrap();
        let source = resolution_source(&dir);
        assert!(
            source.contains("devEngines.runtime") && source.contains(">=22"),
            "the governing source must be reported with its raw spec, got: {source}"
        );

        // No source at all → PATH, named as such.
        let bare = pm_tmpdir("res-src-bare");
        std::fs::write(bare.join("package.json"), r#"{"name":"app"}"#).unwrap();
        assert_eq!(resolution_source(&bare), "node on PATH");
    }

    #[test]
    fn which_yarn_path_prints_abs_path_and_yarnrc_provenance() {
        // A committed Berry release short-circuits provisioning: `which` prints the
        // absolute release path (stdout) and ".yarnrc.yml yarnPath" provenance
        // (stderr). Asserted at the resolution seam (the stdout/stderr split is the
        // same as `nub node which`, exercised there).
        let (dir, release) = yarn_path_fixture("which-yarn");
        let target = nub_core::pm::resolve::resolve_target(&dir).unwrap();
        match target {
            nub_core::pm::resolve::PmTarget::YarnPath(p) => {
                assert_eq!(p, release, "which resolves to the committed release path");
                assert!(p.is_absolute(), "the printed path must be absolute");
            }
            other => panic!("expected YarnPath, got {other:?}"),
        }
    }

    // ── PM shim: plan / provenance / dynamic default ─────────────────────

    #[test]
    fn pinned_exec_plan_carries_the_compile_cache_env() {
        // The PM bundle's parse+compile dominates its startup; the exec plan must
        // point NODE_COMPILE_CACHE at a nub-owned dir (corepack does the
        // equivalent in-process and was measurably faster until this landed).
        // Skipped when the ambient env already sets it — the user's value wins
        // and mutating process env under the parallel harness is the flaky thing
        // this suite deliberately avoids.
        if std::env::var_os("NODE_COMPILE_CACHE").is_some() {
            return;
        }
        let (dir, _release) = yarn_path_fixture("ccache");
        let plan = shim_plan(
            nub_core::pm::shim::ShimName::Yarn,
            &["--version".to_string()],
            &dir,
        )
        .unwrap();
        let ShimPlan::Exec { env, .. } = plan else {
            panic!("a yarnPath project execs the committed release");
        };
        let (_, v) = env
            .iter()
            .find(|(k, _)| k == "NODE_COMPILE_CACHE")
            .expect("the exec plan sets NODE_COMPILE_CACHE for the PM bundle");
        assert!(
            v.ends_with("v8-compile-cache"),
            "the cache dir is nub-owned, got {v}"
        );
    }

    #[test]
    fn shim_plan_refuses_a_mismatched_pm_naming_pin_provenance_and_paste() {
        use nub_core::pm::shim::ShimName;

        // packageManager-pinned pnpm project: bare `npm install react` refuses
        // before any network (the dead registry would yield a fetch error, not
        // a Refuse plan), naming the pin, the field, the paste, and the escape.
        let dir = offline_project("shim-refuse", r#"{"packageManager":"pnpm@9.1.0"}"#);
        let args = vec!["install".to_string(), "react".to_string()];
        match shim_plan(ShimName::Npm, &args, &dir).unwrap() {
            ShimPlan::Refuse { message } => {
                for needle in [
                    "pnpm",
                    "package.json#packageManager",
                    "pnpm install react",
                    "nub pm unshim",
                ] {
                    assert!(
                        message.contains(needle),
                        "refusal must contain {needle:?}, got:\n{message}"
                    );
                }
            }
            other => panic!("a mismatched npm must refuse, got {other:?}"),
        }

        // A flags-only invocation (`npm --version`) still refuses, but with NO
        // "run instead" line: there is no verb to redirect, and echoing argv
        // back produced the nonsense "run instead: pnpm --version".
        match shim_plan(ShimName::Npm, &["--version".to_string()], &dir).unwrap() {
            ShimPlan::Refuse { message } => {
                assert!(
                    message.contains("pins pnpm") && message.contains("nub pm unshim"),
                    "the flags-only refusal keeps the why + the escape, got:\n{message}"
                );
                assert!(
                    !message.contains("run instead"),
                    "a flags-only invocation must drop the redirect line, got:\n{message}"
                );
            }
            other => panic!("a flags-only mismatched npm still refuses, got {other:?}"),
        }

        // A committed-yarnPath project refuses invoked pnpm naming the yarnrc
        // provenance (decision 2: yarnPath projects, wrong name → refuse).
        let (dir, _release) = yarn_path_fixture("shim-refuse-yarnpath");
        match shim_plan(ShimName::Pnpm, &["install".to_string()], &dir).unwrap() {
            ShimPlan::Refuse { message } => assert!(
                message.contains("yarn") && message.contains(".yarnrc.yml#yarnPath"),
                "the yarnPath refusal must name yarn + the yarnrc provenance, got:\n{message}"
            ),
            other => panic!("pnpm in a yarnPath project must refuse, got {other:?}"),
        }
    }

    #[test]
    fn shim_version_line_is_the_documented_via_nub_shim_notice() {
        use nub_core::pm::Pm;
        // Exact wording the docs reproduce verbatim; yarn/yarnberry both surface
        // as `yarn` (the bare PM name), matching `Pm`'s Display.
        assert_eq!(
            shim_version_line(Pm::Pnpm, "11.0.1"),
            "pnpm@11.0.1 (via nub shim)"
        );
        assert_eq!(
            shim_version_line(Pm::YarnBerry, "4.9.1"),
            "yarn@4.9.1 (via nub shim)"
        );
    }

    #[test]
    fn shim_pin_state_reports_the_field_that_carried_the_pin() {
        use nub_core::pm::Pm;
        use nub_core::pm::resolve::resolve_target;
        use nub_core::pm::shim::{PinProvenance, PinState};

        let state = |dir: &Path| shim_pin_state(dir, resolve_target(dir).as_ref());

        let dir = pm_tmpdir("pinstate-field");
        std::fs::write(
            dir.join("package.json"),
            r#"{"packageManager":"pnpm@9.1.0"}"#,
        )
        .unwrap();
        assert_eq!(
            state(&dir),
            PinState::Pinned {
                pm: Pm::Pnpm,
                provenance: PinProvenance::PackageManagerField
            }
        );

        let dir = pm_tmpdir("pinstate-dev");
        std::fs::write(
            dir.join("package.json"),
            r#"{"devEngines":{"packageManager":{"name":"pnpm","version":"9.1.0"}}}"#,
        )
        .unwrap();
        assert_eq!(
            state(&dir),
            PinState::Pinned {
                pm: Pm::Pnpm,
                provenance: PinProvenance::DevEngines
            }
        );

        let (dir, _release) = yarn_path_fixture("pinstate-yarnpath");
        assert_eq!(
            state(&dir),
            PinState::Pinned {
                pm: Pm::YarnBerry,
                provenance: PinProvenance::YarnPath
            },
            "a committed yarnPath is a Berry pin with yarnrc provenance"
        );

        // `pm use nub` writes `packageManager: "nub@…"`. `resolve_target`
        // rejects it (nub isn't a provisionable Pm), so it would arrive as
        // `None`/Unpinned and a foreign shim would provision a competing PM —
        // the bug. It must resolve to NubPinned instead.
        let dir = pm_tmpdir("pinstate-nub");
        std::fs::write(
            dir.join("package.json"),
            r#"{"packageManager":"nub@0.0.31"}"#,
        )
        .unwrap();
        assert_eq!(
            state(&dir),
            PinState::NubPinned {
                provenance: PinProvenance::PackageManagerField
            },
            "a nub@ self-pin is NubPinned, not Unpinned — a foreign shim must \
             refuse to nub, never provision a competing PM"
        );

        let dir = pm_tmpdir("pinstate-none");
        std::fs::write(dir.join("package.json"), r#"{"name":"app"}"#).unwrap();
        assert_eq!(state(&dir), PinState::Unpinned);
    }

    #[test]
    fn dynamic_default_spec_follows_the_lockfile_and_errors_on_bun() {
        use nub_core::pm::Pm;

        // Matching lockfile → the implied family (pnpm-lock 6.0 → pnpm 8).
        let dir = pm_tmpdir("dyndef-pnpm");
        std::fs::write(dir.join("pnpm-lock.yaml"), "lockfileVersion: '6.0'\n").unwrap();
        let (spec, why) = dynamic_default_spec(Pm::Pnpm, &dir).unwrap();
        assert_eq!(spec, "8", "lockfileVersion 6.0 implies the pnpm 8 family");
        assert!(
            why.contains("pnpm 8"),
            "the announcement names the family: {why}"
        );

        // A DIFFERENT PM's lockfile → the invoked PM's latest, never the
        // lockfile owner's (decision 3).
        let (spec, why) = dynamic_default_spec(Pm::Npm, &dir).unwrap();
        assert_eq!(spec, "latest");
        assert!(
            why.contains("pnpm"),
            "the why names whose lockfile it actually is: {why}"
        );

        // No lockfile at all → latest.
        let bare = pm_tmpdir("dyndef-bare");
        assert_eq!(dynamic_default_spec(Pm::Yarn, &bare).unwrap().0, "latest");

        // A bun lockfile errors naming bun — nub never provisions bun.
        let dir = pm_tmpdir("dyndef-bun");
        std::fs::write(dir.join("bun.lockb"), b"\x00bun\x00").unwrap();
        let err = dynamic_default_spec(Pm::Pnpm, &dir)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("bun") && err.contains("never provisions"),
            "the bun lockfile error must name bun, got: {err}"
        );
    }

    /// Read the declaration back out of a manifest: `(packageManager value,
    /// devEngines.packageManager value — Null when absent)`. Shared by the
    /// network e2e tests.
    fn read_declaration(dir: &Path) -> (String, serde_json::Value) {
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap())
                .unwrap();
        (
            m["packageManager"].as_str().unwrap_or_default().to_string(),
            m["devEngines"]["packageManager"].clone(),
        )
    }

    /// Real-network e2e for `nub pm use`: declare an exact pnpm, and confirm the
    /// declaration lands with an HONEST hash — a fresh store provisioning from the
    /// written pin must pass the fail-closed pin-hash gate (`verify_pin_hash`) —
    /// and that devEngines is NOT created (Axiom 3). Provisions into the real user
    /// cache (run_pm has no store override), like a real use would.
    /// `#[ignore]` — downloads real pnpm tarballs.
    ///   cargo test -p nub-cli --bin nub -- --ignored use_writes
    #[test]
    #[ignore = "network: provisions real pnpm@10.0.0 and verifies the written pin hash"]
    fn use_writes_the_verified_declaration_end_to_end() {
        let dir = pm_tmpdir("use-net");
        std::fs::write(dir.join("package.json"), r#"{"name":"app"}"#).unwrap();
        let code = with_cwd(&dir, || run_pm(&["use".into(), "pnpm@10.0.0".into()])).unwrap();
        assert_eq!(code, 0);

        let (pkg_mgr, dev) = read_declaration(&dir);
        let hex = pkg_mgr
            .strip_prefix("pnpm@10.0.0+sha512.")
            .unwrap_or_else(|| panic!("packageManager must be exact+sha512, got {pkg_mgr}"));
        assert!(
            hex.len() == 128 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "the suffix must be a full sha512 hex digest, got {hex}"
        );
        assert_eq!(
            dev,
            serde_json::Value::Null,
            "use must never create devEngines"
        );

        // The committed hash is the true artifact digest: a FRESH store must
        // provision from this pin (downloading + verifying against the hash).
        // A dishonest hash would fail closed here.
        let fresh = pm_tmpdir("use-net-fresh-store");
        let pin = nub_core::pm::resolve::resolve_pin(&dir).expect("the pin just written");
        nub_core::pm::provision::provision_pm(&pin, &fresh, &dir, None)
            .expect("a fresh store must verify and install from the written pin hash");
        let _ = std::fs::remove_dir_all(&fresh);
    }

    /// Real-network e2e for cross-PM `nub pm use`: spec defaults to latest, the
    /// lockfile converts to the target's format with the source removed, and
    /// devEngines.packageManager is rewritten beside the pin ({name, ^range,
    /// onFail:warn}). `#[ignore]` — downloads real npm tarballs.
    ///   cargo test -p nub-cli --bin nub -- --ignored use_defaults
    #[test]
    #[ignore = "network: moves a pnpm project to npm@latest (real provision + conversion)"]
    fn use_defaults_to_latest_crosses_pm_and_migrates_the_lockfile() {
        let dir = pm_tmpdir("use-cross-net");
        std::fs::write(
            dir.join("package.json"),
            r#"{"packageManager":"pnpm@9.1.0","devEngines":{"packageManager":{"name":"pnpm","version":"^9"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: true\n  excludeLinksFromLockfile: false\n",
        )
        .unwrap();
        let code = with_cwd(&dir, || run_pm(&["use".into(), "npm".into()])).unwrap();
        assert_eq!(code, 0);

        let (pkg_mgr, dev) = read_declaration(&dir);
        assert!(
            pkg_mgr.starts_with("npm@") && pkg_mgr.contains("+sha512."),
            "use must rewrite the pin cross-PM with the resolved exact + hash, got {pkg_mgr}"
        );
        let exact = pkg_mgr
            .trim_start_matches("npm@")
            .split('+')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            dev,
            serde_json::json!({"name": "npm", "version": format!("^{exact}"), "onFail": "warn"}),
            "devEngines must be rewritten beside the pin"
        );
        assert!(
            dir.join("package-lock.json").is_file(),
            "the lockfile must convert to npm's format"
        );
        assert!(
            !dir.join("pnpm-lock.yaml").exists(),
            "the migrated source lockfile must be removed"
        );
    }

    /// Real-network e2e for `nub pm update`: with a devEngines range present,
    /// update floats within it (^9 stays on 9.x — never a silent cross-major jump
    /// to 10/11), rewrites the hash, and re-writes devEngines beside the pin
    /// (the caret of the new exact). `#[ignore]` — hits the registry.
    ///   cargo test -p nub-cli --bin nub -- --ignored update_floats
    #[test]
    #[ignore = "network: re-resolves pnpm@^9.0.0 from the registry (real provision)"]
    fn update_floats_within_the_dev_engines_range() {
        let dir = pm_tmpdir("update-net");
        std::fs::write(
            dir.join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0","devEngines":{"packageManager":{"name":"pnpm","version":"^9.0.0","onFail":"download"}}}"#,
        )
        .unwrap();
        let code = with_cwd(&dir, || run_pm(&["update".into()])).unwrap();
        assert_eq!(code, 0);

        let (pkg_mgr, dev) = read_declaration(&dir);
        assert!(
            pkg_mgr.starts_with("pnpm@9.") && pkg_mgr.contains("+sha512."),
            "update must stay within the ^9 range and carry a fresh hash, got {pkg_mgr}"
        );
        assert_ne!(
            pkg_mgr, "pnpm@9.0.0",
            "the pin must advance past the seed (newer 9.x releases exist)"
        );
        assert!(
            dev["version"]
                .as_str()
                .unwrap_or_default()
                .starts_with("^9."),
            "the nub-shaped devEngines range is rewritten consistent with the new exact"
        );
    }

    /// Real-network e2e for the hand-written-range half of `nub pm update`: a
    /// devEngines range the user wrote themselves (">=9 <10" — not nub's
    /// ^x.y.z shape) constrains the resolve AND survives verbatim, while
    /// `packageManager` bumps within it.
    ///   cargo test -p nub-cli --bin nub -- --ignored update_preserves
    #[test]
    #[ignore = "network: re-resolves pnpm@'>=9 <10' from the registry (real provision)"]
    fn update_preserves_a_hand_written_dev_engines_range() {
        let dir = pm_tmpdir("update-keep-range");
        std::fs::write(
            dir.join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0","devEngines":{"packageManager":{"name":"pnpm","version":">=9 <10","onFail":"error"}}}"#,
        )
        .unwrap();
        let code = with_cwd(&dir, || run_pm(&["update".into()])).unwrap();
        assert_eq!(code, 0);

        let (pkg_mgr, dev) = read_declaration(&dir);
        assert!(
            pkg_mgr.starts_with("pnpm@9.") && pkg_mgr.contains("+sha512."),
            "the record must bump within the hand-written range, got {pkg_mgr}"
        );
        assert_ne!(pkg_mgr, "pnpm@9.0.0", "newer 9.x releases exist");
        assert_eq!(
            dev["version"].as_str(),
            Some(">=9 <10"),
            "the hand-written range is the user's intent and stays verbatim"
        );
        assert_eq!(
            dev["onFail"].as_str(),
            Some("error"),
            "the kept devEngines entry is untouched — onFail included"
        );
    }

    // ── nubx argv0 dispatch ─────────────────────────────────────────

    /// Parse a `nubx <args...>` invocation exactly as `run_nubx` does: prepend
    /// the `nubx` subcommand, split off the verbatim post-bin suffix, clap-parse
    /// the prefix, then fold the suffix back into `args`. Returns the settled
    /// `Command::Nubx { .. }` for assertions.
    fn parse_nubx(args: &[&str]) -> Command {
        let mut rest = vec!["nubx".to_string()];
        rest.extend(args.iter().map(|s| s.to_string()));
        let (prefix, suffix) = split_subcommand_argv(rest);
        let cmd = Cli::parse_from(std::iter::once("nub".to_string()).chain(prefix)).command;
        match cmd {
            Some(mut nubx @ Command::Nubx { .. }) => {
                if let Command::Nubx { args, .. } = &mut nubx {
                    args.extend(suffix);
                }
                nubx
            }
            other => panic!("expected Command::Nubx, got {other:?}"),
        }
    }

    #[test]
    fn nubx_preserves_node_flag_and_forwards_post_bin_args() {
        // `--node` before the bin is nubx's (→ compat mode); `--run` after the bin
        // reaches the tool verbatim (the three-position rule). This is the contract
        // `run_nubx` relies on when it routes through the `Nubx` grammar.
        let Command::Nubx {
            node, bin, args, ..
        } = parse_nubx(&["--node", "vitest", "--run"])
        else {
            unreachable!()
        };
        assert!(node, "a leading --node sets compat mode");
        assert_eq!(bin, "vitest", "the first non-flag token is the bin");
        assert_eq!(
            args,
            vec!["--run".to_string()],
            "post-bin args forward verbatim"
        );
    }

    #[test]
    fn nubx_package_flag_binds_spec_and_keeps_bin() {
        // `nubx -p @tanstack/cli tanstack --help`: `@tanstack/cli` is the package
        // to fetch, `tanstack` the bin to run from it, `--help` the tool's.
        let Command::Nubx {
            package, bin, args, ..
        } = parse_nubx(&["-p", "@tanstack/cli", "tanstack", "--help"])
        else {
            unreachable!()
        };
        assert_eq!(
            package,
            vec!["@tanstack/cli".to_string()],
            "-p binds the spec"
        );
        assert_eq!(bin, "tanstack", "the positional is the bin to run");
        assert_eq!(args, vec!["--help".to_string()], "post-bin args forward");
    }

    #[test]
    fn nubx_refuse_fetch_and_quiet_flags_parse() {
        let Command::Nubx {
            no_install,
            quiet,
            bin,
            ..
        } = parse_nubx(&["--no-install", "-q", "cowsay"])
        else {
            unreachable!()
        };
        assert!(no_install, "--no-install parses");
        assert!(quiet, "-q parses");
        assert_eq!(bin, "cowsay");

        let Command::Nubx { no_fetch, .. } = parse_nubx(&["--no", "cowsay"]) else {
            unreachable!()
        };
        assert!(no_fetch, "--no is its own refuse-fetch flag");
    }

    #[test]
    fn nubx_parity_noops_parse_without_consuming_the_bin() {
        // `-y`/`--yes` (no-op) and `--ignore-existing` (warn+ignore) must not be
        // mistaken for the bin positional.
        let Command::Nubx {
            yes,
            ignore_existing,
            bin,
            ..
        } = parse_nubx(&["-y", "--ignore-existing", "create-vite"])
        else {
            unreachable!()
        };
        assert!(yes, "-y parses");
        assert!(ignore_existing, "--ignore-existing parses");
        assert_eq!(bin, "create-vite", "neither flag steals the bin");
    }

    #[test]
    fn nubx_workspace_flags_survive_the_new_grammar() {
        // The fan-out flags nubx inherited from `nub exec` must still parse.
        let Command::Nubx {
            recursive,
            filter,
            parallel,
            bin,
            ..
        } = parse_nubx(&["-r", "-F", "@org/api", "--parallel", "tsc"])
        else {
            unreachable!()
        };
        assert!(recursive, "-r preserved");
        assert!(parallel, "--parallel preserved");
        assert_eq!(
            filter,
            vec!["@org/api".to_string()],
            "-F preserved + value-bound"
        );
        assert_eq!(bin, "tsc");
    }

    #[test]
    fn nub_exec_grammar_is_unaffected_by_nubx_flags() {
        // nubx's npx flags live on the `Nubx` variant only — `nub exec` must reject
        // them (its grammar never grew `-p`/`--no-install`/`-q`).
        assert!(
            parse(&["nub", "exec", "--no-install", "tsc"]).is_err(),
            "nub exec must not accept nubx's --no-install"
        );
        // And a plain `nub exec` still parses to Exec, not Nubx.
        let cli = parse(&["nub", "exec", "vitest"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Exec { ref bin, .. }) if bin == "vitest"));
    }

    // ── .bin launcher resolution (the node-vs-shim decision) ─────────────

    #[test]
    fn is_node_bin_classifies_by_extension_and_shebang() {
        // JS extensions run under node; native/Windows-shim extensions never do.
        // (The decision is pure extension/shebang inspection — platform-shared, so a
        // regression in either branch is caught on every CI OS.)
        let dir = pm_tmpdir("is-node-bin");
        let by_ext = |name: &str| {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            is_node_bin(&p)
        };
        assert!(by_ext("a.js"), ".js runs under node");
        assert!(by_ext("a.cjs"), ".cjs runs under node");
        assert!(by_ext("a.mjs"), ".mjs runs under node");
        assert!(!by_ext("a.cmd"), ".cmd is a Windows shim, never node");
        assert!(!by_ext("a.exe"), ".exe is native, never node");
        assert!(!by_ext("a.ps1"), ".ps1 is a PowerShell shim, never node");

        // Extensionless: the shebang decides (the typical Unix .bin symlink).
        let node_shim = dir.join("node-shim");
        std::fs::write(&node_shim, b"#!/usr/bin/env node\nconsole.log(1)\n").unwrap();
        assert!(
            is_node_bin(&node_shim),
            "a `#!…node` shebang runs under node"
        );
        let sh_shim = dir.join("sh-shim");
        std::fs::write(&sh_shim, b"#!/bin/sh\necho hi\n").unwrap();
        assert!(!is_node_bin(&sh_shim), "a non-node shebang does not");
    }

    /// The Windows launcher must route `.cmd`/`.bat` through `cmd /C` and `.ps1`
    /// through PowerShell (neither is launchable by a bare `CreateProcess`), with the
    /// user args appended after the script — a regression here (wrong flag, dropped
    /// args) would silently break every `nubx`-launched Windows shim. Asserted by
    /// inspecting the constructed `Command` (no spawn), so it's fast and hermetic.
    #[cfg(windows)]
    #[test]
    fn bin_launcher_routes_windows_shims_through_their_interpreter() {
        use std::ffi::OsStr;
        let argv = vec!["--flag".to_string(), "x".to_string()];

        let cmd = bin_launcher(Path::new(r"C:\tools\tool.cmd"), &argv);
        assert_eq!(cmd.get_program(), OsStr::new("cmd"));
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            [
                OsStr::new("/C"),
                OsStr::new(r"C:\tools\tool.cmd"),
                OsStr::new("--flag"),
                OsStr::new("x"),
            ],
            "a .cmd runs as `cmd /C <path> <args...>`"
        );

        let ps = bin_launcher(Path::new(r"C:\tools\tool.ps1"), &argv);
        assert_eq!(ps.get_program(), OsStr::new("powershell"));
        let ps_args: Vec<&OsStr> = ps.get_args().collect();
        assert!(
            ps_args.contains(&OsStr::new("-File"))
                && ps_args.last() == Some(&OsStr::new("x"))
                && ps_args.contains(&OsStr::new("Bypass")),
            "a .ps1 runs via powershell -NoProfile -ExecutionPolicy Bypass -File <path> <args>, got {ps_args:?}"
        );
    }
}
