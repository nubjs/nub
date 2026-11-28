//! A `module.register` call at run time, from a `--require` preload or from the program
//! itself, runs the program as plain Node does. On 22.15 through 24.11 a preload Nub put in
//! `NODE_OPTIONS` also ran inside Node's hook thread and registered its sync hooks there,
//! where they cannot compose with the loader, so the hook thread crashed: a `--require`
//! preload exited 1 with no output, and a call from the program threw. Where Node's sync
//! validator refuses a missing CommonJS source (through 22.22.2 and 24.11.0), an `import`
//! of a CommonJS file then threw instead of loading it, an extensionless one included. A
//! loader's missing source for a JSON file still fails as under Node.
//!
//! Each leg runs on the Node on `PATH` and on every pinned Node CI provides.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn write(root: &Path, name: &str, body: &str) {
    std::fs::write(root.join(name), body).unwrap();
}

/// `program` with `node_dir` first on `PATH`, as a user's Node would be found.
fn run(
    program: &Path,
    node_dir: Option<&Path>,
    root: &Path,
    args: &[&str],
    node_options: Option<&str>,
) -> Output {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .env("XDG_CACHE_HOME", root.join(".cache"))
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .env_remove("__NUB_RUNTIME_CONFIG");
    if let Some(dir) = node_dir {
        let mut path = vec![dir.to_path_buf()];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let path: OsString = std::env::join_paths(path).unwrap();
        command.env("PATH", path);
    }
    if let Some(options) = node_options {
        command.env("NODE_OPTIONS", options);
    }
    command.output().unwrap()
}

/// The Node on `PATH`, then each pinned Node that is set.
fn nodes() -> Vec<(String, Option<PathBuf>)> {
    let mut nodes = vec![("PATH node".to_string(), None)];
    for key in [
        "TEST_NODE_BIN_22_13_0",
        "TEST_NODE_BIN_22_15_0",
        "TEST_NODE_BIN_22_16_0",
        "TEST_NODE_BIN_22_23_3",
        "TEST_NODE_BIN_24_4_0",
        "TEST_NODE_BIN_26_5_0",
    ] {
        match std::env::var_os(key) {
            Some(dir) => nodes.push((key.to_string(), Some(PathBuf::from(dir)))),
            None => eprintln!("skipping the {key} leg: it is not set"),
        }
    }
    nodes
}

/// `node` and `nub` ended the same way and printed the same thing, and `node` succeeded.
fn assert_matches_node(node: &Output, nub: &Output, what: &str) {
    let detail = format!(
        "{what}\nnode stderr: {}\nnub stderr: {}",
        String::from_utf8_lossy(&node.stderr),
        String::from_utf8_lossy(&nub.stderr)
    );
    assert!(node.status.success(), "{detail}");
    assert_eq!(
        (nub.status.code(), String::from_utf8_lossy(&nub.stdout)),
        (node.status.code(), String::from_utf8_lossy(&node.stdout)),
        "{detail}"
    );
}

