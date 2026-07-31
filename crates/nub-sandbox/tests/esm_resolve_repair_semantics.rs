//! The ES-module half of the build jail's resolution repair, measured on every platform.
//!
//! WHY IT EXISTS SEPARATELY FROM `realpath_shim_semantics`. That repair replaces
//! `fs.realpathSync`, and CommonJS picks it up because `helpers.js`'s `toRealPath` reads the
//! property at call time. `internal/modules/esm/resolve.js` does not: on 18.19-24.16, 25.x and
//! 26.0 it DESTRUCTURES the binding at module-load time, before any `--import` preload exists, so
//! the ESM resolver keeps calling the jail's refused realpath and every relative or bare `import`
//! in a lifecycle script dies EPERM. `--preserve-symlinks` is the only lever that stops those
//! calls, and on its own it silently binds a DIFFERENT version of a dependency — which is exactly
//! what `windows_realpath_node_options` was disqualified for. This file measures the claim that
//! makes the flag usable anyway: that the realpath can be put BACK at every seam the flag
//! disarmed, so the answer returns to the no-flag control's.
//!
//! THE FOUR ARMS, and the two of them that are controls rather than treatments:
//!
//! | arm | what it is |
//! | --- | --- |
//! | `control` | no jail, no flags — the truth every other arm is read against |
//! | `shipped` | today's stamp; the POSITIVE CONTROL that the ESM defect is real |
//! | `flag` | `--preserve-symlinks` with no repair; the NEGATIVE CONTROL that the fixture can tell a wrong answer from a right one |
//! | `repair` | the shipping term under test |
//!
//! A `repair` arm that passes beside a `flag` arm that also passes proves nothing — the fixture
//! would not be discriminating. Both controls are asserted, not merely printed.
//!
//! THE SEAMS ARE THE POINT. `--preserve-symlinks` removes the realpath from `Module._findPath`
//! AND from `finalizeResolution`, and those are reached by different callers: `require.resolve`
//! and `createRequire` go through `Module._resolveFilename`, a DIFFERENT function from the
//! `resolveForCJSWithHooks` a resolve hook sees, and a `.node` addon's path reaches
//! `process.dlopen` through that same function. Each gets its own cell, because a repair that
//! covers one and misses another returns a wrong module with no error at all.
//!
//! RESIDUAL, stated because a green run here does not cover it: the compat-tier arm exercises the
//! `module.register` loader's REPAIR but not its TOLERANCE. `--require` preloads do not run on the
//! loader thread, so the simulated refusal cannot be installed there; only a real AppContainer
//! refuses that thread's `lstat`. That half is measured on Windows CI.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The simulated jail, byte-for-byte the shape `realpath_shim_semantics` uses: a strict ancestor
/// of the granted root is refused, and Node's own `realpathSync` — which reaches `lstat` through
/// an internal binding no preload can patch — is refused outright. `--require` so it lands before
/// `esm/resolve.js` captures the binding, which is what makes the simulation faithful.
const SIM: &str = r#"
const fs = require('fs');
const path = require('path');
const ROOT = fs.realpathSync(path.resolve(process.env.SIM_ROOT));
const origLstat = fs.lstatSync.bind(fs);
fs.lstatSync = function (p, o) {
  const c = typeof p === 'string' ? path.resolve(p) : null;
  if (c && ROOT.length > c.length && ROOT.startsWith(c) &&
      (ROOT[c.length] === path.sep || c.endsWith(path.sep))) {
    const e = new Error("EPERM: operation not permitted, lstat '" + p + "'");
    e.code = 'EPERM'; e.syscall = 'lstat'; e.path = p; throw e;
  }
  return origLstat(p, o);
};
const boom = () => { const e = new Error('EPERM: operation not permitted, lstat'); e.code = 'EPERM'; throw e; };
fs.realpathSync = boom; fs.realpathSync.native = boom;
"#;

/// Forces the compat-tier code path on ANY host: with the sync surface hidden the shim must fall
/// back to `module.register`, which is otherwise unreachable above Node 22.15. Its own preload
/// rather than part of [`SIM`], because the loader arms deliberately run without the simulation.
const HIDE_SYNC_HOOKS: &str = "delete require('module').registerHooks;\n";

/// One ESM entry point reporting every seam. `foo`'s private dependency is `bar@2.0.0`; the
/// unrelated `bar@1.0.0` at the top level is where a link-path walk lands, so every cell that
/// says `2.0.0` says "this seam resolved like unconfined Node".
const SCRIPT: &str = r#"
import { createRequire } from 'node:module';
const out = [];
try { out.push('rel=ok:' + createRequire(import.meta.url)('./rel.cjs').v); }
catch (e) { out.push('rel=ERR:' + (e.code || e.message)); }
const probe = await import('foo/probe.js');
console.log([...out, ...probe.cells].join(' | '));
"#;

