//! Behavioral coverage for the install family's registry verbs (`nub add`,
//! `rm`, `up`, `dlx`, `import`, `link`/`unlink`, the yarn write gate) through
//! the real binary — real fixtures, real lockfiles, real node_modules. The
//! wiring under test lives in `src/pm_engine/install_family.rs`; `install` /
//! `ci` have their own file (`install_engine.rs`).
//!
//! Network tests are `#[ignore]` per the provisioning-test convention — run
//! via `cargo test -p nub-cli --test pm_verbs -- --ignored` — and self-skip
//! when the registry is unreachable. Everything else is offline by
//! construction (gate pre-flights, lockfile conversion, symlink plumbing).
//!
//! Brand guard: every test asserts no `aube` token in the combined output.
//! Exception: `link`/`unlink -g` print the engine's global-links registry
//! path (`<XDG_CACHE_HOME>/aube/global-links` — leaf-fixed at the pinned
//! API, documented residual), so the link test scopes its guard to the
//! non-path lines it owns.

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
        "nub-pmverb-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Output {
    stdout: String,
    stderr: String,
    code: i32,
}

impl Output {
    fn combined(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }

    #[track_caller]
    fn assert_brand_clean(&self) {
        assert!(
            !self.combined().to_lowercase().contains("aube"),
            "no engine branding may reach the output:\nstdout: {}\nstderr: {}",
            self.stdout,
            self.stderr
        );
    }
}

/// Spawn `nub <args>` in `dir` with the engine store *and* cache pinned to
/// the given roots — pass the same pair across spawns that must share engine
/// state (the CAS store rides `XDG_DATA_HOME`; the packument cache and the
/// global-links registry ride `XDG_CACHE_HOME`).
fn run_nub_with(dir: &Path, args: &[&str], xdg_data: &Path, xdg_cache: &Path) -> Output {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", xdg_data)
        .env("XDG_CACHE_HOME", xdg_cache)
        .output()
        .expect("failed to spawn nub");
    Output {
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        code: out.status.code().unwrap_or(-1),
    }
}

/// One-shot spawn against fresh engine roots (never warm-hits, never
/// pollutes the dev box's real store).
fn run_nub(dir: &Path, args: &[&str]) -> Output {
    run_nub_with(dir, args, &pm_tmpdir("xdg-data"), &pm_tmpdir("xdg-cache"))
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

/// In-sync npm v3 lockfile for is-positive@3.1.0 (the integrity is the
/// published registry value — stable forever for a published version).
const IS_POSITIVE_PACKAGE_LOCK: &str = r#"{
  "name": "fixture",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "fixture",
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

/// `nub add` then `nub rm` (alias) round-trip on a truly-fresh project: add
/// persists the dep + writes nub's neutral `lock.yaml` + links node_modules;
/// remove strips the dep from the manifest again. Both outputs brand-clean.
#[test]
#[ignore = "network: resolves + fetches is-positive@3.1.0 from the npm registry"]
fn add_then_remove_round_trips_manifest_lockfile_and_node_modules() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("addrm");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"addrm","version":"1.0.0"}"#,
    )
    .unwrap();
    let (data, cache) = (pm_tmpdir("addrm-data"), pm_tmpdir("addrm-cache"));

    let add = run_nub_with(&dir, &["add", "is-positive@3.1.0"], &data, &cache);
    assert_eq!(
        add.code, 0,
        "stdout: {}\nstderr: {}",
        add.stdout, add.stderr
    );
    add.assert_brand_clean();
    let manifest = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(
        manifest.contains("\"is-positive\""),
        "add must persist the dependency: {manifest}"
    );
    assert!(
        dir.join("lock.yaml").is_file()
            && !dir.join("pnpm-lock.yaml").exists()
            && !dir.join("aube-lock.yaml").exists(),
        "add on a truly-fresh project writes nub's neutral lock.yaml"
    );
    assert!(
        dir.join("node_modules/is-positive/package.json").is_file(),
        "add must link the package: stderr: {}",
        add.stderr
    );

    let rm = run_nub_with(&dir, &["rm", "is-positive"], &data, &cache);
    assert_eq!(rm.code, 0, "stdout: {}\nstderr: {}", rm.stdout, rm.stderr);
    rm.assert_brand_clean();
    let manifest = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(
        !manifest.contains("is-positive"),
        "remove must strip the dependency: {manifest}"
    );
}

