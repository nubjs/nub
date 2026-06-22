//! Info-family verbs (`list`/`why`/`outdated`/`audit`/`peers`, …) through
//! the embedded aube engine, end-to-end through the binary. The wiring under
//! test lives in `crates/nub-cli/src/pm_engine/info_family.rs`.
//!
//! The lockfile-reading verbs are offline-testable against a handcrafted
//! `pnpm-lock.yaml` (the engine reads the graph straight from the lockfile).
//! `outdated`/`audit` need registry data and follow the `#[ignore]` +
//! self-skip convention from `install_engine.rs` — run via
//! `cargo test -p nub-cli --test info_engine -- --ignored`.

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
        "nub-info-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Spawn `nub <args>` in `dir` with the engine store/cache isolated to fresh
/// temp roots so tests never touch the dev box's real store.
fn run_nub(dir: &Path, args: &[&str]) -> (String, String, i32) {
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

/// Offline guard for the `#[ignore]` network tests.
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

/// The output brand boundary: no engine name on either stream, ever.
/// (Temp-dir fixtures keep legitimately-preserved on-disk names — there is
/// no `aube-lock.yaml` etc. in these projects — so the blanket check is
/// exact here.)
fn assert_no_engine_branding(streams: &[(&str, &str)]) {
    for (name, s) in streams {
        assert!(
            !s.to_lowercase().contains("aube"),
            "engine branding leaked on {name}: {s}"
        );
    }
}

/// A single-dep project with a handcrafted pnpm v9 lockfile — enough for
/// every lockfile-reading query verb, no install required.
fn lockfile_fixture(
    tag: &str,
    name: &str,
    specifier: &str,
    version: &str,
    integrity: &str,
) -> PathBuf {
    let dir = pm_tmpdir(tag);
    std::fs::write(
        dir.join("package.json"),
        format!(
            r#"{{"name":"{tag}","version":"1.0.0","dependencies":{{"{name}":"{specifier}"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("pnpm-lock.yaml"),
        format!(
            "lockfileVersion: '9.0'\n\n\
             importers:\n\n\
             \x20\x20.:\n\
             \x20\x20\x20\x20dependencies:\n\
             \x20\x20\x20\x20\x20\x20{name}:\n\
             \x20\x20\x20\x20\x20\x20\x20\x20specifier: {specifier}\n\
             \x20\x20\x20\x20\x20\x20\x20\x20version: {version}\n\n\
             packages:\n\n\
             \x20\x20{name}@{version}:\n\
             \x20\x20\x20\x20resolution: {{integrity: {integrity}}}\n\n\
             snapshots:\n\n\
             \x20\x20{name}@{version}: {{}}\n"
        ),
    )
    .unwrap();
    dir
}

const IS_POSITIVE_310: &str = "sha512-8ND1j3y9/HP94TOvGzr69/FgbkX2ruOldhLEsTWwcJVfo4oRjwemJmJxt7RJkKYH8tz7vYBP9JcKQY8CLuJ90Q==";
const IS_POSITIVE_300: &str = "sha512-JDkaKp5jWv24ZaFuYDKTcBrC/wBOHdjhzLDkgrrkJD/j7KqqXsGcAkex336qHoOFEajMy7bYqUgm0KH9/MzQvw==";
const LODASH_41720: &str = "sha512-PlhdFcillOINfeV7Ni6oF1TAEayyZBoZ8bcshTHqOYJYlrqzRK5hagpagky5o4HfCzzd1TRkXPMFq6cKk9rGmA==";

/// The offline read verbs against one lockfile fixture: `list` (plus the
/// `ls` alias and the `ll` long form), `why`, and `peers check` all read the
/// handcrafted graph, print the dep on stdout, exit 0, and leak no engine
/// branding on either stream.
#[test]
fn lockfile_read_verbs_work_offline_and_stay_brand_clean() {
    let dir = lockfile_fixture("reads", "is-positive", "3.1.0", "3.1.0", IS_POSITIVE_310);

    for argv in [
        &["list"][..],
        &["ls", "--json"][..],
        &["ll"][..],
        &["why", "is-positive"][..],
        &["peers", "check"][..],
    ] {
        let (stdout, stderr, code) = run_nub(&dir, argv);
        assert_eq!(code, 0, "nub {argv:?}: stdout: {stdout}\nstderr: {stderr}");
        if argv[0] != "peers" {
            assert!(
                stdout.contains("is-positive"),
                "nub {argv:?} must print the dep: {stdout}"
            );
        }
        assert_no_engine_branding(&[("stdout", &stdout), ("stderr", &stderr)]);
    }

    // `--json` is machine-readable and carries the version.
    let (stdout, _, _) = run_nub(&dir, &["list", "--json"]);
    assert!(
        stdout.contains("\"3.1.0\""),
        "list --json must carry the version: {stdout}"
    );
}

/// The no-lockfile pre-flight: the engine's own handling of this case is a
/// direct branded eprintln, so nub short-circuits it — same message shape,
/// nub spelling, exit 0 (matching the engine's exit behavior).
#[test]
fn missing_lockfile_reports_the_nub_install_hint_and_exits_zero() {
    let dir = pm_tmpdir("nolock");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"nolock","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();

    let (stdout, stderr, code) = run_nub(&dir, &["list"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("Run `nub install` to populate node_modules"),
        "list must speak the rebranded hint: {stderr}"
    );

    let (stdout, stderr, code) = run_nub(&dir, &["outdated"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("Run `nub install` first."),
        "outdated must speak the rebranded hint: {stderr}"
    );
    assert_no_engine_branding(&[("stdout", &stdout), ("stderr", &stderr)]);
}

/// The `--filter requires a workspace root (…)` error names the workspace
/// markers the engine looks for. The engine's own phrasing lists
/// `aube-workspace.yaml` — a brand leak in nub's public output (the
/// conformance sweep hit it on `nub list -r` across 5/6 fixtures). nub's
/// embedder has no branded workspace yaml, so the error must name only
/// `pnpm-workspace.yaml`, with zero `aube` on either stream. Exercised across
/// the verbs that share the workspace-root resolver (list/why).
#[test]
fn filter_workspace_root_error_is_brand_clean() {
    let dir = pm_tmpdir("filter-nonws");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    // A lockfile so the no-lockfile pre-flight doesn't short-circuit before the
    // --filter workspace-root check fires.
    std::fs::write(
        dir.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\n\nimporters:\n  .:\n    dependencies: {}\n",
    )
    .unwrap();

    for verb in [
        vec!["list", "--filter", "foo"],
        vec!["list", "-r", "--filter", "foo"],
        vec!["why", "react", "--filter", "foo"],
    ] {
        let (stdout, stderr, _code) = run_nub(&dir, &verb);
        assert!(
            stderr.contains("pnpm-workspace.yaml"),
            "{verb:?} must name pnpm-workspace.yaml as the workspace marker: {stderr}"
        );
        assert_no_engine_branding(&[("stdout", &stdout), ("stderr", &stderr)]);
    }
}

/// `--json` must ALWAYS emit parseable JSON on stdout — never empty-stdout +
/// a prose stderr note — in the never-installed (no-lockfile) state, so
/// `nub list --json | jq` / `nub outdated --json | jq` behave like pnpm's,
/// which emit the empty shape (an importer-header array for `list`, `{}` for
/// `outdated`). Regression for D2.
#[test]
fn json_queries_emit_parseable_empty_shape_without_a_lockfile() {
    let dir = pm_tmpdir("nolock-json");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"nolock-json","version":"2.3.4","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();

    // list --json: a one-element array with the project's name/version/path.
    let (stdout, stderr, code) = run_nub(&dir, &["list", "--json"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("list --json must be parseable JSON");
    assert_eq!(v[0]["name"], "nolock-json", "importer name: {stdout}");
    assert_eq!(v[0]["version"], "2.3.4", "importer version: {stdout}");
    assert!(v[0]["path"].is_string(), "importer path: {stdout}");

    // `--format json` (the long spelling) goes through the same path.
    let (stdout, _, code) = run_nub(&dir, &["list", "--format", "json"]);
    assert_eq!(code, 0);
    assert!(
        serde_json::from_str::<serde_json::Value>(stdout.trim()).is_ok(),
        "list --format json must be parseable JSON: {stdout}"
    );

    // outdated --json: the empty object.
    let (stdout, stderr, code) = run_nub(&dir, &["outdated", "--json"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(stdout.trim())
            .expect("outdated --json must be parseable JSON"),
        serde_json::json!({}),
        "outdated --json empty shape must be {{}}: {stdout}"
    );

    assert_no_engine_branding(&[("stdout", &stdout), ("stderr", &stderr)]);
}

/// The path verbs print the resolved project locations without any install,
/// `check` on a never-installed project reports zero packages and exits 0,
/// and `licenses` accepts pnpm's documented `list` spelling beside the
/// engine's `ls` (reviewer #6). All offline, all brand-clean.
#[test]
fn check_bin_root_and_licenses_list_work_offline_and_stay_brand_clean() {
    let dir = lockfile_fixture("paths", "is-positive", "3.1.0", "3.1.0", IS_POSITIVE_310);

    let (root_out, stderr, code) = run_nub(&dir, &["root"]);
    assert_eq!(code, 0, "root: stderr: {stderr}");
    assert!(
        Path::new(root_out.trim()).ends_with(format!("{}/node_modules", tag_leaf(&dir))),
        "root must print the project's node_modules: {root_out}"
    );

    let (bin_out, stderr, code) = run_nub(&dir, &["bin"]);
    assert_eq!(code, 0, "bin: stderr: {stderr}");
    assert!(
        Path::new(bin_out.trim()).ends_with(format!("{}/node_modules/.bin", tag_leaf(&dir))),
        "bin must print the project's bin dir: {bin_out}"
    );

    let (check_out, stderr, code) = run_nub(&dir, &["check"]);
    assert_eq!(code, 0, "check: stdout: {check_out}\nstderr: {stderr}");
    assert!(
        check_out.contains("checked 0 packages"),
        "check without an install must report zero packages: {check_out}"
    );

    for argv in [&["licenses", "list"][..], &["licenses", "ls"][..]] {
        let (stdout, stderr, code) = run_nub(&dir, argv);
        assert_eq!(code, 0, "nub {argv:?}: stdout: {stdout}\nstderr: {stderr}");
        assert_no_engine_branding(&[("stdout", &stdout), ("stderr", &stderr)]);
    }
    assert_no_engine_branding(&[
        ("root", &root_out),
        ("bin", &bin_out),
        ("check", &check_out),
        ("stderr", &stderr),
    ]);
}

/// Last path segment of a fixture dir (macOS canonicalizes `/var` →
/// `/private/var`, so suffix comparison is the stable form).
fn tag_leaf(dir: &Path) -> String {
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

/// The workspace-yaml brand toggle: an `aube-workspace.yaml` on disk is
/// another tool's state and must not change what nub reads. The probe rides
/// the workspace-root walk — from a member directory with no lockfile of
/// its own, `nub list` resolves the workspace root (which holds a lockfile)
/// only if the yaml is honored. With the toggle, the member is a standalone
/// project and the no-lockfile short-circuit fires; an identical fixture
/// keyed by `pnpm-workspace.yaml` resolves the root and runs the engine.
#[test]
fn aube_workspace_yaml_is_not_consulted_for_workspace_discovery() {
    let fixture = |yaml_name: &str| {
        let root = pm_tmpdir(&format!("wsyaml-{}", &yaml_name[..4]));
        std::fs::write(root.join(yaml_name), "packages:\n  - 'pkgs/*'\n").unwrap();
        std::fs::write(
            root.join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n\n  pkgs/app: {}\n",
        )
        .unwrap();
        let member = root.join("pkgs/app");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(member.join("package.json"), r#"{"name":"app"}"#).unwrap();
        member
    };

    let (_, stderr, code) = run_nub(&fixture("aube-workspace.yaml"), &["list"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stderr.contains("No lockfile found"),
        "aube-workspace.yaml must not promote the member into a workspace: {stderr}"
    );

    let (_, stderr, code) = run_nub(&fixture("pnpm-workspace.yaml"), &["list"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        !stderr.contains("No lockfile found"),
        "pnpm-workspace.yaml must resolve the root's lockfile: {stderr}"
    );
}

/// Role-gating: the pnpm-specific config surface (`pnpm-workspace.yaml`, the
/// `package.json#pnpm.*` namespace) is OFF for an npm/yarn/bun incumbent. A
/// yarn project someone copied a pnpm tutorial's workspace yaml into must not
/// silently adopt its `packages` glob — under nub identity that yaml is already
/// ignored, and a non-pnpm compat role gets the same treatment. The same
/// fixture keyed to a pnpm project still resolves the root (the pnpm surface is
/// live for the pnpm role). Rides the workspace-root walk like the
/// `aube-workspace.yaml` probe above: from a member with no lockfile, the
/// member is standalone unless the yaml is honored.
#[test]
fn pnpm_workspace_yaml_is_gated_off_for_a_non_pnpm_role() {
    // `(pm, lockfile_name, lockfile_body)` — the yarn role vs. the pnpm role,
    // both carrying an otherwise-promoting `pnpm-workspace.yaml`.
    let fixture = |pm: &str, lock_name: &str, lock_body: &str| {
        let root = pm_tmpdir(&format!("rolegate-{}", &pm[..3]));
        std::fs::write(
            root.join("package.json"),
            format!(r#"{{"name":"root","packageManager":"{pm}"}}"#),
        )
        .unwrap();
        std::fs::write(root.join(lock_name), lock_body).unwrap();
        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages:\n  - 'pkgs/*'\n",
        )
        .unwrap();
        let member = root.join("pkgs/app");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(member.join("package.json"), r#"{"name":"app"}"#).unwrap();
        member
    };

    // Yarn role: the stray yaml is not read, so the member stands alone.
    let (_, stderr, code) = run_nub(&fixture("yarn@4.0.0", "yarn.lock", "# yarn\n"), &["list"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stderr.contains("No lockfile found"),
        "a yarn project's stray pnpm-workspace.yaml must not promote the member: {stderr}"
    );

    // pnpm role: the yaml is live, so the member resolves the workspace root.
    let pnpm_lock = "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n\n  pkgs/app: {}\n";
    let (_, stderr, code) = run_nub(
        &fixture("pnpm@9.0.0", "pnpm-lock.yaml", pnpm_lock),
        &["list"],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        !stderr.contains("No lockfile found"),
        "a pnpm project's pnpm-workspace.yaml must still resolve the root: {stderr}"
    );
}

/// Per-verb `--help` renders (engine verbs bypass nub's top-level clap), is
/// named for nub, and carries no engine verb spellings.
#[test]
fn verb_help_is_rendered_and_rebranded() {
    let dir = pm_tmpdir("help");
    let (stdout, stderr, code) = run_nub(&dir, &["outdated", "--help"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("Usage: nub outdated"),
        "help must be named for nub: {stdout}"
    );
    assert!(
        !stdout.contains("aube outdated") && !stdout.contains("`aube "),
        "engine verb spellings must rebrand in help: {stdout}"
    );
}

/// The audit write gate: `--fix=update` would rewrite the lockfile, which is
/// refused on yarn projects (write-tier policy), byte-preserving yarn.lock.
/// Fires pre-network, so this is offline-safe.
#[test]
fn audit_fix_update_refuses_to_touch_yarn_lock() {
    let dir = pm_tmpdir("yarnaudit");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"yarnaudit","version":"1.0.0","dependencies":{"left-pad":"^1.3.0"}}"#,
    )
    .unwrap();
    let yarn_lock = "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n\
                     # yarn lockfile v1\n\n\n\
                     left-pad@^1.3.0:\n\
                     \x20\x20version \"1.3.0\"\n\
                     \x20\x20resolved \"https://registry.yarnpkg.com/left-pad/-/left-pad-1.3.0.tgz#5b8a3a7765dfe001261dde915589e782f8c94d1e\"\n\
                     \x20\x20integrity sha512-XI5MPzVNApjAyhQzphX8BkmKsKUxD4LdyK24iZeQGinBN9yTQT3bFlCBy/aVx2HrNcqQGsdot8ghrjyrvMCoEA==\n";
    std::fs::write(dir.join("yarn.lock"), yarn_lock).unwrap();

    let (stdout, stderr, code) = run_nub(&dir, &["audit", "--fix=update"]);
    assert_ne!(code, 0, "the yarn write gate must refuse: {stdout}{stderr}");
    assert!(
        stderr.contains("refusing to modify yarn.lock"),
        "the gate must name the refusal: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("yarn.lock")).unwrap(),
        yarn_lock,
        "yarn.lock must be byte-identical after the refusal"
    );
}

/// `outdated` against the real registry: a lockfile pinned behind the
/// manifest range reports drift and exits 1 (pnpm-compat). is-positive's
/// latest has been 3.1.0 for years — stable fixture data.
#[test]
#[ignore = "network: fetches the is-positive packument from the npm registry"]
fn outdated_reports_registry_drift_and_exits_one() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = lockfile_fixture(
        "outdated",
        "is-positive",
        "^3.0.0",
        "3.0.0",
        IS_POSITIVE_300,
    );
    let (stdout, stderr, code) = run_nub(&dir, &["outdated"]);
    assert_eq!(
        code, 1,
        "drift must exit 1: stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("is-positive") && stdout.contains("3.0.0") && stdout.contains("3.1.0"),
        "the drift row must show current and wanted: {stdout}"
    );
    assert_no_engine_branding(&[("stdout", &stdout), ("stderr", &stderr)]);
}

/// `audit` against the real registry: lodash 4.17.20 carries published
/// advisories (advisories are never retracted), so the report is non-empty
/// and exits 1 (pnpm-compat).
#[test]
#[ignore = "network: fetches bulk advisories from the npm registry"]
fn audit_surfaces_known_advisories_and_exits_one() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = lockfile_fixture("audit", "lodash", "4.17.20", "4.17.20", LODASH_41720);
    let (stdout, stderr, code) = run_nub(&dir, &["audit"]);
    assert_eq!(
        code, 1,
        "known advisories must exit 1: stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("lodash"),
        "the advisory table must name the package: {stdout}"
    );
    assert_no_engine_branding(&[("stdout", &stdout), ("stderr", &stderr)]);
}
