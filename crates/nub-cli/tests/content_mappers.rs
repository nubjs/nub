//! tsconfig `contentMappers` as the registry of runtime plugins: a listed package's
//! `./register` loads with no `nub.jsonc` line.
//!
//! Each fixture's register announces itself on stderr and answers its extension with
//! its own identity, so a test sees WHICH copy loaded, how many times, and which hook
//! answered, rather than only that something did.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn write(path: impl AsRef<Path>, body: &str) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A mapper package at `<root>/node_modules/<name>` answering `extension` with
/// `identity`.
fn mapper(root: &Path, name: &str, extension: &str, identity: &str) {
    let dir = root.join("node_modules").join(name);
    write(
        dir.join("package.json"),
        &format!(r#"{{ "name": "{name}", "exports": {{ "./register": "./register.mjs" }} }}"#),
    );
    write(
        dir.join("register.mjs"),
        &format!(
            r#"import {{ registerHooks }} from "node:module";
console.error("register:{identity}");
registerHooks({{ load(url, context, next) {{
  if (url.endsWith("{extension}")) return {{ format: "module", source: 'export default "{identity}"', shortCircuit: true }};
  return next(url, context);
}} }});
"#
        ),
    );
}

fn tsconfig(entries: &[(&str, &str)]) -> String {
    let entries: Vec<String> = entries
        .iter()
        .map(|(package, ext)| format!(r#"{{ "package": "{package}", "extensions": ["{ext}"] }}"#))
        .collect();
    format!(r#"{{ "contentMappers": [{}] }}"#, entries.join(", "))
}

/// A `main.ts` printing the default export of each of `imports`, one per line.
fn entry(dir: &Path, imports: &[&str]) {
    let mut source = String::new();
    for (index, file) in imports.iter().enumerate() {
        source.push_str(&format!("import v{index} from \"./{file}\";\n"));
    }
    for index in 0..imports.len() {
        source.push_str(&format!("console.log(String(v{index}));\n"));
    }
    write(dir.join("main.ts"), &source);
    for file in imports {
        write(dir.join(file), "a: 1\n");
    }
}

fn nub(cwd: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_nub")));
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .env_remove("__NUB_RUNTIME_CONFIG");
    command
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    nub(cwd, args).output().unwrap()
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The entry's own directory picks the tsconfig, and the register's hook sits above
/// Nub's built-in `.yaml` loader, which would otherwise answer with a parsed object.
#[test]
fn a_listed_package_register_answers_with_no_nub_config_and_beats_a_builtin_loader() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("sub");
    mapper(&project, "yaml-mapper", ".yaml", "from-mapper");
    write(
        project.join("tsconfig.json"),
        &tsconfig(&[("yaml-mapper", ".yaml")]),
    );
    entry(&project, &["a.yaml"]);

    assert_eq!(stdout(&run(&project, &["main.ts"])), "from-mapper");
    assert_eq!(
        stdout(&run(dir.path(), &["sub/main.ts"])),
        "from-mapper",
        "a run from the parent directory reads the entry's tsconfig"
    );
    assert!(
        !dir.path().join("node_modules").exists(),
        "the chainer belongs with the mapper's package, not in the CWD"
    );
}

/// microsoft/TypeScript#65105: `package` resolves from the config that declares the
/// array. Both copies exist, so resolving from the leaf would load the wrong one and
/// still look like it worked.
#[test]
fn a_mapper_inherited_through_extends_resolves_from_the_declaring_config() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(&root.join("config"), "shared", ".data", "config-copy");
    mapper(&root.join("app"), "shared", ".data", "app-copy");
    write(
        root.join("config/base.json"),
        &tsconfig(&[("shared", ".data")]),
    );
    write(
        root.join("app/tsconfig.json"),
        r#"{ "extends": ["../config/base.json"] }"#,
    );
    entry(&root.join("app"), &["a.data"]);

    assert_eq!(stdout(&run(&root.join("app"), &["main.ts"])), "config-copy");
}

/// `create-vite`'s layout: the config a directory walk finds only has `references`.
#[test]
fn a_solution_style_root_loads_the_mappers_of_its_referenced_projects() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(root, "app-mapper", ".view", "from-app-project");
    write(
        root.join("tsconfig.json"),
        r#"{ "files": [], "references": [{ "path": "./tsconfig.app.json" }, { "path": "./tsconfig.node.json" }] }"#,
    );
    write(
        root.join("tsconfig.app.json"),
        &tsconfig(&[("app-mapper", ".view")]),
    );
    write(
        root.join("tsconfig.node.json"),
        r#"{ "include": ["vite.config.ts"] }"#,
    );
    entry(&root.join("src"), &["a.view"]);

    assert_eq!(stdout(&run(root, &["src/main.ts"])), "from-app-project");
}

/// Declaration order must not matter: TypeScript routes by longest suffix, and
/// `registerHooks` runs the last-registered hook first. Listing the general mapper
/// last would make it swallow both files if Nub kept declaration order.
#[test]
fn the_longest_extension_answers_first_in_either_declaration_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(root, "general", ".jsonc", "general");
    mapper(root, "specific", ".schema.jsonc", "specific");
    entry(root, &["a.schema.jsonc", "b.jsonc"]);

    for order in [
        [("general", ".jsonc"), ("specific", ".schema.jsonc")],
        [("specific", ".schema.jsonc"), ("general", ".jsonc")],
    ] {
        write(root.join("tsconfig.json"), &tsconfig(&order));
        assert_eq!(
            stdout(&run(root, &["main.ts"])),
            "specific\ngeneral",
            "{order:?}"
        );
    }
}

/// An explicit entry registers after every mapper so it answers first, on the `.cjs`
/// path too, where it would otherwise run in the earlier `--require` phase. One that
/// names a discovered register loads it once, in the slot the user gave it: listed
/// after `mine.cjs`, the mapper registers later and answers first.
#[test]
fn an_explicit_preload_takes_precedence_and_a_duplicate_loads_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(root, "m", ".data", "from-mapper");
    write(root.join("tsconfig.json"), &tsconfig(&[("m", ".data")]));
    entry(root, &["a.data"]);
    write(
        root.join("mine.cjs"),
        r#"require("node:module").registerHooks({ load(url, context, next) {
  if (url.endsWith(".data")) return { format: "module", source: 'export default "from-preload"', shortCircuit: true };
  return next(url, context);
} });
"#,
    );

    write(root.join("nub.jsonc"), r#"{ "preload": ["./mine.cjs"] }"#);
    assert_eq!(stdout(&run(root, &["main.ts"])), "from-preload");

    write(
        root.join("nub.jsonc"),
        r#"{ "preload": ["./mine.cjs", "m/register"] }"#,
    );
    let output = run(root, &["main.ts"]);
    assert_eq!(stdout(&output), "from-mapper");
    assert_eq!(
        stderr(&output).matches("register:from-mapper").count(),
        1,
        "{}",
        stderr(&output)
    );
}

