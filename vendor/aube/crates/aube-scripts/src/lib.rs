//! Lifecycle script runner for aube.
//!
//! **Security model**:
//! - Scripts from the **root package** (the project's own `package.json`)
//!   run by default. They're written by the user, so they're trusted the
//!   same way a user trusts `aube run <script>`.
//! - Scripts from **installed dependencies** (e.g. `node-gyp` postinstall
//!   from a native module) are SKIPPED by default. A package runs its
//!   lifecycle scripts only if the active [`BuildPolicy`] allows it —
//!   configured via `pnpm.allowBuilds` in `package.json`, `allowBuilds`
//!   in `aube-workspace.yaml` (or `pnpm-workspace.yaml`), or the
//!   escape-hatch `--dangerously-allow-all-builds` flag.
//! - `--ignore-scripts` forces everything off, matching pnpm/npm.

pub mod content_sniff;
pub mod default_trust;
pub mod policy;

#[cfg(target_os = "linux")]
mod linux_jail;

#[cfg(windows)]
mod windows_job;

pub use content_sniff::{Suspicion, SuspicionKind, sniff_lifecycle};
pub use default_trust::is_default_trusted;
pub use policy::{AllowDecision, BuildPolicy, BuildPolicyError, pattern_matches};

use aube_manifest::PackageJson;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Settings that affect every package-script shell aube spawns.
#[derive(Debug, Clone, Default)]
pub struct ScriptSettings {
    pub node_options: Option<String>,
    pub script_shell: Option<PathBuf>,
    pub unsafe_perm: Option<bool>,
    pub shell_emulator: bool,
    /// Directory of the project's resolved Node runtime, prepended to
    /// PATH after the project `.bin` so the switched node beats the
    /// system one while project-local binaries still win. `None` when
    /// no runtime switching is active.
    pub node_bin_dir: Option<PathBuf>,
    /// The resolved node executable, exported as `npm_node_execpath` /
    /// `NODE` (npm parity) for every script.
    pub node_exe: Option<PathBuf>,
    /// Embedder-supplied environment overlay applied verbatim to every
    /// lifecycle spawn (`(key, value)` pairs, set last so they win over the
    /// other `ScriptSettings`-derived keys). Generic by design — aube assigns
    /// no meaning to the keys; an embedder fills it to route scripts through a
    /// provisioned/augmented runtime (e.g. nub points `NODE` at its node shim,
    /// pins `npm_node_execpath`, and injects its preload via `NODE_OPTIONS`)
    /// without aube growing a runtime-specific field. Default-empty =
    /// behavior-preserving: a stock aube spawns exactly as before.
    pub env_overlay: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// Embedder-supplied PATH entries prepended (in order, ahead of the
    /// existing PATH) to every lifecycle spawn. Counterpart to `env_overlay`
    /// for the one variable whose composition aube already owns: an embedder
    /// uses it to place a runtime shim dir first so a bare `node` in a build
    /// script resolves to the augmented runtime. Default-empty = no-op.
    pub path_prepends: Vec<PathBuf>,
    /// The top-level package-manager command, exported as `npm_command`
    /// (npm/pnpm parity): `"run-script"` for `aube run`, `"install"`
    /// for install lifecycle hooks, `"rebuild"`, `"pack"`, etc. `None`
    /// leaves `npm_command` unset.
    pub command: Option<String>,
    /// Path to a runnable `node-gyp` stand-in, exported as
    /// `npm_config_node_gyp` (npm/pnpm parity). npm/pnpm point this at
    /// their bundled `node-gyp/bin/node-gyp.js`; aube bootstraps node-gyp
    /// lazily, so this is a tiny shim that resolves (and bootstraps on
    /// first use) the real node-gyp and forwards argv. `None` leaves
    /// `npm_config_node_gyp` unset.
    pub node_gyp_js: Option<PathBuf>,
    /// The resolved default registry URL, exported as
    /// `npm_config_registry` (npm/pnpm parity) so dependency lifecycle
    /// scripts (and `aube run` scripts) see the same registry aube
    /// resolved from `.npmrc` / env. Mirrors the `aube run` script-spawn
    /// path so install postinstalls and `run` scripts agree. `None`
    /// leaves `npm_config_registry` unset.
    pub registry: Option<String>,
}

/// Native build jail applied to dependency lifecycle scripts.
#[derive(Debug, Clone)]
pub struct ScriptJail {
    pub package_dir: PathBuf,
    pub env: Vec<String>,
    pub read_paths: Vec<PathBuf>,
    pub write_paths: Vec<PathBuf>,
    pub network: bool,
}

impl ScriptJail {
    pub fn new(package_dir: impl Into<PathBuf>) -> Self {
        Self {
            package_dir: package_dir.into(),
            env: Vec::new(),
            read_paths: Vec::new(),
            write_paths: Vec::new(),
            network: false,
        }
    }

    pub fn with_env(mut self, env: impl IntoIterator<Item = String>) -> Self {
        self.env = env.into_iter().collect();
        self
    }

    pub fn with_read_paths(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.read_paths = paths.into_iter().collect();
        self
    }

    pub fn with_write_paths(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.write_paths = paths.into_iter().collect();
        self
    }

    pub fn with_network(mut self, network: bool) -> Self {
        self.network = network;
        self
    }
}

pub struct ScriptJailHomeCleanup {
    path: PathBuf,
}

impl ScriptJailHomeCleanup {
    pub fn new(jail: &ScriptJail) -> Self {
        Self {
            path: jail_home(&jail.package_dir),
        }
    }
}

impl Drop for ScriptJailHomeCleanup {
    fn drop(&mut self) {
        if self.path.exists()
            && let Err(err) = std::fs::remove_dir_all(&self.path)
        {
            tracing::debug!("failed to clean jail HOME {}: {err}", self.path.display());
        }
    }
}

static SCRIPT_SETTINGS: std::sync::OnceLock<std::sync::RwLock<ScriptSettings>> =
    std::sync::OnceLock::new();

fn script_settings_lock() -> &'static std::sync::RwLock<ScriptSettings> {
    SCRIPT_SETTINGS.get_or_init(|| std::sync::RwLock::new(ScriptSettings::default()))
}

/// Replace the process-wide script settings snapshot. CLI commands call
/// this after resolving `.npmrc` / workspace settings for the active
/// project.
pub fn set_script_settings(settings: ScriptSettings) {
    match script_settings_lock().write() {
        Ok(mut guard) => *guard = settings,
        Err(poisoned) => *poisoned.into_inner() = settings,
    }
}

fn script_settings() -> ScriptSettings {
    match script_settings_lock().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Public snapshot of the process-wide [`ScriptSettings`]. Lets a caller
/// read the current settings — in particular the embedder-owned
/// `env_overlay` / `path_prepends` — so a later `set_script_settings`
/// (e.g. aube's `.npmrc`/workspace settings pass) can carry the embedder
/// fields forward instead of clobbering them.
pub fn script_settings_snapshot() -> ScriptSettings {
    script_settings()
}

/// Prepend `bin_dir` to the current `PATH` using the platform's path
/// separator (`:` on Unix, `;` on Windows).
pub fn prepend_path(bin_dir: &Path) -> std::ffi::OsString {
    prepend_paths(std::slice::from_ref(&bin_dir.to_path_buf()))
}

/// [`prepend_path`] for multiple directories, prepended in order.
pub fn prepend_paths(bin_dirs: &[PathBuf]) -> std::ffi::OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut entries: Vec<PathBuf> = bin_dirs.to_vec();
    entries.extend(std::env::split_paths(&path));
    std::env::join_paths(entries).unwrap_or(path)
}

/// Spawn a shell command line. On Unix we go through `sh -c`, on
/// Windows through `cmd.exe /d /s /c` — matching what npm passes in
/// `@npmcli/run-script`.
///
/// On Windows, the script command line is appended with
/// [`std::os::windows::process::CommandExt::raw_arg`] instead of
/// the normal `.arg()` path. `.arg()` would run the string through
/// Rust's `CommandLineToArgvW`-oriented encoder, which wraps it in
/// `"..."` and escapes interior `"` as `\"` — but `cmd.exe` parses
/// command lines with a different set of rules and does not
/// understand `\"`, so a script like
/// `node -e "require('is-odd')(3)"` arrives mangled. `raw_arg`
/// hands the command line to `CreateProcessW` verbatim, so we
/// control the exact bytes cmd.exe sees. We wrap the whole script
/// in an outer pair of double quotes, which `/s` tells cmd.exe to
/// strip (just those outer quotes — the rest of the string is
/// preserved literally). This is the same trick
/// `@npmcli/run-script` and `node-cross-spawn` use.
pub fn spawn_shell(script_cmd: &str) -> tokio::process::Command {
    let settings = script_settings();
    spawn_shell_with_settings(script_cmd, &settings)
}

