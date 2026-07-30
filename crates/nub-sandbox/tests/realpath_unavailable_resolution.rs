//! `--preserve-symlinks-main` alone does NOT make Node usable when realpath is unavailable.
//!
//! THE DECISION THIS SETTLES. The Windows build jail's AppContainer cannot perform `realpathSync`
//! — it opens every ancestor as a TARGET and bypass-traverse exempts intermediate components only
//! — so an unflagged confined `node` dies in `resolveMainPath` before a line of user code runs.
//! The only lever that stops Node calling realpath is a preserve-symlinks flag, and
//! `--preserve-symlinks` is disqualified for silently binding the wrong package version
//! (`preserve_symlinks_isolated_layout`, which also shows that hazard is the NON-main flag's).
//! That left exactly one candidate — `--preserve-symlinks-main` by itself — and this file is the
//! measurement that closes it out: main-only clears the entry point and nothing else, so every
//! `require()` still dies.
//!
//! Node decides this structurally, in `Module._findPath`:
//!
//! ```text
//! if (!isMain) {
//!   if (getOptionValue('--preserve-symlinks')) { filename = path.resolve(basePath); }
//!   else                                       { filename = toRealPath(basePath); }
//! } else if (getOptionValue('--preserve-symlinks-main')) { filename = path.resolve(basePath); }
//!   else                                                 { filename = toRealPath(basePath); }
//! ```
//!
//! Every NON-main resolution realpaths unless the non-main flag is set. There is no cache to fall
//! back on either: `toRealPath` memoises into a module-level map that only a SUCCESSFUL walk
//! populates, and under the jail no walk succeeds.
//!
//! WHY A SIMULATION AND NOT THE JAIL. The jail needs an AppContainer, which cannot be launched
//! over SSH (services session 0 has no window station) and so needs a CI runner. What fails there
//! is realpath AS A CALL — measured, not assumed: `fs.realpathSync` on a file the jail GRANTED
//! and can `readFileSync` in the same breath is also `EPERM ... lstat 'C:\'`. So making realpath
//! unavailable process-wide reproduces the jail's condition exactly, on any platform, in
//! milliseconds. The CONTROL arm is what licenses the substitution: with no flags this fixture
//! dies before user code, which is the real jail's observed failure. An arm that reached user
//! code would mean the simulation had stopped modelling the thing.
//!
//! Kept as a standing regression test rather than a one-off probe: if a future Node resolves
//! dependencies without realpath — or grants `GetFinalPathNameByHandleW` under an AppContainer —
//! the main-only arm starts passing its requires and the Windows realpath repair becomes
//! available again. This test is where that shows up.

use std::path::Path;
use std::process::Command;

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A project with both resolution shapes a lifecycle script uses: a relative file and a bare
/// specifier out of `node_modules`. No symlinks — this fixture is about realpath being refused,
/// not about what realpath would have resolved.
fn build_fixture(root: &Path) {
    write(&root.join("rel.js"), "module.exports={v:'rel'};\n");
    let dep = root.join("node_modules").join("dep");
    write(&dep.join("index.js"), "module.exports={v:'dep'};\n");
    write(
        &dep.join("package.json"),
        "{\"name\":\"dep\",\"version\":\"1.0.0\",\"main\":\"index.js\"}\n",
    );

    // The jail condition. `realpathSync` is refused for EVERY path under the AppContainer,
    // granted files included, so this throws unconditionally rather than modelling a path
    // predicate the kernel does not actually apply. `.native` is covered too: it is refused
    // there as well (its single `GetFinalPathNameByHandleW` needs more than the leaf handle the
    // jail allows), so leaving it live would let Node route around a failure the jail does not
    // let it route around.
    write(
        &root.join("shim.cjs"),
        "const fs=require('fs');\n\
         const boom=()=>{const e=new Error('EPERM: operation not permitted, lstat');\
         e.code='EPERM';throw e;};\n\
         fs.realpathSync=boom;fs.realpathSync.native=boom;\n",
    );

    write(
        &root.join("entry.js"),
        "console.log('entry:reached');\n\
         for(const [n,f] of [['require-relative',()=>require('./rel.js').v],\
                             ['require-bare',()=>require('dep').v]]){\n\
           try{console.log(n+'=OK '+f());}catch(e){console.log(n+'=ERR '+e.code);}\n\
         }\n",
    );
}

