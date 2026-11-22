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
fn loader(
    node_dir: Option<&Path>,
    preload: &str,
    cwd: &Path,
    cache: &Path,
    args: &[&str],
) -> Output {
    let program = node_dir.map_or(PathBuf::from("node"), |dir| dir.join("node"));
    let mut command = Command::new(program);
    let flag = if preload.ends_with(".cjs") {
        "--require"
    } else {
        "--import"
    };
    command
        .arg(format!("{flag}={}/{preload}", runtime_dir().display()))
        .args(args);
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

/// A package whose manifest names itself after a Node builtin (`events`) is not
/// asked for by that name, which Node would answer with the builtin: the target's
/// own file loads, as it did before targets went through `exports`.
#[test]
fn a_paths_target_named_like_a_builtin_loads_the_package() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"type":"module"}"#);
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"@polyfill/events":["./node_modules/events-polyfill"]}}}"#,
    );
    write(
        root,
        "node_modules/events-polyfill/package.json",
        r#"{"name":"events","main":"./main.cjs","exports":{".":"./main.cjs"}}"#,
    );
    write(
        root,
        "node_modules/events-polyfill/main.cjs",
        "exports.which = 'polyfill';",
    );
    write(
        root,
        "main.ts",
        r#"import { which } from "@polyfill/events";
import { createRequire } from "node:module";
console.log(which, createRequire(import.meta.url)("@polyfill/events").which);
"#,
    );
    let out = nub(root, &root.join(".cache"), &["main.ts"]);
    assert_eq!(stdout(&out, "nub main.ts"), "polyfill polyfill");

    // `test` is a builtin only as `node:test`, so a package by that name keeps its
    // `exports`. This one has nothing else to load.
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"@fixtures/test":["./node_modules/test-fixtures"]}}}"#,
    );
    write(
        root,
        "node_modules/test-fixtures/package.json",
        r#"{"name":"test","exports":{".":{"import":"./esm.mjs","require":"./cjs.cjs"}}}"#,
    );
    write(
        root,
        "node_modules/test-fixtures/esm.mjs",
        "export const which = 'import';",
    );
    write(
        root,
        "node_modules/test-fixtures/cjs.cjs",
        "exports.which = 'require';",
    );
    write(
        root,
        "main.ts",
        r#"import { which } from "@fixtures/test";
import { createRequire } from "node:module";
console.log(which, createRequire(import.meta.url)("@fixtures/test").which);
"#,
    );
    let out = nub(root, &root.join(".cache"), &["main.ts"]);
    assert_eq!(stdout(&out, "nub main.ts, `test`"), "import require");
}

/// A `paths` target names one installed copy, and that copy answers: not a nested
/// copy nearer the importer, and not the project itself when it shares the
/// package's name. A target Node refuses falls to the next candidate.
#[test]
fn a_paths_target_resolves_the_package_copy_it_names() {
    // Once as a project of another name, once as one named like the package.
    for project in [
        r#"{"type":"module"}"#,
        r#"{"name":"dual","type":"module","exports":"./self.js"}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", project);
        write(root, "self.js", "export const which = 'self';\n");
        write(
            root,
            "tsconfig.json",
            r#"{"compilerOptions":{"paths":{
            "alias":["./node_modules/dual"],
            "second":["./node_modules/dual/missing","./second.cjs"]}}}"#,
        );
        write(root, "second.cjs", "exports.which = 'second';\n");
        for (copy, label) in [
            ("node_modules/dual", "root"),
            ("src/node_modules/dual", "nested"),
        ] {
            write(
                root,
                &format!("{copy}/package.json"),
                r#"{"name":"dual","exports":{".":{"import":"./esm.mjs","require":"./cjs.cjs"}}}"#,
            );
            write(
                root,
                &format!("{copy}/esm.mjs"),
                &format!("export const which = '{label} import';"),
            );
            write(
                root,
                &format!("{copy}/cjs.cjs"),
                &format!("exports.which = '{label} require';"),
            );
        }
        write(
            root,
            "src/main.ts",
            r#"import { which as a } from "alias";
import { which as b } from "second";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
console.log([a, require("alias").which, b, require("second").which].join(", "));
"#,
        );
        let out = nub(root, &root.join(".cache"), &["src/main.ts"]);
        assert_eq!(
            stdout(&out, project),
            "root import, root require, second, second"
        );
    }
}

