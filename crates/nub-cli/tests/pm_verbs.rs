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

/// A real published peer pair — `ajv-keywords@3.5.2` peers on `ajv@6.12.6` —
/// as npm writes it. Real names rather than synthetic ones because `import`
/// re-resolves against the registry, which is pnpm 12.4.1's own behaviour on
/// the same fixture (both fail identically against a dead registry), so an
/// invented package name cannot survive the pass it is meant to exercise.
const AJV_PEER_PACKAGE_LOCK: &str = r#"{
  "name": "fixture",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "fixture",
      "version": "1.0.0",
      "dependencies": { "ajv": "6.12.6", "ajv-keywords": "3.5.2" }
    },
    "node_modules/ajv": {
      "version": "6.12.6",
      "resolved": "https://registry.npmjs.org/ajv/-/ajv-6.12.6.tgz",
      "integrity": "sha512-j3fVLgvTo527anyYyJOGTYJbG+vnnQYvE0m5mmkc1TK+nxAppkCLMIL0aZ4dblVCNoGShhm+kzE4ZUykBoMg4g==",
      "dependencies": {
        "fast-deep-equal": "^3.1.1",
        "fast-json-stable-stringify": "^2.0.0",
        "json-schema-traverse": "^0.4.1",
        "uri-js": "^4.2.2"
      }
    },
    "node_modules/ajv-keywords": {
      "version": "3.5.2",
      "resolved": "https://registry.npmjs.org/ajv-keywords/-/ajv-keywords-3.5.2.tgz",
      "integrity": "sha512-5p6WTN0DdTGVQk6VjcEju19IgaHudalcfabD7yhDGeA6bcQnmL+CpveLJq/3hvfwd1aof6L386Ougkx6RfyMIQ==",
      "peerDependencies": { "ajv": "^6.9.1" }
    },
    "node_modules/fast-deep-equal": {
      "version": "3.1.3",
      "resolved": "https://registry.npmjs.org/fast-deep-equal/-/fast-deep-equal-3.1.3.tgz",
      "integrity": "sha512-f3qQ9oQy9j2AhBe/H9VC91wLmKBCCU/gDOnKNAYG5hswO7BLKj09Hc5HYNz9cGI++xlpDCIgDaitVs03ATR84Q=="
    },
    "node_modules/fast-json-stable-stringify": {
      "version": "2.1.0",
      "resolved": "https://registry.npmjs.org/fast-json-stable-stringify/-/fast-json-stable-stringify-2.1.0.tgz",
      "integrity": "sha512-lhd/wF+Lk98HZoTCtlVraHtfh5XYijIjalXck7saUtuanSDyLMxnHhSXEDJqHxD7msR8D0uCmqlkwjCV8xvwHw=="
    },
    "node_modules/json-schema-traverse": {
      "version": "0.4.1",
      "resolved": "https://registry.npmjs.org/json-schema-traverse/-/json-schema-traverse-0.4.1.tgz",
      "integrity": "sha512-xbbCH5dCYU5T8LcEhhuh7HJ88HXuW3qsI3Y0zOZFKfZEHcpWiHU/Jxzk629Brsab/mMiHQti9wMP+845RPe3Vg=="
    },
    "node_modules/punycode": {
      "version": "2.3.1",
      "resolved": "https://registry.npmjs.org/punycode/-/punycode-2.3.1.tgz",
      "integrity": "sha512-vYt7UD1U9Wg6138shLtLOvdAu+8DsC/ilFtEVHcH+wydcSpNE20AfSOduf6MkRFahL5FY7X1oU7nKVZFtfq8Fg=="
    },
    "node_modules/uri-js": {
      "version": "4.4.1",
      "resolved": "https://registry.npmjs.org/uri-js/-/uri-js-4.4.1.tgz",
      "integrity": "sha512-7rKUyy33Q1yc98pQ1DAmLtwX109F7TIfWlW1Ydo8Wl1ii1SeHieeh0HHfPeL2fMXK6z0s8ecKs9frCuLJvndBg==",
      "dependencies": { "punycode": "^2.1.0" }
    }
  }
}"#;

