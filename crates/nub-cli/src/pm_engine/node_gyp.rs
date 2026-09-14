//! On-demand bootstrap of `node-gyp` into nub's own PM cache.
//!
//! Many npm packages ship a native addon and rely on `node-gyp` during their
//! install lifecycle — explicitly (`"install": "node-gyp rebuild"`) or
//! transitively through tooling like `node-gyp-build` that shells out to it.
//! npm and pnpm solve that by bundling node-gyp inside their own npm package;
//! the engine's equivalent lookup (`pnpm_executor::bundled_node_gyp_bin`) wants
//! a `dist/node-gyp-bin` directory beside the running executable, a layout that
//! exists beside pnpm's published `pnpm` and not beside nub's binary. nub is a
//! Rust binary with no bundled JavaScript, so it bootstraps one lazily instead.
//!
//! Nothing bootstraps eagerly, which is what keeps this free for the installs
//! that never compile anything: both entry points write a handful of tiny shim
//! files and return. The real install runs only when a build script invokes a
//! shim, which re-enters nub through the hidden `__node-gyp-bootstrap` verb
//! ([`super::run_node_gyp_bootstrap`]) and execs whatever path that prints.
//!
//! That bootstrap install is nub's own, driven in-process through the engine's
//! front door against a synthetic single-package project in the cache — the
//! same code path `nub install` takes, so it inherits the project's registry
//! configuration, the store, and every policy without a second implementation.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Major-version pin. Bumping the bucket invalidates the cache and triggers a
/// re-bootstrap on the next install.
const BUCKET: &str = "v12";
/// Semver range the bootstrap install resolves. Keep aligned with `BUCKET`.
const SPEC: &str = "^12.0.0";

/// The nub binary the shims re-enter, and the project their bootstrap install
/// inherits registry configuration from. Internal plumbing nub sets for its own
/// child process — never a documented user knob.
pub(crate) const EXE_ENV: &str = "__NUB_NODE_GYP_EXE";
pub(crate) const PROJECT_DIR_ENV: &str = "__NUB_NODE_GYP_PROJECT_DIR";

/// npm's own name for the "run node-gyp this way" channel, which both npm and
/// pnpm always point at a runnable script so `node $npm_config_node_gyp …`
/// works without a global install.
pub(crate) const CONFIG_ENV: &str = "npm_config_node_gyp";

#[cfg(windows)]
const BINARY_NAMES: &[&str] = &["node-gyp.cmd", "node-gyp.exe", "node-gyp"];
#[cfg(not(windows))]
const BINARY_NAMES: &[&str] = &["node-gyp"];

/// nub's PM cache root. `pm` is the leaf the engine's `cache_namespace`
/// ("nub/pm") appends, so this addresses the same tree `nub pm cache` lists.
fn tool_root() -> Result<PathBuf> {
    let cache = nub_core::node::discovery::cache_dir()
        .context("could not locate nub's cache directory (no $HOME / $XDG_CACHE_HOME)")?;
    Ok(cache.join("pm").join("tools").join("node-gyp"))
}

/// The platform's accepted `node-gyp` shim filename in `bin_dir`, if any. On
/// Windows the installed shim is `node-gyp.cmd` (sometimes `.exe` alongside),
/// so a bare-string check would always miss it and the fast path would never
/// fire.
fn node_gyp_binary(bin_dir: &Path) -> Option<PathBuf> {
    BINARY_NAMES
        .iter()
        .map(|name| bin_dir.join(name))
        .find(|path| path.is_file())
}

/// The cached node-gyp executable, but only when the tree behind it is still
/// live.
///
/// The `.bin` entry is a generated wrapper rather than a symlink, so it remains
/// a perfectly valid file after the virtual-store package it execs is pruned —
/// which once made this fast path hand out a broken node-gyp forever. Checking
/// the package root instead of decoding the wrapper keeps the test independent
/// of whichever wrapper format the engine writes.
fn cached_node_gyp_binary(tool_dir: &Path) -> Option<PathBuf> {
    let node_modules = tool_dir.join("node_modules");
    if !node_modules.join("node-gyp").join("package.json").is_file() {
        return None;
    }
    node_gyp_binary(&node_modules.join(".bin"))
}