/// With a file entry, the entry's tsconfig alone supplies `customConditions`, so
/// `nub app/main.ts` from the root resolves as `cd app && nub main.ts` does.
#[test]
fn custom_conditions_come_from_the_entrys_tsconfig() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"customConditions":["root"]}}"#,
    );
    write(root, "app/package.json", r#"{"type":"module"}"#);
    write(
        root,
        "app/tsconfig.json",
        r#"{"compilerOptions":{"customConditions":["app"]}}"#,
    );
    write(
        root,
        "app/node_modules/cond/package.json",
        r#"{"name":"cond","exports":{"root":"./root.cjs","app":"./app.cjs","default":"./default.cjs"}}"#,
    );
    for label in ["root", "app", "default"] {
        write(
            root,
            &format!("app/node_modules/cond/{label}.cjs"),
            &format!("module.exports = '{label}';"),
        );
    }
    write(
        root,
        "app/main.ts",
        r#"import cond from "cond";
import { createRequire } from "node:module";
console.log(cond, createRequire(import.meta.url)("cond"));
"#,
    );
    let cache = root.join(".cache");
    let from_root = nub(root, &cache, &["app/main.ts"]);
    assert_eq!(stdout(&from_root, "nub app/main.ts"), "app app");
    let from_app = nub(&root.join("app"), &cache, &["main.ts"]);
    assert_eq!(stdout(&from_app, "cd app && nub main.ts"), "app app");
}

/// The loader stops on a tsconfig it cannot read in full, as `nub` does, rather than
/// running under options the author never wrote: the working directory's, and the
/// one governing a file entry launched from above it.
#[cfg(unix)]
#[test]
fn the_standalone_loader_stops_on_an_unreadable_tsconfig() {
    for (config, entry) in [
        ("tsconfig.json", "main.ts"),
        ("app/tsconfig.json", "app/main.ts"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, config, r#"{"extends":"./missing.json"}"#);
        write(root, entry, "console.log('ran');\n");
        let out = loader(
            None,
            "loader-register.mjs",
            root,
            &root.join(".cache"),
            &[entry],
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "{entry}: the run must stop: {stderr}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("ran"),
            "{entry}: the entry must not run: {stderr}"
        );
        assert!(
            stderr.contains("missing.json"),
            "{entry}: the failure names the file: {stderr}"
        );
    }

    // Under `-e`/`-p`, the path after the code is an argument to it, not an entry:
    // its directory's config is not one this run reads.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "app/tsconfig.json", r#"{"extends":"./missing.json"}"#);
    write(root, "app/main.mjs", "console.log('entry');\n");
    let arg = root.join("app/main.mjs");
    let arg = arg.to_str().unwrap();
    let cache = root.join(".cache");
    for args in [["-e", "console.log('ran')", arg], ["--print", "'ran'", arg]] {
        let out = loader(None, "loader-register.mjs", root, &cache, &args);
        assert_eq!(stdout(&out, args[0]), "ran");
    }
    // These run the file after all: `-p` with no code, and `-i`, which forces the
    // REPL past the eval string.
    for args in [&["-p", "--", arg][..], &["-i", "-e", "0", arg]] {
        let out = loader(None, "loader-register.mjs", root, &cache, args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "{args:?}: the run must stop: {stderr}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("entry"),
            "{args:?}: the entry must not run: {stderr}"
        );
    }
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
                &loader(None, "loader-register.mjs", root, &cache, &["main.ts"]),
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
        &["main.ts"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "22.13 must refuse it: {stderr}");
    assert!(
        stderr.contains("SyntaxError"),
        "the refusal is a SyntaxError: {stderr}"
    );
}
