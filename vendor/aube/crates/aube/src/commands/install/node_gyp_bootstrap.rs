//! On-demand bootstrap of `node-gyp` into an aube-owned cache dir.
//!
//! Many npm packages ship a native addon and rely on `node-gyp` being
//! available on `PATH` during their `install` lifecycle — either
//! explicitly (`"install": "node-gyp rebuild"`), implicitly through
//! aube's `default_install_script` fallback when the package ships a
//! `binding.gyp` with no install/preinstall, or transitively via
//! tooling like `node-gyp-build` that shells out to `node-gyp`. pnpm
//! and npm solve this by bundling node-gyp with themselves; aube (a
//! Rust binary) bootstraps it lazily on first need.
//!
//! User precedence: if `node-gyp` is already resolvable from the
//! package's own `.bin` or ambient `PATH` (system install, nvm, a shim
//! in a test fixture), [`lazy_shim_bin_dir`] stays out of the way — the
//! user's copy wins. Otherwise it prepends a cheap shim that installs
//! node-gyp under `<cache_dir>/tools/node-gyp/<bucket>/` only if invoked.
//!
//! The install runs in-process against a freshly-written
//! `package.json` that pins node-gyp, via
//! [`super::run_with_project_lock`] with `ignore_scripts` set. It
//! deliberately does *not* shell out to the aube binary: an embedder
//! links this crate into its own executable, so
//! `std::env::current_exe` would name the host program and the
//! recursive `install --ignore-scripts --silent` would be parsed as
//! host arguments.
//!
//! The outer project's `.npmrc` (if any) is copied into private staging
//! as its own project-level `.npmrc` so private-registry URLs and
//! auth tokens configured by monorepo / enterprise setups flow
//! through to the bootstrap install, which resolves against that temporary
//! dir and would otherwise only pick up `~/.npmrc`.
//!
//! The tool dir is its own single-package project (stub workspace
//! yaml), so its project lock is keyed off the tool dir and both
//! serializes concurrent bootstraps across processes and stays
//! disjoint from the outer install's lock. The fast-path existence
//! check short-circuits every subsequent invocation.
use miette::{IntoDiagnostic, WrapErr, miette};
use std::path::{Path, PathBuf};

/// Major-version pin. Bumping the bucket invalidates the cache and
/// triggers a re-bootstrap on the next install.
const BUCKET: &str = "v12";
/// Semver range passed to `aube install`. Keep aligned with `BUCKET`.
const SPEC: &str = "^12.0.0";

#[cfg(windows)]
const BINARY_NAMES: &[&str] = &["node-gyp.cmd", "node-gyp.exe", "node-gyp"];
#[cfg(not(windows))]
const BINARY_NAMES: &[&str] = &["node-gyp"];

fn node_gyp_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        if node_gyp_bin_exists(&dir) {
            return true;
        }
    }
    false
}

/// True if `bin_dir` contains any of the platform's accepted
/// `node-gyp` shim filenames. On Windows npm installs `node-gyp.cmd`
/// (sometimes `.exe` alongside), so a bare-string check would always
/// miss the bootstrapped shim and the fast-path would never fire.
pub(crate) fn node_gyp_bin_exists(bin_dir: &Path) -> bool {
    node_gyp_binary(bin_dir).is_some()
}

fn node_gyp_binary(bin_dir: &Path) -> Option<PathBuf> {
    BINARY_NAMES
        .iter()
        .map(|name| bin_dir.join(name))
        .find(|path| path.is_file())
}