/// Install (or reuse) the pinned node-gyp under the tool dir and return its
/// executable.
///
/// `project_dir` is the outer install's project root; its `.npmrc` (if any) is
/// propagated to the tool dir so private-registry URLs and auth tokens
/// configured at project scope reach the bootstrap install, which resolves
/// against the tool dir and would otherwise only see `~/.npmrc`.
pub(crate) fn bootstrap(project_dir: &Path) -> Result<PathBuf> {
    let root = tool_root()?;
    let tool_dir = root.join(BUCKET);
    if let Some(binary) = cached_node_gyp_binary(&tool_dir) {
        return Ok(binary);
    }

    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    // A single install can run several build scripts at once, each of which may
    // invoke the shim and re-enter nub, so the bootstrap is serialized across
    // processes. Keyed on the bucket rather than the outer project, so it stays
    // disjoint from whatever lock the install itself holds.
    let mut lock = fslock::LockFile::open(&root.join(format!("{BUCKET}.lock")))
        .with_context(|| format!("opening the node-gyp bootstrap lock in {}", root.display()))?;
    lock.lock()
        .context("acquiring the node-gyp bootstrap lock")?;
    // Re-check under the lock: another process may have finished between the
    // check above and acquisition.
    if let Some(binary) = cached_node_gyp_binary(&tool_dir) {
        return Ok(binary);
    }

    write_bootstrap_project(&tool_dir, &project_dir.join(".npmrc"))?;
    tracing::info!("bootstrapping node-gyp {SPEC} into {}", tool_dir.display());
    // `--ignore-scripts` because node-gyp's own tree needs no build scripts and
    // running them would recurse straight back into this bootstrap; `--silent`
    // because the shim reads this process's stdout to learn the path.
    let code = super::run_pnpm_engine(vec![
        std::ffi::OsString::from("nub"),
        std::ffi::OsString::from("--dir"),
        std::ffi::OsString::from(&tool_dir),
        std::ffi::OsString::from("install"),
        std::ffi::OsString::from("--ignore-scripts"),
        std::ffi::OsString::from("--silent"),
    ])?;
    if code != 0 {
        bail!(
            "failed to bootstrap node-gyp {SPEC} into {} — \
             pre-populate it or run `nub install` once while online",
            tool_dir.display()
        );
    }

    cached_node_gyp_binary(&tool_dir).ok_or_else(|| {
        anyhow::anyhow!(
            "node-gyp bootstrap into {} reported success but left no node-gyp executable",
            tool_dir.display()
        )
    })
}

/// The directory to put on a script's `PATH` so a bare `node-gyp` resolves —
/// the channel `"install": "node-gyp rebuild"`, by far the commonest spelling,
/// actually uses. `None` when node-gyp already resolves without nub, from the
/// project's own `.bin` or the ambient `PATH`: the user's copy wins, and
/// shadowing it with a shim that re-enters nub would buy nothing.
pub(crate) fn lazy_shim_bin_dir(project_bin_dir: &Path) -> Result<Option<PathBuf>> {
    if resolves_without_us(project_bin_dir, std::env::var_os("PATH").as_deref()) {
        return Ok(None);
    }
    let shim_dir = tool_root()?.join("lazy-bin");
    write_lazy_shims(&shim_dir)?;
    Ok(Some(shim_dir))
}

/// Takes `path` rather than reading the environment so the decision is testable
/// without mutating a process-global every sibling test also reads.
fn resolves_without_us(project_bin_dir: &Path, path: Option<&std::ffi::OsStr>) -> bool {
    node_gyp_binary(project_bin_dir).is_some()
        || path.is_some_and(|path| {
            std::env::split_paths(path).any(|dir| node_gyp_binary(&dir).is_some())
        })
}

/// Path to the lazy `node-gyp.js` shim, exported as `npm_config_node_gyp` for
/// parity with npm and pnpm (which point it at their bundled
/// `node-gyp/bin/node-gyp.js`). Returned unconditionally, as they set it
/// unconditionally: writing the shim is a couple of tiny files and never
/// bootstraps, and `npm_config_node_gyp` is a separate channel from `PATH`.
/// Content-checked on every call so a shipped shim fix self-heals rather than
/// being pinned to whatever first landed in the cache — see [`write_lazy_shims`].
pub(crate) fn lazy_js_shim_path() -> Result<PathBuf> {
    let shim_dir = tool_root()?.join("lazy-bin");
    write_lazy_shims(&shim_dir)?;
    Ok(shim_dir.join("node-gyp.js"))
}

