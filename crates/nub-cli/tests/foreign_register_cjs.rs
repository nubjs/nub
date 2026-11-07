//! A `--import` that calls `module.registerHooks` leaves CommonJS loading as it does
//! under plain `node --import`. On Node 22.15–24.11 such an import puts Nub on its async
//! worker tier, where the hook's `next()` used to throw on every CommonJS file, so
//! the `node 22.15` CI leg is the one that can go red. Other Nodes pass trivially, and
//! Nodes below 22.15 have no `registerHooks` to call.
//!
//! From 22.22.3 the same tier takes a different path, because Node's validator lets the
//! missing source through. `TEST_NODE_BIN_22_23_3` pins one such release; the tests that
//! name it skip without it, and CI sets it on Linux.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn write(root: &Path, name: &str, body: &str) {
    std::fs::write(root.join(name), body).unwrap();
}

fn without_nub_env(mut command: Command, root: &Path) -> Output {
    command
        .current_dir(root)
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .env_remove("__NUB_RUNTIME_CONFIG")
        .output()
        .unwrap()
}

fn nub(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nub"));
    command.args(args);
    without_nub_env(command, root)
}

/// `nub` on a pinned Node, found through `PATH` as a user's would be.
fn nub_on(node_dir: &Path, root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nub"));
    let mut path = vec![node_dir.to_path_buf()];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path: OsString = std::env::join_paths(path).unwrap();
    command.args(args).env("PATH", path);
    without_nub_env(command, root)
}

fn node_on(node_dir: Option<&Path>, root: &Path, args: &[&str]) -> Output {
    let program = node_dir.map_or(PathBuf::from("node"), |dir| dir.join("node"));
    let mut command = Command::new(program);
    command.args(args);
    without_nub_env(command, root)
}

fn permissive_node() -> Option<PathBuf> {
    let dir = std::env::var_os("TEST_NODE_BIN_22_23_3").map(PathBuf::from);
    if dir.is_none() {
        eprintln!("skipping the 22.23.3 half: TEST_NODE_BIN_22_23_3 is not set");
    }
    dir
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `node_output` and `nub_output` ended the same way and printed the same thing.
fn assert_matches_node(node_output: &Output, nub_output: &Output, what: &str) {
    assert_eq!(
        (
            node_output.status.success(),
            String::from_utf8_lossy(&node_output.stdout)
        ),
        (
            nub_output.status.success(),
            String::from_utf8_lossy(&nub_output.stdout)
        ),
        "{what}\nnode stderr: {}\nnub stderr: {}",
        String::from_utf8_lossy(&node_output.stderr),
        String::from_utf8_lossy(&nub_output.stderr)
    );
}

/// `register.mjs` installs a pass-through hook through a static `node:module` import.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "c.cjs", "module.exports = 'from-cjs';\n");
    write(
        root,
        "main.mjs",
        "import c from './c.cjs';\nconsole.log(c);\n",
    );
    write(root, "main.cjs", "console.log(require('./c.cjs'));\n");
    write(
        root,
        "register.mjs",
        r#"import nodeModule from "node:module";
if (nodeModule.registerHooks) await import("./static.mjs");
"#,
    );
    write(
        root,
        "static.mjs",
        r#"import { registerHooks } from "node:module";
registerHooks({ load: (url, context, next) => next(url, context) });
"#,
    );
    dir
}

#[test]
fn a_foreign_register_leaves_commonjs_files_running() {
    let dir = fixture();
    let root = dir.path();
    for entry in ["main.mjs", "main.cjs"] {
        assert_eq!(
            stdout(&nub(root, &["--import", "./register.mjs", entry])),
            "from-cjs",
            "entry {entry}"
        );
    }

    // The standalone runner picks the same tier from either entry. `--require` also
    // re-runs inside Node's hook worker. Unix-only: the flags need a URL for an absolute
    // Windows path, and the behaviour is not platform-specific.
    #[cfg(unix)]
    {
        let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
        for runner in [
            format!("--import={}/loader-register.mjs", runtime.display()),
            format!("--require={}/loader-register.cjs", runtime.display()),
        ] {
            let mut command = Command::new("node");
            command.args([runner.as_str(), "--import=./register.mjs", "main.mjs"]);
            assert_eq!(
                stdout(&without_nub_env(command, root)),
                "from-cjs",
                "{runner}"
            );
        }
    }
}