/// Resolve a cached binary all the way through an aube-generated wrapper.
///
/// Isolated installs use regular shell/cmd wrappers rather than symlinks so
/// they can inject `NODE_PATH`. The wrapper itself remains a valid file after
/// its virtual-store target disappears, which made the cache fast path accept
/// a broken node-gyp install. Unknown regular files are retained for forward
/// compatibility; only wrappers we can decode are held to target existence.
fn cached_node_gyp_binary(bin_dir: &Path) -> Option<PathBuf> {
    let mut opaque_binary = None;
    let mut stale_wrapper = false;
    for path in BINARY_NAMES.iter().map(|name| bin_dir.join(name)) {
        if !path.is_file() {
            continue;
        }
        match aube_linker::sys::resolve_bin_shim(&path) {
            // A decoded live wrapper wins. Record stale wrappers so their
            // opaque Windows siblings cannot make the cache look healthy,
            // while still allowing a later decoded live wrapper to win.
            Ok(Some(shim)) if shim.target.is_file() => return Some(path),
            Ok(Some(_)) => stale_wrapper = true,
            Ok(None) => {
                opaque_binary.get_or_insert(path);
            }
            Err(_) => continue,
        };
    }
    (!stale_wrapper).then_some(opaque_binary).flatten()
}

fn tool_root() -> miette::Result<PathBuf> {
    let cache = aube_store::dirs::cache_dir()
        .ok_or_else(|| miette!("could not resolve cache dir for node-gyp bootstrap"))?;
    Ok(cache.join("tools").join("node-gyp"))
}

