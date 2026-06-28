//! `nub install` / `nub ci` through the embedded aube engine, end-to-end
//! through the binary: real fixtures, real node_modules, real lockfiles.
//! The layout policy and the yarn write gate live in
//! `crates/nub-cli/src/pm_engine.rs`.
//!
//! The two installing tests are `#[ignore]` (network) following the
//! provisioning-test convention — run them via
//! `cargo test -p nub-cli --test install_engine -- --ignored`. They also
//! self-skip when the npm registry is unreachable so an offline `--ignored`
//! sweep doesn't report false failures.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// A unique temp project dir under the system temp root (never under $HOME,
/// so manifest/lockfile walk-ups can't escape into stray ancestors).
fn pm_tmpdir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-install-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Spawn `nub <args>` in `dir` with the aube store/cache isolated to fresh
/// temp roots (XDG_DATA_HOME carries the CAS store, XDG_CACHE_HOME the
/// packument cache) so tests never warm-hit the dev box's real store.
fn run_install(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", pm_tmpdir("xdg-data"))
        .env("XDG_CACHE_HOME", pm_tmpdir("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// Offline guard for the `#[ignore]` network tests: true when the registry
/// answers a TCP connect within 3s.
fn registry_reachable() -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    "registry.npmjs.org:443"
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .is_some_and(|addr| {
            TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3)).is_ok()
        })
}

/// Truly-fresh project (no lockfile, no PM declaration, no pnpm-named file):
/// nub claims identity via the neutral lockfile only. The engine resolves, links
/// the isolated (pnpm-style) layout under `node_modules/.nub`, and writes nub's
/// neutral `lock.yaml` — the quiet identity marker. It must NOT auto-stamp
/// `packageManager` / `devEngines` into `package.json`: that exclusivity claim
/// is reserved for the explicit `nub pm use nub` command.
#[test]
#[ignore = "network: resolves + fetches is-positive@3.1.0 from the npm registry"]
fn install_truly_fresh_project_claims_nub_identity() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("fresh");
    // The impossible `engines.aube` pin proves the embedder toggle: stock
    // aube would warn (or hard-fail under engine-strict) on the mismatch;
    // nub skips the field entirely — its users aren't running that tool.
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"fresh","version":"1.0.0","engines":{"aube":"999.0.0"},"dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();

    let (stdout, stderr, code) = run_install(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !stderr.to_lowercase().contains("engine"),
        "engines.aube must be ignored, not warned about: {stderr}"
    );

    // Isolated layout: the top-level entry is a symlink into the virtual
    // store, which nub relocates to `node_modules/.nub`.
    let dep = dir.join("node_modules/is-positive");
    assert!(
        dep.join("package.json").is_file(),
        "is-positive must be installed: stderr: {stderr}"
    );
    assert!(
        dep.symlink_metadata().unwrap().file_type().is_symlink(),
        "no-lockfile projects default to the isolated layout (symlink into .nub)"
    );
    let target = std::fs::read_link(&dep).unwrap();
    assert!(
        target.to_string_lossy().contains(".nub/"),
        "the virtual store must live under node_modules/.nub, got: {}",
        target.display()
    );
    assert!(
        !dir.join("node_modules/.aube").exists(),
        "no .aube directory may materialize"
    );

    assert!(
        dir.join("lock.yaml").is_file(),
        "truly-fresh install writes nub's neutral lock.yaml"
    );
    assert!(
        !dir.join("pnpm-lock.yaml").exists() && !dir.join("aube-lock.yaml").exists(),
        "neither pnpm-lock.yaml nor aube-lock.yaml may appear on the truly-fresh path"
    );

    // Identity is self-reinforcing via the lockfile alone (the next install
    // sees lock.yaml and resolves as nub-identity). The manifest is left
    // untouched — no `packageManager`, no `devEngines` auto-stamp on a plain
    // install (that exclusivity claim is reserved for `nub pm use nub`).
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert!(
        manifest.get("packageManager").is_none(),
        "a plain install must not auto-stamp packageManager: {manifest}"
    );
    assert!(
        manifest.get("devEngines").is_none(),
        "a plain install must not auto-stamp devEngines: {manifest}"
    );
}

