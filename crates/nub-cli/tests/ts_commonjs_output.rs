//! A tsconfig with `"module": "commonjs"` compiles `import`/`export` in `.ts` files to
//! CommonJS the way `tsc` emits it, under `nub` and under the standalone loader
//! (`node --import <runtime>/loader-register.mjs`, what `@nubjs/runner` ships).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// `nub` with `node_dir` first on `PATH` (Nub runs the first Node it finds there), or
/// the standalone loader on that Node when `standalone` is set.
fn run(root: &Path, node_dir: Option<&Path>, standalone: bool, entry: &str) -> Output {
    run_with(root, node_dir, standalone, entry, None)
}

/// `run`, with `NODE_OPTIONS` set to `node_options`.
fn run_with(
    root: &Path,
    node_dir: Option<&Path>,
    standalone: bool,
    entry: &str,
    node_options: Option<&str>,
) -> Output {
    let mut command = if standalone {
        let node = node_dir.map_or(PathBuf::from("node"), |dir| dir.join("node"));
        let mut command = Command::new(node);
        let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
        command.arg(format!(
            "--import={}/loader-register.mjs",
            runtime.display()
        ));
        command
    } else {
        Command::new(env!("CARGO_BIN_EXE_nub"))
    };
    if let Some(dir) = node_dir {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![dir.to_path_buf()];
        dirs.extend(std::env::split_paths(&path));
        command.env("PATH", std::env::join_paths(dirs).unwrap());
    }
    command
        .arg(entry)
        .current_dir(root)
        .env("XDG_CACHE_HOME", root.join(".cache"))
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .env_remove("__NUB_RUNTIME_CONFIG")
        .env_remove("__NUB_TSCONFIG_REPORTED");
    if let Some(options) = node_options {
        command.env("NODE_OPTIONS", options);
    }
    command.output().unwrap()
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

/// The pinned Nodes CI installs: a compat-tier one and a fast-tier one, both of which
/// remap stack traces (Nub withholds source maps on 26.0 through 26.7).
fn pinned_nodes() -> Vec<(&'static str, PathBuf)> {
    ["TEST_NODE_BIN_22_13_0", "TEST_NODE_BIN_22_23_3"]
        .into_iter()
        .filter_map(|key| match std::env::var_os(key) {
            Some(dir) => Some((key, PathBuf::from(dir))),
            None => {
                eprintln!("skipping the {key} leg: it is not set");
                None
            }
        })
        .collect()
}

const MAIN: &str = r#"import path from "node:path";
import fn from "cjs-fn";
import greet, { named, fail, bump, count } from "./lib";

console.log(path.basename(__dirname), typeof require, require.main === module, fn(), greet(), named, bump(), count);
try {
  fail();
} catch (error) {
  console.log((error as Error).stack!.split("\n")[1].trim().replace(__dirname + path.sep, ""));
}
"#;

const LIB: &str = r#"export const named: string = "named";
export default function greet() { return "default"; }
export function fail(): never {
  throw new Error("from lib");
}
let count = 0;
export { count };
export function bump() { return count++; }
"#;

#[test]
fn module_commonjs_compiles_imports_to_require() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("app");
    write(&root, "package.json", r#"{"name":"app"}"#);
    write(
        &root,
        "tsconfig.json",
        r#"{"compilerOptions":{"module":"commonjs","esModuleInterop":true}}"#,
    );
    write(
        &root,
        "node_modules/cjs-fn/package.json",
        r#"{"main":"index.js"}"#,
    );
    write(
        &root,
        "node_modules/cjs-fn/index.js",
        "module.exports = () => 'fn';\n",
    );
    write(&root, "main.ts", MAIN);
    write(&root, "lib.ts", LIB);
    write(
        &root,
        "side.ts",
        "const before: number = 1;\n\n\n\nimport \"./boom\";\n",
    );
    write(
        &root,
        "boom.ts",
        "Error.stackTraceLimit = 50;\nthrow new Error(\"boom\");\n",
    );
    write(
        &root,
        "using.ts",
        "import { sep } from \"node:path\";\nusing r = { [Symbol.dispose]() { console.log(\"disposed\"); } };\nconsole.log(typeof sep);\n",
    );

    // `bump()` returns the old count, and the import reads the new one live.
    let first = "app function true fn default named 0 1";
    let mut legs = vec![(false, None)];
    if cfg!(unix) {
        legs.push((true, None));
    }
    for (standalone, node) in legs {
        let out = stdout(&run(&root, node, standalone, "main.ts"), "PATH node");
        assert_eq!(out.lines().next(), Some(first), "standalone: {standalone}");
    }
    // The stack frames prove the source map points at the original lines, a
    // side-effect import's `require()` included.
    for (key, node_dir) in pinned_nodes() {
        let out = stdout(&run(&root, Some(&node_dir), false, "main.ts"), key);
        assert_eq!(out, format!("{first}\nat fail (lib.ts:4:9)"), "{key}");
        let out = run(&root, Some(&node_dir), false, "side.ts");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("side.ts:5:1)"), "{key}: {stderr}");
        // The `require()` of Nub's `using` helper resolves from Nub's runtime, which
        // the standalone loader keeps out of the project's reach.
        if cfg!(unix) {
            let out = stdout(&run(&root, Some(&node_dir), true, "using.ts"), key);
            assert_eq!(out, "string\ndisposed", "{key}");
        }
    }
}

