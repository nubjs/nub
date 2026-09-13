//! End-to-end (through the binary) tests for the install abort-eagerly policy:
//! a dependency nub can't resolve aborts at PLAN time — before any
//! `node_modules` write — with a precise, rebranded refusal, instead of a
//! silent reclassify→404 / downgrade, and an OPTIONAL one warns and proceeds
//! rather than aborting.
//!
//! These were written against the vendored aube engine, which enforced the
//! policy in its foreign-lockfile READER: it parsed a `yarn.lock`/`bun.lock`,
//! and an entry whose source it could not resolve raised
//! `ERR_NUB_LOCKFILE_UNSUPPORTED_SOURCE` (exit 14), with a sibling refusal for
//! a Yarn PnP project. Nub no longer reads another package manager's lockfile
//! at all, so both of those codes are unreachable and the reader they lived in
//! is gone. The POLICY survives, relocated to the engine's own resolver: an
//! unresolvable spec in the MANIFEST fails the dependency-tree resolve, exits
//! non-zero, and leaves the tree untouched.
//!
//! Every fixture below was run on both engines (`NUB_PM_ENGINE=aube` and the
//! default) and differentialled against pnpm 12.4.1, which produces the same
//! refusals byte for byte with `ERR_PNPM_*` in place of `ERR_NUB_*`. That
//! rewrite is the only difference, which is why the no-brand-leak assertions
//! below now watch for `ERR_PNPM_`.
//!
//! Still hermetic: `exotic:bar` is rejected by spec classification before any
//! network call — confirmed by re-running each fixture with `registry` pointed
//! at a dead port and getting the identical refusal — and the optional and PnP
//! cases have nothing left to install. So none needs `#[ignore]`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/ (or fast/)
    path.push("nub");
    path
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

/// A manifest whose only dependency carries a protocol no resolver claims.
/// Spec classification rejects it before any network call, which is what makes
/// every fixture built on it hermetic.
const UNRESOLVABLE_PKG: &str =
    r#"{"name":"t","version":"1.0.0","dependencies":{"foo":"exotic:bar"}}"#;

/// Each foreign lockfile pins a plain, perfectly resolvable `foo@1.0.0`. That
/// is the point: an engine that still read one would install `1.0.0` instead
/// of refusing, so a refusal naming the MANIFEST's `exotic:bar` is positive
/// evidence the lockfile was never consulted.
const YARN_LOCK_PINNING_FOO: &str = "# yarn lockfile v1\n\nfoo@^1.0.0:\n  version \"1.0.0\"\n  resolved \"https://registry.npmjs.org/foo/-/foo-1.0.0.tgz\"\n";
const BUN_LOCK_PINNING_FOO: &str = r#"{
  "lockfileVersion": 1,
  "workspaces": { "": { "dependencies": { "foo": "^1.0.0" } } },
  "packages": { "foo": ["foo@1.0.0", {}] }
}"#;

/// An unresolvable dependency aborts the install before the tree is touched,
/// and no foreign lockfile sitting beside it changes that.
///
/// Was two tests, one per lockfile flavor, because aube gave yarn and bun
/// separate readers and each had to refuse its own unsupported source. Neither
/// file is read now, so both flavors take one code path and a second copy would
/// assert nothing the first did not.
#[test]
fn install_aborts_on_an_unresolvable_spec_no_foreign_lockfile_rescues_it() {
    for (lockfile, body) in [
        ("yarn.lock", YARN_LOCK_PINNING_FOO),
        ("bun.lock", BUN_LOCK_PINNING_FOO),
    ] {
        let dir = tmpdir("unresolvable");
        write(
            &dir,
            &[("package.json", UNRESOLVABLE_PKG), (lockfile, body)],
        );
        let (out, code) = run(&dir, &["install"]);
        assert_ne!(
            code, 0,
            "`nub install` beside {lockfile} must abort on an unresolvable spec; got:\n{out}"
        );
        assert!(
            out.contains("ERR_NUB_SPEC_NOT_SUPPORTED_BY_ANY_RESOLVER"),
            "the refusal beside {lockfile} should carry the rebranded code; got:\n{out}"
        );
        // The refusal names the offending spec. miette wraps the rendered
        // diagnostic at terminal width (CI's width differs from a dev box's),
        // so match against a whitespace-flattened copy — the spec carries no
        // internal whitespace, so a wrap can only have split it across a
        // newline + indent.
        let flat: String = out.split_whitespace().collect();
        assert!(
            flat.contains("foo@exotic:bar"),
            "the refusal should name the manifest's spec, not {lockfile}'s pin; got:\n{out}"
        );
        assert!(
            !flat.contains("foo@1.0.0"),
            "{lockfile} pins foo@1.0.0; naming it would mean the lockfile was read; got:\n{out}"
        );
        // No brand leak — the engine's `pnpm` codes must be rewritten.
        assert!(!out.contains("ERR_PNPM_"), "brand leak; got:\n{out}");
        // Genuinely pre-mutation: no node_modules was created.
        assert!(
            !dir.join("node_modules").exists(),
            "`nub install` must abort before writing node_modules"
        );
    }
}

