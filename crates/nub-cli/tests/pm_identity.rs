//! The PM identity decision table, behaviorally, through the binary
//! (spec: wiki/commands/pm/identity-policy.md). Identity resolution is the
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

/// Rows "none|none → nub identity" (truly-fresh) and "declared X|none → X's
/// format" (the fresh-with-pin row): an empty-deps install writes the
/// identity's lockfile without any network.
#[test]
fn fresh_projects_write_the_identity_format_declared_first_else_nub() {
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
            "onFail": "warn"
        })),
        "a virgin install stamps a devEngines.packageManager caret range: {manifest}"
    );
    assert!(
        manifest.get("packageManager").is_none(),
        "the virgin stamp writes only the devEngines range, never the exact packageManager pin: {manifest}"
    );

    // declared npm + none → package-lock.json, NOT the nub default.
    let dir = project(
        "fresh-npm",
        r#"{"name":"app","version":"1.0.0","packageManager":"npm@11.0.0"}"#,
    );
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("package-lock.json").is_file(),
        "declared-npm fresh install must write package-lock.json: {stderr}"
    );
    assert!(
        !dir.join("pnpm-lock.yaml").exists(),
        "the declaration must outrank the pnpm fresh default"
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

/// Row "X|only a different PM's lockfile → error": the contradiction is loud,
/// carries the rewritten stable code, and names the `nub pm use` remedy.
#[test]
fn a_declaration_contradicted_by_the_lockfile_errors_with_code_and_remedy() {
    let dir = project("contradiction", EMPTY_PNPM);
    std::fs::write(
        dir.join("package-lock.json"),
        r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{}}"#,
    )
    .unwrap();
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_ne!(code, 0, "a contradicted project must refuse to install");
    assert!(
        stderr.contains("ERR_NUB_LOCKFILE_DECLARATION_MISMATCH"),
        "the stable code must be present (rewritten): {stderr}"
    );
    assert!(
        stderr.contains("set the declaration: nub pm use <pm> — or remove the stale lockfile"),
        "the remedy must be nub's: {stderr}"
    );
    assert!(
        !stderr.contains("aube") && !stderr.contains("AUBE"),
        "no engine branding may leak: {stderr}"
    );
    assert!(
        !dir.join("pnpm-lock.yaml").exists() && !dir.join("node_modules").exists(),
        "nothing may be written past the contradiction: {stdout}"
    );
}