/// A CommonJS entry on the compat tier (22.13) `require()`s an ES-module-only package:
/// a `.ts` file compiled from `import`, one written with `require`, and a plain `.cjs`.
/// Node runs a `.ts` entry with the loader's source through a wrapper whose `require()`
/// of an ES module fails on 20.19-22.14, so the entry has to reach Node's own CommonJS
/// loader.
///
/// Each runs again behind a resolve hook that holds Node's resolve of the entry back,
/// so Nub's fetch-handler pass imports the entry while Node's own import is still in
/// flight. Answered first, that import made the entry a module that is not the main:
/// `require.main` was undefined, or the `.ts` entry kept the loader's source. A CI
/// runner lost that race without the hook.
#[test]
fn a_commonjs_entry_requires_an_es_module_on_the_compat_tier() {
    let Some(node_dir) = std::env::var_os("TEST_NODE_BIN_22_13_0").map(PathBuf::from) else {
        eprintln!("skipping: TEST_NODE_BIN_22_13_0 is not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", "{}");
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"module":"commonjs","esModuleInterop":true}}"#,
    );
    write(
        root,
        "node_modules/esm-only/package.json",
        r#"{"type":"module","exports":"./index.js"}"#,
    );
    write(
        root,
        "node_modules/esm-only/index.js",
        "export const named = 'named';\nexport default 'default';\n",
    );
    write(
        root,
        "imports.ts",
        "import esm, { named } from \"esm-only\";\nconsole.log(named, esm, require.main === module);\n",
    );
    write(
        root,
        "requires.ts",
        "const esm = require(\"esm-only\");\nconsole.log(esm.named, esm.default, require.main === module);\n",
    );
    write(
        root,
        "plain.cjs",
        "const esm = require(\"esm-only\");\nconsole.log(esm.named, esm.default, require.main === module);\n",
    );
    write(
        root,
        "slow-resolve.cjs",
        r#"require("node:module").register(
  "data:text/javascript," +
    encodeURIComponent(
      "export async function resolve(s, c, next) { if (c.parentURL === undefined) await new Promise((r) => setTimeout(r, 50)); return next(s, c); }",
    ),
);
"#,
    );
    let mut legs = vec![(false, None), (false, Some("--require=./slow-resolve.cjs"))];
    if cfg!(unix) {
        legs.push((true, None));
    }
    for entry in ["imports.ts", "requires.ts", "plain.cjs"] {
        for &(standalone, node_options) in &legs {
            let out = run_with(root, Some(&node_dir), standalone, entry, node_options);
            let what = format!("{entry}, standalone: {standalone}, NODE_OPTIONS: {node_options:?}");
            assert_eq!(stdout(&out, &what), "named default true", "{what}");
        }
    }
}