/// The patch workflow round-trips: `patch` extracts into a nub-named edit
/// dir and prints the rebranded patch-commit hint; `patch-commit` writes
/// the `.patch` file, records `pnpm.patchedDependencies`, and re-links the
/// edited content; `patch-remove` reverts all of it. All outputs brand-clean.
#[test]
#[ignore = "network: resolves + fetches is-positive@3.1.0 from the npm registry"]
fn patch_workflow_round_trips_through_commit_and_remove() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("patchwf");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"patchwf","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();
    let (data, cache) = (pm_tmpdir("patchwf-data"), pm_tmpdir("patchwf-cache"));
    let install = run_nub_with(&dir, &["install"], &data, &cache);
    assert_eq!(install.code, 0, "install: {}", install.stderr);

    let patch = run_nub_with(&dir, &["patch", "is-positive@3.1.0"], &data, &cache);
    assert_eq!(patch.code, 0, "stderr: {}", patch.stderr);
    patch.assert_brand_clean();
    assert!(
        patch.stdout.contains("nub patch-commit"),
        "the follow-up hint must be rebranded: {}",
        patch.stdout
    );
    // The edit dir is the nub-named default (printed path = real path).
    let edit_dir = patch
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("You can now edit the following folder: "))
        .unwrap_or_else(|| panic!("patch must print the edit dir: {}", patch.stdout));
    assert!(
        edit_dir.contains("nub-patch-is-positive"),
        "default edit dir must be nub-named: {edit_dir}"
    );
    let edited = Path::new(edit_dir).join("index.js");
    std::fs::write(&edited, "module.exports = () => 'patched';\n").unwrap();

    let commit = run_nub_with(&dir, &["patch-commit", edit_dir], &data, &cache);
    assert_eq!(commit.code, 0, "stderr: {}", commit.stderr);
    commit.assert_brand_clean();
    assert!(
        dir.join("patches/is-positive@3.1.0.patch").is_file(),
        "patch-commit must write the patch file: {}",
        commit.stderr
    );
    let manifest = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(
        manifest.contains("patchedDependencies") && manifest.contains("\"pnpm\""),
        "patch-commit must record the entry under the pnpm namespace: {manifest}"
    );
    let linked = dir.join("node_modules/is-positive/index.js");
    assert!(
        std::fs::read_to_string(&linked)
            .unwrap()
            .contains("'patched'"),
        "the chained install must materialize the patched content"
    );

    let remove = run_nub_with(&dir, &["patch-remove", "is-positive@3.1.0"], &data, &cache);
    assert_eq!(remove.code, 0, "stderr: {}", remove.stderr);
    remove.assert_brand_clean();
    assert!(
        !dir.join("patches/is-positive@3.1.0.patch").exists(),
        "patch-remove must delete the patch file"
    );
    assert!(
        !std::fs::read_to_string(dir.join("package.json"))
            .unwrap()
            .contains("patchedDependencies"),
        "patch-remove must drop the manifest entry"
    );
}

/// `nub up --latest` moves a pinned manifest range + lockfile resolution
/// forward (is-positive 3.0.0 → 3.1.0, the package's final release).
#[test]
#[ignore = "network: resolves is-positive's dist-tags from the npm registry"]
fn update_latest_moves_the_manifest_and_lockfile_forward() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("update");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"update","version":"1.0.0","dependencies":{"is-positive":"3.0.0"}}"#,
    )
    .unwrap();

    let up = run_nub(&dir, &["up", "--latest"]);
    assert_eq!(up.code, 0, "stdout: {}\nstderr: {}", up.stdout, up.stderr);
    up.assert_brand_clean();
    let manifest = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(
        manifest.contains("3.1.0"),
        "--latest must rewrite the manifest past the pin: {manifest}"
    );
    let lock = std::fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap();
    assert!(
        lock.contains("3.1.0") && !lock.contains("is-positive@3.0.0"),
        "the lockfile must resolve the updated version: {lock}"
    );
}