/// Row "none|multiple → error": two lockfiles and no declaration is an
/// ambiguity nub refuses to guess through — same code/remedy contract.
#[test]
fn undeclared_multi_lockfile_projects_error_as_ambiguous() {
    let dir = project("ambiguous", r#"{"name":"app","version":"1.0.0"}"#);
    std::fs::write(
        dir.join("package-lock.json"),
        r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();
    let (_, stderr, code) = run(&dir, &["install"]);
    assert_ne!(code, 0, "an ambiguous project must refuse to install");
    assert!(
        stderr.contains("ERR_NUB_LOCKFILE_AMBIGUOUS"),
        "the stable code must be present (rewritten): {stderr}"
    );
    assert!(
        stderr.contains("package-lock.json") && stderr.contains("yarn.lock"),
        "the error must name the conflicting files: {stderr}"
    );
    assert!(
        stderr.contains("set the declaration: nub pm use <pm> — or remove the stale lockfile"),
        "the remedy must be nub's: {stderr}"
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

/// The follow-up over-scope class (maintainer report 2026-06-26, follow-up to
/// #197): GLOBAL-SCOPE commands that operate on the global store/config or the
/// registry — never the project lockfile — must not raise the ambiguity guard
/// either. `store path`, `config get`, `bin`, `root` run leniently and succeed;
/// the PROJECT-GRAPH readers (`why`) and the mutating install family (`add`)
/// stay strict and keep the loud `ERR_NUB_LOCKFILE_AMBIGUOUS`.
#[test]
fn global_scope_commands_ignore_multi_lockfile_ambiguity() {
    let dir = project("ambiguous-global", r#"{"name":"app","version":"1.0.0"}"#);
    std::fs::write(
        dir.join("package-lock.json"),
        r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();

    // Global-scope reads succeed and print their datum — no ambiguity preflight.
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
            !stderr.contains("ERR_NUB_LOCKFILE_AMBIGUOUS"),
            "`nub {}` must not raise the ambiguity guard: {stderr}",
            args.join(" ")
        );
        assert!(
            !stdout.trim().is_empty(),
            "`nub {}` should print its datum: stdout empty",
            args.join(" ")
        );
    }

    // Project-graph reader stays strict: `why` reads the lockfile, so ambiguity
    // is a loud error (a silent degrade would yield a wrong/empty graph).
    let (_, stderr, _) = run(&dir, &["why", "is-odd"]);
    assert!(
        stderr.contains("ERR_NUB_LOCKFILE_AMBIGUOUS"),
        "`nub why` must keep the ambiguity guard (it reads the project lockfile): {stderr}"
    );

    // The mutating install family keeps the guard — it would WRITE a lockfile,
    // and must never silently pick one under ambiguity.
    let (_, stderr, _) = run(&dir, &["add", "left-pad"]);
    assert!(
        stderr.contains("ERR_NUB_LOCKFILE_AMBIGUOUS"),
        "`nub add` must keep the ambiguity guard (it writes the project lockfile): {stderr}"
    );
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
    // rather than the command dying on some unrelated early exit.
    assert!(
        stderr.contains("ERR_NUB_REGISTRY_ERROR"),
        "the scratch install should get past identity to the registry fetch: {stderr}"
    );
}

/// The declared-yarn corner of the fresh row: identity resolves to yarn with
/// no yarn.lock on disk, and the first install would CREATE yarn.lock — the
/// gated write. Refused with the gate message, nothing written.
#[test]
fn a_fresh_declared_yarn_project_hits_the_write_gate_not_a_pnpm_lockfile() {
    let dir = project(
        "yarn-fresh",
        r#"{"name":"app","version":"1.0.0","packageManager":"yarn@1.22.19"}"#,
    );
    let (_, stderr, code) = run(&dir, &["install"]);
    assert_ne!(code, 0, "a fresh declared-yarn install must refuse");
    assert!(
        stderr.contains("refusing to modify yarn.lock") && stderr.contains("yarn install"),
        "the refusal must be the yarn gate with its remedy: {stderr}"
    );
    assert!(
        !dir.join("pnpm-lock.yaml").exists() && !dir.join("yarn.lock").exists(),
        "no lockfile of any format may be written past the gate"
    );
}

/// The nub.lock rows (two-mode model, the maintainer 2026-06-10): the generically
/// named `nub.lock` (the engine's canonical slot under nub's filename
/// toggle) IS nub identity — alone it resolves and installs in place; beside
/// a foreign lockfile or against a contradicting declaration it is the same
/// loud error as any other identity conflict, never a silent winner (nub
/// opts out of upstream's canonical-always-wins carve-out).
#[test]
fn lock_yaml_is_nub_identity_and_conflicts_are_loud() {
    let empty_lock = "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n";

    // nub.lock + no declaration → nub identity: install works in place,
    // nub.lock stays the lockfile, no pnpm-lock.yaml appears.
    let dir = project("lockyaml-nub", r#"{"name":"app","version":"1.0.0"}"#);
    std::fs::write(dir.join("nub.lock"), empty_lock).unwrap();
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("nub.lock").is_file() && !dir.join("pnpm-lock.yaml").exists(),
        "nub.lock is the lockfile under nub identity: {stderr}"
    );

    // nub.lock + package-lock.json, no declaration → ambiguity naming both.
    let dir = project("lockyaml-ambig", r#"{"name":"app","version":"1.0.0"}"#);
    std::fs::write(dir.join("nub.lock"), empty_lock).unwrap();
    std::fs::write(
        dir.join("package-lock.json"),
        r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{}}"#,
    )
    .unwrap();
    let (_, stderr, code) = run(&dir, &["install"]);
    assert_ne!(code, 0, "nub.lock beside a foreign lockfile must refuse");
    assert!(
        stderr.contains("ERR_NUB_LOCKFILE_AMBIGUOUS")
            && stderr.contains("nub.lock")
            && stderr.contains("package-lock.json"),
        "the ambiguity must carry the code and name both files: {stderr}"
    );

    // Declared pnpm + only nub.lock → contradiction (a half-reversed switch;
    // `nub pm use` is the remedy in the message).
    let dir = project("lockyaml-contra", EMPTY_PNPM);
    std::fs::write(dir.join("nub.lock"), empty_lock).unwrap();
    let (_, stderr, code) = run(&dir, &["install"]);
    assert_ne!(code, 0, "declared pnpm over nub.lock must refuse");
    assert!(
        stderr.contains("ERR_NUB_LOCKFILE_DECLARATION_MISMATCH") && stderr.contains("nub.lock"),
        "the contradiction must carry the code and name nub.lock: {stderr}"
    );

    // Declared nub + nub.lock → clean nub identity (the post-`use nub`
    // state): resolves and installs.
    let dir = project(
        "lockyaml-declared",
        r#"{"name":"app","version":"1.0.0","packageManager":"nub@0.0.1"}"#,
    );
    std::fs::write(dir.join("nub.lock"), empty_lock).unwrap();
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("nub.lock").is_file() && !dir.join("pnpm-lock.yaml").exists(),
        "declared nub keeps nub.lock: {stderr}"
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
        .args([
            "config",
            "set",
            "--location",
            "user",
            "minimumReleaseAge",
            "1000",
        ])
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