/// `nub add` then `nub rm` (alias) round-trip on a truly-fresh project: add
/// persists the dep + writes nub's neutral `nub.lock` + links node_modules;
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
        dir.join("nub.lock").is_file()
            && !dir.join("pnpm-lock.yaml").exists()
            && !dir.join("aube-lock.yaml").exists(),
        "add on a truly-fresh project writes nub's neutral nub.lock"
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
    // Fresh project with no incumbent lockfile → nub writes its neutral
    // nub.lock; a project already on pnpm-lock.yaml keeps that.
    let lock = std::fs::read_to_string(dir.join("nub.lock"))
        .or_else(|_| std::fs::read_to_string(dir.join("pnpm-lock.yaml")))
        .expect("update must write a lockfile");
    assert!(
        lock.contains("3.1.0") && !lock.contains("is-positive@3.0.0"),
        "the lockfile must resolve the updated version: {lock}"
    );
}

/// `nub up <pkg>@<version>` pins the named dep to that version, preserving
/// the manifest's existing range operator (`^3.0.0` + `is-positive@3.1.0` ->
/// `^3.1.0`) — matching `pnpm update <pkg>@<version>`. The pre-fix engine
/// rejected any non-`latest` spec here.
#[test]
#[ignore = "network: resolves is-positive from the npm registry"]
fn update_pins_named_version_preserving_manifest_operator() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("updatepin");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"updatepin","version":"1.0.0","dependencies":{"is-positive":"^3.0.0"}}"#,
    )
    .unwrap();

    let up = run_nub(&dir, &["up", "is-positive@3.1.0"]);
    assert_eq!(up.code, 0, "stdout: {}\nstderr: {}", up.stdout, up.stderr);
    up.assert_brand_clean();
    let manifest = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(
        manifest.contains("\"is-positive\": \"^3.1.0\""),
        "the caret operator must be preserved on the pinned version: {manifest}"
    );
    // Fresh project with no incumbent lockfile → nub writes its neutral
    // nub.lock; a project already on pnpm-lock.yaml keeps that.
    let lock = std::fs::read_to_string(dir.join("nub.lock"))
        .or_else(|_| std::fs::read_to_string(dir.join("pnpm-lock.yaml")))
        .expect("update must write a lockfile");
    assert!(
        lock.contains("3.1.0") && !lock.contains("is-positive@3.0.0"),
        "the lockfile must resolve the pinned version: {lock}"
    );
}

/// `nub up <pkg>@<protocol-spec>` (`file:`, git, a tarball URL, a
/// non-`latest` dist-tag) fails with a non-zero exit and the manifest left
/// untouched — pinning one of those into a semver slot would silently
/// corrupt package.json. The fixture points at a dead registry, so nothing
/// here depends on the network either way.
///
/// Two specifiers a stricter draft of this test also refused are gone from
/// the list, because pnpm 12.4.1 accepts both on the same fixture and
/// rewrites the manifest to them: an `npm:` alias and a `link:`. Neither
/// corrupts anything — both are the portable spelling every package manager
/// reads — so refusing them would make `up` reject syntax a project is
/// entitled to write, which is the opposite of the protection this is for.
#[test]
fn update_rejects_non_semver_specs_without_touching_the_manifest() {
    let dir = pm_tmpdir("updatereject");
    std::fs::write(
        dir.join(".npmrc"),
        "registry=http://127.0.0.1:1/\nfetch-retries=0\n",
    )
    .unwrap();
    let manifest_src =
        r#"{"name":"updatereject","version":"1.0.0","dependencies":{"is-odd":"^3.0.0"}}"#;
    for spec in [
        "is-odd@file:./bar",
        "is-odd@github:a/b",
        "is-odd@https://example.com/is-odd.tgz",
        "is-odd@next",
    ] {
        std::fs::write(dir.join("package.json"), manifest_src).unwrap();
        let out = run_nub(&dir, &["up", spec]);
        assert_ne!(out.code, 0, "`{spec}` must be rejected: {}", out.combined());
        out.assert_brand_clean();
        let after = std::fs::read_to_string(dir.join("package.json")).unwrap();
        assert_eq!(
            after, manifest_src,
            "a rejected `{spec}` must leave package.json byte-identical"
        );
    }
}

