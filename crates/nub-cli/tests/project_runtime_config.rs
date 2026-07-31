//! End-to-end coverage for project `nub.jsonc` runtime consumers.
//!
//! Runs on Windows too, deliberately. This file was `#![cfg(unix)]` when it
//! landed, which meant the Windows leg verified NONE of the project runtime
//! plumbing — and Windows is exactly where it diverges: `NODE_OPTIONS` is a
//! single tokenized string whose quoting rules differ, and every path in the
//! snapshot is anchored differently. A `nodeOptions` quoting bug that survived
//! four separate producers on this branch is the shape of defect that gap hides.
//!
//! Two constructs genuinely need unix and are guarded per-test rather than
//! per-file: a `#!/usr/bin/env node` shim in `node_modules/.bin` (Windows uses
//! `.cmd` shims, a different resolution path with its own coverage), and the
//! mode bit that makes it executable. Everything else — the config parse, the
//! snapshot, `nodeOptions`/`v8Flags`/`envFile`/`loader`/`conditions`/`tsconfig`
//! — is platform-independent by construction and now runs on both.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
    path.push(format!("nub{}", std::env::consts::EXE_SUFFIX));
    path
}

struct Fixture {
    _temp: tempfile::TempDir,
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
        let xdg_config = temp.path().join("config");
        let config_root = project.clone();
        let cache = temp.path().join("cache");
        let node = temp
            .path()
            .join(format!("node{}", std::env::consts::EXE_SUFFIX));
        // Nub dispatches on argv0, so this alias reaches its `node` shim; the
        // same alias mechanism supplies the unix-only `nubx` route below.
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
        std::fs::create_dir_all(&config_root).unwrap();