/// The seam probes live INSIDE `foo`'s store cell, because that is the only place they
/// discriminate: asked from the project root, `bar` legitimately IS the top-level `bar@1.0.0`.
const PROBE: &str = r#"
import { createRequire } from 'node:module';
import { v } from 'bar';
const req = createRequire(import.meta.url);
const ver = (p) => /bar@2\.0\.0/.test(p) ? '2.0.0' : /bar@1\.0\.0/.test(p) ? '1.0.0' : 'other:' + p;
const cells = ['esm=' + v];
const cell = (name, fn) => { try { cells.push(name + '=' + fn()); } catch (e) { cells.push(name + '=ERR:' + (e.code || e.message)); } };
cell('rreq', () => ver(req.resolve('bar')));
cell('meta', () => typeof import.meta.resolve === 'function' ? ver(import.meta.resolve('bar')) : 'n/a');
cell('addon', () => {
  const orig = process.dlopen;
  let seen = null;
  process.dlopen = (mod, filename) => { seen = filename; const e = new Error('x'); e.code = 'STOP'; throw e; };
  try { req('bar/probe.node'); } catch (e) { if (e.code !== 'STOP') throw e; } finally { process.dlopen = orig; }
  return ver(seen);
});
export { cells };
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

/// nub's default `Isolated` layout, ESM-flavoured: each package in its own store cell, wired to
/// its dependencies by symlink. `bar` exports a `.node` path as well, so the addon seam resolves
/// through package `exports` the way a real prebuilt addon does.
fn build_fixture(root: &Path) {
    let nm = root.join("node_modules");
    let store = nm.join(".aube");
    for (dep, version) in [("bar@2.0.0", "2.0.0"), ("bar@1.0.0", "1.0.0")] {
        let cell = store.join(dep).join("node_modules").join("bar");
        write(
            &cell.join("index.js"),
            &format!("export const v='{version}';\n"),
        );
        write(
            &cell.join("package.json"),
            "{\"name\":\"bar\",\"version\":\"0\",\"type\":\"module\",\"exports\":{\
             \".\":\"./index.js\",\"./probe.node\":\"./probe.node\"}}\n",
        );
        // Never dlopen'd — the arm intercepts `process.dlopen` and reports the path it was
        // handed, which is the seam. Real addon bytes would make this a per-platform build.
        write(&cell.join("probe.node"), "");
    }
    let foo = store.join("foo@1.0.0").join("node_modules").join("foo");
    write(
        &foo.join("index.js"),
        "import { v } from 'bar';\nexport const barVersion = v;\n",
    );
    write(&foo.join("probe.js"), PROBE);
    write(
        &foo.join("package.json"),
        "{\"name\":\"foo\",\"version\":\"1.0.0\",\"type\":\"module\",\"exports\":{\
         \".\":\"./index.js\",\"./probe.js\":\"./probe.js\"}}\n",
    );
    link(
        "../../bar@2.0.0/node_modules/bar",
        &store.join("foo@1.0.0").join("node_modules").join("bar"),
    );
    link(".aube/foo@1.0.0/node_modules/foo", &nm.join("foo"));
    link(".aube/bar@1.0.0/node_modules/bar", &nm.join("bar"));

    write(&root.join("rel.cjs"), "module.exports={v:'rel'};\n");
    write(
        &root.join("package.json"),
        "{\"name\":\"fx\",\"type\":\"module\"}\n",
    );
    write(&root.join("script.js"), SCRIPT);
    write(&root.join("sim.cjs"), SIM);
    write(&root.join("hide.cjs"), HIDE_SYNC_HOOKS);
}

