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
//! disjoint from the outer install's lock. The [`tool_tree_usable`]
//! fast path short-circuits every subsequent invocation.
use miette::{IntoDiagnostic, WrapErr, miette};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The node-gyp to bootstrap for a given Node major, MEASURED by real addon
/// builds — never read off `engines.node` alone, which is a declaration and
/// disagrees with reality in both directions.
///
/// A FIXED pin cannot be correct, because the constraint has TWO axes that
/// pull opposite ways:
///
///   - node-gyp must be OLD enough to RUN on the ambient Node. node-gyp 12
///     calls `String.replaceAll` (Node 15+) and 9 calls `Object.fromEntries`
///     (Node 12+), so each release silently loses the Nodes beneath it.
///   - node-gyp must be NEW enough for the ambient PYTHON. Python 3.12
///     removed `distutils`, which the gyp vendored in node-gyp <=9 imports,
///     so the older releases die on any modern interpreter.
///
/// Measured (real `node-gyp rebuild` of a minimal addon, macOS arm64):
///
/// ```text
///           node 8   10   12   14   16   18   20   22   24   26  | py3.12+
///   gyp 5       ok   ok   ok   ok   ok   ok    -    -    -    -  |   no
///   gyp 8        -   ok   ok   ok   ok   ok   ok   ok   ok   ok  |   no
///   gyp 9        -    -   ok   ok   ok   ok   ok   ok   ok   ok  |   no
///   gyp 10       -    -    -   ok   ok   ok   ok   ok   ok   ok  |  yes
///   gyp 12       -    -    -    -    -   ok   ok   ok   ok   ok  |  yes
/// ```
///
/// This was `^12.0.0` unconditionally, which made EVERY native addon fail
/// below Node 18 — and a confined lifecycle script always takes our copy, so
/// the package's own correct `engines` could not save it. Selecting the newest
/// release that still runs on the ambient Node recovers Node 10..17.
///
/// Where the axes genuinely conflict (Node <=13 on Python 3.12+) no release
/// satisfies both and there is nothing to choose: we take the newest that fits
/// the Node, so the failure is a legible Python error rather than a confusing
/// `replaceAll` TypeError.
fn bucket_for(node_major: u64) -> (&'static str, &'static str) {
    match node_major {
        // Node <6 CANNOT PARSE node-gyp 5, which declares `>= 6.0.0` and means it: the whole
        // bucket dies `SyntaxError: Unexpected token {` before printing its own banner, so the
        // log shows no `using node-gyp@…` line at all. node-gyp 3.8.0 declares `>= 0.8.0` and is
        // what the ecosystem of that era actually used.
        //
        // MEASURED on `lzo@0.1.1` and `gcstats.js@1.0.0` under Node 4.9.1: with our v5 bucket both
        // fail at that SyntaxError; the same packages built an addon and printed `gyp info ok` with
        // node-gyp 3.8.0. Registry-confirmed engines: gyp 3.8.0 `>= 0.8.0`, gyp 4.0.0 `>= 4.0.0`,
        // gyp 5.1.1 `>= 6.0.0`.
        //
        // ⚠ NEEDS PYTHON 2, AND THAT IS A DOCUMENTED PREREQUISITE RATHER THAN A CODE PATH.
        // node-gyp 3's bundled gyp predates Python 3: it runs `print "%s.%s.%s" % ...`, so on any
        // Python 3 it dies `SyntaxError: Missing parentheses in call to 'print'`. MEASURED on
        // `lzo@0.1.1` under Node 4.9.1 — confined, nub named Python 3.10 and hit exactly that;
        // unconfined, node-gyp found `/usr/local/bin/python2` and built the addon.
        //
        // We do NOT teach the Python resolver to prefer python2 for this band. `python_reads`
        // floors candidates at 3.6 by design, the band sits far below nub's own 18.19 support
        // floor, and a Python 2 is absent from almost every modern machine — so the added
        // complexity in a security-sensitive resolver buys a case only a harness can reach anyway.
        // Selecting gyp 3 here is still right for the reason this file already states about the
        // other axis: it makes the failure a LEGIBLE Python error instead of an opaque
        // `SyntaxError: Unexpected token {` from a node-gyp the interpreter cannot even parse.
        // A harness that wants this band green provisions a python2 alongside the era Node.
        0..=5 => ("v3", "^3.8.0"),
        6..=9 => ("v5", "^5.0.0"),
        10..=11 => ("v8", "^8.0.0"),
        // 14..15 take node-gyp 9, NOT 10. node-gyp 10 declares `^16.14.0 || >=18.0.0` and means
        // it: on Node 14 it dies at `TypeError: Cannot read property 'pipeline' of undefined`
        // (measured on cbor-extract@0.1.0). node-gyp 9 declares `^12.13 || ^14.13 || >=16`, which
        // covers 14 and 15 properly. This is why the table above is measured rather than derived —
        // 10 "runs" on 14 far enough to print its banner and then fails in the build.
        12..=15 => ("v9", "^9.0.0"),
        16..=17 => ("v10", "^10.0.0"),
        _ => ("v12", "^12.0.0"),
    }
}

