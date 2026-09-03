//! End-to-end coverage for project `nub.jsonc` runtime consumers.
//!
//! Deliberately NOT `#![cfg(unix)]`. Windows is exactly where this plumbing
//! diverges — `NODE_OPTIONS` is a single tokenized string with its own quoting
//! rules, and every path in the snapshot is anchored differently — so gating the
//! whole file would leave that leg verifying none of it. Only the two constructs
//! that genuinely need unix are gated per-test: a `#!/usr/bin/env node` shim in
//! `node_modules/.bin` (Windows resolves local bins through `.cmd` shims, a
//! different path with its own coverage) and the mode bit that makes it
//! executable.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

/// The `nub-node-shim-*` PATH entries this test process already carries. A
/// nested launch must end up with exactly these plus its own: the outer run's
/// shim is replaced, never accumulated on top.
fn ambient_shim_entries() -> Vec<String> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    std::env::split_paths(&path)
        .map(|entry| entry.to_string_lossy().into_owned())
        .filter(|entry| entry.contains("nub-node-shim-"))
        .collect()
}

fn probe_output(label: &str, mut command: Command) -> std::process::Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{label} exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Parse a probe whose only stdout is one JSON object plus its trailing newline.
fn probe_json(label: &str, command: Command) -> serde_json::Value {
    let output = probe_output(label, command);
    let json = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    serde_json::from_slice(json).unwrap_or_else(|err| {
        panic!(
            "{label}: stdout was not exactly one JSON object: {err}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// Locate the JSON line for dlx probes, which may print fetch progress first.
fn probe_json_after_progress(label: &str, command: Command) -> serde_json::Value {
    let output = probe_output(label, command);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("{label}: no JSON line in output: {stdout}"));
    serde_json::from_str(line).unwrap_or_else(|err| panic!("{label}: {err} in {line}"))
}

/// The observations every route shares: the config's `envFile`, its `loader`
/// map, its `tsconfig` paths and JSX factory, and both condition sources — the
/// `nub.jsonc` `conditions` list AND the tsconfig's `customConditions` — all reached
/// the child.
fn assert_config_applied(label: &str, value: &serde_json::Value) {
    assert_eq!(value["env"], "from-config", "{label}: {value}");
    assert_eq!(value["text"], "loaded-text", "{label}: {value}");
    assert_eq!(value["alias"], "aliased", "{label}: {value}");
    assert_eq!(value["condition"], "condition", "{label}: {value}");
    assert_eq!(value["tsCondition"], "ts-condition", "{label}: {value}");
    assert_eq!(value["jsxMode"], "classic", "{label}: {value}");
}

/// Spawn a `nub watch` run — which never exits on its own — and return the first
/// line its watched child prints. The reader thread drains the rest so the child
/// never blocks on a full pipe, and the child is killed before the line is
/// unwrapped so a timeout cannot strand a watcher.
fn watch_first_line(mut command: Command) -> String {
    use std::io::BufRead;

    let mut child = command.stdout(Stdio::piped()).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = std::io::BufReader::new(stdout).lines();
        let _ = tx.send(lines.next().transpose());
        for _ in lines {}
    });
    let first = rx.recv_timeout(Duration::from_secs(15));
    let _ = child.kill();
    let _ = child.wait();
    first
        .expect("watch child produced no output within the budget")
        .expect("reading the watch child's first line")
        .expect("watch child closed stdout without printing")
}

struct Fixture {
    temp: tempfile::TempDir,
    project: PathBuf,
    xdg_config: PathBuf,
    cache: PathBuf,
    node: PathBuf,
    #[cfg(unix)]
    nubx: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let node = temp
            .path()
            .join(format!("node{}", std::env::consts::EXE_SUFFIX));
        // Nub dispatches on argv0, so this alias reaches its `node` shim; the
        // same mechanism supplies the unix-only `nubx` route below.
        #[cfg(unix)]
        std::os::unix::fs::symlink(nub_binary(), &node).unwrap();
        #[cfg(windows)]
        if std::fs::hard_link(nub_binary(), &node).is_err() {
            std::fs::copy(nub_binary(), &node).unwrap();
        }
        #[cfg(unix)]
        let nubx = {
            let nubx = temp.path().join("nubx");
            std::os::unix::fs::symlink(nub_binary(), &nubx).unwrap();
            nubx
        };
        std::fs::create_dir_all(project.join("node_modules/.bin")).unwrap();
        std::fs::create_dir_all(project.join("node_modules/conditional-pkg")).unwrap();
        std::fs::create_dir_all(project.join("node_modules/ts-condition-pkg")).unwrap();

        std::fs::write(
            project.join("nub.jsonc"),
            r#"{
              "preload": ["./preload.mjs"],
              "nodeOptions": ["--stack-trace-limit=23"],
              "v8Flags": ["--max-old-space-size=256"],
              "envFile": ["./runtime.env"],
              "loader": { ".blob": "text", ".view": "jsx" },
              "conditions": ["runtime-config"],
              "tsconfig": "./tsconfig.runtime.jsonc"
            }"#,
        )
        .unwrap();
        std::fs::write(
            project.join("preload.mjs"),
            "globalThis.__runtimePreload = 'preloaded';\n",
        )
        .unwrap();
        std::fs::write(project.join("runtime.env"), "RUNTIME_ENV=from-config\n").unwrap();
        std::fs::write(
            project.join("alias.ts"),
            "export const alias = 'aliased';\n",
        )
        .unwrap();
        std::fs::write(
            project.join("tsconfig.runtime.jsonc"),
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "runtime-alias": ["./alias.ts"] }, "jsx": "react", "jsxFactory": "make", "customConditions": ["ts-declared"] } }"#,
        )
        .unwrap();
        std::fs::write(project.join("message.blob"), "loaded-text").unwrap();
        std::fs::write(
            project.join("component.view"),
            "const make = (tag) => ({ tag, mode: 'classic' });\nexport default <widget />;\n",
        )
        .unwrap();
        std::fs::write(
            project.join("node_modules/conditional-pkg/package.json"),
            r#"{ "type": "module", "exports": { ".": { "runtime-config": "./custom.js", "default": "./default.js" } } }"#,
        )
        .unwrap();
        std::fs::write(
            project.join("node_modules/conditional-pkg/custom.js"),
            "export default 'condition';\n",
        )
        .unwrap();
        std::fs::write(
            project.join("node_modules/conditional-pkg/default.js"),
            "export default 'default';\n",
        )
        .unwrap();
        // A SECOND conditional package, keyed on the condition the tsconfig declares
        // rather than the one nub.jsonc does. Separate from `conditional-pkg` because
        // one `exports` map can only prove whichever key it lists first — two packages
        // let each source be observed on its own.
        std::fs::write(
            project.join("node_modules/ts-condition-pkg/package.json"),
            r#"{ "type": "module", "exports": { ".": { "ts-declared": "./custom.js", "default": "./default.js" } } }"#,
        )
        .unwrap();
        std::fs::write(
            project.join("node_modules/ts-condition-pkg/custom.js"),
            "export default 'ts-condition';\n",
        )
        .unwrap();
        std::fs::write(
            project.join("node_modules/ts-condition-pkg/default.js"),
            "export default 'default';\n",
        )
        .unwrap();
        std::fs::write(
            project.join("main.ts"),
            r#"import text from './message.blob';
import { alias } from 'runtime-alias';
import condition from 'conditional-pkg';
import tsCondition from 'ts-condition-pkg';
import component from './component.view';
console.log(JSON.stringify({
  env: process.env.RUNTIME_ENV,
  preload: globalThis.__runtimePreload,
  text, alias, condition, tsCondition,
  jsxMode: component.mode,
  stack: Error.stackTraceLimit,
  execArgv: process.execArgv,
  nodeOptions: process.env.NODE_OPTIONS,
  runtimeSnapshot: process.env.__NUB_RUNTIME_CONFIG,
}));
"#,
        )
        .unwrap();
        std::fs::write(
            project.join("package.json"),
            r#"{ "scripts": { "probe": "node main.ts" }, "dependencies": { "conditional-pkg": "*" } }"#,
        )
        .unwrap();
        // A shebang shim, so unix-only: Windows resolves `node_modules/.bin`
        // through `.cmd`/`.ps1` shims instead, which is a different code path
        // with its own coverage. Only the tests that actually route through
        // `.bin` are gated on this; the rest of the fixture is portable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let bin = project.join("node_modules/.bin/probe");
            std::fs::write(
                &bin,
                "#!/usr/bin/env node\n(async () => { await import('../../main.ts'); })();\n",
            )
            .unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        Self {
            xdg_config: temp.path().join("config"),
            cache: temp.path().join("cache"),
            temp,
            project,
            node,
            #[cfg(unix)]
            nubx,
        }
    }

    fn command(&self) -> Command {
        self.command_for(nub_binary())
    }

    fn command_for(&self, binary: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.project)
            .env("XDG_CONFIG_HOME", &self.xdg_config)
            .env("XDG_CACHE_HOME", &self.cache);
        command
    }

    /// Every entrypoint that runs `main.ts` applies the whole config and routes
    /// the two runtime-option channels the same way.
    fn assert_probe(&self, args: &[&str]) {
        let label = format!("nub {}", args.join(" "));
        let mut command = self.command();
        command.args(args);
        let value = probe_json(&label, command);

        assert_config_applied(&label, &value);
        assert_eq!(value["preload"], "preloaded", "{label}: {value}");
        assert_eq!(value["stack"], 23, "{label}: {value}");

        // The two runtime option fields ride DIFFERENT channels, and the split is
        // the whole point: Node refuses most V8-only flags in `NODE_OPTIONS`
        // ("is not allowed in NODE_OPTIONS", exit 9) but accepts them on argv, so
        // `v8Flags` must arrive as argv and `nodeOptions` as `NODE_OPTIONS`.
        let exec_argv = value["execArgv"].to_string();
        assert!(
            exec_argv.contains("--max-old-space-size=256"),
            "{label}: v8Flags must reach the child on argv: {exec_argv}"
        );
        let options = value["nodeOptions"].as_str().unwrap_or_default();
        assert!(
            !options.contains("--max-old-space-size=256"),
            "{label}: v8Flags must not be routed through NODE_OPTIONS: {options}"
        );
        assert!(
            options.contains("--stack-trace-limit=23"),
            "{label}: nodeOptions must still travel through NODE_OPTIONS: {options}"
        );

        let snapshot: serde_json::Value = serde_json::from_str(
            value["runtimeSnapshot"]
                .as_str()
                .expect("runtime snapshot reaches the augmented child"),
        )
        .expect("runtime snapshot is JSON");
        for key in ["install", "dlx"] {
            assert!(
                snapshot.get(key).is_none(),
                "{label}: runtime transport must exclude {key}: {snapshot}"
            );
        }
    }
}

