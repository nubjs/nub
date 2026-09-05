//! Real dependency lifecycle confinement and explicit opt-out controls.

#[cfg(not(feature = "build-jail-catalog-override"))]
#[test]
fn unsupported_catalog_override_is_not_silently_ignored() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nub"))
        .arg("--version")
        .env("NUB_BUILD_JAIL_CATALOG", "missing-test-catalog.json")
        .output()
        .expect("run catalog override refusal");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot honour it"));
}

#[test]
fn dependency_lifecycle_contract() {
    let output = std::process::Command::new("node")
        .arg("--test")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/build-jail-corpus/contract.mjs"
        ))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/build-jail-corpus/private-registry.mjs"
        ))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/build-jail-corpus/descendants.mjs"
        ))
        .env("NUB_BIN", env!("CARGO_BIN_EXE_nub"))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/build-jail-corpus/cache-modes.mjs"
        ))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/build-jail-corpus/layouts.mjs"
        ))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/build-jail-corpus/fetched-native.mjs"
        ))
        .output()
        .expect("run the lifecycle contract with Node");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
