//! A script body on Windows runs under the bundled busybox-w32 `sh`, whose `ash`
//! rewrites `\` to `/` in every variable it imports unless started with `-X`. The
//! programs a script starts inherit what the shell holds, so both checks here read the
//! environment of a `node` the script starts: a path keeps its backslashes, as under
//! npm and pnpm, and Nub's preload appears in `NODE_OPTIONS` once. A rewritten
//! `NODE_OPTIONS` no longer matches the re-entrancy key, so that `node` augmented a
//! second time and its `NODE_OPTIONS` carried the preload twice.
//!
//! CI points `__NUB_BUSYBOX_EXE` at the vendored busybox for the Windows test leg.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

const PROBE_PATH: &str = r"C:\nub-probe\dir";

/// Writes what its `node` saw to the file named by its argument.
const PROBE: &str = r#"const o = process.env.NODE_OPTIONS || "";
require("fs").writeFileSync(process.argv[2], JSON.stringify({
  path: process.env.ENV_PROBE_PATH,
  preloads: (o.match(/preload\.[cm]js/g) || []).length,
  nodeOptions: o,
}));
"#;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

/// An offline project whose `dump` script and `postinstall` both run the probe.
fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nub-win-env-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    std::fs::write(
        dir.join("nub.lock"),
        "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n",
    )
    .unwrap();
    std::fs::write(dir.join("probe.cjs"), PROBE).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"app","version":"1.0.0","packageManager":"nub@0.0.1","scripts":{"dump":"node probe.cjs run.json","postinstall":"node probe.cjs install.json"}}"#,
    )
    .unwrap();
    dir
}

fn run_and_read(dir: &Path, args: &[&str], output: &str) -> serde_json::Value {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("ENV_PROBE_PATH", PROBE_PATH)
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_COMPAT")
        .env("NUB_SELF_SHIM", "0")
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "nub {args:?} failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    let text = std::fs::read_to_string(dir.join(output)).unwrap_or_else(|_| {
        panic!("the script's `node` wrote no {output}\nstdout: {stdout}\nstderr: {stderr}")
    });
    serde_json::from_str(&text).unwrap()
}

fn assert_unchanged(seen: &serde_json::Value, surface: &str) {
    assert_eq!(
        seen["path"], PROBE_PATH,
        "a `node` started by {surface} must see the environment as Windows set it; \
         busybox without `-X` rewrites `\\` to `/`. Saw {seen}"
    );
    assert_eq!(
        seen["preloads"], 1,
        "a `node` started by {surface} must carry Nub's preload once: a second token means \
         it did not recognize the parent's injection and augmented again. Saw {seen}"
    );
}

#[test]
fn a_script_hands_its_programs_the_environment_unchanged() {
    let dir = fixture("run");
    let seen = run_and_read(&dir, &["run", "dump"], "run.json");
    assert_unchanged(&seen, "`nub run`");
}

#[test]
fn a_lifecycle_script_hands_its_programs_the_environment_unchanged() {
    let dir = fixture("install");
    let seen = run_and_read(&dir, &["install"], "install.json");
    assert_unchanged(&seen, "a `postinstall`");
}