/// Remove registry configuration left in public tool buckets by older versions.
/// A confining embedder must propagate failure before granting this cache to a child.
pub fn clear_legacy_registry_configs() -> miette::Result<()> {
    let root = tool_root()?;
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).into_diagnostic(),
    };
    for entry in entries {
        let entry = entry.into_diagnostic()?;
        if entry.file_type().into_diagnostic()?.is_dir() {
            remove_legacy_registry_config(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_legacy_registry_config(tool_dir: &Path) -> miette::Result<()> {
    match std::fs::remove_file(tool_dir.join(".npmrc")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e)
            .into_diagnostic()
            .wrap_err("removing cached node-gyp credentials"),
    }
}

/// Install the pinned node-gyp outside child-readable caches, then publish only
/// its modules. Project registry credentials stay in the private staging directory.
pub async fn ensure_cached(project_dir: &Path) -> miette::Result<PathBuf> {
    let root = tool_root()?;
    let tool_dir = root.join(BUCKET);
    let bin_dir = tool_dir.join("node_modules").join(".bin");
    std::fs::create_dir_all(&tool_dir).into_diagnostic()?;
    let _publish_lock = crate::commands::take_project_lock(&tool_dir)?;
    // Retire configuration copied by older versions before exposing a cached tool.
    remove_legacy_registry_config(&tool_dir)?;
    if tool_tree_usable(&tool_dir, &bin_dir) {
        return Ok(bin_dir);
    }

    let cache = aube_store::dirs::cache_dir()
        .ok_or_else(|| miette!("could not resolve node-gyp staging directory"))?;
    let staging_root = cache.join("tool-bootstrap");
    std::fs::create_dir_all(&staging_root).into_diagnostic()?;
    let stage = tempfile::Builder::new()
        .prefix("node-gyp-")
        .tempdir_in(staging_root)
        .into_diagnostic()?;
    write_bootstrap_project(stage.path(), &project_dir.join(".npmrc"))?;
    let stage_lock = crate::commands::take_project_lock(stage.path())?;
    let mut opts = super::InstallOptions::with_mode(super::FrozenMode::Prefer);
    opts.ignore_scripts = true;
    opts.register_in_store = false;
    // Publish a self-contained tree: Windows isolated-layout junctions retain
    // absolute staging targets and become dangling when the staging dir drops.
    opts.cli_flags
        .push(("node-linker".into(), "hoisted".into()));
    opts.control = super::InstallControl::silent();
    super::run_with_project_lock(opts, &stage_lock)
        .await
        .wrap_err("bootstrapping node-gyp in private staging directory")?;

    let modules = tool_dir.join("node_modules");
    if modules.exists() {
        std::fs::remove_dir_all(&modules).into_diagnostic()?;
    }
    std::fs::rename(stage.path().join("node_modules"), &modules)
        .into_diagnostic()
        .wrap_err("publishing node-gyp modules")?;
    if !tool_tree_usable(&tool_dir, &bin_dir) {
        return Err(miette!(
            "node-gyp bootstrap left no usable tool in {}",
            tool_dir.display()
        ));
    }
    Ok(bin_dir)
}

fn tool_tree_usable(tool_dir: &Path, bin_dir: &Path) -> bool {
    cached_node_gyp_binary(bin_dir).is_some()
        && tool_dir
            .join("node_modules/node-gyp/package.json")
            .is_file()
}

/// Eager counterpart to [`lazy_shim_bin_dir`], for builds that will run jailed.
///
/// A jailed script cannot use the lazy shim. The jail clears the environment
/// and substitutes a temporary HOME, so the shim's `__node-gyp-bootstrap`
/// re-entry resolves [`aube_store::dirs::cache_dir`] under *that* HOME, finds
/// no tool dir, and cannot refill one because the jail also denies network.
/// Pre-warming the real cache does not help — the re-entry never looks there.
/// Resolving out here, outside the jail, puts a directly executable node-gyp on
/// the script's PATH.
///
/// `npm_config_node_gyp` is a separate channel: its JS shim resolves this same
/// prepared tree directly, without a package-manager re-entry or PATH search.
///
/// A project-local tool wins. Ambient tools may sit outside the jail's read grants,
/// so they are not sufficient for the confined path.
pub(crate) async fn ensure_bin_dir_for_jail(
    project_bin_dir: &Path,
    project_dir: &Path,
) -> miette::Result<Option<PathBuf>> {
    if node_gyp_bin_exists(project_bin_dir) {
        return Ok(None);
    }
    ensure_cached(project_dir).await.map(Some)
}

pub(crate) fn lazy_shim_bin_dir(project_bin_dir: &Path) -> miette::Result<Option<PathBuf>> {
    if node_gyp_bin_exists(project_bin_dir) || node_gyp_on_path() {
        return Ok(None);
    }
    let shim_dir = tool_root()?.join("lazy-bin");
    write_lazy_shims(&shim_dir)?;
    Ok(Some(shim_dir))
}

/// Path to the lazy `node-gyp.js` shim, exported as `npm_config_node_gyp`
/// for parity with npm/pnpm (which point it at their bundled
/// `node-gyp/bin/node-gyp.js`). Unlike [`lazy_shim_bin_dir`], this is
/// returned unconditionally — `npm_config_node_gyp` is a separate channel
/// from `PATH`, and npm/pnpm always set it even when a system node-gyp
/// exists. Writing the shim is cheap (a few tiny files) and never
/// bootstraps; the real node-gyp install is deferred until a tool runs
/// `node $npm_config_node_gyp`. Content-checked on every call (like
/// [`lazy_shim_bin_dir`]) so a shipped shim fix self-heals rather than
/// being pinned to whatever first landed in the cache — see
/// [`write_lazy_shims`] for why that check beats an unconditional
/// rewrite.
// `pub`: an embedder that spawns Node itself (nub's file runner) points
// `npm_config_node_gyp` at this shim so a `node-gyp` invocation inside the
// child resolves through the cached bootstrap.
pub fn lazy_js_shim_path() -> miette::Result<PathBuf> {
    let shim_dir = tool_root()?.join("lazy-bin");
    write_lazy_shims(&shim_dir)?;
    Ok(shim_dir.join("node-gyp.js"))
}

/// Materialize aube's cached node-gyp installation and return its executable.
///
/// Aube's lazy `node-gyp` shims invoke the current executable with the private
/// `__node-gyp-bootstrap <project-dir>` command. For a standalone aube process
/// that executable is aube itself. An embedding host must intercept that
/// command before its own argument parser, call this function, and print the
/// returned path to stdout.
///
/// The bootstrap stays fully in-process and inherits the outer project's
/// registry configuration. Aube owns the cache layout, version selection, and
/// cross-process locking; the host does not need an `aube` or `npm` executable.
pub async fn bootstrap_node_gyp(project_dir: &Path) -> miette::Result<PathBuf> {
    let bin_dir = ensure_cached(project_dir).await?;
    node_gyp_binary(&bin_dir).ok_or_else(|| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_EMBED_INSTALL_FAILED,
            "node-gyp bootstrap completed but no executable exists in {}",
            bin_dir.display()
        )
    })
}

