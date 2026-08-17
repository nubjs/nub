//! Top-level `package.json#packageExtensions` under Nub identity (#492).
//!
//! `packageExtensions` is a sanctioned neutral field: it patches a
//! dependency's manifest at resolve time — adding to its dependencies /
//! optionalDependencies / peerDependencies / peerDependenciesMeta, add-only,
//! never overriding a declared range — matching pnpm's `packageExtensions`.
//! This proves two contracts end-to-end in one shared-store install loop: the
//! field shapes the resolved graph, and EDITING it after an install invalidates
//! freshness (the install shape digest folds `packageExtensions`, so the edit
//! is not treated as cosmetic and the fast path re-resolves).
//!
//! Network (`#[ignore]`, self-skips when the registry is unreachable), per the
//! install-test convention — run via
//! `cargo test -p nub-cli --test package_extensions -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn pm_tmpdir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-pkgext-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn registry_reachable() -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    "registry.npmjs.org:443"
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .is_some_and(|addr| {
            TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3)).is_ok()
        })
}

/// Run `nub <args>` in `dir` against a CALLER-OWNED store/cache so the second
/// install warm-hits the first's node_modules + freshness state. A fresh store
/// per call would re-resolve unconditionally and mask the freshness contract.
fn run_install_in_store(dir: &Path, store: &Path, cache: &Path, args: &[&str]) -> (String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", store)
        .env("XDG_CACHE_HOME", cache)
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// Whether the virtual store under `node_modules/.store` holds any version of
/// `name`.
fn store_has(dir: &Path, name: &str) -> bool {
    let prefix = format!("{name}@");
    std::fs::read_dir(dir.join("node_modules/.store"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .any(|n| n.starts_with(&prefix))
}

/// Whether the virtual store holds the exact `name@version` entry.
fn store_has_version(dir: &Path, name: &str, version: &str) -> bool {
    let target = format!("{name}@{version}");
    std::fs::read_dir(dir.join("node_modules/.store"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .any(|n| n == target)
}

/// A top-level `packageExtensions` entry injecting a dependency into a resolved
/// package must shape the graph under Nub identity, and editing it after an
/// install must invalidate the fast path so the injected dep lands.
#[test]
#[ignore = "network: resolves is-positive@3.1.0 + the injected is-number@7.0.0 from the npm registry"]
fn top_level_package_extensions_shapes_resolution_and_invalidates_freshness() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("shape");
    let store = pm_tmpdir("store");
    let cache = pm_tmpdir("cache");

    // Nub-identity project (no lockfile, no PM declaration): one zero-dep
    // dependency, no packageExtensions yet.
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"pkgext","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();

    let (err1, code1) = run_install_in_store(&dir, &store, &cache, &["install"]);
    assert_eq!(code1, 0, "baseline install failed: {err1}");
    assert!(
        dir.join("nub.lock").is_file(),
        "the nub-identity install writes nub.lock: {err1}"
    );
    assert!(
        !store_has(&dir, "is-number"),
        "without the extension the graph must not contain is-number: {err1}"
    );

    // Add a top-level packageExtensions entry injecting is-number into
    // is-positive. is-positive declares no deps, so the add-only merge adds it.
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"pkgext","version":"1.0.0","dependencies":{"is-positive":"3.1.0"},"packageExtensions":{"is-positive@3.1.0":{"dependencies":{"is-number":"7.0.0"}}}}"#,
    )
    .unwrap();

    // Re-install against the SAME store. The packageExtensions edit changes the
    // install shape digest, so the fast path re-resolves and pulls the injected
    // is-number. Without packageExtensions in the shape digest the install
    // short-circuits ("Already up to date") and is-number never lands.
    let (err2, code2) = run_install_in_store(&dir, &store, &cache, &["install"]);
    assert_eq!(code2, 0, "extended install failed: {err2}");
    assert!(
        store_has(&dir, "is-number"),
        "top-level packageExtensions must inject is-number into is-positive, and \
         the edit must invalidate the install fast path so it re-resolves: {err2}"
    );
}

/// Read the `packageExtensionsChecksum` aube stamps onto `nub.lock` (pnpm-v9
/// YAML format), or `None` when the lockfile carries no checksum.
fn lockfile_checksum(dir: &Path) -> Option<String> {
    let lock = std::fs::read_to_string(dir.join("nub.lock")).ok()?;
    for line in lock.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("packageExtensionsChecksum:") {
            let v = rest.trim().trim_matches('"');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// A bundled ecosystem default (Yarn ∪ pnpm ∪ nub-phantom, vendored at
/// `vendor/package-extensions/unified.json`) must shape the resolved graph
/// with NO user `packageExtensions` — and must NOT leak into the lockfile
/// `packageExtensionsChecksum` (routing it there would drift every existing
/// lockfile on each bundled-list bump and abort `--frozen-lockfile`).
///
/// `gatsby-core-utils@2.13.0` declares neither `got` nor `@babel/runtime`,
/// and `2.13.0` satisfies the bundled selector
/// `gatsby-core-utils@<2.14.0-next.1`, so the bundled extension injecting
/// `got` is observable: without it, `got` is absent from the graph.
///
/// The checksum guard has two cases: (1) empty user `packageExtensions` →
/// aube writes NO `packageExtensionsChecksum` field (the checksum fn returns
/// `None` for an empty map), so a bundled-list bump cannot drift the
/// lockfile — there is nothing to mismatch; (2) non-empty user
/// `packageExtensions` with the bundled default ALSO shaping the graph → the
/// checksum must equal `package_extensions_checksum(&user_pe_only)`, proving
/// the bundled map is not folded into the checksum input.
#[test]
#[ignore = "network: resolves gatsby-core-utils@2.13.0 + the bundled got from the npm registry"]
fn bundled_default_shapes_graph_and_stays_out_of_checksum() {
    use aube_lockfile::pnpm::package_extensions_checksum;
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let store = pm_tmpdir("store");
    let cache = pm_tmpdir("cache");

    // (1) No user packageExtensions: the bundled default must still apply,
    // and the lockfile must carry NO checksum (empty user PE → None → a
    // bundled-list bump cannot drift this lockfile).
    let dir_a = pm_tmpdir("bundled-a");
    let pkg_a =
        r#"{"name":"bundled-a","version":"1.0.0","dependencies":{"gatsby-core-utils":"2.13.0"}}"#;
    std::fs::write(dir_a.join("package.json"), pkg_a).unwrap();
    let (err_a, code_a) = run_install_in_store(&dir_a, &store, &cache, &["install"]);
    assert_eq!(code_a, 0, "bundled-default install A failed: {err_a}");
    assert!(
        store_has(&dir_a, "got"),
        "the bundled `gatsby-core-utils@<2.14.0-next.1` extension must inject `got` \
         (undeclared by 2.13.0) into the graph with no user packageExtensions: {err_a}"
    );
    assert!(
        dir_a.join("nub.lock").is_file(),
        "A: nub-identity install writes nub.lock: {err_a}"
    );
    assert_eq!(
        lockfile_checksum(&dir_a),
        None,
        "empty user packageExtensions must produce NO packageExtensionsChecksum \
         (the checksum fn returns None for an empty map), so a bundled-list bump \
         cannot drift the lockfile: {err_a}"
    );

    // (2) Non-empty user packageExtensions, with the bundled default ALSO
    // shaping the graph: the checksum must reflect ONLY the user's
    // packageExtensions, not the bundled map. Compare against
    // `package_extensions_checksum` computed on the user-PE-only map.
    let dir_b = pm_tmpdir("bundled-b");
    let user_pe = r#"{"is-positive@3.1.0":{"dependencies":{"is-number":"7.0.0"}}}"#;
    let pkg_b = format!(
        r#"{{"name":"bundled-b","version":"1.0.0","dependencies":{{"gatsby-core-utils":"2.13.0","is-positive":"3.1.0"}},"packageExtensions":{user_pe}}}"#
    );
    std::fs::write(dir_b.join("package.json"), pkg_b).unwrap();
    let (err_b, code_b) = run_install_in_store(&dir_b, &store, &cache, &["install"]);
    assert_eq!(code_b, 0, "bundled-default install B failed: {err_b}");
    assert!(
        store_has(&dir_b, "got"),
        "B: bundled default still applies alongside user packageExtensions: {err_b}"
    );
    let user_pe_map: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_str(user_pe).unwrap();
    let expected =
        package_extensions_checksum(&user_pe_map).expect("non-empty user PE yields a checksum");
    assert_eq!(
        lockfile_checksum(&dir_b).as_deref(),
        Some(expected.as_str()),
        "the lockfile packageExtensionsChecksum must equal the hash of the \
         USER packageExtensions only — the bundled map (actively shaping this \
         graph via `got`) must not be folded into the checksum input, or every \
         bundled-list bump drifts existing lockfiles and aborts \
         --frozen-lockfile: {err_b}"
    );

    // (3) User packageExtensions OVERRIDE the bundled default on a matching
    // selector + dependency key. The bundled `gatsby-core-utils@<2.14.0-next.1`
    // injects `got: 8.3.2`; a user entry for the SAME selector injecting
    // `got: 8.3.0` must win (user-first Vec ordering + `extend_missing`
    // first-write-wins), so the resolved `got` is the user's 8.3.0, not the
    // bundled 8.3.2. This guards the precedence construction in
    // `resolve_dependency_policy`, which the existing aube `extend_missing`
    // unit tests do not cover.
    let dir_c = pm_tmpdir("bundled-c");
    let user_pe_c = r#"{"gatsby-core-utils@<2.14.0-next.1":{"dependencies":{"got":"8.3.0"}}}"#;
    let pkg_c = format!(
        r#"{{"name":"bundled-c","version":"1.0.0","dependencies":{{"gatsby-core-utils":"2.13.0"}},"packageExtensions":{user_pe_c}}}"#
    );
    std::fs::write(dir_c.join("package.json"), pkg_c).unwrap();
    let (err_c, code_c) = run_install_in_store(&dir_c, &store, &cache, &["install"]);
    assert_eq!(code_c, 0, "bundled-default install C failed: {err_c}");
    assert!(
        store_has_version(&dir_c, "got", "8.3.0"),
        "user packageExtensions must override the bundled `got: 8.3.2` with the \
         user's `got: 8.3.0` on the same selector: {err_c}"
    );
    assert!(
        !store_has_version(&dir_c, "got", "8.3.2"),
        "the bundled `got: 8.3.2` must NOT be resolved when the user overrides \
         the same selector+key with 8.3.0: {err_c}"
    );
}

/// The bundled compatibility database repairs a real published package whose
/// manifest is wrong, and `ignoreCompatibilityDb` turns it off.
///
/// This is pnpm's own bundled data — Yarn's `packageExtensions` database plus
/// pnpm's additions — and pnpm merges it into every install. `reactcss@1.2.3`
/// requires `react` and declares it nowhere, so the catalog entry
/// `reactcss@* -> peerDependencies.react` is what makes `auto-install-peers`
/// supply it. Until the engine's embedder gate came off, that catalog applied
/// only to standalone aube, so this package installed under pnpm and threw
/// `Cannot find module 'react'` under nub.
///
/// Both arms matter. The opt-out arm is the control: it proves the pass is the
/// database doing work rather than `react` arriving by some other route, and it
/// proves the setting is still reachable — an escape hatch that silently did
/// nothing would leave no way to decline the repair.
#[test]
#[ignore = "network: resolves reactcss@1.2.3 and the react the compat database adds"]
fn the_bundled_compatibility_database_repairs_a_published_phantom() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let manifest = r#"{"name":"compatdb","version":"1.0.0","dependencies":{"reactcss":"1.2.3"}}"#;
    let store = pm_tmpdir("compatdb-store");
    let cache = pm_tmpdir("compatdb-cache");

    let on = pm_tmpdir("compatdb-on");
    std::fs::write(on.join("package.json"), manifest).unwrap();
    let (err_on, code_on) = run_install_in_store(&on, &store, &cache, &["install"]);
    assert_eq!(code_on, 0, "install with the database failed: {err_on}");
    assert!(
        store_has(&on, "react"),
        "the bundled database must add reactcss's undeclared react peer so it is \
         installed: {err_on}"
    );

    let off = pm_tmpdir("compatdb-off");
    std::fs::write(off.join("package.json"), manifest).unwrap();
    std::fs::write(off.join(".npmrc"), "ignore-compatibility-db=true\n").unwrap();
    let (err_off, code_off) = run_install_in_store(&off, &store, &cache, &["install"]);
    assert_eq!(
        code_off, 0,
        "install with the database off failed: {err_off}"
    );
    assert!(
        !store_has(&off, "react"),
        "ignore-compatibility-db must decline the repair, leaving reactcss's \
         undeclared import unresolved: {err_off}"
    );
}
