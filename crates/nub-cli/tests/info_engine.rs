//! Info-family verbs (`list`/`why`/`outdated`/`audit`/`peers`, …) through
//! the embedded pnpm 12 engine, end-to-end through the binary. There is no
//! host module left to point at: pnpm's grammar knows every verb here, so
//! the front door hands each command line straight to the engine.
//!
//! Every verb here is an ENGINE verb, so the contract is pnpm 12's behavior
//! with the embedder's rebrand over it — and the rebrand is scoped by project
//! IDENTITY. A pnpm-incumbent project (a `pnpm-lock.yaml` or a
//! `pnpm-workspace.yaml`) must be indistinguishable from pnpm, `ERR_PNPM_*`
//! codes included; everything else is nub-incumbent, speaks `ERR_NUB_*`, and
//! names `nub.lock` as its lockfile. Several fixtures below are one file away
//! from flipping identity, so each states the one it means.
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

/// The output brand boundary: no `pnpm` spelling on either stream. It holds
/// only under NUB identity — under pnpm identity the `pnpm` spelling is the
/// contract, not a leak. A1.7 exempts real on-disk names, so this is only
/// exact for a fixture that has no pnpm-named file in it; every caller below
/// is one.
fn assert_no_pnpm_branding(streams: &[(&str, &str)]) {
    for (name, s) in streams {
        assert!(
            !s.to_lowercase().contains("pnpm"),
            "pnpm branding leaked on {name} under nub identity: {s}"
        );
    }
}

/// Whether `dir` resolves as a workspace, read off `deploy`'s two refusals:
/// `CANNOT_DEPLOY` is "not in a workspace", `CANNOT_DEPLOY_MANY` is "in one,
/// but it has more than one project". A cheap probe that needs no install,
/// and the only one that answers for a member directory.
fn in_workspace(dir: &Path) -> bool {
    let (_, stderr, _) = run_nub(dir, &["deploy", "out"]);
    assert!(
        stderr.contains("CANNOT_DEPLOY"),
        "the workspace probe expects one of deploy's two refusals: {stderr}"
    );
    stderr.contains("CANNOT_DEPLOY_MANY")
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
/// handcrafted graph, print the dep on stdout, and exit 0.
#[test]
fn lockfile_read_verbs_work_offline() {
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
    }

    // `--json` is machine-readable and carries the version.
    let (stdout, _, _) = run_nub(&dir, &["list", "--json"]);
    assert!(
        stdout.contains("\"3.1.0\""),
        "list --json must carry the version: {stdout}"
    );
}

/// The never-installed project, on a nub-incumbent fixture.
///
/// nub does not short-circuit ahead of the engine here: A1.6 routes these
/// verbs to it unchanged. The two verbs diverge, and that divergence is pnpm's:
///   - `list` is a valid query against an empty graph, so it prints the real
///     empty listing on stdout and exits 0.
///   - `outdated` cannot answer without a lockfile, so it errors and exits 1.
///
/// MEASURED against pnpm 12.4.1 on an identical fixture: byte-identical on
/// both verbs once the identity-correct brand is substituted
/// (`ERR_PNPM_OUTDATED_NO_LOCKFILE` / `pnpm install`). The nub-identity
/// rebrand is therefore what this pins. The error body is line-wrapped by the
/// diagnostic renderer at a width the fixture's path length decides, so it is
/// asserted in fragments that survive the wrap rather than as one sentence.
#[test]
fn missing_lockfile_defers_to_the_engine_and_rebrands_for_nub_identity() {
    let dir = pm_tmpdir("nolock");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"nolock","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();

    // `list`: the real empty listing, not a short-circuit note.
    let (stdout, stderr, code) = run_nub(&dir, &["list"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("nolock@1.0.0") && stdout.contains("0 packages"),
        "list must print the engine's empty listing: {stdout}"
    );
    assert!(
        !stderr.contains("No lockfile found"),
        "nub's own no-lockfile short-circuit must not fire: {stderr}"
    );
    assert_no_pnpm_branding(&[("stdout", &stdout), ("stderr", &stderr)]);

    // `outdated`: the engine's error, rebranded for nub identity, exit 1.
    let (stdout, stderr, code) = run_nub(&dir, &["outdated"]);
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("ERR_NUB_OUTDATED_NO_LOCKFILE"),
        "the error code must rebrand under nub identity: {stderr}"
    );
    assert!(
        stderr.contains("No lockfile in directory"),
        "outdated must speak the engine's own diagnostic: {stderr}"
    );
    assert_no_pnpm_branding(&[("stdout", &stdout), ("stderr", &stderr)]);
}

