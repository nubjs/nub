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

/// A register answering `extension` with `identity`, on either tier: `registerHooks`
/// where Node has it, else `module.register` with a sibling hooks file. Written to
/// `<dir>/<stem>.mjs`.
fn register(dir: &Path, stem: &str, extension: &str, identity: &str) {
    write(
        dir.join(format!("{stem}.mjs")),
        &format!(
            r#"import nodeModule from "node:module";
console.error("register:{identity}");
if (nodeModule.registerHooks) {{
  nodeModule.registerHooks({{ load(url, context, next) {{
    if (url.endsWith("{extension}")) return {{ format: "module", source: 'export default "{identity}"', shortCircuit: true }};
    return next(url, context);
  }} }});
}} else {{
  nodeModule.register("./{stem}-hooks.mjs", import.meta.url);
}}
"#
        ),
    );
    write(
        dir.join(format!("{stem}-hooks.mjs")),
        &format!(
            r#"export async function load(url, context, next) {{
  if (url.endsWith("{extension}")) return {{ format: "module", source: 'export default "{identity}"', shortCircuit: true }};
  return next(url, context);
}}
"#
        ),
    );
}

/// A mapper package at `<root>/node_modules/<name>` answering `extension` with
/// `identity`.
fn mapper(root: &Path, name: &str, extension: &str, identity: &str) {
    let dir = root.join("node_modules").join(name);
    write(
        dir.join("package.json"),
        &format!(r#"{{ "name": "{name}", "exports": {{ "./register": "./register.mjs" }} }}"#),
    );
    register(&dir, "register", extension, identity);
}

/// Whether the Node these tests run on has `module.registerHooks`.
fn has_register_hooks(cwd: &Path) -> bool {
    let output = nub(
        cwd,
        &[
            "--node",
            "-p",
            "typeof require('node:module').registerHooks",
        ],
    )
    .output()
    .unwrap();
    String::from_utf8_lossy(&output.stdout).trim() == "function"
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

/// A package whose register prints what it was handed, tagged with its name.
fn options_reader(root: &Path, name: &str) {
    let dir = root.join("node_modules").join(name);
    write(
        dir.join("package.json"),
        &format!(r#"{{ "name": "{name}", "exports": {{ "./register": "./register.mjs" }} }}"#),
    );
    write(
        dir.join("register.mjs"),
        &format!("console.error(\"{name}:\", process.env.CONTENT_MAPPER_OPTIONS);\n"),
    );
}

/// Each register sees its own entry's options while it loads, and nothing else does.
/// A plain Node user sets the same variable by hand, so the value stays theirs after.
#[test]
fn options_reach_only_their_own_register_while_it_loads() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    options_reader(root, "with");
    options_reader(root, "without");
    let options = r#"{"pattern":"q\"b\\c%5C%22"}"#;
    write(
        root.join("tsconfig.json"),
        &format!(
            r#"{{ "contentMappers": [
                {{ "package": "with", "extensions": [".a"], "options": {options} }},
                {{ "package": "without", "extensions": [".b"] }}
            ] }}"#
        ),
    );
    write(
        root.join("package.json"),
        r#"{ "scripts": { "start": "node main.mjs" } }"#,
    );
    write(
        root.join("main.mjs"),
        r#"import { spawnSync } from "node:child_process";
console.log("app:", process.env.CONTENT_MAPPER_OPTIONS);
const child = spawnSync(process.execPath, ["-p", "process.env.CONTENT_MAPPER_OPTIONS"], { encoding: "utf8" });
console.log("child:", child.stdout.trim());
"#,
    );

    let output = run(root, &["main.mjs"]);
    assert_eq!(stdout(&output), "app: undefined\nchild: undefined");
    let log = stderr(&output);
    assert!(log.contains(&format!("with: {options}")), "{log}");
    assert!(log.contains("without: undefined"), "{log}");

    // A script's `node` gets them from the snapshot. Only Windows can go red on the
    // spelling: its script shell rewrites backslashes in every variable it imports.
    let output = run(root, &["run", "start"]);
    assert!(stdout(&output).contains("app: undefined"));
    assert!(
        stderr(&output).contains(&format!("with: {options}")),
        "{}",
        stderr(&output)
    );

    // Listed under `preload` too, it still gets them, once, in the user's slot.
    write(
        root.join("nub.jsonc"),
        r#"{ "preload": ["with/register"] }"#,
    );
    let log = stderr(&run(root, &["main.mjs"]));
    assert!(log.contains(&format!("with: {options}")), "{log}");
    assert_eq!(log.matches("with: ").count(), 1, "{log}");
    std::fs::remove_file(root.join("nub.jsonc")).unwrap();

    let output = nub(root, &["main.mjs"])
        .env("CONTENT_MAPPER_OPTIONS", r#"{"mine":true}"#)
        .output()
        .unwrap();
    assert!(
        stdout(&output).starts_with(r#"app: {"mine":true}"#),
        "{}",
        stdout(&output)
    );
    assert!(stderr(&output).contains(&format!("with: {options}")));
}

/// A descendant's entry is the nearer config, so its options replace what it inherited.
#[test]
fn a_descendant_takes_options_from_its_own_config() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    options_reader(root, "m");
    let entry = |from: &str| {
        format!(
            r#"{{ "contentMappers": [{{ "package": "m", "extensions": [".a"], "options": {{ "from": "{from}" }} }}] }}"#
        )
    };
    write(root.join("tsconfig.json"), &entry("root"));
    write(root.join("member/tsconfig.json"), &entry("member"));
    write(root.join("member/main.mjs"), "\n");
    write(
        root.join("main.mjs"),
        r#"import { spawnSync } from "node:child_process";
process.stderr.write(spawnSync("node", ["member/main.mjs"], { encoding: "utf8", shell: true }).stderr);
process.stderr.write(spawnSync(process.execPath, ["-e", ""], { encoding: "utf8" }).stderr);
"#,
    );
    let log = stderr(&run(root, &["main.mjs"]));
    assert!(log.contains(r#"m: {"from":"root"}"#), "{log}");
    assert!(log.contains(r#"m: {"from":"member"}"#), "{log}");

    // A member listing it with no options has none, even with the register under the
    // inherited `preload` too, where no mapper entry is left to name the chainer. The
    // last child re-reads the root's chainer, so the member's must not overwrite it.
    write(root.join("nub.jsonc"), r#"{ "preload": ["m/register"] }"#);
    write(
        root.join("member/tsconfig.json"),
        r#"{ "contentMappers": [{ "package": "m", "extensions": [".a"] }] }"#,
    );
    let log = stderr(&run(root, &["main.mjs"]));
    assert_eq!(log.matches(r#"m: {"from":"root"}"#).count(), 2, "{log}");
    assert!(log.contains("m: undefined"), "{log}");
}

/// A config's own entry decides, so a referenced project's options do not reach it.
#[test]
fn the_first_entry_for_a_register_decides_its_options() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    options_reader(root, "m");
    write(
        root.join("tsconfig.json"),
        r#"{ "contentMappers": [{ "package": "m", "extensions": [".a"] }],
             "references": [{ "path": "./sub" }] }"#,
    );
    write(
        root.join("sub/tsconfig.json"),
        r#"{ "contentMappers": [{ "package": "m", "extensions": [".b"], "options": { "from": "sub" } }] }"#,
    );
    write(root.join("main.mjs"), "\n");
    let log = stderr(&run(root, &["main.mjs"]));
    assert!(log.contains("m: undefined"), "{log}");
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

/// One maximum per package could not order this: the general mapper's unrelated long
/// extension would sort it after the specific one, and its `.jsonc` hook would answer
/// for `a.schema.jsonc`.
#[test]
fn a_suffix_relation_decides_order_whatever_else_a_package_declares() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(root, "general", ".jsonc", "general");
    mapper(root, "specific", ".schema.jsonc", "specific");
    entry(root, &["a.schema.jsonc", "b.jsonc"]);
    write(
        root.join("tsconfig.json"),
        r#"{ "contentMappers": [
            { "package": "specific", "extensions": [".schema.jsonc"] },
            { "package": "general", "extensions": [".jsonc", ".unrelated-long-extension"] }
        ] }"#,
    );
    assert_eq!(stdout(&run(root, &["main.ts"])), "specific\ngeneral");

    // A package relating two of its OWN extensions is no constraint on itself, or nothing
    // can go first and the order falls back to declaration, wherever that puts the rest.
    mapper(root, "selfie", ".jsonc", "selfie");
    let selfie = r#"{ "package": "selfie", "extensions": [".jsonc", ".other.jsonc"] }"#;
    let specific = r#"{ "package": "specific", "extensions": [".schema.jsonc"] }"#;
    for mappers in [[selfie, specific], [specific, selfie]] {
        write(
            root.join("tsconfig.json"),
            &format!(r#"{{ "contentMappers": [{}] }}"#, mappers.join(",")),
        );
        assert_eq!(
            stdout(&run(root, &["main.ts"])),
            "specific\nselfie",
            "{mappers:?}"
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
    register(root, "mine", ".data", "from-preload");
    write(
        root.join("mine.cjs"),
        r#"const nodeModule = require("node:module");
if (nodeModule.registerHooks) {
  nodeModule.registerHooks({ load(url, context, next) {
    if (url.endsWith(".data")) return { format: "module", source: 'export default "from-preload"', shortCircuit: true };
    return next(url, context);
  } });
} else {
  nodeModule.register("./mine-hooks.mjs", require("node:url").pathToFileURL(__filename));
}
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

/// The run that discovers a mapper and the processes it spawns: a real-path child
/// inherits only the environment, and a shim child trusts the snapshot's list, so both
/// fail if the mapper reached the discovering process alone.
#[test]
fn a_descendant_that_discovers_a_mapper_passes_it_to_spawned_grandchildren() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(root, "one", ".one", "from-one");
    write(
        root.join("package.json"),
        r#"{ "scripts": { "reach": "node member/main.mjs" } }"#,
    );
    let member = root.join("member");
    write(member.join("tsconfig.json"), &tsconfig(&[("one", ".one")]));
    write(member.join("a.one"), "x\n");
    write(
        member.join("grandchild.mjs"),
        "console.log('grandchild:' + (await import('./a.one')).default);\n",
    );
    write(
        member.join("main.mjs"),
        r#"import { spawnSync } from "node:child_process";
const cwd = new URL(".", import.meta.url);
for (const bin of [process.execPath, "node"]) {
  const child = spawnSync(bin, ["grandchild.mjs"], { cwd, stdio: "inherit" });
  if (child.status !== 0) process.exit(1);
}
"#,
    );
    let output = stdout(&run(root, &["run", "reach"]));
    assert_eq!(output.matches("grandchild:from-one").count(), 2, "{output}");
}

/// The same descent with an explicit `preload` already on the inherited chainer: left
/// there, it registers before the new mapper and the mapper wrongly outranks it.
#[test]
fn a_descendant_keeps_explicit_preload_above_a_newly_discovered_mapper() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(root, "one", ".data", "from-mapper");
    write(
        root.join("package.json"),
        r#"{ "scripts": { "reach": "node member/main.ts" } }"#,
    );
    write(root.join("nub.jsonc"), r#"{ "preload": ["./mine.mjs"] }"#);
    register(root, "mine", ".data", "from-preload");
    let member = root.join("member");
    write(member.join("tsconfig.json"), &tsconfig(&[("one", ".data")]));
    entry(&member, &["a.data"]);

    let output = run(root, &["run", "reach"]);
    assert!(
        stdout(&output).contains("from-preload"),
        "{}",
        stderr(&output)
    );
    // The script's `node` is one process, so one evaluation each. The parent `nub run`
    // runs no Node of its own here.
    assert_eq!(
        stderr(&output).matches("register:from-preload").count(),
        1,
        "{}",
        stderr(&output)
    );
    assert_eq!(
        stderr(&output).matches("register:from-mapper").count(),
        1,
        "{}",
        stderr(&output)
    );
}

/// An ambient `--require` folded into the chainer, beside a mapper. The chainer's name
/// is what stops the runtime reading it as a foreign loader: on the Node 22.15-24.11
/// band that would force the async tier, whose loader worker runs every `--require`
/// again. Passes trivially elsewhere; CI's 22.15 leg is the one that can go red.
#[test]
fn a_mapper_chain_does_not_rerun_a_folded_require_in_a_loader_worker() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(root, "m", ".data", "from-mapper");
    write(root.join("tsconfig.json"), &tsconfig(&[("m", ".data")]));
    write(
        root.join("agent.cjs"),
        "console.error('agent thread ' + require('node:worker_threads').threadId);\n",
    );
    entry(root, &["a.data"]);

    let output = nub(root, &["main.ts"])
        .env(
            "NODE_OPTIONS",
            format!("--require={}", root.join("agent.cjs").display()),
        )
        .output()
        .unwrap();
    assert_eq!(stdout(&output), "from-mapper");
    let agents: Vec<&str> = std::str::from_utf8(&output.stderr)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("agent thread"))
        .collect();
    assert_eq!(agents, ["agent thread 0"]);
}

/// A folded preload's only trace is the chainer, and a descendant `node` writes its own.
#[test]
fn a_descendant_keeps_preloads_folded_from_the_ambient_node_options() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root.join("nub.jsonc"), r#"{ "preload": ["./mine.mjs"] }"#);
    write(root.join("mine.mjs"), "\n");
    write(root.join("agent.cjs"), "console.log('agent cjs');\n");
    write(root.join("agent.mjs"), "console.log('agent mjs');\n");
    write(root.join("kid.mjs"), "\n");
    write(
        root.join("main.mjs"),
        r#"import { spawnSync } from "node:child_process";
for (const bin of ["node", process.execPath, "node"]) {
  spawnSync(bin, ["kid.mjs"], { stdio: "inherit" });
}
"#,
    );
    let output = nub(root, &["main.mjs"])
        .env(
            "NODE_OPTIONS",
            format!(
                "--require={} --import={}",
                root.join("agent.cjs").display(),
                root.join("agent.mjs").display()
            ),
        )
        .output()
        .unwrap();
    let printed = stdout(&output);
    assert_eq!(printed.matches("agent cjs").count(), 4, "{printed}");
    assert_eq!(printed.matches("agent mjs").count(), 4, "{printed}");
}

/// The other side of carrying them: a child that took them out of its `NODE_OPTIONS`
/// does not get them back, as under plain Node.
#[test]
fn a_child_that_clears_or_replaces_node_options_drops_the_ambient_preloads() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root.join("agent.cjs"), "console.log('agent cjs');\n");
    write(root.join("agent.mjs"), "console.log('agent mjs');\n");
    write(root.join("kid.mjs"), "console.log('kid');\n");
    write(
        root.join("main.mjs"),
        r#"import { spawnSync } from "node:child_process";
const { NODE_OPTIONS, ...cleared } = process.env;
for (const [label, env] of [
  ["inherited", process.env],
  ["cleared", cleared],
  ["replaced", { ...process.env, NODE_OPTIONS: "--no-warnings" }],
]) {
  const { stdout } = spawnSync("node", ["kid.mjs"], { env, encoding: "utf8" });
  console.log(label, stdout.trim().split(/\r?\n/).join(" "));
}
"#,
    );
    let ambient = |flags: String| {
        let output = nub(root, &["main.mjs"])
            .env("NODE_OPTIONS", flags)
            .output()
            .unwrap();
        stdout(&output)
    };
    let require = format!("--require={}", root.join("agent.cjs").display());
    let import = format!("--import={}", root.join("agent.mjs").display());
    assert_eq!(
        ambient(format!("{require} {import}")),
        "agent cjs\nagent mjs\ninherited agent cjs agent mjs kid\ncleared kid\nreplaced kid"
    );
    assert_eq!(
        ambient(require),
        "agent cjs\ninherited agent cjs kid\ncleared kid\nreplaced kid"
    );
}

/// A space in the project's path reaches `NODE_OPTIONS` quoted once, for a `preload` line
/// and for a mapper, directly and through a script's `node`.
#[test]
fn a_project_path_with_a_space_loads_preloads_and_mappers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sp ace");
    write(root.join("mine.mjs"), "console.error('mine ran');\n");
    write(root.join("nub.jsonc"), r#"{ "preload": ["./mine.mjs"] }"#);
    write(root.join("plain.mjs"), "console.log('plain');\n");
    let output = run(&root, &["plain.mjs"]);
    assert_eq!(stdout(&output), "plain");
    assert!(stderr(&output).contains("mine ran"), "{}", stderr(&output));

    std::fs::remove_file(root.join("nub.jsonc")).unwrap();
    mapper(&root, "m", ".data", "from-mapper");
    write(root.join("tsconfig.json"), &tsconfig(&[("m", ".data")]));
    write(
        root.join("package.json"),
        r#"{ "scripts": { "start": "node main.ts" } }"#,
    );
    entry(&root, &["a.data"]);
    assert_eq!(stdout(&run(&root, &["main.ts"])), "from-mapper");
    assert!(stdout(&run(&root, &["run", "start"])).contains("from-mapper"));
}

/// A foreign `--import` on Node 22.15-24.11 puts Nub on the async tier, where a discovered
/// register must not make CommonJS files throw: nobody asked for it on this run.
/// `foreign_register_cjs.rs` covers the rest of that tier. Passes trivially off that band;
/// CI's 22.15 leg is the one that can go red.
#[test]
fn a_foreign_import_and_a_register_leave_commonjs_loading() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mapper(root, "m", ".data", "from-mapper");
    write(root.join("tsconfig.json"), &tsconfig(&[("m", ".data")]));
    write(root.join("noop.mjs"), "\n");
    write(root.join("a.data"), "x\n");
    write(root.join("c.cjs"), "module.exports = 'from-cjs';\n");
    write(
        root.join("main.mjs"),
        "import c from './c.cjs';\nimport d from './a.data';\nconsole.log(c, d);\n",
    );
    assert_eq!(
        stdout(&run(root, &["--import", "./noop.mjs", "main.mjs"])),
        "from-cjs from-mapper"
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
    if !has_register_hooks(root) {
        // Indistinguishable from a register that needs the missing API, so it is skipped.
        assert_eq!(stdout(&output), "ran");
        return;
    }
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
        let output = run(&dir, &["run", "start"]);
        assert!(
            stdout(&output).contains(&format!("from-{member}")),
            "{member}"
        );
        // Only Windows can go red here: its script shell rewrites the snapshot's
        // backslashes, which gave the descendant a second spelling of the register.
        let log = stderr(&output);
        assert_eq!(
            log.matches(&format!("register:from-{member}")).count(),
            1,
            "{log}"
        );
    }

    let chains: Vec<_> = std::fs::read_dir(root.join("node_modules/.nub"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let body = std::fs::read_to_string(entry.path()).unwrap();
            let imports: Vec<String> = body
                .lines()
                .filter(|line| line.contains("import("))
                .map(str::to_string)
                .collect();
            (entry.file_name(), imports)
        })
        .collect();
    assert_eq!(
        chains.len(),
        2,
        "each member's mapper set gets its own chainer: {chains:#?}"
    );
    assert!(stdout(&run(root, &["run", "reach"])).contains("from-one"));
}
