//! Integration tests: spawn `nub` against fixture projects and assert
//! stdout/stderr/exit-code.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn fixtures_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/fixtures")
}

fn run_nub(fixture: &str, file: &str) -> (String, String, i32) {
    run_nub_with_env(fixture, file, &[])
}

/// A unique per-invocation cache dir, so concurrent integration tests never share
/// the transpile cache / project-keyed webstorage under the ambient
/// `~/.cache/nub` — keeps the suite hermetic and removes the cross-test
/// shared-state vector at high `--test-threads`.
fn unique_test_cache() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "nub-itest-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

fn run_nub_with_env(fixture: &str, file: &str, env: &[(&str, &str)]) -> (String, String, i32) {
    let fixture_path = fixtures_dir().join(fixture);
    let mut cmd = Command::new(nub_binary());
    cmd.arg(fixture_path.join(file).to_str().unwrap())
        .current_dir(&fixture_path);
    // Isolate cache state per invocation unless the test sets its own
    // XDG_CACHE_HOME (e.g. the cache-atomicity test, which wins).
    if !env.iter().any(|(k, _)| *k == "XDG_CACHE_HOME") {
        cmd.env("XDG_CACHE_HOME", unique_test_cache());
    }
    for &(k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to spawn nub");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    (stdout, stderr, code)
}

/// Flagship provisioning, end-to-end through the binary: a project pinned (via
/// `.node-version`) to an EXACT version that is on neither PATH nor in nub's store
/// nor nvm → `nub <file>` downloads + installs it from nodejs.org (uv-style
/// progress on STDERR) and runs the script on it; a second run is cache-silent.
/// `nub run`/`exec` must NOT provision. `#[ignore]` — real network (~25MB),
/// isolated under a temp XDG_CACHE_HOME + an empty NVM_DIR so nothing leaks.
///   cargo test -p nub-cli --test integration provisions_ -- --ignored --nocapture
#[test]
#[ignore = "network: provisions a real Node (~25MB)"]
fn provisions_uncached_pinned_node_and_runs() {
    let work = unique_test_cache(); // a fresh temp dir
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    // 22.12.0 is a real published version, below the dev box's PATH node, unlikely
    // to be the active PATH/nvm version — so it forces the provision path.
    std::fs::write(proj.join(".node-version"), "22.12.0\n").unwrap();
    std::fs::write(proj.join("a.ts"), "console.log('pv:' + process.version);\n").unwrap();
    let cache = work.join("cache");
    let empty_nvm = work.join("empty-nvm");
    std::fs::create_dir_all(&empty_nvm).unwrap();

    let run = || {
        let out = Command::new(nub_binary())
            .arg(proj.join("a.ts"))
            .current_dir(&proj)
            .env("XDG_CACHE_HOME", &cache)
            .env("NVM_DIR", &empty_nvm)
            .output()
            .expect("spawn nub");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.code().unwrap_or(-1),
        )
    };

    // First run: installs + runs.
    let (stdout, stderr, code) = run();
    assert_eq!(code, 0, "first run must succeed: stderr={stderr}");
    assert!(
        stdout.contains("pv:v22.12.0"),
        "script ran on the provisioned 22.12.0: stdout={stdout:?}"
    );
    assert!(
        stderr.contains("Installing Node 22.12.0"),
        "uv-style install on stderr: stderr={stderr:?}"
    );
    assert!(
        stderr.contains("✓ Installed Node 22.12.0"),
        "install-complete on stderr: stderr={stderr:?}"
    );
    assert!(
        !stdout.contains("Installing"),
        "progress must never touch stdout: stdout={stdout:?}"
    );

    // Second run: cache hit — silent (the load-bearing invariant).
    let (stdout2, stderr2, code2) = run();
    assert_eq!(code2, 0);
    assert!(stdout2.contains("pv:v22.12.0"));
    assert!(
        stderr2.is_empty(),
        "a cached version must produce ZERO stderr: stderr={stderr2:?}"
    );

    let _ = std::fs::remove_dir_all(&work);
}

// ── Version-gated tests ─────────────────────────────────────────────
// A handful of integration tests assert behavior that is Node-VERSION-specific
// (not nub-specific) — e.g. detect-module's handling of a `.js` containing
// `import`. The suite runs across a Node matrix (see ci.yml + `make
// test-node-matrix`), so those tests must branch their assertion by the resolved
// Node version rather than be pinned to the dev box's. These helpers expose that
// version; gate with a logged reason, never a silent skip.

/// Parse a `vMAJOR.MINOR.PATCH[-tag]` string into a tuple.
fn parse_node_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.');
    let maj = it.next()?.parse().ok()?;
    let min = it.next()?.parse().ok()?;
    let pat = it.next()?.split(['-', '+']).next()?.parse().ok()?;
    Some((maj, min, pat))
}

/// The Node version `nub` resolves in this environment (the first `node` on PATH,
/// which is what the suite's PATH-prepend matrix selects). Resolved once.
fn target_node_version() -> (u32, u32, u32) {
    use std::sync::OnceLock;
    static V: OnceLock<(u32, u32, u32)> = OnceLock::new();
    *V.get_or_init(|| {
        // Prefer the exact binary nub would pick (`nub node which`); fall back to
        // PATH `node`. Either resolves the same version the spawned-nub tests use.
        // (`nub node which` prints the path to stdout, the explainer to stderr —
        // capturing stdout gives just the path.) Resolved FROM the fixtures dir so
        // the answer goes through the same pin-free project boundary
        // (tests/fixtures/package.json) the fixture tests run under — from the
        // crate dir the walk-up hits the repo-root engines.node (>=22.15.0) and
        // can report a store/nvm Node instead of the PATH-matrix Node the
        // fixture tests actually spawn.
        let node = Command::new(nub_binary())
            .args(["node", "which"])
            .current_dir(fixtures_dir())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "node".to_string());
        let out = Command::new(&node)
            .arg("--version")
            .output()
            .expect("`node --version` to resolve the target Node version");
        parse_node_version(String::from_utf8_lossy(&out.stdout).trim())
            .expect("parse `node --version` output")
    })
}

/// True when the resolved Node is at least `want`.
fn node_at_least(want: (u32, u32, u32)) -> bool {
    target_node_version() >= want
}

/// True when the target Node supports synchronous `require(esm)` — unflagged in
/// Node 22.12 and backported to 20.19 (18.x never got it; 21.x is EOL and didn't).
/// Below this line the compat tier's async loader-worker `load` hook can't serve a
/// `require()` routed through Node's synchronous ESM-translator special-require —
/// see wiki/research/compat-tier-cjs-entry-helpers.md.
fn node_has_require_esm() -> bool {
    let (maj, min, _) = target_node_version();
    maj >= 23 || (maj == 22 && min >= 12) || (maj == 20 && min >= 19)
}

#[test]
fn vanilla_ts_executes() {
    let (stdout, stderr, code) = run_nub("vanilla-ts", "main.ts");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("status=active"));
    assert!(stdout.contains("url=http://localhost:3000"));
    assert!(stdout.contains("OK"));
}

#[test]
fn type_only_import_erased() {
    let (stdout, stderr, code) = run_nub("vanilla-ts", "type-only-import.ts");
    assert_eq!(code, 0, "type-only import should run: {stderr}");
    assert!(
        stdout.contains("type-only:square is red"),
        "type used as value: {stdout}"
    );
    assert!(
        !stdout.contains("SIDE_EFFECT"),
        "type-only module must not be loaded at runtime: {stdout}"
    );
}

#[test]
fn using_and_await_using() {
    let (stdout, stderr, code) = run_nub("vanilla-ts", "using-syntax.ts");
    assert_eq!(code, 0, "using syntax should work: {stderr}");
    assert!(
        stdout.contains("sync:a.txt,b.txt"),
        "sync using block: {stdout}"
    );
    assert!(
        stdout.contains("close:b.txt\nclose:a.txt"),
        "dispose in reverse order: {stdout}"
    );
    assert!(stdout.contains("async:db"), "await using block: {stdout}");
    assert!(stdout.contains("disconnect:db"), "async dispose: {stdout}");
    assert!(stdout.contains("using:done"), "completed: {stdout}");
}

#[test]
fn stage3_decorators_error_clearly() {
    // KNOWN GAP: TC39 Stage 3 decorators (the default when experimentalDecorators
    // is not set — matching tsc) are not lowered by oxc, which passes the syntax
    // through verbatim with no error. Nub detects this and rejects with the
    // documented Option-A diagnostic (oxc#9170) instead of letting V8 throw a
    // bare `SyntaxError: Invalid or unexpected token` — and the file must NOT be
    // miscompiled as a legacy decorator.
    let (_stdout, stderr, code) = run_nub("vanilla-ts", "stage3-decorators.ts");
    assert_ne!(
        code, 0,
        "Stage 3 decorators (no experimentalDecorators) should fail"
    );
    assert!(
        stderr.contains("Stage 3 decorators are not supported"),
        "should be the Nub-branded Option-A diagnostic, not a raw V8 SyntaxError: {stderr}"
    );
    assert!(
        stderr.contains("experimentalDecorators"),
        "diagnostic must name the legacy-decorators workaround: {stderr}"
    );
}

#[test]
fn legacy_decorators_require_experimental_flag() {
    // Legacy decorators are opt-in via `experimentalDecorators: true` in tsconfig
    // (matching tsc). With the flag set, a method decorator runs with legacy
    // semantics. (Without it, decorators are Stage 3 → error, above.)
    //
    // KNOWN LIMITATION on the compat tier WITHOUT require(esm) (Node <20.19 / 22.0–
    // 22.11): this fixture is a CommonJS-format entry whose transpiled output
    // `require()`s an external @oxc-project/runtime helper, and the async
    // loader-worker `load` hook can't serve the synchronous ESM-translator
    // special-require that path takes below require(esm). Real but narrow (a CJS
    // *entry* using helpers, on old patch versions); the named ship gate (22.15+24)
    // is unaffected. Full analysis + the v0.x fix options:
    // wiki/research/compat-tier-cjs-entry-helpers.md. Assert the feature where it's
    // supported; skip-with-reason (NOT silently) where it isn't.
    if !node_has_require_esm() {
        eprintln!(
            "SKIP legacy_decorators_require_experimental_flag on Node {:?}: CJS-entry helper \
             require is unsupported below require(esm) (documented v0.x limitation — see \
             wiki/research/compat-tier-cjs-entry-helpers.md)",
            target_node_version()
        );
        return;
    }
    let (stdout, stderr, code) = run_nub("decorators-legacy", "main.ts");
    assert_eq!(
        code, 0,
        "legacy decorators with experimentalDecorators:true should run: {stderr}"
    );
    assert!(
        stdout.contains("legacy-decorator:HI WORLD"),
        "decorator must run with legacy semantics: {stdout}"
    );
}

#[test]
fn js_parent_no_extensionless_probe() {
    // Contract: a non-TS (`.js`) parent does NOT get nub's TS-parent extensionless
    // probing, so `import "./nonexistent"` from a `.js` fails. The EXACT failure is
    // Node-version-specific (not nub's): with detect-module (default on Node 22+)
    // the `.js` is treated as ESM and the missing specifier surfaces as
    // ERR_MODULE_NOT_FOUND; below that the `.js` is CommonJS, so the `import`
    // keyword itself is a SyntaxError before any resolution. Either way nub didn't
    // probe — assert the contract (it fails) with the version-appropriate error.
    let (_stdout, stderr, code) = run_nub("vanilla-ts", "js-no-probe.js");
    assert_ne!(code, 0, ".js importing extensionless should fail: {stderr}");
    if node_at_least((22, 0, 0)) {
        assert!(
            stderr.contains("ERR_MODULE_NOT_FOUND"),
            "detect-module Node should treat the .js as ESM and fail to resolve ./nonexistent: {stderr}"
        );
    } else {
        assert!(
            stderr.contains("import statement outside a module") || stderr.contains("Cannot find"),
            "pre-detect-module Node treats the .js as CommonJS → `import` is a SyntaxError: {stderr}"
        );
    }
}

#[test]
fn tsconfig_paths_resolve() {
    let (stdout, stderr, code) = run_nub("ts-paths", "main.ts");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("Hello, World!"));
    assert!(stdout.contains("OK"));
}

#[test]
fn js_to_ts_extension_swap() {
    let (stdout, stderr, code) = run_nub("ts-paths", "js-to-ts-swap.ts");
    assert_eq!(code, 0, ".js→.ts swap should resolve: {stderr}");
    assert!(
        stdout.contains("swap-add:5"),
        "add(2,3) via .js→.ts: {stdout}"
    );
    assert!(
        stdout.contains("swap-pi:3.14159"),
        "PI via .js→.ts: {stdout}"
    );
}

#[test]
fn directory_index_resolution() {
    let (stdout, stderr, code) = run_nub("ts-paths", "dir-index.ts");
    assert_eq!(code, 0, "directory index via tsconfig paths: {stderr}");
    assert!(
        stdout.contains("dir-index:localhost:5432"),
        "index.ts resolved: {stdout}"
    );
}