#[test]
fn runtime_snapshot_reaches_the_file_run_and_script_entrypoints() {
    let fixture = Fixture::new();
    fixture.assert_probe(&["main.ts"]);
    fixture.assert_probe(&["run", "probe"]);
}

/// The layout the feature exists for, and the one the `Fixture` above cannot cover
/// because it names its tsconfig explicitly in `nub.jsonc`: no nub config at all, a
/// leaf `tsconfig.json` found by walking up, and the `customConditions` declared in
/// the shared base it `extends`.
///
/// `--node` is the control, and it is what makes this a test rather than a
/// coincidence: compat mode contributes no config-derived flags, so the SAME fixture
/// must fall through to `default`. Without it a passing assertion could just mean the
/// condition matched for some reason of Node's own.
#[test]
fn tsconfig_custom_conditions_reach_node_through_the_extends_chain() {
    let fixture = ConditionFixture::new("repo-source", "./src.js");
    // Both targets are `.js` here: this test is about which branch of the `exports` map
    // Node picks, and pointing the condition at TypeScript would fold in a second
    // question (see the symlinked-workspace test below).
    std::fs::write(fixture.pkg.join("src.js"), "export default 'source';\n").unwrap();

    assert_eq!(
        fixture.run(&["main.mjs"]),
        "source",
        "the base config's customConditions must reach Node's resolver"
    );
    assert_eq!(
        fixture.run(&["--node", "main.mjs"]),
        "dist",
        "compat mode contributes no config-derived conditions"
    );
}

/// The motivating layout end to end: the condition points at TypeScript SOURCE in a
/// workspace package, so nothing is built before running.
///
/// The symlink is load-bearing, not incidental scaffolding. Node resolves a module to
/// its REALPATH, and a workspace package is linked into `node_modules` rather than
/// copied — so the `.ts` file nub transpiles lives at its real location outside
/// `node_modules`. A package that is a genuine directory under `node_modules` instead
/// dies in Node's own type stripping with `ERR_UNSUPPORTED_NODE_MODULES_TYPE_STRIPPING`
/// (observed while writing this test), which is nub's existing transpile scope and not
/// something conditions change.
///
/// Unix-only for the symlink: Windows needs a privilege or developer mode to create
/// one, and the condition plumbing itself is covered on both platforms above.
#[cfg(unix)]
#[test]
fn a_custom_condition_can_point_at_typescript_source_in_a_linked_workspace_package() {
    let fixture = ConditionFixture::new("repo-source", "./src.ts");
    let real = fixture.temp.path().join("packages/live-pkg");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::rename(&fixture.pkg, &real).unwrap();
    std::os::unix::fs::symlink(&real, &fixture.pkg).unwrap();
    std::fs::write(
        real.join("src.ts"),
        "const which: string = 'source';\nexport default which;\n",
    )
    .unwrap();

    assert_eq!(
        fixture.run(&["main.mjs"]),
        "source",
        "nub must transpile the TypeScript the condition selected"
    );
}

/// A project whose `customConditions` live in a base config it `extends`, plus one
/// dependency whose `exports` map has that condition and a `default`.
struct ConditionFixture {
    temp: tempfile::TempDir,
    project: PathBuf,
    pkg: PathBuf,
}

impl ConditionFixture {
    /// `target` is what the condition resolves to; `default` is always `./dist.js`, and
    /// the caller writes whatever `target` names.
    fn new(condition: &str, target: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let pkg = project.join("node_modules/live-pkg");
        std::fs::create_dir_all(&pkg).unwrap();

        std::fs::write(
            project.join("tsconfig.base.json"),
            format!(r#"{{ "compilerOptions": {{ "customConditions": ["{condition}"] }} }}"#),
        )
        .unwrap();
        std::fs::write(
            project.join("tsconfig.json"),
            r#"{ "extends": "./tsconfig.base.json" }"#,
        )
        .unwrap();
        std::fs::write(
            pkg.join("package.json"),
            format!(
                r#"{{ "type": "module", "exports": {{ ".": {{ "{condition}": "{target}", "default": "./dist.js" }} }} }}"#
            ),
        )
        .unwrap();
        std::fs::write(pkg.join("dist.js"), "export default 'dist';\n").unwrap();
        std::fs::write(
            project.join("package.json"),
            r#"{ "dependencies": { "live-pkg": "*" } }"#,
        )
        .unwrap();
        // A `.mjs` entry, not `.ts`: the `--node` control has to run under vanilla Node
        // on every version in the support band, and a `.ts` entry would instead be
        // measuring whether that Node happens to strip types natively.
        std::fs::write(
            project.join("main.mjs"),
            "import which from 'live-pkg';\nconsole.log(which);\n",
        )
        .unwrap();

        Self { temp, project, pkg }
    }

    fn run(&self, args: &[&str]) -> String {
        let mut command = Command::new(nub_binary());
        command
            .current_dir(&self.project)
            .env("XDG_CONFIG_HOME", self.temp.path().join("config"))
            .env("XDG_CACHE_HOME", self.temp.path().join("cache"))
            .args(args);
        let output = probe_output(&format!("nub {}", args.join(" ")), command);
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
}

#[test]
fn top_level_typescript_options_override_the_selected_tsconfig() {
    let fixture = Fixture::new();
    // Keep the entry files in ESM format so this config-surface test does not
    // conflate decorator behavior with the documented compat-tier limitation
    // for CommonJS entries that need external transform helpers.
    std::fs::write(
        fixture.project.join("package.json"),
        r#"{ "type": "module" }"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("tsconfig.runtime.jsonc"),
        r#"{ "compilerOptions": {
          "jsx": "react-jsx",
          "jsxImportSource": "./missing-runtime",
          "experimentalDecorators": false,
          "emitDecoratorMetadata": false
        } }"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("classic.tsx"),
        r#"function make(tag: unknown, props: unknown, ...children: unknown[]) {
  return { tag, props, children };
}
const Fragment = "fragment";
console.log(JSON.stringify(<><widget /></>));
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("nub.jsonc"),
        r#"{
          "tsconfig": "./tsconfig.runtime.jsonc",
          "jsx": "react",
          "jsxFactory": "make",
          "jsxFragmentFactory": "Fragment"
        }"#,
    )
    .unwrap();

    let mut classic = fixture.command();
    classic.arg("classic.tsx");
    let classic = probe_json("top-level classic JSX options", classic);
    assert_eq!(classic["tag"], "fragment", "{classic}");
    assert_eq!(classic["children"][0]["tag"], "widget", "{classic}");

    std::fs::create_dir_all(fixture.project.join("runtime")).unwrap();
    std::fs::write(
        fixture.project.join("runtime/jsx-dev-runtime.js"),
        r#"export const Fragment = "fragment";
export function jsxDEV(tag, props) { return { mode: "development", tag, props }; }
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("automatic.tsx"),
        r#"console.log(JSON.stringify({
  element: <widget answer={42} />,
  snapshot: JSON.parse(process.env.__NUB_RUNTIME_CONFIG ?? "{}"),
}));
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("nub.jsonc"),
        r#"{
          "tsconfig": "./tsconfig.runtime.jsonc",
          "jsx": "react-jsxdev",
          "jsxImportSource": "./runtime",
          "decorators": "legacy",
          "emitDecoratorMetadata": true
        }"#,
    )
    .unwrap();

    let mut automatic = fixture.command();
    automatic.arg("automatic.tsx");
    let automatic = probe_json("top-level automatic JSX and decorator options", automatic);
    assert_eq!(automatic["element"]["mode"], "development", "{automatic}");
    assert_eq!(automatic["element"]["tag"], "widget", "{automatic}");
    assert_eq!(automatic["snapshot"]["jsx"], "react-jsxdev", "{automatic}");
    assert_eq!(
        automatic["snapshot"]["jsxImportSource"], "./runtime",
        "{automatic}"
    );
    assert_eq!(
        automatic["snapshot"]["experimentalDecorators"], true,
        "{automatic}"
    );
    assert_eq!(
        automatic["snapshot"]["emitDecoratorMetadata"], true,
        "{automatic}"
    );

    std::fs::write(
        fixture.project.join("decorated.ts"),
        r#"const parameterTypes: string[][] = [];
Reflect.metadata = (key: string, value: unknown[]) => () => {
  if (key === "design:paramtypes") parameterTypes.push(value.map(type => type.name));
};
function marked<T>(value: T): T { return value; }
@marked class Dependency {}
@marked class Service { constructor(_dependency: Dependency) {} }
console.log(JSON.stringify(parameterTypes));
"#,
    )
    .unwrap();
    let mut typescript = fixture.command();
    typescript.arg("decorated.ts");
    let typescript = probe_json("top-level legacy decorators and metadata", typescript);
    assert_eq!(typescript, serde_json::json!([["Dependency"]]));

    std::fs::write(
        fixture.project.join("decorated.js"),
        r#"function marked(_target, _key, descriptor) { return descriptor; }
class Example { @marked greet() { return "hello"; } }
console.log(new Example().greet());
"#,
    )
    .unwrap();
    let mut javascript = fixture.command();
    javascript.arg("decorated.js");
    let output = probe_output("top-level legacy decorators in JavaScript", javascript);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
}

