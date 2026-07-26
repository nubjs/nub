//! End-to-end enforcement of nub's build-jail (the policy `compile_build_jail`
//! produces, applied to a real child) — the load-bearing security assertions: the
//! package dir is writable, and home SECRETS stay both unreadable AND unwritable even
//! when the confined child runs inside a writable scratch.
//!
//! macOS only. Seatbelt (`sandbox-exec`) enforces in a plain process, so this runs in
//! `cargo test` on the dev host. Linux enforcement (Bubblewrap + the empirically
//! derived read set — project/`$tooldirs`/interpreter grants, own-pkg write, egress
//! deny) rides CI / a real-kernel VM; the read-set spike validated it via Docker+bwrap.
//!
//! macOS caveat (deliberate build-jail behavior): the DARWIN confstr scratch
//! (`/var/folders/<uid>/T`, where `tempfile` puts this fixture) stays WRITABLE — the
//! Apple toolchain's `xcrun_db` lives there and is not `TMPDIR`-redirectable. So a
//! generic "write outside the package dir" cannot be asserted for a fixture under the
//! scratch; this asserts the SECURITY-critical denials (secrets read+write) instead,
//! which hold even inside the scratch via the secret floor + the move-block re-deny.
#![cfg(target_os = "macos")]

use std::collections::BTreeMap;

fn homes(home: &std::path::Path, project: &std::path::Path) -> nub_sandbox::Homes {
    nub_sandbox::Homes {
        home: home.to_path_buf(),
        tmp: std::env::temp_dir(),
        cache: home.join("Library/Caches"),
        project: project.to_path_buf(),
    }
}

/// A native build/postinstall confined by the build-jail: it may read its own source
/// and write its own package dir, but the home secret is neither readable nor writable.
#[test]
fn build_jail_confines_writes_and_hides_secrets() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let package_dir = project.join("node_modules/.aube/native@1.0.0/node_modules/native");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_rsa"), b"PRIVATE-KEY-DO-NOT-LEAK").unwrap();
    // A source file in the package dir the build legitimately reads.
    std::fs::write(package_dir.join("binding.gyp"), b"{}").unwrap();

    // The interpreter path is only granted-read; the script doesn't run it here, so a
    // placeholder under the (real) home cache keeps the compile honest without needing
    // a provisioned Node in the test.
    let interpreter = home.join("Library/Caches/nub/node/bin/node");
    let ambient: BTreeMap<String, String> =
        [("PATH", "/usr/bin:/bin"), ("npm_package_name", "native")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

    let policy = nub_sandbox::compile_build_jail(
        homes(&home, &project),
        &package_dir,
        vec![interpreter],
        ambient,
    )
    .expect("compile build-jail");

    // The confined script: (1) read its own source (ok), (2) write its own build output
    // (ok), (3) try to READ the home secret, (4) try to WRITE into the home secret dir.
    // It prints a deterministic marker per outcome so the parent asserts on captured
    // stdout rather than on file side effects alone.
    let secret = home.join(".ssh/id_rsa");
    let secret_write = home.join(".ssh/planted.txt");
    let script = format!(
        r#"
        cat binding.gyp >/dev/null 2>&1 && echo READ_OK || echo READ_FAIL
        echo built > build_out.txt 2>/dev/null && echo WRITE_PKG_OK || echo WRITE_PKG_FAIL
        cat '{secret}' >/dev/null 2>&1 && echo SECRET_LEAK || echo SECRET_HIDDEN
        echo evil > '{secret_write}' 2>/dev/null && echo SECRET_WRITE_WROTE || echo SECRET_WRITE_BLOCKED
        "#,
        secret = secret.display(),
        secret_write = secret_write.display(),
    );

    let spec = nub_sandbox::CommandSpec::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .cwd(&package_dir)
        .deny_search_roots([project.clone(), package_dir.clone()]);
    let runtime = nub_sandbox::earliest_bootstrap().expect("bootstrap");
    let prepared = nub_sandbox::apply_with_runtime(&policy, spec, &runtime)
        .expect("apply build-jail (fail-closed on error)");
    let out = prepared.output().expect("run confined script");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("READ_OK"),
        "package source must be readable:\n{stdout}"
    );
    assert!(
        stdout.contains("WRITE_PKG_OK"),
        "the package dir must be writable:\n{stdout}"
    );
    assert!(
        stdout.contains("SECRET_HIDDEN") && !stdout.contains("SECRET_LEAK"),
        "the home secret must be unreadable:\n{stdout}"
    );
    assert!(
        stdout.contains("SECRET_WRITE_BLOCKED") && !stdout.contains("SECRET_WRITE_WROTE"),
        "a write into the home secret dir must be blocked:\n{stdout}"
    );
    // Belt-and-suspenders: the planted file must not exist; the secret is untouched.
    assert!(
        !secret_write.exists(),
        "the secret-dir write must not have landed"
    );
    assert!(package_dir.join("build_out.txt").exists());
    assert_eq!(
        std::fs::read(&secret).unwrap(),
        b"PRIVATE-KEY-DO-NOT-LEAK",
        "the secret file is untouched"
    );
}