/// The `node-gyp` shell shim: resolves the real executable through the hidden
/// `__node-gyp-bootstrap` verb, then execs it.
///
/// [`PROJECT_DIR_ENV`] is optional (cwd fallback, matching the `.js` shim
/// below), so it is expanded defensively — under `set -u` a bare expansion
/// aborts with "unbound variable" on any path that doesn't set it.
/// [`EXE_ENV`] is the one hard requirement, and it gets an explicit message
/// rather than an exec of the empty string. There is deliberately no fallback
/// to a bare `node-gyp`: this file *is* the `node-gyp` on PATH, so resolving
/// that name again would re-exec the shim forever.
const SH_SHIM: &str = r#"#!/usr/bin/env sh
set -eu
if [ -z "${__NUB_NODE_GYP_EXE:-}" ]; then
  echo "node-gyp shim invoked outside a lifecycle script (__NUB_NODE_GYP_EXE unset)" >&2
  exit 1
fi
real="$("$__NUB_NODE_GYP_EXE" __node-gyp-bootstrap "${__NUB_NODE_GYP_PROJECT_DIR:-$PWD}")"
exec "$real" "$@"
"#;

/// `node-gyp.js`: the value of `npm_config_node_gyp`. Consumers run it as
/// `node $npm_config_node_gyp …`, so it must be a Node script rather than the
/// shell shim above. Falls back to a `node-gyp` on PATH when nub's env markers
/// are absent (a script spawned outside nub's wrappers).
const JS_SHIM: &str = r#"#!/usr/bin/env node
"use strict";
// nub's lazy node-gyp stand-in for npm_config_node_gyp. Resolves (and
// bootstraps on first use) nub's node-gyp, then forwards argv. Kept
// dependency-free; writing this file is free, the bootstrap only fires
// when something actually invokes it. Bare `require` (no `node:` prefix)
// so the shim runs under any Node the user drives, including pre-16.
const { execFileSync, spawnSync } = require("child_process");
const isWin = process.platform === "win32";
let real;
const exe = process.env.__NUB_NODE_GYP_EXE;
if (exe) {
  const dir = process.env.__NUB_NODE_GYP_PROJECT_DIR || process.cwd();
  real = execFileSync(exe, ["__node-gyp-bootstrap", dir], { encoding: "utf8" }).trim();
} else {
  real = isWin ? "node-gyp.cmd" : "node-gyp";
}
const result = spawnSync(real, process.argv.slice(2), { stdio: "inherit", shell: isWin });
if (result.error) {
  console.error("nub: failed to run node-gyp (" + real + "): " + result.error.message);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
"#;

/// The Windows `node-gyp.cmd` shim. Two cmd.exe rules bite here, and both were
/// live bugs:
///
/// 1. `for /f` runs its command through `cmd /c`, which STRIPS the outer quote
///    pair when the string both starts and ends with a quote. The natural
///    spelling therefore degrades to
///    `C:\...\nub.exe" __node-gyp-bootstrap "C:\...\proj` and dies with "The
///    filename, directory name, or volume label syntax is incorrect" —
///    measured on windows-latest, with and without spaces in the path. Wrapping
///    the whole command in one MORE quote pair makes that strip leave exactly
///    the intended string.
/// 2. An undefined `%VAR%` expands to its own literal text, so without the
///    fallback the bootstrap receives `%__NUB_NODE_GYP_PROJECT_DIR%` as a path.
///    `setlocal` keeps that fallback out of the caller's env.
#[cfg(windows)]
const CMD_SHIM: &str = r#"@echo off
setlocal
if not defined __NUB_NODE_GYP_EXE (
  echo node-gyp shim invoked outside a lifecycle script ^(__NUB_NODE_GYP_EXE unset^)>&2
  exit /b 1
)
if not defined __NUB_NODE_GYP_PROJECT_DIR set "__NUB_NODE_GYP_PROJECT_DIR=%CD%"
for /f "usebackq delims=" %%i in (`""%__NUB_NODE_GYP_EXE%" __node-gyp-bootstrap "%__NUB_NODE_GYP_PROJECT_DIR%""`) do set "__NUB_REAL_NODE_GYP=%%i"
if not defined __NUB_REAL_NODE_GYP exit /b 1
"%__NUB_REAL_NODE_GYP%" %*
"#;

#[cfg(unix)]
const SHIM_MODE: u32 = 0o755;

/// Materialize the lazy shims into `shim_dir`.
///
/// Called on every `nub run` and once per engine session, so the steady state
/// has to be cheap: each shim is rewritten only when its on-disk copy differs,
/// which keeps the common case to a couple of small reads instead of a
/// create-dir + write-temp + rename + chmod per file per invocation. Not
/// writing unless the content changed also stops concurrent lifecycle jobs from
/// renaming over each other's shims, and stops interrupted-write temp files
/// from accumulating in the cache dir.
fn write_lazy_shims(shim_dir: &Path) -> Result<()> {
    write_shim_if_stale(&shim_dir.join("node-gyp"), SH_SHIM)?;
    write_shim_if_stale(&shim_dir.join("node-gyp.js"), JS_SHIM)?;
    #[cfg(windows)]
    write_shim_if_stale(&shim_dir.join("node-gyp.cmd"), CMD_SHIM)?;
    Ok(())
}

/// Write one shim, skipping the write when the file on disk already matches.
///
/// The comparison reads through a single open handle and takes the mode from
/// that same handle's `fstat`, so a hit costs open + fstat + read + close and
/// touches nothing. A miss — absent, stale content, or an exec bit that got
/// stripped — falls through to the atomic write, which is also what repairs it.
fn write_shim_if_stale(path: &Path, contents: &str) -> Result<()> {
    if shim_is_current(path, contents) {
        return Ok(());
    }
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        Some(std::fs::Permissions::from_mode(SHIM_MODE))
    };
    #[cfg(not(unix))]
    let permissions = None;
    // `atomic_write_with_permissions` creates the parent dir and applies the
    // mode before the rename, so the fast path above can skip `create_dir_all`
    // entirely: a matching file proves the dir.
    aube_util::fs_atomic::atomic_write_with_permissions(path, contents.as_bytes(), permissions)
        .with_context(|| format!("writing the node-gyp shim {}", path.display()))
}

