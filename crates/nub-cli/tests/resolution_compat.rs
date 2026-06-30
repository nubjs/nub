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
/// Format: (test path relative to the suite, reason).
///
/// Empty as of 2026-06-15. NOTE: nub again injects `--experimental-webstorage` on
/// the 22.4–24 band (always, so sessionStorage works out of the box — the maintainer,
/// 2026-06-15), so the webstorage-flag/warning perturbations that previously forced
/// entries here CAN resurface in exact-stderr / expected-warning / re-spawned-child-
/// flag assertions of the Node-suite resolution corpus. None are listed because
/// those are not RESOLUTION divergences; if the corpus surfaces one on the band,
/// classify it (webstorage perturbation vs genuine resolver bug) and add an entry
/// only for a NEW, verified non-resolution divergence.
/// See wiki/research/resolution-conformance.md.
const KNOWN_DIVERGENCES: &[(&str, &str)] = &[];

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

    // import: tsconfig `paths` alias + extensionless `.ts` + dotted filename extensionless `.ts` + `.js`→`.ts` emit swap.
    assert_eq!(
        run("main.ts"),
        "alias-ok extless-ok dotted-extless-ok swap-ok",
        "tsconfig path / extensionless / dotted extensionless / .js→.ts swap must all resolve via import (parity with tsc/tsx)"
    );
    // require: a tsconfig-paths alias from a `.cts` (CommonJS-TS) parent.
    assert_eq!(
        run("cjsmain.cts"),
        "require:alias-ok",
        "require() of a tsconfig-paths alias from a TS parent must resolve (parity with tsc/tsx)"
    );
}

/// Load-hook fidelity: importing a `data:` URL whose MIME maps to no module format
/// must surface Node's ERR_UNKNOWN_MODULE_FORMAT — same code, same message, and a
/// stack with NO nub preload frames — both augmented (`nub`) and on the baseline
/// Node nub resolves. The bug this guards: nub's fast-tier sync `module.registerHooks`
/// load hook used to return the default step's `format: null`, which Node's hook
/// validator (`validateFormat`) rejects with ERR_INVALID_RETURN_PROPERTY_VALUE before
/// the loader ever reaches the ERR_UNKNOWN_MODULE_FORMAT path — leaking nub's
/// `preload-common.cjs` frame into the user-visible stack. Differential, fast (two
/// spawns), so not `#[ignore]`d.
#[test]
fn data_url_unknown_format_matches_node() {
    let nub = nub_binary();
    let node = baseline_node(&nub)
        .expect("`nub node which` must resolve a baseline Node for the data:-URL fidelity diff");

    // Inline ESM that imports a data: URL with an unsupported MIME. `--input-type`
    // lets us drive both binaries with `--eval` and no fixture file.
    let src = "await import('data:application/x-unknown,hello');";
    let run = |bin: &Path| -> (String, bool) {
        let out = Command::new(bin)
            .args(["--input-type=module", "--eval", src])
            .stdin(Stdio::null())
            .output()
            .expect("spawn for data:-URL diff");
        // nub prints a one-line `» node …` provenance banner to stderr on some
        // invocations; strip any such line so the comparison is the error only.
        let stderr = String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter(|l| !l.trim_start().starts_with('»'))
            .collect::<Vec<_>>()
            .join("\n");
        (stderr, out.status.success())
    };

    let (node_err, node_ok) = run(&node);
    let (nub_err, nub_ok) = run(&nub);

    assert!(
        !node_ok,
        "baseline Node should reject the unknown data: format"
    );
    assert!(!nub_ok, "nub should reject the unknown data: format too");

    // Code + message: the exact ERR_UNKNOWN_MODULE_FORMAT Node emits.
    assert!(
        node_err.contains("ERR_UNKNOWN_MODULE_FORMAT"),
        "baseline Node must throw ERR_UNKNOWN_MODULE_FORMAT; got:\n{node_err}"
    );
    assert!(
        nub_err.contains("ERR_UNKNOWN_MODULE_FORMAT"),
        "nub must surface Node's ERR_UNKNOWN_MODULE_FORMAT, not ERR_INVALID_RETURN_PROPERTY_VALUE; got:\n{nub_err}"
    );
    assert!(
        nub_err.contains("Unknown module format: application/x-unknown"),
        "nub must reproduce Node's exact message; got:\n{nub_err}"
    );

    // Stack fidelity (issue 3): no nub preload frame may leak into the user-visible
    // stack for this error.
    for marker in ["preload-common", "transform-core", "/runtime/"] {
        assert!(
            !nub_err.contains(marker),
            "nub leaked an internal preload frame ({marker}) into the data:-URL error stack:\n{nub_err}"
        );
    }
}

