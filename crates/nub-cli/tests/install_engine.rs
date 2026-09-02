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

/// Like [`run_install`], but with `CI=true` in the environment so the
/// install hits nub's CI-aware frozen-mode auto-default.
fn run_install_ci(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("CI", "true")
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

#[test]
fn install_dir_initializes_one_project_snapshot_from_final_cwd() {
    let outer = pm_tmpdir("dir-snapshot-outer");
    let target = outer.join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(
        target.join("package.json"),
        r#"{"name":"target","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::write(target.join("nub.jsonc"), r#"{ "conditions": [] }"#).unwrap();

    // `--dir` and its `-C` alias are the same verb-local chdir.
    for flag in ["--dir", "-C"] {
        let log = outer.join(format!("snapshot-{}.log", flag.trim_start_matches('-')));
        let output = Command::new(nub_binary())
            .args(["install", flag, "target", "--lockfile-only", "--offline"])
            .current_dir(&outer)
            .env("XDG_DATA_HOME", pm_tmpdir("dir-snapshot-data"))
            .env("XDG_CACHE_HOME", pm_tmpdir("dir-snapshot-cache"))
            .env("__NUB_TEST_CONFIG_SNAPSHOT_LOG", &log)
            .output()
            .expect("run install with verb-local cwd");
        assert_eq!(
            output.status.code(),
            Some(0),
            "install {flag} should succeed; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let lines: Vec<_> = std::fs::read_to_string(&log)
            .expect("snapshot log")
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "install {flag}: one snapshot init: {lines:?}"
        );
        let logged = lines[0]
            .strip_prefix("cwd=")
            .and_then(|rest| rest.strip_suffix(" project=loaded"))
            .unwrap_or_else(|| panic!("unexpected snapshot log line: {}", lines[0]));
        // Compare resolved directories, not spellings. Windows hands the child
        // whatever form the environment carried — an 8.3 short name under a
        // RUNNER~1 home — while `canonicalize` yields the extended-length `\\?\`
        // form. Both name the same directory, and the contract under test is
        // which directory the snapshot resolved from.
        assert_eq!(
            Path::new(logged).canonicalize().expect("logged cwd exists"),
            target.canonicalize().expect("target exists"),
            "install {flag}: snapshot must resolve from the verb-local cwd"
        );
    }
}

/// Truly-fresh project (no lockfile, no PM declaration, no pnpm-named file):
/// nub claims identity via the neutral lockfile only. The engine resolves, links
/// the isolated (pnpm-style) layout under `node_modules/.store`, and writes nub's
/// neutral `nub.lock` — the quiet identity marker. It must NOT auto-stamp
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
    // store, which nub relocates to `node_modules/.store`.
    let dep = dir.join("node_modules/is-positive");
    assert!(
        dep.join("package.json").is_file(),
        "is-positive must be installed: stderr: {stderr}"
    );
    assert!(
        dep.symlink_metadata().unwrap().file_type().is_symlink(),
        "no-lockfile projects default to the isolated layout (symlink into .store)"
    );
    let target = std::fs::read_link(&dep).unwrap();
    assert!(
        target.to_string_lossy().contains(".store/"),
        "the virtual store must live under node_modules/.store, got: {}",
        target.display()
    );
    assert!(
        !dir.join("node_modules/.aube").exists(),
        "no .aube directory may materialize"
    );

    // A patch-free install writes no applied-patches sidecar (an empty `{}`
    // manifest is information-free clutter; a missing file reads back the same).
    assert!(
        !dir.join("node_modules/.nub-applied-patches.json").exists(),
        "a patch-free install must not write an empty applied-patches sidecar"
    );

    assert!(
        dir.join("nub.lock").is_file(),
        "truly-fresh install writes nub's neutral nub.lock"
    );
    assert!(
        !dir.join("pnpm-lock.yaml").exists() && !dir.join("aube-lock.yaml").exists(),
        "neither pnpm-lock.yaml nor aube-lock.yaml may appear on the truly-fresh path"
    );

    // A virgin install stamps a caret RANGE into `devEngines.packageManager`
    // (the non-locking PM signal nub's neutral nub.lock withholds) — never
    // the hard, corepack-visible `packageManager: nub@<v>` pin, which stays the
    // opt-in of an explicit `nub pm use nub@<exact>`. Identity is also
    // self-reinforcing via the lockfile: the next install sees nub.lock and is
    // no longer virgin, so it never re-stamps.
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(
        manifest.pointer("/devEngines/packageManager"),
        Some(&serde_json::json!({
            "name": "nub",
            "version": concat!("^", env!("CARGO_PKG_VERSION")),
            "onFail": "ignore"
        })),
        "a virgin install stamps a devEngines.packageManager caret range: {manifest}"
    );
    assert!(
        manifest.get("packageManager").is_none(),
        "the virgin stamp writes only the devEngines range, never the exact packageManager pin: {manifest}"
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

/// Tightening `minimumReleaseAge` invalidates the warm install state, but that
/// miss must also make a normal prefer-frozen install revalidate the versions
/// pinned in the existing lockfile. Before the fix, bare `nub install` trusted
/// the lockfile and merely rewrote the new settings hash while `--force`
/// correctly failed the same pinned version under the hard age gate.
#[test]
#[ignore = "network: resolves is-number@7 and revalidates its publish time"]
fn release_age_policy_drift_bare_install_matches_force_validation() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }

    let dir = pm_tmpdir("release-age-drift");
    let home = pm_tmpdir("release-age-home");
    let data = pm_tmpdir("release-age-data");
    let cache = pm_tmpdir("release-age-cache");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"release-age-drift","version":"1.0.0","dependencies":{"is-number":"7"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join(".npmrc"), "minimum-release-age=0\n").unwrap();

    let run = |args: &[&str]| {
        Command::new(nub_binary())
            .args(args)
            .current_dir(&dir)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CACHE_HOME", &cache)
            .env_remove("CI")
            .output()
            .expect("run release-age install")
    };

    let baseline = run(&["install"]);
    assert_eq!(
        baseline.status.code(),
        Some(0),
        "baseline install failed: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );

    let state_path = dir.join("node_modules/.store/.nub-state/fresh.json");
    let state_before = std::fs::read(&state_path).expect("baseline freshness state");
    std::fs::write(dir.join(".npmrc"), "minimum-release-age=10069920\n").unwrap();

    let lockfile_only = run(&["install", "--lockfile-only"]);
    let lockfile_only_stderr = String::from_utf8_lossy(&lockfile_only.stderr);
    assert_eq!(
        lockfile_only.status.code(),
        Some(21),
        "lockfile-only stderr: {lockfile_only_stderr}"
    );
    assert!(
        lockfile_only_stderr.contains("ERR_NUB_NO_MATURE_MATCHING_VERSION"),
        "lockfile-only must enforce the tightened policy: {lockfile_only_stderr}"
    );
    assert_eq!(
        std::fs::read(&state_path).expect("freshness state after failed lockfile-only install"),
        state_before,
        "failed lockfile-only validation must not bless the new settings hash"
    );

    let bare = run(&["install"]);
    let bare_stderr = String::from_utf8_lossy(&bare.stderr);
    assert_eq!(bare.status.code(), Some(21), "bare stderr: {bare_stderr}");
    assert!(
        bare_stderr.contains("ERR_NUB_NO_MATURE_MATCHING_VERSION"),
        "bare install must enforce the tightened policy: {bare_stderr}"
    );
    assert_eq!(
        std::fs::read(&state_path).expect("freshness state after failed install"),
        state_before,
        "a failed policy revalidation must not bless the new settings hash"
    );

    let forced = run(&["install", "--force"]);
    let forced_stderr = String::from_utf8_lossy(&forced.stderr);
    assert_eq!(
        forced.status.code(),
        Some(21),
        "forced stderr: {forced_stderr}"
    );
    assert!(
        forced_stderr.contains("ERR_NUB_NO_MATURE_MATCHING_VERSION"),
        "bare and forced installs must enforce the same policy: {forced_stderr}"
    );
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
        !dir.join("nub.lock").exists(),
        "a pnpm-incumbent project must not get nub's nub.lock"
    );
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert!(
        manifest.get("packageManager").is_none(),
        "a pnpm-incumbent project must not be stamped: {manifest}"
    );
}

/// A project with a (frozen-satisfiable) package-lock.json: the layout policy
/// defaults to the isolated layout (the GVS flip — npm/yarn/bun incumbents no
/// longer force hoisted), and the lockfile format is preserved — no
/// aube-lock.yaml appears next to package-lock.json.
#[test]
#[ignore = "network: fetches is-positive@3.1.0 (resolution comes from the lockfile)"]
fn install_with_package_lock_isolates_and_preserves_the_npm_lockfile() {
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
    // npm/yarn/bun incumbents now default to the isolated layout (the GVS flip):
    // a declared dep is a top-level SYMLINK into the `.store` virtual store, not a
    // real directory. (GVS engagement itself is off-CI-gated, but the isolated
    // symlink layout holds regardless.)
    assert!(
        dep.symlink_metadata().unwrap().file_type().is_symlink(),
        "package-lock projects default to the isolated layout (a symlink into .store)"
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

#[test]
fn frozen_yarn_berry_installs_reject_a_drifted_workspace_member() {
    let fixture = |tag: &str| {
        let dir = pm_tmpdir(tag);
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"berry-drift-root","private":true,"packageManager":"yarn@4.17.0","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        std::fs::write(dir.join(".yarnrc.yml"), "nodeLinker: node-modules\n").unwrap();
        for (member, manifest) in [
            (
                "app",
                r#"{"name":"@fixture/app","version":"1.0.0","dependencies":{"@fixture/utils":"workspace:*","is-odd":"3.0.1"}}"#,
            ),
            ("utils", r#"{"name":"@fixture/utils","version":"1.0.0"}"#),
        ] {
            let member_dir = dir.join("packages").join(member);
            std::fs::create_dir_all(&member_dir).unwrap();
            std::fs::write(member_dir.join("package.json"), manifest).unwrap();
        }
        let yarn_lock = r#"__metadata:
  version: 10
  cacheKey: 10c0

"@fixture/app@workspace:packages/app":
  version: 0.0.0-use.local
  resolution: "@fixture/app@workspace:packages/app"
  dependencies:
    "@fixture/utils": "workspace:*"
  languageName: unknown
  linkType: soft

"@fixture/utils@workspace:*, @fixture/utils@workspace:packages/utils":
  version: 0.0.0-use.local
  resolution: "@fixture/utils@workspace:packages/utils"
  languageName: unknown
  linkType: soft

"berry-drift-root@workspace:.":
  version: 0.0.0-use.local
  resolution: "berry-drift-root@workspace:."
  languageName: unknown
  linkType: soft
"#;
        std::fs::write(dir.join("yarn.lock"), yarn_lock).unwrap();
        (dir, yarn_lock)
    };

    for (tag, args) in [
        (
            "berry-member-drift-install",
            &["install", "--frozen-lockfile", "--ignore-scripts"][..],
        ),
        ("berry-member-drift-ci", &["ci", "--ignore-scripts"][..]),
    ] {
        let (dir, yarn_lock) = fixture(tag);
        let (_, stderr, code) = run_install(&dir, args);
        assert_ne!(
            code, 0,
            "{args:?} must reject member manifest drift: {stderr}"
        );
        assert!(
            stderr.contains("packages/app: is-odd@3.0.1 is not satisfied by yarn.lock"),
            "the failure must identify the drifted member dependency: {stderr}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("yarn.lock")).unwrap(),
            yarn_lock,
            "{args:?} must leave yarn.lock byte-identical"
        );
        assert!(
            !dir.join("node_modules").exists() && !dir.join("packages/app/node_modules").exists(),
            "{args:?} must fail before linking any dependency"
        );
    }
}

/// pnpm parity (`opts.ci && !opts.lockfileOnly`): under `CI=true` nub
/// auto-selects frozen mode for a plain install, but a `--lockfile-only`
/// run is exempt — it exists to regenerate the lock, so it re-resolves a
/// drifted manifest and rewrites the lock instead of erroring. Regression
/// for the CI-frozen-default swallowing `--lockfile-only`. The contrast
/// arm proves the auto-default is unchanged for a non-lockfile-only run.
#[test]
#[ignore = "network: resolves is-positive@{1.0.0,3.1.0} from the npm registry"]
fn ci_lockfile_only_regenerates_a_drifted_lock() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }

    // Seed a project whose nub.lock pins is-positive@1.0.0, then bump the
    // manifest to 3.1.0 so the lock is drifted (stale) relative to it.
    let seed = |tag: &str| -> PathBuf {
        let dir = pm_tmpdir(tag);
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"drift","version":"1.0.0","dependencies":{"is-positive":"1.0.0"}}"#,
        )
        .unwrap();
        let (out, err, code) = run_install(&dir, &["install"]);
        assert_eq!(code, 0, "seed install must succeed: {out}\n{err}");
        assert!(
            dir.join("nub.lock").is_file(),
            "seed writes nub.lock: {err}"
        );
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"drift","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
        )
        .unwrap();
        dir
    };

    // `--lockfile-only` under CI: exempt from the frozen auto-default, so
    // it re-resolves the bumped 3.1.0 spec and rewrites the lock, rc=0.
    let dir = seed("lockonly");
    let (out, err, code) = run_install_ci(&dir, &["install", "--lockfile-only"]);
    assert_eq!(
        code, 0,
        "CI=true install --lockfile-only must regenerate a drifted lock, not error: {out}\n{err}"
    );
    let lock = std::fs::read_to_string(dir.join("nub.lock")).unwrap();
    assert!(
        lock.contains("3.1.0"),
        "the lock must be re-resolved to the bumped 3.1.0 spec: {lock}"
    );

    // Contrast: a plain install under CI stays frozen and rejects the same
    // drift — the auto-default is unchanged for non-lockfile-only runs.
    let dir2 = seed("plain");
    let (_out2, err2, code2) = run_install_ci(&dir2, &["install"]);
    assert_ne!(
        code2, 0,
        "CI=true plain install must still auto-freeze and reject a drifted lock: {err2}"
    );
    // Pin the failure to the frozen-drift path (not an unrelated network/store
    // error), so the contrast can't pass vacuously.
    assert!(
        err2.contains("ERR_NUB_OUTDATED_LOCKFILE"),
        "the rejection must be the frozen outdated-lockfile error: {err2}"
    );
}

