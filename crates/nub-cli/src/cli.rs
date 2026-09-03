//! CLI argument parsing and dispatch.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use anyhow::{Context, Result, bail};
#[cfg(feature = "compile")]
use clap::ArgAction;
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
/// The self-shim's failure surface — provisioning or verifying the pinned nub
/// named by `packageManager: "nub@x.y.z"` failed. Every message carrying it names
/// the `NUB_SELF_SHIM=0` escape hatch so a bad pin someone else committed can't
/// brick a clone.
const ERR_NUB_SELF_SHIM: &str = "ERR_NUB_SELF_SHIM";

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
/// `error: file:line`) see the child's raw output. Affects the CHILD's per-line
/// prefix only — Nub's own framing (the `$ <cmd>` echo and the trailing
/// `Done`/`exit`/`error` status) keeps its label, matching pnpm. Stripping the
/// status line too left a run emitting several unattributable bare `Done`s.
static HIDE_STREAM_PREFIX: AtomicBool = AtomicBool::new(false);

/// The resolved `--color` / `--no-color` choice, as a [`ColorWhen`] discriminant.
/// Set once at startup from the flag; read by [`color_enabled`] at every ANSI
/// decision and by the script launcher when it exports `FORCE_COLOR` to a child.
/// A process-global (rather than a threaded-through parameter) because the same
/// answer has to reach Nub's own output, the PM engine's warnings, and the
/// per-child environment, and it is decided before any of them run.
static COLOR_MODE: AtomicU8 = AtomicU8::new(COLOR_AUTO);
const COLOR_AUTO: u8 = 0;
const COLOR_ALWAYS: u8 = 1;
const COLOR_NEVER: u8 = 2;

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
    /// `-y`/`--yes`: the explicit consent escape hatch for the implicit registry
    /// tier — let CI / non-TTY through and skip the TTY first-fetch prompt
    /// ([`crate::nubx_consent`]). Without it, a registry fallthrough fails closed
    /// in CI / non-TTY and prompts once per spec in an interactive terminal.
    pub yes: bool,
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

/// Un-stripped merge of the explicit `--env-file` contents — the values Node's
/// own `--env-file` parser delivers. The watch path forwards the explicit files
/// to the watched Node (#479) and diffs [`ENV_FILE_VARS`] against this map (see
/// [`watch_inject_vars`]) so a var Node already delivers is left to Node's
/// re-read and live-reloads across restarts. The two maps now differ only by the
/// denied-key strip, since `--env-file` values are no longer expanded; keeping
/// them separate is what preserves that strip (a denied key must stay out of
/// [`ENV_FILE_VARS`] but stay IN the forwarded set, so the guard can neutralize
/// it rather than let a startup consumer act on it).
static ENV_FILE_VARS_RAW: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Explicit `--env-file` / `--env-file-if-exists` paths in command-line order
/// (absolutized against the scan-time cwd — the directory `read_env_file`
/// resolved them from), tagged with the if-exists flavor. Retained for the
/// watch path, which forwards them to the watched Node as `--env-file*` args
/// so Node watches each file and every restarted child re-reads it (#479);
/// the parsed values in [`ENV_FILE_VARS`] alone would freeze at their startup
/// snapshot because the long-lived watch supervisor never restarts. Non-watch
/// paths never read this.
static ENV_FILE_PATHS: OnceLock<Vec<(PathBuf, bool)>> = OnceLock::new();

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

/// Whether `--no-env-file` was passed. It means "load ZERO env files": eager
/// `.env*` auto-discovery is suppressed AND any explicit `--env-file` is ignored
/// (`--no-env-file` WINS over `--env-file`, decided 2026-07-07). All non-env
/// augmentation (TS/JSX/module hooks) is unaffected. Flag-only, no env-var
/// companion and no tree-wide inheritance — the same per-process boundary as
/// `--env-file` (a child `nub`/`node` it spawns does not inherit the suppression).
static NO_ENV_FILE: OnceLock<bool> = OnceLock::new();

/// Per-child watch cleanup state. The long-lived Node watch supervisor skips
/// preloads, so this survives there and is consumed independently by every
/// restarted child before user preloads run.
const WATCH_ENV_GUARD_ENV: &str = "__NUB_WATCH_ENV_GUARD";

/// True iff the user passed `--no-env-file`. The authoritative kill-switch read
/// by every env-file consumer ([`merge_child_env`], [`overlay_env_file_vars`],
/// [`apply_env_file_vars`], and the auto-discovery load sites).
fn no_env_file() -> bool {
    NO_ENV_FILE.get().copied().unwrap_or(false)
}

/// Overlay the `--env-file` vars onto an env map bound for a child's
/// `Command::env`. Shell env still wins (skip keys already in this process's
/// environment); `--env-file` overrides `.env` (insert over existing entries).
/// `--env-file` vars are thus handled uniformly with `.env` vars — both flow
/// through the same per-spawn `Command::env` application.
fn overlay_env_file_vars(env_map: &mut HashMap<String, String>) {
    // `--no-env-file` wins over `--env-file`: overlay nothing.
    if no_env_file() {
        return;
    }
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
/// four auto files (`.env.<mode>.local`, `.env.local`, `.env.<mode>`, `.env`)
/// load, and only the explicit file(s) reach the child. With no `--env-file`, the
/// autos load as before.
///
/// `auto_env` is the already-loaded `.env*` map (callers pass `load_env_files`'s
/// result, whose `<mode>` comes from `APP_ENV`, else a clamped `NODE_ENV`
/// fallback, with `${VAR}` expansion); `env_file_present` is the
/// flag-presence signal; `explicit_vars` is `ENV_FILE_VARS` (the parsed
/// `--env-file` contents); `no_env_file` is the `--no-env-file` kill-switch. This
/// is a pure function over its inputs so the suppression contract can be
/// unit-tested without spawning a child.
fn merge_child_env(
    auto_env: HashMap<String, String>,
    env_file_present: bool,
    explicit_vars: &HashMap<String, String>,
    no_env_file: bool,
) -> HashMap<String, String> {
    // `--no-env-file` WINS over everything: load ZERO env files — both auto-
    // discovery and explicit `--env-file` are suppressed (decided 2026-07-07).
    if no_env_file {
        return HashMap::new();
    }
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

/// Select which `.env*` vars the watch path injects into the watched Node via
/// `Command::env` (#207). The watched `node --watch` process also receives the
/// `.env*` files as `--env-file` args, which Node re-reads on every restart — so
/// injecting a var Node already delivers identically would FREEZE it at the `nub
/// watch` startup value (Node's `--env-file` never overrides an already-present
/// env var). Inject a key iff nub's value differs from the raw value Node
/// actually delivers — `forwarded_raw` is the unexpanded merge of the files that
/// really reach Node as `--env-file` args; every var Node delivers identically is
/// left to Node's `--env-file` and live-reloads on restart. In practice the
/// difference comes from `${VAR}` expansion, which only the auto-discovered and
/// `nub.jsonc`-sourced families get: an explicit `--env-file` is delivered
/// verbatim (Node's own semantics), so it never diverges and is always left to
/// Node once forwarded. A key absent from
/// `forwarded_raw` is injected, because injection is then its only delivery
/// channel: that covers the explicit `--env-file` case (auto-discovery is
/// suppressed) and every source below Node's 20.6.0 `--env-file` floor, where
/// nothing is forwarded at all. A CLI value that overrides a `nub.jsonc` `env`
/// source likewise differs from `forwarded_raw` and is injected, keeping CLI
/// strongest. Pure over its inputs so the selection can be unit-tested without
/// spawning Node.
fn watch_inject_vars<'a>(
    env_vars: &'a HashMap<String, String>,
    forwarded_raw: &HashMap<String, String>,
) -> Vec<(&'a String, &'a String)> {
    env_vars
        .iter()
        .filter(|(k, v)| forwarded_raw.get(*k) != Some(*v))
        .collect()
}

/// Node's `--env-file` floor (landed 20.6.0). Below it Node aborts on the
/// unknown option *before executing anything*, so a watch child handed the flag
/// dies instantly with no output — every env-file source must fall back to
/// whole-map injection instead. nub supports Node from 18.19, so this range is
/// live, not theoretical. Gates the auto-discovered cascade as well as the
/// explicit flags.
fn node_accepts_env_file(version: &nub_core::node::version::NodeVersion) -> bool {
    *version >= nub_core::node::version::NodeVersion::new(20, 6, 0)
}

/// Whether the watch path forwards the explicit `--env-file` flags to the
/// watched Node (#479): the pinned Node must accept every flag flavor present —
/// `--env-file` landed in 20.6.0, `--env-file-if-exists` in 22.9.0. All-or-
/// nothing per run: mixing forwarded and injected explicit files would corrupt
/// last-writer-wins precedence. Below the floor the watch path falls back to
/// whole-map injection (values freeze at startup — the pre-#479 behavior).
fn should_forward_explicit_env_files(
    version: &nub_core::node::version::NodeVersion,
    explicit: &[(PathBuf, bool)],
) -> bool {
    use nub_core::node::version::NodeVersion;
    if explicit.is_empty() {
        return false;
    }
    let needs_if_exists = explicit.iter().any(|(_, if_exists)| *if_exists);
    node_accepts_env_file(version) && (!needs_if_exists || *version >= NodeVersion::new(22, 9, 0))
}

/// Render a forwarded watch env file (auto-discovered or explicit) for Node's
/// platform-specific watcher. Node 20.11 on Linux rejects absolute `--env-file`
/// paths even when the file exists. Node 24's Windows watcher also aborts when
/// an argv path contains an 8.3 component (for example `RUNNER~1`) but the
/// filesystem event arrives with the long spelling. Keep Windows absolute and
/// expand only short components; make Unix cwd-relative without canonicalizing
/// so symlink and watch identity stay unchanged. `if_exists` picks the
/// `--env-file-if-exists` spelling (and tolerates an absent file, which that
/// flag's semantics allow).
fn watch_env_file_arg(path: &Path, cwd: &Path, windows: bool, if_exists: bool) -> Result<String> {
    let flag = if if_exists {
        "--env-file-if-exists"
    } else {
        "--env-file"
    };
    if !path.is_absolute() || !cwd.is_absolute() {
        bail!("watch env-file paths and cwd must be absolute");
    }

    if windows {
        // GetLongPathNameW fails on a nonexistent path; an absent if-exists
        // target is legal (Node silently no-ops), so forward it verbatim — an
        // unwatchable file has no event for the 8.3-spelling mismatch to bite.
        let path = if if_exists && !path.exists() {
            path.to_path_buf()
        } else {
            windows_long_watch_path(path)?
        };
        let path = path
            .to_str()
            .context("watch env-file path is not valid UTF-8")?;
        let path = strip_windows_verbatim_prefix(path);
        return Ok(format!("{flag}={path}"));
    }

    for (parent_count, ancestor) in cwd.ancestors().enumerate() {
        let Ok(suffix) = path.strip_prefix(ancestor) else {
            continue;
        };
        let mut relative = PathBuf::new();
        for _ in 0..parent_count {
            relative.push("..");
        }
        relative.push(suffix);
        if relative.as_os_str().is_empty() {
            bail!("watch env-file path must name a file");
        }
        let relative = relative
            .to_str()
            .context("watch env-file relative path is not valid UTF-8")?;
        return Ok(format!("{flag}={relative}"));
    }

    bail!(
        "watch env-file path {} cannot be made relative to {}",
        path.display(),
        cwd.display()
    )
}

/// Expand Windows 8.3 components without resolving symlinks or junctions.
/// libuv normalizes filesystem-event paths with this same API; doing it for the
/// watched path keeps both sides of its prefix comparison in one spelling.
/// Callers pass either an existing auto-discovered env file or the existing
/// watcher cwd, so failure is exceptional and should be reported rather than
/// letting Node hit its process-aborting assertion.
#[cfg(windows)]
fn windows_long_watch_path(path: &Path) -> Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use windows_sys::Win32::Storage::FileSystem::GetLongPathNameW;

    let mut input: Vec<u16> = path.as_os_str().encode_wide().collect();
    input.push(0);

    // SAFETY: `input` is NUL-terminated and lives across both calls. The first
    // call requests the required capacity without writing an output buffer.
    let mut capacity = unsafe { GetLongPathNameW(input.as_ptr(), std::ptr::null_mut(), 0) };
    if capacity == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("could not expand Windows watch path {}", path.display()));
    }

    loop {
        let mut output = vec![0u16; capacity as usize];
        // SAFETY: the output pointer is valid for `capacity` UTF-16 code units;
        // the input remains NUL-terminated and neither buffer aliases the other.
        let written =
            unsafe { GetLongPathNameW(input.as_ptr(), output.as_mut_ptr(), output.len() as u32) };
        if written == 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("could not expand Windows watch path {}", path.display())
            });
        }
        if (written as usize) < output.len() {
            output.truncate(written as usize);
            return Ok(PathBuf::from(OsString::from_wide(&output)));
        }
        // The path changed between calls or the first capacity became stale.
        // `GetLongPathNameW` returns the required size when the buffer is short.
        capacity = written.saturating_add(1);
    }
}

#[cfg(not(windows))]
fn windows_long_watch_path(path: &Path) -> Result<PathBuf> {
    Ok(path.to_path_buf())
}

/// Rust canonicalization and some Windows APIs emit verbatim paths, but Node's
/// option parser expects the ordinary drive/UNC spelling. Handle UNC first so
/// `\\?\UNC\server\share` becomes `\\server\share`, not `UNC\server\share`.
fn strip_windows_verbatim_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
    }
}

/// A path spelled for a HUMAN to read. `current_nub_binary` canonicalizes, and
/// Windows canonicalization returns the extended-length `\\?\C:\…` form, so any
/// path derived from it lands in output with a prefix no user typed and no shell
/// echoes back (#704). Strip it at the print site only: the `PathBuf` itself
/// stays verbatim so filesystem operations keep the `MAX_PATH` exemption a deep
/// install dir depends on.
fn display_path(path: &Path) -> String {
    strip_windows_verbatim_prefix(&path.to_string_lossy())
}

/// The keys the watch guard strips from a forwarded env file's values.
///
/// The runtime-control denylist is unconditional. `NODE_ENV` joins it only when
/// the AUTO `.env*` cascade is the live source, which is what keeps `nub watch`
/// byte-identical to `nub <file>`: `load_env_files` drops a file-set `NODE_ENV`
/// (#263 — a `.env` pinning it broke `next build`, whose prerender forks
/// inherited the wrong mode), while the explicit `--env-file` map deliberately
/// does not. Forwarding the raw files to Node bypasses that load-time drop
/// entirely, so the watch path has to re-apply it at the process boundary. The
/// two families never co-occur — an explicit flag suppresses auto-discovery — so
/// keying the extra drop on which family is live reproduces each one's
/// direct-runner behavior exactly.
fn watch_guarded_env_file_keys(auto_cascade: bool) -> Vec<&'static str> {
    let mut keys = nub_core::workspace::env::denied_env_file_keys().to_vec();
    if auto_cascade {
        keys.push("NODE_ENV");
    }
    keys
}

/// Exact ambient spellings of the guarded keys. A Unix process may carry both
/// canonical and mixed-case spellings; Windows collapses them through its
/// case-insensitive environment. An ambient value is the user's own and must
/// survive untouched — only file-derived values are stripped. Nub-owned keys
/// that this particular launcher actually stamps are appended in their
/// canonical spelling because they live on the child command rather than in
/// this process environment. Do not treat the whole internal namespace as
/// launcher-owned: an unstamped key must keep its placeholder so a raw env file
/// cannot forge it.
fn ambient_guarded_env_file_keys(
    guarded: &[&'static str],
    launcher_owned: &[String],
) -> Vec<String> {
    let ambient = env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .filter(|key| guarded.iter().any(|g| key.eq_ignore_ascii_case(g)));
    let stamped = launcher_owned
        .iter()
        .filter(|key| guarded.contains(&key.as_str()))
        .cloned();
    ambient.chain(stamped).collect()
}

/// Canonical placeholders the watch supervisor must carry so Node's early
/// startup consumers cannot read a value from a raw env file. Unix startup
/// lookup is exact-case, so a mixed-case ambient key does not cover the canonical
/// spelling; Windows lookup is case-insensitive.
fn watch_env_guard_placeholders(
    guarded: &[&'static str],
    ambient_keys: &[String],
    windows: bool,
) -> Vec<&'static str> {
    guarded
        .iter()
        .copied()
        .filter(|denied| {
            !ambient_keys.iter().any(|ambient| {
                if windows {
                    ambient.eq_ignore_ascii_case(denied)
                } else {
                    ambient == denied
                }
            })
        })
        .collect()
}

/// Apply the `--env-file` vars directly to a child command, for spawn paths
/// (`nubx` non-node launchers, the dlx fallback) that don't build an env map.
/// Same precedence as [`overlay_env_file_vars`].
fn apply_env_file_vars(cmd: &mut std::process::Command) {
    // `--no-env-file` wins over `--env-file`: apply nothing.
    if no_env_file() {
        return;
    }
    if let Some(vars) = ENV_FILE_VARS.get() {
        for (k, v) in vars {
            if env::var_os(k).is_none() {
                cmd.env(k, v);
            }
        }
    }
}

/// Build the fetched tool's env overlay. The engine spawns the tool itself, so
/// the explicit `--env-file` vars have to be handed over as a map rather than
/// applied to a `Command` the way [`apply_env_file_vars`] does.
///
/// `compat_mode` carries the CLI `--node` flag across the dlx boundary. The
/// fetched bin's own `node` shebang re-enters nub as a PATH-shim (the same
/// re-entrant-as-node trick that gives it augmentation at all — see
/// `apply_lifecycle_augmentation`'s `runtime_node_dir`), and that shim reads
/// compat from its OWN inherited environment, not from this process's argv.
/// An ambient `NODE_COMPAT` already survives the hop for free (real env vars
/// inherit); a bare `--node` flag does not, since it never became an env var.
/// Stamping `NODE_COMPAT=1` here is what makes `--node` reach the re-entrant
/// child the same way the persistent env var already does.
pub(crate) fn dlx_child_env(compat_mode: bool) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    if compat_mode {
        values.insert("NODE_COMPAT".to_string(), "1".to_string());
    }
    if no_env_file() {
        return values;
    }
    for (key, value) in ENV_FILE_VARS.get().into_iter().flatten() {
        if env::var_os(key).is_none() {
            values.insert(key.clone(), value.clone());
        }
    }
    values
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
    /// ([`run_pm_shim`]). Spec: `package-manager-shims` (no such document).
    PmShim(nub_core::pm::shim::ShimName),
}

/// The verb the launcher told us to be, captured out of `__NUB_ARGV0` once at
/// startup and then erased from the environment (see [`capture_argv0_override`]).
static ARGV0_OVERRIDE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Read `__NUB_ARGV0` into [`ARGV0_OVERRIDE`] and REMOVE it from the environment.
///
/// The platform packages ship ONE binary. `nub` and `nubx` are the same file, and
/// the verb normally comes from argv[0]'s basename — but the launcher's healed sh
/// trampoline `exec`s that file by its real path, and POSIX `sh` has no portable way
/// to set argv[0] (`exec -a` is a bash/zsh-ism; dash, which is `/bin/sh` on Debian
/// and Ubuntu, rejects it). So the launcher passes the verb in this variable instead.
/// argv[0] remains the fallback and still carries every direct invocation — the
/// installer's `~/.nub/bin/nubx` symlink, `nub pm shim` hardlinks, `nubx-dev`.
///
/// ERASING IT IS LOAD-BEARING, not hygiene: nub spawns Node, and a script under that
/// Node may invoke `nub` again. An inherited `__NUB_ARGV0=nubx` would silently put
/// that grandchild in exec mode. Removing it here means the variable survives exactly
/// one process — the one the launcher aimed it at.
///
/// Captured into a `OnceLock` rather than re-read, because `Argv0::detect` runs more
/// than once per process (invocation-boundary normalization, then dispatch) and the
/// value is gone from the environment after this call.
///
/// # Safety
///
/// Mutates the process environment, which is sound only while nub is single-threaded.
/// `main` calls this through [`normalize_invocation_environment`] as its first action.
pub unsafe fn capture_argv0_override() {
    ARGV0_OVERRIDE.get_or_init(|| {
        let verb = env::var("__NUB_ARGV0").ok().filter(|v| !v.is_empty());
        if verb.is_some() {
            // SAFETY: upheld by this function's caller — no other thread exists yet.
            unsafe { env::remove_var("__NUB_ARGV0") };
        }
        verb
    });
}

impl Argv0 {
    pub fn detect() -> Self {
        // The launcher-supplied verb wins over argv[0]: on the healed fast path the
        // binary is exec'd by its real name (`bin/nub`) for BOTH verbs, so argv[0]
        // cannot distinguish them. Absent the override — every direct invocation —
        // argv[0]'s basename is still the signal.
        if let Some(verb) = ARGV0_OVERRIDE.get().and_then(Option::as_deref) {
            return Self::classify(verb);
        }
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

/// Establish the logical invocation boundary before any subsystem can spawn a
/// thread or discover configuration. `node` is Nub's PATH-shim continuation, so
/// it keeps the inherited runtime snapshot and augmentation; every other public
/// argv0 is a fresh user invocation and gets the parent's captured ambient
/// environment back. A hidden internal re-entry belongs to its parent and is
/// exempt for the same reason `node` is.
pub fn normalize_invocation_environment() {
    // UNCONDITIONALLY FIRST, before the early returns below: this both establishes the
    // verb every later `Argv0::detect()` sees and erases `__NUB_ARGV0` so no child
    // inherits it. Gating it behind the returns would leak the variable into the whole
    // `node`-shim and internal-reentry subtree, and would leave the OnceLock unset on
    // exactly the paths that re-enter nub.
    //
    // SAFETY: `main` calls this as its first action, before logging, config
    // initialization, or any thread-capable subsystem. No other thread exists yet.
    unsafe { capture_argv0_override() };

    let internal_reentry = env::args_os().nth(1).is_some_and(|arg| {
        (cfg!(unix) && arg == "__pdeath-watch") || arg == "__node-gyp-bootstrap"
    });
    if internal_reentry || matches!(Argv0::detect(), Argv0::Node) {
        return;
    }

    // SAFETY: `main` calls this before logging, config initialization, or any
    // thread-capable Nub subsystem. No other thread exists yet.
    unsafe {
        nub_core::node::spawn::restore_fresh_invocation_environment();
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

    // Declared to clap as well as caught by the pre-subcommand argv scan, because
    // pnpm accepts it in BOTH positions and the scan only sees tokens before the
    // verb: without this, `nub run -r --no-color build` was refused by the parser
    // while the pre-verb spelling worked.
    //
    // Kept as a plain comment, NOT a doc comment: clap renders a doc comment into
    // `--help`, and a `global` arg's text lands in EVERY subcommand's help — where
    // `cli_grammar_parity` greps for the parser's rejection wording to decide
    // whether a form was refused. Quoting that wording here made every probe in
    // that suite read as a rejection.
    /// Disable color. The pnpm-compatible spelling of `--color=never`.
    #[arg(long = "no-color", global = true, conflicts_with = "color")]
    pub no_color: bool,

    /// Enable watch mode (alias for `nub watch`).
    #[arg(long)]
    pub watch: bool,

    /// File to execute, or `-` for stdin. When no subcommand matches,
    /// the first positional is treated as a file path and everything
    /// after it passes through to Node.
    #[arg(trailing_var_arg = true)]
    pub args: Vec<String>,
}

// One of these is built once per process, straight from argv, and matched
// immediately — it is never held in a collection, moved in a hot loop, or sent
// across a channel, so the size gap between the largest variant and the rest buys
// nothing to fix. Boxing a clap `Subcommand` variant would also put an indirection
// in front of every field the parser writes and every match arm reads.
#[allow(clippy::large_enum_variant)]
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

        /// Skip the pre-run dependency-freshness check for this invocation
        /// (`--no-install` is the alias).
        #[arg(long = "no-check", visible_alias = "no-install")]
        no_check: bool,

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

        /// Skip the pre-run dependency-freshness check for this invocation.
        /// (Spelled `--no-check` — `--no-install` is reserved for the npx
        /// fetch semantics on `nubx`, so `nub exec` does not accept it.)
        #[arg(long = "no-check")]
        no_check: bool,

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

        /// Consent up-front to the implicit registry fetch (`npx -y`): skips the
        /// first-fetch prompt and is the escape hatch out of the CI / non-TTY
        /// fail-closed default.
        #[arg(short = 'y', long)]
        yes: bool,

        /// Accepted for `npx` parity. Removed from npm v9+; nubx warns and ignores it.
        #[arg(long = "ignore-existing")]
        ignore_existing: bool,

        /// Skip the pre-run dependency-freshness check for this invocation.
        /// (Spelled `--no-check` here — `--no-install` already means "don't
        /// fetch a missing tool" on this surface.)
        #[arg(long = "no-check")]
        no_check: bool,

        /// Per-invocation `minimumReleaseAge` overrides for the fetch path.
        /// `nubx <just-published-tool>` is the common way to hit the age gate,
        /// so the escape hatch belongs on this surface.
        #[command(flatten)]
        age_gate: crate::pm_engine::AgeGateFlags,

        /// Which platforms' optional dependencies to install
        /// (`--os`/`--cpu`/`--libc`), overriding host detection for this
        /// run only. Mirrors pnpm's flags of the same names.
        #[command(flatten)]
        platform: crate::pm_engine::PlatformFlags,

        /// Remaining arguments forwarded to the binary.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    // nub's own project init, not the engine's npm-style manifest write —
    // deliberately excluded from ENGINE_VERBS; design record in
    // internal/commands/init.md. (The doc comment below is user-facing `--help`
    // text: no internal references.)
    /// Compile a file to a standalone executable.
    ///
    /// Bundles the entry (Rolldown, in-process), embeds a Node for the target
    /// (default shape), and emits one self-contained binary. `--smol` embeds no
    /// Node — it discovers or provisions one at first run.
    ///
    /// Gated behind the `compile` cargo feature: release builds enable it, while
    /// feature-off development builds expose no `compile` verb rather than one
    /// that can only error.
    #[cfg(feature = "compile")]
    Compile {
        /// Entry file (TS/JS) to bundle and compile.
        entry: String,

        /// Output path. Default: ./<entry-stem> (plus `.exe` for a Windows target).
        #[arg(long, value_name = "PATH")]
        out: Option<String>,

        /// No embedded Node: discover or provision one at runtime.
        #[arg(long)]
        smol: bool,

        /// Node version to target (overrides the project's pin chain). Accepts a
        /// concrete version, a major, a range, or an alias (`lts`/`latest`).
        /// Omitted → inferred from `.node-version` / `engines.node` / etc.
        #[arg(long, value_name = "VERSION")]
        target: Option<String>,

        /// Target platform. Default: the host. One of `darwin-arm64`,
        /// `darwin-x64`, `linux-arm64`, `linux-arm64-musl`, `linux-x64`,
        /// `linux-x64-musl`, `win32-arm64`, `win32-x64`. A foreign platform's
        /// launcher is fetched from this release and cached.
        #[arg(long, value_name = "PLATFORM")]
        platform: Option<String>,

        /// Disable minification (default: minify on).
        #[arg(long = "no-minify")]
        no_minify: bool,

        /// Where the source map goes: `none` (default), `linked`, `inline`, or
        /// `external`. Written `--sourcemap=<MODE>`; bare `--sourcemap` is inline.
        #[arg(
            long,
            value_name = "MODE",
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "inline"
        )]
        sourcemap: Option<SourcemapArg>,

        /// Replace an expression at build time, repeatable. Values are JavaScript
        /// expressions, so a string needs its own quotes:
        /// `--define 'API="https://example.com"'`.
        #[arg(long, value_name = "KEY=VALUE", action = ArgAction::Append)]
        define: Vec<String>,

        /// Replace an expression at build time with a file's contents, repeatable.
        /// The file holds the JavaScript expression `--define` would take, for a
        /// value too big for a command line:
        /// `--define-file MODELS=./models.json`.
        #[arg(long = "define-file", value_name = "KEY=PATH", action = ArgAction::Append)]
        define_file: Vec<String>,

        /// Embed a file or directory in the executable, byte for byte, repeatable.
        /// Accepts globs. Embedded files are extracted beside the compiled entry,
        /// keeping the layout they had in your source tree, so the app reads them
        /// through the same relative paths it always did.
        #[arg(long, value_name = "PATH", action = ArgAction::Append)]
        include: Vec<String>,

        /// Leave a path out of what `--include` embeds, repeatable. Accepts globs.
        /// A pattern that matches nothing is ignored.
        #[arg(long, value_name = "PATH", action = ArgAction::Append)]
        exclude: Vec<String>,

        /// Start the binary's Node with these options, spelled like the
        /// `NODE_OPTIONS` environment variable and repeatable. For a program
        /// that only works behind a flag, which the person running your binary
        /// cannot supply for you:
        /// `--node-options "--experimental-vm-modules --max-old-space-size=4096"`.
        /// Whoever runs the binary can still set `NODE_OPTIONS` themselves; the
        /// two are additive.
        #[arg(
            long = "node-options",
            value_name = "OPTIONS",
            action = ArgAction::Append,
            allow_hyphen_values = true
        )]
        node_options: Vec<String>,

        /// Icon to show on a Windows executable, as a `.ico` file. Works when
        /// cross-compiling, so a Windows binary built on macOS or Linux gets its
        /// icon too. Windows carries the icon inside the executable; macOS and
        /// Linux read one from a bundle or desktop entry, so the flag is refused
        /// for those targets rather than silently ignored.
        #[arg(long = "icon", value_name = "FILE")]
        icon: Option<PathBuf>,

        /// Windows version-resource field, as `Key=value`; repeatable. These are
        /// the fields Explorer's Details tab shows. Defaults come from
        /// `package.json` (name, version, description, author), so most builds
        /// need no flag; `Key=` drops a defaulted field. Known keys: Comments,
        /// CompanyName, FileDescription, FileVersion, InternalName,
        /// LegalCopyright, LegalTrademarks, OriginalFilename, PrivateBuild,
        /// ProductName, ProductVersion, SpecialBuild. Works when cross-compiling,
        /// and is refused for a non-Windows target rather than ignored.
        #[arg(
            long = "metadata",
            value_name = "KEY=VALUE",
            action = ArgAction::Append
        )]
        metadata: Vec<String>,

        /// Custom message the compiled binary shows on a terminal while it sets
        /// itself up on first run. Default: `Initializing...`.
        #[arg(long, value_name = "TEXT")]
        install_message: Option<String>,

        /// Remove a category of call at build time, repeatable:
        /// `--drop console --drop debugger`. A dropped call is not evaluated, so
        /// an argument with a side effect goes with it. Needs minification, which
        /// is on by default.
        #[arg(long, value_name = "NAME", action = ArgAction::Append)]
        drop: Vec<DropArg>,

        /// Write a build report to this path, in esbuild's metafile JSON schema:
        /// every module the bundler read, every file it emitted, and what each
        /// module contributed to each. Reads in esbuild's `analyzeMetafile`,
        /// esbuild-visualizer, and bundle-buddy.
        #[arg(long, value_name = "PATH")]
        metafile: Option<String>,

        /// Let minification rename functions and classes. Names are preserved by
        /// default: minified class names break frameworks that key on them
        /// (dependency injection, ORM entities, class registries).
        #[arg(long = "no-keep-names", help_heading = COMPILE_ADVANCED)]
        no_keep_names: bool,

        /// Keep every module's side effects, for a dependency that declares
        /// itself pure and is not. Tree-shaking is on by default.
        #[arg(long = "no-treeshake", help_heading = COMPILE_ADVANCED)]
        no_treeshake: bool,

        /// Ignore `/*@__PURE__*/` annotations while tree-shaking.
        #[arg(long = "ignore-annotations", help_heading = COMPILE_ADVANCED)]
        ignore_annotations: bool,

        /// Resolve one specifier as another, repeatable: `--alias lodash=lodash-es`.
        #[arg(long, value_name = "FROM=TO", action = ArgAction::Append, help_heading = COMPILE_ADVANCED)]
        alias: Vec<String>,

        /// Choose what importing a file extension evaluates to, repeatable:
        /// `--loader .html=file`. Types: `file` (embeds the file and yields its
        /// path), `text`, `json`, `base64`, `dataurl`, `binary`, `empty`.
        #[arg(long, value_name = "EXT=TYPE", action = ArgAction::Append, help_heading = COMPILE_ADVANCED)]
        loader: Vec<String>,

        /// Extra `exports` condition to honor, repeatable. Added to the
        /// defaults rather than replacing them.
        #[arg(long, value_name = "NAME", action = ArgAction::Append, help_heading = COMPILE_ADVANCED)]
        conditions: Vec<String>,

        /// Leave a package out of the bundle and resolve it at run time from the
        /// directory the binary is run in, repeatable. Covers the package and
        /// its subpaths. The package must be installed on the target machine.
        #[arg(long, value_name = "PKG", action = ArgAction::Append, help_heading = COMPILE_ADVANCED)]
        external: Vec<String>,

        /// Ship a package unbundled INSIDE the binary, in its own installed
        /// layout, repeatable. For a package that loads a file by a path it
        /// computes at run time — a worker script, a data file, an addon that
        /// detection did not recognise.
        ///
        /// Distinct from `--external`, which leaves the package OUT of the binary
        /// to be resolved on the target machine. This one still carries it.
        #[arg(long, value_name = "PKG", action = ArgAction::Append, help_heading = COMPILE_ADVANCED)]
        unbundled: Vec<String>,

        /// Bundle a package that would otherwise ship unbundled, repeatable.
        ///
        /// The escape hatch for detection firing on a package that does not need
        /// it — a false positive costs that package its tree-shaking, and waiting
        /// on a nub release to correct it is worse than a flag.
        #[arg(long, value_name = "PKG", action = ArgAction::Append, help_heading = COMPILE_ADVANCED)]
        bundled: Vec<String>,

        /// Keep a dynamic `import()` whose specifier the program computes at run
        /// time — a plugin loader, a config module. Such an import is refused by
        /// default: the binary resolves it from the directory it is run in, so
        /// what it loads depends on the machine you ship to.
        #[arg(long = "allow-dynamic-import", help_heading = COMPILE_ADVANCED)]
        allow_dynamic_import: bool,

        /// Use this tsconfig.json instead of the one discovered from the entry.
        #[arg(long, value_name = "PATH", help_heading = COMPILE_ADVANCED)]
        tsconfig: Option<String>,

        /// Exclude the original source text from the source map.
        #[arg(long = "sourcemap-exclude-sources", help_heading = COMPILE_ADVANCED)]
        sourcemap_exclude_sources: bool,
    },

    /// Scaffold a new TypeScript-first project.
    Init {
        /// Non-interactive: skip all prompts and take the defaults.
        #[arg(short = 'y', long)]
        yes: bool,

        /// JavaScript variant: `index.js`, no tsconfig.json, no type devDeps.
        #[arg(long)]
        js: bool,

        /// Project name (default: the directory name, sanitized).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,

        /// Skip `git init`.
        #[arg(long = "no-git")]
        no_git: bool,

        /// Skip the `nub install` step.
        #[arg(long = "no-install")]
        no_install: bool,

        /// Overwrite existing files (default: refuse and list conflicts).
        #[arg(long)]
        force: bool,

        /// Rejected with a `nubx create-<template>` hint — `init` takes no
        /// positionals (pnpm parity).
        #[arg(trailing_var_arg = true, hide = true)]
        args: Vec<String>,
    },

    /// Upgrade Nub to the latest version.
    Upgrade {
        /// Target version (default: latest).
        #[arg(long, conflicts_with_all = ["canary", "stable"])]
        version: Option<String>,

        /// Upgrade to the latest canary build (rebuilt from every commit).
        #[arg(long, conflicts_with = "stable")]
        canary: bool,

        /// Upgrade to the latest stable release — the default on stable
        /// builds; on a canary build this opts back out of the canary channel.
        #[arg(long)]
        stable: bool,

        /// Show what would happen without performing the upgrade.
        #[arg(long)]
        dry_run: bool,

        /// Accepted for scripted use; `nub upgrade` never prompts.
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

        #[command(flatten)]
        output: crate::pm_engine::OutputFlags,

        #[command(flatten)]
        age_gate: crate::pm_engine::AgeGateFlags,

        /// Which platforms' optional dependencies to install
        /// (`--os`/`--cpu`/`--libc`), overriding host detection for this
        /// run only. Mirrors pnpm's flags of the same names.
        #[command(flatten)]
        platform: crate::pm_engine::PlatformFlags,
    },

    /// Clean install for CI: delete node_modules, install strictly from the
    /// lockfile (drift or a missing lockfile is a hard error).
    Ci {
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

        #[command(flatten)]
        output: crate::pm_engine::OutputFlags,

        #[command(flatten)]
        age_gate: crate::pm_engine::AgeGateFlags,

        /// Which platforms' optional dependencies to install
        /// (`--os`/`--cpu`/`--libc`), overriding host detection for this
        /// run only. Mirrors pnpm's flags of the same names.
        #[command(flatten)]
        platform: crate::pm_engine::PlatformFlags,
    },
}

/// The `nub node` version-management verbs. Spec: `internal/commands/node-versions.md`.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorWhen {
    Auto,
    Always,
    Never,
}

/// Record the resolved `--color` choice. Called from the pre-subcommand argv scan
/// and again from the parsed clap `Cli`, so either spelling position takes effect.
pub(crate) fn set_color_mode(when: ColorWhen) {
    let raw = match when {
        ColorWhen::Auto => COLOR_AUTO,
        ColorWhen::Always => COLOR_ALWAYS,
        ColorWhen::Never => COLOR_NEVER,
    };
    COLOR_MODE.store(raw, Ordering::Relaxed);
}

pub(crate) fn color_mode() -> ColorWhen {
    match COLOR_MODE.load(Ordering::Relaxed) {
        COLOR_ALWAYS => ColorWhen::Always,
        COLOR_NEVER => ColorWhen::Never,
        _ => ColorWhen::Auto,
    }
}

/// The one place Nub decides whether to emit ANSI to a stream. Precedence, highest
/// first: an explicit `--color`/`--no-color`, then `NO_COLOR`, then `FORCE_COLOR`,
/// then whether the stream is really a terminal.
///
/// It replaces an open-coded `is_terminal() || var_os("FORCE_COLOR").is_some()`,
/// which never consulted `NO_COLOR` on the stream-prefix path (so `NO_COLOR=1`
/// still produced a colored prefix, despite `--help` promising otherwise) and read
/// a mere presence as ON (so `FORCE_COLOR=0` switched color on).
///
/// One deliberate divergence from Node: when `FORCE_COLOR` and `NO_COLOR` are BOTH
/// set, Node lets `FORCE_COLOR` win and warns; Nub lets `NO_COLOR` win. That keeps
/// the promise `--help` prints and matches what the engine's own warning path
/// already did. Either way an explicit flag outranks both, which is the case a user
/// actually hits.
///
/// That divergence would otherwise split a run against itself — Nub's label plain,
/// the child's lines colored, because the child applies Node's order to the same two
/// variables. The script launcher closes it by exporting `FORCE_COLOR=0` for exactly
/// that contradictory pair; see the `ColorWhen::Auto` arm in `build_script_command`.
pub(crate) fn color_enabled(stream_is_tty: bool) -> bool {
    match color_mode() {
        ColorWhen::Always => return true,
        ColorWhen::Never => return false,
        ColorWhen::Auto => {}
    }
    // The NO_COLOR convention is "present and non-empty", so an empty value is
    // explicitly NOT an opt-out. Node spells the same check in `getColorDepth`.
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if let Some(v) = std::env::var_os("FORCE_COLOR") {
        return force_color_enables(&v);
    }
    stream_is_tty
}

/// Whether a `FORCE_COLOR` value turns color ON, per Node's `getColorDepth` table
/// (lib/internal/tty.js): only '', '1', 'true', '2' and '3' enable — every other
/// value, '0' and 'false' included, falls through to its 2-color monochrome branch.
/// An EMPTY value is Node's shortest spelling of ON, which is why it cannot be
/// lumped in with '0'.
///
/// Shared by [`color_enabled`] and the script launcher so the two can never drift:
/// Nub hands this variable to the children whose output it prefixes, and a value
/// Nub read as ON while Node read it as OFF would split the two apart.
fn force_color_enables(v: &std::ffi::OsStr) -> bool {
    matches!(
        v.to_str(),
        Some("") | Some("1") | Some("true") | Some("2") | Some("3")
    )
}

/// `nub compile`'s power set. Grouping it under its own `--help` heading (the
/// shape esbuild uses) is what keeps the common six flags readable while the
/// bundler knobs stay discoverable.
#[cfg(feature = "compile")]
const COMPILE_ADVANCED: &str = "Advanced options";

#[cfg(feature = "compile")]
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SourcemapArg {
    /// A `.map` shipped inside the executable, referenced by the bundle.
    Linked,
    /// A base64 data URI in the bundle itself.
    Inline,
    /// A `.map` written beside the executable and not shipped.
    External,
    None,
}

/// What `--drop` accepts. A closed set rather than a free string: the two are
/// what the bundler's compress pass can remove, and an unrecognised name would
/// otherwise be a build that silently drops nothing.
#[cfg(feature = "compile")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DropArg {
    /// Every `console.*()` call.
    Console,
    /// Every `debugger` statement. Minification already removes these, so this
    /// pins the guarantee rather than changing what is emitted.
    Debugger,
}

/// Top-level entry point. Returns the process exit code.
pub fn run() -> Result<i32> {
    // The macOS parent-death watcher (#480) re-invokes `current_exe()` under
    // the private launcher mode, with its `<child-pgid> <read-fd>` payload as
    // ordinary arguments. `current_exe()` is whatever NAME nub is running
    // under — `node` for workloads spawned through nub's PATH shim — so this
    // must dispatch before `Argv0::detect()`. Otherwise `Argv0::Node` would
    // treat the payload as an application script, spawning another watcher per
    // level (regression from #504).
    //
    // The legacy `__pdeath-watch` hidden token remains accepted for already
    // built callers, but new watchers select the mode through the internal env
    // channel so no application argument is reserved. Both forms require the
    // exact two-item watcher payload before they bypass normal CLI dispatch.
    // Landing above the guards below is also deliberate: the watcher must not
    // reclaim or reap PATH shim dirs, which belong to the nub that spawned it.
    #[cfg(unix)]
    {
        let args: Vec<String> = env::args().skip(1).collect();
        let env_mode = env::var("__NUB_COMPILED_LAUNCHER_MODE").ok();
        let pdeath_args = if env_mode.as_deref() == Some("pdeath-watch") && args.len() == 2 {
            Some(args.as_slice())
        } else if args.first().map(String::as_str) == Some("__pdeath-watch") && args.len() == 3 {
            Some(&args[1..])
        } else {
            None
        };
        if let Some(pdeath_args) = pdeath_args {
            return Ok(nub_core::node::spawn::run_pdeath_watch(pdeath_args));
        }
    }

    // Reclaim the active PATH shim temp dir exactly once, on return. The shim
    // is created lazily/idempotently by the spawn paths and is process-wide; it
    // must outlive every — possibly parallel — child, so
    // cleanup belongs here at the top level, not per-spawn (which would race
    // concurrent workspace scripts). Drop runs on every return path, including
    // errors, because `run()` returns to `main` rather than calling `exit`.
    // Invalid retired records are left for a later dead-process reaper because
    // their pathname identity can no longer be trusted.
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

    // The engine's lazy node-gyp shims re-invoke `current_exe()` with this hidden
    // verb mid-lifecycle-script, and `current_exe()` carries whatever NAME nub is
    // running under — a PM shim when the install was driven through one. Force the
    // `nub` identity so the verb resolves exactly as it does under argv0 `nub`;
    // left to the argv0 match below, a shim-named re-invocation instead hands the
    // verb to the PM engine (`ERR_PNPM_RECURSIVE_EXEC_FIRST_FAIL`), runs it as a
    // SCRIPT under `node`, or — under `nubx` — tries to FETCH it from the registry
    // as a package name. Same argv0-coupling class as the parent-death watcher
    // above; that one wants the minimal short-circuit, this one wants the full
    // `run_nub` path (its bootstrap relies on the guards above).
    if !matches!(argv0, Argv0::Nub) && env::args().nth(1).as_deref() == Some("__node-gyp-bootstrap")
    {
        return run_nub();
    }

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

/// Workspace execution options extracted from clap flags. The field set is
/// pnpm's recursive-execution surface, not a nub invention — selector parsing,
/// graph traversal, topological chunking and the flag interactions between
/// `--parallel` / `--no-sort` / `--workspace-concurrency` all mirror it.
// @lat: [[research/pnpm-filter-grammar]]
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
    "run",
    "watch",
    "exec",
    "upgrade",
    "help",
    "node",
    "pm",
    "agent",
    "global",
    "install",
    "i",
    "ci",
    "init",
    // Only a real verb in a `--features compile` build; the variant and handler
    // are gated the same way so feature-off development builds reject it cleanly.
    #[cfg(feature = "compile")]
    "compile",
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
            // Output-control flags: their space-separated value would otherwise
            // be read as a package positional and trigger a wrong route to `add`.
            // (`--loglevel silent` is the canonical misroute case.) The install
            // clap variant accepts them via the flattened `OutputFlags`; listing
            // them here prevents the space-separated value from looking like a pkg.
            "--loglevel",
            "--reporter",
            // Same shape: `nub install --minimum-release-age 0` would otherwise
            // read `0` as a package and route the whole command to `add`.
            "--minimum-release-age",
            "--minimum-release-age-exclude",
            // Platform selection, same shape: `nub install --os linux` would
            // otherwise read `linux` as a package spec and route the whole
            // command to `add`. The `--os=linux` form never had the problem,
            // which is what makes omitting these easy to ship unnoticed.
            "--os",
            "--cpu",
            "--libc",
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
/// surface (`package-manager-normalized-surface` (no such document)):
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
    let mut color_when: Option<ColorWhen> = None;
    // Pre-verb PM output flags (`nub --reporter=silent install`,
    // `nub --loglevel=error add foo`): captured here and recorded as process
    // defaults below so the per-verb `OutputFlags` resolution can fall back to
    // them (per-verb always wins). Without these arms a leading `--reporter`/
    // `--loglevel` falls through to the file-run path and is shipped to Node
    // (`node: bad option`). `--silent`/`-s` is already captured above.
    let mut reporter_val: Option<String> = None;
    let mut loglevel_val: Option<String> = None;
    // Top-level `--node`: provision the project's Node (version management stays
    // on) but run with zero augmentation — the compat escape hatch. Routed to
    // `run_file_with_compat(_, true)`. See internal/commands/node.md.
    let mut compat = false;
    let mut rest: Vec<String> = Vec::new();
    let mut subcommand_found = false;
    let mut env_file_vars: std::collections::HashMap<String, String> = Default::default();
    // Parallel pre-strip merge + the flag paths themselves, retained for the
    // watch path's --env-file forwarding (#479). See ENV_FILE_VARS_RAW /
    // ENV_FILE_PATHS.
    let mut env_file_raw_vars: std::collections::HashMap<String, String> = Default::default();
    let mut env_file_paths: Vec<(PathBuf, bool)> = Vec::new();
    // Tracks whether any `--env-file` flag appeared, independent of whether it
    // yielded vars (an empty file still counts as intent). Drives suppression of
    // the eager `.env*` auto-discovery.
    let mut env_file_present = false;
    // `--no-env-file`: load zero env files. Wins over `--env-file` regardless of
    // order (both are captured, the flag decides at the end of the scan).
    let mut no_env_file = false;
    let mut no_check = false;

    let mut i = 0;
    while i < raw_args.len() {
        let arg = &raw_args[i];
        // Once a subcommand (`run`/`exec`/`watch`/…) has been seen, stop matching
        // Nub's own flags: everything after it is that subcommand's argv and is
        // handed to clap verbatim, whose `trailing_var_arg` forwards post-
        // positional flags to the script/bin. Without this, `nub exec tsc
        // --version` would print Nub's version instead of tsc's, and
        // `nub run build --watch` would steal `--watch` from the script (the
        // three-position rule — see internal/commands/run.md).
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
            // The pre-run dependency-freshness opt-out on the top-level file
            // runner (`nub --no-check foo.ts`). Consumed here so it never reaches
            // Node as an unknown flag; after the entry point it forwards to the
            // script (the three-position rule). `--no-install` is the alias.
            "--no-check" | "--no-install" => {
                no_check = true;
                crate::verify_deps::disable();
            }
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
            // `--color` (bare = always), `--color=<when>`, `--no-color`: consumed
            // here so they never reach Node as unknown flags, and RECORDED so the
            // choice actually takes effect. Dropping the value on the floor left
            // `--color=always` a documented no-op (#685): it is listed in
            // `nub --help`, and the only test covering it drove clap directly, which
            // never sees these tokens because this scan strips them first.
            s if s == "--color" || s.starts_with("--color=") || s == "--no-color" => {
                color_when = Some(if s == "--no-color" {
                    ColorWhen::Never
                } else {
                    match s.split_once('=').map(|(_, v)| v) {
                        // Bare `--color` means always, matching clap's
                        // `default_missing_value` for the same flag.
                        None | Some("always") => ColorWhen::Always,
                        Some("never") => ColorWhen::Never,
                        // `auto`, and anything unrecognized: fall back to the
                        // default rather than failing. This scan runs before the
                        // command is even known and has no error channel, and the
                        // post-verb spelling still gets clap's value validation.
                        _ => ColorWhen::Auto,
                    }
                });
            }
            // Pre-verb PM output flags. `--reporter`/`--loglevel` only appear
            // here BEFORE a subcommand (after the verb they belong to the
            // verb's own clap surface and never reach this scan). Captured, not
            // forwarded to Node; recorded as process defaults below. The value
            // is validated where it's recorded (clean usage error on a bad
            // spelling) — a separate concern from grabbing the token here.
            "--reporter" => {
                i += 1;
                if i < raw_args.len() {
                    reporter_val = Some(raw_args[i].clone());
                }
            }
            s if s.starts_with("--reporter=") => {
                reporter_val = Some(s["--reporter=".len()..].to_string());
            }
            "--loglevel" => {
                i += 1;
                if i < raw_args.len() {
                    loglevel_val = Some(raw_args[i].clone());
                }
            }
            s if s.starts_with("--loglevel=") => {
                loglevel_val = Some(s["--loglevel=".len()..].to_string());
            }
            // `--no-env-file` suppresses ALL env-file loading (auto-discovery +
            // any explicit `--env-file`). Consumed here so it never reaches Node
            // (which has no such flag); after the entrypoint it forwards to the
            // script (the three-position rule). Wins over `--env-file` regardless
            // of order — the two flags may both appear, this decides at scan end.
            "--no-env-file" => no_env_file = true,
            s if s == "--env-file"
                || s.starts_with("--env-file=")
                || s == "--env-file-if-exists"
                || s.starts_with("--env-file-if-exists=") =>
            {
                // `--env-file-if-exists` mirrors Node v22: load the file if present,
                // skip SILENTLY when it is absent — vs `--env-file`, which errors on
                // a missing file. Everything else is identical: same last-writer
                // merge, same `${VAR}` expansion, and the flag's presence opts the
                // run out of eager `.env*` auto-discovery either way (even when the
                // if-exists target turns out to be absent — the user named explicit
                // file(s), so guessing is off).
                let if_exists = s.starts_with("--env-file-if-exists");
                let prefix = if if_exists {
                    "--env-file-if-exists="
                } else {
                    "--env-file="
                };
                env_file_present = true;
                let file_path = if let Some(v) = s.strip_prefix(prefix) {
                    v.to_string()
                } else {
                    i += 1;
                    if i < raw_args.len() {
                        raw_args[i].clone()
                    } else {
                        String::new()
                    }
                };
                if !file_path.is_empty() {
                    let path = std::path::Path::new(&file_path);
                    // Absolutize against the CURRENT cwd — the directory
                    // read_env_file resolves the relative path from right below
                    // (this scan runs before any `--cwd` chdir) — so the watch
                    // path's forwarding names the same file the parse read.
                    let abs_path = if path.is_absolute() {
                        Some(path.to_path_buf())
                    } else {
                        env::current_dir().ok().map(|d| d.join(path))
                    };
                    // `--env-file-if-exists`: a non-existent file is a silent no-op
                    // (Node's whole reason for the flag). A file that DOES exist but
                    // can't be read still surfaces the error, matching Node — only
                    // the missing-file case is suppressed.
                    if if_exists && !path.exists() {
                        // Silent no-op for values — but still retained for watch
                        // forwarding: Node's own `--env-file-if-exists` no-ops on
                        // an absent file too, and a restarted child picks the file
                        // up if it appears later (parity).
                        if let Some(abs) = abs_path {
                            env_file_paths.push((abs, if_exists));
                        }
                    } else if let Some(content) =
                        // read_env_file refuses non-regular files (e.g. /dev/zero) and
                        // oversized files, so a hostile --env-file can't hang or OOM.
                        nub_core::workspace::env::read_env_file(path)
                    {
                        if let Some(abs) = abs_path {
                            env_file_paths.push((abs, if_exists));
                        }
                        // Route through parse_env (not dotenvy directly) so the
                        // explicit --env-file flag strips backtick-quoted values
                        // like Node's parser, matching the .env auto-load path.
                        for (k, v) in nub_core::workspace::env::parse_env(&content) {
                            // Last-writer-wins across multiple `--env-file` flags,
                            // matching Node: "subsequent files override pre-existing
                            // variables defined in previous files" (doc/api/cli.md).
                            // Shell env still wins over all of them — enforced later
                            // in `overlay_env_file_vars` (skips keys already set).
                            env_file_raw_vars.insert(k.clone(), v.clone());
                            env_file_vars.insert(k, v);
                        }
                    } else {
                        // Warn-and-continue (softer than Node's hard error) — and do
                        // NOT retain the path: forwarding an unreadable file would
                        // turn the warning into a per-restart child error under watch.
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
                // re-invoke current_exe() with it mid-lifecycle-script). The
                // parent-death watcher's verb is handled in `run()`, above
                // argv0 dispatch, so it never reaches here.
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

    // ── nub self-shim ── honor `packageManager: "nub@x.y.z"` by provisioning that
    // exact nub and delegating to it (crates/nub-cli/src/self_shim.rs). Gated on
    // the resolved verb FIRST — a pure allowlist string compare, so the flagship
    // `nub <file>` / `run` / `nubx` / `node` / `upgrade` paths pay only that and
    // never read the manifest. Placed BEFORE the `--cwd` set_current_dir below so
    // a delegated child re-applies `--cwd` from the original directory exactly
    // once (delegation execs the ORIGINAL argv from the unchanged cwd).
    if subcommand_found
        && let Some(version) = crate::self_shim::delegate_target(&rest, cwd.as_deref())
    {
        return delegate_to_self(&version);
    }

    // No `${VAR}` expansion here, deliberately: `--env-file` is the Node-compat
    // flag and Node's own parser delivers every value verbatim. Auto-discovered
    // `.env*` DO expand (load_env_files) — the asymmetry is the recorded decision
    // in wiki/research/env-file-loading.md, "expand in defaults, don't expand in
    // `--env-file=`". Expanding was a real compat bug: it silently truncated any
    // value holding a literal `$` (`PASSWORD=foo$bar` → `foo`) and failed Node's
    // own test/parallel/test-dotenv.js.

    // Env hygiene (Deno parity): ignore runtime-control vars (NODE_OPTIONS et al.)
    // from the explicit `--env-file` map too, so no env-file-sourced value silently
    // reconfigures the spawned Node — uniform with the auto-loaded `.env*` strip in
    // env.rs, and matching Deno, which ignores its control vars from `--env-file`.
    let denied_env_file_keys =
        nub_core::workspace::env::strip_denied_env_file_keys(&mut env_file_vars);
    nub_core::workspace::env::warn_denied_env_file_keys(&denied_env_file_keys);

    // Capture --env-file vars for per-child Command::env application (A19): no
    // process-env mutation, so no `unsafe { env::set_var }` and no data race if a
    // dep threads during init. Shell-wins / `.env`-override precedence is applied
    // at each spawn site via overlay_env_file_vars / apply_env_file_vars.
    let _ = ENV_FILE_VARS.set(env_file_vars);
    // Raw values + flag paths for the watch path's --env-file forwarding (#479).
    // The raw map deliberately keeps denied keys: watch_inject_vars iterates the
    // stripped ENV_FILE_VARS side, so a denied key is never injected, and the
    // forwarded file's copy is neutralized by the watch env guard's placeholder
    // pinning (Node's --env-file never overrides an already-set var).
    let _ = ENV_FILE_VARS_RAW.set(env_file_raw_vars);
    let _ = ENV_FILE_PATHS.set(env_file_paths);
    // Record flag presence so the run/watch paths can suppress eager `.env*`
    // auto-discovery when the user named explicit env file(s).
    let _ = ENV_FILE_PRESENT.set(env_file_present);
    // Record `--no-env-file` so every env-file consumer loads nothing.
    let _ = NO_ENV_FILE.set(no_env_file);

    SHOW_WARNINGS.store(show_warnings, Ordering::Relaxed);
    SILENT.store(silent, Ordering::Relaxed);
    if let Some(when) = color_when {
        set_color_mode(when);
    }

    // Record the pre-verb PM output flags as process defaults so a PM verb
    // dispatched below (`nub --silent install`, `nub --reporter=silent add foo`)
    // honors them — the per-verb clap flag still wins. A `run`/file-run path
    // simply doesn't read these defaults (run carries its own `--reporter`), so
    // they're inert there. Invalid `--reporter`/`--loglevel` values get the same
    // clean usage error clap gives for the per-verb form.
    if silent {
        crate::pm_engine::output::set_global_silent();
    }
    if let Some(ref value) = reporter_val
        && let Err(bad) = crate::pm_engine::output::set_global_reporter_str(value)
    {
        bail!(
            "invalid value '{bad}' for '--reporter <NAME>'\n  \
             [possible values: default, append-only, silent]"
        );
    }
    if let Some(ref value) = loglevel_val
        && let Err(bad) = crate::pm_engine::output::set_global_loglevel_str(value)
    {
        bail!(
            "invalid value '{bad}' for '--loglevel <LEVEL>'\n  \
             [possible values: silent, error, warn, info, debug]"
        );
    }

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
        // Unlike nub's own version/help below, this SPAWNS the resolved Node, so
        // it still needs the snapshot.
        if !subcommand_found {
            initialize_runtime_config_snapshot(compat, no_check)?;
        }
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

    // AFTER the version/help short-circuits: neither consumes project config, and
    // discovery walks unbounded to the filesystem root, so initializing ahead of
    // them let one malformed `nub.jsonc` anywhere above the cwd break the two
    // commands a user reaches for when something is already broken.
    if !subcommand_found {
        initialize_runtime_config_snapshot(compat, no_check)?;
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
            // External-subcommand fallthrough (git `git-foo` / cargo `cargo-foo`):
            // an unknown bareword that isn't a built-in, a redirected PM/init verb,
            // or a likely script resolves to an executable named `nub-<verb>` and
            // runs it with the remaining argv forwarded verbatim (no clap, so every
            // flag passes through). Built-ins always win — they match in the
            // pre-verb scan above, so a plugin can never shadow one. We probe ONLY
            // the prefixed name (never the bare `first`), so a verb typo can't exec
            // a random PATH binary. Resolution order: project-local
            // node_modules/.bin (the nub-idiomatic place a plugin installs) then a
            // PATH search (a globally-installed plugin). launch_bin runs a JS/TS
            // plugin under nub's augmentation (the runtime value prop) and execs a
            // native one. If nothing resolves, fall through to the bail below.
            //
            // PnP gap (deliberate, additive): unlike the nubx/exec path, this
            // does NOT fall back to the PnP bin-runner when find_bin misses in a
            // Yarn-PnP tree (no node_modules/.bin) — a plugin in a PnP project
            // resolves only via PATH, not via PnP. Symmetry with the sibling
            // resolver is a follow-up; PnP plugin support isn't documented.
            //
            // Resolution base reflects any `--cwd` already applied via
            // set_current_dir above; cwd is unchanged since then.
            let cwd = env::current_dir()?;
            let plugin_name = format!("nub-{first}");
            let plugin = nub_core::workspace::scripts::find_bin(&plugin_name, &cwd)
                .or_else(|| find_on_path(&plugin_name));
            if let Some(plugin_path) = plugin {
                return launch_bin(&plugin_path, &rest[1..], compat, &cwd);
            }
            // A bareword reaching here is not a known/conventional script (those
            // took the `nub run` branch above) — so it's most likely a dependency
            // binary the user meant to exec (`nub turbo login`, `nub eslint`).
            // Lead with the `nub exec`/`nubx` hint accordingly, then the script
            // and file fallbacks.
            bail!(
                "nub: \"{first}\" is not a nub command — see `nub --help`\n\
                 \x20\x20(to run a dependency's binary: nub exec {first} or nubx {first} · \
                 to run a script: nub run {first} · to run a file: nub ./{first})"
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
            "--reporter",
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
        // The age-gate value flags take a following token, so `nubx
        // --minimum-release-age 1d cowsay` must bind `1d` to the flag rather
        // than read it as the bin. Both age-gate flags are value-taking; there
        // is no boolean sibling, since nub ships no strictness flag.
        "nubx" => &[
            "--filter",
            "-F",
            "--workspace",
            "--workspace-concurrency",
            "--package",
            "-p",
            "--cwd",
            "--minimum-release-age",
            "--minimum-release-age-exclude",
            // Platform selection is value-taking too, and the sibling list in
            // `install_to_add_args` needed the same three. `nubx --os linux -p
            // left-pad pad` would otherwise split at `linux`, pushing `-p
            // left-pad` into the forwarded suffix instead of binding it.
            "--os",
            "--cpu",
            "--libc",
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
        // First bare token after the subcommand = the positional. It TERMINATES
        // runner parsing: everything after it is the target's, forwarded VERBATIM
        // — including any `--`, which is NOT stripped and NOT special-cased
        // (Option A, decided 2026-06-28). Matches the gold standard — real `node
        // <file> -- a`, `pnpm 10 run build -- a`, and `pnpm 10 exec bin -- a` all
        // keep the `--` in the child's argv — and the already-correct `nub <file>`
        // runner. Uniform across run/exec/watch/nubx; no per-subcommand carve-out.
        // (`nub run build -- a b` → args `["--","a","b"]`.)
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
fn run_global(rest: &[String]) -> Result<i32> {
    const USAGE: &str = "Usage: nub global config <COMMAND> [OPTIONS]\n\n\
Commands:\n\
  get      Print a user setting\n\
  set      Write a user setting\n\
  delete   Remove a user setting\n\
  list     List user settings\n\
  init     Create the user nub.jsonc\n\
  path     Print the user nub.jsonc path\n";

    let Some(command) = rest.first() else {
        eprint!("nub global: expected `config`\n\n{USAGE}");
        return Ok(2);
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return Ok(0);
    }
    if !matches!(command.as_str(), "config" | "c") {
        eprint!("nub global: unknown command `{command}`\n\n{USAGE}");
        return Ok(2);
    }

    let Some(spec) = crate::pm_engine::lookup_verb("config") else {
        unreachable!("config is a registered engine verb")
    };
    let mut args = rest[1..].to_vec();
    // Put the selector immediately after the config subcommand. Besides
    // matching the documented spelling, this leaves Nub-owned `init`/`path`
    // in argv[0], where the config dispatcher claims them before the engine
    // parser. Bare `global config` has no subcommand, so the flag stands alone.
    if args.is_empty() {
        args.push("--global".to_string());
    } else {
        args.insert(1, "--global".to_string());
    }
    let pm = suggest_package_manager(&env::current_dir()?);
    crate::pm_engine::dispatch_verb(spec, "global config", &args, &pm)
}

fn dispatch_subcommand(rest: Vec<String>) -> Result<i32> {
    let subcommand = rest[0].clone();

    // `node` is a non-forwarding command group with bespoke bare-usage + invalid-
    // positional messages (spec: internal/commands/node-versions.md). Handle it with a
    // manual sub-verb match rather than clap's generic "invalid subcommand" error,
    // so `nub node script.ts` yields the exact "use 'nub <file>'" guidance and bare
    // `nub node` prints the verb list instead of a clap usage error.
    // No snapshot here, and none for `pm` below: neither group reads project
    // config, and the sub-verbs that need an engine session build one
    // themselves. Initializing would let a malformed `nub.jsonc` in any
    // ancestor block `nub node install` — one of the commands you reach for
    // when the toolchain is already broken.
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
    // clap dispatch. Spec: .fray/ai-friendliness.md. Print-only, so it reads no
    // project config and never initializes the snapshot — a malformed `nub.jsonc`
    // in some ancestor must not silence the offline docs.
    if subcommand == "agent" {
        return crate::agent::run(&rest[1..]);
    }

    // `global config ...` is Nub's prefix spelling for the same user-config
    // scope as `config ... --global`. Keep one implementation by injecting the
    // selector and routing to the existing config family; no second config
    // parser or storage semantics can drift from it.
    if subcommand == "global" {
        return run_global(&rest[1..]);
    }

    // The engine's lazy node-gyp shims re-invoke `current_exe()` (= nub)
    // with this hidden verb mid-lifecycle-script; intercept it before clap
    // (it's internal plumbing, not a documented verb) and dispatch straight
    // to the engine's bootstrap entry point.
    if subcommand == "__node-gyp-bootstrap" {
        initialize_config_snapshot(false, false)?;
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
    // Only a non-default value, so `nub --color=never run build` (recorded by the
    // position-1 scan) isn't reset to Auto by clap's default on this second pass.
    if cli.no_color {
        set_color_mode(ColorWhen::Never);
    } else if cli.color != ColorWhen::Auto {
        set_color_mode(cli.color);
    }
    if let Some(ref dir) = cli.cwd {
        env::set_current_dir(dir)?;
    }
    let (config_node, config_no_check) = command_config_flags(&cli.command);
    match &cli.command {
        // Install-family commands own a verb-local `-C/--dir` that is parsed
        // below this point. Their engine session applies it first, then
        // initializes the one process snapshot from that final cwd.
        Some(Command::Install { .. } | Command::Ci { .. }) => {}
        // Self-update, the scaffold, and the help pages consume no project
        // config, and `upgrade` is a plausible remedy for whatever broke it —
        // none of them may be gated on a file they never read. `init`'s
        // in-process install initializes the snapshot itself, from the
        // scaffold's final cwd.
        Some(Command::Init { .. } | Command::Upgrade { .. } | Command::Help { .. }) => {}
        // The runtime entrypoints, where forced compat degrades an unloadable
        // file rather than aborting.
        Some(
            Command::Run { .. }
            | Command::Watch { .. }
            | Command::Exec { .. }
            | Command::Nubx { .. },
        ) => initialize_runtime_config_snapshot(config_node, config_no_check)?,
        _ => initialize_config_snapshot(config_node, config_no_check)?,
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
            if_present,
            no_check,
            ignore_scripts,
            script_shell,
            aggregate_output,
            resume_from,
            mut args,
        }) => {
            args.extend(suffix);
            if no_check {
                crate::verify_deps::disable();
            }
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
            no_check,
            mut args,
        }) => {
            args.extend(suffix);
            if no_check {
                crate::verify_deps::disable();
            }
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
            yes,
            ignore_existing,
            no_check,
            age_gate,
            platform,
            mut args,
        }) => {
            args.extend(suffix);
            if no_check {
                crate::verify_deps::disable();
            }
            // Only the DLX fallback below resolves from the registry, but the
            // bag is inert on the local-bin path, so publish unconditionally
            // rather than duplicating the call into each branch.
            age_gate.apply();
            platform.apply();
            filter.extend(workspace);
            let recursive = recursive || parallel || include_workspace_root;
            let workspace_run = recursive || !filter.is_empty() || parallel;

            // `--ignore-existing` was removed from npm v9+; accept it for muscle
            // memory but warn that it does nothing (matches real npx, which prints
            // a removed-argument notice and proceeds).
            if ignore_existing {
                eprintln!("nubx: --ignore-existing was removed in npm v9 and is ignored.");
            }

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
                // Local bin → registry, npx's model. `run_exec_with_dlx` owns the
                // tail: it resolves `node_modules/.bin` first and only a miss reaches
                // the consent-gated fetch.
                let dlx_flags = NubxDlxFlags {
                    package,
                    // `--no` is npx's alias for `--no-install`: refuse to fetch.
                    no_install: no_install || no_fetch,
                    quiet,
                    yes,
                };
                run_exec_with_dlx(&bin, node, &args, Some(&dlx_flags))
            }
        }
        #[cfg(feature = "compile")]
        Some(Command::Compile {
            entry,
            out,
            smol,
            target,
            platform,
            no_minify,
            sourcemap,
            define,
            define_file,
            include,
            exclude,
            install_message,
            drop,
            metafile,
            node_options,
            icon,
            metadata,
            no_keep_names,
            no_treeshake,
            ignore_annotations,
            alias,
            loader,
            conditions,
            external,
            unbundled,
            bundled,
            allow_dynamic_import,
            tsconfig,
            sourcemap_exclude_sources,
        }) => crate::compile::run(crate::compile::CompileOptions {
            entry,
            out,
            smol,
            target,
            platform,
            include,
            exclude,
            install_message,
            define_file,
            node_options,
            icon,
            metadata,
            metafile: metafile.as_deref().map(PathBuf::from),
            bundle: crate::compile::BundleOptions {
                minify: !no_minify,
                keep_names: !no_keep_names,
                // Off by default: a compiled artifact is something you SHIP, and a
                // map is source. Inline was the old default and cost 81% of the
                // bundle's bytes (1.04 MB of 1.29 MB on a hello world); `linked`
                // removed the load-time parse but still shipped 780 KB of map into
                // every binary. Neither belongs in a distributed executable unless
                // the publisher asks for it.
                //
                // The tradeoff, stated plainly because it is real: with no map an
                // uncaught error in a compiled TypeScript program points into
                // minified JS. `--sourcemap=linked` restores exact original-source
                // traces (verified: a throwing fixture reports `throw.ts:2` with the
                // TypeScript source line) at ~0 startup cost, so it is the setting to
                // reach for when debugging a shipped binary matters.
                sourcemap: match sourcemap.unwrap_or(SourcemapArg::None) {
                    SourcemapArg::Linked => crate::compile::SourcemapMode::Linked,
                    SourcemapArg::Inline => crate::compile::SourcemapMode::Inline,
                    SourcemapArg::External => crate::compile::SourcemapMode::External,
                    SourcemapArg::None => crate::compile::SourcemapMode::None,
                },
                sources_content: !sourcemap_exclude_sources,
                define,
                auto_define: Vec::new(),
                tree_shake: !no_treeshake,
                ignore_annotations,
                alias,
                conditions,
                external,
                unbundled,
                bundled,
                allow_dynamic_import,
                tsconfig: tsconfig.map(PathBuf::from),
                loaders: loader,
                native_target: None,
                drop_console: drop.contains(&DropArg::Console),
                drop_debugger: drop.contains(&DropArg::Debugger),
                metafile: metafile.is_some(),
                // Filled in by `compile()` once the pin chain resolves the exact
                // target Node; the CLI layer does not know it yet.
                target_node: None,
            },
        }),
        Some(Command::Init {
            yes,
            js,
            name,
            no_git,
            no_install,
            force,
            args,
        }) => crate::init::run_init(crate::init::InitOptions {
            yes,
            js,
            name,
            no_git,
            no_install,
            force,
            args,
        }),
        Some(Command::Upgrade {
            version,
            canary,
            stable,
            dry_run,
            yes,
        }) => run_upgrade(version.as_deref(), canary, stable, dry_run, yes),
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
            output,
            age_gate,
            platform,
        }) => {
            age_gate.apply();
            platform.apply();
            crate::pm_engine::run_install(crate::pm_engine::InstallFlags {
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
                output,
            })
        }
        Some(Command::Ci {
            prod,
            dev,
            ignore_scripts,
            no_optional,
            registry,
            dir,
            filter,
            filter_prod,
            recursive,
            fail_if_no_match,
            include_workspace_root,
            output,
            age_gate,
            platform,
        }) => {
            age_gate.apply();
            platform.apply();
            crate::pm_engine::run_ci(crate::pm_engine::CiFlags {
                prod,
                dev,
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
                output,
            })
        }
        // `node` is intercepted at the top of `dispatch_subcommand` (manual
        // sub-verb match in `run_node`) and never reaches clap here.
        Some(Command::Node { .. }) => unreachable!("`node` is handled before clap dispatch"),
        None => unreachable!(),
    }
}

fn run_nubx() -> Result<i32> {
    // `nubx <bin> [args...]` is nub's `npx`: a locally-installed bin runs out of
    // `node_modules/.bin` with no network, and a local miss falls back to fetching
    // the tool and running it. Resolution is BINS-ONLY — a file is `nub <file>`, a
    // script is `nub run <script>`. (nubx briefly resolved those tiers too; that was
    // reverted, so a bare token here is always a bin/package name.)
    //
    // The registry fallthrough is IMPLICIT, so it is consent-gated where it happens
    // in `run_exec_with_dlx`: prompt once per spec in a terminal, fail closed in CI
    // and any non-TTY, `-y` to bypass. Explicit `nub dlx` skips the gate entirely.
    //
    // Three-position rule: a flag BEFORE the bin (`nubx --node eslint`, `nubx -p
    // left-pad pad`) is nubx's; a flag AFTER it reaches the bin verbatim.
    let mut args: Vec<String> = env::args().skip(1).collect();

    // `--help`/`--version` are nubx's own only BEFORE the bin positional — after it
    // they belong to the bin (`nubx eslint --help` is eslint's help).
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

    // `--no-env-file` in the LEADING region. The `Nubx` clap grammar carries no
    // env-file flags, so this family has to be consumed here or clap rejects it as
    // unknown. `--no-env-file` WINS over `--env-file`, so the whole `--env-file*`
    // family is stripped alongside it. Both scans account for the space-form value
    // token so a flag's value is never mistaken for the bin; the leading-only scope
    // preserves a post-bin occurrence as the program's own arg.
    let is_env_file_value_flag = |a: &str| a == "--env-file" || a == "--env-file-if-exists";
    let mut no_env_file_seen = false;
    let mut idx = 0;
    while idx < args.len() {
        let a = args[idx].as_str();
        if a == "--" || !a.starts_with('-') {
            break; // subject / separator
        }
        if a == "--no-env-file" {
            no_env_file_seen = true;
        } else if is_env_file_value_flag(a) {
            idx += 1; // its value is not a subject — skip it
        }
        idx += 1;
    }
    if no_env_file_seen {
        let _ = NO_ENV_FILE.set(true);
        let mut stripped = Vec::with_capacity(args.len());
        let mut in_leading = true;
        let mut idx = 0;
        while idx < args.len() {
            let a = args[idx].as_str();
            if in_leading && (a == "--" || !a.starts_with('-')) {
                in_leading = false;
            }
            if in_leading {
                if a == "--no-env-file" {
                    idx += 1;
                    continue;
                }
                if is_env_file_value_flag(a) {
                    idx += 2; // drop the flag and its value token
                    continue;
                }
                if a.starts_with("--env-file=") || a.starts_with("--env-file-if-exists=") {
                    idx += 1;
                    continue;
                }
            }
            stripped.push(args[idx].clone());
            idx += 1;
        }
        args = stripped;
    }

    if args.is_empty() || args.iter().all(|a| a.starts_with('-') && a != "--") {
        // No bin name at all (empty, or only leading flags like `nubx --node`).
        bail!(
            "nubx: missing binary name\nUsage: nubx [-p <spec>] [--node] [--no-install] <bin> [args...]"
        );
    }

    // Arm the registry fallback for the exec path below — `nub exec` itself stays
    // no-network, so only the `nubx` entry point can reach the gated registry tier.
    NUBX_DLX_FALLBACK.store(true, Ordering::Relaxed);

    let mut rest = vec!["nubx".to_string()];
    rest.extend(args);
    dispatch_subcommand(rest)
}

fn run_as_node() -> Result<i32> {
    // Invoked as `node` via PATH shim. Default = full augmentation (Colin,
    // 2026-06-20: go all-in on the hijack). The single opt-out is `--node`: a
    // standalone `--node` in the LEADING option region is nub-owned — consume it
    // and run the resolved/pinned Node VANILLA (`compat_mode=true` — no preload,
    // no flag injection, no `.env`, no NODE_OPTIONS/NODE_PATH/PATH-shim), exactly
    // like `nub --node`/`nub run --node`. Version resolution/pinning stays on in
    // BOTH cases — that's the legitimate job of the hijack.
    let args: Vec<String> = env::args().skip(1).collect();
    let (compat_flag, forwarded) = scan_node_compat_flag(&args);
    // The PERSISTENT global `node` shim (`nub node shim`, ~/.nub/node-shim) runs
    // VANILLA by default (maintainer, 2026-07-03): a bare-shell `node` must
    // respect node semantics — version resolution/provisioning is the shim's
    // job, augmentation belongs to `nub`/`nubx`. The per-invocation hijack
    // (temp-dir shim, inside a `nub …` subtree the user opted into) keeps
    // augment-by-default. `--node`/`NODE_COMPAT` force vanilla in either case.
    let compat = compat_flag || nub_core::node::shim::invoked_as_persistent_node_shim();
    initialize_runtime_config_snapshot(compat, false)?;
    if compat {
        run_file_with_compat(&forwarded, true)
    } else {
        run_file(&forwarded)
    }
}

/// Leading-only `--node` scan for the `node` PATH-hijack. Real `node` reads
/// options until the ENTRY POINT — the script file, `-`/stdin, an `-e`/`--eval`
/// eval, or the token after `--` — then passes every remaining arg to the script
/// VERBATIM. nub's `--node` opt-out follows the same grammar: a standalone
/// `--node` in the LEADING option region is nub-owned (consume it, never forward
/// it to real Node, run vanilla); a `--node` AT or AFTER the entry point is the
/// SCRIPT's argument and is forwarded untouched. Returns `(compat, argv)` where
/// `argv` is the original args with the leading `--node` flags removed (equal to
/// the input when none were present). Fixes the prior strip, which removed
/// `--node` from anywhere before a `--` and so ate a program-arg `--node`
/// (`node app.js --node`), silently flipping compat off and dropping the arg.
///
/// Subject detection follows the blessed top-level `run_nub` file-run grammar
/// (stop at the first bare token or an `-e`/`-p` eval flag) and is arity-UNAWARE:
/// a value-flag's separate-token value (the `4096` in `--max-old-space-size
/// 4096`) reads as the entry and stops the scan. This is harmless because every
/// token from the entry onward is forwarded to real Node verbatim and Node does
/// the authoritative flag-vs-entry binding; the only input it affects is the rare
/// `--flag <bare-value> --node <entry>`, which forwards `--node` to Node (a loud
/// "bad option" error) exactly as `run_nub` does today. It additionally treats a
/// bare `--` as an explicit terminator (`run_nub` has no `--` arm) so a `--node`
/// after `--` is the script's, matching real node. Unify with the arity-aware
/// nubx resolver scan + `run_nub` once universal-nubx (#224) lands a shared
/// leading-only helper.
fn scan_node_compat_flag(args: &[String]) -> (bool, Vec<String>) {
    let mut compat = false;
    let mut forwarded = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        // `--` ends nub's option region: from here every token (including a
        // `--node`) is the script's — forward verbatim and stop interpreting.
        if arg == "--" {
            forwarded.extend(args[i..].iter().cloned());
            break;
        }
        // A standalone LEADING `--node` is nub's compat opt-out: consume it (never
        // forward to real Node) and flip compat. Only reachable while still in the
        // leading option region — the entry-point arm below breaks out first.
        if arg == "--node" {
            compat = true;
            i += 1;
            continue;
        }
        // The entry point: the first bare (non-flag) token, `-` (stdin), or an
        // eval flag whose value + trailing argv are the script's (mirrors
        // `run_nub`'s eval arm — needed so `node -e --node` keeps `--node` as the
        // eval code). From here, everything is forwarded verbatim.
        if !arg.starts_with('-') || arg == "-" || matches!(arg, "-e" | "--eval" | "-p" | "--print")
        {
            forwarded.extend(args[i..].iter().cloned());
            break;
        }
        // Any other leading flag (a Node option, an inline `--flag=value`, `-r`,
        // …): forward verbatim and keep scanning the leading region.
        forwarded.push(args[i].clone());
        i += 1;
    }
    (compat, forwarded)
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
fn node_compat_env_setting() -> Option<bool> {
    let value = env::var("NODE_COMPAT").ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// The `verifyDeps` environment override, for the overlay that outranks both
/// `nub.jsonc` layers. BOTH spellings resolve here rather than deeper in
/// [`crate::verify_deps`], because a value that does not reach this overlay
/// ranks BELOW the project file instead of above it.
///
/// The pre-rename spelling `NUB_VERIFY_DEPS_BEFORE_RUN` is still exported by CI
/// jobs pinned to a released binary. Honoring it silently would keep two
/// spellings alive forever; ignoring it silently would revert such a job to the
/// default policy with no signal — the quiet no-op this config surface is
/// fail-loud to avoid. So: say it moved, then apply it.
fn verify_deps_env_setting() -> Option<crate::project_config::VerifyDeps> {
    let (setting, renamed) = verify_deps_env_choice(
        env::var_os("NUB_VERIFY_DEPS").as_deref(),
        env::var_os("NUB_VERIFY_DEPS_BEFORE_RUN").as_deref(),
    )?;
    if renamed {
        warn_verify_deps_env_renamed();
    }
    Some(setting)
}

/// Returns `(setting, came from the pre-rename name)`. The old name is read
/// only when the current one is ABSENT — a present-but-unparseable
/// `NUB_VERIFY_DEPS` is still the user naming the current variable, so it must
/// not silently hand the decision to a stale one.
fn verify_deps_env_choice(
    current: Option<&std::ffi::OsStr>,
    legacy: Option<&std::ffi::OsStr>,
) -> Option<(crate::project_config::VerifyDeps, bool)> {
    match current {
        Some(value) => Some((parse_verify_deps_env(value.to_str()?)?, false)),
        None => Some((parse_verify_deps_env(legacy?.to_str()?)?, true)),
    }
}

/// nub's OWN value space, deliberately a subset of pnpm's: `install` and
/// `prompt` are absent because they exist only on the pnpm-mirroring surfaces
/// (`.npmrc`, `pnpm-workspace.yaml`), where accepting what the incumbent accepts
/// is the point. Neither is implemented, so accepting one here would do
/// something other than what it says — what `nub.jsonc` rejects them for.
fn parse_verify_deps_env(raw: &str) -> Option<crate::project_config::VerifyDeps> {
    use crate::project_config::VerifyDeps;

    match raw.trim().to_ascii_lowercase().as_str() {
        "off" | "false" | "0" | "no" | "none" | "skip" => Some(VerifyDeps::Enabled(false)),
        "true" => Some(VerifyDeps::Enabled(true)),
        "warn" => Some(VerifyDeps::Warn),
        "error" => Some(VerifyDeps::Error),
        _ => None,
    }
}

/// Once per process: the snapshot is initialized from several entrypoints (a PM
/// verb re-initializes after a runtime path already has), and each would
/// otherwise repeat the line.
fn warn_verify_deps_env_renamed() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "nub: NUB_VERIFY_DEPS_BEFORE_RUN is now NUB_VERIFY_DEPS; rename it to silence this."
        );
    }
}

fn command_config_flags(command: &Option<Command>) -> (bool, bool) {
    match command {
        Some(
            Command::Run { node, no_check, .. }
            | Command::Exec { node, no_check, .. }
            | Command::Nubx { node, no_check, .. },
        ) => (*node, *no_check),
        _ => (false, false),
    }
}

fn config_overlays(cli_node: bool, cli_no_check: bool) -> crate::project_config::ConfigOverlays {
    use crate::project_config::{ConfigOverlays, ProjectConfig, VerifyDeps};

    ConfigOverlays {
        cli: ProjectConfig {
            node_compat: cli_node.then_some(true),
            verify_deps: cli_no_check.then_some(VerifyDeps::Enabled(false)),
            ..ProjectConfig::default()
        },
        environment: ProjectConfig {
            node_compat: node_compat_env_setting(),
            // The field's pre-existing environment surface. Modelled here so the
            // snapshot reports the value that will actually win; discovery reads
            // the variable itself rather than trusting this layer (see
            // `project_config::publish_node_executable`).
            node_executable: env::var("NODE_EXECUTABLE").ok().filter(|v| !v.is_empty()),
            verify_deps: verify_deps_env_setting(),
            ..ProjectConfig::default()
        },
        defaults: ProjectConfig::builtin_defaults(),
    }
}

/// The `nodeExecutable` alone, for the `nub node` group. Every verb here resolves
/// a Node — `which` and bare status to REPORT which one governs, `ls` to mark the
/// active entry, `install` and `uninstall` to skip or guard one — so the field
/// reaches all of them and the group cannot disagree with itself about which
/// binary is current. `nub node which` printing one binary while `nub app.ts`
/// runs another is the drift `resolution_source` exists to prevent, and `ls`
/// marking a different version would be the same drift one verb over.
///
/// Not a snapshot, though: [`dispatch_subcommand`] explains why this group
/// initializes none (a malformed `nub.jsonc` in some ancestor must never block
/// `nub node install`), and best-effort keeps that intact — a file that will not
/// load simply contributes nothing, and a `$(command)` that fails is swallowed by
/// every verb except `which`, where the failure IS the answer.
fn publish_node_executable_best_effort(cwd: &Path) {
    if let Ok(config) =
        crate::project_config::load_effective_config(cwd, config_overlays(false, false))
    {
        crate::project_config::publish_node_executable(&config);
    }
}

pub(crate) fn initialize_config_snapshot(cli_node: bool, cli_no_check: bool) -> Result<()> {
    let cwd = env::current_dir()?;
    let effective = crate::project_config::initialize_effective_config(
        &cwd,
        config_overlays(cli_node, cli_no_check),
    )?;
    crate::project_config::publish_node_executable(effective);
    debug_assert!(crate::project_config::effective_config().is_some());
    Ok(())
}

/// The runtime entrypoints' snapshot init (`nub <file>`, `run`/`exec`/`nubx`/
/// `watch`, the `node` hijack). Identical to [`initialize_config_snapshot`]
/// except when compat is already FORCED — the `--node` flag, a truthy
/// `NODE_COMPAT`, or the persistent `node` shim. There an unloadable
/// `nub.jsonc` degrades to the built-in defaults with a warning instead of
/// aborting: [`effective_compat_mode`] only ORs the file's `nodeCompat` in, so a
/// file that cannot be read cannot change the outcome, and failing shut would
/// disarm the zero-augmentation escape hatch exactly when a broken config is
/// what the user is escaping. Discovery walks to the filesystem root, so the
/// offending file is often one the user did not author and cannot see.
///
/// Compat is NOT inferred for the install/PM paths, which keep
/// [`initialize_config_snapshot`]: `--node`/`NODE_COMPAT` disable runtime
/// augmentation only, and `install.*` still decides linker, soak window, and
/// registry behavior there — degrading those to defaults would silently ignore
/// settings the invocation really does consume.
fn initialize_runtime_config_snapshot(cli_node: bool, cli_no_check: bool) -> Result<()> {
    let cwd = env::current_dir()?;
    let overlays = config_overlays(cli_node, cli_no_check);
    let forced_compat = cli_node || overlays.environment.node_compat == Some(true);
    match crate::project_config::initialize_effective_config(&cwd, overlays.clone()) {
        Ok(effective) => crate::project_config::publish_node_executable(effective),
        // The error names the offending file by absolute path, which is what
        // makes a degrade visible when the file sits above the cwd.
        Err(error) if forced_compat => {
            eprintln!(
                "nub: {error}\n\x20\x20ignored — compat mode (--node / NODE_COMPAT) runs with \
                 no project config"
            );
            crate::project_config::publish_node_executable(
                crate::project_config::initialize_effective_config_without_project(&cwd, overlays),
            );
        }
        Err(error) => return Err(error.into()),
    }
    debug_assert!(crate::project_config::effective_config().is_some());
    Ok(())
}

fn runtime_config() -> Result<crate::project_config::RuntimeConfig> {
    crate::project_config::runtime_config().map_err(Into::into)
}

fn effective_compat_mode(explicit: bool, runtime: &crate::project_config::RuntimeConfig) -> bool {
    explicit || runtime.node_compat
}

/// `v8Flags` reach Node as ARGV, never through `NODE_OPTIONS` — the two channels
/// accept DIFFERENT flag sets. Node refuses `--stack-size`, `--no-opt` and most
/// other V8-only flags in `NODE_OPTIONS` ("is not allowed in NODE_OPTIONS",
/// exit 9) while accepting them on the command line, so the field is only useful
/// on the argv channel and validating it against `allowedNodeEnvironmentFlags`
/// would reject exactly the flags it exists to carry. Hence no accepted-set
/// check here: only the structural one, with Node itself rejecting a flag it
/// does not know.
fn runtime_v8_flags(runtime: &crate::project_config::RuntimeConfig) -> Result<Vec<String>> {
    for value in &runtime.v8_flags {
        if !is_node_option_token(value) {
            bail!("nub.jsonc `v8Flags` entry `{value}` must be one complete Node option token");
        }
    }
    Ok(runtime.v8_flags.clone())
}

/// One whole Node option, as it would appear in argv: a flag, carrying no
/// embedded whitespace (which would split it into two arguments once it reaches
/// `NODE_OPTIONS`) and no NUL (which no `execve` can carry).
fn is_node_option_token(value: &str) -> bool {
    value.starts_with('-') && !value.contains('\0') && !value.chars().any(char::is_whitespace)
}

/// Synthesize the preload chainer and decide how it reaches Node.
///
/// nub used to emit one `NODE_OPTIONS` token per `nub.jsonc` `preload` entry. That
/// breaks under any consumer that re-parses `NODE_OPTIONS`: Next.js parses it into a
/// `Record` keyed by option name and reformats it for every forked worker, so
/// `--require=a --require=b` collapses to `--require=b` — silently dropping whichever
/// came first, which is nub's OWN preload and with it the entire augmentation layer.
/// Measured end-to-end on a real `next build`; filed as vercel/next.js#96582.
///
/// The fix is to emit at most ONE token per flag name by writing a single module that
/// loads the user's entries in declared order. Two properties make it work:
///
/// - **It lives INSIDE the project** (`<preload_root>/node_modules/.nub/`), so a BARE
///   entry such as `dotenv/config` resolves through that project's `node_modules`
///   walk-up exactly as Node resolves the same specifier on a `--require` token. The
///   identical file outside the project dies with ERR_MODULE_NOT_FOUND (measured,
///   with a passing in-project control). nub does NOT resolve these itself — its own
///   resolver is additive-only and returns null for every bare specifier, because
///   `node_modules` and `exports` are deliberately Node's (the compat boundary).
/// - **It loads AFTER nub's hooks are installed**, so the entries get the full
///   augmentation: measured, a `.ts` preload transpiles and its tsconfig `paths`
///   alias resolves, identically to the old per-entry token.
///
/// Channel, mirroring the tier rules the per-entry router used to encode:
/// - Fast tier, every entry a `.cjs`: a `.cjs` chainer loaded by nub's own preload,
///   so no second token exists and `--require`'s synchronous entry semantics (R1)
///   survive.
/// - Fast tier with any non-`.cjs` entry: a `.mjs` chainer on its own `--import`,
///   because nub's `--require` preload cannot await one. Top-level await keeps
///   working, which `require()` could not offer (ERR_REQUIRE_ASYNC_MODULE).
/// - Compat tier: a `.mjs` chainer loaded by nub's own `--import` preload.
fn prepare_preload_chain(
    runtime: &mut crate::project_config::RuntimeConfig,
    node: &nub_core::node::discovery::ResolvedNode,
    fold: FoldInherited,
) -> Result<Option<nub_core::node::spawn::PreloadInjection>> {
    let Some(root) = runtime.preload_root.clone() else {
        return Ok(None);
    };
    // Preload entries reaching nub through an INHERITED NODE_OPTIONS are folded into
    // the same chainers as the config's, so the child ends up with at most one
    // `--require` and one `--import` no matter how many arrive. Without this an
    // ambient `--require` (what every APM injector sets) adds a second token, and a
    // consumer that keys NODE_OPTIONS by flag name drops one of them — which, since
    // nub appends the inherited value last, was nub's own preload.
    let inherited = match fold {
        FoldInherited::Yes => std::env::var("NODE_OPTIONS").unwrap_or_default(),
        FoldInherited::No => String::new(),
    };
    let (_, inherited_requires, inherited_imports) =
        nub_core::node::spawn::split_inherited_preloads(&inherited);
    if runtime.preload.is_empty() && inherited_requires.is_empty() && inherited_imports.is_empty() {
        return Ok(None);
    }

    // Which channel the CONFIG entries ride. Unchanged from the per-entry router it
    // replaced: a `.cjs`-only list keeps `--require`'s synchronous entry semantics,
    // anything else needs the async channel (and the compat tier is always async).
    // Folding an inherited `--import` makes nub force the async loader-worker tier on
    // the broken-compose band (see the folded-import marker in spawn.rs) — and Node
    // RE-RUNS every `--require` preload inside that worker's realm. So whenever a
    // loader worker is coming, everything rides the ESM chain, which loader workers
    // skip. Same rule the compat tier already needs, for the same reason.
    let async_tier_coming = !inherited_imports.is_empty()
        && nub_core::node::spawn::force_async_tier_env(&node.version, ["--import"]).is_some();
    let config_is_esm = !node.version.supports_augmentation()
        || async_tier_coming
        || runtime.preload.iter().any(|spec| {
            !std::path::Path::new(spec)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("cjs"))
        });

    // Config entries first, then inherited — which reproduces the ordering nub had
    // before the fold in every measured combination, because Node's phase rule
    // (every `--require` before any `--import`) still separates the two channels.
    let mut cjs: Vec<String> = Vec::new();
    let mut esm: Vec<String> = Vec::new();
    if config_is_esm {
        esm.extend(runtime.preload.iter().cloned());
    } else {
        cjs.extend(runtime.preload.iter().cloned());
    }
    // Inherited `--require` values ride the CJS chain only on the fast tier, where
    // nub's own preload carries it and no loader worker exists. On the compat tier
    // nub registers a `module.register` loader worker, and Node RE-RUNS every
    // `--require` preload inside that worker's realm — so a `--require` token there
    // would run the entry twice. The original per-entry router made the same trade
    // for config `.cjs` entries: moving them to the async channel costs ordering
    // (they land in the import phase) and buys running exactly once.
    if node.version.supports_augmentation() && !async_tier_coming {
        cjs.extend(inherited_requires);
    } else {
        esm.extend(inherited_requires);
    }
    esm.extend(inherited_imports);

    let dir = root.join("node_modules").join(".nub");
    if !cjs.is_empty() || !esm.is_empty() {
        std::fs::create_dir_all(&dir).with_context(|| {
            format!("could not create the preload chainer dir {}", dir.display())
        })?;
    }
    let cjs_path = write_preload_chain(&dir, false, &cjs)?;
    let esm_path = write_preload_chain(&dir, true, &esm)?;

    // nub's own preload carries whichever chainer matches its channel, so that one
    // costs no token at all; the other gets the single token of its name.
    let (carried, tokened) = if node.version.supports_augmentation() {
        (cjs_path, esm_path.map(|p| ("--import", p)))
    } else {
        // Compat tier: everything is on the ESM chain (see the routing above), which
        // nub's own `--import` preload carries — so no extra token at all.
        debug_assert!(cjs_path.is_none());
        (esm_path, None)
    };
    runtime.preload_chain = carried;
    Ok(
        tokened.map(|(flag, path)| nub_core::node::spawn::PreloadInjection {
            flag,
            value: if flag == "--import" {
                nub_core::node::spawn::file_url_for(&path.to_string_lossy())
            } else {
                path.to_string_lossy().into_owned()
            },
        }),
    )
}

/// Write one chainer, or `Ok(None)` when it would be empty.
///
/// Entries are emitted as literal statements in order. A bare specifier stays bare so
/// Node resolves it; an absolute path on the ESM side becomes a `file://` URL, because
/// Windows rejects a raw `C:\...` as an ESM specifier (ERR_UNSUPPORTED_ESM_URL_SCHEME)
/// where POSIX would have tolerated it.
fn write_preload_chain(dir: &Path, esm: bool, entries: &[String]) -> Result<Option<PathBuf>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut body = String::from(
        "// Generated by nub from `nub.jsonc` `preload` and any inherited NODE_OPTIONS\n\
         // preload flags. Regenerated every run; do not edit. One module instead of one\n\
         // token per entry — see vercel/next.js#96582.\n",
    );
    let resolver = bare_preload_resolver(esm);
    for spec in entries {
        let spec = resolve_bare_preload(&resolver, spec);
        let spec = if esm && Path::new(&spec).is_absolute() {
            nub_core::node::spawn::file_url_for(&spec)
        } else {
            spec
        };
        // JSON string escaping is exactly JS string escaping for our purposes, and it
        // is what makes a Windows path or a quote in a specifier safe to embed.
        let literal = serde_json::to_string(&spec)?;
        if esm {
            // `await import(...)`, not a static `import` — Node awaits each `--import`
            // entry before starting the next, but STATIC imports of sibling modules
            // may interleave, so an entry with top-level await would let a later one
            // land first. Sequential dynamic import reproduces the token ordering.
            body.push_str(&format!("await import({literal});\n"));
        } else {
            body.push_str(&format!("require({literal});\n"));
        }
    }
    let name = if esm {
        "preload-chain.mjs"
    } else {
        "preload-chain.cjs"
    };
    let path = dir.join(name);
    // Write via a sibling temp file so a concurrent nub in the same project can never
    // observe a half-written chainer.
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, body)
        .with_context(|| format!("could not write the preload chainer {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("could not install the preload chainer {}", path.display()))?;
    Ok(Some(path))
}

/// A resolver for BARE `nub.jsonc` `preload` entries, with the condition set the
/// chainer's channel implies (`import` for the `.mjs` chainer, `require` for the
/// `.cjs` one) — the same conditions Node itself would use for that flag.
///
fn bare_preload_resolver(esm: bool) -> oxc_resolver::Resolver {
    oxc_resolver::Resolver::new(oxc_resolver::ResolveOptions {
        condition_names: vec![
            "node".to_string(),
            if esm { "import" } else { "require" }.to_string(),
        ],
        extensions: vec![".js".into(), ".json".into(), ".node".into()],
        // pnpm links dependencies as symlinks; Node reports the realpath, so a
        // resolver that stopped at the link would disagree on module identity.
        symlinks: true,
        ..oxc_resolver::ResolveOptions::default()
    })
}

/// Turn a BARE preload entry into an absolute path, anchored at the CWD.
///
/// The anchor is the whole point. Node resolves a bare `--require`/`--import`
/// specifier **from the current working directory**, and nub did the same before the
/// chainer existed, because it emitted the bare specifier as its own token and let
/// Node resolve it. The chainer changed that by accident: a bare `import "foo"`
/// inside a generated file resolves from THAT FILE's directory, so in a workspace
/// where `packages/web/node_modules/foo` shadows the root copy, `cd packages/web &&
/// nub app.js` silently loaded the ROOT copy where plain Node loads the local one.
/// Resolving here restores the CWD anchor and makes it a decision rather than a
/// side effect of where nub happened to write the file.
///
/// Path-like entries are already absolute (config resolution anchors them at the
/// `nub.jsonc` that declared them, which is deliberate and unchanged). An
/// unresolvable bare entry is passed through untouched so Node emits its own
/// `ERR_MODULE_NOT_FOUND` naming the specifier the user actually wrote.
fn resolve_bare_preload(resolver: &oxc_resolver::Resolver, spec: &str) -> String {
    // Only an ABSOLUTE spec is already anchored. A relative one still needs the CWD:
    // config entries arrive pre-absolutized, but an entry folded out of an inherited
    // NODE_OPTIONS does not, and `./x.mjs` left verbatim would resolve against the
    // CHAINER's directory instead of the CWD Node would have used.
    if Path::new(spec).is_absolute() {
        return spec.to_string();
    }
    // No readable cwd: leave the entry alone and let Node raise its own error.
    let Ok(cwd) = std::env::current_dir() else {
        return spec.to_string();
    };
    resolver
        .resolve(&cwd, spec)
        .map(|r| r.full_path().to_string_lossy().into_owned())
        .unwrap_or_else(|_| spec.to_string())
}

/// npm/pnpm parity for the `node-options` npmrc field on SCRIPT execution.
///
/// npm and pnpm both apply it to `run` (measured: `.npmrc` `node-options=…` reaches
/// the script's `NODE_OPTIONS` under both). Its dominant real use is raising the
/// heap ceiling for a big build, so a project that needs it to build at all fails
/// with an OOM under nub and succeeds under pnpm — a silent divergence.
///
/// Two deliberate departures from npm, both in nub's favour:
///
/// - npm and pnpm ASSIGN `NODE_OPTIONS` from this field, destroying whatever the
///   ambient environment held (measured on npm 11.17.0). nub returns the tokens for
///   `compute_augmentation_env` to APPEND, so nub's own augmentation survives.
/// - Values land BEFORE nub.jsonc `nodeOptions` in the assembled list, so on a
///   conflicting flag the tool-owned `nub.jsonc` value wins under Node's last-wins
///   rule. The generic `.npmrc` surface should not override nub's own config.
///
/// The env form is npm's own `npm_config_node_options` and takes precedence over
/// the file, matching npm's config precedence. Skipped in compat mode by the caller,
/// consistent with nub.jsonc `nodeOptions` being skipped there.
fn npmrc_script_node_options(project_root: &Path) -> Vec<String> {
    env::var("NPM_CONFIG_NODE_OPTIONS")
        .or_else(|_| env::var("npm_config_node_options"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            crate::pm_engine::unsupported_config::npmrc_scalar_value(
                project_root,
                "node-options",
                true,
            )
            .filter(|value| !value.trim().is_empty())
        })
        .map(|value| nub_core::node::spawn::split_node_options(&value))
        .unwrap_or_default()
}

/// The tsconfig warnings this process already wrote, ready to hand to the child.
///
/// Both the CLI and the addon parse the project's tsconfig, so without this the
/// user reads the same warning twice — the exact problem that made tsx drop its
/// warning entirely. Call it AFTER the node options are built, since that is what
/// parses the config. A spawn path that skips this prints the warning twice rather
/// than losing it, so missing one degrades the output instead of the behavior.
fn tsconfig_reported_env() -> Option<(String, String)> {
    let reported = nub_tsconfig::reported_config_paths();
    (!reported.is_empty()).then(|| (nub_tsconfig::REPORTED_ENV.to_string(), reported.join("\n")))
}

/// Directory of the entry file for a `nub <file>` run, resolved against `cwd`.
///
/// The first non-flag argument is the entry; anything else (a bare `--flag`) leaves
/// this `None` and the CWD-anchored check stands alone.
fn entry_file_dir(args: &[String], cwd: &Path) -> Option<PathBuf> {
    let entry = args.iter().find(|a| !a.starts_with('-'))?;
    let joined = cwd.join(entry);
    joined.parent().map(Path::to_path_buf)
}

/// Refuse the run when the tsconfig governing `dir` will not parse.
///
/// The diagnostics themselves are already on stderr by the time this reads them
/// (`nub_tsconfig` writes each one once per config path), so this adds the verdict
/// and the way out rather than repeating the detail.
fn ensure_tsconfig_parses(dir: &str, explicit: Option<&str>) -> Result<()> {
    if nub_tsconfig::diagnostics(dir, explicit).is_empty() {
        return Ok(());
    }
    // `--node` is named as the way past this, but NOT as a way to keep the program
    // working: compat mode turns off every config-derived behavior, so a project
    // that needed `paths` fails there too, just further downstream. Say what it
    // costs, or the suggestion sends the reader somewhere worse than where they are.
    bail!(
        "the project's tsconfig.json could not be read in full (see above).\n\
         Fix the config, or use --node to run without Nub's TypeScript features, \
         path aliases included."
    );
}

pub(crate) fn runtime_node_options(
    runtime: &mut crate::project_config::RuntimeConfig,
    node: &nub_core::node::discovery::ResolvedNode,
) -> Result<Vec<String>> {
    runtime_node_options_with(runtime, node, FoldInherited::Yes)
}

/// Whether inherited `NODE_OPTIONS` preloads may be folded into nub's chainer.
///
/// `No` for the watch path: `node --watch` runs a supervisor that is deliberately
/// preload-free, and the chainer is only loaded by nub's own preload — so folding
/// there would move the user's ambient preload out of the supervisor entirely. A
/// user setting `NODE_OPTIONS=--require=<agent>` expects it in EVERY Node process,
/// the supervisor included, so those entries stay on the inherited value verbatim.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum FoldInherited {
    Yes,
    No,
}

pub(crate) fn runtime_node_options_with(
    runtime: &mut crate::project_config::RuntimeConfig,
    node: &nub_core::node::discovery::ResolvedNode,
    fold: FoldInherited,
) -> Result<Vec<String>> {
    let accepted = nub_core::node::discovery::accepted_env_flags(node.path.as_std_path());
    let mut options = Vec::new();

    for value in &runtime.node_options {
        validate_runtime_node_option(value, accepted.as_ref(), &node.version)?;
        options.push(value.clone());
    }

    let mut seen_conditions = std::collections::HashSet::new();
    for condition in &runtime.conditions {
        if condition.is_empty() || condition.chars().any(char::is_whitespace) {
            bail!("nub.jsonc `conditions` entries must be non-empty and contain no whitespace");
        }
        if seen_conditions.insert(condition.clone()) {
            options.push(format!("--conditions={condition}"));
        }
    }
    // tsconfig `compilerOptions.customConditions` joins the same set. TypeScript uses
    // it to resolve TYPES out of a package's `exports`, so without this the checker and
    // the runtime disagree about which file a specifier means — the "live types" layout
    // (a `source` condition pointing at `.ts`) type-checks and then loads the built
    // `dist` copy, or fails to resolve at all. Every other runner makes you declare the
    // conditions a second time in its own config; nub reads the one that is already
    // there. Union, not override: an explicit nub.jsonc `conditions` entry is not
    // contradicted by this, since conditions are a SET and it is the order of keys
    // inside `exports` that decides which one wins.
    //
    // Anchored at the CWD, which is where nub already discovers `nub.jsonc` from and
    // where Node resolves a bare `--import`/`--require` specifier from — one rule, and
    // it reaches a `customConditions` declared in the base config a leaf package
    // `extends`. Skipped entirely in compat mode by the caller, like every other
    // config-derived flag.
    if let Ok(cwd) = std::env::current_dir() {
        // A tsconfig that will not parse is FATAL, not a warning (#731). Reporting it
        // and carrying on still runs the program under options its author never
        // wrote — the same silent-wrong-answer the issue reported, only quieter — and
        // `tsc` likewise exits rather than guessing at the missing half. Nothing is
        // salvageable enough to be worth guessing: `extends` is how a project factors
        // out `strict`, `target` and `paths`, so the base is usually where the load
        // lives. `--node` / `NODE_COMPAT` skip this whole function, so the escape
        // hatch for a config nub cannot read is the one that already turns off every
        // other config-derived behavior.
        ensure_tsconfig_parses(&cwd.to_string_lossy(), runtime.tsconfig.as_deref())?;
        for condition in
            nub_tsconfig::custom_conditions(&cwd.to_string_lossy(), runtime.tsconfig.as_deref())
        {
            // A condition name with whitespace is a user error in THEIR tsconfig that
            // `tsc` itself tolerates, so it cannot be fatal here the way a bad
            // nub.jsonc entry is: skip it and leave the rest of the set intact.
            if condition.is_empty() || condition.chars().any(char::is_whitespace) {
                continue;
            }
            if seen_conditions.insert(condition.clone()) {
                options.push(format!("--conditions={condition}"));
            }
        }
    }

    for preload in &runtime.preload {
        if preload.is_empty() || preload.contains('\0') {
            bail!("nub.jsonc `preload` entries must be non-empty paths or module specifiers");
        }
    }
    // ONE synthesized chainer instead of a token per entry — see prepare_preload_chain
    // for why (vercel/next.js#96582) and where the chainer has to live.
    if let Some(injection) = prepare_preload_chain(runtime, node, fold)? {
        options.push(injection.node_options_token());
    }

    if let Some(tsconfig) = runtime.tsconfig.as_deref()
        && !Path::new(tsconfig).is_file()
    {
        bail!("nub.jsonc `tsconfig` does not name a file: {tsconfig}");
    }

    Ok(options)
}

fn validate_runtime_node_option(
    value: &str,
    accepted: Option<&BTreeSet<String>>,
    node_version: &impl std::fmt::Display,
) -> Result<()> {
    if !is_node_option_token(value) {
        bail!("nub.jsonc `nodeOptions` entry `{value}` must be one complete Node option token");
    }
    let name = value.split_once('=').map_or(value, |(name, _)| name);
    if let Some(accepted) = accepted
        && !accepted.contains(name)
    {
        // `accepted` is `allowedNodeEnvironmentFlags`, which is NARROWER than the
        // flags Node supports — this field ships through NODE_OPTIONS, and Node
        // keeps its V8 and test-runner flags command-line-only. The old "not
        // supported by Node" wording sent a `--stack-size` author to check their
        // Node version, past the `v8Flags` field that takes it.
        bail!(
            "nub.jsonc `nodeOptions` option `{name}` is not accepted in NODE_OPTIONS by Node {node_version}\n\
             \x20\x20V8 flags that Node takes only on the command line go in `v8Flags`"
        );
    }
    Ok(())
}

pub(crate) fn runtime_config_json(
    runtime: &crate::project_config::RuntimeConfig,
) -> Result<String> {
    serde_json::to_string(runtime).context("could not serialize the resolved runtime config")
}

/// [`nub_core::node::spawn::apply_expected_augmentation_marker`] against a child
/// command, which is the only sink the launchers here use.
fn stamp_augmentation_marker(
    command: &mut std::process::Command,
    name: &str,
    value: Option<&std::ffi::OsStr>,
) {
    nub_core::node::spawn::apply_expected_augmentation_marker(name, value, |key, value| {
        command.env(key, value);
    });
}

/// Environment-key equality follows the target platform. Windows collapses
/// ASCII case, while Unix permits distinct spellings such as `FOO` and `foo`.
fn runtime_env_keys_equal(left: &str, right: &str, windows: bool) -> bool {
    if windows {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

/// Insert a runtime `envFile` value with its source-order precedence. Replacing
/// an equivalent existing spelling before insertion both gives the later source
/// precedence and preserves that source's spelling in the resulting map.
fn merge_runtime_env_source_value(
    values: &mut HashMap<String, String>,
    key: String,
    value: String,
    windows: bool,
) {
    if let Some(previous) = values
        .keys()
        .find(|previous| runtime_env_keys_equal(previous, &key, windows))
        .cloned()
    {
        values.remove(&previous);
    }
    values.insert(key, value);
}

fn load_runtime_env_sources_raw(paths: &[PathBuf]) -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    let mut denied = Vec::new();
    for path in paths {
        let content = nub_core::workspace::env::read_env_file(path).with_context(|| {
            format!(
                "nub.jsonc `envFile` source is not a readable regular file: {}",
                path.display()
            )
        })?;
        for (key, value) in nub_core::workspace::env::parse_env(&content) {
            if env::var_os(&key).is_some()
                || runtime_env_keys_equal(&key, "NODE_ENV", cfg!(windows))
            {
                continue;
            }
            if nub_core::workspace::env::is_denied_env_file_key(&key) {
                denied.push(key);
                continue;
            }
            // Explicit source arrays follow Node/CLI ordering: later files win.
            merge_runtime_env_source_value(&mut values, key, value, cfg!(windows));
        }
    }
    denied.sort();
    denied.dedup();
    nub_core::workspace::env::warn_denied_env_file_keys(&denied);
    Ok(values)
}

fn load_runtime_env_sources(paths: &[PathBuf]) -> Result<HashMap<String, String>> {
    let mut values = load_runtime_env_sources_raw(paths)?;
    nub_core::workspace::env::expand_env_map(&mut values);
    Ok(values)
}

fn runtime_child_env(
    runtime: &crate::project_config::RuntimeConfig,
    project_root: Option<&Path>,
    compat_mode: bool,
    env_owner: Option<&crate::env_owner::EnvOwner>,
) -> Result<HashMap<String, String>> {
    use crate::project_config::RuntimeEnvFile;

    if no_env_file() {
        return Ok(HashMap::new());
    }
    // An owner that reaches here owns the DEFAULT cascade — nothing else can still
    // be in play, because a declared `envFile` or `--env-file` displaces the
    // hand-over before detection (see `env_file_displaces_owner`).
    let owner = env_owner.filter(|owner| owner.suppresses_env_files());
    let base = if compat_mode {
        HashMap::new()
    } else {
        match &runtime.env_file {
            RuntimeEnvFile::Default if !env_file_flag_present() => match owner {
                // The loader owns the environment end to end — nub contributes
                // nothing, and does not resolve anything on its behalf.
                Some(_) => HashMap::new(),
                None => project_root
                    .map(nub_core::workspace::env::load_env_files)
                    .unwrap_or_default(),
            },
            // Unlike the arm above, no `owner` gate: a declared source list
            // displaces the hand-over outright, so the two cannot coexist here.
            RuntimeEnvFile::Sources(paths) => load_runtime_env_sources(paths)?,
            RuntimeEnvFile::Default | RuntimeEnvFile::Disabled => HashMap::new(),
        }
    };
    // CLI --env-file remains the strongest env-file layer and composes with an
    // explicit config source list; for the default cascade it retains its
    // established suppression semantics.
    if matches!(&runtime.env_file, RuntimeEnvFile::Default) {
        return Ok(merge_child_env(
            base,
            env_file_flag_present(),
            ENV_FILE_VARS.get().unwrap_or(&HashMap::new()),
            false,
        ));
    }
    let mut result = base;
    overlay_env_file_vars(&mut result);
    Ok(result)
}

/// Whether a declared `envFile` instruction displaces a `.env.schema` hand-over.
///
/// A schema is INFERRED intent — the file is present, so a loader is presumed to
/// own the environment. An `envFile` value is DECLARED intent, and declared beats
/// inferred: the project named what it wants loaded, so nub loads that and the
/// loader stays out of the spawn chain. This replaces a rule that refused the run
/// and told the user to pick one, which left a project wanting a schema in CI and
/// a plain `.env` locally unable to say so.
///
/// `--no-env-file` and `envFile: false` displace, and that IS the point of them.
/// They used to be classified as non-conflicting on the grounds that standing
/// down already loads nothing — which read the hand-over as the absence of
/// loading rather than as its own answer, so both did nothing in a schema project
/// and handed a fully resolved environment to someone who asked for none.
///
/// `"varlock"` is the one value that does not displace: it SELECTS the loader, so
/// it lands here as a no-op. Treating it as a displacement would invert exactly
/// what it asks for, and it is the spelling a project uses to override a
/// machine-wide `envFile: false` — see [`declared_env_file_setting`], which is also
/// where the "a schema is a DEFAULT" framing this rule rests on is written down.
fn env_file_displaces_owner() -> bool {
    if no_env_file() || env_file_flag_present() {
        return true;
    }
    !matches!(
        crate::project_config::declared_env_file_setting(),
        None | Some(crate::project_config::EnvFileSetting::Varlock)
    )
}

/// [`crate::env_owner::detect`], unless a declared `envFile` displaces it.
///
/// Suppressing DETECTION is what keeps the rule contained: every downstream site
/// — the diagnostics, the child env, the spawn chain, the watch path — already
/// treats `None` as "no schema here", so a displaced owner needs no new branch in
/// any of them. Compat mode is vanilla Node: no augmentation, hence no adapter to
/// load with, hence no detection.
fn detect_env_owner(
    project: Option<&nub_core::workspace::detect::Project>,
    compat_mode: bool,
) -> Option<crate::env_owner::EnvOwner> {
    if compat_mode || env_file_displaces_owner() {
        return None;
    }
    project.and_then(|project| {
        crate::env_owner::detect(&project.root, project.workspace_root.as_deref())
    })
}

/// Every env-owner diagnostic nub raises.
///
/// 1. `envFile: "varlock"` at a project with no schema for it to read.
/// 2. A schema nub cannot act on. Always fatal.
///
/// Falling back to `.env*` is the wrong answer for the second case, not a softer
/// one. A schema the project has not disclaimed (by declaring a rival claimant of
/// the filename) says the environment is schema-resolved; running on nub's own
/// cascade instead hands the program no defaults, no validation, no providers, and
/// for a schema-only project with no committed `.env`, nothing at all — silently.
/// Only launchers are gated, so `nub install` and `nub add` still fix it. A
/// declared `envFile` is now also a fix: displacing the hand-over displaces this
/// error with it, which is the one escape from a schema whose loader will not
/// install that does not mean giving up augmentation entirely.
///
/// When the loader IS installed, nub says nothing at all: it stands down, puts the
/// loader in front of Node, and the loader owns everything from there — including
/// its own errors.
fn check_schema_usable(
    env_owner: Option<&crate::env_owner::EnvOwner>,
    compat_mode: bool,
) -> Result<()> {
    // `"varlock"` never displaces, so a `None` owner beside it means there is no
    // schema to hand over — the run would quietly get nub's own cascade under a
    // name that asked for something else. Skipped in compat mode, where no
    // runtime config applies at all.
    if !compat_mode
        && env_owner.is_none()
        && matches!(
            crate::project_config::declared_env_file_setting(),
            Some(crate::project_config::EnvFileSetting::Varlock)
        )
    {
        bail!(crate::env_owner::missing_schema_for_declared_loader());
    }
    let Some(problem) = env_owner.and_then(crate::env_owner::EnvOwner::schema_problem) else {
        return Ok(());
    };
    bail!(problem.message());
}

fn run_file(args: &[String]) -> Result<i32> {
    run_file_with_compat(args, false)
}

fn run_file_with_compat(args: &[String], compat_mode: bool) -> Result<i32> {
    let cwd = env::current_dir()?;
    // A plain file run (`nub <file>`, the `node`-hijack descendant) — not a bin
    // launch, so no `npm_config_user_agent` (parity with `node <file>`).
    run_file_in_dir(args, compat_mode, &cwd, false)
}

/// Run a file with the project context (Node pin, `.env`, PnP, webstorage) and
/// the spawned child's working directory all keyed to `cwd` — an EXPLICIT cwd
/// that overrides the process's `env::current_dir()`. The plain `nub <file>` path
/// passes the process cwd (a no-op override); the workspace-bin path threads each
/// member's dir so a node bin (`eslint`/`tsc`/`vitest`) run via `nub exec -r` sees
/// the member's `.env`, Node pin, and `.bin` chain — not the workspace root's. The
/// child's cwd is set on `SpawnConfig` so the override reaches Node itself, not
/// just nub's discovery (spawn_node otherwise inherits the parent's cwd).
fn run_file_in_dir(args: &[String], compat_mode: bool, cwd: &Path, exec_ua: bool) -> Result<i32> {
    let mut runtime = runtime_config()?;
    let compat_mode = effective_compat_mode(compat_mode, &runtime);
    // Preflight the project manifest first: an EACCES/unparseable package.json
    // otherwise reads as "no project" through every Option-returning reader on
    // this path (pin resolution, .env, PnP), so the run silently drops the
    // project context with no diagnostic. Surface the coded cause up front.
    check_manifest_json(cwd)?;
    // Pre-run dependency-freshness gate (#252). Fires for `nub <file>`, the
    // hijack-descendant `node`, and node-bin launches (which reach here via
    // `launch_bin`); the once-per-process latch keeps a bin launch that already
    // checked at the exec entry from re-checking. Skipped in compat mode.
    if let Some(code) = crate::verify_deps::gate(cwd, compat_mode) {
        return Ok(code);
    }
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

    // .env loading: eager for all non-compat invocations per internal/runtime/env-loading.md —
    // UNLESS the user passed `--env-file`, which suppresses auto-discovery entirely
    // (only the named file(s) load; the maintainer, 2026-06-15), OR `--no-env-file`,
    // which suppresses everything. `merge_child_env` applies the gate. --env-file
    // vars apply even in compat mode (explicit user flag); --no-env-file wins.
    let project = nub_core::workspace::detect::detect_project(cwd);
    let project_root = project.as_ref().map(|project| project.root.as_path());
    // A `.env.schema` means an external loader owns env for this project, so nub
    // stands down from its own cascade rather than resolving a file set the
    // loader would resolve differently. Detection is a `stat` and runs before any
    // loading, so nothing has to be undone.
    let env_owner = detect_env_owner(project.as_ref(), compat_mode);
    check_schema_usable(env_owner.as_ref(), compat_mode)?;
    let mut env_vars = runtime_child_env(&runtime, project_root, compat_mode, env_owner.as_ref())?;
    if let Some((_, schema_dir)) = env_owner
        .as_ref()
        .and_then(crate::env_owner::EnvOwner::spawn_target)
    {
        // Reaches the loader process AND the Node it spawns, so neither re-enters
        // nub and wraps a second time. Carries the schema dir rather than a bare
        // flag: a nested nub in a DIFFERENT schema-owned project must still wrap
        // its own, instead of inheriting this project's environment silently.
        env_vars.insert(
            crate::env_owner::WRAPPED_ENV.to_string(),
            crate::env_owner::wrapped_marker(schema_dir),
        );
    }

    // Bin-exec parity with `nub run`: when this spawn is nub LAUNCHING a resolved
    // node bin (a `nubx`/`nub exec` scaffolder — `exec_ua`), set the same role-
    // aware `npm_config_user_agent` the run path emits so the tool detects nub as
    // the invoking PM. Not set for a plain `nub <file>` run (`exec_ua == false`),
    // matching `node <file>`, which leaves it undefined; skipped in compat mode
    // (`--node` = vanilla). nub's value overrides an inherited one — nub is the
    // running PM here, exactly as the run path overrides it.
    if exec_ua && !compat_mode {
        env_vars.insert(
            "npm_config_user_agent".to_string(),
            exec_user_agent(cwd, &node.version.to_string()),
        );
    }

    // Dep-check dedup across processes (#252): once this process owns the
    // decision, mark the spawned child so a hijack-descendant `node` (a worker a
    // test runner forks) skips re-checking and the warning appears at most once.
    if crate::verify_deps::should_propagate_marker() {
        env_vars.insert(
            crate::verify_deps::CHECKED_MARKER.to_string(),
            "1".to_string(),
        );
    }

    let nub_binary = nub_core::node::spawn::current_nub_binary()?;
    // Compat contributes nothing config-derived: no argv flags, and no resolved
    // snapshot handed down for a re-entrant child to adopt.
    let (runtime_node_options, runtime_v8_flags) = if compat_mode {
        (Vec::new(), Vec::new())
    } else {
        let options = runtime_node_options(&mut runtime, &node)?;
        // `runtime_node_options` anchors its own check at the CWD, which is the config
        // `nub.jsonc` and a bare preload specifier resolve against. The ENTRY can sit
        // under a different one — `nub sub/main.ts` from the parent — and that is the
        // config the addon will actually transform against, so it gets the same
        // refusal. Without this, the identical project fails from inside `sub/` and
        // merely warned from above it.
        if let Some(entry_dir) = entry_file_dir(args, cwd) {
            ensure_tsconfig_parses(&entry_dir.to_string_lossy(), runtime.tsconfig.as_deref())?;
        }
        let v8_flags = runtime_v8_flags(&runtime)?;
        env_vars.insert(
            crate::project_config::RUNTIME_CONFIG_ENV.to_string(),
            runtime_config_json(&runtime)?,
        );
        if let Some((key, value)) = tsconfig_reported_env() {
            env_vars.insert(key, value);
        }
        (options, v8_flags)
    };
    // Yarn PnP: inject the user's own `.pnp.cjs` (spawn.rs gates this on
    // `!compat_mode`, so `--node` skips it regardless).
    let pnp_ctx = nub_core::pnp::detect(cwd);
    let config = nub_core::node::spawn::SpawnConfig {
        // Put the loader in front of Node when one owns this project.
        env_owner: env_owner
            .as_ref()
            .and_then(crate::env_owner::EnvOwner::spawn_target),
        node: &node,
        user_args: args,
        compat_mode,
        show_warnings: SHOW_WARNINGS.load(Ordering::Relaxed),
        nub_binary: &nub_binary,
        env_vars: &env_vars,
        pnp: pnp_ctx.as_ref().map(|c| c.pnp_cjs.as_path()),
        cwd,
        runtime_node_options: &runtime_node_options,
        runtime_v8_flags: &runtime_v8_flags,
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
    let runtime = runtime_config()?;
    let compat_mode = effective_compat_mode(compat_mode, &runtime);
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

    // Pre-run dependency-freshness gate (#252): warn (or, per policy, abort)
    // when node_modules looks stale, so a missing dep surfaces as a nub message
    // rather than a raw `foo: command not found`. Cheap and once-per-process;
    // skipped in compat mode and inside a running script (see `verify_deps`).
    if let Some(code) = crate::verify_deps::gate(&project.root, compat_mode) {
        return Ok(code);
    }

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
    let runtime = runtime_config()?;
    let compat_mode = effective_compat_mode(compat_mode, &runtime);
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
            if let Ok(manifest) =
                serde_json::from_str::<serde_json::Value>(nub_core::strip_utf8_bom(&content))
            {
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

    // Share the discovered members read-only across all chunks/workers via a
    // single refcount bump, instead of structurally cloning every chunk member
    // into every worker (`num_workers × chunk_len` clones of a
    // `WorkspacePackage`, whose `manifest` is a 10–50 KB `serde_json::Value`).
    // Built ONCE here, above the chunk loop: topological chunking produces
    // MANY chunks, so constructing it inside the loop would deep-clone the
    // whole member set per chunk (`O(num_chunks × members.len())`). Both run
    // paths index it by the global member index — the sequential branch
    // directly (`&members[idx]`), each concurrent worker via an `Arc::clone`.
    let members: std::sync::Arc<[nub_core::workspace::filter::WorkspacePackage]> =
        std::sync::Arc::from(members.as_slice());

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

            // The per-worker loop body, factored out so it backs BOTH a spawned
            // worker thread and the inline fallback (when thread creation fails
            // under resource pressure). Each invocation pulls indices from the
            // shared channel until it drains, so any number of live workers — even
            // one, even the calling thread — completes ALL the work; thread-create
            // EAGAIN only costs parallelism, never correctness.
            let run_worker = |rx: Arc<std::sync::Mutex<mpsc::Receiver<usize>>>,
                              failed: Arc<AtomicUsize>,
                              ran: Arc<AtomicUsize>| {
                let members = Arc::clone(&members);
                let ws_root_buf = ws_root.to_path_buf();
                let target = OwnedTarget::from(target);
                let ignore_scripts = exec.ignore_scripts;
                let script_shell = ws.script_shell.clone();

                move || {
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
                        let Some(member) = members.get(work_idx) else {
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
                }
            };

            let num_workers = concurrency.min(chunk.len());
            // `Builder::spawn` (returns `io::Result`) over `thread::spawn` (which
            // PANICS — and under `panic = "abort"` aborts the install — on
            // thread-create EAGAIN under PID/thread pressure). A worker that fails
            // to spawn is simply absent; the survivors drain the whole queue.
            let workers: Vec<_> = (0..num_workers)
                .filter_map(|i| {
                    let body = run_worker(Arc::clone(&rx), Arc::clone(&failed), Arc::clone(&ran));
                    thread::Builder::new()
                        .name(format!("nub-run-worker-{i}"))
                        .spawn(body)
                        .ok()
                })
                .collect();

            for &idx in chunk {
                let _ = tx.send(idx);
            }
            drop(tx);

            // If EVERY worker failed to spawn (total thread exhaustion), drain the
            // queue inline on the calling thread so the work still completes —
            // serially, but never lost, and never a panic/abort.
            if workers.is_empty() {
                run_worker(Arc::clone(&rx), Arc::clone(&failed), Arc::clone(&ran))();
            }

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
            // Nub's own per-member status line KEEPS its label even under
            // `--reporter-hide-prefix`, matching pnpm (measured on 10.15.1: it emits
            // `packages/a hello: Done` with the flag set). The flag exists to hand a
            // CI annotation matcher the child's raw output; these three lines are
            // Nub's framing, not the child's, and stripping them turned a workspace
            // run into a stack of identical unattributable `Done`s.
            Ok(0) => {
                // pnpm prints a per-package "Done" suffix on success.
                let done_prefix = format_status_prefix(&prefix, script, leaf.color_idx);
                eprintln!("{done_prefix}Done");
                MemberOutcome::Ran(0)
            }
            Ok(code) => {
                let err_prefix = format_status_prefix(&prefix, script, leaf.color_idx);
                eprintln!("{err_prefix}exit {code}");
                MemberOutcome::Ran(code)
            }
            Err(e) => {
                let err_prefix = format_status_prefix(&prefix, script, leaf.color_idx);
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
    // Pre-run dependency-freshness gate (#252) for direct callers that bypass
    // `run_script`. A `nub run` already gated at `run_script`, and a workspace
    // fan-out gated at its root, so the once-per-process latch makes this a no-op
    // there.
    if let Some(code) = crate::verify_deps::gate(&project.root, compat_mode) {
        return Ok(code);
    }

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

/// The nub-owned subdirectory the npm launcher carries the bundled shell into when it
/// relocates `nub.exe`. Must match `SHELL_SUBDIR` in `npm/nub/bin/launch.js`.
const NUB_SHELL_SUBDIR: &str = "nub-sh";

/// Resolve the bundled busybox-w32 POSIX-`sh` sidecar that backs `nub run` script
/// bodies on Windows. `__NUB_BUSYBOX_EXE` overrides the location — an internal
/// test/CI seam that lets the Rust suite and the branch-scoped Windows probe supply a
/// busybox without the release-packaging step; it is NOT a documented user knob
/// (`--script-shell` is the user-facing override).
///
/// Otherwise it resolves relative to the running executable (canonicalized, matching
/// `current_nub_binary`), checking TWO layouts in order:
///
///   * `<exe dir>/busybox.exe` — how the win32 npm package, the release `.zip` behind
///     `install.ps1`, and `nub upgrade`'s self-owned `~/.nub/bin` all lay it out.
///   * `<exe dir>/nub-sh/busybox.exe` — where the npm launcher stages it after
///     hardlinking `nub.exe` into npm's global bin dir for the PATHEXT fast path
///     (`healWindowsBinDir`). That directory is on PATH, so the shell goes in a
///     subdirectory rather than beside the binary: a bare `busybox.exe` on PATH would
///     shadow a busybox the user installed themselves.
///
/// The second layout is what #687 needed. 0.7.0 began relocating the binary and this
/// resolution had no way to follow it, so every `nub run` on a Windows npm install
/// failed from the second invocation onward.
///
/// A missing sidecar is a clean error, never a panic and never a silent cmd.exe
/// fallback — that would resurrect the non-POSIX script semantics busybox replaces.
/// Only reached on Windows (the `cfg!(windows)` default arm); cross-platform std so
/// it compiles everywhere.
/// Re-bind, inside the script body, the lowercase environment names nub set.
///
/// busybox-w32's shell UP-CASES every name when it loads the Windows environment,
/// so a script body sees `NPM_PACKAGE_NAME` and `$npm_package_name` expands to
/// nothing — while npm, pnpm, yarn and bun all deliver the lowercase name on the
/// same fixture. Upstream considers the up-casing correct and declined the
/// preserve-casing patch (rmyorston/busybox-w32#125), so the restoration is nub's
/// to do.
///
/// One `export` prologue fixes all three symptoms at once, because busybox's own
/// variable lookup is case-SENSITIVE: `$npm_package_name` expands, the lowercase
/// name is back in the environment every child inherits, and `Object.keys` sees
/// it — for a consumer in any language, not only the Node children nub augments.
///
/// It re-binds NAMES, never values, so nothing needs quoting and a value carrying
/// quotes or newlines cannot break the body. Derived from the command's own env
/// rather than from a hand-kept list, so a variable added later is covered
/// without anyone remembering this function.
fn lowercase_env_prologue(command: &std::process::Command) -> String {
    let mut names: Vec<&str> = command
        .get_envs()
        // A `None` value is a REMOVAL, and re-exporting one would put the name back.
        .filter(|(_, value)| value.is_some())
        .filter_map(|(name, _)| name.to_str())
        .filter(|name| {
            // An uppercase-only name is unaffected by the up-casing, and a name
            // that is not a shell identifier cannot be exported at all.
            name.contains(|c: char| c.is_ascii_lowercase())
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names
        .iter()
        .map(|name| format!("export {name}=\"${}\"; ", name.to_ascii_uppercase()))
        .collect()
}

fn resolve_bundled_busybox() -> Result<String> {
    let to_utf8 = |p: PathBuf| -> Result<String> {
        p.to_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("busybox path is not valid UTF-8: {}", p.display()))
    };
    if let Some(over) = std::env::var_os("__NUB_BUSYBOX_EXE") {
        let p = PathBuf::from(over);
        if !p.is_file() {
            bail!(
                "__NUB_BUSYBOX_EXE points at a missing file: {}",
                p.display()
            );
        }
        return to_utf8(p);
    }
    let exe = std::env::current_exe().context("could not determine path to nub binary")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("nub binary has no parent directory"))?;
    let [beside, staged] = busybox_candidates(dir);
    if beside.is_file() {
        return to_utf8(beside);
    }
    if staged.is_file() {
        return to_utf8(staged);
    }
    bail!(
        "nub's bundled POSIX shell (busybox.exe) was not found next to the nub \
         executable — looked at {} and {}. Reinstall nub, or set `script-shell` in \
         .npmrc to a POSIX shell on PATH.",
        display_path(&beside),
        display_path(&staged)
    );
}

/// The two busybox layouts, in resolution order, for an executable directory.
/// Split out from [`resolve_bundled_busybox`] so the ORDER is unit-testable without
/// having to relocate the test binary's own `current_exe()`.
fn busybox_candidates(dir: &Path) -> [PathBuf; 2] {
    [
        dir.join("busybox.exe"),
        dir.join(NUB_SHELL_SUBDIR).join("busybox.exe"),
    ]
}

/// Build the shell `Command` for a package script with Nub's augmentation
/// applied exactly once: `NODE_OPTIONS` (source maps + preload + webstorage; the
/// version-gated feature flags ride argv instead — see `compute_augmentation_env`),
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
) -> Result<(std::process::Command, String)> {
    use std::process::Command as StdCommand;

    let mut runtime = runtime_config()?;
    let compat_mode = effective_compat_mode(compat_mode, &runtime);

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
    // wide (overlay below) — it's not auto-discovery. See internal/runtime/env-loading.md.
    let mut env_vars: HashMap<String, String> = HashMap::new();
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
    // Values stay node-scoped per the note above — each inner `node` resolves the
    // environment at its own startup. What must flow from here is the env-owner
    // MARKERS and preload tokens, so that inner node (and any node a script
    // spawns) loads through the same owner instead of falling back to nub's
    // cascade.
    let env_owner = detect_env_owner(Some(project), compat_mode);
    check_schema_usable(env_owner.as_ref(), compat_mode)?;
    let runtime_node_options = if compat_mode {
        Vec::new()
    } else {
        // npmrc first so nub.jsonc `nodeOptions` wins a conflicting flag under
        // Node's last-wins rule. See npmrc_script_node_options.
        let mut options = npmrc_script_node_options(&project.root);
        options.extend(runtime_node_options(&mut runtime, &node)?);
        options
    };
    let runtime_json = if compat_mode {
        None
    } else {
        Some(runtime_config_json(&runtime)?)
    };

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

    // Shell precedence: an explicit `--script-shell <path>` flag wins, then a
    // `.npmrc` `script-shell=` setting, then the platform default. The default is
    // the system `/bin/sh` on Unix and, on Windows, a bundled busybox-w32 POSIX
    // `sh` resolved next to nub.exe — a real spawnable child process, so one POSIX
    // script body runs identically on macOS/Linux/Windows and a future OS sandbox
    // can confine it (an in-process interpreter could do neither). This replaces
    // the former implicit `cmd.exe` default. busybox is a multi-call binary, so
    // its `sh` applet name precedes `-c`; every other shell here takes plain `-c`.
    let custom_shell = script_shell_override
        .map(str::to_string)
        .or_else(|| nub_core::workspace::scripts::script_shell(&project.root));
    let (shell, shell_args): (String, Vec<&str>) = match custom_shell {
        Some(ref s) => (s.clone(), vec!["-c"]),
        None if cfg!(windows) => (resolve_bundled_busybox()?, vec!["sh", "-c"]),
        None => ("sh".to_string(), vec!["-c"]),
    };
    // Only the bundled busybox needs the casing prologue below: a `--script-shell`
    // the user chose is theirs, and an explicit `cmd` is case-insensitive.
    let uses_bundled_busybox = cfg!(windows) && custom_shell.is_none();

    // Append the user's extra args the way npm does (@npmcli/promise-spawn):
    // each arg is escaped for the target shell and spliced onto the UNescaped
    // script body, so multi-word / metachar args reach the script as single
    // literal tokens while the body's own globs/expansions still run. A raw
    // join (the prior behavior) let the shell re-split/expand the args. Compat,
    // not security — the args are the user's own argv (A42). The returned
    // `full_cmd` is also what the `$ <cmd>` preamble echoes, so the displayed
    // command matches the effective one (issue #146).
    let full_cmd = nub_core::workspace::shell_escape::splice_args(cmd, args, &shell);

    // Augmentation: NODE_OPTIONS + PATH shim so child `node` processes inside
    // the script inherit transpilation, polyfills, flag injection, and
    // webstorage. Computed once; `None` in compat or re-entrant invocations.
    // `node` was resolved above (its path fed npm_node_execpath).
    let nub_binary = nub_core::node::spawn::current_nub_binary()?;
    let pnp_ctx = nub_core::pnp::detect(&project.root);
    // Force-async-tier decision, captured before `node.version` is moved into
    // compute_augmentation_env below: the common `nub run dev` = `tsx …` case
    // crashes with ERR_METHOD_NOT_IMPLEMENTED on a broken-compose Node.
    let force_async_tier = nub_core::node::spawn::force_async_tier_env(
        &node.version,
        cmd.split_whitespace()
            .chain(args.iter().map(String::as_str)),
    );
    let aug = nub_core::node::spawn::compute_augmentation_env(
        &nub_binary,
        node.version,
        compat_mode,
        pnp_ctx.as_ref().map(|c| c.pnp_cjs.as_path()),
        &runtime_node_options,
    );

    // Every shell here parses `-c <body>` with the body as a single word (system
    // `sh`, busybox `sh <-c>`, or a custom POSIX `--script-shell`), so the args
    // are escaped + spliced onto the body above and passed as one string. The
    // former implicit Windows `cmd` default — the sole `windowsVerbatimArguments`
    // consumer — is gone; an explicit `script-shell=cmd` still takes this path
    // with cmd-escaped args (unchanged), it was never the verbatim default.
    let mut command = StdCommand::new(&shell);
    command.args(&shell_args);
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

    // busybox-w32 script-shell integration (Windows default only; `bin_path` on
    // the PATH above already lets it resolve `node_modules/.bin/*.cmd` shims).
    // Two POSIX temp conventions the native-Win32 busybox does not provide:
    //   * `${TMPDIR}` shell expansion — ash reads it from the process env, and on
    //     Windows it is normally unset (the OS uses %TMP%/%TEMP%), so a script's
    //     `${TMPDIR}` would be empty. Point it at the real OS temp dir, leaving a
    //     user-set value untouched. (busybox's own C `getenv` already falls back
    //     to %TMP%/%TEMP%, so `mktemp` worked; ash variable expansion does not.)
    //   * literal `/tmp` — busybox resolves an absolute POSIX path against the
    //     current drive root, so `/tmp` is `<project-drive>:\tmp` with no remap.
    //     Materialize it ONLY when the body actually references `/tmp` — most
    //     scripts never do, so the ordinary run creates nothing and no stray
    //     drive-root `\tmp` appears. When a body does write `> /tmp/x`, best-effort
    //     create it; ignoring the error keeps a locked-down drive root a
    //     no-regression (the old cmd.exe default had no `/tmp` at all), while
    //     `$TMPDIR`/`mktemp` still resolve via the env above. Skipped under an
    //     explicit `--script-shell` (that shell owns its own environment model).
    #[cfg(windows)]
    if custom_shell.is_none() {
        if std::env::var_os("TMPDIR").is_none()
            && let Some(tmp) = std::env::temp_dir().to_str()
        {
            command.env("TMPDIR", tmp);
        }
        if full_cmd.contains("/tmp")
            && let Some(drive_root) = project.root.ancestors().last()
        {
            let _ = std::fs::create_dir_all(drive_root.join("tmp"));
        }
    }

    // Default-on compile cache for script children (same decision as spawn_node,
    // 2026-06-10): a script's node subtree inherits this env, so heavyweight
    // single-file tools it launches (tsc/eslint/prettier-class bundles) load
    // their V8 blobs instead of reparsing. User-set values are untouched —
    // they're already in the inherited env and this only fills the unset case.
    // Remembered either way: the ownership marker below has to record the value
    // the child will actually see, ambient or nub-supplied.
    let expected_compile_cache = match std::env::var_os("NODE_COMPILE_CACHE") {
        ambient @ Some(_) => ambient,
        None => nub_core::node::spawn::default_compile_cache_dir().inspect(|dir| {
            command.env("NODE_COMPILE_CACHE", dir);
        }),
    };

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
    command.env("NODE", &node_env);

    if let Some(node_opts) = aug.as_ref().and_then(|a| a.node_options.as_ref()) {
        command.env("NODE_OPTIONS", node_opts);
    }
    if let Some(node_path) = aug.as_ref().and_then(|a| a.node_path.as_ref()) {
        command.env("NODE_PATH", node_path);
    }
    if let Some((key, value)) = tsconfig_reported_env() {
        command.env(key, value);
    }
    // localStorage-neutralize signal for the script subtree's node children (webstorage
    // flag-needed band, no user --localstorage-file): the preload reads + deletes it.
    if let Some(aug) = aug.as_ref() {
        aug.apply_restore_markers(|key, value| {
            command.env(key, value);
        });
        stamp_augmentation_marker(&mut command, "NODE", Some(&node_env));
        stamp_augmentation_marker(
            &mut command,
            "NODE_COMPILE_CACHE",
            expected_compile_cache.as_deref(),
        );
        stamp_augmentation_marker(
            &mut command,
            "PATH",
            aug.shim_dir.as_deref().map(std::ffi::OsStr::new),
        );
        aug.apply_localstorage_env(|k, v| {
            command.env(k, v);
        });
    }
    if let Some(runtime_json) = runtime_json {
        command.env(crate::project_config::RUNTIME_CONFIG_ENV, runtime_json);
    }

    // Force nub's async tier for a script that runs a foreign async loader
    // (tsx/ts-node) on a broken-compose Node — the common `nub run dev` = `tsx …`
    // case that otherwise crashes with ERR_METHOD_NOT_IMPLEMENTED. Only when we
    // establish augmentation (`aug` is Some); a re-entrant child inherits the var.
    if aug.is_some()
        && let Some((k, val)) = force_async_tier
    {
        command.env(k, val);
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
    // Compat is tree-wide, not only a choice made by this one launcher. An
    // inherited PATH can already contain a Nub node shim from an outer logical
    // invocation; stamping the neutral public opt-out makes any such bare
    // `node` descendant stay vanilla. Apply it last so an explicit `--node` (or
    // resolved `nodeCompat`) cannot be undone by a forwarded env file.
    if compat_mode {
        command.env("NODE_COMPAT", "1");
    }

    // An explicit `--color` has to reach the CHILD, not just Nub's own framing —
    // that is the whole point of asking for it. A workspace run pipes child stdio
    // to prefix each line, which hides the TTY from the child, so tools that
    // autodetect (tsc, vite, vitest, …) turn their own color off; `FORCE_COLOR` is
    // the switch they already honor. Measured against pnpm 10.15.1, which resolves
    // the flag exactly this way: `--color=always` gives the child `FORCE_COLOR=1`
    // and `--no-color` gives it `FORCE_COLOR=0`. Default `auto` sets nothing, so
    // the deliberate refusal to force color unasked (see aube's run_output.rs) is
    // preserved — this is the opt-in escape hatch, not a new default.
    match color_mode() {
        ColorWhen::Always => {
            command.env("FORCE_COLOR", "1");
        }
        ColorWhen::Never => {
            command.env("FORCE_COLOR", "0");
        }
        // `auto` forces nothing — with one exception, scoped to the prefixed path
        // alone. Nub resolves NO_COLOR over FORCE_COLOR; Node resolves them the other
        // way, so with BOTH set Nub's label went plain while the child still
        // colorized (measured: the child's `getColorDepth` returned 4 under
        // `NO_COLOR=1 FORCE_COLOR=1`) and a run emitted colored text inside uncolored
        // labels. Pinning the child to Nub's answer repairs that.
        //
        // ONLY under `StreamMode::Prefixed`, because only there does Nub pipe the
        // child and wrap its lines — an inherited-stdio run puts nothing around the
        // child's output, so there is no contradiction to repair and overriding the
        // pair would just diverge from everyone else: on plain `nub run <script>`,
        // pnpm, npm and a bare shell all hand the script `FORCE_COLOR=1` here.
        ColorWhen::Auto if matches!(stream, StreamMode::Prefixed) => {
            let child_would_color =
                std::env::var_os("FORCE_COLOR").is_some_and(|v| force_color_enables(&v));
            if child_would_color && !color_enabled(true) {
                command.env("FORCE_COLOR", "0");
            }
        }
        ColorWhen::Auto => {}
    }

    if let StreamMode::Prefixed = stream {
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
    }

    // The script body goes on LAST, because on Windows its prologue is derived
    // from every `command.env` call above.
    let prologue = if uses_bundled_busybox {
        lowercase_env_prologue(&command)
    } else {
        String::new()
    };
    command.arg(format!("{prologue}{full_cmd}"));

    Ok((command, full_cmd))
}

fn spawn_script(
    cmd: &str,
    project: &nub_core::workspace::detect::Project,
    compat_mode: bool,
    args: &[String],
    lifecycle_event: &str,
    exec: &ScriptExecOpts,
) -> Result<i32> {
    let (mut command, display_cmd) = build_script_command(
        cmd,
        project,
        compat_mode,
        args,
        lifecycle_event,
        StreamMode::Inherit,
        exec.script_shell,
    )?;
    // Echo the command before running it, like npm/pnpm (and like Nub's own
    // workspace/streaming path). `display_cmd` is the script body with the
    // forwarded args spliced + escaped exactly as executed, so the preamble
    // matches the effective command (issue #146). Single-package runs inherit
    // stdio with no per-package prefix, so just `$ <command>`, to stderr so it
    // never pollutes the script's stdout. Runs once per lifecycle script
    // (pre/main/post). Suppressed by `--silent`.
    if !SILENT.load(Ordering::Relaxed) {
        eprintln!("$ {display_cmd}");
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
/// The `$ <cmd>` echo for each lifecycle step is emitted inside
/// [`spawn_script_prefixed`] (suppressed by `--silent`), alongside the ndjson
/// `start` event, so both stream call sites echo identically — and the main
/// step's echo carries the forwarded args spliced exactly as executed.
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
    // --ignore-scripts skips pre/post for the whole lifecycle; only the main
    // body runs (matching npm's interpretation, which run.md adopts).
    let run_hooks = !exec.ignore_scripts;

    // pre<script>: no user args, short-circuits the run on failure.
    if run_hooks {
        let pre_name = format!("pre{script}");
        if let Some(pre_cmd) =
            nub_core::workspace::scripts::resolve_script(&project.manifest, &pre_name)
        {
            let code = spawn_script_prefixed(
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

    let code = spawn_script_prefixed(
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
            let post_code = spawn_script_prefixed(
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

/// Per-stream formatting + emission policy for a script's stdout/stderr drain.
/// Owned so it can cross a thread boundary OR drive an inline drain — the same
/// logic backs both paths. The stream (`R`) is held SEPARATELY (see
/// [`PipeReaders`]) so a failed thread-spawn never loses the pipe: the policy
/// can always still drain the stream inline.
#[derive(Clone)]
struct DrainPolicy {
    ndjson: bool,
    aggregate: bool,
    is_stderr: bool,
    prefix: String,
    name: String,
    script: String,
}

impl DrainPolicy {
    /// Emit one raw line per this policy, returning the prefixed form to collect —
    /// `None` whenever nothing downstream will read it back.
    fn emit(&self, line: &str) -> Option<String> {
        if self.ndjson {
            let level = if self.is_stderr { "error" } else { "info" };
            emit_ndjson("log", level, &self.name, &self.script, Some(line), None);
            return None;
        }
        let prefixed = format!("{}{line}", self.prefix);
        if !self.aggregate {
            if self.is_stderr {
                eprintln!("{prefixed}");
            } else {
                println!("{prefixed}");
            }
            // STREAMING: the line has already reached the user's terminal and
            // nothing reads it back, so retaining a copy is pure growth. The
            // collected `Vec` lives for the child's whole life, which for a
            // script that never exits — `nub run -r dev`, the standard monorepo
            // workflow — means the supervisor grows 1:1 with child output until
            // it is killed or OOMs. Collect ONLY for the aggregate flush, the
            // one consumer that genuinely replays these lines.
            return None;
        }
        Some(prefixed)
    }

    /// Drain `stream` to EOF on the CURRENT thread, returning the collected lines.
    /// Aggregate mode holds at most [`AGGREGATE_MAX_HELD_BYTES`] before flushing
    /// early, so a child that never exits cannot grow this without bound.
    fn run<R: std::io::Read>(&self, stream: R) -> Vec<String> {
        self.run_capped(stream, AGGREGATE_MAX_HELD_BYTES)
    }

    /// `run` with an explicit hold ceiling, so a test can exercise the early
    /// flush without pushing the shipped 8 MiB through the harness's capture.
    fn run_capped<R: std::io::Read>(&self, stream: R, max_held: usize) -> Vec<String> {
        use std::io::BufRead as _;
        let mut lines = Vec::new();
        let mut held = 0usize;
        for line in std::io::BufReader::new(stream)
            .lines()
            .map_while(Result::ok)
        {
            if let Some(prefixed) = self.emit(&line) {
                held += prefixed.len() + 1;
                lines.push(prefixed);
                if held >= max_held {
                    flush_aggregated(&mut lines, self.is_stderr);
                    held = 0;
                }
            }
        }
        lines
    }
}

/// Ceiling on the output one stream holds for the deferred aggregate flush.
///
/// Aggregate mode buffers a child's whole output so each package prints as ONE
/// contiguous block — which quietly assumes the script COMPLETES. It does not
/// only apply to `--aggregate-output`: a non-TTY stdout selects it too (see the
/// `aggregate` binding in the workspace run path), so `nub run -r dev` in CI, or
/// piped to a file, takes this path with a dev server that never exits. The
/// buffer then grew for the life of the run — measured at 46 → 555 MB in 80 s.
///
/// Past the cap the held lines are flushed early and buffering resumes. An
/// ordinary script still prints as one block; only a runaway producer is split
/// into several, and no output is dropped. That is the right trade against a
/// supervisor that OOMs after long enough.
const AGGREGATE_MAX_HELD_BYTES: usize = 8 * 1024 * 1024;

/// Write buffered aggregate lines and clear the buffer, under the shared flush
/// lock so concurrent workers never tear each other's output.
fn flush_aggregated(lines: &mut Vec<String>, is_stderr: bool) {
    use std::io::Write as _;
    if lines.is_empty() {
        return;
    }
    let _guard = AGGREGATE_FLUSH_LOCK.lock();
    if is_stderr {
        let stderr = std::io::stderr();
        let mut se = stderr.lock();
        for line in lines.iter() {
            let _ = writeln!(se, "{line}");
        }
        let _ = se.flush();
    } else {
        let stdout = std::io::stdout();
        let mut so = stdout.lock();
        for line in lines.iter() {
            let _ = writeln!(so, "{line}");
        }
        let _ = so.flush();
    }
    lines.clear();
}

/// Both of a script child's output pipes plus their per-stream policies, drained
/// together so the child can never deadlock on a full pipe buffer.
///
/// The two pipes MUST be drained CONCURRENTLY: draining one to EOF before
/// starting the other lets a child that fills the not-yet-read pipe block on
/// `write` forever. The happy path drains stdout on its own thread while stderr
/// drains on the calling thread. Under OS thread-create EAGAIN (the `nub ci`
/// exit-101 family) — where `thread::spawn` would PANIC, and under
/// `panic = "abort"` abort the whole process — we fall back to a SINGLE-THREAD
/// concurrent drain that interleaves both pipes via `poll(2)`, preserving the
/// no-deadlock guarantee without a second thread.
struct PipeReaders {
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
    out_policy: DrainPolicy,
    err_policy: DrainPolicy,
}

impl PipeReaders {
    /// Drain both pipes to EOF, returning `(stdout_lines, stderr_lines)`. Never
    /// deadlocks: stdout on a thread + stderr inline normally; both interleaved
    /// on one thread via `poll(2)` when a thread can't be created.
    fn drain(self) -> (Vec<String>, Vec<String>) {
        let PipeReaders {
            stdout,
            stderr,
            out_policy,
            err_policy,
        } = self;

        let Some(out) = stdout else {
            // No stdout pipe — only stderr to drain (inline, always concurrent
            // enough since there's nothing to race it against).
            let err_lines = stderr.map(|e| err_policy.run(e)).unwrap_or_default();
            return (Vec::new(), err_lines);
        };

        // Happy path: stdout on its own thread (concurrent with the inline stderr
        // drain below). On Unix we hand the thread a `dup`ed fd and KEEP the
        // original `ChildStdout` — so a `Builder::spawn` failure (thread-create
        // EAGAIN, which a bare `thread::spawn` would panic/abort on) leaves the
        // original recoverable for the inline fallback. The dup and the original
        // share the same OS pipe; only one ever reads it (the thread on success,
        // the original inline on failure), so there's no double-drain.
        #[cfg(unix)]
        {
            use std::os::unix::io::{AsRawFd, FromRawFd};
            // SAFETY: dup an fd we own; wrap the new fd in an owning File. -1 on
            // failure is handled below (drain inline).
            let dup_fd = unsafe { libc::dup(out.as_raw_fd()) };
            let spawn_result = if dup_fd < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                // SAFETY: `dup_fd` is a fresh, owned fd; `File` takes ownership.
                let dup_file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
                let policy = out_policy.clone();
                std::thread::Builder::new()
                    .name("nub-script-stdout".into())
                    .spawn(move || policy.run(dup_file))
            };
            match spawn_result {
                Ok(handle) => {
                    let err_lines = stderr.map(|e| err_policy.run(e)).unwrap_or_default();
                    let out_lines = handle.join().unwrap_or_default();
                    (out_lines, err_lines)
                }
                Err(_) => {
                    // Thread-create (or dup) EAGAIN: drain BOTH pipes concurrently
                    // on this one thread so neither can deadlock the child.
                    drain_both_inline(out, out_policy, stderr, err_policy)
                }
            }
        }
        #[cfg(not(unix))]
        {
            // Windows never exhibited the thread-exhaustion abort, and has no
            // `poll`-based inline multiplex here — move stdout into the thread as
            // before. `Builder::spawn` still avoids the unconditional panic of
            // `thread::spawn`; on the (astronomically rare) failure we drain
            // stdout inline AFTER stderr, accepting the sequential-drain window.
            let policy = out_policy.clone();
            let spawn_result = std::thread::Builder::new()
                .name("nub-script-stdout".into())
                .spawn(move || policy.run(out));
            match spawn_result {
                Ok(handle) => {
                    let err_lines = stderr.map(|e| err_policy.run(e)).unwrap_or_default();
                    let out_lines = handle.join().unwrap_or_default();
                    (out_lines, err_lines)
                }
                Err(_) => {
                    let err_lines = stderr.map(|e| err_policy.run(e)).unwrap_or_default();
                    // stdout was moved into the failed closure; it's gone. Lose
                    // its lines (Windows-only, vanishing-probability path).
                    (Vec::new(), err_lines)
                }
            }
        }
    }
}

/// Concurrently drain stdout + stderr on the CURRENT thread (no second thread),
/// using `poll(2)` to multiplex the two pipes so a full buffer on either can
/// never block the other — preserving the no-deadlock guarantee without a thread.
/// Non-Unix has no `poll` here; it falls back to sequential draining, accepting
/// the rare large-output deadlock window (the abort this replaces was Linux-only,
/// and Windows never exhibited the thread-exhaustion bug).
#[cfg(unix)]
fn drain_both_inline(
    out: std::process::ChildStdout,
    out_policy: DrainPolicy,
    err: Option<std::process::ChildStderr>,
    err_policy: DrainPolicy,
) -> (Vec<String>, Vec<String>) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // `out` / `err` stay owned (and so their fds stay valid) until this block
        // ends. Set both pipes non-blocking so a `read` after a `poll`-ready
        // signal can never block (and returns `WouldBlock` when momentarily
        // drained), which is what makes the single-thread multiplex safe.
        set_nonblocking(out.as_raw_fd());
        if let Some(e) = err.as_ref() {
            set_nonblocking(e.as_raw_fd());
        }
        let mut out_pipe = LinePipe::new(out.as_raw_fd(), out_policy);
        let mut err_pipe = err
            .as_ref()
            .map(|e| LinePipe::new(e.as_raw_fd(), err_policy));

        loop {
            let mut fds: Vec<libc::pollfd> = Vec::with_capacity(2);
            if !out_pipe.done {
                fds.push(libc::pollfd {
                    fd: out_pipe.fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            if let Some(ep) = err_pipe.as_ref() {
                if !ep.done {
                    fds.push(libc::pollfd {
                        fd: ep.fd,
                        events: libc::POLLIN,
                        revents: 0,
                    });
                }
            }
            if fds.is_empty() {
                break;
            }
            // SAFETY: `fds` is a valid, len-sized slice of pollfd; -1 = no timeout.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break; // unrecoverable poll error — stop draining
            }
            for pfd in &fds {
                if pfd.revents == 0 {
                    continue;
                }
                if pfd.fd == out_pipe.fd {
                    out_pipe.pump();
                } else if let Some(ep) = err_pipe.as_mut() {
                    if pfd.fd == ep.fd {
                        ep.pump();
                    }
                }
            }
        }
        let out_lines = out_pipe.finish();
        let err_lines = err_pipe.map(LinePipe::finish).unwrap_or_default();
        (out_lines, err_lines)
    }
    #[cfg(not(unix))]
    {
        // No `poll` — drain sequentially. Safe for the small-output common case;
        // the thread-exhaustion abort this guards was never seen off Linux.
        let out_lines = out_policy.run(out);
        let err_lines = err.map(|e| err_policy.run(e)).unwrap_or_default();
        (out_lines, err_lines)
    }
}

/// Put a raw fd into non-blocking mode (best-effort — a failure just leaves the
/// `poll`-then-`read` slightly more cautious; it does not break correctness).
#[cfg(unix)]
fn set_nonblocking(fd: std::os::unix::io::RawFd) {
    // SAFETY: F_GETFL/F_SETFL on an fd we own; flags OR'd with O_NONBLOCK.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// A raw-fd pipe drained incrementally under `poll(2)`: reads available bytes,
/// splits complete lines, emits + collects them per its [`DrainPolicy`], and
/// buffers any partial trailing line until more arrives or EOF.
#[cfg(unix)]
struct LinePipe {
    fd: std::os::unix::io::RawFd,
    policy: DrainPolicy,
    buf: Vec<u8>,
    lines: Vec<String>,
    done: bool,
}

#[cfg(unix)]
impl LinePipe {
    fn new(fd: std::os::unix::io::RawFd, policy: DrainPolicy) -> Self {
        LinePipe {
            fd,
            policy,
            buf: Vec::new(),
            lines: Vec::new(),
            done: false,
        }
    }

    /// Read whatever is ready and emit any newly-complete lines.
    fn pump(&mut self) {
        let mut chunk = [0u8; 8192];
        loop {
            // SAFETY: read into a valid local buffer on a raw fd we own.
            let n = unsafe {
                libc::read(
                    self.fd,
                    chunk.as_mut_ptr() as *mut libc::c_void,
                    chunk.len(),
                )
            };
            if n > 0 {
                self.buf.extend_from_slice(&chunk[..n as usize]);
                self.drain_complete_lines();
                continue; // keep reading until the pipe is momentarily empty
            }
            if n == 0 {
                self.done = true; // EOF
                return;
            }
            // n < 0
            let e = std::io::Error::last_os_error();
            match e.kind() {
                std::io::ErrorKind::WouldBlock => return, // nothing more right now
                std::io::ErrorKind::Interrupted => continue,
                _ => {
                    self.done = true;
                    return;
                }
            }
        }
    }

    fn drain_complete_lines(&mut self) {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line = self.buf.drain(..=pos).collect::<Vec<u8>>();
            line.pop(); // drop '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line);
            if let Some(prefixed) = self.policy.emit(&text) {
                self.lines.push(prefixed);
            }
        }
    }

    /// Flush any partial trailing line (no terminating newline) and return all
    /// collected lines.
    fn finish(mut self) -> Vec<String> {
        if !self.buf.is_empty() {
            let text = String::from_utf8_lossy(&self.buf);
            if let Some(prefixed) = self.policy.emit(&text) {
                self.lines.push(prefixed);
            }
        }
        self.lines
    }
}

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
) -> Result<i32> {
    use std::io::Write;

    let (mut command, display_cmd) = build_script_command(
        cmd,
        project,
        compat_mode,
        args,
        script_name,
        StreamMode::Prefixed,
        exec.script_shell,
    )?;

    nub_core::node::spawn::group_on_spawn(&mut command);

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
    // The `$ <cmd>` preamble / ndjson `start` event is emitted BEFORE the spawn so
    // it always precedes any of the child's output. `display_cmd` carries the
    // forwarded args spliced + escaped exactly as executed, so the preamble
    // matches the effective command (issue #146); pre/post hooks receive no args,
    // so theirs is just the body. Emitted here (not in the caller) so every step —
    // and both the sequential and concurrent run paths — echo identically.
    if ndjson {
        emit_ndjson("start", "info", &pkg_name, script_name, None, None);
    } else if !SILENT.load(Ordering::Relaxed) {
        let cmd_prefix = format_stream_prefix_sep(prefix, script_name, color_idx, "$ ");
        eprintln!("{cmd_prefix}{display_cmd}");
    }

    let mut child = nub_core::node::spawn::spawn_with_eagain_retry(&mut command)?;
    // Relay docker stop / Ctrl-C to the streamed child's whole process group too
    // (workspace `-r` runs) — the `sh -c` won't pass a forwarded signal to node.
    // This is the ONLY multi-child caller: `-r` runs members on concurrent worker
    // threads, so several groups are tracked at once and each unregisters its own.
    let child_pid = child.id();
    nub_core::node::spawn::track_child_group(child_pid);
    // SIGKILL-on-the-leader backstop (#480) — macOS-only inside; held across
    // the wait below, dropped (disarmed) on return.
    #[cfg(unix)]
    let _reaper = nub_core::node::spawn::spawn_group_reaper(child_pid);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let prefix_out = format_stream_prefix(prefix, script_name, color_idx);
    let prefix_err = prefix_out.clone();

    let (name_out, script_out) = (pkg_name.clone(), script_name.to_string());
    let (name_err, script_err) = (pkg_name.clone(), script_name.to_string());

    // In aggregate mode the drains collect prefixed lines instead of emitting
    // them live; the parent flushes the buffered blocks once, below. `PipeReaders`
    // drains both pipes CONCURRENTLY and never deadlocks — stdout on its own
    // thread + stderr inline normally, or both interleaved on one thread via
    // `poll(2)` when OS thread-create EAGAIN forces the inline fallback (the
    // `nub ci` exit-101 family, where a bare `thread::spawn` would panic/abort).
    let (out_lines, err_lines) = PipeReaders {
        stdout,
        stderr,
        out_policy: DrainPolicy {
            ndjson,
            aggregate,
            is_stderr: false,
            prefix: prefix_out,
            name: name_out,
            script: script_out,
        },
        err_policy: DrainPolicy {
            ndjson,
            aggregate,
            is_stderr: true,
            prefix: prefix_err,
            name: name_err,
            script: script_err,
        },
    }
    .drain();

    let status = child.wait()?;
    nub_core::node::spawn::untrack_child(child_pid);
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

    Ok(exit_code)
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

/// Prefix for Nub's OWN per-member status line (`Done` / `exit N` / `error: …`).
/// Unlike [`format_stream_prefix`] this ignores `--reporter-hide-prefix`: that flag
/// hides the label on the CHILD's output so a CI matcher sees raw lines, and pnpm
/// likewise keeps the label on its own status line. Without the label a workspace
/// run ends in N identical bare `Done`s that name no package.
fn format_status_prefix(dir: &str, script: &str, idx: usize) -> String {
    format_stream_prefix_sep(dir, script, idx, ": ")
}

fn format_stream_prefix_sep(dir: &str, script: &str, idx: usize, sep: &str) -> String {
    const DIR_COLORS: &[u8] = &[36, 35, 34, 33, 32, 31];
    let use_color = color_enabled(std::io::IsTerminal::is_terminal(&std::io::stderr()));
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
    use crate::project_config::RuntimeEnvFile;

    let cwd = env::current_dir()?;
    let mut runtime = runtime_config()?;
    let compat_mode = effective_compat_mode(false, &runtime);
    let env_file_present = env_file_flag_present();
    let no_env_file = no_env_file();
    let project = nub_core::workspace::detect::detect_project(&cwd);
    if let Some(project) = project.as_ref()
        && let Some(code) = crate::verify_deps::gate(&project.root, compat_mode)
    {
        return Ok(code);
    }
    // Same stand-down as the run path: with an external owner, watch must not
    // hand Node the `.env*` cascade as `--env-file` args either, or the watched
    // process would re-acquire exactly the file set the owner displaces.
    let env_owner = detect_env_owner(project.as_ref(), compat_mode);
    check_schema_usable(env_owner.as_ref(), compat_mode)?;
    let env_owner_suppresses = env_owner
        .as_ref()
        .is_some_and(crate::env_owner::EnvOwner::suppresses_env_files);
    let config_env_sources = matches!(&runtime.env_file, RuntimeEnvFile::Sources(_));
    let env_file_paths = if no_env_file || compat_mode {
        Vec::new()
    } else {
        match &runtime.env_file {
            RuntimeEnvFile::Default if !env_file_present && env_owner_suppresses => Vec::new(),
            RuntimeEnvFile::Default if !env_file_present => project
                .as_ref()
                .map(|p| nub_core::workspace::env::discover_env_files(&p.root))
                .unwrap_or_default(),
            // No owner gate here, unlike the `Default` arm above: a declared source
            // list DISPLACES a hand-over, so an owner and a `Sources` cannot both be
            // in play. Being inside a wrap does not change that — the declaration is
            // still the project's, and it still wins.
            RuntimeEnvFile::Sources(paths) => paths.clone(),
            RuntimeEnvFile::Default | RuntimeEnvFile::Disabled => Vec::new(),
        }
    };
    // Explicit `--env-file` flags forward to the watched Node too (#479), so
    // each file is watched and re-read per restart exactly like the autos —
    // these are the candidates; the per-version gate lives below.
    let explicit_env_files: &[(PathBuf, bool)] = if env_file_present && !no_env_file {
        ENV_FILE_PATHS.get().map(|v| v.as_slice()).unwrap_or(&[])
    } else {
        &[]
    };
    // A raw env-file path arms the child guard for the watch lifetime. Validate
    // its serialized ambient state before Node discovery: an uncached
    // `node --version` probe inherits NODE_OPTIONS, so waiting until command
    // assembly would let invalid bytes fail with an unrelated Node error first.
    if !compat_mode
        && (!env_file_paths.is_empty() || !explicit_env_files.is_empty())
        && env::var_os("NODE_OPTIONS").is_some_and(|value| value.to_str().is_none())
    {
        bail!("watch env-file filtering requires NODE_OPTIONS to be valid UTF-8");
    }
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
    if compat_mode {
        let mut node_args = vec!["--watch".to_string(), "--watch-preserve-output".to_string()];
        node_args.push(file.to_string());
        node_args.extend(args.iter().cloned());
        let mut cmd = std::process::Command::new(node.path.as_str());
        cmd.args(&node_args)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
        // Not `.status()` — see the note on the augmented path below. `node --watch`
        // never exits on its own, so a bare status() leaks the supervisor and its
        // watched child on leader death.
        let status = nub_core::node::spawn::status_forwarding_signals(&mut cmd)?;
        return Ok(nub_core::node::spawn::exit_code_from_status(&status));
    }
    let runtime_node_options = runtime_node_options_with(&mut runtime, &node, FoldInherited::No)?;
    let runtime_v8_flags = runtime_v8_flags(&runtime)?;
    let runtime_json = runtime_config_json(&runtime)?;

    // Auto-loaded `.env*` files are handed to the watched Node as `--env-file`
    // args (below) so Node watches each path and re-reads it on every restart.
    // However Node's own `--env-file` parser does NOT expand `${VAR}` cross-
    // references, which would make `nub watch` inconsistent with `nub <file>`
    // (which uses `load_env_files` and gets full expansion). To close the gap we
    // also load and expand the env files here via `load_env_files` and inject the
    // expansion-changed values via `cmd.env()` below.
    //
    // Live-reload correctness (#207): Node's `--env-file` never overrides a var
    // already present in the inherited environment (shell-wins), so any var we
    // inject via `cmd.env()` FREEZES at its `nub watch` startup value — the long-
    // lived watcher process never restarts, so it never re-reads `.env`. Injecting
    // every var (the old behavior) therefore froze plain vars across restarts even
    // though Node's own `--env-file` would have live-reloaded them. Fix: inject
    // ONLY the keys whose `${VAR}` expansion actually changed the raw value (Node's
    // `--env-file` can't reproduce those); every plain/unexpanded var is delivered
    // SOLELY through the `--env-file` args, so editing `.env` is picked up on the
    // next restart, matching `node --watch --env-file=.env`. See the inject loop.
    //
    // Live-reload trade-off (unchanged): an expansion-dependent var (`B=${A}`) is
    // still injected, so if `A` changes while the watcher runs, `B` won't update
    // until nub is restarted — the same trade-off `nub <file>` already has.
    //
    // Precedence among the `.env*` files: Node is *last*-writer-wins, so we pass
    // `discover_env_files`' highest-priority-first list in reverse for nub's
    // first-writer-wins precedence to line up.
    //
    // CLI `--env-file` suppresses only the automatic `.env*` cascade. An explicit
    // `nub.jsonc` `env` source list still composes first, and CLI values overlay
    // it. `--no-env-file` suppresses BOTH — the watched Node receives no
    // `--env-file` args at all.
    //
    // Forward the explicit `--env-file` files only when the pinned Node accepts
    // every flag flavor present (#479). All-or-nothing: mixing forwarded and
    // injected explicit files would corrupt last-writer-wins precedence (an
    // injected earlier file, arriving via the inherited env, would beat a
    // forwarded later one — Node's `--env-file` never overrides an already-set
    // var). Below the gate, fall back to whole-map injection: values freeze at
    // their startup snapshot (the pre-#479 behavior — degraded, never broken).
    //
    // The BASE set needs the SAME floor, whether it came from auto-discovery or
    // from a config `env` source list. It never had one: `--env-file` args for
    // the discovered `.env*` files were pushed unconditionally, so on Node
    // 18.19–20.5 every `nub watch` in a project with a `.env` died on `bad
    // option: --env-file=…` before running a line (#573). A config source list
    // reaches Node through the identical arg, so it crashes identically — the
    // gate covers both. The base set always uses the plain `--env-file`
    // spelling, so only the 20.6.0 floor applies to it.
    let forward_base = node_accepts_env_file(&node.version);
    let forward_explicit = should_forward_explicit_env_files(&node.version, explicit_env_files);
    // The base `.env` set, unexpanded — the auto cascade, or the config's source
    // list. Loaded whenever a base source is live, INDEPENDENT of forwarding:
    // `auto_env` below needs it as the injection base even when the files never
    // reach Node. A single read (rather than a loader plus a second raw loader)
    // avoids re-parsing every file twice and closes the TOCTOU window where a
    // file changing between two reads could spuriously inject a plain var.
    let base_raw_env = if no_env_file {
        HashMap::new()
    } else {
        match &runtime.env_file {
            // Same gate the `--env-file` list above uses, and for the same reason:
            // the loader owns this environment end to end. Reading the cascade here
            // would put nub's own values back on the loader's command through the
            // inject loop below, which is exactly what `runtime_child_env` refuses
            // to do on the file-run path.
            RuntimeEnvFile::Default if !env_file_present && env_owner_suppresses => HashMap::new(),
            RuntimeEnvFile::Default if !env_file_present => project
                .as_ref()
                .map(|p| nub_core::workspace::env::load_env_files_raw_warning(&p.root))
                .unwrap_or_default(),
            RuntimeEnvFile::Sources(_) if env_owner_suppresses => HashMap::new(),
            RuntimeEnvFile::Sources(paths) => load_runtime_env_sources_raw(paths)?,
            RuntimeEnvFile::Default | RuntimeEnvFile::Disabled => HashMap::new(),
        }
    };
    // Pre-expand the config/automatic base to the same map the direct file
    // runner produces, then overlay CLI values so they are strongest.
    let mut env_vars = base_raw_env.clone();
    nub_core::workspace::env::expand_env_map(&mut env_vars);
    overlay_env_file_vars(&mut env_vars);
    // Raw values that the watched Node receives through forwarded files — the set
    // the inject loop must SKIP, since injecting a var Node already delivers
    // freezes it at the startup value (#207). Values absent here must be injected,
    // while values that differ after expansion are injected with their expanded
    // form. A family that is not forwarded contributes nothing, which makes
    // injection its only delivery channel and restores the pre-#207
    // freeze-at-startup behavior: degraded live-reload, never a dead watcher.
    let mut forwarded_raw_env = if forward_base {
        base_raw_env
    } else {
        HashMap::new()
    };
    if forward_explicit && let Some(explicit) = ENV_FILE_VARS_RAW.get() {
        forwarded_raw_env.extend(explicit.clone());
    }

    let nub_binary = nub_core::node::spawn::current_nub_binary()?;
    let preload_path = nub_core::node::spawn::find_public_preload(&nub_binary);
    // Additivity: nub's own augmentation preload must load AFTER the user's
    // ambient NODE_OPTIONS-derived requires — nub never changes the user's
    // observable preload order — and BEFORE the project-config preloads, so
    // those load with nub's hooks already active. Both `NODE_OPTIONS` assemblies
    // below place the token accordingly, matching the non-watch spawn order.
    let nub_preload_token = preload_path.as_deref().map(|preload| {
        nub_core::node::spawn::preload_injection(preload, &node.version).node_options_token()
    });

    let mut node_args = vec!["--watch".to_string(), "--watch-preserve-output".to_string()];

    let node_options_os = env::var_os("NODE_OPTIONS");
    let node_options = node_options_os.as_ref().and_then(|value| value.to_str());
    let accepted = nub_core::node::discovery::accepted_env_flags(node.path.as_std_path());
    let inject = nub_core::node::flags::compute_inject_flags(
        node.version.clone(),
        args,
        node_options,
        false,
        accepted.as_ref(),
    );
    for flag in &inject {
        node_args.push(flag.to_string());
    }
    // `v8Flags` and the matrix's ARGV-only V8 unflags ride argv here for the same
    // reason they do in `spawn_node`: `NODE_OPTIONS` refuses most V8-only flags.
    // Node's watch supervisor re-execs the child with this same argv, so they
    // survive every restart.
    let argv_only_flags = nub_core::node::flags::argv_inject_flags(
        Some(node.path.as_std_path()),
        &node.version,
        args,
    );
    for flag in &argv_only_flags {
        node_args.push(flag.to_string());
    }
    node_args.extend(runtime_v8_flags.iter().cloned());
    let sanitized_node_options = node_options.map(|existing| {
        nub_core::node::flags::strip_unsupported_node_options(existing, &node.version)
    });

    // Reverse so the highest-priority `.env*` file lands last (Node's
    // last-writer-wins ⇒ nub's first-writer-wins precedence).
    // A config `env` source list is already in user order (later files win, like
    // repeated `--env-file`), so only the auto cascade is reversed.
    if forward_base {
        let mut ordered: Vec<&Path> = env_file_paths.iter().map(PathBuf::as_path).collect();
        if !config_env_sources {
            ordered.reverse();
        }
        for path in ordered {
            // `cmd` inherits `cwd`; the helper chooses the path form required by
            // the platform watcher without changing cwd or file precedence.
            node_args.push(watch_env_file_arg(path, &cwd, cfg!(windows), false)?);
        }
    }
    // Explicit `--env-file` files forward in USER order — no reverse: Node is
    // last-writer-wins and nub's parse across repeated flags is last-wins too,
    // so the orders already agree (#479).
    if forward_explicit {
        for (path, if_exists) in explicit_env_files {
            node_args.push(watch_env_file_arg(path, &cwd, cfg!(windows), *if_exists)?);
        }
    }

    node_args.push(file.to_string());
    node_args.extend(args.iter().cloned());

    // Watch assembles its own command instead of going through `spawn_node`, so
    // it must put the loader in front of Node itself. Detection above has already
    // stood nub's own cascade down; without this the watched process gets no
    // environment at all, silently — measured, every variable `undefined`.
    //
    // The loader resolves once, at watcher startup, and Node's `--watch`
    // supervisor re-execs the child inside it. Values therefore freeze across
    // restarts, which is the trade-off this path already makes for every
    // expansion-dependent var it injects.
    let mut cmd = match env_owner
        .as_ref()
        .and_then(crate::env_owner::EnvOwner::spawn_target)
    {
        Some((loader, schema_dir)) => {
            let mut cmd = nub_core::node::spawn::loader_command(loader);
            cmd.arg("run")
                .arg("--path")
                .arg(schema_dir)
                .arg("--")
                .arg(node.path.as_str());
            cmd.env(
                crate::env_owner::WRAPPED_ENV,
                crate::env_owner::wrapped_marker(schema_dir),
            );
            cmd
        }
        None => std::process::Command::new(node.path.as_str()),
    };
    cmd.args(&node_args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    cmd.env(crate::project_config::RUNTIME_CONFIG_ENV, runtime_json);
    // Tell the preload to hide nub's argv-only V8 flags from `process.execArgv`, the same
    // signal the direct-spawn path sets. Node's watch supervisor re-execs the child with
    // this environment, so it survives every restart.
    if !argv_only_flags.is_empty() {
        cmd.env(
            nub_core::node::flags::ARGV_ONLY_FLAGS_ENV,
            argv_only_flags.join(" "),
        );
    }
    let mut launcher_owned_env_keys = vec![crate::project_config::RUNTIME_CONFIG_ENV.to_string()];
    if nub_preload_token.is_some() {
        // Watch assembles NODE_OPTIONS directly instead of using AugmentationEnv,
        // so it must stamp the same fresh-invocation restoration metadata itself.
        nub_core::node::spawn::apply_augmentation_restore_markers(|key, value| {
            cmd.env(key, value);
            launcher_owned_env_keys.push(key.to_string());
        });
        // Watch changes NODE_OPTIONS below. The other restoration-controlled
        // variables retain their inherited values, but still need exact
        // ownership markers so a watched script's later mutation wins at a
        // fresh nested-Nub boundary.
        for name in ["NODE", "NODE_PATH", "NODE_COMPILE_CACHE", "PATH"] {
            let value = env::var_os(name);
            nub_core::node::spawn::apply_expected_augmentation_marker(
                name,
                value.as_deref(),
                |key, value| {
                    cmd.env(key, value);
                    launcher_owned_env_keys.push(key.to_string());
                },
            );
        }
        // Reserve and protect the marker now; the final NODE_OPTIONS value is
        // assembled after the raw-env guard state below and overwrites this
        // absent sentinel before the command starts.
        nub_core::node::spawn::apply_expected_augmentation_marker(
            "NODE_OPTIONS",
            None,
            |key, value| {
                cmd.env(key, value);
                launcher_owned_env_keys.push(key.to_string());
            },
        );
    }
    // Node's Windows watch supervisor first registers the long-spelled env-file
    // directory, then registers module paths reported by the watched child. If
    // the inherited cwd contains an 8.3 component (for example RUNNER~1), those
    // child paths use the short spelling and Node installs a second watcher for
    // the same directory. libuv reports the event under the long spelling and
    // older bundled versions abort on the mismatch. Align only this raw-env
    // watch path's supervisor cwd; GetLongPathNameW preserves symlink/junction
    // identity and non-Windows behavior stays byte-for-byte unchanged.
    // Whether ANY env file (base — auto-discovered or config-sourced — or
    // explicit) reaches Node as a raw `--env-file` arg: the trigger for the
    // per-child guard machinery below.
    let any_env_forwarded = (forward_base && !env_file_paths.is_empty()) || forward_explicit;
    #[cfg(windows)]
    if any_env_forwarded {
        cmd.current_dir(windows_long_watch_path(&cwd)?);
    }
    // Inject ONLY the .env* vars whose `${VAR}` expansion changed the raw value
    // (#207) — see [`watch_inject_vars`]. A var Node's `--env-file` already
    // delivers identically is left to Node so the long-lived watcher re-reads it
    // from the file on every restart (a changed `.env` value is picked up) instead
    // of freezing at startup.
    for (k, v) in watch_inject_vars(&env_vars, &forwarded_raw_env) {
        cmd.env(k, v);
    }
    // Node receives the forwarded files (auto-discovered + explicit) as raw
    // `--env-file` paths and re-reads them in each watched child. Keep canonical
    // placeholders in the preload-free supervisor so startup consumers cannot
    // act on file values; an early child-only `--require` then removes every
    // non-ambient denied spelling and restores the sanitized ambient
    // NODE_OPTIONS before user preloads run. Arming by path presence, rather
    // than current contents, keeps edits made after startup behind the same
    // boundary.
    let cleanup_token = if any_env_forwarded {
        if node_options_os.is_some() && node_options.is_none() {
            bail!("watch env-file filtering requires NODE_OPTIONS to be valid UTF-8");
        }
        let preload = preload_path
            .as_deref()
            .context("watch env-file filtering requires the Nub runtime preload")?;
        let cleanup_preload = Path::new(preload).with_file_name("watch-env-guard.cjs");
        if !cleanup_preload.is_file() {
            bail!(
                "watch env-file filtering preload is missing: {}",
                cleanup_preload.display()
            );
        }
        let cleanup_preload = cleanup_preload
            .to_str()
            .context("watch env-file filtering preload path is not valid UTF-8")?;
        let token = nub_core::node::spawn::PreloadInjection {
            flag: "--require",
            value: cleanup_preload.to_string(),
        }
        .node_options_token();

        // The auto cascade is the live family exactly when no explicit flag
        // suppressed it; that decides whether `NODE_ENV` is guarded here (see
        // `watch_guarded_env_file_keys`).
        let guarded_keys = watch_guarded_env_file_keys(!env_file_present && !no_env_file);
        let ambient_keys = ambient_guarded_env_file_keys(&guarded_keys, &launcher_owned_env_keys);
        for key in watch_env_guard_placeholders(&guarded_keys, &ambient_keys, cfg!(windows)) {
            cmd.env(key, "");
        }
        let state = serde_json::json!({
            "denylist": guarded_keys,
            "ambientKeys": ambient_keys,
            "nodeOptions": &sanitized_node_options,
        });
        cmd.env(
            WATCH_ENV_GUARD_ENV,
            serde_json::to_string(&state).context("could not serialize watch env-file guard")?,
        );
        Some(token)
    } else {
        None
    };

    // Fixed order: env-file cleanup guard → the user's ambient requires → nub's
    // own preload → the project config's `nodeOptions`. Watch assembles
    // NODE_OPTIONS itself rather than going through `spawn.rs`, so every quoting
    // rule that file enforces has to be repeated here or it regresses on this
    // surface alone — which is how it regressed. The CLI validator rejects
    // whitespace and NUL but NOT a double quote, so an entry like `--title=a"b`
    // would reach Node's tokenizer unbalanced and abort startup (rc=9) before
    // the watcher exists.
    let nub_augmented = nub_preload_token.is_some();
    let node_options_parts: Vec<String> = cleanup_token
        .into_iter()
        .chain(sanitized_node_options.filter(|value| !value.is_empty()))
        .chain(nub_preload_token)
        .chain(
            runtime_node_options
                .iter()
                .map(|option| nub_core::node::spawn::node_options_token(option)),
        )
        .collect();
    if !node_options_parts.is_empty() {
        let effective_node_options = node_options_parts.join(" ");
        cmd.env("NODE_OPTIONS", &effective_node_options);
        if nub_augmented {
            stamp_augmentation_marker(
                &mut cmd,
                "NODE_OPTIONS",
                Some(std::ffi::OsStr::new(&effective_node_options)),
            );
        }
    }
    // NOT `cmd.status()`. Every other long-lived spawn in nub goes through a
    // process-group + reaper path; watch was the sole exception, and it is the one
    // that matters most: `node --watch` is a supervisor that by design never exits,
    // and it spawns a watched grandchild of its own. A bare `status()` leaves both
    // outside nub's group, so when the leader dies (a killed agent session, a test
    // harness that gives up, a closed terminal) they reparent to launchd and run
    // forever, each holding an fsevents watch on a source tree. They accumulate
    // monotonically — 99 such orphans were found on the dev host, the oldest 17h
    // old, none of them rustc or cargo. `status_forwarding_signals` is the
    // signal-faithful equivalent of `status()`: own process group, the #480
    // SIGKILL-on-leader reaper held across the wait, and terminating signals
    // forwarded to the whole subtree.
    let status = nub_core::node::spawn::status_forwarding_signals(&mut cmd)?;

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
    let runtime = runtime_config()?;
    let compat_mode = effective_compat_mode(compat_mode, &runtime);
    let cwd = env::current_dir()?;

    // `-p <spec>` forces the fetch path: the bin to run may not match any local
    // bin, and npx never lets a local bin shadow an explicit `--package`.
    let force_fetch = dlx_flags.is_some_and(|f| !f.package.is_empty());

    if !force_fetch {
        if let Some(bin_path) = nub_core::workspace::scripts::find_bin(bin, &cwd) {
            // A local bin is about to run: gate on dependency freshness (#252).
            // Only here — never on the registry-fetch fallback below, where the
            // whole point is an ad-hoc tool the project need not have installed.
            // A node bin re-enters `run_file_in_dir`, but the once-per-process
            // latch keeps that from re-checking.
            if let Some(code) = crate::verify_deps::gate(&cwd, compat_mode) {
                return Ok(code);
            }
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
                // This is still a bin LAUNCH (a PnP-resolved local bin), so it
                // carries `npm_config_user_agent` like the node_modules/.bin path
                // above — `run_file_in_dir` directly (not run_file_with_compat)
                // to pass `exec_ua=true`. `cwd` is the PnP tree already resolved.
                return run_file_in_dir(&cmd_args, compat_mode, &cwd, true);
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
    // to the registry here — fetch the tool into a throwaway project and run it,
    // matching `npx` local-first resolution. This is the IMPLICIT registry tier,
    // so it is gated: `nubx` never executes remote code silently. The gate fails
    // closed in CI and any non-TTY, prompts once per spec in an interactive
    // terminal, and runs without a prompt once a spec is recorded as consented;
    // `-y` is the explicit escape hatch. (Explicit `nub dlx` bypasses all of this
    // — invoking it IS the consent — and reaches the engine on its own verb path.)
    if NUBX_DLX_FALLBACK.load(Ordering::Relaxed) {
        let flags = dlx_flags.cloned().unwrap_or_default();
        // The gate keys on the canonical install set — the `-p` packages, or the
        // bare bin token — the same identity the engine resolves and runs.
        let specs: Vec<String> = if flags.package.is_empty() {
            vec![bin.to_string()]
        } else {
            flags.package.clone()
        };
        let record = match crate::nubx_consent::gate(&specs, flags.yes) {
            crate::nubx_consent::Decision::Proceed { record } => record,
            crate::nubx_consent::Decision::Refused(code) => return Ok(code),
        };
        let (code, fetched_ok) =
            crate::pm_engine::run_dlx_for_nubx(bin, args, &flags, compat_mode)?;
        // Record consent ONLY after a confirmed successful fetch+run (`fetched_ok`),
        // never on a 404 / failed install — otherwise a one-time `y` on a
        // not-yet-published spec would become a standing silent run-grant for a name
        // an attacker could later publish. A tool that ran and exited non-zero still
        // counts as fetched (consent stands); only a fetch/install failure does not.
        if record && fetched_ok {
            crate::nubx_consent::record(&specs);
        }
        return Ok(code);
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

/// Resolve `name` against the `PATH` env, returning the first existing
/// executable file. Used by the external-subcommand fallthrough for a globally-
/// installed `nub-<verb>` plugin (the project-local node_modules/.bin lookup is
/// tried first). On Windows a bare `name` is probed against the executable
/// extensions (`.exe`/`.cmd`/`.bat`); on Unix the literal name is the entry.
fn find_on_path(name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let exts: &[&str] = &["", ".exe", ".cmd", ".bat"];
    #[cfg(not(windows))]
    let exts: &[&str] = &[""];

    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        for ext in exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
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
        // child's working directory is set, not just nub's discovery. `true` =
        // this is a bin LAUNCH, so it carries `npm_config_user_agent` (the non-
        // node branch below sets it via apply_exec_augmentation).
        return run_file_in_dir(&cmd_args, compat_mode, cwd, true);
    }

    let mut cmd = bin_launcher(bin_path, args);
    // --env-file first (applies in compat too); aug's NODE_OPTIONS/PATH/NODE_PATH
    // are set after so nub's values win over any same-named env-file keys (A19).
    apply_env_file_vars(&mut cmd);
    if !compat_mode {
        apply_exec_augmentation(&mut cmd, cwd)?;
    }
    // Dep-check dedup (#252): a non-node bin still gets nub's augmentation env, so
    // a `node` it spawns re-enters nub — mark it so the warning isn't repeated.
    if crate::verify_deps::should_propagate_marker() {
        cmd.env(crate::verify_deps::CHECKED_MARKER, "1");
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

/// The `npm_config_user_agent` value nub emits when it LAUNCHES a bin/tool
/// (`nubx`, `nub exec`, a workspace-bin run), reusing the same role-aware
/// composer + platform tail as the `nub run` / lifecycle paths — no second
/// hardcoded format. `nub/<v> npm/? …` under nub identity / fresh, incumbent-
/// first (`pnpm/<pin> nub/<v> …`) in a compat project. `node_version` is the
/// caller's already-resolved Node so this does not re-discover it.
// @lat: [[research/npm-config-user-agent#Nub — code + empirical#The exec surface — three routes, not one]]
fn exec_user_agent(cwd: &Path, node_version: &str) -> String {
    let product = crate::pm_engine::run_lifecycle_ua_product(cwd, node_version);
    nub_core::workspace::scripts::user_agent_string(&product)
}

/// Apply nub's augmentation env (NODE_OPTIONS preload + PATH shim + `.bin`
/// chain) to a non-node launcher, so any `node` the tool spawns is transpile-
/// enabled — the same env `nub run` gives a script. No-op if augmentation can't
/// be set up (e.g. preload not found).
fn apply_exec_augmentation(cmd: &mut std::process::Command, cwd: &Path) -> Result<()> {
    let mut runtime = runtime_config()?;
    if runtime.node_compat {
        return Ok(());
    }
    let Ok(nub_binary) = nub_core::node::spawn::current_nub_binary() else {
        return Ok(());
    };
    let node = nub_core::node::discovery::discover_node(cwd)
        .unwrap_or_else(|_| nub_core::node::discovery::ResolvedNode::fallback());
    let runtime_node_options = runtime_node_options(&mut runtime, &node)?;
    let runtime_json = runtime_config_json(&runtime)?;
    // Force-async-tier decision for a foreign async loader (`nubx tsx …`) on a
    // broken-compose Node — captured now, before `node.version` is moved into
    // compute_augmentation_env below and before `cmd` is mutated (the returned
    // pair is 'static, so it outlives both). Applied only once aug is confirmed.
    let exec_tokens: Vec<String> = std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let force_async_tier = nub_core::node::spawn::force_async_tier_env(
        &node.version,
        exec_tokens.iter().map(String::as_str),
    );
    // Exec-path parity with `nub run`/lifecycle: a launched non-node bin (a
    // create-* scaffolder, a tool that shells out) must see the same role-aware
    // `npm_config_user_agent` so it detects nub as the invoking PM instead of a
    // blank value (which the whitelist detectors fall back to npm on). Set before
    // the preload early-return below — the UA is independent of whether the
    // transpile preload was found.
    cmd.env(
        "npm_config_user_agent",
        exec_user_agent(cwd, &node.version.to_string()),
    );
    let pnp_ctx = nub_core::pnp::detect(cwd);
    let Some(aug) = nub_core::node::spawn::compute_augmentation_env(
        &nub_binary,
        node.version,
        false,
        pnp_ctx.as_ref().map(|c| c.pnp_cjs.as_path()),
        &runtime_node_options,
    ) else {
        return Ok(());
    };
    // $NODE for tools that spawn a child node via `process.env.NODE` — point it at
    // the shim (→ nub) so the child stays augmented, matching `nub run` (see
    // build_script_command). Computed while `aug` is still whole; its fields are
    // moved out below.
    let node_env = aug
        .node_shim_exe()
        .unwrap_or_else(|| std::ffi::OsString::from(node.path.as_str()));
    cmd.env("NODE", &node_env);
    // localStorage-neutralize signal (webstorage flag-needed band, no user
    // --localstorage-file); applied before the partial moves of aug below.
    aug.apply_restore_markers(|key, value| {
        cmd.env(key, value);
    });
    aug.apply_localstorage_env(|k, v| {
        cmd.env(k, v);
    });
    cmd.env(crate::project_config::RUNTIME_CONFIG_ENV, runtime_json);
    // Stamp the env-owner markers wherever the adapter is injected — without them
    if let Some((k, val)) = force_async_tier {
        cmd.env(k, val);
    }
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
    let path = match aug.shim_dir.as_deref() {
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
    stamp_augmentation_marker(cmd, "NODE", Some(&node_env));
    stamp_augmentation_marker(
        cmd,
        "PATH",
        aug.shim_dir.as_deref().map(std::ffi::OsStr::new),
    );
    Ok(())
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
/// (`https://github.com/<repo>/releases/download`). When set, `archive_url` /
/// `checksum_url` point at it instead of github.com, so the self-owned
/// download+verify+swap path can be driven against a local `file://` fixture
/// with no network. UNSET in all normal operation (default behavior is
/// byte-identical); this is NOT a documented user knob — it exists purely so the
/// upgrade flow is end-to-end testable without publishing a real release. The
/// value is used verbatim as a URL prefix: the artifact for `v<version>` /
/// `<target>` is `<base>/v<version>/nub-<target>.<ext>` (`.zip` for win32
/// targets, `.tar.gz` otherwise — mirroring GitHub's release-asset layout), so
/// a fixture must reproduce that path shape.
const RELEASE_DOWNLOAD_BASE_ENV: &str = "NUB_RELEASE_BASE_URL";

/// Internal, test-only override for the "resolve `latest`" endpoint
/// (default: GitHub's `…/releases/latest` API). When set, `resolve_version`
/// fetches the JSON `{ "tag_name": "v<X.Y.Z>" }` from here instead. Same
/// contract as [`RELEASE_DOWNLOAD_BASE_ENV`]: unset in normal operation, never a
/// documented user surface, present only to make the upgrade resolve path
/// testable against a `file://` fixture.
const RELEASE_LATEST_API_ENV: &str = "NUB_RELEASE_LATEST_URL";

/// The rolling release tag the canary channel publishes under — release.yml's
/// canary-release job recreates it at every built main commit, so the archive
/// lives at `<base>/canary/nub-<target>.<ext>` with no `v` prefix (bun's exact
/// layout). npm carries the same builds under the `canary` dist-tag; Homebrew
/// and winget carry only stable releases.
pub(crate) const CANARY_TAG: &str = "canary";

/// Internal, test-only override for the canary release-by-tag endpoint
/// (default: GitHub's `…/releases/tags/canary`). Serves the JSON whose `name`
/// is `Canary <X.Y.Z>-canary.<date>.<run>` (release.yml's canary-release job
/// titles it so). Same contract as [`RELEASE_LATEST_API_ENV`].
const RELEASE_CANARY_API_ENV: &str = "NUB_RELEASE_CANARY_URL";

/// The releases-download base URL — the test seam's override if set, else the
/// canonical github.com path. Centralized so the override is read in exactly one
/// place and the default is the single source of truth. Shared with
/// `compile::launcher`, which pulls per-target launcher templates from the same
/// release, so one seam redirects both channels.
pub(crate) fn release_download_base() -> String {
    std::env::var(RELEASE_DOWNLOAD_BASE_ENV)
        .unwrap_or_else(|_| format!("https://github.com/{RELEASE_REPO}/releases/download"))
}

/// The "resolve latest" API URL — the test seam's override if set, else GitHub's
/// `releases/latest` endpoint.
fn release_latest_api() -> String {
    std::env::var(RELEASE_LATEST_API_ENV)
        .unwrap_or_else(|_| format!("https://api.github.com/repos/{RELEASE_REPO}/releases/latest"))
}

/// The canary release-by-tag API URL — the test seam's override if set, else
/// GitHub's `releases/tags/canary` endpoint.
fn release_canary_api() -> String {
    std::env::var(RELEASE_CANARY_API_ENV).unwrap_or_else(|_| {
        format!("https://api.github.com/repos/{RELEASE_REPO}/releases/tags/{CANARY_TAG}")
    })
}

/// True when THIS binary is a canary build — the canary pipeline stamps the
/// compiled version as `<X.Y.Z>-canary.<date>.<run>` (release.yml's
/// set-version.mjs step), so the marker rides CARGO_PKG_VERSION. bun-mirror: a
/// canary build's bare `nub upgrade` stays on the canary channel (see
/// [`choose_release_channel`]).
pub(crate) fn is_canary_build() -> bool {
    env!("CARGO_PKG_VERSION").contains("-canary.")
}

/// Which release channel an upgrade pulls from — orthogonal to
/// [`UpgradeChannel`] (HOW the binary got installed): stable is the versioned
/// `v*` tags, canary the rolling [`CANARY_TAG`] prerelease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseChannel {
    Stable,
    Canary,
}

/// The bun-mirrored channel decision, pure for testability: `--canary` opts
/// in; a canary build defaults back to canary so bare `nub upgrade` tracks the
/// channel you're on; `--stable` or an explicit `--version` opts out.
fn choose_release_channel(
    flag_canary: bool,
    flag_stable: bool,
    explicit_version: bool,
    running_is_canary: bool,
) -> ReleaseChannel {
    if flag_canary || (running_is_canary && !flag_stable && !explicit_version) {
        ReleaseChannel::Canary
    } else {
        ReleaseChannel::Stable
    }
}

/// Where to send a user whose install channel cannot serve the canary (npm and
/// Homebrew only carry stable releases): the standalone installer, whose
/// `canary` argument installs the rolling build into the self-owned `~/.nub`.
fn canary_install_hint() -> &'static str {
    if cfg!(windows) {
        r#"iex "& { $(irm https://nubjs.com/install.ps1) } canary""#
    } else {
        "curl -fsSL https://nubjs.com/install.sh | bash -s canary"
    }
}

/// The npm package users `npm install -g`. The bare `nub` name is an unrelated
/// third-party package — emitting it would clobber a working install, so every
/// npm-channel command must target the scoped `@nubjs/nub`.
const NPM_PACKAGE: &str = "@nubjs/nub";

/// On-disk file name of the nub executable inside a self-owned install's
/// `bin/` — `nub.exe` on Windows, `nub` elsewhere (same fact install.ps1 /
/// install.sh encode).
const NUB_EXE: &str = if cfg!(windows) { "nub.exe" } else { "nub" };

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
    /// winget portable install (path under WinGet's package store). ADVISE,
    /// never execute: spawning `winget upgrade` from inside a running nub would
    /// try to replace the in-use nub.exe, which fails AND corrupts winget's
    /// version bookkeeping (winget-cli #5235 records the upgrade as done);
    /// self-swapping the file instead would leave winget's tracked version
    /// stale forever (portable packages have no version hook, winget-cli
    /// discussion #3304) and the next real `winget upgrade` would silently
    /// clobber it. So this channel only prints the command to run.
    Winget,
    /// The curl/`~/.nub` self-owned install — Nub owns the binary and swaps it
    /// in place. `install_dir` is the `…/.nub` root (parent of `bin/`).
    SelfOwned { install_dir: PathBuf },
    /// Couldn't tell — print the manual-instruction message and exit non-zero.
    Unknown,
}

/// The exact command a winget-installed user upgrades with. Advisory only —
/// see [`UpgradeChannel::Winget`] for why nub never spawns it.
const WINGET_UPGRADE_DISPLAY: &str = "winget upgrade --id Nubjs.Nub";

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
    // winget's portable-package store (%LOCALAPPDATA%\Microsoft\WinGet\Packages\…).
    // The Links\ stub is a symlink into it, and `current_nub_binary` canonicalizes,
    // so the real store path is what arrives here.
    if s.contains("\\Microsoft\\WinGet\\Packages\\") || s.contains("/Microsoft/WinGet/Packages/") {
        return UpgradeChannel::Winget;
    }
    // Self-owned: the binary sits at `<install_dir>/bin/nub`. Derive install_dir
    // by walking up from the binary, then accept it as self-owned when EITHER the
    // dir is the default `.nub` (covers installs predating the receipt) OR it
    // carries a `.nub-receipt` marker the installer writes. The receipt is what
    // makes a relocated `NUB_INSTALL_DIR` in-place-upgradeable, while still
    // rejecting an unrelated `<dir>/bin/nub` (e.g. a distro `/usr/bin/nub`) that
    // has neither signal. Order matters: npm/homebrew already returned above, so a
    // package-managed binary never reaches here.
    let install_dir = bin_path
        .parent()
        .filter(|bin_dir| bin_dir.file_name().is_some_and(|n| n == "bin"))
        .and_then(Path::parent)
        .filter(|install_dir| {
            install_dir.file_name().is_some_and(|n| n == ".nub")
                || install_dir.join(".nub-receipt").is_file()
        });
    match install_dir {
        Some(dir) => UpgradeChannel::SelfOwned {
            install_dir: dir.to_path_buf(),
        },
        None => UpgradeChannel::Unknown,
    }
}

/// The release-artifact platform token for the current build, mirroring
/// install.sh's `uname -ms` → target mapping (install.ps1's OSArchitecture
/// switch on Windows). `None` on a platform Nub doesn't publish an archive for
/// (the self-owned channel then falls back to a clear error rather than
/// fetching a 404). musl vs glibc on Linux is a runtime distinction install.sh
/// makes via `ldd`/`/etc/alpine-release`; we encode the glibc default here and
/// document musl as the known gap (see note below).
pub(crate) fn platform_target() -> Option<&'static str> {
    // NOTE: a glibc build and a musl build of the same arch are the same Rust
    // `target_env` only under `target_env = "musl"`, so this distinguishes them
    // correctly when Nub itself is built for musl. The detection matches the
    // archive names release.yml publishes.
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "x86_64") if cfg!(target_env = "musl") => Some("linux-x64-musl"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") if cfg!(target_env = "musl") => Some("linux-arm64-musl"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("windows", "x86_64") => Some("win32-x64"),
        ("windows", "aarch64") => Some("win32-arm64"),
        _ => None,
    }
}

/// The exact npm command `nub upgrade` runs / suggests on an npm install. Pure
/// and centralized so there is a single place the scoped `@nubjs/nub` name is
/// emitted — the regression guard test pins it.
fn npm_upgrade_command(target: &str) -> String {
    format!("npm install -g {NPM_PACKAGE}@{target}")
}

/// The Homebrew self-upgrade refreshes the tap before upgrading (#375):
/// `brew upgrade` evaluates the formula from the LOCAL tap checkout, and only
/// `brew update` refreshes it — without the refresh a stale tap clone reports
/// the installed version as newest and never sees a published release. The two
/// invocations are split (not a shelled-out `&&`) so nub controls exit-code
/// semantics: `brew update` is best-effort, `brew upgrade` is authoritative.
const HOMEBREW_UPDATE_ARGS: &[&str] = &["update"];
const HOMEBREW_UPGRADE_ARGS: &[&str] = &["upgrade", "nub"];
const HOMEBREW_UPGRADE_DISPLAY: &str = "brew update && brew upgrade nub";

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

/// Release-archive extension for a platform token: the win32 targets publish
/// portable `.zip` (release.yml), every other target a `.tar.gz`. Keyed on the
/// target STRING rather than `cfg!` so the URL builders stay pure and testable
/// on any host.
fn archive_ext(target: &str) -> &'static str {
    if target.starts_with("win32-") {
        "zip"
    } else {
        "tar.gz"
    }
}

/// GitHub release archive URL for a release TAG (`v<X.Y.Z>` for stable,
/// [`CANARY_TAG`] for the rolling canary) + platform target. Mirrors
/// install.sh's `url=` line (install.ps1's on Windows) so the self-owned
/// channel pulls the same artifact the installer did. The base is
/// [`release_download_base`] so the test seam can redirect it to a local
/// fixture; unset → the canonical github.com URL.
fn archive_url_for_tag(tag: &str, target: &str) -> String {
    format!(
        "{}/{tag}/nub-{target}.{}",
        release_download_base(),
        archive_ext(target)
    )
}

/// [`archive_url_for_tag`] for a resolved stable version (`v<version>` tag).
fn archive_url(version: &str, target: &str) -> String {
    archive_url_for_tag(&format!("v{version}"), target)
}

/// SHA-256 checksum sidecar URL for the archive. release.yml publishes a
/// `<archive>.sha256` next to each archive (stable and canary alike); the
/// self-owned channel fetches it
/// and verifies the download before extracting.
fn checksum_url_for_tag(tag: &str, target: &str) -> String {
    format!("{}.sha256", archive_url_for_tag(tag, target))
}

/// [`checksum_url_for_tag`] for a resolved stable version (`v<version>` tag).
fn checksum_url(version: &str, target: &str) -> String {
    checksum_url_for_tag(&format!("v{version}"), target)
}

fn run_upgrade(
    version: Option<&str>,
    canary: bool,
    stable: bool,
    dry_run: bool,
    _yes: bool,
) -> Result<i32> {
    let nub_binary = nub_core::node::spawn::current_nub_binary()?;
    let bin_str = display_path(&nub_binary);
    let channel = detect_channel(&nub_binary);
    let release_channel =
        choose_release_channel(canary, stable, version.is_some(), is_canary_build());
    let target = version.unwrap_or("latest");

    if dry_run {
        match &channel {
            UpgradeChannel::Npm => {
                // npm serves the canary channel as the `canary` dist-tag
                // (release.yml publishes every canary build under it), so the
                // npm path is the same command with a different spec.
                let npm_target = match release_channel {
                    ReleaseChannel::Canary => CANARY_TAG,
                    ReleaseChannel::Stable => target,
                };
                println!("would upgrade to {npm_target} via npm");
                println!("  command: {}", npm_upgrade_command(npm_target));
            }
            UpgradeChannel::Homebrew => {
                if release_channel == ReleaseChannel::Canary {
                    println!(
                        "would upgrade to canary, but canary builds are not published to Homebrew"
                    );
                    println!("  install instead: {}", canary_install_hint());
                } else {
                    println!("would upgrade to {target} via homebrew");
                    println!("  command: {HOMEBREW_UPGRADE_DISPLAY}");
                }
            }
            UpgradeChannel::Winget => {
                if release_channel == ReleaseChannel::Canary {
                    println!(
                        "would upgrade to canary, but canary builds are not published to winget"
                    );
                    println!("  install instead: {}", canary_install_hint());
                } else {
                    println!("would upgrade to {target} via winget (run it yourself)");
                    println!("  command: {WINGET_UPGRADE_DISPLAY}");
                }
            }
            UpgradeChannel::SelfOwned { install_dir } => {
                // Show the resolved dir, not a hardcoded ~/.nub — a receipt-marked
                // NUB_INSTALL_DIR relocates it.
                let channel_word = match release_channel {
                    ReleaseChannel::Canary => "canary",
                    ReleaseChannel::Stable => target,
                };
                println!(
                    "would upgrade to {channel_word} via self-owned ({})",
                    display_path(install_dir)
                );
                match platform_target() {
                    Some(plat) => {
                        let tag = match release_channel {
                            ReleaseChannel::Canary => {
                                // The rolling tag needs no resolution; surface the
                                // advertised canary version when the API answers.
                                if let Some(v) = resolve_canary_version() {
                                    println!("  canary:   v{v}");
                                }
                                CANARY_TAG.to_string()
                            }
                            ReleaseChannel::Stable => {
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
                                if stable_upgrade_is_current(ver, env!("CARGO_PKG_VERSION"), target)
                                {
                                    println!("  (already on v{ver}; a real run would do nothing)");
                                }
                                format!("v{ver}")
                            }
                        };
                        println!("  platform: {plat}");
                        println!("  archive:  {}", archive_url_for_tag(&tag, plat));
                        println!("  sha256:   {}", checksum_url_for_tag(&tag, plat));
                        println!("  install:  {}", display_path(install_dir));
                    }
                    None => println!(
                        "  (no published archive for this platform: {}/{})",
                        std::env::consts::OS,
                        std::env::consts::ARCH
                    ),
                }
            }
            UpgradeChannel::Unknown => {
                // Mirror the real-run Unknown arm: a canary ask gets the
                // installer hint, not the stable npm command.
                let channel_word = match release_channel {
                    ReleaseChannel::Canary => "canary",
                    ReleaseChannel::Stable => target,
                };
                println!("would upgrade to {channel_word}, but the install channel is unknown");
                println!("  binary: {bin_str}");
                match release_channel {
                    ReleaseChannel::Canary => {
                        println!("  manual: {}", canary_install_hint())
                    }
                    ReleaseChannel::Stable => {
                        println!("  manual: {}", npm_upgrade_command(target))
                    }
                }
            }
        }
        return Ok(0);
    }

    // Homebrew and winget only carry stable releases — a canary ask on those
    // channels has nothing to install, so route the user to the standalone
    // installer rather than silently handing them a stable build. (npm DOES
    // carry canary, as the `canary` dist-tag — handled in the npm arm below.)
    if release_channel == ReleaseChannel::Canary
        && matches!(channel, UpgradeChannel::Homebrew | UpgradeChannel::Winget)
    {
        bail!(
            "nub upgrade: canary builds are not published to {}.\n\
             Install the canary via the standalone installer instead:\n  {}",
            if channel == UpgradeChannel::Homebrew {
                "Homebrew"
            } else {
                "winget"
            },
            canary_install_hint()
        );
    }

    match channel {
        UpgradeChannel::Npm => {
            // The canary channel rides npm's `canary` dist-tag (release.yml
            // publishes every canary build under it), so a canary ask is the
            // same install with a different spec.
            let npm_target = match release_channel {
                ReleaseChannel::Canary => CANARY_TAG,
                ReleaseChannel::Stable => target,
            };
            let cmd = npm_upgrade_command(npm_target);
            println!("running `{cmd}`");
            let status = npm_upgrade_command_invocation(npm_target).status()?;
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
            println!("running `{HOMEBREW_UPGRADE_DISPLAY}`");
            // `brew update` first (#375): `brew upgrade` evaluates the formula
            // from the un-refreshed local tap, so a stale clone reports the
            // installed version as newest and never sees a published release.
            // Best-effort — a transient refresh failure must not block the
            // upgrade, whose own exit code stays authoritative.
            let update_ok = std::process::Command::new("brew")
                .args(HOMEBREW_UPDATE_ARGS)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !update_ok {
                eprintln!(
                    "warning: `brew update` did not succeed; the tap may be stale \
                     and the upgrade may not find a newer version."
                );
            }
            let status = std::process::Command::new("brew")
                .args(HOMEBREW_UPGRADE_ARGS)
                .status()?;
            let code = nub_core::node::spawn::exit_code_from_status(&status);
            if code == 0 {
                if let Some(msg) = shim_relink_reminder() {
                    eprintln!("{msg}");
                }
            }
            Ok(code)
        }
        UpgradeChannel::Winget => {
            // Advisory only — see the enum doc: spawning winget here would try
            // to replace this running nub.exe (fails + corrupts winget's version
            // tracking), and self-swapping would strand winget's bookkeeping.
            bail!(
                "nub upgrade: this nub was installed by winget, which must perform the upgrade \
                 itself.\nRun: {WINGET_UPGRADE_DISPLAY}"
            );
        }
        UpgradeChannel::SelfOwned { install_dir } => {
            if is_canary_build() && release_channel == ReleaseChannel::Stable {
                // Explicit --stable (or --version) from a canary build: make the
                // channel switch visible (bun's "Downgrading … to stable" moment).
                println!(
                    "leaving the canary channel (v{}) for a stable release",
                    env!("CARGO_PKG_VERSION")
                );
            }
            perform_selfowned_upgrade(&install_dir, release_channel, target)?;
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
                if release_channel == ReleaseChannel::Canary {
                    canary_install_hint().to_string()
                } else {
                    npm_upgrade_command(target)
                }
            );
        }
    }
}

/// Best-effort resolve of the canary release's advertised version: release.yml
/// titles the rolling release `Canary <X.Y.Z>-canary.<date>.<run>`, so the
/// release-by-tag API's `name` field carries the version after the word
/// (tolerating a bare or `v`-prefixed name too). `None` on ANY failure — the
/// rolling-tag download needs no version resolution, so a flaky API must never
/// block a canary upgrade; it only costs the version label and the
/// already-up-to-date short-circuit.
fn resolve_canary_version() -> Option<String> {
    let api = release_canary_api();
    let out = std::process::Command::new("curl")
        .args(["--fail", "--silent", "--location", &api])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let name = body.get("name")?.as_str()?.trim();
    let version = name
        .strip_prefix("Canary ")
        .unwrap_or(name)
        .trim()
        .trim_start_matches('v');
    if version.is_empty() {
        return None;
    }
    Some(version.to_string())
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

/// Whether a resolved stable release is the one already running, so the upgrade
/// can report "already on the latest" instead of re-downloading identical bytes
/// (#664). Generalizes the arm the canary channel has always had.
///
/// Two cases deliberately do NOT short-circuit, because in both the versions
/// matching does not mean the install is what the user asked for:
///
/// - An explicit `--version X` is a re-install request. Matching the running
///   version is exactly when a user repairs a damaged install, so honor it.
/// - A canary build carries the version it was cut from, so a canary at
///   `0.7.4-canary.…` can resolve `latest` to a stable `0.7.4` it is NOT
///   running. `--stable` off a canary is a channel switch and must download.
fn stable_upgrade_is_current(resolved: &str, running: &str, version_spec: &str) -> bool {
    version_spec == "latest" && !running.contains("-canary.") && resolved == running
}

/// Download + SHA-256-verify + atomic-swap a release archive into a self-owned
/// `~/.nub` install. Mirrors install.sh's layout exactly: the archive contains
/// `bin/` + `runtime/`, extracted into `<install_dir>` after replacing the prior
/// `bin/`+`runtime/`.
///
/// Atomicity contract ([upgrade.md#atomicity]): on any failure post-download the
/// existing install is untouched. We stage the extraction in a sibling temp dir
/// on the same filesystem, verify the SHA-256 before extracting, then swap the
/// new `bin`/`runtime` into place via directory rename (atomic per-dir on POSIX);
/// the prior dirs move aside to `.old` first and are GC'd on success. Windows
/// cannot take the directory-rename path at all — see [`swap_bin_files_windows`]
/// for the per-file rename dance and its own all-or-nothing contract.
///
/// MANUAL-VERIFICATION NOTE: the download/verify/rename path is network- and
/// release-artifact-hard to unit-test (it needs a live GitHub release + a real
/// platform tarball). It is verified ad hoc via `nub upgrade --dry-run` (which
/// prints channel, URL, sha source, and install dir) and by running a real
/// `nub upgrade` against a published release once one exists. The pure pieces —
/// `detect_channel`, `archive_url`, `checksum_url`, `platform_target`,
/// `sha256_hex`, and sidecar parsing — are individually exercised; the glue here
/// is kept deliberately linear and small so its correctness is reviewable by eye.
fn perform_selfowned_upgrade(
    install_dir: &Path,
    release_channel: ReleaseChannel,
    version_spec: &str,
) -> Result<()> {
    let target = platform_target().ok_or_else(|| {
        anyhow::anyhow!(
            "nub upgrade: no published archive for this platform ({}/{}). \
             Reinstall via the install script instead.",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let (tag, label) = match release_channel {
        ReleaseChannel::Stable => {
            let version = resolve_version(version_spec)?;
            if stable_upgrade_is_current(&version, env!("CARGO_PKG_VERSION"), version_spec) {
                // The resolve above is the same API call the download path
                // needed anyway, so the check costs nothing and saves the whole
                // archive fetch in the no-op case.
                println!("already on the latest release (v{version})");
                return Ok(());
            }
            (format!("v{version}"), format!("v{version}"))
        }
        ReleaseChannel::Canary => match resolve_canary_version() {
            Some(v) => {
                if v == env!("CARGO_PKG_VERSION") {
                    // bun-mirror: a no-op canary upgrade says so instead of
                    // re-downloading identical bytes.
                    println!("already on the latest canary build (v{v})");
                    println!("to return to the latest stable release: nub upgrade --stable");
                    return Ok(());
                }
                (CANARY_TAG.to_string(), format!("canary v{v}"))
            }
            // The rolling tag downloads without resolution; a flaky API only
            // costs the label + the up-to-date short-circuit.
            None => (CANARY_TAG.to_string(), "canary".to_string()),
        },
    };
    let url = archive_url_for_tag(&tag, target);
    let sha_url = checksum_url_for_tag(&tag, target);

    // Name the version being left as well as the one arriving: an upgrade that
    // prints only its destination gives no way to tell a real move from a
    // re-install (#664).
    println!(
        "upgrading from v{} to {label} ({target})",
        env!("CARGO_PKG_VERSION")
    );

    // Stage downloads + extraction in a sibling temp dir on the same filesystem
    // as the install so the final swap is a same-filesystem rename (atomic).
    let staging = tempfile::Builder::new()
        .prefix(".nub-upgrade-")
        .tempdir_in(install_dir)
        .context("nub upgrade: could not create staging directory")?;
    let archive_path = staging.path().join(format!("nub.{}", archive_ext(target)));

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

    // Extract into a fresh `staged/` subdir (the archive carries bin/ plus a
    // vestigial empty runtime/ that this path ignores — see the swap below),
    // matching install.sh's `tar -xzf … -C $install_dir`.
    let staged_root = staging.path().join("staged");
    std::fs::create_dir_all(&staged_root)?;
    let extracted = extract_release_archive(&archive_path, target, &staged_root)
        .context("nub upgrade: failed to invoke tar")?;
    if !extracted {
        bail!("nub upgrade: failed to extract archive {url}");
    }
    let new_bin = staged_root.join("bin");
    if !new_bin.join(NUB_EXE).is_file() {
        bail!("nub upgrade: downloaded archive did not contain bin/{NUB_EXE}");
    }

    // RESILIENCE CONTRACT: `bin/nub` is the ONLY component an upgrade hard-requires.
    // Everything else the archive ships or the prior install left behind (a
    // runtime/ sidecar, the nubx alias) is OPTIONAL and handled best-effort —
    // verify the download fully (checksum above), swap the binary, then never let a
    // missing/extra/shape-changed optional component abort an otherwise-successful
    // upgrade. This is the lesson of the sidecar→bin-only transition: the v0.1.x
    // upgrader hard-bailed when the runtime/ it expected was absent and stranded
    // every user, so the new upgrader must tolerate artifact-shape evolution in
    // BOTH directions (an absent optional, an unexpected extra). The archive may or
    // may not carry runtime/ (current releases ship a vestigial empty one only to
    // satisfy old upgraders — see release.yml); either way the embedded-runtime
    // binary ignores ~/.nub/runtime, so any prior runtime/ is dead weight removed
    // best-effort for hygiene (mirrors install.sh / install.ps1).
    #[cfg(windows)]
    swap_bin_files_windows(install_dir, &new_bin)?;
    #[cfg(not(windows))]
    swap_dir(install_dir, "bin", &new_bin)?;
    let _ = std::fs::remove_dir_all(install_dir.join("runtime"));

    // The release tarball ships `bin/nub` — one binary; there is no `bin/nubx` any
    // more, and `install.sh` / this upgrade path both create that alias themselves as
    // a symlink. It arrives at mode 0644 — the
    // upload-artifact → download-artifact round-trip in CI strips the executable
    // bit, so the published archive is non-executable. install.sh heals fresh
    // installs with its own `chmod +x`; the self-owned upgrade path must do the
    // same or every upgrade leaves `~/.nub/bin/nub` as `-rw-r--r--` and the next
    // invocation is "command not found" / a silent fall-back to a stale npm binary.
    // Set +x on the freshly-swapped-in binary before it can be invoked. (Not a
    // Windows concern — executability there is by extension, not a mode bit.)
    #[cfg(not(windows))]
    ensure_bin_executable(&install_dir.join("bin").join(NUB_EXE))?;

    // Recreate the `nubx` alias install.sh creates (relative symlink → nub; the CLI
    // dispatches on argv[0], so only the alias NAME matters — see Argv0::detect).
    // BEST-EFFORT per the resilience contract above: the binary is already swapped
    // and executable, so `nub` works regardless. nubx is a derived convenience —
    // its recreation failing (an exotic FS, a permissions quirk) must NOT abort an
    // otherwise-successful upgrade; warn and continue rather than bail. POSIX-only:
    // on Windows the nubx COPY is refreshed inside `swap_bin_files_windows`.
    #[cfg(unix)]
    {
        let nubx = install_dir.join("bin").join("nubx");
        let _ = std::fs::remove_file(&nubx);
        if let Err(e) = std::os::unix::fs::symlink("nub", &nubx) {
            eprintln!(
                "nub upgrade: warning: could not recreate the nubx alias at {} ({e}); \
                 `nub` is upgraded and usable. Re-run the installer to restore nubx.",
                nubx.display()
            );
        }
    }

    println!("installed {label} to {}", display_path(install_dir));
    Ok(())
}

/// The Windows half of the swap: per-FILE renames instead of the POSIX
/// directory rename. Two Windows filesystem facts shape it (upgrade.md#windows):
/// a memory-mapped (running) executable cannot be deleted or overwritten, but
/// its directory ENTRY can be renamed — and a directory containing a mapped
/// executable cannot be renamed at all, which rules out [`swap_dir`] here
/// entirely. So: move the live `nub.exe` aside to `nub.exe.old` (succeeds even
/// mid-run; rustup and uv ride the same fact), rename the staged binary into
/// place, then refresh the `nubx.exe` COPY from it (install.ps1 ships nubx as a
/// copy — symlinks need admin/Developer Mode), then install the `busybox.exe`
/// sidecar and overwrite the remaining bin/ sidecars so none can go stale
/// against the new binary.
///
/// All-or-nothing (the upgrade.md#atomicity contract, per-file form): if the
/// first rename fails the install is untouched; if the second fails the old
/// binary is rolled back into place. The `.old` files cannot be deleted while
/// the old binary is still executing (this very process), so they are left
/// behind and GC'd best-effort at the START of the next upgrade — deliberately
/// never on the hot run path, which stays free of upgrade bookkeeping. The
/// pre-GC also clears the way for the rename: `std::fs::rename` on Windows
/// replaces an existing destination only when it isn't locked, and a stale
/// `.old` still held by a long-lived pre-upgrade process would otherwise block
/// the dance (the error message says to close old nub processes and retry).
#[cfg(windows)]
fn swap_bin_files_windows(install_dir: &Path, staged_bin: &Path) -> Result<()> {
    let bin_dir = install_dir.join("bin");
    std::fs::create_dir_all(&bin_dir)
        .with_context(|| format!("could not create {}", display_path(&bin_dir)))?;
    let nub = bin_dir.join("nub.exe");
    let nub_old = bin_dir.join("nub.exe.old");
    let nubx = bin_dir.join("nubx.exe");
    let nubx_old = bin_dir.join("nubx.exe.old");
    let busybox = bin_dir.join("busybox.exe");
    let busybox_old = bin_dir.join("busybox.exe.old");

    let _ = std::fs::remove_file(&nub_old);
    let _ = std::fs::remove_file(&nubx_old);
    let _ = std::fs::remove_file(&busybox_old);

    if nub.exists() {
        std::fs::rename(&nub, &nub_old).with_context(|| {
            format!(
                "nub upgrade: could not move the running {} aside. If a stale \
                 nub.exe.old is held open by another running nub process, close \
                 it and retry; the install has not been modified.",
                display_path(&nub)
            )
        })?;
    }
    if let Err(e) = std::fs::rename(staged_bin.join("nub.exe"), &nub) {
        // Roll the old binary back so the install keeps working.
        let _ = std::fs::rename(&nub_old, &nub);
        return Err(e)
            .with_context(|| format!("nub upgrade: could not install {}", display_path(&nub)));
    }

    // nubx refresh is BEST-EFFORT per the resilience contract: `nub` is already
    // swapped and authoritative. A running nubx.exe blocks the delete but not
    // the rename-aside; if even that fails, warn and leave the stale copy.
    if nubx.exists() && std::fs::remove_file(&nubx).is_err() {
        let _ = std::fs::rename(&nubx, &nubx_old);
    }
    if let Err(e) = std::fs::copy(&nub, &nubx) {
        eprintln!(
            "nub upgrade: warning: could not refresh the nubx alias at {} ({e}); \
             `nub` is upgraded and usable. Re-run the installer to restore nubx.",
            display_path(&nubx)
        );
    }

    // busybox.exe is nub's bundled POSIX shell for `nub run`, and unlike nubx it
    // is NOT derivable from nub.exe — it has to come out of the archive. Archives
    // before v0.6.0 carried no busybox at all, so an upgrade from one of those
    // used to install nub.exe and leave `nub run` hard-erroring with "nub's
    // bundled POSIX shell (busybox.exe) was not found" (resolve_bundled_busybox
    // has no fallback, by design). Copy whatever the archive staged.
    //
    // Guarded on the staged file EXISTING, not on the destination: an archive
    // that stops shipping busybox must not abort the upgrade, per the resilience
    // contract above. Best-effort for the same reason nubx is. It gets the
    // rename-aside dance rather than the generic refresh below because busybox
    // can be RUNNING (it is the shell `nub run` spawns), and a rename over a
    // running image fails on Windows.
    let staged_busybox = staged_bin.join("busybox.exe");
    if staged_busybox.is_file() {
        if busybox.exists() && std::fs::remove_file(&busybox).is_err() {
            let _ = std::fs::rename(&busybox, &busybox_old);
        }
        if let Err(e) = std::fs::rename(&staged_busybox, &busybox) {
            eprintln!(
                "nub upgrade: warning: could not install the bundled shell at {} ({e}); \
                 `nub` is upgraded and usable, but `nub run` may fail until you re-run \
                 the installer.",
                display_path(&busybox)
            );
        }
    }

    // Everything ELSE the archive ships in bin/ — the `nub compile` launcher
    // template above all — must travel with the binary, or an upgraded install
    // silently keeps the PREVIOUS release's copies. The POSIX path gets that free
    // by swapping the whole directory. Here `nub.exe` was renamed OUT of the
    // staging dir above, nubx is refreshed from it, and busybox is installed by
    // the block just above, so all three are skipped and everything remaining is
    // refreshed. Best-effort per the resilience contract: nub is already swapped
    // and authoritative.
    //
    // Staged as temp-then-rename rather than remove-then-copy: a remove that
    // succeeds followed by a copy that fails (AV lock, full disk) would leave the
    // install with NO copy of the file at all — strictly worse than the
    // stale-but-working one the failure started from.
    if let Ok(entries) = std::fs::read_dir(staged_bin) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == "nubx.exe"
                || name == "busybox.exe"
                || !entry.file_type().is_ok_and(|t| t.is_file())
            {
                continue;
            }
            let dest = bin_dir.join(&name);
            let tmp = bin_dir.join(format!("{}.new", name.to_string_lossy()));
            let staged =
                std::fs::copy(entry.path(), &tmp).and_then(|_| std::fs::rename(&tmp, &dest));
            if let Err(e) = staged {
                let _ = std::fs::remove_file(&tmp);
                eprintln!(
                    "nub upgrade: warning: could not refresh {} ({e}); the previous \
                     copy is left in place and `nub` is upgraded and usable.",
                    dest.display()
                );
            }
        }
    }
    Ok(())
}

/// Atomically replace `<install_dir>/<name>` with `new_src` via rename: move any
/// existing dir to a `.old` sibling (which we then remove), rename the staged dir
/// into place. Same-filesystem, so each rename is atomic on POSIX. (Unix-only:
/// on Windows renaming a directory that contains the running executable fails —
/// see [`swap_bin_files_windows`].)
#[cfg(not(windows))]
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

/// Make a freshly-downloaded binary runnable: set the executable bit
/// (0o755), then clear `com.apple.quarantine`.
///
/// The release archive ships the binary at 0o644 — CI's
/// upload/download-artifact round-trip strips the +x install.sh would
/// otherwise rely on — so the self-owned upgrade path must re-apply it or
/// the upgraded `nub` is non-executable. Both callers hand this an
/// archive nub itself downloaded and checksum-verified, and both then run
/// it (the upgrade swap, and the self-shim's provision-then-exec), so the
/// quarantine strip belongs on the same seam; see `drop_quarantine`.
/// Not compiled on Windows (executability is by extension, not a mode
/// bit, and quarantine is a macOS concept).
#[cfg(not(windows))]
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
    drop_quarantine(path);
    Ok(())
}

/// Drop `com.apple.quarantine` from a binary nub just downloaded.
///
/// Released nub binaries are ad-hoc signed — `codesign` reports
/// `adhoc, linker-signed` with no Team ID — and macOS kills a quarantined
/// ad-hoc-signed executable on exec rather than merely warning about it.
/// `curl` and `tar` run as children of this process, so when nub is invoked
/// from a terminal that inherited quarantine flags (one embedded in a
/// Gatekeeper-enabled app), everything they write is stamped: the upgrade
/// would install a nub that cannot start, and the self-shim would exec one.
/// Both archives are checksum-verified before use, and the attribute is not
/// a judgement about them — it reflects only which terminal the upgrade was
/// run from. See `nub_core::quarantine` for the full rationale.
///
/// Best-effort by the resilience contract in `perform_selfowned_upgrade`:
/// the binary is already swapped and executable, so a failure warns with the
/// manual fix rather than aborting.
#[cfg(not(windows))]
fn drop_quarantine(path: &Path) {
    if let Err(err) = nub_core::quarantine::clear(path) {
        eprintln!(
            "nub: warning: could not clear com.apple.quarantine on {p} ({err}); \
             if macOS refuses to run it, clear it with: xattr -d com.apple.quarantine {p}",
            p = path.display()
        );
    }
}

/// Extract a release archive into `dest` by shelling to `tar`, returning whether
/// tar succeeded (spawn failure is the `Err`). Handles both artifact formats:
/// `.tar.gz` under `-xzf`, and the win32 `.zip` under plain `-xf` — `tar` on
/// Windows IS bsdtar (in System32 since Windows 10 1803, alongside the curl.exe
/// this channel already requires), which auto-detects zip on read, so no unzip
/// dependency is needed. `-z` stays on the tar.gz path only because GNU tar
/// (Linux) does not auto-detect under an explicit `-z` mismatch.
fn extract_release_archive(archive: &Path, target: &str, dest: &Path) -> Result<bool> {
    let status = std::process::Command::new("tar")
        .arg(if archive_ext(target) == "zip" {
            "-xf"
        } else {
            "-xzf"
        })
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .context("failed to invoke tar")?;
    Ok(status.success())
}

/// Download `url` to `dest` via curl (the same tool install.sh uses — keeps Nub
/// free of a bundled HTTP/TLS stack and inherits the user's CA + proxy config).
/// Shared with `compile::launcher`, which pulls launcher templates from the same
/// release: one transport for every release asset, so the `file://` test seam
/// works for both.
pub(crate) fn curl_download(url: &str, dest: &Path) -> Result<()> {
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
pub(crate) fn fetch_expected_sha256(sha_url: &str) -> Result<String> {
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
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Provision an exact nub version into the version-addressed self store and return
/// the path to its `bin/nub` — the delegated-artifact half of the self-shim
/// (`self_shim.rs` owns the decision). Reuses the `nub upgrade` release channel:
/// download `<base>/v<ver>/nub-<target>.<ext>`, SHA-256-verify it against the
/// published `.sha256` sidecar BEFORE extracting, then confirm the extracted
/// binary reports the expected version before it is trusted for a default-on exec.
///
/// INTEGRITY: nub's own `packageManager` self-pin carries no `+sha512` (a
/// pnpm/yarn pin's hash covers a platform-independent npm tarball; nub's release
/// artifact is per-platform, so a single pin hash could not cover all 8), so this
/// release-channel checksum is the integrity anchor. A verified store entry
/// (`<cache>/self/<ver>/bin/nub`) IS the trust cache — a hit is network- and
/// verification-free. Atomic: stage in a sibling temp dir, then rename into place.
fn provision_self(version: &str) -> Result<PathBuf> {
    let store = pm_store_root()?.join("self");
    let final_dir = store.join(version);
    let bin = final_dir.join("bin").join(NUB_EXE);
    if bin.is_file() {
        return Ok(bin); // verified store hit — silent, no network
    }

    // "No build for this platform" is a distinct, actionable failure — surfaced
    // BEFORE any download so it never reads as a network error. The 8-platform
    // release model means a pin can exist for linux-x64 and not win32-arm64.
    let target = platform_target().ok_or_else(|| {
        anyhow::anyhow!(
            "{ERR_NUB_SELF_SHIM}: this project pins nub@{version}, but nub publishes no build \
             for this platform ({}/{}). Set NUB_SELF_SHIM=0 to run with your installed nub.",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let url = archive_url(version, target);
    let sha_url = checksum_url(version, target);

    std::fs::create_dir_all(&store)
        .with_context(|| format!("creating the nub self store at {}", store.display()))?;
    // Defense-in-depth: the self store holds binaries this process EXECs, and a
    // store hit skips re-verification, so restrict it to the owner (0700) — a
    // shared/world-writable cache dir can't be used to pre-plant a binary a later
    // run would exec unverified. Best-effort; a failure here doesn't block the
    // provision (the verify-before-exec chain below is the real gate).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700));
    }
    let staging = tempfile::Builder::new()
        .prefix(&format!(".self-{version}-"))
        .tempdir_in(&store)
        .context("creating a staging dir for the pinned-nub provision")?;
    let archive = staging.path().join(format!("nub.{}", archive_ext(target)));

    eprintln!("nub: provisioning pinned nub@{version} ({target})...");
    curl_download(&url, &archive).map_err(|_| {
        anyhow::anyhow!(
            "{ERR_NUB_SELF_SHIM}: could not download the pinned nub@{version} from {url} — the \
             version may not exist or may have been yanked. Set NUB_SELF_SHIM=0 to run with \
             your installed nub."
        )
    })?;

    // Verify BEFORE extracting — a checked digest gates the executable landing on
    // disk, the same order `perform_selfowned_upgrade` uses.
    let expected = fetch_expected_sha256(&sha_url).map_err(|_| {
        anyhow::anyhow!(
            "{ERR_NUB_SELF_SHIM}: could not fetch the checksum for nub@{version} from {sha_url}. \
             Set NUB_SELF_SHIM=0 to run with your installed nub."
        )
    })?;
    let actual = sha256_hex(&std::fs::read(&archive)?);
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!(
            "{ERR_NUB_SELF_SHIM}: checksum mismatch for the pinned nub@{version}\n  \
             expected: {expected}\n  actual:   {actual}\n\
             Refusing to run a corrupted or tampered nub binary. \
             Set NUB_SELF_SHIM=0 to run with your installed nub."
        );
    }

    let staged = staging.path().join("staged");
    std::fs::create_dir_all(&staged)?;
    let extracted = extract_release_archive(&archive, target, &staged)
        .context("invoking tar to extract the pinned nub")?;
    if !extracted {
        bail!(
            "{ERR_NUB_SELF_SHIM}: failed to extract the pinned nub@{version} archive. \
             Set NUB_SELF_SHIM=0 to run with your installed nub."
        );
    }
    let staged_bin = staged.join("bin").join(NUB_EXE);
    if !staged_bin.is_file() {
        bail!(
            "{ERR_NUB_SELF_SHIM}: the pinned nub@{version} archive did not contain bin/{NUB_EXE}. \
             Set NUB_SELF_SHIM=0 to run with your installed nub."
        );
    }
    // CI's upload/download-artifact round-trip strips +x from the archived binary
    // (see perform_selfowned_upgrade) — re-apply it before the version probe.
    #[cfg(not(windows))]
    ensure_bin_executable(&staged_bin)?;

    // Provision-then-verify: a version-mismatched or corrupt artifact hard-errors
    // HERE instead of exec-looping. The tarball was already checksum-verified and
    // the store path is version-addressed, so this is belt-and-suspenders — but it
    // is the last gate before a default-on exec, so it stays.
    verify_provisioned_version(&staged_bin, version)?;

    // Atomic place. A concurrent provisioner may have won the race — keep theirs.
    if !bin.is_file() {
        if let Err(e) = std::fs::rename(&staged, &final_dir) {
            if !bin.is_file() {
                return Err(e).with_context(|| {
                    format!(
                        "installing pinned nub@{version} into {}",
                        final_dir.display()
                    )
                });
            }
        }
    }
    eprintln!("nub: provisioned nub@{version}");
    Ok(final_dir.join("bin").join(NUB_EXE))
}

/// Run the freshly-provisioned binary's `--version` and confirm it reports the
/// expected version — the loop-safety gate before a default-on exec. `nub
/// --version` prints a bare `v<X.Y.Z>` on its first stdout line. The probe carries
/// the re-entry guard so it can never itself delegate (`--version` short-circuits
/// before the dispatcher anyway, but the guard is cheap insurance).
fn verify_provisioned_version(bin: &Path, expected: &str) -> Result<()> {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .env(crate::self_shim::SELF_DISPATCHED_ENV, expected)
        .output()
        .with_context(|| format!("probing {} --version", bin.display()))?;
    if !out.status.success() {
        bail!(
            "{ERR_NUB_SELF_SHIM}: the provisioned nub@{expected} failed its --version probe. \
             Set NUB_SELF_SHIM=0 to run with your installed nub."
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let reported = stdout
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('v');
    if reported != expected {
        bail!(
            "{ERR_NUB_SELF_SHIM}: the provisioned binary reports nub@{reported}, expected \
             nub@{expected} — refusing to run a version-mismatched artifact. \
             Set NUB_SELF_SHIM=0 to run with your installed nub."
        );
    }
    Ok(())
}

/// Provision the pinned nub and hand off to it: replace the process image (Unix
/// `exec`) / spawn+wait (Windows) with the pinned binary, running the ORIGINAL
/// argv so the pinned nub applies its own CLI grammar (it may accept flags this
/// nub would reject). The child carries `__NUB_SELF_DISPATCHED=<version>`, whose
/// presence suppresses ALL further delegation — the exec-loop guard. Never returns
/// on success (Unix).
fn delegate_to_self(version: &str) -> Result<i32> {
    let bin = provision_self(version)?;
    // The unmodified original argv (env::args is stable within the process).
    let args: Vec<String> = std::env::args().skip(1).collect();
    let envs = vec![(
        crate::self_shim::SELF_DISPATCHED_ENV.to_string(),
        version.to_string(),
    )];
    exec_program(&bin, &args, &envs)
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
    "run",
    "watch",
    "exec",
    "nubx",
    "upgrade",
    "install",
    "i",
    "ci",
    "init",
    #[cfg(feature = "compile")]
    "compile",
];

/// True for any word `nub <word> -h` / `nub help <word>` can route to a real help
/// page: a native clap command, the `node`/`pm`/`agent` groups, or an engine verb
/// (canonical or alias). Unknown words fall through to the top-level page instead
/// of exiting silently — the routing inconsistency the help-router fix addresses.
fn is_help_routable(word: &str) -> bool {
    CLAP_HELP_COMMANDS.contains(&word)
        || matches!(word, "node" | "pm" | "agent" | "global")
        || crate::pm_engine::lookup_verb(word).is_some()
}

/// True when a non-forwarding command group was asked for its help. The three
/// are `nub pm`, `nub node` and `nub agent` — the groups that bypass clap for a
/// manual sub-verb match, so each has to recognize its own help.
///
/// A help FLAG counts ANYWHERE in the group's argv, not just at argv[0]: the
/// top-level scan stops matching nub's own flags once a subcommand is seen (the
/// three-position rule, so `nub run build --watch` reaches the script), which is
/// right for the forwarding commands but leaves these groups to parse their own
/// help. Without the flag-anywhere rule each group's help GUARD inspected argv[0]
/// alone, and past it the flag met whatever the verb does with its arguments — so
/// the same defect surfaced three ways. A verb taking no arguments ignored the
/// flag and RAN: `nub pm shim --help` installs the shims and `nub pm unshim
/// --help` removes them, both editing shell startup files a user was only asking
/// about (#653). A verb taking a value consumed it as a bad one (`no published
/// Node version matches "--help"`). And `nub agent docs` rejected it outright
/// (`unexpected argument '--help'`).
///
/// Safe as a blanket scan because no group forwards argv to a child process:
/// their only argument consumers take a package-manager name, a version, or a
/// `/docs/...` slug, and no help flag is a valid value for any of them.
pub(crate) fn group_help_requested(args: &[String]) -> bool {
    args.first().is_some_and(|a| a == "help") || args.iter().any(|a| a == "--help" || a == "-h")
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

    // `node` / `pm` / `agent` / `global`: bespoke usage (their own help guards print the
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
        "global" => {
            let _ = run_global(&["--help".to_string()]);
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

/// Bold a header for the help pages when color is on for stdout. Plain text
/// otherwise, so piped/redirected help stays clean. Shares [`color_enabled`] with
/// the stream-prefix coloring so one `--color` answers for every surface.
fn help_bold(s: &str) -> String {
    let color = color_enabled(std::io::IsTerminal::is_terminal(&std::io::stdout()));
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
  init                        scaffold a new project
{compileverb}
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
    dlx, x / create           fetch and run a package or create template

  Publish and registry:
    publish / pack / version  publish, pack, or bump the package version
    dist-tag / deprecate / undeprecate / unpublish
    login / logout / whoami / owner / token / stage

  Store and config:
    store / cache             inspect and maintain the content-addressable store
    cat-file / cat-index / find-hash
    config, c / get / set / pkg / set-script
    global config             manage user configuration

{toolchain}
  node                        manage Node versions (install / ls / uninstall / pin)
  pm                          manage the project's package manager (which / use / shim)
  upgrade                     upgrade nub itself

{footer}
",
        v = v,
        usage = help_bold("Usage:"),
        headline = help_bold("Headline commands:"),
        compileverb = COMPILE_HELP_LINE,
        runtime = help_bold("Runtime options (passed through to Node):"),
        pm = help_bold("Package manager commands:"),
        toolchain = help_bold("Manage the toolchain:"),
        footer = help_bold(
            "See `nub --help` for the expanded Node flag + environment-variable reference."
        )
    )
}

/// The `compile` entry for the two help surfaces, or nothing when the verb is not
/// in this build. Release binaries pass `--features compile`, so users see it; a
/// feature-off dev build must not advertise a verb it would reject. Each surface
/// gets its own spelling because their indentation and grouping differ, and the
/// verbose one carries its section heading so the heading disappears with it
/// rather than leaving an empty group.
#[cfg(feature = "compile")]
const COMPILE_HELP_LINE: &str = "  compile <file>              build a standalone executable\n";
#[cfg(not(feature = "compile"))]
const COMPILE_HELP_LINE: &str = "";

#[cfg(feature = "compile")]
const COMPILE_HELP_SECTION: &str =
    "  Build:\n    compile                  build a file into a standalone executable\n\n";
#[cfg(not(feature = "compile"))]
const COMPILE_HELP_SECTION: &str = "";

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
    dlx <pkg> / x <pkg>      fetch-and-run a package's bin (also: nubx)
    watch <file>             run a file in watch mode

  Start a project:
    init                     scaffold a new project (TypeScript-first)

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

{compileverb}  Manage the toolchain:
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
  --no-color           disable color (same as --color=never)
  --env-file <file>    load environment variables from <file>
  --env-file-if-exists <file>  like --env-file, but skip silently if <file> is absent
  --no-env-file        load no env files: no `.env*` auto-discovery, no --env-file
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
  --env-file-if-exists <file>    like --env-file, but skip silently if the file is absent
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
        compileverb = COMPILE_HELP_SECTION,
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
             \x20\x20The pin comes from .node-version / .nvmrc / .tool-versions / engines.node / devEngines.runtime."
        ),
        other => anyhow::Error::new(other),
    })
}

/// `nub node …` — the version-management command group (install / ls / uninstall
/// / pin). Non-forwarding; manual sub-verb match so the bare-usage and the
/// `nub node <file>` error read exactly as the spec specifies.
/// Spec: `internal/commands/node-versions.md`.
fn run_node(args: &[String]) -> Result<i32> {
    let cwd = env::current_dir()?;
    publish_node_executable_best_effort(&cwd);
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
         \x20 pin <version>            write the project's Node pin\n\
         \x20 shim                     make `node` on PATH resolve through nub (re-run after `nub upgrade`)\n\
         \x20 unshim                   remove the `node` shim and its PATH block";

    // A help request — `help` at argv[0], or `--help`/`-h` at any position,
    // including after the verb: the short usage listing the verbs.
    let verb = args.first().map(String::as_str);
    if group_help_requested(args) {
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
            println!("  resolved  {}", resolution_source(&cwd, &node));
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
            eprintln!("» resolved from {}", resolution_source(&cwd, &node));
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
                bail!("nub node pin requires a version (e.g. nub node pin 26)");
            };
            let result = manage::pin(version, &cwd)?;
            println!("pinned Node {} → {}", result.spec, result.path.display());
            Ok(0)
        }
        "shim" => run_node_shim_install(),
        "unshim" => run_node_unshim(),
        // `nub node <file>` (or any non-verb positional) is an error, NOT a
        // passthrough — the exact wording is locked by the spec
        // (node-versions.md line 25). The literal `<file>` placeholder is part of
        // the locked string; do NOT interpolate the typed token (it would both
        // drop trailing args and diverge from the spec).
        _ => {
            bail!(
                "nub node takes a subcommand (which, install, ls, uninstall, pin, shim, unshim). \
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
            if let Err(e) =
                serde_json::from_str::<serde_json::Value>(nub_core::strip_utf8_bom(&content))
            {
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
/// the v0 policy (`identity-policy` (no such document), Axiom 3).
fn run_pm(args: &[String]) -> Result<i32> {
    use nub_core::pm::Pm;
    use nub_core::pm::resolve::{self, PmTarget};
    use std::io::Write as _;

    let cwd = env::current_dir()?;

    let verb = args.first().map(String::as_str);
    if verb.is_none() || group_help_requested(args) {
        println!(
            "nub pm — manage the project's package manager\n\n\
             Usage: nub pm <command>\n\n\
             Commands:\n\
             \x20 which              print the resolved package-manager path (why → stderr)\n\
             \x20 use <pm>[@<spec>]  declare the project's package manager (npm|pnpm|yarn|bun|nub;\n\
             \x20                    default: latest) — writes packageManager and aligns the lockfile;\n\
             \x20                    `use nub` migrates the full config surface, `use pnpm` reverses it\n\
             \x20 pin [<version>]    lock this project to an exact nub version (default: the running nub)\n\
             \x20 update             re-resolve within the pinned range and bump the pin (alias: up)\n\
             \x20 cache [clear]      list cached package managers (or clear the cache)\n\
             \x20 shim               link npm/pnpm/yarn shims onto PATH (re-run after `nub upgrade`)\n\
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
                println!("{}", display_path(&exe));
                std::io::stdout().flush().ok();
                // Name the field the nub identity actually resolved from — the
                // virgin/bare-`use` path pins via `devEngines.packageManager`
                // (no exact `packageManager`), only `use nub@<exact>` writes the
                // latter.
                let field = field_pin_provenance(&cwd);
                eprintln!("» this project uses nub (resolved from {field})");
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
        // `identity-policy` (no such document) §`nub pm use`). Declarative
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
            // A project may DECLARE a manager nub doesn't provision (bun, or any
            // name outside npm/pnpm/yarn). `resolve_pin_with_source` reads such a
            // pin as unresolvable, so the generic "no pin — declare one" context
            // below would dead-end the user: they already declared it, and
            // `nub pm use <name>` can't provision it either. Name the real
            // identity and its own update path instead. Excludes nub (its own
            // manager) and Berry (which resolves below — the YarnBerry arm owns
            // that message). Mirrors the `which` verb's declared-unprovisionable
            // branch.
            if let Some(id) = resolve::project_pm_identity(&cwd)
                && id.name != "nub"
                && !id.berry
                && resolve::resolve_pin_with_source(&cwd).is_none()
            {
                let name = &id.name;
                bail!(
                    "this project is pinned to {name} — nub doesn't provision or update {name}. \
                     Update {name} with its own tooling (e.g. `{name} upgrade`)."
                );
            }
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
        // Lock the project to an EXACT nub version — the sole writer of the
        // hard `packageManager: "nub@<v>"` pin that arms nub's self-shim
        // (provision+delegate when the pinned nub ≠ the running one). Writes only
        // the two identity fields; the heavier into-nub migration (lockfile,
        // pnpm-workspace.yaml, settings) is `use nub`'s job. Symmetric with
        // `nub node pin <version>`.
        "pin" => run_pm_pin(args.get(1).map(String::as_str), &cwd),
        // Install / remove the PM shims (spec: `package-manager-shims` (no such document)).
        "shim" => run_pm_shim_install(),
        "unshim" => run_pm_unshim(),
        // `switch` (the old cross-PM, declaration-only verb) was replaced by
        // `use` (2026-06-10, identity-policy ratification) — name the successor
        // instead of the generic unknown. (`pin` is a live verb again above, with
        // its new nub-lock meaning — not the retired incumbent-version pin.)
        "switch" => bail!(
            "`nub pm switch` was replaced by `nub pm use <pm>[@<spec>]` — one verb declares \
             the package manager and aligns the lockfile."
        ),
        _ => {
            bail!("nub pm takes a subcommand (which, use, pin, update (up), cache, shim, unshim).")
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
/// honest `+sha512` for the pin (`package-manager-provisioning` (no such document)
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
/// identity) takes an optional EXACT version: bare `nub` writes the non-locking
/// devEngines caret range, `nub@<exact>` opts into the hard `packageManager`
/// pin. A range/dist-tag spec for nub is refused — nub is the running binary,
/// not a registry package, so there is nothing to resolve a range against.
fn split_pm_arg(arg: &str) -> Result<(&str, Option<&str>)> {
    let (name, spec) = match arg.split_once('@') {
        Some((n, s)) => (n, Some(s.trim())),
        None => (arg, None),
    };
    // `nub pm use nub@<exact>` is the opt-in HARD pin (writes `packageManager:
    // nub@<v>`); bare `nub pm use nub` is the non-locking devEngines range. nub
    // is the running binary, not a registry package, so its spec must be an
    // EXACT semver to pin — a dist-tag or range (`next`, `^1`, `1.x`, `1`) has
    // nothing to resolve against (unlike npm/pnpm/yarn, whose specs resolve via
    // the registry), so it never reaches the `packageManager` field.
    if name == "nub"
        && let Some(s) = spec
        && s != "latest"
        && semver::Version::parse(s).is_err()
    {
        bail!(
            "`nub pm use nub@{s}` needs an exact version (e.g. nub@{0}) — nub is the \
             running binary, not a registry package, so a range/tag can't be resolved. \
             Bare `nub pm use nub` writes a non-locking devEngines range instead.",
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
/// policy 2026-06-10 — `identity-policy` (no such document) §`nub pm use`):
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
    // Host-match the registry auth to the resolved tarball before downloading it:
    // a (malicious/MITM) packument's `dist.tarball` can name a foreign host, and
    // the registry `_authToken` must never leave the registry's own origin (N1b).
    let fetched =
        fetch_verify_and_hash_tarball(name, &dist, registry::auth_for_tarball(&cfg, &dist.tarball))
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
        // A bare `nub pm use nub` arrives here with the "latest" default spec —
        // it means the non-locking range, not an exact pin, so map it to `None`.
        // Only an explicit `nub@<version>` (a concrete pin) opts into the hard
        // `packageManager` field.
        let exact_pin = (spec != "latest").then_some(spec);
        return crate::pm_engine::use_nub::run_use_nub(&root, exact_pin);
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
        // nub → pnpm: nub.lock renamed back, byte-identical (the two-mode
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

/// `nub pm pin [<version>]` — write the locked identity pin
/// `packageManager: "nub@<exact>"` (defaulting to the running nub when no
/// version is given). This is the SOLE deliberate lock gesture and the one
/// thing that arms nub's self-shim: a locked exact `nub@<v>` ≠ the running nub
/// makes the next PM command provision that version and delegate to it. It is
/// the PM-namespace analog of `nub node pin <version>`.
///
/// It is DELIBERATELY lightweight — it writes only the two identity fields
/// (via [`use_nub::write_nub_identity_fields`]) and does no lockfile conversion
/// or `pnpm-workspace.yaml`/settings migration; that heavier into-nub switch is
/// `nub pm use nub`'s job. It never touches the network: nub is the running
/// binary, so the pin is a pure local declaration.
fn run_pm_pin(arg: Option<&str>, cwd: &Path) -> Result<i32> {
    use crate::pm_engine::use_nub;
    use nub_core::pm::resolve;

    let running = env!("CARGO_PKG_VERSION");

    // Resolve the exact version to pin. Bare `nub pm pin` (and the forgiving
    // `nub pm pin nub`) lock the running binary. An explicit version must be an
    // EXACT semver — nub is the running binary, not a registry package, so a
    // range/dist-tag (`^1`, `1.x`, `next`, `latest`) has nothing to resolve
    // against and can't be pinned (the same rule `split_pm_arg` enforces for
    // `pm use nub@<spec>`). A leading `nub@` is stripped for forgiveness so
    // `pin nub@<v>` and `pin <v>` are the same gesture.
    let version = match arg {
        None => running.to_string(),
        Some(a) => {
            let spec = a.strip_prefix("nub@").unwrap_or(a);
            if spec == "nub" {
                running.to_string()
            } else if semver::Version::parse(spec).is_ok() {
                spec.to_string()
            } else {
                bail!(
                    "`nub pm pin {spec}` needs an exact version (e.g. {running}) — nub is the \
                     running binary, not a registry package, so a range/tag can't be pinned."
                );
            }
        }
    };

    // The pin is written into the workspace-root manifest (the same home `use`
    // targets). Surface a malformed manifest first — resolution otherwise reads
    // it as "no project" and this would misreport a typo as a missing manifest.
    check_manifest_json(cwd)?;
    let Some(project) = nub_core::workspace::detect::detect_project(cwd) else {
        bail!(
            "no package.json found from {} — the pin is written into the project manifest",
            cwd.display()
        );
    };
    let root = project.workspace_root.unwrap_or(project.root);

    resolve::edit_root_manifest(&root, |obj| {
        use_nub::write_nub_identity_fields(obj, Some(&version), &version);
    })?;

    println!("pinned nub@{version}");
    println!("  package.json: packageManager = nub@{version}");
    println!(
        "  package.json: devEngines.packageManager = {{ name: \"nub\", version: \"^{version}\", onFail: \"ignore\" }}"
    );
    // A pin at a version other than the running nub is honored by the self-shim,
    // not eagerly: say what happens next, download nothing now.
    if version != running {
        println!(
            "  note: pinned nub@{version} (the running nub is {running}) — the next package-manager \
             command provisions and delegates to it."
        );
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
            let raw = std::fs::read_to_string(ws.join("package.json")).ok()?;
            serde_json::from_str(nub_core::strip_utf8_bom(&raw)).ok()?
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
// Spec: `package-manager-shims` (no such document) (mechanism + strict-by-default
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

    // The shim dir moved out of `~/.nub`. Clear a pre-move install FIRST, along
    // with its PATH block, or the profile ends up carrying two blocks and the
    // stale dir — still holding the pre-upgrade binary — keeps shadowing
    // npm/pnpm/yarn from earlier in PATH.
    let migrated = dirs_next::home_dir()
        .map(|home| shim::migrate_legacy_shim_dir(&home, shim::SHIMS_LEAF_PUBLIC))
        .transpose()?
        .flatten();
    if migrated.is_some() {
        shim::remove_path_block()?;
    }

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
    // Name the move: the user's shims were somewhere else a moment ago, and a
    // silent relocation of something that shadows `npm` deserves a line.
    if let Some(old) = &migrated {
        println!("moved the shims out of {}", old.display());
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
        // Reached on upgrade. The directory is a compile-time constant within
        // one build, but the CONSTANT ITSELF moved under XDG in #752, so a
        // profile written by an older nub still names `$HOME/.nub/shims` beneath
        // this marker and the line is rewritten in place. Only when that legacy
        // directory is already GONE — the migration above strips the block
        // outright while it still exists — so the live shell carries a
        // directory on PATH that holds no shims, which is why the re-source
        // hint matters more here than on a fresh add.
        ProfileOutcome::Rewritten(profile) => println!(
            "  PATH: updated the entry in {} to point at {}\n  \
             restart your shell, or run: source {}",
            profile.display(),
            dir.display(),
            profile.display()
        ),
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

    // Every candidate dir is swept, not just the one that resolves now — an XDG
    // install unshimmed from a shell without XDG_DATA_HOME would otherwise leave
    // its shim binaries behind while the PATH line was stripped.
    let removed = shim::remove_shims()?;
    let changed = shim::remove_path_block()?;
    if removed.is_empty() {
        println!("{} was already gone", shim::shim_dir()?.display());
    } else {
        for dir in &removed {
            println!("removed {}", dir.display());
        }
    }
    for profile in &changed {
        println!("  PATH: removed the shims block from {}", profile.display());
    }
    if changed.is_empty() {
        println!("  PATH: no profile carried the shims block");
    }
    // A stripped PATH block with nothing removed means the shims were installed
    // somewhere this process cannot name — a custom XDG_DATA_HOME that is unset
    // now. Say so rather than reporting a clean removal: the binaries are still
    // on disk, and re-running with the variable set is what clears them.
    if removed.is_empty() && !changed.is_empty() {
        eprintln!(
            "warning: a shims PATH block was removed but no shims directory was found. \
             If they were installed with XDG_DATA_HOME set, re-run with that variable set \
             to remove them."
        );
    }
    Ok(0)
}

/// `nub node shim`: hardlink the running nub as `node` in `~/.nub/node-shim`,
/// wire that dir onto PATH, and verify reachability — so a bare-shell `node`
/// resolves through nub. Idempotent; re-run to refresh after `nub upgrade`.
/// Mirrors [`run_pm_shim_install`] for the single `node` entry; the shim runs
/// the resolved Node VANILLA (version management only — augmentation is `nub`).
fn run_node_shim_install() -> Result<i32> {
    use nub_core::node::shim;
    use nub_core::pm::shim::{ProfileOutcome, ShimAction};

    let nub_binary = nub_core::node::spawn::current_nub_binary()?;
    let dir = shim::node_shim_dir()?;

    // Same migration as the PM shims: clear the pre-move `~/.nub/node-shim` and
    // its PATH block, or a stale `node` keeps shadowing from earlier in PATH.
    let migrated = dirs_next::home_dir()
        .map(|home| nub_core::pm::shim::migrate_legacy_shim_dir(&home, shim::NODE_SHIM_LEAF_PUBLIC))
        .transpose()?
        .flatten();
    if migrated.is_some() {
        shim::remove_node_path_block()?;
    }

    let entry = shim::install_node_shim(&nub_binary)?;
    if let Some(old) = &migrated {
        println!("moved the node shim out of {}", old.display());
    }

    let state = match entry.action {
        ShimAction::Created => "created",
        ShimAction::Relinked => "re-linked",
        ShimAction::Current => "already current",
    };
    println!("node shim in {} ({state})", dir.display());
    if entry.copied {
        println!(
            "  note: {} is on a different filesystem than the nub binary — \
             a copy was made instead of a hardlink",
            dir.display()
        );
    }
    // The shim runs the resolved Node VANILLA — state it so nobody expects
    // TypeScript / `.env` loading from a bare `node`; that's what `nub` is for.
    println!(
        "  `node` now resolves through nub (version management only — no augmentation; run `nub` for that)"
    );

    if shim::check_node_shim_noexec(&dir) {
        eprintln!(
            "warning: {} is on a filesystem mounted noexec — the shim is installed but every \
             invocation will fail with \"Permission denied\". Remount without noexec, or use a \
             HOME on an exec-allowed filesystem.",
            dir.display()
        );
    }

    // Windows profile editing is out of scope for v0 — print the dir to add.
    if cfg!(windows) {
        println!(
            "  PATH: add {} to your PATH (PATH editing isn't automated on Windows yet)",
            dir.display()
        );
        return Ok(0);
    }
    match shim::add_node_path_block()? {
        ProfileOutcome::Added(profile) => println!(
            "  added {} to PATH in {}\n  restart your shell, or run: source {}",
            dir.display(),
            profile.display(),
            profile.display()
        ),
        ProfileOutcome::AlreadyPresent(profile) => {
            println!("  PATH: already present in {}", profile.display())
        }
        // Reached on upgrade. The directory is a compile-time constant within
        // one build, but the CONSTANT ITSELF moved under XDG in #752, so a
        // profile written by an older nub still names `$HOME/.nub/node-shim` beneath
        // this marker and the line is rewritten in place. Only when that legacy
        // directory is already GONE — the migration above strips the block
        // outright while it still exists — so the live shell carries a
        // directory on PATH that holds no shims, which is why the re-source
        // hint matters more here than on a fresh add.
        ProfileOutcome::Rewritten(profile) => println!(
            "  PATH: updated the entry in {} to point at {}\n  \
             restart your shell, or run: source {}",
            profile.display(),
            dir.display(),
            profile.display()
        ),
        ProfileOutcome::Manual { line } => println!(
            "  PATH: no known shell profile to edit — add this line to your shell config:\n    {line}"
        ),
    }

    // Reachability check is meaningful only once the dir is on THIS process's
    // PATH (right after a fresh install the block isn't sourced yet — the source
    // hint above already covers that).
    if path_contains_dir(&dir) {
        let r = shim::check_node_shim_reachable(&dir);
        if !r.ok {
            match &r.first_hit {
                Some(hit) => eprintln!(
                    "warning: node resolves to {} which shadows the shim — move {} earlier \
                     in PATH, or remove that binary",
                    hit.display(),
                    dir.display()
                ),
                None => eprintln!(
                    "warning: node resolves to nothing on PATH even though {} is on it — \
                     is the shim dir readable?",
                    dir.display()
                ),
            }
        }
    }
    Ok(0)
}

/// `nub node unshim`: delete `~/.nub/node-shim` and strip its PATH block from
/// every profile. Touches only the dedicated dir + profiles, so it works from
/// any nub still on PATH. Mirrors [`run_pm_unshim`]. Idempotent.
fn run_node_unshim() -> Result<i32> {
    use nub_core::node::shim;

    // Name the dirs the sweep actually cleared, not the one resolving now — the
    // two differ whenever the shim was installed under a different root, and
    // printing the resolved path would name somewhere nothing happened.
    let (removed, changed) = shim::remove_node_shim()?;
    if removed.is_empty() {
        println!("{} was already gone", shim::node_shim_dir()?.display());
    } else {
        for dir in &removed {
            println!("removed {}", dir.display());
        }
    }
    for profile in &changed {
        println!(
            "  PATH: removed the node-shim block from {}",
            profile.display()
        );
    }
    if changed.is_empty() {
        println!("  PATH: no profile carried the node-shim block");
    }
    // Same honesty as `run_pm_unshim`: a stripped block with nothing removed
    // means the shim lives somewhere this process cannot name (a custom
    // XDG_DATA_HOME that is unset now), so the hardlink is still on disk.
    if removed.is_empty() && !changed.is_empty() {
        eprintln!(
            "warning: a node-shim PATH block was removed but no shim directory was found. \
             If it was installed with XDG_DATA_HOME set, re-run with that variable set \
             to remove it."
        );
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
    // The shim either hands argv to another package manager or makes a
    // pin/lockfile decision from that manager's native files. It never reads
    // nub's resolved project config, so do not let an unrelated malformed
    // ancestor `nub.jsonc` block a transparent `npm`/`pnpm`/`yarn` passthrough.
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

    // A GLOBAL op (`npm install -g`, `yarn global add`, `npm ls -g`) writes the
    // user's global prefix, never this project's lockfile/node_modules, so the
    // cross-PM project-pin refusal does not apply — `decide` lets it fall through.
    let global = shim::is_global_invocation(invoked, args);

    match shim::decide(
        invoked,
        &pin_state,
        args.first().map(String::as_str),
        nesting,
        global,
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
            // matching PM on PATH: zero network (a lockfile-family range still
            // re-resolves against the registry per invocation; "latest" is
            // TTL-cached but pays a resolve on expiry) and no run-to-run drift
            // as new versions publish. Provision the lockfile-implied family /
            // registry latest only on a true PATH miss.
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
            // True PATH miss: run a dynamic default of the INVOKED PM —
            // announced, never a baked version, and the shim never writes a pin.
            // "using", not "provisioning": with the dist-tag TTL cache a warm
            // call resolves and execs with zero network, and the install path
            // prints its own Installing…/Installed… lines when it does run.
            let root = shim_lockfile_root(cwd);
            let (spec, why) = dynamic_default_spec(invoked.pm(), &root)?;
            eprintln!(
                "nub: no {} on PATH — using {}@{spec} ({why}); one-time default, no pin written",
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
                    let raw = std::fs::read_to_string(ws.join("package.json")).ok()?;
                    serde_json::from_str(nub_core::strip_utf8_bom(&raw)).ok()?
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
    let mut notes = Vec::new();
    if nub_core::pm::shim::shim_dir().is_ok_and(|d| d.is_dir()) {
        notes.push("`nub pm shim`");
    }
    if nub_core::node::shim::node_shim_dir().is_ok_and(|d| d.is_dir()) {
        notes.push("`nub node shim`");
    }
    (!notes.is_empty()).then(|| {
        format!(
            "note: existing shims still run the previous nub until re-linked — run {}.",
            notes.join(" and ")
        )
    })
}

/// The self-owned channel owns the new binary's path, so re-link every installed
/// shim family in place right after the swap (best-effort: a failure downgrades
/// to the reminder). Covers both the PM shims and the persistent `node` shim.
fn relink_shims_after_selfowned(install_dir: &Path) {
    let new_bin = install_dir.join("bin").join(NUB_EXE);
    if let Ok(dir) = nub_core::pm::shim::shim_dir()
        && dir.is_dir()
    {
        match nub_core::pm::shim::install_shims(&new_bin) {
            Ok(_) => println!("re-linked the PM shims in {}", dir.display()),
            Err(e) => eprintln!("could not re-link the PM shims: {e:#} — run `nub pm shim`."),
        }
    }
    if let Ok(dir) = nub_core::node::shim::node_shim_dir()
        && dir.is_dir()
    {
        match nub_core::node::shim::install_node_shim(&new_bin) {
            Ok(_) => println!("re-linked the node shim in {}", dir.display()),
            Err(e) => eprintln!("could not re-link the node shim: {e:#} — run `nub node shim`."),
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
///
/// `node` is the resolution being explained, and an explicit-binary override
/// (`NODE_EXECUTABLE`, `nub.jsonc#nodeExecutable`) reports ITSELF: it never
/// consulted the chain, so crediting a pin source there would name a file that
/// had no say in the path printed on the line above.
fn resolution_source(cwd: &Path, node: &nub_core::node::discovery::ResolvedNode) -> String {
    if let Some(source) = node.pin_source.as_deref()
        && nub_core::node::discovery::is_explicit_binary_source(source)
    {
        return source.to_string();
    }
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

    // A streaming drain must retain NOTHING. The collected `Vec` lives as long as
    // the child, so for a script that never exits (`nub run -r dev`) anything kept
    // here grows the supervisor 1:1 with child output until it OOMs — while in
    // streaming mode the line has already gone to the terminal and nothing ever
    // reads the copy back. Only the aggregate flush genuinely replays these lines.
    #[test]
    fn drain_retains_lines_only_for_the_aggregate_flush() {
        let policy = |aggregate: bool| DrainPolicy {
            ndjson: false,
            aggregate,
            is_stderr: false,
            prefix: "pkg | ".to_string(),
            name: "pkg".to_string(),
            script: "dev".to_string(),
        };

        let streamed = policy(false).run(&b"one\ntwo\nthree\n"[..]);
        assert!(
            streamed.is_empty(),
            "streaming drain retained {} line(s); it must retain none",
            streamed.len(),
        );

        let aggregated = policy(true).run(&b"one\ntwo\n"[..]);
        assert_eq!(
            aggregated,
            vec!["pkg | one".to_string(), "pkg | two".to_string()],
            "aggregate drain must keep every line, prefixed, for the deferred flush",
        );
    }

    // Aggregate mode buffers so each package prints as one block, which assumes
    // the script exits. A non-TTY stdout selects aggregate too, so `nub run -r dev`
    // in CI took this path with a server that never exits and the buffer grew for
    // the life of the run. Past the ceiling it must flush early and keep going.
    #[test]
    fn aggregate_drain_flushes_early_instead_of_growing_without_bound() {
        let policy = DrainPolicy {
            ndjson: false,
            aggregate: true,
            is_stderr: false,
            prefix: String::new(),
            name: "pkg".to_string(),
            script: "dev".to_string(),
        };

        const CAP: usize = 2 * 1024;
        // ~16 KiB of input — eight times the ceiling, small enough that the early
        // flushes this deliberately triggers stay out of the suite's output.
        let mut input = Vec::new();
        for _ in 0..200 {
            input.extend_from_slice(&b"z".repeat(79));
            input.push(b'\n');
        }
        let total = input.len();

        let held = policy.run_capped(&input[..], CAP);
        let held_bytes: usize = held.iter().map(|l| l.len() + 1).sum();
        assert!(
            held_bytes < CAP,
            "drain held {held_bytes} B of {total} B after the early flush; \
             it must stay under the {CAP} B ceiling",
        );
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
        let merged = merge_child_env(auto(), true, &explicit, false);
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
        let merged = merge_child_env(auto(), false, &HashMap::new(), false);
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
        let merged = merge_child_env(auto(), true, &explicit_override, false);
        assert_eq!(
            merged.get("NUB_TEST_FOO").map(String::as_str),
            Some("from_explicit"),
            "explicit file's value wins; no auto `.env` leakage"
        );
        assert!(
            !merged.contains_key("NUB_TEST_BAR"),
            "non-overridden auto var stays suppressed; got {merged:?}"
        );

        // (d) `--no-env-file` WINS over everything: no autos, and the explicit
        // `--env-file` map is ignored too — the child gets ZERO env-file vars,
        // even when `--env-file` was also present (decided 2026-07-07).
        let merged = merge_child_env(auto(), true, &explicit, true);
        assert!(
            merged.is_empty(),
            "--no-env-file must suppress both auto-discovery and explicit --env-file; got {merged:?}"
        );
    }

    // `nub watch` regression guard (#207): the watch path injects via `Command::env`
    // ONLY the vars whose `${VAR}` expansion changed their raw value. Plain vars are
    // left to Node's `--env-file` (which re-reads on every restart), so editing
    // `.env` is picked up live instead of freezing at startup. `watch_inject_vars`
    // is the selection; the integration suite separately locks the real restart.
    #[test]
    fn watch_injects_only_expansion_changed_vars() {
        // A plain var (raw == expanded) and an expansion-changed var.
        let env_vars = HashMap::from([
            ("NUB_TEST_PLAIN".to_string(), "value_one".to_string()),
            (
                "NUB_TEST_URL".to_string(),
                "postgres://localhost:5432/db".to_string(),
            ),
        ]);
        let raw_env = HashMap::from([
            ("NUB_TEST_PLAIN".to_string(), "value_one".to_string()),
            (
                "NUB_TEST_URL".to_string(),
                "postgres://${NUB_TEST_HOST}:5432/db".to_string(),
            ),
        ]);

        let injected: HashMap<_, _> = watch_inject_vars(&env_vars, &raw_env)
            .into_iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Plain var: Node's `--env-file` delivers it identically → NOT injected, so
        // it live-reloads on restart (#207).
        assert!(
            !injected.contains_key("NUB_TEST_PLAIN"),
            "a var unchanged by expansion must be left to Node's --env-file (live reload); got {injected:?}"
        );
        // Expansion-changed var: Node's `--env-file` can't reproduce it → injected
        // with the expanded value (the documented frozen-until-restart trade-off).
        assert_eq!(
            injected.get("NUB_TEST_URL").copied(),
            Some("postgres://localhost:5432/db"),
            "an expansion-changed var must be injected with its expanded value; got {injected:?}"
        );

        // Below-floor fallback (explicit `--env-file` on a Node without the flag):
        // `raw_env` is empty — no `--env-file` args reach Node, so injection is
        // the only delivery channel and everything is injected (frozen).
        let all: HashMap<_, _> = watch_inject_vars(&env_vars, &HashMap::new())
            .into_iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            all.len(),
            2,
            "with no raw_env every CLI env-file var is injected; got {all:?}"
        );
    }

    #[test]
    fn watch_env_file_arg_is_relative_to_the_watcher_cwd() {
        let root = std::env::temp_dir().join("nub-watch-env-arg-root");
        assert_eq!(
            watch_env_file_arg(&root.join(".env"), &root, false, false).unwrap(),
            "--env-file=.env"
        );

        let member = root.join("packages").join("app");
        let one_level = Path::new("..").join(".env.local");
        assert_eq!(
            watch_env_file_arg(
                &root.join("packages").join(".env.local"),
                &member,
                false,
                false
            )
            .unwrap(),
            format!("--env-file={}", one_level.to_str().unwrap())
        );
        let two_levels = Path::new("..").join("..").join(".env");
        assert_eq!(
            watch_env_file_arg(&root.join(".env"), &member, false, false).unwrap(),
            format!("--env-file={}", two_levels.to_str().unwrap())
        );
    }

    #[test]
    fn watch_env_file_arg_spells_the_if_exists_flag() {
        let root = std::env::temp_dir().join("nub-watch-env-arg-if-exists");
        // Unix: same cwd-relative form, if-exists spelling (#479).
        assert_eq!(
            watch_env_file_arg(&root.join("custom.env"), &root, false, true).unwrap(),
            "--env-file-if-exists=custom.env"
        );
        // Windows: an ABSENT if-exists target must not fail long-path expansion —
        // Node itself silently no-ops on it, so the arg forwards verbatim.
        let arg = watch_env_file_arg(&root.join("missing.env"), &root, true, true).unwrap();
        assert!(arg.starts_with("--env-file-if-exists="), "{arg}");
        assert!(arg.ends_with("missing.env"), "{arg}");
    }

    /// The floor the auto-discovered cascade shares with the explicit flags.
    /// Empirically pinned against installed Nodes: 19.3.0 rejects `--env-file`,
    /// 20.10.0 accepts it — bracketing the documented 20.6.0 landing.
    #[test]
    fn auto_env_file_forwarding_gates_on_node_version() {
        use nub_core::node::version::NodeVersion;
        assert!(!node_accepts_env_file(&NodeVersion::new(18, 19, 0)));
        assert!(!node_accepts_env_file(&NodeVersion::new(19, 3, 0)));
        assert!(!node_accepts_env_file(&NodeVersion::new(20, 5, 0)));
        assert!(node_accepts_env_file(&NodeVersion::new(20, 6, 0)));
        assert!(node_accepts_env_file(&NodeVersion::new(26, 0, 0)));
    }

    #[test]
    fn explicit_env_file_forwarding_gates_on_node_version() {
        use nub_core::node::version::NodeVersion;
        let plain = vec![(PathBuf::from("/p/custom.env"), false)];
        let if_exists = vec![(PathBuf::from("/p/custom.env"), true)];
        let mixed = vec![
            (PathBuf::from("/p/a.env"), false),
            (PathBuf::from("/p/b.env"), true),
        ];

        // No explicit files → nothing to forward.
        assert!(!should_forward_explicit_env_files(
            &NodeVersion::new(26, 0, 0),
            &[]
        ));
        // `--env-file` needs Node 20.6.0.
        assert!(!should_forward_explicit_env_files(
            &NodeVersion::new(20, 5, 0),
            &plain
        ));
        assert!(should_forward_explicit_env_files(
            &NodeVersion::new(20, 6, 0),
            &plain
        ));
        // `--env-file-if-exists` needs Node 22.9.0 — and one if-exists flag
        // gates the whole set (all-or-nothing preserves precedence).
        assert!(!should_forward_explicit_env_files(
            &NodeVersion::new(22, 8, 0),
            &if_exists
        ));
        assert!(should_forward_explicit_env_files(
            &NodeVersion::new(22, 9, 0),
            &if_exists
        ));
        assert!(!should_forward_explicit_env_files(
            &NodeVersion::new(21, 0, 0),
            &mixed
        ));
        assert!(should_forward_explicit_env_files(
            &NodeVersion::new(26, 0, 0),
            &mixed
        ));
    }

    #[test]
    fn watch_env_file_arg_stays_absolute_for_windows_watchers() {
        let root = std::env::temp_dir().join("nub-watch-env-arg-windows");
        let path = root.join(".env");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&path, "A=1\n").unwrap();
        let arg = watch_env_file_arg(&path, &root, true, false).unwrap();
        let _ = std::fs::remove_dir_all(&root);

        assert!(arg.starts_with("--env-file="), "{arg}");
        assert!(arg.ends_with(".env"), "{arg}");
        assert!(!arg.starts_with(r"--env-file=\\?\"), "{arg}");
    }

    #[test]
    fn windows_verbatim_prefix_stripping_handles_drive_and_unc_paths() {
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\D:\project\.env"),
            r"D:\project\.env"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\UNC\server\share\.env"),
            r"\\server\share\.env"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(r"D:\project\.env"),
            r"D:\project\.env"
        );
    }

    // #704: the upgrade path derives its install dir from the canonicalized
    // binary, so on Windows every path it prints arrives carrying `\\?\`.
    // `display_path` is the single place that spelling is dropped, and only for
    // display — the PathBuf the swap operates on keeps the verbatim form.
    #[test]
    fn display_path_drops_the_windows_verbatim_prefix_a_user_never_typed() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\u\.nub")),
            r"C:\Users\u\.nub"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\.nub")),
            r"\\server\share\.nub"
        );
        // POSIX paths, and an already-ordinary Windows path, pass through whole.
        assert_eq!(display_path(Path::new("/home/u/.nub")), "/home/u/.nub");
    }

    #[cfg(windows)]
    #[test]
    fn windows_long_watch_path_keeps_existing_file_and_cwd_in_one_spelling() {
        let root = std::env::temp_dir().join("nub-watch-env-long-path");
        let path = root.join(".env");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&path, "A=1\n").unwrap();

        let expanded_root = windows_long_watch_path(&root).unwrap();
        let expanded = windows_long_watch_path(&path).unwrap();
        assert_eq!(expanded.parent(), Some(expanded_root.as_path()));

        let expanded_root = expanded_root.to_str().unwrap();
        let ordinary_root = strip_windows_verbatim_prefix(expanded_root);
        let expanded = expanded.to_str().unwrap();
        let ordinary = strip_windows_verbatim_prefix(expanded);
        let _ = std::fs::remove_dir_all(&root);

        assert!(Path::new(&ordinary_root).is_absolute(), "{ordinary_root}");
        assert!(!ordinary_root.starts_with(r"\\?\"), "{ordinary_root}");
        assert!(Path::new(&ordinary).is_absolute(), "{ordinary}");
        assert!(ordinary.ends_with(".env"), "{ordinary}");
        assert!(!ordinary.starts_with(r"\\?\"), "{ordinary}");
    }

    #[test]
    fn watch_env_file_arg_rejects_relative_inputs() {
        let absolute = std::env::temp_dir().join("nub-watch-env-arg-absolute");
        assert!(watch_env_file_arg(Path::new(".env"), &absolute, false, false).is_err());
        assert!(
            watch_env_file_arg(&absolute.join(".env"), Path::new("project"), true, false).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn watch_env_file_arg_handles_non_utf8_paths_without_lossy_argv() {
        use std::os::unix::ffi::OsStringExt as _;

        let base = std::env::temp_dir();
        let non_utf8 = std::ffi::OsString::from_vec(vec![b'n', 0xff]);
        let ancestor = base.join(&non_utf8);
        let cwd = ancestor.join("member");
        assert_eq!(
            watch_env_file_arg(&ancestor.join(".env"), &cwd, false, false).unwrap(),
            format!("--env-file={}", Path::new("..").join(".env").display())
        );

        let non_utf8_file = cwd.join(std::ffi::OsString::from_vec(vec![b'.', 0xff]));
        assert!(
            watch_env_file_arg(&non_utf8_file, &cwd, false, false)
                .unwrap_err()
                .to_string()
                .contains("not valid UTF-8")
        );
        assert!(
            watch_env_file_arg(&non_utf8_file, &cwd, true, false)
                .unwrap_err()
                .to_string()
                .contains("not valid UTF-8")
        );
    }

    #[test]
    fn watch_env_guard_placeholders_follow_os_key_semantics() {
        let guarded = watch_guarded_env_file_keys(false);
        let ambient = vec![
            "NODE_OPTIONS".to_string(),
            "node_extra_ca_certs".to_string(),
        ];

        let unix = watch_env_guard_placeholders(&guarded, &ambient, false);
        assert!(!unix.contains(&"NODE_OPTIONS"));
        assert!(unix.contains(&"NODE_EXTRA_CA_CERTS"));
        assert!(unix.contains(&"NODE_TLS_REJECT_UNAUTHORIZED"));
        assert!(unix.contains(&"NODE_REPL_EXTERNAL_MODULE"));

        let windows = watch_env_guard_placeholders(&guarded, &ambient, true);
        assert!(!windows.contains(&"NODE_OPTIONS"));
        assert!(!windows.contains(&"NODE_EXTRA_CA_CERTS"));
        assert!(windows.contains(&"NODE_TLS_REJECT_UNAUTHORIZED"));
        assert!(windows.contains(&"NODE_REPL_EXTERNAL_MODULE"));
    }

    /// `NODE_ENV` is guarded only for the auto `.env*` cascade, matching what the
    /// direct runner does for each family (#263 drops a file-set `NODE_ENV` from
    /// the cascade; the explicit `--env-file` map keeps it). An ambient value is
    /// the user's own and never gets a placeholder.
    #[test]
    fn node_env_is_guarded_only_for_the_auto_cascade() {
        assert!(watch_guarded_env_file_keys(true).contains(&"NODE_ENV"));
        assert!(!watch_guarded_env_file_keys(false).contains(&"NODE_ENV"));

        let guarded = watch_guarded_env_file_keys(true);
        assert!(watch_env_guard_placeholders(&guarded, &[], false).contains(&"NODE_ENV"));
        assert!(
            !watch_env_guard_placeholders(&guarded, &["NODE_ENV".to_string()], false)
                .contains(&"NODE_ENV"),
            "an ambient NODE_ENV must pass through untouched"
        );
    }

    #[test]
    fn watch_guard_keeps_launcher_stamped_keys_denied_without_blanketing_them() {
        let guarded = watch_guarded_env_file_keys(false);
        let launcher_owned = [
            nub_core::node::spawn::RUNTIME_CONFIG_ENV,
            "__NUB_COMPAT_NODE_OPTIONS",
            "__NUB_AUGMENTED_NODE_OPTIONS",
            "__NUB_AUGMENTED_NODE_OPTIONS_PRESENT",
        ]
        .map(str::to_string);

        for key in &launcher_owned {
            assert!(
                guarded.contains(&key.as_str()),
                "a raw env file must remain unable to supply {key}"
            );
        }

        let placeholders = watch_env_guard_placeholders(&guarded, &launcher_owned, false);
        assert!(placeholders.contains(&"NODE_OPTIONS"));
        for key in &launcher_owned {
            assert!(
                !placeholders.contains(&key.as_str()),
                "a placeholder must not overwrite the launcher-stamped {key}"
            );
        }
        assert!(
            placeholders.contains(&"__NUB_COMPAT_NODE_PATH"),
            "an unstamped internal key must remain guarded"
        );
        assert!(
            placeholders.contains(&"__NUB_AUGMENTED_NODE_PATH"),
            "the namespace must not be treated as launcher-owned wholesale"
        );
    }

    #[test]
    fn runtime_env_source_merge_uses_platform_key_semantics() {
        let mut windows = HashMap::new();
        merge_runtime_env_source_value(&mut windows, "FOO".to_string(), "first".to_string(), true);
        merge_runtime_env_source_value(&mut windows, "foo".to_string(), "second".to_string(), true);
        assert_eq!(windows.len(), 1, "Windows has one case-folded env key");
        assert_eq!(windows.get("foo").map(String::as_str), Some("second"));
        assert!(
            !windows.contains_key("FOO"),
            "the later source's spelling must be preserved"
        );

        let mut unix = HashMap::new();
        merge_runtime_env_source_value(&mut unix, "FOO".to_string(), "first".to_string(), false);
        merge_runtime_env_source_value(&mut unix, "foo".to_string(), "second".to_string(), false);
        assert_eq!(unix.get("FOO").map(String::as_str), Some("first"));
        assert_eq!(unix.get("foo").map(String::as_str), Some("second"));
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

        // Output-control flags with a space-separated value must NOT be mistaken
        // for a package positional (the root bug: `--loglevel silent` with a
        // space caused `silent` to be read as a package → misrouted to `add`).
        assert_eq!(
            install_to_add_args(&args(&["install", "--loglevel", "silent"])),
            None,
            "nub install --loglevel silent stays on the native install path"
        );
        assert_eq!(
            install_to_add_args(&args(&["install", "--reporter", "silent"])),
            None,
            "nub install --reporter silent stays on the native install path"
        );
        assert_eq!(
            install_to_add_args(&args(&["i", "--loglevel", "info"])),
            None,
            "nub i --loglevel info stays on the native install path (non-silent level)"
        );
        // Equals form was never affected (no space → no positional confusion).
        assert_eq!(
            install_to_add_args(&args(&["install", "--loglevel=silent"])),
            None,
            "nub install --loglevel=silent stays on the native install path (equals form)"
        );
        // `--minimum-release-age` has the same shape: its MINUTES value in the
        // space form would read as a package spec and misroute the install.
        assert_eq!(
            install_to_add_args(&args(&["install", "--minimum-release-age", "0"])),
            None,
            "nub install --minimum-release-age 0 stays on the native install path"
        );
        assert_eq!(
            install_to_add_args(&args(&["install", "--minimum-release-age", "0", "react"])),
            Some(args(&["add", "--minimum-release-age", "0", "react"])),
            "--minimum-release-age 0 with a package routes to add, minutes not mis-forwarded"
        );
        // Output-control flags combined with a real package still route to add,
        // with the flag consumed as a flag (value NOT forwarded as a package).
        assert_eq!(
            install_to_add_args(&args(&["install", "--loglevel", "silent", "react"])),
            Some(args(&["add", "--loglevel", "silent", "react"])),
            "--loglevel silent with a package routes to add, value is not mis-forwarded"
        );
        // Platform selection, same shape again. Only the SPACE form can
        // misroute — `--os=linux` carries its value inside the token — so a
        // test that checks just the `=` spelling proves nothing here.
        for flag in ["--os", "--cpu", "--libc"] {
            assert_eq!(
                install_to_add_args(&args(&["install", flag, "linux"])),
                None,
                "nub install {flag} linux stays on the native install path"
            );
        }
        assert_eq!(
            install_to_add_args(&args(&["install", "--os", "linux", "react"])),
            Some(args(&["add", "--os", "linux", "react"])),
            "--os linux with a package routes to add, the os value is not mis-forwarded"
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
    fn runners_keep_the_post_target_dashdash_verbatim() {
        // Option A (decided 2026-06-28): the target token terminates runner
        // parsing and everything after it forwards VERBATIM — the `--` is kept,
        // uniform across run/exec/watch and byte-identical to `node` / `pnpm 10`.
        // No `--` is ever consumed as a separator; only a LEADING `--` (before the
        // target — handled by the explicit-separator branch) ends runner options.
        for verb in ["run", "exec", "watch"] {
            let (_, suffix) = split_subcommand_argv(
                [verb, "target", "--", "a", "b", "c"]
                    .map(String::from)
                    .to_vec(),
            );
            assert_eq!(
                suffix,
                ["--", "a", "b", "c"],
                "{verb}: post-target `--` kept"
            );

            // A repeated `--` is equally literal — nothing is consumed.
            let (_, suffix) = split_subcommand_argv(
                [verb, "target", "--", "a", "--", "b"]
                    .map(String::from)
                    .to_vec(),
            );
            assert_eq!(
                suffix,
                ["--", "a", "--", "b"],
                "{verb}: repeated `--` literal"
            );

            // No separator: args forward verbatim, including a literal `--` mid-stream.
            let (_, suffix) =
                split_subcommand_argv([verb, "target", "a", "b"].map(String::from).to_vec());
            assert_eq!(suffix, ["a", "b"], "{verb}: no-separator passthrough");
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

    // The channel flags are mutually exclusive, and an explicit version pins a
    // stable release so it can't combine with either channel flag.
    #[test]
    fn subcommand_upgrade_channel_flags_conflict() {
        assert!(parse(&["nub", "upgrade", "--canary"]).is_ok());
        assert!(parse(&["nub", "upgrade", "--stable"]).is_ok());
        assert!(parse(&["nub", "upgrade", "--canary", "--stable"]).is_err());
        assert!(parse(&["nub", "upgrade", "--version", "1.2.3", "--canary"]).is_err());
        assert!(parse(&["nub", "upgrade", "--version", "1.2.3", "--stable"]).is_err());
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
        // winget's portable-package store, in the canonicalized (post-Links-
        // symlink) form the router actually sees.
        assert_eq!(
            detect_channel(Path::new(
                "C:\\Users\\u\\AppData\\Local\\Microsoft\\WinGet\\Packages\\Nubjs.Nub_Microsoft.Winget.Source_8wekyb3d8bbwe\\nub.exe"
            )),
            UpgradeChannel::Winget
        );
        match detect_channel(Path::new("/home/u/.nub/bin/nub")) {
            UpgradeChannel::SelfOwned { install_dir } => {
                assert_eq!(install_dir, Path::new("/home/u/.nub"));
            }
            other => panic!("expected SelfOwned, got {other:?}"),
        }
        // An arbitrary `<dir>/bin/nub` with neither the `.nub` name nor a receipt
        // stays Unknown — this is what keeps a distro `/usr/bin/nub` from being
        // mistaken for a self-managed install and overwritten by `nub upgrade`.
        assert_eq!(
            detect_channel(Path::new("/some/random/place/bin/nub")),
            UpgradeChannel::Unknown
        );
        assert_eq!(
            detect_channel(Path::new("/some/random/place/nub")),
            UpgradeChannel::Unknown
        );
    }

    // #664: `nub upgrade` on an up-to-date install must report that instead of
    // re-downloading the archive it is already running. The two non-short-circuit
    // cases are the point of the test — each is a request the version match does
    // not actually satisfy.
    #[test]
    fn stable_upgrade_skips_the_download_only_for_an_unpinned_matching_stable() {
        assert!(stable_upgrade_is_current("0.7.4", "0.7.4", "latest"));
        assert!(!stable_upgrade_is_current("0.7.5", "0.7.4", "latest"));

        // An explicit `--version` is a re-install request — matching the running
        // version is exactly when a user repairs a damaged install.
        assert!(!stable_upgrade_is_current("0.7.4", "0.7.4", "0.7.4"));

        // A canary carries the version it was cut from, so `--stable` off a
        // canary can resolve to a stable release it is NOT running. Downloading
        // is what performs the channel switch.
        assert!(!stable_upgrade_is_current(
            "0.7.4",
            "0.7.4-canary.20260809.1",
            "latest"
        ));
    }

    // Receipt-based self-owned detection: a relocated NUB_INSTALL_DIR (not named
    // `.nub`) is in-place-upgradeable IFF the installer's `.nub-receipt` marker is
    // present next to bin/. Without it, the same layout stays Unknown. Uses a real
    // temp dir because the receipt check hits the filesystem.
    #[test]
    fn detect_channel_honors_install_receipt_for_relocated_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let install_dir = tmp.path().join("custom-nub");
        let bin = install_dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let nub = bin.join("nub");

        // No receipt yet → not recognized as self-owned.
        assert_eq!(detect_channel(&nub), UpgradeChannel::Unknown);

        // Installer drops the receipt → recognized, with install_dir recovered.
        std::fs::write(install_dir.join(".nub-receipt"), "# marker\n").unwrap();
        match detect_channel(&nub) {
            UpgradeChannel::SelfOwned { install_dir: dir } => assert_eq!(dir, install_dir),
            other => panic!("expected SelfOwned once the receipt exists, got {other:?}"),
        }
    }

    // The critical repeat-upgrade invariant: a self-owned upgrade swaps only bin/,
    // so the `.nub-receipt` marker MUST survive it — otherwise the NEXT upgrade
    // would fail to re-detect a relocated install as self-owned. Exercises the real
    // `swap_dir` the upgrade uses (Unix-only, like the fn — the Windows per-file
    // dance never touches anything outside bin/, and its e2e twin in
    // tests/upgrade_windows.rs asserts against a receipt-marked install).
    #[cfg(not(windows))]
    #[test]
    fn install_receipt_survives_a_selfowned_bin_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let install_dir = tmp.path();
        std::fs::create_dir_all(install_dir.join("bin")).unwrap();
        std::fs::write(install_dir.join("bin").join("nub"), b"old").unwrap();
        let receipt = install_dir.join(".nub-receipt");
        std::fs::write(&receipt, "# marker\n").unwrap();

        // Stage the replacement bin/ in a sibling dir (same filesystem, as the real
        // upgrade does) and swap it in.
        let staged = install_dir.join("staged-bin");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("nub"), b"new").unwrap();
        swap_dir(install_dir, "bin", &staged).unwrap();

        assert_eq!(
            std::fs::read(install_dir.join("bin").join("nub")).unwrap(),
            b"new",
            "bin/ should hold the swapped-in binary"
        );
        assert!(
            receipt.is_file(),
            ".nub-receipt must survive the bin swap so repeat upgrades keep detecting self-owned"
        );
    }

    // #375 regression guard: the Homebrew upgrade refreshes the tap FIRST.
    // `brew upgrade` alone reads the un-refreshed local tap, so a stale clone
    // reports "already installed" and never sees a published release. The
    // display string and the two arg vecs must agree that `brew update`
    // precedes `brew upgrade nub`.
    #[test]
    fn homebrew_upgrade_refreshes_tap_before_upgrading() {
        assert_eq!(HOMEBREW_UPDATE_ARGS, &["update"]);
        assert_eq!(HOMEBREW_UPGRADE_ARGS, &["upgrade", "nub"]);
        let update_at = HOMEBREW_UPGRADE_DISPLAY.find("brew update").unwrap();
        let upgrade_at = HOMEBREW_UPGRADE_DISPLAY.find("brew upgrade").unwrap();
        assert!(
            update_at < upgrade_at,
            "brew update must precede brew upgrade in {HOMEBREW_UPGRADE_DISPLAY:?}"
        );
    }

    /// Serializes the tests that read or mutate the release-URL env seams
    /// (`NUB_RELEASE_BASE_URL` / `NUB_RELEASE_LATEST_URL` /
    /// `NUB_RELEASE_CANARY_URL`). Those vars are
    /// process-global, so a test that sets them mustn't run concurrently with one
    /// that asserts the unset-default URL shape.
    static RELEASE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // Verification correctness: the checksum sidecar URL is the archive URL plus
    // `.sha256`, the win32 targets resolve to the `.zip` artifacts release.yml
    // actually publishes (everything else `.tar.gz`), and the digest helper
    // matches the well-known empty-input vector. Asserts the DEFAULT (env-seam
    // unset) URLs, so it holds the release-env lock to never observe the seam
    // mid-override from the e2e test below.
    #[test]
    fn archive_and_checksum_urls_pair_up_per_platform() {
        let _g = RELEASE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let url = archive_url("0.0.6", "darwin-arm64");
        assert_eq!(
            url,
            "https://github.com/nubjs/nub/releases/download/v0.0.6/nub-darwin-arm64.tar.gz"
        );
        assert_eq!(
            checksum_url("0.0.6", "darwin-arm64"),
            format!("{url}.sha256")
        );
        assert_eq!(
            archive_url("0.0.6", "win32-x64"),
            "https://github.com/nubjs/nub/releases/download/v0.0.6/nub-win32-x64.zip"
        );
        assert_eq!(
            checksum_url("0.0.6", "win32-arm64"),
            "https://github.com/nubjs/nub/releases/download/v0.0.6/nub-win32-arm64.zip.sha256"
        );
        // SHA-256 of the empty string — pins the digest formatting (lowercase hex).
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // The rolling canary tag's URLs: un-versioned `download/canary/` path, the
    // same `.sha256` pairing + per-platform extension rules as stable, and the
    // release-by-tag resolve endpoint. Default (env-seam unset) shapes, so it
    // holds the release-env lock like the stable URL test above.
    #[test]
    fn canary_urls_use_the_rolling_tag_path() {
        let _g = RELEASE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            archive_url_for_tag(CANARY_TAG, "linux-x64"),
            "https://github.com/nubjs/nub/releases/download/canary/nub-linux-x64.tar.gz"
        );
        assert_eq!(
            checksum_url_for_tag(CANARY_TAG, "win32-x64"),
            "https://github.com/nubjs/nub/releases/download/canary/nub-win32-x64.zip.sha256"
        );
        assert_eq!(
            release_canary_api(),
            "https://api.github.com/repos/nubjs/nub/releases/tags/canary"
        );
    }

    // The bun-mirrored channel decision table: `--canary` opts in, a canary
    // build's bare `nub upgrade` stays on canary, and `--stable` or an explicit
    // `--version` opts back to the stable channel.
    #[test]
    fn release_channel_decision_mirrors_bun() {
        use ReleaseChannel::*;
        // (flag_canary, flag_stable, explicit_version, running_is_canary) → chosen
        let cases = [
            ((false, false, false, false), Stable),
            ((true, false, false, false), Canary),
            ((false, true, false, false), Stable),
            ((false, false, true, false), Stable),
            ((false, false, false, true), Canary),
            ((false, true, false, true), Stable),
            ((false, false, true, true), Stable),
            ((true, false, false, true), Canary),
        ];
        for ((c, s, v, r), want) in cases {
            assert_eq!(
                choose_release_channel(c, s, v, r),
                want,
                "choose_release_channel({c}, {s}, {v}, {r})"
            );
        }
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
    // Unix-only: the self-owned swap path here (symlink, mode bits, dir rename)
    // is POSIX; the Windows per-file rename dance is exercised end-to-end —
    // against the REAL running binary — by `tests/upgrade_windows.rs` on the
    // windows-latest CI leg.
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

        // 1. Build the artifact the fake channel serves: a tar.gz of the
        //    single-binary layout (bin/ ONLY — the runtime is embedded in the
        //    binary, not a sidecar), with sentinel bytes so we can prove the
        //    SWAPPED-IN binary is the downloaded one, not the old one.
        const NEW_NUB_BYTES: &[u8] = b"#!/bin/sh\necho fake-upgraded-nub 9.9.9\n";
        let build = fixture.path().join("build");
        std::fs::create_dir_all(build.join("bin")).unwrap();
        std::fs::write(build.join("bin").join("nub"), NEW_NUB_BYTES).unwrap();

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
            .args(["bin"])
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
        //    has something to replace and we can prove the bytes changed. The stale
        //    runtime/ sidecar (a pre-single-binary install) must be cleaned up by
        //    the upgrade — asserted below.
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
        let result = perform_selfowned_upgrade(&install, ReleaseChannel::Stable, "latest");

        // Wrong-checksum sub-case: corrupt the archive after digesting it and prove
        // the verify step REFUSES it (run before asserting the happy path so a
        // mistakenly-passing verify can't be masked by the good run). Use a second
        // install so the good run's result is independent.
        let bad_install = fixture.path().join(".nub-bad");
        std::fs::create_dir_all(bad_install.join("bin")).unwrap();
        std::fs::write(bad_install.join("bin").join("nub"), b"OLD\n").unwrap();
        std::fs::write(
            asset_dir.join(format!("nub-{target}.tar.gz.sha256")),
            format!("{}  nub-{target}.tar.gz\n", "0".repeat(64)),
        )
        .unwrap();
        let bad = perform_selfowned_upgrade(&bad_install, ReleaseChannel::Stable, "latest");

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
        assert!(
            !install.join("runtime").exists(),
            "single-binary upgrade must remove a stale pre-single-binary runtime/ sidecar"
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

    // ARTIFACT-SHAPE RESILIENCE: the new upgrader must succeed against the
    // back-compat archive shape that ships a (vestigial, empty) runtime/ ALONGSIDE
    // bin/ — the shape release.yml now publishes to rescue sidecar-era v0.1.x
    // upgraders. The new upgrader ignores the archive's runtime/, swaps bin/, and
    // removes any ~/.nub/runtime; this proves re-adding runtime/ to the artifact
    // causes NO regression for the current bin-only upgrader (the no-regression leg
    // of the upgrade-compat fix). Pairs with `…_against_a_local_fake_release`, which
    // covers the bin-ONLY shape; together they pin tolerance of BOTH shapes.
    #[cfg(unix)]
    #[test]
    fn new_upgrader_tolerates_a_compat_runtime_dir_in_the_archive() {
        let Some(target) = platform_target() else {
            eprintln!("skipping: no published tarball target for this platform");
            return;
        };
        let _g = RELEASE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        const FAKE_VERSION: &str = "9.9.9";
        const NEW_NUB_BYTES: &[u8] = b"#!/bin/sh\necho fake-upgraded-nub\n";
        let fixture = tempfile::tempdir().expect("fixture root");

        // Build a bin/ + EMPTY runtime/ archive — exactly what release.yml's
        // `tar -czf … bin runtime` produces with the transitional empty runtime/.
        let build = fixture.path().join("build");
        std::fs::create_dir_all(build.join("bin")).unwrap();
        std::fs::create_dir_all(build.join("runtime")).unwrap();
        std::fs::write(build.join("bin").join("nub"), NEW_NUB_BYTES).unwrap();

        let asset_dir = fixture
            .path()
            .join("releases")
            .join("download")
            .join(format!("v{FAKE_VERSION}"));
        std::fs::create_dir_all(&asset_dir).unwrap();
        let archive = asset_dir.join(format!("nub-{target}.tar.gz"));
        assert!(
            std::process::Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&build)
                .args(["bin", "runtime"])
                .status()
                .expect("tar the fixture archive")
                .success(),
            "fixture archive must tar cleanly"
        );
        let digest = sha256_hex(&std::fs::read(&archive).unwrap());
        std::fs::write(
            asset_dir.join(format!("nub-{target}.tar.gz.sha256")),
            format!("{digest}  nub-{target}.tar.gz\n"),
        )
        .unwrap();

        // A self-owned install whose prior runtime/ is the dead-weight sidecar.
        let install = fixture.path().join(".nub");
        std::fs::create_dir_all(install.join("bin")).unwrap();
        std::fs::write(install.join("bin").join("nub"), b"#!/bin/sh\necho OLD\n").unwrap();
        std::fs::create_dir_all(install.join("runtime")).unwrap();

        let base_url = format!("file://{}/releases/download", fixture.path().display());
        unsafe {
            std::env::set_var(RELEASE_DOWNLOAD_BASE_ENV, &base_url);
        }
        let result = perform_selfowned_upgrade(&install, ReleaseChannel::Stable, FAKE_VERSION);
        unsafe {
            std::env::remove_var(RELEASE_DOWNLOAD_BASE_ENV);
        }

        result.expect("upgrade against a bin+runtime archive must succeed");
        assert_eq!(
            std::fs::read(install.join("bin").join("nub")).unwrap(),
            NEW_NUB_BYTES,
            "the new binary must be swapped in regardless of the archive's runtime/"
        );
        assert!(
            !install.join("runtime").exists(),
            "the new upgrader removes ~/.nub/runtime; the archive's runtime/ is ignored"
        );
    }

    // CANARY CHANNEL e2e: drives `ReleaseChannel::Canary` through the same
    // file:// seams. (a) With the release-by-tag API advertising a version other
    // than this build's, the full download→verify→swap runs from the
    // UN-VERSIONED rolling-tag asset path (`releases/download/canary/…` — no
    // `v` prefix, the exact layout the canary pipeline publishes). (b) With the API
    // advertising THIS build's own version, the upgrade short-circuits as
    // already-up-to-date and leaves the install untouched.
    #[cfg(unix)]
    #[test]
    fn canary_upgrade_pulls_the_rolling_tag_and_short_circuits_when_current() {
        let Some(target) = platform_target() else {
            eprintln!("skipping: no published tarball target for this platform");
            return;
        };
        let _g = RELEASE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        const NEW_NUB_BYTES: &[u8] = b"#!/bin/sh\necho fake-canary-nub\n";
        let fixture = tempfile::tempdir().expect("fixture root");

        let build = fixture.path().join("build");
        std::fs::create_dir_all(build.join("bin")).unwrap();
        std::fs::write(build.join("bin").join("nub"), NEW_NUB_BYTES).unwrap();

        let asset_dir = fixture
            .path()
            .join("releases")
            .join("download")
            .join(CANARY_TAG);
        std::fs::create_dir_all(&asset_dir).unwrap();
        let archive = asset_dir.join(format!("nub-{target}.tar.gz"));
        assert!(
            std::process::Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&build)
                .args(["bin"])
                .status()
                .expect("tar the fixture archive")
                .success(),
            "fixture archive must tar cleanly"
        );
        let digest = sha256_hex(&std::fs::read(&archive).unwrap());
        std::fs::write(
            asset_dir.join(format!("nub-{target}.tar.gz.sha256")),
            format!("{digest}  nub-{target}.tar.gz\n"),
        )
        .unwrap();

        // The release-by-tag response: `name` is release.yml's `Canary <ver>`
        // title form; the advertised version here ≠ this build's version.
        let canary_json = fixture.path().join("canary.json");
        std::fs::write(
            &canary_json,
            r#"{"tag_name":"canary","name":"Canary 9.9.10-canary.20990101.7"}"#,
        )
        .unwrap();

        let install = fixture.path().join(".nub");
        std::fs::create_dir_all(install.join("bin")).unwrap();
        std::fs::write(install.join("bin").join("nub"), b"OLD\n").unwrap();

        let base_url = format!("file://{}/releases/download", fixture.path().display());
        unsafe {
            std::env::set_var(RELEASE_DOWNLOAD_BASE_ENV, &base_url);
            std::env::set_var(
                RELEASE_CANARY_API_ENV,
                format!("file://{}", canary_json.display()),
            );
        }

        // (a) advertised ≠ running → the rolling-tag download+swap runs.
        let upgraded = perform_selfowned_upgrade(&install, ReleaseChannel::Canary, "latest");

        // (b) advertised == running → short-circuit, nothing touched.
        let current_install = fixture.path().join(".nub-current");
        std::fs::create_dir_all(current_install.join("bin")).unwrap();
        std::fs::write(current_install.join("bin").join("nub"), b"OLD\n").unwrap();
        std::fs::write(
            &canary_json,
            format!(
                r#"{{"tag_name":"canary","name":"Canary {}"}}"#,
                env!("CARGO_PKG_VERSION")
            ),
        )
        .unwrap();
        let current = perform_selfowned_upgrade(&current_install, ReleaseChannel::Canary, "latest");

        unsafe {
            std::env::remove_var(RELEASE_DOWNLOAD_BASE_ENV);
            std::env::remove_var(RELEASE_CANARY_API_ENV);
        }

        upgraded.expect("canary upgrade against the local fake rolling release must succeed");
        assert_eq!(
            std::fs::read(install.join("bin").join("nub")).unwrap(),
            NEW_NUB_BYTES,
            "canary upgrade must swap in the rolling-tag archive's bytes"
        );

        current.expect("an already-current canary upgrade must succeed as a no-op");
        assert_eq!(
            std::fs::read(current_install.join("bin").join("nub")).unwrap(),
            b"OLD\n",
            "an already-current canary must short-circuit without touching the install"
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
    fn group_help_is_recognized_after_the_verb() {
        // #653: `nub pm shim --help` installed the shims and edited the user's
        // shell profiles, because the group's help guard only ever looked at
        // argv[0]. The flag has to count at ANY position — the top-level scan
        // hands it through untouched once `pm`/`node` is seen.
        //
        // Asserted on the predicate rather than by calling `run_pm`, on purpose:
        // a test that drove the real verb would install shims into the test
        // runner's own HOME the moment this regressed, and the suite has no
        // HOME-isolation helper.
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        for args in [
            &["shim", "--help"][..],
            &["shim", "-h"][..],
            &["unshim", "--help"][..],
            &["unshim", "-h"][..],
            &["install", "22.13.0", "--help"][..],
            // `nub agent` is the third group on this guard.
            &["docs", "--help"][..],
            &["skill", "-h"][..],
        ] {
            assert!(
                group_help_requested(&argv(args)),
                "`{}` is a help request, not a command to run",
                args.join(" ")
            );
        }
        // The pre-existing argv[0] forms keep working.
        for args in [&["--help"][..], &["-h"][..], &["help"][..]] {
            assert!(group_help_requested(&argv(args)));
        }
        // A real verb with a real argument is untouched — no help flag, no match.
        for args in [
            &["shim"][..],
            &["use", "pnpm"][..],
            &["cache", "clear"][..],
            &["install", "22.13.0"][..],
            &["docs", "--page", "/docs/runtime/jsx"][..],
        ] {
            assert!(
                !group_help_requested(&argv(args)),
                "`{}` must still run",
                args.join(" ")
            );
        }
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

    #[test]
    fn node_hijack_strips_only_a_leading_node_flag() {
        let s = |args: &[&str]| {
            let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let (compat, fwd) = scan_node_compat_flag(&owned);
            (compat, fwd.join(" "))
        };

        // No `--node`: argv is forwarded byte-for-byte, compat off.
        assert_eq!(s(&["app.js", "a", "b"]), (false, "app.js a b".into()));
        // A LEADING standalone `--node` is nub's opt-out: consumed + compat on.
        assert_eq!(s(&["--node", "app.js"]), (true, "app.js".into()));
        // The reported bug: a `--node` AFTER the entry point is the script's arg —
        // forwarded verbatim, compat stays OFF (was eaten + flipped compat before).
        assert_eq!(s(&["app.js", "--node"]), (false, "app.js --node".into()));
        assert_eq!(
            s(&["app.js", "a", "--node", "b"]),
            (false, "app.js a --node b".into())
        );
        // Leading Node options precede the entry; a leading `--node` among them is
        // still nub's, a post-entry one is the script's.
        assert_eq!(
            s(&["--inspect", "--node", "app.js", "--node"]),
            (true, "--inspect app.js --node".into())
        );
        // `--` ends nub's option region: a `--node` after it is a program arg.
        assert_eq!(
            s(&["--node", "--", "app.js", "--node"]),
            (true, "-- app.js --node".into())
        );
        // Eval entry: a `--node` after `-e`/`-p` is the script's (and `node -e
        // --node` keeps `--node` as the eval code).
        assert_eq!(s(&["-e", "--node"]), (false, "-e --node".into()));
        assert_eq!(
            s(&["--node", "-p", "code", "--node"]),
            (true, "-p code --node".into())
        );
        // stdin entry (`-`): trailing `--node` is the script's.
        assert_eq!(s(&["-", "--node"]), (false, "- --node".into()));
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
            "run", "exec", "node", "pm", "global", "watch", "upgrade", "help", "install", "i",
            "ci", "init",
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

    // `init`: the engine-registry exclusion is asserted in
    // pm_engine::tests::verb_registry_excludes_reserved_and_tool_identity_verbs;
    // the command itself (src/init.rs, a clap subcommand since it shipped) is
    // covered through the spawned binary in tests/init_cmd.rs and
    // tests/pm_verbs.rs.

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
        // `use nub` is a live target (the full switch). Bare + `nub@<exact>` are
        // both valid; only a range/dist-tag spec is refused — nub is the running
        // binary, so there is nothing to resolve a range against. Both a dist-tag
        // (`next`) and a digit-LEADING range (`1.x`) must be caught: the guard
        // requires an exact semver parse, not merely a leading digit.
        for bad in ["nub@next", "nub@1.x", "nub@1", "nub@^1.2.3"] {
            let err = run(&["use", bad]);
            assert!(
                err.contains("needs an exact version") && err.contains("running binary"),
                "`use {bad}` must refuse with the exact-version rule: {err}"
            );
        }
        assert!(
            run(&["use", "pnpm@"]).contains("empty version spec"),
            "a trailing @ is named, not treated as latest"
        );
        // `switch` (removed) names its successor — a clean break, not an alias.
        // (`pin` is a LIVE verb again — the nub-lock gesture — so it is NOT here.)
        let err = run(&["switch", "pnpm@9.1.0"]);
        assert!(
            err.contains("replaced by `nub pm use"),
            "`switch` must name the successor verb, got: {err}"
        );
        let err = run(&["frobnicate"]);
        assert!(
            err.contains("which, use, pin, update (up), cache"),
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
    fn update_on_a_bun_pin_names_bun_not_the_declare_a_pm_dead_end() {
        // A bun pin resolves as unprovisionable, so the generic "no pin —
        // declare one" remedy would loop the user (bun IS declared; nub can't
        // provision it). The update verb must name bun and its own tooling
        // instead — no `nub pm use` dead-end.
        let dir = offline_project("up-bun", r#"{"packageManager":"bun@1.1.0"}"#);
        let err = with_cwd(&dir, || run_pm(&["update".into()]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("pinned to bun") && err.contains("bun upgrade"),
            "update on a bun pin must name bun and its own update path, got: {err}"
        );
        assert!(
            !err.contains("nub pm use"),
            "update on a bun pin must NOT emit the declare-a-pm dead-end, got: {err}"
        );
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

        // A UTF-8-BOM-prefixed manifest (PowerShell 5.1 / Windows editors write
        // one by default) is valid JSON to npm/pnpm — the pre-flight check must
        // strip the BOM before serde_json rather than reject it as "not valid
        // JSON at line 1 column 1".
        let bom = pm_tmpdir("manifest-bom");
        std::fs::write(
            bom.join("package.json"),
            format!("\u{feff}{}", r#"{"name":"app"}"#),
        )
        .unwrap();
        assert!(
            check_manifest_json(&bom).is_ok(),
            "a UTF-8 BOM-prefixed package.json must pass the pre-flight check"
        );

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
        // A stand-in resolution: `resolution_source` reads only `pin_source`, and
        // `None` is the ordinary case that consults the chain.
        let chain_resolved = nub_core::node::discovery::ResolvedNode::fallback();
        let source = resolution_source(&dir, &chain_resolved);
        assert!(
            source.contains("devEngines.runtime") && source.contains(">=22"),
            "the governing source must be reported with its raw spec, got: {source}"
        );

        // No source at all → PATH, named as such.
        let bare = pm_tmpdir("res-src-bare");
        std::fs::write(bare.join("package.json"), r#"{"name":"app"}"#).unwrap();
        assert_eq!(resolution_source(&bare, &chain_resolved), "node on PATH");

        // An explicit-binary override reports itself: the pin chain named above
        // had no say in which binary ran, so crediting it would be a lie the user
        // acts on.
        let mut overridden = nub_core::node::discovery::ResolvedNode::fallback();
        overridden.pin_source = Some("nub.jsonc#nodeExecutable".to_string());
        assert_eq!(
            resolution_source(&dir, &overridden),
            "nub.jsonc#nodeExecutable"
        );
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
    fn busybox_resolves_beside_the_binary_before_the_relocated_shell_dir() {
        // #687: 0.7.0 began hardlinking nub.exe into npm's global bin dir, away from
        // the `busybox.exe` this resolution had always assumed was its sibling, and
        // every `nub run` on a Windows npm install broke. The launcher now carries the
        // shell into a `nub-sh/` subdir beside the relocated binary, so BOTH layouts
        // must resolve — and the sibling must win, because that is the untouched
        // packaged layout (win32 package, install.ps1's .zip, `nub upgrade`'s
        // ~/.nub/bin) and a stale staged copy must never shadow it.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let beside = root.join("busybox.exe");
        let staged = root.join(NUB_SHELL_SUBDIR).join("busybox.exe");

        assert_eq!(
            busybox_candidates(root),
            [beside.clone(), staged.clone()],
            "the sibling sidecar must be probed before the relocated nub-sh/ copy"
        );

        // Neither present: the caller bails. Asserting on the candidate list rather
        // than resolve_bundled_busybox() because that reads the TEST binary's
        // current_exe(), which no fixture can relocate.
        assert!(
            !busybox_candidates(root).iter().any(|p| p.is_file()),
            "an empty dir must offer no resolvable candidate"
        );

        // Only the relocated copy: found. This is the case that was broken.
        std::fs::create_dir_all(staged.parent().unwrap()).expect("mkdir nub-sh");
        std::fs::write(&staged, b"stub").expect("write staged busybox");
        assert_eq!(
            busybox_candidates(root).into_iter().find(|p| p.is_file()),
            Some(staged.clone()),
            "a binary relocated away from its sidecar must resolve nub-sh/busybox.exe"
        );

        // Both present: the sibling wins.
        std::fs::write(&beside, b"stub").expect("write sibling busybox");
        assert_eq!(
            busybox_candidates(root).into_iter().find(|p| p.is_file()),
            Some(beside),
            "the packaged sibling layout must take precedence over the staged copy"
        );
    }

    /// The prologue restores the lowercase names busybox-w32 up-cases, and the
    /// filters are the whole contract: re-exporting the wrong thing is worse than
    /// re-exporting nothing, because it puts a name back into the environment that
    /// the caller had removed, or writes a line the shell refuses to parse.
    #[test]
    fn the_casing_prologue_rebinds_only_the_names_a_shell_can_export() {
        let mut command = std::process::Command::new("sh");
        command.env("npm_package_name", "acme");
        command.env("npm_config_user_agent", "nub/0.9");
        // Already uppercase: busybox leaves it alone, so re-binding it is noise.
        command.env("NODE_OPTIONS", "--x");
        // Not a shell identifier, and not exportable under any casing.
        command.env("weird-name", "v");
        command.env("2fast", "v");
        // A REMOVAL. Re-exporting it would resurrect the name.
        command.env_remove("npm_lifecycle_event");

        let prologue = lowercase_env_prologue(&command);

        assert_eq!(
            prologue,
            "export npm_config_user_agent=\"$NPM_CONFIG_USER_AGENT\"; \
             export npm_package_name=\"$NPM_PACKAGE_NAME\"; ",
            "only lowercase, exportable, still-set names belong in the prologue"
        );

        // A body prefixed with it is still one shell word away from the original.
        let body = format!("{prologue}echo $npm_package_name");
        assert!(
            body.ends_with("; echo $npm_package_name"),
            "the prologue must end in a separator so the body is a fresh command: {body}"
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

    /// `value_consuming_flags("nubx")` and `install_to_add_args`'s `VALUE_FLAGS`
    /// are two hand-maintained lists carrying the same contract, so a flag added
    /// to one is easy to forget in the other. The miss is silent in the worst
    /// way here: `nubx --os linux cowsay` alone comes out right, and only a
    /// SECOND nubx-owned flag after the value exposes the bad split.
    #[test]
    fn nubx_platform_flag_values_do_not_steal_the_positional() {
        for flag in ["--os", "--cpu", "--libc"] {
            let Command::Nubx {
                package, bin, args, ..
            } = parse_nubx(&[flag, "linux", "-p", "left-pad", "pad", "--wrap"])
            else {
                unreachable!()
            };
            assert_eq!(
                package,
                vec!["left-pad".to_string()],
                "{flag}'s value must not be read as the bin, which leaves -p to forward"
            );
            assert_eq!(bin, "pad", "the positional is still the bin: {flag}");
            assert_eq!(
                args,
                vec!["--wrap".to_string()],
                "only post-bin args forward: {flag}"
            );
        }
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
        // `-y`/`--yes` (the consent escape hatch) and `--ignore-existing`
        // (warn+ignore) must not be mistaken for the bin positional.
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

    /// A CI job exporting the pre-rename name keeps the policy it asked for, and
    /// is told once that the name moved — while the current name, once present,
    /// decides alone. Asserted on the pure seam so no process env is mutated;
    /// the notice itself is a process-global latch and not observable here.
    #[test]
    fn the_pre_rename_verify_deps_variable_is_honored_only_while_the_current_one_is_absent() {
        use crate::project_config::VerifyDeps;
        let choose = |current: Option<&str>, legacy: Option<&str>| {
            verify_deps_env_choice(
                current.map(std::ffi::OsStr::new),
                legacy.map(std::ffi::OsStr::new),
            )
        };

        assert_eq!(
            choose(None, Some("off")),
            Some((VerifyDeps::Enabled(false), true)),
            "the old name must still supply a policy, flagged as renamed"
        );
        assert_eq!(
            choose(Some("error"), None),
            Some((VerifyDeps::Error, false)),
            "the current name supplies a policy with no rename notice"
        );
        assert_eq!(
            choose(Some("error"), Some("off")),
            Some((VerifyDeps::Error, false)),
            "with both set the current name wins outright and the notice stays silent"
        );
        assert_eq!(
            choose(Some("nonsense"), Some("off")),
            None,
            "a present-but-unparseable current name must not hand the decision to a stale one"
        );
        assert_eq!(
            choose(None, Some("nonsense")),
            None,
            "an unparseable old value supplies nothing, so there is no rename to announce"
        );
        assert_eq!(choose(None, None), None);
    }

    /// nub's own environment variable accepts only values nub implements — the
    /// same subset `nub.jsonc` takes. pnpm's `install`/`prompt` reach the policy
    /// solely through the pnpm-mirroring surfaces.
    #[test]
    fn the_verify_deps_variable_accepts_only_the_values_nub_implements() {
        use crate::project_config::VerifyDeps;
        assert_eq!(parse_verify_deps_env("  ERROR "), Some(VerifyDeps::Error));
        assert_eq!(parse_verify_deps_env("warn"), Some(VerifyDeps::Warn));
        assert_eq!(
            parse_verify_deps_env("true"),
            Some(VerifyDeps::Enabled(true))
        );
        for off in ["off", "false", "0", "no", "none", "skip"] {
            assert_eq!(
                parse_verify_deps_env(off),
                Some(VerifyDeps::Enabled(false)),
                "`{off}` disables the gate"
            );
        }
        for unimplemented in ["install", "prompt"] {
            assert_eq!(
                parse_verify_deps_env(unimplemented),
                None,
                "`{unimplemented}` is pnpm-only; accepting it would do something other than what it says"
            );
        }
        assert_eq!(parse_verify_deps_env(""), None);
    }
}
