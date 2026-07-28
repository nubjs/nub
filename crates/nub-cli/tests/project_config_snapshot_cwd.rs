//! P5 — the effective-config snapshot must initialize from the FINAL working
//! directory after the GLOBAL `--cwd` is applied, not from the ambient parent.
//! The ancestor route is covered in integration.rs (`node_argv0_…`), the argv0
//! PM route in pm_shim.rs, and the verb-local `-C`/`--dir` route in
//! install_engine.rs; this file adds only the global-flag variant.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

#[test]
fn global_cwd_flag_initializes_one_snapshot_from_the_requested_dir() {
    let temp = tempfile::tempdir().unwrap();
    let ambient = temp.path().join("ambient");
    let target = temp.path().join("target");
    std::fs::create_dir_all(&ambient).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    // The ambient file is malformed, while the requested directory has a valid
    // config. Success proves discovery starts after `--cwd` is applied.
    std::fs::write(ambient.join("nub.jsonc"), "{ malformed").unwrap();
    std::fs::write(target.join("nub.jsonc"), r#"{ "sandbox": true }"#).unwrap();
    std::fs::write(target.join("probe.js"), "console.log('probe-ok');\n").unwrap();
    let log = temp.path().join("snapshot.log");

    let output = Command::new(nub_binary())
        // Relative value: `--cwd` resolves against the ambient dir (the pnpm
        // spelling), and the entry file resolves after the chdir.
        .args(["--cwd", "../target", "probe.js"])
        .current_dir(&ambient)
        .env("XDG_CACHE_HOME", temp.path().join("cache"))
        .env("__NUB_TEST_CONFIG_SNAPSHOT_LOG", &log)
        .output()
        .expect("run nub --cwd file-run");
    assert_eq!(
        output.status.code(),
        Some(0),
        "the ambient malformed config must not block the --cwd run; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "probe-ok");

    let lines: Vec<_> = std::fs::read_to_string(&log)
        .expect("the --cwd route must initialize the config snapshot")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 1, "exactly one loaded snapshot: {lines:?}");
    let (cwd, loaded) = lines[0]
        .strip_prefix("cwd=")
        .and_then(|line| line.rsplit_once(' '))
        .unwrap_or_else(|| panic!("unexpected snapshot line: {}", lines[0]));
    assert_eq!(loaded, "project=loaded");
    // Both sides are canonicalized rather than string-compared: nub reports the cwd as
    // `env::current_dir` spells it, which on Windows keeps the 8.3 short components it was
    // handed and carries no `\\?\` prefix, while `canonicalize` expands and prefixes both.
    // The directory identity is the contract. (Same reasoning as the verb-local `-C`/
    // `--dir` variant in install_engine.rs.)
    assert_eq!(
        Path::new(cwd).canonicalize().unwrap(),
        target.canonicalize().unwrap(),
        "the snapshot must be anchored to the --cwd dir"
    );
}

#[test]
fn malformed_project_config_stops_before_the_entry_file_runs() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("nub.jsonc"), "{ malformed").unwrap();
    std::fs::write(
        temp.path().join("probe.js"),
        "require('fs').writeFileSync('must-not-run', 'ran');\n",
    )
    .unwrap();

    let output = Command::new(nub_binary())
        .arg("probe.js")
        .current_dir(temp.path())
        .env("XDG_CACHE_HOME", temp.path().join("cache"))
        .output()
        .expect("run nub with malformed project config");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("parsing nub.jsonc"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!temp.path().join("must-not-run").exists());
}
