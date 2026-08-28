//! CommonJS `require()` of a SCHEME-ONLY builtin — `node:test`, `node:sqlite`,
//! `node:sea`, `node:test/reporters` — must work under nub's runtime augmentation.
//!
//! Root cause (upstream Node, not nub): with ANY sync `module.registerHooks`
//! resolve hook registered, `resolveForCJSWithHooks` leaves its fast path and
//! recomputes the resolved URL as `convertCJSFilenameToURL(<normalized id>)`. The
//! old form of that helper keyed on `BuiltinModule.normalizeRequirableId(id)`,
//! which is false for a bare scheme-only id — `require("test")` is not legal — so
//! `test` matched neither the builtin branch nor `isAbsolute` and came back
//! VERBATIM as `"test"`. The load chain then rejected the default step's own
//! correct `{ format: "builtin", source: null }`, because `validateLoad` waives
//! the string-source requirement only for a url starting with `node:`. The user
//! saw `ERR_INVALID_RETURN_PROPERTY_VALUE` ("Expected a string, an ArrayBuffer, or
//! a TypedArray … but got null") thrown from inside nub's own load hook.
//!
//! Version band, measured per-release against plain Node with a pass-through hook:
//! broken on 22.15.0–22.17.1, 23.5.0–23.11.1 and 24.0.0–24.3.0; fixed in 22.18.0+,
//! 24.4.0+ and 25+, which rewrote the helper to strip any `node:` prefix and test
//! `canBeRequiredByUsers`. The 23.x line reached end-of-life without the backport,
//! and below 22.15/23.5 `module.registerHooks` does not exist, so the bug cannot
//! occur there. A REGULAR builtin was never affected. nub registers a resolve hook
//! unconditionally on the fast tier, so on that band the breakage was unconditional
//! for its users.
//!
//! Fix (`runtime/preload-common.cjs`, resolve hook): re-prefix a bare
//! scheme-only builtin id returned by the default resolve step, reproducing the
//! fixed helper's output. Provably a no-op on a fixed Node, where the url already
//! starts with `node:` and the guard cannot fire.
//!
//! Both tests assert behavior that holds on EVERY leg of the CI matrix (18.19 …
//! 26): the compat tier registers no sync resolve hook and so never had the bug,
//! and the fixture self-enumerates which scheme-only builtins the running Node
//! actually has. They spawn the real `nub` binary against whatever `node` is
//! first on PATH, and skip when none is usable.

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
    PathBuf::from(manifest).join("../../tests/fixtures/cjs-scheme-only-builtin")
}

/// `<version>` of the `node` first on PATH, for failure messages, or `None` when
/// no usable Node is present.
fn path_node_version() -> Option<String> {
    let out = Command::new("node").arg("--version").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run `nub` with `args` in `dir`, returning `(stdout, stderr, exit_code)`.
fn run_nub(dir: &PathBuf, args: &[&str]) -> (String, String, i32) {
    let output = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
    )
}

/// The regression: every scheme-only builtin this Node has must `require()`
/// cleanly, and the neighbours the fix must not disturb — a regular builtin, and
/// a project file whose basename collides with a scheme-only id — must still
/// resolve to themselves.
#[test]
fn scheme_only_builtins_require_cleanly_from_cjs() {
    let Some(version) = path_node_version() else {
        eprintln!("skipping cjs-scheme-only-builtin: no usable node on PATH");
        return;
    };
    let dir = fixture_dir();
    let (stdout, stderr, code) = run_nub(&dir, &["require-builtins.cjs"]);
    assert!(
        !stderr.contains("ERR_INVALID_RETURN_PROPERTY_VALUE"),
        "requiring a scheme-only builtin must not hit Node's bare-id load-hook \
         validation (node {version}); stderr={stderr:?}"
    );
    assert_eq!(
        code, 0,
        "require of the scheme-only builtins must exit 0 (node {version}); \
         stdout={stdout:?} stderr={stderr:?}"
    );
    // Positive control on the self-enumeration. Exit 0 plus a bare `OK` prefix is
    // also what an EMPTY list produces — `OK 0` would mean the fixture required
    // nothing at all and passed vacuously. Every Node from the 18.19 floor up has
    // at least `node:test`, so parse the count and require it to be non-zero.
    let required: usize = stdout
        .strip_prefix("OK ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "expected the fixture's `OK <count> <names>` line (node {version}); \
                 stdout={stdout:?} stderr={stderr:?}"
            )
        });
    assert!(
        required > 0,
        "the fixture must have actually required a scheme-only builtin — a count of \
         0 means the enumeration found none and the test proved nothing \
         (node {version}); stdout={stdout:?}"
    );
}

/// The reported symptom, end to end: Node's test runner over a CommonJS test file
/// that does `require("node:test")`. Before the fix the test FILE failed on the
/// broken band while the runner itself exited non-zero.
#[test]
fn node_test_runner_runs_a_cjs_test_file() {
    let Some(version) = path_node_version() else {
        eprintln!("skipping cjs-scheme-only-builtin runner: no usable node on PATH");
        return;
    };
    let dir = fixture_dir().join("runner");
    let (stdout, stderr, code) = run_nub(&dir, &["--test", "--test-reporter=tap"]);
    assert_eq!(
        code, 0,
        "`nub --test` over a CJS test file must exit 0 (node {version}); \
         stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("# pass 1") && stdout.contains("# fail 0"),
        "the CJS test file must run and pass (node {version}); stdout={stdout:?}"
    );
}
