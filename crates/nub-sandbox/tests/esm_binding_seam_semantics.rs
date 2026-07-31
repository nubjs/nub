//! The ES-module half of the build jail's realpath repair, measured on every platform.
//!
//! WHY IT EXISTS SEPARATELY FROM `realpath_shim_semantics`. That file simulates the jail at
//! `fs.lstatSync`, which is the layer the shim's own walk reads, so it measures the walk. It
//! cannot measure the ESM defect at all: `internal/modules/esm/resolve.js` DESTRUCTURES
//! `realpathSync` off `fs` at module-load time on 18.19-24.16, 25.x and 26.0, and on 25.7-26.0
//! the resolver lives in the V8 startup snapshot — so the property replacement never reaches it
//! and every relative or bare `import` in a lifecycle script dies EPERM. The repair is a patch on
//! `process.binding('fs')`, which `lib/fs.js` looks up at CALL time. This file measures that.
//!
//! THE SIMULATION, AND WHY IT REFUSES TWO FUNCTIONS RATHER THAN ONE. `sim.cjs` refuses at the
//! BINDING, which is the layer the AppContainer actually refuses — patching `fs.lstatSync` would
//! model only a JS-visible break and leave the binding seam untested against anything. It refuses
//! `binding.lstat` for a strict ancestor of the granted root AND `binding.realpath` outright. The
//! second half is not optional: an earlier measurement of this design concluded that
//! `fs.realpathSync.native` and `fs.promises.realpath` needed no repair, because the simulation it
//! ran against had only broken `binding.lstat`. A simulation that models LESS than the OS refuses
//! produces a confident false pass, and that one nearly shipped.
//!
//! WHAT IS AND IS NOT A CONTROL HERE. Only cells whose `control` arm FAILS prove anything. Two
//! families qualify and both are asserted: the ESM import cells (ESM's `finalizeResolution` keeps
//! its own realpath cache, which is cold) and the `binding.realpath` cells. CommonJS resolution
//! deliberately carries NO assertion in the confined arms — a `--require`-delivered simulation
//! installs its patch only after its own module has been resolved, by which point Node's CJS
//! `realpathCache` already holds every ancestor, so a CJS require succeeds even with no repair
//! whatsoever. That is an artifact of the delivery channel, not a property of the jail, and
//! asserting on it would be asserting on the harness. The real jail refuses from process start;
//! `realpath_shim_semantics` covers the CJS walk directly.
//!
//! RESIDUAL, stated because a green run here does not cover it: a dependency that registers its
//! own `module.register` loader gets a real worker thread with its own realm, which no `--import`
//! preload reaches, so its resolutions still EPERM. Measured 0 of 354 corpus packages doing this.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The jail, simulated at the layer the OS refuses. `--require` so it is installed before
/// `esm/resolve.js` captures its binding, which is what makes the simulation faithful to a
/// refusal that is already in force when the process starts.
const SIM: &str = r#"
const path = require('path');
const b = process.binding('fs');
const ROOT = require('fs').realpathSync(path.resolve(process.env.SIM_ROOT));
const LOG = process.env.SIM_LOG;
const strip = (p) => (typeof p === 'string' && p.startsWith('\\\\?\\') ? p.slice(4) : p);
function isStrictAncestor(p) {
  const c = path.resolve(strip(String(p)));
  return ROOT.length > c.length && ROOT.startsWith(c) &&
    (ROOT[c.length] === path.sep || c.endsWith(path.sep));
}
function note(what) { if (LOG) { try { require('fs').appendFileSync(LOG, what + '\n'); } catch {} } }
function refuse(call, p) {
  const e = new Error('EPERM: operation not permitted, ' + call + " '" + p + "'");
  e.code = 'EPERM'; e.errno = -1; e.syscall = call; e.path = String(p); return e;
}
const origLstat = b.lstat;
b.lstat = function (p, bigint, req, ctxOrThrow) {
  if (isStrictAncestor(p)) {
    note('lstat ' + p);
    if (ctxOrThrow !== null && typeof ctxOrThrow === 'object') {
      ctxOrThrow.errno = -1; ctxOrThrow.syscall = 'lstat'; ctxOrThrow.path = String(p);
      return undefined;
    }
    throw refuse('lstat', p);
  }
  return origLstat.call(this, p, bigint, req, ctxOrThrow);
};
b.realpath = function (p, encoding, reqOrPromises, ctx) {
  note('realpath ' + p);
  const e = refuse('realpath', p);
  if (typeof reqOrPromises === 'symbol') return Promise.reject(e);
  if (reqOrPromises !== null && typeof reqOrPromises === 'object') {
    process.nextTick(() => reqOrPromises.oncomplete(e));
    return undefined;
  }
  if (ctx !== null && typeof ctx === 'object') {
    ctx.errno = -1; ctx.syscall = 'realpath'; ctx.path = String(p);
    return undefined;
  }
  throw e;
};
"#;

