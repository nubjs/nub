//! Resolution-correctness harness (A34-ROOT).
//!
//! Runs Node's module-resolution test subset under BOTH the exact Node binary
//! nub resolves (`nub node which`, the passthrough baseline — equivalent to
//! `nub --node`) and under `nub` itself (augmented: TS hook, tsconfig paths,
//! extensionless probing, package clobbering). It then asserts nub matches Node
//! — any test Node passes that nub fails is an augmented-mode DIVERGENCE that
//! would break real Node code, and is a bug unless explicitly documented.
//!
//! This is the proof-of-correctness gate for the resolver items (A34, A35, D4,
//! A26): fix them, then prove parity here. Methodology + the current divergence
//! list live in wiki/research/resolution-conformance.md.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn suite_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/node-suite/test")
}

/// The exact Node binary nub resolves — the passthrough baseline, so the
/// comparison is apples-to-apples (same Node, augmented vs not).
fn baseline_node(nub: &Path) -> Option<PathBuf> {
    let out = Command::new(nub).args(["node", "which"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then(|| PathBuf::from(p))
}

/// Resolution-relevant test files: ESM/CJS resolution, specifiers, extensionless
/// probing, package exports/imports, self-reference, legacy main. Excludes the
/// `module-hooks` API tests (nub itself uses those hooks, so they're not a
/// resolution-correctness signal) and `--expose-*` internal-flag tests.
fn is_resolution_test(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let relevant = [
        "resolve",
        "specifier",
        "extensionless",
        "exports",
        "imports",
        "self-ref",
        "legacymainresolve",
        "module-resolution",
        "esm-cjs",
        "cjs-esm",
    ];
    let excluded = ["hook", "expose", "loader-mock", "permission"];
    relevant.iter().any(|k| n.contains(k)) && !excluded.iter().any(|k| n.contains(k))
}

fn has_internal_flags(test_path: &Path) -> bool {
    let content = std::fs::read_to_string(test_path).unwrap_or_default();
    let header: String = content.lines().take(20).collect::<Vec<_>>().join("\n");
    header.contains("--expose-internals")
        || header.contains("--allow-natives-syntax")
        || header.contains("--expose-externalize-string")
        || header.contains("--expose-gc")
}

fn discover(suite: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for sub in ["es-module", "parallel"] {
        let dir = suite.join(sub);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let is_src = name.ends_with(".mjs") || name.ends_with(".js");
            if is_src && is_resolution_test(name) && !has_internal_flags(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Run a test file under `bin`, return whether it exited 0. stdout/stderr are
/// discarded — we compare exit codes (the suite's own pass/fail contract).
fn passes(bin: &Path, test: &Path, suite: &Path) -> bool {
    Command::new(bin)
        .arg(test)
        .current_dir(suite)
        .env("NODE_TEST_KNOWN_GLOBALS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Accepted augmented-mode divergences — verified NOT to be resolution bugs.
/// Each was traced to nub's default webstorage augmentation (the
/// `--experimental-webstorage` flag plus nub's warning suppression), not its
/// resolver: every one passes under `node --import=<preload>` (nub's resolution
/// hooks active) and fails under `node --experimental-webstorage` (the flag,
/// hooks absent). They assert on exact stderr / expected warnings / re-spawned
/// child flags, which that augmentation perturbs. So nub's thin
/// resolver-over-Node matches Node across the resolution subset — zero genuine
/// resolution divergences. See wiki/research/resolution-conformance.md.
/// Format: (test path relative to the suite, reason).
const KNOWN_DIVERGENCES: &[(&str, &str)] = &[
    (
        "es-module/test-esm-cjs-named-error.mjs",
        "webstorage augmentation, not resolution",
    ),
    (
        "es-module/test-esm-exports-deprecations.mjs",
        "webstorage augmentation, not resolution",
    ),
    (
        "es-module/test-esm-imports-deprecations.mjs",
        "webstorage augmentation, not resolution",
    ),
    (
        "es-module/test-esm-extensionless-esm-and-wasm.mjs",
        "webstorage augmentation, not resolution",
    ),
    (
        "es-module/test-require-module-cycle-esm-cjs-esm.js",
        "webstorage augmentation, not resolution",
    ),
    (
        "es-module/test-require-module-cycle-esm-cjs-esm-esm.js",
        "webstorage augmentation, not resolution",
    ),
    (
        "es-module/test-require-module-cycle-esm-esm-cjs-esm-esm.js",
        "webstorage augmentation, not resolution",
    ),
];

/// Resolution-parity corpus — discovers the resolution-relevant Node-suite tests
/// and runs each twice (augmented + baseline Node) to compare. Like
/// [`node_compat_suite`], this is a CI-scale gate, not a unit test, so it is
/// `#[ignore]` and excluded from the default `cargo test`. Run it explicitly:
///   cargo test -p nub-cli --test resolution_compat -- --ignored --nocapture
/// or via the CI `compat` job. Requires the tests/node-suite submodule.
#[test]
#[ignore = "resolution-parity corpus (double-spawns the node-suite subset) — run via `cargo test -p nub-cli --test resolution_compat -- --ignored` or the CI compat job"]
fn resolution_parity() {
    let suite = suite_dir();
    // Fail LOUDLY when the suite is absent — a silent `return;` let a missing
    // submodule masquerade as a passing resolution gate. CI initializes the
    // tests/node-suite submodule; locally run
    // `git submodule update --init --depth 1 tests/node-suite`.
    assert!(
        suite.exists(),
        "resolution_compat: suite missing at {suite:?}. The resolution-parity gate cannot run. \
         Initialize the submodule: `git submodule update --init --depth 1 tests/node-suite`. \
         (Refusing to skip silently — a vacuous pass would hide resolver divergences.)"
    );
    let nub = nub_binary();
    // No baseline Node means the augmented-vs-passthrough comparison is
    // impossible — that's a broken harness/environment, not a pass. Panic so it
    // can't read as green.
    let node = baseline_node(&nub).unwrap_or_else(|| {
        panic!(
            "resolution_compat: `{} node which` resolved no baseline Node, so the \
             augmented-vs-passthrough parity comparison cannot run. Ensure a Node is on PATH \
             and the nub binary built. (Refusing to skip silently.)",
            nub.display()
        )
    });

    let tests = discover(&suite);
    assert!(
        tests.len() >= 30,
        "expected to discover a meaningful resolution subset, found {}",
        tests.len()
    );

    let mut parity = 0usize;
    let mut baseline_skipped = 0usize;
    let mut nub_more_permissive = Vec::new();
    let mut divergences = Vec::new();

    for test in &tests {
        let rel = test
            .strip_prefix(&suite)
            .unwrap_or(test)
            .to_string_lossy()
            .to_string();
        let node_ok = passes(&node, test, &suite);
        let nub_ok = passes(&nub, test, &suite);
        match (node_ok, nub_ok) {
            // Node can't run it standalone (needs setup we don't provide) — not a
            // valid parity signal.
            (false, false) => baseline_skipped += 1,
            // nub runs something Node rejects standalone — more permissive, not a
            // regression. Noted, not asserted.
            (false, true) => nub_more_permissive.push(rel),
            (true, true) => parity += 1,
            // Node passes, nub fails — the divergence we care about.
            (true, false) => divergences.push(rel),
        }
    }

    eprintln!(
        "\n=== Resolution conformance (A34-ROOT): {parity} parity, {baseline_skipped} baseline-skipped, \
         {} nub-more-permissive, {} divergence(s) of {} discovered ===",
        nub_more_permissive.len(),
        divergences.len(),
        tests.len()
    );
    for d in &divergences {
        match KNOWN_DIVERGENCES.iter().find(|entry| entry.0 == d.as_str()) {
            Some((_, reason)) => eprintln!("  divergence (known): {d} — {reason}"),
            None => eprintln!("  DIVERGENCE (undocumented): {d}"),
        }
    }
    for p in &nub_more_permissive {
        eprintln!("  nub-more-permissive: {p}");
    }

    let undocumented: Vec<&String> = divergences
        .iter()
        .filter(|d| !KNOWN_DIVERGENCES.iter().any(|entry| entry.0 == d.as_str()))
        .collect();
    assert!(
        undocumented.is_empty(),
        "{} undocumented resolution divergence(s) (Node passes, nub fails) — each breaks real Node code: {undocumented:?}",
        undocumented.len()
    );
}

fn ts_resolution_fixture() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/fixtures/ts-resolution")
}

/// TS-resolution conformance — the resolver features the node-suite corpus above
/// can NEVER exercise, because Node can't run TypeScript. The corpus validates
/// that nub's resolver matches Node on plain ESM/CJS; this validates the
/// TS-specific resolution nub adds: tsconfig `paths`, extensionless `.ts`, the
/// `.js`→`.ts` emit-convention swap, and a CJS `require()` of an alias from a
/// `.cts` parent. There is no Node baseline (Node would just error on the TS), so
/// parity is with tsc/tsx, encoded as the expected resolved output. Fast (a few
/// spawns), so unlike the corpus this is NOT `#[ignore]`d. See
/// wiki/research/resolution-conformance.md.
#[test]
fn ts_resolution_conformance() {
    let nub = nub_binary();
    let fixture = ts_resolution_fixture();
    assert!(
        fixture.exists(),
        "ts-resolution fixture missing at {fixture:?} — the TS-resolution conformance section cannot run"
    );

    let run = |entry: &str| -> String {
        let out = Command::new(&nub)
            .arg(entry)
            .current_dir(&fixture)
            .stdin(Stdio::null())
            .output()
            .expect("spawn nub");
        assert!(
            out.status.success(),
            "{entry} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // import: tsconfig `paths` alias + extensionless `.ts` + `.js`→`.ts` emit swap.
    assert_eq!(
        run("main.ts"),
        "alias-ok extless-ok swap-ok",
        "tsconfig path / extensionless / .js→.ts swap must all resolve via import (parity with tsc/tsx)"
    );
    // require: a tsconfig-paths alias from a `.cts` (CommonJS-TS) parent.
    assert_eq!(
        run("cjsmain.cts"),
        "require:alias-ok",
        "require() of a tsconfig-paths alias from a TS parent must resolve (parity with tsc/tsx)"
    );
}