fn spawn_shell_with_settings(
    script_cmd: &str,
    settings: &ScriptSettings,
) -> tokio::process::Command {
    #[cfg(unix)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new(
            settings
                .script_shell
                .as_deref()
                .unwrap_or_else(|| Path::new("sh")),
        );
        cmd.arg("-c").arg(script_cmd);
        cmd
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new(
            settings
                .script_shell
                .as_deref()
                .unwrap_or_else(|| Path::new("cmd.exe")),
        );
        if settings.script_shell.is_some() {
            cmd.arg("-c").arg(script_cmd);
        } else {
            // `/d` skips AutoRun, `/s` flips the quote-stripping rule
            // so only the *outer* `"..."` pair is removed, `/c` runs
            // the command and exits. Build the raw argv tail manually
            // so cmd.exe sees the original script bytes.
            cmd.raw_arg("/d /s /c \"").raw_arg(script_cmd).raw_arg("\"");
        }
        cmd
    };
    apply_script_settings_env(&mut cmd, settings);
    // Aborting the `JoinSet` that drives the parallel lifecycle pass
    // drops the spawned `Child`, which without `kill_on_drop` would
    // leave the shell running detached (Discussion #654). On Windows
    // that's only half the fix — `TerminateProcess` on `cmd.exe`
    // doesn't reach grandchildren like `node-gyp` → `MSBuild` → `node`;
    // [`run_command_killing_descendants`] also assigns the shell to a
    // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` job object to reap the
    // whole tree.
    cmd.kill_on_drop(true);
    cmd
}

#[cfg(target_os = "macos")]
fn sbpl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
fn push_write_rule(rules: &mut Vec<String>, path: &Path) {
    let path = sbpl_escape(&path.to_string_lossy());
    let rule = format!("(allow file-write* (subpath \"{path}\"))");
    if !rules.iter().any(|existing| existing == &rule) {
        rules.push(rule);
    }
}

#[cfg(target_os = "macos")]
fn jail_profile(jail: &ScriptJail, home: &Path) -> String {
    let mut rules = vec![
        "(version 1)".to_string(),
        "(allow default)".to_string(),
        "(allow network* (local unix))".to_string(),
        "(deny file-write*)".to_string(),
    ];
    if !jail.network {
        rules.insert(2, "(deny network*)".to_string());
    }

    for path in [
        Path::new("/tmp"),
        Path::new("/private/tmp"),
        Path::new("/dev"),
    ] {
        push_write_rule(&mut rules, path);
    }
    for path in [&jail.package_dir, home] {
        push_write_rule(&mut rules, path);
    }
    for path in &jail.write_paths {
        push_write_rule(&mut rules, path);
    }
    for path in [&jail.package_dir, home] {
        if let Ok(canonical) = path.canonicalize() {
            push_write_rule(&mut rules, &canonical);
        }
    }
    for path in &jail.write_paths {
        if let Ok(canonical) = path.canonicalize() {
            push_write_rule(&mut rules, &canonical);
        }
    }
    rules.join("\n")
}

#[cfg(target_os = "macos")]
fn spawn_jailed_shell(
    script_cmd: &str,
    settings: &ScriptSettings,
    jail: &ScriptJail,
    home: &Path,
) -> tokio::process::Command {
    let shell = settings
        .script_shell
        .as_deref()
        .unwrap_or_else(|| Path::new("sh"));
    let profile = jail_profile(jail, home);
    let mut cmd = tokio::process::Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(profile)
        .arg("--")
        .arg(shell)
        .arg("-c")
        .arg(script_cmd);
    apply_script_settings_env(&mut cmd, settings);
    // Matches the unjailed path — see `spawn_shell_with_settings`.
    cmd.kill_on_drop(true);
    cmd
}

#[cfg(target_os = "linux")]
fn spawn_jailed_shell(
    script_cmd: &str,
    settings: &ScriptSettings,
    jail: &ScriptJail,
    home: &Path,
) -> tokio::process::Command {
    let mut cmd = spawn_shell_with_settings(script_cmd, settings);
    let jail = jail.clone();
    let home = home.to_path_buf();
    unsafe {
        cmd.pre_exec(move || {
            linux_jail::apply_landlock(&jail, &home).map_err(std::io::Error::other)?;
            if !jail.network {
                linux_jail::apply_seccomp_net_filter().map_err(std::io::Error::other)?;
            }
            Ok(())
        });
    }
    cmd
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn spawn_jailed_shell(
    script_cmd: &str,
    settings: &ScriptSettings,
    _jail: &ScriptJail,
    _home: &Path,
) -> tokio::process::Command {
    spawn_shell_with_settings(script_cmd, settings)
}

/// Shell-quote one arg for safe splicing into a shell command line.
///
/// Used by `aube run <script> -- args`. Args get joined into the
/// script string, then sh -c or cmd /c reparses the whole thing. If
/// user arg contains $, backticks, ;, |, &, (, ), etc, the shell
/// interprets those as metacharacters. That is shell injection.
/// `aube run echo 'hello; rm -rf ~'` would run two commands. Same
/// issue npm had pre-2016. Quote each arg so shell treats it as one
/// literal token.
///
/// Unix: wrap in single quotes. sh treats interior of '...' as pure
/// literal with one exception, embedded single quote. Handle that
/// with the standard '\'' escape trick: close the single-quoted
/// string, emit an escaped quote, reopen. Works in every POSIX sh.
///
/// Windows cmd.exe: wrap in double quotes. cmd interprets many
/// metachars even inside double quotes, but CreateProcessW hands the
/// string to our spawn_shell that uses `/d /s /c "..."`, the outer
/// quotes get stripped per /s rule and the content runs. Escape
/// interior " and backslash per CommandLineToArgvW. Full cmd.exe
/// metachar caret-escaping is a rabbit hole, so this is best-effort,
/// works for the common cases, matches what node's shell-quote does.
pub fn shell_quote_arg(arg: &str) -> String {
    #[cfg(unix)]
    {
        let mut out = String::with_capacity(arg.len() + 2);
        out.push('\'');
        for ch in arg.chars() {
            if ch == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(ch);
            }
        }
        out.push('\'');
        out
    }
    #[cfg(windows)]
    {
        let mut out = String::with_capacity(arg.len() + 2);
        out.push('"');
        let mut backslashes: usize = 0;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    for _ in 0..backslashes * 2 + 1 {
                        out.push('\\');
                    }
                    out.push('"');
                    backslashes = 0;
                }
                // cmd.exe expands %VAR% even inside double quotes.
                // Outer `/s /c "..."` only strips the outermost
                // quote pair, the shell still runs env expansion
                // on the body. Argument like `%COMSPEC%` would
                // otherwise get replaced with the shell path
                // before the child saw it. Double the percent so
                // cmd passes a literal `%` through. Full
                // caret-escaping of `^ & | < > ( )` is a deeper
                // rabbit hole, this handles the common injection
                // vector.
                '%' => {
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push_str("%%");
                }
                _ => {
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push(ch);
                }
            }
        }
        for _ in 0..backslashes * 2 {
            out.push('\\');
        }
        out.push('"');
        out
    }
}

/// Translate child ExitStatus to a parent exit code.
///
/// On Unix a signal-killed child has None from .code(). Old code
/// collapsed that to 1. That loses signal identity: SIGKILL (OOM
/// killer, exit 137), SIGSEGV (139), Ctrl-C (130) all look like
/// plain exit 1. CI pipelines watching for 137 to detect OOM cannot
/// distinguish it from a normal script error anymore. Bash convention
/// is 128 + signum, match that.
///
/// Windows has no signal concept so .code() is always Some, the
/// fallback 1 is dead code there but keeps the function total.
pub fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