        std::fs::write(
            config_root.join("nub.jsonc"),
            r#"{
              "preload": ["./preload.mjs"],
              "nodeOptions": ["--stack-trace-limit=23"],
              "v8Flags": ["--max-old-space-size=256"],
              "envFile": "./runtime.env",
              "loader": { ".blob": "text", ".view": "jsx" },
              "conditions": ["runtime-config"],
              "tsconfig": "./tsconfig.runtime.jsonc"
            }"#,
        )
        .unwrap();
        std::fs::write(
            config_root.join("preload.mjs"),
            "globalThis.__runtimePreload = 'preloaded';\n",
        )
        .unwrap();
        std::fs::write(config_root.join("runtime.env"), "RUNTIME_ENV=from-config\n").unwrap();
        std::fs::write(
            config_root.join("alias.ts"),
            "export const alias = 'aliased';\n",
        )
        .unwrap();
        std::fs::write(
            config_root.join("tsconfig.runtime.jsonc"),
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "runtime-alias": ["./alias.ts"] }, "jsx": "react", "jsxFactory": "make" } }"#,
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
        std::fs::write(
            project.join("main.ts"),
            r#"import text from './message.blob';
import { alias } from 'runtime-alias';
import condition from 'conditional-pkg';
import component from './component.view';
console.log(JSON.stringify({
  env: process.env.RUNTIME_ENV,
  preload: globalThis.__runtimePreload,
  text, alias, condition,
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
            _temp: temp,
            project,
            xdg_config,
            cache,
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

    fn assert_probe(&self, args: &[&str]) {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "route {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value =
            serde_json::from_slice(output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout))
                .unwrap();
        assert_eq!(value["env"], "from-config");
        assert_eq!(value["preload"], "preloaded");
        assert_eq!(value["text"], "loaded-text");
        assert_eq!(value["alias"], "aliased");
        assert_eq!(value["condition"], "condition");
        assert_eq!(value["jsxMode"], "classic");
        assert_eq!(value["stack"], 23);
        // The two runtime option fields ride DIFFERENT channels, and the split is
        // the whole point: Node refuses most V8-only flags in `NODE_OPTIONS`
        // ("is not allowed in NODE_OPTIONS", exit 9) but accepts them on argv, so
        // `v8Flags` must arrive as argv and `nodeOptions` as `NODE_OPTIONS`.
        let exec_argv = value["execArgv"].to_string();
        assert!(
            exec_argv.contains("--max-old-space-size=256"),
            "v8Flags must reach the child on argv: {exec_argv}"
        );
        let options = value["nodeOptions"].as_str().unwrap_or_default();
        assert!(
            !options.contains("--max-old-space-size=256"),
            "v8Flags must not be routed through NODE_OPTIONS: {options}"
        );
        assert!(
            options.contains("--stack-trace-limit=23"),
            "nodeOptions must still travel through NODE_OPTIONS: {options}"
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
                "runtime transport must exclude {key}: {snapshot}"
            );
        }
    }

    #[cfg(unix)]
    fn assert_nubx_probe(&self) {
        let output = self.command_for(&self.nubx).arg("probe").output().unwrap();
        assert!(
            output.status.success(),
            "nubx route: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value =
            serde_json::from_slice(output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout))
                .unwrap();
        assert_eq!(value["env"], "from-config");
        assert_eq!(value["text"], "loaded-text");
        assert_eq!(value["alias"], "aliased");
        assert_eq!(value["condition"], "condition");
        assert_eq!(value["jsxMode"], "classic");
    }

    /// A `file:` package OUTSIDE the project — forces the nubx registry-
    /// fallback fetch path (not the `node_modules/.bin` local-bin path
    /// `assert_nubx_probe` covers), with no network involved. Its bin has a
    /// `#!/usr/bin/env node` shebang, which is what re-enters nub's `node`
    /// PATH shim and is the ONLY place augmentation reaches a fetched tool.
    #[cfg(unix)]
    fn assert_nubx_dlx_fallback_node_flag(&self) {
        let pkg_dir = self._temp.path().join("dlx-fallback-pkg");
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
            // Three-position rule: a flag BEFORE the bin positional is nubx's
            // own; after it, the flag would forward to the bin verbatim.
            let mut args: Vec<&str> = extra.to_vec();
            args.extend(["-y", "-p", package_spec.as_str(), "rtc-dlx-probe"]);
            let output = self.command_for(&self.nubx).args(args).output().unwrap();
            assert!(
                output.status.success(),
                "nubx dlx-fallback route {extra:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            let line = stdout
                .lines()
                .find(|l| l.starts_with('{'))
                .unwrap_or_else(|| panic!("no JSON line in: {stdout}"));
            serde_json::from_str::<serde_json::Value>(line).unwrap()
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

    /// The `dlx` / `x` spellings of the same ephemeral runner. They reach the
    /// engine verb instead of `nubx`'s clap surface, so `--node` gets its own
    /// argv handling there — and dlx's positional is `trailing_var_arg` +
    /// `allow_hyphen_values`, which used to make an unrecognized `--node` the
    /// package name rather than a usage error.
    fn assert_dlx_verb_node_flag(&self) {
        let pkg_dir = self._temp.path().join("dlx-verb-pkg");
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
        // `[pre] <verb> [post] -p <spec> rtc-dlx-verb-probe [tail]` — `pre`
        // covers `nub --node dlx`, `post` covers `nub dlx --node`, and `tail`
        // is the fetched tool's own argv.
        let run = |pre: &[&str], verb: &str, post: &[&str], tail: &[&str]| {
            let mut args: Vec<&str> = pre.to_vec();
            args.push(verb);
            args.extend(post);
            args.extend(["-p", package_spec.as_str(), "rtc-dlx-verb-probe"]);
            args.extend(tail);
            let output = self.command().args(&args).output().unwrap();
            assert!(
                output.status.success(),
                "nub {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            let line = stdout
                .lines()
                .find(|l| l.starts_with('{'))
                .unwrap_or_else(|| panic!("no JSON line in: {stdout}"));
            serde_json::from_str::<serde_json::Value>(line).unwrap()
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
}

#[test]
fn runtime_snapshot_reaches_the_file_run_and_script_entrypoints() {
    let fixture = Fixture::new();
    fixture.assert_probe(&["main.ts"]);
    fixture.assert_probe(&["run", "probe"]);
}

/// The `node_modules/.bin` routes, split out because they ride a
/// `#!/usr/bin/env node` shim. Windows resolves local bins through `.cmd`
/// shims instead — a different path that this fixture does not build — so
/// gating these two keeps the entrypoints above running on both platforms
/// rather than losing the whole file to one construct.
#[cfg(unix)]
#[test]
fn runtime_snapshot_reaches_the_local_bin_routes() {
    let fixture = Fixture::new();
    fixture.assert_probe(&["exec", "probe"]);
    fixture.assert_nubx_probe();
}

/// `nubx --node <tool>` on a bin NOT present locally (the registry/`-p`
/// fetch fallback, not `assert_nubx_probe`'s local-bin path) must disable
/// augmentation exactly like every other runtime entrypoint. The fetched
/// bin's `node` shebang re-enters nub as a PATH shim rather than reading
/// `--node` from argv, so this exercises a genuinely different code path
/// (`dlx_child_env` stamping `NODE_COMPAT` for the child) than the local-bin
/// case above.
#[cfg(unix)]
#[test]
fn nubx_node_flag_reaches_the_dlx_fallback_fetch_path() {
    let fixture = Fixture::new();
    fixture.assert_nubx_dlx_fallback_node_flag();
}

/// `nubx`, `nub dlx`, and `nub x` are the same command, so `--node` must work
/// on all three — in either flag order, and without being stolen from the
/// fetched tool when it appears after the tool name.
#[test]
fn node_flag_works_on_the_dlx_and_x_spellings_in_either_order() {
    let fixture = Fixture::new();
    fixture.assert_dlx_verb_node_flag();
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
    let nested = fixture._temp.path().join("nested-project");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("nub.jsonc"),
        r#"{
          "nodeOptions": ["--stack-trace-limit=41"]
        }"#,
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

    let inherited_shim_entries: Vec<String> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|entry| entry.to_string_lossy().contains("nub-node-shim-"))
                .map(|entry| entry.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
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
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "nested fresh Nub invocation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(
            output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout),
        )
        .unwrap()
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
            inherited_shim_entries.len() + 1,
            "fresh child must replace the parent shim, not accumulate it: {value}"
        );
        let inherited_tail: Vec<&str> = shim_entries[1..]
            .iter()
            .map(|entry| entry.as_str().unwrap())
            .collect();
        let expected_tail: Vec<&str> = inherited_shim_entries.iter().map(String::as_str).collect();
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
    let nested = fixture._temp.path().join("nested-bin-path");
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

    let output = fixture
        .command()
        .env("NESTED_NUB", nub_binary())
        .env("NESTED_NUBX", &fixture.nubx)
        .env("NESTED_PROJECT", &nested)
        .env("PARENT_BIN", &outer_bin)
        .args(["run", "nested-bin-path"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "nested bin-path probes failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout))
            .unwrap();
    assert_eq!(value["file"]["hasParentBin"], true, "{value}");
    assert_eq!(value["nubx"]["hasParentBin"], true, "{value}");
}

#[test]
fn fresh_nested_nub_node_mode_keeps_descendant_node_vanilla() {
    let fixture = Fixture::new();
    let nested = fixture._temp.path().join("nested-compat-project");
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

    let inherited_shim_entries: Vec<String> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|entry| entry.to_string_lossy().contains("nub-node-shim-"))
                .map(|entry| entry.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let path_addition = fixture._temp.path().join("user-path-addition");
    let replacement_compile_cache = fixture._temp.path().join("user-compile-cache");
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
            let quoted = fixture._temp.path().join("quoted;path");
            std::fs::create_dir_all(&quoted).unwrap();
            let mut path = std::ffi::OsString::from(format!("\"{}\";", quoted.display()));
            path.push(std::env::var_os("PATH").unwrap_or_default());
            command.env("PATH", path);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "fresh nested --node invocation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(
            output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout),
        )
        .unwrap()
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
        serde_json::to_value(&inherited_shim_entries).unwrap(),
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
        serde_json::to_value(&inherited_shim_entries).unwrap(),
        "the old shim must still be removed from a user-modified PATH: {mutated}"
    );
}

#[test]
fn runtime_snapshot_reaches_watch_child() {
    use std::io::BufRead;

    let fixture = Fixture::new();
    let mut child = fixture
        .command()
        .args(["watch", "main.ts"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = std::io::BufReader::new(stdout).lines();
        let _ = tx.send(lines.next().transpose());
        for _ in lines {}
    });
    let line = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("watch child produced no output")
        .unwrap()
        .unwrap();
    let _ = child.kill();
    let _ = child.wait();
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["env"], "from-config");
    assert_eq!(value["text"], "loaded-text");
    assert_eq!(value["alias"], "aliased");
    assert_eq!(value["condition"], "condition");
    assert_eq!(value["jsxMode"], "classic");
}

