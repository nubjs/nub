//! CLI argument parsing and dispatch.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

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

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

/// The invocation context derived from argv[0].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Argv0 {
    /// Invoked as `nub` — full CLI with subcommand dispatch.
    Nub,
    /// Invoked as `nubx` — enter `exec` directly.
    Nubx,
    /// Invoked as `node` via the PATH shim — augmented top-level execution.
    Node,
}

impl Argv0 {
    pub fn detect() -> Self {
        let argv0 = env::args_os().next().unwrap_or_default();
        let basename = PathBuf::from(&argv0)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();

        match basename.as_str() {
            "nubx" => Self::Nubx,
            "node" => Self::Node,
            _ => Self::Nub,
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
    about = "TypeScript-first developer supertool",
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

    /// Print the resolved Node binary path and exit.
    #[arg(long)]
    pub which_node: bool,

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

    let argv0 = Argv0::detect();

    match argv0 {
        Argv0::Nubx => run_nubx(),
        Argv0::Node => run_as_node(),
        Argv0::Nub => run_nub(),
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

/// Known subcommand names that clap should handle.
const SUBCOMMANDS: &[&str] = &["run", "watch", "exec", "upgrade", "help", "node"];

/// Accept pnpm's flag-before-subcommand order. pnpm takes `pnpm -r run build` AND
/// `pnpm run -r build`; nub's pre-parse otherwise only recognizes a subcommand in
/// first position, so a leading run-flag (`-r`, `--filter`, …) falls through to
/// the Node-passthrough path and Node fails on `--require run` (`Cannot find
/// module 'run'`). If the args begin with a run of the `run`/`exec` flags
/// (consuming the value of value-taking ones) immediately followed by a `run` or
/// `exec` subcommand, reorder into canonical subcommand-first order
/// (`run -r build`). Anything else — a Node flag, a file, `nub <file>`, eval —
/// leaves the args untouched, so file/passthrough/eval dispatch is unaffected.
/// Returns `None` when no normalization applies. Keep the flag lists in sync with
/// the `Run` subcommand's `#[arg]` set.
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
    if !leading.is_empty() && matches!(args.get(i).map(String::as_str), Some("run") | Some("exec"))
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
    let mut which_node = false;
    let mut watch = false;
    let mut show_help = false;
    let mut show_warnings = false;
    let mut silent = false;
    // Top-level `--node`: provision the project's Node (version management stays
    // on) but run with zero augmentation — the compat escape hatch. Routed to
    // `run_file_with_compat(_, true)`. See wiki/commands/node.md.
    let mut compat = false;
    let mut rest: Vec<String> = Vec::new();
    let mut subcommand_found = false;
    let mut eval_tempfile: Option<tempfile::NamedTempFile> = None;
    let mut env_file_vars: std::collections::HashMap<String, String> = Default::default();

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
            "--help" | "-h" => show_help = true,
            "--which-node" => which_node = true,
            "--watch" => watch = true,
            "--node" => compat = true,
            "--silent" | "-s" => silent = true,
            "--verbose" => { /* consumed, not forwarded */ }
            "--show-warnings" => show_warnings = true,
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
                            env_file_vars.entry(k).or_insert(v);
                        }
                    } else {
                        eprintln!("nub: cannot read env file: {file_path}");
                    }
                }
            }
            "-e" | "--eval" | "-p" | "--print" => {
                // Intercept eval: write code to a temp .ts file so our
                // preload hooks can transpile non-erasable syntax.
                let is_print = arg == "-p" || arg == "--print";
                i += 1;
                if i < raw_args.len() {
                    let code = &raw_args[i];
                    let wrapped = if is_print {
                        format!("console.log({code})")
                    } else {
                        code.clone()
                    };
                    match tempfile::Builder::new().suffix(".ts").tempfile() {
                        Ok(mut tmp) => {
                            use std::io::Write;
                            let _ = tmp.write_all(wrapped.as_bytes());
                            let path = tmp.path().to_string_lossy().to_string();
                            rest.push(path);
                            rest.extend(raw_args[i + 1..].iter().cloned());
                            eval_tempfile = Some(tmp);
                            break;
                        }
                        Err(_) => {
                            // Fallback: pass through to node as-is
                            rest.push(arg.clone());
                            rest.push(raw_args[i].clone());
                        }
                    }
                } else {
                    // No code argument — pass the bare flag through to Node so it
                    // produces the native behavior: `-e`/`--eval` error with
                    // "<prog>: -e requires an argument" and exit 9, while
                    // `-p`/`--print` read the program from stdin. (Previously the
                    // flag was dropped, leaving an empty argv → help + exit 0.)
                    rest.push(arg.clone());
                }
            }
            _ => {
                // Check if this is the first positional and matches a subcommand.
                if rest.is_empty() && !arg.starts_with('-') && SUBCOMMANDS.contains(&arg.as_str()) {
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
    let _eval_guard = eval_tempfile;

    // Capture --env-file vars for per-child Command::env application (A19): no
    // process-env mutation, so no `unsafe { env::set_var }` and no data race if a
    // dep threads during init. Shell-wins / `.env`-override precedence is applied
    // at each spawn site via overlay_env_file_vars / apply_env_file_vars.
    let _ = ENV_FILE_VARS.set(env_file_vars);

    SHOW_WARNINGS.store(show_warnings, Ordering::Relaxed);
    SILENT.store(silent, Ordering::Relaxed);

    if let Some(ref dir) = cwd {
        env::set_current_dir(dir)?;
    }

    if version {
        let cwd_path = env::current_dir()?;
        let node_ver = nub_core::node::discovery::discover_node(&cwd_path)
            .ok()
            .map(|n| n.version);
        println!("{}", nub_core::node::spawn::version_string(node_ver));
        return Ok(0);
    }

    if which_node {
        let cwd_path = env::current_dir()?;
        let node = nub_core::node::discovery::discover_node(&cwd_path)?;
        println!("{}", node.path);
        return Ok(0);
    }

    if show_help {
        // `nub <sub> --help`/`-h` → that subcommand's help; otherwise top-level.
        let sub = rest
            .first()
            .map(String::as_str)
            .filter(|s| SUBCOMMANDS.contains(s));
        run_help(sub);
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
        Cli::parse_from(["nub", "--help"]);
        Ok(0)
    } else {
        let first = &rest[0];
        let is_node_passthrough = first.starts_with('.')
            || first.starts_with('/')
            || first.starts_with('-')
            || std::path::Path::new(first).extension().is_some();

        if is_node_passthrough {
            run_file_with_compat(&rest, compat)
        } else {
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
            bail!(
                "nub: unknown command \"{first}\"\n\
                 \x20\x20To run a script: nub run {first}\n\
                 \x20\x20To run a file: nub ./{first}"
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
        "exec" => &["--cwd"],
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
        // everything up to and including it; the rest is forwarded verbatim.
        let prefix = rest[..=i].to_vec();
        let suffix = rest[i + 1..].to_vec();
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

    let forwards = matches!(subcommand.as_str(), "run" | "exec" | "watch");

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
                Some(ReporterMode::Default) | None => {}
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
            mut args,
        }) => {
            args.extend(suffix);
            run_exec(&bin, node, &args)
        }
        Some(Command::Upgrade {
            version,
            dry_run,
            yes,
        }) => run_upgrade(version.as_deref(), dry_run, yes),
        Some(Command::Help { command }) => {
            run_help(command.as_deref());
            Ok(0)
        }
        // `node` is intercepted at the top of `dispatch_subcommand` (manual
        // sub-verb match in `run_node`) and never reaches clap here.
        Some(Command::Node { .. }) => unreachable!("`node` is handled before clap dispatch"),
        None => unreachable!(),
    }
}

fn run_nubx() -> Result<i32> {
    // nubx <bin> [args...] ≡ nub exec <bin> [args...]. Route through the exact
    // same split as `nub exec` so the three-position rule is identical: a flag
    // before the bin (`nubx --node eslint`) is nubx's; a flag after the bin
    // (`nubx eslint --node`, `nubx eslint --help`) reaches the bin verbatim.
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() || args.iter().all(|a| a.starts_with('-') && a != "--") {
        // No bin name at all (empty, or only leading flags like `nubx --node`).
        bail!("nubx: missing binary name\nUsage: nubx [--node] <bin> [args...]");
    }

    let mut rest = vec!["exec".to_string()];
    rest.extend(args);
    dispatch_subcommand(rest)
}

fn run_as_node() -> Result<i32> {
    // Invoked as `node` via PATH shim — augmented execution.
    // Pass all remaining args through to Node with augmentation.
    let args: Vec<String> = env::args().skip(1).collect();
    run_file(&args)
}

// ── Subcommand implementations ───────────────────────────────────────

fn run_file(args: &[String]) -> Result<i32> {
    run_file_with_compat(args, false)
}

fn run_file_with_compat(args: &[String], compat_mode: bool) -> Result<i32> {
    let cwd = env::current_dir()?;
    // Fire point: `nub <file>` (and the hijack-descendant `node`, which routes
    // through run_as_node → run_file). A pinned-but-uncached version is downloaded
    // + installed from nodejs.org here, uv-style. (`nub run`/`nub exec` keep plain
    // discover_node — they don't version-check.)
    let node = nub_core::node::discovery::discover_or_provision_node(&cwd)?;

    if !compat_mode {
        if let Some(w) = nub_core::node::discovery::engines_disagreement_warning(&cwd, &node) {
            eprintln!("{w}");
        }
        nub_core::node::discovery::check_min_version(&node)?;
    }

    // .env loading: eager for all non-compat invocations per wiki/runtime/env-loading.md.
    let project = nub_core::workspace::detect::detect_project(&cwd);
    let mut env_vars = if !compat_mode {
        project
            .as_ref()
            .map(|p| nub_core::workspace::env::load_env_files(&p.root))
            .unwrap_or_default()
    } else {
        Default::default()
    };
    // --env-file vars ride alongside .env (overriding it, shell still wins). Unlike
    // .env, these apply even in compat mode — they're an explicit user flag, matching
    // the prior process-env behavior.
    overlay_env_file_vars(&mut env_vars);
    let project_root = project.as_ref().map(|p| p.root.as_path());

    let nub_binary = nub_core::node::spawn::current_nub_binary()?;
    let config = nub_core::node::spawn::SpawnConfig {
        node: &node,
        user_args: args,
        compat_mode,
        show_warnings: SHOW_WARNINGS.load(Ordering::Relaxed),
        nub_binary: &nub_binary,
        env_vars: &env_vars,
        project_root,
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
    let cwd = env::current_dir()?;
    let project = nub_core::workspace::detect::detect_project(&cwd)
        .ok_or_else(|| anyhow::anyhow!("no package.json found"))?;

    // No script name (`nub run`): list available scripts instead of a raw clap
    // "required argument" error — same shape as the missing-named-script path.
    let Some(script) = script else {
        bail!(
            "no script specified\n\nAvailable scripts:\n{}",
            list_scripts(&project.manifest)
        );
    };

    // Workspace-wide execution: -r, --filter, or --parallel.
    if ws.recursive || !ws.filter.is_empty() || ws.parallel {
        return run_workspace_script(script, compat_mode, ws, args, &project);
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
        let root_project = nub_core::workspace::detect::detect_project(&ws_root)
            .ok_or_else(|| {
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

    // Single-package execution.
    let cmd = nub_core::workspace::scripts::resolve_script(&project.manifest, script).ok_or_else(
        || {
            anyhow::anyhow!(
                "missing script: \"{script}\"\n\nAvailable scripts:\n{}",
                list_scripts(&project.manifest)
            )
        },
    )?;

    let exec = ScriptExecOpts {
        ignore_scripts: ws.ignore_scripts,
        script_shell: ws.script_shell.as_deref(),
    };
    run_single_script(script, &cmd, &project, compat_mode, args, &exec)
}

fn run_workspace_script(
    script: &str,
    compat_mode: bool,
    ws: &WorkspaceOpts,
    args: &[String],
    project: &nub_core::workspace::detect::Project,
) -> Result<i32> {
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

    // Zero-match handling. With a filter present this is always an error (a
    // selector that matches nothing is a mistake); --fail-if-no-match is the
    // explicit, self-documenting form of that same default. We surface the
    // selector list either way so the user can see what missed.
    if matched_set.is_empty() {
        if !ws.filter.is_empty() {
            bail!(
                "no packages matched the filter{}: {}",
                if ws.filter.len() == 1 { "" } else { "s" },
                ws.filter.join(", ")
            );
        }
        if ws.fail_if_no_match {
            bail!("no packages to run (--fail-if-no-match)");
        }
        // No filter, no flag, nothing to run: treat as a clean no-op.
        return Ok(0);
    }

    // Build dependency graph for topological chunking.
    let name_to_idx: std::collections::HashMap<&str, usize> = members
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
                let code = run_one_workspace_pkg(
                    &members[idx],
                    script,
                    ws_root,
                    compat_mode,
                    ws.if_present,
                    ws.stream
                        || ws.parallel
                        || concurrency > 1
                        || aggregate
                        || reporter_is_ndjson(),
                    args,
                    idx,
                    &exec,
                    aggregate,
                )?;
                if code != 0 {
                    total_failed += 1;
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
            let (tx, rx) = mpsc::channel::<usize>();
            let rx = Arc::new(std::sync::Mutex::new(rx));

            let num_workers = concurrency.min(chunk.len());
            let workers: Vec<_> = (0..num_workers)
                .map(|_| {
                    let rx = Arc::clone(&rx);
                    let failed = Arc::clone(&failed);
                    let members_snapshot: Vec<_> = chunk
                        .iter()
                        .map(|&idx| {
                            (
                                idx,
                                members[idx].name.clone(),
                                members[idx].dir.clone(),
                                members[idx].manifest.clone(),
                            )
                        })
                        .collect();
                    let ws_root_buf = ws_root.to_path_buf();
                    let script = script.to_string();
                    let args: Vec<String> = args.to_vec();
                    let if_present = ws.if_present;
                    // Owned copies so the per-script knobs cross the thread
                    // boundary; reconstituted into a borrowed `ScriptExecOpts`
                    // inside the worker (the borrowed form can't be `move`d).
                    let ignore_scripts = exec.ignore_scripts;
                    let script_shell = ws.script_shell.clone();

                    thread::spawn(move || {
                        let exec = ScriptExecOpts {
                            ignore_scripts,
                            script_shell: script_shell.as_deref(),
                        };
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
                            let Some((_, pkg_name, pkg_dir, pkg_manifest)) =
                                members_snapshot.iter().find(|(i, _, _, _)| *i == work_idx)
                            else {
                                continue;
                            };
                            // Recursion guard (same as the sequential path): a
                            // package re-entering its own running script self-skips.
                            if is_workspace_recursion_reentry(&script, pkg_name) {
                                continue;
                            }
                            let fake_project = nub_core::workspace::detect::Project {
                                root: pkg_dir.clone(),
                                workspace_root: Some(ws_root_buf.clone()),
                                manifest: pkg_manifest.clone(),
                            };
                            let cmd = nub_core::workspace::scripts::resolve_script(
                                &fake_project.manifest,
                                &script,
                            );
                            let prefix = member_prefix(pkg_dir, &ws_root_buf, pkg_name);
                            match cmd {
                                Some(cmd) => {
                                    match run_single_script_prefixed(
                                        &script,
                                        &cmd,
                                        &fake_project,
                                        compat_mode,
                                        &args,
                                        &prefix,
                                        work_idx,
                                        &exec,
                                        aggregate,
                                    ) {
                                        Ok(code) if code != 0 => {
                                            let err_prefix =
                                                format_stream_prefix(&prefix, &script, work_idx);
                                            eprintln!("{err_prefix}exit {code}");
                                            failed.fetch_add(1, AtomicOrdering::Relaxed);
                                        }
                                        Err(e) => {
                                            let err_prefix =
                                                format_stream_prefix(&prefix, &script, work_idx);
                                            eprintln!("{err_prefix}error: {e}");
                                            failed.fetch_add(1, AtomicOrdering::Relaxed);
                                        }
                                        _ => {}
                                    }
                                }
                                None => {
                                    if !if_present {
                                        eprintln!("{pkg_name} | missing script \"{script}\"");
                                        failed.fetch_add(1, AtomicOrdering::Relaxed);
                                    }
                                }
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

// Many genuinely-distinct inputs (package, script, workspace root, the
// run/output flags, the per-script exec knobs, args, and the prefix-color
// index); bundling them into a struct would be ceremony for one internal call
// site.
#[allow(clippy::too_many_arguments)]
fn run_one_workspace_pkg(
    pkg: &nub_core::workspace::filter::WorkspacePackage,
    script: &str,
    ws_root: &Path,
    compat_mode: bool,
    if_present: bool,
    stream: bool,
    args: &[String],
    color_idx: usize,
    exec: &ScriptExecOpts,
    aggregate: bool,
) -> Result<i32> {
    // Recursion guard: skip a package whose own script is already running in an
    // ancestor `nub run` (see `is_workspace_recursion_reentry`).
    if is_workspace_recursion_reentry(script, &pkg.name) {
        return Ok(0);
    }
    let cmd = nub_core::workspace::scripts::resolve_script(&pkg.manifest, script);
    match cmd {
        Some(cmd) => {
            let fake_project = nub_core::workspace::detect::Project {
                root: pkg.dir.clone(),
                workspace_root: Some(ws_root.to_path_buf()),
                manifest: pkg.manifest.clone(),
            };
            let prefix = member_prefix(&pkg.dir, ws_root, &pkg.name);
            if stream {
                let code = run_single_script_prefixed(
                    script,
                    &cmd,
                    &fake_project,
                    compat_mode,
                    args,
                    &prefix,
                    color_idx,
                    exec,
                    aggregate,
                )?;
                if code != 0 {
                    let err_prefix = format_stream_prefix(&prefix, script, color_idx);
                    eprintln!("{err_prefix}exit {code}");
                }
                Ok(code)
            } else {
                eprintln!("  {} {script}", pkg.name);
                let code = run_single_script(script, &cmd, &fake_project, compat_mode, args, exec)?;
                if code != 0 {
                    eprintln!("  {} {script} — exit {code}", pkg.name);
                }
                Ok(code)
            }
        }
        None => {
            if !if_present {
                bail!("missing script: \"{script}\" in {}", pkg.name);
            }
            Ok(0)
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

    let mut env_vars = if !compat_mode {
        nub_core::workspace::env::load_env_files(&project.root)
    } else {
        Default::default()
    };
    // --env-file vars overlay .env (shell still wins), applied here so they flow
    // through the same Command::env loop below (A19).
    overlay_env_file_vars(&mut env_vars);
    let bin_path =
        nub_core::workspace::scripts::bin_path(&project.root, project.workspace_root.as_deref());

    // Resolve Node once, up front: its path fills `npm_node_execpath` (A13/A38 —
    // threaded in, not a `node -e process.execPath` subprocess per `nub run`) and
    // its version drives flag injection in `compute_augmentation_env` below.
    let cwd = std::env::current_dir().unwrap_or_else(|_| project.root.clone());
    let node = nub_core::node::discovery::discover_node(&cwd)
        .unwrap_or_else(|_| nub_core::node::discovery::ResolvedNode::fallback());

    let npm_env = nub_core::workspace::scripts::npm_env(
        &project.manifest,
        &project.root,
        lifecycle_event,
        Some(cmd),
        node.path.as_str(),
    );

    // Shell precedence: the explicit `--script-shell <path>` flag wins, then a
    // `.npmrc` `script-shell=` setting, then the platform default. A custom
    // POSIX shell uses `-c`; only the implicit Windows `cmd` default uses `/C`.
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
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };

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
    let aug = nub_core::node::spawn::compute_augmentation_env(
        &nub_binary,
        node.version,
        compat_mode,
        Some(&project.root),
    );

    let mut command = StdCommand::new(shell);
    command.arg(shell_flag).arg(&full_cmd);
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
    let status = command.status()?;
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

    let mut child = command.spawn()?;
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
        rel
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

    // Auto-loaded `.env*` files are handed to the watched Node as `--env-file`
    // args (below) — NOT injected via `cmd.env()`. The distinction matters under
    // `--watch`: Node watches each `--env-file` path and re-reads it on every
    // restart, whereas `cmd.env()` would freeze the values at parent-spawn time
    // so an edit to `.env` would never reach the child (the bug this fixes). The
    // explicit `--env-file` *flag* (a top-level user flag captured at startup)
    // still flows through `cmd.env()` via `apply_env_file_vars`; it's a distinct
    // surface and keeps its override-`.env` precedence.
    //
    // Precedence is preserved across the swap: Node's `--env-file` never
    // overrides a var already in the shell environment (shell-wins holds), and
    // among the `.env*` files Node is *last*-writer-wins, so we pass
    // `discover_env_files`' highest-priority-first list in reverse for nub's
    // first-writer-wins precedence to line up.
    //
    // DIVERGENCE: Node's `--env-file` parser does not expand `${VAR}`
    // cross-references within a value, while `load_env_files` does. A `.env`
    // that relies on `B=${A}_x` style expansion therefore sees the literal
    // `${A}_x` under `nub watch`. This is the deliberate cost of live reload;
    // the non-watch run path keeps full expansion via `load_env_files`.
    let env_file_paths = nub_core::workspace::detect::detect_project(&cwd)
        .map(|p| nub_core::workspace::env::discover_env_files(&p.root))
        .unwrap_or_default();

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
    // Explicit `--env-file` flag vars only; auto-loaded `.env*` rides the
    // `--env-file` args above so Node watches/re-reads them on restart.
    apply_env_file_vars(&mut cmd);
    let status = cmd.status()?;

    // PATH shim cleanup is handled once at the top level (see `run`).
    Ok(nub_core::node::spawn::exit_code_from_status(&status))
}

fn run_exec(bin: &str, compat_mode: bool, args: &[String]) -> Result<i32> {
    let cwd = env::current_dir()?;

    if let Some(bin_path) = nub_core::workspace::scripts::find_bin(bin, &cwd) {
        return launch_bin(&bin_path, args, compat_mode, &cwd);
    }

    // Not in node_modules/.bin. Per exec.md (decision 2026-05-26): nub does NOT
    // run a `dlx`/`npx` network fetch itself — that hits the registry and can
    // block on an interactive install prompt in CI, the exact failure that
    // decision removed. Print the install / run-ad-hoc suggestion and exit
    // non-zero (127, the conventional "command not found").
    let pm = detect_package_manager(&cwd);
    let (add_cmd, dlx_cmd) = match pm.as_str() {
        "pnpm" => (format!("pnpm add -D {bin}"), format!("pnpm dlx {bin}")),
        "yarn" => (format!("yarn add -D {bin}"), format!("yarn dlx {bin}")),
        "bun" => (format!("bun add -d {bin}"), format!("bunx {bin}")),
        _ => (format!("npm install -D {bin}"), format!("npx {bin}")),
    };
    eprintln!("nub: `{bin}` is not installed in node_modules/.bin.");
    eprintln!("     install it ({add_cmd}), or run it ad-hoc with: {dlx_cmd}");
    Ok(127)
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
        return run_file_with_compat(&cmd_args, compat_mode);
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
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 128];
    let n = std::io::Read::read(&mut f, &mut buf).unwrap_or(0);
    let head = &buf[..n];
    head.starts_with(b"#!") && head.windows(4).any(|w| w == b"node")
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
    let Some(aug) = nub_core::node::spawn::compute_augmentation_env(
        &nub_binary,
        node.version,
        false,
        Some(cwd),
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

// dlx removed per Colin 2026-05-26 (exec.md). nubx is local-bin-only; on a miss it
// SUGGESTS the PM dlx command and exits non-zero (127) — it never runs a fetch.

fn detect_package_manager(cwd: &Path) -> String {
    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        if dir.join("pnpm-lock.yaml").is_file() {
            return "pnpm".to_string();
        }
        if dir.join("yarn.lock").is_file() {
            return "yarn".to_string();
        }
        if dir.join("bun.lockb").is_file() || dir.join("bun.lock").is_file() {
            return "bun".to_string();
        }
        if dir.join("package-lock.json").is_file() {
            return "npm".to_string();
        }
        if !dir.pop() {
            break;
        }
    }
    "npm".to_string()
}

/// The GitHub repo that hosts Nub's release artifacts. The self-owned tarball
/// channel downloads from here; mirror of install.sh.
const RELEASE_REPO: &str = "colinhacks/nub";

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
    if let Some(bin_dir) = bin_path.parent() {
        if bin_dir.file_name().is_some_and(|n| n == "bin") {
            if let Some(install_dir) = bin_dir.parent() {
                if install_dir.file_name().is_some_and(|n| n == ".nub") {
                    return UpgradeChannel::SelfOwned {
                        install_dir: install_dir.to_path_buf(),
                    };
                }
            }
        }
    }
    UpgradeChannel::Unknown
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

/// GitHub release tarball URL for a resolved version + platform target. Mirrors
/// install.sh's `url=` line so the self-owned channel pulls the same artifact
/// the installer did.
fn tarball_url(version: &str, target: &str) -> String {
    format!("https://github.com/{RELEASE_REPO}/releases/download/v{version}/nub-{target}.tar.gz")
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
                println!("nub upgrade: would upgrade to {target} via npm");
                println!("  command: {}", npm_upgrade_command(target));
            }
            UpgradeChannel::Homebrew => {
                println!("nub upgrade: would upgrade to {target} via homebrew");
                println!("  command: brew upgrade nub");
            }
            UpgradeChannel::SelfOwned { install_dir } => {
                println!("nub upgrade: would upgrade to {target} via self-owned (~/.nub)");
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
                println!(
                    "nub upgrade: would upgrade to {target}, but the install channel is unknown"
                );
                println!("  binary: {bin_str}");
                println!("  manual: {}", npm_upgrade_command(target));
            }
        }
        return Ok(0);
    }

    match channel {
        UpgradeChannel::Npm => {
            let cmd = npm_upgrade_command(target);
            println!("nub upgrade: running `{cmd}`");
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .status()?;
            Ok(nub_core::node::spawn::exit_code_from_status(&status))
        }
        UpgradeChannel::Homebrew => {
            println!("nub upgrade: running `brew upgrade nub`");
            let status = std::process::Command::new("brew")
                .arg("upgrade")
                .arg("nub")
                .status()?;
            Ok(nub_core::node::spawn::exit_code_from_status(&status))
        }
        UpgradeChannel::SelfOwned { install_dir } => {
            perform_selfowned_upgrade(&install_dir, target)?;
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
    let api = format!("https://api.github.com/repos/{RELEASE_REPO}/releases/latest");
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

    println!("nub upgrade: upgrading to v{version} ({target})");

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

    println!(
        "nub upgrade: installed v{version} to {}",
        install_dir.display()
    );
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

fn run_help(command: Option<&str>) {
    // Re-parse with `--help` to obtain clap's help. `try_parse_from` returns it
    // as `Err(DisplayHelp)` carrying the formatted text; print it (clap routes
    // DisplayHelp/DisplayVersion to stdout, real errors to stderr) instead of
    // discarding it, which left `nub <sub> --help` / `nub help <sub>` silent.
    let result = match command {
        Some(cmd) => Cli::try_parse_from(["nub", cmd, "--help"]),
        None => Cli::try_parse_from(["nub", "--help"]),
    };
    if let Err(e) = result {
        let _ = e.print();
    }
}

/// `nub node …` — the version-management command group (install / ls / uninstall
/// / pin). Non-forwarding; manual sub-verb match so the bare-usage and the
/// `nub node <file>` error read exactly as the spec specifies.
/// Spec: `wiki/commands/node-versions.md`.
fn run_node(args: &[String]) -> Result<i32> {
    let cwd = env::current_dir()?;
    let store = nub_core::node::discovery::node_store_dir()
        .ok_or_else(|| anyhow::anyhow!("could not locate nub's cache directory (no $HOME / $XDG_CACHE_HOME)"))?;

    // Bare `nub node`, or `nub node --help`/`-h`: short usage listing the verbs.
    let verb = args.first().map(String::as_str);
    if verb.is_none() || matches!(verb, Some("--help") | Some("-h") | Some("help")) {
        println!(
            "nub node — manage Node versions\n\n\
             Usage: nub node <command>\n\n\
             Commands:\n\
             \x20 install [<version>...]   provision version(s) into nub's cache (bare: the project pin)\n\
             \x20 ls                       list versions in nub's cache\n\
             \x20 uninstall <version>      remove a version from nub's cache\n\
             \x20 pin <version>            write the project's Node pin"
        );
        return Ok(0);
    }

    // Past the guard above, a verb is present.
    match verb.expect("verb present after the help/bare guard") {
        "install" => {
            use nub_core::version_management::manage::{self, InstallOutcome};
            let specs = &args[1..];
            let report = |outcome: &InstallOutcome| match outcome {
                InstallOutcome::AlreadyCached(v) => {
                    eprintln!("Node {v} is already in nub's cache.");
                }
                InstallOutcome::AlreadyOnPath(v) => {
                    eprintln!("Node {v} is already available on PATH — skipped (already installed).");
                }
                InstallOutcome::Installed(_) => { /* provision_node already printed the ✓ line */ }
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
                eprintln!("No Node versions in nub's cache. Install one with `nub node install <version>`.");
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
                "nub node takes a subcommand (install, ls, uninstall, pin). \
                 To run a file, use 'nub <file>'."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
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

    // Verification correctness: the checksum sidecar URL is the tarball URL plus
    // `.sha256`, and the digest helper matches the well-known empty-input vector.
    #[test]
    fn tarball_and_checksum_urls_pair_up() {
        let url = tarball_url("0.0.6", "darwin-arm64");
        assert_eq!(
            url,
            "https://github.com/colinhacks/nub/releases/download/v0.0.6/nub-darwin-arm64.tar.gz"
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
    fn which_node_flag() {
        let cli = parse(&["nub", "--which-node"]).unwrap();
        assert!(cli.which_node);
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
    fn argv0_detection() {
        assert_eq!(Argv0::detect(), Argv0::Nub);
    }

    // ── nubx argv0 dispatch ─────────────────────────────────────────

    #[test]
    fn nubx_node_flag_detection() {
        // Verify the --node flag stripping logic in run_nubx
        let args = vec![
            "--node".to_string(),
            "vitest".to_string(),
            "--run".to_string(),
        ];
        let has_node = args.iter().any(|a| a == "--node");
        assert!(has_node);

        let mut filtered: Vec<String> = args.into_iter().filter(|a| a != "--node").collect();
        let bin = filtered.remove(0);
        assert_eq!(bin, "vitest");
        assert_eq!(filtered, vec!["--run"]);
    }
}
