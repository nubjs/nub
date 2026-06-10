//! PM-shim integration tests: spawn the real `nub` binary through PM-named
//! links (argv0 dispatch) and as `nub pm shim`/`unshim`, asserting the ratified
//! contract (wiki/research/package-manager-shims.md, 2026-06-09) end to end.
//!
//! Hermetic by construction: every child gets an explicit PATH / HOME /
//! XDG_CACHE_HOME, fall-through targets are fake shell scripts that print their
//! own `$0` + argv (so the exact exec'd program is asserted, not inferred), the
//! pinned-PM run is satisfied from a pre-seeded cache (zero network), and the
//! provisioning failure path runs against a dead-registry `.npmrc`
//! (`127.0.0.1:1`). Only the `#[ignore]` e2e touches the real registry.

#![cfg(unix)] // exec + shell-script fakes; the Windows half is pm_shim_windows.rs.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// Unique temp dir under the system temp root (never under $HOME — the
/// manifest walk-up must not escape into a stray ancestor package.json).
///
/// The name carries a per-process startup-nanos component BESIDES the PID +
/// counter. PID + counter alone collided with STALE dirs from earlier suite
/// runs: these dirs are never cleaned (a panicking test must leave its state
/// inspectable), thousands accumulate in $TMPDIR across a work session, and
/// macOS recycles PIDs (the observed leftovers already spanned a full wrap of
/// the ~99k PID space) — so a later run with a recycled PID re-entered a stale
/// sibling and found last run's links, failing `shim_link`'s hard_link/symlink
/// with EEXIST. That was the intermittent full-suite flake (seen as
/// `nub_from_the_shim_dir_defers_to_the_real_nub_on_path` failing); the nanos
/// component makes names unique across runs, the counter within one.
fn tmp(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    static STARTED_NANOS: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
    let nanos = STARTED_NANOS.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    });
    let dir = std::env::temp_dir().join(format!(
        "nub-pmshim-{tag}-{}-{nanos:x}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Link the real nub binary under a PM name — what `nub pm shim` produces.
/// Hardlink like the real shim; symlink fallback if temp is another filesystem
/// (argv0 detection reads the invoked name either way).
fn shim_link(dir: &Path, name: &str) -> PathBuf {
    let link = dir.join(name);
    if std::fs::hard_link(nub_binary(), &link).is_err() {
        std::os::unix::fs::symlink(nub_binary(), &link).unwrap();
    }
    link
}

/// A fake system PM: prints `FAKE:<its own path>:<args>` so a test asserts the
/// exact (program, argv) the shim exec'd.
fn fake_pm(dir: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\necho \"FAKE:$0:$@\"\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Spawn `program args…` with a fully explicit environment (PATH/HOME/cache
/// set per test, ambient `npm_config_registry` stripped so a dev-box override
/// can't re-route dead-registry assertions). Returns (stdout, stderr, code).
/// Signal-deaths (`code() == None`) retry once: macOS very occasionally kills
/// a freshly-hardlinked binary on exec (signature-cache transient); a
/// deterministic failure still fails — it dies twice.
fn run(program: &Path, args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> (String, String, i32) {
    let attempt = || {
        let mut cmd = Command::new(program);
        cmd.args(args).current_dir(cwd);
        cmd.env_remove("npm_config_registry");
        // Strip the PM-nesting markers from the inherited env so a top-level
        // refusal/fall-through assertion is deterministic even when the suite
        // itself was launched by a package manager (which would set these). The
        // nested-re-entry tests set them back EXPLICITLY via `env`.
        cmd.env_remove("npm_config_user_agent");
        cmd.env_remove("npm_execpath");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("failed to spawn");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.code(),
        )
    };
    match attempt() {
        (out, err, Some(code)) => (out, err, code),
        (_, _, None) => {
            let (out, err, code) = attempt();
            (out, err, code.unwrap_or(-1))
        }
    }
}

/// [`run`] with a watchdog: kills the child and panics if it outlives `secs`.
/// For regressions whose failure mode is an infinite exec loop (recursion-guard
/// holes) — a plain `.output()` would hang the suite forever instead of failing.
///
/// Signal-deaths retry once, the same policy (and reason) as [`run`]: macOS
/// very occasionally SIGKILLs a freshly-hardlinked binary on exec (the
/// signature-cache transient), which here surfaced as an intermittent
/// `code == -1` with empty output. A watchdog expiry is NOT retried — an
/// exec loop is deterministic and the panic should name it immediately.
fn run_with_timeout(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    secs: u64,
) -> (String, String, i32) {
    let attempt = || {
        let mut cmd = Command::new(program);
        cmd.args(args).current_dir(cwd);
        cmd.env_remove("npm_config_registry");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect("failed to spawn");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            match child.try_wait().expect("wait failed") {
                Some(_) => break,
                None if std::time::Instant::now() > deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child still running after {secs}s — exec-loop regression?");
                }
                None => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        }
        let out = child.wait_with_output().expect("collecting output");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.code(),
        )
    };
    match attempt() {
        (out, err, Some(code)) => (out, err, code),
        (_, _, None) => {
            let (out, err, code) = attempt();
            (out, err, code.unwrap_or(-1))
        }
    }
}

#[test]
fn argv0_pnpm_dispatches_to_the_shim_and_falls_through_when_unpinned() {
    let work = tmp("fallthrough");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("package.json"), r#"{"name":"app"}"#).unwrap();
    let sys = work.join("sys");
    std::fs::create_dir_all(&sys).unwrap();
    let fake = fake_pm(&sys, "pnpm");
    let link = shim_link(&work, "pnpm");

    let (stdout, stderr, code) = run(
        &link,
        &["install", "--frozen-lockfile"],
        &proj,
        &[
            ("PATH", sys.to_str().unwrap()),
            ("HOME", work.to_str().unwrap()),
        ],
    );
    assert_eq!(
        code, 0,
        "fall-through must exit with the system PM's code; stderr:\n{stderr}"
    );
    assert_eq!(
        stdout,
        format!("FAKE:{}:install --frozen-lockfile\n", fake.display()),
        "the exec target must be the system pnpm with argv passed verbatim"
    );
}

#[test]
fn mismatched_pm_in_a_pinned_project_refuses_with_the_redirect() {
    let work = tmp("refuse");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("package.json"),
        r#"{"packageManager":"pnpm@9.0.0"}"#,
    )
    .unwrap();
    // A system npm exists on PATH — the strict check must refuse BEFORE it.
    let sys = work.join("sys");
    std::fs::create_dir_all(&sys).unwrap();
    fake_pm(&sys, "npm");
    let link = shim_link(&work, "npm");

    let (stdout, stderr, code) = run(
        &link,
        &["install", "react"],
        &proj,
        &[
            ("PATH", sys.to_str().unwrap()),
            ("HOME", work.to_str().unwrap()),
        ],
    );
    assert_eq!(code, 1, "the strict refusal exits 1; stderr:\n{stderr}");
    assert!(
        !stdout.contains("FAKE"),
        "the system npm must NOT run on a refusal, got stdout:\n{stdout}"
    );
    for needle in [
        "pnpm",
        "package.json#packageManager",
        "pnpm install react",
        "nub pm unshim",
    ] {
        assert!(
            stderr.contains(needle),
            "the refusal must contain {needle:?}, got:\n{stderr}"
        );
    }
}