/// The Node major the lifecycle scripts will run under, asked of the `node`
/// THEY will resolve rather than of the host process — under an embedder those
/// are different binaries, and a decision made from the wrong one is exactly
/// the class of bug this function exists to avoid.
///
/// `None` when no `node` is reachable, in which case the caller keeps the
/// modern default: a machine with no Node has no addon to build.
fn ambient_node_major() -> Option<u64> {
    let out = std::process::Command::new("node")
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()?
        .parse()
        .ok()
}
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
pub async fn ensure(project_dir: &Path, reach: ScriptReach) -> miette::Result<Option<PathBuf>> {
    if ambient_node_gyp_wins(reach, std::env::var_os("PATH").as_deref()) {
        return Ok(None);
    }
    ensure_cached(project_dir).await.map(Some)
}

/// Install (or reuse) the pinned node-gyp under the tool dir and return its
/// `.bin`. Nothing bootstraps eagerly on the ordinary path — the lazy shims
/// reach it — so an install whose builds never invoke node-gyp never pays for
/// this. A confined caller ([`ensure`], [`ensure_bin_dir_for_jail`]) is the
/// exception: its scripts cannot re-enter from inside the jail.
///
/// `project_dir` is the outer install's project root; its `.npmrc`
/// (if any) is propagated to the tool dir so the bootstrap inherits
/// the same registry/auth configuration.
pub async fn ensure_cached(project_dir: &Path) -> miette::Result<PathBuf> {
    let root = tool_root()?;
    // Modern default when no `node` answers: nothing to build there anyway.
    let (bucket, spec) = bucket_for(ambient_node_major().unwrap_or(u64::MAX));
    let tool_dir = root.join(bucket);
    let bin_dir = tool_dir.join("node_modules").join(".bin");
    if tool_tree_usable(&tool_dir, &bin_dir) {
        return Ok(bin_dir);
    }
    let tool_dir_blocking = tool_dir.clone();
    let project_npmrc = project_dir.join(".npmrc");
    tokio::task::spawn_blocking(move || {
        write_bootstrap_project(&tool_dir_blocking, &project_npmrc, spec)
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
    if tool_tree_usable(&tool_dir, &bin_dir) {
        return Ok(bin_dir);
    }

    tracing::info!("bootstrapping node-gyp {spec} into {}", tool_dir.display());
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
                "failed to bootstrap node-gyp {spec} into {} — \
                 pre-populate it or run `{}` once while online",
                tool_dir.display(),
                aube_util::cmd("install")
            )
        })?;

    if !tool_tree_usable(&tool_dir, &bin_dir) {
        return Err(miette!(
            "node-gyp bootstrap into {} reported success but left no usable tree \
             (missing shim in {}, or an unresolvable {PKG} require root)",
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
/// `None` only when the PROJECT's own `.bin` already has one — that dir is inside
/// the tree a confined script can read. The ambient `PATH` is NOT, which is why
/// this defers to [`ensure`] with [`ScriptReach::Confined`] rather than repeating
/// the lazy path's `node_gyp_on_path` check: a host node-gyp the script cannot
/// reach must not suppress the bootstrap.
pub(crate) async fn ensure_bin_dir_for_jail(
    project_bin_dir: &Path,
    project_dir: &Path,
) -> miette::Result<Option<PathBuf>> {
    if node_gyp_bin_exists(project_bin_dir) {
        return Ok(None);
    }
    ensure(project_dir, ScriptReach::Confined).await
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

/// Write-time placeholders the shim templates carry. Deliberately BRAND-FREE:
/// what they stand in for is chosen by the active embedder, so a token spelled
/// with one tool's brand would misread as fixed.
///
/// [`BUCKET_TOKEN`] stands in for [`BUCKET`] — the shims sit in
/// `<tool_root>/lazy-bin`, so the bootstrapped tree is a `../<BUCKET>` sibling,
/// derived from the shim's own location rather than baked as an absolute path, so
/// a relocated or re-homed cache still resolves.
///
/// The `*_VAR` tokens stand in for the cross-process plumbing var NAMES, composed
/// through the embedder's `internal_env_prefix` (`AUBE_NODE_GYP_EXE` standalone,
/// `__NUB_NODE_GYP_EXE` under nub). The brand is load-bearing here rather than
/// cosmetic: an embedding host's build-jail env policy withholds these BY NAME, so
/// the engine's brand must not be the spelling that policy reasons about.
const BUCKET_TOKEN: &str = "__NODE_GYP_BUCKET__";
const EXE_VAR_TOKEN: &str = "__NODE_GYP_EXE_VAR__";
const PROJECT_DIR_VAR_TOKEN: &str = "__NODE_GYP_PROJECT_DIR_VAR__";
const SHIM_DEPTH_VAR_TOKEN: &str = "__NODE_GYP_SHIM_DEPTH_VAR__";
const TOOL_VAR_TOKEN: &str = "__NODE_GYP_TOOL_VAR__";
const REAL_VAR_TOKEN: &str = "__REAL_NODE_GYP_VAR__";
/// These shims write to the USER'S stderr and never pass through `present.rs`, so a
/// hardcoded name here is the one brand leak the rebranding layer structurally cannot
/// reach — a nub user would read `aube:` on a failed native build.
const PROG_TOKEN: &str = "__PROG__";

/// Substitute every write-time placeholder in a shim template. One function so the
/// three shims cannot drift into substituting different subsets — a missed token
/// is not a lookup miss, it is a shell/JS syntax error in a file written weeks
/// before anything runs it.
fn render_shim(template: &str) -> String {
    let var = aube_util::env::internal_env_name;
    // Same bucket the bootstrap will populate — resolved here rather than fixed,
    // so the shim points at the tree that actually gets installed for this Node.
    let (bucket, _) = bucket_for(ambient_node_major().unwrap_or(u64::MAX));
    template
        .replace(BUCKET_TOKEN, bucket)
        .replace(EXE_VAR_TOKEN, &var("NODE_GYP_EXE"))
        .replace(PROJECT_DIR_VAR_TOKEN, &var("NODE_GYP_PROJECT_DIR"))
        .replace(SHIM_DEPTH_VAR_TOKEN, &var("NODE_GYP_SHIM_DEPTH"))
        .replace(TOOL_VAR_TOKEN, &var("NODE_GYP_TOOL"))
        .replace(REAL_VAR_TOKEN, &var("REAL_NODE_GYP"))
        .replace(PROG_TOKEN, aube_util::prog())
}

fn write_lazy_shims(shim_dir: &Path) -> miette::Result<()> {
    // Resolution order is the same in both shims and in the same priority:
    // bootstrapped tree, then the PM trampoline. A bare `node-gyp` PATH lookup is
    // NOT an option here — this file is itself on `PATH`, so the lookup resolves
    // straight back into it.
    //
    // The project-dir var is optional (cwd fallback), so it must be expanded
    // defensively: under `set -u` a bare expansion aborts with "unbound variable"
    // on any path that doesn't set it. The exe var is the one hard requirement and
    // gets an explicit message rather than an exec of the empty string.
    let sh = render_shim(
        r#"#!/usr/bin/env sh
set -eu
tool="$(dirname "$0")/../__NODE_GYP_BUCKET__/node_modules"
if [ -f "$tool/node-gyp/package.json" ] && [ -x "$tool/.bin/node-gyp" ]; then
  exec "$tool/.bin/node-gyp" "$@"
fi
if [ -z "${__NODE_GYP_EXE_VAR__:-}" ]; then
  echo "__PROG__: no bootstrapped node-gyp under $tool and no bootstrap entry point" >&2
  exit 1
fi
real="$("$__NODE_GYP_EXE_VAR__" __node-gyp-bootstrap "${__NODE_GYP_PROJECT_DIR_VAR__:-$PWD}")"
exec "$real" "$@"
"#,
    );
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
    // the shell `node-gyp` shim above).
    //
    // npm and pnpm both hold one invariant here: `npm_config_node_gyp` names a
    // TERMINAL node-gyp entry script (`require.resolve("node-gyp/bin/node-gyp.js")`),
    // and their PATH stub is a one-hop `node "$npm_config_node_gyp" "$@"`. aube's
    // shim is lazy rather than terminal, so it must reach a terminal target by a
    // route npm cannot redirect — which rules out resolving by NAME, since `PATH`
    // is exactly what npm's run-script rewrites. See the cycle note below.
    let js = render_shim(
        r#"#!/usr/bin/env node
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

// The bootstrapped tree, as a `../<bucket>` sibling of this file. Preferred over
// the trampoline: same binary, one fewer process, and it still resolves where the
// PM executable is unreadable (a confining embedder need not grant its own exe).
// Both halves are probed, mirroring Rust's `tool_tree_usable` — the generated
// `.bin` entry outlives a global-store purge that takes its target with it, so a
// bin-only hit would exec straight into `Cannot find module`.
function bootstrapped() {
  const tool = join(__dirname, "..", "__NODE_GYP_BUCKET__", "node_modules");
  if (!existsSync(join(tool, "node-gyp", "package.json"))) return null;
  for (const name of isWin ? ["node-gyp.cmd", "node-gyp"] : ["node-gyp"]) {
    const p = join(tool, ".bin", name);
    if (existsSync(p)) return p;
  }
  return null;
}

let real = bootstrapped();
if (!real && process.env.__NODE_GYP_EXE_VAR__) {
  const dir = process.env.__NODE_GYP_PROJECT_DIR_VAR__ || process.cwd();
  real = execFileSync(process.env.__NODE_GYP_EXE_VAR__, ["__node-gyp-bootstrap", dir], {
    encoding: "utf8",
  }).trim();
} else if (!real) {
  // Nothing bootstrapped and no trampoline. A bare PATH lookup is the only route
  // left, and it is the one that can resolve back to THIS shim and never terminate:
  // npm's run-script prepends its own node-gyp bin dir to PATH *and* re-exports
  // npm_config_node_gyp pointing here, so a bare `node-gyp` finds npm's stub, which
  // re-execs this file, which falls back to a bare `node-gyp` again. Every hop holds
  // a live synchronous spawnSync, so the cycle consumes memory rather than CPU and is
  // not self-limiting (measured under nub's build jail: ~1500 processes, 15 GB, load
  // ~1). Bound the RE-ENTRY, not any single build's nesting: a genuine nested native
  // build is a couple of hops deep, a cycle is unbounded.
  const depth = Number(process.env.__NODE_GYP_SHIM_DEPTH_VAR__ || 0) + 1;
  if (depth > 3) {
    console.error(
      "__PROG__: node-gyp shim re-entered " + depth + " times without reaching a real " +
      "node-gyp — `node-gyp` on PATH resolves back to this shim. Refusing to recurse."
    );
    process.exit(1);
  }
  process.env.__NODE_GYP_SHIM_DEPTH_VAR__ = String(depth);
  real = isWin ? "node-gyp.cmd" : "node-gyp";
}
const result = spawnSync(real, process.argv.slice(2), { stdio: "inherit", shell: isWin });
if (result.error) {
  console.error("__PROG__: failed to run node-gyp (" + real + "): " + result.error.message);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
"#,
    );
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
        //    the fallback the bootstrap receives the var's own spelling as a
        //    path. `setlocal` keeps that fallback out of the caller's env.
        let cmd = render_shim(
            r#"@echo off
setlocal
set "__NODE_GYP_TOOL_VAR__=%~dp0..\__NODE_GYP_BUCKET__\node_modules"
if exist "%__NODE_GYP_TOOL_VAR__%\node-gyp\package.json" if exist "%__NODE_GYP_TOOL_VAR__%\.bin\node-gyp.cmd" (
  "%__NODE_GYP_TOOL_VAR__%\.bin\node-gyp.cmd" %*
  exit /b %ERRORLEVEL%
)
if not defined __NODE_GYP_EXE_VAR__ (
  echo __PROG__: no bootstrapped node-gyp and no bootstrap entry point>&2
  exit /b 1
)
if not defined __NODE_GYP_PROJECT_DIR_VAR__ set "__NODE_GYP_PROJECT_DIR_VAR__=%CD%"
for /f "usebackq delims=" %%i in (`""%__NODE_GYP_EXE_VAR__%" __node-gyp-bootstrap "%__NODE_GYP_PROJECT_DIR_VAR__%""`) do set "__REAL_NODE_GYP_VAR__=%%i"
if not defined __REAL_NODE_GYP_VAR__ exit /b 1
"%__REAL_NODE_GYP_VAR__%" %*
"#,
        );
        aube_util::fs_atomic::atomic_write(&shim_dir.join("node-gyp.cmd"), cmd.as_bytes())
            .into_diagnostic()?;
    }

    Ok(())
}

/// Materialize the synthetic single-package project that the bootstrap
/// install runs against. Writes are atomic and idempotent, so racing
/// processes converge on the same content; serialization is the caller's
/// project lock on `tool_dir`.
fn write_bootstrap_project(
    tool_dir: &Path,
    project_npmrc: &Path,
    spec: &str,
) -> miette::Result<()> {
    std::fs::create_dir_all(tool_dir).into_diagnostic()?;
    // The scratch manifest lands in the embedder's own cache tree, so even its
    // `name` follows the active brand rather than hardcoding the engine's.
    let manifest = format!(
        r#"{{"name":"{tool}-tool-node-gyp","private":true,"dependencies":{{"node-gyp":"{spec}"}}}}"#,
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

    /// Every write-time placeholder is substituted, and the plumbing vars are
    /// spelled with the ACTIVE embedder's `internal_env_prefix` rather than a
    /// hardcoded brand — the property the build jail's withhold policy depends on,
    /// since it scrubs those vars by name.
    ///
    /// Asserting "no `__…__` token survives" rather than listing the expected
    /// substitutions is the point: a token that goes unreplaced is not a lookup
    /// miss the shim recovers from, it is a shell/JS syntax error in a file
    /// written weeks before anything executes it, so the failure has no line back
    /// to `render_shim`. This catches a template that grows a placeholder the
    /// renderer does not know.
    #[test]
    fn generated_shims_leave_no_unsubstituted_placeholder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_lazy_shims(tmp.path()).expect("write shims");

        let exe_var = aube_util::env::internal_env_name("NODE_GYP_EXE");
        let names: &[&str] = if cfg!(windows) {
            &["node-gyp", "node-gyp.js", "node-gyp.cmd"]
        } else {
            &["node-gyp", "node-gyp.js"]
        };
        for name in names {
            let body = std::fs::read_to_string(tmp.path().join(name)).expect("read shim");
            assert!(
                !body.contains("__NODE_GYP") && !body.contains("__REAL_NODE_GYP"),
                "{name} still carries an unsubstituted placeholder:\n{body}"
            );
            assert!(
                body.contains(&exe_var),
                "{name} must name the trampoline var as {exe_var}"
            );
        }
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

    /// A bootstrapped tool tree beside the shim dir, tallying to its OWN marker so a
    /// test can assert WHICH node-gyp answered rather than merely that one did.
    /// Returns the tree's `node_modules`.
    #[cfg(unix)]
    fn staged_bootstrapped_tree(tmp: &std::path::Path, tally: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let modules = tmp
            .join(bucket_for(ambient_node_major().unwrap_or(u64::MAX)).0)
            .join("node_modules");
        std::fs::create_dir_all(modules.join(".bin")).expect("bin dir");
        std::fs::create_dir_all(modules.join(PKG)).expect("pkg dir");
        std::fs::write(modules.join(PKG).join("package.json"), b"{}").expect("pkg json");
        let bin = modules.join(".bin").join(primary_binary_name());
        std::fs::write(
            &bin,
            format!("#!/bin/sh\necho x >> '{}'\nexit 0\n", tally.display()),
        )
        .expect("write bootstrapped node-gyp");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        modules
    }

    #[cfg(unix)]
    fn tally_lines(tally: &std::path::Path) -> usize {
        std::fs::read_to_string(tally)
            .map(|t| t.lines().count())
            .unwrap_or(0)
    }

    #[cfg(unix)]
    fn run_shim(shim: &std::path::Path, bin_dir: &std::path::Path) -> std::process::Output {
        run_shim_with(shim, bin_dir, &[])
    }

    #[cfg(unix)]
    fn run_shim_with(
        shim: &std::path::Path,
        bin_dir: &std::path::Path,
        extra_env: &[(&str, &OsStr)],
    ) -> std::process::Output {
        let mut cmd = std::process::Command::new("node");
        cmd.arg(shim)
            .arg("rebuild")
            // No trampoline entry point: this is the bare-PATH fallback branch.
            // Named through the embedder so the removal tracks whatever brand the
            // shim was RENDERED with — a hardcoded spelling would silently stop
            // clearing the var under a host profile and leave the branch untested.
            .env_remove(aube_util::env::internal_env_name("NODE_GYP_EXE"))
            .env_remove(aube_util::env::internal_env_name("NODE_GYP_SHIM_DEPTH"))
            // `bin_dir` FIRST so a bare `node-gyp` hits the staged stub, but the
            // ambient PATH must stay reachable or the stub cannot find `node`.
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin_dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        cmd.output()
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
        let hops = tally_lines(&tally);
        assert_eq!(hops, 1, "the real node-gyp must run exactly once");
    }

    /// THE COMPAT DEFECT the re-entry guard only contained. A dependency whose install
    /// script reaches `npm run build` gets npm's run-script PATH: it prepends npm's own
    /// `node-gyp` stub AND re-exports `npm_config_node_gyp` pointing back here, so a
    /// shim that resolves by NAME cycles and NO native addon ever compiles — the guard
    /// then turns an unbounded fork bomb into a clean failure, which is still a failure.
    /// npm and pnpm never hit this because `npm_config_node_gyp` names a terminal entry
    /// script; this shim earns the same property by resolving the bootstrapped tree
    /// beside it. Two separate tallies are what make the assertion non-hollow — they
    /// say WHICH node-gyp answered, not merely that the run exited zero.
    #[cfg(unix)]
    #[test]
    fn the_shim_reaches_the_bootstrapped_tree_past_a_shadowing_npm_stub() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // npm's stub verbatim (`@npmcli/run-script/lib/node-gyp-bin/node-gyp`).
        let (shim, bin_dir, path_tally) =
            staged_path_node_gyp(tmp.path(), "exec node \"$npm_config_node_gyp\" \"$@\"");
        let boot_tally = tmp.path().join("bootstrapped-tally");
        staged_bootstrapped_tree(tmp.path(), &boot_tally);

        let out = run_shim_with(
            &shim,
            &bin_dir,
            &[("npm_config_node_gyp", shim.as_os_str())],
        );
        assert!(
            out.status.success(),
            "a shadowing npm stub must not stop the shim reaching node-gyp; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            tally_lines(&boot_tally),
            1,
            "the bootstrapped node-gyp must run exactly once"
        );
        assert_eq!(
            tally_lines(&path_tally),
            0,
            "PATH must not be consulted at all — it is the namespace npm rewrites"
        );
    }

    /// Why the probe checks the require root and not just the `.bin` entry. A global
    /// store purge leaves the generated shim standing and takes its target with it, so
    /// pinning to a bin-only hit would exec into `Cannot find module` with no route
    /// left. Falling through keeps the run recoverable — here onto a real PATH
    /// node-gyp, in production onto the bootstrap trampoline that rebuilds the tree.
    #[cfg(unix)]
    #[test]
    fn a_purged_store_does_not_pin_the_shim_to_a_dead_bootstrapped_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (shim, bin_dir, path_tally) = staged_path_node_gyp(tmp.path(), "exit 0");
        let boot_tally = tmp.path().join("bootstrapped-tally");
        let modules = staged_bootstrapped_tree(tmp.path(), &boot_tally);
        std::fs::remove_dir_all(modules.join(PKG)).expect("purge the store-backed require root");

        let out = run_shim(&shim, &bin_dir);
        assert!(
            out.status.success(),
            "a purged store must fall through, not fail; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            tally_lines(&path_tally),
            1,
            "the fallback must still forward"
        );
        assert_eq!(
            tally_lines(&boot_tally),
            0,
            "the dead bootstrapped entry must not be executed"
        );
    }
}