/// The `node_modules/.bin` routes, split out because they ride a
/// `#!/usr/bin/env node` shim. Windows resolves local bins through `.cmd`
/// shims instead — a different path this fixture does not build — so gating
/// these keeps the entrypoints above running on both platforms rather than
/// losing the whole file to one construct.
#[cfg(unix)]
#[test]
fn runtime_snapshot_reaches_the_local_bin_routes() {
    let fixture = Fixture::new();
    fixture.assert_probe(&["exec", "probe"]);

    let mut nubx_run = fixture.command_for(&fixture.nubx);
    nubx_run.arg("probe");
    assert_config_applied("nubx probe", &probe_json("nubx probe", nubx_run));
}

/// `nubx --node <tool>` on a bin NOT present locally: the registry/`-p` fetch
/// fallback rather than the `node_modules/.bin` route above. A `file:` package
/// outside the project forces that path with no network involved. The fetched
/// bin's `#!/usr/bin/env node` shebang re-enters nub as a PATH shim rather than
/// reading `--node` from argv, so augmentation is disabled by `dlx_child_env`
/// stamping `NODE_COMPAT` for the child — a genuinely different mechanism.
#[cfg(unix)]
#[test]
fn nubx_node_flag_reaches_the_dlx_fallback_fetch_path() {
    let fixture = Fixture::new();
    let pkg_dir = fixture.temp.path().join("dlx-fallback-pkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(
        pkg_dir.join("package.json"),
        r#"{ "name": "rtc-dlx-probe", "version": "1.0.0", "bin": { "rtc-dlx-probe": "./bin.js" } }"#,
    )
    .unwrap();
    std::fs::write(
        pkg_dir.join("bin.js"),
        "#!/usr/bin/env node\nconsole.log(JSON.stringify({ stack: Error.stackTraceLimit, snapshot: process.env.__NUB_RUNTIME_CONFIG || null }));\n",
    )
    .unwrap();

    let package_spec = format!("file:{}", pkg_dir.to_string_lossy().replace('\\', "/"));
    let run = |extra: &[&str]| {
        // Three-position rule: a flag BEFORE the bin positional is nubx's own;
        // after it, the flag would forward to the bin verbatim.
        let mut args: Vec<&str> = extra.to_vec();
        args.extend(["-y", "-p", package_spec.as_str(), "rtc-dlx-probe"]);
        let mut command = fixture.command_for(&fixture.nubx);
        command.args(args);
        probe_json_after_progress(&format!("nubx {extra:?} dlx-fallback"), command)
    };

    let augmented = run(&[]);
    assert_eq!(
        augmented["stack"], 23,
        "fetched bin should be augmented by default: {augmented}"
    );
    assert!(
        !augmented["snapshot"].is_null(),
        "fetched bin should see the runtime snapshot by default: {augmented}"
    );

    let compat = run(&["--node"]);
    assert_eq!(
        compat["stack"], 10,
        "--node must reach the fetched bin's re-entrant node shim: {compat}"
    );
    assert!(
        compat["snapshot"].is_null(),
        "--node must suppress the runtime snapshot on the dlx-fallback path: {compat}"
    );
}

/// `nubx`, `nub dlx`, and `nub x` are the same command, so `--node` must work on
/// all three, in either flag order, and must not be stolen from the fetched tool
/// when it appears after the tool name. These spellings reach the engine verb
/// rather than `nubx`'s clap surface, and dlx's positional is `trailing_var_arg` +
/// `allow_hyphen_values`, which used to make an unrecognized `--node` the package
/// name instead of a usage error.
#[test]
fn node_flag_works_on_the_dlx_and_x_spellings_in_either_order() {
    let fixture = Fixture::new();
    let pkg_dir = fixture.temp.path().join("dlx-verb-pkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(
        pkg_dir.join("package.json"),
        r#"{ "name": "rtc-dlx-verb-probe", "version": "1.0.0", "bin": { "rtc-dlx-verb-probe": "./bin.js" } }"#,
    )
    .unwrap();
    std::fs::write(
        pkg_dir.join("bin.js"),
        "#!/usr/bin/env node\nconsole.log(JSON.stringify({ stack: Error.stackTraceLimit, snapshot: process.env.__NUB_RUNTIME_CONFIG || null, argv: process.argv.slice(2) }));\n",
    )
    .unwrap();

    let package_spec = format!("file:{}", pkg_dir.to_string_lossy().replace('\\', "/"));
    // `[pre] <verb> [post] -p <spec> rtc-dlx-verb-probe [tail]` — `pre` covers
    // `nub --node dlx`, `post` covers `nub dlx --node`, and `tail` is the fetched
    // tool's own argv.
    let run = |pre: &[&str], verb: &str, post: &[&str], tail: &[&str]| {
        let mut args: Vec<&str> = pre.to_vec();
        args.push(verb);
        args.extend(post);
        args.extend(["-p", package_spec.as_str(), "rtc-dlx-verb-probe"]);
        args.extend(tail);
        let mut command = fixture.command();
        command.args(&args);
        probe_json_after_progress(&format!("nub {args:?}"), command)
    };

    let augmented = run(&[], "dlx", &[], &[]);
    assert_eq!(
        augmented["stack"], 23,
        "a dlx-fetched bin is augmented by default: {augmented}"
    );
    assert!(
        !augmented["snapshot"].is_null(),
        "a dlx-fetched bin sees the runtime snapshot by default: {augmented}"
    );

    for (verb, pre, post) in [
        ("dlx", &["--node"][..], &[][..]),
        ("dlx", &[][..], &["--node"][..]),
        ("x", &["--node"][..], &[][..]),
        ("x", &[][..], &["--node"][..]),
    ] {
        let compat = run(pre, verb, post, &[]);
        assert_eq!(
            compat["stack"], 10,
            "nub {pre:?} {verb} {post:?} must reach the fetched bin's node shim: {compat}"
        );
        assert!(
            compat["snapshot"].is_null(),
            "nub {pre:?} {verb} {post:?} must suppress the runtime snapshot: {compat}"
        );
    }

    // Three-position rule: past the tool name the flag is the tool's.
    let forwarded = run(&[], "dlx", &[], &["--node"]);
    assert_eq!(
        forwarded["argv"],
        serde_json::json!(["--node"]),
        "a post-tool --node forwards verbatim: {forwarded}"
    );
    assert_eq!(
        forwarded["stack"], 23,
        "a post-tool --node must not disable augmentation: {forwarded}"
    );
}

