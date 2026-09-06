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