/// A hook above the recovery reads what plain Node gives it: the file's bytes, labelled
/// `commonjs`, with `require.cache` as Node has it on that release.
#[test]
fn a_load_hook_sees_commonjs_as_under_node() {
    let dir = fixture();
    let root = dir.path();
    write(
        root,
        "rewrite.mjs",
        r#"import nodeModule from "node:module";
nodeModule.registerHooks?.({ load(url, context, next) {
  const result = next(url, context);
  if (!url.endsWith("/c.cjs") || result.format !== "commonjs") return result;
  return { ...result, source: String(result.source).replace("from-cjs", "rewritten") };
} });
"#,
    );
    write(
        root,
        "cache.cjs",
        "module.exports = Boolean(require.cache[__filename]);\n",
    );
    write(
        root,
        "cache.mjs",
        "import c from './cache.cjs';\nconsole.log(c);\n",
    );
    let cases: [&[&str]; 3] = [
        &["--import", "./rewrite.mjs", "main.mjs"],
        &["--import", "./rewrite.mjs", "main.cjs"],
        &["--import", "./register.mjs", "cache.mjs"],
    ];
    for args in cases {
        let what = args.join(" ");
        assert_matches_node(&node_on(None, root, args), &nub(root, args), &what);
        if let Some(pinned) = permissive_node() {
            assert_matches_node(
                &node_on(Some(&pinned), root, args),
                &nub_on(&pinned, root, args),
                &format!("{what} on 22.23.3"),
            );
        }
    }
}

/// The recovery moves CommonJS onto the translator whose `require` has no `.cache`, so it
/// exists only while a user load hook does, and a run with none keeps Node's own path.
#[test]
fn a_register_with_no_active_load_hook_keeps_require_cache() {
    let dir = fixture();
    let root = dir.path();
    write(
        root,
        "cache.cjs",
        "module.exports = Boolean(require.cache[__filename]);\n",
    );
    write(
        root,
        "cache.mjs",
        "import c from './cache.cjs';\nconsole.log(c);\n",
    );
    write(
        root,
        "resolve-only.mjs",
        r#"import nodeModule from "node:module";
nodeModule.registerHooks?.({ resolve: (specifier, context, next) => next(specifier, context) });
"#,
    );
    write(
        root,
        "deregistered.mjs",
        r#"import nodeModule from "node:module";
nodeModule.registerHooks?.({ load: (url, context, next) => next(url, context) }).deregister();
"#,
    );
    for register in ["./resolve-only.mjs", "./deregistered.mjs"] {
        assert_eq!(
            stdout(&nub(root, &["--import", register, "cache.mjs"])),
            "true",
            "register {register}"
        );
    }
}

/// `foreign.mjs` registers an async loader answering `/rejected.cjs` with `source: 42`,
/// and `/nullish.cjs` and `/nullish-esm.mjs` with `source: null`, all as
/// `format: "module"`, beside a register.
fn foreign_loader_fixture() -> tempfile::TempDir {
    let dir = fixture();
    let root = dir.path();
    write(
        root,
        "foreign.mjs",
        r#"import nodeModule from "node:module";
nodeModule.register("./foreign-hooks.mjs", import.meta.url);
if (nodeModule.registerHooks) await import("./static.mjs");
"#,
    );
    write(
        root,
        "foreign-hooks.mjs",
        r#"export async function load(url, context, next) {
  if (url.endsWith("/rejected.cjs")) return { format: "module", source: 42, shortCircuit: true };
  if (url.endsWith("/nullish.cjs")) return { format: "module", source: null, shortCircuit: true };
  if (url.endsWith("/nullish-esm.mjs")) return { format: "module", source: null, shortCircuit: true };
  return next(url, context);
}
"#,
    );
    write(root, "nullish-esm.mjs", "console.log('raw file ran');\n");
    for stem in ["rejected", "nullish"] {
        write(
            root,
            &format!("{stem}.cjs"),
            "console.log('raw file ran');\n",
        );
        write(
            root,
            &format!("{stem}.mjs"),
            &format!("import './{stem}.cjs';\n"),
        );
    }
    dir
}

fn assert_loader_error(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "ran to success: {stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert!(
        stderr.contains("ERR_INVALID_RETURN_PROPERTY_VALUE"),
        "{stderr}"
    );
}

/// The recovery answers only Node's own missing source. A loader's wrong `source` for a
/// `.cjs` file stays that loader's error, and the raw file never runs.
#[test]
fn a_foreign_loaders_invalid_source_still_fails_the_run() {
    let dir = foreign_loader_fixture();
    let root = dir.path();

    // The same register and loader run an ordinary CommonJS file.
    assert_eq!(
        stdout(&nub(root, &["--import", "./foreign.mjs", "main.mjs"])),
        "from-cjs"
    );

    assert_loader_error(&nub(root, &["--import", "./foreign.mjs", "rejected.mjs"]));
    // An ES module's missing source is never CommonJS to read from disk.
    assert_loader_error(&nub(
        root,
        &["--import", "./foreign.mjs", "nullish-esm.mjs"],
    ));
}

/// Where Node's validator is permissive the recovery has no failure of Node's to answer,
/// so a loader's nullish `source` stays that loader's error too.
#[test]
fn a_permissive_release_keeps_a_loaders_nullish_source_an_error() {
    let Some(pinned) = permissive_node() else {
        return;
    };
    let dir = foreign_loader_fixture();
    let root = dir.path();
    assert_loader_error(&nub_on(
        &pinned,
        root,
        &["--import", "./foreign.mjs", "nullish.mjs"],
    ));
}