/// User agent string exported to lifecycle scripts as
/// `npm_config_user_agent`. Mirrors pnpm's format
/// (`<name>/<version> <os> <arch>`) so dep build scripts that sniff
/// the env var to detect the running PM (e.g. `husky`,
/// `unrs-resolver`) recognize aube without falling back to npm-mode.
/// OS/arch use Node's `process.platform` / `process.arch` vocabulary
/// (`darwin`/`linux`/`win32`, `x64`/`arm64`), not Rust's native
/// `std::env::consts::{OS,ARCH}` values, so tools that parse the full
/// UA string identify the platform the same way npm/yarn/pnpm do.
///
/// The product token defaults to the compile-time
/// [`Embedder::user_agent`](aube_util::identity::Embedder::user_agent)
/// (`aube/<version>` for standalone aube). A lifecycle-specific token set on
/// the engine context's `lifecycle_user_agent_product` outranks it — that
/// token is genuinely runtime (an embedder advertises the served PM's role
/// plus the project's *resolved* node version, which can't be a compile-time
/// literal), and it exists exactly for this export so postinstall sniffers see
/// it while the const identity governs everything else.
pub fn aube_user_agent() -> String {
    let product = aube_util::engine_context()
        .lifecycle_user_agent_product
        .unwrap_or_else(|| aube_util::embedder().user_agent.to_owned());
    format!("{product} {} {}", node_platform(), node_arch())
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn node_arch() -> &'static str {
    // Mappings from Rust's `std::env::consts::ARCH` to Node's
    // `process.arch`. Common arches first; the rare ones at the bottom
    // exist so the test below stays a real guarantee on every host
    // Rust ships, not just x64/arm64. Pass-through covers `arm`,
    // `mips`, `riscv64`, `s390x` — those tokens match between the two
    // vocabularies.
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "powerpc" => "ppc",
        "powerpc64" => "ppc64",
        "loongarch64" => "loong64",
        other => other,
    }
}

fn apply_script_settings_env(cmd: &mut tokio::process::Command, settings: &ScriptSettings) {
    // Strip credentials that aube itself owns before we spawn any
    // lifecycle script. AUBE_AUTH_TOKEN is aube's own registry login
    // token. No transitive postinstall has any business reading it.
    // NPM_TOKEN and NODE_AUTH_TOKEN stay untouched because release
    // flows ("npm publish" in a postpublish script) genuinely need
    // them. Matches what pnpm does today.
    cmd.env_remove("AUBE_AUTH_TOKEN");
    // pnpm parity: every lifecycle script gets `npm_config_user_agent`
    // so dep postinstalls can detect the running PM. Set here (not at
    // spawn time) so it flows through both the jailed and the
    // non-jailed paths.
    cmd.env("npm_config_user_agent", aube_user_agent());
    // pnpm parity: lifecycle scripts get `npm_config_registry` set to the
    // resolved default registry, matching the `aube run` script-spawn
    // path so install postinstalls and `run` scripts see the same value.
    if let Some(registry) = settings.registry.as_deref() {
        cmd.env("npm_config_registry", registry);
    }
    // `npm_execpath`: the package-manager binary that drove the script.
    // Tools (and pnpm's own `$npm_execpath run …` postinstalls) read it
    // to re-invoke the *same* PM. `current_exe()` is the aube binary;
    // ignore the rare resolution failure rather than abort the script.
    // Reused below for `AUBE_NODE_GYP_EXE` (same binary), so resolve once.
    let aube_exe = std::env::current_exe().ok();
    if let Some(exe) = aube_exe.as_deref() {
        cmd.env("npm_execpath", exe);
    }
    // `npm_node_execpath` / `NODE`: the node binary scripts should use
    // — the switched runtime's node, or the ambient `node` on PATH.
    // Set here (not at spawn) so it survives the jail's `env_clear`.
    if let Some(node_exe) = settings.node_exe.as_deref() {
        cmd.env("npm_node_execpath", node_exe).env("NODE", node_exe);
    }
    // `npm_command`: the top-level PM command (run-script / install / …).
    if let Some(command) = settings.command.as_deref() {
        cmd.env("npm_command", command);
    }
    // `npm_config_node_gyp`: path to a runnable node-gyp. npm/pnpm bundle
    // node-gyp and point this at its `bin/node-gyp.js`; aube hands out a
    // lazy shim that resolves the bootstrapped node-gyp on first use. The
    // shim trampolines back into aube via `AUBE_NODE_GYP_EXE`, so always
    // stamp the running aube — it must be a real aube that implements
    // `__node-gyp-bootstrap`, never an inherited/user-set value (which
    // could be stale or wrong). `aube run` stamps the same value at its
    // own spawn site; keeping both unconditional holds the two paths in
    // lockstep. `AUBE_NODE_GYP_PROJECT_DIR` is optional — the shim falls
    // back to the script's cwd.
    if let Some(node_gyp_js) = settings.node_gyp_js.as_deref() {
        cmd.env("npm_config_node_gyp", node_gyp_js);
        if let Some(exe) = aube_exe.as_deref() {
            cmd.env("AUBE_NODE_GYP_EXE", exe);
        }
    }
    if let Some(node_options) = settings.node_options.as_deref() {
        cmd.env("NODE_OPTIONS", node_options);
    }
    if let Some(unsafe_perm) = settings.unsafe_perm {
        cmd.env(
            "npm_config_unsafe_perm",
            if unsafe_perm { "true" } else { "false" },
        );
    }
    if settings.shell_emulator {
        cmd.env("npm_config_shell_emulator", "true");
    }
    // Embedder env overlay, applied LAST so it outranks the keys above (an
    // embedder that, say, pins its own NODE_OPTIONS wins over the
    // settings-derived one). Generic: aube assigns no meaning to the keys.
    for (key, value) in &settings.env_overlay {
        cmd.env(key, value);
    }
}

/// Prepend `prepends` (in order) ahead of `existing`, joined with the
/// platform PATH separator. Pure so it's unit-testable without a spawn;
/// used to compose the embedder's `path_prepends` onto a lifecycle
/// script's PATH (the shim dir lands first so a bare `node` in a build
/// script resolves to the embedder's augmented runtime).
fn compose_overlay_path(prepends: &[PathBuf], existing: &std::ffi::OsStr) -> std::ffi::OsString {
    if prepends.is_empty() {
        return existing.to_owned();
    }
    let mut entries: Vec<PathBuf> = prepends.to_vec();
    entries.extend(std::env::split_paths(existing));
    std::env::join_paths(entries).unwrap_or_else(|_| existing.to_owned())
}

/// Apply the manifest-derived `npm_package_*` env (name, version,
/// absolute `npm_package_json` path, and the deep-flattened
/// `engines`/`config`/`bin`) plus `npm_lifecycle_script` (the raw
/// script body) to a lifecycle or `aube run` command. Call last so the
/// values land *after* any jail `env_clear`. `script_dir` is the
/// directory the script runs in, whose `package.json` is the manifest
/// being executed; `lifecycle_script` is the raw script body exported
/// as `npm_lifecycle_script`.
///
/// pnpm rebuilds the `npm_package_*` namespace per package: it drops
/// every inherited `npm_package_*` key and stamps only the running
/// manifest's allowlist. We mirror that — scrub first, then re-stamp —
/// so a script never sees a parent/sibling package's fields (verified
/// against pnpm 11.5). Without the scrub, non-allowlisted inherited
/// keys (e.g. an outer `npm run`'s `npm_package_description`) would
/// leak through on the unjailed path and break allowlist parity. On
/// the jailed path the prior `env_clear` already dropped them, so the
/// scrub is a harmless no-op there.
pub fn apply_npm_manifest_env(
    cmd: &mut tokio::process::Command,
    manifest: &PackageJson,
    script_dir: &Path,
    lifecycle_script: &str,
) {
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|k| k.starts_with("npm_package_")) {
            cmd.env_remove(&key);
        }
    }
    cmd.env("npm_lifecycle_script", lifecycle_script);
    cmd.env("npm_package_json", script_dir.join("package.json"));
    for (key, value) in manifest.npm_package_env() {
        cmd.env(key, value);
    }
}