/// On the compat tier (22.13), Nub's fetch-handler pass imports the entry only once the
/// loader thread has seen Node load it, and nothing else waits. A preload that awaits an
/// `import()` of the entry runs it, as on plain Node: Node awaits that preload before it
/// imports the entry, so holding the preload's import back left the run to exit 0 with
/// neither module's output. A hook from the configured preload that holds Node's resolve
/// of the entry for 1.5 seconds after Nub's has answered it still leaves a CommonJS
/// entry the main module, and an ES module server whose timer keeps the loop busy still
/// binds.
#[test]
fn an_early_import_of_the_entry_neither_stalls_startup_nor_takes_the_main_module() {
    let Some(node_dir) = std::env::var_os("TEST_NODE_BIN_22_13_0").map(PathBuf::from) else {
        eprintln!("skipping: TEST_NODE_BIN_22_13_0 is not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", "{}");
    write(
        root,
        "pre.mjs",
        "await import(new URL('./main.mjs', import.meta.url).href);\nconsole.log('preload');\n",
    );
    write(root, "main.mjs", "console.log('main');\n");
    let out = run_with(
        root,
        Some(&node_dir),
        false,
        "main.mjs",
        Some("--import=./pre.mjs"),
    );
    assert_eq!(
        stdout(&out, "a preload awaiting the entry"),
        "main\npreload"
    );

    write(
        root,
        "slow-answer.cjs",
        r#"require("node:module").register(
  "data:text/javascript," +
    encodeURIComponent(
      "export async function resolve(s, c, next) { const r = await next(s, c); if (c.parentURL === undefined) await new Promise((r) => setTimeout(r, 1500)); return r; }",
    ),
);
"#,
    );
    write(root, "nub.jsonc", r#"{ "preload": ["./slow-answer.cjs"] }"#);
    write(root, "main.cjs", "console.log(require.main === module);\n");
    let out = run(root, Some(&node_dir), false, "main.cjs");
    assert_eq!(stdout(&out, "a configured preload's resolve hook"), "true");

    write(
        root,
        "server.mjs",
        "setInterval(() => {}, 1000);\nexport default { fetch: () => new Response('served') };\n",
    );
    assert!(
        binds(root, &node_dir, "server.mjs"),
        "a server behind a configured preload's slow resolve hook never bound"
    );
}

/// `nub <entry>` on `PORT=0` reports a listener within 30 seconds. The process is
/// killed either way.
fn binds(root: &Path, node_dir: &Path, entry: &str) -> bool {
    use std::io::BufRead;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![node_dir.to_path_buf()];
    dirs.extend(std::env::split_paths(&path));
    let mut child = Command::new(env!("CARGO_BIN_EXE_nub"))
        .arg(entry)
        .current_dir(root)
        .env("PATH", std::env::join_paths(dirs).unwrap())
        .env("PORT", "0")
        .env("XDG_CACHE_HOME", root.join(".cache"))
        .env_remove("HOST")
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .env_remove("__NUB_RUNTIME_CONFIG")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (send, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut bound = false;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match lines.recv_timeout(left) {
            Ok(line) if line.contains("Listening on") => {
                bound = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    bound
}

/// `nub` and (on Unix) the standalone loader, on each pinned Node and, when
/// `path_node` is set, the Node on `PATH`.
fn legs(path_node: bool) -> Vec<(String, Option<PathBuf>, bool)> {
    let mut nodes = Vec::new();
    if path_node {
        nodes.push(("PATH node".to_string(), None));
    }
    nodes.extend(
        pinned_nodes()
            .into_iter()
            .map(|(key, dir)| (key.to_string(), Some(dir))),
    );
    let mut legs = Vec::new();
    for (name, dir) in nodes {
        legs.push((name.clone(), dir.clone(), false));
        if cfg!(unix) {
            legs.push((format!("{name}, standalone"), dir, true));
        }
    }
    legs
}

/// A dynamic `import()` takes what the real `import()` loads: its attributes, the
/// package's `import` target, and an ES module's own namespace, here one that
/// default-exports the CommonJS module its package's `require` target names, even
/// beside an export of its own under a name that module also exports, and an ES module
/// that `require()` has already loaded under its `module.exports` export. JSON that
/// both conditions name, through a package's `exports` or the project's `imports`,
/// comes back as `require()` returns it, but an ES module target that imports the
/// `require` target's JSON without an attribute still fails as it does under Node. A
/// CommonJS module, an extensionless one in a `"type": "commonjs"` package included,
/// comes back with the values it has now, not the ones Node copied on the first import, and without a second request a hook could send elsewhere:
/// `redirect.cjs` points only `dep.cjs`'s absolute URL at a module that announces
/// itself. Where it can, it uses
/// `registerHooks`, and a file with `export` under one still compiles to CommonJS:
/// where Node strips types by default it used to run nothing and exit 0. On the
/// pinned Nodes only, since Node 18 reads no `with` attributes.
#[test]
fn dynamic_import_follows_what_import_loads() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r##"{"imports":{"#data":"./data.json"}}"##,
    );
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"module":"commonjs","esModuleInterop":true}}"#,
    );
    write(root, "data.json", r#"{"a":1}"#);
    write(
        root,
        "node_modules/split/package.json",
        r#"{"name":"split","exports":{"import":"./index.mjs","require":"./data.json"}}"#,
    );
    write(
        root,
        "node_modules/split/index.mjs",
        "export const from = 'index.mjs';
",
    );
    write(
        root,
        "node_modules/split/data.json",
        r#"{"from":"data.json"}"#,
    );
    write(
        root,
        "node_modules/dual/package.json",
        r#"{"name":"dual","exports":{"import":"./index.mjs","require":"./index.cjs"}}"#,
    );
    write(
        root,
        "node_modules/dual/index.cjs",
        "exports.cjs = 'cjs';
",
    );
    write(
        root,
        "node_modules/dual/index.mjs",
        "import wrapped from './index.cjs';
export default wrapped;
export const named = 'esm';
",
    );
    write(
        root,
        "node_modules/dataset/package.json",
        r#"{"name":"dataset","exports":"./data.json"}"#,
    );
    write(
        root,
        "node_modules/dataset/data.json",
        r#"{"from":"dataset"}"#,
    );
    write(
        root,
        "main.ts",
        r##"async function main() {
  const text: any = await import("./data.json", { with: { type: "text" } });
  const split: any = await import("split");
  const dual: any = await import("dual");
  const dataset: any = await import("dataset");
  const alias: any = await import("#data");
  const counter: any = await import("./counter.cjs");
  counter.bump();
  const again: any = await import("./counter.cjs");
  const typeless: any = await import("./cjs/counter");
  typeless.bump();
  const typelessAgain: any = await import("./cjs/counter");
  const clash: any = await import("clash");
  require("./override.mjs");
  const override: any = await import("./override.mjs");
  require("./cjs/node_modules/loose/entry");
  const loose: any = await import("./cjs/node_modules/loose/entry");
  const jsondep = await import("jsondep").then(() => "loaded", (e: any) => e.code);
  console.log(typeof text.default, split.from, dual.named, dual.default.cjs, dataset.from, alias.a, again.value, typelessAgain.value, clash.value, override.named, loose.named, jsondep);
}
main();
"##,
    );
    write(
        root,
        "node_modules/clash/package.json",
        r#"{"name":"clash","exports":{"import":"./wrapper.mjs","require":"./index.cjs"}}"#,
    );
    write(root, "node_modules/clash/index.cjs", "exports.value = 1;\n");
    write(
        root,
        "node_modules/clash/wrapper.mjs",
        "import cjs from './index.cjs';\nexport default cjs;\nexport const value = 42;\n",
    );
    write(
        root,
        "override.mjs",
        "const x = { named: 'require override' };\nexport default x;\nexport const named = 'namespace';\nexport { x as 'module.exports' };\n",
    );
    write(
        root,
        "node_modules/jsondep/package.json",
        r#"{"name":"jsondep","exports":{"import":"./index.mjs","require":"./data.json"}}"#,
    );
    write(
        root,
        "node_modules/jsondep/index.mjs",
        "import data from './data.json';\nexport default data;\n",
    );
    write(
        root,
        "node_modules/jsondep/data.json",
        r#"{"from":"jsondep"}"#,
    );
    write(root, "cjs/package.json", r#"{"type":"commonjs"}"#);
    // No package.json of its own, and Node's package scope stops at `node_modules`, so
    // the `cjs` package's type does not reach it and its syntax makes it an ES module.
    write(
        root,
        "cjs/node_modules/loose/entry",
        "const x = { named: 'override' };\nexport const named = 'namespace';\nexport { x as 'module.exports' };\n",
    );
    write(
        root,
        "cjs/counter",
        "exports.value = 1;\nexports.bump = function () { exports.value = 2; };\n",
    );
    write(
        root,
        "counter.cjs",
        "exports.value = 1;\nexports.bump = function () { exports.value = 2; };\n",
    );
    write(
        root,
        "dep.cjs",
        "module.exports = { extra: 1 };\nObject.defineProperty(module.exports, 'hidden', { value: 2 });\n",
    );
    write(
        root,
        "elsewhere.mjs",
        "console.log('elsewhere ran');\nexport default 0;\n",
    );
    write(
        root,
        "redirect.cjs",
        r#"const { pathToFileURL } = require("node:url");
const mod = require("node:module");
const from = pathToFileURL(require.resolve("./dep.cjs")).href;
const to = pathToFileURL(require.resolve("./elsewhere.mjs")).href;
if (mod.registerHooks) {
  mod.registerHooks({ resolve: (s, c, next) => next(s === from ? to : s, c) });
} else {
  const hook = `export async function resolve(s, c, next) { return next(s === ${JSON.stringify(from)} ? ${JSON.stringify(to)} : s, c); }`;
  mod.register("data:text/javascript," + encodeURIComponent(hook));
}
"#,
    );
    write(
        root,
        "hooked.ts",
        "export async function main() {\n  const dep: any = await import(\"./dep.cjs\");\n  console.log(dep.extra, dep.hidden);\n}\nmain();\n",
    );
    for (what, node_dir, standalone) in legs(false) {
        let out = run(root, node_dir.as_deref(), standalone, "main.ts");
        assert_eq!(
            stdout(&out, &what),
            "string index.mjs esm cjs dataset 1 2 2 42 namespace namespace ERR_IMPORT_ATTRIBUTE_MISSING",
            "{what}"
        );
        // Under a user hook the standalone runner hands a TypeScript entry a `require`
        // with no cache on 22.15+, so there is no CommonJS module to unwrap there.
        if !standalone {
            let out = run_with(
                root,
                node_dir.as_deref(),
                false,
                "hooked.ts",
                Some("--require=./redirect.cjs"),
            );
            assert_eq!(stdout(&out, &what), "1 2", "{what}");
        }
    }
}

