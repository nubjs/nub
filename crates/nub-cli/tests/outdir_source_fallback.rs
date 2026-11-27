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
    let out = Command::new(nub_binary())
        .arg(entry)
        .current_dir(dir)
        .env_remove("NODE_COMPAT")
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
