//! End-to-end coverage for project `nub.jsonc` runtime consumers.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
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
    nubx: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let xdg_config = temp.path().join("config");
        let config_root = project.clone();
        let cache = temp.path().join("cache");
        let nubx = temp.path().join("nubx");
        std::os::unix::fs::symlink(nub_binary(), &nubx).unwrap();
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
        let bin = project.join("node_modules/.bin/probe");
        std::fs::write(
            &bin,
            "#!/usr/bin/env node\n(async () => { await import('../../main.ts'); })();\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        Self {
            _temp: temp,
            project,
            xdg_config,
            cache,
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

        let package_spec = format!("file:{}", pkg_dir.display());
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

        let package_spec = format!("file:{}", pkg_dir.display());
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
fn runtime_snapshot_reaches_file_script_node_argv0_exec_and_nubx() {
    let fixture = Fixture::new();
    fixture.assert_probe(&["main.ts"]);
    fixture.assert_probe(&["run", "probe"]);
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
        .command()
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
  shimInPath: (process.env.PATH ?? '').includes('nub-node-shim-'),
}));
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.project.join("package.json"),
        r#"{ "scripts": {
          "nested-env": "NODE_COMPAT=1 node nested-compat.js",
          "nested-cli": "node --node nested-compat.js"
        } }"#,
    )
    .unwrap();
    let user_compile_cache = fixture.project.join("user-compile-cache");
    let user_compile_cache_text = user_compile_cache.to_string_lossy().into_owned();
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
        assert_eq!(value["preload"], serde_json::Value::Null, "{script}");
        assert_eq!(value["execArgv"], serde_json::json!([]), "{script}");
        assert_eq!(value["nodeOptions"], "--trace-warnings", "{script}");
        assert_eq!(value["nodePath"], "/user/node-path", "{script}");
        assert_eq!(value["node"], "/user/node", "{script}");
        assert_eq!(value["compileCache"], user_compile_cache_text, "{script}");
        assert_eq!(value["runtimeConfig"], serde_json::Value::Null, "{script}");
        assert_eq!(value["nubVersion"], serde_json::Value::Null, "{script}");
        assert_eq!(value["shimInPath"], false, "{script}");
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
