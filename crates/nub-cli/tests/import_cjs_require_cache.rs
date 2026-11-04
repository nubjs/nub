//! #18 — `import()` of a CommonJS module that touches `require.cache` at load
//! time must not crash under nub's runtime augmentation.
//!
//! Root cause: nub's sync `module.registerHooks` LOAD hook made Node route an
//! `import()`-of-CJS (and its inner `require()`s) through the synchronous ESM
//! translator's `loadCJSModuleWithSpecialRequire`, whose hand-rolled `require`
//! lacks `.cache`/`.extensions`. CJS that runs `require.cache[...]` at module
//! eval (next's bundled `conf`: `delete require.cache[__filename]`) then crashed
//! with `TypeError: Cannot convert undefined or null to object`. Broken on Node
//! 22.15–~25; Node 26 repaired the special-require, so it was already fine there.
//!
//! Fix (`runtime/preload-common.cjs`, load hook): relabel a `commonjs` result as
//! `commonjs-sync` ON THE `import()`-OF-CJS PATH ONLY, so Node uses the sound
//! `loadCJSModuleWithModuleLoad` translator (real CJS `require` with `.cache`).
//! Two guards keep it surgical: (1) gate on the `import` load condition so plain
//! `require()` loads are untouched (a `require()`-loaded `.cjs` with ESM syntax
//! must still throw "Unexpected token 'export'"); (2) skip whenever a USER async
//! ESM loader or sync `registerHooks` hook is active, so a user loader's inner
//! `require()`s keep the native-CJS handoff instead of being routed through the
//! user's ESM resolve hook.
//!
//! These tests spawn the real `nub` binary against whatever `node` is first on
//! PATH (the CI matrix supplies the version per leg — the 24 and 22.15 legs are
//! the broken band; newer legs exercise the no-op path). When no usable Node is
//! found they skip — a local + CI signal, not a hard build-time dependency.

use std::path::PathBuf;
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn fixture_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    PathBuf::from(manifest).join("../../tests/fixtures/import-cjs-require-cache")
}

/// The major version of the `node` first on PATH, or `None` if none is usable.
/// The fix targets the whole 22.15+ fast tier; the major number is enough to
/// label which behavior band the run is in (`< 26` broken, `>= 26` no-op).
fn path_node_major() -> Option<u32> {
    let out = Command::new("node").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout);
    let v = v.trim().trim_start_matches('v');
    v.split('.').next()?.parse().ok()
}

/// Run `nub <file>` in the fixture dir against the PATH Node, optionally with
/// extra env (e.g. a user loader via `NODE_OPTIONS`). Returns
/// `(stdout, stderr, exit_code)`.
fn run_nub(file: &str, extra_env: &[(&str, &str)]) -> (String, String, i32) {
    let dir = fixture_dir();
    let mut cmd = Command::new(nub_binary());
    cmd.arg(dir.join(file)).current_dir(&dir);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
    )
}

/// The #18 regression: `import()` of a CJS module that reads `require.cache` at
/// load must run clean (real `require.cache`), not crash. Holds across the whole
/// fast tier — the broken band (22.15–25) where the relabel does the work, and
/// 26+ where it's a no-op (Node already repaired the special-require).
#[test]
fn import_of_cjs_touching_require_cache_runs_clean() {
    let Some(major) = path_node_major() else {
        eprintln!("skipping import-of-cjs-require-cache: no usable node on PATH");
        return;
    };
    // Below the fast-path floor the sync registerHooks load hook isn't used, so
    // the relabel never applies — not the band this regression guards.
    if major < 22 {
        eprintln!("skipping: PATH node major {major} is below the fast-tier floor");
        return;
    }
    let (stdout, stderr, code) = run_nub("entry.mjs", &[]);
    assert_eq!(
        code, 0,
        "import()-of-CJS-touching-require.cache must exit 0 (node major {major}); stderr={stderr}"
    );
    assert!(
        stdout.contains("loaded 42"),
        "expected the CJS module to load with a real require.cache (node major {major}); stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        !stderr.contains("Cannot convert undefined or null to object"),
        "the #18 require.cache crash must not surface (node major {major}); stderr={stderr:?}"
    );
}

