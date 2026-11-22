//! How a project's tsconfig reaches resolution, under `nub` and under the standalone
//! loader (`node --import <runtime>/loader-register.mjs`, what `@nubjs/runner` ships).
//! The loader halves are unix-only: the flags need a URL for an absolute Windows
//! path, and none of this is platform-specific.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn hermetic(mut command: Command, cwd: &Path, cache: &Path) -> Output {
    command
        .current_dir(cwd)
        .env("XDG_CACHE_HOME", cache)
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .env_remove("__NUB_RUNTIME_CONFIG")
        .env_remove("__NUB_TSCONFIG_REPORTED")
        .output()
        .unwrap()
}

fn nub(cwd: &Path, cache: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nub"));
    command.args(args);
    hermetic(command, cwd, cache)
}

fn runtime_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime")
}

/// Plain `node` (from `node_dir`, else `PATH`) with the standalone loader preloaded.
#[cfg(unix)]
fn loader(node_dir: Option<&Path>, preload: &str, cwd: &Path, cache: &Path, entry: &str) -> Output {
    let program = node_dir.map_or(PathBuf::from("node"), |dir| dir.join("node"));
    let mut command = Command::new(program);
    let flag = if preload.ends_with(".cjs") {
        "--require"
    } else {
        "--import"
    };
    command.args([
        format!("{flag}={}/{preload}", runtime_dir().display()),
        entry.to_string(),
    ]);
    hermetic(command, cwd, cache)
}

fn stdout(output: &Output, what: &str) -> String {
    assert!(
        output.status.success(),
        "{what}: exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `"*": ["./node_modules/*"]` sends `import "dual"` into the package directory. Node,
/// not the package's legacy `main`, picks the file: `exports` and the active condition
/// decide. A deep path the package does not export still loads the file it names.
#[test]
fn a_catch_all_paths_entry_resolves_packages_through_their_exports() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"type":"module"}"#);
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"*":["./node_modules/*"]}}}"#,
    );
    write(
        root,
        "node_modules/dual/package.json",
        r#"{"name":"dual","main":"./main.cjs","exports":{".":{"import":"./esm.mjs","require":"./cjs.cjs"}}}"#,
    );
    for (file, body) in [
        ("main.cjs", "exports.which = 'main';"),
        ("cjs.cjs", "exports.which = 'require';"),
        ("esm.mjs", "export const which = 'import';"),
        ("internal.js", "exports.which = 'internal';"),
    ] {
        write(root, &format!("node_modules/dual/{file}"), body);
    }
    write(
        root,
        "main.ts",
        r#"import { which } from "dual";
import { which as internal } from "dual/internal";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
console.log(which, require("dual").which, internal);
"#,
    );
    let out = nub(root, &root.join(".cache"), &["main.ts"]);
    assert_eq!(stdout(&out, "nub main.ts"), "import require internal");
}

/// `nub app/main.ts` from outside `app/` applies `app/tsconfig.json`'s
/// `customConditions`, as running it from inside does.
#[test]
fn custom_conditions_come_from_the_entrys_tsconfig() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "app/package.json", r#"{"type":"module"}"#);
    write(
        root,
        "app/tsconfig.json",
        r#"{"compilerOptions":{"customConditions":["dev"]}}"#,
    );
    write(
        root,
        "app/node_modules/cond/package.json",
        r#"{"name":"cond","exports":{"dev":"./dev.cjs","default":"./prod.cjs"}}"#,
    );
    write(
        root,
        "app/node_modules/cond/dev.cjs",
        "module.exports = 'dev';\n",
    );
    write(
        root,
        "app/node_modules/cond/prod.cjs",
        "module.exports = 'prod';\n",
    );
    write(
        root,
        "app/main.ts",
        r#"import cond from "cond";
import { createRequire } from "node:module";
console.log(cond, createRequire(import.meta.url)("cond"));
"#,
    );
    let out = nub(root, &root.join(".cache"), &["app/main.ts"]);
    assert_eq!(stdout(&out, "nub app/main.ts"), "dev dev");
}

/// The loader stops on a tsconfig it cannot read in full, as `nub` does, rather than
/// running under options the author never wrote.
#[cfg(unix)]
#[test]
fn the_standalone_loader_stops_on_an_unreadable_tsconfig() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "tsconfig.json", r#"{"extends":"./missing.json"}"#);
    write(root, "main.ts", "console.log('ran');\n");
    let out = loader(
        None,
        "loader-register.mjs",
        root,
        &root.join(".cache"),
        "main.ts",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the run must stop: {stderr}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("ran"),
        "the entry must not run: {stderr}"
    );
    assert!(
        stderr.contains("missing.json"),
        "the failure names the file: {stderr}"
    );
}

/// An empty, whitespace-only or comment-only tsconfig is `{}`, as it is to `tsc`. A
/// literal `null` is still refused (`tsc` TS5092).
#[test]
fn an_empty_tsconfig_reads_as_an_empty_object() {
    for body in ["", " \n\t\n", "// nothing yet\n"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "tsconfig.json", body);
        write(root, "main.ts", "const n: number = 1;\nconsole.log(n);\n");
        let cache = root.join(".cache");
        assert_eq!(
            stdout(&nub(root, &cache, &["main.ts"]), &format!("nub, {body:?}")),
            "1"
        );
        #[cfg(unix)]
        assert_eq!(
            stdout(
                &loader(None, "loader-register.mjs", root, &cache, "main.ts"),
                &format!("loader, {body:?}")
            ),
            "1"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "tsconfig.json", "null\n");
    write(root, "main.ts", "console.log(1);\n");
    let out = nub(root, &root.join(".cache"), &["main.ts"]);
    assert!(
        !out.status.success(),
        "a `null` tsconfig must still stop the run"
    );
}

/// `export` in a `.ts` file under `"type": "commonjs"` fails on every tier. The
/// `--require` loader on a compat-tier Node (22.13 here) used to let Node's own
/// ES-module syntax detection load it as ESM, where the fast tier and Node's type
/// stripping follow the package type.
#[cfg(unix)]
#[test]
fn esm_syntax_in_a_commonjs_package_fails_on_the_compat_tier_too() {
    let Some(node_dir) = std::env::var_os("TEST_NODE_BIN_22_13_0").map(PathBuf::from) else {
        eprintln!("skipping: TEST_NODE_BIN_22_13_0 is not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"type":"commonjs"}"#);
    write(
        root,
        "main.ts",
        "export const n: number = 1;\nconsole.log('ran');\n",
    );
    let cache = root.join(".cache");
    let out = loader(
        Some(&node_dir),
        "loader-register.cjs",
        root,
        &cache,
        "main.ts",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "22.13 must refuse it: {stderr}");
    assert!(
        stderr.contains("SyntaxError"),
        "the refusal is a SyntaxError: {stderr}"
    );
}
