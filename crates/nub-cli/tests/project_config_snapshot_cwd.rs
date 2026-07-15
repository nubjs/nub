//! P5 — the effective-config snapshot must initialize from the FINAL working
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
    // Malformed dormant project files in BOTH dirs: gate-off must read neither,
    // while the log proves the snapshot anchored to the `--cwd` dir.
    std::fs::write(ambient.join("nub.jsonc"), "{ malformed").unwrap();
    std::fs::write(target.join("nub.jsonc"), "{ malformed").unwrap();
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
        "gate-off malformed project files must not block the --cwd run; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "probe-ok");

    let lines: Vec<_> = std::fs::read_to_string(&log)
        .expect("the --cwd route must initialize the config snapshot")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        lines,
        vec![format!(
            "cwd={} project=none",
            target.canonicalize().unwrap().display()
        )],
        "exactly one snapshot, anchored to the --cwd dir (not the ambient parent)"
    );
}
