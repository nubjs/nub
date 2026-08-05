//! An unloadable `nub.jsonc` is fail-loud for the commands that consume it (see
//! `project_config_snapshot_cwd.rs`), but discovery walks unbounded to the
//! filesystem root, so a stray file above the cwd must not take out the commands
//! that read no project config, nor the `--node` / `NODE_COMPAT` escape hatch
//! whose whole contract is to run with augmentation off.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

/// A directory holding a `nub.jsonc` that cannot be parsed, plus a probe script
/// that records having run.
fn fixture(temp: &Path) {
    std::fs::write(temp.join("nub.jsonc"), "{ malformed").unwrap();
    std::fs::write(
        temp.join("probe.js"),
        "require('fs').writeFileSync('ran', 'ran');\n",
    )
    .unwrap();
}

fn nub_in(dir: &Path, cache: &Path) -> Command {
    let mut cmd = Command::new(nub_binary());
    cmd.current_dir(dir).env("XDG_CACHE_HOME", cache);
    cmd
}

#[test]
fn commands_that_read_no_project_config_survive_an_unloadable_one() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fixture(temp.path());

    for args in [
        vec!["--version"],
        vec!["--help"],
        // A pinned version keeps the dry run offline (`latest` would resolve
        // through the GitHub API).
        vec!["upgrade", "--dry-run", "--version", "0.0.0"],
        vec!["agent", "skill"],
        // These manual command groups print their verb lists before dispatching
        // any subcommand, so they must not initialize a project snapshot.
        vec!["node"],
        vec!["pm"],
    ] {
        let output = nub_in(temp.path(), &cache)
            .args(&args)
            .output()
            .unwrap_or_else(|e| panic!("run nub {}: {e}", args.join(" ")));
        assert_eq!(
            output.status.code(),
            Some(0),
            "`nub {}` must not be gated on a config it never reads; stderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn forced_compat_degrades_an_unloadable_config_instead_of_aborting() {
    // `--node` and `NODE_COMPAT` are the same contract in two spellings, and
    // both must survive: compat already decided augmentation is off, so nothing
    // in the file could change the outcome.
    for spelling in ["--node", "NODE_COMPAT"] {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        fixture(temp.path());

        let mut cmd = nub_in(temp.path(), &cache);
        if spelling == "--node" {
            cmd.args(["--node", "probe.js"]);
        } else {
            cmd.env("NODE_COMPAT", "1").arg("probe.js");
        }
        let output = cmd.output().expect("run the compat file runner");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "{spelling} must still run with augmentation off; stderr: {stderr}"
        );
        assert!(
            temp.path().join("ran").exists(),
            "{spelling} must reach the entry file; stderr: {stderr}"
        );
        assert!(
            stderr.contains("nub.jsonc") && stderr.contains("ignored"),
            "{spelling} must name the ignored file on stderr, got: {stderr}"
        );
    }
}