fn safe_jail_env_key(key: &str) -> bool {
    const EXACT: &[&str] = &[
        "PATH",
        "HOME",
        "TERM",
        "LANG",
        "LC_ALL",
        "INIT_CWD",
        "npm_lifecycle_event",
        "npm_package_name",
        "npm_package_version",
    ];
    if EXACT.contains(&key) {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    if lower.contains("token")
        || lower.contains("auth")
        || lower.contains("password")
        || lower.contains("credential")
        || lower.contains("secret")
    {
        return false;
    }
    key.starts_with("npm_config_")
}

fn inherit_jail_env_key(key: &str, extra_env: &[String]) -> bool {
    (safe_jail_env_key(key) || extra_env.iter().any(|env| env == key))
        && !matches!(
            key,
            "PATH" | "HOME" | "npm_lifecycle_event" | "npm_package_name" | "npm_package_version"
        )
}

fn jail_home(package_dir: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    package_dir.hash(&mut hasher);
    let hash = hasher.finish();
    let name = package_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("package")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    std::env::temp_dir()
        .join("aube-jail")
        .join(std::process::id().to_string())
        .join(format!("{name}-{hash:016x}"))
}

fn apply_jail_env(
    cmd: &mut tokio::process::Command,
    path_env: &std::ffi::OsStr,
    home: &Path,
    project_root: &Path,
    manifest: &PackageJson,
    script_name: &str,
    extra_env: &[String],
) {
    cmd.env_clear();
    cmd.env("PATH", path_env)
        .env("HOME", home)
        .env("TMPDIR", home)
        .env("TMP", home)
        .env("TEMP", home)
        .env("npm_lifecycle_event", script_name);
    if std::env::var_os("INIT_CWD").is_none() {
        cmd.env("INIT_CWD", project_root);
    }
    if let Some(ref name) = manifest.name {
        cmd.env("npm_package_name", name);
    }
    if let Some(ref version) = manifest.version {
        cmd.env("npm_package_version", version);
    }
    for (key, val) in std::env::vars_os() {
        let Some(key_str) = key.to_str() else {
            continue;
        };
        if inherit_jail_env_key(key_str, extra_env) {
            cmd.env(key, val);
        }
    }
}

/// Lifecycle hooks that `aube install` runs against the root package's
/// `scripts` field, in this order: `preinstall` → (dependencies link) →
/// `install` → `postinstall` → `prepare`. Matches pnpm / npm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleHook {
    PreInstall,
    Install,
    PostInstall,
    Prepare,
}

impl LifecycleHook {
    pub fn script_name(self) -> &'static str {
        match self {
            Self::PreInstall => "preinstall",
            Self::Install => "install",
            Self::PostInstall => "postinstall",
            Self::Prepare => "prepare",
        }
    }
}

/// Dependency lifecycle hooks, in the order aube runs them for each
/// allowlisted package. `prepare` is intentionally omitted — it's meant
/// for the root package and git-dep preparation, not installed tarballs.
pub const DEP_LIFECYCLE_HOOKS: [LifecycleHook; 3] = [
    LifecycleHook::PreInstall,
    LifecycleHook::Install,
    LifecycleHook::PostInstall,
];

/// Holds the real stderr fd saved before `aube` redirects fd 2 to
/// `/dev/null` under `--silent`. Child processes spawned through
/// `child_stderr()` get a fresh dup of this fd so their stderr still
/// reaches the user's terminal — `--silent` only silences aube's own
/// output, not the scripts / binaries it invokes (matches `pnpm
/// --loglevel silent`). A value of `-1` means silent mode is off and
/// children should inherit stderr normally.
#[cfg(unix)]
static SAVED_STDERR_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Called once by `aube` after it saves + redirects fd 2. Passing
/// the caller-owned saved fd here means child processes spawned via
/// `child_stderr()` will write to the real terminal stderr instead of
/// `/dev/null`.
#[cfg(unix)]
pub fn set_saved_stderr_fd(fd: std::os::fd::RawFd) {
    SAVED_STDERR_FD.store(fd, std::sync::atomic::Ordering::SeqCst);
}

/// Windows has no equivalent fd-based silencing plumbing: aube's
/// `SilentStderrGuard` is `libc::dup`/`libc::dup2` on fd 2, and those
/// calls are gated to unix in `aube`. The stub keeps the public
/// API shape identical so call sites compile unchanged.
#[cfg(not(unix))]
pub fn set_saved_stderr_fd(_fd: i32) {}

/// Returns a `Stdio` suitable for a child process's stderr. When silent
/// mode is active, this dups the saved real-stderr fd so the child
/// bypasses the `/dev/null` redirect on fd 2. Otherwise returns
/// `Stdio::inherit()`.
#[cfg(unix)]
pub fn child_stderr() -> std::process::Stdio {
    let fd = SAVED_STDERR_FD.load(std::sync::atomic::Ordering::SeqCst);
    if fd < 0 {
        return std::process::Stdio::inherit();
    }
    // SAFETY: `fd` was registered by `set_saved_stderr_fd` from a live
    // `dup` that `aube`'s `SilentStderrGuard` keeps open for the
    // duration of main. `BorrowedFd` only borrows, so this does not
    // transfer ownership.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    match borrowed.try_clone_to_owned() {
        Ok(owned) => std::process::Stdio::from(owned),
        Err(_) => std::process::Stdio::inherit(),
    }
}

#[cfg(not(unix))]
pub fn child_stderr() -> std::process::Stdio {
    std::process::Stdio::inherit()
}

/// Write `line` plus a newline to the parent's real stderr. Used by
/// the recursive-run output multiplexer, which pipes child stderr
/// through aube and re-emits each line with a `<package>: ` prefix —
/// `eprintln!` writes to fd 2, which `SilentStderrGuard` has redirected
/// to `/dev/null` under `--silent`, so child stderr would otherwise be
/// silently swallowed in `--silent --parallel` mode. Routes through the
/// saved real-stderr fd when silent mode is active, fd 2 otherwise.
///
/// `write_all` of a pre-built `<line>\n` buffer issues a single short
/// write to the kernel; on TTYs and pipes the kernel's `PIPE_BUF`
/// (= 4096+ on every supported unix) atomicity keeps lines from
/// concurrent pump tasks intact without explicit locking. The dup
/// happens per line so we don't share a long-lived `File` handle that
/// would need its own lock — a duplicate `write` syscall pair is
/// cheaper than an `Arc<Mutex<File>>` and correct under concurrency.
#[cfg(unix)]
pub fn write_line_to_real_stderr(line: &str) {
    use std::io::Write;
    let saved = SAVED_STDERR_FD.load(std::sync::atomic::Ordering::SeqCst);
    let fd = if saved >= 0 { saved } else { 2 };
    // SAFETY: `fd` is either the saved real-stderr fd (kept live by
    // `SilentStderrGuard` for the duration of main) or fd 2 (always
    // open). `BorrowedFd` only borrows; ownership stays with the
    // saved-fd / std-stream side and `try_clone_to_owned` issues a
    // `dup` so dropping the resulting `File` does not close fd 2 or
    // the saved fd.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let Ok(owned) = borrowed.try_clone_to_owned() else {
        return;
    };
    let mut file = std::fs::File::from(owned);
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    let _ = file.write_all(buf.as_bytes());
}

#[cfg(not(unix))]
pub fn write_line_to_real_stderr(line: &str) {
    eprintln!("{line}");
}

/// Spawn `cmd`, wait for it, and on Windows attach the shell to a
/// kill-on-job-close job object so an aborted lifecycle script reaps
/// its full descendant tree instead of leaving orphans behind.
///
/// `kill_on_drop(true)` on the parent `Command` (set by
/// [`spawn_shell_with_settings`]) covers `TerminateProcess` /
/// `SIGKILL` on the direct shell. That alone is enough on Unix
/// because most build tooling handles the parent dying — and the
/// shell itself is the foreground process for the subscript pipeline.
/// On Windows the shell's grandchildren (`node-gyp` → `MSBuild` →
/// `node`) are *not* part of the shell's job by default, so killing
/// the shell leaves them running detached. Discussion #654 is the
/// in-the-wild bug: `aube add --global` failed, aube exited, and
/// node/MSBuild kept writing to the console.
///
/// We mitigate by spawning, then assigning the child process handle
/// to a job created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The
/// `_job` binding's `Drop` (called when this future returns, panics,
/// or is aborted) closes the last job handle, and the kernel kills
/// every assigned process — including everything the shell has
/// spawned by that point. There is a microscopic race between spawn
/// and `AssignProcessToJobObject`, but the shell does not have time
/// to spawn anything in that window; the `tokio::process::Child`
/// returns control to us synchronously after `CreateProcessW`
/// returns.
///
/// Job-object failures are fail-open: restricted Windows environments
/// (nested-job parents, container policy, handle quota) can refuse
/// either `CreateJobObjectW` or `AssignProcessToJobObject`. In those
/// cases we surface a `WARN_AUBE_WINDOWS_JOB_OBJECT_UNAVAILABLE`
/// warning and run the script anyway — degrading to the
/// `kill_on_drop`-only path that aube used before this fix. Failing
/// closed would block lifecycle scripts entirely on those hosts,
/// which is a worse regression than the orphaning we're trying to
/// avoid.
async fn run_command_killing_descendants(
    mut cmd: tokio::process::Command,
    script_name: &str,
) -> Result<std::process::ExitStatus, Error> {
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Spawn(script_name.to_string(), e.to_string()))?;
    #[cfg(windows)]
    let _job = match windows_job::JobObject::new() {
        Ok(job) => {
            // raw_handle() returns None only if the child has already
            // been reaped, which can't happen between spawn() and the
            // very next line.
            if let Some(handle) = child.raw_handle()
                && let Err(err) = job.assign(handle)
            {
                // Realistic causes: parent job created without
                // JOB_OBJECT_LIMIT_BREAKAWAY_OK (pre-Win8 nested-job
                // restrictions, enterprise policy), or the shell
                // already exited. In either case the kill-tree
                // guarantee is gone — log loud enough that CI logs
                // pick it up.
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_WINDOWS_JOB_OBJECT_UNAVAILABLE,
                    "windows: AssignProcessToJobObject failed for `{script_name}` shell ({err}); \
                     grandchildren may be orphaned if the script is aborted"
                );
            }
            Some(job)
        }
        Err(err) => {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_WINDOWS_JOB_OBJECT_UNAVAILABLE,
                "windows: CreateJobObjectW failed for `{script_name}` shell ({err}); \
                 running without orphan-reaping — grandchildren may leak if aborted"
            );
            None
        }
    };
    child
        .wait()
        .await
        .map_err(|e| Error::Spawn(script_name.to_string(), e.to_string()))
}