#[test]
fn inherited_runtime_snapshot_tolerates_a_field_this_binary_does_not_know() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project.join("inherited.js"),
        "console.log('inherited')\n",
    )
    .unwrap();
    // The wire shape a parent nub writes, plus one field only a NEWER nub would
    // emit. A malformed snapshot is a hard error, so version skew across a
    // nested launch must not be able to abort the child.
    let snapshot = r#"{
      "nodeCompat": false,
      "preload": [],
      "nodeOptions": [],
      "v8Flags": [],
      "envFile": { "kind": "default" },
      "loader": {},
      "conditions": [],
      "tsconfig": null,
      "fieldFromANewerNub": { "unrecognized": true }
    }"#;
    let output = fixture
        .command_for(&fixture.node)
        .env("__NUB_RUNTIME_CONFIG", snapshot)
        .arg("inherited.js")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "an unrecognized snapshot field must deserialize inertly: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "inherited");
}

#[test]
fn fresh_nested_nub_rediscovers_config_and_preserves_replaced_node_options() {
    let fixture = Fixture::new();
    let nested = fixture.temp.path().join("nested-project");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("nub.jsonc"),
        r#"{ "nodeOptions": ["--stack-trace-limit=41"] }"#,
    )
    .unwrap();
    std::fs::write(
        nested.join("child.ts"),
        r#"console.log(JSON.stringify({
  stack: Error.stackTraceLimit,
  nodeOptions: process.env.NODE_OPTIONS,
  snapshotOptions: JSON.parse(process.env.__NUB_RUNTIME_CONFIG).nodeOptions,
  shimEntries: (process.env.PATH ?? '')
    .split(require('node:path').delimiter)
    .filter((entry) => entry.includes('nub-node-shim-')),
}));"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("nested-launcher.cjs"),
        r#"const { spawnSync } = require('node:child_process');
const env = { ...process.env };
if (process.env.REPLACE_NODE_OPTIONS === '1') env.NODE_OPTIONS = '--trace-warnings';
const child = spawnSync(process.env.NESTED_NUB, ['child.ts'], {
  cwd: process.env.NESTED_PROJECT,
  encoding: 'utf8',
  env,
});
process.stdout.write(child.stdout);
process.stderr.write(child.stderr);
process.exit(child.status ?? 1);
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("package.json"),
        r#"{ "scripts": { "nested-fresh": "node nested-launcher.cjs" } }"#,
    )
    .unwrap();

    let ambient_shims = ambient_shim_entries();
    let run = |replace_node_options: bool| {
        let mut command = fixture.command();
        command
            .env("NODE_OPTIONS", "--no-warnings")
            .env("NESTED_NUB", nub_binary())
            .env("NESTED_PROJECT", &nested)
            .args(["run", "nested-fresh"]);
        if replace_node_options {
            command.env("REPLACE_NODE_OPTIONS", "1");
        }
        probe_json(
            if replace_node_options {
                "nested fresh Nub invocation (NODE_OPTIONS replaced)"
            } else {
                "nested fresh Nub invocation (NODE_OPTIONS inherited)"
            },
            command,
        )
    };

    let inherited = run(false);
    let replaced = run(true);
    for value in [&inherited, &replaced] {
        assert_eq!(value["stack"], 41, "{value}");
        assert_eq!(
            value["snapshotOptions"],
            serde_json::json!(["--stack-trace-limit=41"]),
            "{value}"
        );
        let node_options = value["nodeOptions"].as_str().unwrap();
        assert!(
            node_options.contains("--stack-trace-limit=41"),
            "child config did not win: {node_options}"
        );
        assert!(
            !node_options.contains("--stack-trace-limit=23"),
            "parent config leaked into the fresh child: {node_options}"
        );
        let shim_entries = value["shimEntries"].as_array().unwrap();
        assert_eq!(
            shim_entries.len(),
            ambient_shims.len() + 1,
            "fresh child must replace the parent shim, not accumulate it: {value}"
        );
        let inherited_tail: Vec<&str> = shim_entries[1..]
            .iter()
            .map(|entry| entry.as_str().unwrap())
            .collect();
        let expected_tail: Vec<&str> = ambient_shims.iter().map(String::as_str).collect();
        assert_eq!(
            inherited_tail, expected_tail,
            "pre-existing unrelated shim entries must be preserved: {value}"
        );
    }

    let inherited_options = inherited["nodeOptions"].as_str().unwrap();
    assert!(
        inherited_options.contains("--no-warnings"),
        "the pre-augmentation ambient value was not restored: {inherited_options}"
    );

    let replaced_options = replaced["nodeOptions"].as_str().unwrap();
    assert!(
        replaced_options.contains("--trace-warnings"),
        "a user-replaced NODE_OPTIONS must survive the fresh boundary: {replaced_options}"
    );
    assert!(
        !replaced_options.contains("--no-warnings"),
        "the captured ambient value overwrote the user's replacement: {replaced_options}"
    );
}

#[cfg(unix)]
#[test]
fn fresh_nested_nub_and_nubx_keep_the_parent_project_bin_path() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = Fixture::new();
    let nested = fixture.temp.path().join("nested-bin-path");
    std::fs::create_dir_all(&nested).unwrap();
    let outer_bin = fixture.project.join("node_modules/.bin");
    let probe = r#"
const path = require('node:path');
const { realpathSync } = require('node:fs');
const entries = (process.env.PATH ?? '').split(path.delimiter);
const parentBin = realpathSync.native(process.env.PARENT_BIN);
console.log(JSON.stringify({
  hasParentBin: entries.some((entry) => {
    try {
      return realpathSync.native(entry) === parentBin;
    } catch {
      return false;
    }
  }),
}));
"#;
    std::fs::write(nested.join("file-probe.cjs"), probe).unwrap();
    let nubx_probe = outer_bin.join("nested-bin-probe");
    std::fs::write(&nubx_probe, format!("#!/usr/bin/env node\n{probe}")).unwrap();
    std::fs::set_permissions(&nubx_probe, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        fixture.project.join("nested-bin-launcher.cjs"),
        r#"const { spawnSync } = require('node:child_process');
const run = (program, args, cwd) => {
  const child = spawnSync(program, args, { cwd, encoding: 'utf8', env: process.env });
  if (child.status !== 0) {
    process.stderr.write(child.stderr);
    process.exit(child.status ?? 1);
  }
  return JSON.parse(child.stdout);
};
const file = run(process.env.NESTED_NUB, ['file-probe.cjs'], process.env.NESTED_PROJECT);
const nubx = run(process.env.NESTED_NUBX, ['nested-bin-probe'], process.cwd());
console.log(JSON.stringify({ file, nubx }));
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("package.json"),
        r#"{ "scripts": { "nested-bin-path": "node nested-bin-launcher.cjs" } }"#,
    )
    .unwrap();

    let mut command = fixture.command();
    command
        .env("NESTED_NUB", nub_binary())
        .env("NESTED_NUBX", &fixture.nubx)
        .env("NESTED_PROJECT", &nested)
        .env("PARENT_BIN", &outer_bin)
        .args(["run", "nested-bin-path"]);
    let value = probe_json("nested bin-path probes", command);
    assert_eq!(value["file"]["hasParentBin"], true, "{value}");
    assert_eq!(value["nubx"]["hasParentBin"], true, "{value}");
}