/// `--silent` (and its spellings) quiet the install: nothing on stderr but a
/// fatal error, matching `pnpm install --silent` (#179). The default install,
/// by contrast, prints the dependency summary + the `✓ installed` line. We
/// assert the silent contract (empty stderr, success, deps actually linked) and
/// that the default is NOT empty — so a regression that silences the default,
/// or one that fails to silence `--silent`, both fail.
#[test]
#[ignore = "network: resolves + fetches is-positive@3.1.0 from the npm registry"]
fn install_silent_flag_suppresses_all_nonerror_output() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let manifest = r#"{"name":"q","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#;

    // Baseline: the default install writes a human summary to stderr.
    let base = pm_tmpdir("silent-base");
    std::fs::write(base.join("package.json"), manifest).unwrap();
    let (_, default_stderr, default_code) = run_install(&base, &["install"]);
    assert_eq!(default_code, 0, "default install failed: {default_stderr}");
    assert!(
        !default_stderr.trim().is_empty(),
        "the default install should print a summary on stderr (guards against \
         over-silencing): got empty output"
    );

    // Every silent spelling produces empty stderr while still linking the dep —
    // both AFTER the verb (per-verb clap surface) and BEFORE it (the pre-verb
    // global position, recorded as a process default in cli::dispatch).
    for form in [
        &["install", "--silent"][..],
        &["install", "-s"][..],
        &["install", "--reporter=silent"][..],
        &["install", "--loglevel=silent"][..],
        &["--silent", "install"][..],
        &["-s", "install"][..],
        &["--reporter=silent", "install"][..],
        &["--loglevel=silent", "install"][..],
    ] {
        let dir = pm_tmpdir(&format!("silent-{}", form.join("-").replace('=', "")));
        std::fs::write(dir.join("package.json"), manifest).unwrap();
        let (stdout, stderr, code) = run_install(&dir, form);
        assert_eq!(code, 0, "nub {form:?} failed: {stdout}\n{stderr}");
        assert!(
            stderr.is_empty(),
            "nub {form:?} must write nothing to stderr, got: {stderr:?}"
        );
        assert!(
            dir.join("node_modules/is-positive/package.json").is_file(),
            "nub {form:?} still installs the dependency"
        );
    }
}

/// Regression (non-network): a PRE-verb `--reporter`/`--loglevel`/`--silent`
/// reaches the PM verb instead of falling through the dispatch scan to the file
/// runner — which shipped it to Node as `node: bad option: --reporter=silent`
/// before the fix. A no-dependency manifest installs fully offline, so the only
/// thing under test is that the global form parses and dispatches to `install`.
#[test]
fn pre_verb_output_flags_reach_install_not_node() {
    for form in [
        &["--reporter=silent", "install"][..],
        &["--loglevel=error", "install"][..],
        &["--silent", "install"][..],
    ] {
        let dir = pm_tmpdir(&format!("preverb-{}", form.join("-").replace('=', "")));
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"q","version":"1.0.0"}"#,
        )
        .unwrap();
        let (stdout, stderr, code) = run_install(&dir, form);
        let combined = format!("{stdout}\n{stderr}");
        assert!(
            !combined.contains("bad option") && !combined.contains("is not a nub command"),
            "nub {form:?} misrouted instead of dispatching to install: {combined}"
        );
        assert_eq!(
            code, 0,
            "nub {form:?} (no deps) should install cleanly offline: {combined}"
        );
    }
}