/// A Node with `registerHooks` has no reason to excuse a register, so its failure ends
/// the run with its own stack, as `node --import` does. Only a Node lacking the API
/// skips one (`version_tiers.rs`).
#[test]
fn a_register_that_throws_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(root, "m", ".data", "from-mapper");
    write(
        root.join("node_modules/m/register.mjs"),
        "throw new Error('register blew up');\n",
    );
    write(root.join("tsconfig.json"), &tsconfig(&[("m", ".data")]));
    write(root.join("plain.js"), "console.log('ran');\n");

    let output = run(root, &["plain.js"]);
    assert!(!output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("register blew up"),
        "{}",
        stderr(&output)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("ran"));
}

/// `--node` and `NODE_COMPAT` add nothing, and neither explains anything: the error is
/// Node's own. The first test here is the positive control.
#[test]
fn compat_mode_loads_no_register() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(root, "m", ".data", "from-mapper");
    write(
        root.join("tsconfig.json"),
        &tsconfig(&[("m", ".data"), ("gone", ".gone")]),
    );
    write(root.join("plain.js"), "console.log('ran');\n");
    write(root.join("broken.mjs"), "await import('./a.gone');\n");
    write(root.join("a.gone"), "x\n");

    for mut command in [nub(root, &["--node", "plain.js"]), {
        let mut compat = nub(root, &["plain.js"]);
        compat.env("NODE_COMPAT", "1");
        compat
    }] {
        let output = command.output().unwrap();
        assert_eq!(stdout(&output), "ran");
        assert!(
            !stderr(&output).contains("register:"),
            "{}",
            stderr(&output)
        );
    }
    let output = run(root, &["--node", "broken.mjs"]);
    assert!(
        stderr(&output).contains("ERR_UNKNOWN_FILE_EXTENSION"),
        "{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("is mapped in"),
        "{}",
        stderr(&output)
    );
}