#[test]
fn fresh_nested_nub_node_mode_keeps_descendant_node_vanilla() {
    let fixture = Fixture::new();
    let nested = fixture.temp.path().join("nested-compat-project");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("grandchild.cjs"),
        r#"console.log(JSON.stringify({
  nubVersion: process.versions.nub ?? null,
  runtimeSnapshot: process.env.__NUB_RUNTIME_CONFIG ?? null,
  nodeOptions: process.env.NODE_OPTIONS ?? null,
  nodePresent: Object.hasOwn(process.env, 'NODE'),
  node: process.env.NODE ?? null,
  nodePath: process.env.NODE_PATH ?? null,
  compileCache: process.env.NODE_COMPILE_CACHE ?? null,
  pathEntries: (process.env.PATH ?? '').split(require('node:path').delimiter),
  shimEntries: (process.env.PATH ?? '')
    .split(require('node:path').delimiter)
    .filter((entry) => entry.includes('nub-node-shim-')),
}));"#,
    )
    .unwrap();
    std::fs::write(
        nested.join("compat-parent.cjs"),
        r#"const { spawnSync } = require('node:child_process');
const child = spawnSync('node', ['grandchild.cjs'], {
  encoding: 'utf8',
  env: process.env,
});
process.stdout.write(child.stdout);
process.stderr.write(child.stderr);
process.exit(child.status ?? 1);
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("nested-compat-launcher.cjs"),
        r#"const { delimiter } = require('node:path');
const { spawnSync } = require('node:child_process');
const env = { ...process.env };
if (process.env.MUTATE_AFTER_AUGMENTATION === '1') {
  env.NODE_OPTIONS = `${env.NODE_OPTIONS ?? ''} --trace-deprecation`.trim();
  env.PATH = `${env.PATH ?? ''}${delimiter}${process.env.PATH_ADDITION}`;
  env.NODE = 'user-replaced-node';
  env.NODE_PATH = 'user-replaced-node-path';
  env.NODE_COMPILE_CACHE = process.env.REPLACEMENT_COMPILE_CACHE;
}
const child = spawnSync(process.env.NESTED_NUB, ['--node', 'compat-parent.cjs'], {
  cwd: process.env.NESTED_PROJECT,
  encoding: 'utf8',
  env,
});
process.stdout.write(child.stdout);
process.stderr.write(child.stderr);
process.exit(child.status ?? 1);
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("package.json"),
        r#"{ "scripts": { "nested-compat": "node nested-compat-launcher.cjs" } }"#,
    )
    .unwrap();

    let ambient_shims = ambient_shim_entries();
    let path_addition = fixture.temp.path().join("user-path-addition");
    let replacement_compile_cache = fixture.temp.path().join("user-compile-cache");
    std::fs::create_dir_all(&path_addition).unwrap();
    let run = |mutate_after_augmentation: bool| {
        let mut command = fixture.command();
        command
            .env("NODE_OPTIONS", "--trace-warnings")
            .env("NODE", "")
            .env("NESTED_NUB", nub_binary())
            .env("NESTED_PROJECT", &nested)
            .env("PATH_ADDITION", &path_addition)
            .env("REPLACEMENT_COMPILE_CACHE", &replacement_compile_cache)
            .args(["run", "nested-compat"]);
        if mutate_after_augmentation {
            command.env("MUTATE_AFTER_AUGMENTATION", "1");
        }
        #[cfg(windows)]
        {
            // Rust's Windows PATH parser unquotes entries. Keep a quoted entry with
            // a semicolon in the ambient PATH to prove the fresh boundary restores
            // the parent's raw PATH instead of reconstructing it lossily.
            let quoted = fixture.temp.path().join("quoted;path");
            std::fs::create_dir_all(&quoted).unwrap();
            let mut path = std::ffi::OsString::from(format!("\"{}\";", quoted.display()));
            path.push(std::env::var_os("PATH").unwrap_or_default());
            command.env("PATH", path);
        }
        probe_json(
            if mutate_after_augmentation {
                "fresh nested --node invocation (env mutated after augmentation)"
            } else {
                "fresh nested --node invocation"
            },
            command,
        )
    };

    let unchanged = run(false);
    assert_eq!(
        unchanged["nubVersion"],
        serde_json::Value::Null,
        "{unchanged}"
    );
    assert_eq!(
        unchanged["runtimeSnapshot"],
        serde_json::Value::Null,
        "{unchanged}"
    );
    assert_eq!(unchanged["nodeOptions"], "--trace-warnings", "{unchanged}");
    assert_eq!(unchanged["nodePresent"], true, "{unchanged}");
    assert_eq!(
        unchanged["node"], "",
        "an explicitly empty ambient variable must not collapse to absent: {unchanged}"
    );
    assert_eq!(
        unchanged["shimEntries"],
        serde_json::json!(ambient_shims),
        "the outer shim must not survive the fresh compat boundary: {unchanged}"
    );

    let mutated = run(true);
    assert_eq!(mutated["nubVersion"], serde_json::Value::Null, "{mutated}");
    assert_eq!(
        mutated["runtimeSnapshot"],
        serde_json::Value::Null,
        "{mutated}"
    );
    assert_eq!(
        mutated["nodeOptions"], "--trace-warnings --trace-deprecation",
        "an appended user option must survive while Nub's parent-owned options are removed: {mutated}"
    );
    assert_eq!(mutated["node"], "user-replaced-node", "{mutated}");
    assert_eq!(mutated["nodePath"], "user-replaced-node-path", "{mutated}");
    assert_eq!(
        mutated["compileCache"]
            .as_str()
            .map(|path| path.replace('\\', "/")),
        Some(
            replacement_compile_cache
                .to_string_lossy()
                .replace('\\', "/")
        ),
        "{mutated}"
    );
    let expected_path_addition = path_addition.canonicalize().unwrap();
    assert!(
        mutated["pathEntries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter_map(|entry| PathBuf::from(entry).canonicalize().ok())
            .any(|entry| entry == expected_path_addition),
        "a PATH addition made after augmentation must survive: {mutated}"
    );
    assert_eq!(
        mutated["shimEntries"],
        serde_json::json!(ambient_shims),
        "the old shim must still be removed from a user-modified PATH: {mutated}"
    );
}

#[test]
fn runtime_snapshot_reaches_watch_child() {
    let fixture = Fixture::new();
    let mut command = fixture.command();
    command.args(["watch", "main.ts"]);
    let line = watch_first_line(command);
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_config_applied("nub watch main.ts", &value);
}

#[test]
fn node_compat_config_is_zero_augmentation_and_environment_false_overrides_it() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project.join("compat.js"),
        "console.log(globalThis.__runtimePreload ?? 'vanilla');\n",
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("nub.jsonc"),
        r#"{ "nodeCompat": true, "preload": ["./preload.mjs"] }"#,
    )
    .unwrap();

    let vanilla = fixture.command().arg("compat.js").output().unwrap();
    assert!(
        vanilla.status.success(),
        "{}",
        String::from_utf8_lossy(&vanilla.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&vanilla.stdout).trim(), "vanilla");

    let augmented = fixture
        .command()
        .env("NODE_COMPAT", "false")
        .arg("compat.js")
        .output()
        .unwrap();
    assert!(
        augmented.status.success(),
        "{}",
        String::from_utf8_lossy(&augmented.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&augmented.stdout).trim(),
        "preloaded"
    );
}

#[test]
fn inherited_runtime_snapshot_yields_to_nested_node_compat_with_zero_augmentation() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project.join("nested-compat.js"),
        r#"console.log(JSON.stringify({
  preload: globalThis.__runtimePreload ?? null,
  execArgv: process.execArgv,
  nodeOptions: process.env.NODE_OPTIONS ?? null,
  nodePath: process.env.NODE_PATH ?? null,
  node: process.env.NODE ?? null,
  compileCache: process.env.NODE_COMPILE_CACHE ?? null,
  runtimeConfig: process.env.__NUB_RUNTIME_CONFIG ?? null,
  nubVersion: process.versions.nub ?? null,
  shimEntries: (process.env.PATH ?? '')
    .split(require('node:path').delimiter)
    .filter((entry) => entry.includes('nub-node-shim-')),
}));
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("package.json"),
        // Exercise the nested child-environment boundary, not only the script
        // shell's POSIX-valid `NODE_COMPAT=1 node …` prefix (Windows uses sh too).
        serde_json::to_vec(&serde_json::json!({ "scripts": {
            "nested-env": "node nested-env-launcher.cjs",
            "nested-cli": "node --node nested-compat.js",
        }}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("nested-env-launcher.cjs"),
        r#"const { spawnSync } = require('node:child_process');
const child = spawnSync('node', ['nested-compat.js'], {
  stdio: ['ignore', 'inherit', 'inherit'],
  env: { ...process.env, NODE_COMPAT: '1' },
});
if (child.error) throw child.error;
process.exit(child.status ?? 1);
"#,
    )
    .unwrap();

    let user_compile_cache = fixture.project.join("user-compile-cache");
    // The nested Node process may surface an equivalent Windows path with forward
    // slashes; this assertion is about preserving the path, not its separator
    // spelling.
    let user_compile_cache_text = user_compile_cache.to_string_lossy().replace('\\', "/");
    let ambient_shims = ambient_shim_entries();
    for script in ["nested-env", "nested-cli"] {
        let mut command = fixture.command();
        command
            .env("NODE_OPTIONS", "--trace-warnings")
            .env("NODE_PATH", "/user/node-path")
            .env("NODE", "/user/node")
            .env("NODE_COMPILE_CACHE", &user_compile_cache)
            .args(["run", script]);
        let value = probe_json(script, command);

        assert_eq!(
            value["preload"],
            serde_json::Value::Null,
            "{script}: {value}"
        );
        assert_eq!(value["execArgv"], serde_json::json!([]), "{script}");
        assert_eq!(value["nodeOptions"], "--trace-warnings", "{script}");
        assert_eq!(value["nodePath"], "/user/node-path", "{script}");
        assert_eq!(value["node"], "/user/node", "{script}");
        let compile_cache = value["compileCache"]
            .as_str()
            .map(|path| path.replace('\\', "/"));
        assert_eq!(
            compile_cache.as_deref(),
            Some(user_compile_cache_text.as_str()),
            "{script}"
        );
        assert_eq!(value["runtimeConfig"], serde_json::Value::Null, "{script}");
        assert_eq!(value["nubVersion"], serde_json::Value::Null, "{script}");
        assert_eq!(
            value["shimEntries"],
            serde_json::json!(ambient_shims),
            "{script}"
        );
    }
}

