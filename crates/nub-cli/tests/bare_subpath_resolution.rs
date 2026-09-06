//! Extension probing for bare package SUBPATHS (`pkg/sub`) — nubjs/nub#562.
//!
//! Node's ESM resolver gives a subpath no probing at all, so an extensionless
//! `pkg/sub` is a hard `ERR_MODULE_NOT_FOUND` there even where the CJS
//! `require()` of the same specifier works. nub probes it, but ONLY into a
//! dependency that declares no `exports` — the tests below pin both halves: the
//! probing that #562 asked for, and the `exports` encapsulation it must not
//! breach.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push(format!("nub{}", std::env::consts::EXE_SUFFIX));
    path
}

fn work_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nub-subpath-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A project with `thirdparty` (published CJS shape: no `exports`, a subpath
/// `.js`) and `exp` (an `exports` map that deliberately withholds `lib/private`).
fn fixture(name: &str) -> PathBuf {
    let dir = work_dir(name);
    write(
        &dir.join("package.json"),
        r#"{"name":"root","private":true}"#,
    );

    write(
        &dir.join("node_modules/thirdparty/package.json"),
        r#"{"name":"thirdparty","main":"index.js"}"#,
    );
    write(
        &dir.join("node_modules/thirdparty/index.js"),
        "module.exports={};",
    );
    write(
        &dir.join("node_modules/thirdparty/components/prism-python.js"),
        r#"module.exports={lang:"python"};"#,
    );

    write(
        &dir.join("node_modules/exp/package.json"),
        r#"{"name":"exp","main":"index.js","exports":{".":"./index.js"}}"#,
    );
    write(&dir.join("node_modules/exp/index.js"), "module.exports={};");
    write(
        &dir.join("node_modules/exp/lib/private.js"),
        "module.exports={};",
    );

    dir
}

/// `(major, minor)` of the `node` first on PATH. 22.15 is the fast-tier floor; the
/// classic `require.extensions` behavior one test below pins exists only under it.
#[cfg(unix)]
fn path_node_version() -> Option<(u32, u32)> {
    let out = Command::new("node").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout);
    let v = v.trim().trim_start_matches('v');
    let mut parts = v.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

#[cfg(unix)]
fn on_compat_tier() -> bool {
    matches!(path_node_version(), Some((m, n)) if (m, n) < (22, 15))
}

fn run(dir: &Path, entry: &str) -> (String, String) {
    let out = Command::new(nub_binary())
        .arg(dir.join(entry).to_str().unwrap())
        .current_dir(dir)
        .env(
            "XDG_CACHE_HOME",
            std::env::temp_dir().join(format!("nub-subpath-cache-{}", std::process::id())),
        )
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn extensionless_subpath_into_exportless_package_resolves() {
    let dir = fixture("thirdparty");
    write(
        &dir.join("entry.ts"),
        r#"import p from "thirdparty/components/prism-python";
console.log("lang:" + p.lang);
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.ts");
    assert_eq!(stdout, "lang:python", "stderr: {stderr}");
}

/// The `.js`→`.ts` emit-convention swap, which TypeScript's `module: node16`
/// guidance tells users to write, must apply across a subpath too — not just
/// relative specifiers.
///
/// The target has to be a symlinked WORKSPACE package rather than a published
/// one, and that is a property of the feature, not a quirk of the fixture: a
/// `.ts` sitting in a real `node_modules` directory is unshipped source that
/// nub declines to transpile, so swapping onto it would resolve a file that
/// then fails to load. The swap is only reachable where the `.ts` is genuinely
/// the built artifact — i.e. a workspace package.
#[cfg(unix)]
#[test]
fn js_specifier_swaps_to_ts_across_a_subpath() {
    let dir = fixture("emit-swap");
    write(
        &dir.join("packages/ui/package.json"),
        r#"{"name":"@repro/ui","main":"./index.ts"}"#,
    );
    write(&dir.join("packages/ui/index.ts"), "export const root = 1;");
    write(&dir.join("packages/ui/deep/mod.ts"), "export const v = 7;");
    std::fs::create_dir_all(dir.join("node_modules/@repro")).unwrap();
    std::os::unix::fs::symlink(dir.join("packages/ui"), dir.join("node_modules/@repro/ui"))
        .unwrap();
    write(
        &dir.join("entry.ts"),
        r#"import { v } from "@repro/ui/deep/mod.js";
console.log("v:" + v);
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.ts");
    assert_eq!(stdout, "v:7", "stderr: {stderr}");
}

/// A published package that ships BOTH a built `.js` and its unshipped `.ts`
/// source must resolve to the `.js`. nub declines to transpile anything under a
/// `node_modules/` path, so preferring the `.ts` would resolve to a file that
/// then fails to load.
#[test]
fn built_js_wins_over_unshipped_ts_source() {
    let dir = fixture("probe-order");
    write(
        &dir.join("node_modules/dual/package.json"),
        r#"{"name":"dual","main":"index.js"}"#,
    );
    write(
        &dir.join("node_modules/dual/index.js"),
        "module.exports={};",
    );
    write(
        &dir.join("node_modules/dual/sub.js"),
        r#"module.exports={from:"js"};"#,
    );
    write(
        &dir.join("node_modules/dual/sub.ts"),
        r#"export default {from:"ts"};"#,
    );
    write(
        &dir.join("entry.ts"),
        r#"import s from "dual/sub";
console.log("from:" + s.from);
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.ts");
    assert_eq!(stdout, "from:js", "stderr: {stderr}");
}

/// A `.ts` that outranks a sibling DIRECTORY Node resolves today would be a
/// working-to-broken regression: probing settles on a file before it considers a
/// directory, so `sub.ts` would beat `sub/index.js`, and the `.ts` is then a path
/// the load hooks refuse. Node's CJS resolver loads `sub/index.js` here, and so
/// must nub.
///
/// Runs on EVERY tier. Below 22.15 the classic `require.extensions` handlers put
/// the TS family into `Module._extensions`, which Node's `_findPath` walks during
/// LOAD_AS_FILE before it ever tries LOAD_AS_DIRECTORY — so this used to hand back
/// a dependency's unshipped `sub.ts` instead. The require-side resolver now
/// re-resolves without those keys whenever Node's answer is a TS file under
/// node_modules, which is what keeps the two tiers agreeing here.
#[test]
fn directory_index_outranks_an_unloadable_ts_sibling() {
    let dir = fixture("dir-vs-ts");
    write(
        &dir.join("node_modules/pkg/package.json"),
        r#"{"name":"pkg","main":"index.js"}"#,
    );
    write(&dir.join("node_modules/pkg/index.js"), "module.exports={};");
    write(&dir.join("node_modules/pkg/sub.ts"), "export const x = 1;");
    write(
        &dir.join("node_modules/pkg/sub/index.js"),
        r#"module.exports={who:"dir-index"};"#,
    );
    write(
        &dir.join("entry.cts"),
        r#"const m = require("pkg/sub");
console.log("who:" + m.who);
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.cts");
    assert_eq!(stdout, "who:dir-index", "stderr: {stderr}");
}

/// A dependency's own relative require must not reach its unshipped TS source
/// either. Plain Node reports `./util` missing when only `util.ts` exists, and so
/// must nub — deps are never transpiled, on any tier. Below 22.15 the classic
/// `require.extensions` keys made this resolve and load, silently.
#[test]
fn dep_internal_relative_require_does_not_reach_ts_source() {
    let dir = fixture("dep-internal-ts");
    write(
        &dir.join("node_modules/depts/package.json"),
        r#"{"name":"depts","main":"index.js"}"#,
    );
    write(
        &dir.join("node_modules/depts/index.js"),
        r#"const u = require("./util");
module.exports = { who: u.who };
"#,
    );
    write(
        &dir.join("node_modules/depts/util.ts"),
        r#"const v: string = "unshipped";
module.exports = { who: v };
"#,
    );
    write(
        &dir.join("entry.cts"),
        r#"try {
  console.log("who:" + require("depts").who);
} catch (e) {
  console.log("threw:" + (e as { code?: string }).code);
}
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.cts");
    assert!(
        !stdout.contains("who:unshipped"),
        "a dep's relative require reached its unshipped TS source: {stdout} / {stderr}"
    );
    assert!(
        stdout.contains("threw:MODULE_NOT_FOUND"),
        "expected Node's own MODULE_NOT_FOUND, got: {stdout} / {stderr}"
    );
}

/// The dependency test has to decide on where a file REALLY lives. Under
/// `--preserve-symlinks` Node hands back the un-realpathed path, so a workspace
/// package symlinked into node_modules still carries a `/node_modules/` segment —
/// and treating it as a dependency would cost it the TypeScript source that is its
/// actual build output.
///
/// COMPAT TIER ONLY, because that is the only band where the behavior exists at
/// all: resolving an extensionless `main` onto TS source depends on the classic
/// `require.extensions` keys, which the fast tier never registers — Node 26 reports
/// the same package missing both before and after this branch.
///
/// COUPLING with the load side: every gate that decides how a file loads — the
/// classic `require.extensions` handler included — goes through the shared
/// `isDependency` predicate, so all of them judge the file's REAL location. Testing
/// the literal filename in any of them would break this test and quietly undo the
/// resolution-side fix.
#[cfg(unix)]
#[test]
fn preserve_symlinks_keeps_a_workspace_package_loadable() {
    if !on_compat_tier() {
        eprintln!("skipping: needs the compat tier (PATH node below 22.15)");
        return;
    }
    let dir = fixture("preserve-symlinks");
    write(
        &dir.join("packages/ws/package.json"),
        r#"{"name":"@repro/ws","main":"./index"}"#,
    );
    write(
        &dir.join("packages/ws/index.ts"),
        r#"const v: string = "ws-source";
module.exports = { who: v };
"#,
    );
    std::fs::create_dir_all(dir.join("node_modules/@repro")).unwrap();
    std::os::unix::fs::symlink(dir.join("packages/ws"), dir.join("node_modules/@repro/ws"))
        .unwrap();
    write(
        &dir.join("entry.cjs"),
        r#"console.log("who:" + require("@repro/ws").who);
"#,
    );
    let out = Command::new(nub_binary())
        .arg(dir.join("entry.cjs").to_str().unwrap())
        .current_dir(&dir)
        .env("NODE_OPTIONS", "--preserve-symlinks")
        .env(
            "XDG_CACHE_HOME",
            std::env::temp_dir().join(format!("nub-subpath-cache-{}", std::process::id())),
        )
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("who:ws-source"),
        "workspace package was treated as a dependency under --preserve-symlinks: {stdout} / {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Naming a dependency's TypeScript by explicit path skips resolution entirely, so
/// the "deps are never transpiled" invariant has to hold at the LOAD step too. nub
/// hands the file to Node's own `.js` handler — the same fallback Node uses for an
/// extension it does not register — so the failure is Node's, not one nub invented.
/// The exact error differs by version (a raw-JS `SyntaxError` below 22.15, type-
/// stripping's own refusal above it), so this pins the invariant: the dependency's
/// TypeScript never becomes a loaded module.
#[test]
fn explicit_path_require_of_dep_ts_is_not_transpiled() {
    let dir = fixture("explicit-dep-ts");
    write(
        &dir.join("node_modules/xp/package.json"),
        r#"{"name":"xp","main":"index.js"}"#,
    );
    write(&dir.join("node_modules/xp/index.js"), "module.exports={};");
    write(
        &dir.join("node_modules/xp/inner.ts"),
        r#"const v: string = "dep-ts-explicit";
module.exports = { who: v };
"#,
    );
    write(
        &dir.join("entry.cjs"),
        r#"try {
  console.log("who:" + require("./node_modules/xp/inner.ts").who);
} catch {
  console.log("refused");
}
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.cjs");
    assert!(
        !stdout.contains("who:dep-ts-explicit"),
        "a dependency's TypeScript was transpiled through an explicit path: {stdout} / {stderr}"
    );
    assert!(
        stdout.contains("refused"),
        "the require neither loaded nor threw — the run produced nothing: {stdout} / {stderr}"
    );
}

/// A published package shipping ONLY TypeScript source must surface Node's own
/// ERR_MODULE_NOT_FOUND. Resolving the `.ts` would hand back a file nub declines
/// to transpile, turning a clear "no such module" into a type-stripping error
/// that points the reader at the wrong problem.
#[test]
fn ts_only_published_package_keeps_nodes_own_error() {
    let dir = fixture("ts-only");
    write(
        &dir.join("node_modules/tsonly/package.json"),
        r#"{"name":"tsonly","main":"index.js"}"#,
    );
    write(
        &dir.join("node_modules/tsonly/index.js"),
        "module.exports={};",
    );
    write(
        &dir.join("node_modules/tsonly/deep.ts"),
        "export const v = 1;",
    );
    write(
        &dir.join("entry.ts"),
        r#"import "tsonly/deep";
console.log("LOADED");
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.ts");
    assert!(!stdout.contains("LOADED"), "unexpectedly loaded: {stdout}");
    assert!(
        stderr.contains("ERR_MODULE_NOT_FOUND")
            && !stderr.contains("ERR_UNSUPPORTED_NODE_MODULES_TYPE_STRIPPING"),
        "expected Node's own ERR_MODULE_NOT_FOUND, got: {stderr}"
    );
}

/// A traversing subpath must never resolve outside the package root. The guard
/// splits on both separators, since lexical normalization honors `\` on Windows;
/// this pins the separator-independent half on every platform.
#[test]
fn traversing_subpath_never_escapes_the_package_root() {
    let dir = fixture("traversal");
    write(&dir.join("outside-secret.js"), r#"module.exports={};"#);
    write(
        &dir.join("entry.ts"),
        r#"import "thirdparty/../../outside-secret";
console.log("ESCAPED");
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.ts");
    assert!(
        !stdout.contains("ESCAPED"),
        "a traversing subpath escaped the package root: {stdout}"
    );
    assert!(
        stderr.contains("ERR_"),
        "expected the traversal to stay unresolved, got: {stderr}"
    );
}

/// The emit swaps reach `.mts` and `.cts` as well as `.ts`/`.tsx`, and a swap that
/// lands on one inside a dependency is exactly as unloadable — so the discard has to
/// cover the whole TypeScript family, not just what the probe order can hit.
#[test]
fn emit_swap_onto_dep_mts_and_cts_is_discarded() {
    let dir = fixture("mts-cts-discard");
    write(
        &dir.join("node_modules/mp/package.json"),
        r#"{"name":"mp","main":"index.js"}"#,
    );
    write(&dir.join("node_modules/mp/index.js"), "module.exports={};");
    write(
        &dir.join("node_modules/mp/deep.mts"),
        r#"export const v: string = "unshipped-mts";"#,
    );
    write(
        &dir.join("node_modules/mp/other.cts"),
        r#"const v: string = "unshipped-cts";
module.exports = { v };
"#,
    );
    write(
        &dir.join("entry.ts"),
        r#"import { v } from "mp/deep.mjs";
console.log("LOADED:" + v);
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.ts");
    assert!(!stdout.contains("LOADED"), "unexpectedly loaded: {stdout}");
    assert!(
        stderr.contains("ERR_MODULE_NOT_FOUND")
            && !stderr.contains("ERR_UNSUPPORTED_NODE_MODULES_TYPE_STRIPPING"),
        "expected Node's own ERR_MODULE_NOT_FOUND for .mts, got: {stderr}"
    );

    // The symmetric `.cjs`→`.cts` arm, through require so it exercises the CJS path.
    write(
        &dir.join("entry.cts"),
        r#"try {
  console.log("LOADED:" + require("mp/other.cjs").v);
} catch {
  console.log("refused");
}
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.cts");
    assert!(
        !stdout.contains("LOADED"),
        "the .cjs→.cts swap reached a dependency's unshipped source: {stdout} / {stderr}"
    );
    assert!(
        stdout.contains("refused"),
        "the require neither loaded nor threw: {stdout} / {stderr}"
    );
}

/// Under `--preserve-symlinks` Node keys a symlinked workspace package by the path
/// it was reached through, so the probe has to hand back that same path. Returning
/// the real one instead keys the SAME file two ways and the module instantiates
/// twice — silently, since both halves "work".
#[cfg(unix)]
#[test]
fn preserve_symlinks_does_not_duplicate_a_workspace_module() {
    let dir = fixture("dual-instance");
    write(
        &dir.join("packages/jslib/package.json"),
        r#"{"name":"@repro/jslib","main":"./index.js"}"#,
    );
    write(
        &dir.join("packages/jslib/index.js"),
        r#"const s = require("./state");
module.exports = { id: s.id };
"#,
    );
    write(
        &dir.join("packages/jslib/state.js"),
        r#"module.exports = { id: {} };"#,
    );
    std::fs::create_dir_all(dir.join("node_modules/@repro")).unwrap();
    std::os::unix::fs::symlink(
        dir.join("packages/jslib"),
        dir.join("node_modules/@repro/jslib"),
    )
    .unwrap();
    write(
        &dir.join("entry.cjs"),
        r#"const viaBare = require("@repro/jslib").id;
const viaSubpath = require("@repro/jslib/state").id;
console.log("same:" + (viaBare === viaSubpath));
"#,
    );
    let out = Command::new(nub_binary())
        .arg(dir.join("entry.cjs").to_str().unwrap())
        .current_dir(&dir)
        .env("NODE_OPTIONS", "--preserve-symlinks")
        .env(
            "XDG_CACHE_HOME",
            std::env::temp_dir().join(format!("nub-subpath-cache-{}", std::process::id())),
        )
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("same:true"),
        "the same module was instantiated twice under --preserve-symlinks: {stdout} / {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The #562 headline shape must survive `--preserve-symlinks` on every tier. This is
/// the combination the resolver and the load hooks have to agree on: the resolver
/// deliberately keys the module by its symlink path so it is not instantiated twice,
/// and every load gate has to recognise that same path as project source anyway.
/// Deciding it on the literal URL in either place refuses the workspace package's
/// TypeScript — which is exactly the import #562 was filed about.
#[cfg(unix)]
#[test]
fn preserve_symlinks_still_loads_a_workspace_ts_subpath() {
    let dir = fixture("preserve-symlinks-ts");
    write(
        &dir.join("packages/lib/package.json"),
        r#"{"name":"@repro/lib","main":"./index.ts"}"#,
    );
    write(&dir.join("packages/lib/index.ts"), "export const root = 1;");
    write(
        &dir.join("packages/lib/prompt.ts"),
        r#"export const x: string = "ws-ts-subpath";"#,
    );
    std::fs::create_dir_all(dir.join("node_modules/@repro")).unwrap();
    std::os::unix::fs::symlink(
        dir.join("packages/lib"),
        dir.join("node_modules/@repro/lib"),
    )
    .unwrap();
    write(
        &dir.join("entry.ts"),
        r#"import { x } from "@repro/lib/prompt";
console.log("got:" + x);
"#,
    );
    let out = Command::new(nub_binary())
        .arg(dir.join("entry.ts").to_str().unwrap())
        .current_dir(&dir)
        .env("NODE_OPTIONS", "--preserve-symlinks")
        .env(
            "XDG_CACHE_HOME",
            std::env::temp_dir().join(format!("nub-subpath-cache-{}", std::process::id())),
        )
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("got:ws-ts-subpath"),
        "a symlinked workspace package's TS subpath was refused under --preserve-symlinks: {stdout} / {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every other `--preserve-symlinks` fixture here uses the workspace package as the
/// import TARGET. This one makes it the IMPORTER, which is the other half: the
/// resolver has to classify the importing file the same way the load gates do, or a
/// symlinked workspace package reads as a dependency and its own imports silently
/// lose the additive resolution they get without the flag.
#[cfg(unix)]
#[test]
fn preserve_symlinks_keeps_additive_resolution_inside_a_workspace_package() {
    let dir = fixture("ws-importer");
    write(
        &dir.join("packages/lib/package.json"),
        r#"{"name":"@repro/lib","main":"./index.ts"}"#,
    );
    write(
        &dir.join("packages/lib/index.ts"),
        r#"import p from "thirdparty/components/prism-python";
export const lang: string = p.lang;
"#,
    );
    std::fs::create_dir_all(dir.join("node_modules/@repro")).unwrap();
    std::os::unix::fs::symlink(
        dir.join("packages/lib"),
        dir.join("node_modules/@repro/lib"),
    )
    .unwrap();
    write(
        &dir.join("entry.ts"),
        r#"import { lang } from "@repro/lib";
console.log("lang:" + lang);
"#,
    );
    let out = Command::new(nub_binary())
        .arg(dir.join("entry.ts").to_str().unwrap())
        .current_dir(&dir)
        .env("NODE_OPTIONS", "--preserve-symlinks")
        .env(
            "XDG_CACHE_HOME",
            std::env::temp_dir().join(format!("nub-subpath-cache-{}", std::process::id())),
        )
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("lang:python"),
        "a symlinked workspace package lost additive resolution for its own imports: {stdout} / {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A trailing `/` names a DIRECTORY, and probing must not answer it with a sibling
/// file. Node's CJS resolver goes straight to LOAD_AS_DIRECTORY here, so
/// `require("pkg/sub/")` is `sub/index.js` — never `sub.js`. Lexical normalization
/// drops the empty component, which is exactly how this went wrong: silently, with
/// no error, returning a different module than Node loads.
#[test]
fn a_trailing_slash_still_means_the_directory() {
    let dir = fixture("trailing-slash");
    write(
        &dir.join("node_modules/ts/package.json"),
        r#"{"name":"ts","main":"index.js"}"#,
    );
    write(
        &dir.join("node_modules/ts/index.js"),
        r#"module.exports="IDX";"#,
    );
    write(
        &dir.join("node_modules/ts/sub.js"),
        r#"module.exports="THE-FILE";"#,
    );
    write(
        &dir.join("node_modules/ts/sub/index.js"),
        r#"module.exports="THE-DIR-INDEX";"#,
    );
    // A sibling file with no directory at all: Node reports it missing, and so must nub.
    write(
        &dir.join("node_modules/ts/lone.js"),
        r#"module.exports="LONE";"#,
    );
    write(
        &dir.join("entry.cjs"),
        r#"console.log("dir:" + require("ts/sub/"));
try {
  console.log("lone:" + require("ts/lone/"));
} catch (e) {
  console.log("lone:" + e.code);
}
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.cjs");
    assert!(
        stdout.contains("dir:THE-DIR-INDEX"),
        "a trailing-slash directory specifier resolved to a sibling file: {stdout} / {stderr}"
    );
    assert!(
        stdout.contains("lone:MODULE_NOT_FOUND"),
        "a trailing-slash specifier resolved a file Node reports missing: {stdout} / {stderr}"
    );
}

/// The same directory rule on the RELATIVE branch, which had it too: `./sub/` names
/// a directory, so it must never be answered with a sibling `./sub.ts`. An interior
/// dot-segment is a different matter and must still resolve — Node normalizes
/// `./d/./x` and loads it — which is why only the LAST segment decides.
#[test]
fn a_trailing_slash_means_the_directory_for_relative_specifiers_too() {
    let dir = fixture("relative-trailing-slash");
    write(&dir.join("sub.ts"), r#"export const w = "SIBLING-FILE";"#);
    write(
        &dir.join("sub/index.ts"),
        r#"export const w = "DIR-INDEX";"#,
    );
    write(&dir.join("d/x.ts"), r#"export const w = "INTERIOR-OK";"#);
    write(
        &dir.join("entry.ts"),
        r#"const out: string[] = [];
try {
  const m = await import("./sub/");
  out.push("dir:" + m.w);
} catch (e) {
  out.push("dir:" + (e as { code?: string }).code);
}
const i = await import("./d/./x");
out.push("interior:" + i.w);
const p = await import("./sub");
out.push("plain:" + p.w);
console.log(out.join(" "));
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.ts");
    assert!(
        !stdout.contains("dir:SIBLING-FILE"),
        "a trailing-slash relative specifier resolved a sibling file: {stdout} / {stderr}"
    );
    assert!(
        stdout.contains("dir:ERR_UNSUPPORTED_DIR_IMPORT"),
        "expected Node's own directory-import error: {stdout} / {stderr}"
    );
    assert!(
        stdout.contains("interior:INTERIOR-OK"),
        "an interior dot-segment must still resolve: {stdout} / {stderr}"
    );
    assert!(
        stdout.contains("plain:SIBLING-FILE"),
        "an ordinary relative TS import must be untouched: {stdout} / {stderr}"
    );
}

/// THE ENCAPSULATION GUARD. A package that declares `exports` owns its subpath
/// map; probing must never reach a path it withheld. Without this, #562's fix
/// would be a sandbox escape out of every `exports` map on disk.
#[test]
fn exports_map_subpath_is_never_probed() {
    let dir = fixture("exports-guard");
    write(
        &dir.join("entry.ts"),
        r#"import "exp/lib/private";
console.log("LEAKED");
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.ts");
    assert!(
        !stdout.contains("LEAKED"),
        "probe breached an exports map: {stdout}"
    );
    assert!(
        stderr.contains("ERR_PACKAGE_PATH_NOT_EXPORTED"),
        "expected Node's exports error, got: {stderr}"
    );
}

/// `"exports": null` is the deliberate "export nothing" form, NOT an absent map.
/// Treating it as absent would probe straight past a package that blocked every
/// subpath on purpose.
#[test]
fn null_exports_blocks_probing() {
    let dir = fixture("null-exports");
    write(
        &dir.join("node_modules/sealed/package.json"),
        r#"{"name":"sealed","main":"index.js","exports":null}"#,
    );
    write(
        &dir.join("node_modules/sealed/index.js"),
        "module.exports={};",
    );
    write(
        &dir.join("node_modules/sealed/inner.js"),
        "module.exports={};",
    );
    write(
        &dir.join("entry.ts"),
        r#"import "sealed/inner";
console.log("LEAKED");
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.ts");
    assert!(
        !stdout.contains("LEAKED"),
        "probed past an `exports: null` package: {stdout}"
    );
    // Node reports the sealed subpath as ERR_MODULE_NOT_FOUND, not
    // ERR_PACKAGE_PATH_NOT_EXPORTED (verified against plain `node` on the same
    // fixture) — the point is that nub defers and Node's own error survives.
    assert!(
        stderr.contains("ERR_MODULE_NOT_FOUND"),
        "expected Node's own error to survive, got: {stderr}"
    );
}

/// A workspace package symlinked into `node_modules` with `main` pointing at TS
/// source — the shape #562 was filed against. Also pins the realpath step: the
/// probe hits a path under `node_modules/`, and only resolving the symlink moves
/// it somewhere nub will transpile.
#[cfg(unix)]
#[test]
fn symlinked_workspace_package_subpath_resolves_to_ts_source() {
    let dir = fixture("workspace");
    write(
        &dir.join("packages/lib/package.json"),
        r#"{"name":"@repro/lib","main":"./index.ts"}"#,
    );
    write(&dir.join("packages/lib/index.ts"), "export const root = 1;");
    write(
        &dir.join("packages/lib/prompt.ts"),
        r#"export const createPrompt = (): string => "hi";"#,
    );
    std::fs::create_dir_all(dir.join("node_modules/@repro")).unwrap();
    std::os::unix::fs::symlink(
        dir.join("packages/lib"),
        dir.join("node_modules/@repro/lib"),
    )
    .unwrap();
    write(
        &dir.join("entry.ts"),
        r#"import { createPrompt } from "@repro/lib/prompt";
console.log("prompt:" + createPrompt());
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.ts");
    assert_eq!(stdout, "prompt:hi", "stderr: {stderr}");
}

/// `require()` of the same shape. Node's CJS resolver probes `.js`/`.json` but
/// never `.ts`, so the TS half of the gap exists on the require side too.
#[cfg(unix)]
#[test]
fn require_of_a_ts_subpath_resolves() {
    let dir = fixture("require-ts");
    write(
        &dir.join("packages/lib/package.json"),
        r#"{"name":"@repro/lib","main":"./index.ts"}"#,
    );
    write(&dir.join("packages/lib/index.ts"), "export const root = 1;");
    write(
        &dir.join("packages/lib/prompt.ts"),
        "module.exports = { tag: 9 };",
    );
    std::fs::create_dir_all(dir.join("node_modules/@repro")).unwrap();
    std::os::unix::fs::symlink(
        dir.join("packages/lib"),
        dir.join("node_modules/@repro/lib"),
    )
    .unwrap();
    write(
        &dir.join("entry.cts"),
        r#"const m = require("@repro/lib/prompt");
console.log("tag:" + m.tag);
"#,
    );
    let (stdout, stderr) = run(&dir, "entry.cts");
    assert_eq!(stdout, "tag:9", "stderr: {stderr}");
}
