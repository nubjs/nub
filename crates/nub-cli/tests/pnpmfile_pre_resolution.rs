//! The `preResolution` pnpmfile hook, against pnpm's contract.
//!
//! pnpm calls the hook on EVERY install — before it decides whether the
//! lockfile is up to date — and hands it the in-memory `LockfileObject`:
//! never `null`, `packages[key].resolution` readable, importer
//! specifiers split out into their own map. A `console.log` in the hook
//! body reaches the terminal, and the second argument is a
//! `{info, warn}` logger rather than `{log}`.
//!
//! Every row is OFFLINE: the only dependency is a sibling directory
//! (`file:`), and the project points its registry at a dead port so an
//! accidental fetch fails loudly instead of passing quietly.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// A pnpm-incumbent project whose one dependency is the local `dep/`
/// directory, plus a pnpmfile that records the whole `preResolution`
/// context to `observed-<n>.json` and prints a marker on stdout.
fn fixture(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-preresolution-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dep")).unwrap();
    std::fs::write(dir.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@10.0.0","dependencies":{"dep":"file:./dep"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("dep/package.json"),
        r#"{"name":"dep","version":"2.3.4"}"#,
    )
    .unwrap();
    // Numbering the output file is what makes "did it run a second
    // time?" observable — a single overwritten file cannot tell a
    // second run from a first.
    std::fs::write(
        dir.join(".pnpmfile.cjs"),
        r#"
module.exports = {
  hooks: {
    preResolution(ctx, logger) {
      const fs = require('fs');
      const seen = fs.readdirSync('.').filter((f) => f.startsWith('observed-')).length;
      fs.writeFileSync('observed-' + (seen + 1) + '.json', JSON.stringify({
        wantedLockfile: ctx.wantedLockfile,
        currentLockfile: ctx.currentLockfile,
        existsCurrentLockfile: ctx.existsCurrentLockfile,
        existsNonEmptyWantedLockfile: ctx.existsNonEmptyWantedLockfile,
        loggerKeys: logger == null ? [] : Object.keys(logger).sort(),
      }, null, 2));
      console.log('PRERESOLUTION_MARKER');
    },
  },
};
"#,
    )
    .unwrap();
    dir
}

fn run(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("NUB_SELF_SHIM", "0")
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

fn observed(dir: &Path, n: usize) -> serde_json::Value {
    let path = dir.join(format!("observed-{n}.json"));
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("preResolution run {n} left no {}: {e}", path.display()));
    serde_json::from_str(&body).expect("hook wrote valid JSON")
}

/// The whole contract in one project, because the interesting part is
/// the *sequence*: what the hook sees with no lockfile, that it runs
/// again once the lockfile is current, and what it sees then.
#[test]
fn pre_resolution_runs_on_every_install_with_pnpms_lockfile_shape() {
    let dir = fixture("contract");

    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "first install\nstdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("PRERESOLUTION_MARKER"),
        "a console.log in the hook body belongs on stdout, as it does under pnpm; \
         nub used to swallow it because the shim owned the child's stdout.\n\
         stdout: {stdout}\nstderr: {stderr}"
    );

    let first = observed(&dir, 1);
    assert!(
        first["wantedLockfile"].is_object(),
        "with no lockfile pnpm passes an empty OBJECT, never null: {first}"
    );
    assert!(
        first["wantedLockfile"].get("packages").is_none(),
        "pnpm's synthesized empty lockfile has no `packages` key at all — \
         hooks guard on `if (!lockfile.packages) return`: {first}"
    );
    assert_eq!(
        first["wantedLockfile"]["importers"]["."]["specifiers"],
        serde_json::json!({}),
        "every importer carries a specifiers map, empty or not: {first}"
    );
    assert_eq!(
        first["existsCurrentLockfile"],
        serde_json::json!(false),
        "no lockfile was on disk: {first}"
    );
    assert_eq!(
        first["loggerKeys"],
        serde_json::json!(["info", "log", "warn"]),
        "pnpm's preResolution logger is {{info, warn}}; `log` is aube's own \
         spelling, kept so pnpmfiles written against aube keep working: {first}"
    );

    // The regression the issue reported: a second install has an
    // up-to-date lockfile, takes the reuse path, and used to skip the
    // hook entirely.
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(
        code, 0,
        "second install\nstdout: {stdout}\nstderr: {stderr}"
    );
    let second = observed(&dir, 2);

    let packages = second["wantedLockfile"]["packages"]
        .as_object()
        .unwrap_or_else(|| panic!("second run must see the written lockfile: {second}"));
    let (key, entry) = packages
        .iter()
        .find(|(k, _)| k.starts_with("dep@"))
        .unwrap_or_else(|| panic!("the local dependency must appear in packages: {second}"));
    assert!(
        entry.get("resolution").is_some(),
        "packages[{key}].resolution is what a hook rewriting resolutions reads; \
         nub's old projection had no such field: {entry}"
    );
    assert_eq!(
        second["wantedLockfile"]["importers"]["."]["specifiers"]["dep"],
        serde_json::json!("file:./dep"),
        "the importer's inline {{specifier, version}} pair splits into a \
         specifiers map, the way pnpm's reader leaves it: {second}"
    );
    assert_eq!(
        second["existsNonEmptyWantedLockfile"],
        serde_json::json!(true),
        "a lockfile with packages is non-empty: {second}"
    );
    assert_eq!(
        second["currentLockfile"], second["wantedLockfile"],
        "aube keeps no separate installed-state lockfile, so the two sides \
         are the same object at preResolution time: {second}"
    );
}

/// `--frozen-lockfile` and `--no-frozen-lockfile` both withhold the
/// parsed lockfile from the resolver, for reasons that have nothing to
/// do with the hook. `wantedLockfile` is the file on disk, so neither
/// flag may empty it out.
#[test]
fn frozen_modes_still_hand_the_hook_the_on_disk_lockfile() {
    for flag in ["--frozen-lockfile", "--no-frozen-lockfile"] {
        let dir = fixture("frozen");
        let (stdout, stderr, code) = run(&dir, &["install"]);
        assert_eq!(code, 0, "seed install\nstdout: {stdout}\nstderr: {stderr}");

        let (stdout, stderr, code) = run(&dir, &["install", flag]);
        assert_eq!(code, 0, "{flag}\nstdout: {stdout}\nstderr: {stderr}");
        let second = observed(&dir, 2);
        let packages = second["wantedLockfile"]["packages"]
            .as_object()
            .unwrap_or_else(|| panic!("{flag} must still show the lockfile: {second}"));
        assert!(
            packages.keys().any(|k| k.starts_with("dep@")),
            "{flag} left the hook with an empty lockfile: {second}"
        );
    }
}
