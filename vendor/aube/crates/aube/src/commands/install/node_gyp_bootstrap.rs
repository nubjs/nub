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
//! dep's own `.bin`. User precedence holds only for a lifecycle
//! script that can actually reach the ambient `PATH` — see
//! [`ScriptReach`].
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
//! processes; the [`tool_tree_usable`] fast path short-circuits every
//! subsequent invocation.
use miette::{IntoDiagnostic, WrapErr, miette};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Major-version pin. Bumping the bucket invalidates the cache and
/// triggers a re-bootstrap on the next install.
const BUCKET: &str = "v12";
/// Semver range passed to `aube install`. Keep aligned with `BUCKET`.
const SPEC: &str = "^12.0.0";
/// The bootstrapped package's name — also the tool dir's require root
/// (`node_modules/<PKG>`), which is what [`tool_tree_usable`] probes.
const PKG: &str = "node-gyp";

#[cfg(windows)]
const BINARY_NAMES: &[&str] = &["node-gyp.cmd", "node-gyp.exe", "node-gyp"];
#[cfg(not(windows))]
const BINARY_NAMES: &[&str] = &["node-gyp"];

/// Whether the lifecycle scripts this bootstrap feeds can reach the ambient
/// `PATH` at all.
///
/// The user-precedence rule is only sound when the script that consumes
/// node-gyp inherits the caller's `PATH` *and* can read what it points at.
/// Under an embedder that confines dependency lifecycle scripts, neither
/// holds: the probe runs unconfined while the script runs confined, so
/// deferring to an ambient node-gyp hands the script a path its sandbox never
/// granted — the script sees `node-gyp: command not found` (or a denial) and
/// every native compile fails. A confinement decision made by probing state
/// the confined child cannot observe is incoherent, so confined callers always
/// take aube's own copy, which lands under the cache dir the sandbox does
/// grant. Widening the sandbox to admit whichever node-gyp happened to be on
/// the host's `PATH` was the alternative and is strictly worse: it makes the
/// read set a function of arbitrary host state, where this is deterministic on
/// a clean machine, a dev box, and CI alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptReach {
    /// The spawn inherits the caller's `PATH` unconfined, so a node-gyp found
    /// there is genuinely usable and the user's copy wins.
    AmbientPath,
    /// The spawn is confined by the embedder's sandbox.
    Confined,
}

impl ScriptReach {
    /// Read off the same embedder flag that routes dependency lifecycle spawns
    /// through the embedder's sandbox, so the two can never disagree.
    /// Standalone aube leaves it false and keeps user precedence unchanged.
    pub fn for_active_embedder() -> Self {
        if aube_util::embedder().embedder_owns_lifecycle_sandbox {
            Self::Confined
        } else {
            Self::AmbientPath
        }
    }
}

/// Split out from the `PATH` lookup so the reach gate is testable without
/// mutating the process environment — a global whose mutation makes a
/// parallel test suite order-dependent.
fn ambient_node_gyp_wins(reach: ScriptReach, path: Option<&OsStr>) -> bool {
    reach == ScriptReach::AmbientPath && path.is_some_and(node_gyp_in_path)
}

fn node_gyp_in_path(path: &OsStr) -> bool {
    std::env::split_paths(path).any(|dir| node_gyp_bin_exists(&dir))
}

fn node_gyp_on_path() -> bool {
    std::env::var_os("PATH").is_some_and(|path| node_gyp_in_path(&path))
}

/// True if `bin_dir` contains any of the platform's accepted
/// `node-gyp` shim filenames. On Windows npm installs `node-gyp.cmd`
/// (sometimes `.exe` alongside), so a bare-string check would always
/// miss the bootstrapped shim and the fast-path would never fire.
pub(crate) fn node_gyp_bin_exists(bin_dir: &Path) -> bool {
    BINARY_NAMES.iter().any(|name| bin_dir.join(name).exists())
}