/// `register.cjs` registers a hook that serves `virtual:x`, so a run that prints its
/// value went through the hook. `c.cjs` reports whether its `require` has a real cache.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", "{}");
    write(
        root,
        "register.cjs",
        r#"const hook = `
export async function resolve(specifier, context, next) {
  return specifier === "virtual:x" ? { url: "virtual:x", shortCircuit: true } : next(specifier, context);
}
export async function load(url, context, next) {
  return url === "virtual:x" ? { format: "module", source: "export default 'virtual'", shortCircuit: true } : next(url, context);
}`;
require("node:module").register("data:text/javascript," + encodeURIComponent(hook));
"#,
    );
    write(
        root,
        "c.cjs",
        "module.exports = Boolean(require.cache[__filename]);\n",
    );
    write(
        root,
        "main.mjs",
        "import v from 'virtual:x';\nimport c from './c.cjs';\nconsole.log(v, c);\n",
    );
    write(
        root,
        "main.cjs",
        "console.log(require.main === module, require('./c.cjs'));\nimport('./main.mjs');\n",
    );
    write(
        root,
        "self.cjs",
        "require('./register.cjs');\nimport('./main.mjs');\n",
    );
    // A loader that answers a JSON file and a CommonJS file with no source, the second
    // after its resolve hinted `module`. Node refuses the first and loads the second.
    write(
        root,
        "nullish.cjs",
        r#"const hook = `
export async function resolve(specifier, context, next) {
  const result = await next(specifier, context);
  return specifier.endsWith("hinted.cjs") ? { ...result, format: "module" } : result;
}
export async function load(url, context, next) {
  if (url.endsWith("/data.json")) return { format: "json", source: null, shortCircuit: true };
  if (url.endsWith("/hinted.cjs")) return { format: "commonjs", source: null, shortCircuit: true };
  return next(url, context);
}`;
require("node:module").register("data:text/javascript," + encodeURIComponent(hook));
"#,
    );
    write(root, "data.json", "{}\n");
    // Extensionless files Node loads as CommonJS, for their package's `type` and, with
    // none, for their syntax.
    for (dir, package, value) in [
        ("cjs", r#"{"type":"commonjs"}"#, "typed"),
        ("untyped", "{}", "untyped"),
    ] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        write(root, &format!("{dir}/package.json"), package);
        write(
            root,
            &format!("{dir}/plain"),
            &format!("module.exports = '{value}';\n"),
        );
    }
    write(
        root,
        "extensionless.mjs",
        "import typed from './cjs/plain';\nimport untyped from './untyped/plain';\nconsole.log(typed, untyped);\n",
    );
    write(root, "hinted.cjs", "module.exports = 'hinted';\n");
    write(
        root,
        "nullish.mjs",
        "const json = await import('./data.json', { with: { type: 'json' } }).then(() => 'loaded', (e) => e.code);\nconst hinted = await import('./hinted.cjs').then((m) => m.default, (e) => e.code);\nconsole.log(json, hinted);\n",
    );
    dir
}

#[test]
fn a_runtime_register_runs_the_program_as_under_node() {
    let dir = fixture();
    let root = dir.path();
    let nub = PathBuf::from(env!("CARGO_BIN_EXE_nub"));
    let cases: [(&str, Option<&str>); 5] = [
        ("main.cjs", Some("--require=./register.cjs")),
        ("main.mjs", Some("--require=./register.cjs")),
        ("self.cjs", None),
        ("nullish.mjs", Some("--require=./nullish.cjs")),
        ("extensionless.mjs", Some("--require=./register.cjs")),
    ];
    for (name, node_dir) in nodes() {
        let node = node_dir
            .as_ref()
            .map_or(PathBuf::from("node"), |dir| dir.join("node"));
        for (entry, options) in cases {
            let what = format!("{name}: {entry} with NODE_OPTIONS {options:?}");
            let expected = run(&node, None, root, &[entry], options);
            let actual = run(&nub, node_dir.as_deref(), root, &[entry], options);
            assert_matches_node(&expected, &actual, &what);
        }

        // The standalone runner, with the preload ahead of it on the command line.
        // Unix-only: the flag needs a URL for an absolute Windows path, and the
        // behaviour is not platform-specific.
        #[cfg(unix)]
        {
            let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
            let runner = format!("--import={}/loader-register.mjs", runtime.display());
            for (entry, preload) in [
                ("main.cjs", "--require=./register.cjs"),
                ("main.mjs", "--require=./register.cjs"),
                ("nullish.mjs", "--require=./nullish.cjs"),
                ("extensionless.mjs", "--require=./register.cjs"),
            ] {
                let what = format!("{name}: the standalone runner on {entry}");
                let expected = run(&node, None, root, &[preload, entry], None);
                let actual = run(&node, None, root, &[preload, &runner, entry], None);
                assert_matches_node(&expected, &actual, &what);
            }
        }
    }
}
