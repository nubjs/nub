//! A URL whose scheme is not `file:` is not nub's to claim.
//!
//! Both load hooks dispatch on the URL's extension, and every branch that dispatch
//! reaches reads the module's bytes off disk through `fileURLToPath`. A non-`file:`
//! URL that merely ENDED in a plain-JS extension entered those branches anyway, and
//! the conversion threw `ERR_INVALID_URL_SCHEME` — nub's TypeError standing in for
//! whatever Node was about to do. Two user-visible faces, one per test below:
//! Node's own `ERR_UNSUPPORTED_ESM_URL_SCHEME` was masked, and every
//! custom-protocol ESM loader died on a URL plain Node serves fine.
//!
//! Fix: `extname` (`runtime/transform-core.mjs`) reports an extension only for a
//! `file:` URL, so every other scheme falls through to the next hook untouched.
//! The gate is in the core, which is why one edit covers both tiers — the fast
//! tier's sync `module.registerHooks` hook (`runtime/preload-common.cjs`) and the
//! compat tier's async loader worker (`runtime/preload-async-hooks.mjs`) share it.
//!
//! These spawn the real `nub` binary against whatever `node` is first on PATH, and
//! skip when none is usable — a local + CI signal, not a build-time dependency.

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
    PathBuf::from(manifest).join("../../tests/fixtures/loader-nonfile-urls")
}

fn node_on_path() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Run `nub <file>` in the fixture dir. Returns `(stdout, stderr, exit_code)`.
fn run_nub(file: &str) -> (String, String, i32) {
    let dir = fixture_dir();
    let output = Command::new(nub_binary())
        .arg(dir.join(file))
        .current_dir(&dir)
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
    )
}

/// `import()` of an unsupported scheme must reject with Node's own
/// `ERR_UNSUPPORTED_ESM_URL_SCHEME`. Asserting the exact code is the point: the
/// import failed either way, but nub replaced the diagnosis with a TypeError about
/// `fileURLToPath`, so user code branching on `err.code` saw the wrong error.
#[test]
fn unsupported_scheme_keeps_nodes_own_error_code() {
    if !node_on_path() {
        eprintln!("skipping unsupported-scheme: no usable node on PATH");
        return;
    }
    let (stdout, stderr, code) = run_nub("unsupported-scheme.mjs");
    assert_eq!(
        code, 0,
        "the fixture handles its own rejection; stderr={stderr}"
    );
    assert!(
        stdout.contains("CODE ERR_UNSUPPORTED_ESM_URL_SCHEME"),
        "import('http://…/foo.js') must reject with Node's own code, not nub's \
         ERR_INVALID_URL_SCHEME; stdout={stdout:?} stderr={stderr:?}"
    );
}

/// A `module.register` loader serving its own protocol must work under nub exactly
/// as it does on plain Node. This is the higher-impact face: it covers every
/// virtual-module and http-style loader, none of which nub could run.
#[test]
fn custom_scheme_loader_serves_its_own_protocol() {
    if !node_on_path() {
        eprintln!("skipping custom-scheme loader: no usable node on PATH");
        return;
    }
    let (stdout, stderr, code) = run_nub("custom-scheme-entry.mjs");
    assert_eq!(
        code, 0,
        "a custom-protocol ESM loader must run under nub; stderr={stderr}"
    );
    assert!(
        stdout.contains("LOADED served-by-custom-loader"),
        "nub's load hook must defer `custom://virtual/index.js` to the user's \
         loader instead of claiming it on the `.js` suffix; \
         stdout={stdout:?} stderr={stderr:?}"
    );
}
