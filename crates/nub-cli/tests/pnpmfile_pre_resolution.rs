//! The `preResolution` pnpmfile hook, against pnpm 12.4.1's contract.
//!
//! Two things decide what happens here, and the suite is organised around
//! them. WHICH PROJECT: the pnpmfile is pnpm's own file, so it is read under
//! pnpm's incumbency and nowhere else. WHETHER THE INSTALL RESOLVES: pnpm
//! calls the hook from the resolver, so an install that reuses an up-to-date
//! lockfile never reaches it — the hook is not a per-command event.
//!
//! Every claim below was measured against real pnpm 12.4.1 on the same
//! fixture, and nub matched it byte for byte. Where a row reads as the
//! opposite of what it used to assert, that is the previous engine's
//! contract being retired rather than a regression: it synthesized a lockfile
//! skeleton for the hook, split importer entries into a `specifiers` map, and
//! called the hook once per install. pnpm does none of those.
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

/// A project whose one dependency is the local `dep/` directory, plus a
/// pnpmfile recording the whole `preResolution` context to
/// `observed-<n>.json` and printing a marker on stdout.
///
/// `pnpm_incumbent` is spelled as an empty `pnpm-workspace.yaml` rather than
/// a `packageManager` pin, deliberately: a pin naming a pnpm the engine is
/// not is a FOREIGN pin, which both pnpm and nub honor by provisioning that
/// version and handing the whole command over to it. A fixture pinned that
/// way measures the pinned pnpm, not the engine under test — and it does so
/// silently, since the delegate installs perfectly well.
fn fixture(tag: &str, pnpm_incumbent: bool) -> PathBuf {
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
        r#"{"name":"app","version":"1.0.0","dependencies":{"dep":"file:./dep"}}"#,
    )
    .unwrap();
    if pnpm_incumbent {
        std::fs::write(dir.join("pnpm-workspace.yaml"), "packages: []\n").unwrap();
    }
    std::fs::write(
        dir.join("dep/package.json"),
        r#"{"name":"dep","version":"2.3.4"}"#,
    )
    .unwrap();
    std::fs::write(dir.join(".pnpmfile.cjs"), RECORDER).unwrap();
    dir
}

/// Numbering the output file is what makes "did it run again?" observable —
/// a single overwritten file cannot tell a second run from a first.
const RECORDER: &str = r#"
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
"#;

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

/// How many times the hook has run in this fixture so far — one
/// `observed-<n>.json` per firing.
fn firings(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .expect("fixture dir readable")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("observed-"))
        .count()
}

fn observed(dir: &Path, n: usize) -> serde_json::Value {
    let path = dir.join(format!("observed-{n}.json"));
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("preResolution run {n} left no {}: {e}", path.display()));
    serde_json::from_str(&body).expect("hook wrote valid JSON")
}

/// The hook belongs to the RESOLVER, not to the command. An install that
/// reuses an up-to-date lockfile resolves nothing and so calls nothing;
/// `update` re-resolves and calls it exactly once, not once per stage.
#[test]
fn the_hook_runs_when_an_install_resolves_and_not_when_it_reuses() {
    let dir = fixture("resolves", true);

    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "first install\nstdout: {stdout}\nstderr: {stderr}");
    assert_eq!(firings(&dir), 1, "the first install resolves: {stderr}");
    assert!(
        stdout.contains("PRERESOLUTION_MARKER"),
        "a console.log in the hook body belongs on stdout, as it does under \
         pnpm; nub used to swallow it because the shim owned the child's \
         stdout.\nstdout: {stdout}\nstderr: {stderr}"
    );

    for args in [
        ["install"].as_slice(),
        &["install", "--frozen-lockfile"],
        &["install", "--no-frozen-lockfile"],
    ] {
        let (stdout, stderr, code) = run(&dir, args);
        assert_eq!(code, 0, "{args:?}\nstdout: {stdout}\nstderr: {stderr}");
        assert_eq!(
            firings(&dir),
            1,
            "{args:?} reuses the lockfile, so nothing resolves and the hook \
             must not fire again\nstdout: {stdout}\nstderr: {stderr}"
        );
    }

    let (stdout, stderr, code) = run(&dir, &["update"]);
    assert_eq!(code, 0, "update\nstdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        firings(&dir),
        2,
        "update re-resolves, and it calls the hook ONCE — the regression this \
         guards is a call per resolution stage\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// What the hook receives, on both sides of the first write.
///
/// `update` is what produces the second observation, because it is the only
/// command above that re-resolves — which is also why the two halves live in
/// one test: the interesting part is the pair, not either shape alone.
#[test]
fn the_hook_sees_pnpms_own_lockfile_shape() {
    let dir = fixture("shape", true);

    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "first install\nstdout: {stdout}\nstderr: {stderr}");

    let first = observed(&dir, 1);
    assert_eq!(
        first["wantedLockfile"],
        serde_json::json!({}),
        "with no lockfile on disk pnpm passes a bare empty object — not null, \
         and not a synthesized skeleton with importers and settings: {first}"
    );
    assert_eq!(
        first["currentLockfile"],
        serde_json::json!({}),
        "and the installed-state side is empty for the same reason: {first}"
    );
    assert_eq!(
        first["existsCurrentLockfile"],
        serde_json::json!(false),
        "no lockfile was on disk: {first}"
    );
    assert_eq!(
        first["existsNonEmptyWantedLockfile"],
        serde_json::json!(false),
        "nor a non-empty wanted one: {first}"
    );
    assert_eq!(
        first["loggerKeys"],
        serde_json::json!(["info", "warn"]),
        "pnpm's preResolution logger is {{info, warn}}: {first}"
    );

    let (stdout, stderr, code) = run(&dir, &["update"]);
    assert_eq!(code, 0, "update\nstdout: {stdout}\nstderr: {stderr}");
    let second = observed(&dir, 2);

    assert_eq!(
        second["wantedLockfile"]["importers"]["."]["dependencies"]["dep"],
        serde_json::json!({"specifier": "file:./dep", "version": "file:dep"}),
        "the importer entry stays the inline {{specifier, version}} pair pnpm \
         writes; it is not split into a separate specifiers map: {second}"
    );
    assert_eq!(
        second["wantedLockfile"]["packages"]["dep@file:dep"]["resolution"],
        serde_json::json!({"type": "directory", "directory": "dep"}),
        "packages[key].resolution is what a hook rewriting resolutions reads: {second}"
    );
    assert_eq!(
        second["existsNonEmptyWantedLockfile"],
        serde_json::json!(true),
        "a lockfile with packages is non-empty: {second}"
    );
    assert_eq!(
        second["currentLockfile"], second["wantedLockfile"],
        "with the install already applied, the two sides agree: {second}"
    );
}