/// The `node-gyp` shell shim: resolves the real binary through the
/// hidden `__node-gyp-bootstrap` subcommand, then execs it.
///
/// `AUBE_NODE_GYP_PROJECT_DIR` is optional (cwd fallback, matching the `.js`
/// shim below), so it is expanded defensively — under `set -u` a bare
/// `$AUBE_NODE_GYP_PROJECT_DIR` aborts with "unbound variable" on any path
/// that doesn't set it. `AUBE_NODE_GYP_EXE` is the one hard requirement, and
/// it gets an explicit message rather than an exec of the empty string. There
/// is deliberately no fallback to a bare `node-gyp`: this file *is* the
/// `node-gyp` on PATH, so resolving that name again would re-exec the shim
/// forever.
const SH_SHIM: &str = r#"#!/usr/bin/env sh
set -eu
if [ -z "${AUBE_NODE_GYP_EXE:-}" ]; then
  echo "node-gyp shim invoked outside a lifecycle script (AUBE_NODE_GYP_EXE unset)" >&2
  exit 1
fi
real="$("$AUBE_NODE_GYP_EXE" __node-gyp-bootstrap "${AUBE_NODE_GYP_PROJECT_DIR:-$PWD}")"
exec "$real" "$@"
"#;

/// `node-gyp.js`: the value of `npm_config_node_gyp`. Consumers run it
/// as `node $npm_config_node_gyp …`, so it must be a Node script (not
/// the shell `node-gyp` shim above). It resolves the real node-gyp the
/// same way — via the hidden `__node-gyp-bootstrap` subcommand — then
/// forwards argv. Falls back to a `node-gyp` on PATH when aube's env
/// markers are absent (e.g. a script spawned outside aube's wrappers).
const JS_SHIM: &str = r#"#!/usr/bin/env node
"use strict";
// aube lazy node-gyp stand-in for npm_config_node_gyp. Resolves (and
// bootstraps on first use) aube's node-gyp, then forwards argv. Kept
// dependency-free; writing this file is free, the bootstrap only fires
// when something actually invokes it. Bare `require` (no `node:` prefix)
// so the shim runs under any Node the user drives, including pre-16.
const { execFileSync, spawnSync } = require("child_process");
const { existsSync } = require("fs");
const { join } = require("path");
const isWin = process.platform === "win32";
const cached = join(__dirname, "..", "__NODE_GYP_BUCKET__", "node_modules", "node-gyp", "bin", "node-gyp.js");
if (existsSync(cached)) {
  require(cached);
  return;
}
let real;
const exe = process.env.AUBE_NODE_GYP_EXE;
if (exe) {
  const dir = process.env.AUBE_NODE_GYP_PROJECT_DIR || process.cwd();
  real = execFileSync(exe, ["__node-gyp-bootstrap", dir], { encoding: "utf8" }).trim();
} else {
  const depth = Number(process.env.AUBE_NODE_GYP_SHIM_DEPTH || 0) + 1;
  if (depth > 3) {
    console.error("node-gyp shim re-entered without reaching a prepared node-gyp");
    process.exit(1);
  }
  process.env.AUBE_NODE_GYP_SHIM_DEPTH = String(depth);
  real = isWin ? "node-gyp.cmd" : "node-gyp";
}
const result = spawnSync(real, process.argv.slice(2), { stdio: "inherit", shell: isWin });
if (result.error) {
  console.error("aube: failed to run node-gyp (" + real + "): " + result.error.message);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
"#;

/// The Windows `node-gyp.cmd` shim. Two cmd.exe rules bite here, and both
/// were live bugs that never surfaced while the eager bootstrap kept a real
/// node-gyp on PATH:
///
/// 1. `for /f` runs its command through `cmd /c`, which STRIPS the outer
///    quote pair when the string both starts and ends with a quote. The
///    natural spelling therefore degrades to
///    `C:\...\nub.exe" __node-gyp-bootstrap "C:\...\proj` and dies with
///    "The filename, directory name, or volume label syntax is incorrect" —
///    measured on windows-latest, with and without spaces in the path.
///    Wrapping the whole command in one MORE quote pair makes that strip
///    leave exactly the intended string.
/// 2. An undefined `%VAR%` expands to its own literal text, so without the
///    fallback the bootstrap receives `%AUBE_NODE_GYP_PROJECT_DIR%` as a
///    path. `setlocal` keeps that fallback out of the caller's env.
#[cfg(windows)]
const CMD_SHIM: &str = r#"@echo off
setlocal
if not defined AUBE_NODE_GYP_EXE (
  echo node-gyp shim invoked outside a lifecycle script ^(AUBE_NODE_GYP_EXE unset^)>&2
  exit /b 1
)
if not defined AUBE_NODE_GYP_PROJECT_DIR set "AUBE_NODE_GYP_PROJECT_DIR=%CD%"
for /f "usebackq delims=" %%i in (`""%AUBE_NODE_GYP_EXE%" __node-gyp-bootstrap "%AUBE_NODE_GYP_PROJECT_DIR%""`) do set "AUBE_REAL_NODE_GYP=%%i"
if not defined AUBE_REAL_NODE_GYP exit /b 1
"%AUBE_REAL_NODE_GYP%" %*
"#;

/// Write one shim, skipping the write when the file on disk already
/// matches — see [`write_lazy_shims`] for why that matters.
///
/// The comparison reads through a single open handle and takes the mode
/// from that same handle's `fstat`, so a hit costs open + fstat + read +
/// close and touches nothing. A miss (absent, stale content, or an exec
/// bit that got stripped) falls through to the original
/// atomic-write-then-chmod, which is also what repairs the file.
fn write_shim_if_stale(path: &Path, contents: &str) -> miette::Result<()> {
    if shim_is_current(path, contents) {
        return Ok(());
    }
    // `atomic_write` creates the parent dir, so the fast path above can
    // skip `create_dir_all` entirely: a matching file proves the dir.
    aube_util::fs_atomic::atomic_write(path, contents.as_bytes()).into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(SHIM_MODE))
            .into_diagnostic()?;
    }
    Ok(())
}