/// A truly-fresh `nub add` claims nub identity exactly like a fresh `install`:
/// the add resolves + writes nub's neutral `nub.lock` and adds the dep, and —
/// because the project is virgin (nub is the first PM to touch it) — stamps the
/// non-locking `devEngines.packageManager` caret range. Never the exact
/// `packageManager: nub@<v>` pin (that is `nub pm use nub@<exact>`'s opt-in).
/// This is the common case the stamp targets: `nub add <pkg>` as the first
/// command on a fresh project.
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
        dir.join("nub.lock").is_file(),
        "a truly-fresh add writes nub's neutral nub.lock: {stderr}"
    );
    assert!(
        !dir.join("pnpm-lock.yaml").exists() && !dir.join("aube-lock.yaml").exists(),
        "neither pnpm-lock.yaml nor aube-lock.yaml may appear on the truly-fresh path"
    );

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(
        manifest.pointer("/devEngines/packageManager"),
        Some(&serde_json::json!({
            "name": "nub",
            "version": concat!("^", env!("CARGO_PKG_VERSION")),
            "onFail": "ignore"
        })),
        "a virgin add stamps a devEngines.packageManager caret range: {manifest}"
    );
    assert!(
        manifest.get("packageManager").is_none(),
        "the virgin stamp writes only the devEngines range, never the exact packageManager pin: {manifest}"
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

/// Regression (Expo/RN out-of-box failure): an auto-installed WILDCARD peer
/// must bind to a major already present in the resolved graph, never a
/// registry-highest major nothing declared a dependency on.
///
/// `react-native-worklets@0.10.1` declares `@babel/core: "*"` as a peer with
/// no co-declared dependency, while `react-native@0.86.0` hard-deps
/// `@babel/core: "^7.25.2"` (→ a 7.x). pnpm binds the `*` peer to that 7.x.
/// nub used to resolve the auto-installed peer inline mid-BFS, racing the hard
/// dep; when the peer won it fetched the registry-highest `@babel/core` (8.x)
/// that no range asked for, so Metro's Worklets babel plugin rejected the tree
/// (`Requires Babel "^7.0.0-0", but was loaded with "8.0.1"`) and `expo
/// export` failed. The fix parks auto-installed peers until the tree resolves,
/// so the peer reuses the graph's 7.x. Assert no `@babel/core` 8.x lands.
#[test]
#[ignore = "network: installs react-native + react-native-worklets from the npm registry"]
fn wildcard_peer_binds_resolved_major_not_registry_highest() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("wildcard-peer");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"wp","version":"1.0.0","dependencies":{"react-native":"0.86.0","react-native-worklets":"0.10.1"}}"#,
    )
    .unwrap();

    let (stdout, stderr, code) = run_install(&dir, &["install"]);
    assert_eq!(code, 0, "install must succeed: {stdout}{stderr}");

    // The isolated store keys every resolved version as `@babel+core@<ver>`;
    // @babel/core declares no peers so its own dirs carry no peer suffix.
    let store = dir.join("node_modules/.store");
    let babel_majors: Vec<String> = std::fs::read_dir(&store)
        .expect("virtual store must exist under node_modules/.store")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_prefix("@babel+core@").map(str::to_string))
        .collect();
    assert!(
        !babel_majors.is_empty(),
        "react-native hard-deps @babel/core, so a version must resolve: {stderr}"
    );
    assert!(
        babel_majors.iter().all(|v| v.starts_with("7.")),
        "the `*` @babel/core peer must reuse the resolved 7.x, never introduce \
         a higher major; store held: {babel_majors:?}"
    );
}

