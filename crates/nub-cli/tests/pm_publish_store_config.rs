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
        let out = Command::new(nub_binary())
            .args(args)
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", self.home.join("xdg-data"))
            .env("XDG_CACHE_HOME", self.home.join("xdg-cache"))
            .env("XDG_CONFIG_HOME", self.home.join("xdg-config"))
            .output()
            .expect("failed to spawn nub");
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
    let expected = ctx.home.join("xdg-data/nub/store/v1");
    assert_eq!(
        stdout.trim(),
        expected.to_string_lossy(),
        "store path must resolve through nub's storeDir default"
    );
}

/// npmrc-first write routing: a pnpm-surface (non-npm-shared) key lands in
/// the *project* `.npmrc`; an npm-shared key (registry) delegates to the
/// engine and lands in the *user* `~/.npmrc`. No `config.toml` is ever
/// written, and `config get` reads both values back.
#[test]
fn config_set_routes_npmrc_first_and_get_reads_it_back() {
    let ctx = Ctx::new("config", MANIFEST);

    // Non-shared key → project .npmrc (top-level `set` shorthand).
    let (_, stderr, code) = ctx.run(&["set", "auto-install-peers", "false"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let project_npmrc = read(&ctx.project.join(".npmrc"));
    assert!(
        project_npmrc.contains("auto-install-peers=false"),
        "non-shared key must land in the project .npmrc: {project_npmrc:?}"
    );
    assert!(
        !read(&ctx.home.join(".npmrc")).contains("auto-install-peers"),
        "non-shared key must not touch the user .npmrc"
    );

    // npm-shared key → user ~/.npmrc via the engine's own writer.
    let (_, stderr, code) = ctx.run(&["config", "set", "registry", "https://r.example.test/"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        read(&ctx.home.join(".npmrc")).contains("registry=https://r.example.test/"),
        "npm-shared key must land in the user .npmrc"
    );

    // No config.toml anywhere (the npmrc-first rule's hard line).
    for forbidden in [ctx.home.join("xdg-config"), ctx.project.join(".config")] {
        assert!(
            !forbidden.join("aube/config.toml").exists()
                && !forbidden.join("nub/config.toml").exists(),
            "config set must never write a config.toml under {}",
            forbidden.display()
        );
    }

    // Read-back through both spellings of get.
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

/// An unset `registry` reports the effective default at the (default)
/// merged view — what install would actually use — instead of the engine's
/// `undefined` (pnpm parity; reviewer #7). A restricted location still
/// reports `undefined` (that file genuinely doesn't set it), as do other
/// unset keys (engine behavior, documented in the family module doc).
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

/// The npm-fallback verbs mirror the engine: without an `npmPath` setting
/// they fail with the rewritten npm-only diagnostic (and the engine's
/// non-zero exit), pointing the user at npm.
#[test]
fn whoami_falls_back_with_the_rewritten_npm_only_error() {
    let ctx = Ctx::new("whoami", MANIFEST);
    for verb in ["whoami", "search"] {
        let (stdout, stderr, code) = ctx.run(&[verb]);
        assert_ne!(code, 0, "{verb}: stdout: {stdout}");
        assert!(
            stderr.contains("ERR_NUB_NPM_ONLY_COMMAND"),
            "{verb}: diagnostic code must be rewritten to the nub namespace: {stderr}"
        );
        assert!(
            stderr.contains(&format!("`npm {verb}`")),
            "{verb}: the npm remedy must survive the rewrite: {stderr}"
        );
    }
}
