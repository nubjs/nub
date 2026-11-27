//! A workspace package whose `main`, `exports` or `imports` names its tsconfig
//! `outDir` runs from source before anything is built: the missing output maps to
//! the same path under `rootDir`, as `tsc` maps it. Only once Node has failed, only
//! through the owning package's own tsconfig, and never inside `node_modules`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn work_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nub-outdir-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A monorepo root with `packages/lib` linked into `node_modules/<name>`, the way
/// every workspace-aware package manager installs it.
#[cfg(unix)]
fn workspace(name: &str, lib_manifest: &str, lib_tsconfig: &str) -> PathBuf {
    let dir = work_dir(name);
    write(&dir.join("package.json"), r#"{"private":true}"#);
    write(&dir.join("packages/lib/package.json"), lib_manifest);
    write(&dir.join("packages/lib/tsconfig.json"), lib_tsconfig);
    let pkg: serde_json::Value = serde_json::from_str(lib_manifest).unwrap();
    let link = dir.join("node_modules").join(pkg["name"].as_str().unwrap());
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(dir.join("packages/lib"), link).unwrap();
    dir
}

const OWN_DIRS: &str = r#"{"compilerOptions":{"rootDir":"src","outDir":"dist"}}"#;

/// `(stdout, stderr)` of `nub <entry>` run from `dir`.
fn run(dir: &Path, entry: &str) -> (String, String) {
    run_with(dir, entry, &[])
}

fn run_with(dir: &Path, entry: &str, envs: &[(&str, &str)]) -> (String, String) {
    let out = Command::new(nub_binary())
        .arg(entry)
        .current_dir(dir)
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .envs(envs.iter().copied())
        .env(
            "XDG_CACHE_HOME",
            std::env::temp_dir().join(format!("nub-outdir-cache-{}", std::process::id())),
        )
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Both module systems reach the same source: `import` through Node's ESM legacy
/// `main` (whose error names no path) and `require()` through CommonJS.
#[cfg(unix)]
#[test]
fn unbuilt_main_runs_the_source_under_import_and_require() {
    let dir = workspace("main", r#"{"name":"lib","main":"dist/index.js"}"#, OWN_DIRS);
    write(
        &dir.join("packages/lib/src/index.ts"),
        r#"exports.from = "src/index.ts" as string;"#,
    );
    write(
        &dir.join("app.ts"),
        r#"import lib from "lib"; console.log("import:" + lib.from);"#,
    );
    write(
        &dir.join("app.cts"),
        r#"const lib = require("lib"); console.log("require:" + lib.from);"#,
    );
    let (stdout, stderr) = run(&dir, "app.ts");
    assert_eq!(stdout, "import:src/index.ts", "stderr: {stderr}");
    let (stdout, stderr) = run(&dir, "app.cts");
    assert_eq!(stdout, "require:src/index.ts", "stderr: {stderr}");
    // Kept on its path through the link, the source is still the workspace's, not a
    // dependency's.
    let (stdout, stderr) = run_with(&dir, "app.cts", &[("NODE_PRESERVE_SYMLINKS", "1")]);
    assert_eq!(stdout, "require:src/index.ts", "stderr: {stderr}");
}

/// Node still evaluates `exports` and its conditions: each condition's target maps
/// to its own source, `.mjs` to `.mts` and `.cjs` to `.cts`.
#[cfg(unix)]
#[test]
fn each_exports_condition_maps_its_own_target() {
    let dir = workspace(
        "conditions",
        r#"{"name":"lib","exports":{".":{"import":"./dist/index.mjs","require":"./dist/index.cjs"}}}"#,
        OWN_DIRS,
    );
    write(
        &dir.join("packages/lib/src/index.mts"),
        r#"export const which: string = "index.mts";"#,
    );
    write(
        &dir.join("packages/lib/src/index.cts"),
        r#"exports.which = "index.cts" as string;"#,
    );
    write(
        &dir.join("app.mts"),
        r#"import { which } from "lib";
import { createRequire } from "node:module";
console.log(which + "|" + createRequire(import.meta.url)("lib").which);
"#,
    );
    let (stdout, stderr) = run(&dir, "app.mts");
    assert_eq!(stdout, "index.mts|index.cts", "stderr: {stderr}");
    // Node takes `--preserve-symlinks=<anything>` as the bare flag, and the sources
    // reached through the link must still compile.
    let (stdout, stderr) = run_with(
        &dir,
        "app.mts",
        &[("NODE_OPTIONS", "--preserve-symlinks=true")],
    );
    assert_eq!(stdout, "index.mts|index.cts", "stderr: {stderr}");
}

/// A symlinked `outDir` belongs to the package it points into: its source answers,
/// as its build would, not a same-named file beside the link.
#[cfg(unix)]
#[test]
fn symlinked_out_dir_maps_in_the_package_it_points_into() {
    let dir = work_dir("linked-dist");
    for (pkg, imports) in [
        ("a", r##","imports":{"#x":"./dist/index.mjs"}"##),
        ("b", ""),
    ] {
        write(
            &dir.join(pkg).join("package.json"),
            &format!(r#"{{"name":"{pkg}","type":"module"{imports}}}"#),
        );
        write(&dir.join(pkg).join("tsconfig.json"), OWN_DIRS);
        write(
            &dir.join(pkg).join("src/index.mts"),
            &format!(r#"export default "{pkg}/src/index.mts" as string;"#),
        );
    }
    std::fs::create_dir_all(dir.join("b/dist")).unwrap();
    std::os::unix::fs::symlink("../b/dist", dir.join("a/dist")).unwrap();
    write(
        &dir.join("a/main.mts"),
        r##"import x from "#x"; console.log(x);"##,
    );
    let (stdout, stderr) = run(&dir.join("a"), "main.mts");
    assert_eq!(stdout, "b/src/index.mts", "stderr: {stderr}");
}

/// The package's own `imports` map pointing into `outDir` (`"#~/*": "./dist/*"`).
#[test]
fn imports_target_in_out_dir_maps_to_root_dir() {
    let dir = work_dir("imports");
    write(
        &dir.join("package.json"),
        r##"{"type":"module","imports":{"#~/*":"./dist/*"}}"##,
    );
    write(&dir.join("tsconfig.json"), OWN_DIRS);
    write(
        &dir.join("src/main.ts"),
        r##"import example from "#~/example/example.js"; console.log(example);"##,
    );
    write(
        &dir.join("src/example/example.ts"),
        r#"export default "src/example/example.ts" as string;"#,
    );
    let (stdout, stderr) = run(&dir, "src/main.ts");
    assert_eq!(stdout, "src/example/example.ts", "stderr: {stderr}");
}

/// `nub dist/index.js`, the `start` script a package runs from its build.
#[test]
fn entry_in_out_dir_runs_its_source() {
    let dir = work_dir("entry");
    write(&dir.join("package.json"), r#"{"type":"module"}"#);
    write(&dir.join("tsconfig.json"), OWN_DIRS);
    write(
        &dir.join("src/index.ts"),
        r#"const from: string = "src/index.ts"; console.log(from);"#,
    );
    let (stdout, stderr) = run(&dir, "dist/index.js");
    assert_eq!(stdout, "src/index.ts", "stderr: {stderr}");
}

/// A built file is never second-guessed, the `imports` it uses included.
#[cfg(unix)]
#[test]
fn built_output_wins_over_source() {
    let dir = workspace(
        "built",
        r##"{"name":"lib","type":"module","main":"dist/index.js","imports":{"#b":"./dist/b.js"}}"##,
        OWN_DIRS,
    );
    for (file, body) in [
        ("src/index.ts", r##"export { b as v } from "#b";"##),
        ("src/b.ts", r#"export const b = "source";"#),
        ("dist/index.js", r##"export { b as v } from "#b";"##),
        ("dist/b.js", r#"export const b = "built";"#),
    ] {
        write(&dir.join("packages/lib").join(file), body);
    }
    write(
        &dir.join("app.ts"),
        r#"import { v } from "lib"; console.log(v);"#,
    );
    let (stdout, stderr) = run(&dir, "app.ts");
    assert_eq!(stdout, "built", "stderr: {stderr}");
}

/// An `outDir` inherited through `extends` resolves against the config that
/// declares it, as in `tsc`: here the monorepo root's `build/`, which the package's
/// `main: "build"` is not inside, so Node's error stands. Setting `outDir` in the
/// package's own config is what maps it.
#[cfg(unix)]
#[test]
fn inherited_out_dir_belongs_to_the_declaring_config() {
    let dir = workspace(
        "inherited",
        r#"{"name":"lib","main":"build"}"#,
        r#"{"extends":"../../tsconfig.json","compilerOptions":{"rootDir":"src"}}"#,
    );
    write(
        &dir.join("tsconfig.json"),
        r#"{"compilerOptions":{"outDir":"build"}}"#,
    );
    write(
        &dir.join("packages/lib/src/index.ts"),
        r#"exports.from = "src/index.ts";"#,
    );
    write(
        &dir.join("app.cts"),
        r#"console.log("from:" + require("lib").from);"#,
    );
    let (stdout, stderr) = run(&dir, "app.cts");
    assert!(
        stdout.is_empty() && stderr.contains("Cannot find module"),
        "the root's outDir must not map the package's build/: stdout {stdout:?}, stderr {stderr}"
    );

    write(
        &dir.join("packages/lib/tsconfig.json"),
        r#"{"extends":"../../tsconfig.json","compilerOptions":{"rootDir":"src","outDir":"build"}}"#,
    );
    let (stdout, stderr) = run(&dir, "app.cts");
    assert_eq!(stdout, "from:src/index.ts", "stderr: {stderr}");
}

/// A package installed into `node_modules` is a dependency, and dependencies are
/// never transpiled: its missing build stays Node's error.
#[test]
fn dependency_in_node_modules_never_maps() {
    let dir = work_dir("dependency");
    write(&dir.join("package.json"), r#"{"private":true}"#);
    write(
        &dir.join("node_modules/dep/package.json"),
        r#"{"name":"dep","main":"dist/index.js"}"#,
    );
    write(&dir.join("node_modules/dep/tsconfig.json"), OWN_DIRS);
    write(
        &dir.join("node_modules/dep/src/index.ts"),
        r#"exports.from = "src";"#,
    );
    write(&dir.join("app.cts"), r#"console.log(require("dep").from);"#);
    let (stdout, stderr) = run(&dir, "app.cts");
    assert!(
        stdout.is_empty() && stderr.contains("Cannot find module"),
        "a dependency's source must not load: stdout {stdout:?}, stderr {stderr}"
    );
}

/// `require.resolve` with `paths` looks where Node looked, root by root: another
/// project's own link wins over the caller's, a `lib` without the source passes to
/// the next root, and a project with no link keeps Node's error.
#[cfg(unix)]
#[test]
fn require_resolve_paths_searches_only_the_given_roots() {
    let root = work_dir("paths");
    for (pkg, from, source) in [
        ("lib-a", "caller", true),
        ("lib-b", "other", true),
        ("lib-c", "first", false),
    ] {
        let dir = root.join("packages").join(pkg);
        write(&dir.join("package.json"), r#"{"name":"lib"}"#);
        write(&dir.join("tsconfig.json"), OWN_DIRS);
        if source {
            write(&dir.join("src/missing.ts"), "exports.x = 1;");
        }
        let link = root.join(from).join("node_modules/lib");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&dir, link).unwrap();
    }
    write(&root.join("empty/package.json"), "{}");
    write(
        &root.join("caller/app.cts"),
        r#"// A `require` from `createRequire`: on the compat tier the entry's own `require` is
// the ESM translator's, whose `resolve` takes no options.
const { resolve } = require("node:module").createRequire(__filename);
const at = (...roots: string[]) => {
  const paths = roots.map((r) => __dirname + "/../" + r);
  try { return resolve("lib/dist/missing", { paths }).split("packages/")[1]; }
  catch (e: any) { return e.code; }
};
console.log([at("other"), at("empty"), at("first", "other")].join("|"));
"#,
    );
    let (stdout, stderr) = run(&root.join("caller"), "app.cts");
    assert_eq!(
        stdout, "lib-b/src/missing.ts|MODULE_NOT_FOUND|lib-b/src/missing.ts",
        "stderr: {stderr}"
    );
}

/// A query or fragment on the target Node selected stays on the source URL, so the
/// two spellings stay two module instances, as they are once built. The `enum` needs
/// Nub's transpiler, which Node's own type stripping refuses, so each URL must still
/// read as `.mts`.
#[test]
fn target_query_and_fragment_survive_the_mapping() {
    let dir = work_dir("suffix");
    write(
        &dir.join("package.json"),
        r##"{"type":"module","imports":{"#one":"./dist/item.mjs?one","#two":"./dist/item.mjs#two"}}"##,
    );
    write(&dir.join("tsconfig.json"), OWN_DIRS);
    write(
        &dir.join("src/item.mts"),
        "enum Kind { A } export const url: string = import.meta.url + Kind.A;",
    );
    write(
        &dir.join("main.mts"),
        r##"import * as one from "#one";
import * as two from "#two";
console.log([one.url, two.url].map((u) => u.split("/").pop()).join("|") + "|" + (one !== two));
"##,
    );
    let (stdout, stderr) = run(&dir, "main.mts");
    assert_eq!(
        stdout, "item.mts?one0|item.mts#two0|true",
        "stderr: {stderr}"
    );
}

/// An empty `?` or `#` keys the module as Node keys the built file: one module by
/// default, where Node's realpath step rebuilds the URL, and three under
/// `--preserve-symlinks`, where it keeps the URL as written.
#[test]
fn empty_query_and_fragment_key_the_module_as_node_does() {
    let dir = work_dir("empty-suffix");
    write(
        &dir.join("package.json"),
        r##"{"type":"module","imports":{"#a":"./dist/item.mjs","#b":"./dist/item.mjs?","#c":"./dist/item.mjs#"}}"##,
    );
    write(&dir.join("tsconfig.json"), OWN_DIRS);
    write(
        &dir.join("src/item.mts"),
        "export const url: string = import.meta.url;",
    );
    write(
        &dir.join("main.mts"),
        r##"import * as a from "#a";
import * as b from "#b";
import * as c from "#c";
console.log([a, b, c].map((m) => m.url.split("/").pop()).join(" "), a === b && b === c);
"##,
    );
    let (stdout, stderr) = run(&dir, "main.mts");
    assert_eq!(
        stdout, "item.mts item.mts item.mts true",
        "stderr: {stderr}"
    );
    let (stdout, stderr) = run_with(&dir, "main.mts", &[("NODE_OPTIONS", "--preserve-symlinks")]);
    assert_eq!(
        stdout, "item.mts item.mts? item.mts# false",
        "stderr: {stderr}"
    );
}

/// A symlinked source is keyed by its real path, as Node keys every module, so the
/// fallback and a direct import share one instance. Node's own symlink settings keep
/// the path through the link: `--preserve-symlinks` in any spelling Node accepts,
/// and for the entry `--preserve-symlinks-main` unless a later flag turns it off.
#[cfg(unix)]
#[test]
fn symlinked_source_is_one_module() {
    let dir = work_dir("symlink");
    write(
        &dir.join("package.json"),
        r##"{"type":"module","imports":{"#item":"./dist/item.js","#citem":"./dist/citem.cjs"}}"##,
    );
    write(&dir.join("tsconfig.json"), OWN_DIRS);
    write(
        &dir.join("real/item.ts"),
        "export const url: string = import.meta.url;",
    );
    write(&dir.join("real/citem.cts"), "exports.id = {};");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    for file in ["item.ts", "citem.cts"] {
        std::os::unix::fs::symlink(format!("../real/{file}"), dir.join("src").join(file)).unwrap();
    }
    write(
        &dir.join("main.ts"),
        r##"import * as viaMap from "#item";
import * as direct from "./real/item.ts";
console.log(viaMap === direct, viaMap.url.split("/").slice(-2).join("/"));
"##,
    );
    write(
        &dir.join("main.cts"),
        r##"console.log(require("#citem") === require("./real/citem.cts"));"##,
    );
    let (stdout, stderr) = run(&dir, "main.ts");
    assert_eq!(stdout, "true real/item.ts", "stderr: {stderr}");
    let (stdout, stderr) = run(&dir, "main.cts");
    assert_eq!(stdout, "true", "stderr: {stderr}");
    for preserve in [
        ("NODE_OPTIONS", "--preserve-symlinks"),
        ("NODE_OPTIONS", r#""--preserve-symlinks""#),
        ("NODE_PRESERVE_SYMLINKS", "1"),
    ] {
        let (stdout, stderr) = run_with(&dir, "main.ts", &[preserve]);
        assert_eq!(stdout, "false src/item.ts", "{preserve:?} stderr: {stderr}");
    }

    write(
        &dir.join("real/index.ts"),
        r#"console.log(import.meta.url.split("/").slice(-2).join("/"));"#,
    );
    std::os::unix::fs::symlink("../real/index.ts", dir.join("src/index.ts")).unwrap();
    for (options, entry) in [
        ("", "real/index.ts"),
        ("--preserve-symlinks-main", "src/index.ts"),
        (
            "--preserve-symlinks-main --no-preserve-symlinks-main",
            "real/index.ts",
        ),
    ] {
        let (stdout, stderr) = run_with(&dir, "dist/index.js", &[("NODE_OPTIONS", options)]);
        assert_eq!(stdout, entry, "NODE_OPTIONS={options:?} stderr: {stderr}");
    }
}