/// `nub dlx` installs into a scratch project and runs the package's bin —
/// the full npx-shaped flow, exit code and stdout from the child.
#[test]
#[ignore = "network: installs uuid into a dlx scratch project"]
fn dlx_installs_and_runs_a_bin_from_a_scratch_project() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("dlx");
    let out = run_nub(&dir, &["dlx", "uuid"]);
    assert_eq!(
        out.code, 0,
        "stdout: {}\nstderr: {}",
        out.stdout, out.stderr
    );
    out.assert_brand_clean();
    let printed = out.stdout.trim();
    assert!(
        printed.len() == 36 && printed.chars().filter(|c| *c == '-').count() == 4,
        "uuid's bin must print one v4 uuid, got: {printed:?}"
    );
}

/// The yarn write gate on the mutating daily drivers: a yarn project refuses
/// add/remove/update/dedupe outright (no network — the gate is a pre-flight),
/// yarn.lock stays byte-identical, and `dedupe --check` is still allowed
/// through (it writes nothing). `--global` forms are exempt by design but
/// mutate real user state, so they're not exercised here.
#[test]
fn yarn_gate_refuses_mutating_verbs_and_names_the_remedy() {
    let dir = pm_tmpdir("yarngate");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"yarngate","version":"1.0.0","dependencies":{"left-pad":"^1.3.0"}}"#,
    )
    .unwrap();
    // Satisfiable yarn-classic lockfile — proves the gate fires on the
    // verb's nature (always re-resolves), not on drift.
    let yarn_lock = "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n\
                     # yarn lockfile v1\n\n\n\
                     left-pad@^1.3.0:\n\
                     \x20\x20version \"1.3.0\"\n\
                     \x20\x20resolved \"https://registry.yarnpkg.com/left-pad/-/left-pad-1.3.0.tgz#5b8a3a7765dfe001261dde915589e782f8c94d1e\"\n\
                     \x20\x20integrity sha512-XI5MPzVNApjAyhQzphX8BkmKsKUxD4LdyK24iZeQGinBN9yTQT3bFlCBy/aVx2HrNcqQGsdot8ghrjyrvMCoEA==\n";
    std::fs::write(dir.join("yarn.lock"), yarn_lock).unwrap();

    for (args, remedy) in [
        (&["add", "is-positive"][..], "yarn add is-positive"),
        (&["rm", "left-pad"][..], "yarn remove left-pad"),
        (&["up", "--latest"][..], "yarn upgrade"),
        (&["dedupe"][..], "yarn dedupe"),
    ] {
        let out = run_nub(&dir, args);
        assert_ne!(
            out.code, 0,
            "nub {args:?} must be refused on a yarn project"
        );
        assert!(
            out.stderr.contains("refusing to modify yarn.lock") && out.stderr.contains(remedy),
            "nub {args:?} must name the gate + remedy `{remedy}`: {}",
            out.stderr
        );
        out.assert_brand_clean();
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("yarn.lock")).unwrap(),
        yarn_lock,
        "yarn.lock must be byte-identical after refused commands"
    );
    assert!(
        !dir.join("node_modules").exists(),
        "nothing may be installed past the gate"
    );

    // `dedupe --check` writes nothing and passes the gate; on this in-sync
    // lockfile it reports no changes (exit 0) without touching the network
    // (resolution is satisfied from the lockfile read).
    let check = run_nub(&dir, &["dedupe", "--check"]);
    assert!(
        !check.stderr.contains("refusing to modify yarn.lock"),
        "dedupe --check must pass the gate: {}",
        check.stderr
    );
}