/// Run a single npm-style script line through `sh -c` with the usual
/// environment (`$PATH` extended with `node_modules/.bin`, `INIT_CWD`,
/// `npm_lifecycle_event`, `npm_package_name`, `npm_package_version`).
///
/// `extra_bin_dirs` are prepended to `PATH` in order, *before* the
/// project-level `.bin`. Dep lifecycle scripts pass the dep's own
/// sibling `node_modules/.bin/` so transitive binaries (e.g.
/// `prebuild-install`, `node-gyp`) declared in the dep's
/// `dependencies` are reachable, optionally followed by aube-owned
/// tool dirs (e.g. the bootstrapped node-gyp). Root scripts pass
/// `&[]` — their transitive bins are already hoisted into the
/// project-level `.bin`.
///
/// Inherits stdio from the parent so the user sees script output live.
/// Returns Err on non-zero exit so install fails fast if a lifecycle
/// script breaks, matching pnpm.
#[allow(clippy::too_many_arguments)]
pub async fn run_script(
    script_dir: &Path,
    project_root: &Path,
    modules_dir_name: &str,
    manifest: &PackageJson,
    script_name: &str,
    script_cmd: &str,
    extra_bin_dirs: &[&Path],
    jail: Option<&ScriptJail>,
) -> Result<(), Error> {
    // Per-script diag span. Tags the package name (when present) and the
    // script name so the analyzer can attribute postinstall / preinstall /
    // build cost to the exact lifecycle entry rather than the aggregate
    // `dep_lifecycle` phase total.
    let _diag = aube_util::diag::Span::new(aube_util::diag::Category::Script, "run_script")
        .with_meta_fn(|| {
            let pkg = manifest.name.as_deref().unwrap_or("(root)");
            format!(
                r#"{{"pkg":{},"script":{}}}"#,
                aube_util::diag::jstr(pkg),
                aube_util::diag::jstr(script_name)
            )
        });
    // PATH prepends (most-local-first): `extra_bin_dirs` in caller
    // order, then the project root's `<modules_dir>/.bin`. For root
    // scripts `script_dir == project_root` and `extra_bin_dirs` is
    // empty, which matches the old behavior. `modules_dir_name`
    // honors pnpm's `modulesDir` setting — defaults to
    // `"node_modules"` at the call site, but a workspace may have
    // configured something else.
    let project_bin = project_root.join(modules_dir_name).join(".bin");
    let settings = script_settings();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut entries: Vec<PathBuf> = Vec::with_capacity(extra_bin_dirs.len() + 2);
    for dir in extra_bin_dirs {
        entries.push(dir.to_path_buf());
    }
    entries.push(project_bin);
    // The switched Node runtime sits between project bins and the
    // inherited PATH: scripts spawning `node` (directly or via
    // `#!/usr/bin/env node`) get the project's pinned version, while
    // anything installed into `.bin` still wins.
    if let Some(dir) = &settings.node_bin_dir {
        entries.push(dir.clone());
    }
    entries.extend(std::env::split_paths(&path));
    let new_path = std::env::join_paths(entries).unwrap_or(path);

    let settings = script_settings();
    // Embedder PATH prepends (e.g. nub's node-shim dir) lead the composed
    // PATH so a bare `node` in a build script hits the augmented runtime
    // before `extra_bin_dirs` / the project `.bin` / the system PATH. Flows
    // through both the jailed and non-jailed branches: `apply_jail_env`
    // re-applies this same `new_path` after its `env_clear()`.
    let new_path = compose_overlay_path(&settings.path_prepends, &new_path);
    let jail_home = jail.map(|j| jail_home(&j.package_dir));
    if let Some(home) = &jail_home {
        std::fs::create_dir_all(home)
            .map_err(|e| Error::Spawn(script_name.to_string(), e.to_string()))?;
    }
    let mut cmd = match (jail, jail_home.as_deref()) {
        (Some(jail), Some(home)) => spawn_jailed_shell(script_cmd, &settings, jail, home),
        _ => spawn_shell_with_settings(script_cmd, &settings),
    };
    cmd.current_dir(script_dir)
        .stderr(child_stderr())
        .env("PATH", &new_path)
        .env("npm_lifecycle_event", script_name);

    // Pass INIT_CWD the way npm/pnpm do — the directory the user
    // invoked the package manager from, *not* the script's own cwd.
    // Native-module build tooling (node-gyp, prebuild-install, etc.)
    // reads INIT_CWD to locate the project root when caching binaries.
    // Preserve if already set by a parent aube invocation so nested
    // scripts see the outermost cwd.
    if std::env::var_os("INIT_CWD").is_none() {
        cmd.env("INIT_CWD", project_root);
    }

    if let (Some(jail), Some(home)) = (jail, jail_home.as_deref()) {
        apply_jail_env(
            &mut cmd,
            &new_path,
            home,
            project_root,
            manifest,
            script_name,
            &jail.env,
        );
        apply_script_settings_env(&mut cmd, &settings);
    }

    // npm-compat manifest env, applied last so it survives the jail's
    // `env_clear`: name/version/json plus the deep-flattened
    // engines/config/bin, and the raw script body (`npm_lifecycle_script`).
    apply_npm_manifest_env(&mut cmd, manifest, script_dir, script_cmd);

    tracing::debug!("lifecycle: {script_name} → {script_cmd}");
    let status = run_command_killing_descendants(cmd, script_name).await?;

    if !status.success() {
        return Err(Error::NonZeroExit {
            script: script_name.to_string(),
            // Resolve the child's status to a concrete exit code so the
            // user sees a plain integer. On Unix a signal-killed child
            // (status.code() == None) renders as the bash-convention
            // 128 + signum rather than the misleading Rust `None`.
            code: exit_code_from_status(status),
        });
    }

    Ok(())
}

/// Run a lifecycle hook against the root package, if a script for it is
/// defined. Returns `Ok(false)` if the hook wasn't defined (no-op),
/// `Ok(true)` if it ran successfully.
///
/// The caller is responsible for gating on `--ignore-scripts`.
pub async fn run_root_hook(
    project_dir: &Path,
    modules_dir_name: &str,
    manifest: &PackageJson,
    hook: LifecycleHook,
) -> Result<bool, Error> {
    run_root_script_by_name(project_dir, modules_dir_name, manifest, hook.script_name()).await
}

/// Run a named root-package script if it's defined. Used by commands
/// (pack, publish, version) that need to run lifecycle hooks outside
/// the install-focused [`LifecycleHook`] enum. Returns `Ok(false)` if
/// the script isn't defined.
///
/// The caller is responsible for gating on `--ignore-scripts`.
pub async fn run_root_script_by_name(
    project_dir: &Path,
    modules_dir_name: &str,
    manifest: &PackageJson,
    name: &str,
) -> Result<bool, Error> {
    let Some(script_cmd) = manifest.scripts.get(name) else {
        return Ok(false);
    };
    run_script(
        project_dir,
        project_dir,
        modules_dir_name,
        manifest,
        name,
        script_cmd,
        &[],
        None,
    )
    .await?;
    Ok(true)
}