/// Regression — async `module.register` loader coexisting with nub's sync hooks must NOT
/// crash with the async-loader sync stub. This is the FAITHFUL, FAST, deterministic guard
/// for the Next.js + Turbopack + Tailwind v4 crash, reduced to its essence: a user
/// `module.register(<async loader>)` whose own loader-module specifier Node resolves (and
/// loads) SYNCHRONOUSLY during registration — exactly what `@tailwindcss/node` does.
///
/// Root cause: nub's fast-tier sync `module.registerHooks` hooks force EVERY resolve/load
/// onto the synchronous chain. With a user async loader registered, Node's default
/// resolve/load step (`#resolveAndMaybeBlockOnLoaderThread` / `#loadAndMaybeBlockOn...`)
/// reaches the async-hooks proxy's `resolveSync`/`loadSync` — which on the affected Node
/// band (≤ ~22.16 / ~24.11) either is a stub throwing
/// `ERR_METHOD_NOT_IMPLEMENTED('resolveSync()'/'loadSync()')` or is ABSENT entirely
/// (a `TypeError: … is not a function`). Either kills the build. nub's hooks now detect
/// both shapes and recover (resolve via the parent CJS resolver / `file:`+builtin
/// passthrough; load by reading source from disk and deriving the format). Node 24.12+ /
/// 25.2+ / 26 implement these methods, so the stub never fires and the recovery is a
/// no-op there.
///
/// IMPORTANT: this is only a meaningful guard when run on an AFFECTED Node version
/// (e.g. 22.16 / 24.11). On a Node-fixed version it passes trivially (no bug to catch).
/// CI must run it on at least one broken-Node leg. Validated 2026-06-23: pre-fix EXIT 1
/// (crash) on 22.16.0 AND 24.11.0; post-fix EXIT 0 on both; no-op green on 24.12 / 26.
/// Single spawn, so NOT `#[ignore]`d.
#[test]
fn async_module_register_loader_sync_resolve_does_not_crash() {
    use std::fs;

    let nub = nub_binary();
    let tmp = std::env::temp_dir().join(format!("nub-async-loader-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // A trivial passthrough async ESM loader — the @tailwindcss/node shape.
    fs::write(
        tmp.join("loader.mjs"),
        "export async function resolve(s, c, n) { return n(s, c); }\n",
    )
    .unwrap();
    // Registering it resolves+loads loader.mjs SYNCHRONOUSLY through nub's sync hooks →
    // the async-loader stub on a broken Node. `writeSync` avoids any buffering ambiguity.
    fs::write(
        tmp.join("main.mjs"),
        "import { register } from 'node:module';\nimport { writeSync } from 'node:fs';\nregister('./loader.mjs', import.meta.url);\nwriteSync(1, 'OK\\n');\n",
    )
    .unwrap();

    let out = Command::new(&nub)
        .arg("main.mjs")
        .current_dir(&tmp)
        .stdin(Stdio::null())
        .output()
        .expect("spawn nub for the async-loader regression");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = fs::remove_dir_all(&tmp);

    assert!(
        !stderr.contains("ERR_METHOD_NOT_IMPLEMENTED")
            && !stderr.contains("resolveSync is not a function")
            && !stderr.contains("loadSync is not a function"),
        "nub leaked the async-loader sync stub — the Turbopack/Tailwind crash class regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        out.status.success() && stdout.contains("OK"),
        "nub crashed registering an async module loader (sync resolve/load into the async stub).\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// Heavy end-to-end twin of the fast guard above: a real, minimal Next 16 + Tailwind v4
/// Turbopack build — the in-the-wild manifestation. `#[ignore]`d because it scaffolds an
/// app, runs `nub install`, and runs a full `next build`; run via
/// `cargo test … -- --ignored` or the CI compat job. Like the fast guard, this is only
/// meaningful on an AFFECTED Node version.
#[test]
#[ignore = "heavy e2e: scaffolds a Next 16 + Tailwind v4 app, installs deps, and runs `next build` (Turbopack). Run via `cargo test -p nub-cli --test resolution_compat -- --ignored` or the CI compat job."]
fn next_turbopack_tailwind_build_does_not_crash() {
    use std::fs;

    let nub = nub_binary();
    let tmp = std::env::temp_dir().join(format!("nub-next-tw-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(tmp.join("app")).unwrap();

    // Minimal Next 16 App Router app whose ONLY non-default ingredient is Tailwind v4 —
    // the dependency that registers the async `module.register` loader. `app/globals.css`
    // with `@import "tailwindcss"` is what drives Turbopack's Node evaluation.
    fs::write(
        tmp.join("package.json"),
        r#"{
  "name": "nub-next-tw-regression",
  "private": true,
  "scripts": { "build": "next build" },
  "dependencies": {
    "next": "^16.1.1",
    "react": "^19",
    "react-dom": "^19",
    "tailwindcss": "^4.1.17",
    "@tailwindcss/postcss": "^4.1.17"
  }
}
"#,
    )
    .unwrap();
    fs::write(
        tmp.join("postcss.config.mjs"),
        "export default { plugins: { '@tailwindcss/postcss': {} } };\n",
    )
    .unwrap();
    fs::write(tmp.join("next.config.ts"), "export default {};\n").unwrap();
    fs::write(tmp.join("app/globals.css"), "@import \"tailwindcss\";\n").unwrap();
    fs::write(
        tmp.join("app/layout.tsx"),
        "import './globals.css';\nexport default function RootLayout({ children }: { children: React.ReactNode }) {\n  return (<html><body>{children}</body></html>);\n}\n",
    )
    .unwrap();
    fs::write(
        tmp.join("app/page.tsx"),
        "export default function Page() { return <main className=\"p-4\">ok</main>; }\n",
    )
    .unwrap();

    let install = Command::new(&nub)
        .arg("install")
        .current_dir(&tmp)
        .stdin(Stdio::null())
        .output()
        .expect("spawn nub install");
    assert!(
        install.status.success(),
        "nub install failed for the Next+Tailwind regression fixture:\n{}",
        String::from_utf8_lossy(&install.stderr)
    );

    let build = Command::new(&nub)
        .args(["run", "build"])
        .current_dir(&tmp)
        .stdin(Stdio::null())
        .output()
        .expect("spawn nub run build");
    let stdout = String::from_utf8_lossy(&build.stdout);
    let stderr = String::from_utf8_lossy(&build.stderr);

    let _ = fs::remove_dir_all(&tmp);

    assert!(
        !stderr.contains("ERR_METHOD_NOT_IMPLEMENTED")
            && !stdout.contains("ERR_METHOD_NOT_IMPLEMENTED"),
        "nub leaked the async-loader resolveSync() stub error — the Turbopack/Tailwind crash regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        build.status.success(),
        "`nub run build` failed for the Next 16 + Tailwind v4 Turbopack app.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