/// `--filter` on a project that is not a workspace, on a nub-incumbent
/// fixture.
///
/// nub runs no `--filter` pre-flight of its own, so the filter is simply a
/// selector that matches no project — a silent no-op at
/// exit 0. That is not an obviously-right behavior, so it is pinned against
/// the reference rather than reasoned about: MEASURED identical to pnpm 12.4.1
/// on the same fixture for all three verb shapes, and identical again for a
/// matching and a non-matching filter inside a real workspace.
///
/// The regression this guards is a nub-side pre-flight coming back: it would
/// fail the command where pnpm succeeds, on a path A1.6 routes to the engine
/// unchanged.
#[test]
fn filter_on_a_non_workspace_is_a_silent_no_op() {
    let dir = pm_tmpdir("filter-nonws");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    // `nub.lock`, not `pnpm-lock.yaml`: a pnpm-named lockfile would make this
    // a pnpm-incumbent project and put the `pnpm` spelling legitimately back
    // in range of the brand assertion below.
    std::fs::write(
        dir.join("nub.lock"),
        "lockfileVersion: '9.0'\n\nimporters:\n  .:\n    dependencies: {}\n",
    )
    .unwrap();

    for verb in [
        vec!["list", "--filter", "foo"],
        vec!["list", "-r", "--filter", "foo"],
        vec!["why", "react", "--filter", "foo"],
    ] {
        let (stdout, stderr, code) = run_nub(&dir, &verb);
        assert_eq!(code, 0, "{verb:?}: stdout: {stdout}\nstderr: {stderr}");
        assert!(
            !stderr.contains("--filter requires a workspace root"),
            "nub's own --filter pre-flight must not fire: {stderr}"
        );
        assert_no_pnpm_branding(&[("stdout", &stdout), ("stderr", &stderr)]);
    }
}

/// `--json` in the never-installed (no-lockfile) state, on a nub-incumbent
/// fixture.
///
/// OLD CONTRACT (D2): `--json` must ALWAYS put parseable JSON on stdout, so
/// `nub list --json | jq` and `nub outdated --json | jq` both work before the
/// first install — an importer-header array for `list`, `{}` for `outdated`.
/// `--format json` was accepted as the long spelling of `--json`.
///
/// NEW CONTRACT: `list` keeps the guarantee, `outdated` does not, and
/// `--format` is not a flag at all. D2 was written against what pnpm did at
/// the time; A1.6 routes all three to the engine unchanged, so pnpm 12 is now
/// what decides. MEASURED against pnpm 12.4.1 on an identical fixture, all
/// three identical:
///   - `list --json` → the same importer array, now also carrying `private`.
///   - `list --format json` → rejected by the parser, exit 2, `--format` gone
///     from pnpm's `list` (it suggests `--sort`).
///   - `outdated --json` → the no-lockfile error, exit 1, no JSON at all.
///
/// So the surviving JSON guarantee is `list`'s alone, and it is the one worth
/// pinning: it is what a caller pipes into `jq` on a fresh checkout.
#[test]
fn list_json_emits_the_empty_importer_shape_without_a_lockfile() {
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
    assert_no_pnpm_branding(&[("stdout", &stdout), ("stderr", &stderr)]);

    // `--format json` is not pnpm 12 grammar. The rejection is the parser's,
    // so what matters is that it is spelled in the host's name.
    let (stdout, stderr, code) = run_nub(&dir, &["list", "--format", "json"]);
    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("unexpected argument '--format'") && stderr.contains("Usage: nub list"),
        "the rejection must be rendered in nub's name: {stderr}"
    );

    // `outdated --json` has no lockfile to report on, so it errors rather than
    // emitting an empty document.
    let (stdout, stderr, code) = run_nub(&dir, &["outdated", "--json"]);
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.trim().is_empty(),
        "outdated --json must not emit a document it cannot fill: {stdout}"
    );
    assert!(
        stderr.contains("ERR_NUB_OUTDATED_NO_LOCKFILE"),
        "the error code must rebrand under nub identity: {stderr}"
    );
    assert_no_pnpm_branding(&[("stdout", &stdout), ("stderr", &stderr)]);
}

/// The path verbs print the resolved project locations without any install,
/// and `licenses` accepts pnpm's documented `list` spelling beside the
/// engine's `ls` (reviewer #6). All offline.
#[test]
fn bin_root_and_licenses_list_work_offline() {
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

    for argv in [&["licenses", "list"][..], &["licenses", "ls"][..]] {
        let (stdout, stderr, code) = run_nub(&dir, argv);
        assert_eq!(code, 0, "nub {argv:?}: stdout: {stdout}\nstderr: {stderr}");
    }
}

