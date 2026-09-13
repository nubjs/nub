//! The PM identity decision table, behaviorally, through the binary
//! (spec: `identity-policy` (no such document)). Identity resolution is the
//! engine's declaration-aware policy (pin-over-inference, Axiom 1), wired
//! into nub's engine preflight; the contradiction/ambiguity rows render
//! nub-side with the rewritten stable codes and the `nub pm use` remedy.
//!
//! All rows run OFFLINE: the lockfile-writing rows use empty-dependency
//! manifests (nothing to resolve, but the lockfile still lands — pointing
//! the registry at a dead port proves no network is involved), and the
//! error rows fail in preflight before any resolution.

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
/// so manifest/lockfile walk-ups can't escape into stray ancestors). The
/// `.npmrc` dead-port registry makes any accidental network use fail loudly.
fn project(tag: &str, manifest: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-pm-identity-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("package.json"), manifest).unwrap();
    std::fs::write(dir.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    dir
}

/// Spawn `nub <args>` in `dir` with the engine store/cache isolated to fresh
/// temp roots.
fn run(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        // These fixtures pin a differing `nub@<v>` to exercise nub identity, not
        // the self-shim — opt out so a PM verb doesn't try to provision that nub.
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

const EMPTY_PNPM: &str = r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@9.1.0"}"#;

#[test]
fn nub_defaults_to_a_strict_24_hour_release_age_floor() {
    let dir = project("release-age-default", r#"{"name":"app","version":"1.0.0"}"#);
    let (age, stderr, code) = run(&dir, &["config", "get", "minimumReleaseAge"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(age.trim(), "1440");
    let (strict, stderr, code) = run(&dir, &["config", "get", "minimumReleaseAgeStrict"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(strict.trim(), "true");

    std::fs::write(
        dir.join(".npmrc"),
        "registry=http://127.0.0.1:1/\nminimumReleaseAge=0\nminimumReleaseAgeStrict=false\n",
    )
    .unwrap();
    let (age, stderr, code) = run(&dir, &["config", "get", "minimumReleaseAge"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(age.trim(), "0", "explicit project config must still win");
    let (strict, stderr, code) = run(&dir, &["config", "get", "minimumReleaseAgeStrict"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        strict.trim(),
        "false",
        "explicit project config must still win"
    );
}

/// A truly-fresh project is nub's: an empty-deps install writes `nub.lock`
/// without any network, and stamps the cross-tool signal that lockfile
/// deliberately withholds.
///
/// The declared-npm arm is gone with npm incumbency. It asserted that
/// `packageManager: npm@11.0.0` made a fresh install write a
/// `package-lock.json`; nub writes no npm lockfile at all now, and a foreign
/// declaration confers no identity, so the project is nub's and the hint is
/// what points at the conversion.
#[test]
fn a_truly_fresh_project_writes_nub_lock_and_stamps_the_signal() {
    // none + none → truly fresh: nub claims identity via the neutral lockfile
    // (writes nub.lock) AND stamps a caret RANGE into `devEngines.packageManager`
    // — the non-locking PM signal nub's unbranded nub.lock withholds, the
    // coherent counterpart to keeping the lockfile neutral. Never the exact
    // `packageManager: nub@<v>` pin (that hard claim is `nub pm use nub@<exact>`'s).
    let dir = project("fresh-default", r#"{"name":"app","version":"1.0.0"}"#);
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("nub.lock").is_file(),
        "truly-fresh install must write nub's neutral nub.lock: {stderr}"
    );
    assert!(
        !dir.join("pnpm-lock.yaml").exists(),
        "no pnpm-lock.yaml on the truly-fresh nub-identity path"
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
        "a virgin install stamps a devEngines.packageManager caret range: {manifest}"
    );
    assert!(
        manifest.get("packageManager").is_none(),
        "the virgin stamp writes only the devEngines range, never the exact packageManager pin: {manifest}"
    );
}

/// Row "none|exactly one → that identity": an undeclared project keeps its
/// single lockfile's format, and a declared project keeps its own lockfile
/// even with a stray other-format file next to it (declaration wins; the
/// stray is ignored, not adopted).
#[test]
fn a_single_lockfile_infers_the_identity_and_a_declaration_outranks_strays() {
    let npm_lock = r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"app","version":"1.0.0"}}}"#;

    let dir = project("infer-npm", r#"{"name":"app","version":"1.0.0"}"#);
    std::fs::write(dir.join("package-lock.json"), npm_lock).unwrap();
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("package-lock.json").is_file() && !dir.join("pnpm-lock.yaml").exists(),
        "a lone package-lock.json keeps the npm identity: {stderr}"
    );

    // Declared pnpm + pnpm-lock.yaml + stray package-lock.json → pnpm wins,
    // the stray is left alone (removal is `nub pm use`'s job, not install's).
    let dir = project("declared-vs-stray", EMPTY_PNPM);
    std::fs::write(
        dir.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n",
    )
    .unwrap();
    std::fs::write(dir.join("package-lock.json"), npm_lock).unwrap();
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("pnpm-lock.yaml").is_file() && dir.join("package-lock.json").is_file(),
        "the declared format is used; the stray is not deleted by install"
    );
}

/// A DECLARED project is never ambiguous: the declaration says who owns it,
/// so a stray second lockfile no longer refuses the install. `nub install`
/// writes `devEngines.packageManager: nub` itself, so leaving this ambiguous
/// made the state inescapable — the remedy was already applied. Regression for
/// the state a hosted builder manufactures when it runs its own install
/// (bun/npm) beside a committed `nub.lock`.
#[test]
fn a_nub_declared_project_resolves_past_a_stray_lockfile() {
    let dir = project(
        "declared-stray",
        r#"{"name":"app","version":"1.0.0","devEngines":{"packageManager":{"name":"nub","version":"^0.6.0","onFail":"warn"}}}"#,
    );
    std::fs::write(dir.join("nub.lock"), "lockfileVersion: '9.0'\n").unwrap();
    // A REAL bun lockfile, not a `{}` placeholder: resolving past the stray
    // means the stray still gets parsed, so an unparseable one fails the
    // install on its own merits rather than on identity.
    std::fs::write(
        dir.join("bun.lock"),
        r#"{"lockfileVersion":1,"workspaces":{"":{"name":"app"}},"packages":{}}"#,
    )
    .unwrap();
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "a declared project must install: {stderr}{stdout}");
    assert!(
        !stderr.contains("ERR_NUB_LOCKFILE_AMBIGUOUS"),
        "a declaration resolves ownership — no ambiguity: {stderr}"
    );
    assert!(
        dir.join("nub.lock").is_file(),
        "the canonical lockfile stays the project's own"
    );
}

/// The over-scope regression (maintainer report 2026-06-26): the ambiguity
/// guard belongs ONLY to the mutating install family that writes a lockfile.
/// A TRANSIENT fetch-and-run — `nubx <tool>` / `nub dlx <tool>` — never touches
/// the project's lockfile, so a multi-lockfile project must run it without the
/// `ERR_NUB_LOCKFILE_AMBIGUOUS` hard error (matching `npx`/`pnpm dlx`/`bunx`).
/// The mutating-side guard stays proven by
/// [`undeclared_multi_lockfile_projects_error_as_ambiguous`].
#[test]
fn transient_runs_do_not_error_on_multi_lockfile_projects() {
    let dir = project("ambiguous-transient", r#"{"name":"app","version":"1.0.0"}"#);
    std::fs::write(
        dir.join("package-lock.json"),
        r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();
    // These arms get PAST identity and reach the (dead-port) registry; drop the
    // retry backoff so the fetch fails fast instead of sleeping ~70s.
    std::fs::write(
        dir.join(".npmrc"),
        "registry=http://127.0.0.1:1/\nfetch-retries=0\n",
    )
    .unwrap();

    // `nub dlx <tool>`: the dead-port registry (see `project`) makes the fetch
    // itself fail, but the point is the command gets PAST identity resolution —
    // no ambiguity hard-error in preflight, where it used to die.
    let (_, stderr, _) = run(&dir, &["dlx", "cowsay"]);
    assert!(
        !stderr.contains("ERR_NUB_LOCKFILE_AMBIGUOUS"),
        "dlx must not raise the ambiguity guard in a multi-lockfile project: {stderr}"
    );
    // Positive proof it cleared the identity preflight and reached the (dead-port)
    // registry, rather than no-op-passing on some unrelated early exit.
    assert!(
        stderr.contains("ERR_NUB_REGISTRY_ERROR"),
        "dlx should get past identity to the registry fetch: {stderr}"
    );

    // The reported repro, exactly: invoked as `nubx`. argv0 dispatch selects the
    // nubx entry point from a symlink named `nubx`. (Unix-only — Windows symlink
    // creation needs privilege; the `dlx` arm above already covers the same
    // transient session on every platform.)
    #[cfg(unix)]
    {
        let nubx = dir.join("nubx");
        std::os::unix::fs::symlink(nub_binary(), &nubx).unwrap();
        // `-y` clears the registry-consent gate: this test runs non-interactively
        // (no TTY), where `nubx` now fails closed by default. The escape hatch lets
        // it proceed to the fetch, which is the property under test (identity
        // preflight cleared → reaches the dead-port registry).
        let out = Command::new(&nubx)
            .args(["-y", "cowsay", "hi"])
            .current_dir(&dir)
            .env("XDG_DATA_HOME", dir.join("xdg-data"))
            .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
            .output()
            .expect("failed to spawn nubx");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("ERR_NUB_LOCKFILE_AMBIGUOUS"),
            "nubx must not raise the ambiguity guard in a multi-lockfile project: {stderr}"
        );
        // Confirms argv0 actually routed to the nubx DLX fallback (not an
        // unrelated help/early exit that would no-op-pass the assert above).
        assert!(
            stderr.contains("ERR_NUB_REGISTRY_ERROR"),
            "nubx should get past identity to the registry fetch: {stderr}"
        );
    }
}

/// Global-scope commands — the ones that read the global store, the config or
/// the registry and never the project lockfile — succeed in a project littered
/// with other package managers' lockfiles, and print their datum.
///
/// The contrast this used to draw is gone with the thing it contrasted against.
/// It paired these against the project-graph readers (`why`, `add`), which
/// raised a loud `ERR_NUB_LOCKFILE_AMBIGUOUS` on the same fixture — two
/// lockfiles and no declaration. Neither of those lockfiles confers an identity
/// any more, so there is no ambiguity left for anything to raise, and what
/// survives is the half that was always about scope: a global read does not
/// care what the project is.
#[test]
fn global_scope_commands_succeed_beside_foreign_lockfiles() {
    let dir = project("ambiguous-global", r#"{"name":"app","version":"1.0.0"}"#);
    std::fs::write(
        dir.join("package-lock.json"),
        r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();

    for args in [
        &["store", "path"][..],
        &["config", "get", "registry"],
        &["bin"],
        &["root"],
    ] {
        let (stdout, stderr, code) = run(&dir, args);
        assert_eq!(
            code,
            0,
            "`nub {}` must succeed in a multi-lockfile project: {stderr}",
            args.join(" ")
        );
        assert!(
            !stdout.trim().is_empty(),
            "`nub {}` should print its datum: stdout empty",
            args.join(" ")
        );
    }
}

/// The internal-re-entry corner of the #197/#199 class (#489): the node-gyp
/// bootstrap runs a recursive `nub install` inside the PM cache
/// (`$XDG_CACHE_HOME/nub/pm/tools/node-gyp/<bucket>`), whose first run has a
/// manifest but no lockfile yet. The identity walk-up must stop at nub's cache
/// root instead of climbing into ancestor dirs ($HOME on a real machine) and
/// hard-failing on lockfile ambiguity that has nothing to do with the install.
#[test]
fn cache_scratch_installs_never_inherit_ambient_identity() {
    // Ambiguous ANCESTOR of the cache root — two lockfile families, as a
    // $HOME with leftovers from unrelated experiments would have.
    let dir = project("cache-clamp", r#"{"name":"app","version":"1.0.0"}"#);
    std::fs::write(
        dir.join("package-lock.json"),
        r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();

    // The tool dir exactly as `bootstrap_blocking` shapes it: manifest +
    // empty workspace-yaml stub, no lockfile. The dead-port `.npmrc` proves
    // the install gets past identity to the resolve phase.
    let tool_dir = dir
        .join("xdg-cache")
        .join("nub")
        .join("pm")
        .join("tools")
        .join("node-gyp")
        .join("v12");
    std::fs::create_dir_all(&tool_dir).unwrap();
    std::fs::write(
        tool_dir.join("package.json"),
        r#"{"name":"aube-tool-node-gyp","private":true,"dependencies":{"node-gyp":"^12.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(tool_dir.join("pnpm-workspace.yaml"), "").unwrap();
    std::fs::write(
        tool_dir.join(".npmrc"),
        "registry=http://127.0.0.1:1/\nfetch-retries=0\n",
    )
    .unwrap();

    // The exact recursive invocation the bootstrap spawns — cwd is the tool
    // dir while XDG_CACHE_HOME points at the cache root above it (the layout
    // the walk must not escape), so the shared `run` helper (which pins the
    // XDG roots to its cwd) doesn't fit here.
    let out = Command::new(nub_binary())
        .args(["install", "--ignore-scripts", "--silent"])
        .current_dir(&tool_dir)
        .env("NUB_SELF_SHIM", "0")
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !stderr.contains("ERR_NUB_LOCKFILE_AMBIGUOUS"),
        "an install inside nub's cache root must not inherit ambient lockfile ambiguity: {stderr}"
    );
    // Positive proof the walk was clamped (fresh identity, resolution reached)
    // rather than the command dying on some unrelated early exit. The proof is
    // the dead registry the fixture configured appearing in the failure: only
    // an install that got past identity and into resolution can have tried to
    // reach it. It replaces an assertion on `ERR_NUB_REGISTRY_ERROR`, which is
    // one package manager's code for this and not the other's — a detail of
    // who is serving, where the claim is about how far the install got.
    assert!(
        stderr.contains("127.0.0.1:1"),
        "the scratch install should get past identity to the registry fetch: {stderr}"
    );
}

/// `nub.lock` — the engine's canonical lockfile under nub's filename toggle
/// — IS nub identity: alone it resolves and installs in place, and no
/// `pnpm-lock.yaml` appears beside it.
///
/// This used to assert three more shapes, all of them a CONFLICT between
/// `nub.lock` and another package manager's lockfile, and all three are
/// states that no longer conflict: npm, yarn and bun confer no identity at
/// all now, so a `package-lock.json` sitting beside `nub.lock` says nothing
/// for nub to weigh against it. The install reads nub's own lockfile and
/// leaves the other file alone, which is what the migrate hint is for.
#[test]
fn lock_yaml_alone_is_nub_identity() {
    let empty_lock = "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n";
    let dir = project("lockyaml-nub", r#"{"name":"app","version":"1.0.0"}"#);
    std::fs::write(dir.join("nub.lock"), empty_lock).unwrap();
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("nub.lock").is_file() && !dir.join("pnpm-lock.yaml").exists(),
        "nub.lock is the lockfile under nub identity: {stderr}"
    );
}

/// Brand boundary on the config-FILE surface: under the NUB profile the engine
/// reads NO branded user/project config file. The vendored engine's
/// `~/.config/aube/config.toml` + `<cwd>/.config/aube/config.toml` (the leak)
/// are ignored, and nub authors no `~/.config/nub/` home of its own — a planted
/// `.config/nub/config.{toml}` is ignored too. The reader is `nub config get`,
/// whose value would echo any honored file source. All four plants set
/// `minimumReleaseAge`; with every branded file ignored the setting falls back
/// to its built-in default, so the readout is NOT any planted value.
///
/// HOME + XDG_CONFIG_HOME are pinned to throwaway dirs so the user-scope plant
/// is hermetic (never the developer's real `~/.config`).
#[test]
fn nub_profile_reads_no_branded_user_or_project_config_file() {
    let dir = project("config-file-brand", r#"{"name":"app","version":"1.0.0"}"#);
    let xdg_config = dir.join("xdg-config");

    // User scope (XDG_CONFIG_HOME): both the aube leak and a would-be nub home.
    std::fs::create_dir_all(xdg_config.join("aube")).unwrap();
    std::fs::write(
        xdg_config.join("aube").join("config.toml"),
        "minimumReleaseAge = 4321\n",
    )
    .unwrap();
    std::fs::create_dir_all(xdg_config.join("nub")).unwrap();
    std::fs::write(
        xdg_config.join("nub").join("config.toml"),
        "minimumReleaseAge = 5555\n",
    )
    .unwrap();

    // Project scope (<cwd>/.config/<brand>/config.toml).
    std::fs::create_dir_all(dir.join(".config").join("aube")).unwrap();
    std::fs::write(
        dir.join(".config").join("aube").join("config.toml"),
        "minimumReleaseAge = 7777\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".config").join("nub")).unwrap();
    std::fs::write(
        dir.join(".config").join("nub").join("config.toml"),
        "minimumReleaseAge = 8888\n",
    )
    .unwrap();

    let out = Command::new(nub_binary())
        .args(["config", "get", "minimumReleaseAge"])
        .current_dir(&dir)
        .env("HOME", dir.join("home"))
        .env("USERPROFILE", dir.join("home"))
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    for planted in ["4321", "5555", "7777", "8888"] {
        assert!(
            !stdout.contains(planted),
            "nub must ignore every branded config file (read `{planted}`): {stdout}"
        );
    }

    // Write side: a `config set` must never AUTHOR a branded config file under
    // nub — the value lands on the neutral `.npmrc`, and the pre-existing
    // branded plants are left byte-for-byte untouched.
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let set = Command::new(nub_binary())
        .args(["config", "set", "--global", "minimumReleaseAge", "1000"])
        .current_dir(&dir)
        .env("HOME", dir.join("home"))
        .env("USERPROFILE", dir.join("home"))
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    assert_eq!(
        set.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&set.stderr)
    );
    assert!(
        dir.join("home").join(".npmrc").exists(),
        "the write must land on the neutral .npmrc"
    );
    assert_eq!(
        std::fs::read_to_string(xdg_config.join("aube").join("config.toml")).unwrap(),
        "minimumReleaseAge = 4321\n",
        "config set must not write the aube-branded user config file"
    );
    assert_eq!(
        std::fs::read_to_string(xdg_config.join("nub").join("config.toml")).unwrap(),
        "minimumReleaseAge = 5555\n",
        "config set must not write a nub-branded user config file"
    );
}

/// Brand boundary on the on-disk PATH surface: nub must never create a path
/// carrying the embedded engine's brand. Two independent mechanisms broke this
/// before, and the assertions below pin both.
///
/// 1. `aube_util::embedder()` falls back to the *aube* profile whenever the
///    identity OnceLock is unset, and identity was registered only inside the
///    PM engine's preflight. `nub run` never reaches preflight, so it wrote the
///    engine's lazy node-gyp shim to `<cache>/aube/tools/…` and handed that path
///    to every script as `npm_config_node_gyp`. `main` now registers identity
///    before anything else runs.
/// 2. On-disk marker/probe/temp names inside the engine were hardcoded to
///    `aube` rather than composed from the active profile, so they landed
///    brand-crossed even once identity was correct.
///
/// The run path is the probe because it is the one that regressed; the
/// assertion is on the whole isolated home, so any new brand-crossed write
/// from any subsystem trips it. `npm_config_node_gyp` is asserted positively
/// too — absence alone would also pass if the var stopped being exported.
#[test]
fn nub_never_writes_an_aube_branded_path() {
    let dir = project(
        "brand-path",
        r#"{"name":"app","version":"1.0.0","scripts":{"probe":"node -e \"console.log(process.env.npm_config_node_gyp)\""}}"#,
    );
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = Command::new(nub_binary())
        .args(["run", "probe"])
        .current_dir(&dir)
        .env("NUB_SELF_SHIM", "0")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );

    let node_gyp = stdout
        .lines()
        .find(|l| l.contains("node-gyp"))
        .unwrap_or_else(|| panic!("script did not print npm_config_node_gyp: {stdout}"));
    // Separator-normalized: `cache_namespace` is the literal "nub/pm", so on
    // Windows `Path::join` yields `…\\xdg-cache\\nub/pm\\tools\\…` — a raw
    // `contains("/nub/")` fails there while the leak it guards is unchanged.
    let normalized = node_gyp.replace('\\', "/");
    assert!(
        normalized.contains("/nub/") && !normalized.contains("/aube/"),
        "npm_config_node_gyp must live under nub's cache namespace: {node_gyp}"
    );

    let mut offenders = Vec::new();
    collect_brand_crossed_paths(&dir, &mut offenders);
    assert!(
        offenders.is_empty(),
        "nub wrote aube-branded path(s): {offenders:#?}"
    );
}

/// Every path under `root` whose file name carries the embedded engine's
/// brand. Walks rather than globbing so a brand-crossed leaf at any depth is
/// caught (the side-effects marker sits several levels inside the store).
fn collect_brand_crossed_paths(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.to_ascii_lowercase().contains("aube"))
        {
            out.push(path.clone());
        }
        if path.is_dir() && !path.is_symlink() {
            collect_brand_crossed_paths(&path, out);
        }
    }
}
