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

/// `(major, minor)` of the `node` first on PATH. 22.15 is the fast-tier floor,
/// and one test below is a fast-tier-only contract.
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

fn on_fast_tier() -> bool {
    matches!(path_node_version(), Some((m, n)) if (m, n) >= (22, 15))
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
/// FAST TIER ONLY, and the reason is a separate pre-existing divergence rather
/// than anything this probe does. Below 22.15 nub registers a `.ts` handler in
/// `require.extensions`, so Node's OWN CJS resolver finds `sub.ts` during
/// LOAD_AS_FILE and never reaches LOAD_AS_DIRECTORY — the additive resolver has
/// already declined the file by then. Verified against nub v0.5.0, which predates
/// this branch and behaves identically on 20.11 and 18.19.
#[test]
fn directory_index_outranks_an_unloadable_ts_sibling() {
    if !on_fast_tier() {
        return;
    }
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
