//! The Windows build jail's realpath repair, measured on every platform.
//!
//! WHAT IS BEING MEASURED, AND WHY IT IS NOT A WINDOWS-ONLY TEST. The repair is a port of
//! Microsoft's `PatchModuleResolutionLStat.ts` (`aspnet/JavaScriptServices#1102`): tolerate a
//! refused `lstat` only for a path that is a strict ancestor prefix of a granted root, only
//! inside a realpath walk, and assert it is a plain directory. Which paths the AppContainer
//! refuses is a Windows fact and needs a runner (`tests/win-bypass-traverse/`). Whether the
//! walk still reproduces Node's OWN resolution — symlinks followed, the per-component cache
//! honoured, the right version of a package bound — is not a Windows fact, and it is the half
//! most likely to break. So it is measured here, on the machines people develop on, against a
//! simulated jail.
//!
//! THE SIMULATION, AND THE CONTROL THAT LICENSES IT. `sim.cjs` reproduces exactly what run
//! 30506129146 measured: `lstat` of a strict ancestor of the granted root is EPERM, reads of
//! granted files succeed, and Node's own `realpathSync` — which reaches `lstat` through an
//! internal binding no preload can patch — is refused outright. The CONTROL arm is what makes
//! that substitution legitimate: with no repair, Node dies in `resolveMainPath` with the same
//! stack the real jail produced. An arm that reached user code would mean the simulation had
//! stopped modelling the jail.
//!
//! AND THE ACCOUNTING THAT STOPS A FALSE PASS. The first version of this differential passed
//! for the wrong reason: on macOS `/tmp` is a symlink, so the roots it configured never
//! intersected the `/private/tmp` paths Node actually walked, and the simulated denials never
//! fired at all. The fixture now realpaths its own root and the simulation RECORDS every denial,
//! so the treatment arm fails outright if the repair was never exercised. A green
//! differential whose treatment never met the condition is worse than no test.
//!
//! THE CELL THAT DECIDES WHETHER THIS MAY SHIP is the `iso=` cell of the first test,
//! against the same fixture shape as `preserve_symlinks_isolated_layout`: `--preserve-symlinks`
//! silently binds `bar@1.0.0` there and exits 0, which is what disqualified it. The repair must
//! answer `bar@2.0.0` — the unconfined truth — or it has reintroduced that hazard.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The simulated jail. Kept as a `--require` preload because `--require` runs BEFORE
/// `resolveMainPath`, so the control arm's main path is already affected — which is the point.
const SIM: &str = r#"
const fs = require('fs');
const path = require('path');
const ROOT = fs.realpathSync(path.resolve(process.env.SIM_ROOT));
const LOG = process.env.SIM_LOG;
const origLstat = fs.lstatSync.bind(fs);
function isStrictAncestor(p) {
  const c = path.resolve(p);
  return ROOT.length > c.length && ROOT.startsWith(c) &&
    (ROOT[c.length] === path.sep || c.endsWith(path.sep));
}
fs.lstatSync = function (p, o) {
  if (typeof p === 'string' && isStrictAncestor(p)) {
    if (LOG) { try { fs.appendFileSync(LOG, path.resolve(p) + '\n'); } catch {} }
    const e = new Error("EPERM: operation not permitted, lstat '" + p + "'");
    e.code = 'EPERM'; e.syscall = 'lstat'; e.path = p; throw e;
  }
  return origLstat(p, o);
};
const boom = () => {
  const e = new Error('EPERM: operation not permitted, lstat');
  e.code = 'EPERM'; throw e;
};
fs.realpathSync = boom; fs.realpathSync.native = boom;
"#;

/// Reports the three things a lifecycle script needs and one thing only a correct walk gets
/// right: a bare-specifier resolution through a store-cell link, a relative require, and
/// realpath's own answer.
const SCRIPT: &str = r#"
const out = [];
try { out.push('iso=bar@' + require('foo').barVersion); } catch (e) { out.push('iso=ERR:' + e.code); }
try { out.push('rel=' + require('./rel.js').v); } catch (e) { out.push('rel=ERR:' + e.code); }
try { out.push('rp=' + require('fs').realpathSync(__dirname + '/node_modules/foo/index.js')); }
catch (e) { out.push('rp=ERR:' + e.code); }
console.log(out.join(' | '));
"#;

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[cfg(unix)]
fn link(original: &str, link: &Path) {
    std::os::unix::fs::symlink(original, link).unwrap();
}

#[cfg(windows)]
fn link(original: &str, link: &Path) {
    // A directory symlink needs Developer Mode or elevation; a junction needs neither but wants
    // an absolute target. Resolved against the link's own parent so the fixture keeps one shape.
    let target = link.parent().unwrap().join(original);
    if std::os::windows::fs::symlink_dir(original, link).is_err() {
        let status = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(&target)
            .status()
            .expect("run mklink");
        assert!(status.success(), "could not link {}", link.display());
    }
}

