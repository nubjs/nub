//! Publish + store/config family verbs end-to-end through the real binary:
//! `nub pack`, `nub publish --dry-run`, `nub store path`, and the
//! npmrc-first `config get`/`set` routing. All offline (pack/publish
//! --dry-run never touch the registry; store path only opens the local
//! store) — no `#[ignore]` legs.
//!
//! Every run asserts the brand boundary: no `aube`/`AUBE` token and no
//! engine doc URL may appear in stdout or stderr.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// A unique temp dir under the system temp root (never under $HOME, so
/// manifest/lockfile walk-ups can't escape into stray ancestors).
fn pm_tmpdir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-pmfam-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One isolated invocation context: project dir plus a fake HOME (user
/// `.npmrc`, XDG roots) so tests never read or write the dev box's real
/// config, store, or cache.
struct Ctx {
    project: PathBuf,
    home: PathBuf,
}

impl Ctx {
    fn new(tag: &str, manifest: &str) -> Self {
        let project = pm_tmpdir(tag);
        let home = pm_tmpdir(&format!("{tag}-home"));
        std::fs::write(project.join("package.json"), manifest).unwrap();
        Ctx { project, home }
    }

    fn run(&self, args: &[&str]) -> (String, String, i32) {
        self.run_env_in(&self.project, args, &[])
    }

    /// Like [`Ctx::run`], from an explicit working directory. Store resolution
    /// walks UP from the cwd, so a test that must distinguish "anchored at the
    /// project root" from "anchored at the cwd" has to invoke from somewhere
    /// other than the root itself.
    fn run_in(&self, cwd: &Path, args: &[&str]) -> (String, String, i32) {
        self.run_env_in(cwd, args, &[])
    }

    /// Like [`Ctx::run`], with extra environment variables.
    fn run_env(&self, args: &[&str], envs: &[(&str, &str)]) -> (String, String, i32) {
        self.run_env_in(&self.project, args, envs)
    }

    /// The one spawn site: an explicit working directory plus extra environment.
    fn run_env_in(
        &self,
        cwd: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> (String, String, i32) {
        let mut cmd = Command::new(nub_binary());
        cmd.args(args)
            .current_dir(cwd)
            // The fixture pins a differing `nub@<v>` to exercise nub identity, not
            // the self-shim — opt out so a PM verb doesn't provision that nub.
            .env("NUB_SELF_SHIM", "0")
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", self.home.join("xdg-data"))
            .env("XDG_CACHE_HOME", self.home.join("xdg-cache"))
            .env("XDG_CONFIG_HOME", self.home.join("xdg-config"));
        for (name, value) in envs {
            cmd.env(name, value);
        }
        let out = cmd.output().expect("failed to spawn nub");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert_brand_clean(args, &stdout, &stderr);
        (stdout, stderr, out.status.code().unwrap_or(-1))
    }
}

/// The hard requirement: no engine branding and no engine doc URLs in
/// anything nub prints. Real on-disk paths under an `aube`-named segment
/// can't occur here because every test isolates HOME/XDG into fresh roots
/// whose paths carry no such segment.
fn assert_brand_clean(args: &[&str], stdout: &str, stderr: &str) {
    for (stream, text) in [("stdout", stdout), ("stderr", stderr)] {
        assert!(
            !text.to_lowercase().contains("aube"),
            "`nub {}` leaked engine branding on {stream}:\n{text}",
            args.join(" ")
        );
        assert!(
            !text.contains("jdx.dev"),
            "`nub {}` leaked an engine doc URL on {stream}:\n{text}",
            args.join(" ")
        );
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

const MANIFEST: &str = r#"{"name":"pmfam-fixture","version":"1.2.3","files":["index.js"]}"#;

/// `nub pack` builds the tarball offline, names it `<name>-<version>.tgz`,
/// and `--json` reports the same filename as data on stdout.
#[test]
fn pack_writes_the_tarball_and_reports_it_as_json() {
    let ctx = Ctx::new("pack", MANIFEST);
    std::fs::write(ctx.project.join("index.js"), "module.exports = 1;\n").unwrap();

    let (stdout, stderr, code) = ctx.run(&["pack", "--json"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        ctx.project.join("pmfam-fixture-1.2.3.tgz").is_file(),
        "pack must write the tarball next to package.json: {stdout}"
    );
    // pnpm-compatible shape: an array of per-package results.
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("pack --json stdout must be JSON ({e}): {stdout}"));
    assert_eq!(
        json[0]["filename"].as_str(),
        Some("pmfam-fixture-1.2.3.tgz"),
        "pack --json must name the tarball: {stdout}"
    );
}

/// `nub publish --dry-run` runs the full pre-publish chain (archive build
/// included) without any network and exits 0; nothing is uploaded, no
/// tarball is left behind.
#[test]
fn publish_dry_run_stays_offline_and_exits_clean() {
    let ctx = Ctx::new("publish-dry", MANIFEST);
    std::fs::write(ctx.project.join("index.js"), "module.exports = 1;\n").unwrap();

    // --no-git-checks: the temp dir may live inside an unrelated git
    // worktree on a non-release branch; the gate under test is dry-run.
    let (stdout, stderr, code) = ctx.run(&["publish", "--dry-run", "--no-git-checks", "--json"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("publish --json stdout must be JSON ({e}): {stdout}"));
    assert_eq!(json["name"].as_str(), Some("pmfam-fixture"), "{stdout}");
    assert_eq!(json["version"].as_str(), Some("1.2.3"), "{stdout}");
}

/// `nub store path` prints the resolved store-version dir — under nub's
/// embedder defaults that is `$XDG_DATA_HOME/nub/store/v1`, never an
/// `aube`-named location.
#[test]
fn store_path_prints_the_nub_namespaced_store() {
    let ctx = Ctx::new("store-path", MANIFEST);
    let (stdout, stderr, code) = ctx.run(&["store", "path"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    // Separator-normalize before comparing: on Windows the engine prints
    // native `\` components while the expectation is joined with `/`.
    let expected = ctx.home.join("xdg-data/nub/store/v1");
    assert_eq!(
        stdout.trim().replace('\\', "/"),
        expected.to_string_lossy().replace('\\', "/"),
        "store path must resolve through nub's storeDir default"
    );
}

/// `nub config path` prints the global settings file under the resolved config
/// home. The fixture never creates it: the path must print for a machine with no
/// settings yet (`$EDITOR "$(nub config path)"`), and printing it must not
/// materialize the file. The `c` alias reaches the same subcommand.
#[test]
fn config_path_prints_the_unwritten_global_settings_file() {
    let ctx = Ctx::new("config-path", MANIFEST);
    let expected = ctx.home.join("xdg-config/nub/nub.jsonc");

    let (stdout, stderr, code) = ctx.run(&["config", "path"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        stdout.trim().replace('\\', "/"),
        expected.to_string_lossy().replace('\\', "/"),
        "config path must resolve through nub's config home"
    );
    assert!(
        !expected.exists(),
        "printing the path must not create the settings file"
    );

    let (alias_stdout, alias_stderr, alias_code) = ctx.run(&["c", "path"]);
    assert_eq!(alias_code, 0, "stderr: {alias_stderr}");
    assert_eq!(
        alias_stdout, stdout,
        "the `c` alias must print the same path"
    );
}

/// The setting takes the store ROOT; `store path` prints the `v1` dir under it.
///
/// Both sides are canonicalized, which is the only comparison that survives
/// every platform's path normalization at once: Windows `canonicalize` returns
/// a `\\?\` verbatim path AND expands 8.3 short names (`RUNNER~1` →
/// `runneradmin`), while macOS resolves the `/var` → `/private/var` symlink.
/// Normalizing one side only — or just swapping separators — leaves a real
/// match reading as a failure. `canonicalize` needs the path to exist, and
/// `store path` creates nothing, so both are materialized first.
fn assert_store_path(stdout: &str, stderr: &str, code: i32, root: &Path) {
    assert_eq!(code, 0, "stderr: {stderr}");
    let printed = PathBuf::from(stdout.trim());
    let expected = root.join("v1");
    std::fs::create_dir_all(&printed).unwrap();
    std::fs::create_dir_all(&expected).unwrap();
    assert_eq!(
        printed.canonicalize().unwrap(),
        expected.canonicalize().unwrap(),
        "store path must resolve to the configured store-dir plus the v1 suffix \
         (printed {}, expected {})",
        printed.display(),
        expected.display()
    );
}

#[test]
fn store_path_honors_npm_config_store_dir_env() {
    let ctx = Ctx::new("store-env", MANIFEST);
    let relocated = ctx.home.join("relocated-store");
    let (stdout, stderr, code) = ctx.run_env(
        &["store", "path"],
        &[("npm_config_store_dir", relocated.to_str().unwrap())],
    );
    assert_store_path(&stdout, &stderr, code, &relocated);
}

#[test]
fn store_path_honors_project_npmrc_store_dir() {
    let ctx = Ctx::new("store-npmrc", MANIFEST);
    let relocated = ctx.home.join("npmrc-store");
    std::fs::write(
        ctx.project.join(".npmrc"),
        format!("store-dir={}\n", relocated.display()),
    )
    .unwrap();
    let (stdout, stderr, code) = ctx.run(&["store", "path"]);
    assert_store_path(&stdout, &stderr, code, &relocated);
}

/// From inside a workspace MEMBER, `store path` must report the store the
/// workspace root configures — which is the store installs from that member
/// actually use. Anchoring at the nearest `package.json` reported the default
/// instead, so the command contradicted the installs it describes. Real pnpm
/// reports the override from both locations.
#[test]
fn store_path_reports_the_workspace_store_from_a_member() {
    let (ctx, member) = workspace("store-ws-path");
    let relocated = ctx.home.join("ws-path-store");
    // The override lives at the workspace ROOT only; the member declares nothing.
    std::fs::write(
        ctx.project.join(".npmrc"),
        format!("store-dir={}\n", relocated.display()),
    )
    .unwrap();

    let (stdout, stderr, code) = ctx.run_in(&member, &["store", "path"]);
    assert_store_path(&stdout, &stderr, code, &relocated);
}

#[test]
fn store_path_resolves_relative_store_dir_against_the_project() {
    let ctx = Ctx::new("store-rel", MANIFEST);
    std::fs::write(ctx.project.join(".npmrc"), "store-dir=local-store\n").unwrap();
    // Invoked from a SUBDIRECTORY, so "against the project root" and "against
    // the cwd" are different answers and the test can tell them apart. Running
    // at the root makes both the same directory and asserts nothing.
    let nested = ctx.project.join("src/nested");
    std::fs::create_dir_all(&nested).unwrap();
    let (stdout, stderr, code) = ctx.run_env_in(&nested, &["store", "path"], &[]);
    let project = ctx.project.canonicalize().unwrap();
    assert_store_path(&stdout, &stderr, code, &project.join("local-store"));
}

#[test]
fn store_dir_env_wins_over_project_npmrc() {
    let ctx = Ctx::new("store-prec", MANIFEST);
    let from_env = ctx.home.join("env-store");
    std::fs::write(
        ctx.project.join(".npmrc"),
        format!("store-dir={}\n", ctx.home.join("npmrc-store").display()),
    )
    .unwrap();
    let (stdout, stderr, code) = ctx.run_env(
        &["store", "path"],
        &[("npm_config_store_dir", from_env.to_str().unwrap())],
    );
    assert_store_path(&stdout, &stderr, code, &from_env);
}

/// Pack a fixture whose undeclared import gives the extract-time phantom scan
/// a verdict to record — without one, the sidecar tier is empty and a test
/// asserting on its location passes for the wrong reason.
fn pack_phantom_fixture(tag: &str) -> PathBuf {
    let packer = Ctx::new(tag, MANIFEST);
    std::fs::write(
        packer.project.join("index.js"),
        "require('undeclared-phantom');\n",
    )
    .unwrap();
    let (stdout, stderr, code) = packer.run(&["pack"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let tarball = packer.project.join("pmfam-fixture-1.2.3.tgz");
    assert!(tarball.is_file(), "pack must produce the fixture tarball");
    tarball
}

/// Every store tier — CAS and phantom sidecar alike — landed under `relocated`,
/// and the default store location was never even created.
fn assert_every_tier_relocated(home: &Path, relocated: &Path) {
    assert!(
        relocated.join("v1/files").is_dir(),
        "the CAS files tier must land under the override"
    );
    assert!(
        count_files(&relocated.join("v1/phantom")) > 0,
        "the phantom sidecar tier must move with the store override"
    );
    let default_store = home.join("xdg-data/nub/store");
    assert!(
        !default_store.exists(),
        "an overridden install must write NOTHING to the default store location: {:?}",
        std::fs::read_dir(&default_store).map(|it| it
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect::<Vec<_>>())
    );
}

/// A relocated store must take every tier with it — the phantom sidecar tier
/// used to stay at the default data dir, leaking writes into the real store
/// and leaving detection reading a store the packages aren't in (#643).
#[test]
fn store_dir_override_relocates_every_store_tier() {
    let tarball = pack_phantom_fixture("store-tiers-pack");
    let ctx = Ctx::new(
        "store-tiers",
        r#"{"name":"store-tiers-consumer","version":"1.0.0"}"#,
    );
    std::fs::copy(&tarball, ctx.project.join("pmfam-fixture-1.2.3.tgz")).unwrap();
    let relocated = ctx.home.join("relocated-store");
    let (stdout, stderr, code) = ctx.run_env(
        &["add", "./pmfam-fixture-1.2.3.tgz"],
        &[("npm_config_store_dir", relocated.to_str().unwrap())],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_every_tier_relocated(&ctx.home, &relocated);
}

/// The same invariant from inside a workspace MEMBER, with the override
/// declared at the workspace root — the shape a monorepo actually installs in.
///
/// This is the case an anchor bug hides in: `.npmrc` and `pnpm-workspace.yaml`
/// discovery does not walk up, while the install pipeline anchors at the
/// walked-up workspace root. A sidecar tier resolved against the raw process
/// cwd therefore sees no override at all and silently returns to the default
/// store, even though the CAS correctly relocates (#643).
#[test]
fn store_dir_override_relocates_every_tier_from_a_workspace_member() {
    let tarball = pack_phantom_fixture("store-ws-pack");

    let (ctx, member) = workspace("store-ws");
    let relocated = ctx.home.join("ws-relocated-store");
    // The override lives at the workspace ROOT only; the member declares nothing.
    std::fs::write(
        ctx.project.join(".npmrc"),
        format!("store-dir={}\n", relocated.display()),
    )
    .unwrap();
    std::fs::copy(&tarball, member.join("pmfam-fixture-1.2.3.tgz")).unwrap();

    let (stdout, stderr, code) = ctx.run_in(&member, &["add", "./pmfam-fixture-1.2.3.tgz"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_every_tier_relocated(&ctx.home, &relocated);
}

/// Recursive file count under `dir`; 0 when the directory does not exist.
fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| {
            let path = e.path();
            if path.is_dir() { count_files(&path) } else { 1 }
        })
        .sum()
}

/// The default initializer creates a behavior-neutral project file: editor
/// metadata is active, every project setting is discoverable, and the
/// machine-wide `dlx` policy is absent.
#[test]
fn config_init_creates_the_commented_project_template() {
    let ctx = Ctx::new("config-init-project", MANIFEST);
    let path = ctx.project.join("nub.jsonc");

    let (stdout, stderr, code) = ctx.run(&["config", "init"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains(&path.to_string_lossy().to_string()),
        "{stdout}"
    );
    let body = read(&path);
    assert!(
        body.contains(r#""$schema": "https://nubjs.com/schema/latest.json""#),
        "{body}"
    );
    for field in [
        "preload",
        "nodeOptions",
        "v8Flags",
        "nodeCompat",
        "envFile",
        "loader",
        "conditions",
        "tsconfig",
        "verifyDeps",
        "install",
        "linker",
        "publicHoist",
        "minimumReleaseAge",
        "minimumReleaseAgeExclude",
    ] {
        assert!(
            body.contains(&format!(r#""{field}":"#)),
            "omits {field}: {body}"
        );
    }
    assert!(
        !body.contains(r#""dlx":"#),
        "project lists global dlx: {body}"
    );

    let (value, stderr, code) = ctx.run(&["config", "get", "nodeCompat"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value.trim(), "undefined", "template changed a default");
}

/// Initialization uses the same project-file resolver as `config set`: from a
/// package subdirectory it creates at the package.json root, and an existing
/// up-tree config is the file protected by the no-clobber guarantee.
#[test]
fn config_init_from_a_subdirectory_targets_the_project_file() {
    let ctx = Ctx::new("config-init-nested", MANIFEST);
    let nested = ctx.project.join("packages/app");
    std::fs::create_dir_all(&nested).unwrap();
    let project_file = ctx.project.join("nub.jsonc");

    let (stdout, stderr, code) = ctx.run_in(&nested, &["config", "init"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(project_file.is_file(), "{stdout}");
    assert!(!nested.join("nub.jsonc").exists(), "{stdout}");

    let before = std::fs::read(&project_file).unwrap();
    let (_, stderr, code) = ctx.run_in(&nested, &["config", "init"]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(
        stderr.contains(&project_file.display().to_string()),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&project_file).unwrap(), before);
}

/// A workspace root holding one member at `packages/app`, both with manifests.
fn workspace(tag: &str) -> (Ctx, PathBuf) {
    let ctx = Ctx::new(
        tag,
        r#"{"name":"ws-root","version":"1.0.0","private":true,"workspaces":["packages/*"]}"#,
    );
    let member = ctx.project.join("packages/app");
    std::fs::create_dir_all(&member).unwrap();
    std::fs::write(
        member.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    (ctx, member)
}

/// A workspace member is not its own config scope. One workspace materializes
/// one tree under one engine, so `init` from a member anchors at the workspace
/// root instead of dropping a `nub.jsonc` in every package — and `set` resolves
/// the same file, so the two cannot disagree about where the config lives.
#[test]
fn config_init_from_a_workspace_member_targets_the_workspace_root() {
    let (ctx, member) = workspace("config-init-workspace");
    let root_file = ctx.project.join("nub.jsonc");

    let (stdout, stderr, code) = ctx.run_in(&member, &["config", "init"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains(&root_file.display().to_string()),
        "init must report the workspace-root path: {stdout}"
    );
    assert!(
        root_file.is_file(),
        "no file at the workspace root: {stdout}"
    );
    assert!(
        !member.join("nub.jsonc").exists(),
        "init created a second config in the member: {stdout}"
    );
}

/// The write path resolves the same root. Deliberately a FRESH workspace with no
/// `init` first: the template comments out every project field, so `"nodeCompat":
/// true` appears in an initialized file whether or not the write landed, and the
/// assertion would hold on the template alone.
#[test]
fn config_set_from_a_workspace_member_writes_the_workspace_root() {
    let (ctx, member) = workspace("config-set-workspace");
    let root_file = ctx.project.join("nub.jsonc");

    let (_, stderr, code) = ctx.run_in(&member, &["config", "set", "nodeCompat", "true"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let body = read(&root_file);
    assert!(
        body.contains("\"nodeCompat\": true"),
        "a member `set` must write the workspace-root file: {body}"
    );
    assert!(
        !member.join("nub.jsonc").exists(),
        "a member `set` forked a second config"
    );
}

/// Deliberate, and it reads like a gap: `detect_project` never glob-matches the
/// cwd against the workspace patterns, so a nested package the patterns EXCLUDE
/// anchors at the root too. That is the point — absent a local file the root
/// config is already what a run there READS, so the write edits the file that
/// governs it, where minting a local one would shadow the root for that subtree.
///
/// Nothing writes before this, deliberately: once a root `nub.jsonc` exists,
/// plain up-tree discovery resolves it and the workspace rung this pins is never
/// consulted.
#[test]
fn config_set_from_an_excluded_nested_package_writes_the_workspace_root() {
    let (ctx, _member) = workspace("config-set-excluded");
    let excluded = ctx.project.join("examples/demo");
    std::fs::create_dir_all(&excluded).unwrap();
    std::fs::write(excluded.join("package.json"), r#"{"name":"demo"}"#).unwrap();

    let (_, stderr, code) = ctx.run_in(&excluded, &["config", "set", "tsconfig", "./t.json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        !excluded.join("nub.jsonc").exists(),
        "an excluded package must not shadow the workspace config"
    );
    let body = read(&ctx.project.join("nub.jsonc"));
    assert!(body.contains("./t.json"), "write missed the root: {body}");
}

/// Both public global spellings create the user template and add the
/// global-only consent example without enabling it.
#[test]
fn config_init_global_spellings_create_the_global_template() {
    for (name, args) in [
        ("flag", &["config", "init", "--global"][..]),
        ("short-flag", &["config", "init", "-g"][..]),
        ("prefix", &["global", "config", "init"][..]),
    ] {
        let ctx = Ctx::new(&format!("config-init-{name}"), MANIFEST);
        let path = ctx.home.join("xdg-config/nub/nub.jsonc");

        let (stdout, stderr, code) = ctx.run(args);
        assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
        let reported = stdout
            .trim()
            .strip_prefix("Created ")
            .unwrap_or_else(|| panic!("{stdout}"));
        assert_eq!(
            std::fs::canonicalize(reported).unwrap(),
            std::fs::canonicalize(&path).unwrap(),
            "{stdout}"
        );
        let body = read(&path);
        assert!(body.contains(r#""dlx": { "consent": "prompt" }"#), "{body}");
        assert!(
            !ctx.project.join("nub.jsonc").exists(),
            "a user-global init must not create a project file"
        );

        let (value, stderr, code) = ctx.run(&["config", "get", "dlx.consent"]);
        assert_eq!(code, 0, "stderr: {stderr}");
        assert_eq!(value.trim(), "undefined", "template enabled consent");
    }
}

/// Credentials default to the user file so an unqualified write cannot place
/// a token in a commonly tracked project `.npmrc`. The report is redacted,
/// deletion follows the same default, and explicit `--local` remains available.
#[test]
fn config_auth_defaults_to_user_scope_unless_local_is_explicit() {
    let ctx = Ctx::new("config-auth-scope", MANIFEST);
    let key = "//registry.example.test/:_authToken";
    let token = "scope-secret";

    let (stdout, stderr, code) = ctx.run(&["config", "set", key, token]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !stdout.contains(token) && !stderr.contains(token),
        "{stdout}{stderr}"
    );
    assert!(stderr.contains("(protected)"), "{stderr}");
    assert!(read(&ctx.home.join(".npmrc")).contains(&format!("{key}={token}")));
    assert!(!read(&ctx.project.join(".npmrc")).contains(key));

    let nub_routed_key = "_machineCredential";
    let nub_routed_token = "another-secret";
    let (_, stderr, code) = ctx.run(&["config", "set", nub_routed_key, nub_routed_token]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!stderr.contains(nub_routed_token), "{stderr}");
    assert!(stderr.contains("(protected)"), "{stderr}");
    assert!(
        read(&ctx.home.join(".npmrc")).contains(&format!("{nub_routed_key}={nub_routed_token}"))
    );

    let (_, stderr, code) = ctx.run(&["config", "delete", key]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!read(&ctx.home.join(".npmrc")).contains(key));

    let (stdout, stderr, code) = ctx.run(&["config", "set", "--local", key, token]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !stdout.contains(token) && !stderr.contains(token),
        "{stdout}{stderr}"
    );
    assert!(read(&ctx.project.join(".npmrc")).contains(&format!("{key}={token}")));
    assert!(!read(&ctx.home.join(".npmrc")).contains(key));
}

#[test]
fn config_init_refuses_existing_files_and_removed_location_flag() {
    let ctx = Ctx::new("config-init-refuse", MANIFEST);
    let path = ctx.project.join("nub.jsonc");
    let original = b"{\n  // mine\n}\n";
    std::fs::write(&path, original).unwrap();

    let (_, stderr, code) = ctx.run(&["config", "init", "--local"]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert!(stderr.contains("nothing was written"), "{stderr}");
    assert_eq!(std::fs::read(&path).unwrap(), original);

    std::fs::remove_file(&path).unwrap();
    let (_, stderr, code) = ctx.run(&["config", "init", "--location", "user"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("unexpected argument '--location'"),
        "{stderr}"
    );
    assert!(!path.exists(), "invalid scope must not create the file");
}

/// A pnpm-**v11** manifest. v11 reads scalar settings solely from
/// `pnpm-workspace.yaml`, so non-shared scalars route there.
const PNPM11_MANIFEST: &str =
    r#"{"name":"pmfam-fixture","version":"1.2.3","packageManager":"pnpm@11.3.0"}"#;

/// A pnpm-**v10** manifest. v10 reads scalars from `.npmrc`, so non-shared
/// scalars route to the project `.npmrc` (round-trips with real pnpm@10) —
/// NOT to `pnpm-workspace.yaml` (the bug this guards against).
const PNPM10_MANIFEST: &str =
    r#"{"name":"pmfam-fixture","version":"1.2.3","packageManager":"pnpm@10.15.1"}"#;

/// A pnpm incumbent with NO declared version: `packageManager: "pnpm"` (bare
/// name, no `@version`) resolves to a pnpm surface with an unknown major,
/// exercising the unknown-version default. (A versionless name is what
/// `declared_pm_raw` returns name=pnpm/version=None for.)
const PNPM_UNVERSIONED_MANIFEST: &str =
    r#"{"name":"pmfam-fixture","version":"1.2.3","packageManager":"pnpm"}"#;

/// Generic pnpm-incumbent manifest used where the SCALAR home is irrelevant
/// (the global read/write tests). Points at v11.
const PNPM_MANIFEST: &str = PNPM11_MANIFEST;

/// Write routing under a pnpm-**v11** incumbent: a non-shared scalar lands in
/// `pnpm-workspace.yaml` (created if absent) for round-trip fidelity with pnpm
/// v11; an npm-shared key (registry) delegates to the engine and lands in the
/// project `.npmrc`. No `config.toml` is ever written, and `config get` reads
/// both values back.
#[test]
fn config_set_under_pnpm_v11_incumbent_routes_scalar_to_workspace_yaml() {
    let ctx = Ctx::new("config-pnpm11", PNPM11_MANIFEST);

    // Non-shared scalar → pnpm-workspace.yaml (top-level `set` shorthand).
    let (_, stderr, code) = ctx.run(&["set", "auto-install-peers", "false"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let ws_yaml = read(&ctx.project.join("pnpm-workspace.yaml"));
    assert!(
        ws_yaml.contains("autoInstallPeers") && ws_yaml.contains("false"),
        "non-shared scalar must land in pnpm-workspace.yaml under a pnpm incumbent: {ws_yaml:?}"
    );
    assert!(
        !read(&ctx.project.join(".npmrc")).contains("auto-install-peers"),
        "under a pnpm incumbent the scalar must NOT go to the project .npmrc"
    );

    // npm-shared key → project .npmrc via the engine's own writer.
    let (_, stderr, code) = ctx.run(&["config", "set", "registry", "https://r.example.test/"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        read(&ctx.project.join(".npmrc")).contains("registry=https://r.example.test/"),
        "npm-shared key must land in the project .npmrc"
    );

    // No config.toml anywhere (the hard line — nub never writes config.toml).
    for forbidden in [ctx.home.join("xdg-config"), ctx.project.join(".config")] {
        assert!(
            !forbidden.join("aube/config.toml").exists()
                && !forbidden.join("nub/config.toml").exists(),
            "config set must never write a config.toml under {}",
            forbidden.display()
        );
    }

    // Read-back: the YAML value resolves (YAML outranks .npmrc, pnpm v11).
    let (stdout, _, code) = ctx.run(&["get", "autoInstallPeers"]);
    assert_eq!((stdout.trim(), code), ("false", 0));
    let (stdout, _, code) = ctx.run(&["config", "get", "registry"]);
    assert_eq!((stdout.trim(), code), ("https://r.example.test/", 0));

    // Workspace map settings are refused with the pnpm-workspace.yaml
    // pointer instead of writing a package.json field or an unread line.
    let (_, stderr, code) = ctx.run(&["set", "allowBuilds.esbuild", "true"]);
    assert_ne!(code, 0, "map-entry write must be refused");
    assert!(
        stderr.contains("pnpm-workspace.yaml"),
        "refusal must point at pnpm-workspace.yaml: {stderr}"
    );
    assert!(
        !read(&ctx.project.join("package.json")).contains("allowBuilds"),
        "refused map write must not touch package.json"
    );
}

/// Layout settings use the neutral `.npmrc` even under pnpm 11, while ordinary
/// pnpm 11 scalars continue to use `pnpm-workspace.yaml`. Delete removes both
/// the current `.npmrc` value and any stale workspace-YAML copy left by an older
/// Nub build.
#[test]
fn config_set_and_delete_pnpm_v11_layout_through_npmrc() {
    let ctx = Ctx::new("config-pnpm11-layout", PNPM11_MANIFEST);

    let (_, stderr, code) = ctx.run(&["config", "set", "node-linker", "hoisted"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        read(&ctx.project.join(".npmrc")).contains("node-linker=hoisted"),
        "the layout setting must land in the neutral project .npmrc"
    );
    assert!(
        !read(&ctx.project.join("pnpm-workspace.yaml")).contains("nodeLinker"),
        "pnpm 11 workspace YAML is not a readable layout source"
    );

    let (stdout, stderr, code) = ctx.run(&["config", "get", "node-linker"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout.trim(), "hoisted");

    std::fs::write(
        ctx.project.join("pnpm-workspace.yaml"),
        "nodeLinker: isolated\nautoInstallPeers: false\n",
    )
    .unwrap();
    let (_, stderr, code) = ctx.run(&["config", "delete", "node-linker"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        !read(&ctx.project.join(".npmrc")).contains("node-linker"),
        "delete must remove the value from the file config set populated"
    );
    let ws_yaml = read(&ctx.project.join("pnpm-workspace.yaml"));
    assert!(
        !ws_yaml.contains("nodeLinker"),
        "delete must also remove a stale workspace-YAML layout value: {ws_yaml:?}"
    );
    assert!(
        ws_yaml.contains("autoInstallPeers"),
        "delete must preserve unrelated workspace-YAML settings: {ws_yaml:?}"
    );

    let (stdout, stderr, code) = ctx.run(&["config", "get", "node-linker"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout.trim(), "isolated");

    let (_, stderr, code) = ctx.run(&["config", "delete", "node-linker"]);
    assert_ne!(code, 0, "deleting an absent setting must fail");
    assert!(
        stderr.contains("not set") && stderr.contains(".npmrc"),
        "the error must name the swept neutral config surface: {stderr}"
    );
}

/// A nub-identity manifest (declares `packageManager: nub@…`), so the config
/// surface resolves to nub identity. Non-shared scalars route to the NEUTRAL
/// project `.npmrc` — never a pnpm-branded `pnpm-workspace.yaml`, never
/// `config.toml` (brand boundary). An npm-shared key still goes to `.npmrc`.
const NUB_MANIFEST: &str =
    r#"{"name":"pmfam-fixture","version":"1.2.3","packageManager":"nub@0.1.0"}"#;

#[test]
fn config_set_under_nub_identity_routes_scalar_to_neutral_npmrc() {
    let ctx = Ctx::new("config-nub", NUB_MANIFEST);

    // Non-shared scalar → project .npmrc (the neutral home); NO pnpm-branded
    // file is emitted under nub identity.
    let (_, stderr, code) = ctx.run(&["set", "auto-install-peers", "false"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        read(&ctx.project.join(".npmrc")).contains("auto-install-peers=false"),
        "under nub identity a non-shared scalar must land in the neutral project .npmrc"
    );
    assert!(
        !ctx.project.join("pnpm-workspace.yaml").exists(),
        "nub identity must NEVER write a pnpm-branded pnpm-workspace.yaml (brand boundary)"
    );

    // No config.toml under nub identity either.
    for forbidden in [ctx.home.join("xdg-config"), ctx.project.join(".config")] {
        assert!(
            !forbidden.join("aube/config.toml").exists()
                && !forbidden.join("nub/config.toml").exists(),
            "config set must never write a config.toml under {}",
            forbidden.display()
        );
    }

    let (stdout, _, code) = ctx.run(&["get", "autoInstallPeers"]);
    assert_eq!((stdout.trim(), code), ("false", 0));
}

/// Write routing under a pnpm-**v10** incumbent: v10 reads scalar settings
/// from `.npmrc`, so a non-shared scalar must land there (and round-trip), NOT
/// in `pnpm-workspace.yaml`. This is the correctness bug the version-aware
/// router fixes: a v11-shaped yaml write would silently no-op on v10.
#[test]
fn config_set_under_pnpm_v10_incumbent_routes_scalar_to_npmrc() {
    let ctx = Ctx::new("config-pnpm10", PNPM10_MANIFEST);

    let (_, stderr, code) = ctx.run(&["set", "auto-install-peers", "false"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        read(&ctx.project.join(".npmrc")).contains("auto-install-peers=false"),
        "under a pnpm-v10 incumbent a non-shared scalar must land in the project .npmrc"
    );
    assert!(
        !ctx.project.join("pnpm-workspace.yaml").exists(),
        "pnpm-v10 scalar must NOT be written to pnpm-workspace.yaml (v10 wouldn't read it back)"
    );

    // Read-back works (the resolver reads scalars from .npmrc too).
    let (stdout, _, code) = ctx.run(&["get", "autoInstallPeers"]);
    assert_eq!((stdout.trim(), code), ("false", 0));
}

/// Unknown pnpm version (no `packageManager` pin) → the dominant/most-
/// compatible default: the v10 `.npmrc` model. A non-shared scalar lands in
/// `.npmrc`, never a pnpm-branded yaml.
#[test]
fn config_set_under_unversioned_pnpm_defaults_to_npmrc() {
    let ctx = Ctx::new("config-pnpm-unversioned", PNPM_UNVERSIONED_MANIFEST);

    let (_, stderr, code) = ctx.run(&["set", "auto-install-peers", "false"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        read(&ctx.project.join(".npmrc")).contains("auto-install-peers=false"),
        "unknown pnpm version must default to the .npmrc model"
    );
    assert!(
        !ctx.project.join("pnpm-workspace.yaml").exists(),
        "unknown-version default must NOT write pnpm-workspace.yaml"
    );
}

/// An UNKNOWN declared PM name at a high major must NOT leak a pnpm-branded
/// file. `resolve_config_surface` maps an unknown declared tool (`deno`, …) to
/// the pnpm-shaped surface (conservative), so a `packageManager: "deno@11.0.0"`
/// reaches the scalar router with `pnpm_incumbent = true` — but the version
/// gate only applies to a name of literally `pnpm`, so the major-11 here is
/// ignored and the scalar lands in the neutral `.npmrc`, never
/// `pnpm-workspace.yaml`. (Regression guard for the discarded-name bug.)
#[test]
fn config_set_under_unknown_pm_name_at_high_major_does_not_leak_yaml() {
    const UNKNOWN_PM_HIGH_MAJOR: &str =
        r#"{"name":"pmfam-fixture","version":"1.2.3","packageManager":"deno@11.0.0"}"#;
    let ctx = Ctx::new("config-unknown-pm", UNKNOWN_PM_HIGH_MAJOR);

    let (_, stderr, code) = ctx.run(&["set", "auto-install-peers", "false"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        read(&ctx.project.join(".npmrc")).contains("auto-install-peers=false"),
        "an unknown PM name must route the scalar to the neutral .npmrc"
    );
    assert!(
        !ctx.project.join("pnpm-workspace.yaml").exists(),
        "an unknown PM name at major >=11 must NEVER leak a pnpm-workspace.yaml (brand boundary)"
    );
}

/// Pnpm 11's global `config.yaml` is still a pnpm-named, per-major source. Nub
/// reads it under a v11+ incumbent, not under pnpm 10's rc model and never in a
/// Nub-identity project.
#[cfg(unix)]
#[test]
fn global_pnpm_config_is_read_only_under_pnpm_v11_incumbency() {
    // One shared fake HOME carrying a pnpm GLOBAL config.yaml.
    let home = pm_tmpdir("global-read-home");
    let pnpm_cfg = home.join("xdg-config").join("pnpm");
    std::fs::create_dir_all(&pnpm_cfg).unwrap();
    // A global scalar pnpm resolves from config.yaml.
    std::fs::write(pnpm_cfg.join("config.yaml"), "networkConcurrency: 7\n").unwrap();

    let run = |project: &Path, args: &[&str]| -> (String, i32) {
        let out = Command::new(nub_binary())
            .args(args)
            .current_dir(project)
            .env("NUB_SELF_SHIM", "0")
            .env("HOME", &home)
            .env("XDG_DATA_HOME", home.join("xdg-data"))
            .env("XDG_CACHE_HOME", home.join("xdg-cache"))
            .env("XDG_CONFIG_HOME", home.join("xdg-config"))
            .output()
            .expect("failed to spawn nub");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            out.status.code().unwrap_or(-1),
        )
    };

    for (tag, manifest, expected) in [
        ("gread-pnpm11", PNPM11_MANIFEST, "7"),
        ("gread-pnpm10", PNPM10_MANIFEST, "undefined"),
        ("gread-nub", NUB_MANIFEST, "undefined"),
    ] {
        let project = pm_tmpdir(tag);
        std::fs::write(project.join("package.json"), manifest).unwrap();
        let (stdout, code) = run(&project, &["config", "get", "networkConcurrency"]);
        assert_eq!(code, 0, "[{tag}] get exited non-zero: {stdout}");
        assert_eq!(
            stdout.trim(),
            expected,
            "[{tag}] pnpm global config must follow incumbent identity and major"
        );
    }
}

/// GLOBAL writes (`config set --global` / `global config set`) are NEUTRAL-ONLY: Nub
/// never writes back a PM-branded global file (pnpm's `config.yaml`/`auth.ini`)
/// nor a `config.toml`. A non-shared scalar lands in the user `~/.npmrc` (the
/// neutral global home), regardless of the cwd's incumbent PM — even under a
/// pnpm incumbent, where a PROJECT write would go to `pnpm-workspace.yaml`.
#[cfg(unix)]
#[test]
fn global_set_writes_neutral_never_a_pm_branded_global_file() {
    // pnpm incumbent cwd — the case where the project path WOULD pick a
    // pnpm-branded file; the global path must not.
    let ctx = Ctx::new("global-write", PNPM_MANIFEST);

    let (_, stderr, code) = ctx.run(&["config", "set", "--global", "network-concurrency", "5"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    // Neutral home: user ~/.npmrc carries the value.
    assert!(
        read(&ctx.home.join(".npmrc")).contains("network-concurrency=5"),
        "global non-shared scalar must land in the neutral user ~/.npmrc"
    );
    // NOT a pnpm-branded global file, NOT config.toml, NOT the project.
    let pnpm_cfg = ctx.home.join("xdg-config").join("pnpm");
    assert!(
        !pnpm_cfg.join("config.yaml").exists(),
        "global write must NEVER create pnpm's global config.yaml"
    );
    assert!(
        !ctx.home.join("xdg-config/aube/config.toml").exists()
            && !ctx.home.join("xdg-config/nub/config.toml").exists(),
        "global write must never create a config.toml"
    );
    assert!(
        !ctx.project.join("pnpm-workspace.yaml").exists(),
        "a GLOBAL write must not touch the project pnpm-workspace.yaml"
    );

    // An auth/registry key at global scope → the neutral user ~/.npmrc too
    // (the engine's own user-scope writer), never a pnpm-branded global file.
    let (_, stderr, code) = ctx.run(&[
        "global",
        "config",
        "set",
        "registry",
        "https://g.example.test/",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        read(&ctx.home.join(".npmrc")).contains("registry=https://g.example.test/"),
        "global auth/registry key must land in the neutral user ~/.npmrc"
    );
    assert!(
        !pnpm_cfg.join("auth.ini").exists(),
        "global write must NEVER create pnpm's global auth.ini"
    );
}

/// The flag and prefix select the same user scope for the whole config family,
/// while unflagged commands retain the project-first view. The old scope flag
/// is absent from the parser rather than retained as an alias.
#[cfg(unix)]
#[test]
fn config_global_scope_is_consistent_across_read_list_write_and_delete() {
    let ctx = Ctx::new("global-config-family", MANIFEST);

    let (_, stderr, code) = ctx.run(&["config", "set", "scope-probe", "project"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let (_, stderr, code) = ctx.run(&["global", "config", "set", "scope-probe", "user"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let (value, stderr, code) = ctx.run(&["config", "get", "scope-probe"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value.trim(), "project");

    for args in [
        &["config", "get", "--global", "scope-probe"][..],
        &["config", "get", "-g", "scope-probe"][..],
        &["config", "--global", "get", "scope-probe"][..],
        &["global", "config", "get", "scope-probe"][..],
    ] {
        let (value, stderr, code) = ctx.run(args);
        assert_eq!(code, 0, "stderr: {stderr}");
        assert_eq!(value.trim(), "user");
    }

    let (list, stderr, code) = ctx.run(&["config", "list", "--global"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(list.contains("scope-probe=user"), "{list}");
    assert!(!list.contains("scope-probe=project"), "{list}");

    let (list, stderr, code) = ctx.run(&["config", "--global", "list", "--local"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(list.contains("scope-probe=project"), "{list}");
    assert!(!list.contains("scope-probe=user"), "{list}");

    let (list, stderr, code) = ctx.run(&["config", "--local", "list", "--global"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(list.contains("scope-probe=user"), "{list}");
    assert!(!list.contains("scope-probe=project"), "{list}");

    let (_, stderr, code) = ctx.run(&["config", "--global", "set", "parent-scope-probe", "user"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let (value, stderr, code) = ctx.run(&["config", "get", "--global", "parent-scope-probe"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value.trim(), "user");
    let (value, stderr, code) = ctx.run(&["config", "get", "--local", "parent-scope-probe"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value.trim(), "undefined");

    let (_, stderr, code) = ctx.run(&["global", "config", "delete", "scope-probe"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let (value, stderr, code) = ctx.run(&["config", "get", "--global", "scope-probe"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value.trim(), "undefined");
    let (value, stderr, code) = ctx.run(&["config", "get", "scope-probe"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value.trim(), "project");

    let (_, stderr, code) = ctx.run(&[
        "config",
        "set",
        "--location",
        "user",
        "scope-probe",
        "ignored",
    ]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("unexpected argument '--location'"),
        "{stderr}"
    );
    assert!(
        !std::fs::read_to_string(ctx.home.join(".npmrc"))
            .unwrap_or_default()
            .contains("ignored")
    );
}

/// An unset `registry` reports the effective default at the (default)
/// merged view — what install would actually use — instead of the engine's
/// `undefined` (pnpm parity; reviewer #7). A restricted location still
/// reports `undefined` (that file genuinely doesn't set it), as do other
/// unset keys (engine behavior, documented in the family module doc).
/// Unix-only: the substitution rides the stdout fd capture, which is a
/// documented no-op on Windows — there the engine's `undefined` passes
/// through (same bucket as the other fd-capture Windows residuals).
#[cfg(unix)]
#[test]
fn config_get_registry_resolves_the_default_when_unset() {
    let ctx = Ctx::new("get-reg", MANIFEST);
    let (stdout, _, code) = ctx.run(&["config", "get", "registry"]);
    assert_eq!((stdout.trim(), code), ("https://registry.npmjs.org/", 0));
    let (stdout, _, code) = ctx.run(&["config", "get", "--json", "registry"]);
    assert_eq!(
        (stdout.trim(), code),
        ("\"https://registry.npmjs.org/\"", 0)
    );
    let (stdout, _, code) = ctx.run(&["config", "get", "--local", "registry"]);
    assert_eq!((stdout.trim(), code), ("undefined", 0));

    // A configured value passes through byte-identical (no substitution).
    std::fs::write(
        ctx.project.join(".npmrc"),
        "registry=https://mirror.example.test/\n",
    )
    .unwrap();
    let (stdout, _, code) = ctx.run(&["config", "get", "registry"]);
    assert_eq!((stdout.trim(), code), ("https://mirror.example.test/", 0));
}

/// `pkg` and `set-script` are native package.json editors — fully offline.
/// Round-trip a set → get and a set-script, asserting the manifest is
/// edited in place (and key order preserved).
#[test]
fn pkg_and_set_script_edit_the_manifest_in_place() {
    let ctx = Ctx::new("pkg", MANIFEST);

    // get an existing field.
    let (stdout, _, code) = ctx.run(&["pkg", "get", "name"]);
    assert_eq!((stdout.trim(), code), ("pmfam-fixture", 0));

    // set a nested field, then read it back.
    let (_, _, code) = ctx.run(&["pkg", "set", "scripts.test=vitest"]);
    assert_eq!(code, 0);
    let (stdout, _, code) = ctx.run(&["pkg", "get", "scripts.test"]);
    assert_eq!((stdout.trim(), code), ("vitest", 0));

    // set-script is the scripts-map sugar.
    let (_, _, code) = ctx.run(&["set-script", "build", "tsc", "-p", "."]);
    assert_eq!(code, 0);
    let (stdout, _, code) = ctx.run(&["pkg", "get", "scripts.build"]);
    assert_eq!((stdout.trim(), code), ("tsc -p .", 0));

    // The original top-level fields survive the edits.
    let (stdout, _, code) = ctx.run(&["pkg", "get", "version"]);
    assert_eq!((stdout.trim(), code), ("1.2.3", 0));
}

/// `whoami` / `search` / `owner` / `token` are native registry verbs now,
/// not npm-only fallbacks — they must NOT emit the old npm-only diagnostic
/// or point the user at npm. (They still fail offline / unauthenticated,
/// but with their own engine error, not `ERR_NUB_NPM_ONLY_COMMAND`.)
#[test]
fn registry_verbs_are_native_not_npm_only_fallbacks() {
    let ctx = Ctx::new("registry-verbs", MANIFEST);
    for args in [
        vec!["whoami"],
        vec!["search", "lodash"],
        vec!["owner", "ls", "lodash"],
        vec!["token", "list"],
    ] {
        let (_stdout, stderr, _code) = ctx.run(&args);
        assert!(
            !stderr.contains("ERR_NUB_NPM_ONLY_COMMAND"),
            "{args:?}: must not surface the npm-only diagnostic: {stderr}"
        );
        assert!(
            !stderr.contains("npm-only command") && !stderr.contains("run it with `npm"),
            "{args:?}: must not steer the user to npm: {stderr}"
        );
    }
}

/// A `nub.jsonc` field round-trips through `set` and `get`, lands typed (not as
/// the shell's string), and stays out of `.npmrc`. The reverse direction is the
/// other half of the boundary: a setting key that is not a field must not
/// materialize a `nub.jsonc`.
#[test]
fn nub_jsonc_fields_round_trip_and_stay_out_of_npmrc() {
    let ctx = Ctx::new("field-roundtrip", MANIFEST);
    let file = ctx.project.join("nub.jsonc");

    for (key, value) in [
        ("nodeCompat", "true"),
        ("preload", r#"["./a.ts","./b.ts"]"#),
        (
            "runtime.nodeOptions",
            r#"["--enable-source-maps","--trace-warnings"]"#,
        ),
        ("install.linker", "hoisted"),
        ("install.minimumReleaseAge", "3d"),
    ] {
        let (_, stderr, code) = ctx.run(&["config", "set", key, "--", value]);
        assert_eq!(code, 0, "set {key}: {stderr}");
        let (stdout, stderr, code) = ctx.run(&["config", "get", key]);
        assert_eq!((stdout.trim(), code), (value, 0), "get {key}: {stderr}");
    }

    let written: serde_json::Value = serde_json::from_str(&read(&file)).unwrap();
    assert_eq!(written["nodeCompat"], serde_json::json!(true), "{written}");
    assert_eq!(
        written["preload"],
        serde_json::json!(["./a.ts", "./b.ts"]),
        "{written}"
    );
    assert_eq!(
        written["nodeOptions"],
        serde_json::json!(["--enable-source-maps", "--trace-warnings"]),
        "{written}"
    );
    assert_eq!(read(&ctx.project.join(".npmrc")), "", "{written}");

    // A key with no field row keeps its `.npmrc` routing untouched.
    let (_, stderr, code) = ctx.run(&["config", "set", "auto-install-peers", "false"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(read(&ctx.project.join(".npmrc")).contains("auto-install-peers=false"));
    assert!(
        !read(&file).contains("auto-install-peers"),
        "an .npmrc key must not leak into nub.jsonc: {}",
        read(&file)
    );
}

/// `nodeOptions` is a pnpm-compatible engine key, so the bare spelling must
/// stay on the `.npmrc` route. Nub's runtime array uses the explicitly
/// namespaced address, while retaining `nodeOptions` as its JSON path.
#[test]
fn node_options_keeps_the_engine_route_and_runtime_alias_is_distinct() {
    let ctx = Ctx::new("node-options-routes", MANIFEST);
    let npmrc = ctx.project.join(".npmrc");
    let nub_jsonc = ctx.project.join("nub.jsonc");
    std::fs::write(&npmrc, "nodeOptions=--trace-warnings\n").unwrap();

    let (stdout, stderr, code) = ctx.run(&["config", "get", "nodeOptions"]);
    assert_eq!((stdout.trim(), code), ("--trace-warnings", 0), "{stderr}");

    let (_, stderr, code) = ctx.run(&[
        "config",
        "set",
        "--local",
        "nodeOptions",
        "--",
        "--no-warnings",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        read(&npmrc).contains("nodeOptions=--no-warnings"),
        "the bare engine key must still update .npmrc: {}",
        read(&npmrc)
    );
    assert!(
        !nub_jsonc.exists(),
        "bare nodeOptions must not create nub.jsonc"
    );

    let (_, stderr, code) = ctx.run(&["config", "delete", "--local", "nodeOptions"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !read(&npmrc).contains("nodeOptions="),
        "engine delete must clear the .npmrc key: {}",
        read(&npmrc)
    );

    let (_, stderr, code) = ctx.run(&[
        "config",
        "set",
        "runtime.nodeOptions",
        r#"["--enable-source-maps","--trace-warnings"]"#,
    ]);
    assert_eq!(code, 0, "{stderr}");
    let written: serde_json::Value = serde_json::from_str(&read(&nub_jsonc)).unwrap();
    assert_eq!(
        written["nodeOptions"],
        serde_json::json!(["--enable-source-maps", "--trace-warnings"]),
        "{written}"
    );
    assert!(
        !read(&npmrc).contains("runtime.nodeOptions"),
        "the runtime address must never enter .npmrc"
    );
}

/// The whole reason writes go through a CST edit: `nub.jsonc` is JSON *with
/// comments*, and a `set` that touches one key must leave a hand-authored file
/// intact everywhere else — comments, blank lines, trailing commas, and key
/// order included.
#[test]
fn set_preserves_comments_in_a_hand_authored_project_file() {
    let ctx = Ctx::new("field-comments", MANIFEST);
    let file = ctx.project.join("nub.jsonc");
    let before = "{\n  \
        // relative paths or bare specifiers, loaded in order\n  \
        \"preload\": [\"./setup.ts\"],\n\n  \
        /* the whole V8 flag set is available here */\n  \
        \"v8Flags\": [\"--expose-gc\"],\n  \
        \"install\": {\n    \
            // \"global-virtual-store\" (default) | \"isolated\" | \"hoisted\"\n    \
            \"linker\": \"isolated\",\n  \
        },\n\
        }\n";
    std::fs::write(&file, before).unwrap();

    let (_, stderr, code) = ctx.run(&["config", "set", "install.minimumReleaseAge", "3d"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let after = read(&file);
    for fragment in [
        "// relative paths or bare specifiers, loaded in order",
        "/* the whole V8 flag set is available here */",
        "// \"global-virtual-store\" (default) | \"isolated\" | \"hoisted\"",
        "\"preload\": [\"./setup.ts\"],",
        "\"v8Flags\": [\"--expose-gc\"],",
        "\"linker\": \"isolated\",",
    ] {
        assert!(after.contains(fragment), "lost {fragment:?}:\n{after}");
    }
    assert!(after.contains("\"minimumReleaseAge\": \"3d\""), "{after}");
}

/// An invalid value is refused with the FILE PARSER's own message, and nothing
/// is written — the writer must never accept a value that would make the next
/// run fail on a file the user did not hand-edit.
#[test]
fn an_invalid_field_value_is_refused_before_anything_is_written() {
    let ctx = Ctx::new("field-invalid", MANIFEST);
    let file = ctx.project.join("nub.jsonc");

    for (key, value, expected) in [
        ("install.minimumReleaseAge", "3", "invalid duration `3`"),
        ("nodeCompat", "yes", "must be a boolean"),
        ("install.linker", "flat", "unknown strategy `flat`"),
    ] {
        let (_, stderr, code) = ctx.run(&["config", "set", key, value]);
        assert_eq!(code, 1, "set {key}={value} must fail: {stderr}");
        assert!(stderr.contains(expected), "set {key}={value}: {stderr}");
    }
    assert!(!file.exists(), "a refused set must not create the file");
}

/// The `dlx` section configures the machine, not a checkout, so it is written to
/// the GLOBAL file with no scope flag and refused at an explicit project scope.
#[test]
fn dlx_consent_targets_the_global_file() {
    let ctx = Ctx::new("field-dlx", MANIFEST);
    let global = ctx.home.join("xdg-config/nub/nub.jsonc");

    let (_, stderr, code) = ctx.run(&["config", "set", "dlx.consent", "never"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let body = read(&global);
    assert!(body.contains("\"consent\": \"never\""), "{body}");
    assert!(
        !ctx.project.join("nub.jsonc").exists(),
        "a global-only field must not create a project file"
    );

    let (stdout, _, code) = ctx.run(&["config", "get", "dlx.consent"]);
    assert_eq!((stdout.trim(), code), ("never", 0));

    let (_, stderr, code) = ctx.run(&["config", "set", "--local", "dlx.consent", "prompt"]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("configured globally"), "{stderr}");
}

/// Help must advertise exactly the surface that runs: `init` and `path` are
/// nub's own subcommands, while `explain`/`find`/`tui` are listed by the engine
/// and error on use.
#[test]
fn config_help_lists_the_wired_subcommands_only() {
    let ctx = Ctx::new("config-help", MANIFEST);
    let (stdout, stderr, code) = ctx.run(&["config", "--help"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let commands = stdout
        .split("Flags:")
        .next()
        .expect("help starts with the command list");
    for wired in ["get", "set", "delete", "list", "init", "path"] {
        assert!(
            commands.contains(wired),
            "{wired} must be listed:\n{stdout}"
        );
    }
    for unwired in ["explain", "find", "tui"] {
        assert!(
            !commands.contains(unwired),
            "{unwired} is refused and must not be listed:\n{stdout}"
        );
    }

    let (path_help, stderr, code) = ctx.run(&["config", "path", "--help"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(path_help.contains("Usage: nub config path"), "{path_help}");

    let (path_short, stderr, code) = ctx.run(&["config", "path", "-g"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(path_short.contains("nub.jsonc"), "{path_short}");

    let (init_help, stderr, code) = ctx.run(&["config", "init", "--help"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(init_help.contains("Usage: nub config init"), "{init_help}");
    assert!(init_help.contains("--global"), "{init_help}");
    assert!(!init_help.contains("--location"), "{init_help}");

    let (set_help, stderr, code) = ctx.run(&["config", "set", "--help"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(set_help.contains("--global"), "{set_help}");
    assert!(!set_help.contains("--location"), "{set_help}");

    let (global_help, stderr, code) = ctx.run(&["global", "--help"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(global_help.contains("nub global config"), "{global_help}");

    // `path` is claimed ahead of the parse, so the interception must also
    // survive a bare `config` — no subcommand, no args at all.
    let (_, stderr, code) = ctx.run(&["config"]);
    assert!(!stderr.contains("panicked"), "a bare `config`: {stderr}");
    assert_eq!(code, 0, "{stderr}");
}

/// `stage` is not a real npm/pnpm command and is no longer in the verb
/// table — it falls through to the generic unknown-command path, never the
/// npm-only diagnostic.
#[test]
fn stage_is_an_unknown_command_not_an_npm_only_fallback() {
    let ctx = Ctx::new("stage", MANIFEST);
    let (_stdout, stderr, code) = ctx.run(&["stage"]);
    assert_ne!(code, 0);
    assert!(
        !stderr.contains("ERR_NUB_NPM_ONLY_COMMAND"),
        "stage must not be an npm-only fallback: {stderr}"
    );
}