#[test]
fn watch_composes_explicit_config_env_sources_before_cli_env_file() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project.join("first.env"),
        "CONFIG_ONLY=first\nSHARED=first\n",
    )
    .unwrap();
    std::fs::write(fixture.project.join("second.env"), "SHARED=second\n").unwrap();
    std::fs::write(
        fixture.project.join("nub.jsonc"),
        r#"{ "envFile": ["./first.env", "./second.env"] }"#,
    )
    .unwrap();
    let cli_env = fixture.project.join("cli.env");
    std::fs::write(&cli_env, "SHARED=cli\nCLI_ONLY=cli\n").unwrap();
    std::fs::write(
        fixture.project.join("env-probe.js"),
        "console.log(JSON.stringify({ config: process.env.CONFIG_ONLY, shared: process.env.SHARED, cli: process.env.CLI_ONLY }));\n",
    )
    .unwrap();

    let mut command = fixture.command();
    command
        .arg(format!("--env-file={}", cli_env.display()))
        .args(["watch", "env-probe.js"]);
    let line = watch_first_line(command);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap(),
        serde_json::json!({ "config": "first", "shared": "cli", "cli": "cli" })
    );
}

#[test]
fn unsupported_runtime_option_fails_before_node_startup() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project.join("nub.jsonc"),
        r#"{ "nodeOptions": ["--definitely-not-a-node-option"] }"#,
    )
    .unwrap();
    let output = fixture.command().arg("main.ts").output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not accepted in NODE_OPTIONS"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The refusal above fires on two populations, and only one of them is a typo.
/// `--stack-size` is a REAL flag the installed Node supports — it is simply
/// command-line-only, and `v8Flags` is the field that delivers it. Naming Node's
/// support as the problem sent that author to check their Node version.
#[test]
fn a_command_line_only_v8_flag_is_refused_by_naming_the_field_that_takes_it() {
    let fixture = Fixture::new();
    // Its own entry, not the shared `main.ts`: that one imports `./message.blob`
    // and so only runs under the fixture's `loader` map, which the single-key
    // documents below deliberately replace.
    std::fs::write(fixture.project.join("v8probe.js"), "console.log('ran');\n").unwrap();

    std::fs::write(
        fixture.project.join("nub.jsonc"),
        r#"{ "nodeOptions": ["--stack-size=2000"] }"#,
    )
    .unwrap();
    let output = fixture.command().arg("v8probe.js").output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("not accepted in NODE_OPTIONS") && stderr.contains("`v8Flags`"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("not supported by Node"),
        "the installed Node does support this flag: {stderr}"
    );

    // Positive control: the same flag through `v8Flags` reaches Node on its
    // command line and runs, so the refusal above is about the DELIVERY channel
    // rather than the flag.
    std::fs::write(
        fixture.project.join("nub.jsonc"),
        r#"{ "v8Flags": ["--stack-size=2000"] }"#,
    )
    .unwrap();
    let output = fixture.command().arg("v8probe.js").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// npm/pnpm parity for the `node-options` npmrc field on `nub run`, plus the two
/// places it is easy to get wrong. npm and pnpm ASSIGN `NODE_OPTIONS` from this
/// field and destroy the ambient value; nub appends, so its own augmentation
/// preload has to survive alongside. And because `compute_augmentation_env`
/// re-quotes every option individually, a multi-flag value must arrive already
/// split — pushed whole it would emit the single broken token `"--a --b"`.
#[test]
fn npmrc_node_options_reach_nub_run_without_displacing_augmentation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"npmrc-node-options","version":"1.0.0","scripts":{"show":"node -e \"console.log(process.env.NODE_OPTIONS ?? '')\""}}"#,
    )
    .unwrap();
    std::fs::write(
        root.join(".npmrc"),
        "node-options=--max-old-space-size=8192 --trace-warnings\n",
    )
    .unwrap();

    let show = || -> String {
        let mut command = Command::new(nub_binary());
        command
            .current_dir(root)
            .args(["run", "show"])
            .stdin(Stdio::null());
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "nub run show exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        // `nub run` echoes the command line first; NODE_OPTIONS is the last line.
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .last()
            .unwrap_or_default()
            .to_string()
    };

    let options = show();
    assert!(
        options.contains("--max-old-space-size=8192"),
        "the npmrc `node-options` field must reach the script (pnpm applies it): {options}"
    );
    assert!(
        options.contains("--trace-warnings"),
        "a multi-flag value must split into separate tokens, not one quoted blob: {options}"
    );
    assert!(
        options.contains("runtime/preload"),
        "nub's own preload must survive alongside it — nub appends where npm assigns: {options}"
    );

    // nub.jsonc is nub's OWN surface and outranks the generic `.npmrc` one, which
    // under Node's last-wins rule means its token must come after npmrc's.
    std::fs::write(
        root.join("nub.jsonc"),
        "{ \"nodeOptions\": [\"--max-old-space-size=4096\"] }\n",
    )
    .unwrap();
    let options = show();
    // rfind, not find: a nested nub run can carry more than one augmentation block,
    // and the token Node actually applies is the LAST one of each.
    let from_npmrc = options
        .rfind("--max-old-space-size=8192")
        .unwrap_or_else(|| panic!("npmrc value missing once nub.jsonc is present: {options}"));
    let from_nub_jsonc = options
        .rfind("--max-old-space-size=4096")
        .unwrap_or_else(|| panic!("nub.jsonc value missing: {options}"));
    assert!(
        from_npmrc < from_nub_jsonc,
        "nub.jsonc `nodeOptions` must come last so it wins the conflict: {options}"
    );
}

/// The contract the synthesized preload chainer exists to hold: however many
/// `nub.jsonc` `preload` entries a project declares, nub emits AT MOST ONE
/// `--require` and AT MOST ONE `--import` into NODE_OPTIONS.
///
/// One token per entry is destroyed by any consumer that re-parses NODE_OPTIONS —
/// Next.js keys it by option name and reformats it for every forked worker, so a
/// repeated flag collapses to its last value and silently drops nub's OWN preload
/// (vercel/next.js#96582). The `.cjs`-only case is the sharp one: nub's preload is
/// also a `--require`, so before the chainer it was the entry that got dropped.
///
/// Bare specifiers are the reason the chainer is written INSIDE the project: they
/// resolve through that project's node_modules walk-up from the chainer's own
/// directory, which is what Node does for a `--require` token and what nub cannot
/// do itself (its resolver is additive-only and returns null for every bare name).
#[test]
fn preload_entries_collapse_to_one_token_per_flag_name() {
    for entries in [
        r#"["./one.cjs"]"#,
        r#"["./one.cjs", "./two.cjs"]"#,
        r#"["./a.mjs", "./b.mjs"]"#,
        r#"["./a.mjs", "./one.cjs", "./b.mjs"]"#,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"chain","version":"1.0.0"}"#,
        )
        .unwrap();
        for name in ["one.cjs", "two.cjs"] {
            std::fs::write(root.join(name), "").unwrap();
        }
        for name in ["a.mjs", "b.mjs"] {
            std::fs::write(root.join(name), "").unwrap();
        }
        std::fs::write(
            root.join("nub.jsonc"),
            format!("{{ \"preload\": {entries} }}\n"),
        )
        .unwrap();

        let mut command = Command::new(nub_binary());
        command
            .current_dir(root)
            .args(["-e", "process.stdout.write(process.env.NODE_OPTIONS ?? '')"])
            .stdin(Stdio::null());
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "nub -e failed for {entries}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let options = String::from_utf8_lossy(&output.stdout);
        let requires = options.matches("--require=").count();
        let imports = options.matches("--import=").count();
        assert!(
            requires <= 1 && imports <= 1,
            "preload {entries} emitted {requires} --require and {imports} --import; \
             a repeated flag name is destroyed by NODE_OPTIONS re-parsers: {options}"
        );
        // Windows emits `runtime\preload.cjs`; compare on a normalized copy so the
        // assertion is about the TOKEN, not the platform's separator.
        let normalized = options.replace('\\', "/");
        assert!(
            normalized.contains("runtime/preload."),
            "nub's own preload token must always be present for {entries}: {options}"
        );
    }
}