#[cfg(unix)]
const SHIM_MODE: u32 = 0o755;

/// True when `path` already holds exactly `contents` and (on unix) is
/// still executable. Any error — missing file, permission trouble,
/// unreadable — reports "not current" so the caller rewrites it.
fn shim_is_current(path: &Path, contents: &str) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(meta) = f.metadata() else {
        return false;
    };
    if !meta.is_file() || meta.len() != contents.len() as u64 {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Compare only the permission bits; `st_mode` also carries the
        // file type, which `is_file` above has already vetted.
        if meta.permissions().mode() & 0o777 != SHIM_MODE {
            return false;
        }
    }
    let mut on_disk = Vec::with_capacity(contents.len());
    f.read_to_end(&mut on_disk).is_ok() && on_disk == contents.as_bytes()
}

/// Materialize the lazy shims into `shim_dir`.
///
/// Called on *every* `aube run` (twice — once for `PATH`, once for
/// `npm_config_node_gyp`) and once per dependency during install
/// lifecycle scripts, so the steady state has to be cheap. Each shim is
/// only rewritten when its on-disk copy differs, which keeps the common
/// case to a couple of small reads instead of a
/// create-dir + write-temp + rename + chmod per file per invocation.
/// Content-addressed rather than pinned, so a shipped shim fix still
/// self-heals on the first run of the new binary — the bytes change, the
/// comparison misses, and the file is rewritten.
///
/// Not writing unless the content changed also stops concurrent
/// lifecycle jobs from renaming over each other's shims, and stops the
/// interrupted-write temp files from accumulating in the cache dir.
fn write_lazy_shims(shim_dir: &Path) -> miette::Result<()> {
    write_shim_if_stale(&shim_dir.join("node-gyp"), SH_SHIM)?;
    write_shim_if_stale(&shim_dir.join("node-gyp.js"), &js_shim())?;
    #[cfg(windows)]
    write_shim_if_stale(&shim_dir.join("node-gyp.cmd"), CMD_SHIM)?;
    Ok(())
}