/// `nub add <pkg>@<version> --lockfile-only` resolves and writes the
/// lockfile + manifest but never links `node_modules` — the grammar the
/// pre-fix `add` rejected outright (`unexpected argument '--lockfile-only'`).
#[test]
#[ignore = "network: resolves is-positive from the npm registry"]
fn add_lockfile_only_writes_lockfile_without_linking_node_modules() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("addlockonly");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"addlockonly","version":"1.0.0"}"#,
    )
    .unwrap();

    let add = run_nub(&dir, &["add", "is-positive@3.1.0", "--lockfile-only"]);
    assert_eq!(
        add.code, 0,
        "stdout: {}\nstderr: {}",
        add.stdout, add.stderr
    );
    add.assert_brand_clean();
    let manifest = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(
        manifest.contains("is-positive"),
        "add must still persist the dependency to package.json: {manifest}"
    );
    assert!(
        dir.join("nub.lock").exists() || dir.join("pnpm-lock.yaml").exists(),
        "the lockfile must be written"
    );
    assert!(
        !dir.join("node_modules/is-positive").exists(),
        "--lockfile-only must skip linking node_modules"
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

/// dlx/create children carry the role-aware `npm_config_user_agent` (pnpm
/// parity) — create-* scaffolders sniff it to emit the invoking PM's commands
/// and fall back to npm-mode when it's absent.
#[test]
#[ignore = "network: installs uuid into a dlx scratch project"]
fn dlx_child_sees_nub_user_agent() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("dlxua");
    let out = run_nub(
        &dir,
        &[
            "dlx",
            "--shell-mode",
            "-p",
            "uuid",
            "node -p process.env.npm_config_user_agent",
        ],
    );
    assert_eq!(
        out.code, 0,
        "stdout: {}\nstderr: {}",
        out.stdout, out.stderr
    );
    assert!(
        out.stdout.contains("nub/"),
        "dlx child must see a nub-first user agent, got: {}",
        out.stdout
    );
}

/// Workspace `link:` deps must not surface in dedupe's diff (#494): the
/// lockfile parser synthesizes `<name>@link+<hash>` package entries that a
/// fresh resolve never produces, so dedupe reported every workspace link as
/// "removed" on each run — while the lockfile stayed byte-identical (the
/// writer never serializes links) and `--check` failed forever. Offline by
/// construction: the fixture's only deps are workspace members.
#[test]
fn dedupe_ignores_workspace_links_and_check_passes() {
    let dir = pm_tmpdir("dedupe-ws");
    // Declared the neutral way, so the project is nub's own. A
    // `pnpm-workspace.yaml` here would make pnpm the incumbent, and a
    // pnpm-incumbent project is answered in pnpm's name and pnpm's wording —
    // which is exactly right, and leaves this test asserting nothing about
    // nub. Both are verified against pnpm 12.4.1: the wording below is what
    // it prints, and under that file nub reproduces its whole line.
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"fixture","private":true,"workspaces":["packages/*"],"devDependencies":{"@repro/a":"workspace:*"}}"#,
    )
    .unwrap();
    for (rel, manifest) in [
        (
            "packages/a",
            r#"{"name":"@repro/a","version":"1.0.0","dependencies":{"@repro/b":"workspace:*"}}"#,
        ),
        ("packages/b", r#"{"name":"@repro/b","version":"1.0.0"}"#),
    ] {
        let pkg = dir.join(rel);
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), manifest).unwrap();
    }

    let (xdg_data, xdg_cache) = (pm_tmpdir("dedupe-ws-data"), pm_tmpdir("dedupe-ws-cache"));
    let install = run_nub_with(&dir, &["install"], &xdg_data, &xdg_cache);
    assert_eq!(
        install.code, 0,
        "stdout: {}\nstderr: {}",
        install.stdout, install.stderr
    );
    let lock_before = std::fs::read_to_string(dir.join("nub.lock")).unwrap();

    let dedupe = run_nub_with(&dir, &["dedupe"], &xdg_data, &xdg_cache);
    assert_eq!(
        dedupe.code, 0,
        "stdout: {}\nstderr: {}",
        dedupe.stdout, dedupe.stderr
    );
    // Two spellings because two package managers are in the tree and they
    // word the same verdict differently — `Already up to date` against
    // `Lockfile is already deduped (0 packages)`. Both say the lockfile was
    // left alone, which is the claim; the two assertions below pin it
    // independently of any wording, by byte-comparing the lockfile and by
    // making `--check` agree. The list collapses to one entry when the
    // second package manager leaves.
    assert!(
        ["Already up to date", "already deduped"]
            .iter()
            .any(|settled| dedupe.combined().contains(settled)),
        "workspace links must not be reported as dedupe changes: {}",
        dedupe.combined()
    );
    dedupe.assert_brand_clean();
    assert_eq!(
        std::fs::read_to_string(dir.join("nub.lock")).unwrap(),
        lock_before,
        "dedupe must not change the lockfile"
    );

    let check = run_nub_with(&dir, &["dedupe", "--check"], &xdg_data, &xdg_cache);
    assert_eq!(
        check.code,
        0,
        "dedupe --check must pass on a deduped workspace: {}",
        check.combined()
    );
}