/// The dep-build fan-out used to bootstrap node-gyp *before* running anything,
/// for every approved build and without checking whether the graph wanted it.
/// A cold tool dir plus an unreachable registry therefore aborted an install
/// whose only build was a plain `node -e` — and the tool dir lives in the cache,
/// not the store, so restoring a store cache in CI did not help. The bootstrap
/// is lazy now: nothing is fetched until a script actually runs `node-gyp`.
///
/// Hermetic by construction — a `file:` dep needs no registry, and neither must
/// the node-gyp path. Holds whether or not the host happens to have a node-gyp
/// on PATH: with one, no shim is written; without, the shim is written but never
/// invoked. Either way the bootstrap bucket must not appear.
#[test]
fn approved_build_that_never_calls_node_gyp_installs_with_no_registry() {
    let dir = pm_tmpdir("no-gyp-bootstrap");
    let dep = dir.join("plainbuild");
    std::fs::create_dir_all(&dep).unwrap();
    // Writes relative to its own cwd (the materialized package dir) rather than
    // through an env var — the build jail scrubs the environment, so a marker
    // path passed as env would not survive.
    std::fs::write(
        dep.join("package.json"),
        r#"{"name":"plainbuild","version":"1.0.0","scripts":{"postinstall":"node -e \"require('fs').writeFileSync('built-ok','ok')\""}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"app","version":"1.0.0","private":true,"dependencies":{"plainbuild":"file:./plainbuild"},"allowBuilds":{"plainbuild@file:./plainbuild":true}}"#,
    )
    .unwrap();
    // `fetch-retries=0` so a regression fails fast instead of burning the
    // retry backoff (the original bug took ~70s to surface).
    std::fs::write(
        dir.join(".npmrc"),
        "registry=http://127.0.0.1:1/\nfetch-retries=0\n",
    )
    .unwrap();

    let cache = dir.join("xdg-cache");
    let out = Command::new(nub_binary())
        .arg("install")
        .current_dir(&dir)
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", &cache)
        .output()
        .expect("failed to spawn nub");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a non-gyp build must not need a registry: {stderr}"
    );
    assert!(
        dir.join("node_modules/plainbuild/built-ok").exists(),
        "the approved build script must actually have run: {stderr}"
    );
    assert!(
        !cache.join("nub/pm/tools/node-gyp/v12").exists(),
        "nothing invoked node-gyp, so it must not have been bootstrapped: {stderr}"
    );
}