/// Single source of truth for the implicit `node-gyp rebuild`
/// fallback: returns `Some("node-gyp rebuild")` when the package ships
/// a `binding.gyp` at its root AND the manifest leaves both `install`
/// and `preinstall` empty (either one is the author's explicit
/// opt-out from the default).
///
/// `has_binding_gyp` is passed by the caller so this helper is
/// agnostic to *how* presence was detected — the install pipeline
/// stats the materialized package dir, while `aube ignored-builds`
/// reads the store `PackageIndex` since the package may not be
/// linked into `node_modules` yet. Both paths must agree on the gate
/// condition, so they both go through this.
pub fn implicit_install_script(
    manifest: &PackageJson,
    has_binding_gyp: bool,
) -> Option<&'static str> {
    if !has_binding_gyp {
        return None;
    }
    if manifest
        .scripts
        .contains_key(LifecycleHook::Install.script_name())
        || manifest
            .scripts
            .contains_key(LifecycleHook::PreInstall.script_name())
    {
        return None;
    }
    Some("node-gyp rebuild")
}

/// Default `install` command for a materialized dependency directory.
/// Thin wrapper around [`implicit_install_script`] that supplies
/// `has_binding_gyp` by stat'ing `<package_dir>/binding.gyp`.
pub fn default_install_script(package_dir: &Path, manifest: &PackageJson) -> Option<&'static str> {
    implicit_install_script(manifest, package_dir.join("binding.gyp").is_file())
}

/// True if [`run_dep_hook`] would actually execute something for this
/// package across any of the dependency lifecycle hooks. Callers use
/// this to skip fan-out work for packages that have nothing to run —
/// including the implicit `node-gyp rebuild` default.
pub fn has_dep_lifecycle_work(package_dir: &Path, manifest: &PackageJson) -> bool {
    if DEP_LIFECYCLE_HOOKS
        .iter()
        .any(|h| manifest.scripts.contains_key(h.script_name()))
    {
        return true;
    }
    default_install_script(package_dir, manifest).is_some()
}

/// Break any content-addressed-store hardlinks under a freshly
/// materialized package directory before a lifecycle script is allowed
/// to mutate it in place.
///
/// On a copy-on-write filesystem (APFS clonefile, btrfs/xfs FICLONE)
/// the linker reflinks store blobs into the package directory, so every
/// materialized file already has its own inode and an in-place write
/// touches only the project copy. On a hardlink filesystem (the
/// ext4/most-Linux/CI default) the linker hard-links the store blob
/// into the package directory instead: the materialized file *shares*
/// the store inode. A dependency's `install`/`postinstall` runs with
/// its `current_dir` set to this directory, and a build step that
/// rewrites a file in place (node-gyp emitting `build/Release/*.node`, a
/// postinstall patching its own sources) writes *through* the shared
/// inode and corrupts the machine-wide store blob — poisoning every
/// other project on the machine whose content hash matches.
///
/// This walks `package_dir` and, for every regular file whose hard-link
/// count is greater than one (i.e. it still shares an inode with the
/// store, or with a sibling project's materialized copy), replaces it
/// with a private copy on a fresh inode: copy the bytes to a sibling
/// temp file, restore the mode, then atomically rename it over the
/// original. After this the build script can only ever write to inodes
/// this project owns.
///
/// It is a deliberate no-op on reflink/copy filesystems: a reflinked or
/// copied file has `nlink == 1`, so the link count gate skips it and
/// behavior is byte-for-byte unchanged from before this pass existed.
/// The scope is one package directory — only the dep about to build —
/// never the whole store. Symlinks are left as-is (they are recreated
/// by the linker, not shared with the store) and directories are
/// recursed into. Errors are surfaced so a half-broken copy never
/// silently leaves a script writing through a live store link.
///
/// Cheap on the common path: the walk only `lstat`s each entry and acts
/// solely on the (few) files that are still store-linked. The byte copy
/// cost is bounded by the size of the one package being built, and only
/// paid on hardlink filesystems for packages that actually run a build.
#[cfg(unix)]
pub fn break_cas_hardlinks(package_dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mut stack = vec![package_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            // A missing dir is nothing to unshare; surface anything else.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            // `symlink_metadata` does not follow symlinks: a symlink's
            // own nlink is irrelevant (the linker recreates these; they
            // never share a store inode), and following it could walk
            // out of the package or deref a store path we must not edit.
            let meta = std::fs::symlink_metadata(&path)?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            // nlink == 1 means this file already owns its inode (reflink
            // or copy strategy, or already unshared): leave it untouched
            // so reflink/copy filesystems stay byte-for-byte unchanged.
            if meta.nlink() <= 1 {
                continue;
            }
            unshare_one_file(&path, meta.permissions().mode())?;
        }
    }
    Ok(())
}

/// Replace a single hardlinked file with a private copy on a fresh
/// inode, preserving its mode (notably the +x bit). Copy the bytes to a
/// sibling temp file on the same directory, fix its mode, then
/// atomically rename it over the original — the rename drops the old
/// name's reference to the shared store inode without ever opening that
/// inode for writing, so the store blob is never touched. A plain
/// unlink+rewrite would also work but leaves a window where the file is
/// absent; the temp+rename keeps the path continuously present.
#[cfg(unix)]
fn unshare_one_file(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let bytes = std::fs::read(path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(
        ".aube-unshare-{}-{}.tmp",
        std::process::id(),
        file_name
    ));
    // Best-effort cleanup of a leftover temp from a crashed prior run.
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, &bytes)?;
    // Restore the original mode, including the +x bit, so executables
    // stay executable. The fresh inode owner is always us, so it is
    // owner-writable enough for the build to overwrite it (the store
    // blobs are 0o644 / 0o755, both owner-writable).
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Run a lifecycle hook against an installed dependency's package
/// directory. Mirrors [`run_root_hook`] but spawns inside `package_dir`
/// (the actual linked package directory, e.g.
/// `node_modules/.aube/<dep_path>/node_modules/<name>`). The manifest
/// is the dependency's own `package.json`, *not* the project root's.
///
/// `dep_modules_dir` is the dep's sibling `node_modules/` — i.e.
/// `package_dir`'s parent for unscoped packages, or `package_dir`'s
/// grandparent for scoped (`@scope/name`). `<dep_modules_dir>/.bin`
/// is prepended to `PATH` so the dep's postinstall can spawn tools
/// declared in its own `dependencies` (the transitive-bin case —
/// `prebuild-install`, `node-gyp`, `napi-postinstall`). The install
/// driver writes shims there via `link_dep_bins`; `rebuild` mirrors
/// the same pass.
///
/// For the `install` hook specifically, if the manifest leaves both
/// `install` and `preinstall` empty but the package has a top-level
/// `binding.gyp`, this falls back to running `node-gyp rebuild` — the
/// node-gyp default that npm and pnpm both honor so native modules
/// without a prebuilt binary still compile on install.
///
/// `tool_bin_dirs` are prepended to `PATH` *after* the dep's own
/// `.bin` so that aube-bootstrapped tools (e.g. node-gyp) fill the
/// gap for deps that shell out to them without declaring them as
/// their own `dependencies`. The dep's local bin still wins if it
/// shipped its own copy.
///
/// The caller is responsible for gating on `BuildPolicy` and
/// `--ignore-scripts`. Returns `Ok(false)` if the hook wasn't defined.
#[allow(clippy::too_many_arguments)]
pub async fn run_dep_hook(
    package_dir: &Path,
    dep_modules_dir: &Path,
    project_root: &Path,
    modules_dir_name: &str,
    manifest: &PackageJson,
    hook: LifecycleHook,
    tool_bin_dirs: &[&Path],
    jail: Option<&ScriptJail>,
) -> Result<bool, Error> {
    let name = hook.script_name();
    let script_cmd: &str = match manifest.scripts.get(name) {
        Some(s) => s.as_str(),
        None => match hook {
            LifecycleHook::Install => match default_install_script(package_dir, manifest) {
                Some(s) => s,
                None => return Ok(false),
            },
            _ => return Ok(false),
        },
    };
    let dep_bin_dir = dep_modules_dir.join(".bin");
    let mut bin_dirs: Vec<&Path> = Vec::with_capacity(tool_bin_dirs.len() + 1);
    bin_dirs.push(&dep_bin_dir);
    bin_dirs.extend(tool_bin_dirs.iter().copied());
    run_script(
        package_dir,
        project_root,
        modules_dir_name,
        manifest,
        name,
        script_cmd,
        &bin_dirs,
        jail,
    )
    .await?;
    Ok(true)
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("failed to spawn script {0}: {1}")]
    #[diagnostic(code(ERR_AUBE_SCRIPT_SPAWN))]
    Spawn(String, String),
    #[error("script `{script}` exited with code {code}")]
    #[diagnostic(code(ERR_AUBE_SCRIPT_NON_ZERO_EXIT))]
    NonZeroExit { script: String, code: i32 },
}