/// A BARE `nub.jsonc` `preload` entry resolves from the CURRENT WORKING DIRECTORY,
/// the way Node resolves a bare `--require`/`--import` specifier.
///
/// The anchor is easy to get wrong and fails silently. nub loads preload entries
/// through a generated chainer module, and a bare `import "foo"` inside that file
/// would otherwise resolve from the FILE's directory — so in a workspace where a
/// member shadows a root dependency, running from the member would load the ROOT
/// copy while plain Node loads the member's. Same specifier, different module, no
/// error. nub resolves bare entries itself to keep the CWD anchor.
#[test]
fn a_bare_preload_entry_resolves_from_the_cwd_like_node_does() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"anchor","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::write(root.join("nub.jsonc"), "{ \"preload\": [\"shadowed\"] }\n").unwrap();

    // Two copies of the same dependency name: one at the root, one shadowing it in
    // a member directory.
    for (dir, marker) in [
        (root.to_path_buf(), "ROOT"),
        (root.join("member"), "MEMBER"),
    ] {
        let pkg = dir.join("node_modules").join("shadowed");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"shadowed","version":"1.0.0","main":"i.js"}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("i.js"), format!("console.log(\"{marker}\");")).unwrap();
    }
    let member = root.join("member");
    std::fs::write(member.join("app.js"), "console.log(\"entry\");").unwrap();

    let mut command = Command::new(nub_binary());
    command
        .current_dir(&member)
        .arg("app.js")
        .stdin(Stdio::null());
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "nub app.js failed from the member dir: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("MEMBER"),
        "a bare preload must resolve from the CWD (the member's own copy), \
         not from wherever nub wrote its chainer: {stdout}"
    );
    assert!(
        !stdout.contains("ROOT"),
        "the root copy must not shadow the member's: {stdout}"
    );
}

/// Preload flags arriving through an INHERITED `NODE_OPTIONS` are folded into nub's
/// chainers rather than forwarded as extra tokens.
///
/// Without the fold, an ambient `--require` (what every APM injector sets) lands as a
/// second token of a name nub already emits. A consumer that keys `NODE_OPTIONS` by
/// flag name then keeps one — and since nub appends the inherited value last, the one
/// it dropped was nub's own preload, taking the whole augmentation layer with it.
///
/// Both preloads must still RUN, and in the same order as before the fold.
#[test]
fn inherited_node_options_preloads_fold_into_the_chainer() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"fold","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::write(root.join("nub.jsonc"), "{ \"preload\": [\"./cfg.cjs\"] }\n").unwrap();
    let log = root.join("order.log");
    for (file, marker) in [("cfg.cjs", "CFG"), ("amb.cjs", "AMB")] {
        std::fs::write(
            root.join(file),
            format!(
                "require(\"node:fs\").appendFileSync(process.env.FOLD_LOG, {:?});",
                format!("{marker}\n")
            ),
        )
        .unwrap();
    }
    std::fs::write(
        root.join("app.js"),
        "require(\"node:fs\").appendFileSync(process.env.FOLD_LOG, \"ENTRY\\n\");\n\
         console.log(process.env.NODE_OPTIONS ?? \"\");",
    )
    .unwrap();

    let mut command = Command::new(nub_binary());
    command
        .current_dir(root)
        .arg("app.js")
        .env("FOLD_LOG", &log)
        .env(
            "NODE_OPTIONS",
            format!("--require {}", root.join("amb.cjs").display()),
        )
        .stdin(Stdio::null());
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "nub app.js failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let options = String::from_utf8_lossy(&output.stdout);
    let requires = options.matches("--require").count();
    let imports = options.matches("--import").count();
    assert!(
        requires <= 1 && imports <= 1,
        "an inherited preload must be folded, not forwarded as a second token; \
         got {requires} --require and {imports} --import: {options}"
    );
    let normalized = options.replace('\\', "/");
    assert!(
        normalized.contains("runtime/preload."),
        "nub's own preload token must survive the fold: {options}"
    );

    let order = std::fs::read_to_string(&log).unwrap_or_default();
    let order: Vec<&str> = order.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        order,
        vec!["CFG", "AMB", "ENTRY"],
        "both preloads must run, config before inherited, both before the entry"
    );
}

/// A `nodeExecutable` project fixture with an UNSATISFIABLE pin beside it, so no
/// assertion below can pass by accident: without the field, discovery has no
/// binary to reach and `nub node which` fails.
struct NodeExecutableFixture {
    temp: tempfile::TempDir,
    project: PathBuf,
    /// The `node` the field is pointed at — this box's PATH node, learned by
    /// asking nub for it before the pin exists.
    node: String,
}

impl NodeExecutableFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        // Every path assertion below compares against one nub PRINTED, and nub
        // anchors to `env::current_dir()`. macOS hands `tempfile` a
        // `/var/folders/...` that is a symlink to `/private/var/...`, so the raw
        // spelling never matches there — and no pull-request leg runs macOS.
        //
        // Unix only. On Windows `fs::canonicalize` returns a VERBATIM `\\?\C:\...`
        // path, a spelling nothing else in the process produces, so canonicalizing
        // there would trade the macOS mismatch for a Windows one. Windows needs no
        // fixup: it hands back the same path it was given.
        #[cfg(not(windows))]
        let project = std::fs::canonicalize(&project).unwrap();
        let fixture = Self {
            temp,
            project,
            node: String::new(),
        };
        let baseline = fixture.which();
        assert!(
            baseline.status.success(),
            "no node on PATH to point the field at: {}",
            String::from_utf8_lossy(&baseline.stderr)
        );
        let node = String::from_utf8_lossy(&baseline.stdout).trim().to_string();
        // A version nobody has published, so no PATH node, no store entry, and no
        // nvm install can quietly rescue the pin on a developer's own machine.
        std::fs::write(fixture.project.join(".node-version"), "999.0.0\n").unwrap();
        let control = fixture.which();
        assert!(
            !control.status.success(),
            "the control pin must be unreachable, else every assertion below is vacuous: {}",
            String::from_utf8_lossy(&control.stdout)
        );
        Self { node, ..fixture }
    }

    fn command_in(&self, cwd: &Path) -> Command {
        let mut command = Command::new(nub_binary());
        command
            .current_dir(cwd)
            .env("XDG_CONFIG_HOME", self.temp.path().join("config"))
            .env("XDG_CACHE_HOME", self.temp.path().join("cache"))
            .env_remove("NODE_EXECUTABLE");
        command
    }

    fn which(&self) -> std::process::Output {
        self.which_in(&self.project)
    }

    fn which_in(&self, cwd: &Path) -> std::process::Output {
        self.command_in(cwd)
            .args(["node", "which"])
            .output()
            .unwrap()
    }

    fn write_config(&self, spec: &str) {
        std::fs::write(
            self.project.join("nub.jsonc"),
            format!(r#"{{ "nodeExecutable": {} }}"#, serde_json::json!(spec)),
        )
        .unwrap();
    }

    /// A shell command printing `text` and exiting 0, spelled for the shell the
    /// running platform's substitution uses.
    fn print_command(text: &str) -> String {
        if cfg!(windows) {
            format!("$(echo {text})")
        } else {
            format!("$(printf %s '{text}')")
        }
    }
}

/// Both spellings of the field select the binary, and the resolution is reported
/// as the field rather than as the pin chain it bypassed.
#[test]
fn node_executable_config_selects_the_binary_in_both_forms() {
    let fixture = NodeExecutableFixture::new();

    for spec in [
        fixture.node.clone(),
        NodeExecutableFixture::print_command(&fixture.node),
    ] {
        fixture.write_config(&spec);
        let output = fixture.which();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{spec}: {stderr}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            fixture.node,
            "{spec} must resolve to the binary it names"
        );
        assert!(
            stderr.contains("nub.jsonc#nodeExecutable"),
            "{spec} must report itself as the source, not the pin it bypassed: {stderr}"
        );
    }

    // NODE_EXECUTABLE outranks the file: same binary, different reported source.
    let stderr = String::from_utf8_lossy(
        &fixture
            .command_in(&fixture.project)
            .args(["node", "which"])
            .env("NODE_EXECUTABLE", &fixture.node)
            .output()
            .unwrap()
            .stderr,
    )
    .into_owned();
    assert!(
        stderr.contains("resolved from NODE_EXECUTABLE"),
        "the environment must outrank the file: {stderr}"
    );
}