/// `jailBuilds` is the one case the lazy shim cannot serve on its own: the jail
/// clears the environment and substitutes a temporary HOME, so a shim re-entry
/// resolves the tool dir under *that* home, finds nothing, and cannot refetch
/// because the jail denies network too. Jailed jobs therefore get node-gyp
/// resolved up front — but best-effort, so failing to reach a registry cannot
/// sink an install whose builds never wanted node-gyp.
///
/// Hermetic in both directions, which took two tries to get right. The dep is a
/// `file:` dep and the registry is deliberately dead, so the attempt fails on
/// every run — but the attempt only HAPPENS when node-gyp is not already
/// resolvable, so `PATH` is scrubbed of it. Without that scrub this passed
/// vacuously on any machine with a node-gyp installed (nothing was ever
/// bootstrapped, so nothing warned). The build script is a shell `echo`, not
/// `node`, so scrubbing cannot take the interpreter with it.
#[test]
fn jailed_build_that_never_needs_node_gyp_survives_a_failed_bootstrap() {
    let dir = pm_tmpdir("jail-no-gyp");
    let dep = dir.join("plainbuild");
    std::fs::create_dir_all(&dep).unwrap();
    // `echo x > file` is spelled the same for sh and cmd.exe, so this needs no
    // interpreter on PATH and no inline quoting that either shell re-parses.
    std::fs::write(
        dep.join("package.json"),
        r#"{"name":"plainbuild","version":"1.0.0","scripts":{"postinstall":"echo ok > built-ok"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"app","version":"1.0.0","private":true,"dependencies":{"plainbuild":"file:./plainbuild"},"allowBuilds":{"plainbuild@file:./plainbuild":true}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join(".npmrc"),
        "jail-builds=true\nregistry=http://127.0.0.1:1/\nfetch-retries=0\n",
    )
    .unwrap();

    // Drop every directory that provides a node-gyp, so the up-front resolve is
    // actually attempted rather than short-circuiting on the host's own copy.
    let names: &[&str] = if cfg!(windows) {
        &["node-gyp.cmd", "node-gyp.exe", "node-gyp"]
    } else {
        &["node-gyp"]
    };
    let scrubbed = std::env::var_os("PATH").map(|p| {
        let keep: Vec<_> = std::env::split_paths(&p)
            .filter(|d| !names.iter().any(|n| d.join(n).exists()))
            .collect();
        std::env::join_paths(keep).expect("rejoin PATH")
    });

    let mut cmd = Command::new(nub_binary());
    cmd.arg("install")
        .current_dir(&dir)
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"));
    if let Some(p) = scrubbed {
        cmd.env("PATH", p);
    }
    let out = cmd.output().expect("failed to spawn nub");
    let stderr = String::from_utf8_lossy(&out.stderr);

    // What this test actually owns: the bootstrap failure was DEMOTED to a
    // warning instead of propagating. The presenter rebrands `WARN_AUBE_*` to
    // `WARN_NUB_*` on the way out, so match either spelling — grepping only the
    // raw aube one silently finds nothing.
    assert!(
        stderr.contains("WARN_AUBE_NODE_GYP_BOOTSTRAP_FAILED")
            || stderr.contains("WARN_NUB_NODE_GYP_BOOTSTRAP_FAILED"),
        "the failed up-front resolve must warn rather than abort the install: {stderr}"
    );

    // Windows aborts node at startup under the build jail (`ncrypto::CSPRNG`
    // assertion), so the build script cannot run there at all and the install
    // fails for a reason this test does not own. Asserting success anyway would
    // pin an unrelated platform defect. Where the jail can run node, the full
    // contract holds.
    if stderr.contains("ncrypto::CSPRNG") {
        return;
    }
    assert_eq!(
        out.status.code(),
        Some(0),
        "an unreachable registry must not fail a jailed install that never uses node-gyp: {stderr}"
    );
    assert!(
        dir.join("node_modules/plainbuild/built-ok").exists(),
        "the approved build script must still have run: {stderr}"
    );
}