/// One ESM entry reporting every seam, each cell naming a VERSION or a path so a wrong answer is
/// distinguishable from a right one by CONTENT. `foo`'s private dependency is `bar@2.0.0` through
/// a store-cell link; the unrelated `bar@1.0.0` at the top level is where a link-path walk lands.
const SCRIPT: &str = r#"
import { createRequire } from 'node:module';
const out = [];
const cell = (name, fn) => {
  try { out.push(name + '=' + fn()); }
  catch (e) { out.push(name + '=ERR:' + (e.code || e.message)); }
};
cell('rel', () => createRequire(import.meta.url)('./rel.cjs').v);
const probe = await import('foo/probe.js');
const res = await probe.cells();
console.log([...out, ...res].join(' | '));
"#;

/// The seam probes live INSIDE `foo`'s store cell, because that is the only place they
/// discriminate: asked from the project root, `bar` legitimately IS the top-level `bar@1.0.0`.
/// THE NAMED IMPORTS ARE THE DISCRIMINATOR, and without them this file measures nothing.
///
/// A builtin's ESM named exports are snapshotted when its facade is FIRST instantiated, and the
/// preload's own `import fs from "node:fs"` instantiates `node:fs` before it assigns anything —
/// so `realpathSync` here is the PRE-PATCH function on every Node version, while `fs.realpathSync`
/// is the patched property. That makes the two cells a clean isolation of the layers: `snapfs`
/// reaches `binding.lstat` and `snapnative` reaches `binding.realpath`, neither of which the
/// fs-layer replacement touches, so both fail unless the binding seam is installed. Measured
/// EPERM-vs-ok against the fs-layer-only stamp on 20.19, 22.15 and 26.5.
///
/// The `esm`/`rreq` cells are the PRODUCT assertion rather than the isolation: on 24.17+ and
/// 26.1+ upstream restored the late property read (nodejs/node#62835), so there the fs-layer
/// stamp resolves ESM on its own and those cells cannot tell the layers apart.
const PROBE: &str = r#"
import { realpathSync } from 'node:fs';
import { createRequire } from 'node:module';
import { v } from 'bar';
import fs from 'node:fs';
const req = createRequire(import.meta.url);
const ver = (p) => /bar@2\.0\.0/.test(p) ? '2.0.0' : /bar@1\.0\.0/.test(p) ? '1.0.0' : 'other';
export async function cells() {
  const out = ['esm=' + v, 'snapshotted=' + (realpathSync !== fs.realpathSync)];
  const cell = (name, fn) => {
    try { out.push(name + '=' + fn()); }
    catch (e) { out.push(name + '=ERR:' + (e.code || e.message)); }
  };
  const here = req.resolve('./probe.js');
  const same = (got) => got === here ? 'ok' : 'moved:' + got;
  cell('rreq', () => ver(req.resolve('bar')));
  cell('meta', () => typeof import.meta.resolve === 'function' ? ver(import.meta.resolve('bar')) : 'n/a');
  cell('rpjs', () => same(fs.realpathSync(here)));
  cell('snapfs', () => same(realpathSync(here)));
  cell('snapnative', () => same(realpathSync.native(here)));
  try { out.push('rppromise=' + same(await fs.promises.realpath(here))); }
  catch (e) { out.push('rppromise=ERR:' + (e.code || e.message)); }
  return out;
}
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