/// Precedence (non-network): a per-verb `--reporter` overrides a PRE-verb
/// `--silent`. `--silent` folds into the same `--reporter=silent` process
/// default as a pre-verb `--reporter`, so the "per-verb always wins" invariant
/// holds for every pre-verb spelling — a pre-verb global is only a fallback.
#[test]
fn per_verb_reporter_overrides_pre_verb_silent() {
    let manifest = r#"{"name":"q","version":"1.0.0"}"#;

    // Pre-verb --silent alone silences (empty stderr).
    let quiet = pm_tmpdir("prec-quiet");
    std::fs::write(quiet.join("package.json"), manifest).unwrap();
    let (_, s1, c1) = run_install(&quiet, &["--silent", "install"]);
    assert_eq!(c1, 0, "pre-verb --silent install failed: {s1}");
    assert!(
        s1.is_empty(),
        "pre-verb --silent should silence, got: {s1:?}"
    );

    // A per-verb --reporter=default un-silences it: per-verb wins over the
    // pre-verb default.
    let loud = pm_tmpdir("prec-loud");
    std::fs::write(loud.join("package.json"), manifest).unwrap();
    let (_, s2, c2) = run_install(&loud, &["--silent", "install", "--reporter=default"]);
    assert_eq!(c2, 0, "install failed: {s2}");
    assert!(
        !s2.trim().is_empty(),
        "per-verb --reporter=default must override pre-verb --silent: got empty stderr"
    );
}

/// A `pnpm-workspace.yaml` with no lockfile is a genuine pnpm signal, NOT a
/// truly-fresh project: nub stays pnpm-shaped — writes `pnpm-lock.yaml` and
/// does NOT stamp the manifest.
#[test]
#[ignore = "network: resolves + fetches is-positive@3.1.0 from the npm registry"]
fn install_with_pnpm_workspace_stays_pnpm_shaped_no_stamp() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("pnpm-ws");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"pnpmws","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("pnpm-workspace.yaml"), "packages: []\n").unwrap();

    let (stdout, stderr, code) = run_install(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("pnpm-lock.yaml").is_file(),
        "a pnpm-workspace.yaml project writes pnpm-lock.yaml"
    );
    assert!(
        !dir.join("lock.yaml").exists(),
        "a pnpm-incumbent project must not get nub's lock.yaml"
    );
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert!(
        manifest.get("packageManager").is_none(),
        "a pnpm-incumbent project must not be stamped: {manifest}"
    );
}

/// A project with a (frozen-satisfiable) package-lock.json: the layout policy
/// defaults to the hoisted (npm-style) layout, and the lockfile format is
/// preserved — no aube-lock.yaml appears next to package-lock.json.
#[test]
#[ignore = "network: fetches is-positive@3.1.0 (resolution comes from the lockfile)"]
fn install_with_package_lock_hoists_and_preserves_the_npm_lockfile() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("npmlock");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"npmlock","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();
    // In-sync npm v3 lockfile for is-positive@3.1.0 (integrity is the
    // published registry value — stable forever for a published version).
    let package_lock = r#"{
  "name": "npmlock",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "npmlock",
      "version": "1.0.0",
      "dependencies": { "is-positive": "3.1.0" }
    },
    "node_modules/is-positive": {
      "version": "3.1.0",
      "resolved": "https://registry.npmjs.org/is-positive/-/is-positive-3.1.0.tgz",
      "integrity": "sha512-8ND1j3y9/HP94TOvGzr69/FgbkX2ruOldhLEsTWwcJVfo4oRjwemJmJxt7RJkKYH8tz7vYBP9JcKQY8CLuJ90Q==",
      "engines": { "node": ">=0.10.0" }
    }
  }
}
"#;
    std::fs::write(dir.join("package-lock.json"), package_lock).unwrap();

    let (stdout, stderr, code) = run_install(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let dep = dir.join("node_modules/is-positive");
    assert!(
        dep.join("package.json").is_file(),
        "is-positive must be installed: stderr: {stderr}"
    );
    assert!(
        !dep.symlink_metadata().unwrap().file_type().is_symlink(),
        "package-lock projects default to the hoisted layout (real dir, not a symlink)"
    );
    assert!(
        dir.join("package-lock.json").is_file(),
        "the npm lockfile must be preserved"
    );
    assert!(
        !dir.join("aube-lock.yaml").exists() && !dir.join("pnpm-lock.yaml").exists(),
        "no foreign lockfile may appear next to package-lock.json"
    );
}