/// A cold CI install must link only the optional platform variants it actually
/// materializes. The resolver widens the graph with every platform's optional
/// native dep so the committed lockfile stays portable, and the virtual-store
/// prewarm writes one symlink per optional edge — so a prewarm running on the
/// unfiltered graph leaves a DANGLING link for every variant the host filter
/// drops (25 of esbuild's 26 `@esbuild/*`). CI is what auto-selects the
/// isolated layout that prewarm feeds, so this is the shape CI shipped. The
/// lockfile-reuse path filters before the prewarm and was never affected.
#[test]
#[ignore = "network: installs esbuild from the npm registry"]
fn ci_install_links_only_the_optional_platform_variants_it_materializes() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("optional-platform-links");
    // esbuild ships 26 platform-specific optional deps, exactly one of which is
    // installable on any given host — the widest cheap fixture for this.
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"opt","private":true,"dependencies":{"esbuild":"0.25.10"}}"#,
    )
    .unwrap();
    let (_out, err, code) = run_install_ci(&dir, &["install"]);
    assert_eq!(code, 0, "cold CI install must succeed: {err}");

    let nested = dir.join("node_modules/.store/esbuild@0.25.10/node_modules/@esbuild");
    let entries: Vec<_> = std::fs::read_dir(&nested)
        .unwrap_or_else(|e| panic!("no nested @esbuild dir at {}: {e}: {err}", nested.display()))
        .map(|e| e.unwrap().path())
        .collect();

    // Positive control: without this the dangling assertion below passes
    // vacuously on an empty or absent directory.
    assert!(
        !entries.is_empty(),
        "the host's own @esbuild variant must be linked: {err}"
    );

    // `Path::exists` follows symlinks, so a link whose target was never
    // materialized reads as absent — which is exactly the defect.
    let dangling: Vec<_> = entries
        .iter()
        .filter(|p| !p.exists())
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        dangling.is_empty(),
        "every linked @esbuild variant must be materialized; {} dangling: {dangling:?}",
        dangling.len()
    );
}

/// `--os`/`--cpu` select which platform-specific optional deps get installed,
/// overriding host detection for the run. The assertion is host-independent by
/// construction: it names platforms explicitly and never mentions the host, so
/// it reads the same on every CI leg.
///
/// The override is per AXIS — naming `--os` must leave a configured `cpu`
/// alone. That is the behavior pnpm pins too, and it is the one a union
/// implementation would silently get wrong (it would install darwin AND linux
/// here instead of linux only).
#[test]
#[ignore = "network: installs esbuild from the npm registry"]
fn platform_flags_override_the_named_axis_and_leave_the_others_configured() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("platform-flags");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"pf","private":true,"packageManager":"pnpm@10.15.1",
            "dependencies":{"esbuild":"0.25.10"},
            "pnpm":{"supportedArchitectures":{"os":["darwin"],"cpu":["arm64","x64"]}}}"#,
    )
    .unwrap();
    let (_out, err, code) = run_install(&dir, &["install", "--os", "linux"]);
    assert_eq!(code, 0, "flagged install must succeed: {err}");

    let mut got: Vec<String> = std::fs::read_dir(dir.join("node_modules/.store"))
        .unwrap_or_else(|e| panic!("no virtual store: {e}: {err}"))
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().to_string();
            name.strip_prefix("@esbuild+")
                .and_then(|rest| rest.split('@').next())
                .map(str::to_string)
        })
        .collect();
    got.sort();

    // `os` came from the flag and replaced `darwin`; `cpu` was never named, so
    // both configured values survive. A union would also install darwin-*.
    assert_eq!(
        got,
        vec!["linux-arm64".to_string(), "linux-x64".to_string()],
        "--os must replace the configured os and leave cpu alone: {err}"
    );
}

/// A platform selection has to invalidate the install-freshness fast path in
/// BOTH directions. The selection changes which prebuilt is correct, exactly as
/// swapping machines does, but the host bytes are identical across a flagged
/// and a bare run — so a flagged install on an already-installed tree reported
/// "up to date" and fetched nothing, and dropping the flags again left the
/// foreign tree in place. Neither is visible from a fresh-fixture test, which
/// is why this one installs twice.
#[test]
#[ignore = "network: installs esbuild from the npm registry"]
fn changing_the_platform_selection_re_materializes_an_already_installed_tree() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("platform-rematerialize");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"pr","private":true,"packageManager":"pnpm@10.15.1",
            "dependencies":{"esbuild":"0.25.10"}}"#,
    )
    .unwrap();

    // Whichever variant the host earns. Named rather than assumed, so the
    // assertions below stay true on every CI leg.
    let (_o, err, code) = run_install(&dir, &["install"]);
    assert_eq!(code, 0, "baseline install must succeed: {err}");
    let host_variants = linked_esbuild_variants(&dir);
    assert_eq!(
        host_variants.len(),
        1,
        "a plain install links exactly the host's variant, got {host_variants:?}: {err}"
    );

    // win32/x64 is never the host on any leg we run, so this is a real change.
    let (_o, err, code) = run_install(&dir, &["install", "--os", "win32", "--cpu", "x64"]);
    assert_eq!(
        code, 0,
        "flagged install over a warm tree must succeed: {err}"
    );
    assert_eq!(
        linked_esbuild_variants(&dir),
        vec!["win32-x64".to_string()],
        "the warm tree must be re-materialized for the named platform: {err}"
    );

    // ...and back. The state a flagged install writes must not read as
    // up-to-date for a bare one.
    let (_o, err, code) = run_install(&dir, &["install"]);
    assert_eq!(
        code, 0,
        "install after dropping the flags must succeed: {err}"
    );
    assert_eq!(
        linked_esbuild_variants(&dir),
        host_variants,
        "dropping the flags must restore the host's own variant: {err}"
    );
}

