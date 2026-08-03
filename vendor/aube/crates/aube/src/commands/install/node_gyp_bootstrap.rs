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
//! User precedence: if `node-gyp` is already resolvable on the
//! ambient `PATH` (system install, nvm, a shim in a test fixture),
//! [`ensure`] returns `None` and we stay out of the way — the user's
//! copy wins. Otherwise node-gyp is installed under
//! `<cache_dir>/tools/node-gyp/<bucket>/` and the returned `.bin`
//! dir is prepended to the lifecycle script's `PATH` *after* the
//! dep's own `.bin`.
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
//! The outer project's `.npmrc` (if any) is copied into the tool dir
//! as its own project-level `.npmrc` so private-registry URLs and
//! auth tokens configured by monorepo / enterprise setups flow
//! through to the bootstrap install, which resolves against the tool
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
    BINARY_NAMES.iter().any(|name| bin_dir.join(name).exists())
}

fn primary_binary_name() -> &'static str {
    BINARY_NAMES[0]
}

fn tool_root() -> miette::Result<PathBuf> {
    let cache = aube_store::dirs::cache_dir()
        .ok_or_else(|| miette!("could not resolve cache dir for node-gyp bootstrap"))?;
    Ok(cache.join("tools").join("node-gyp"))
}

/// Install (or reuse) the pinned node-gyp under the tool dir and return its
/// `.bin`. Reached only through the lazy shims — nothing bootstraps eagerly,
/// so an install whose builds never invoke node-gyp never pays for this.
///
/// `project_dir` is the outer install's project root; its `.npmrc`
/// (if any) is propagated to the tool dir so the bootstrap inherits
/// the same registry/auth configuration.
pub async fn ensure_cached(project_dir: &Path) -> miette::Result<PathBuf> {
    let root = tool_root()?;
    let tool_dir = root.join(BUCKET);
    let bin_dir = tool_dir.join("node_modules").join(".bin");
    if node_gyp_bin_exists(&bin_dir) {
        return Ok(bin_dir);
    }
    let tool_dir_blocking = tool_dir.clone();
    let project_npmrc = project_dir.join(".npmrc");
    tokio::task::spawn_blocking(move || {
        write_bootstrap_project(&tool_dir_blocking, &project_npmrc)
    })
    .await
    .into_diagnostic()
    .wrap_err("node-gyp bootstrap task panicked")??;

    // The tool dir is its own single-package project (see the stub workspace
    // yaml written above), so this lock is keyed on `tool_dir` and cannot
    // contend with the outer install's lock on the real project.
    let lock = crate::commands::take_project_lock(&tool_dir)?;
    // Re-check under the lock: another process may have raced us between the
    // check above and acquisition.
    if node_gyp_bin_exists(&bin_dir) {
        return Ok(bin_dir);
    }

    tracing::info!("bootstrapping node-gyp {SPEC} into {}", tool_dir.display());
    let mut opts = super::InstallOptions::with_mode(super::FrozenMode::Prefer);
    // Equivalent of the `install --ignore-scripts --silent` this used to shell
    // out for. node-gyp's own dependency tree needs no build scripts, and
    // running them here would recurse straight back into this bootstrap.
    opts.ignore_scripts = true;
    opts.control = super::InstallControl::silent();
    super::run_with_project_lock(opts, &lock)
        .await
        .wrap_err_with(|| {
            format!(
                "failed to bootstrap node-gyp {SPEC} into {} — \
                 pre-populate it or run `{}` once while online",
                tool_dir.display(),
                aube_util::cmd("install")
            )
        })?;

    if !node_gyp_bin_exists(&bin_dir) {
        return Err(miette!(
            "node-gyp bootstrap into {} reported success but left no node-gyp binary in {}",
            tool_dir.display(),
            bin_dir.display()
        ));
    }
    Ok(bin_dir)
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
/// This covers the PATH channel only. `npm_config_node_gyp` is a separate one:
/// [`lazy_js_shim_path`] is stamped unconditionally and jail-unaware, so a
/// consumer reading that variable still re-enters from inside the jail and hits
/// the same empty tool dir. That is pre-existing rather than new — the variable
/// already pointed at the lazy shim — but it is the half this function does not
/// close.
///
/// `None` when node-gyp already resolves without us, same as the lazy path.
pub(crate) async fn ensure_bin_dir_for_jail(
    project_bin_dir: &Path,
    project_dir: &Path,
) -> miette::Result<Option<PathBuf>> {
    if node_gyp_bin_exists(project_bin_dir) || node_gyp_on_path() {
        return Ok(None);
    }
    ensure_cached(project_dir).await.map(Some)
}

pub(crate) fn lazy_shim_bin_dir(project_bin_dir: &Path) -> miette::Result<Option<PathBuf>> {
    if node_gyp_bin_exists(project_bin_dir) || node_gyp_on_path() {
        return Ok(None);
    }
    let shim_dir = tool_root()?.join("lazy-bin");
    std::fs::create_dir_all(&shim_dir).into_diagnostic()?;
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
/// `node $npm_config_node_gyp`. Rewritten on every call (like
/// [`lazy_shim_bin_dir`]) so a shipped shim fix self-heals rather than
/// being pinned to whatever first landed in the cache.
pub fn lazy_js_shim_path() -> miette::Result<PathBuf> {
    let shim_dir = tool_root()?.join("lazy-bin");
    std::fs::create_dir_all(&shim_dir).into_diagnostic()?;
    write_lazy_shims(&shim_dir)?;
    Ok(shim_dir.join("node-gyp.js"))
}

/// `pub` so an embedder driving the lazy node-gyp shim re-entry (its own
/// `current_exe()` is what the shim execs) can print the bootstrapped binary
/// path. Pairs with the `pub`-widened [`ensure_cached`]; standalone aube is
/// unaffected.
pub async fn print_bootstrapped_binary(project_dir: &Path) -> miette::Result<()> {
    let bin_dir = ensure_cached(project_dir).await?;
    println!("{}", bin_dir.join(primary_binary_name()).display());
    Ok(())
}

fn write_lazy_shims(shim_dir: &Path) -> miette::Result<()> {
    // `AUBE_NODE_GYP_PROJECT_DIR` is optional (cwd fallback, matching the
    // `.js` shim below), so it must be expanded defensively — under
    // `set -u` a bare `$AUBE_NODE_GYP_PROJECT_DIR` aborts with "unbound
    // variable" on any path that doesn't set it. `AUBE_NODE_GYP_EXE` is
    // the one hard requirement, and it gets an explicit message rather
    // than an exec of the empty string. There is deliberately no fallback
    // to a bare `node-gyp`: this file *is* the `node-gyp` on PATH, so
    // resolving that name again would re-exec the shim forever.
    let sh = r#"#!/usr/bin/env sh
set -eu
if [ -z "${AUBE_NODE_GYP_EXE:-}" ]; then
  echo "node-gyp shim invoked outside a lifecycle script (AUBE_NODE_GYP_EXE unset)" >&2
  exit 1
fi
real="$("$AUBE_NODE_GYP_EXE" __node-gyp-bootstrap "${AUBE_NODE_GYP_PROJECT_DIR:-$PWD}")"
exec "$real" "$@"
"#;
    let sh_path = shim_dir.join("node-gyp");
    aube_util::fs_atomic::atomic_write(&sh_path, sh.as_bytes()).into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sh_path, std::fs::Permissions::from_mode(0o755))
            .into_diagnostic()?;
    }

    // `node-gyp.js`: the value of `npm_config_node_gyp`. Consumers run it
    // as `node $npm_config_node_gyp …`, so it must be a Node script (not
    // the shell `node-gyp` shim above). It resolves the real node-gyp the
    // same way — via the hidden `__node-gyp-bootstrap` subcommand — then
    // forwards argv. Falls back to a `node-gyp` on PATH when aube's env
    // markers are absent (e.g. a script spawned outside aube's wrappers).
    let js = r#"#!/usr/bin/env node
"use strict";
// aube lazy node-gyp stand-in for npm_config_node_gyp. Resolves (and
// bootstraps on first use) aube's node-gyp, then forwards argv. Kept
// dependency-free; writing this file is free, the bootstrap only fires
// when something actually invokes it. Bare `require` (no `node:` prefix)
// so the shim runs under any Node the user drives, including pre-16.
const { execFileSync, spawnSync } = require("child_process");
const isWin = process.platform === "win32";
let real;
const exe = process.env.AUBE_NODE_GYP_EXE;
if (exe) {
  const dir = process.env.AUBE_NODE_GYP_PROJECT_DIR || process.cwd();
  real = execFileSync(exe, ["__node-gyp-bootstrap", dir], { encoding: "utf8" }).trim();
} else {
  real = isWin ? "node-gyp.cmd" : "node-gyp";
}
const result = spawnSync(real, process.argv.slice(2), { stdio: "inherit", shell: isWin });
if (result.error) {
  console.error("aube: failed to run node-gyp (" + real + "): " + result.error.message);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
"#;
    let js_path = shim_dir.join("node-gyp.js");
    aube_util::fs_atomic::atomic_write(&js_path, js.as_bytes()).into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&js_path, std::fs::Permissions::from_mode(0o755))
            .into_diagnostic()?;
    }

    #[cfg(windows)]
    {
        // Two cmd.exe rules bite here, and both were live bugs that never
        // surfaced while the eager bootstrap kept a real node-gyp on PATH:
        //
        // 1. `for /f` runs its command through `cmd /c`, which STRIPS the outer
        //    quote pair when the string both starts and ends with a quote. The
        //    natural spelling therefore degrades to
        //    `C:\...\nub.exe" __node-gyp-bootstrap "C:\...\proj` and dies with
        //    "The filename, directory name, or volume label syntax is
        //    incorrect" — measured on windows-latest, with and without spaces
        //    in the path. Wrapping the whole command in one MORE quote pair
        //    makes that strip leave exactly the intended string.
        // 2. An undefined `%VAR%` expands to its own literal text, so without
        //    the fallback the bootstrap receives `%AUBE_NODE_GYP_PROJECT_DIR%`
        //    as a path. `setlocal` keeps that fallback out of the caller's env.
        let cmd = r#"@echo off
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
        aube_util::fs_atomic::atomic_write(&shim_dir.join("node-gyp.cmd"), cmd.as_bytes())
            .into_diagnostic()?;
    }

    Ok(())
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