struct Arm {
    stdout: String,
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

fn run(root: &Path, simulate: bool, extra: &str, hide_sync_hooks: bool) -> Arm {
    let mut options = String::new();
    if simulate {
        options.push_str("--require ./sim.cjs");
    }
    if hide_sync_hooks {
        if !options.is_empty() {
            options.push(' ');
        }
        options.push_str("--require ./hide.cjs");
    }
    if !extra.is_empty() {
        if !options.is_empty() {
            options.push(' ');
        }
        options.push_str(extra);
    }
    let mut cmd = Command::new("node");
    cmd.arg("script.js").current_dir(root);
    if options.is_empty() {
        cmd.env_remove("NODE_OPTIONS");
    } else {
        cmd.env("NODE_OPTIONS", &options);
    }
    cmd.env("SIM_ROOT", root);
    if extra.is_empty() {
        cmd.env_remove("__NUB_JAIL_REALPATH_SHIM_FORCE");
    } else {
        cmd.env("__NUB_JAIL_REALPATH_SHIM_FORCE", root);
    }
    let out = cmd.output().expect("run node");
    Arm {
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ok: out.status.success(),
    }
}

/// Taken from the shipping functions rather than restated: a test carrying its own copy of the
/// string goes green on a repair the product does not deliver.
fn shipped(root: &Path) -> String {
    nub_sandbox::realpath_shim_node_options(std::slice::from_ref(&root.to_path_buf()))
}

fn repair(root: &Path, needs_loader: bool) -> String {
    format!(
        "{} {}",
        shipped(root),
        nub_sandbox::esm_resolve_repair_node_options(
            std::slice::from_ref(&root.to_path_buf()),
            needs_loader
        )
    )
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    // Realpath'd because on macOS `/tmp` is itself a link: roots that did not intersect the paths
    // Node walks made an earlier differential pass without the denials ever firing.
    let root = std::fs::canonicalize(tmp.path()).unwrap_or_else(|_| tmp.path().to_path_buf());
    build_fixture(&root);
    (tmp, root)
}

fn have_node() -> bool {
    Command::new("node").arg("-v").output().is_ok()
}

/// Whether the ESM defect this term repairs actually exists on THIS host.
/// `fs.realpathSync` is observed by the ESM resolver at all.
///
/// Measured behaviourally, with an identity patch, rather than inferred from a version range,
/// because two independent facts move that range and only one of them is about versions.
/// nodejs/node#62835 turned the destructured capture into a late property read (24.17+, 26.1+, and
/// NOT v22.x); separately, 25.9 and 26.0 put the resolver in the V8 STARTUP SNAPSHOT, so it holds
/// the pre-patch function and no preload of any kind reaches it. Under the real jail that second
/// group still fails, because there the refusal comes from the OS rather than from a preload — so
/// asserting the positive control there would be asserting a property of this test harness, not of
/// the product. `false` skips that one assertion and leaves every other cell measured.
fn defect_is_present_on_this_host(root: &Path) -> bool {
    write(
        &root.join("idpatch.cjs"),
        "const fs=require('fs');const id=(p)=>p+'@@PATCHED';id.native=id;fs.realpathSync=id;\n",
    );
    // An identity patch makes every realpathed resolution point at a file that does not exist, so
    // "the import died" means the resolver OBSERVED the patch. Two channels, and the defect is
    // exactly the asymmetry between them: the simulation must be able to break the resolver
    // (`--require`, which is where a real jail's refusal already sits by the time Node starts),
    // and the shim's own channel must NOT be able to repair it (`--import`, which runs too late
    // when the binding was destructured at module-load time).
    //
    // `--preserve-symlinks-main` is load-bearing in BOTH probes and its absence made this control
    // lie: without it the `--require` patch also breaks `resolveMainPath`, so the process died
    // before reaching any import and every host read as "the sim works" — including the
    // snapshotted 25.9/26.0, where in fact no preload reaches the resolver at all.
    let by_require = !run(
        root,
        false,
        "--preserve-symlinks-main --require ./idpatch.cjs",
        false,
    )
    .ok;
    let by_import = !run(
        root,
        false,
        "--preserve-symlinks-main --import data:text/javascript,\
         import%20fs%20from%22node:fs%22;const%20id=(p)=>p+%22@@PATCHED%22;id.native=id;\
         fs.realpathSync=id;",
        false,
    )
    .ok;
    by_require && !by_import
}

/// Whether this host has the sync `module.registerHooks`, i.e. which tier the repair must be built
/// for. The product reads the same fact from a version probe (`has_sync_module_hooks`); the test
/// reads it from the interpreter it is about to run.
fn host_has_sync_hooks() -> bool {
    Command::new("node")
        .args([
            "-p",
            "typeof require('module').registerHooks === 'function'",
        ])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
}

/// The whole differential on one fixture: the defect, the hazard, and the repair.
#[test]
fn the_repair_restores_every_seam_the_flag_disarms() {
    if !have_node() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let (_tmp, root) = fixture();

    let control = run(&root, false, "", false);
    let shipped_arm = run(&root, true, &shipped(&root), false);
    let flag = run(
        &root,
        true,
        &format!("{} --preserve-symlinks", shipped(&root)),
        false,
    );
    // Parameterised from the interpreter under test, exactly as `build_jail.rs` parameterises it
    // from the interpreter it is about to confine.
    let repaired = run(&root, true, &repair(&root, !host_has_sync_hooks()), false);

    assert!(
        control.ok && control.cell("esm") == "2.0.0",
        "control: unconfined Node must reach foo's OWN bar@2.0.0 ({:?})",
        control.stdout
    );

    // POSITIVE CONTROL, on the hosts where the defect exists: today's stamp must leave an ESM
    // import DEAD, or the repair below is measuring nothing. Gated on the resolver's source form
    // rather than skipped wholesale, so a modern host still runs every other cell.
    if defect_is_present_on_this_host(&root) {
        assert!(
            !shipped_arm.ok && shipped_arm.cell("esm") == "ABSENT",
            "positive control: under today's stamp an ESM import must die LOUDLY on a Node that \
             captures realpathSync early ({:?})",
            shipped_arm.stdout
        );
    }

    // NEGATIVE CONTROL, and the reason the treatment below means anything: the flag alone answers
    // with the unrelated top-level copy and exits 0.
    assert_eq!(
        flag.cell("esm"),
        "1.0.0",
        "negative control: --preserve-symlinks alone must silently bind the WRONG version, or \
         this fixture cannot tell a repair from a no-op ({:?})",
        flag.stdout
    );
    assert!(
        flag.ok,
        "the hazard is SILENT — a throw would be recoverable and not disqualifying ({:?})",
        flag.stdout
    );

    assert!(
        repaired.ok,
        "repaired arm exited non-zero ({:?})",
        repaired.stdout
    );
    for (name, want) in [
        ("esm", "2.0.0"),
        ("rel", "ok:rel"),
        ("rreq", "2.0.0"),
        ("addon", "2.0.0"),
    ] {
        assert_eq!(
            repaired.cell(name),
            want,
            "seam {name} must resolve like the control, not like the flag ({:?})",
            repaired.stdout
        );
    }
    // `import.meta.resolve` is absent below 20.6 with no flag; `n/a` records that rather than
    // asserting a version floor this file does not otherwise care about.
    let meta = repaired.cell("meta");
    assert!(
        meta == "2.0.0" || meta == "n/a",
        "import.meta.resolve rides the resolve hook and must not answer the link path ({meta})"
    );
}

/// The compat tier (18.19-22.14), forced onto any host by hiding `module.registerHooks`. Run
/// WITHOUT the simulated refusal on purpose: the `module.register` loader thread is unreachable
/// from a `--require` preload, so what is measured here is that the loader puts the realpath BACK
/// (the wrong-version half), not that it tolerates a refusal (the Windows half).
#[test]
fn the_compat_tier_loader_repairs_the_same_seams() {
    if !have_node() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let (_tmp, root) = fixture();

    let flag = run(
        &root,
        false,
        &format!("{} --preserve-symlinks", shipped(&root)),
        true,
    );
    let repaired = run(&root, false, &repair(&root, true), true);

    assert_eq!(
        flag.cell("esm"),
        "1.0.0",
        "negative control ({:?})",
        flag.stdout
    );
    assert!(
        repaired.ok,
        "loader arm exited non-zero ({:?})",
        repaired.stdout
    );
    for (name, want) in [("esm", "2.0.0"), ("rreq", "2.0.0"), ("addon", "2.0.0")] {
        assert_eq!(
            repaired.cell(name),
            want,
            "seam {name} under the async loader ({:?})",
            repaired.stdout
        );
    }
}

/// A mis-gated loader must ABORT, never degrade. `needs_loader=false` on an interpreter with no
/// sync hooks leaves `--preserve-symlinks` stamped and nothing repairing ESM, which is the one
/// outcome worse than the defect this whole term fixes.
#[test]
fn a_missing_loader_aborts_rather_than_resolving_wrongly() {
    if !have_node() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let (_tmp, root) = fixture();
    let arm = run(&root, false, &repair(&root, false), true);
    assert!(
        !arm.ok && arm.cell("esm") == "ABSENT",
        "the shim must throw when it has neither hook surface ({:?})",
        arm.stdout
    );
}

#[test]
fn the_stamped_term_survives_the_node_options_whitespace_split() {
    let term = nub_sandbox::esm_resolve_repair_node_options(&[PathBuf::from("/a/b")], true);
    let mut parts = term.split(' ');
    assert_eq!(parts.next(), Some("--preserve-symlinks"));
    assert_eq!(parts.next(), Some("--import"));
    let url = parts.next().expect("a payload");
    assert!(
        parts.next().is_none(),
        "exactly three whitespace-free terms"
    );
    assert!(url.starts_with("data:text/javascript;base64,"));
    assert!(
        nub_sandbox::esm_resolve_repair_node_options(&[], true).is_empty(),
        "no roots means no tolerance to scope, so nothing is stamped"
    );
}