/// nub's default `Isolated` layout, ESM-flavoured: each package in its own store cell, wired to
/// its dependencies by symlink.
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
            "{\"name\":\"bar\",\"version\":\"0\",\"type\":\"module\",\"exports\":\"./index.js\"}\n",
        );
    }
    let foo_cell = store.join("foo@1.0.0").join("node_modules").join("foo");
    write(&foo_cell.join("probe.js"), PROBE);
    write(
        &foo_cell.join("package.json"),
        "{\"name\":\"foo\",\"version\":\"1.0.0\",\"type\":\"module\",\"exports\":{\
         \"./probe.js\":\"./probe.js\"}}\n",
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
    write(&root.join("script.mjs"), SCRIPT);
    write(&root.join("sim.cjs"), SIM);
}

struct Arm {
    stdout: String,
    refusals: Vec<String>,
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

/// One arm. `simulate` installs the jail condition; `extra` carries whatever repair is under
/// test. The refusal log is read back so an arm can prove the condition actually fired.
fn run(root: &Path, simulate: bool, extra: &str) -> Arm {
    let log = root.join("refusals.log");
    let _ = std::fs::remove_file(&log);
    let mut options = String::new();
    if simulate {
        options.push_str("--require ./sim.cjs");
    }
    if !extra.is_empty() {
        if !options.is_empty() {
            options.push(' ');
        }
        options.push_str(extra);
    }

    let mut cmd = Command::new("node");
    cmd.arg("script.mjs").current_dir(root);
    // Cleared rather than left ambient: an inherited NODE_OPTIONS would make every arm measure
    // whatever the developer's shell happens to carry.
    if options.is_empty() {
        cmd.env_remove("NODE_OPTIONS");
    } else {
        cmd.env("NODE_OPTIONS", &options);
    }
    cmd.env("SIM_ROOT", root).env("SIM_LOG", &log);
    // The shim's force seam, which is what lets this file run off Windows. It carries the
    // fixture's own realpath'd root, which the real stamp gets from the jail's grants.
    if extra.is_empty() {
        cmd.env_remove("__NUB_JAIL_REALPATH_SHIM_FORCE");
    } else {
        cmd.env("__NUB_JAIL_REALPATH_SHIM_FORCE", root);
    }
    let out = cmd.output().expect("run node");
    Arm {
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        refusals: std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect(),
        ok: out.status.success(),
    }
}

/// The shipping `NODE_OPTIONS` term, taken from the shipping function rather than restated — a
/// test carrying its own copy of the string goes green on a repair the product does not deliver.
fn shim(root: &Path) -> String {
    nub_sandbox::realpath_shim_node_options(std::slice::from_ref(&root.to_path_buf()))
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    // CANONICALISED, and that is load-bearing: on macOS `/tmp` is a symlink, so a fixture rooted
    // at the un-resolved path configures the simulation with ancestors Node never walks and the
    // treatment arm meets no refusal at all. That produced a green false pass once already.
    let root = std::fs::canonicalize(tmp.path()).unwrap_or_else(|_| tmp.path().to_path_buf());
    build_fixture(&root);
    (tmp, root)
}

fn have_node() -> bool {
    Command::new("node").arg("-v").output().is_ok()
}

#[test]
fn the_binding_seam_repairs_esm_resolution_without_moving_it() {
    if !have_node() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let (_tmp, root) = fixture();

    // PREMISE: with realpath working the fixture resolves, and `foo` reaches its OWN `bar@2.0.0`.
    // Without this an arm that failed because the fixture was malformed would read as a finding.
    let truth = run(&root, false, "");
    assert!(
        truth.ok && truth.cell("esm") == "2.0.0",
        "premise: unconfined Node must reach foo's OWN bar@2.0.0 ({:?})",
        truth.stdout
    );

    // POSITIVE CONTROL. Unrepaired, an ESM import must die LOUDLY — no output, non-zero exit.
    // Everything below is measuring nothing if this arm succeeds.
    let control = run(&root, true, "");
    assert!(
        !control.ok && control.cell("esm") == "ABSENT",
        "positive control: with the binding refusal in force and no repair, an ESM import must \
         die before user code ({:?})",
        control.stdout
    );

    let repaired = run(&root, true, &shim(&root));

    // THE REPAIR MET THE CONDITION. Without this the arm below could be green because nothing
    // was ever refused.
    assert!(
        !repaired.refusals.is_empty(),
        "the treatment arm must actually HIT the simulated refusal, or it measures nothing at \
         all — refusals={:?} stdout={:?}",
        repaired.refusals,
        repaired.stdout
    );
    assert!(
        repaired.ok,
        "repaired arm exited non-zero ({:?})",
        repaired.stdout
    );

    // THE CELLS THAT DECIDE WHETHER THIS MAY SHIP. Each names a version, so binding the
    // unrelated top-level `bar@1.0.0` — the silent wrong answer `--preserve-symlinks` produces,
    // and the reason that flag stays unwired — is distinguishable from a working repair.
    for (name, want) in [("esm", "2.0.0"), ("rel", "rel"), ("rreq", "2.0.0")] {
        assert_eq!(
            repaired.cell(name),
            want,
            "seam {name} must resolve like unconfined Node. `1.0.0` means a store-cell link was \
             skipped and the wrong version bound ({:?})",
            repaired.stdout
        );
    }
    // `import.meta.resolve` is absent below 20.6 with no flag; `n/a` records that rather than
    // asserting a version floor this file does not otherwise care about.
    let meta = repaired.cell("meta");
    assert!(
        meta == "2.0.0" || meta == "n/a",
        "import.meta.resolve must not answer the link path ({meta})"
    );
}

/// THE TEST THAT ISOLATES THE BINDING SEAM FROM THE FS-LAYER REPLACEMENT, on any Node version.
///
/// Every other assertion in this file can be satisfied by the fs-layer stamp alone on 24.17+ and
/// 26.1+, because upstream restored the resolver's late property read there — so on a modern
/// developer machine they go green with the binding seam deleted, which is how this file was
/// first written and why it proved nothing. These two cells cannot: they call bindings captured
/// BEFORE the preload assigned anything, so they reach `binding.lstat` and `binding.realpath`
/// directly and are repaired only at that layer.
#[test]
fn the_seam_repairs_bindings_captured_before_the_preload() {
    if !have_node() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let (_tmp, root) = fixture();

    let repaired = run(&root, true, &shim(&root));
    assert!(
        !repaired.refusals.is_empty(),
        "the arm must actually hit the simulated refusal ({:?})",
        repaired.refusals
    );
    // PREMISE, asserted in the TREATMENT arm because that is the only arm where it can hold: with
    // no preload there is nothing to diverge from. If the named export and the patched property
    // were the same object, both cells below would be measuring the fs-layer twice.
    assert_eq!(
        repaired.cell("snapshotted"),
        "true",
        "premise: under the stamp the builtin's ESM named export must be a DIFFERENT function \
         object from the patched property ({:?})",
        repaired.stdout
    );
    for (name, layer) in [
        ("snapfs", "binding.lstat"),
        ("snapnative", "binding.realpath"),
    ] {
        assert_eq!(
            repaired.cell(name),
            "ok",
            "{name} reaches {layer}, which the fs-layer replacement does not touch — an EPERM \
             here means the binding seam is absent or mis-scoped ({:?})",
            repaired.stdout
        );
    }
    // The fs-layer half, asserted in the same arm so that dropping it in favour of the binding
    // patch alone — which an earlier reading of this design proposed — reads as a regression.
    for name in ["rpjs", "rppromise"] {
        assert_eq!(
            repaired.cell(name),
            "ok",
            "{name} is served by the fs-layer replacement; the seam is an ADDITION to it, not a \
             swap ({:?})",
            repaired.stdout
        );
    }
}

/// The scoping rule, and the reason it is not simply "tolerate every refused ancestor". The
/// tolerance fires only for the two argument shapes Node's realpath walk uses; an ordinary
/// `fs.lstatSync` of the same refused path must still throw, or the seam has silently changed
/// `fs.lstatSync` semantics for every caller in the confined process.
#[test]
fn an_ordinary_lstat_of_a_refused_ancestor_still_throws() {
    if !have_node() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let (_tmp, root) = fixture();
    let ancestor = root.parent().expect("the fixture root has a parent");

    // Paths arrive by environment rather than baked into the source: a Windows fixture path
    // carries backslashes, and escaping them into a JS literal is the kind of quoting bug that
    // turns a real refusal into a `ENOENT` nobody notices.
    std::fs::write(
        root.join("scope.cjs"),
        "const fs=require('fs');\n\
         const A=process.env.PROBE_ANCESTOR, R=process.env.PROBE_ROOT;\n\
         try { const s=fs.lstatSync(A); console.log('lstat='+(s.isDirectory()?'dir':'other')); }\n\
         catch (e) { console.log('lstat=ERR:'+e.code); }\n\
         try { console.log('realpath='+(fs.realpathSync(R)===R?'ok':'moved')); }\n\
         catch (e) { console.log('realpath=ERR:'+e.code); }\n",
    )
    .unwrap();

    let out = Command::new("node")
        .arg("scope.cjs")
        .current_dir(&root)
        .env("SIM_ROOT", &root)
        .env("PROBE_ANCESTOR", ancestor)
        .env("PROBE_ROOT", &root)
        .env("__NUB_JAIL_REALPATH_SHIM_FORCE", &root)
        .env(
            "NODE_OPTIONS",
            format!("--require ./sim.cjs {}", shim(&root)),
        )
        .output()
        .expect("run node");
    let text = String::from_utf8_lossy(&out.stdout);

    assert!(
        text.contains("lstat=ERR:EPERM"),
        "a direct fs.lstatSync of a refused ancestor must still throw — the walk's tolerance is \
         scoped to its own argument shapes, not to the path ({text})"
    );
    assert!(
        text.contains("realpath=ok"),
        "while realpath of a granted path, whose walk crosses that same ancestor, must succeed \
         ({text})"
    );
}

/// The property that makes stamping this unconditionally safe: with nothing refused, the binding
/// wrappers must be indistinguishable from stock Node. This is the case where the backend's
/// ancestor-ACE repair DID land and the tolerance never fires.
#[test]
fn the_seam_is_inert_when_nothing_is_refused() {
    if !have_node() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let (_tmp, root) = fixture();
    let baseline = run(&root, false, "");
    let shimmed = run(&root, false, &shim(&root));
    // Cell by cell, and deliberately NOT over raw stdout: `snapshotted` reports whether the
    // preload replaced the property, so it differs between these arms BY CONSTRUCTION. It is a
    // harness observable for the isolation test above, not a behaviour, and comparing it here
    // would assert that the stamp does nothing at all rather than that it changes no ANSWER.
    for name in [
        "rel",
        "esm",
        "rreq",
        "meta",
        "rpjs",
        "snapfs",
        "snapnative",
        "rppromise",
    ] {
        assert_eq!(
            shimmed.cell(name),
            baseline.cell(name),
            "with nothing refused, cell {name} must be indistinguishable from stock Node \
             (shimmed={:?} baseline={:?})",
            shimmed.stdout,
            baseline.stdout
        );
    }
    assert!(shimmed.ok && baseline.ok, "both arms must exit zero");
}