#[test]
fn transparent_verb_falls_through_to_the_system_pm_not_the_pin() {
    let work = tmp("transparent");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("package.json"),
        r#"{"packageManager":"pnpm@9.0.0"}"#,
    )
    .unwrap();
    let sys = work.join("sys");
    std::fs::create_dir_all(&sys).unwrap();
    let fake = fake_pm(&sys, "npm");
    let link = shim_link(&work, "npm");

    // `npm create vite` in a pnpm repo must work — via the SYSTEM npm.
    let (stdout, stderr, code) = run(
        &link,
        &["create", "vite", "my-app"],
        &proj,
        &[
            ("PATH", sys.to_str().unwrap()),
            ("HOME", work.to_str().unwrap()),
        ],
    );
    assert_eq!(
        code, 0,
        "a transparent verb never refuses; stderr:\n{stderr}"
    );
    assert_eq!(
        stdout,
        format!("FAKE:{}:create vite my-app\n", fake.display()),
        "the transparent fall-through targets the system npm, not the pinned pnpm"
    );
}

#[test]
fn nested_mismatched_pm_falls_through_instead_of_refusing() {
    // The highest-value bug: the pinned pnpm runs a lifecycle script (a
    // postinstall) that shells out to `npm`. That `npm` re-enters the shim as a
    // name mismatch — and a strict refusal there would break an install the
    // user issued as `pnpm install`, never typed `npm` for. The nesting marker
    // `npm_config_user_agent` (set by every PM for its children) tells the shim
    // we're nested, so the mismatch falls through to the system npm.
    let work = tmp("nested");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("package.json"),
        r#"{"packageManager":"pnpm@9.0.0"}"#,
    )
    .unwrap();
    let sys = work.join("sys");
    std::fs::create_dir_all(&sys).unwrap();
    let fake = fake_pm(&sys, "npm");
    let link = shim_link(&work, "npm");

    let (stdout, stderr, code) = run(
        &link,
        &["install"],
        &proj,
        &[
            ("PATH", sys.to_str().unwrap()),
            ("HOME", work.to_str().unwrap()),
            // The "a PM is running above me" marker a real pnpm sets for its
            // spawned children — brand-safe (npm-owned), not a NUB_* sentinel.
            ("npm_config_user_agent", "pnpm/9.0.0 npm/? node/v22.0.0"),
        ],
    );
    assert_eq!(
        code, 0,
        "a nested mismatch must NOT refuse — it falls through; stderr:\n{stderr}"
    );
    assert_eq!(
        stdout,
        format!("FAKE:{}:install\n", fake.display()),
        "the nested npm runs the system npm rather than breaking the install"
    );

    // Control: the SAME invocation WITHOUT the marker (the user typed `npm
    // install` at a shell) keeps the strict refusal.
    let (stdout, _stderr, code) = run(
        &link,
        &["install"],
        &proj,
        &[
            ("PATH", sys.to_str().unwrap()),
            ("HOME", work.to_str().unwrap()),
        ],
    );
    assert_eq!(code, 1, "a top-level mismatch still refuses");
    assert!(
        !stdout.contains("FAKE"),
        "the strict refusal must not run the system npm, got:\n{stdout}"
    );
}

