//! The two-mode model, behaviorally, through the binary: `pm use nub`'s
//! offline migration invariants, the role-first lifecycle UA, and the
//! nub-identity config gating (stray-yaml warning). All rows run OFFLINE —
//! `pm use nub` never touches a registry by design, the install rows use
//! empty-dependency manifests, and every project points its registry at a
//! dead port so accidental network fails loudly. The online halves (real
//! pnpm judging the reversed state) live in tests/aube-conformance (the
//! `nub` format leg) and tests/brand-sweep.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn project(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-two-mode-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
    }
    dir
}

fn run(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        // Fixtures pin a differing `nub@<v>` to exercise nub identity, not the
        // self-shim — opt out so a PM verb doesn't try to provision that nub.
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

const EMPTY_LOCK: &str = "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n";

/// `pm use nub` on a single-package pnpm project carrying a catalog: the
/// whole switch is offline, the yaml dies, the catalog lands as a
/// packages-less `workspaces` object (the Bun shape), settings land in
/// `.npmrc`, and the lockfile is renamed byte-identically. Rerunning is a
/// no-op (idempotence is the contract).
#[test]
fn use_nub_migrates_a_single_package_catalog_project_offline_and_idempotently() {
    let dir = project(
        "use-nub",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@10.0.0"}"#,
            ),
            ("pnpm-lock.yaml", EMPTY_LOCK),
            (
                "pnpm-workspace.yaml",
                "catalog:\n  left-pad: 1.3.0\nminimumReleaseAge: 1440\nproduction: true\n",
            ),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["pm", "use", "nub"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    // Zero pnpm-named files; nub.lock carries the exact prior bytes.
    assert!(!dir.join("pnpm-workspace.yaml").exists(), "{stdout}");
    assert!(!dir.join("pnpm-lock.yaml").exists(), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(dir.join("nub.lock")).unwrap(),
        EMPTY_LOCK,
        "the rename must be byte-identical"
    );

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    // Bare `pm use nub` is the non-locking switch: the incumbent pnpm@ pin is
    // cleared and only the devEngines caret range is written — never a hard
    // `packageManager: nub@<v>` pin (that is `pm use nub@<exact>`'s opt-in).
    assert!(
        manifest.get("packageManager").is_none(),
        "bare pm use nub writes no exact packageManager pin: {manifest}"
    );
    assert_eq!(manifest["devEngines"]["packageManager"]["name"], "nub");
    assert_eq!(manifest["devEngines"]["packageManager"]["onFail"], "ignore");
    assert_eq!(
        manifest["devEngines"]["packageManager"]["version"],
        format!("^{}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        manifest["workspaces"]["catalog"]["left-pad"], "1.3.0",
        "single-package catalogs land as a packages-less workspaces object"
    );
    assert!(
        manifest["workspaces"].get("packages").is_none(),
        "no packages key must be invented for a single-package repo"
    );

    let npmrc = std::fs::read_to_string(dir.join(".npmrc")).unwrap();
    assert!(
        npmrc.contains("minimum-release-age=1440"),
        "settings must land in .npmrc: {npmrc}"
    );
    assert!(
        stdout.contains("production"),
        "the warn tail must name the transient key loudly: {stdout}"
    );
    // The corepack line is gated to the exact-pin path; the pnpm-refuses line is
    // the always-present consequence for the bare switch.
    assert!(
        stdout.contains("nub pm use pnpm") && !stdout.contains("corepack"),
        "bare switch carries the consequences block without the corepack (exact-pin) line: {stdout}"
    );

    // Idempotent rerun: same identity, lockfile kept, nothing new to migrate.
    let (stdout2, stderr2, code2) = run(&dir, &["pm", "use", "nub"]);
    assert_eq!(code2, 0, "stdout: {stdout2}\nstderr: {stderr2}");
    assert!(
        stdout2.contains("nub.lock: kept"),
        "rerun must keep, not re-convert: {stdout2}"
    );
}

/// Crash-recovery for the one half-migrated window `use nub` can be killed in:
/// the manifest edit (atomic — devEngines range written, the yaml's catalog
/// already copied into `workspaces`, the `pnpm` namespace dropped) and the
/// lockfile rename both completed, but the process died BEFORE the final
/// `pnpm-workspace.yaml` deletion. The project then declares nub identity yet
/// still carries the stray yaml. The recovery contract is re-run idempotence,
/// not atomicity: a second `use nub` reads the leftover yaml, re-derives the
/// (already-present) migration, deletes the yaml, and lands in clean nub
/// identity with the catalog data intact — never silently dropped. We build
/// the half-state directly rather than racing a real SIGKILL (the command is
/// too fast to interrupt mid-write reliably).
#[test]
fn use_nub_recovers_from_a_crash_before_the_yaml_deletion() {
    let dir = project(
        "use-nub-halfstate",
        &[
            // Manifest as the atomic (bare) edit would have left it: nub identity
            // via the devEngines range (no exact packageManager), catalog migrated
            // into the workspaces object, no pnpm namespace.
            (
                "package.json",
                &format!(
                    r#"{{"name":"app","version":"1.0.0",{}}}"#,
                    r#""devEngines":{"packageManager":{"name":"nub","version":"^0.0.0","onFail":"warn"}},"workspaces":{"catalog":{"left-pad":"1.3.0"}}"#
                ),
            ),
            // The rename already happened: nub.lock present, no pnpm-lock.yaml.
            ("nub.lock", EMPTY_LOCK),
            // The leftover the crash never got to delete — still carrying the
            // catalog that the atomic manifest edit already preserved.
            ("pnpm-workspace.yaml", "catalog:\n  left-pad: 1.3.0\n"),
        ],
    );

    let (stdout, stderr, code) = run(&dir, &["pm", "use", "nub"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    // The leftover yaml is gone; nub.lock is kept (not re-renamed); the
    // project is now in clean, fully-migrated nub identity.
    assert!(
        !dir.join("pnpm-workspace.yaml").exists(),
        "the recovery run must delete the stray yaml: {stdout}"
    );
    assert!(
        dir.join("nub.lock").is_file() && !dir.join("pnpm-lock.yaml").exists(),
        "nub.lock stays the lockfile, untouched: {stdout}"
    );

    // The migrated data survived the half-state — never silently dropped.
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert!(
        manifest.get("packageManager").is_none(),
        "bare use nub leaves no exact packageManager pin after recovery: {manifest}"
    );
    assert_eq!(
        manifest["devEngines"]["packageManager"]["version"],
        format!("^{}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        manifest["workspaces"]["catalog"]["left-pad"], "1.3.0",
        "the catalog must remain in the manifest after recovery"
    );
    assert!(
        manifest.get("pnpm").is_none(),
        "the pnpm namespace stays removed"
    );
}

/// Plain `dependenciesMeta.injected` deps no longer block the switch: the
/// engine materializes the hard-copy peer closure on install under nub
/// identity exactly as it did under pnpm, so migrating does not change install
/// semantics. The migration completes and the dependenciesMeta is preserved
/// byte-for-byte in the manifest.
#[test]
fn use_nub_migrates_with_injected_deps_and_preserves_meta() {
    let dir = project(
        "injected",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@10.0.0","dependenciesMeta":{"sibling":{"injected":true}}}"#,
            ),
            ("pnpm-lock.yaml", EMPTY_LOCK),
            ("pnpm-workspace.yaml", "packages:\n  - \"packages/*\"\n"),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["pm", "use", "nub"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    // The switch completed: yaml renamed, identity flipped, pnpm namespace gone.
    assert!(
        dir.join("nub.lock").is_file()
            && !dir.join("pnpm-lock.yaml").exists()
            && !dir.join("pnpm-workspace.yaml").exists(),
        "the migration must complete the lockfile rename + yaml deletion: {stdout}"
    );
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert!(
        manifest.get("packageManager").is_none(),
        "bare pm use nub writes no exact packageManager pin: {manifest}"
    );
    assert_eq!(
        manifest["devEngines"]["packageManager"]["version"],
        format!("^{}", env!("CARGO_PKG_VERSION"))
    );
    // The injected dependenciesMeta survives untouched — the install path honors it.
    assert_eq!(
        manifest["dependenciesMeta"]["sibling"]["injected"], true,
        "dependenciesMeta.injected must be preserved through the migration: {manifest}"
    );
}

/// Under nub identity a stray pnpm-workspace.yaml is ignore-with-warning:
/// exactly one warning naming it unread plus the remedies, and the install
/// proceeds against nub.lock.
#[test]
fn stray_workspace_yaml_under_nub_identity_warns_once_and_install_proceeds() {
    let dir = project(
        "stray-yaml",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"nub@0.0.1"}"#,
            ),
            ("nub.lock", EMPTY_LOCK),
            ("pnpm-workspace.yaml", "nodeLinker: hoisted\n"),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        stderr
            .matches("pnpm-workspace.yaml is not read under nub identity")
            .count(),
        1,
        "exactly one warning: {stderr}"
    );
    assert!(
        stderr.contains("nub pm use pnpm") && stderr.contains("nub pm use nub"),
        "the warning must carry both remedies: {stderr}"
    );
    assert!(
        dir.join("nub.lock").is_file() && !dir.join("pnpm-lock.yaml").exists(),
        "nub.lock stays the lockfile"
    );
}

/// The role-first lifecycle UA, observed by a real root postinstall: a
/// pnpm-declared project is served pnpm-first at the PINNED version with the
/// nub token second; a nub-identity project is nub-first in the runner
/// dialect. (The fresh + engine-parity cases live in tests/brand-sweep and
/// the pm_engine unit tests.)
#[test]
fn lifecycle_ua_is_pnpm_first_in_compat_and_nub_first_under_nub_identity() {
    let postinstall = r#""scripts":{"postinstall":"node -e \"require('fs').writeFileSync('ua.txt', process.env.npm_config_user_agent||'')\""}"#;

    let dir = project(
        "ua-compat",
        &[(
            "package.json",
            &format!(
                r#"{{"name":"app","version":"1.0.0","packageManager":"pnpm@9.9.9",{postinstall}}}"#
            ),
        )],
    );
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let ua = std::fs::read_to_string(dir.join("ua.txt")).expect("postinstall must run");
    assert!(
        ua.starts_with(&format!(
            "pnpm/9.9.9 nub/{} node/v",
            env!("CARGO_PKG_VERSION")
        )),
        "compat UA must be pnpm-first at the pinned version, nub second: {ua}"
    );

    let dir = project(
        "ua-nub",
        &[
            (
                "package.json",
                &format!(
                    r#"{{"name":"app","version":"1.0.0","packageManager":"nub@0.0.1",{postinstall}}}"#
                ),
            ),
            ("nub.lock", EMPTY_LOCK),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let ua = std::fs::read_to_string(dir.join("ua.txt")).expect("postinstall must run");
    assert!(
        ua.starts_with(&format!("nub/{} npm/? node/v", env!("CARGO_PKG_VERSION"))),
        "nub-identity UA must be nub-first in the runner dialect: {ua}"
    );
}

/// An empty-dependency, in-sync npm v3 package-lock — converts to nub.lock
/// offline (no graph to fetch), exercising the npm→nub `Convert` path.
const EMPTY_NPM_LOCK: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": { "": { "name": "app", "version": "1.0.0" } }
}
"#;

/// The phantom-dependency layout-change warning (writeup §6): switching a
/// project FROM a hoisting PM (npm/yarn — flat node_modules) to nub's isolated
/// layout can break undeclared imports, so `pm use nub` warns. The warning is
/// gated to npm/yarn only — pnpm/bun are already isolated, and a fresh project
/// has no incumbent layout to change. stderr is a pipe here, so the text is
/// plain (no ANSI), matching a NO_COLOR / non-terminal shell.
#[test]
fn use_nub_warns_about_phantom_deps_only_when_leaving_a_hoisting_pm() {
    let pkg = r#"{"name":"app","version":"1.0.0"}"#;

    // npm incumbent → the layout-change warning fires.
    let npm = project(
        "phantom-npm",
        &[("package.json", pkg), ("package-lock.json", EMPTY_NPM_LOCK)],
    );
    let (stdout, stderr, code) = run(&npm, &["pm", "use", "nub"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("isolated node_modules")
            && stderr.contains("phantom dependencies")
            && stderr.contains("npm and yarn"),
        "npm→nub must warn that the isolated layout can break phantom deps: {stderr}"
    );

    // pnpm incumbent → already isolated, no phantom warning.
    let pnpm = project(
        "phantom-pnpm",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@10.0.0"}"#,
            ),
            ("pnpm-lock.yaml", EMPTY_LOCK),
        ],
    );
    let (stdout, stderr, code) = run(&pnpm, &["pm", "use", "nub"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !stderr.contains("phantom dependencies"),
        "pnpm is already non-hoisting — no phantom-deps warning: {stderr}"
    );

    // Fresh project (no lockfile) → no incumbent layout, no warning.
    let fresh = project("phantom-fresh", &[("package.json", pkg)]);
    let (stdout, stderr, code) = run(&fresh, &["pm", "use", "nub"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !stderr.contains("phantom dependencies"),
        "a fresh project has no incumbent layout — no phantom-deps warning: {stderr}"
    );
}

/// A `.pnpmfile.cjs` is pnpm-proprietary AND shapes resolution (its
/// hooks rewrite the dep graph), so under a non-pnpm incumbent it's
/// another tool's config and must not be honored. A `preResolution` hook
/// writes a marker file when it runs — the cleanest cross-tool "did the
/// hook fire?" signal. `--no-frozen-lockfile` forces the resolve so the
/// hook actually gets a chance to run (a frozen/already-current install
/// short-circuits before pnpmfile detection).
///
/// nub identity: the cwd-default `.pnpmfile` is gated off silently. Unlike
/// `pnpm-workspace.yaml`, this stray pnpm-named file intentionally gets no
/// warning under nub identity.
#[test]
fn pnpmfile_ignored_silently_under_nub_identity() {
    let hook = r#"module.exports = { hooks: { preResolution(ctx) { require('fs').writeFileSync('hook-ran.txt', 'yes'); return ctx; } } };"#;
    let dir = project(
        "pnpmfile-nub",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"nub@0.0.1"}"#,
            ),
            ("nub.lock", EMPTY_LOCK),
            (".pnpmfile.cjs", hook),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["install", "--no-frozen-lockfile"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !dir.join("hook-ran.txt").exists(),
        "the cwd-default .pnpmfile must NOT run under nub identity: {stderr}"
    );
    assert!(
        !stderr.contains(".pnpmfile") && !stderr.contains("pnpmfile"),
        "nub identity must not warn about the default .pnpmfile: {stderr}"
    );
}

/// npm incumbent: the cwd-default `.pnpmfile` is gated off — the hook
/// never runs and exactly one dim warning names the file + the incumbent.
#[test]
fn pnpmfile_ignored_under_npm_incumbent_with_one_warning() {
    let hook = r#"module.exports = { hooks: { preResolution(ctx) { require('fs').writeFileSync('hook-ran.txt', 'yes'); return ctx; } } };"#;
    let dir = project(
        "pnpmfile-npm",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"npm@10.0.0"}"#,
            ),
            (
                "package-lock.json",
                r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"app","version":"1.0.0"}}}"#,
            ),
            (".pnpmfile.cjs", hook),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["install", "--no-frozen-lockfile"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !dir.join("hook-ran.txt").exists(),
        "the cwd-default .pnpmfile must NOT run under an npm incumbent: {stderr}"
    );
    assert_eq!(
        stderr.matches(".pnpmfile.cjs` ignored").count(),
        1,
        "exactly one ignore warning naming the file: {stderr}"
    );
    assert!(
        stderr.contains("this project uses npm")
            && stderr.contains("--pnpmfile")
            && stderr.contains("nub pm use pnpm"),
        "the warning names the incumbent and both escape hatches: {stderr}"
    );
}

/// pnpm incumbent: the cwd-default `.pnpmfile` is honored exactly as
/// upstream — the hook runs and there is no ignore warning. This is the
/// pnpm "special relationship": its proprietary config stays live when
/// pnpm is the incumbent.
#[test]
fn pnpmfile_honored_under_pnpm_incumbent_without_warning() {
    let hook = r#"module.exports = { hooks: { preResolution(ctx) { require('fs').writeFileSync('hook-ran.txt', 'yes'); return ctx; } } };"#;
    let dir = project(
        "pnpmfile-pnpm",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@9.9.9"}"#,
            ),
            ("pnpm-lock.yaml", EMPTY_LOCK),
            (".pnpmfile.cjs", hook),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["install", "--no-frozen-lockfile"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("hook-ran.txt").is_file(),
        "the cwd-default .pnpmfile must run under a pnpm incumbent: {stderr}"
    );
    assert_eq!(
        stderr.matches("ignored").count(),
        0,
        "no ignore warning when pnpm is the incumbent: {stderr}"
    );
}

/// `nub pm pin` — the lightweight lock. It writes ONLY the two nub identity
/// fields (the exact corepack `packageManager: nub@<v>` pin that arms the
/// self-shim, plus the beside-it devEngines caret) and touches nothing else:
/// no lockfile, no pnpm-workspace.yaml, no settings migration, and — unlike the
/// heavier `pm use nub` switch — NOT a project's `pnpm.*` config. Bare pins the
/// running nub; an explicit exact version pins that and notes the delegation; a
/// range/tag is refused; a missing manifest bails; reruns are idempotent. All
/// offline — the pin never touches the network.
#[test]
fn pm_pin_locks_only_the_nub_identity_fields_offline() {
    let running = env!("CARGO_PKG_VERSION");
    let manifest = |d: &Path| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(d.join("package.json")).unwrap()).unwrap()
    };

    // A project carrying an unrelated pnpm.* config block: pin must NOT strip it
    // (the full `use nub` migration moves then removes it; pin leaves it alone).
    let dir = project(
        "pm-pin",
        &[(
            "package.json",
            r#"{"name":"app","version":"1.0.0","pnpm":{"overrides":{"left-pad":"1.3.0"}}}"#,
        )],
    );

    // (a) bare `nub pm pin` locks the running nub — both fields, nothing else.
    let (stdout, stderr, code) = run(&dir, &["pm", "pin"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let m = manifest(&dir);
    assert_eq!(m["packageManager"], format!("nub@{running}"));
    assert_eq!(m["devEngines"]["packageManager"]["name"], "nub");
    assert_eq!(
        m["devEngines"]["packageManager"]["version"],
        format!("^{running}")
    );
    assert_eq!(m["devEngines"]["packageManager"]["onFail"], "ignore");
    assert_eq!(
        m["pnpm"]["overrides"]["left-pad"], "1.3.0",
        "pin must not touch the pnpm.* config it does not migrate: {m}"
    );
    assert!(
        !stdout.contains("note:"),
        "pinning the running nub emits no delegation note: {stdout}"
    );

    // (e) idempotent rerun — same state, exit 0.
    let (stdout2, stderr2, code2) = run(&dir, &["pm", "pin"]);
    assert_eq!(code2, 0, "stdout: {stdout2}\nstderr: {stderr2}");
    assert_eq!(manifest(&dir)["packageManager"], format!("nub@{running}"));

    // (b) an explicit exact version (≠ running) pins THAT and announces the
    // delegation; the `nub@` prefix is forgiven (pin nub@<v> == pin <v>).
    let (stdout3, stderr3, code3) = run(&dir, &["pm", "pin", "nub@0.0.1"]);
    assert_eq!(code3, 0, "stdout: {stdout3}\nstderr: {stderr3}");
    let m3 = manifest(&dir);
    assert_eq!(m3["packageManager"], "nub@0.0.1");
    assert_eq!(m3["devEngines"]["packageManager"]["version"], "^0.0.1");
    assert!(
        stdout3.contains("note:") && stdout3.contains("0.0.1"),
        "a pin off the running version announces the provision+delegate: {stdout3}"
    );

    // (c) a range or dist-tag is refused — nub is the running binary, not a
    // registry package, so there is nothing to resolve a non-exact spec against.
    for bad in ["^1", "1.x", "next", "latest"] {
        let (o, e, c) = run(&dir, &["pm", "pin", bad]);
        assert_ne!(c, 0, "`pin {bad}` must fail: {o}\n{e}");
        assert!(
            e.contains("needs an exact version") && e.contains("running binary"),
            "`pin {bad}` must name the exact-version rule: {e}"
        );
    }
    // The failed pins wrote nothing — the last good pin still stands.
    assert_eq!(manifest(&dir)["packageManager"], "nub@0.0.1");

    // (d) no package.json → the write has no home, so it bails cleanly.
    let empty = project("pm-pin-nomanifest", &[]);
    let (o, e, c) = run(&empty, &["pm", "pin"]);
    assert_ne!(c, 0, "pin with no manifest must fail: {o}\n{e}");
    assert!(
        e.contains("no package.json found"),
        "the missing-manifest bail must name the cause: {e}"
    );
}