/// The `@esbuild/*` variants actually linked under the `esbuild` package —
/// resolvable ones only, so a dangling link never counts as present.
fn linked_esbuild_variants(dir: &Path) -> Vec<String> {
    let nested = dir.join("node_modules/.store/esbuild@0.25.10/node_modules/@esbuild");
    let mut out: Vec<String> = std::fs::read_dir(&nested)
        .unwrap_or_else(|e| panic!("no nested @esbuild dir at {}: {e}", nested.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.exists())
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

/// Build a workspace whose consumer `app` declares `deps`, alongside members
/// `lib` (name `lib`, 1.0.0, bin `lib-cli`), `@acme/scoped` (2.0.0) and
/// `shadow` (9.9.9). Each member's `main` returns its own name, so a test can
/// assert WHICH package answered rather than that something did.
fn workspace_alias_fixture(tag: &str, deps: &str) -> PathBuf {
    let dir = pm_tmpdir(tag);
    std::fs::write(
        dir.join("package.json"),
        r#"{ "name": "root", "private": true, "workspaces": ["packages/*"] }"#,
    )
    .unwrap();
    for (member, name, version) in [
        ("lib", "lib", "1.0.0"),
        ("scoped", "@acme/scoped", "2.0.0"),
        ("shadow", "shadow", "9.9.9"),
    ] {
        let mdir = dir.join("packages").join(member);
        std::fs::create_dir_all(&mdir).unwrap();
        let bin = if member == "lib" {
            r#", "bin": { "lib-cli": "cli.js" }"#
        } else {
            ""
        };
        std::fs::write(
            mdir.join("package.json"),
            format!(r#"{{ "name": "{name}", "version": "{version}", "main": "index.js"{bin} }}"#),
        )
        .unwrap();
        std::fs::write(mdir.join("index.js"), format!("module.exports = '{name}';")).unwrap();
        std::fs::write(mdir.join("cli.js"), "#!/usr/bin/env node\n").unwrap();
    }
    let app = dir.join("packages/app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("package.json"),
        format!(r#"{{ "name": "app", "version": "1.0.0", "dependencies": {{ {deps} }} }}"#),
    )
    .unwrap();
    dir
}

/// What `require(spec)` returns from `packages/app`, or the node error.
fn require_from_app(dir: &Path, spec: &str) -> String {
    let out = Command::new("node")
        .args(["-e", &format!("process.stdout.write(require({spec:?}))")])
        .current_dir(dir.join("packages/app"))
        .output()
        .expect("failed to spawn node");
    if out.status.success() {
        String::from_utf8_lossy(&out.stdout).to_string()
    } else {
        format!("<require failed: {}>", String::from_utf8_lossy(&out.stderr))
    }
}

/// `workspace:<member>@<range>` aliases a workspace package under a different
/// dependency key, and `workspace:<relative-path>` addresses one by directory.
/// Both are documented pnpm spec forms (nubjs/nub#713) that resolve against the
/// workspace, never the registry.
///
/// The `shadow` case is the one that matters most: the dependency key names a
/// member too, and the ALIAS TARGET has to win. Real pnpm 10 resolves it to
/// `lib`; before the fix nub silently linked `shadow` to itself, returned the
/// wrong package with exit 0, and wrote that wrong answer to the lockfile.
#[test]
fn workspace_alias_resolves_the_target_member_not_the_dependency_key() {
    let dir = workspace_alias_fixture(
        "ws-alias",
        r#""lib-alias": "workspace:lib@*",
           "pinned": "workspace:lib@^1.0.0",
           "s-alias": "workspace:@acme/scoped@*",
           "by-path": "workspace:../lib",
           "shadow": "workspace:lib@*""#,
    );

    let (stdout, stderr, code) = run_install(&dir, &["install"]);
    assert_eq!(code, 0, "install must succeed: {stdout}{stderr}");

    for (key, want) in [
        ("lib-alias", "lib"),
        ("pinned", "lib"),
        ("s-alias", "@acme/scoped"),
        ("by-path", "lib"),
        ("shadow", "lib"),
    ] {
        assert_eq!(
            require_from_app(&dir, key),
            want,
            "`require({key:?})` must load the aliased member `{want}`"
        );
    }

    // pnpm records both the plain and the aliased form as `link:<rel>` while
    // keeping the `workspace:` text as the specifier. Matching that shape is
    // what keeps the lockfile readable by pnpm.
    let lock = std::fs::read_to_string(dir.join("nub.lock")).unwrap();
    for needle in [
        "specifier: workspace:lib@*",
        "specifier: workspace:@acme/scoped@*",
        "specifier: workspace:../lib",
        "version: link:../lib",
        "version: link:../scoped",
    ] {
        assert!(
            lock.contains(needle),
            "nub.lock must contain `{needle}`, got:\n{lock}"
        );
    }

    // An aliased member's bins reach the consumer's `.bin/` like any other
    // workspace dep's — the alias is a rename, not a downgrade.
    let bin = dir.join("packages/app/node_modules/.bin/lib-cli");
    assert!(
        bin.exists(),
        "`lib-cli` must be shimmed into packages/app/node_modules/.bin, found: {:?}",
        std::fs::read_dir(dir.join("packages/app/node_modules/.bin"))
            .map(|d| d.map(|e| e.unwrap().file_name()).collect::<Vec<_>>())
            .unwrap_or_default()
    );
}

/// `nub up` run INSIDE a workspace member must leave every workspace-local
/// dependency resolving exactly where the root install left it.
///
/// A member-scoped update merges its graph into the workspace-root lockfile,
/// so it has to resolve in the workspace-root frame. Anchored at the member
/// instead (nubjs/nub#721) it broke two ways at once, one loud and one silent:
///
///   - `workspace:lib@*` — the alias rewrite looks the target member's
///     directory up among the manifests it was handed, and a member-scoped
///     resolve is handed only its own, so every sibling was missing and the
///     update died with `ERR_NUB_WORKSPACE_PKG_NOT_FOUND` while the error
///     itself listed the member as present.
///   - `link:../lib` — `LocalSource` paths are stored relative to the
///     resolver's project root, so a member anchor made them member-relative
///     and the lockfile writer rebased them a SECOND time against the
///     root-relative importer key. `link:../lib` was written `link:../../../lib`
///     and the symlink pointed clean out of the workspace, with exit 0.
///
/// Asserting on the post-`up` lockfile AND on `require` catches both: the
/// version strings pin the anchoring, `require` proves the tree on disk still
/// resolves rather than dangling.
#[test]
fn member_scoped_update_keeps_workspace_local_deps_anchored_at_the_root() {
    let dir = workspace_alias_fixture(
        "ws-alias-up",
        r#""lib-alias": "workspace:lib@*",
           "by-path": "workspace:../lib",
           "linked": "link:../lib""#,
    );

    let (stdout, stderr, code) = run_install(&dir, &["install"]);
    assert_eq!(code, 0, "install must succeed: {stdout}{stderr}");

    // The update runs from the member, which is the whole point.
    let app = dir.join("packages/app");
    let (stdout, stderr, code) = run_install(&app, &["up"]);
    assert_eq!(
        code, 0,
        "`nub up` inside packages/app must succeed: {stdout}{stderr}"
    );

    let lock = std::fs::read_to_string(dir.join("nub.lock")).unwrap();
    for needle in [
        "specifier: workspace:lib@*",
        "specifier: workspace:../lib",
        "specifier: link:../lib",
        "version: link:../lib",
    ] {
        assert!(
            lock.contains(needle),
            "after a member-scoped `up`, nub.lock must still contain `{needle}`, got:\n{lock}"
        );
    }
    assert!(
        !lock.contains("link:../../"),
        "a `link:` target must stay anchored at the workspace root — an extra `../` \
         walks out of the workspace entirely, got:\n{lock}"
    );

    for key in ["lib-alias", "by-path", "linked"] {
        assert_eq!(
            require_from_app(&dir, key),
            "lib",
            "after a member-scoped `up`, `require({key:?})` must still load `lib`"
        );
    }
}

/// The alias rewrite needs the workspace's members in EVERY frame, not just
/// the workspace-root one a shared-lockfile member resolves in.
///
/// Both configurations here keep the resolve at the `"."` importer, so an
/// earlier cut of the #721 fix — which supplied the member map only when it
/// switched frames — left the reported `ERR_NUB_WORKSPACE_PKG_NOT_FOUND`
/// reproducing in exactly these two, while closing the issue.
#[test]
fn workspace_alias_resolves_for_updates_that_stay_in_the_dot_frame() {
    // A member whose graph does NOT merge into a shared root lockfile:
    // it writes its own, so it resolves at `.` anchored on itself, and
    // the alias target has to come out `../lib` rather than `packages/lib`.
    let dir = workspace_alias_fixture("ws-alias-unshared", r#""lib-alias": "workspace:lib@*""#);
    std::fs::write(dir.join(".npmrc"), "shared-workspace-lockfile=false\n").unwrap();

    let (stdout, stderr, code) = run_install(&dir, &["install"]);
    assert_eq!(code, 0, "install must succeed: {stdout}{stderr}");
    let (stdout, stderr, code) = run_install(&dir.join("packages/app"), &["up"]);
    assert_eq!(
        code, 0,
        "`up` in a member with its own lockfile must resolve the alias: {stdout}{stderr}"
    );
    assert_eq!(
        require_from_app(&dir, "lib-alias"),
        "lib",
        "the aliased member must still load after an unshared-lockfile `up`"
    );

    // An alias declared in the workspace ROOT's own manifest. `up` at the
    // root is already the `.` frame, so nothing switches — but the root
    // still needs the member map to find `lib`.
    let root_dir = workspace_alias_fixture("ws-alias-rootdep", r#""x": "workspace:lib@*""#);
    let root_manifest = root_dir.join("package.json");
    std::fs::write(
        &root_manifest,
        r#"{ "name": "root", "private": true, "workspaces": ["packages/*"],
             "dependencies": { "lib-at-root": "workspace:lib@*" } }"#,
    )
    .unwrap();

    let (stdout, stderr, code) = run_install(&root_dir, &["install"]);
    assert_eq!(code, 0, "install must succeed: {stdout}{stderr}");
    let (stdout, stderr, code) = run_install(&root_dir, &["up"]);
    assert_eq!(
        code, 0,
        "`up` at the workspace root must resolve an alias the root declares: {stdout}{stderr}"
    );
    let lock = std::fs::read_to_string(root_dir.join("nub.lock")).unwrap();
    assert!(
        lock.contains("version: link:packages/lib"),
        "the root's own alias must link to the member directory, got:\n{lock}"
    );
}

/// `workspace:` only ever resolves against the workspace, so a spec naming a
/// package that is not a member is a hard error — never a silent fall-through
/// to the registry, which used to report the confusing
/// `no version of <key> matches range \`workspace:*\``.
#[test]
fn workspace_spec_naming_a_non_member_fails_without_reaching_the_registry() {
    for (deps, expect_code, expect_text) in [
        // The alias form: the message must name the TARGET, not the key.
        (
            r#""lib-alias": "workspace:nosuchpkg@*""#,
            "ERR_NUB_WORKSPACE_PKG_NOT_FOUND",
            "nosuchpkg",
        ),
        // The plain form, where the key itself is not a member.
        (
            r#""ms": "workspace:*""#,
            "ERR_NUB_WORKSPACE_PKG_NOT_FOUND",
            "ms",
        ),
        // An aliased range the local copy cannot satisfy.
        (
            r#""lib-alias": "workspace:lib@^2.0.0""#,
            "ERR_NUB_NO_MATCHING_VERSION",
            "^2.0.0",
        ),
    ] {
        let dir = workspace_alias_fixture("ws-alias-err", deps);
        let (stdout, stderr, code) = run_install(&dir, &["install"]);
        let all = format!("{stdout}{stderr}");
        assert_ne!(code, 0, "`{deps}` must fail the install, got 0:\n{all}");
        assert!(
            all.contains(expect_code) && all.contains(expect_text),
            "`{deps}` must fail with {expect_code} mentioning `{expect_text}`, got:\n{all}"
        );
    }
}

/// A lockfile that records the root importer with ZERO direct deps while
/// `package.json` declares one must read as drift, in every format
/// (nubjs/nub#657). Two shapes reach it: a `package-lock.json` whose root
/// entry declares a dep with no matching `node_modules/<name>` package node,
/// and a `pnpm-lock.yaml` written as `importers: { .: {} }`. Both used to
/// satisfy the drift check's `all(specifier.is_none())` guard vacuously, so
/// `nub ci` exited 0 having linked nothing and printed "Already up to date" —
/// a green CI run with an empty `node_modules`. `npm ci` rejects the same
/// input with `EUSAGE`, `pnpm install --frozen-lockfile` with
/// `ERR_PNPM_OUTDATED_LOCKFILE`.
///
/// No network: the frozen drift check runs before any resolution, so the
/// rejection is reached without touching a registry.
#[test]
fn ci_rejects_lockfile_whose_root_importer_is_empty() {
    for (tag, lockfile, body) in [
        (
            "npm-no-package-node",
            "package-lock.json",
            r#"{"name":"empty-importer","version":"1.0.0","lockfileVersion":3,"requires":true,
                "packages":{"":{"name":"empty-importer","version":"1.0.0",
                "dependencies":{"is-odd":"3.0.1"}}}}"#,
        ),
        (
            "pnpm-empty-importer",
            "pnpm-lock.yaml",
            "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n",
        ),
    ] {
        let dir = pm_tmpdir(tag);
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"empty-importer","version":"1.0.0","dependencies":{"is-odd":"3.0.1"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join(lockfile), body).unwrap();

        let (out, err, code) = run_install(&dir, &["ci"]);
        assert_ne!(
            code, 0,
            "{tag}: `nub ci` must reject a lockfile that resolves none of the \
             manifest's deps, not report success: {out}\n{err}"
        );
        // Pin the failure to the drift path so it cannot pass on an unrelated
        // network or store error.
        assert!(
            err.contains("ERR_NUB_OUTDATED_LOCKFILE"),
            "{tag}: the rejection must be the outdated-lockfile error: {err}"
        );
        assert!(
            !dir.join("node_modules").join("is-odd").exists(),
            "{tag}: nothing may be linked when the frozen install is rejected"
        );
    }
}

/// A dependency build the previous install could not run leaves the tree
/// incomplete, so the next install must not report it up to date
/// (nubjs/nub#764). Before the fix nothing recorded that a build was owed:
/// `check_needs_install` compared only inputs describing what the tree was
/// built FROM, found them all unchanged, printed "Already up to date" and
/// exited 0 — permanently, on every later install, with no way out but
/// `--force` or deleting `node_modules`.
///
/// The assertion is the install's own verdict rather than the build's output,
/// and that is not a shortcut. Three earlier drafts asserted on a marker file
/// and each answered the wrong question. A marker written inside the dependency
/// is part of a `file:` package's content, so deleting it to set up the retry
/// changes the very input the install compares — and the side-effects cache
/// restores it whether the script re-ran or not. A marker written outside the
/// dependency never survives the build jail, which scrubs the environment and
/// confines writes. The verdict has neither problem: it is exactly what the
/// issue reports and exactly what the fix changes.
///
/// This lives nub-side rather than in aube's own e2e suite because the warm
/// short-circuit is not reachable there at all — measured: under aube's
/// defaults a `file:` dependency takes the full path on every install, with or
/// without a build script, so only a dependency-free project ever goes warm.
#[test]
fn an_owed_dependency_build_stops_the_next_install_reporting_up_to_date() {
    let dir = pm_tmpdir("owed-build-retry");
    let dep = dir.join("plainbuild");
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::write(
        dep.join("package.json"),
        r#"{"name":"plainbuild","version":"1.0.0","scripts":{"postinstall":"node -e \"process.exit(0)\""}}"#,
    )
    .unwrap();
    // The approval is keyed by SOURCE (`name@file:./path`), not bare name — a
    // bare-name entry never authorizes a source-backed build.
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"app","version":"1.0.0","private":true,"dependencies":{"plainbuild":"file:./plainbuild"},"allowBuilds":{"plainbuild@file:./plainbuild":true}}"#,
    )
    .unwrap();
    // Dead registry + no retries: the fixture is entirely local, so any
    // network attempt is a regression and must fail fast rather than hang.
    std::fs::write(
        dir.join(".npmrc"),
        "registry=http://127.0.0.1:1/\nfetch-retries=0\n",
    )
    .unwrap();

    // Run an install and report whether it took the warm short-circuit.
    let up_to_date = || -> bool {
        let out = Command::new(nub_binary())
            .arg("install")
            .current_dir(&dir)
            .env("XDG_DATA_HOME", dir.join("xdg-data"))
            .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
            .output()
            .expect("failed to spawn nub");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(
            out.status.code(),
            Some(0),
            "install failed unexpectedly\nstdout: {stdout}\nstderr: {stderr}"
        );
        format!("{stdout}{stderr}").contains("Already up to date")
    };

    assert!(!up_to_date(), "the first install has work to do");

    // CONTROL. Without it every "not up to date" below would also pass on a
    // fixture that simply never reaches the warm path — which is precisely how
    // three earlier drafts of this test managed to prove nothing.
    assert!(
        up_to_date(),
        "control: a settled tree must take the warm path, or the assertions below are vacuous"
    );

    let state_dir = dir.join("node_modules/.store/.nub-state");
    assert!(
        state_dir.is_dir(),
        "expected install state at {}",
        state_dir.display()
    );

    // Record a build as owed, exactly as an install that could not run one
    // does. `None` strips the field entirely: state written before it existed,
    // which is the shape a tree already sealed in the wild carries.
    let strand = |deferred: Option<&str>| {
        let mut touched = 0;
        for name in ["state.json", "fresh.json"] {
            let path = state_dir.join(name);
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
            match deferred {
                Some(key) => doc["deferred_dep_builds"] = serde_json::json!([key]),
                None => {
                    doc.as_object_mut().unwrap().remove("deferred_dep_builds");
                }
            }
            std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
            touched += 1;
        }
        assert!(
            touched > 0,
            "no install state found at {}",
            state_dir.display()
        );
    };

    strand(Some("plainbuild@file:./plainbuild"));
    assert!(
        !up_to_date(),
        "an install that recorded an owed build must re-run the pipeline rather than report \
         the tree up to date — that verdict is the seal this issue is about"
    );
    assert!(
        up_to_date(),
        "and the retry must clear the record: an owed build costs one install, not a full \
         install forever"
    );

    strand(None);
    assert!(
        !up_to_date(),
        "install state predating the field cannot say what it deferred, so it must re-check \
         once rather than read as nothing owed — that is what heals an already-sealed tree"
    );
    assert!(
        up_to_date(),
        "and that migration must be one-time: the re-check writes the field, so it cannot repeat"
    );
}