#[test]
fn directory_main_field_resolution() {
    // A TS-ESM directory import honors the directory's package.json `main`
    // (bun-parity, A34): `main` wins over a sibling `index`, and a directory
    // with a `main` but no `index` still resolves. nub already resolves
    // directory imports as a convenience — Node rejects them in ESM
    // (ERR_UNSUPPORTED_DIR_IMPORT) — so this completes that path; require()
    // already got `main` for free via Node's native resolver.
    let (stdout, stderr, code) = run_nub("dir-main", "main.ts");
    assert_eq!(code, 0, "package.json#main directory resolution: {stderr}");
    assert!(
        stdout.contains("main-wins:ENTRY"),
        "main must win over index.ts: {stdout}"
    );
    assert!(
        stdout.contains("no-index:LIB"),
        "main resolves a dir with no index: {stdout}"
    );
}

#[test]
fn baseurl_without_paths_resolution() {
    // A35: a tsconfig with `baseUrl` but no `paths` resolves bare specifiers
    // relative to baseUrl (tsc semantics; the whitepaper promises it). nub
    // already honors this — get-tsconfig's createPathsMatcher returns a
    // baseUrl-fallback matcher even without `paths` — so this is the missing
    // regression lock, not a behavior change. (Node builtins still win over
    // baseUrl, which is Node-faithful: `import "os"` is the builtin, never a
    // baseUrl `./os`. That collision is covered by other builtin tests.)
    let (stdout, stderr, code) = run_nub("baseurl-only", "main.ts");
    assert_eq!(code, 0, "baseUrl-relative bare specifiers: {stderr}");
    assert!(
        stdout.contains("baseurl-nested:5432"),
        "lib/config via baseUrl: {stdout}"
    );
    assert!(
        stdout.contains("baseurl-top:hi"),
        "greeting via baseUrl: {stdout}"
    );
}

#[test]
fn cjs_to_cts_extension_swap() {
    // D4: `import "./x.cjs"` resolves x.cts — the CommonJS analog of the
    // .mjs→.mts swap. tsc resolves the emitted .cjs extension to the .cts
    // source (verified via --traceResolution), so TS source using it must
    // resolve at runtime. A real .cjs on disk still wins over a sibling .cts
    // (the existing-file check precedes the swap), so the swap only fires when
    // the .cjs is absent.
    let (stdout, stderr, code) = run_nub("cts-swap", "main.ts");
    assert_eq!(code, 0, ".cjs→.cts swap: {stderr}");
    assert!(
        stdout.contains("cjs-swap:CTS"),
        "import './helper.cjs' resolves helper.cts: {stdout}"
    );
    assert!(
        stdout.contains("cjs-real:CJS"),
        "a real .cjs wins over a sibling .cts: {stdout}"
    );
}

#[test]
fn user_preload_named_preload_mjs_does_not_disable_augmentation() {
    // A26: a user's own `--import` of a file that happens to be named preload.mjs
    // must NOT be mistaken for nub's preload. The old re-entrancy check matched
    // the bare "preload.mjs" substring in NODE_OPTIONS and false-positived,
    // skipping augmentation entirely (TS would then break). nub now matches its
    // full preload path. Proof: a non-erasable `enum` — which only nub's oxc
    // transpiler handles, since Node's native strip-only mode rejects it — still
    // runs when NODE_OPTIONS imports an unrelated user preload.mjs.
    let user_preload = fixtures_dir().join("reentrancy").join("preload.mjs");
    let node_options = format!("--import=file://{}", user_preload.display());
    let (stdout, stderr, code) =
        run_nub_with_env("reentrancy", "main.ts", &[("NODE_OPTIONS", &node_options)]);
    assert_eq!(
        code, 0,
        "augmentation must stay active despite a user preload.mjs: {stderr}"
    );
    assert!(
        stdout.contains("reentrancy-ok:42:1"),
        "enum transpiled and ran: {stdout}"
    );
}

#[test]
fn temporal_lazy_global_and_import() {
    // A37: Temporal is installed as a lazy global — loaded on first access, not
    // eagerly at startup (the polyfill is ~18ms). It must still be fully usable
    // both as the `Temporal` global and via `import "@js-temporal/polyfill"`,
    // and both must resolve to the same object (the import clobber re-exports
    // globalThis.Temporal, which the lazy getter populates).
    let (stdout, stderr, code) = run_nub("temporal-lazy", "main.ts");
    assert_eq!(code, 0, "lazy Temporal must still be usable: {stderr}");
    assert!(
        stdout.contains("temporal-year:2026"),
        "global Temporal works: {stdout}"
    );
    assert!(
        stdout.contains("temporal-same:true"),
        "import resolves to the same global Temporal: {stdout}"
    );
    // The clobber mirrors all three of the polyfill's named exports, so a
    // destructured import of Intl + toTemporalInstant binds (not just Temporal).
    assert!(
        stdout.contains("temporal-intl:true"),
        "Intl re-exported and usable: {stdout}"
    );
    assert!(
        stdout.contains("temporal-instant:1970-01-01T00:00:00Z"),
        "toTemporalInstant re-exported, bound to Date.prototype, and callable: {stdout}"
    );
}

#[test]
fn urlpattern_available() {
    // URLPattern is available under nub. A39 feature-detects before requiring
    // the polyfill: native on Node 24+ (the polyfill is skipped), polyfilled on
    // the 22.15 floor. CI runs on 24+, so this exercises the native branch; the
    // polyfill branch is verified ad-hoc on Node 22.15 (URLPattern absent there).
    let (stdout, stderr, code) = run_nub("urlpattern", "main.ts");
    assert_eq!(code, 0, "URLPattern must work: {stderr}");
    assert!(
        stdout.contains("urlpattern-id:42"),
        "URLPattern.exec named groups: {stdout}"
    );
    assert!(
        stdout.contains("urlpattern-nomatch:true"),
        "URLPattern non-match returns null: {stdout}"
    );
}

#[test]
fn jsonc_import_works() {
    let (stdout, stderr, code) = run_nub("jsonc-import", "main.ts");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("host=localhost"));
    assert!(stdout.contains("port=5432"));
    assert!(stdout.contains("db=test_db"));
    assert!(stdout.contains("OK"));
}

#[test]
fn version_flag_works() {
    let output = Command::new(nub_binary())
        .arg("--version")
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("nub "),
        "version output should start with 'nub ': {stdout}"
    );
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn node_which_prints_path_to_stdout() {
    let output = Command::new(nub_binary())
        .args(["node", "which"])
        .output()
        .expect("failed to spawn nub");
    // Path → stdout (capturable); resolution explainer → stderr.
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        stdout.contains("node"),
        "expected a node path on stdout, got: {stdout}"
    );
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn help_flag_works() {
    let output = Command::new(nub_binary())
        .arg("--help")
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("augments Node.js"));
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn missing_file_errors() {
    let output = Command::new(nub_binary())
        .arg("/nonexistent/file.ts")
        .output()
        .expect("failed to spawn nub");
    assert_ne!(output.status.code(), Some(0));
}