/// True when `path` already holds exactly `contents` and (on unix) is still
/// executable. Any error — missing file, permission trouble, unreadable —
/// reports "not current" so the caller rewrites it.
fn shim_is_current(path: &Path, contents: &str) -> bool {
    use std::io::Read as _;
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
        // Compare only the permission bits; `st_mode` also carries the file
        // type, which `is_file` above has already vetted.
        if meta.permissions().mode() & 0o777 != SHIM_MODE {
            return false;
        }
    }
    let mut on_disk = Vec::with_capacity(contents.len());
    f.read_to_end(&mut on_disk).is_ok() && on_disk == contents.as_bytes()
}

/// Materialize the synthetic single-package project the bootstrap install runs
/// against. Writes are atomic and idempotent, so racing processes converge on
/// the same content; serialization is the caller's lock.
fn write_bootstrap_project(tool_dir: &Path, project_npmrc: &Path) -> Result<()> {
    std::fs::create_dir_all(tool_dir)
        .with_context(|| format!("creating {}", tool_dir.display()))?;
    let manifest = format!(
        r#"{{"name":"nub-tool-node-gyp","private":true,"dependencies":{{"node-gyp":"{SPEC}"}}}}"#
    );
    write_atomic(&tool_dir.join("package.json"), manifest.as_bytes())?;
    // Pin the bootstrap install to `tool_dir` so its workspace-root walk stops
    // here instead of escaping upward into $HOME — or, under a test that points
    // HOME at a temp dir, into the outer project whose lock the outer install
    // already holds. A workspace yaml is a hard boundary, so the empty stub
    // ends the walk at the first marker check.
    write_atomic(&tool_dir.join("pnpm-workspace.yaml"), b"")?;
    // Forward the outer project's `.npmrc`. Overwritten on every bootstrap so a
    // user updating theirs between runs picks up fresh config; the stale copy is
    // deleted if the project no longer has one.
    let tool_npmrc = tool_dir.join(".npmrc");
    if project_npmrc.exists() {
        std::fs::copy(project_npmrc, &tool_npmrc).with_context(|| {
            format!(
                "propagating {} to the node-gyp bootstrap dir",
                project_npmrc.display()
            )
        })?;
    } else if tool_npmrc.exists() {
        let _ = std::fs::remove_file(&tool_npmrc);
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    aube_util::fs_atomic::atomic_write(path, bytes)
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nub-gyp-shim-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The shims land even though nothing created `shim_dir` first — the entry
    /// points rely on the atomic write for that.
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
            JS_SHIM
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// The point of the content check: a second call with the content already
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

        write_lazy_shims(&dir).unwrap();

        let after = (
            std::fs::metadata(&sh).unwrap().modified().unwrap(),
            std::fs::metadata(&js).unwrap().modified().unwrap(),
        );
        assert_eq!(before, after, "shims were rewritten despite matching bytes");
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(strays.is_empty(), "left temp files behind: {strays:?}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// Self-heal: a shim whose bytes drifted (an older nub shipped different
    /// content) is rewritten rather than pinned. Same-length drift too, so the
    /// length pre-check cannot let it through.
    #[test]
    fn stale_content_is_rewritten() {
        let dir = tempdir().join("lazy-bin");
        write_lazy_shims(&dir).unwrap();
        let sh = dir.join("node-gyp");

        std::fs::write(&sh, "#!/usr/bin/env sh\necho from an older nub\n").unwrap();
        assert!(!shim_is_current(&sh, SH_SHIM));
        write_lazy_shims(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&sh).unwrap(), SH_SHIM);

        let mut drifted = SH_SHIM.as_bytes().to_vec();
        *drifted.last_mut().unwrap() = b' ';
        assert_eq!(drifted.len(), SH_SHIM.len());
        std::fs::write(&sh, &drifted).unwrap();
        assert!(!shim_is_current(&sh, SH_SHIM));
        write_lazy_shims(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&sh).unwrap(), SH_SHIM);

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// A shim that lost its exec bit is still repaired — the mode is part of
    /// "current", not just the bytes.
    #[cfg(unix)]
    #[test]
    fn stripped_exec_bit_is_restored() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().join("lazy-bin");
        write_lazy_shims(&dir).unwrap();
        let sh = dir.join("node-gyp");
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

    /// A directory sitting where the shim belongs must not read as current (and
    /// must not panic).
    #[test]
    fn directory_in_the_way_is_not_current() {
        let dir = tempdir();
        let path = dir.join("node-gyp");
        std::fs::create_dir_all(&path).unwrap();
        assert!(!shim_is_current(&path, SH_SHIM));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The wrapper outliving its target is the case the package-root check
    /// exists for: the `.bin` entry is a generated script, so it stays a valid
    /// file after the store entry behind it is pruned.
    #[test]
    fn a_wrapper_without_its_package_is_not_cached() {
        let tool_dir = tempdir();
        let node_modules = tool_dir.join("node_modules");
        std::fs::create_dir_all(node_modules.join(".bin")).unwrap();
        std::fs::create_dir_all(node_modules.join("node-gyp")).unwrap();
        std::fs::write(node_modules.join(".bin").join("node-gyp"), "#!/bin/sh\n").unwrap();
        assert!(cached_node_gyp_binary(&tool_dir).is_none());

        std::fs::write(node_modules.join("node-gyp").join("package.json"), "{}").unwrap();
        assert!(cached_node_gyp_binary(&tool_dir).is_some());

        std::fs::remove_file(node_modules.join("node-gyp").join("package.json")).unwrap();
        assert!(cached_node_gyp_binary(&tool_dir).is_none());
        let _ = std::fs::remove_dir_all(tool_dir);
    }

    /// The `PATH` channel stands down for a node-gyp the user already has, from
    /// either source — and only for a real one, not for an empty `.bin` or a
    /// `PATH` entry that merely exists.
    #[test]
    fn a_resolvable_node_gyp_keeps_the_shim_off_path() {
        let dir = tempdir();
        let project_bin = dir.join("project-bin");
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&project_bin).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let path = std::env::join_paths([&elsewhere]).unwrap();

        assert!(!resolves_without_us(&project_bin, Some(&path)));

        std::fs::write(elsewhere.join(BINARY_NAMES[0]), "#!/bin/sh\n").unwrap();
        assert!(resolves_without_us(&project_bin, Some(&path)));
        assert!(!resolves_without_us(&project_bin, None));

        std::fs::write(project_bin.join(BINARY_NAMES[0]), "#!/bin/sh\n").unwrap();
        assert!(resolves_without_us(&project_bin, None));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The shim text and the reader that sets the variables are two halves of
    /// one contract, and a mismatch fails silently — the shim exits 1 mid-build
    /// with a message nothing reads. Pin both spellings in both shims.
    #[test]
    fn shims_read_the_variables_nub_stamps() {
        for shim in [SH_SHIM, JS_SHIM] {
            assert!(shim.contains(EXE_ENV), "shim does not read {EXE_ENV}");
            assert!(
                shim.contains(PROJECT_DIR_ENV),
                "shim does not read {PROJECT_DIR_ENV}"
            );
        }
        #[cfg(windows)]
        {
            assert!(CMD_SHIM.contains(EXE_ENV));
            assert!(CMD_SHIM.contains(PROJECT_DIR_ENV));
        }
    }
}