/// The emit requires its helpers from Nub's runtime while a project's own `require()` of
/// the same package keeps the installed copy, as on plain Node.
#[test]
fn a_project_require_of_the_helper_package_gets_its_own_copy() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", "{}");
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"module":"commonjs"}}"#,
    );
    write(
        root,
        "node_modules/@oxc-project/runtime/package.json",
        r#"{"name":"@oxc-project/runtime","version":"0.0.0"}"#,
    );
    write(
        root,
        "node_modules/@oxc-project/runtime/helpers/usingCtx.js",
        "module.exports = 'project copy';
",
    );
    write(
        root,
        "user.cjs",
        "console.log(require('@oxc-project/runtime/helpers/usingCtx'));
",
    );
    write(
        root,
        "main.ts",
        "import \"./user.cjs\";
using r = { [Symbol.dispose]() { console.log(\"disposed\"); } };
console.log(\"body\");
",
    );
    for (what, node_dir, standalone) in legs(true) {
        let out = run(root, node_dir.as_deref(), standalone, "main.ts");
        assert_eq!(
            stdout(&out, &what),
            "project copy\nbody\ndisposed",
            "{what}"
        );
    }
}

/// Neither has a CommonJS form, so the run stops on the file instead of loading it as an
/// ES module. Under `"type": "module"` the same file stays an ES module and runs.
#[test]
fn module_commonjs_refuses_import_meta_and_top_level_await() {
    for (source, needle) in [
        (
            "console.log(import.meta.url);\n",
            "`import.meta` is not available",
        ),
        (
            "await Promise.resolve();\nconsole.log('ran');\n",
            "Top-level `await` is not available",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", "{}");
        write(
            root,
            "tsconfig.json",
            r#"{"compilerOptions":{"module":"commonjs"}}"#,
        );
        write(root, "main.ts", source);
        let out = run(root, None, false, "main.ts");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "{needle}: the run must stop: {stderr}"
        );
        assert!(stderr.contains(needle), "{needle}: {stderr}");
        assert!(
            stderr.contains("main.ts"),
            "the error names the file: {stderr}"
        );

        write(root, "package.json", r#"{"type":"module"}"#);
        let out = run(root, None, false, "main.ts");
        assert!(
            out.status.success(),
            "{needle}, type module: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// With `esModuleInterop` unset, `import * as` follows the installed TypeScript's
/// default: off in TypeScript 5, so the namespace is the callable `module.exports`; on
/// from 6, so it is an object.
#[test]
fn es_module_interop_defaults_to_the_installed_typescript() {
    for (version, expected) in [("5.9.3", "function"), ("6.0.3", "object")] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", "{}");
        write(
            root,
            "tsconfig.json",
            r#"{"compilerOptions":{"module":"commonjs"}}"#,
        );
        write(
            root,
            "node_modules/typescript/package.json",
            &format!(r#"{{"name":"typescript","version":"{version}"}}"#),
        );
        write(
            root,
            "node_modules/cjs-fn/package.json",
            r#"{"main":"index.js"}"#,
        );
        write(
            root,
            "node_modules/cjs-fn/index.js",
            "module.exports = () => 'fn';\n",
        );
        write(
            root,
            "main.ts",
            "import * as fn from \"cjs-fn\";\nconsole.log(typeof fn);\n",
        );
        let out = stdout(&run(root, None, false, "main.ts"), version);
        assert_eq!(out, expected, "TypeScript {version}");
    }
}