/// nub's default `Isolated` layout: `foo`'s private dependency is `bar@2.0.0` inside its own
/// store cell, and an unrelated `bar@1.0.0` sits at the top level where a link-path walk lands.
fn build_fixture(root: &Path) {
    let nm = root.join("node_modules");
    let store = nm.join(".aube");
    for (dep, version) in [("bar@2.0.0", "2.0.0"), ("bar@1.0.0", "1.0.0")] {
        let cell = store.join(dep).join("node_modules").join("bar");
        write(
            &cell.join("index.js"),
            &format!("module.exports={{v:'{version}'}};\n"),
        );
        write(
            &cell.join("package.json"),
            &format!("{{\"name\":\"bar\",\"version\":\"{version}\"}}\n"),
        );
    }
    let foo_cell = store.join("foo@1.0.0").join("node_modules").join("foo");
    write(
        &foo_cell.join("index.js"),
        "module.exports={barVersion:require('bar').v};\n",
    );
    write(
        &foo_cell.join("package.json"),
        "{\"name\":\"foo\",\"version\":\"1.0.0\",\"main\":\"index.js\"}\n",
    );
    link(
        "../../bar@2.0.0/node_modules/bar",
        &store.join("foo@1.0.0").join("node_modules").join("bar"),
    );
    link(".aube/foo@1.0.0/node_modules/foo", &nm.join("foo"));
    link(".aube/bar@1.0.0/node_modules/bar", &nm.join("bar"));

    write(&root.join("rel.js"), "module.exports={v:'rel'};\n");
    write(&root.join("package.json"), "{\"name\":\"fx\"}\n");
    write(&root.join("script.js"), SCRIPT);
    write(&root.join("sim.cjs"), SIM);
}

struct Arm {
    stdout: String,
    denials: Vec<String>,
    ok: bool,
}

impl Arm {
    fn cell(&self, name: &str) -> String {
        self.stdout
            .split(" | ")
            .map(str::trim)
            .find(|c| c.starts_with(&format!("{name}=")))
            .map(|c| c[name.len() + 1..].to_string())
            .unwrap_or_else(|| "ABSENT".to_string())
    }
}

/// One arm. `simulate` installs the jail condition; `node_options` carries whatever repair is
/// under test. The denial log is read back so an arm can prove the condition actually fired.
fn run(root: &Path, simulate: bool, extra_options: &str) -> Arm {
    let log = root.join("denials.log");
    let _ = std::fs::remove_file(&log);
    let mut options = String::new();
    if simulate {
        options.push_str("--require ./sim.cjs");
    }
    if !extra_options.is_empty() {
        if !options.is_empty() {
            options.push(' ');
        }
        options.push_str(extra_options);
    }

    let mut cmd = Command::new("node");
    cmd.arg("script.js").current_dir(root);
    // Cleared rather than left ambient: an inherited NODE_OPTIONS would make every arm measure
    // whatever the developer's shell happens to carry.
    if options.is_empty() {
        cmd.env_remove("NODE_OPTIONS");
    } else {
        cmd.env("NODE_OPTIONS", &options);
    }
    cmd.env("SIM_ROOT", root).env("SIM_LOG", &log);
    // The shim's force seam. On Windows it is redundant (the shim installs on platform) and
    // additive-only; off Windows it is what lets this whole file run at all. It carries the
    // fixture's own root because a test fixture is not a jail grant.
    if !extra_options.is_empty() {
        cmd.env("__NUB_JAIL_REALPATH_SHIM_FORCE", root);
    } else {
        cmd.env_remove("__NUB_JAIL_REALPATH_SHIM_FORCE");
    }
    let out = cmd.output().expect("run node");
    Arm {
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        denials: std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect(),
        ok: out.status.success(),
    }
}

/// The shipping `NODE_OPTIONS` term, taken from the shipping function rather than restated — a
/// test that measured its own copy of the string would go green on a repair the product does not
/// deliver. The force seam is what activates the shim off Windows; it also carries the fixture's
/// realpath'd root, which the real stamp gets from the jail's grants.
fn shim_options(root: &Path) -> String {
    nub_sandbox::realpath_shim_node_options(&[root.to_path_buf()])
}

fn realpath_root(tmp: &Path) -> PathBuf {
    std::fs::canonicalize(tmp).unwrap_or_else(|_| tmp.to_path_buf())
}