#[test]
fn refusal_suggests_use_pnpm_for_an_npm_only_verb() {
    // The verb-swap fix: a strict refusal must not synthesize a verb the pinned
    // PM lacks. `npm ci` in a pnpm-pinned project must NOT suggest `pnpm ci`
    // (pnpm has no `ci`) — it suggests the verbless `use pnpm`.
    let work = tmp("ci-redirect");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("package.json"),
        r#"{"packageManager":"pnpm@9.0.0"}"#,
    )
    .unwrap();
    let link = shim_link(&work, "npm");

    let (_stdout, stderr, code) = run(
        &link,
        &["ci"],
        &proj,
        &[
            ("PATH", work.to_str().unwrap()),
            ("HOME", work.to_str().unwrap()),
        ],
    );
    assert_eq!(code, 1, "a top-level mismatch refuses; stderr:\n{stderr}");
    assert!(
        stderr.contains("use pnpm") && !stderr.contains("pnpm ci"),
        "the redirect must suggest `use pnpm`, never the nonexistent `pnpm ci`, got:\n{stderr}"
    );
}

#[test]
fn pinned_name_match_runs_the_cached_pm_under_the_project_node() {
    let work = tmp("pinned");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("package.json"),
        r#"{"packageManager":"pnpm@9.0.0"}"#,
    )
    .unwrap();

    // Seed the store with a fake cached pnpm@9.0.0 (the exact-pin cache hit is
    // zero-network), plus a dead-registry .npmrc beside the store so any
    // accidental cache miss fails fast instead of touching the real registry.
    let cache = work.join("cache");
    let nub_cache = cache.join("nub");
    let pkg = nub_cache.join("pm/pnpm/9.0.0/package");
    std::fs::create_dir_all(pkg.join("bin")).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{"name":"pnpm","bin":{"pnpm":"bin/pnpm.cjs","pnpx":"bin/pnpx.cjs"}}"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("bin/pnpm.cjs"),
        "console.log('PINNED-PNPM ' + process.argv.slice(2).join(' '))\n",
    )
    .unwrap();
    std::fs::write(nub_cache.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    let link = shim_link(&work, "pnpm");

    // PATH is inherited: the project Node resolves from it (no Node pin here);
    // RunPinned never PATH-scans for pnpm, so an ambient real pnpm is inert.
    let path = std::env::var("PATH").unwrap();
    let (stdout, stderr, code) = run(
        &link,
        &["install", "--offline"],
        &proj,
        &[
            ("PATH", path.as_str()),
            ("XDG_CACHE_HOME", cache.to_str().unwrap()),
        ],
    );
    assert_eq!(
        code, 0,
        "the cached pinned pnpm must run cleanly; stderr:\n{stderr}"
    );
    assert_eq!(
        stdout, "PINNED-PNPM install --offline\n",
        "the pinned PM's bin must run under node with argv passed verbatim"
    );
    assert!(
        !stderr.contains("Installing"),
        "an exact cached pin must be a silent zero-network hit, got stderr:\n{stderr}"
    );
}