/// A package with nothing to preload changes nothing until a mapped file is imported,
/// and then the error says why. `stray` has a `register.js` but no `exports` map, so it
/// never promised one; the unrelated extension is the control for the hint.
#[test]
fn a_mapped_extension_with_no_runtime_is_skipped_until_imported_then_named() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let stray = root.join("node_modules/stray");
    write(stray.join("package.json"), r#"{ "name": "stray" }"#);
    write(stray.join("register.js"), "console.log('stray loaded');\n");
    write(
        root.join("tsconfig.json"),
        &tsconfig(&[("stray", ".ini"), ("gone", ".gone")]),
    );
    write(root.join("plain.js"), "console.log('ran');\n");
    for file in ["a.ini", "a.gone", "a.other"] {
        write(root.join(file), "x\n");
    }

    let plain = run(root, &["plain.js"]);
    assert_eq!(stdout(&plain), "ran");

    let import_error = |file: &str| {
        write(
            root.join("imp.mjs"),
            &format!("await import('./{file}');\n"),
        );
        let output = run(root, &["imp.mjs"]);
        assert!(!output.status.success());
        stderr(&output)
    };
    let no_register = import_error("a.ini");
    assert!(
        no_register.contains(r#""stray", which has no "./register" export"#),
        "{no_register}"
    );
    let missing = import_error("a.gone");
    assert!(
        missing.contains(r#""gone", which is not installed"#),
        "{missing}"
    );
    let unrelated = import_error("a.other");
    assert!(
        unrelated.contains("ERR_UNKNOWN_FILE_EXTENSION"),
        "{unrelated}"
    );
    assert!(!unrelated.contains("is mapped in"), "{unrelated}");
}

/// Members sharing a hoisted `node_modules` list different mappers. A chainer with one
/// fixed name would hand a member the other's plugins the next time it forked. A root
/// script reaches a member's entry through `node`, whose environment is already
/// augmented, and still gets that entry's mappers.
#[test]
fn workspace_members_keep_their_own_mappers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(root, "one", ".one", "from-one");
    mapper(root, "two", ".two", "from-two");
    write(
        root.join("package.json"),
        r#"{ "private": true, "workspaces": ["packages/*"],
             "scripts": { "reach": "node packages/one/main.ts" } }"#,
    );
    for (member, extension) in [("one", ".one"), ("two", ".two")] {
        let dir = root.join("packages").join(member);
        write(
            dir.join("package.json"),
            &format!(r#"{{ "name": "{member}", "scripts": {{ "start": "node main.ts" }} }}"#),
        );
        write(dir.join("tsconfig.json"), &tsconfig(&[(member, extension)]));
        entry(&dir, &[&format!("a{extension}")]);
        assert!(
            stdout(&run(&dir, &["run", "start"])).contains(&format!("from-{member}")),
            "{member}"
        );
    }

    let chains = std::fs::read_dir(root.join("node_modules/.nub"))
        .unwrap()
        .count();
    assert_eq!(chains, 2, "each member's mapper set gets its own chainer");
    assert!(stdout(&run(root, &["run", "reach"])).contains("from-one"));
}