/// An `npm:` alias must not read as a dedupe change (#578). The pnpm
/// reader synthesizes an alias-keyed package and used to keep the
/// canonical real-name entry too, while a fresh resolve emits only the
/// clone — so `dedupe` reported the target as removed on every run and
/// `--check` failed forever on a byte-identical lockfile. Needs the
/// registry: dedupe's whole job is a fresh resolve.
#[test]
#[ignore = "network: resolves + fetches is-number@7.0.0 from the npm registry"]
fn dedupe_ignores_npm_alias_targets_and_check_passes() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("dedupe-alias");
    // `packageManager` selects pnpm-lock.yaml; the bug is specific to that
    // format, since only its writer re-keys an alias under the real name.
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"fixture","private":true,"packageManager":"pnpm@11.17.0","dependencies":{"number-alias":"npm:is-number@7.0.0"}}"#,
    )
    .unwrap();
    let (data, cache) = (
        pm_tmpdir("dedupe-alias-data"),
        pm_tmpdir("dedupe-alias-cache"),
    );

    let install = run_nub_with(&dir, &["install"], &data, &cache);
    assert_eq!(
        install.code, 0,
        "stdout: {}\nstderr: {}",
        install.stdout, install.stderr
    );
    let lock_before = std::fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap();

    let dedupe = run_nub_with(&dir, &["dedupe"], &data, &cache);
    assert_eq!(
        dedupe.code, 0,
        "stdout: {}\nstderr: {}",
        dedupe.stdout, dedupe.stderr
    );
    assert!(
        dedupe.combined().contains("already deduped"),
        "an alias target must not be reported as removed: {}",
        dedupe.combined()
    );
    dedupe.assert_brand_clean();
    assert_eq!(
        std::fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap(),
        lock_before,
        "dedupe must not change the lockfile"
    );

    let check = run_nub_with(&dir, &["dedupe", "--check"], &data, &cache);
    assert_eq!(
        check.code,
        0,
        "dedupe --check must pass on an aliased project: {}",
        check.combined()
    );
}

/// The lockfile the project's own package manager writes, whichever of the
/// two names it goes by, with the path it was found at.
///
/// Two package managers are in this tree and they disagree on the name — one
/// writes `pnpm-lock.yaml`, the other `nub.lock` under nub's identity, which
/// is the whole point of the filename toggle. A test that names one of them
/// asserts which package manager is serving rather than what it produced.
#[track_caller]
fn canonical_lockfile(dir: &Path) -> (PathBuf, String) {
    for name in ["nub.lock", "pnpm-lock.yaml"] {
        let path = dir.join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            return (path, text);
        }
    }
    panic!("no lockfile was written in {}", dir.display());
}

