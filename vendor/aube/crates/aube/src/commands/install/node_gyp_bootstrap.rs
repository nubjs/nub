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
//! The install is performed by recursively invoking the current aube
//! binary with `install --ignore-scripts` inside a freshly-written
//! `package.json` that pins node-gyp. The outer project's `.npmrc`
//! (if any) is copied into the tool dir as its own project-level
//! `.npmrc` so private-registry URLs and auth tokens configured by
//! monorepo / enterprise setups flow through to the recursive
//! install — the subprocess's cwd is the tool dir, which would
//! otherwise only pick up `~/.npmrc`. An `xx::fslock` lock keyed
//! off the tool dir serializes concurrent bootstraps across
//! processes; the fast-path existence check short-circuits every
//! subsequent invocation.
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

/// Returns `Some(bin_dir)` containing a freshly-bootstrapped `node-gyp`
/// when the ambient `PATH` doesn't already provide one, or `None` when
/// the user already has a copy on `PATH` — in which case we don't
/// touch their setup.
///
/// `project_dir` is the outer install's project root; its `.npmrc`
/// (if any) is propagated to the tool dir so the bootstrap inherits
/// the same registry/auth configuration.
pub async fn ensure(project_dir: &Path) -> miette::Result<Option<PathBuf>> {
    if node_gyp_on_path() {
        return Ok(None);
    }
    ensure_cached(project_dir).await.map(Some)
}

pub async fn ensure_cached(project_dir: &Path) -> miette::Result<PathBuf> {
    let root = tool_root()?;
    let tool_dir = root.join(BUCKET);
    let bin_dir = tool_dir.join("node_modules").join(".bin");
    if node_gyp_bin_exists(&bin_dir) {
        return Ok(bin_dir);
    }
    let lock_key = root.join(format!("{BUCKET}.lock"));
    let tool_dir_blocking = tool_dir.clone();
    let bin_dir_blocking = bin_dir.clone();
    let project_npmrc = project_dir.join(".npmrc");
    tokio::task::spawn_blocking(move || {
        bootstrap_blocking(
            &lock_key,
            &tool_dir_blocking,
            &bin_dir_blocking,
            &project_npmrc,
        )
    })
    .await
    .into_diagnostic()
    .wrap_err("node-gyp bootstrap task panicked")??;
    Ok(bin_dir)
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
    let sh = r#"#!/usr/bin/env sh
set -eu
real="$("$AUBE_NODE_GYP_EXE" __node-gyp-bootstrap "$AUBE_NODE_GYP_PROJECT_DIR")"
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
        let cmd = r#"@echo off
for /f "usebackq delims=" %%i in (`"%AUBE_NODE_GYP_EXE%" __node-gyp-bootstrap "%AUBE_NODE_GYP_PROJECT_DIR%"`) do set "AUBE_REAL_NODE_GYP=%%i"
if not defined AUBE_REAL_NODE_GYP exit /b 1
"%AUBE_REAL_NODE_GYP%" %*
"#;
        aube_util::fs_atomic::atomic_write(&shim_dir.join("node-gyp.cmd"), cmd.as_bytes())
            .into_diagnostic()?;
    }

    Ok(())
}

fn bootstrap_blocking(
    lock_key: &Path,
    tool_dir: &Path,
    bin_dir: &Path,
    project_npmrc: &Path,
) -> miette::Result<()> {
    std::fs::create_dir_all(tool_dir).into_diagnostic()?;
    let _lock = xx::fslock::FSLock::new(lock_key)
        .with_callback(|_| {
            tracing::info!("waiting for another aube process to finish bootstrapping node-gyp");
        })
        .lock()
        .map_err(|e| miette!("failed to acquire node-gyp bootstrap lock: {e}"))?;
    // Re-check under the lock: another process may have raced us.
    if node_gyp_bin_exists(bin_dir) {
        return Ok(());
    }
    let manifest = format!(
        r#"{{"name":"aube-tool-node-gyp","private":true,"dependencies":{{"node-gyp":"{SPEC}"}}}}"#
    );
    aube_util::fs_atomic::atomic_write(&tool_dir.join("package.json"), manifest.as_bytes())
        .into_diagnostic()?;
    // Pin the recursive `aube install` invocation below to `tool_dir`
    // so its workspace-root walk-up stops here instead of escaping
    // upward. `tool_dir` lives under `$XDG_CACHE_HOME/aube/tools/` —
    // i.e. inside the user's HOME and inside any test temp dir set
    // via `HOME=$TEST_TEMP_DIR`. Without this stub yaml,
    // `find_workspace_root` would walk past `$XDG_CACHE_HOME`,
    // discover the outer project's `pnpm-workspace.yaml`, and run
    // the recursive install against the *outer* tree — deadlocking
    // on the outer process's project lock. Any `pnpm-workspace.yaml`
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
    // recursive install. The subprocess's cwd is `tool_dir`, so
    // without this copy its `.npmrc` walk would only ever see
    // `~/.npmrc`. Overwrite on every bootstrap so a user updating
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
    let exe = std::env::current_exe()
        .into_diagnostic()
        .wrap_err("could not locate current aube executable for node-gyp bootstrap")?;
    tracing::info!("bootstrapping node-gyp {SPEC} into {}", tool_dir.display());
    let status = std::process::Command::new(&exe)
        .args(["install", "--ignore-scripts", "--silent"])
        .current_dir(tool_dir)
        .status()
        .into_diagnostic()
        .wrap_err(format!(
            "failed to spawn recursive {} for node-gyp bootstrap",
            aube_util::cmd("install")
        ))?;
    if !status.success() {
        return Err(miette!(
            "recursive {} failed while bootstrapping node-gyp (exit {}) — \
             pre-populate {} or run `{}` once while online",
            aube_util::cmd("install"),
            aube_scripts::exit_code_from_status(status),
            tool_dir.display(),
            aube_util::cmd("install")
        ));
    }
    if !node_gyp_bin_exists(bin_dir) {
        return Err(miette!(
            "node-gyp bootstrap completed but no shim found under {}",
            bin_dir.display()
        ));
    }
    Ok(())
}