#[test]
fn node_compat_config_is_zero_augmentation_and_environment_false_overrides_it() {
    let fixture = Fixture::new();
    let config = fixture.project.join("nub.jsonc");
    std::fs::write(
        fixture.project.join("compat.js"),
        "console.log(globalThis.__runtimePreload ?? 'vanilla');\n",
    )
    .unwrap();
    std::fs::write(
        &config,
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
    // The nested Node process may surface an equivalent Windows path with
    // forward slashes; this assertion is about preserving the path, not its
    // separator spelling.
    let user_compile_cache_text = user_compile_cache.to_string_lossy().replace('\\', "/");
    let inherited_shim_entries: Vec<String> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|entry| entry.to_string_lossy().contains("nub-node-shim-"))
                .map(|entry| entry.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    for script in ["nested-env", "nested-cli"] {
        let output = fixture
            .command()
            .env("NODE_OPTIONS", "--trace-warnings")
            .env("NODE_PATH", "/user/node-path")
            .env("NODE", "/user/node")
            .env("NODE_COMPILE_CACHE", &user_compile_cache)
            .args(["run", script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value =
            serde_json::from_slice(output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout))
                .unwrap();
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
            serde_json::json!(inherited_shim_entries),
            "{script}"
        );
    }
}

#[test]
fn watch_composes_explicit_config_env_sources_before_cli_env_file() {
    use std::io::BufRead;

    let fixture = Fixture::new();
    let config_root = fixture.project.clone();
    std::fs::write(
        config_root.join("first.env"),
        "CONFIG_ONLY=first\nSHARED=first\n",
    )
    .unwrap();
    std::fs::write(config_root.join("second.env"), "SHARED=second\n").unwrap();
    std::fs::write(
        config_root.join("nub.jsonc"),
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

    let mut child = fixture
        .command()
        .arg(format!("--env-file={}", cli_env.display()))
        .args(["watch", "env-probe.js"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = std::io::BufReader::new(stdout).lines();
        let _ = tx.send(lines.next().transpose());
        for _ in lines {}
    });
    let line = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("watch child produced no output")
        .unwrap()
        .unwrap();
    let _ = child.kill();
    let _ = child.wait();
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
        String::from_utf8_lossy(&output.stderr).contains("not supported by Node"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
