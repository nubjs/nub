//! The runtime embedder seams that route through `EngineContext`, exercised
//! end-to-end against a real manifest.
//!
//! Lives in its own integration-test binary (= its own process) because the
//! engine context is process-global: in-crate unit tests would race the
//! default (upstream-neutral) context. Mirrors the `Embedder`-registration
//! sibling-process pattern used by `aube-lockfile/tests/custom_lock_filename.rs`.
//!
//! One test fn so the unset→set ordering on the process-global context is
//! deterministic — two `#[test]`s in this binary could run in parallel and
//! race the shared context.

use std::collections::BTreeMap;

use aube_manifest::PackageJson;
use aube_util::update_engine_context;

fn manifest(json: &str) -> PackageJson {
    serde_json::from_str(json).expect("manifest parses")
}

#[test]
fn embedder_overrides_and_trusted_deps_gates_route_through_engine_context() {
    let pkg = manifest(
        r#"{
            "name": "t",
            "resolutions": { "lodash": "4.17.21" },
            "pnpm": { "overrides": { "minimist": "1.2.8" } },
            "trustedDependencies": ["esbuild"]
        }"#,
    );

    // --- upstream-neutral default: both gates open ---
    // overrides_map folds every source; trusted_dependencies honors the array.
    let folded = pkg.overrides_map();
    assert_eq!(folded.get("lodash").map(String::as_str), Some("4.17.21"));
    assert_eq!(folded.get("minimist").map(String::as_str), Some("1.2.8"));
    assert_eq!(pkg.trusted_dependencies(), vec!["esbuild".to_string()]);

    // --- embedder_overrides: a scoped map replaces the fold verbatim ---
    let scoped: BTreeMap<String, String> = [("only-this".to_string(), "9.9.9".to_string())]
        .into_iter()
        .collect();
    update_engine_context(|c| c.embedder_overrides = Some(scoped.clone()));
    assert_eq!(
        pkg.overrides_map(),
        scoped,
        "a Some(map) override source is returned verbatim, skipping the manifest fold"
    );

    // --- trusted_dependencies_honored = false: the array contributes nothing ---
    update_engine_context(|c| c.trusted_dependencies_honored = false);
    assert!(
        pkg.trusted_dependencies().is_empty(),
        "a non-Bun incumbent suppresses trustedDependencies entirely"
    );

    // --- restore the upstream-neutral context, confirm both gates reopen ---
    update_engine_context(|c| {
        c.embedder_overrides = None;
        c.trusted_dependencies_honored = true;
    });
    assert!(
        pkg.overrides_map().contains_key("lodash"),
        "clearing embedder_overrides restores the manifest fold"
    );
    assert_eq!(pkg.trusted_dependencies(), vec!["esbuild".to_string()]);
}
