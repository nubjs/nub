//! End-to-end (through the binary) tests for the install abort-eagerly policy:
//! a lockfile source nub can't resolve, or a Yarn PnP project, aborts at PLAN
//! time — before any `node_modules` write — with a precise, branded refusal,
//! instead of a silent reclassify→404 / downgrade. The reader-level behavior
//! lives in `vendor/aube/crates/aube-lockfile`; these assert the nub surface:
//! the rebranded code, a non-zero exit, an untouched tree, and the optional
//! carve-out (warn + proceed, not abort).
//!
//! All three are hermetic — the fatals abort before any fetch, and the
//! optional case has nothing left to install — so none need `#[ignore]`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn tmpdir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-abort-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Spawn `nub <args>` in `dir`, isolating the store/cache to fresh temp roots.
fn run(dir: &Path, args: &[&str]) -> (String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", tmpdir("xdg-data"))
        .env("XDG_CACHE_HOME", tmpdir("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (combined, out.status.code().unwrap_or(-1))
}

fn write(dir: &Path, files: &[(&str, &str)]) {
    for (name, body) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }
}

const GIT_DEP_PKG: &str =
    r#"{"name":"t","version":"1.0.0","dependencies":{"foo":"user/repo#abc123"}}"#;
const GIT_DEP_LOCK: &str = "# yarn lockfile v1\n\n\"foo@user/repo#abc123\":\n  version \"1.0.0\"\n  resolved \"https://codeload.github.com/user/repo/tar.gz/abc123\"\n";

#[test]
fn install_aborts_on_unresolvable_yarn_lock_source() {
    for verb in ["install", "ci"] {
        let dir = tmpdir("git");
        write(
            &dir,
            &[("package.json", GIT_DEP_PKG), ("yarn.lock", GIT_DEP_LOCK)],
        );
        let (out, code) = run(&dir, &[verb]);
        assert_ne!(code, 0, "`nub {verb}` must abort on an unresolvable source");
        assert!(
            out.contains("ERR_NUB_LOCKFILE_UNSUPPORTED_SOURCE"),
            "`nub {verb}` output should carry the rebranded code; got:\n{out}"
        );
        // The refusal names the offending entry and protocol. miette wraps the
        // rendered diagnostic at terminal width (CI's width differs from a dev
        // box's), so match against a whitespace-flattened copy — the entry key
        // and protocol token carry no internal whitespace, so a wrap can only
        // have split them across a newline + indent.
        let flat: String = out.split_whitespace().collect();
        assert!(
            flat.contains("foo@user/repo#abc123") && flat.contains("git"),
            "`nub {verb}` should name the offending entry and protocol; got:\n{out}"
        );
        // No brand leak — the engine's `aube` code must be rewritten.
        assert!(!out.contains("ERR_AUBE_LOCKFILE_UNSUPPORTED_SOURCE"));
        // Genuinely pre-mutation: no node_modules was created.
        assert!(
            !dir.join("node_modules").exists(),
            "`nub {verb}` must abort before writing node_modules"
        );
    }
}

#[test]
fn install_aborts_on_yarn_pnp() {
    for verb in ["install", "ci"] {
        let dir = tmpdir("pnp");
        write(
            &dir,
            &[
                ("package.json", r#"{"name":"t","version":"1.0.0"}"#),
                ("yarn.lock", "# yarn lockfile v1\n"),
                (".yarnrc.yml", "nodeLinker: pnp\n"),
            ],
        );
        let (out, code) = run(&dir, &[verb]);
        assert_ne!(code, 0, "`nub {verb}` must abort on a Yarn PnP project");
        assert!(
            out.contains("ERR_NUB_PNP_UNSUPPORTED"),
            "`nub {verb}` should refuse PnP with the branded code; got:\n{out}"
        );
        assert!(!dir.join("node_modules").exists());
    }
}

#[test]
fn install_proceeds_on_optional_unresolvable_source() {
    // decision #3: an OPTIONAL unresolvable dep warns and the install proceeds
    // (matching the incumbent's tolerance of a missing optional), instead of
    // aborting. With nothing else to install, the run completes offline.
    let dir = tmpdir("opt");
    write(
        &dir,
        &[
            (
                "package.json",
                r#"{"name":"t","version":"1.0.0","optionalDependencies":{"foo":"user/repo#abc123"}}"#,
            ),
            (
                "yarn.lock",
                "# yarn lockfile v1\n\n\"foo@user/repo#abc123\":\n  version \"1.0.0\"\n  resolved \"x\"\n",
            ),
        ],
    );
    let (out, code) = run(&dir, &["install"]);
    assert_eq!(
        code, 0,
        "an optional unresolvable dep must not abort; got:\n{out}"
    );
    assert!(
        out.contains("WARN_NUB_LOCKFILE_UNSUPPORTED_SOURCE"),
        "the optional skip should warn; got:\n{out}"
    );
}

const BUN_EXOTIC_PKG: &str =
    r#"{"name":"t","version":"1.0.0","dependencies":{"foo":"exotic:bar"}}"#;
const BUN_EXOTIC_LOCK: &str = r#"{
  "lockfileVersion": 1,
  "workspaces": { "": { "dependencies": { "foo": "exotic:bar" } } },
  "packages": { "foo": ["foo@exotic:bar", {}] }
}"#;

#[test]
fn install_aborts_on_unresolvable_bun_lock_source() {
    // The bun twin of the yarn case: an unknown-protocol bun.lock entry is
    // a plan-time fatal, not a silent reclassify-to-registry that 404s
    // mid-install. (No real bun protocol triggers this today — the fixture
    // is the future-proof/defense-in-depth path.)
    for verb in ["install", "ci"] {
        let dir = tmpdir("bun");
        write(
            &dir,
            &[
                ("package.json", BUN_EXOTIC_PKG),
                ("bun.lock", BUN_EXOTIC_LOCK),
            ],
        );
        let (out, code) = run(&dir, &[verb]);
        assert_ne!(code, 0, "`nub {verb}` must abort on an unresolvable source");
        assert!(
            out.contains("ERR_NUB_LOCKFILE_UNSUPPORTED_SOURCE"),
            "`nub {verb}` output should carry the rebranded code; got:\n{out}"
        );
        let flat: String = out.split_whitespace().collect();
        assert!(
            flat.contains("foo@exotic:bar") && flat.contains("exotic"),
            "`nub {verb}` should name the offending entry and protocol; got:\n{out}"
        );
        assert!(!out.contains("ERR_AUBE_LOCKFILE_UNSUPPORTED_SOURCE"));
        assert!(
            !dir.join("node_modules").exists(),
            "`nub {verb}` must abort before writing node_modules"
        );
    }
}

#[test]
fn ci_proceeds_on_optional_unresolvable_bun_lock_source() {
    // The optional carve-out on the bun reader: warn + skip, recorded as a
    // consciously-skipped optional so the frozen drift check tolerates it.
    let dir = tmpdir("bun-opt");
    write(
        &dir,
        &[
            (
                "package.json",
                r#"{"name":"t","version":"1.0.0","optionalDependencies":{"foo":"exotic:bar"}}"#,
            ),
            (
                "bun.lock",
                r#"{
  "lockfileVersion": 1,
  "workspaces": { "": { "optionalDependencies": { "foo": "exotic:bar" } } },
  "packages": { "foo": ["foo@exotic:bar", {}] }
}"#,
            ),
        ],
    );
    let (out, code) = run(&dir, &["ci"]);
    assert_eq!(
        code, 0,
        "an optional unresolvable dep must not abort; got:\n{out}"
    );
    assert!(
        out.contains("WARN_NUB_LOCKFILE_UNSUPPORTED_SOURCE"),
        "the optional skip should warn; got:\n{out}"
    );
}
