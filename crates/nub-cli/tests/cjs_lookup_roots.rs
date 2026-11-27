//! Nub's own CommonJS resolution looks where Node looks: in each root of a
//! `require.resolve` `paths` option in turn, and in the parent's `module.paths`,
//! which need not lie above the importer. Cases with no TypeScript run under plain
//! `node`, under `nub` and under the standalone loader, and all three must print the
//! same. Unix-only: the fixture links workspace packages into `node_modules`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn link(target: &str, at: &Path) {
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, at).unwrap();
}

fn hermetic(mut command: Command, cwd: &Path, cache: &Path) -> String {
    let out: Output = command
        .current_dir(cwd)
        .env("XDG_CACHE_HOME", cache)
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{command:?} exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn nub(cwd: &Path, cache: &Path, entry: &str) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nub"));
    command.arg(entry);
    hermetic(command, cwd, cache)
}

fn node(cwd: &Path, cache: &Path, preload: Option<&str>, entry: &str) -> String {
    let mut command = Command::new("node");
    if let Some(preload) = preload {
        let flag = if preload.ends_with(".cjs") {
            "--require"
        } else {
            "--import"
        };
        let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
        command.arg(format!("{flag}={}/{preload}", runtime.display()));
    }
    command.arg(entry);
    hermetic(command, cwd, cache)
}

/// `caller` links `lib` to `lib-a`, `other` to `lib-b`, and `first` to a `lib-c`
/// with no `dist/missing.js`; `empty` links nothing. The extensionless
/// `lib/dist/missing` is a subpath Nub probes itself. In `ended`, Node's lookup
/// stops with an error: `lib` exports a `./feature` that is missing, and `legacy`
/// names a missing `main`, while `other` has both.
fn fixture(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("nub-cjs-roots-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for pkg in ["lib-a", "lib-b", "lib-c"] {
        write(
            &root.join("packages").join(pkg).join("package.json"),
            r#"{"name":"lib"}"#,
        );
    }
    for pkg in ["lib-a", "lib-b"] {
        write(
            &root.join("packages").join(pkg).join("dist/missing.js"),
            &format!("module.exports = {pkg:?};"),
        );
    }
    for (dir, pkg) in [("caller", "lib-a"), ("other", "lib-b"), ("first", "lib-c")] {
        link(
            &format!("../../packages/{pkg}"),
            &root.join(dir).join("node_modules/lib"),
        );
    }
    write(
        &root.join("packages/lib-e/package.json"),
        r#"{"name":"lib","exports":{"./feature":"./feature.js"}}"#,
    );
    link("../../packages/lib-e", &root.join("ended/node_modules/lib"));
    write(&root.join("packages/lib-b/feature.js"), "");
    write(
        &root.join("ended/node_modules/legacy/package.json"),
        r#"{"main":"missing.js"}"#,
    );
    write(&root.join("other/node_modules/legacy/index.js"), "");
    for dir in ["caller", "other", "first", "empty", "ended"] {
        write(&root.join(dir).join("package.json"), "{}");
    }
    for dir in ["caller", "other"] {
        write(&root.join(dir).join("x.js"), "module.exports = 1;");
        write(&root.join(dir).join("y.ts"), "module.exports = 1;");
    }
    root
}

const LOOKUPS: &str = r#"const Module = require("node:module");
const path = require("node:path");
const root = path.resolve(__dirname, "..");
const at = (dir) => path.join(root, dir);
const show = (resolve) => {
  try { return path.relative(root, resolve()); } catch (e) { return e.code; }
};
const { resolve } = Module.createRequire(__filename);
const lines = [
  show(() => resolve("lib/dist/missing", { paths: [at("other")] })),
  show(() => resolve("lib/dist/missing", { paths: [at("empty")] })),
  show(() => resolve("lib/dist/missing", { paths: [at("first"), at("other")] })),
  show(() => require.resolve("lib/dist/missing", { paths: [at("other")] })),
  show(() => Module._resolveFilename("lib/dist/missing", { paths: [at("other/node_modules")] })),
  show(() => resolve("./x.js", { paths: [at("other")] })),
  show(() => resolve("lib/feature", { paths: [at("ended"), at("other")] })),
  show(() => resolve("legacy", { paths: [at("ended"), at("other")] })),
];
module.paths.unshift(at("other/node_modules"));
lines.push(show(() => require.resolve("lib/dist/missing")));
try { lines.push(require("lib/dist/missing")); } catch (e) { lines.push(e.code); }
console.log(lines.join("\n"));
"#;