#[test]
fn regexp_escape_polyfill_matches_native() {
    // RegExp.escape is native on Node 24+ and polyfilled (spec-faithful) on the
    // 22.x floor — both must be byte-identical. On the dev box this exercises
    // native; the matrix run on Node 22.13 (ci.yml) is what validates the
    // polyfill. Covers the inputs the old reduced-fidelity version got wrong:
    // a leading letter (→ \x61), whitespace (space → \x20), and "other
    // punctuators" (comma → \x2c, hyphen → \x2d).
    let (stdout, stderr, code) = run_nub("regexp-escape", "main.ts");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains(r#""\\x61\\.b\\*c""#),
        "leading-letter + syntax chars: {stdout}"
    );
    assert!(
        stdout.contains(r#""\\x61\\x20b\\tc""#),
        "whitespace (space→\\x20, tab→\\t): {stdout}"
    );
    assert!(
        stdout.contains(r#""\\x61\\x2cb\\x2dc""#),
        "other punctuators (,→\\x2c, -→\\x2d): {stdout}"
    );
    assert!(
        stdout.contains(r#""😀x""#),
        "astral code points pass through: {stdout}"
    );
}

#[test]
fn node_compile_cache_zero_disables_the_transpile_cache() {
    // NODE_COMPILE_CACHE=0 is Node's compile-cache disable signal; nub honors it
    // as "no caching in this pipeline" (transpile-cache.md) — so its transpile
    // cache is not written/read either. Otherwise the documented escape hatch is
    // a no-op.
    let cache_off = unique_test_cache();
    let (stdout, stderr, code) = run_nub_with_env(
        "vanilla-ts",
        "main.ts",
        &[
            ("XDG_CACHE_HOME", cache_off.to_str().unwrap()),
            ("NODE_COMPILE_CACHE", "0"),
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!stdout.is_empty(), "script still runs: {stdout}");
    assert!(
        !cache_off.join("nub").join("transpile").exists(),
        "NODE_COMPILE_CACHE=0 must not create the transpile cache dir"
    );

    // Control: a default run DOES write the transpile cache (proving the test
    // would catch a regression where the env check is dropped).
    let cache_on = unique_test_cache();
    let (_o, _e, c) = run_nub_with_env(
        "vanilla-ts",
        "main.ts",
        &[("XDG_CACHE_HOME", cache_on.to_str().unwrap())],
    );
    assert_eq!(c, 0);
    assert!(
        cache_on.join("nub").join("transpile").exists(),
        "a default run should write the transpile cache"
    );
}

#[test]
fn polyfills_available() {
    let fixture_path = fixtures_dir().join("vanilla-ts");
    let test_file = fixture_path.join("_polyfill_check.ts");
    std::fs::write(
        &test_file,
        "console.log(typeof RegExp.escape, typeof Error.isError, typeof Promise.try)\n",
    )
    .unwrap();

    let output = Command::new(nub_binary())
        .arg(test_file.to_str().unwrap())
        .current_dir(&fixture_path)
        .output()
        .expect("failed to spawn nub");

    let _ = std::fs::remove_file(&test_file);

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stdout, "function function function", "stderr: {stderr}");
}

/// Child processes spawned via `execSync("node ...")` inside a Nub-run
/// script should inherit Nub's TypeScript augmentation through the PATH
/// shim — `node` resolves to the shim symlink which points back to `nub`.
#[test]
fn subprocess_inherits_augmentation() {
    let (stdout, stderr, code) = run_nub("subprocess", "parent.ts");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("child-ok:42"),
        "expected 'child-ok:42' in stdout, got: {stdout:?}\nstderr: {stderr}"
    );
}

#[test]
fn three_level_nested_ts_transpilation() {
    let (stdout, stderr, code) = run_nub("nested-spawn", "main.ts");
    assert_eq!(code, 0, "3-level nested spawn failed: {stderr}");
    assert!(stdout.contains("LEVEL1"), "level 1 enum missing: {stdout}");
    assert!(stdout.contains("LEVEL2"), "level 2 enum missing: {stdout}");
    assert!(stdout.contains("LEVEL3"), "level 3 enum missing: {stdout}");
    let l1 = stdout.find("LEVEL1").unwrap();
    let l2 = stdout.find("LEVEL2").unwrap();
    let l3 = stdout.find("LEVEL3").unwrap();
    assert!(
        l1 < l2 && l2 < l3,
        "levels should appear in order: {stdout}"
    );

    // NODE_OPTIONS must not grow with nesting depth (task 5.2).
    let extract_len = |tag: &str| -> usize {
        stdout
            .lines()
            .find(|l| l.starts_with(tag))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    };
    let len1 = extract_len("opts1:");
    let len2 = extract_len("opts2:");
    let len3 = extract_len("opts3:");
    assert!(len1 > 0, "NODE_OPTIONS should be set: {stdout}");
    assert_eq!(
        len1, len2,
        "NODE_OPTIONS grew from level 1 to 2: {len1} vs {len2}"
    );
    assert_eq!(
        len2, len3,
        "NODE_OPTIONS grew from level 2 to 3: {len2} vs {len3}"
    );
}

#[test]
fn fork_ts_with_ipc() {
    let (stdout, stderr, code) = run_nub("nested-spawn", "fork-parent.ts");
    assert_eq!(code, 0, "fork .ts should work: {stderr}");
    assert!(
        stdout.contains("echo:42"),
        "IPC message round-trip: {stdout}"
    );
    assert!(
        stdout.contains("tag:forked-child"),
        "enum in forked child: {stdout}"
    );
}

#[test]
fn absolute_path_node_spawn() {
    let (stdout, stderr, code) = run_nub("nested-spawn", "abs-spawn.ts");
    assert_eq!(code, 0, "abs-path spawn should work: {stderr}");
    assert!(
        stdout.contains("abs-exit:0"),
        "child should exit 0: {stdout}"
    );
    assert!(
        stdout.contains("abs-child-ok"),
        "enum transpiled via NODE_OPTIONS dual-channel: {stdout}"
    );
}

#[test]
fn fifty_concurrent_child_processes() {
    let (stdout, stderr, code) = run_nub("nested-spawn", "concurrent-50.ts");
    assert_eq!(code, 0, "concurrent spawn should work: {stderr}");
    assert!(
        stdout.contains("concurrent:50/50"),
        "all 50 should succeed: {stdout}"
    );
    assert!(stdout.contains("fail:0"), "zero failures: {stdout}");
}

#[test]
fn concurrent_nub_processes_no_shim_collision() {
    let nub = nub_binary();

    let handles: Vec<_> = (0..5)
        .map(|_| {
            let nub = nub.clone();
            std::thread::spawn(move || {
                Command::new(&nub)
                    .args(["-e", "console.log('pid:' + process.pid)"])
                    .output()
                    .expect("failed to spawn nub")
            })
        })
        .collect();

    let mut pids = Vec::new();
    for h in handles {
        let output = h.join().unwrap();
        assert_eq!(output.status.code(), Some(0), "nub process failed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(pid) = stdout.trim().strip_prefix("pid:") {
            pids.push(pid.to_string());
        }
    }
    assert_eq!(pids.len(), 5, "all 5 should produce distinct PIDs");
    let unique: std::collections::HashSet<&String> = pids.iter().collect();
    assert_eq!(unique.len(), 5, "PIDs should be unique: {pids:?}");
}

/// Nub must not inject a `nub` global or any `NUB_*` environment
/// variables — the brand stops at the binary boundary.
#[test]
fn brand_boundary_no_globals_no_env() {
    let (stdout, stderr, code) = run_nub("vanilla-ts", "brand_check.ts");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("nub-global:undefined"),
        "expected no globalThis.nub, got: {stdout:?}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("nub-env:0"),
        "expected no NUB_* env vars, got: {stdout:?}\nstderr: {stderr}"
    );
}

/// Workspace -r runs scripts across all packages.
#[test]
fn workspace_recursive_run() {
    let fixture = fixtures_dir().join("monorepo");
    let output = Command::new(nub_binary())
        .args(["run", "-r", "build"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(stdout.contains("built-a"), "missing built-a in: {stdout}");
    assert!(stdout.contains("built-b"), "missing built-b in: {stdout}");
    assert!(stdout.contains("built-c"), "missing built-c in: {stdout}");
}

/// `-w` / `--workspace-root` runs the script in the workspace ROOT package, not
/// the member you're standing in (run.md: "targets *only* the root, regardless of
/// cwd"). Regression for the standalone-`-w` bug where it silently fell through to
/// single-package execution and ran the cwd member's script instead.
#[test]
fn workspace_root_flag_runs_root_script_from_member() {
    let dir = std::env::temp_dir().join(format!("nub-wroot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let foo = dir.join("packages").join("foo");
    std::fs::create_dir_all(&foo).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"root","private":true,"workspaces":["packages/*"],"scripts":{"who":"echo ROOT_RAN"}}"#,
    )
    .unwrap();
    std::fs::write(
        foo.join("package.json"),
        r#"{"name":"foo","scripts":{"who":"echo FOO_RAN"}}"#,
    )
    .unwrap();

    // From inside the member, `-w who` must run the ROOT's `who`.
    let output = Command::new(nub_binary())
        .args(["run", "-w", "who"])
        .current_dir(&foo)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(
        stdout.contains("ROOT_RAN"),
        "`-w` from a member must run the workspace ROOT's script; got stdout: {stdout:?}"
    );
    assert!(
        !stdout.contains("FOO_RAN"),
        "`-w` must NOT run the member's own script; got stdout: {stdout:?}"
    );
}

/// Top-level `--node` runs with zero augmentation: nub's automatic `.env`
/// loading is off (vanilla Node doesn't read `.env`), while the default run
/// loads it. Differential proof that the compat flag drops the augmentation
/// layer. (Provisioning stays on, but that's network-gated and not asserted here.)
#[test]
fn node_compat_flag_disables_augmentation() {
    let dir = std::env::temp_dir().join(format!("nub-compat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("package.json"), r#"{"name":"t"}"#).unwrap();
    std::fs::write(dir.join(".env"), "COMPAT_PROBE=loaded\n").unwrap();
    std::fs::write(
        dir.join("app.js"),
        "console.log('probe:' + (process.env.COMPAT_PROBE ?? 'unset'))",
    )
    .unwrap();

    let run = |extra: &[&str]| {
        let mut args: Vec<&str> = extra.to_vec();
        args.push("app.js");
        let out = Command::new(nub_binary())
            .args(&args)
            .current_dir(&dir)
            .output()
            .expect("failed to spawn nub");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let (default_out, default_err) = run(&[]);
    let (compat_out, compat_err) = run(&["--node"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        default_out.contains("probe:loaded"),
        "default run should auto-load .env; got {default_out:?} (stderr {default_err:?})"
    );
    assert!(
        compat_out.contains("probe:unset"),
        "`--node` must NOT auto-load .env (vanilla Node behavior); got {compat_out:?} (stderr {compat_err:?})"
    );
}

/// Workspace topological ordering: core (no deps) before utils (depends
/// on core) before app (depends on utils).
#[test]
fn workspace_topological_order() {
    let fixture = fixtures_dir().join("monorepo-deps");
    let output = Command::new(nub_binary())
        .args(["run", "-r", "build"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {stderr}\nstdout: {stdout}"
    );

    let core_pos = stdout.find("core-built").expect("missing core-built");
    let utils_pos = stdout.find("utils-built").expect("missing utils-built");
    let app_pos = stdout.find("app-built").expect("missing app-built");
    assert!(core_pos < utils_pos, "core should build before utils");
    assert!(utils_pos < app_pos, "utils should build before app");
}

/// --parallel runs all packages concurrently — wall clock should be
/// ~1 script duration, not N * duration.
#[test]
fn workspace_parallel_timing() {
    let fixture = fixtures_dir().join("monorepo-deps");
    let start = std::time::Instant::now();
    let output = Command::new(nub_binary())
        .args(["run", "-r", "--parallel", "slow"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let elapsed = start.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(stdout.contains("core-done"), "core missing: {stdout}");
    assert!(stdout.contains("utils-done"), "utils missing: {stdout}");
    assert!(stdout.contains("app-done"), "app missing: {stdout}");
    assert!(
        elapsed.as_secs() < 3,
        "parallel should take ~1s, took {}s — not concurrent",
        elapsed.as_secs()
    );
}

/// --workspace-concurrency=1 forces sequential execution even with --parallel.
#[test]
fn workspace_concurrency_one_forces_sequential() {
    let fixture = fixtures_dir().join("monorepo-deps");
    let start = std::time::Instant::now();
    let output = Command::new(nub_binary())
        .args([
            "run",
            "-r",
            "--parallel",
            "--workspace-concurrency=1",
            "slow",
        ])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let elapsed = start.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(
        stdout.contains("core-done")
            && stdout.contains("utils-done")
            && stdout.contains("app-done"),
        "all packages should run: {stdout}"
    );
    assert!(
        elapsed.as_secs() >= 3,
        "concurrency=1 should take ~3s, took {}s — not sequential",
        elapsed.as_secs()
    );
}

/// --stream prefix format: "packages/<dir> <script>$" for commands,
/// "packages/<dir> <script>:" for output.
#[test]
fn workspace_stream_prefix_format() {
    let fixture = fixtures_dir().join("monorepo-deps");
    let output = Command::new(nub_binary())
        .args(["run", "-r", "--stream", "build"])
        .current_dir(&fixture)
        .env_remove("FORCE_COLOR")
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stderr.contains("packages/core build$ "),
        "command echo on stderr with $: {stderr}"
    );
    assert!(
        stdout.contains("packages/core build: core-built"),
        "output on stdout with colon: {stdout}"
    );
}

/// Default `-r` runs the FULL pre<x> → <x> → post<x> lifecycle for each
/// member, not just the main script. The default path is streamed/concurrent
/// (a two-member chunk forces the worker-thread route, distinct from the
/// single-package path the other lifecycle tests cover); regressing it back to
/// "main only" silently mis-builds any monorepo with prebuild/postbuild — the
/// exact failure mode that killed `node --run`. Asserts strict pre < main <
/// post ordering within the package.
#[test]
fn workspace_recursive_runs_full_lifecycle_in_order() {
    let fixture = fixtures_dir().join("monorepo-lifecycle");
    let output = Command::new(nub_binary())
        .args(["run", "-r", "build"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {stderr}\nstdout: {stdout}"
    );

    let pre = stdout
        .find("builder-pre")
        .expect("prebuild was skipped (main-only regression)");
    let main = stdout.find("builder-main").expect("missing build output");
    let post = stdout
        .find("builder-post")
        .expect("postbuild was skipped (main-only regression)");
    assert!(pre < main, "prebuild must precede build: {stdout}");
    assert!(main < post, "postbuild must follow build: {stdout}");
}

/// --if-present skips packages missing the named script.
#[test]
fn workspace_if_present() {
    let fixture = fixtures_dir().join("monorepo-deps");
    let output = Command::new(nub_binary())
        .args(["run", "-r", "--if-present", "test"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // app has no "test" script — should be silently skipped
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("core-tested"), "missing core-tested");
    assert!(stdout.contains("utils-tested"), "missing utils-tested");
    assert!(
        !stdout.contains("app-tested"),
        "app-tested should not appear"
    );
}

/// --filter by name selects a single package.
#[test]
fn workspace_filter_by_name() {
    let fixture = fixtures_dir().join("monorepo-deps");
    let output = Command::new(nub_binary())
        .args(["run", "--filter", "@mono/utils", "build"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("utils-built"));
    assert!(!stdout.contains("core-built"), "core should not run");
    assert!(!stdout.contains("app-built"), "app should not run");
}

/// --filter with ...pkg includes the package + its dependencies.
#[test]
fn workspace_filter_with_deps() {
    let fixture = fixtures_dir().join("monorepo-deps");
    let output = Command::new(nub_binary())
        .args(["run", "--filter", "...@mono/app", "build"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0));
    // ...@mono/app = app + its deps (utils, core)
    assert!(stdout.contains("core-built"), "core is a transitive dep");
    assert!(stdout.contains("utils-built"), "utils is a direct dep");
    assert!(stdout.contains("app-built"), "app itself");
}

/// Repeated --filter unions the selections (A29). Each `--filter` must
/// contribute; the old `Option<String>` kept only the last, so `--filter core
/// --filter utils` ran utils alone. Verified byte-identical to `pnpm --filter
/// @mono/core --filter @mono/utils run build` (pnpm 10.15.1).
#[test]
fn workspace_multiple_filters_union() {
    let fixture = fixtures_dir().join("monorepo-deps");
    let output = Command::new(nub_binary())
        .args([
            "run",
            "--filter",
            "@mono/core",
            "--filter",
            "@mono/utils",
            "build",
        ])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("core-built"), "core was filtered in");
    assert!(stdout.contains("utils-built"), "utils was filtered in");
    assert!(
        !stdout.contains("app-built"),
        "app was not in either filter, must not run"
    );
}

// ── Section 4: Missing integration tests (v0.1-quality) ──────────

#[test]
fn jsx_execution() {
    let (stdout, stderr, code) = run_nub("jsx-test", "app.tsx");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("Hello"),
        "expected JSX output, got: {stdout}"
    );
}

#[test]
fn jsx_fragments_spread_ternary() {
    let (stdout, stderr, code) = run_nub("jsx-test", "complex.tsx");
    assert_eq!(code, 0, "complex JSX should work: {stderr}");
    assert!(
        stdout.contains("\"type\":\"Fragment\""),
        "fragment wrapper: {stdout}"
    );
    assert!(
        stdout.contains("\"label\":\"OK\""),
        "spread props on Button: {stdout}"
    );
    assert!(
        stdout.contains("\"disabled\":false"),
        "spread boolean prop: {stdout}"
    );
    assert!(
        stdout.contains("\"children\":\"visible\""),
        "ternary resolved to visible: {stdout}"
    );
    assert!(
        !stdout.contains("hidden"),
        "ternary false branch excluded: {stdout}"
    );
}

#[test]
fn jsx_classic_mode() {
    let (stdout, stderr, code) = run_nub("jsx-test/classic", "classic.tsx");
    assert_eq!(code, 0, "classic JSX should work: {stderr}");
    assert!(
        stdout.contains("\"type\":\"div\""),
        "outer div element: {stdout}"
    );
    assert!(
        stdout.contains("\"type\":\"Heading\""),
        "component resolved by name: {stdout}"
    );
    assert!(stdout.contains("\"id\":\"root\""), "div props: {stdout}");
    assert!(
        stdout.contains("\"text\":\"Classic\""),
        "component props: {stdout}"
    );
}

#[test]
fn jsx_custom_factory() {
    let (stdout, stderr, code) = run_nub("jsx-test/factory", "factory.tsx");
    assert_eq!(code, 0, "custom jsxFactory should work: {stderr}");
    assert!(
        stdout.contains("\"type\":\"div\""),
        "h() called for div: {stdout}"
    );
    assert!(
        stdout.contains("\"type\":\"Fragment\""),
        "Fragment used for <>: {stdout}"
    );
    assert!(
        !stdout.contains("React"),
        "should not reference React: {stdout}"
    );
}

#[test]
fn non_erasable_syntax() {
    let (stdout, stderr, code) = run_nub("non-erasable", "main.ts");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("enum:green"), "string enum: {stdout}");
    assert!(
        stdout.contains("reverse:Up"),
        "numeric reverse mapping: {stdout}"
    );
    assert!(stdout.contains("const-enum:1"), "const enum: {stdout}");
    assert!(
        stdout.contains("computed:3"),
        "computed initializer: {stdout}"
    );
    assert!(stdout.contains("namespace:42"), "namespace: {stdout}");
    assert!(
        stdout.contains("nested-ns:deeply-nested"),
        "nested namespace A.B.C: {stdout}"
    );
    assert!(
        stdout.contains("merge-class:true"),
        "namespace-class merge (class method): {stdout}"
    );
    assert!(
        stdout.contains("merge-fn:true"),
        "namespace-class merge (ns function): {stdout}"
    );
    assert!(
        stdout.contains("merge-const:1.0"),
        "namespace-class merge (ns const): {stdout}"
    );
    assert!(
        stdout.contains("param-prop:Alice:30"),
        "param props with default: {stdout}"
    );
}

#[test]
fn import_equals_require_cts() {
    let (stdout, stderr, code) = run_nub("non-erasable", "import-require.cts");
    assert_eq!(code, 0, "import = require() in .cts should work: {stderr}");
    assert!(
        stdout.contains("exists:true"),
        "fs.existsSync via import=require: {stdout}"
    );
    assert!(
        stdout.contains("ext:.cts"),
        "path.extname via import=require: {stdout}"
    );
    assert!(
        stdout.contains("import-require:ok"),
        "import=require overall: {stdout}"
    );
}

#[test]
fn commonjs_typed_package_ts_loads_as_cjs() {
    // A `.ts` in a "type": "commonjs" package uses require/module.exports; it
    // must load as CommonJS, not ESM. Before the format fix it was forced to
    // ESM and crashed with `module is not defined`.
    let (stdout, stderr, code) = run_nub("module-format", "cjs/index.ts");
    assert_eq!(code, 0, "commonjs-typed .ts should run as CJS: {stderr}");
    assert!(
        stdout.contains("typeof module=object"),
        "CJS `module` present: {stdout}"
    );
    assert!(
        stdout.contains("typeof require=function"),
        "CJS `require` present: {stdout}"
    );
    assert!(stdout.contains("n=42"), "type-stripped value: {stdout}");
}

#[test]
fn commonjs_typed_package_ts_with_type_import_runs() {
    // A type-only import is erased; oxc injects a stray `export {};` marker that
    // would break the CJS file. Nub strips it so the file still runs as CJS.
    let (stdout, stderr, code) = run_nub("module-format", "cjs/with-type-import.ts");
    assert_eq!(
        code, 0,
        "type-only import must not turn a CJS file into ESM: {stderr}"
    );
    assert!(
        stdout.contains("typeof module=object"),
        "still CommonJS: {stdout}"
    );
    assert!(stdout.contains("value=7"));
}

#[test]
fn module_typed_package_ts_loads_as_esm() {
    // A `.ts` in a "type": "module" package uses import/export + import.meta; it
    // must load as ESM (import.meta present, CJS require absent).
    let (stdout, stderr, code) = run_nub("module-format", "esm/index.ts");
    assert_eq!(code, 0, "module-typed .ts should run as ESM: {stderr}");
    assert!(
        stdout.contains("import.meta=object"),
        "ESM import.meta present: {stdout}"
    );
    assert!(
        stdout.contains("typeof require=undefined"),
        "no CJS require in ESM: {stdout}"
    );
    assert!(stdout.contains("ok=true"));
}

#[test]
fn typeless_package_ts_with_cjs_syntax_loads_as_cjs() {
    // Full Node parity (A6b): a `.ts` with require/module.exports and NO
    // package.json "type" is detected as CommonJS. It runs on Node, so it must
    // run on nub — before A6b it was forced to ESM and crashed (`module is not
    // defined`).
    let (stdout, stderr, code) = run_nub("module-format", "notype/cjs.ts");
    assert_eq!(
        code, 0,
        "typeless CJS-syntax .ts should run as CJS: {stderr}"
    );
    assert!(
        stdout.contains("typeof require=function"),
        "detected as CommonJS: {stdout}"
    );
    assert!(
        stdout.contains("typeof module=object"),
        "CJS module present: {stdout}"
    );
}

#[test]
fn typeless_package_ts_with_esm_syntax_loads_as_esm() {
    // The inverse: ESM syntax with no "type" is detected as ESM.
    let (stdout, stderr, code) = run_nub("module-format", "notype/esm.ts");
    assert_eq!(
        code, 0,
        "typeless ESM-syntax .ts should run as ESM: {stderr}"
    );
    assert!(
        stdout.contains("import.meta=object"),
        "detected as ESM: {stdout}"
    );
    assert!(
        stdout.contains("typeof require=undefined"),
        "no CJS require in ESM: {stdout}"
    );
}

#[test]
fn worker_transpiles_ts_entry() {
    // A `Worker(new URL("./worker.ts", ...))` inherits nub's augmentation, so the
    // worker thread transpiles its own .ts entry — including non-erasable `enum`
    // syntax. The preload runs exactly once per thread (Node dedupes the
    // --import that arrives via both execArgv and NODE_OPTIONS).
    let (stdout, stderr, code) = run_nub("worker", "main.ts");
    assert_eq!(
        code, 0,
        "Worker with a .ts entry should transpile + run: {stderr}"
    );
    assert!(
        stdout.contains("main-got:worker-ts:ready"),
        "worker transpiled its .ts entry (enum lowered): {stdout}"
    );
}

#[test]
fn worker_message_roundtrip() {
    // The worker receives the parent's message via the web `self.onmessage` API
    // (A32: the polyfill wires parentPort → self message events) and replies via
    // self.postMessage. Before A32 this hung — only the outbound path was wired.
    let (stdout, stderr, code) = run_nub("worker", "roundtrip-main.ts");
    assert_eq!(code, 0, "worker round-trip should complete: {stderr}");
    assert!(
        stdout.contains("roundtrip:echo:ping"),
        "worker must receive via self.onmessage and reply: {stdout}"
    );
}

#[test]
fn worker_throw_surfaces_to_parent_onerror() {
    // A worker that throws at top level must surface as an ErrorEvent on the
    // parent's `Worker.onerror`, and the parent must NOT crash. Below Node 26
    // `ErrorEvent` is not a global, so the polyfill's own shim is what keeps the
    // parent alive — without it `new ErrorEvent(...)` throws a ReferenceError
    // inside the worker-error handler and takes down the whole parent thread.
    let (stdout, stderr, code) = run_nub("worker", "throwing-main.ts");
    assert_eq!(
        code, 0,
        "parent must survive a throwing worker, not crash: {stderr}"
    );
    assert!(
        stdout.contains("parent-onerror:boom from worker"),
        "parent onerror must fire with the worker error's message: {stdout}"
    );
    assert!(
        stdout.contains("parent-alive:true"),
        "parent must still be running after the worker error: {stdout}"
    );
}

#[test]
fn worker_without_inbound_listener_exits_naturally() {
    // Regression (worker-polyfill delegation — worker-polyfill.md §4): the worker
    // scope once held a persistent `parentPort.on("message")` forwarder, keeping
    // every worker's event loop alive — a pure `node:worker_threads` worker that
    // posted and then idled hung forever (the compat corpus's ~37 worker
    // timeouts). The fix delegates `self` message listeners onto parentPort so
    // Node's native ref-counting governs lifetime. The fixture's parent runs a 5s
    // watchdog and prints "worker-hung" (exit 3) if the worker never exits, so a
    // regression fails fast instead of hanging the whole suite.
    let (stdout, stderr, code) = run_nub("worker", "natural-exit-main.ts");
    assert_eq!(
        code, 0,
        "a worker that posts then idles must exit naturally, not hang: {stderr}\n{stdout}"
    );
    assert!(
        stdout.contains("main-got:posted") && stdout.contains("worker-exited:0"),
        "worker must deliver its message AND exit on its own: {stdout}"
    );
    assert!(
        !stdout.contains("worker-hung"),
        "worker hung — the parentPort ref-counting regression is back: {stdout}"
    );
}

#[test]
fn recursive_run_self_reference_terminates_via_guard() {
    // A `"build": "nub run -r build"` script must terminate via the recursion
    // guard (npm_package_name + npm_lifecycle_event identify the re-entered
    // package), not loop forever — and the guard must cover BOTH the sequential
    // and the concurrent worker path (two members force the concurrent path, where
    // the guard was once missing). Poll with a deadline + kill on timeout, so a
    // regression fails fast instead of hanging the whole suite.
    let dir = std::env::temp_dir().join(format!("nub-recguard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("packages/a")).unwrap();
    std::fs::create_dir_all(dir.join("packages/b")).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"root","private":true,"workspaces":["packages/*"]}"#,
    )
    .unwrap();
    // pkg-a recurses; the nested nub is located via $TEST_NUB_BIN (set below). The
    // re-entry runs through `node -e` (not a shell `&&` + `$VAR`) so the body is
    // identical under POSIX `sh` and Windows `cmd` — the recursion-guard contract is
    // OS-independent, so the test must exercise it on every CI leg.
    std::fs::write(
        dir.join("packages/a/package.json"),
        r#"{"name":"@w/a","scripts":{"build":"node -e \"console.log('a-built');require('child_process').execFileSync(process.env.TEST_NUB_BIN,['run','-r','build'],{stdio:'inherit'})\""}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("packages/b/package.json"),
        r#"{"name":"@w/b","scripts":{"build":"echo b-built"}}"#,
    )
    .unwrap();

    let mut child = Command::new(nub_binary())
        .args(["run", "-r", "build"])
        .current_dir(&dir)
        .env("TEST_NUB_BIN", nub_binary())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn nub");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break Some(s);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let _ = std::fs::remove_dir_all(&dir);

    let status = status.expect(
        "`nub run -r build` with a self-recursive script LOOPED past 20s — guard regressed",
    );
    assert_eq!(
        status.code(),
        Some(0),
        "recursive run should exit 0 once the guard skips the re-entry"
    );
}

#[test]
fn reporter_hide_prefix_strips_per_line_prefix() {
    // --reporter-hide-prefix emits the child's raw output on stdout (no
    // `<dir> <script>: ` lead) so CI annotation matchers parse the child's lines.
    let fixture = fixtures_dir().join("monorepo-deps");
    let output = Command::new(nub_binary())
        .args(["run", "-r", "--stream", "--reporter-hide-prefix", "build"])
        .current_dir(&fixture)
        .output()
        .expect("spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("core-built"),
        "build output should appear: {stdout}"
    );
    for line in stdout.lines().filter(|l| l.ends_with("-built")) {
        assert!(
            !line.contains("packages/") && !line.contains("build:"),
            "output line must carry no per-line prefix under --reporter-hide-prefix: {line:?}"
        );
    }
}

#[test]
fn ndjson_reporter_emits_valid_json_events() {
    // `--reporter=ndjson` emits one JSON object per line on stdout, covering
    // start / log / end / summary, so CI tools can parse a run structurally.
    let fixture = fixtures_dir().join("monorepo-deps");
    let output = Command::new(nub_binary())
        .args(["run", "-r", "--reporter=ndjson", "build"])
        .current_dir(&fixture)
        .output()
        .expect("spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut events = std::collections::HashSet::new();
    let mut lines = 0;
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        lines += 1;
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("ndjson line is not valid JSON ({e}): {line}"));
        if let Some(ev) = v.get("event").and_then(|e| e.as_str()) {
            events.insert(ev.to_string());
        }
    }
    assert!(
        lines >= 4,
        "expected ≥4 ndjson lines, got {lines}:\n{stdout}"
    );
    for ev in ["start", "log", "end", "summary"] {
        assert!(
            events.contains(ev),
            "ndjson must emit a `{ev}` event; got {events:?}\n{stdout}"
        );
    }
}

#[test]
fn cjs_require_resolves_tsconfig_paths_and_extensionless_from_ts_parent() {
    // `require()` from a `.cts` (transpiled-TS CommonJS) parent must resolve a
    // tsconfig-paths alias AND an extensionless `.ts` target — identically to
    // `import` and tsx. The parent extension must not change resolution: the
    // `.cts`/`.mts` extensionless probe order once omitted `.ts`, so a `.ts`
    // target was unreachable from a `.cts` parent (worked from `.js`/`.cjs`).
    let (stdout, stderr, code) = run_nub("cjs-ts-require", "main.cts");
    assert_eq!(code, 0, "stderr: {stderr}\nstdout: {stdout}");
    assert!(
        stdout.contains("alias:42") && stdout.contains("extless:42"),
        "both the tsconfig-paths alias and the extensionless require must resolve the .ts target: {stdout}"
    );
}

#[test]
fn data_format_loaders() {
    let (stdout, stderr, code) = run_nub("data-loaders", "main.ts");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("jsonc:localhost"), "jsonc failed: {stdout}");
    assert!(
        stdout.contains("txt:Hello from txt"),
        "txt failed: {stdout}"
    );
    assert!(
        stdout.contains("yaml-host:db.example.com"),
        "yaml named export (nested): {stdout}"
    );
    assert!(
        stdout.contains("yaml-port:5432"),
        "yaml named export (number): {stdout}"
    );
    assert!(
        stdout.contains("yaml-tags:production,primary"),
        "yaml named export (array): {stdout}"
    );
    assert!(
        stdout.contains("yaml-default:myapp"),
        "yaml default export: {stdout}"
    );
    assert!(
        stdout.contains("toml-title:App Config"),
        "toml named export (string): {stdout}"
    );
    assert!(
        stdout.contains("toml-port:8080"),
        "toml named export (nested number): {stdout}"
    );
    assert!(
        stdout.contains("toml-tls:true"),
        "toml nested table: {stdout}"
    );
    assert!(
        stdout.contains("toml-debug:false"),
        "toml default export (boolean): {stdout}"
    );
    assert!(
        stdout.contains("toml-pkg:data-demo"),
        "reserved-word key `package` via default export, no SyntaxError (A15): {stdout}"
    );
    assert!(
        stdout.contains("json5-name:myapp"),
        "json5 named export: {stdout}"
    );
    assert!(
        stdout.contains("json5-ver:2"),
        "json5 default export (number): {stdout}"
    );
    assert!(
        stdout.contains("json5-feat:auth,logging"),
        "json5 named export (array): {stdout}"
    );
}

#[test]
fn env_loading_direct_file() {
    let (stdout, stderr, code) = run_nub("env-test", "main.ts");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("FOO=bar-from-env"),
        "expected FOO=bar-from-env, got: {stdout}"
    );
}

#[test]
fn env_precedence_with_node_env() {
    let (stdout, stderr, code) =
        run_nub_with_env("env-test", "precedence.ts", &[("NODE_ENV", "development")]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("SHARED=local-wins"),
        ".env.local beats .env.development: {stdout}"
    );
    assert!(
        stdout.contains("LOCAL_VAR=from-local"),
        ".env.local loaded: {stdout}"
    );
    assert!(
        stdout.contains("DEV_VAR=from-dev"),
        ".env.development loaded: {stdout}"
    );
    assert!(
        stdout.contains("FOO=bar-from-env"),
        ".env still loaded (lowest priority): {stdout}"
    );
}

#[test]
fn shell_env_overrides_dotenv() {
    let (stdout, stderr, code) = run_nub_with_env("env-test", "main.ts", &[("FOO", "shell-wins")]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("FOO=shell-wins"),
        "shell env must override .env: {stdout}"
    );
}

#[test]
fn npm_run_threads_node_execpath() {
    // A13/A38: npm_node_execpath is threaded from Node discovery — no
    // `node -e process.execPath` subprocess per `nub run`. End-to-end check that
    // `nub run` still exposes it as the resolved Node binary path (guards the
    // build_script_command wiring, not just the npm_env helper).
    let fixture_path = fixtures_dir().join("env-test");
    let output = Command::new(nub_binary())
        .args(["run", "node-execpath"])
        .current_dir(&fixture_path)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    let path = stdout
        .lines()
        .find_map(|l| l.strip_prefix("execpath="))
        .unwrap_or("")
        .trim();
    assert!(
        path.ends_with("node") || path.ends_with("node.exe"),
        "npm_node_execpath must be the resolved Node binary, got {path:?}\n{stdout}"
    );
}

/// nubx/`exec` on a bin that isn't in node_modules/.bin must SUGGEST the PM dlx
/// command and exit non-zero — never run a `dlx`/`npx` network fetch (exec.md
/// 2026-05-26: that hits the registry and can block on an install prompt in CI).
#[test]
fn exec_missing_bin_suggests_without_fetching() {
    let tmp = std::env::temp_dir().join(format!("nub-exec-miss-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("package.json"), r#"{"name":"x"}"#).unwrap();
    let output = Command::new(nub_binary())
        .args(["exec", "definitely-not-a-real-bin-xyz"])
        .current_dir(&tmp)
        .output()
        .expect("failed to spawn nub");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(
        output.status.code(),
        Some(0),
        "a bin-miss must exit non-zero: {stderr}"
    );
    assert!(
        stderr.contains("is not installed"),
        "should suggest installing: {stderr}"
    );
    assert!(
        stderr.contains("npx definitely-not-a-real-bin-xyz"),
        "should suggest the npx ad-hoc command: {stderr}"
    );
    assert!(
        !stderr.to_lowercase().contains("delegating"),
        "must NOT delegate / run a network fetch: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
#[test]
fn exec_runs_node_and_non_node_bins() {
    // A40: `nub exec` resolves node_modules/.bin and runs the entry
    // shebang-aware. A node tool (`#!…node`) runs via augmented `node`; a
    // non-node `#!/bin/sh` tool execs directly (the old `node <path>` would
    // choke — node strips the shebang and runs `echo` as JS). Unix-only: the
    // fixtures are POSIX shebang scripts created at runtime (node_modules is
    // gitignored); the Windows .cmd/.exe path is unit-tested via find_bin and
    // validated on the windows-latest CI leg.
    use std::os::unix::fs::PermissionsExt;
    let tmp = std::env::temp_dir().join(format!("nub-exec-a40-{}", std::process::id()));
    let bin = tmp.join("node_modules").join(".bin");
    std::fs::create_dir_all(&bin).unwrap();
    let write_exec = |name: &str, body: &str| {
        let p = bin.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    write_exec(
        "greet",
        "#!/usr/bin/env node\nconsole.log('exec-greet:' + process.argv.slice(2).join('|'));\n",
    );
    // A non-node tool still gets nub's augmentation env so any `node` IT spawns
    // stays transpile-enabled (this is what keeps TS configs working under `nubx
    // vite` etc.). It echoes NODE_OPTIONS to prove apply_exec_augmentation fired.
    write_exec(
        "shtool",
        "#!/bin/sh\necho \"exec-sh:$*\"\necho \"opts:$NODE_OPTIONS\"\n",
    );

    let out = Command::new(nub_binary())
        .args(["exec", "greet", "a", "b"])
        .current_dir(&tmp)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "exec greet: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("exec-greet:a|b"),
        "node .bin tool runs with args: {stdout}"
    );

    let out2 = Command::new(nub_binary())
        .args(["exec", "shtool", "x", "y"])
        .current_dir(&tmp)
        .output()
        .expect("failed to spawn nub");
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert_eq!(
        out2.status.code(),
        Some(0),
        "exec shtool: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    assert!(
        stdout2.contains("exec-sh:x y"),
        "non-node .bin execs directly (not via node): {stdout2}"
    );
    // The augmentation env reaches the non-node launcher: NODE_OPTIONS carries
    // nub's preload (`--require`/`--import …preload.…`), so a `node` the tool spawns
    // re-enters nub and stays TS-aware.
    assert!(
        stdout2.contains("opts:") && stdout2.contains("preload"),
        "a non-node .bin must inherit nub's NODE_OPTIONS preload (TS in subprocesses): {stdout2}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn env_disabled_under_node_flag() {
    let fixture_path = fixtures_dir().join("env-test");
    let output = Command::new(nub_binary())
        .args(["run", "--node", "check-env"])
        .current_dir(&fixture_path)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stdout.contains("FOO=undefined"),
        "--node should not load .env: {stdout}"
    );
}

#[test]
fn pre_post_lifecycle_scripts() {
    let fixture = fixtures_dir().join("env-test");
    let output = Command::new(nub_binary())
        .args(["run", "greet"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    // Order by line index, matching each lifecycle line after a `.trim()` so the
    // standalone `hello` is found under both LF (POSIX) and CRLF (Windows `cmd
    // echo`, which emits `hello\r\n`). `== "hello"` excludes `pre-hello`/`post-hello`.
    let line_of = |want: &str| {
        stdout
            .lines()
            .position(|l| l.trim() == want)
            .unwrap_or_else(|| panic!("missing {want} output: {stdout}"))
    };
    let pre = line_of("pre-hello");
    let main = line_of("hello");
    let post = line_of("post-hello");
    assert!(pre < main, "pregreet must run before greet: {stdout}");
    assert!(main < post, "greet must run before postgreet: {stdout}");
}

#[test]
fn single_package_run_echoes_command_to_stderr_unless_silent() {
    // A27: single-package `nub run` echoes `$ <command>` (like npm/pnpm and Nub's
    // workspace path), on stderr so it never pollutes the script's stdout. The
    // previously-inert `--silent` flag suppresses it.
    let fixture = fixtures_dir().join("env-test");

    let out = Command::new(nub_binary())
        .args(["run", "greet"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stderr.contains("$ echo hello"),
        "command must be echoed to stderr: {stderr:?}"
    );
    assert!(
        !stdout.contains("$ echo"),
        "the echo must stay on stderr, not stdout: {stdout:?}"
    );

    // --silent (global flag) suppresses the echo; the script still runs.
    let out_silent = Command::new(nub_binary())
        .args(["--silent", "run", "greet"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stderr_silent = String::from_utf8_lossy(&out_silent.stderr);
    assert_eq!(out_silent.status.code(), Some(0));
    assert!(
        !stderr_silent.contains("$ echo"),
        "--silent must suppress the echo: {stderr_silent:?}"
    );
    assert!(
        String::from_utf8_lossy(&out_silent.stdout).contains("hello"),
        "the script must still run under --silent"
    );
}

#[test]
fn float16_array_and_helpers_work() {
    // Float16Array + its TypedArray methods + Math.f16round + DataView
    // get/setFloat16. Native on Node 24+; from nub's @petamoriken/float16
    // polyfill on the 22.x floor (D5/A25). Exercises the feature through nub
    // regardless of which path is active.
    let (stdout, stderr, code) = run_nub("float16", "main.ts");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("map:3,5,7"), "TypedArray .map: {stdout}");
    assert!(
        stdout.contains("filter:2.5,3.5"),
        "TypedArray .filter: {stdout}"
    );
    assert!(stdout.contains("f16round:1.5"), "Math.f16round: {stdout}");
    assert!(
        stdout.contains("dataview:1.5"),
        "DataView get/setFloat16: {stdout}"
    );
}

#[test]
fn exec_forwards_flags_to_bin_not_nub() {
    // A flag after the bin belongs to the bin: `nub exec <bin> --version` must run
    // the bin with `--version`, not have nub's argv pre-parse consume `--version`
    // as its own flag (which printed nub's version and never ran the bin). The
    // three-position rule — regression for the pre-parse flag-stealing.
    let fixture = fixtures_dir().join("exec-args");
    let output = Command::new(nub_binary())
        .args(["exec", "argecho", "--version", "--help", "foo"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains(r#"ARGS:["--version","--help","foo"]"#),
        "flags after the bin must reach the bin, not nub: stdout={stdout:?} stderr={stderr:?}"
    );
}

#[test]
fn bareword_local_or_common_script_leads_with_the_run_hint() {
    // D3: `nub <bareword>` never auto-runs a script (a deliberate divergence from
    // pnpm/bun). When the name is an actual script or a conventional script name,
    // the bareword leads with the targeted `nub run <name>` hint. (A bareword that
    // is NOT a known/common script errors too — a PM verb redirects to the real PM,
    // anything else gets the generic message; see the PM-management verbs section.)
    let fixture = fixtures_dir().join("env-test"); // defines a `greet` script

    // (a) an actual script in package.json → targeted hint, never auto-run.
    let out = Command::new(nub_binary())
        .arg("greet")
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        out.status.code(),
        Some(0),
        "a known-script bareword must error, never auto-run: {err}"
    );
    assert!(
        err.contains("did you mean `nub run greet`"),
        "known script → run hint: {err:?}"
    );

    // (b) a conventional script name not defined here → still the targeted hint.
    let out_dev = Command::new(nub_binary())
        .arg("dev")
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    assert!(
        String::from_utf8_lossy(&out_dev.stderr).contains("did you mean `nub run dev`"),
        "common script name → run hint"
    );
}

/// Appended script args are escaped the way npm does (A42), so a multi-word arg
/// stays one arg and shell metacharacters stay literal — not split, expanded, or
/// re-parsed. Verified byte-identical to `npm run … --` with npm 11.9.0.
#[test]
fn script_args_preserve_npm_quoting() {
    let fixture = fixtures_dir().join("script-args");
    let output = Command::new(nub_binary())
        .args(["run", "echoargs", "hello world", "$HOME", "a;b"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    // printargs.js prints one `[arg]` line per received argv entry.
    assert!(
        stdout.contains("[hello world]"),
        "multi-word arg must arrive as one token, not split: {stdout:?}"
    );
    assert!(
        !stdout.contains("[hello]"),
        "'hello world' must not be split into two args: {stdout:?}"
    );
    assert!(
        stdout.contains("[$HOME]"),
        "$HOME must stay literal, not be expanded by the shell: {stdout:?}"
    );
    assert!(
        stdout.contains("[a;b]"),
        "';' must stay literal, not act as a command separator: {stdout:?}"
    );
}

#[test]
fn eval_passthrough() {
    let output = Command::new(nub_binary())
        .args(["-e", "console.log(42)"])
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("42"), "expected 42 in stdout: {stdout:?}");
}

#[test]
fn print_passthrough() {
    let output = Command::new(nub_binary())
        .args(["-p", "1+1"])
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("2"), "expected 2 in stdout: {stdout:?}");
}

#[test]
fn eval_without_argument_errors_like_node() {
    // `-e`/`--eval` with no code argument must error and exit non-zero (Node:
    // "<prog>: -e requires an argument", exit 9) — not show help and exit 0.
    let output = Command::new(nub_binary())
        .arg("-e")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to spawn nub");
    assert_ne!(
        output.status.code(),
        Some(0),
        "missing -e arg must not exit 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires an argument"),
        "expected Node's missing-argument error: {stderr:?}"
    );
}

#[test]
fn print_without_argument_reads_stdin_like_node() {
    // `-p`/`--print` with no code reads the program from stdin (Node behavior).
    // Empty stdin evaluates to `undefined` and exits 0 — not help, not an error.
    let output = Command::new(nub_binary())
        .arg("-p")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to spawn nub");
    assert_eq!(
        output.status.code(),
        Some(0),
        "empty-stdin -p should exit 0"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("undefined"),
        "empty stdin -p → undefined: {stdout:?}"
    );
}

#[test]
fn lifecycle_hooks() {
    let fixture = fixtures_dir().join("lifecycle");
    let output = Command::new(nub_binary())
        .args(["run", "greet"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0));
    let pre_pos = stdout.find("pre-greet").expect("missing pre-greet");
    let main_pos = stdout.find("main-greet").expect("missing main-greet");
    let post_pos = stdout.find("post-greet").expect("missing post-greet");
    assert!(pre_pos < main_pos, "pre should come before main");
    assert!(main_pos < post_pos, "main should come before post");
}

#[test]
fn run_without_script_lists_available_scripts() {
    // `nub run` with no script name lists available scripts (A46) instead of a
    // raw clap "required argument" error — same shape as the missing-named path.
    let fixture = fixtures_dir().join("lifecycle");
    let output = Command::new(nub_binary())
        .arg("run")
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    assert_ne!(
        output.status.code(),
        Some(0),
        "`nub run` with no script must not exit 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Available scripts"),
        "should list scripts: {stderr}"
    );
    assert!(
        stderr.contains("greet"),
        "should include the greet script: {stderr}"
    );
}

#[test]
fn subcommand_help_prints_help() {
    // `nub run --help`, `nub run -h`, and `nub help run` all print the run
    // subcommand's help to stdout (A7: clap's help was discarded → silent).
    for args in [
        vec!["run", "--help"],
        vec!["run", "-h"],
        vec!["help", "run"],
    ] {
        let output = Command::new(nub_binary())
            .args(&args)
            .output()
            .expect("failed to spawn nub");
        assert_eq!(output.status.code(), Some(0), "{args:?} should exit 0");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Run a package.json script"),
            "{args:?} should print run's help: {stdout:?}"
        );
        assert!(
            stdout.contains("--filter"),
            "{args:?} should show run's flags: {stdout:?}"
        );
    }
}

#[test]
fn nubx_basic() {
    let fixture = fixtures_dir().join("nubx-test");
    // nubx is argv0 dispatch — the binary is the same, just invoked as "nubx"
    // We can't easily test argv0 dispatch from cargo test, so test via
    // `nub exec` which is the same code path.
    let output = Command::new(nub_binary())
        .args(["exec", "hello"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello-from-bin"),
        "expected hello-from-bin, got: {stdout}"
    );
}

// ── Section 7: pnpm workspace behavior tests ────────────────────

/// --bail: when a workspace package fails, stop execution.
#[test]
fn workspace_bail_on_failure() {
    let fixture = fixtures_dir().join("monorepo-fail");
    let output = Command::new(nub_binary())
        .args(["run", "-r", "build"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    assert_ne!(
        output.status.code(),
        Some(0),
        "should exit non-zero when a package fails"
    );
}

/// Exit code forwarding: a script that exits 42 should make nub exit non-zero.
#[test]
fn exit_code_forwarding() {
    let fixture = fixtures_dir().join("lifecycle");
    // Temporarily not testing exact exit code — just that non-zero propagates
    // through nub run when the underlying script fails.
    let output = Command::new(nub_binary())
        .args(["run", "nonexistent-script"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    assert_ne!(output.status.code(), Some(0));
}

/// --reverse: dependents before dependencies.
#[test]
fn workspace_reverse_order() {
    let fixture = fixtures_dir().join("monorepo-deps");
    let output = Command::new(nub_binary())
        .args(["run", "-r", "--reverse", "build"])
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");

    let app_pos = stdout.find("app-built").expect("missing app-built");
    let core_pos = stdout.find("core-built").expect("missing core-built");
    assert!(
        app_pos < core_pos,
        "with --reverse, app should build BEFORE core"
    );
}

/// Nub's augmentation must not freeze or modify built-in prototypes.
/// Object.prototype, Array.prototype, and String.prototype must remain
/// extensible — any monkey-patching would break code that extends them.
#[test]
fn no_prototype_monkey_patching() {
    let (stdout, stderr, code) = run_nub("vanilla-ts", "frozen_check.ts");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("extensible:true,true,true"),
        "expected all prototypes extensible, got: {stdout:?}\nstderr: {stderr}"
    );
}

#[test]
fn transpile_cache_writes_atomically() {
    // A11: cache entries are written temp-file-then-rename. After a transpile the
    // cache dir must hold the finished 64-hex entry and zero leftover *.tmp files
    // (a leftover would mean a write that didn't atomically rename into place).
    // Full atomicity under concurrency is a race not forced here; this locks the
    // rename path deterministically — entry present, no temp residue.
    let cache = std::env::temp_dir().join(format!("nub-a11-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache);
    std::fs::create_dir_all(&cache).unwrap();

    let (stdout, stderr, code) = run_nub_with_env(
        "vanilla-ts",
        "main.ts",
        &[("XDG_CACHE_HOME", cache.to_str().unwrap())],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("OK"), "fixture should run: {stdout}");

    let transpile_dir = cache.join("nub").join("transpile");
    let (mut entries, mut tmp_files) = (0usize, 0usize);
    for entry in std::fs::read_dir(&transpile_dir).expect("transpile cache dir should exist") {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        if name.ends_with(".tmp") {
            tmp_files += 1;
        } else if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
            entries += 1;
        }
    }
    let _ = std::fs::remove_dir_all(&cache);

    assert!(
        entries >= 1,
        "expected at least one transpile cache entry, found {entries}"
    );
    assert_eq!(
        tmp_files, 0,
        "atomic write must leave no .tmp residue, found {tmp_files}"
    );
}

#[test]
fn corrupt_cache_entry_self_heals() {
    // A corrupt transpile-cache entry (truncation, on-disk damage, tampering)
    // must NOT be served verbatim to V8 — that crashes with a frame pointing at
    // the user's source and never recovers. Each entry carries an integrity
    // prefix (sha256(body)[..16]); cacheGet treats a mismatch as a miss and
    // re-transpiles + overwrites, so the entry self-heals.
    let cache = std::env::temp_dir().join(format!("nub-cache-heal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache);
    std::fs::create_dir_all(&cache).unwrap();
    let env = [("XDG_CACHE_HOME", cache.to_str().unwrap())];

    let (stdout, stderr, code) = run_nub_with_env("vanilla-ts", "main.ts", &env);
    assert_eq!(code, 0, "first run: {stderr}");
    assert!(stdout.contains("OK"), "fixture should run: {stdout}");

    // Corrupt every transpile entry with garbage that has no valid integrity prefix.
    let transpile_dir = cache.join("nub").join("transpile");
    let mut corrupted = 0usize;
    for entry in std::fs::read_dir(&transpile_dir).expect("transpile cache dir") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
            std::fs::write(&path, b"\x00 not valid javascript @#$ ESC[").unwrap();
            corrupted += 1;
        }
    }
    assert!(corrupted >= 1, "expected at least one entry to corrupt");

    // Re-run: must re-transpile and produce correct output, NOT crash on the garbage.
    let (stdout2, stderr2, code2) = run_nub_with_env("vanilla-ts", "main.ts", &env);
    let _ = std::fs::remove_dir_all(&cache);
    assert_eq!(
        code2, 0,
        "corrupt entry must self-heal (re-transpile), not crash: {stderr2}\n{stdout2}"
    );
    assert!(
        stdout2.contains("OK"),
        "output must be correct after a corrupt entry self-heals: {stdout2}"
    );
}

#[test]
fn env_file_flag_reaches_child_and_shell_wins() {
    // A19: --env-file vars are applied to the spawned child via Command::env (no
    // process-env mutation / no unsafe set_var). Verifies the var reaches the
    // child, and that shell env still wins over --env-file.
    let fixture = fixtures_dir().join("env-file-flag");
    let env_file = std::env::temp_dir().join(format!("nub-a19-{}.env", std::process::id()));
    std::fs::write(&env_file, "A19=from_flag\n").unwrap();

    // (a) the var reaches the spawned child
    let out = Command::new(nub_binary())
        .arg(format!("--env-file={}", env_file.display()))
        .arg(fixture.join("print.ts"))
        .current_dir(&fixture)
        .output()
        .expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("VAR=from_flag"),
        "--env-file var must reach the child: {stdout}"
    );

    // (b) shell env wins over --env-file (same key set in both)
    let out2 = Command::new(nub_binary())
        .arg(format!("--env-file={}", env_file.display()))
        .arg(fixture.join("print.ts"))
        .current_dir(&fixture)
        .env("A19", "from_shell")
        .output()
        .expect("failed to spawn nub");
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    let _ = std::fs::remove_file(&env_file);
    assert!(
        stdout2.contains("VAR=from_shell"),
        "shell env must win over --env-file: {stdout2}"
    );
}

#[test]
fn transpile_cache_eviction_evicts_oldest_over_cap() {
    // A16: exercises the eviction logic directly (the fixture imports
    // runtime/cache-evict.mjs and sweeps a temp dir with a small cap), so it
    // verifies LRU-by-mtime eviction, the low-water target, and that the
    // `.sweep` sentinel + `*.tmp` files are skipped — without the 512 MiB
    // shipped cap making it untestable.
    let (stdout, stderr, code) = run_nub("cache-evict", "sweep-test.mjs");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("EVICT-OK"), "eviction behavior: {stdout}");
}

// ── `nub run` full flag set (run.md) ────────────────────────────────────────
// Helper: spawn `nub run <args...>` in a fixture and return (stdout, stderr, code).
fn run_in(fixture: &str, args: &[&str]) -> (String, String, i32) {
    let dir = fixtures_dir().join(fixture);
    let mut cmd = Command::new(nub_binary());
    cmd.arg("run").args(args).current_dir(&dir);
    let out = cmd.output().expect("failed to spawn nub run");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// `--ignore-scripts` runs only the main script body, skipping `pre<x>`/`post<x>`.
/// This is a real CI/security affordance, not an alias: the builder package
/// defines all three, so the contract is that pre/post are absent while main
/// still runs.
#[test]
fn run_ignore_scripts_skips_pre_and_post_hooks() {
    let (stdout, stderr, code) = run_in("monorepo-lifecycle", &["-r", "--ignore-scripts", "build"]);
    assert_eq!(code, 0, "stderr: {stderr}\nstdout: {stdout}");
    assert!(
        stdout.contains("builder-main"),
        "main script must still run: {stdout}"
    );
    assert!(
        !stdout.contains("builder-pre"),
        "prebuild must be skipped: {stdout}"
    );
    assert!(
        !stdout.contains("builder-post"),
        "postbuild must be skipped: {stdout}"
    );
}

/// `--resume-from <pkg>` drops the topological *predecessors* of `<pkg>`, keeping
/// `<pkg>` and everything scheduled after it. In core ← utils ← app, resuming
/// from utils must run utils + app but NOT the already-succeeded core — the CI
/// restart-after-failure contract.
#[test]
fn run_resume_from_drops_topological_predecessors() {
    let (stdout, stderr, code) = run_in(
        "monorepo-deps",
        &["-r", "--resume-from", "@mono/utils", "build"],
    );
    assert_eq!(code, 0, "stderr: {stderr}\nstdout: {stdout}");
    assert!(
        stdout.contains("utils-built"),
        "resume target must run: {stdout}"
    );
    assert!(
        stdout.contains("app-built"),
        "successor of the resume target must run: {stdout}"
    );
    assert!(
        !stdout.contains("core-built"),
        "predecessor of the resume target must be dropped: {stdout}"
    );
}

/// `--resume-from` on a DIAMOND (build order a → {b, c} → d; d depends on b,c
/// which depend on a). Resuming from `c` drops the predecessor chunk (`a`) but
/// keeps `c` AND its co-wave peer `b` (same topological wave), then `d`. A flat
/// linear slice would wrongly drop `b`; the chunk-not-flat semantic
/// (`cli.rs` resume-chunk drop) keeps it. Guards against a future flat-slice
/// regression.
#[test]
fn run_resume_from_keeps_co_wave_peers_on_a_diamond() {
    let (stdout, stderr, code) = run_in(
        "monorepo-diamond",
        &["-r", "--resume-from", "@diamond/c", "build"],
    );
    assert_eq!(code, 0, "stderr: {stderr}\nstdout: {stdout}");
    assert!(
        !stdout.contains("a-built"),
        "the predecessor chunk (a) must be dropped: {stdout}"
    );
    assert!(
        stdout.contains("b-built"),
        "the co-wave peer b must run (chunk, not flat slice): {stdout}"
    );
    assert!(
        stdout.contains("c-built"),
        "the resume target c must run: {stdout}"
    );
    assert!(
        stdout.contains("d-built"),
        "the successor d must run: {stdout}"
    );
    let d = stdout.find("d-built").expect("d-built present");
    assert!(
        stdout.find("b-built").unwrap() < d && stdout.find("c-built").unwrap() < d,
        "d (dependent) must run after both b and c: {stdout}"
    );
}

/// `--workspace <name>` is npm-style member selection (long-only; `-w` stays
/// pnpm's `--workspace-root`). It desugars to a name filter. The load-bearing
/// part is the value-consuming coupling: `--workspace @mono/utils build` must
/// bind `@mono/utils` as the member and `build` as the script — NOT mis-bind
/// `@mono/utils` as the script name (which the positional-split would do if the
/// flag were missing from `value_consuming_flags`).
#[test]
fn run_workspace_selects_member_and_does_not_steal_the_script_positional() {
    let (stdout, stderr, code) = run_in("monorepo-deps", &["--workspace", "@mono/utils", "build"]);
    assert_eq!(code, 0, "stderr: {stderr}\nstdout: {stdout}");
    assert!(
        stdout.contains("utils-built"),
        "the named member's `build` script must run: {stdout}"
    );
    assert!(
        !stdout.contains("core-built") && !stdout.contains("app-built"),
        "only the selected member runs: {stdout}"
    );
}

/// `--aggregate-output` buffers each package's output and flushes it as one
/// contiguous block, so concurrent packages never interleave their lines. The
/// contract: within the combined output, every line of one package precedes
/// every line of the other (no A-then-B-then-A tearing). Uses `--parallel` to
/// force concurrent execution where streamed output would interleave.
#[test]
fn run_aggregate_output_keeps_each_packages_lines_contiguous() {
    let (stdout, stderr, code) = run_in(
        "monorepo-deps",
        &["-r", "--parallel", "--aggregate-output", "slow"],
    );
    assert_eq!(code, 0, "stderr: {stderr}\nstdout: {stdout}");
    // Each package prints exactly one "<pkg>-done" line; with aggregation the
    // three lines are each emitted as part of an uninterrupted per-package block.
    // The strong, non-flaky invariant: all three packages reported, and no
    // package's block is split by another's (checked via the done-marker order
    // being a permutation, which buffered output guarantees and interleaving
    // could violate by emitting partial blocks). We assert presence + that the
    // markers appear once each (buffered blocks don't duplicate).
    for marker in ["core-done", "utils-done", "app-done"] {
        assert_eq!(
            stdout.matches(marker).count(),
            1,
            "{marker} should appear exactly once: {stdout}"
        );
    }
}

/// Stronger `--aggregate-output` non-tear: 3 packages each emit 10 lines with a
/// `sleep` between them, run concurrently (`--workspace-concurrency 3`). Without
/// aggregation those lines would interleave (a-1, b-1, c-1, a-2, …); the
/// `AGGREGATE_FLUSH_LOCK` must flush each package's 10 lines as one uninterrupted
/// block. Verified: the package-id sequence must collapse to exactly 3 runs (one
/// per package), not the interleaved many-run pattern. Guards against removing the
/// mutex.
#[test]
fn run_aggregate_output_blocks_do_not_tear_under_concurrency() {
    let (stdout, stderr, code) = run_in(
        "monorepo-aggregate",
        &[
            "-r",
            "--workspace-concurrency",
            "3",
            "--aggregate-output",
            "build",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}\nstdout: {stdout}");

    // Reduce stdout to the ordered package-id of each `AGG-<pkg>-<n>` output line.
    let seq: Vec<char> = stdout
        .lines()
        .filter_map(|l| l.find("AGG-").and_then(|i| l[i + 4..].chars().next()))
        .collect();
    assert_eq!(
        seq.len(),
        30,
        "expected 30 marker lines (3 pkgs × 10): {stdout}"
    );
    for p in ['a', 'b', 'c'] {
        assert_eq!(
            seq.iter().filter(|&&c| c == p).count(),
            10,
            "package {p} must emit all 10 lines: {stdout}"
        );
    }
    // One contiguous block per package ⇒ exactly 3 runs (2 transitions).
    let runs = seq.windows(2).filter(|w| w[0] != w[1]).count() + 1;
    assert_eq!(
        runs, 3,
        "each package's 10 lines must be one contiguous block (no tearing); got {runs} runs: {seq:?}\n{stdout}"
    );
}

/// `--script-shell <path>` must actually invoke the named shell. Proven with a
/// fake shell that prints a marker before delegating to `/bin/sh` — robust across
/// platforms, unlike bash-vs-sh `$BASH_VERSION` (macOS `/bin/sh` is bash-as-sh and
/// DOES set it, so it can't distinguish). The Windows `--script-shell` path is
/// CI-verified on windows-latest (Docker on the dev box is Linux only).
#[cfg(unix)]
#[test]
fn run_script_shell_invokes_the_named_shell() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("nub-script-shell-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"sstest","scripts":{"build":"echo body-ran"}}"#,
    )
    .unwrap();
    let fake = dir.join("fakeshell");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho FAKESHELL-USED\nexec /bin/sh \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    // With --script-shell <fake>: the marker appears AND the body still runs.
    let out = Command::new(nub_binary())
        .args(["run", "--script-shell", fake.to_str().unwrap(), "build"])
        .current_dir(&dir)
        .output()
        .expect("spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("FAKESHELL-USED"),
        "--script-shell must invoke the named shell: {stdout}"
    );
    assert!(
        stdout.contains("body-ran"),
        "the body must run via the named shell: {stdout}"
    );

    // Without --script-shell: the default shell runs the body — no fake-shell marker.
    let out2 = Command::new(nub_binary())
        .args(["run", "build"])
        .current_dir(&dir)
        .output()
        .expect("spawn nub");
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        !stdout2.contains("FAKESHELL-USED"),
        "default run must not use the fake shell: {stdout2}"
    );
    assert!(
        stdout2.contains("body-ran"),
        "default run still runs the body: {stdout2}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// npm/pnpm parity: a tool in `node_modules/.bin` shadows a same-named tool on the
/// system PATH. This regression-locks a fix — the augmentation layer used to compose
/// the PATH as `shim:system:.bin:system`, putting `node_modules/.bin` *after* the
/// system PATH, so a system tool won the name collision (the opposite of npm/pnpm).
/// The fix composes `shim:.bin:system`, so the local tool wins. Unix-only because the
/// fixture relies on a shebang script + `0o755`; the PATH-ordering logic itself is
/// platform-agnostic (it's pure string composition in `cli.rs`).
#[cfg(unix)]
#[test]
fn run_prefers_local_node_modules_bin_over_system_path() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("nub-bin-shadow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let local_bin = dir.join("node_modules").join(".bin");
    let sys_bin = dir.join("sys");
    std::fs::create_dir_all(&local_bin).unwrap();
    std::fs::create_dir_all(&sys_bin).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"shadowtest","scripts":{"go":"collide"}}"#,
    )
    .unwrap();
    // Same tool name, different output, in the local .bin and on the system PATH.
    let mk = |path: &std::path::Path, marker: &str| {
        std::fs::write(path, format!("#!/bin/sh\necho {marker}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    mk(&local_bin.join("collide"), "LOCAL-BIN");
    mk(&sys_bin.join("collide"), "SYSTEM-BIN");

    let path = format!(
        "{}:{}",
        sys_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(nub_binary())
        .args(["run", "go"])
        .current_dir(&dir)
        .env("PATH", path)
        .output()
        .expect("spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("LOCAL-BIN"),
        "node_modules/.bin must shadow the system tool (npm/pnpm parity): {stdout}"
    );
    assert!(
        !stdout.contains("SYSTEM-BIN"),
        "the system tool must not win the name collision: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// npm/pnpm set `$NODE` to the node binary running the script so `$NODE child.js`
/// invokes "the same Node." nub points it at the PATH-shim node (→ nub) so an
/// absolute-path `$NODE` re-enters nub and the child stays augmented (it used to be
/// unset). Proven with an `enum` child: plain Node strip-only mode REJECTS it
/// (non-erasable), so `$NODE enum-child.ts` succeeding proves `$NODE` reached nub's
/// transpiler, not a raw Node — a discriminator that stays meaningful even on a Node
/// version with native type-stripping. Unix-only (the assertion shells out through
/// `sh`); the env-var wiring is platform-agnostic.
#[cfg(unix)]
#[test]
fn run_points_node_env_at_an_augmenting_shim() {
    let dir = std::env::temp_dir().join(format!("nub-node-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // An enum is non-erasable: plain `node enum.ts` (strip-only) errors; nub transforms it.
    std::fs::write(
        dir.join("enum.ts"),
        "enum E { A, B }\nconsole.log(`ENUM-OK ${E.B}`);\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"nodeenv","scripts":{"show":"echo NODE=[$NODE]","run-ts":"\"$NODE\" enum.ts"}}"#,
    )
    .unwrap();

    // 1. $NODE is set (was empty before this fix).
    let show = Command::new(nub_binary())
        .args(["run", "-s", "show"])
        .current_dir(&dir)
        .output()
        .expect("spawn nub");
    let show_out = String::from_utf8_lossy(&show.stdout);
    assert!(
        !show_out.contains("NODE=[]"),
        "$NODE must be set under `nub run`: {show_out}"
    );

    // 2. `$NODE enum.ts` transpiles — proves $NODE re-enters nub, not a raw Node
    //    (which would reject the enum in strip-only mode).
    let out = Command::new(nub_binary())
        .args(["run", "-s", "run-ts"])
        .current_dir(&dir)
        .output()
        .expect("spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("ENUM-OK 1"),
        "$NODE must run TypeScript children via nub's transpiler: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The npm/pnpm aliases map to their canonical flags (`-F` is `--filter`, `-s`
/// is `--silent`, `--workspaces` is `--recursive`). One run exercises all three
/// at once (no per-alias test): `-F` selects a member (proving the alias plus
/// its value-consuming binding), `-s` suppresses the `$ <cmd>` preamble echo,
/// and the combination runs cleanly.
#[test]
fn run_npm_aliases_map_to_canonical_flags() {
    let (stdout, stderr, code) = run_in("monorepo-deps", &["-F", "@mono/core", "-s", "build"]);
    assert_eq!(code, 0, "stderr: {stderr}\nstdout: {stdout}");
    assert!(
        stdout.contains("core-built"),
        "`-F` must select the member: {stdout}"
    );
    assert!(
        !stdout.contains("utils-built"),
        "`-F` must restrict to the matched member: {stdout}"
    );
    assert!(
        !stderr.contains("$ echo core-built"),
        "`-s` must suppress the preamble echo: {stderr}"
    );

    // `--workspaces` is the npm spelling of `--recursive`: it runs every member.
    let (stdout2, stderr2, code2) = run_in("monorepo-deps", &["--workspaces", "build"]);
    assert_eq!(code2, 0, "stderr: {stderr2}");
    assert!(
        stdout2.contains("core-built")
            && stdout2.contains("utils-built")
            && stdout2.contains("app-built"),
        "`--workspaces` must run every member like `-r`: {stdout2}"
    );
}

// ── PM-management verbs (A2 passthrough disabled) ────────────────────────────

/// A deliberately-excluded engine verb (`deploy`) errors non-zero with its
/// honest status ("not yet supported") and a real-PM fallback. Nothing is
/// dispatched — stdout stays empty.
#[test]
fn bareword_pm_verb_errors_with_the_real_pm_command() {
    let dir = unique_test_cache();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("package.json"), r#"{"name":"app"}"#).unwrap();
    std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap(); // lockfile → pnpm
    let out = Command::new(nub_binary())
        .args(["deploy", "out"])
        .current_dir(&dir)
        .output()
        .expect("spawn nub deploy");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not yet supported") && stderr.contains("pnpm deploy"),
        "the error must state the status and the real-PM fallback: {stderr}"
    );
    assert_ne!(
        out.status.code(),
        Some(0),
        "an excluded PM verb is an error, not a dispatch"
    );
    assert!(
        out.stdout.is_empty(),
        "nothing may be forwarded to a PM: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// A verb nub has never heard of (`frobnicate`) errors with the generic
/// not-a-command message and the script/file hints — there is no passthrough
/// fallback to a PM anymore.
#[test]
fn bareword_unknown_verb_errors() {
    let dir = unique_test_cache();
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(nub_binary())
        .args(["frobnicate", "--wat"])
        .current_dir(&dir)
        .output()
        .expect("spawn nub frobnicate");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("\"frobnicate\" is not a nub command"),
        "an unknown verb must error, not dispatch: {stderr}"
    );
    assert_ne!(out.status.code(), Some(0));
}

/// `nub pm which` with no pin errors clearly (names the unpinned state + the
/// `nub pm pin` remedy) and exits non-zero — exercised through the binary so
/// the dispatch routing (`pm` → `run_pm` → `which`) is covered end-to-end.
#[test]
fn pm_which_without_a_pin_errors_through_the_binary() {
    let dir = unique_test_cache();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("package.json"), r#"{"name":"app"}"#).unwrap();
    let out = Command::new(nub_binary())
        .args(["pm", "which"])
        .current_dir(&dir)
        .env("XDG_CACHE_HOME", unique_test_cache())
        .output()
        .expect("spawn nub pm which");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        out.status.code(),
        Some(0),
        "no-pin which must exit non-zero"
    );
    assert!(
        stderr.contains("no package manager is pinned") && stderr.contains("nub pm pin"),
        "the error must name the unpinned state and the remedy: {stderr}"
    );
}

// ── Section 8: exec/nubx workspace flags (-r / --filter / --parallel) ───
//
// Unix-only: the `.bin` entries are POSIX-shebang node scripts created at
// runtime (node_modules is gitignored), same constraint the `exec_runs_*` tests
// note. The Windows `.cmd`/`.exe` resolution is covered by `find_bin`'s unit
// tests + the windows-latest CI leg.

/// Build a two-member workspace under a fresh temp dir. Each member gets a local
/// `node_modules/.bin/<bin>` node shebang script whose body is `make_body(member)`,
/// so a test can give each member a distinguishable bin. Returns the root dir.
#[cfg(unix)]
fn make_exec_workspace(tag: &str, bin: &str, make_body: impl Fn(&str) -> String) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("nub-execws-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"root","private":true,"workspaces":["packages/*"]}"#,
    )
    .unwrap();
    for member in ["a", "b"] {
        let dir = root.join("packages").join(member);
        let bin_dir = dir.join("node_modules").join(".bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name":"@org/{member}"}}"#),
        )
        .unwrap();
        let bin_file = bin_dir.join(bin);
        std::fs::write(&bin_file, make_body(member)).unwrap();
        std::fs::set_permissions(&bin_file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    root
}

/// `nub exec -r <bin>` runs the bin once in every member — the golden recursive
/// path. Each member's local `.bin` greeter prints its own member name, proving
/// the bin ran per-member (not once at the root).
#[cfg(unix)]
#[test]
fn exec_recursive_runs_the_bin_in_each_member() {
    let root = make_exec_workspace("rec", "greet", |member| {
        format!("#!/usr/bin/env node\nconsole.log('ran-in:{member}');\n")
    });
    let out = Command::new(nub_binary())
        .args(["exec", "-r", "greet"])
        .current_dir(&root)
        .output()
        .expect("spawn nub exec -r");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(
        stdout.contains("ran-in:a"),
        "member a must run the bin: {stdout}"
    );
    assert!(
        stdout.contains("ran-in:b"),
        "member b must run the bin: {stdout}"
    );
}

/// `--filter <name>` narrows a recursive exec to the one matching member; the
/// other member's bin must NOT run.
#[cfg(unix)]
#[test]
fn exec_filter_narrows_to_one_member() {
    let root = make_exec_workspace("filt", "greet", |member| {
        format!("#!/usr/bin/env node\nconsole.log('ran-in:{member}');\n")
    });
    let out = Command::new(nub_binary())
        .args(["exec", "--filter", "@org/a", "greet"])
        .current_dir(&root)
        .output()
        .expect("spawn nub exec --filter");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.contains("ran-in:a"),
        "the filtered member must run: {stdout}"
    );
    assert!(
        !stdout.contains("ran-in:b"),
        "the unfiltered member must NOT run: {stdout}"
    );
}

/// A member missing the bin is a per-member error (exec has no `--if-present`),
/// not a silent skip: the overall run exits non-zero, the error names the missing
/// bin, and the member that DOES have the bin still runs.
#[cfg(unix)]
#[test]
fn exec_recursive_member_missing_bin_is_an_error_not_a_skip() {
    // Build with the bin in both members, then delete it from `b`.
    let root = make_exec_workspace("miss", "greet", |member| {
        format!("#!/usr/bin/env node\nconsole.log('ran-in:{member}');\n")
    });
    std::fs::remove_file(root.join("packages/b/node_modules/.bin/greet")).unwrap();
    let out = Command::new(nub_binary())
        .args(["exec", "-r", "greet"])
        .current_dir(&root)
        .output()
        .expect("spawn nub exec -r");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&root);
    assert_ne!(
        out.status.code(),
        Some(0),
        "a missing bin must fail the run: {stderr}"
    );
    assert!(
        stderr.contains("missing bin \"greet\""),
        "the failure must name the missing bin (not skip silently): {stderr}"
    );
    assert!(
        stdout.contains("ran-in:a"),
        "the member that HAS the bin must still run: {stdout}"
    );
}

/// A plain `nub exec <bin>` (no -r/--filter/--parallel) stays the single-package
/// path: the workspace branch must NOT engage. Run from a member with a local
/// bin; only that member's bin runs, and exactly once.
#[cfg(unix)]
#[test]
fn exec_without_workspace_flags_is_unchanged() {
    let root = make_exec_workspace("plain", "greet", |member| {
        format!("#!/usr/bin/env node\nconsole.log('ran-in:{member}');\n")
    });
    let out = Command::new(nub_binary())
        .args(["exec", "greet"])
        .current_dir(root.join("packages/a"))
        .output()
        .expect("spawn nub exec");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert_eq!(
        stdout.matches("ran-in:").count(),
        1,
        "plain exec runs the bin once in the cwd member only: {stdout}"
    );
    assert!(
        stdout.contains("ran-in:a"),
        "must run member a's own bin: {stdout}"
    );
}

/// argv split: `nubx --filter @org/a greet --flag` binds `@org/a` to the filter
/// (a value-consuming flag, not the bin positional) and forwards `--flag` to the
/// bin. Routed through `nub exec` (the identical split path nubx uses).
#[cfg(unix)]
#[test]
fn exec_filter_value_does_not_steal_the_bin_and_forwards_trailing_flags() {
    let root = make_exec_workspace("argv", "greet", |_member| {
        "#!/usr/bin/env node\nconsole.log('args:' + process.argv.slice(2).join('|'));\n".to_string()
    });
    let out = Command::new(nub_binary())
        .args(["exec", "--filter", "@org/a", "greet", "--fix", "x"])
        .current_dir(&root)
        .output()
        .expect("spawn nub exec --filter ... greet --fix");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(
        stdout.contains("args:--fix|x"),
        "trailing flags after the bin must reach the bin: {stdout}"
    );
}

/// Per-member cwd (the correctness core of this phase): a node bin runs IN its
/// member's directory, so it sees that member's auto-loaded `.env` — not the
/// workspace root's. The bin is HOISTED to the root `.bin` (one file, resolved by
/// `find_bin`'s walk-up for both members), and each member's `.env` sets `WHO` to
/// a distinct value the bin echoes. Before the cwd fix, both members ran with the
/// root cwd and would have echoed the same (root/none) value.
#[cfg(unix)]
#[test]
fn exec_recursive_node_bin_uses_each_members_cwd_and_env() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("nub-execws-cwd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"root","private":true,"workspaces":["packages/*"]}"#,
    )
    .unwrap();
    // Hoisted bin in the ROOT .bin only — both members resolve it via walk-up.
    let root_bin = root.join("node_modules").join(".bin");
    std::fs::create_dir_all(&root_bin).unwrap();
    let bin_file = root_bin.join("whoami-env");
    std::fs::write(
        &bin_file,
        "#!/usr/bin/env node\nconsole.log('who:' + (process.env.WHO ?? 'unset'));\n",
    )
    .unwrap();
    std::fs::set_permissions(&bin_file, std::fs::Permissions::from_mode(0o755)).unwrap();
    for member in ["a", "b"] {
        let dir = root.join("packages").join(member);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name":"@org/{member}"}}"#),
        )
        .unwrap();
        std::fs::write(dir.join(".env"), format!("WHO={member}\n")).unwrap();
    }

    let out = Command::new(nub_binary())
        .args(["exec", "-r", "whoami-env"])
        .current_dir(&root)
        .env("XDG_CACHE_HOME", unique_test_cache())
        .output()
        .expect("spawn nub exec -r whoami-env");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(
        stdout.contains("who:a"),
        "member a's bin must see a's cwd/.env (WHO=a): {stdout}"
    );
    assert!(
        stdout.contains("who:b"),
        "member b's bin must see b's cwd/.env (WHO=b): {stdout}"
    );
}

// ── `nub pm` / `nub node` UX-message fixes ───────────────────────────────────

/// `nub pm which` must name the TRUE pin source. A project pinned ONLY via
/// `devEngines.packageManager` (no `packageManager` field) used to be mislabeled
/// "resolved from packageManager"; the provenance now reads
/// "resolved from devEngines.packageManager". Seeds nub's PM cache with the exact
/// version so the provision under `which` is a pure cache hit — no network.
#[test]
fn pm_which_reports_dev_engines_provenance() {
    let work = unique_test_cache();
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    // devEngines-ONLY pin (no `packageManager` field), exact version so the
    // cache-hit path fires without touching the registry.
    std::fs::write(
        proj.join("package.json"),
        r#"{"name":"app","devEngines":{"packageManager":{"name":"pnpm","version":"9.1.0"}}}"#,
    )
    .unwrap();

    // Seed <XDG_CACHE_HOME>/nub/pm/pnpm/9.1.0/package/ — the shape provision_pm's
    // cache-hit reads (a manifest naming the bin + the bin file itself).
    let cache = work.join("cache");
    let pkg = cache.join("nub/pm/pnpm/9.1.0/package");
    std::fs::create_dir_all(pkg.join("bin")).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{"name":"pnpm","bin":"bin/pnpm.cjs"}"#,
    )
    .unwrap();
    std::fs::write(pkg.join("bin/pnpm.cjs"), "// pnpm\n").unwrap();

    let out = Command::new(nub_binary())
        .args(["pm", "which"])
        .current_dir(&proj)
        .env("XDG_CACHE_HOME", &cache)
        .output()
        .expect("spawn nub pm which");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&work);

    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stdout.contains("bin/pnpm.cjs"),
        "the cached pnpm bin path goes to stdout: {stdout:?}"
    );
    assert!(
        stderr.contains("resolved from devEngines.packageManager"),
        "a devEngines-only pin must report its true source, not packageManager: {stderr:?}"
    );
    assert!(
        !stderr.contains("resolved from packageManager"),
        "the old hard-coded packageManager label must be gone: {stderr:?}"
    );
}

/// A truncated / invalid `package.json` must be diagnosed as a JSON parse failure
/// (naming the file), not as "no package manager is pinned" — the misleading
/// message it produced when resolution silently swallowed the parse error.
#[test]
fn pm_which_reports_malformed_manifest_not_unpinned() {
    let work = unique_test_cache();
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    // Truncated mid-object — serde_json errors with a line/column.
    std::fs::write(
        proj.join("package.json"),
        "{\n  \"packageManager\": \"pnpm@9.1.0\"",
    )
    .unwrap();

    let out = Command::new(nub_binary())
        .args(["pm", "which"])
        .current_dir(&proj)
        .env("XDG_CACHE_HOME", unique_test_cache())
        .output()
        .expect("spawn nub pm which");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&work);

    assert_ne!(out.status.code(), Some(0), "a malformed manifest must fail");
    assert!(
        stderr.contains("package.json is not valid JSON") && stderr.contains("package.json"),
        "malformed JSON must be named as such (with the path): {stderr:?}"
    );
    assert!(
        !stderr.contains("no package manager is pinned"),
        "a parse failure must NOT be misreported as unpinned: {stderr:?}"
    );
}

/// `nub node which` against an unsatisfiable pin must give nub-correct remedy:
/// provision via `nub node install` and the pin fields nub honors — NOT the old
/// `nvm install` + nonexistent "compat mode" suggestion that contradicts nub's
/// model. Pins `.nvmrc` to a version no PATH node satisfies, with an empty store
/// and NVM_DIR so discovery exhausts every source and hits `PinnedNotFound`.
#[test]
fn node_which_unsatisfiable_pin_gives_nub_remedy_not_nvm() {
    let work = unique_test_cache();
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    // 0.0.1 is real-shaped but no installed Node satisfies it.
    std::fs::write(proj.join(".nvmrc"), "0.0.1\n").unwrap();
    let empty_nvm = work.join("empty-nvm");
    std::fs::create_dir_all(&empty_nvm).unwrap();

    let out = Command::new(nub_binary())
        .args(["node", "which"])
        .current_dir(&proj)
        .env("XDG_CACHE_HOME", work.join("cache")) // empty store
        .env("NVM_DIR", &empty_nvm)
        .output()
        .expect("spawn nub node which");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&work);

    // No node on PATH at all surfaces a different (NoNodeOnPath) error — skip,
    // since this test is specifically about the PinnedNotFound remedy text.
    if stderr.contains("no Node binary found on PATH") {
        eprintln!("skipping: no node on PATH to drive PinnedNotFound");
        return;
    }
    assert_ne!(out.status.code(), Some(0), "an unsatisfiable pin must fail");
    assert!(
        stderr.contains("nub node install"),
        "the remedy must point at nub's own provisioning: {stderr:?}"
    );
    assert!(
        !stderr.to_lowercase().contains("nvm install")
            && !stderr.to_lowercase().contains("compat mode"),
        "the nvm-install / compat-mode suggestions contradict nub and must be gone: {stderr:?}"
    );
}