/// A Yarn PnP project installs an ordinary `node_modules` tree.
///
/// Was an abort (`ERR_NUB_PNP_UNSUPPORTED`). That refusal existed because nub
/// honored the project's `yarn.lock` while being unable to reproduce yarn's PnP
/// layout, so installing would have silently diverged from the incumbent. Nub
/// no longer reads the `yarn.lock` or any yarn-branded config, `.yarnrc.yml`
/// included, so there is no incumbent layout left to diverge from and nothing
/// to refuse. pnpm 12.4.1 installs this fixture identically.
#[test]
fn a_yarn_pnp_project_installs_a_node_modules_tree() {
    let dir = tmpdir("pnp");
    write(
        &dir,
        &[
            ("package.json", r#"{"name":"t","version":"1.0.0"}"#),
            ("yarn.lock", "# yarn lockfile v1\n"),
            (".yarnrc.yml", "nodeLinker: pnp\n"),
        ],
    );
    let (out, code) = run(&dir, &["install"]);
    assert_eq!(
        code, 0,
        "PnP config must not divert the install; got:\n{out}"
    );
    assert!(
        !out.contains("ERR_NUB_PNP_UNSUPPORTED"),
        "the retired PnP refusal must not come back; got:\n{out}"
    );
    assert!(
        dir.join("node_modules").exists(),
        "a node_modules tree is what nub installs; got:\n{out}"
    );
    // Nub's own lockfile, not yarn's, and not a PnP runtime file.
    assert!(dir.join("nub.lock").exists(), "got:\n{out}");
    assert!(!dir.join(".pnp.cjs").exists(), "got:\n{out}");
}

/// An OPTIONAL unresolvable dependency is skipped and the install proceeds,
/// matching every incumbent's tolerance of a missing optional.
///
/// The carve-out survived the engine change; only its wording moved. Was
/// `WARN_NUB_LOCKFILE_UNSUPPORTED_SOURCE` from aube's reader. The engine says
/// it the way pnpm 12.4.1 does, verbatim, on a successful run.
#[test]
fn install_proceeds_on_an_optional_unresolvable_spec() {
    let dir = tmpdir("opt");
    write(
        &dir,
        &[
            (
                "package.json",
                r#"{"name":"t","version":"1.0.0","optionalDependencies":{"foo":"exotic:bar"}}"#,
            ),
            ("yarn.lock", "# yarn lockfile v1\n"),
        ],
    );
    let (out, code) = run(&dir, &["install"]);
    assert_eq!(
        code, 0,
        "an optional unresolvable dep must not abort; got:\n{out}"
    );
    // Flattened for the same wrapping reason as the abort case above.
    let flat: String = out.split_whitespace().collect();
    assert!(
        flat.contains("foo@exotic:bar") && flat.contains("Excludingitfrominstallation"),
        "the optional skip should name the dep and say it was excluded; got:\n{out}"
    );
    // Skipped, not quietly installed from somewhere.
    assert!(
        !dir.join("node_modules").join("foo").exists(),
        "the unresolvable optional must not land in the tree; got:\n{out}"
    );
}

/// `nub ci` is the headless install, so it needs nub's OWN lockfile. A foreign
/// one lying beside it is not a substitute — it is not read, so the project has
/// no lockfile at all as far as `ci` is concerned, and `ci` refuses rather than
/// resolving fresh.
///
/// Was: aube's bun reader drove `ci` straight off `bun.lock`, so this fixture
/// installed successfully. Reaching a green `nub ci` here now takes a `nub
/// install` (or `nub pm migrate`) first.
///
/// Only the code is asserted, deliberately. The message it carries still names
/// `pnpm-lock.yaml`, a file that never exists in a project nub installed — a
/// rebranding gap in the engine's copy, not a contract to pin.
#[test]
fn ci_refuses_when_only_a_foreign_lockfile_is_present() {
    let dir = tmpdir("bun-ci");
    write(
        &dir,
        &[
            ("package.json", UNRESOLVABLE_PKG),
            ("bun.lock", BUN_LOCK_PINNING_FOO),
        ],
    );
    let (out, code) = run(&dir, &["ci"]);
    assert_ne!(
        code, 0,
        "`nub ci` must not install off another package manager's lockfile; got:\n{out}"
    );
    assert!(
        out.contains("ERR_NUB_NO_LOCKFILE"),
        "`nub ci` should report the missing lockfile; got:\n{out}"
    );
    assert!(!out.contains("ERR_PNPM_"), "brand leak; got:\n{out}");
    assert!(
        !dir.join("node_modules").exists(),
        "`nub ci` must refuse before writing node_modules"
    );
}