#[test]
fn bare_subpaths_resolve_from_the_roots_node_searches() {
    let root = fixture("lookups");
    let caller = root.join("caller");
    let cache = root.join(".cache");
    write(&caller.join("app.cjs"), LOOKUPS);

    let expected = node(&caller, &cache, None, "app.cjs");
    assert_eq!(
        expected,
        [
            "packages/lib-b/dist/missing.js",
            "MODULE_NOT_FOUND",
            "packages/lib-b/dist/missing.js",
            "packages/lib-b/dist/missing.js",
            "packages/lib-b/dist/missing.js",
            "other/x.js",
            "MODULE_NOT_FOUND",
            "MODULE_NOT_FOUND",
            "packages/lib-b/dist/missing.js",
            "lib-b",
        ]
        .join("\n"),
        "plain node is the reference"
    );
    assert_eq!(nub(&caller, &cache, "app.cjs"), expected, "nub");
    for preload in ["loader-register.mjs", "loader-register.cjs"] {
        assert_eq!(
            node(&caller, &cache, Some(preload), "app.cjs"),
            expected,
            "node with {preload}"
        );
    }
}

/// A TypeScript importer's `./y.js` maps to `y.ts`, and under a `paths` option it
/// does so in the root, where Node resolves the relative request, not beside the
/// importer. A root reached through a symlink answers with the real path, as Node
/// does, and a root where Node's lookup ends in an error ends it here too.
#[test]
fn relative_request_resolves_in_the_paths_root() {
    let root = fixture("relative");
    let caller = root.join("caller");
    link("other", &root.join("other-link"));
    write(
        &root.join("ended/dir/package.json"),
        r#"{"main":"gone.js"}"#,
    );
    write(&root.join("other/dir/index.js"), "");
    write(
        &caller.join("app.cts"),
        r#"const path = require("node:path");
const { resolve } = require("node:module").createRequire(__filename);
const at = (dir) => path.join(__dirname, "..", dir);
const show = (request, ...roots) => {
  try { return path.relative(path.dirname(__dirname), resolve(request, { paths: roots.map(at) })); }
  catch (e) { return e.code; }
};
console.log([
  show("./x.js", "other"),
  show("./y.js", "other"),
  show("./x.js", "other-link"),
  show("./y.js", "other-link"),
  show("./y.js", "first", "other"),
  show("./dir", "ended", "other"),
].join(" "));
"#,
    );
    assert_eq!(
        nub(&caller, &root.join(".cache"), "app.cts"),
        "other/x.js other/y.ts other/x.js other/y.ts other/y.ts MODULE_NOT_FOUND"
    );
}

/// A tsconfig `paths` alias belongs to the importing file, so the caller's own
/// mapping answers whatever roots a `require.resolve` `paths` option names.
#[test]
fn aliases_come_from_the_importers_tsconfig() {
    let root = fixture("alias");
    let caller = root.join("caller");
    for (dir, target) in [("caller", "a"), ("other", "b")] {
        write(
            &root.join(dir).join("tsconfig.json"),
            &format!(r#"{{"compilerOptions":{{"paths":{{"alias/*":["./{target}/*"]}}}}}}"#),
        );
        write(&root.join(dir).join(target).join("feature.ts"), "");
    }
    write(
        &caller.join("app.cts"),
        r#"const path = require("node:path");
const { resolve } = require("node:module").createRequire(__filename);
const other = path.join(__dirname, "../other");
console.log([resolve("alias/feature"), resolve("alias/feature", { paths: [other] }),
  resolve("alias/feature", { paths: [] })]
  .map((file) => path.relative(path.dirname(__dirname), file))
  .join(" "));
"#,
    );
    assert_eq!(
        nub(&caller, &root.join(".cache"), "app.cts"),
        "caller/a/feature.ts caller/a/feature.ts caller/a/feature.ts"
    );
}