/// `nub import` converts a foreign lockfile to the project's own, leaves the
/// source in place, and never writes an engine-named file.
///
/// Two claims this used to make are gone because they were the old engine's
/// alone, measured against pnpm 12.4.1 on the same fixture. It reported
/// `Imported 1 packages from package-lock.json to pnpm-lock.yaml`, where pnpm
/// prints only its `Done in …` footer — so the wording is not a contract, and
/// the conversion is asserted on the lockfile's contents instead. And a second
/// `import` was refused pending `--force`, where pnpm exits 0 and overwrites;
/// pnpm has no `--force` on this verb at all, so the guard is not something to
/// port.
#[test]
fn import_converts_package_lock_to_the_projects_own_lockfile() {
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
    let (path, lock) = canonical_lockfile(&dir);
    assert!(
        lock.contains("is-positive"),
        "converted lockfile {} must carry the dependency: {lock}",
        path.display()
    );
    assert!(
        !dir.join("aube-lock.yaml").exists() && !dir.join("pnpm-lock.yaml.import-backup").exists(),
        "no foreign lockfile or leftover backup may appear"
    );
    assert!(
        dir.join("package-lock.json").is_file(),
        "the source lockfile is left in place (parity with pnpm import)"
    );
}

/// Regression for #453: importing a suffix-less source (npm) must run the
/// peer-context pass so the written lockfile carries peer suffixes. The
/// install path skips that pass for pnpm incumbents, assuming a pnpm-lock
/// already has them; a bare `ajv-keywords@3.5.2` would leave the
/// store-resident plugin with no peer sibling under the isolated layout.
///
/// The bun.lock counterpart this used to sit beside is retired rather than
/// ported. `import` does not read a `bun.lock` — refused
/// `ERR_NUB_LOCKFILE_NOT_FOUND`, whose help names yarn, package-lock and
/// shrinkwrap, and pnpm 12.4.1 refuses the identical fixture with the same
/// sentence — so the assertion was aimed at a door that is closed on both
/// sides. Converting a `bun.lock` is `nub pm migrate`, which does read one.
#[test]
fn import_writes_peer_suffixes_for_suffixless_source() {
    let dir = pm_tmpdir("import-peer");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"fixture","version":"1.0.0","dependencies":{"ajv":"6.12.6","ajv-keywords":"3.5.2"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("package-lock.json"), AJV_PEER_PACKAGE_LOCK).unwrap();

    let out = run_nub(&dir, &["import"]);
    assert_eq!(
        out.code, 0,
        "stdout: {}\nstderr: {}",
        out.stdout, out.stderr
    );
    let (path, lock) = canonical_lockfile(&dir);
    assert!(
        lock.contains("ajv-keywords@3.5.2(ajv@6.12.6)"),
        "imported lockfile {} must peer-suffix the plugin key (#453): {lock}",
        path.display()
    );
    // Bound the check to ajv-keywords' OWN snapshot block: split on its
    // suffixed key (unique to the snapshots section — the packages key stays
    // bare), then cut at the blank line that separates snapshot entries. The
    // whole-file remainder would let a peer edge that landed on the wrong
    // snapshot still pass.
    let snapshot = lock
        .split("ajv-keywords@3.5.2(ajv@6.12.6):")
        .nth(1)
        .unwrap_or("")
        .split("\n\n")
        .next()
        .unwrap_or("");
    assert!(
        snapshot.contains("ajv: 6.12.6"),
        "resolved peer must be mirrored into ajv-keywords' snapshot deps: {snapshot}"
    );
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

/// `init` is nub's own project scaffolder (src/init.rs), never an engine verb
/// or a PM redirect. In a dir that already has a manifest it refuses with
/// nub's own conflict message — no engine routing, no "pnpm init" spelling.
/// The scaffold contract itself lives in tests/init_cmd.rs.
#[test]
fn init_is_nub_own_and_never_redirects_to_a_pm() {
    let dir = pm_tmpdir("init");
    std::fs::write(dir.join("package.json"), r#"{"name":"init-fixture"}"#).unwrap();
    let out = run_nub(&dir, &["init", "-y", "--no-install"]);
    assert_ne!(out.code, 0, "init must refuse over an existing manifest");
    out.assert_brand_clean();
    assert!(
        out.stderr.contains("refusing to overwrite"),
        "the refusal is nub's own conflict message: {}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("package manager") && !out.stderr.contains("pnpm init"),
        "init must not redirect to a PM: {}",
        out.stderr
    );
}

/// The verbs that answer for themselves rather than with stub text.
///
/// Four of the five this used to list are no longer excluded from anything:
/// `clean` and `purge` run and exit 0 silently, byte-for-byte what pnpm
/// 12.4.1 does on the same fixture, and `deploy` and `sbom` refuse with
/// reasons of their own — a deploy that has no workspace to deploy from, and
/// a required argument clap names. Asserting "not supported" for any of them
/// would pin a status that is no longer true.
///
/// What survives is the claim the name makes: an answer is the verb's own,
/// never the generic placeholder, and never another package manager's brand.
/// `recursive` is the one that still refuses outright, and it keeps the
/// wording because nub's `-r` really does belong to each verb.
#[test]
fn excluded_verbs_answer_honestly_not_with_stub_text() {
    let dir = pm_tmpdir("excluded");

    let recursive = run_nub(&dir, &["recursive"]);
    assert_ne!(recursive.code, 0, "recursive must error");
    recursive.assert_brand_clean();
    assert!(
        recursive.stderr.contains("verb's own workspace flags"),
        "recursive must explain its status: {}",
        recursive.stderr
    );

    // Each refuses over something it names itself. Read as a pair — the exit
    // code alone would also be satisfied by a crash, and the text alone by a
    // command that printed a reason and then succeeded anyway. The reason
    // differs by package manager (one has no deploy at all; the other has one
    // and there is no workspace to deploy from), so the assertion is on the
    // verb being named rather than on either wording.
    for verb in ["deploy", "sbom"] {
        let out = run_nub(&dir, &[verb]);
        assert_ne!(out.code, 0, "{verb} must error");
        assert!(
            out.combined().to_lowercase().contains(verb),
            "{verb} must name what it refused over: {}",
            out.combined()
        );
    }

    // `clean` and `purge` carry no exit-code claim here, and that is the
    // honest reading rather than a gap. The two package managers in this tree
    // answer them differently on purpose: one refuses to delete node_modules
    // for you, the other runs and exits 0 silently because a project this bare
    // has nothing to remove — measured byte-for-byte against pnpm 12.4.1,
    // which is A1.2. Asserting either one reddens the other arm. The claim
    // lands as an equality against real pnpm once the second package manager
    // leaves the tree; until then these two are carried by the brand and
    // stub-text sweep below, which holds whoever is serving.

    // The claim the name carries, over every one of them.
    for verb in ["recursive", "clean", "purge", "deploy", "sbom"] {
        let out = run_nub(&dir, &[verb]);
        out.assert_brand_clean();
        assert!(
            !out.combined().contains("wired in phase Surface"),
            "{verb} must not use the generic stub text: {}",
            out.combined()
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

    // Surface 1 — the `migrate` PM-verb redirect. There is no top-level `nub
    // migrate`: nub keeps the one-shot lockfile migration in its package-
    // manager namespace, so the nub-identity redirect must name the *real*
    // `nub pm migrate`, never a phantom top-level verb and never a blind npm.
    let migrate = run_nub(&nub_dir, &["migrate", "yarn.lock"]);
    assert_ne!(migrate.code, 0, "migrate is not a nub command");
    migrate.assert_brand_clean();
    assert!(
        migrate.stderr.contains("nub pm migrate yarn.lock"),
        "migrate redirect must suggest the real `nub pm migrate`, not npm: {}",
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
///
/// `prefix` is excluded for the opposite reason: it is a real command that
/// prints the project directory and exits 0, measured identical to pnpm
/// 12.4.1's own. A list of things that are "not a command" cannot hold it.
///
/// The brand assertion reads case-insensitively and then names pnpm as
/// forbidden outright. The refusals carry the brand only as the uppercase
/// `ERR_NUB_*` code, so a case-sensitive search for `nub` found nothing in a
/// message that was in fact correctly branded; and three of these commands
/// used to answer a nub user with "not yet implemented in pnpm", which is the
/// same leak in the other direction and is what the loosened check would have
/// let through. This fixture is nub-identity, so pnpm has no business in any
/// of its output.
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
            lower.contains("nub"),
            "the refusal must be nub-branded:\nstdout: {}\nstderr: {}",
            out.stdout,
            out.stderr
        );
        assert!(
            !lower.contains("pnpm"),
            "`nub {cmd}` must not name the engine to a nub project:\nstdout: {}\nstderr: {}",
            out.stdout,
            out.stderr
        );
    }
}