/// Last path segment of a fixture dir (macOS canonicalizes `/var` →
/// `/private/var`, so suffix comparison is the stable form).
fn tag_leaf(dir: &Path) -> String {
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

/// Which file declares a workspace, per identity.
///
/// nub's embedder sets `workspaces_from_package_manifest`, so a
/// nub-incumbent project declares members in the neutral
/// `package.json#workspaces`; a pnpm-incumbent one uses
/// `pnpm-workspace.yaml`, exactly as pnpm does. The two are not
/// interchangeable, and the cross cells are the point: neither file works
/// under the other identity.
///
/// Note the fixtures cannot be varied by yaml alone — planting a
/// `pnpm-workspace.yaml` is itself a pnpm-incumbency signal — so identity and
/// declaration move together, and the lockfile name carries the identity.
///
/// MEASURED, with pnpm 12.4.1 agreeing on both pnpm-identity rows.
#[test]
fn workspace_members_are_declared_per_identity() {
    // `(tag, lockfile name, yaml?, neutral workspaces field?)` → the member.
    let fixture = |tag: &str, lock_name: &str, yaml: bool, neutral: bool| {
        let root = pm_tmpdir(&format!("wsdecl-{tag}"));
        let field = if neutral {
            r#","workspaces":["pkgs/*"]"#
        } else {
            ""
        };
        std::fs::write(
            root.join("package.json"),
            format!(r#"{{"name":"root","version":"1.0.0"{field}}}"#),
        )
        .unwrap();
        if yaml {
            std::fs::write(
                root.join("pnpm-workspace.yaml"),
                "packages:\n  - 'pkgs/*'\n",
            )
            .unwrap();
        }
        std::fs::write(
            root.join(lock_name),
            "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n\n  pkgs/app: {}\n",
        )
        .unwrap();
        let member = root.join("pkgs/app");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            member.join("package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();
        member
    };

    // nub identity: the neutral field declares the workspace…
    assert!(
        in_workspace(&fixture("nub-neutral", "nub.lock", false, true)),
        "package.json#workspaces must declare a nub project's members"
    );
    // …and nothing else does. This is the negative control that proves the
    // probe discriminates rather than always answering yes.
    assert!(
        !in_workspace(&fixture("nub-bare", "nub.lock", false, false)),
        "a nub project with no workspaces field is a single project"
    );

    // pnpm identity: the yaml declares the workspace and the neutral field is
    // ignored — pnpm's own rule, mirrored.
    assert!(
        in_workspace(&fixture("pnpm-yaml", "pnpm-lock.yaml", true, false)),
        "pnpm-workspace.yaml must declare a pnpm project's members"
    );
    assert!(
        !in_workspace(&fixture("pnpm-neutral", "pnpm-lock.yaml", false, true)),
        "pnpm ignores package.json#workspaces, so a pnpm project must too"
    );
}

/// The `packageManager` pin naming a PM that is not us.
///
/// OLD CONTRACT: a "yarn role" existed beside a "pnpm role", and the
/// pnpm-specific config surface was gated OFF for it — a yarn project someone
/// copied a pnpm tutorial's `pnpm-workspace.yaml` into must not adopt its
/// `packages` glob.
///
/// That test could not do what it said. Its yarn fixture planted a
/// `pnpm-workspace.yaml`, which IS a pnpm-incumbency signal, so the project it
/// built was pnpm-incumbent and the gate under test was never reached. A1.5
/// then removed the yarn role outright.
///
/// NEW CONTRACT, and the live question the old one was circling: the engine
/// refuses to run when the pin names another PM. nub switches that check OFF
/// under its own identity — a `packageManager` pin is not nub's to enforce,
/// and nub provisions the runtime itself (`manage_package_manager_versions` is
/// false) — and mirrors pnpm's behavior under pnpm identity, where the refusal
/// and its `pnpm`-spelled help text are the contract rather than a leak.
///
/// MEASURED: the pnpm-identity refusal is byte-identical to pnpm 12.4.1's,
/// `ERR_PNPM_OTHER_PM_EXPECTED` included. The two fixtures differ by one file.
#[test]
fn a_foreign_package_manager_pin_is_enforced_only_under_pnpm_identity() {
    // The same yarn pin either way; `pnpm_incumbent` adds the one file that
    // hands the project to pnpm's rules.
    let fixture = |tag: &str, pnpm_incumbent: bool| {
        let root = pm_tmpdir(&format!("pingate-{tag}"));
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"root","version":"1.0.0","packageManager":"yarn@4.0.0"}"#,
        )
        .unwrap();
        std::fs::write(root.join("yarn.lock"), "# yarn\n").unwrap();
        if pnpm_incumbent {
            std::fs::write(
                root.join("pnpm-workspace.yaml"),
                "packages:\n  - 'pkgs/*'\n",
            )
            .unwrap();
        }
        root
    };

    // nub identity: the pin is inert, so an ordinary query just runs. A1.5
    // also makes the yarn.lock a foreign file nub ignores rather than adopts.
    let dir = fixture("nub", false);
    let (stdout, stderr, code) = run_nub(&dir, &["list"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !stderr.contains("OTHER_PM_EXPECTED"),
        "a packageManager pin must not gate a nub-identity project: {stderr}"
    );
    assert!(
        stdout.contains("root@1.0.0"),
        "the query must run to completion: {stdout}"
    );
    assert_no_pnpm_branding(&[("stdout", &stdout), ("stderr", &stderr)]);

    // pnpm identity: pnpm's refusal, mirrored whole. The `pnpm` spellings in
    // the help text are pnpm's own output on a project that asked for pnpm's
    // rules, so `assert_no_pnpm_branding` deliberately does not apply.
    let dir = fixture("pnpm", true);
    let (stdout, stderr, code) = run_nub(&dir, &["list"]);
    assert_ne!(
        code, 0,
        "the pin must gate a pnpm-identity project: {stdout}"
    );
    assert!(
        stderr.contains("ERR_PNPM_OTHER_PM_EXPECTED")
            && stderr.contains("This project is configured to use yarn"),
        "the refusal must match pnpm's: {stderr}"
    );
}

/// Per-verb `--help` renders (engine verbs bypass nub's top-level clap) and is
/// named for nub.
#[test]
fn verb_help_is_rendered_and_rebranded() {
    let dir = pm_tmpdir("help");
    let (stdout, stderr, code) = run_nub(&dir, &["outdated", "--help"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("Usage: nub outdated"),
        "help must be named for nub: {stdout}"
    );
}

/// `audit --fix=update` in a repo whose only lockfile is a foreign
/// `yarn.lock`.
///
/// OLD CONTRACT: nub ran a write-tier gate that refused the command outright
/// with `refusing to modify yarn.lock`, because yarn.lock write fidelity was
/// unproven in the embedded engine.
///
/// NEW CONTRACT: A1.5 removes yarn incumbency, so there is no yarn project and
/// no write tier to gate. This is a nub-incumbent project that happens to have
/// a foreign file in it — MEASURED: the fixture reports `ERR_NUB_CANNOT_DEPLOY`
/// on the identity probe. nub ignores the yarn.lock, finds no lockfile of its
/// own, and refuses for that reason instead. The yarn.lock is still left
/// untouched, which is the half of the old contract that survives.
///
/// FAILING ON PURPOSE — the lockfile name in the diagnostic is wrong, and the
/// assertion below states the correct name rather than the current one. Under
/// nub identity the lockfile is `nub.lock` (`pnpm_engine.rs`'s embedder sets
/// `lockfile_basename`), so `No pnpm-lock.yaml found` names a file nub never
/// writes. A1.7's on-disk-filename exemption does not cover it: there is no
/// such file and there never would be. The string is a compile-time literal in
/// the engine's `AuditError` (`cli/src/cli_args/audit/report.rs`), so the
/// process-wide rebrand cannot reach it; upstream's TypeScript interpolated
/// `WANTED_LOCKFILE` here and the Rust port hardcoded it.
///
/// The same hardcoding has a worse effect one layer down, not covered here
/// because it needs the network: `audit/fix.rs` re-reads the post-update
/// lockfile through `Lockfile::load_wanted_from_dir`, which resolves
/// `Lockfile::FILE_NAME` rather than the embedder's basename. On a
/// nub-incumbent project the update therefore succeeds, writes `nub.lock`, and
/// then fails looking for a `pnpm-lock.yaml` — so `--fix=update` cannot exit 0
/// under nub identity at all. The audit pre-flight itself is embedder-aware
/// (it reads `nub.lock` fine), so only the `fix.rs` call site is wrong.
///
/// Fires pre-network, so this is offline-safe.
#[test]
fn audit_fix_update_leaves_a_foreign_yarn_lock_alone() {
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
    assert_ne!(
        code, 0,
        "a project with no lockfile cannot audit: {stdout}{stderr}"
    );
    assert!(
        stderr.contains("ERR_NUB_AUDIT_NO_LOCKFILE"),
        "the error code must rebrand under nub identity: {stderr}"
    );
    assert!(
        stderr.contains("No nub.lock found"),
        "the diagnostic must name nub's own lockfile, not one nub never \
         writes: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("yarn.lock")).unwrap(),
        yarn_lock,
        "a foreign yarn.lock must be byte-identical after the refusal"
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
}
