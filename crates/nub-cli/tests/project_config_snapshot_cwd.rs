//! The effective-config snapshot must initialize from the FINAL working
//! directory after the GLOBAL `--cwd` is applied, not from the ambient parent.
//! The ancestor route is covered in integration.rs (`node_argv0_…`), the argv0
//! PM route in pm_shim.rs, and the verb-local `-C`/`--dir` route in
//! install_engine.rs; this file adds only the global-flag variant.

use std::path::PathBuf;
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
    std::fs::write(target.join("nub.jsonc"), r#"{ "conditions": [] }"#).unwrap();
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
    assert_eq!(
        lines.len(),
        1,
        "exactly one loaded snapshot, anchored to the --cwd dir: {lines:?}"
    );
    let logged = lines[0]
        .strip_prefix("cwd=")
        .and_then(|rest| rest.strip_suffix(" project=loaded"))
        .unwrap_or_else(|| panic!("unexpected snapshot log line: {}", lines[0]));
    // Compare resolved directories, not path spellings. Windows hands the child
    // whatever form the environment carried — an 8.3 short name under a
    // RUNNER~1 home — while `canonicalize` returns the extended-length `\\?\`
    // form. Both name the same directory; the contract under test is WHICH
    // directory the snapshot resolved from.
    assert_eq!(
        std::path::Path::new(logged)
            .canonicalize()
            .expect("logged cwd exists"),
        target.canonicalize().expect("target exists"),
        "exactly one loaded snapshot, anchored to the --cwd dir"
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
    // The message must LOCATE the offending file, not merely name it: discovery
    // walks the ancestor chain unbounded, so a stray `nub.jsonc` above the cwd is
    // otherwise unfindable. Only the trailing components are pinned — the parent
    // prefix differs by platform (the macOS `/private` symlink, Windows 8.3).
    let stderr = String::from_utf8_lossy(&output.stderr);
    let located = format!(
        "{}{}nub.jsonc",
        temp.path().file_name().unwrap().to_string_lossy(),
        std::path::MAIN_SEPARATOR
    );
    assert!(
        stderr.contains("parsing ") && stderr.contains(&located),
        "{stderr}"
    );
    assert!(!temp.path().join("must-not-run").exists());
}