#[cfg(test)]
mod user_agent_tests {
    use super::*;

    #[test]
    fn user_agent_uses_node_style_platform_and_arch() {
        let ua = aube_user_agent();
        // Format: "aube/<version> <platform> <arch>"
        assert!(ua.starts_with("aube/"), "unexpected prefix: {ua}");
        let parts: Vec<&str> = ua.split(' ').collect();
        assert_eq!(parts.len(), 3, "expected 3 space-separated fields: {ua}");
        // Platform must be a Node-style token, not Rust's `macos`/`windows`.
        let platform = parts[1];
        assert!(
            matches!(
                platform,
                "darwin" | "linux" | "win32" | "freebsd" | "openbsd" | "netbsd" | "dragonfly"
            ),
            "platform `{platform}` should follow Node's `process.platform` vocabulary"
        );
        // Arch must be a Node-style token, not Rust's `x86_64`/`aarch64`.
        // Allowlist is the union of mapped outputs (`node_arch`) and the
        // pass-through tokens that already match Node's vocabulary.
        let arch = parts[2];
        assert!(
            matches!(
                arch,
                "x64"
                    | "arm64"
                    | "ia32"
                    | "arm"
                    | "ppc"
                    | "ppc64"
                    | "loong64"
                    | "mips"
                    | "riscv64"
                    | "s390x"
            ),
            "arch `{arch}` should follow Node's `process.arch` vocabulary"
        );
    }
}

#[cfg(test)]
mod env_overlay_tests {
    use super::*;
    use std::ffi::OsString;

    fn env_value<'a>(cmd: &'a tokio::process::Command, name: &str) -> Option<&'a OsString> {
        cmd.as_std()
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new(name))
            .and_then(|(_, val)| val)
            .map(|v| v.to_owned())
            .map(|v| Box::leak(Box::new(v)) as &OsString)
    }

    /// The embedder-supplied `env_overlay` is applied to every lifecycle
    /// spawn so nub can route dep build scripts through its augmented Node
    /// (NODE → shim, NODE_OPTIONS preload, npm_node_execpath pin) without a
    /// nub-specific field in `ScriptSettings`. Default-empty = behavior
    /// preserved when no embedder fills it.
    #[test]
    fn env_overlay_keys_are_applied() {
        let mut cmd = tokio::process::Command::new("node");
        let settings = ScriptSettings {
            env_overlay: vec![
                (OsString::from("NODE"), OsString::from("/shim/node")),
                (
                    OsString::from("npm_node_execpath"),
                    OsString::from("/pinned/node"),
                ),
            ],
            ..Default::default()
        };
        apply_script_settings_env(&mut cmd, &settings);

        assert_eq!(
            env_value(&cmd, "NODE").map(|v| v.to_string_lossy().into_owned()),
            Some("/shim/node".to_string()),
            "env_overlay must set $NODE so userland $NODE child.js re-enters the augmented Node"
        );
        assert_eq!(
            env_value(&cmd, "npm_node_execpath").map(|v| v.to_string_lossy().into_owned()),
            Some("/pinned/node".to_string()),
            "env_overlay must pin npm_node_execpath to the provisioned Node (ABI fix)"
        );
    }

    /// Empty overlay + prepends is a pure no-op: nothing about the spawn env
    /// changes, so the upstream-default behavior is preserved bit-for-bit.
    #[test]
    fn empty_overlay_is_behavior_preserving() {
        let mut cmd = tokio::process::Command::new("node");
        let settings = ScriptSettings::default();
        apply_script_settings_env(&mut cmd, &settings);
        assert!(
            env_value(&cmd, "NODE").is_none(),
            "default ScriptSettings must not introduce a $NODE override"
        );
        assert!(settings.path_prepends.is_empty());
    }

    /// `path_prepends` lands ahead of the existing PATH in order, so the shim
    /// dir wins over the system node — composed by [`compose_overlay_path`].
    #[test]
    fn path_prepends_lead_the_existing_path() {
        let base = OsString::from("/usr/bin:/bin");
        let prepends = vec![PathBuf::from("/shim"), PathBuf::from("/tools")];
        let composed = compose_overlay_path(&prepends, &base);
        let composed = composed.to_string_lossy();
        #[cfg(unix)]
        assert_eq!(composed, "/shim:/tools:/usr/bin:/bin");
        #[cfg(windows)]
        assert!(composed.starts_with("/shim;/tools;"));
    }
}

#[cfg(test)]
mod jail_tests {
    use super::*;

    #[test]
    fn jail_home_uses_full_package_path() {
        let a = jail_home(Path::new("/tmp/project/node_modules/@scope-a/native"));
        let b = jail_home(Path::new("/tmp/project/node_modules/@scope-b/native"));

        assert_ne!(a, b);
        assert!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("native-")
        );
        assert!(
            b.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("native-")
        );
    }

    #[test]
    fn jail_home_cleanup_removes_temp_home() {
        let package_dir = std::env::temp_dir()
            .join("aube-jail-cleanup-test")
            .join(std::process::id().to_string())
            .join("node_modules")
            .join("native");
        let jail = ScriptJail::new(&package_dir);
        let home = jail_home(&package_dir);
        std::fs::create_dir_all(home.join(".cache")).unwrap();
        std::fs::write(home.join(".cache").join("marker"), "x").unwrap();

        {
            let _cleanup = ScriptJailHomeCleanup::new(&jail);
        }

        assert!(!home.exists());
    }

    #[test]
    fn parent_env_cannot_override_explicit_jail_metadata() {
        for key in [
            "PATH",
            "HOME",
            "npm_lifecycle_event",
            "npm_package_name",
            "npm_package_version",
        ] {
            assert!(!inherit_jail_env_key(key, &[]));
        }
        assert!(inherit_jail_env_key("INIT_CWD", &[]));
        assert!(inherit_jail_env_key("npm_config_arch", &[]));
        assert!(!inherit_jail_env_key("npm_config__authToken", &[]));
        assert!(inherit_jail_env_key(
            "SHARP_DIST_BASE_URL",
            &["SHARP_DIST_BASE_URL".to_string()]
        ));
    }

    #[test]
    fn jail_env_preserves_script_settings_after_clear() {
        let mut cmd = tokio::process::Command::new("node");
        let manifest = PackageJson {
            name: Some("pkg".to_string()),
            version: Some("1.2.3".to_string()),
            ..Default::default()
        };
        let settings = ScriptSettings {
            node_options: Some("--conditions=aube".to_string()),
            unsafe_perm: Some(false),
            shell_emulator: true,
            ..Default::default()
        };

        apply_jail_env(
            &mut cmd,
            std::ffi::OsStr::new("/bin"),
            Path::new("/tmp/aube-jail/home"),
            Path::new("/tmp/project"),
            &manifest,
            "postinstall",
            &[],
        );
        apply_script_settings_env(&mut cmd, &settings);

        let envs = cmd.as_std().get_envs().collect::<Vec<_>>();
        let env = |name: &str| {
            envs.iter()
                .find(|(key, _)| *key == std::ffi::OsStr::new(name))
                .and_then(|(_, val)| *val)
                .and_then(|val| val.to_str())
        };

        assert_eq!(env("NODE_OPTIONS"), Some("--conditions=aube"));
        assert_eq!(env("npm_config_unsafe_perm"), Some("false"));
        assert_eq!(env("npm_config_shell_emulator"), Some("true"));
        assert_eq!(env("npm_lifecycle_event"), Some("postinstall"));
        assert_eq!(env("npm_package_name"), Some("pkg"));
        assert_eq!(env("npm_package_version"), Some("1.2.3"));
    }
}

#[cfg(all(test, windows))]
mod windows_quote_tests {
    use super::shell_quote_arg;

    #[test]
    fn windows_path_backslash_not_doubled() {
        let q = shell_quote_arg(r"C:\Users\me\file.txt");
        assert_eq!(q, "\"C:\\Users\\me\\file.txt\"");
    }