struct Run {
    stdout: String,
    ok: bool,
}

impl Run {
    fn reached_user_code(&self) -> bool {
        self.stdout.contains("entry:reached")
    }
    fn cell(&self, name: &str) -> String {
        self.stdout
            .lines()
            .find(|l| l.starts_with(&format!("{name}=")))
            .map(|l| l[name.len() + 1..].to_string())
            .unwrap_or_else(|| "ABSENT".to_string())
    }
}

/// Every arm carries the realpath-disabling preload; they differ ONLY in the resolution flags.
/// The preload runs before `resolveMainPath`, so the control arm's main path is already affected
/// — which is the point.
fn run_with_realpath_unavailable(root: &Path, flags: &[&str]) -> Run {
    let mut opts = String::from("--require ./shim.cjs");
    for f in flags {
        opts.push(' ');
        opts.push_str(f);
    }
    let out = Command::new("node")
        .arg("entry.js")
        .current_dir(root)
        .env("NODE_OPTIONS", &opts)
        .output()
        .expect("run node");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        ok: out.status.success(),
    }
}

#[test]
fn preserve_symlinks_main_alone_clears_the_entry_point_but_leaves_every_require_broken() {
    if Command::new("node").arg("-v").output().is_err() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    build_fixture(root);

    // Sanity: the fixture itself resolves when realpath works. Without this, an arm that failed
    // because the fixture was malformed would read as a finding about the flags.
    let unpatched = Command::new("node")
        .arg("entry.js")
        .current_dir(root)
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("run node");
    let unpatched = String::from_utf8_lossy(&unpatched.stdout).into_owned();
    assert!(
        unpatched.contains("require-relative=OK") && unpatched.contains("require-bare=OK"),
        "premise: with realpath working, both resolutions must succeed — otherwise the arms \
         below measure a broken fixture, not the flags (got {unpatched:?})"
    );

    let control = run_with_realpath_unavailable(root, &[]);
    let main_only = run_with_realpath_unavailable(root, &["--preserve-symlinks-main"]);
    let both =
        run_with_realpath_unavailable(root, &["--preserve-symlinks-main", "--preserve-symlinks"]);

    // THE CONTROL THAT LICENSES THE SIMULATION: unflagged, Node dies before user code, which is
    // exactly what the real AppContainer produces. If this arm ever reaches `entry:reached`, the
    // simulation has stopped modelling the jail and the other two arms mean nothing.
    assert!(
        !control.reached_user_code() && !control.ok,
        "control: with realpath unavailable and no flags, Node must die in main-path resolution \
         before user code — the real jail's observed failure. stdout={:?}",
        control.stdout
    );

    // The half main-only DOES fix.
    assert!(
        main_only.reached_user_code(),
        "--preserve-symlinks-main must clear resolveMainPath, so user code runs even though \
         realpath is refused. stdout={:?}",
        main_only.stdout
    );

    // The half it does NOT fix, which is the finding.
    assert_eq!(
        main_only.cell("require-relative"),
        "ERR EPERM",
        "main-only leaves `_findPath`'s non-main realpath in place, so a relative require still \
         dies. A pass here would mean the Windows realpath repair is available again — see this \
         file's module doc before acting on it. stdout={:?}",
        main_only.stdout
    );
    assert_eq!(
        main_only.cell("require-bare"),
        "ERR EPERM",
        "same for a bare specifier out of node_modules, which is what every lifecycle script \
         does. stdout={:?}",
        main_only.stdout
    );

    // The upper bound: the pair works, so the failure above is attributable to the MISSING
    // non-main flag and not to the preload, the fixture, or realpath being patched at all.
    assert_eq!(
        both.cell("require-relative"),
        "OK rel",
        "adding --preserve-symlinks must fix what main-only could not, or the arm above failed \
         for some reason other than the flag. stdout={:?}",
        both.stdout
    );
    assert_eq!(
        both.cell("require-bare"),
        "OK dep",
        "stdout={:?}",
        both.stdout
    );
}
