//! End-to-end tier behavior: exercise `nub` against specific Node binaries
//! discovered via PATH, asserting the contract from
//! `wiki/research/supported-node-versions.md`:
//!
//! - **Compat tier (18.19 – 22.14):** runs silently, full feature surface
//!   works (TS executes, stdout is clean, stderr stays empty — no
//!   user-visible distinction from the fast path).
//! - **Hard-error tier (≤ 18.18):** stderr carries the canonical refusal text
//!   including the upgrade-or-fall-back guidance, non-zero exit, no Node spawn.
//!
//! These tests are gated on the presence of a real installed Node at the
//! exact version under test (resolved via nvm layout). When the version is
//! absent locally the test is skipped, not failed — they are a local + CI
//! signal, not a hard build-time dependency. CI installs the matrix via
//! `actions/setup-node`, so the gating only hides them on developer
//! machines that don't have the full nvm matrix.

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

/// Locate a `bin/` directory containing a `node` binary that reports the
/// requested version. Looks first in `$TEST_NODE_BIN_<MAJOR>_<MINOR>_<PATCH>`
/// (set by CI to point at the `actions/setup-node`-installed binary) and
/// then falls back to the standard nvm layout under `$NVM_DIR` /
/// `$HOME/.nvm`. Returns `None` when the version isn't installed — the
/// caller skips the test in that case.
///
/// The env var deliberately does NOT use the `NUB_*` prefix (brand-boundary
/// rule applies even to test-only knobs). It is set by CI, read by this
/// test binary only, and never touches the `nub` runtime's process env.
fn find_node_bin_dir(want: (u32, u32, u32)) -> Option<PathBuf> {
    let (maj, min, pat) = want;

    // CI override: a pre-installed Node whose bin dir is named by env.
    let override_key = format!("TEST_NODE_BIN_{maj}_{min}_{pat}");
    if let Ok(dir) = std::env::var(&override_key) {
        let p = PathBuf::from(dir);
        if verify_node_version(&p, want) {
            return Some(p);
        }
    }

    let nvm_dir = std::env::var_os("NVM_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".nvm")))?;
    let candidate = nvm_dir
        .join("versions/node")
        .join(format!("v{maj}.{min}.{pat}"))
        .join("bin");
    if verify_node_version(&candidate, want) {
        Some(candidate)
    } else {
        None
    }
}

fn home_dir() -> Option<PathBuf> {
    // Test-only home lookup — std::env::home_dir is deprecated, but for a test
    // we just need $HOME / %USERPROFILE%, no symlink resolution needed.
    if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn verify_node_version(bin_dir: &Path, want: (u32, u32, u32)) -> bool {
    let node = bin_dir.join(if cfg!(windows) { "node.exe" } else { "node" });
    if !node.is_file() {
        return false;
    }
    let Ok(output) = Command::new(&node).arg("--version").output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = format!("v{}.{}.{}", want.0, want.1, want.2);
    stdout.trim() == expected
}

/// Run `nub` with PATH pointing at the chosen Node bin dir, so that
/// discovery picks up the exact requested version. Returns `None` when
/// the version isn't installed (test skip).
fn run_nub_against_node(
    want: (u32, u32, u32),
    fixture: &str,
    file: &str,
) -> Option<(String, String, i32)> {
    let bin_dir = find_node_bin_dir(want)?;
    let fixture_path = fixtures_dir().join(fixture);

    // Prepend the chosen bin dir to PATH. Discovery walks PATH in order
    // and stops at the first `node`, so the chosen version wins even if
    // a different Node is also installed on the inherited PATH.
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin_dir];
    paths.extend(std::env::split_paths(&existing));
    let new_path = std::env::join_paths(paths).expect("join PATH");

    let output = Command::new(nub_binary())
        .arg(fixture_path.join(file))
        .current_dir(&fixture_path)
        .env("PATH", new_path)
        .output()
        .expect("failed to spawn nub");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    Some((stdout, stderr, code))
}

/// Node 22.13.0 is the compat-tier representative: above the 18.19 floor,
/// below the 22.15 fast-path floor. The contract: TS still transpiles and
/// runs to completion *silently* — no compat-mode notice on stderr. The
/// two augmentation tiers are an internal mechanism distinction with no
/// user-visible difference. See wiki/research/supported-node-versions.md
/// for the rationale on dropping the notice.
#[test]
fn compat_tier_runs_ts_silently() {
    let Some((stdout, stderr, code)) =
        run_nub_against_node((22, 13, 0), "version-verify", "hello.ts")
    else {
        eprintln!(
            "skipping: Node 22.13.0 not installed (set TEST_NODE_BIN_22_13_0 or install via nvm)"
        );
        return;
    };

    assert_eq!(code, 0, "compat-tier run must succeed: stderr={stderr}");

    // Stdout: the fixture transpiled + ran. The greet() output is the
    // canonical proof that TS reached the runtime (arrow fn + type
    // annotation stripped; `import` resolved).
    assert!(
        stdout.contains("hello, nub"),
        "ts must transpile + run on compat tier: stdout={stdout:?}"
    );

    // Stderr: no notice. The earlier design fired a permanent
    // "Nub running in compatibility mode on Node <ver>..." line on every
    // invocation; that was removed 2026-05-29. Regression-guard against
    // it returning.
    assert!(
        !stderr.contains("running in compatibility mode"),
        "compat-mode notice must NOT fire (dropped 2026-05-29): stderr={stderr:?}"
    );
    assert!(
        !stderr.contains("compatibility mode"),
        "no 'compatibility mode' phrasing should appear on stderr: stderr={stderr:?}"
    );
}

/// Import Text on the COMPAT tier (async `module.register` loader worker). The
/// feature is served by a load-hook branch that exists in BOTH tier entrypoints
/// (`preload.cjs`'s sync `registerHooks` and `preload-async-hooks.mjs`'s async
/// worker); the `integration.rs` cases run on the host's fast-tier Node, so this
/// pins the async path a modern host would otherwise mask. Node 22.13.0 is the
/// compat-tier representative (>18.20 so `with {}` parses, <22.15 so the async
/// worker is used).
#[test]
fn import_text_works_on_compat_tier() {
    let Some((stdout, stderr, code)) = run_nub_against_node((22, 13, 0), "import-text", "main.ts")
    else {
        eprintln!(
            "skipping: Node 22.13.0 not installed (set TEST_NODE_BIN_22_13_0 or nvm install)"
        );
        return;
    };
    assert_eq!(
        code, 0,
        "compat-tier import-text must succeed: stderr={stderr}"
    );
    assert!(
        stdout.contains(r##"md:"# Release notes\n\n- first\n- second\n""##),
        "compat tier: .md read as text via the async loader worker: stdout={stdout:?}"
    );
    assert!(
        stdout.contains("yaml-is-string:true") && stdout.contains("json-is-string:true"),
        "compat tier: the attribute wins over data-loader parsing on the async path: stdout={stdout:?}"
    );
}

/// Import Text on the FAST tier BELOW 26.5 (Node 24.x): sync `module.registerHooks`,
/// but native `--experimental-import-text` does not exist yet, so nub serves text imports
/// via its own `loadTextImport` short-circuit (the `NATIVE_IMPORT_TEXT=false` arm of the
/// makeHooks load hook). Pins the 24.x fast-tier path deterministically — the host-Node
/// `integration.rs` cases silently migrate to the native-defer path once the host reaches
/// >= 26.5 (AGENTS.md "latest major"), leaving [22.15, 26.5) otherwise uncovered.
#[test]
fn import_text_works_on_fast_tier_below_26_5() {
    let Some((stdout, stderr, code)) = run_nub_against_node((24, 4, 0), "import-text", "main.ts")
    else {
        eprintln!("skipping: Node 24.4.0 not installed (set TEST_NODE_BIN_24_4_0 or nvm install)");
        return;
    };
    assert_eq!(
        code, 0,
        "fast-tier (<26.5) import-text must succeed via nub's short-circuit: stderr={stderr}"
    );
    assert!(
        stdout.contains(r##"md:"# Release notes\n\n- first\n- second\n""##),
        "fast tier <26.5: .md read as text via nub's loadTextImport: stdout={stdout:?}"
    );
    assert!(
        stdout.contains("yaml-is-string:true") && stdout.contains("json-is-string:true"),
        "fast tier <26.5: the attribute wins over data-loader parsing: stdout={stdout:?}"
    );
}

/// Import Text on the NATIVE tier (Node >= 26.5.0). There nub injects
/// `--experimental-import-text` and its preload DEFERS `with { type: "text" }` to
/// Node's native text translator (feature-detected via the `NATIVE_IMPORT_TEXT` const
/// in preload-common.cjs, `process.allowedNodeEnvironmentFlags`) instead of
/// serving it with nub's own `loadTextImport`. The user-visible result must be
/// byte-identical to the compat/host tiers — SAME fixture, SAME assertions — proving
/// the mechanism swap is transparent, and the experimental warning native emits must
/// stay suppressed by nub's injected `--disable-warning=ExperimentalWarning`.
#[test]
fn import_text_works_via_native_on_26_5() {
    let Some((stdout, stderr, code)) = run_nub_against_node((26, 5, 0), "import-text", "main.ts")
    else {
        eprintln!("skipping: Node 26.5.0 not installed (set TEST_NODE_BIN_26_5_0 or nvm install)");
        return;
    };
    assert_eq!(
        code, 0,
        "native-tier import-text must succeed: stderr={stderr}"
    );
    assert!(
        stdout.contains(r##"md:"# Release notes\n\n- first\n- second\n""##),
        "native tier: .md read as text via Node's native translator: stdout={stdout:?}"
    );
    assert!(
        stdout.contains("yaml-is-string:true") && stdout.contains("json-is-string:true"),
        "native tier: the attribute wins over data-loader parsing on the native path: stdout={stdout:?}"
    );
    // Native's textStrategy emits ExperimentalWarning('Text import'); nub's injected
    // --disable-warning=ExperimentalWarning must keep it off the user's stderr.
    assert!(
        !stderr.contains("ExperimentalWarning"),
        "native import-text experimental warning must stay suppressed: stderr={stderr:?}"
    );
}

/// Node 18.18.0 is one patch below the 18.19 floor — the boundary case
/// for the hard-error tier. Contract: stderr carries the canonical
/// refusal text, exit is non-zero, and (implicitly) Node was never
/// spawned to evaluate the file (the fixture would have printed
/// `hello, nub` if it ran; we assert stdout is empty of that string).
#[test]
fn unsupported_tier_refuses_with_canonical_text() {
    let Some((stdout, stderr, code)) =
        run_nub_against_node((18, 18, 0), "version-verify", "hello.ts")
    else {
        eprintln!(
            "skipping: Node 18.18.0 not installed (set TEST_NODE_BIN_18_18_0 or install via nvm)"
        );
        return;
    };

    assert_ne!(code, 0, "must refuse with non-zero exit, got {code}");

    // The canonical refusal text from
    // `wiki/research/supported-node-versions.md`. Pinning the exact
    // sentence is deliberate — paraphrasing is the failure mode the
    // research doc warned about.
    assert!(
        stderr.contains("Nub requires Node 18.19 or newer for runtime augmentation."),
        "missing canonical refusal lead-in: stderr={stderr:?}"
    );
    // No pin file lives in the `version-verify` fixture, so the PATH-discovered
    // Node hits the no-pin branch — which, per the committed decision (commit
    // f2adefa, the maintainer 2026-05-29), must NOT attribute a Node choice to the
    // project: no "This project pins …" sentence, no " via " clause.
    assert!(
        !stderr.contains("This project") && !stderr.contains(" via "),
        "no-pin refusal must not claim the project pinned a Node version: stderr={stderr:?}"
    );
    assert!(
        stderr.contains("upgrade Node to 18.19+") || stderr.contains("update the pin to 18.19+"),
        "refusal must offer the upgrade path: stderr={stderr:?}"
    );
    assert!(
        stderr.contains("run plain `node` directly"),
        "refusal must offer the plain-node escape hatch: stderr={stderr:?}"
    );

    // Node was never spawned: the fixture's stdout marker must be absent.
    assert!(
        !stdout.contains("hello, nub"),
        "fixture must not have run — Node spawn should have been refused: stdout={stdout:?}"
    );
}

/// Compat-tier CommonJS `require()` parity (the 2026-05-30 fix). `module.register`
/// hooks only the ESM loader, so before the fix `require()` of a `.ts`, a
/// tsconfig-paths alias, or an extensionless specifier silently failed on Node
/// 18.19–22.14 while working on the fast path — a "feature surface not identical
/// across tiers" break. The fix installs a main-thread Module._resolveFilename +
/// require.extensions shim and corrects the loader-worker's module-format
/// detection (a CJS-content `.ts` is now reported `commonjs`, so Node's
/// CJS-translator loads it as CJS). `cjs-ts-require/main.cts` exercises both a
/// `require("@lib/config")` alias and an extensionless `require("./lib/config")`
/// whose targets are CJS-content `.ts` files. The fast tier prints
/// `alias:42 extless:42`; the compat tier must now print byte-identical output.
#[test]
fn compat_tier_augments_commonjs_require() {
    let Some((stdout, stderr, code)) =
        run_nub_against_node((22, 13, 0), "cjs-ts-require", "main.cts")
    else {
        eprintln!("skipping: Node 22.13.0 not installed (set TEST_NODE_BIN_22_13_0)");
        return;
    };

    assert_eq!(
        code, 0,
        "compat-tier require() of TS must succeed: stderr={stderr}"
    );
    assert!(
        stdout.contains("alias:42 extless:42"),
        "require() of a tsconfig alias + extensionless TS must resolve, transpile, and run \
         on the compat tier identically to the fast tier (was MODULE_NOT_FOUND before the \
         require-side augmentation landed): stdout={stdout:?}"
    );
}

/// Compat-tier `using` / `await using` lowering (the L129 fix). Node 18 (V8 10.2)
/// has no native Explicit Resource Management, so without oxc's `target: 'es2022'`
/// lowering a `using`-using TS file `SyntaxError`s at parse time. The fix routes
/// both tiers through the shared transform core (which always passes
/// `target: 'es2022'`), so the lowering is identical. `using-syntax.ts` declares
/// sync + async disposables; the lowered output must run to `using:done` with the
/// disposers firing in reverse order — proof the `using` was lowered, not left
/// verbatim.
#[test]
fn compat_tier_lowers_using_declarations() {
    let Some((stdout, stderr, code)) =
        run_nub_against_node((22, 13, 0), "vanilla-ts", "using-syntax.ts")
    else {
        eprintln!("skipping: Node 22.13.0 not installed (set TEST_NODE_BIN_22_13_0)");
        return;
    };

    assert_eq!(
        code, 0,
        "compat-tier `using` lowering must succeed: stderr={stderr}"
    );
    assert!(
        !stderr.contains("SyntaxError"),
        "`using` must be lowered, not left verbatim (a verbatim `using` SyntaxErrors on Node \
         18–22.14's V8): stderr={stderr:?}"
    );
    // Reverse-order disposal is the observable signature of correct lowering.
    assert!(
        stdout.contains("close:b.txt")
            && stdout.contains("close:a.txt")
            && stdout.contains("using:done"),
        "lowered `using` must dispose in reverse declaration order and run to completion: stdout={stdout:?}"
    );
}

/// Compat-tier propagation of the Stage-3-decorator rejection. The fast tier's
/// `stage3_decorators_error_clearly` (integration.rs) throws the diagnostic
/// in-thread; on the compat tier the same shared-core check throws inside the
/// loader worker and Node must surface it across the worker boundary. This locks
/// that the worker propagates the Nub-branded diagnostic — not a swallowed error
/// or a bare V8 `SyntaxError` from oxc's verbatim decorator passthrough. (The
/// stack framing differs between tiers; the message + non-zero exit must not.)
#[test]
fn compat_tier_propagates_stage3_decorator_rejection() {
    let Some((_stdout, stderr, code)) =
        run_nub_against_node((22, 13, 0), "vanilla-ts", "stage3-decorators.ts")
    else {
        eprintln!("skipping: Node 22.13.0 not installed (set TEST_NODE_BIN_22_13_0)");
        return;
    };

    assert_ne!(
        code, 0,
        "Stage 3 decorators must be rejected on the compat tier too"
    );
    assert!(
        stderr.contains("Stage 3 decorators are not supported"),
        "the loader worker must propagate the Nub diagnostic across the worker boundary, \
         not swallow it or emit a bare V8 SyntaxError: stderr={stderr:?}"
    );
}

/// `require()` of an ESM-syntax TS module on Node WITHOUT native `.ts` support
/// (here 22.13) — the one case nub can't make WORK (it needs Node's require(esm) of
/// a transpiled ES module, whose loader-worker translator path crashes below the
/// #60380 fix). The contract is that nub turns Node's opaque `cjsCache.get(...)`
/// TypeError into a clean, actionable ERR_REQUIRE_ESM naming the file and the fix —
/// surfaced by the main-thread Module._resolveFilename pre-check before Node's
/// special-require reaches it. The message MUST NOT leak internal mechanism names
/// ("compat tier", "fast tier", a specific Node-version floor). (`require()` of
/// CJS-content TS — the common case — works; covered by
/// `compat_tier_augments_commonjs_require`.)
#[test]
fn compat_tier_rejects_require_of_esm_ts_with_clean_error() {
    let Some((_stdout, stderr, code)) =
        run_nub_against_node((22, 13, 0), "ts-resolution", "requires-esm.cts")
    else {
        eprintln!("skipping: Node 22.13.0 not installed (set TEST_NODE_BIN_22_13_0)");
        return;
    };

    assert_ne!(
        code, 0,
        "require() of an ESM-syntax TS module must be rejected here"
    );
    assert!(
        stderr.contains("ERR_REQUIRE_ESM") && stderr.contains("it is an ES module"),
        "must be nub's clean ERR_REQUIRE_ESM, not Node's opaque cjsCache crash: stderr={stderr:?}"
    );
    // The message is user-facing: no internal tier names or version floors leaking out.
    for leak in ["compat tier", "fast tier", "augmented tier", "22.15"] {
        assert!(
            !stderr.contains(leak),
            "error message must not leak the internal mechanism name {leak:?}: stderr={stderr:?}"
        );
    }
    assert!(
        !stderr.contains("cjsCache"),
        "the pre-check must fire BEFORE Node's special-require crashes: stderr={stderr:?}"
    );
}

/// Run `nub run <script>` in a fixture dir with PATH pinned to the requested
/// Node (same discovery + skip-if-absent contract as `run_nub_against_node`).
fn run_nub_script(
    want: (u32, u32, u32),
    fixture: &str,
    script: &str,
) -> Option<(String, String, i32)> {
    let bin_dir = find_node_bin_dir(want)?;
    let fixture_path = fixtures_dir().join(fixture);
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin_dir];
    paths.extend(std::env::split_paths(&existing));
    let new_path = std::env::join_paths(paths).expect("join PATH");

    let output = Command::new(nub_binary())
        .arg("run")
        .arg(script)
        .current_dir(&fixture_path)
        .env("PATH", new_path)
        .output()
        .expect("failed to spawn nub run");
    Some((
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
    ))
}

/// The resolveSync hook-composition fix (nodejs/node#59666). On the broken band
/// (22.15.0–24.11.0) nub's sync `module.registerHooks` fast tier crashes with
/// ERR_METHOD_NOT_IMPLEMENTED when it composes with a foreign async loader
/// (tsx/ts-node), because the async loader's `resolveSync` is a throwing stub
/// there. nub avoids it by registering its own hooks via the async tier for that
/// process, signalled by `__NUB_FORCE_ASYNC_TIER`. The tier CHOICE is the
/// contract, so this asserts the signal directly (a hermetic proxy for the crash,
/// which needs a real tsx to reproduce — see the PR for the manual tsx validation).
#[test]
fn force_async_tier_signal_gated_on_broken_band_and_foreign_loader() {
    // Broken band (22.16.0): a script carrying `--import` (a foreign async loader)
    // forces the async tier; the signal propagates to the child that prints it.
    if let Some((out, err, code)) = run_nub_script((22, 16, 0), "force-async-tier", "loader") {
        assert_eq!(code, 0, "loader script must run: stderr={err}");
        assert!(
            out.contains("FORCE=1"),
            "a loader-hosting script on the broken band must force the async tier: {out:?}"
        );
        // Broken band + a PLAIN script keeps the sync fast tier (no common-case
        // degradation) — the whole point of gating on foreign-loader presence.
        let (plain, _e, c) = run_nub_script((22, 16, 0), "force-async-tier", "plain").unwrap();
        assert_eq!(c, 0);
        assert!(
            plain.contains("FORCE=unset"),
            "a plain script must keep the sync fast tier on the broken band: {plain:?}"
        );
    } else {
        eprintln!("skipping: Node 22.16.0 not installed (broken-band representative)");
    }

    // Fixed Node (26.2.0): even a loader-hosting script keeps the fast tier —
    // Node implemented the sync methods, so there is nothing to work around.
    if let Some((out, err, code)) = run_nub_script((26, 2, 0), "force-async-tier", "loader") {
        assert_eq!(
            code, 0,
            "loader script must run on fixed Node: stderr={err}"
        );
        assert!(
            out.contains("FORCE=unset"),
            "a fixed Node must NOT force the async tier (fast tier is safe there): {out:?}"
        );
    } else {
        eprintln!("skipping: Node 26.2.0 not installed (fixed-band representative)");
    }
}

/// Module syntax detection — the `--experimental-detect-module` matrix bands
/// [20.10.0, 20.19.0) ∪ [21.1.0, 22.7.0). Node 20.11.0 carries the flag but does not
/// default it on (that came at 20.19), so an ambiguous ESM `.js` — ES-module syntax, a
/// `package.json` with no `"type"` field — that bare Node 20.11 refuses ("To load an ES
/// module, set type: module", exit 1) runs as ESM under nub, which injects the flag from
/// the matrix band. This is the in-band, still-required case; a default-on Node needs no
/// injection.
#[test]
fn detect_module_ambiguous_esm_runs_as_esm_on_compat_tier() {
    let Some((stdout, stderr, code)) = run_nub_against_node((20, 11, 0), "detect-module", "amb.js")
    else {
        eprintln!(
            "skipping: Node 20.11.0 not installed (set TEST_NODE_BIN_20_11_0 or nvm install)"
        );
        return;
    };
    assert_eq!(
        code, 0,
        "compat-tier ambiguous ESM .js must run as ESM under nub (bare Node 20.11 refuses it): stderr={stderr}"
    );
    assert!(
        stdout.contains("detect-module:ran-as-esm:true"),
        "ambiguous .js must be detected and run as an ES module: stdout={stdout:?}"
    );
}

/// The node:ffi (26.1+), node:vfs (26.4+), and node:stream/iter (25.9+) enabler bands.
/// Each module is flag-gated and default-off — bare Node throws
/// ERR_UNKNOWN_BUILTIN_MODULE on import — so nub injecting the three `--experimental-*`
/// flags from the matrix is exactly what makes them importable. Run on Node 26.5.0, which
/// carries all three; the fixture imports each and the markers all read true.
#[test]
fn module_enabler_flags_make_ffi_vfs_stream_iter_importable() {
    let Some((stdout, stderr, code)) =
        run_nub_against_node((26, 5, 0), "module-enablers", "enablers.mjs")
    else {
        eprintln!("skipping: Node 26.5.0 not installed (set TEST_NODE_BIN_26_5_0 or nvm install)");
        return;
    };
    assert_eq!(
        code, 0,
        "node:ffi/vfs/stream-iter must import under nub (bare Node rejects them default-off): stderr={stderr}"
    );
    assert!(
        stdout.contains("module-enablers:true,true,true"),
        "all three enabler modules must load once nub injects their flags: stdout={stdout:?}"
    );
}