    #[test]
    fn windows_trailing_backslash_doubled_before_close_quote() {
        let q = shell_quote_arg(r"C:\path\");
        assert_eq!(q, "\"C:\\path\\\\\"");
    }

    #[test]
    fn windows_quote_in_arg_escapes_with_backslash() {
        assert_eq!(shell_quote_arg(r#"a"b"#), "\"a\\\"b\"");
        assert_eq!(shell_quote_arg(r#"a\"b"#), "\"a\\\\\\\"b\"");
        assert_eq!(shell_quote_arg(r#"a\\"b"#), "\"a\\\\\\\\\\\"b\"");
    }
}

// Regression test for Discussion #654: aborting the lifecycle JoinSet
// after a failed `aube add --global` left node-gyp / MSBuild / node
// running orphaned on Windows because `TerminateProcess` on the cmd.exe
// shell does not propagate to its descendants. The Job Object the
// spawn helper now attaches the shell to must reap the entire process
// tree when the parent future is dropped.
#[cfg(all(test, windows))]
mod windows_job_object_tests {
    use super::*;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    fn is_process_alive(pid: u32) -> bool {
        // SAFETY: documented entry points; we close any handle we
        // successfully obtain. `OpenProcess` returns NULL once the
        // pid has been reaped or never existed.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }

    async fn wait_until<F: Fn() -> bool>(check: F, timeout: Duration) -> bool {
        let start = Instant::now();
        while !check() {
            if start.elapsed() > timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(75)).await;
        }
        true
    }

    #[tokio::test]
    async fn aborting_script_kills_grandchildren() {
        // Unique pid-file path per test run so concurrent test
        // executions don't stomp each other. `tempfile` is not a
        // dep of this crate; std::env::temp_dir + nanos is enough.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid_file = std::env::temp_dir().join(format!("aube-test-grandchild-{nanos}.pid"));
        // Background a hidden powershell that writes its own PID
        // and then sleeps long enough that the test will fail if it
        // isn't reaped. `start /b` detaches the powershell from the
        // cmd.exe shell — exactly the orphaned-grandchild shape that
        // node-gyp / MSBuild produce in Discussion #654. The trailing
        // `ping` keeps the shell itself alive for ~8s so the test
        // can race a liveness check against the running grandchild
        // before aborting the parent future.
        let script = format!(
            "start /b powershell -NoProfile -WindowStyle Hidden -Command \
             \"$pid | Out-File -Encoding ascii -FilePath '{}'; Start-Sleep 60\" \
             & ping -n 10 127.0.0.1 >nul",
            pid_file.display()
        );
        let cmd = spawn_shell_with_settings(&script, &ScriptSettings::default());
        let task = tokio::spawn(async move {
            let _ = run_command_killing_descendants(cmd, "test-grandchild").await;
        });

        let appeared = wait_until(
            || {
                std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    .is_some()
            },
            Duration::from_secs(20),
        )
        .await;
        assert!(appeared, "grandchild never wrote pid file at {pid_file:?}");
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("read pid file")
            .trim()
            .parse()
            .expect("pid file was parseable before reading");
        assert!(
            is_process_alive(pid),
            "grandchild pid {pid} not alive immediately after writing pid file"
        );

        // Drop the future mid-`child.wait().await`. The `_job` local
        // in `run_command_killing_descendants` drops with it, which
        // closes the last handle and fires `KILL_ON_JOB_CLOSE` —
        // killing both the shell *and* the detached powershell.
        task.abort();
        let _ = task.await;

        let reaped = wait_until(|| !is_process_alive(pid), Duration::from_secs(10)).await;
        let _ = std::fs::remove_file(&pid_file);
        assert!(
            reaped,
            "grandchild pid {pid} survived parent abort — job object did not kill the tree"
        );
    }
}

#[cfg(test)]
mod non_zero_exit_display_tests {
    use super::*;

    // A failed lifecycle script's error message is surfaced verbatim to the
    // user (e.g. "root postinstall script failed: <this>"). The exit code must
    // read as a plain integer, not the Rust `Option<i32>` Debug form `Some(42)`,
    // which leaks internal types into user-facing output.
    #[test]
    fn renders_plain_exit_code_not_option_debug() {
        let err = Error::NonZeroExit {
            script: "postinstall".to_string(),
            code: 42,
        };
        let msg = err.to_string();
        assert_eq!(msg, "script `postinstall` exited with code 42");
        assert!(
            !msg.contains("Some("),
            "exit code leaked Option Debug form: {msg}"
        );
    }
}

#[cfg(all(test, unix))]
mod break_cas_hardlinks_tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn unique_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "aube-unshare-test-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    // The core invariant: when a materialized package file is hardlinked
    // to the content-addressed store (nlink > 1), breaking the link gives
    // the package file a private inode so a later in-place write can never
    // reach the store blob. This is what stops a dep's build script from
    // corrupting the machine-wide store on a hardlink filesystem.
    #[test]
    fn in_place_write_after_break_does_not_reach_the_store_blob() {
        let root = unique_dir("repro");
        let store_blob = root.join("store-blob");
        let pkg_dir = root.join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(&store_blob, b"PRISTINE-STORE-CONTENT").unwrap();
        let materialized = pkg_dir.join("payload.txt");
        // Simulate the linker's hardlink strategy: the package file shares
        // the store blob's inode.
        std::fs::hard_link(&store_blob, &materialized).unwrap();
        assert_eq!(
            std::fs::metadata(&materialized).unwrap().nlink(),
            2,
            "precondition: materialized file shares the store inode"
        );

        break_cas_hardlinks(&pkg_dir).unwrap();

        // After unsharing, both files have their own inode.
        assert_eq!(
            std::fs::metadata(&materialized).unwrap().nlink(),
            1,
            "materialized file should own a private inode after the break"
        );
        assert_eq!(
            std::fs::metadata(&store_blob).unwrap().nlink(),
            1,
            "store blob should no longer be shared"
        );
        // Content survived the unshare.
        assert_eq!(
            std::fs::read(&materialized).unwrap(),
            b"PRISTINE-STORE-CONTENT"
        );

        // The decisive check: an in-place rewrite of the package file (what
        // a build script does) leaves the store blob untouched.
        std::fs::write(&materialized, b"BUILT-IN-PLACE").unwrap();
        assert_eq!(
            std::fs::read(&store_blob).unwrap(),
            b"PRISTINE-STORE-CONTENT",
            "store blob was corrupted through a shared inode"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // On a reflink/copy filesystem the linker produces private inodes
    // (nlink == 1), so the pass must be a no-op: it must not touch the
    // file's inode identity, contents, or mode.
    #[test]
    fn nlink_one_files_are_left_untouched() {
        let root = unique_dir("noop");
        let pkg_dir = root.join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let f = pkg_dir.join("private.txt");
        std::fs::write(&f, b"already-private").unwrap();
        let inode_before = std::fs::metadata(&f).unwrap().ino();

        break_cas_hardlinks(&pkg_dir).unwrap();

        let meta_after = std::fs::metadata(&f).unwrap();
        assert_eq!(
            meta_after.ino(),
            inode_before,
            "an nlink==1 file must keep its inode (reflink/copy path is byte-for-byte unchanged)"
        );
        assert_eq!(std::fs::read(&f).unwrap(), b"already-private");
        let _ = std::fs::remove_dir_all(&root);
    }

    // The +x bit and nested-directory structure must survive the copy:
    // CLIs shipped via npm depend on their executable mode.
    #[test]
    fn preserves_executable_mode_and_recurses_into_subdirs() {
        let root = unique_dir("mode");
        let store_bin = root.join("store-bin");
        let pkg_dir = root.join("pkg");
        let nested = pkg_dir.join("bin");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(&store_bin, b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(&store_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let exe = nested.join("cli");
        std::fs::hard_link(&store_bin, &exe).unwrap();
        assert_eq!(std::fs::metadata(&exe).unwrap().nlink(), 2);

        break_cas_hardlinks(&pkg_dir).unwrap();

        let meta = std::fs::metadata(&exe).unwrap();
        assert_eq!(meta.nlink(), 1, "nested file should be unshared");
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o755,
            "executable bit must survive the unshare"
        );
        // The store binary is untouched by an in-place edit of the package copy.
        std::fs::write(&exe, b"overwritten").unwrap();
        assert_eq!(std::fs::read(&store_bin).unwrap(), b"#!/bin/sh\necho hi\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Symlinks must be left exactly as-is — they are recreated by the
    // linker and never share a store inode; following them could deref a
    // store path we must not edit or walk out of the package.
    #[test]
    fn leaves_symlinks_in_place() {
        let root = unique_dir("symlink");
        let pkg_dir = root.join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let real = pkg_dir.join("real.txt");
        std::fs::write(&real, b"data").unwrap();
        let link = pkg_dir.join("link.txt");
        std::os::unix::fs::symlink("real.txt", &link).unwrap();

        break_cas_hardlinks(&pkg_dir).unwrap();

        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "symlink must still be a symlink after the pass"
        );
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            std::path::Path::new("real.txt")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