/// `nub import` converts a foreign lockfile to pnpm-lock.yaml (nub's
/// canonical format — never aube-lock.yaml), refuses a second run without
/// `--force`, and works fully offline.
#[test]
fn import_converts_package_lock_to_pnpm_lock() {
    let dir = pm_tmpdir("import");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"fixture","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("package-lock.json"), IS_POSITIVE_PACKAGE_LOCK).unwrap();

    let out = run_nub(&dir, &["import"]);
    assert_eq!(
        out.code, 0,
        "stdout: {}\nstderr: {}",
        out.stdout, out.stderr
    );
    out.assert_brand_clean();
    assert!(
        out.stderr
            .contains("Imported 1 packages from package-lock.json to pnpm-lock.yaml"),
        "import must report the conversion: {}",
        out.stderr
    );
    let lock = std::fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap();
    assert!(
        lock.contains("is-positive"),
        "converted lockfile must carry the dependency: {lock}"
    );
    assert!(
        !dir.join("aube-lock.yaml").exists() && !dir.join("pnpm-lock.yaml.import-backup").exists(),
        "no foreign lockfile or leftover backup may appear"
    );
    assert!(
        dir.join("package-lock.json").is_file(),
        "the source lockfile is left in place (parity with pnpm import)"
    );

    // Second run: the target exists → refused without --force, allowed with.
    let again = run_nub(&dir, &["import"]);
    assert_ne!(again.code, 0);
    assert!(
        again.stderr.contains("pnpm-lock.yaml already exists") && again.stderr.contains("--force"),
        "re-import must point at --force: {}",
        again.stderr
    );
    let forced = run_nub(&dir, &["import", "--force"]);
    assert_eq!(forced.code, 0, "stderr: {}", forced.stderr);
}

/// `nub link` (register) → `nub link <name>` (consume) → `nub unlink <name>`
/// — the global-links round trip, fully offline. The unlink-all hint path
/// (`Run \`nub install\` to restore…`) is the fd-captured rewrite in action.
#[test]
#[cfg(unix)] // symlink plumbing; the engine's Windows shims are CI-leg territory
fn link_unlink_round_trip_through_the_global_registry() {
    // The global-links registry lives under the engine cache root, so the
    // three spawns must share XDG_CACHE_HOME (and the data root, for the CAS).
    let (data, cache) = (pm_tmpdir("link-data"), pm_tmpdir("link-cache"));
    let lib = pm_tmpdir("linklib");
    std::fs::write(
        lib.join("package.json"),
        r#"{"name":"my-linked-lib","version":"1.0.0"}"#,
    )
    .unwrap();
    let register = run_nub_with(&lib, &["link"], &data, &cache);
    assert_eq!(register.code, 0, "stderr: {}", register.stderr);
    assert!(
        register.stderr.contains("Linked"),
        "registering must confirm: {}",
        register.stderr
    );

    let app = pm_tmpdir("linkapp");
    std::fs::write(
        app.join("package.json"),
        r#"{"name":"linkapp","version":"1.0.0"}"#,
    )
    .unwrap();
    let consume = run_nub_with(&app, &["link", "my-linked-lib"], &data, &cache);
    assert_eq!(consume.code, 0, "stderr: {}", consume.stderr);
    let entry = app.join("node_modules/my-linked-lib");
    assert!(
        entry.symlink_metadata().unwrap().file_type().is_symlink(),
        "consuming a link must symlink into node_modules"
    );

    // Bare `nub unlink` (unlink-all) exercises the captured-stderr hint
    // line, which must come out rebranded.
    let unlink = run_nub_with(&app, &["unlink"], &data, &cache);
    assert_eq!(unlink.code, 0, "stderr: {}", unlink.stderr);
    assert!(!entry.exists(), "unlink must remove the symlink");
    assert!(
        unlink.stderr.contains("Run `nub install` to restore"),
        "the unlink-all hint must be rebranded through the fd capture: {}",
        unlink.stderr
    );
    assert!(
        !unlink.combined().to_lowercase().contains("aube"),
        "unlink output must be brand-clean: {}",
        unlink.combined()
    );
}

/// Wired verbs own their `--help` at the nub layer: rendered from aube's own
/// args surface, rebranded, exit 0. (`dlx --help` takes a bespoke path — the
/// trailing var-arg swallows the flag — and must land on the same contract.)
#[test]
fn verb_help_is_rebranded_and_exits_zero() {
    let dir = pm_tmpdir("help");
    // `create` exercises the nub-side help intercept (its trailing var-arg
    // swallows --help before clap can settle it, like dlx's bare form).
    for verb in ["add", "dlx", "create"] {
        let out = run_nub(&dir, &[verb, "--help"]);
        assert_eq!(out.code, 0, "{verb} --help: stderr: {}", out.stderr);
        out.assert_brand_clean();
        assert!(
            out.stdout.contains(&format!("nub {verb}")),
            "{verb} help must carry nub usage: {}",
            out.stdout
        );
    }
}