#[test]
#[ignore = "network: fetches commander@5.1.0 and commander@12.1.0 from a package-lock v3 workspace"]
fn ci_with_package_lock_keeps_workspace_local_conflicting_dep() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("npmlock-workspace-conflict");
    std::fs::create_dir_all(dir.join("packages/cli")).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"npm-workspace-conflict","version":"1.0.0","private":true,"packageManager":"npm@11.13.0","workspaces":["packages/*"],"dependencies":{"commander":"^5.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("packages/cli/package.json"),
        r#"{"name":"tempo","version":"1.0.0","dependencies":{"commander":"^12.1.0"}}"#,
    )
    .unwrap();
    let package_lock = r#"{
  "name": "npm-workspace-conflict",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "npm-workspace-conflict",
      "version": "1.0.0",
      "workspaces": ["packages/*"],
      "dependencies": { "commander": "^5.0.0" }
    },
    "node_modules/commander": {
      "version": "5.1.0",
      "resolved": "https://registry.npmjs.org/commander/-/commander-5.1.0.tgz",
      "integrity": "sha512-P0CysNDQ7rtVw4QIQtm+MRxV66vKFSvlsQvGYXZWR3qFU0jlMKHZZZgw8e+8DSah4UDKMqnknRDQz+xuQXQ/Zg==",
      "license": "MIT",
      "engines": { "node": ">= 6" }
    },
    "node_modules/tempo": {
      "resolved": "packages/cli",
      "link": true
    },
    "packages/cli": {
      "name": "tempo",
      "version": "1.0.0",
      "dependencies": { "commander": "^12.1.0" }
    },
    "packages/cli/node_modules/commander": {
      "version": "12.1.0",
      "resolved": "https://registry.npmjs.org/commander/-/commander-12.1.0.tgz",
      "integrity": "sha512-Vw8qHK3bZM9y/P10u3Vib8o/DdkvA2OtPtZvD871QKjy74Wj1WSKFILMPRPSdUSx5RFK1arlJzEtA4PkFgnbuA==",
      "license": "MIT",
      "engines": { "node": ">=18" }
    }
  }
}
"#;
    std::fs::write(dir.join("package-lock.json"), package_lock).unwrap();

    let (stdout, stderr, code) = run_install(&dir, &["ci"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let root_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("node_modules/commander/package.json")).unwrap(),
    )
    .unwrap();
    let workspace_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("packages/cli/node_modules/commander/package.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(root_manifest["version"].as_str(), Some("5.1.0"));
    assert_eq!(workspace_manifest["version"].as_str(), Some("12.1.0"));
}

/// The yarn write gate, both trigger paths — no network either way:
/// a drifted yarn.lock is refused at pre-flight (before any resolution), and
/// `--no-frozen-lockfile` (an explicit "rewrite the lockfile" request) is
/// refused upfront. yarn.lock must be byte-identical afterwards.
#[test]
fn install_refuses_to_mutate_a_drifted_yarn_lock() {
    let dir = pm_tmpdir("yarngate");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"yarngate","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();
    // Valid yarn-classic lockfile that does NOT satisfy the manifest
    // (only left-pad) — installing would require a re-resolve + rewrite.
    let yarn_lock = "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n\
                     # yarn lockfile v1\n\n\n\
                     left-pad@^1.3.0:\n\
                     \x20\x20version \"1.3.0\"\n\
                     \x20\x20resolved \"https://registry.yarnpkg.com/left-pad/-/left-pad-1.3.0.tgz#5b8a3a7765dfe001261dde915589e782f8c94d1e\"\n\
                     \x20\x20integrity sha512-XI5MPzVNApjAyhQzphX8BkmKsKUxD4LdyK24iZeQGinBN9yTQT3bFlCBy/aVx2HrNcqQGsdot8ghrjyrvMCoEA==\n";
    std::fs::write(dir.join("yarn.lock"), yarn_lock).unwrap();

    // Drifted lockfile → the gate, with the drift reason and the remedy.
    let (_, stderr, code) = run_install(&dir, &["install"]);
    assert_ne!(code, 0, "a drifted yarn.lock must be refused: {stderr}");
    assert!(
        stderr.contains("refusing to modify yarn.lock") && stderr.contains("yarn install"),
        "the gate must name the refusal and the yarn remedy: {stderr}"
    );
    assert!(
        !dir.join("node_modules/is-positive").exists(),
        "nothing may be installed past the gate"
    );

    // Explicit rewrite request → refused upfront, same gate.
    let (_, stderr2, code2) = run_install(&dir, &["install", "--no-frozen-lockfile"]);
    assert_ne!(code2, 0, "--no-frozen-lockfile must be refused: {stderr2}");
    assert!(
        stderr2.contains("refusing to modify yarn.lock"),
        "the explicit-rewrite path must hit the same gate: {stderr2}"
    );

    assert_eq!(
        std::fs::read_to_string(dir.join("yarn.lock")).unwrap(),
        yarn_lock,
        "yarn.lock must be byte-identical after refused installs"
    );
    assert!(
        !dir.join("aube-lock.yaml").exists(),
        "the gate must not leave an aube-lock.yaml behind"
    );
}