/// Guard (1): the `import`-condition gate must leave plain `require()` loads
/// alone. A `require()` of a `.cjs` containing ESM syntax must still throw the
/// native "Unexpected token 'export'" — relabeling it to `commonjs-sync` would
/// make Node's sync translator accept it, swallowing the syntax error vanilla
/// Node raises.
#[test]
fn require_of_esm_syntax_cjs_still_throws() {
    let Some(major) = path_node_major() else {
        eprintln!("skipping require-of-esm-syntax-cjs: no usable node on PATH");
        return;
    };
    if major < 22 {
        eprintln!("skipping: PATH node major {major} is below the fast-tier floor");
        return;
    }
    let (stdout, stderr, code) = run_nub("require-esm-cjs.mjs", &[]);
    assert_ne!(
        code, 0,
        "require() of ESM-syntax .cjs must fail (node major {major}); stdout={stdout:?}"
    );
    assert!(
        stderr.contains("Unexpected token 'export'"),
        "the native syntax error must still surface — the relabel must not swallow it (node major {major}); stderr={stderr:?}"
    );
}

/// Counter-test (guard 2): with a USER async ESM loader active, the native-CJS
/// handoff must be preserved — the inner `require('node:assert')` of an
/// `import()`ed CJS module must resolve normally, NOT get routed through the
/// user's ESM resolve hook (which would crash). Asserted on Node 26+, where a
/// user async loader coexists cleanly with nub's sync hooks (earlier majors
/// carry a separate, pre-existing nub limitation for that coexistence that is
/// unrelated to this fix, so the relabel-decline is validated there instead by
/// the version-independent argv/register detection logic).
#[test]
fn user_async_loader_preserves_native_cjs_handoff() {
    let Some(major) = path_node_major() else {
        eprintln!("skipping user-async-loader counter-test: no usable node on PATH");
        return;
    };
    if major < 26 {
        eprintln!("skipping user-async-loader counter-test: needs node major >= 26 (got {major})");
        return;
    }
    let (stdout, stderr, code) = run_nub(
        "counter-entry.mjs",
        &[("NODE_OPTIONS", "--import ./user-loader.mjs")],
    );
    assert_eq!(
        code, 0,
        "with a user async loader active, import()-of-CJS must still run (node major {major}); stderr={stderr}"
    );
    assert!(
        stdout.contains("counter cjs-builtin-ok"),
        "the inner require('node:assert') must use the native handoff, not the user resolve hook (node major {major}); stdout={stdout:?} stderr={stderr:?}"
    );
}

/// #669 — the SECOND delivery channel for guard (2). The counter-test above sends
/// its loader as `--import` of a module that calls `module.register()`, so the
/// guard fires on the runtime detector and the flag scan is never exercised.
/// `--experimental-loader` registers natively — no `module.register()` call — and
/// `NODE_OPTIONS` flags are not hoisted into `process.execArgv`, so both of the
/// original channels were blind to it and the relabel ran against a live user
/// loader: Node then rejected `commonjs-sync` with a null source
/// (ERR_INVALID_RETURN_PROPERTY_VALUE). This is how OpenTelemetry's documented ESM
/// attach is delivered, which is what made it a crash on startup rather than a
/// corner case. Same fixture and same assertion as the counter-test — only the
/// channel differs, which is the whole point.
#[test]
fn user_loader_via_experimental_loader_flag_preserves_native_cjs_handoff() {
    let Some(major) = path_node_major() else {
        eprintln!("skipping experimental-loader channel: no usable node on PATH");
        return;
    };
    if major < 26 {
        eprintln!("skipping experimental-loader channel: needs node major >= 26 (got {major})");
        return;
    }
    let (stdout, stderr, code) = run_nub(
        "counter-entry.mjs",
        &[("NODE_OPTIONS", "--experimental-loader=./user-hooks.mjs")],
    );
    assert!(
        !stderr.contains("ERR_INVALID_RETURN_PROPERTY_VALUE"),
        "the relabel must decline for a NODE_OPTIONS --experimental-loader, not hand Node a null-source commonjs-sync (node major {major}); stderr={stderr:?}"
    );
    assert_eq!(
        code, 0,
        "with a user async loader delivered by --experimental-loader, import()-of-CJS must still run (node major {major}); stderr={stderr}"
    );
    assert!(
        stdout.contains("counter cjs-builtin-ok"),
        "the inner require('node:assert') must use the native handoff, not the user resolve hook (node major {major}); stdout={stdout:?} stderr={stderr:?}"
    );
}