/// `init` is reserved for nub's own project init: not an engine verb, not a
/// PM redirect — the answer names the coming nub feature and nothing else.
#[test]
fn init_is_reserved_and_answers_with_the_coming_note() {
    let dir = pm_tmpdir("init");
    std::fs::write(dir.join("package.json"), r#"{"name":"init-fixture"}"#).unwrap();
    let out = run_nub(&dir, &["init"]);
    assert_ne!(out.code, 0, "init must error until nub's own init ships");
    out.assert_brand_clean();
    assert!(
        out.stderr.contains("nub's own project init is coming"),
        "the message must name the coming nub feature: {}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("package manager") && !out.stderr.contains("pnpm init"),
        "init must not redirect to a PM: {}",
        out.stderr
    );
}

/// The excluded verbs answer with their honest per-verb status, never the
/// generic "wired in phase Surface" stub text (nothing is left in backlog).
#[test]
fn excluded_verbs_answer_honestly_not_with_stub_text() {
    let dir = pm_tmpdir("excluded");
    for (verb, expect) in [
        ("recursive", "verb's own workspace flags"),
        ("clean", "not supported"),
        ("purge", "not supported"),
        ("deploy", "not yet supported"),
        ("sbom", "not yet supported"),
    ] {
        let out = run_nub(&dir, &[verb]);
        assert_ne!(out.code, 0, "{verb} must error");
        out.assert_brand_clean();
        assert!(
            out.stderr.contains(expect),
            "{verb} must explain its status: {}",
            out.stderr
        );
        assert!(
            !out.stderr.contains("wired in phase Surface"),
            "{verb} must not use the generic stub text: {}",
            out.stderr
        );
    }
}

/// The PM-suggestion surfaces agree on identity. A fresh / nub-identity
/// project (no lockfile, no foreign pin) gets a `nub`-flavored hint from each
/// redirect; a project with a committed foreign lockfile gets that PM. The
/// blind-`npm` fallback the migrate redirect and engine-verb dispatch used to
/// carry is gone — both now route through the same nub-identity-aware
/// `suggest_package_manager` logic the `nubx`-miss hint already used.
#[test]
fn redirect_surfaces_agree_on_pm_identity() {
    // ── Fresh / nub-identity: every surface speaks nub ──
    let nub_dir = pm_tmpdir("suggest-nub");
    std::fs::write(nub_dir.join("package.json"), r#"{"name":"fresh"}"#).unwrap();

    // Surface 1 — the `migrate` PM-verb redirect. nub has no `migrate` verb;
    // it spells the lockfile migration `import`, so the nub-identity redirect
    // must name a *real* command (`nub import`), never a phantom `nub migrate`.
    let migrate = run_nub(&nub_dir, &["migrate", "yarn.lock"]);
    assert_ne!(migrate.code, 0, "migrate is not a nub command");
    migrate.assert_brand_clean();
    assert!(
        migrate.stderr.contains("nub import yarn.lock"),
        "migrate redirect must suggest the real `nub import`, not npm: {}",
        migrate.stderr
    );
    assert!(
        !migrate.stderr.contains("npm migrate") && !migrate.stderr.contains("nub migrate"),
        "no blind-npm fallback and no phantom `nub migrate`: {}",
        migrate.stderr
    );

    // Surface 3 — the `nubx`-miss hint (exec of an uninstalled bin).
    let exec = run_nub(&nub_dir, &["exec", "definitely-not-installed-xyz"]);
    assert_eq!(exec.code, 127, "missing bin exits 127");
    exec.assert_brand_clean();
    assert!(
        exec.stderr.contains("nub add -D") && exec.stderr.contains("nubx "),
        "nubx hint must speak nub in a fresh project: {}",
        exec.stderr
    );
    assert!(
        !exec.stderr.contains("npm install"),
        "no blind-npm fallback in the nubx hint: {}",
        exec.stderr
    );

    // ── pnpm-pinned: every surface speaks pnpm ──
    let pnpm_dir = pm_tmpdir("suggest-pnpm");
    std::fs::write(pnpm_dir.join("package.json"), r#"{"name":"p"}"#).unwrap();
    std::fs::write(pnpm_dir.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();

    let migrate_pnpm = run_nub(&pnpm_dir, &["migrate", "yarn.lock"]);
    assert_ne!(migrate_pnpm.code, 0);
    assert!(
        migrate_pnpm.stderr.contains("pnpm migrate yarn.lock"),
        "a pnpm project keeps its own PM in the migrate redirect: {}",
        migrate_pnpm.stderr
    );

    let exec_pnpm = run_nub(&pnpm_dir, &["exec", "definitely-not-installed-xyz"]);
    assert_eq!(exec_pnpm.code, 127);
    assert!(
        exec_pnpm.stderr.contains("pnpm add -D") && exec_pnpm.stderr.contains("pnpm dlx"),
        "a pnpm project keeps its own PM in the nubx hint: {}",
        exec_pnpm.stderr
    );
}

// Surface 2 — the engine-verb dispatch's PM hint (`dispatch_subcommand` →
// `dispatch_verb`) — now routes through the same `suggest_package_manager`
// source function as surfaces 1 and 3, so the nub-vs-foreign behavior verified
// in `redirect_surfaces_agree_on_pm_identity` carries to it identically. The
// hint is consumed only by the unwired-verb stub fallback (`{pm} {verb}`),
// which is unreachable through the binary today (every registered verb is
// wired or explicitly excluded), so there is no spawned-binary path to assert
// against — a ceremonial test of an unreachable arm would be sloppification.

/// `nub create <template>` maps to the create-* package and runs the real
/// scaffolder end-to-end (create-vite, zero-dep, non-interactive with an
/// explicit template).
#[test]
#[ignore = "network: installs create-vite into a dlx scratch project"]
fn create_runs_a_real_scaffolder_via_the_dlx_path() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("create");
    let out = run_nub(
        &dir,
        &["create", "vite", "scaffolded", "--template", "vanilla"],
    );
    assert_eq!(
        out.code, 0,
        "stdout: {}\nstderr: {}",
        out.stdout, out.stderr
    );
    out.assert_brand_clean();
    let manifest = dir.join("scaffolded/package.json");
    assert!(
        manifest.is_file(),
        "create-vite must scaffold the project dir: {}",
        out.stdout
    );
    assert!(
        std::fs::read_to_string(&manifest).unwrap().contains("vite"),
        "the scaffolded manifest is create-vite's vanilla template"
    );
}

/// Fabricate a warm pnpm store entry at `<XDG_CACHE_HOME>/nub/pm/pnpm/<v>/` —
/// the layout `cached_bin` reads (`package/package.json` with a `bin` map plus
/// the named bin file present). Enough to satisfy `provision::pm_version_cached`
/// without a real download, so the offline short-circuit can be exercised.
fn seed_pnpm_store(cache: &Path, version: &str) {
    let pkg = cache.join("nub/pm/pnpm").join(version).join("package");
    std::fs::create_dir_all(pkg.join("bin")).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        format!(r#"{{"name":"pnpm","version":"{version}","bin":{{"pnpm":"bin/pnpm.cjs"}}}}"#),
    )
    .unwrap();
    std::fs::write(pkg.join("bin/pnpm.cjs"), "// fake pnpm bin\n").unwrap();
}

/// The warm exact re-pin short-circuit: when the manifest already pins
/// `pnpm@<exact>+sha512.<hex>` and that version is extracted in the store,
/// `nub pm use pnpm@<exact>` must reuse the on-disk hash and touch zero network —
/// no `Fetching` line, declaration preserved verbatim. A `.npmrc` aimed at a dead
/// port makes any stray fetch fail loudly, so a clean exit IS the zero-download
/// proof. The companion assertion: a RANGE spec (`pnpm@^9`) does NOT short-circuit
/// — it must resolve through the registry and so dies against the dead port.
#[test]
fn warm_exact_re_pin_skips_the_network_while_a_range_still_resolves() {
    let dir = pm_tmpdir("warmpin");
    let (data, cache) = (pm_tmpdir("warmpin-data"), pm_tmpdir("warmpin-cache"));
    let version = "9.1.0";
    // The committed hex is reused verbatim, never re-verified — its value is
    // irrelevant to the short-circuit, only its `+sha512.` shape matters.
    let declared =
        r#"{"name":"warmpin","version":"1.0.0","packageManager":"pnpm@9.1.0+sha512.deadbeef"}"#;
    std::fs::write(dir.join("package.json"), declared).unwrap();
    // Dead registry: any fetch/resolve that reaches the network fails fast.
    std::fs::write(dir.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    seed_pnpm_store(&cache, version);

    let spawn = |args: &[&str]| -> Output {
        let out = Command::new(nub_binary())
            .args(args)
            .current_dir(&dir)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CACHE_HOME", &cache)
            .env_remove("npm_config_registry") // a dev-box override can't reroute the dead port
            .output()
            .expect("failed to spawn nub");
        Output {
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            code: out.status.code().unwrap_or(-1),
        }
    };

    // Warm exact re-pin: zero network, no Fetching line, declaration kept.
    let warm = spawn(&["pm", "use", "pnpm@9.1.0"]);
    assert_eq!(
        warm.code, 0,
        "warm exact re-pin must succeed offline:\nstdout: {}\nstderr: {}",
        warm.stdout, warm.stderr
    );
    assert!(
        !warm.combined().contains("Fetching"),
        "nothing was fetched, so no Fetching line may print:\n{}",
        warm.combined()
    );
    let after = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(
        after.contains("\"pnpm@9.1.0+sha512.deadbeef\""),
        "the existing exact+hash pin must survive the re-pin verbatim:\n{after}"
    );
    warm.assert_brand_clean();

    // A range spec is NOT the same version literal — it must resolve through the
    // registry, which is dead here, so it fails. (Proves the short-circuit is
    // gated to exact specs, never ranges/dist-tags.)
    let range = spawn(&["pm", "use", "pnpm@^9"]);
    assert_ne!(
        range.code, 0,
        "a range spec must resolve via the registry (dead here), not short-circuit:\nstdout: {}\nstderr: {}",
        range.stdout, range.stderr
    );
}

/// Brand boundary on the not-a-command front door. pnpm 10.15 delegates its
/// own "not implemented" set (`access`/`edit`/`issues`/`profile`/`team`/…) to
/// the npm CLI, so `pnpm access` prints `npm error code EUSAGE`. nub must NOT
/// inherit that leak: a command nub does not implement is refused with a
/// nub-branded message, never an `npm error` (or `aube`) line, on every output
/// stream. This locks the brand contract regardless of how the refusal message
/// is later worded or which exit code it carries.
///
/// `set-script` and `token` are deliberately excluded — nub ships native verbs
/// for both (a superset of pnpm's not-implemented list, v0.1.9), verified
/// clean elsewhere; they are not refused.
#[test]
fn unimplemented_pm_commands_never_leak_npm() {
    let dir = pm_tmpdir("noleak");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"x","version":"1.0.0"}"#,
    )
    .unwrap();

    // pnpm's npm-delegated "not implemented" set, plus a wholly unsupported
    // word. None of these is a nub command; each must be refused brand-clean.
    for cmd in [
        "access",
        "edit",
        "issues",
        "prefix",
        "profile",
        "team",
        "xmas",
        "totallyfakecommand",
    ] {
        let out = run_nub(&dir, &[cmd]);
        assert_ne!(out.code, 0, "`nub {cmd}` is not a command — must fail");
        out.assert_brand_clean();
        let lower = out.combined().to_lowercase();
        assert!(
            !lower.contains("npm error"),
            "`nub {cmd}` must not leak an npm error (pnpm delegates these to npm; nub does not):\nstdout: {}\nstderr: {}",
            out.stdout,
            out.stderr
        );
        assert!(
            out.combined().contains("nub"),
            "the refusal must be nub-branded:\nstdout: {}\nstderr: {}",
            out.stdout,
            out.stderr
        );
    }
}
