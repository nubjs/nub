//! HTMLRewriter integration tests: run fixture scripts through the built `nub`
//! binary and assert the rewrite output. Exercises the Cloudflare-Workers-shape
//! global backed by the vendored WASM lol-html engine
//! (`runtime/html-rewriter-engine/`, lol-html compiled to WebAssembly with
//! Asyncify), which ships in nub's distribution — no native addon needed for
//! HTMLRewriter. Covers the API surface, async handlers (awaited mid-transform),
//! async-handler rejection, consumer cancel (incl. mid-suspend), the engine
//! leak-loop, and compat-mode absence.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push(format!("nub{}", std::env::consts::EXE_SUFFIX));
    path
}

fn fixture(file: &str) -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest)
        .join("../../tests/fixtures/html-rewriter")
        .join(file)
}

fn run(file: &str, extra_args: &[&str], env: &[(&str, &str)]) -> (String, String, i32) {
    let mut cmd = Command::new(nub_binary());
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.arg(fixture(file));
    for &(k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// The full surface: the global is present + non-enumerable, attribute/content
/// mutation, escaped-vs-raw insertion, element removal, document-end append,
/// doctype read, text rewriting, async handlers awaited mid-transform, streaming
/// over a Response, the non-Response TypeError, and the synchronous selector-error.
#[test]
fn rewrites_html_across_the_workers_api_surface() {
    let (stdout, stderr, code) = run("rewrite.mjs", &[], &[]);
    assert_eq!(code, 0, "fixture must exit 0\nstderr: {stderr}");

    // Attribute injection + setInnerContent.
    assert!(
        stdout.contains(r#"ATTR: <h1>Hi</h1><a href="/" rel="noopener">link</a>"#),
        "attribute/content rewrite wrong:\n{stdout}"
    );
    // Raw HTML inserted verbatim; text-mode insertion HTML-escaped.
    assert!(
        stdout.contains("CONTENT: <p>x<b>raw</b>&lt;i&gt;esc&lt;/i&gt;</p>"),
        "escaped-vs-raw insertion wrong:\n{stdout}"
    );
    // Doctype name is readable.
    assert!(
        stdout.contains("DOCTYPE: html"),
        "doctype name wrong:\n{stdout}"
    );
    // Element removed; document-end content appended.
    assert!(
        stdout.contains("REMOVE: <!DOCTYPE html><div>keep</div><!--end-->"),
        "remove + document-end append wrong:\n{stdout}"
    );
    // Text rewriting.
    assert!(
        stdout.contains("TEXT: <span>HELLO</span>"),
        "text rewrite wrong:\n{stdout}"
    );
    // Async (Promise-returning) handlers are awaited mid-transform (Asyncify):
    // the element handler awaits before setAttribute, the document-end handler
    // awaits before append, and both land in the output.
    assert!(
        stdout.contains(r#"ASYNC: <a href="/" data-async="1">x</a><!--async-end-->"#),
        "async handlers must be awaited mid-transform:\n{stdout}"
    );
    // Streaming over a Response yields the transformed body.
    assert!(
        stdout.contains("STREAM: <title>Streamed</title>"),
        "Response streaming wrong:\n{stdout}"
    );
    // Cloudflare-exact: a non-Response input throws a TypeError.
    assert!(
        stdout.contains("BADINPUT: true"),
        "non-Response transform input must throw:\n{stdout}"
    );
    // Invalid selector throws synchronously at .on().
    assert!(
        stdout.contains("BADSEL: true"),
        "invalid selector must throw:\n{stdout}"
    );

    assert!(
        stdout.contains("DONE"),
        "fixture did not run to completion:\n{stdout}"
    );
}

/// A rejecting async handler must propagate the ORIGINAL error (not the secondary
/// borrow/aliasing error the Asyncify rewind would surface), and a subsequent
/// transform on a fresh instance must still work. Guards the asyncify.js
/// rewind-on-reject patch.
#[test]
fn async_handler_rejection_propagates_original_error() {
    let (stdout, stderr, code) = run("async-reject.mjs", &[], &[]);
    assert_eq!(code, 0, "fixture must exit 0\nstderr: {stderr}");
    assert!(
        stdout.contains("REJECT_ORIGINAL: true"),
        "rejecting async handler must surface the ORIGINAL error:\n{stdout}"
    );
    assert!(
        stdout.contains("REJECT_RECOVERS: true"),
        "a fresh transform after a rejection must still work:\n{stdout}"
    );
    assert!(
        stdout.contains("DONE"),
        "fixture did not complete:\n{stdout}"
    );
}

/// Cancelling the transformed output stream must resolve cleanly — including when
/// an async handler is suspended mid-transform (the held WASM borrow is tolerated
/// and the engine is freed on resume). Guards the safeFree + cancelled-flag fix.
#[test]
fn consumer_cancel_resolves_even_mid_suspend() {
    let (stdout, stderr, code) = run("cancel.mjs", &[], &[]);
    assert_eq!(code, 0, "fixture must exit 0\nstderr: {stderr}");
    assert!(
        stdout.contains("CANCEL_PLAIN_RESOLVED: true"),
        "plain consumer cancel must resolve:\n{stdout}"
    );
    assert!(
        stdout.contains("CANCEL_MIDSUSPEND_RESOLVED: true"),
        "cancel during an in-flight async handler must resolve (not reject):\n{stdout}"
    );
    assert!(
        stdout.contains("DONE"),
        "fixture did not complete:\n{stdout}"
    );
}

/// The cancel-mid-suspend path must RECLAIM the engine: a pure cancel-mid-suspend
/// loop must neither crash (the pre-fix path OOB-crashed by ~1000 cycles) nor grow
/// RSS across a post-warmup measure window (a per-engine WASM leak would). This is
/// the residual-leak guard — cancel() must defer all freeing to cleanup() so it
/// frees exactly once, after the suspended write resumes and the borrow releases.
#[test]
fn cancel_mid_suspend_reclaims_the_engine() {
    // --expose-gc gives a stable RSS reading; nub forwards V8 flags to Node.
    let (stdout, stderr, code) = run("leak-loop.mjs", &["--expose-gc"], &[]);
    // exit 0 + reaching the markers means no "memory access out of bounds" crash.
    assert_eq!(
        code, 0,
        "fixture must exit 0 (no OOB crash)\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("CANCEL_MIDSUSPEND_NO_CRASH: true"),
        "cancel-mid-suspend loop must not crash:\n{stdout}"
    );
    assert!(
        stdout.contains("CANCEL_MIDSUSPEND_FLAT: true"),
        "cancel-mid-suspend must reclaim the engine (RSS flat across the window):\n{stdout}"
    );
    assert!(
        stdout.contains("DONE"),
        "fixture did not complete:\n{stdout}"
    );
}

/// Under `--node` (zero augmentation) the global is absent.
#[test]
fn absent_under_node_flag() {
    let (stdout, stderr, code) = run("compat-absent.mjs", &["--node"], &[]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("HTMLREWRITER: undefined"),
        "HTMLRewriter must be absent under --node:\n{stdout}"
    );
}

/// A truthy `NODE_COMPAT` is the persistent tree-wide augmentation opt-out — same
/// effect as `--node`.
#[test]
fn absent_under_node_compat_env() {
    let (stdout, stderr, code) = run("compat-absent.mjs", &[], &[("NODE_COMPAT", "1")]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("HTMLREWRITER: undefined"),
        "HTMLRewriter must be absent under NODE_COMPAT=1:\n{stdout}"
    );
}