/// A workspace wired together only by `workspace:*` resolves to no package
/// rows at all, and pnpm reports that as an EMPTY wanted lockfile even though
/// the importers carry real specifiers.
///
/// This is the one shape where the flag and the importer content disagree, so
/// a hook branching on the flag takes the path pnpm intends only if nub
/// reports the same answer. The previous engine reported the opposite.
#[test]
fn a_link_only_workspace_reads_as_an_empty_lockfile() {
    let dir = fixture("workspace", true);
    std::fs::remove_dir_all(dir.join("dep")).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0","private":true}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("pnpm-workspace.yaml"),
        "packages:\n  - \"packages/*\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("packages/a")).unwrap();
    std::fs::create_dir_all(dir.join("packages/b")).unwrap();
    std::fs::write(
        dir.join("packages/a/package.json"),
        r#"{"name":"a","version":"1.0.0","dependencies":{"b":"workspace:*"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("packages/b/package.json"),
        r#"{"name":"b","version":"1.0.0"}"#,
    )
    .unwrap();

    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "seed install\nstdout: {stdout}\nstderr: {stderr}");
    let (stdout, stderr, code) = run(&dir, &["update", "-r"]);
    assert_eq!(
        code, 0,
        "recursive update\nstdout: {stdout}\nstderr: {stderr}"
    );

    let second = observed(&dir, 2);
    assert!(
        second["wantedLockfile"].get("packages").is_none(),
        "a link-only workspace resolves to no package rows — that is the \
         precondition this test needs: {second}"
    );
    assert_eq!(
        second["wantedLockfile"]["importers"]["packages/a"]["dependencies"]["b"],
        serde_json::json!({"specifier": "workspace:*", "version": "link:../b"}),
        "…while the importer carries a real link: {second}"
    );
    assert_eq!(
        second["existsNonEmptyWantedLockfile"],
        serde_json::json!(false),
        "and pnpm still calls that lockfile empty: {second}"
    );
}

/// The pnpmfile is pnpm's file, so a project that is not pnpm's does not get
/// one — and there is no longer a flag to point at one either. `--pnpmfile`
/// and `--global-pnpmfile` are gone from pnpm 12; `--ignore-pnpmfile`
/// survives, and is accepted but inert where nothing would have been read.
#[test]
fn a_nub_incumbent_project_reads_no_pnpmfile_and_has_no_flag_to_name_one() {
    let nub_dir = fixture("nub-identity", false);
    std::fs::write(nub_dir.join("custom-hooks.cjs"), RECORDER).unwrap();

    let (stdout, stderr, code) = run(&nub_dir, &["install"]);
    assert_eq!(
        code, 0,
        "nub-identity install\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(
        firings(&nub_dir),
        0,
        "a nub-incumbent project must not read `.pnpmfile.cjs`\n\
         stdout: {stdout}\nstderr: {stderr}"
    );

    // Positive control: the identical fixture differing only by the file that
    // declares pnpm's incumbency DOES run the hook. Without it this test
    // would pass just as well on a build that had stopped loading pnpmfiles
    // altogether.
    let pnpm_dir = fixture("nub-identity-control", true);
    let (stdout, stderr, code) = run(&pnpm_dir, &["install"]);
    assert_eq!(
        code, 0,
        "control install\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(
        firings(&pnpm_dir),
        1,
        "the control must run the hook, or the assertion above means nothing\n\
         stdout: {stdout}\nstderr: {stderr}"
    );

    for dir in [&nub_dir, &pnpm_dir] {
        let (stdout, stderr, code) = run(dir, &["install", "--pnpmfile", "custom-hooks.cjs"]);
        assert_ne!(
            code, 0,
            "--pnpmfile is not a pnpm 12 flag\nstdout: {stdout}"
        );
        assert!(
            stderr.contains("--pnpmfile"),
            "the refusal must name the flag: {stderr}"
        );

        let before = firings(dir);
        let (stdout, stderr, code) = run(dir, &["install", "--ignore-pnpmfile"]);
        assert_eq!(
            code, 0,
            "--ignore-pnpmfile\nstdout: {stdout}\nstderr: {stderr}"
        );
        assert_eq!(
            firings(dir),
            before,
            "--ignore-pnpmfile suppresses the hook under either identity\n\
             stdout: {stdout}\nstderr: {stderr}"
        );
    }
}