#[test]
fn the_ported_lstat_tolerance_repairs_resolution_without_moving_it() {
    if Command::new("node").arg("-v").output().is_err() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // CANONICALISED, and that is load-bearing: on macOS `/tmp` is a symlink, so a fixture rooted
    // at the un-resolved path configures the simulation with ancestors Node never walks and the
    // treatment arm meets no denial at all. That produced a green false pass once already.
    let root = realpath_root(tmp.path());
    build_fixture(&root);

    // PREMISE: with realpath working, the fixture resolves. Without this, an arm that failed
    // because the fixture was malformed would read as a finding about the repair.
    let unconfined = run(&root, false, "");
    assert_eq!(
        unconfined.cell("iso"),
        "bar@2.0.0",
        "premise: a package must reach its OWN private dependency, not the unrelated top-level \
         copy. stdout={:?}",
        unconfined.stdout
    );

    // THE CONTROL THAT LICENSES THE SIMULATION: unrepaired, Node dies in main-path resolution
    // before user code — the real jail's observed failure (run 30506129146).
    let control = run(&root, true, "");
    assert!(
        !control.ok && control.stdout.is_empty(),
        "control: with the jail simulated and no repair, Node must die before user code. \
         stdout={:?}",
        control.stdout
    );

    let shim = run(&root, true, &shim_options(&root));

    // THE REPAIR DID MEET THE CONDITION. Without this the arm below could be green because
    // nothing was ever refused.
    assert!(
        !shim.denials.is_empty(),
        "the treatment arm must actually HIT the simulated refusal, or it measures nothing at \
         all — denials={:?} stdout={:?}",
        shim.denials,
        shim.stdout
    );

    assert_eq!(
        shim.cell("rel"),
        "rel",
        "the repair must make an ordinary relative require work. stdout={:?}",
        shim.stdout
    );
    assert_eq!(
        shim.cell("rp"),
        unconfined.cell("rp"),
        "and realpath must return the SAME answer it returns unconfined, not merely some answer. \
         stdout={:?}",
        shim.stdout
    );

    // THE CELL THAT DECIDES WHETHER THIS MAY SHIP.
    assert_eq!(
        shim.cell("iso"),
        "bar@2.0.0",
        "the repair resolves symlinks for real, so a store-cell link must reach its own private \
         dependency. `bar@1.0.0` here means the wrong-version hazard that disqualified \
         --preserve-symlinks has been reintroduced — see preserve_symlinks_isolated_layout. \
         stdout={:?}",
        shim.stdout
    );
}

#[test]
fn a_refusal_inside_the_granted_tree_stays_loud() {
    if Command::new("node").arg("-v").output().is_err() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = realpath_root(tmp.path());
    build_fixture(&root);

    // One variable off the arm above: the denial moves to a component INSIDE the granted root.
    // The tolerance is scoped to strict ancestors of a root precisely so this case rethrows —
    // a faked directory in here is what could bind a different package than the one requested,
    // which is the whole reason the predicate is not "tolerate any EPERM during realpath".
    write(
        &root.join("sim.cjs"),
        r#"
const fs = require('fs'); const path = require('path');
const TARGET = path.resolve(process.env.SIM_ROOT, 'node_modules');
const origLstat = fs.lstatSync.bind(fs);
fs.lstatSync = function (p, o) {
  if (typeof p === 'string' && path.resolve(p) === TARGET) {
    const e = new Error("EPERM: operation not permitted, lstat '" + p + "'");
    e.code = 'EPERM'; e.syscall = 'lstat'; e.path = p; throw e;
  }
  return origLstat(p, o);
};
const boom = () => { const e = new Error('EPERM'); e.code = 'EPERM'; throw e; };
fs.realpathSync = boom; fs.realpathSync.native = boom;
"#,
    );

    let arm = run(&root, true, &shim_options(&root));
    assert_eq!(
        arm.cell("iso"),
        "ERR:EPERM",
        "a refusal at `node_modules` — inside the granted tree — must propagate, not be absorbed \
         into an assumed plain directory. stdout={:?}",
        arm.stdout
    );
    assert_eq!(
        arm.cell("rel"),
        "rel",
        "and the refusal must be scoped to the path that carries it: a sibling resolution whose \
         walk never touches `node_modules` still works. stdout={:?}",
        arm.stdout
    );
}

#[test]
fn the_repair_is_inert_when_nothing_is_refused() {
    if Command::new("node").arg("-v").output().is_err() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = realpath_root(tmp.path());
    build_fixture(&root);

    // The property that makes stamping this unconditionally safe, and the one that covers the
    // case where the backend's ancestor repair DID land: with every `lstat` succeeding the
    // tolerance never fires, so the walk must agree with Node's own on every cell.
    let baseline = run(&root, false, "");
    let shimmed = run(&root, false, &shim_options(&root));
    assert_eq!(
        shimmed.stdout, baseline.stdout,
        "with nothing refused the repair must be indistinguishable from stock Node"
    );
}

#[test]
fn the_stamped_options_survive_node_options_whitespace_split() {
    let opts = nub_sandbox::realpath_shim_node_options(&[PathBuf::from("/a/b")]);
    // `NODE_OPTIONS` is split on whitespace and each term re-parsed by Node's own option reader,
    // so a payload containing whitespace does not fail loudly — it truncates into a half-loaded
    // preload or an option fragment Node aborts on. Base64's alphabet makes that unreachable.
    let terms: Vec<&str> = opts.split_whitespace().collect();
    assert_eq!(
        terms.len(),
        3,
        "exactly three terms: the main flag, `--import`, and its data: URL. got {opts}"
    );
    assert_eq!(terms[0], "--preserve-symlinks-main");
    assert_eq!(terms[1], "--import");
    assert!(
        terms[2].starts_with("data:text/javascript;base64,"),
        "the payload must be a base64 data: URL, got {}",
        terms[2]
    );
    assert!(
        nub_sandbox::realpath_shim_node_options(&[]).is_empty(),
        "no granted roots means nothing to scope the tolerance to, so nothing is stamped"
    );
}