#[test]
fn unpinned_path_miss_provisions_a_dynamic_default_within_the_lockfile_family() {
    let work = tmp("dyndefault");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let manifest = r#"{"name":"app"}"#;
    std::fs::write(proj.join("package.json"), manifest).unwrap();
    std::fs::write(proj.join("pnpm-lock.yaml"), "lockfileVersion: '6.0'\n").unwrap();

    // No pnpm anywhere on PATH; the store's .npmrc points at a dead registry,
    // so the provisioning ATTEMPT is observable (announcement + fetch error)
    // with zero real network.
    let empty = work.join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();
    let cache = work.join("cache");
    std::fs::create_dir_all(cache.join("nub")).unwrap();
    std::fs::write(cache.join("nub/.npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    let link = shim_link(&work, "pnpm");

    let (_stdout, stderr, code) = run(
        &link,
        &["install"],
        &proj,
        &[
            ("PATH", empty.to_str().unwrap()),
            ("HOME", work.to_str().unwrap()),
            ("XDG_CACHE_HOME", cache.to_str().unwrap()),
        ],
    );
    assert_ne!(code, 0, "the dead registry must fail the default provision");
    assert!(
        stderr.contains("no pnpm on PATH") && stderr.contains("pnpm@8"),
        "the announced default must name the invoked PM and the lockfile-implied \
         family (pnpm-lock 6.0 → pnpm 8), got:\n{stderr}"
    );
    assert!(
        stderr.contains("no pin written"),
        "the announcement must state the no-pin contract, got:\n{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(proj.join("package.json")).unwrap(),
        manifest,
        "the shim must never write a pin"
    );
}

#[test]
fn pm_shim_and_unshim_round_trip_against_a_temp_home() {
    let home = tmp("home");
    let zshrc = home.join(".zshrc");
    let original = "# mine\nexport FOO=1\n";
    std::fs::write(&zshrc, original).unwrap();
    let env: Vec<(&str, &str)> = vec![("HOME", home.to_str().unwrap()), ("SHELL", "/bin/zsh")];

    // Install: 7 hardlinks land, the marked block is appended once.
    let (stdout, stderr, code) = run(&nub_binary(), &["pm", "shim"], &home, &env);
    assert_eq!(code, 0, "nub pm shim must succeed; stderr:\n{stderr}");
    let shims = home.join(".nub/shims");
    for name in ["npm", "npx", "pnpm", "pnpx", "yarn", "yarnpkg", "nub"] {
        assert!(
            shims.join(name).is_file(),
            "{name} must exist in {} — stdout:\n{stdout}",
            shims.display()
        );
    }
    let profile = std::fs::read_to_string(&zshrc).unwrap();
    assert_eq!(
        profile,
        format!("{original}\n# nub shims\nexport PATH=\"$HOME/.nub/shims:$PATH\"\n"),
        "the marked PATH block lands once, install.sh-shaped"
    );
    assert!(
        stdout.contains("source") && stdout.contains(".zshrc"),
        "the report must carry a source hint, got:\n{stdout}"
    );

    // Idempotent re-run: no second block, entries already current.
    let (stdout2, _, code2) = run(&nub_binary(), &["pm", "shim"], &home, &env);
    assert_eq!(code2, 0);
    assert_eq!(
        std::fs::read_to_string(&zshrc).unwrap(),
        profile,
        "re-running must not append a second block"
    );
    assert!(
        stdout2.contains("already current") && stdout2.contains("already present"),
        "the re-run report names the no-op, got:\n{stdout2}"
    );

    // Unshim: dir gone, profile restored byte-for-byte. Idempotent.
    let (_, stderr3, code3) = run(&nub_binary(), &["pm", "unshim"], &home, &env);
    assert_eq!(code3, 0, "nub pm unshim must succeed; stderr:\n{stderr3}");
    assert!(!shims.exists(), "the shim dir must be removed");
    assert_eq!(
        std::fs::read_to_string(&zshrc).unwrap(),
        original,
        "unshim must strip exactly the block it wrote"
    );
    let (_, _, code4) = run(&nub_binary(), &["pm", "unshim"], &home, &env);
    assert_eq!(code4, 0, "a second unshim is a clean no-op");
}

/// Real-network e2e: a shim-invoked bare `pnpm` in a pnpm-pinned project
/// provisions the pinned version into a fresh store and runs it under the
/// project's Node.
///   cargo test -p nub-cli --test pm_shim -- --ignored
#[test]
#[ignore = "network: provisions real pnpm@9.12.3 through the shim"]
fn shim_invoked_pnpm_runs_the_pinned_version_for_real() {
    let work = tmp("e2e");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("package.json"),
        r#"{"packageManager":"pnpm@9.12.3"}"#,
    )
    .unwrap();
    let cache = work.join("cache");
    let link = shim_link(&work, "pnpm");

    let path = std::env::var("PATH").unwrap();
    let (stdout, stderr, code) = run(
        &link,
        &["--version"],
        &proj,
        &[
            ("PATH", path.as_str()),
            ("XDG_CACHE_HOME", cache.to_str().unwrap()),
        ],
    );
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(
        stdout.trim(),
        "9.12.3",
        "bare pnpm must run the PINNED version, whatever the shell has"
    );
}

#[test]
fn empty_path_entry_with_cwd_at_the_shim_does_not_loop() {
    // An empty PATH entry means cwd in POSIX lookup. With cwd == the dir
    // holding the shim link, an unguarded scan would resolve `pnpm` to the
    // shim itself and exec it forever — the recursion-guard hole from review.
    // The guarded scan skips the empty entry and lands on the system fake.
    let work = tmp("empty-entry");
    std::fs::write(work.join("package.json"), r#"{"name":"app"}"#).unwrap();
    let sys = work.join("sys");
    std::fs::create_dir_all(&sys).unwrap();
    let fake = fake_pm(&sys, "pnpm");
    let link = shim_link(&work, "pnpm");

    let path_var = format!(":{}", sys.display()); // leading EMPTY entry
    let (stdout, stderr, code) = run_with_timeout(
        &link,
        &["install"],
        &work, // cwd = the dir containing the shim link
        &[
            ("PATH", path_var.as_str()),
            ("HOME", work.to_str().unwrap()),
        ],
        10,
    );
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(
        stdout,
        format!("FAKE:{}:install\n", fake.display()),
        "the empty entry (cwd, where the shim lives) must be skipped"
    );
}

#[test]
fn nub_from_the_shim_dir_defers_to_the_real_nub_on_path() {
    // Post-`nub pm shim`, ~/.nub/shims is first on PATH and carries a `nub`
    // hardlink. After an upgrade swaps the official binary, that hardlink
    // still pins the OLD bytes — invoked as `nub`, the shim-dir copy must
    // re-exec the real (different-inode) nub found past the shim dir, or
    // upgrades never take effect (including the `nub pm shim` re-link itself).
    let home = tmp("nub-passthrough");
    let shims = home.join(".nub").join("shims");
    std::fs::create_dir_all(&shims).unwrap();
    let shim_nub = shim_link(&shims, "nub");
    // The "freshly upgraded" official nub: a fake that proves it ran (a real
    // upgraded binary would be a different inode just the same).
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let official = fake_pm(&bin, "nub");

    let path_var = std::env::join_paths([shims.clone(), bin.clone()]).unwrap();
    let (stdout, _, code) = run(
        &shim_nub,
        &["pm", "cache"],
        &home,
        &[
            ("PATH", path_var.to_str().unwrap()),
            ("HOME", home.to_str().unwrap()),
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(
        stdout,
        format!("FAKE:{}:pm cache\n", official.display()),
        "the shim-dir nub must exec the real nub with argv intact"
    );

    // Post-uninstall (no other nub anywhere): the shim-dir nub runs ITSELF —
    // `nub pm unshim` must keep working with the official binary gone.
    let path_var = std::env::join_paths([shims.clone()]).unwrap();
    let (_, stderr, code) = run(
        &shim_nub,
        &["pm", "unshim"],
        &home,
        &[
            ("PATH", path_var.to_str().unwrap()),
            ("HOME", home.to_str().unwrap()),
        ],
    );
    assert_eq!(
        code, 0,
        "unshim must work from the shim-dir nub alone; stderr:\n{stderr}"
    );
    assert!(!shims.exists(), "the shim dir is removed");
}

#[test]
fn name_only_pin_prefers_the_system_pm_over_per_run_resolution() {
    // devEngines.packageManager with a name but NO version constrains the
    // NAME only. Running the user's own matching PM is zero-network and
    // drift-free; resolving `latest` would hit the registry on every bare
    // invocation and change behavior as releases publish.
    let work = tmp("name-only");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("package.json"),
        r#"{"devEngines":{"packageManager":{"name":"pnpm"}}}"#,
    )
    .unwrap();
    let sys = work.join("sys");
    std::fs::create_dir_all(&sys).unwrap();
    let fake = fake_pm(&sys, "pnpm");
    let link = shim_link(&work, "pnpm");

    let (stdout, stderr, code) = run(
        &link,
        &["run", "build"],
        &proj,
        &[
            ("PATH", sys.to_str().unwrap()),
            ("HOME", work.to_str().unwrap()),
        ],
    );
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(
        stdout,
        format!("FAKE:{}:run build\n", fake.display()),
        "a name-only pin runs the system pnpm — no registry, no drift"
    );

    // Still a NAME pin: a mismatched PM refuses exactly as an exact pin would.
    fake_pm(&sys, "npm");
    let npm_link = shim_link(&work, "npm");
    let (_, stderr, code) = run(
        &npm_link,
        &["install"],
        &proj,
        &[
            ("PATH", sys.to_str().unwrap()),
            ("HOME", work.to_str().unwrap()),
        ],
    );
    assert_eq!(code, 1, "name-only pins still gate the PM name");
    assert!(
        stderr.contains("pnpm"),
        "the refusal names the pinned PM:\n{stderr}"
    );
}