/// A truly-fresh `nub add` claims nub identity exactly like a fresh `install`:
/// the add resolves + writes nub's neutral `lock.yaml` — the quiet identity
/// marker — and adds the dep. It must NOT auto-stamp `packageManager` /
/// `devEngines` into `package.json`; that exclusivity claim is reserved for the
/// explicit `nub pm use nub` command. Identity self-reinforces via the lockfile.
#[test]
#[ignore = "network: resolves + fetches is-positive@3.1.0 from the npm registry"]
fn add_on_a_truly_fresh_project_claims_nub_identity() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("fresh-add");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"fresh-add","version":"1.0.0"}"#,
    )
    .unwrap();

    let (stdout, stderr, code) = run_install(&dir, &["add", "is-positive@3.1.0"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    assert!(
        dir.join("lock.yaml").is_file(),
        "a truly-fresh add writes nub's neutral lock.yaml: {stderr}"
    );
    assert!(
        !dir.join("pnpm-lock.yaml").exists() && !dir.join("aube-lock.yaml").exists(),
        "neither pnpm-lock.yaml nor aube-lock.yaml may appear on the truly-fresh path"
    );

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert!(
        manifest.get("packageManager").is_none(),
        "a truly-fresh add must not auto-stamp packageManager: {manifest}"
    );
    assert!(
        manifest.get("devEngines").is_none(),
        "a truly-fresh add must not auto-stamp devEngines: {manifest}"
    );
    assert_eq!(
        manifest["dependencies"]["is-positive"].as_str(),
        Some("3.1.0"),
        "the added dep must land in dependencies: {manifest}"
    );
}

/// The yarn `yarn-offline-mirror` fail-loud gate fires only for STRICT offline.
/// `--offline` (yarn `enableNetwork:false` / Berry `--offline`) aborts upfront —
/// nub can't read a configured mirror directory, so silently hitting the registry
/// would diverge. `--prefer-offline` PERMITS network fallback, so it is not strict
/// offline and must pass the mirror preflight (it then hits the ordinary yarn
/// write-gate, never the mirror fatal). No network: both paths fail before any
/// fetch, so this test needs no registry.
#[test]
fn prefer_offline_does_not_trip_the_yarn_offline_mirror_fatal() {
    let dir = pm_tmpdir("mirror");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"mirror","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();
    // A yarn project (yarn.lock present) with a classic-yarnrc offline mirror.
    std::fs::write(
        dir.join("yarn.lock"),
        "# yarn lockfile v1\n\n\nis-positive@3.1.0:\n  version \"3.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".yarnrc"),
        "yarn-offline-mirror \"./npm-packages-offline-cache\"\n",
    )
    .unwrap();

    const MIRROR_FATAL: &str = "yarn-offline-mirror";

    // Strict --offline → the mirror fatal fires.
    let (_, stderr_strict, code_strict) = run_install(&dir, &["install", "--offline"]);
    assert_ne!(
        code_strict, 0,
        "strict --offline + a configured mirror must abort: {stderr_strict}"
    );
    assert!(
        stderr_strict.contains(MIRROR_FATAL),
        "strict --offline must surface the offline-mirror fatal: {stderr_strict}"
    );

    // --prefer-offline → past the mirror preflight (it permits network fallback).
    // It then hits the ordinary yarn write-gate, NOT the mirror fatal.
    let (_, stderr_prefer, code_prefer) = run_install(&dir, &["install", "--prefer-offline"]);
    assert!(
        !stderr_prefer.contains(MIRROR_FATAL),
        "--prefer-offline must NOT trip the offline-mirror fatal: {stderr_prefer}"
    );
    // Whatever it does next, it didn't abort over the mirror — code is governed
    // by the yarn gate / install path, never the mirror preflight.
    let _ = code_prefer;
}

/// Workspace-member linking under a YARN incumbent. A classic yarn.lock never
/// records workspace members (they aren't registry packages), so a member that
/// depends on a SIBLING member has no resolution entry. nub must still symlink
/// the sibling into the consumer's node_modules so it resolves — matching what
/// reference yarn does (and what nub already does under pnpm/npm incumbents).
///
/// No network: every dep is a local member, the yarn.lock is empty, and the
/// install is a frozen read (yarn.lock left byte-identical). Before the fix
/// nub printed "✓ Already up to date" and linked nothing, so `@x/app` could not
/// resolve `@x/utils`. The differential reference (real yarn 1.x links both)
/// lives in `.fray/yarn-workspace-member-linking.md`; this guards the nub side.
#[test]
fn install_links_yarn_workspace_member_into_consumer() {
    let dir = pm_tmpdir("yarn-ws-link");
    // Root: a yarn incumbent (packageManager + an on-disk yarn.lock), workspaces.
    std::fs::write(
        dir.join("package.json"),
        r#"{ "name": "root", "private": true, "packageManager": "yarn@1.13.0", "workspaces": ["packages/*"] }"#,
    )
    .unwrap();
    // An empty-but-valid classic yarn.lock — the members are the only deps, so
    // it genuinely satisfies the manifest (no drift, no write).
    std::fs::write(dir.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();
    for (member, body, index) in [
        (
            "utils",
            r#"{ "name": "@x/utils", "version": "1.0.0", "main": "index.js" }"#,
            "module.exports = 'utils-ok';",
        ),
        (
            "app",
            r#"{ "name": "@x/app", "version": "1.0.0", "dependencies": { "@x/utils": "1.0.0" } }"#,
            "console.log(require('@x/utils'));",
        ),
    ] {
        let mdir = dir.join("packages").join(member);
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("package.json"), body).unwrap();
        std::fs::write(mdir.join("index.js"), index).unwrap();
    }

    let (stdout, stderr, code) = run_install(&dir, &["install"]);
    assert_eq!(code, 0, "install must succeed: {stdout}{stderr}");

    // The sibling must be linked where node resolves it from the consumer.
    // nub's isolated linker hoists into the consumer's own node_modules
    // (`packages/app/node_modules/@x/utils`), the same shape it produces under a
    // pnpm incumbent; reference yarn hoists to the top level. Either resolves —
    // assert the resolution outcome, the contract, not the exact hoist site.
    let app = dir.join("packages").join("app");
    let resolved = Command::new("node")
        .args(["-e", "process.stdout.write(require.resolve('@x/utils'))"])
        .current_dir(&app)
        .output()
        .expect("failed to spawn node");
    assert!(
        resolved.status.success(),
        "`@x/utils` must resolve from `@x/app` after install — it did not.\n\
         stdout: {}\nstderr: {}\ninstall said: {stdout}{stderr}",
        String::from_utf8_lossy(&resolved.stdout),
        String::from_utf8_lossy(&resolved.stderr),
    );
    // And it must resolve to the local member, not a stray copy.
    let resolved_path = String::from_utf8_lossy(&resolved.stdout);
    assert!(
        std::fs::canonicalize(resolved_path.trim()).unwrap()
            == std::fs::canonicalize(dir.join("packages/utils/index.js")).unwrap(),
        "`@x/utils` must resolve to the local member, got: {resolved_path}"
    );

    // yarn.lock is read-only — the install must not have rewritten it.
    assert_eq!(
        std::fs::read_to_string(dir.join("yarn.lock")).unwrap(),
        "# yarn lockfile v1\n",
        "yarn.lock must be byte-identical after a read-only workspace install"
    );
}

/// Run `nub <args>` in `dir` against a CALLER-OWNED store/cache so a second
/// install warm-hits the first's node_modules + CAS — the realistic
/// warm-satisfied loop, unlike [`run_install`] which isolates a fresh store
/// per call.
fn run_install_in_store(dir: &Path, store: &Path, cache: &Path, args: &[&str]) -> (String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", store)
        .env("XDG_CACHE_HOME", cache)
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// A second `nub install` on an unchanged, fully-satisfied tree short-circuits
/// to the instant "Already up to date" exit — even online under the default
/// `trustPolicy=no-downgrade`. Before the fix the trust posture disabled the
/// warm short-circuit on any online install (gated in `install_fast_path_eligible`
/// before `check_needs_install` ran), so the second install re-ran the full
/// resolve/fetch/link pipeline every time; nub's `warm_trust_revalidate=false`
/// profile now lets a no-op skip the redundant re-validation. The short-circuit
/// is the load-independent signal: `emit_up_to_date` fires ONLY on the fast path,
/// and it does so here with a fresh (empty) packument cache, proving nothing was
/// re-resolved or re-fetched. The security half (real work still trips the gate)
/// is covered by `frozen_install_with_trust_downgrade_still_aborts`.
#[test]
#[ignore = "network: resolves + fetches is-positive@3.1.0 from the npm registry"]
fn warm_satisfied_install_short_circuits_under_no_downgrade() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("warm-satisfied");
    let store = pm_tmpdir("warm-store");
    let cache = pm_tmpdir("warm-cache");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"warm","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();

    // Cold install populates node_modules + the freshness state sidecar.
    let (cold_stderr, cold_code) = run_install_in_store(&dir, &store, &cache, &["install"]);
    assert_eq!(cold_code, 0, "cold install must succeed: {cold_stderr}");
    assert!(
        dir.join("node_modules/is-positive/package.json").is_file(),
        "is-positive must be installed by the cold pass: {cold_stderr}"
    );
    assert!(
        !cold_stderr.contains("Already up to date"),
        "the COLD install must NOT report up-to-date (it did real work): {cold_stderr}"
    );

    // Second install, online, default trust posture — must short-circuit.
    let (warm_stderr, warm_code) = run_install_in_store(&dir, &store, &cache, &["install"]);
    assert_eq!(warm_code, 0, "warm install must succeed: {warm_stderr}");
    assert!(
        warm_stderr.contains("Already up to date"),
        "a warm-satisfied online install must short-circuit to 'Already up to date' \
         under the default trustPolicy=no-downgrade, got: {warm_stderr}"
    );
}

/// SECURITY INVARIANT: the warm short-circuit must NOT weaken the trust gate on
/// an install that does real work. A fresh install of a package whose picked
/// version dropped the trust evidence an earlier version carried
/// (`node-gyp@10.3.0` lost the provenance attestation `10.3.1` had) is real work
/// — `check_needs_install` returns `Some`, so the fast path is bypassed and the
/// full pipeline runs, where `trustPolicy=no-downgrade` aborts during
/// resolution. The short-circuit is reachable only on a no-op, never on this.
/// (Depends on `node-gyp@10.3.0`'s live registry provenance metadata staying a
/// downgrade vs `10.3.1`; the canonical case is recorded in
/// `.fray/install-warm-fastpath-trust-gate.md`.)
#[test]
#[ignore = "network: resolves node-gyp@10.3.0 from the npm registry to assert the trust-downgrade abort"]
fn frozen_install_with_trust_downgrade_still_aborts() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("trust-downgrade");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"dg","version":"1.0.0","dependencies":{"node-gyp":"10.3.0"}}"#,
    )
    .unwrap();

    let (stdout, stderr, code) = run_install(&dir, &["install"]);
    assert_eq!(
        code, 23,
        "a trust-downgrade install must abort with the trust exit code (23), got {code}: \
         {stdout}{stderr}"
    );
    assert!(
        stderr.contains("ERR_NUB_TRUST_DOWNGRADE"),
        "the abort must carry the trust-downgrade code: {stderr}"
    );
    assert!(
        !dir.join("node_modules/node-gyp").exists(),
        "no package may be linked when the trust gate aborts resolution"
    );
}