/// Naming a `node` that is really nub is refused, not resolved. Nub-as-node
/// re-enters discovery and reads this same setting, so resolving through it
/// recurses until something kills the process — a hang with no output, the one
/// failure no error text can be read out of. `$(which node)` is the spelling
/// that reaches it, because a user's `which` does not share the skip list nub
/// applies to its own PATH scan.
///
/// Laid out as `.nub/node-shim/node`, the SHAPE of the installed global shim:
/// the real directory moves with `XDG_DATA_HOME`, so it cannot be matched by
/// path alone.
#[test]
fn a_node_executable_that_is_really_nub_is_refused() {
    let fixture = NodeExecutableFixture::new();
    let shim_dir = fixture.project.join(".nub").join("node-shim");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let as_node = shim_dir.join(format!("node{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(&as_node, "").unwrap();
    fixture.write_config(&as_node.to_string_lossy());

    let output = fixture.which();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("ERR_NUB_NODE_EXECUTABLE_SELF"),
        "the refusal must name the self-reference: {stderr}"
    );
}

/// The PATH recipe survives nub's own `node` shim sitting first on PATH — the
/// layout `nub node shim` creates, and so the configuration of exactly the users
/// most likely to reach for `$(which node)`. The shim is taken off the PATH the
/// command runs with, so `which` answers with the first REAL Node instead of nub:
/// neither the recursion nor the refusal is the right answer to a question the
/// user asked in good faith.
///
/// POSIX-only because the PATH and shell spelling are written in `sh`; the guard
/// itself is platform-independent.
#[cfg(unix)]
#[test]
fn the_path_recipe_looks_past_nubs_own_node_shim() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = NodeExecutableFixture::new();
    // A shim in the shape nub installs, and one that would fail loudly if it were
    // ever chosen — so a regression cannot pass by quietly running the wrong file.
    let shim_dir = fixture.project.join(".nub").join("node-shim");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let shim_node = shim_dir.join("node");
    std::fs::write(&shim_node, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&shim_node, std::fs::Permissions::from_mode(0o755)).unwrap();

    let real_dir = fixture.project.join("realbin");
    std::fs::create_dir_all(&real_dir).unwrap();
    let real_node = real_dir.join("node");
    std::os::unix::fs::symlink(&fixture.node, &real_node).unwrap();

    fixture.write_config("$(which node)");
    // The shim FIRST, the real Node second, then the system dirs `sh` and `which`
    // themselves live in.
    let output = fixture
        .command_in(&fixture.project)
        .env(
            "PATH",
            format!(
                "{}:{}:/usr/bin:/bin",
                shim_dir.display(),
                real_dir.display()
            ),
        )
        .args(["node", "which"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        real_node.to_string_lossy(),
        "the shim must be skipped and the real Node named: {stderr}"
    );
}

/// A relative path is anchored to the file that declared it, not to wherever the
/// user happened to stand — one committed value has to mean one binary.
///
/// Deliberately spelled `./tools/node` with no extension, which is the spelling
/// a mixed-platform team can commit: on Windows that must still resolve to the
/// real `node.exe`, separators and all, or the reported path and the version
/// cache disagree with the binary that actually runs.
#[test]
fn a_relative_node_executable_anchors_to_its_config_file() {
    let fixture = NodeExecutableFixture::new();
    let tools = fixture.project.join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let linked = tools.join(format!("node{}", std::env::consts::EXE_SUFFIX));
    #[cfg(unix)]
    std::os::unix::fs::symlink(&fixture.node, &linked).unwrap();
    #[cfg(windows)]
    if std::fs::hard_link(&fixture.node, &linked).is_err() {
        std::fs::copy(&fixture.node, &linked).unwrap();
    }
    fixture.write_config("./tools/node");

    // One component per `join`: `join("src/deep")` would embed a literal `/` in
    // an otherwise `\`-separated Windows path, and the expected string below has
    // to match the canonical cwd the binary reports, not this file's spelling.
    let nested = fixture.project.join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();
    let output = fixture.which_in(&nested);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        linked.to_string_lossy(),
        "a relative spec must resolve against the nub.jsonc, not the cwd: {stderr}"
    );

    // A `$(command)` takes the OTHER anchor — the cwd — because a toolchain
    // manager answers per directory, and the answer wanted is the one for where
    // the user is. Discriminating: the same relative token names a binary that
    // exists only in the nested directory.
    let nested_node = nested.join(format!("node{}", std::env::consts::EXE_SUFFIX));
    #[cfg(unix)]
    std::os::unix::fs::symlink(&fixture.node, &nested_node).unwrap();
    #[cfg(windows)]
    if std::fs::hard_link(&fixture.node, &nested_node).is_err() {
        std::fs::copy(&fixture.node, &nested_node).unwrap();
    }
    fixture.write_config(&NodeExecutableFixture::print_command(&format!(
        "./node{}",
        std::env::consts::EXE_SUFFIX
    )));
    let output = fixture.which_in(&nested);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        nested_node.to_string_lossy(),
        "a command's output must anchor where the command ran: {stderr}"
    );
}

/// A `$(command)` that fails stops the run and hands back the tool's own error.
/// Falling through to the pin chain would be the silent substitution the field
/// exists to prevent — here that fallback is an unsatisfiable pin, so a regression
/// shows up as the wrong error rather than as a pass.
///
/// The RUN is the contract's own subject, so it is the first thing asserted; a
/// `which` that refuses is the report of that same resolution, not a substitute
/// for it. The last leg pins the boundary: `nub node ls` manages nub's own store
/// and runs no user code, so a toolchain that cannot answer leaves it reporting
/// the store rather than refusing. That asymmetry is deliberate — `manage.rs`'s
/// `resolved_version` carries the reasoning — and reads as an oversight without
/// a test that fails when someone "fixes" it.
#[test]
fn a_failing_node_executable_command_stops_the_run() {
    let fixture = NodeExecutableFixture::new();
    let spec = if cfg!(windows) {
        "$(echo mise: command not found 1>&2& exit /b 127)"
    } else {
        "$(echo 'mise: command not found' >&2; exit 127)"
    };
    fixture.write_config(spec);

    let refuses = |output: std::process::Output, what: &str| {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        assert!(!output.status.success(), "{what} must fail: {stderr}");
        assert!(
            stderr.contains("ERR_NUB_NODE_EXECUTABLE_FAILED")
                && stderr.contains("mise: command not found"),
            "{what} must carry the tool's own error: {stderr}"
        );
    };

    std::fs::write(fixture.project.join("app.js"), "console.log('ran')\n").unwrap();
    refuses(
        fixture
            .command_in(&fixture.project)
            .arg("app.js")
            .output()
            .unwrap(),
        "a file run",
    );
    refuses(fixture.which(), "nub node which");

    let ls = fixture
        .command_in(&fixture.project)
        .args(["node", "ls"])
        .output()
        .unwrap();
    assert!(
        ls.status.success(),
        "listing nub's own cache must not depend on the project's toolchain: {}",
        String::from_utf8_lossy(&ls.stderr)
    );
}

/// The override is the one winner that can contradict every declared pin at once,
/// so it is warned about against the whole chain — not only `engines.node`.
#[test]
fn an_overriding_binary_is_warned_about_against_the_pin_it_contradicts() {
    let fixture = NodeExecutableFixture::new();
    fixture.write_config(&fixture.node);
    // The control pin (999.0.0) is what the resolved binary contradicts.
    std::fs::write(fixture.project.join("main.js"), "console.log('ran');\n").unwrap();

    let output = fixture
        .command_in(&fixture.project)
        .arg("main.js")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the warning is non-fatal: {stderr}"
    );
    assert!(
        stderr.contains("nub.jsonc#nodeExecutable") && stderr.contains(".node-version"),
        "the warning must name both the winner and the pin it bypassed: {stderr}"
    );

    // Agreeing sources are silent, which is what proves the warning is about the
    // disagreement rather than about the override existing.
    std::fs::remove_file(fixture.project.join(".node-version")).unwrap();
    let output = fixture
        .command_in(&fixture.project)
        .arg("main.js")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("nub.jsonc#nodeExecutable"),
        "no declared pin, nothing to disagree with: {stderr}"
    );
}
