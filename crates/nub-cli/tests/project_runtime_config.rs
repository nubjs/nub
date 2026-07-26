//! End-to-end coverage for project `nub.jsonc` runtime consumers. Every route
//! also carries a restrictive sandbox value to prove sandbox config stays inert.

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

        // The project file is live, while the restrictive sandbox value remains
        // inert. The runtime assertions below make that distinction non-vacuous.
        std::fs::write(
            config_root.join("nub.jsonc"),
            r#"{
              "preload": ["./preload.mjs"],
              "nodeOptions": ["--stack-trace-limit=23"],
              "v8Flags": ["--max-old-space-size=256"],
              "env": "./runtime.env",
              "define": { "CONFIG_WORD": "\"defined\"" },
              "loader": { ".blob": "text", ".view": "jsx" },
              "conditions": ["runtime-config"],
              "tsconfig": "./tsconfig.runtime.jsonc",
              "sandbox": { "fs": false, "net": false, "vars": false }
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
  define: CONFIG_WORD,
  env: process.env.RUNTIME_ENV,
  preload: globalThis.__runtimePreload,
  text, alias, condition,
  jsxMode: component.mode,
  stack: Error.stackTraceLimit,
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
        assert_eq!(value["define"], "defined");
        assert_eq!(value["env"], "from-config");
        assert_eq!(value["preload"], "preloaded");
        assert_eq!(value["text"], "loaded-text");
        assert_eq!(value["alias"], "aliased");
        assert_eq!(value["condition"], "condition");
        assert_eq!(value["jsxMode"], "classic");
        assert_eq!(value["stack"], 23);
        let options = value["nodeOptions"].as_str().unwrap();
        assert!(options.contains("--max-old-space-size=256"), "{options}");
        let snapshot: serde_json::Value = serde_json::from_str(
            value["runtimeSnapshot"]
                .as_str()
                .expect("runtime snapshot reaches the augmented child"),
        )
        .expect("runtime snapshot is JSON");
        for key in ["sandbox", "install", "dlx"] {
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
        assert_eq!(value["define"], "defined");
        assert_eq!(value["env"], "from-config");
        assert_eq!(value["text"], "loaded-text");
        assert_eq!(value["alias"], "aliased");
        assert_eq!(value["condition"], "condition");
        assert_eq!(value["jsxMode"], "classic");
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

#[test]
fn inherited_runtime_snapshot_ignores_all_sandbox_config_positions() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project.join("inherited.js"),
        "console.log('inherited')\n",
    )
    .unwrap();
    let snapshot = r#"{
      "nodeCompat": false,
      "preload": [],
      "nodeOptions": [],
      "v8Flags": [],
      "env": { "kind": "default" },
      "define": {},
      "loader": {},
      "conditions": [],
      "tsconfig": null,
      "sandbox": { "fs": false, "net": false, "vars": false },
      "install": { "sandbox": { "fs": false, "net": false, "vars": false } },
      "dlx": { "sandbox": { "fs": false, "net": false, "vars": false } }
    }"#;
    let output = fixture
        .command()
        .env("__NUB_RUNTIME_CONFIG", snapshot)
        .arg("inherited.js")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "sandbox-only inherited fields must deserialize inertly: {}",
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
    assert_eq!(value["define"], "defined");
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
        r#"{ "env": ["./first.env", "./second.env"] }"#,
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