/// True when a previously-bootstrapped tool tree is still *usable*, as
/// opposed to merely present.
///
/// The `.bin` entry is a generated shim — on every platform a real file
/// (POSIX `sh` script, Windows `.cmd`) whose existence says nothing
/// about whether the package it execs is still on disk. The shim's
/// target and the tool dir's `node_modules/<PKG>` require root both
/// resolve through the same virtual-store symlink, so purging the
/// global store — a routine disk-reclaim operation — leaves the shim
/// standing and the target gone. Probing the require root is what makes
/// the bootstrap re-run instead of handing back a path that dies at
/// exec with `Cannot find module`. `exists()` follows symlinks, and
/// `package.json` is the same materialization marker
/// `state::verify_install_layout` uses, so this holds for the isolated
/// and hoisted layouts alike.
fn tool_tree_usable(tool_dir: &Path, bin_dir: &Path) -> bool {
    node_gyp_bin_exists(bin_dir)
        && tool_dir
            .join("node_modules")
            .join(PKG)
            .join("package.json")
            .exists()
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
/// touch their setup. A [`ScriptReach::Confined`] caller never defers to
/// the ambient copy, since its scripts cannot reach it.
///
/// `project_dir` is the outer install's project root; its `.npmrc`
/// (if any) is propagated to the tool dir so the bootstrap inherits
/// the same registry/auth configuration.
pub async fn ensure(project_dir: &Path, reach: ScriptReach) -> miette::Result<Option<PathBuf>> {
    if ambient_node_gyp_wins(reach, std::env::var_os("PATH").as_deref()) {
        return Ok(None);
    }
    ensure_cached(project_dir).await.map(Some)
}

pub async fn ensure_cached(project_dir: &Path) -> miette::Result<PathBuf> {
    let root = tool_root()?;
    let tool_dir = root.join(BUCKET);
    let bin_dir = tool_dir.join("node_modules").join(".bin");
    if tool_tree_usable(&tool_dir, &bin_dir) {
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

/// The ALREADY-BOOTSTRAPPED node-gyp's own JS entry point, or `None` when no
/// usable tool tree is on disk. Never bootstraps and never writes — a pure
/// lookup, safe to call from a synchronous spawn path.
///
/// This is the value [`lazy_js_shim_path`] stands in for. An embedder that
/// confines lifecycle scripts has already forced the bootstrap
/// ([`ScriptReach::Confined`]), so it can hand the script the real node-gyp
/// directly and skip the shim's two resolution channels — the
/// `AUBE_NODE_GYP_EXE` trampoline (an exec of the PM binary, which a sandbox
/// need not grant) and the bare-`node-gyp`-on-PATH fallback (which loses to
/// whatever an intermediate `npm run` prepends).
///
/// TWO invariants, and the value needs BOTH because npm's `node-gyp` stub has
/// been spelled two ways and nub does not choose which npm a project runs. It
/// must be node-gyp's own `bin/node-gyp.js` — npm ≥7's `@npmcli/run-script` stub
/// is `node "$npm_config_node_gyp" "$@"`, which a `.bin` shell shim would fail
/// as a syntax error — AND that file must keep its exec bit and `#!/usr/bin/env
/// node` shebang, because npm ≤6 unshifted its own `bin/node-gyp-bin` stub,
/// which execs `"$npm_config_node_gyp"` DIRECTLY. Satisfying only one spelling
/// breaks the other half of the ecosystem, and it breaks silently: the stub that
/// fails is chosen by the consumer's npm, not by anything visible here.
/// Standalone aube never calls this.
pub fn cached_js_entry() -> miette::Result<Option<PathBuf>> {
    Ok(js_entry_in(&tool_root()?.join(BUCKET)))
}

/// Split from [`cached_js_entry`] so the layout contract is testable against a
/// staged tree rather than the caller's real cache dir.
fn js_entry_in(tool_dir: &Path) -> Option<PathBuf> {
    let bin_dir = tool_dir.join("node_modules").join(".bin");
    if !tool_tree_usable(tool_dir, &bin_dir) {
        return None;
    }
    let entry = tool_dir
        .join("node_modules")
        .join(PKG)
        .join("bin")
        .join("node-gyp.js");
    entry.exists().then_some(entry)
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
    // markers are absent (e.g. a script spawned outside aube's wrappers) —
    // bounded by a re-entry counter, because that PATH lookup can resolve
    // back to this shim and recurse without limit (see the fallback below).
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
  // The PATH fallback can resolve back to THIS shim, and then it never terminates:
  // npm's run-script prepends its own node-gyp bin dir to PATH *and* points
  // npm_config_node_gyp here, so a bare `node-gyp` finds npm's stub, which re-execs
  // this file, which falls back to a bare `node-gyp` again. Every hop holds a live
  // synchronous spawnSync, so the cycle consumes memory rather than CPU and is not
  // self-limiting (measured under nub's build jail: ~1500 processes, 15 GB, load ~1).
  // Bound the RE-ENTRY, not any single build's nesting: a genuine nested native build
  // is a couple of hops deep, a cycle is unbounded.
  const depth = Number(process.env.AUBE_NODE_GYP_SHIM_DEPTH || 0) + 1;
  if (depth > 3) {
    console.error(
      "aube: node-gyp shim re-entered " + depth + " times without reaching a real " +
      "node-gyp — `node-gyp` on PATH resolves back to this shim. Refusing to recurse."
    );
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
    if tool_tree_usable(tool_dir, bin_dir) {
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
    if !tool_tree_usable(tool_dir, bin_dir) {
        return Err(miette!(
            "node-gyp bootstrap completed but left no usable tree under {} \
             (missing shim in {}, or an unresolvable {PKG} require root)",
            tool_dir.display(),
            bin_dir.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A directory holding a file named like a real `node-gyp` shim, so the
    /// probe resolves it exactly as it would a user's install.
    fn dir_with_node_gyp() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(primary_binary_name()), b"").expect("write shim");
        dir
    }

    #[test]
    fn an_ambient_node_gyp_wins_for_an_unconfined_script() {
        let dir = dir_with_node_gyp();
        assert!(ambient_node_gyp_wins(
            ScriptReach::AmbientPath,
            Some(dir.path().as_os_str())
        ));
    }

    /// The defect this gate exists for: the probe runs unconfined, the script
    /// runs confined, so an ambient hit must NOT suppress the bootstrap — the
    /// script would resolve nothing and every native compile would fail.
    #[test]
    fn an_ambient_node_gyp_is_ignored_for_a_confined_script() {
        let dir = dir_with_node_gyp();
        assert!(!ambient_node_gyp_wins(
            ScriptReach::Confined,
            Some(dir.path().as_os_str())
        ));
    }

    #[test]
    fn an_empty_path_never_suppresses_the_bootstrap() {
        let empty = tempfile::tempdir().expect("tempdir");
        assert!(!ambient_node_gyp_wins(
            ScriptReach::AmbientPath,
            Some(empty.path().as_os_str())
        ));
        assert!(!ambient_node_gyp_wins(ScriptReach::AmbientPath, None));
    }

    /// Standalone aube registers no embedder profile, so it resolves to the
    /// unconfined reach and keeps user precedence byte-for-byte.
    #[test]
    fn the_default_embedder_reaches_the_ambient_path() {
        assert_eq!(
            ScriptReach::for_active_embedder(),
            ScriptReach::AmbientPath,
            "standalone aube must keep deferring to a user's node-gyp"
        );
    }

    struct Tree {
        _tmp: tempfile::TempDir,
        tool_dir: PathBuf,
        bin_dir: PathBuf,
        store: PathBuf,
    }

    /// Mirrors the real bootstrapped layout: a generated shim (a plain
    /// file, not a symlink) in `.bin`, and a require root that reaches
    /// the package only by traversing a symlink into the global store.
    fn isolated_tree() -> Tree {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tool_dir = tmp.path().join("tools/node-gyp/v12");
        let bin_dir = tool_dir.join("node_modules/.bin");
        let store = tmp.path().join("store/node-gyp@12.4.0-deadbeef");

        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let pkg_in_store = store.join("node_modules/node-gyp");
        std::fs::create_dir_all(&pkg_in_store).expect("store pkg");
        std::fs::write(pkg_in_store.join("package.json"), b"{}").expect("pkg json");
        std::fs::write(bin_dir.join(primary_binary_name()), b"#!/bin/sh\n").expect("shim");

        let virtual_entry = tool_dir.join("node_modules/.store/node-gyp@12.4.0");
        std::fs::create_dir_all(virtual_entry.parent().expect("parent")).expect("virtual store");
        symlink_dir(&store, &virtual_entry);
        symlink_dir(
            &virtual_entry.join("node_modules/node-gyp"),
            &tool_dir.join("node_modules/node-gyp"),
        );

        Tree {
            _tmp: tmp,
            tool_dir,
            bin_dir,
            store,
        }
    }

    fn symlink_dir(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).expect("symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(target, link).expect("symlink");
    }

    #[test]
    fn purging_the_global_store_makes_the_tool_tree_unusable() {
        let t = isolated_tree();
        assert!(
            tool_tree_usable(&t.tool_dir, &t.bin_dir),
            "control: an intact bootstrapped tree must be usable, else this \
             test would pass no matter what the predicate returns"
        );

        std::fs::remove_dir_all(&t.store).expect("purge store");

        assert!(
            node_gyp_bin_exists(&t.bin_dir),
            "precondition: the shim must survive the purge — that is exactly \
             why existence alone is the wrong check"
        );
        assert!(
            !tool_tree_usable(&t.tool_dir, &t.bin_dir),
            "a purged store must force a re-bootstrap, not a success return"
        );
    }

    /// A confining embedder stamps this path as `npm_config_node_gyp`, which npm
    /// invokes one of TWO ways depending on its version — `node "$value"` (npm
    /// ≥7) or `"$value"` directly (npm ≤6) — so the returned path must be both a
    /// `.js` entry and executable with a shebang. Naming the `.bin` shell shim
    /// breaks the first; losing the mode bit breaks the second, and each breaks
    /// only for the npm the user happens to run.
    ///
    /// SCOPE: this pins the contract `js_entry_in` hands its caller, staged to
    /// mirror the materialized layout. It does NOT prove the store preserves the
    /// mode end-to-end — the CAS writes every blob `0o644` and carries
    /// executability as a separate index boolean re-applied at link time, so
    /// that half is covered by materialization, not here.
    #[test]
    fn the_cached_entry_is_a_runnable_node_gyp_entry() {
        let t = isolated_tree();
        let pkg = t.store.join("node_modules/node-gyp");
        std::fs::create_dir_all(pkg.join("bin")).expect("bin dir");
        assert_eq!(js_entry_in(&t.tool_dir), None, "no bin/node-gyp.js yet");

        let staged = pkg.join("bin/node-gyp.js");
        std::fs::write(&staged, b"#!/usr/bin/env node\n").expect("entry");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        let entry = js_entry_in(&t.tool_dir).expect("a usable tree must yield an entry");
        assert_eq!(
            entry,
            t.tool_dir.join("node_modules/node-gyp/bin/node-gyp.js"),
            "must resolve through the virtual store to the package's JS entry"
        );
        assert!(
            std::fs::read(&entry)
                .expect("read entry")
                .starts_with(b"#!"),
            "npm ≤6 execs this path directly, so it must carry a shebang"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&entry)
                .expect("stat entry")
                .permissions()
                .mode();
            assert!(
                mode & 0o111 != 0,
                "npm ≤6 execs this path directly, so it must stay executable; mode {mode:o}"
            );
        }

        std::fs::remove_dir_all(&t.store).expect("purge store");
        assert_eq!(
            js_entry_in(&t.tool_dir),
            None,
            "a purged store must not hand out a path that no longer resolves"
        );
    }

    #[test]
    fn a_hoisted_tree_with_no_virtual_store_is_usable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tool_dir = tmp.path().to_path_buf();
        let bin_dir = tool_dir.join("node_modules/.bin");
        let pkg = tool_dir.join("node_modules/node-gyp");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        std::fs::create_dir_all(&pkg).expect("pkg dir");
        std::fs::write(pkg.join("package.json"), b"{}").expect("pkg json");
        std::fs::write(bin_dir.join(primary_binary_name()), b"#!/bin/sh\n").expect("shim");

        // Guards the fast path: a false negative here would re-run the
        // recursive install on every native dependency.
        assert!(tool_tree_usable(&tool_dir, &bin_dir));
    }

    /// Stage the real generated `node-gyp.js` plus a `node-gyp` on PATH that
    /// tallies each invocation, and return `(shim, bin_dir, tally)`. `body` is
    /// the tallying shim's payload after it records itself.
    #[cfg(unix)]
    fn staged_path_node_gyp(tmp: &std::path::Path, body: &str) -> (PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let shim_dir = tmp.join("lazy-bin");
        std::fs::create_dir_all(&shim_dir).expect("shim dir");
        write_lazy_shims(&shim_dir).expect("write shims");
        let bin_dir = tmp.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let tally = tmp.join("tally");
        let path_gyp = bin_dir.join("node-gyp");
        // The stub enforces its OWN ceiling before running `body`. Without it, a
        // regression that removes the shim's guard would make this test fork-bomb
        // the CI runner instead of failing; with it, the cycle stops at 25 hops and
        // the assertions below report a clean failure.
        std::fs::write(
            &path_gyp,
            format!(
                "#!/bin/sh\necho x >> '{t}'\n\
                 if [ \"$(wc -l < '{t}')\" -ge 25 ]; then\n\
                 \x20 echo 'stub: re-entry ceiling hit — the shim did not stop' >&2; exit 90\n\
                 fi\n{body}\n",
                t = tally.display()
            ),
        )
        .expect("write path node-gyp");
        std::fs::set_permissions(&path_gyp, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        (shim_dir.join("node-gyp.js"), bin_dir, tally)
    }

    #[cfg(unix)]
    fn run_shim(shim: &std::path::Path, bin_dir: &std::path::Path) -> std::process::Output {
        std::process::Command::new("node")
            .arg(shim)
            .arg("rebuild")
            // No AUBE_NODE_GYP_EXE: this is the bare-PATH fallback branch.
            .env_remove("AUBE_NODE_GYP_EXE")
            .env_remove("AUBE_NODE_GYP_SHIM_DEPTH")
            // `bin_dir` FIRST so a bare `node-gyp` hits the staged stub, but the
            // ambient PATH must stay reachable or the stub cannot find `node`.
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin_dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .output()
            .expect("node must be on PATH to exercise the generated shim")
    }

    /// THE REGRESSION. npm's run-script puts its own `node-gyp` stub first on
    /// PATH and points `npm_config_node_gyp` at this shim, so the shim's bare
    /// fallback resolves back to itself. Unguarded that never terminates —
    /// measured under nub's build jail at ~1500 processes and 15 GB, with load
    /// near idle because every hop is a blocking `spawnSync`. The tally is what
    /// makes this non-hollow: it counts real re-entries, so reverting the guard
    /// makes the run diverge instead of quietly passing.
    #[cfg(unix)]
    #[test]
    fn the_path_fallback_refuses_to_resolve_back_into_itself() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (shim, bin_dir, tally) = staged_path_node_gyp(tmp.path(), "exec node \"$0.js\" \"$@\"");
        // `$0.js` is the tallying stub re-execing the shim beside it, which is
        // exactly npm's stub bouncing back through `npm_config_node_gyp`.
        let link = bin_dir.join("node-gyp.js");
        std::fs::copy(&shim, &link).expect("stage the shim next to the stub");

        let out = run_shim(&shim, &bin_dir);
        assert!(!out.status.success(), "a self-resolving fallback must fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Refusing to recurse"),
            "must name the cycle it refused; got: {stderr}"
        );
        let hops = std::fs::read_to_string(&tally)
            .map(|t| t.lines().count())
            .unwrap_or(0);
        assert!(
            hops <= 4,
            "re-entry must be bounded; the stub ran {hops} times"
        );
    }

    /// The CONTROL that keeps the guard honest: when the `node-gyp` on PATH is
    /// a real one, the fallback must still forward to it and succeed, exactly
    /// once. A guard that broke this would break every legitimate native build
    /// that reaches node-gyp through the PATH fallback.
    #[cfg(unix)]
    #[test]
    fn the_path_fallback_still_forwards_to_a_real_node_gyp() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (shim, bin_dir, tally) = staged_path_node_gyp(tmp.path(), "exit 0");

        let out = run_shim(&shim, &bin_dir);
        assert!(
            out.status.success(),
            "the fallback must still reach a real node-gyp; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let hops = std::fs::read_to_string(&tally)
            .map(|t| t.lines().count())
            .unwrap_or(0);
        assert_eq!(hops, 1, "the real node-gyp must run exactly once");
    }
}