fn js_shim() -> String {
    JS_SHIM.replace("__NODE_GYP_BUCKET__", BUCKET)
}

/// Materialize the synthetic single-package project that the bootstrap
/// install runs against. Writes are atomic and idempotent, so racing
/// processes converge on the same content; serialization is the caller's
/// project lock on `tool_dir`.
fn write_bootstrap_project(tool_dir: &Path, project_npmrc: &Path) -> miette::Result<()> {
    std::fs::create_dir_all(tool_dir).into_diagnostic()?;
    // The scratch manifest lands in the embedder's own cache tree, so even its
    // `name` follows the active brand rather than hardcoding the engine's.
    let manifest = format!(
        r#"{{"name":"{tool}-tool-node-gyp","private":true,"dependencies":{{"node-gyp":"{SPEC}"}}}}"#,
        tool = aube_util::prog()
    );
    aube_util::fs_atomic::atomic_write(&tool_dir.join("package.json"), manifest.as_bytes())
        .into_diagnostic()?;
    // Pin the bootstrap install to `tool_dir` so its workspace-root
    // walk-up stops here instead of escaping upward. `tool_dir` lives under `$XDG_CACHE_HOME/aube/tools/` —
    // i.e. inside the user's HOME and inside any test temp dir set
    // via `HOME=$TEST_TEMP_DIR`. Without this stub yaml,
    // `find_workspace_root` would walk past `$XDG_CACHE_HOME`,
    // discover the outer project's `pnpm-workspace.yaml`, and run
    // the bootstrap install against the *outer* tree — taking, and
    // deadlocking on, the project lock the outer install already holds. Any `pnpm-workspace.yaml`
    // is a hard boundary, so the empty stub hits the first marker
    // check at the start of the walk, returns `tool_dir`, and the
    // install runs as a single-package install (`workspace_packages`
    // is empty so `has_workspace` is false).
    // Use whichever workspace-yaml name this tool's discovery recognizes
    // first (its branded YAML, or the shared `pnpm-workspace.yaml`).
    let marker = aube_manifest::workspace::workspace_yaml_names()
        .first()
        .copied()
        .unwrap_or("pnpm-workspace.yaml");
    aube_util::fs_atomic::atomic_write(&tool_dir.join(marker), b"").into_diagnostic()?;
    // Forward the outer project's `.npmrc` so private registries and
    // auth tokens configured at project scope carry through to the
    // bootstrap install. It resolves against `tool_dir`, so without
    // this copy its `.npmrc` walk would only ever see `~/.npmrc`. Overwrite on every bootstrap so a user updating
    // their project `.npmrc` between runs picks up fresh config;
    // delete the stale copy if the project no longer has one.
    let tool_npmrc = tool_dir.join(".npmrc");
    if project_npmrc.exists() {
        std::fs::copy(project_npmrc, &tool_npmrc)
            .into_diagnostic()
            .wrap_err_with(|| {
                format!(
                    "failed to propagate {} to node-gyp bootstrap dir",
                    project_npmrc.display()
                )
            })?;
    } else if tool_npmrc.exists() {
        let _ = std::fs::remove_file(&tool_npmrc);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_config_cleanup_is_idempotent_and_does_not_hide_failure() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(".npmrc");
        std::fs::write(&config, "//registry.invalid/:_authToken=fixture").unwrap();
        remove_legacy_registry_config(root.path()).unwrap();
        assert!(!config.exists());
        remove_legacy_registry_config(root.path()).unwrap();
        std::fs::create_dir(&config).unwrap();
        assert!(remove_legacy_registry_config(root.path()).is_err());
    }

    #[test]
    fn cached_js_shim_uses_the_terminal_tool_without_reentering_the_installer() {
        let root = tempfile::tempdir().unwrap();
        let shim_dir = root.path().join("lazy-bin");
        std::fs::create_dir(&shim_dir).unwrap();
        write_lazy_shims(&shim_dir).unwrap();
        let real_dir = root.path().join(BUCKET).join("node_modules/node-gyp/bin");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(
            real_dir.join("node-gyp.js"),
            "require('fs').writeFileSync(process.argv[2], process.argv[3]);",
        )
        .unwrap();
        let marker = root.path().join("marker");
        let output = std::process::Command::new("node")
            .arg(shim_dir.join("node-gyp.js"))
            .arg(&marker)
            .arg("terminal-tool-ran")
            .env("AUBE_NODE_GYP_EXE", root.path().join("must-not-execute"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            "terminal-tool-ran"
        );
    }

    #[test]
    fn a_leftover_bin_shim_does_not_make_a_missing_tool_usable() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(BINARY_NAMES[0]), "shim").unwrap();
        assert!(!tool_tree_usable(root.path(), &bin));
        let package = root.path().join("node_modules/node-gyp");
        std::fs::create_dir(&package).unwrap();
        std::fs::write(package.join("package.json"), "{}").unwrap();
        assert!(tool_tree_usable(root.path(), &bin));
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aube-gyp-shim-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The shims land even though nothing created `shim_dir` first —
    /// the entry points rely on `atomic_write` for that.
    #[test]
    fn writes_shims_into_a_missing_dir() {
        let dir = tempdir().join("lazy-bin");
        write_lazy_shims(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("node-gyp")).unwrap(),
            SH_SHIM
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("node-gyp.js")).unwrap(),
            js_shim()
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// The point of the change: a second call with the content already
    /// in place must not touch the files.
    #[test]
    fn repeat_calls_do_not_rewrite() {
        let dir = tempdir().join("lazy-bin");
        write_lazy_shims(&dir).unwrap();
        let sh = dir.join("node-gyp");
        let js = dir.join("node-gyp.js");
        let before = (
            std::fs::metadata(&sh).unwrap().modified().unwrap(),
            std::fs::metadata(&js).unwrap().modified().unwrap(),
        );
        assert!(shim_is_current(&sh, SH_SHIM));
        assert!(shim_is_current(&js, &js_shim()));

        write_lazy_shims(&dir).unwrap();

        let after = (
            std::fs::metadata(&sh).unwrap().modified().unwrap(),
            std::fs::metadata(&js).unwrap().modified().unwrap(),
        );
        assert_eq!(before, after, "shims were rewritten despite matching bytes");
        // No temp files left behind either.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(strays.is_empty(), "left temp files behind: {strays:?}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// Self-heal: a shim whose bytes drifted (an older aube shipped
    /// different content) is rewritten rather than pinned.
    #[test]
    fn stale_content_is_rewritten() {
        let dir = tempdir().join("lazy-bin");
        write_lazy_shims(&dir).unwrap();
        let sh = dir.join("node-gyp");
        std::fs::write(&sh, "#!/usr/bin/env sh\necho from an older aube\n").unwrap();
        assert!(!shim_is_current(&sh, SH_SHIM));

        write_lazy_shims(&dir).unwrap();

        assert_eq!(std::fs::read_to_string(&sh).unwrap(), SH_SHIM);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// Same-length-but-different content must not slip through the
    /// length pre-check.
    #[test]
    fn same_length_different_bytes_is_not_current() {
        let dir = tempdir().join("lazy-bin");
        write_lazy_shims(&dir).unwrap();
        let sh = dir.join("node-gyp");
        let mut drifted = SH_SHIM.as_bytes().to_vec();
        *drifted.last_mut().unwrap() = b' ';
        std::fs::write(&sh, &drifted).unwrap();
        assert_eq!(drifted.len(), SH_SHIM.len());
        assert!(!shim_is_current(&sh, SH_SHIM));

        write_lazy_shims(&dir).unwrap();

        assert_eq!(std::fs::read_to_string(&sh).unwrap(), SH_SHIM);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn missing_file_is_not_current() {
        let dir = tempdir();
        assert!(!shim_is_current(&dir.join("nope"), SH_SHIM));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A shim that lost its exec bit is still repaired — the mode is
    /// part of "current", not just the bytes.
    #[cfg(unix)]
    #[test]
    fn stripped_exec_bit_is_restored() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().join("lazy-bin");
        write_lazy_shims(&dir).unwrap();
        let sh = dir.join("node-gyp");
        assert_eq!(
            std::fs::metadata(&sh).unwrap().permissions().mode() & 0o777,
            SHIM_MODE
        );
        std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!shim_is_current(&sh, SH_SHIM));

        write_lazy_shims(&dir).unwrap();

        assert_eq!(
            std::fs::metadata(&sh).unwrap().permissions().mode() & 0o777,
            SHIM_MODE
        );
        assert_eq!(std::fs::read_to_string(&sh).unwrap(), SH_SHIM);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// A directory sitting where the shim belongs must not read as
    /// current (and must not panic).
    #[test]
    fn directory_in_the_way_is_not_current() {
        let dir = tempdir();
        let path = dir.join("node-gyp");
        std::fs::create_dir_all(&path).unwrap();
        assert!(!shim_is_current(&path, SH_SHIM));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn resolves_exe_only_cache() {
        let dir = tempdir();
        let exe = dir.join("node-gyp.exe");
        std::fs::write(&exe, b"").unwrap();

        assert_eq!(node_gyp_binary(&dir), Some(exe.clone()));
        assert!(exe.is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn live_decoded_wrapper_wins_over_an_earlier_stale_wrapper() {
        let dir = tempdir();
        let target = dir.join("target/node-gyp.js");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "#!/usr/bin/env node\n").unwrap();
        aube_linker::create_bin_shim(
            &dir,
            "node-gyp",
            &target,
            aube_linker::BinShimOptions::default(),
        )
        .unwrap();
        std::fs::remove_file(target).unwrap();
        let live_target = dir.join("live.js");
        std::fs::write(&live_target, "#!/usr/bin/env node\n").unwrap();
        let shell_wrapper = dir.join("node-gyp");
        std::fs::write(
            &shell_wrapper,
            format!(
                "#!/bin/sh\n{}live.js\n",
                aube_linker::sys::POSIX_SHIM_MARKER_PREFIX
            ),
        )
        .unwrap();

        assert_eq!(cached_node_gyp_binary(&dir), Some(shell_wrapper));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cached_wrapper_requires_its_target() {
        let dir = tempdir();
        let bin_dir = dir.join("node_modules/.bin");
        let target = dir.join("node_modules/node-gyp/bin/node-gyp.js");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "#!/usr/bin/env node\n").unwrap();
        aube_linker::create_bin_shim(
            &bin_dir,
            "node-gyp",
            &target,
            aube_linker::BinShimOptions {
                prefer_symlinked_executables: Some(false),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(node_gyp_bin_exists(&bin_dir));
        assert!(cached_node_gyp_binary(&bin_dir).is_some());

        std::fs::remove_file(target).unwrap();
        assert!(
            node_gyp_bin_exists(&bin_dir),
            "the wrapper itself should still exist"
        );
        assert!(cached_node_gyp_binary(&bin_dir).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
